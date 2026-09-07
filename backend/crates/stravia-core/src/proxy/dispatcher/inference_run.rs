use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

mod engine;

use axum::http::HeaderMap;
use axum::response::Response;
use futures::Stream;

use crate::Gateway;
use crate::interaction_observation::{IngressStart, RunEvent, RunObserver, RunOutcome};
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, RawEnvelope};
use crate::proxy::context::RequestContext;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Created,
    Request,
    Selecting,
    Calling,
    Inspecting,
    HiddenRound,
    SemanticComplete,
    AwaitingDelivery,
    Finished,
}

pub(super) struct PhaseTracker {
    current: Phase,
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self {
            current: Phase::Created,
        }
    }
}

impl PhaseTracker {
    pub(crate) fn current(&self) -> Phase {
        self.current
    }

    pub(crate) fn at(current: Phase) -> Self {
        Self { current }
    }

    pub(crate) fn transition(&mut self, next: Phase) -> Result<(), String> {
        let current = self.current;
        let valid = matches!(
            (current, next),
            (Phase::Created, Phase::Request)
                | (
                    Phase::Request,
                    Phase::Selecting | Phase::SemanticComplete | Phase::Finished
                )
                | (Phase::Selecting, Phase::Calling | Phase::Finished)
                | (
                    Phase::Calling,
                    Phase::Inspecting
                        | Phase::Selecting
                        | Phase::AwaitingDelivery
                        | Phase::Finished
                )
                | (
                    Phase::Inspecting,
                    Phase::HiddenRound
                        | Phase::SemanticComplete
                        | Phase::AwaitingDelivery
                        | Phase::Finished
                )
                | (
                    Phase::HiddenRound,
                    Phase::Selecting | Phase::SemanticComplete | Phase::Finished
                )
                | (
                    Phase::SemanticComplete,
                    Phase::AwaitingDelivery | Phase::Finished
                )
                | (Phase::AwaitingDelivery, Phase::Finished)
        );
        if !valid {
            return Err(format!(
                "invalid Inference Run phase transition: {current:?} -> {next:?}"
            ));
        }
        self.current = next;
        Ok(())
    }

    pub(crate) fn finish(&mut self) {
        self.current = Phase::Finished;
    }
}

struct Run {
    input: RunInput,
    phase: PhaseTracker,
    inference_run: Option<crate::hook::InferenceRun>,
}

impl Run {
    async fn execute(mut self) -> Response {
        if let Err(error) = self.phase.transition(Phase::Request) {
            return engine::hook_failure_response(error);
        }
        let cancellation = self.input.context.cancellation.clone();
        let deadline = tokio::time::Instant::from_std(self.input.context.deadline.at());
        let deadline_monitor = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            cancellation.cancel();
        });
        let response =
            engine::orchestrate(self.input, &mut self.inference_run, &mut self.phase).await;
        wrap_deadline_monitor(response, deadline_monitor)
    }
}

struct DeadlineLeaseStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>,
    monitor: Option<tokio::task::JoinHandle<()>>,
}

impl DeadlineLeaseStream {
    fn stop_monitor(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            monitor.abort();
        }
    }
}

impl Stream for DeadlineLeaseStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(None) => {
                self.stop_monitor();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.stop_monitor();
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }
}

impl Drop for DeadlineLeaseStream {
    fn drop(&mut self) {
        self.stop_monitor();
    }
}

