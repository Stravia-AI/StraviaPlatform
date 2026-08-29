use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Json;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::protocol::SseEvent;
use crate::protocol::ids::Protocol;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiResponse, AiStreamDelta};
use crate::protocol::transform::{ProtocolTransform, StreamEncodeStage, TransformError};
use crate::proxy::context::CancellationToken;

use super::RoundOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryProgress {
    Sent,
    Cancelled,
    ReceiverClosed,
    ProtocolFailed,
}

pub(super) struct BufferedDelivery {
    pub(super) response: Response,
    pub(super) body: String,
}

/// Static transport choice for one Inference Run.
///
/// The adapter receives canonical IR only. It owns ingress encoding,
/// backpressure, cancellation and receiver-closure observation, while the Run
/// keeps every lifecycle decision.
pub(super) enum DeliveryAdapter {
    NonStream {
        ingress: ProtocolId,
        egress: ProtocolId,
    },
    Stream {
        ingress: ProtocolId,
        egress: ProtocolId,
        encoder: Box<StreamEncodeStage>,
        failed: bool,
        live: Option<LiveStreamSink>,
    },
}

pub(super) struct LiveStreamRequest {
    pub(super) ingress: ProtocolId,
    pub(super) egress: ProtocolId,
    pub(super) tx: tokio::sync::mpsc::Sender<Result<String, Infallible>>,
    pub(super) cancellation: CancellationToken,
    pub(super) preflight: tokio::sync::oneshot::Sender<Result<(), RoundOutcome>>,
    pub(super) terminal_delivery: tokio::sync::oneshot::Receiver<()>,
    pub(super) commit: tokio::sync::oneshot::Receiver<()>,
    pub(super) capture_payload: bool,
}

pub(super) struct LiveStreamSink {
    tx: tokio::sync::mpsc::Sender<Result<String, Infallible>>,
    cancellation: CancellationToken,
    preflight: Option<tokio::sync::oneshot::Sender<Result<(), RoundOutcome>>>,
    commit: Option<tokio::sync::oneshot::Receiver<()>>,
    terminal_delivery: Option<tokio::sync::oneshot::Receiver<()>>,
    captured: Option<Vec<String>>,
    committed: bool,
}

impl DeliveryAdapter {
    pub(super) fn non_stream(ingress: ProtocolId, egress: ProtocolId) -> Self {
        Self::NonStream { ingress, egress }
    }

    pub(super) fn buffered_stream(ingress: ProtocolId, egress: ProtocolId) -> Self {
        Self::Stream {
            ingress,
            egress,
            encoder: Box::new(stream_encoder(ingress, egress)),
            failed: false,
            live: None,
        }
    }

    pub(super) fn live_stream(request: LiveStreamRequest) -> Self {
        let LiveStreamRequest {
            ingress,
            egress,
            tx,
            cancellation,
            preflight,
            terminal_delivery,
            commit,
            capture_payload,
        } = request;
        Self::Stream {
            ingress,
            egress,
            encoder: Box::new(stream_encoder(ingress, egress)),
            failed: false,
            live: Some(LiveStreamSink {
                tx,
                cancellation,
                preflight: Some(preflight),
                commit: Some(commit),
                terminal_delivery: Some(terminal_delivery),
                captured: capture_payload.then(Vec::new),
                committed: false,
            }),
        }
    }

    pub(super) fn set_response_profile(
        &mut self,
        request: &crate::protocol::ir::AiRequest,
        previous_response_id: Option<&str>,
    ) {
        if let Self::Stream { encoder, .. } = self {
            encoder.set_response_profile(request, previous_response_id);
        }
    }

    pub(super) fn deliver_canonical(
        &mut self,
        response: &AiResponse,
        status: StatusCode,
    ) -> BufferedDelivery {
        match self {
            Self::NonStream { ingress, egress } => {
                let pair = ProtocolTransform::global()
                    .bind(*ingress, *egress)
                    .expect("registered protocol pair");
                match pair.encode_response(response) {
                    Ok(output) => {
                        let body = serde_json::to_string(&output).unwrap_or_default();
                        BufferedDelivery {
                            response: (status, Json(output)).into_response(),
                            body,
                        }
                    }
                    Err(error) => {
                        let output = protocol_error_json(*ingress, &error);
                        let body = serde_json::to_string(&output).unwrap_or_default();
                        BufferedDelivery {
                            response: (StatusCode::UNPROCESSABLE_ENTITY, Json(output))
                                .into_response(),
                            body,
                        }
                    }
                }
            }
            Self::Stream {
                ingress,
                encoder,
                failed,
                live,
                ..
            } => {
                debug_assert!(
                    live.is_none(),
                    "live stream cannot be delivered as one body"
                );
                let mut parts = Vec::new();
                let result = encoder
                    .encode_deltas(&super::ai_response_to_deltas(response))
                    .and_then(|mut events| {
                        events.extend(encoder.finish()?);
                        Ok(events)
                    });
                match result {
                    Ok(events) => {
                        for event in events {
                            parts.push(event.to_sse_string());
                        }
                    }
                    Err(error) => {
                        *failed = true;
                        parts.push(protocol_error_event(*ingress, &error).to_sse_string());
                    }
                }
                let body = parts.join("");
                BufferedDelivery {
                    response: streaming_response(Body::from(body.clone())),
                    body,
                }
            }
        }
    }

