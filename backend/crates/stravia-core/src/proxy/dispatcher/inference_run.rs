use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

mod engine;

use axum::http::HeaderMap;
use axum::response::Response;
use futures::Stream;

use crate::Gateway;
use crate::interaction_observation::{IngressStart, RunEvent, RunObserver, RunOutcome};
use crate::proxy::context::RequestContext;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::RawEnvelope;
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
            if self.terminal.has_pending_inline_publications() {
                self.terminal.receive_native_event(&self.observer, &value);
            }
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
            protocol: stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
                .to_string(),
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
        self.terminal
            .finish_delivery_associations(&self.observer, delivered);
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
    pub client_input: std::sync::Arc<Vec<stravia_runtime_contract::protocol::ir::AiItem>>,
    pub client_output: Option<Vec<stravia_runtime_contract::protocol::ir::AiItem>>,
    pub compaction: crate::compaction::Compaction,
    pub principal: stravia_runtime_contract::Principal,
    pub compaction_records: crate::model_turn::CompactionPublications,
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
    unary_native_items: Vec<serde_json::Value>,
    committed: bool,
    finished: bool,
}

impl ObservedDeliveryStream {
    fn receive_body_chunk(&mut self, bytes: &[u8]) {
        if self.status_code >= 400 || !self.terminal.has_pending_inline_publications() {
            return;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return;
        };
        if self.transport != "sse" {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                self.unary_native_items
                    .extend(native_delivery_items(&value));
            }
            return;
        }
        // DeliveryAdapter emits complete SseEvent frames, including for buffered
        // delivery. Inspect those existing boundaries, never accumulate a turn or
        // parse ordinary deltas. Only a complete frame returned to the HTTP body
        // consumer constitutes the existing HTTP delivery receipt.
        for frame in text.split_inclusive("\n\n") {
            if !frame.ends_with("\n\n")
                || !(frame.starts_with("event: response.output_item.added\n")
                    || frame.starts_with("event: response.output_item.done\n")
                    || frame.starts_with("event: response.completed\n")
                    || frame.starts_with("event: response.incomplete\n")
                    || frame.starts_with("event: response.failed\n"))
            {
                continue;
            }
            for data in frame.lines().filter_map(|line| line.strip_prefix("data: ")) {
                if data.contains("\"compaction\"")
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(data)
                {
                    self.terminal.receive_native_event(&self.observer, &value);
                }
            }
        }
    }

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