fn wrap_deadline_monitor(response: Response, monitor: tokio::task::JoinHandle<()>) -> Response {
    let (parts, body) = response.into_parts();
    let stream = DeadlineLeaseStream {
        inner: Box::pin(body.into_data_stream()),
        monitor: Some(monitor),
    };
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

#[derive(Clone, Copy)]
pub(crate) struct DeferredWebSocketDelivery;

pub(crate) struct WebSocketRunDelivery {
    observer: RunObserver,
    terminal: RunTerminalContext,
    committed: bool,
    finished: bool,
}

impl WebSocketRunDelivery {
    pub(crate) fn observer(&self) -> RunObserver {
        self.observer.clone()
    }

    pub(crate) fn sent_text(&mut self, text: &str) {
        if !self.committed {
            self.committed = true;
            self.observer.record(RunEvent::ClientOutputCommitted);
        }
        self.record_wire_text(text);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(visible) = value
                .get("delta")
                .and_then(serde_json::Value::as_str)
                .filter(|_| {
                    matches!(
                        value.get("type").and_then(serde_json::Value::as_str),
                        Some("response.output_text.delta" | "response.refusal.delta")
                    )
                })
            {
                self.observer.record(RunEvent::ClientVisibleContentDelta {
                    text: visible.to_owned(),
                });
            }
            self.observer.record_debug(|| RunEvent::Checkpoint {
                stage: "client_projection_event".into(),
                model_turn_id: None,
                attempt_id: None,
                payload: value,
            });
        }
    }

    pub(crate) fn sent_error_text(&self, text: &str) {
        self.record_wire_text(text);
    }

    fn record_wire_text(&self, text: &str) {
        self.observer.record_debug(|| RunEvent::Wire {
            direction: "platform_to_client".into(),
            transport: "websocket".into(),
            protocol: crate::protocol::ids::OPEN_RESPONSES_2026_04_24.to_string(),
            message_type: "text".into(),
            model_turn_id: None,
            attempt_id: None,
            status_code: None,
            url: None,
            headers: serde_json::Value::Null,
            payload: serde_json::Value::String(text.to_owned()),
        });
    }

    pub(crate) fn finish(&mut self, status: &str, reason: Option<String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.observer.record_debug(|| RunEvent::Checkpoint {
            stage: "delivery_terminal".into(),
            model_turn_id: None,
            attempt_id: None,
            payload: serde_json::json!({ "status": status, "reason": reason.clone() }),
        });
        self.observer.record(RunEvent::DeliveryFinished {
            status: status.to_owned(),
            reason: reason.clone(),
        });
        let delivered = status == "delivered";
        self.observer.finish(RunOutcome {
            status: if delivered {
                if self.terminal.waiting_client {
                    "waiting_client"
                } else {
                    "completed"
                }
            } else if status == "cancelled" {
                "cancelled"
            } else {
                "failed"
            }
            .into(),
            terminal_reason: reason,
            generation_node_id: (delivered
                && self
                    .terminal
                    .generation_committed
                    .load(std::sync::atomic::Ordering::Acquire))
            .then(|| self.terminal.generation_node_id.clone())
            .flatten(),
            generation_root_id: (delivered
                && self
                    .terminal
                    .generation_committed
                    .load(std::sync::atomic::Ordering::Acquire))
            .then(|| self.terminal.generation_root_id.clone())
            .flatten(),
        });
    }
}

impl Drop for WebSocketRunDelivery {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("cancelled", Some("websocket_delivery_dropped".into()));
        }
    }
}

#[derive(Clone)]
pub(super) struct RunTerminalContext {
    pub generation_node_id: Option<String>,
    pub generation_root_id: Option<String>,
    pub generation_committed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub waiting_client: bool,
    pub visible_text: Vec<String>,
}

pub(super) struct StreamDeliveryCompletion(
    tokio::sync::oneshot::Receiver<Option<RunTerminalContext>>,
);

struct ObservedDeliveryStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>,
    observer: RunObserver,
    protocol: String,
    transport: &'static str,
    status_code: u16,
    terminal: RunTerminalContext,
    stream_completion: Option<StreamDeliveryCompletion>,
    committed: bool,
    finished: bool,
}

impl ObservedDeliveryStream {
    fn finish(&mut self, delivery_status: &'static str, reason: Option<String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(mut completion) = self.stream_completion.take() else {
            self.terminal.finish_http_delivery(
                &self.observer,
                self.status_code,
                delivery_status,
                reason,
            );
            return;
        };
        let observer = self.observer.clone();
        let terminal = self.terminal.clone();
        let status_code = self.status_code;
        // HTTP body 的 Drop/EOF 可能早于生成链落盘；协议终态与落盘结果由生产任务裁决。
        let finish = move |result: Result<Option<RunTerminalContext>, ()>| match result {
            Ok(Some(terminal)) => {
                terminal.finish_http_delivery(&observer, status_code, "delivered", None);
            }
            Ok(None) if delivery_status != "delivered" => {
                terminal.finish_http_delivery(&observer, status_code, delivery_status, reason);
            }
            Ok(None) => terminal.finish_http_delivery(
                &observer,
                status_code,
                "delivery_failed",
                Some("stream_incomplete".into()),
            ),
            Err(()) => terminal.finish_http_delivery(
                &observer,
                status_code,
                "delivery_failed",
                Some("stream_task_aborted".into()),
            ),
        };
        match completion.0.try_recv() {
            Ok(result) => finish(Ok(result)),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => finish(Err(())),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                tokio::spawn(async move {
                    finish(completion.0.await.map_err(|_| ()));
                });
            }
        }
    }
}