    pub(super) async fn send_deltas(&mut self, deltas: &[AiStreamDelta]) -> DeliveryProgress {
        let Self::Stream {
            ingress: _,
            encoder,
            failed,
            live,
            ..
        } = self
        else {
            unreachable!("non-stream delivery cannot send deltas")
        };
        if *failed {
            return DeliveryProgress::ProtocolFailed;
        }
        let events = match encoder.encode_deltas(deltas) {
            Ok(events) => events,
            Err(error) => {
                let terminal_events = encoder.fail(crate::protocol::ir::AiError::new(
                    crate::protocol::ir::AiErrorKind::StreamMidError,
                    format!("STRAVIA_PROTOCOL_LOSSY_REJECTED: {error}"),
                ));
                *failed = true;
                for event in terminal_events {
                    let progress = send_event(
                        live.as_mut().expect("live stream sink"),
                        event.to_sse_string(),
                    )
                    .await;
                    if progress != DeliveryProgress::Sent {
                        return progress;
                    }
                }
                return DeliveryProgress::ProtocolFailed;
            }
        };
        for event in events {
            let progress = send_event(
                live.as_mut().expect("live stream sink"),
                event.to_sse_string(),
            )
            .await;
            if progress != DeliveryProgress::Sent {
                return progress;
            }
        }
        DeliveryProgress::Sent
    }

    pub(super) async fn finish_stream(&mut self, stop_reason: String) -> DeliveryProgress {
        let done = [AiStreamDelta::Done { stop_reason }];
        let progress = self.send_deltas(&done).await;
        if progress != DeliveryProgress::Sent {
            return progress;
        }
        let Self::Stream {
            encoder,
            failed,
            live,
            ..
        } = self
        else {
            unreachable!("non-stream delivery cannot finish a stream")
        };
        if *failed {
            return DeliveryProgress::ProtocolFailed;
        }
        let events = match encoder.finish() {
            Ok(events) => events,
            Err(_) => {
                *failed = true;
                return DeliveryProgress::ProtocolFailed;
            }
        };
        for event in events {
            let progress = send_event(
                live.as_mut().expect("live stream sink"),
                event.to_sse_string(),
            )
            .await;
            if progress != DeliveryProgress::Sent {
                return progress;
            }
        }
        DeliveryProgress::Sent
    }

    pub(super) fn reset_stream_encoder(&mut self) {
        let Self::Stream {
            ingress,
            egress,
            encoder,
            failed,
            ..
        } = self
        else {
            unreachable!("non-stream delivery has no stream encoder")
        };
        **encoder = stream_encoder(*ingress, *egress);
        *failed = false;
    }

    pub(super) fn fail_before_commit(&mut self, outcome: RoundOutcome) -> bool {
        let Self::Stream { live, .. } = self else {
            return false;
        };
        live.as_mut()
            .and_then(|sink| sink.preflight.take())
            .is_some_and(|preflight| preflight.send(Err(outcome)).is_ok())
    }

    pub(super) fn captured_body(&mut self) -> Option<String> {
        let Self::Stream { live, .. } = self else {
            return None;
        };
        live.as_mut()
            .and_then(|sink| sink.captured.take())
            .map(|parts| parts.join(""))
    }
    pub(super) fn response_from_receiver(
        receiver: tokio::sync::mpsc::Receiver<Result<String, Infallible>>,
        commit: tokio::sync::oneshot::Sender<()>,
        terminal_delivery: tokio::sync::oneshot::Sender<()>,
        egress: ProtocolId,
    ) -> Response {
        streaming_response(Body::from_stream(CommitOnPollStream {
            inner: ReceiverStream::new(receiver),
            commit: Some(commit),
            terminal_delivery: Some(terminal_delivery),
            egress,
        }))
    }
    pub(super) async fn wait_for_terminal_delivery(&mut self) -> DeliveryProgress {
        let Self::Stream {
            live: Some(live), ..
        } = self
        else {
            return DeliveryProgress::Sent;
        };
        let Some(terminal_delivery) = live.terminal_delivery.take() else {
            return DeliveryProgress::ReceiverClosed;
        };
        tokio::select! {
            _ = live.cancellation.cancelled() => DeliveryProgress::Cancelled,
            result = terminal_delivery => {
                if result.is_ok() {
                    DeliveryProgress::Sent
                } else {
                    DeliveryProgress::ReceiverClosed
                }
            }
        }
    }