fn native_delivery_items(value: &serde_json::Value) -> Vec<serde_json::Value> {
    let native = |item: &&serde_json::Value| {
        item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("response.output_item.added" | "response.output_item.done") => value
            .get("item")
            .filter(native)
            .cloned()
            .into_iter()
            .collect(),
        Some("response.completed" | "response.incomplete" | "response.failed") => value
            .pointer("/response/output")
            .and_then(serde_json::Value::as_array)
            .map(|items| items.iter().filter(native).cloned().collect())
            .unwrap_or_default(),
        None => value
            .get("output")
            .and_then(serde_json::Value::as_array)
            .map(|items| items.iter().filter(native).cloned().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn compaction_delivery_event(
    record: &crate::model_turn::CompactionPublication,
    phase: crate::interaction_observation::CompactionPhase,
    error_code: Option<String>,
) -> RunEvent {
    RunEvent::CompactionOperation {
        operation_id: record.operation_id.clone(),
        model_turn_id: record.model_turn_id.clone(),
        attempt_id: None,
        mode: record.mode.clone(),
        phase,
        source_generation_id: record.source_generation_id.clone(),
        source_operation_id: None,
        registration_id: Some(record.record_id.clone()),
        duration_ms: None,
        error_code,
    }
}

impl RunTerminalContext {
    fn stage_client_output(
        &mut self,
        ingress: stravia_runtime_contract::protocol::ids::ProtocolId,
        response: &stravia_runtime_contract::protocol::ir::AiResponse,
    ) {
        // 诊断比较客户端实际回放的 ingress 形态，不比较交付前的 canonical 分块。
        let prefix = if ingress
            == stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
        {
            std::sync::Arc::make_mut(&mut self.client_input).as_mut_slice()
        } else {
            &mut []
        };
        self.client_output =
            match crate::generation_chain::project_client_history(ingress, response, prefix) {
                Ok(output) => Some(output),
                Err(error) => {
                    tracing::warn!(%error, "diagnostic client history projection failed");
                    None
                }
            };
    }

    fn has_pending_inline_publications(&self) -> bool {
        self.compaction_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|record| {
                record.receipt == crate::model_turn::CompactionReceipt::Pending
                    && matches!(
                        record.mode,
                        crate::interaction_observation::CompactionMode::Inline
                    )
            })
    }

    fn receive_native_items(
        &self,
        items: &[serde_json::Value],
        standalone_window_delivered: bool,
    ) -> Vec<crate::model_turn::CompactionPublication> {
        let mut records = self
            .compaction_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        records
            .iter_mut()
            .filter_map(|record| {
                if record.receipt != crate::model_turn::CompactionReceipt::Pending {
                    return None;
                }
                let received = match record.mode {
                    crate::interaction_observation::CompactionMode::Standalone => {
                        standalone_window_delivered
                    }
                    crate::interaction_observation::CompactionMode::Inline => {
                        stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(
                            &record.state,
                        )
                        .is_some_and(|state| items.contains(&state))
                    }
                };
                if !received {
                    return None;
                }
                record.receipt = crate::model_turn::CompactionReceipt::Delivered;
                Some(record.clone())
            })
            .collect()
    }

    fn confirm_native_receipts(
        &self,
        observer: &RunObserver,
        records: Vec<crate::model_turn::CompactionPublication>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if records.is_empty() {
            return None;
        }
        // Publication is a delivery fact, independent of the enclosing Generation.
        for record in &records {
            observer.record(compaction_delivery_event(
                record,
                crate::interaction_observation::CompactionPhase::Published,
                None,
            ));
        }
        let compaction = self.compaction.clone();
        let principal = self.principal.clone();
        let observer = observer.clone();
        Some(tokio::spawn(async move {
            let ids = records
                .iter()
                .map(|record| record.record_id.clone())
                .collect::<Vec<_>>();
            if let Err(error) = compaction.confirm_delivery(&principal, &ids).await {
                for record in &records {
                    observer.record(compaction_delivery_event(
                        record,
                        crate::interaction_observation::CompactionPhase::DeliveryUnconfirmed,
                        Some(error.code().to_owned()),
                    ));
                }
                tracing::warn!(
                    code = error.code(),
                    "Compaction delivery confirmation failed; durable registration remains pending"
                );
            }
        }))
    }

    fn receive_native_event(&self, observer: &RunObserver, value: &serde_json::Value) {
        let items = native_delivery_items(value);
        let receipts = self.receive_native_items(&items, false);
        let _ = self.confirm_native_receipts(observer, receipts);
    }

    fn finish_delivery_associations(&self, observer: &RunObserver, delivered: bool) {
        if delivered
            && self
                .generation_committed
                .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Some(output) = &self.client_output {
                observer.observe_client_completion(&self.client_input, output);
            } else {
                observer.record(RunEvent::ObservationGap {
                    reason: "client_history_projection_unavailable".into(),
                });
            }
        }
        let records = self
            .compaction_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for record in records
            .iter()
            .filter(|record| record.receipt == crate::model_turn::CompactionReceipt::Pending)
        {
            observer.record(compaction_delivery_event(
                record,
                crate::interaction_observation::CompactionPhase::DeliveryUnconfirmed,
                None,
            ));
        }
    }

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
        self.finish_delivery_associations(observer, delivered && status_code < 400);
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
                self.receive_body_chunk(&bytes);
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
                    if self.transport == "http" {
                        let receipts = self
                            .terminal
                            .receive_native_items(&self.unary_native_items, true);
                        let _ = self
                            .terminal
                            .confirm_native_receipts(&self.observer, receipts);
                    }
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
        unary_native_items: Vec::new(),
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

pub(super) fn execute(input: RunInput) -> impl std::future::Future<Output = Response> {
    // Keep the complete run state out of each caller's async frame, including
    // callers that poll directly rather than spawning a separately boxed task.
    Box::pin(execute_observed(input))
}

async fn execute_observed(input: RunInput) -> Response {
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
mod delivery_tests;
#[cfg(test)]
mod tests;