impl RunTerminalContext {
    fn finish_http_delivery(
        &self,
        observer: &RunObserver,
        status_code: u16,
        delivery_status: &str,
        reason: Option<String>,
    ) {
        observer.record_debug(|| RunEvent::Checkpoint {
            stage: "delivery_terminal".into(),
            model_turn_id: None,
            attempt_id: None,
            payload: serde_json::json!({
                "status": delivery_status,
                "reason": reason.clone(),
                "http_status": status_code,
            }),
        });
        observer.record(RunEvent::DeliveryFinished {
            status: delivery_status.to_owned(),
            reason: reason.clone(),
        });
        let status = if delivery_status == "delivered" {
            if self.waiting_client {
                "waiting_client"
            } else {
                "completed"
            }
        } else if delivery_status == "cancelled" {
            "cancelled"
        } else {
            "failed"
        };
        let delivered = delivery_status == "delivered";
        observer.finish(RunOutcome {
            status: status.to_owned(),
            terminal_reason: reason,
            generation_node_id: (delivered
                && self
                    .generation_committed
                    .load(std::sync::atomic::Ordering::Acquire))
            .then(|| self.generation_node_id.clone())
            .flatten(),
            generation_root_id: (delivered
                && self
                    .generation_committed
                    .load(std::sync::atomic::Ordering::Acquire))
            .then(|| self.generation_root_id.clone())
            .flatten(),
        });
    }
}

impl Stream for ObservedDeliveryStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(bytes))) => {
                if !self.committed && self.status_code < 400 {
                    self.committed = true;
                    self.observer.record(RunEvent::ClientOutputCommitted);
                    for text in std::mem::take(&mut self.terminal.visible_text) {
                        self.observer
                            .record(RunEvent::ClientVisibleContentDelta { text });
                    }
                }
                self.observer.record_debug(|| RunEvent::Wire {
                    direction: "platform_to_client".into(),
                    transport: self.transport.into(),
                    protocol: self.protocol.clone(),
                    message_type: "body_chunk".into(),
                    model_turn_id: None,
                    attempt_id: None,
                    status_code: Some(self.status_code),
                    url: None,
                    headers: serde_json::Value::Null,
                    payload: serde_json::Value::String(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    ),
                });
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish("delivery_failed", Some(error.to_string()));
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if self.status_code < 400 {
                    self.finish("delivered", None);
                } else {
                    let reason = format!("http_status_{}", self.status_code);
                    self.finish("delivery_failed", Some(reason));
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ObservedDeliveryStream {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("cancelled", Some("client_disconnected".into()));
        }
    }
}

fn wrap_observed_delivery(
    response: Response,
    observer: RunObserver,
    protocol: String,
    terminal: RunTerminalContext,
    stream_completion: Option<StreamDeliveryCompletion>,
) -> Response {
    let status_code = response.status().as_u16();
    observer.record_debug(|| {
        let headers = serde_json::Value::Object(
            response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value.to_str().ok().map(|value| {
                        (
                            name.as_str().to_owned(),
                            serde_json::Value::String(value.to_owned()),
                        )
                    })
                })
                .collect(),
        );
        RunEvent::Wire {
            direction: "platform_to_client".into(),
            transport: "http".into(),
            protocol: protocol.clone(),
            message_type: "response_head".into(),
            model_turn_id: None,
            attempt_id: None,
            status_code: Some(status_code),
            url: None,
            headers,
            payload: serde_json::Value::Null,
        }
    });
    let transport = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("text/event-stream"))
        .map_or("http", |_| "sse");
    let (parts, body) = response.into_parts();
    let stream = ObservedDeliveryStream {
        inner: Box::pin(body.into_data_stream()),
        observer,
        protocol,
        transport,
        status_code,
        terminal,
        stream_completion,
        committed: false,
        finished: false,
    };
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

pub(super) struct RunInput {
    pub gateway: Gateway,
    pub executor: std::sync::Arc<dyn crate::agent::ModelTurnExecutor>,
    pub headers: HeaderMap,
    pub envelope: RawEnvelope,
    pub request: AiRequest,
    pub ingress: ProtocolId,
    pub context: RequestContext,
}

pub(super) async fn execute(input: RunInput) -> Response {
    let extensions = input.context.extensions.clone();
    let protocol = input.ingress.to_string();
    if !extensions.contains::<crate::interaction_observation::IngressObserver>() {
        extensions.insert(input.gateway.observation.observe_ingress(IngressStart {
            id: input.context.request_id.clone(),
            method: input.envelope.method.clone(),
            path: input.envelope.path.clone(),
            protocol: protocol.clone(),
        }));
    }
    let response = Run {
        input,
        phase: PhaseTracker::default(),
        inference_run: None,
    }
    .execute()
    .await;
    match (
        extensions.get::<RunObserver>(),
        extensions.get::<RunTerminalContext>(),
    ) {
        (Some(observer), Some(terminal)) if extensions.contains::<DeferredWebSocketDelivery>() => {
            extensions.insert(WebSocketRunDelivery {
                observer,
                terminal,
                committed: false,
                finished: false,
            });
            response
        }
        (Some(observer), Some(terminal)) => wrap_observed_delivery(
            response,
            observer,
            protocol,
            terminal,
            extensions.take::<StreamDeliveryCompletion>(),
        ),
        _ => response,
    }
}

pub(crate) fn decode_error_response(error: impl std::fmt::Display) -> Response {
    engine::error_response(400, &error.to_string())
}

#[cfg(test)]
mod tests;