    pub(super) fn commit_response_after_delivery(
        response: Response,
        mut write: crate::generation_chain::GenerationChainWrite,
    ) -> Response {
        after_body_delivery(response, async move {
            if let Err(error) = write.persist().await {
                tracing::error!("failed to commit delivered Generation Chain node: {error}");
            }
        })
    }
}

fn stream_encoder(ingress: ProtocolId, egress: ProtocolId) -> StreamEncodeStage {
    ProtocolTransform::global()
        .bind(ingress, egress)
        .expect("registered protocol pair")
        .stream()
        .expect("stream-capable protocol pair")
        .into_parts()
        .1
}

fn protocol_error_json(ingress: ProtocolId, error: &TransformError) -> serde_json::Value {
    match ingress.protocol {
        Protocol::AnthropicMessages => serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": error.to_string(),
                "code": "STRAVIA_PROTOCOL_LOSSY_REJECTED"
            }
        }),
        Protocol::OpenResponses => serde_json::json!({
            "type": "error",
            "code": "STRAVIA_PROTOCOL_LOSSY_REJECTED",
            "message": error.to_string()
        }),
        _ => serde_json::json!({
            "error": {
                "code": "STRAVIA_PROTOCOL_LOSSY_REJECTED",
                "message": error.to_string(),
                "type": "invalid_request_error"
            }
        }),
    }
}

fn protocol_error_event(ingress: ProtocolId, error: &TransformError) -> SseEvent {
    let event = match ingress.protocol {
        Protocol::AnthropicMessages | Protocol::OpenResponses => Some("error"),
        _ => None,
    };
    SseEvent::new(event, protocol_error_json(ingress, error).to_string())
}

struct CommitOnPollStream {
    inner: ReceiverStream<Result<String, Infallible>>,
    commit: Option<tokio::sync::oneshot::Sender<()>>,
    terminal_delivery: Option<tokio::sync::oneshot::Sender<()>>,
    egress: ProtocolId,
}

impl Stream for CommitOnPollStream {
    type Item = Result<String, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(commit) = self.commit.take() {
            let _ = commit.send(());
        }
        let poll = Pin::new(&mut self.inner).poll_next(context);
        if let Poll::Ready(Some(Ok(payload))) = &poll
            && terminal_payload_delivered(self.egress, payload)
            && let Some(terminal_delivery) = self.terminal_delivery.take()
        {
            let _ = terminal_delivery.send(());
        }
        poll
    }
}

struct DeliveryConfirmedBody {
    inner: axum::body::BodyDataStream,
    task: Option<Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
    inner_complete: bool,
}

impl Stream for DeliveryConfirmedBody {
    type Item = Result<axum::body::Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.inner_complete {
            match Pin::new(&mut self.inner).poll_next(context) {
                Poll::Ready(None) => self.inner_complete = true,
                poll => return poll,
            }
        }
        let Some(task) = self.task.as_mut() else {
            return Poll::Ready(None);
        };
        match task.as_mut().poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                self.task = None;
                Poll::Ready(None)
            }
        }
    }
}

fn after_body_delivery<F>(response: Response, task: F) -> Response
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let (parts, body) = response.into_parts();
    let body = Body::from_stream(DeliveryConfirmedBody {
        inner: body.into_data_stream(),
        task: Some(Box::pin(task)),
        inner_complete: false,
    });
    Response::from_parts(parts, body)
}

fn terminal_payload_delivered(egress: ProtocolId, payload: &str) -> bool {
    match egress.protocol {
        Protocol::OpenAICompatible | Protocol::OpenResponses => {
            payload.lines().any(|line| line.trim() == "data: [DONE]")
        }
        Protocol::AnthropicMessages => {
            payload
                .lines()
                .any(|line| line.trim() == "event: message_stop")
                || payload.contains(r#""type":"message_stop""#)
        }
        Protocol::GoogleGemini => payload.contains(r#""finishReason":"#),
        Protocol::BedrockConverse => false,
        Protocol::CohereChat => payload.contains(r#""type":"message-end""#),
        Protocol::WatsonxTextChat => payload.contains(r#""finish_reason":"#),
        Protocol::GatewayLanguageModel => payload.contains(r#""type":"finish""#),
    }
}

async fn send_event(sink: &mut LiveStreamSink, event: String) -> DeliveryProgress {
    if sink.cancellation.is_cancelled() {
        return DeliveryProgress::Cancelled;
    }
    let captured_event = sink.captured.is_some().then(|| event.clone());
    let result = tokio::select! {
        biased;
        _ = sink.cancellation.cancelled() => DeliveryProgress::Cancelled,
        result = sink.tx.send(Ok(event)) => {
            if result.is_ok() {
                DeliveryProgress::Sent
            } else {
                DeliveryProgress::ReceiverClosed
            }
        }
    };
    if result != DeliveryProgress::Sent {
        return result;
    }
    if let Some(preflight) = sink.preflight.take() {
        if preflight.send(Ok(())).is_err() {
            return DeliveryProgress::ReceiverClosed;
        }
        let Some(commit) = sink.commit.take() else {
            return DeliveryProgress::ReceiverClosed;
        };
        let committed = tokio::select! {
            biased;
            _ = sink.cancellation.cancelled() => DeliveryProgress::Cancelled,
            _ = sink.tx.closed() => DeliveryProgress::ReceiverClosed,
            result = commit => {
                if result.is_ok() {
                    sink.committed = true;
                    DeliveryProgress::Sent
                } else {
                    DeliveryProgress::ReceiverClosed
                }
            }
        };
        if committed != DeliveryProgress::Sent {
            return committed;
        }
    }
    if let (Some(parts), Some(event)) = (&mut sink.captured, captured_event) {
        parts.push(event);
    }
    DeliveryProgress::Sent
}

pub(super) fn streaming_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .expect("valid streaming response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    };

    type DeliveryReceiver = tokio::sync::mpsc::Receiver<Result<String, Infallible>>;
    type DeliveryPreflight = tokio::sync::oneshot::Receiver<Result<(), RoundOutcome>>;
    type DeliveryCommit = tokio::sync::oneshot::Sender<()>;
    type LiveDelivery = (
        DeliveryAdapter,
        DeliveryReceiver,
        DeliveryPreflight,
        DeliveryCommit,
    );

    fn live_delivery(cancellation: CancellationToken, capture_payload: bool) -> LiveDelivery {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (preflight_tx, preflight_rx) = tokio::sync::oneshot::channel();
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel();
        let (_terminal_delivery_tx, terminal_delivery_rx) = tokio::sync::oneshot::channel();
        (
            DeliveryAdapter::live_stream(LiveStreamRequest {
                ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                egress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                tx,
                cancellation,
                preflight: preflight_tx,
                terminal_delivery: terminal_delivery_rx,
                commit: commit_rx,
                capture_payload,
            }),
            rx,
            preflight_rx,
            commit_tx,
        )
    }

    #[test]
    fn non_stream_delivery_enforces_the_selected_protocol_pair() {
        let mut response = AiResponse::new("response", "model");
        response.vendor.passthrough_safe.insert(
            "provider_metadata".into(),
            serde_json::json!({"trace": "opaque"}),
        );
        let mut delivery = DeliveryAdapter::non_stream(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
        );

        let delivered = delivery.deliver_canonical(&response, StatusCode::OK);

        assert_eq!(
            delivered.response.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert!(delivered.body.contains("STRAVIA_PROTOCOL_LOSSY_REJECTED"));
        assert!(
            delivered
                .body
                .contains("vendor.passthrough_safe.provider_metadata")
        );
    }

    #[tokio::test]
    async fn live_stream_delivery_enforces_the_selected_protocol_pair() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (preflight_tx, _preflight_rx) = tokio::sync::oneshot::channel();
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel();
        let (_terminal_delivery_tx, terminal_delivery_rx) = tokio::sync::oneshot::channel();
        commit_tx.send(()).expect("commit receiver");
        let mut delivery = DeliveryAdapter::live_stream(LiveStreamRequest {
            ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress: ANTHROPIC_MESSAGES_2023_06_01,
            tx,
            cancellation: CancellationToken::new(),
            preflight: preflight_tx,
            terminal_delivery: terminal_delivery_rx,
            commit: commit_rx,
            capture_payload: false,
        });

        let progress = delivery
            .send_deltas(&[AiStreamDelta::Unknown {
                raw: "event: provider_metadata".into(),
            }])
            .await;

        assert_eq!(progress, DeliveryProgress::ProtocolFailed);
        let event = rx
            .recv()
            .await
            .expect("protocol error event")
            .expect("infallible protocol event");
        assert!(event.contains("STRAVIA_PROTOCOL_LOSSY_REJECTED"));
        assert!(event.contains("deltas[0].unknown"));
    }

    #[tokio::test]
    async fn receiver_close_stops_delivery_before_commit() {
        let (mut delivery, receiver, _preflight, _commit) =
            live_delivery(CancellationToken::new(), false);
        drop(receiver);

        let progress = delivery
            .send_deltas(&[AiStreamDelta::TextDelta("unobserved".into())])
            .await;

        assert_eq!(progress, DeliveryProgress::ReceiverClosed);
    }

    #[tokio::test]
    async fn cancellation_wins_before_delivery_commit() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (mut delivery, _receiver, _preflight, _commit) = live_delivery(cancellation, false);

        let progress = delivery
            .send_deltas(&[AiStreamDelta::TextDelta("cancelled".into())])
            .await;

        assert_eq!(progress, DeliveryProgress::Cancelled);
    }

    #[tokio::test]
    async fn first_sent_frame_commits_and_payload_capture_matches_wire() {
        let (mut delivery, mut receiver, preflight, commit) =
            live_delivery(CancellationToken::new(), true);
        commit.send(()).expect("commit receiver");

        let progress = delivery
            .send_deltas(&[AiStreamDelta::TextDelta("visible".into())])
            .await;

        assert_eq!(progress, DeliveryProgress::Sent);
        assert!(preflight.await.expect("preflight sender").is_ok());
        let event = receiver
            .recv()
            .await
            .expect("first wire event")
            .expect("infallible wire event");
        let captured = delivery.captured_body().expect("captured payload");
        assert!(captured.starts_with(&event));
        assert!(captured.contains("visible"));
    }

    #[tokio::test]
    async fn open_responses_post_commit_loss_has_failed_terminal_sequence() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (preflight_tx, _preflight_rx) = tokio::sync::oneshot::channel();
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel();
        let (_terminal_delivery_tx, terminal_delivery_rx) = tokio::sync::oneshot::channel();
        commit_tx.send(()).expect("commit receiver");
        let mut delivery = DeliveryAdapter::live_stream(LiveStreamRequest {
            ingress: OPEN_RESPONSES_2026_04_24,
            egress: ANTHROPIC_MESSAGES_2023_06_01,
            tx,
            cancellation: CancellationToken::new(),
            preflight: preflight_tx,
            terminal_delivery: terminal_delivery_rx,
            commit: commit_rx,
            capture_payload: false,
        });

        let progress = delivery
            .send_deltas(&[AiStreamDelta::Unknown {
                raw: "event: provider_metadata".into(),
            }])
            .await;
        let mut wire = String::new();
        while let Ok(event) = rx.try_recv() {
            wire.push_str(&event.expect("infallible protocol event"));
        }

        assert_eq!(progress, DeliveryProgress::ProtocolFailed);
        assert!(wire.contains("event: error"));
        assert!(wire.contains("event: response.failed"));
        assert!(wire.contains("data: [DONE]"));
    }
    #[tokio::test]
    async fn buffered_delivery_task_waits_for_the_complete_body() {
        let (delivered, mut delivered_rx) = tokio::sync::oneshot::channel();
        let response =
            after_body_delivery((StatusCode::OK, "payload").into_response(), async move {
                let _ = delivered.send(());
            });

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");

        assert_eq!(body, "payload");
        assert_eq!(delivered_rx.try_recv(), Ok(()));
    }

    #[tokio::test]
    async fn dropping_a_buffered_body_discards_its_delivery_task() {
        let (delivered, mut delivered_rx) = tokio::sync::oneshot::channel();
        let response =
            after_body_delivery((StatusCode::OK, "payload").into_response(), async move {
                let _ = delivered.send(());
            });

        drop(response);
        tokio::task::yield_now().await;

        assert!(matches!(
            delivered_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn terminal_delivery_detection_covers_every_sse_egress() {
        assert!(terminal_payload_delivered(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "data: [DONE]\n\n"
        ));
        assert!(terminal_payload_delivered(
            OPEN_RESPONSES_2026_04_24,
            "data: [DONE]\n\n"
        ));
        assert!(terminal_payload_delivered(
            ANTHROPIC_MESSAGES_2023_06_01,
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ));
        assert!(terminal_payload_delivered(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n"
        ));
    }
}
