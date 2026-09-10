mod reasoning;
mod transport_http;
mod transport_responses_websocket;

use reasoning::StreamReasoningNormalizer;
pub(crate) use transport_responses_websocket::ResponsesWebSocketBinding;
use transport_responses_websocket::{ResponsesWebSocketCall, ResponsesWebSocketStream};

use std::borrow::Cow;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use futures::{StreamExt, stream::BoxStream};
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::Gateway;
use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::interaction_observation::{ConfirmedUsage, RunEvent, RunObserver};
use crate::provider::inbound::InboundResponse;
use crate::provider::outbound::OutboundRequest;
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::proxy::client::{
    ProxyClient, ResponsesWebSocketAcquireError, ResponsesWebSocketLease,
    ResponsesWebSocketRegistry, ResponsesWebSocketRequest, ResponsesWebSocketTrace,
};
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;

pub(crate) struct ProviderCall {
    adapter: ProviderAdapter,
    client: ProxyClient,
    outbound: OutboundRequest,
    continuation_fallback: Option<OutboundRequest>,
    websocket: Option<ResponsesWebSocketCall>,
    allow_retries: bool,
}

impl ProviderCall {
    pub(crate) fn disable_retries(&mut self) {
        self.allow_retries = false;
        self.continuation_fallback = None;
    }
}

pub(crate) struct ProviderUnaryResponse {
    pub raw: Value,
    pub canonical: Result<AiResponse, GatewayError>,
    pub status: u16,
    pub headers: HeaderMap,
    pub attempt: AttemptObservation,
}

pub(crate) enum ProviderStreamResponse {
    Error {
        status: u16,
        headers: HeaderMap,
        body: anyhow::Result<Value>,
        attempt: AttemptObservation,
    },
    Stream(Box<ProviderStream>),
    Uncertain {
        message: String,
    },
}

pub(crate) struct ProviderStream {
    adapter: ProviderAdapter,
    decoder: crate::protocol::transform::StreamDecodeStage,
    source: ProviderStreamSource,
    reasoning: StreamReasoningNormalizer,
    pub status: u16,
    pub attempt: AttemptObservation,
    response_continuation_available: Arc<AtomicBool>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProviderStreamError {
    #[error("upstream stream error: {0}")]
    Transport(String),
    #[error("upstream request acceptance is uncertain: {0}")]
    Uncertain(String),
    #[error("upstream stream decode error: {0}")]
    Decode(#[from] crate::protocol::transform::TransformError),
    #[error("upstream stream normalization error: {0}")]
    Normalize(#[source] GatewayError),
}

enum ProviderStreamSource {
    Http(BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>),
    ResponsesWebSocket(Box<ResponsesWebSocketStream>),
}

pub(crate) struct ProviderStreamChunk {
    pub deltas: Vec<AiStreamDelta>,
}

/// Runtime-selected provider mechanics for one Target binding.
///
/// Route selection, retry policy, Hooks, tools, and persistence remain owned by
/// the Inference Run. This adapter owns the Vendor codec context needed to turn
/// canonical requests into upstream wire and upstream wire into canonical IR.
#[derive(Clone)]
pub(crate) struct ProviderAdapter {
    vendor: Arc<dyn Vendor>,
    binding: ProviderBinding,
    normalizes_raw_stream_chunks: bool,
}

#[derive(Clone)]
pub(crate) struct ProviderBinding {
    pub(crate) provider: Provider,
    pub(crate) protocol: ProtocolId,
    pub(crate) egress_base_url: String,
    pub(crate) api_key: String,
    pub(crate) actual_model: String,
    pub(crate) gateway: Gateway,
    pub(crate) disable_default_auth: bool,
    pub(crate) observer: Option<RunObserver>,
    pub(crate) model_turn_id: String,
    pub(crate) target_id: String,
    pub(crate) provider_name: String,
}

pub(crate) struct AttemptObservation {
    observer: Option<RunObserver>,
    pub(crate) id: String,
    model_turn_id: String,
    transport: String,
    protocol: String,
    url: String,
    started_at: Instant,
    finished: AtomicBool,
    usage_confirmed: AtomicBool,
    thinking_active: AtomicBool,
}

impl AttemptObservation {
    fn new(adapter: &ProviderAdapter, transport: &str, url: &str) -> Self {
        let binding = &adapter.binding;
        let id = binding
            .observer
            .as_ref()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .unwrap_or_default();
        if let Some(observer) = &binding.observer {
            observer.record(RunEvent::TargetAttemptStarted {
                model_turn_id: binding.model_turn_id.clone(),
                attempt_id: id.clone(),
                target_id: binding.target_id.clone(),
                provider_id: binding.provider.id.clone(),
                provider_name: binding.provider_name.clone(),
                upstream_model: binding.actual_model.clone(),
                protocol: binding.protocol.to_string(),
                upstream_url: url.to_owned(),
            });
        }
        let observed = binding.observer.is_some();
        let attempt = Self {
            observer: binding.observer.clone(),
            id,
            model_turn_id: observed
                .then(|| binding.model_turn_id.clone())
                .unwrap_or_default(),
            transport: observed.then(|| transport.to_owned()).unwrap_or_default(),
            protocol: observed
                .then(|| binding.protocol.to_string())
                .unwrap_or_default(),
            url: observed.then(|| url.to_owned()).unwrap_or_default(),
            started_at: Instant::now(),
            finished: AtomicBool::new(false),
            usage_confirmed: AtomicBool::new(false),
            thinking_active: AtomicBool::new(false),
        };
        attempt
    }

    pub(crate) fn debug_enabled(&self) -> bool {
        self.observer
            .as_ref()
            .is_some_and(RunObserver::debug_enabled)
    }

    pub(crate) fn wire_lazy(
        &self,
        direction: &str,
        message_type: &str,
        status_code: Option<u16>,
        headers: Option<&HeaderMap>,
        payload: impl FnOnce() -> Value,
    ) {
        if self.debug_enabled() {
            self.wire(direction, message_type, status_code, headers, payload());
        }
    }

    pub(crate) fn wire(
        &self,
        direction: &str,
        message_type: &str,
        status_code: Option<u16>,
        headers: Option<&HeaderMap>,
        payload: Value,
    ) {
        let Some(observer) = self
            .observer
            .as_ref()
            .filter(|observer| observer.debug_enabled())
        else {
            return;
        };
        observer.record(RunEvent::Wire {
            direction: direction.to_owned(),
            transport: self.transport.clone(),
            protocol: self.protocol.clone(),
            message_type: message_type.to_owned(),
            model_turn_id: Some(self.model_turn_id.clone()),
            attempt_id: Some(self.id.clone()),
            status_code,
            url: Some(self.url.clone()),
            headers: headers.map(headers_value).unwrap_or(Value::Null),
            payload,
        });
    }

    pub(crate) fn checkpoint<T: serde::Serialize>(&self, stage: &str, payload: &T) {
        let Some(observer) = self
            .observer
            .as_ref()
            .filter(|observer| observer.debug_enabled())
        else {
            return;
        };
        match serde_json::to_value(payload) {
            Ok(payload) => observer.record(RunEvent::Checkpoint {
                stage: stage.to_owned(),
                model_turn_id: Some(self.model_turn_id.clone()),
                attempt_id: Some(self.id.clone()),
                payload,
            }),
            Err(_) => observer.record(RunEvent::ObservationGap {
                reason: format!("{stage}_serialization_failed"),
            }),
        }
    }

    pub(crate) fn observe_delta(&self, delta: &AiStreamDelta) {
        let Some(observer) = &self.observer else {
            return;
        };
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        match delta {
            AiStreamDelta::ThinkingDelta(text)
            | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
            | AiStreamDelta::ReasoningSummaryDelta { text, .. }
                if !text.is_empty() =>
            {
                self.thinking_active.store(true, Ordering::Release);
                observer.record(RunEvent::ModelThinkingDelta {
                    model_turn_id: self.model_turn_id.clone(),
                    attempt_id: self.id.clone(),
                    text: text.clone(),
                });
            }
            AiStreamDelta::TextDelta(text)
            | AiStreamDelta::TextDeltaWithMetadata { text, .. }
            | AiStreamDelta::RefusalDelta(text)
            | AiStreamDelta::RefusalDeltaWithIndex { text, .. }
                if !text.is_empty() =>
            {
                self.finish_thinking();
            }
            AiStreamDelta::ToolCallStart { .. }
            | AiStreamDelta::ToolCallDelta { .. }
            | AiStreamDelta::ToolCallComplete { .. }
            | AiStreamDelta::Done { .. }
            | AiStreamDelta::StreamError { .. }
            | AiStreamDelta::UnexpectedEof => self.finish_thinking(),
            // Item snapshots, usage, metadata and protected state are not readable deltas.
            _ => {}
        }
    }

    fn finish_thinking(&self) {
        if !self.thinking_active.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::ModelThinkingFinished {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
            });
        }
    }

    pub(crate) fn gap(&self, reason: &str) {
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::ObservationGap {
                reason: reason.to_owned(),
            });
        }
    }

    pub(crate) fn confirm_usage(&self, usage: &stravia_runtime_contract::protocol::ir::Usage) {
        if self.usage_confirmed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::UsageConfirmed {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
                usage: confirmed_usage(usage),
            });
        }
    }

    pub(crate) fn finish(
        &self,
        status: &str,
        status_code: Option<u16>,
        error_code: Option<String>,
        first_token_ms: Option<i64>,
    ) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        self.finish_thinking();
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::TargetAttemptFinished {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
                status: status.to_owned(),
                status_code,
                error_code,
                duration_ms: self.started_at.elapsed().as_millis() as i64,
                first_token_ms,
            });
        }
    }
}

impl Drop for AttemptObservation {
    fn drop(&mut self) {
        self.finish("failed", None, Some("attempt_aborted".into()), None);
    }
}

fn headers_value(headers: &HeaderMap) -> Value {
    let mut values = serde_json::Map::new();
    for (name, value) in headers {
        let value = value
            .to_str()
            .map(|value| Value::String(value.to_owned()))
            .unwrap_or_else(|_| bytes_value(value.as_bytes()));
        match values.entry(name.as_str().to_owned()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(value);
            }
            serde_json::map::Entry::Occupied(mut entry) => match entry.get_mut() {
                Value::Array(existing) => existing.push(value),
                existing => {
                    let first = std::mem::replace(existing, Value::Null);
                    *existing = Value::Array(vec![first, value]);
                }
            },
        }
    }
    Value::Object(values)
}

fn bytes_value(bytes: &[u8]) -> Value {
    std::str::from_utf8(bytes)
        .map(|text| Value::String(text.to_owned()))
        .unwrap_or_else(|_| {
            serde_json::json!({
                "encoding": "base64",
                "data": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    bytes,
                ),
            })
        })
}

fn confirmed_usage(usage: &stravia_runtime_contract::protocol::ir::Usage) -> ConfirmedUsage {
    ConfirmedUsage {
        input_tokens: usage
            .required_components_known
            .then_some(i64::from(usage.prompt_tokens)),
        output_tokens: usage
            .required_components_known
            .then_some(i64::from(usage.completion_tokens)),
        cache_read_tokens: usage.cache_read_tokens.map(i64::from),
        cache_write_tokens: usage.cache_creation_tokens.map(i64::from),
        reasoning_tokens: usage.reasoning_tokens.map(i64::from),
    }
}

impl ProviderAdapter {
    pub(crate) fn new(vendor: Arc<dyn Vendor>, binding: ProviderBinding) -> Self {
        let mut adapter = Self {
            vendor,
            binding,
            normalizes_raw_stream_chunks: false,
        };
        adapter.normalizes_raw_stream_chunks =
            crate::provider::common::pipeline::normalizes_stream_raw_chunks(
                adapter.vendor.as_ref(),
                &adapter.provider_context(),
            );
        adapter
    }
    pub(crate) fn binding(&self) -> &ProviderBinding {
        &self.binding
    }

    fn begin_attempt(
        &self,
        transport: &str,
        url: &str,
        headers: &HeaderMap,
        body: impl FnOnce() -> Value,
    ) -> AttemptObservation {
        self.begin_attempt_with_message(transport, url, "request", headers, body)
    }

    fn begin_attempt_with_message(
        &self,
        transport: &str,
        url: &str,
        message_type: &str,
        headers: &HeaderMap,
        body: impl FnOnce() -> Value,
    ) -> AttemptObservation {
        let attempt = AttemptObservation::new(self, transport, url);
        attempt.wire_lazy("upstream_request", message_type, None, Some(headers), body);
        attempt
    }

    pub(crate) fn bind(self, client: ProxyClient, outbound: OutboundRequest) -> ProviderCall {
        ProviderCall {
            adapter: self,
            client,
            outbound,
            continuation_fallback: None,
            websocket: None,
            allow_retries: true,
        }
    }

    pub(crate) fn bind_with_continuation_fallback(
        self,
        client: ProxyClient,
        outbound: OutboundRequest,
        full_outbound: OutboundRequest,
    ) -> ProviderCall {
        ProviderCall {
            adapter: self,
            client,
            outbound,
            continuation_fallback: Some(full_outbound),
            websocket: None,
            allow_retries: true,
        }
    }

    pub(crate) async fn build_request(
        &self,
        request: &mut AiRequest,
    ) -> Result<OutboundRequest, GatewayError> {
        self.vendor
            .build_request(request, &self.provider_context())
            .await
    }

    pub(crate) async fn build_compact_request(
        &self,
        request: &mut AiRequest,
    ) -> Result<OutboundRequest, GatewayError> {
        self.vendor
            .build_compact_request(request, &self.provider_context())
            .await
    }

    async fn refresh_auth_on_unauthorized(
        &self,
        outbound: &mut OutboundRequest,
    ) -> Result<bool, GatewayError> {
        self.vendor
            .refresh_auth_on_unauthorized(&self.provider_context(), outbound)
            .await
    }

    fn is_continuation_not_found(&self, status: u16, body: &Value) -> bool {
        self.vendor.is_continuation_not_found(status, body)
    }

    pub(crate) async fn parse_response(
        &self,
        response: InboundResponse,
    ) -> Result<AiResponse, GatewayError> {
        self.vendor
            .parse_response(response, &self.provider_context())
            .await
    }

    pub(crate) async fn normalize_stream_chunk<'a>(
        &self,
        bytes: &'a [u8],
    ) -> Result<Cow<'a, [u8]>, GatewayError> {
        if !self.normalizes_raw_stream_chunks {
            return Ok(Cow::Borrowed(bytes));
        }

        let mut chunk = String::from_utf8_lossy(bytes).into_owned();
        crate::provider::common::pipeline::normalize_stream_chunk(
            self.vendor.as_ref(),
            &self.provider_context(),
            &mut chunk,
        )
        .await?;
        Ok(Cow::Owned(chunk.into_bytes()))
    }

    pub(crate) async fn normalize_stream_deltas(
        &self,
        deltas: &mut [AiStreamDelta],
    ) -> Result<(), GatewayError> {
        crate::provider::common::pipeline::normalize_stream_deltas(
            self.vendor.as_ref(),
            &self.provider_context(),
            deltas,
        )
        .await
    }

    fn provider_context(&self) -> ProviderCtx<'_> {
        ProviderCtx {
            provider: &self.binding.provider,
            protocol: self.binding.protocol,
            egress_base_url: &self.binding.egress_base_url,
            api_key: &self.binding.api_key,
            actual_model: &self.binding.actual_model,
            credential: None,
            gw: &self.binding.gateway,
            disable_default_auth: self.binding.disable_default_auth,
        }
    }
}

impl ProviderStream {
    pub(crate) fn attempt(&self) -> &AttemptObservation {
        match &self.source {
            ProviderStreamSource::ResponsesWebSocket(stream) => {
                stream.fallback_attempt.as_deref().unwrap_or(&self.attempt)
            }
            ProviderStreamSource::Http(_) => &self.attempt,
        }
    }

    pub(crate) fn response_continuation_available(&self) -> Arc<AtomicBool> {
        self.response_continuation_available.clone()
    }

    pub(crate) async fn next(
        &mut self,
    ) -> Result<Option<ProviderStreamChunk>, ProviderStreamError> {
        let adapter = &self.adapter;
        let raw = match &mut self.source {
            ProviderStreamSource::Http(bytes) => {
                let Some(raw) = bytes.next().await else {
                    return Ok(None);
                };
                raw.map_err(|error| ProviderStreamError::Transport(error.to_string()))?
            }
            ProviderStreamSource::ResponsesWebSocket(stream) => {
                let raw = stream.next_raw(adapter, &self.attempt, self.status).await?;
                if stream.using_http_fallback() {
                    self.response_continuation_available
                        .store(false, Ordering::Release);
                }
                let Some(raw) = raw else {
                    return Ok(None);
                };
                raw
            }
        };
        if matches!(&self.source, ProviderStreamSource::Http(_)) {
            self.attempt.wire_lazy(
                "upstream_response",
                "sse_chunk",
                Some(self.status),
                None,
                || bytes_value(&raw),
            );
        }
        let normalized = self
            .adapter
            .normalize_stream_chunk(&raw)
            .await
            .map_err(ProviderStreamError::Normalize)?;
        let mut deltas = self.decoder.decode_chunk(&normalized).map_err(|error| {
            tracing::debug!(
                transport = "responses_websocket",
                error = %error,
                "failed to decode upstream WebSocket event"
            );
            error
        })?;
        self.reasoning.normalize(&mut deltas, false);
        self.adapter
            .normalize_stream_deltas(&mut deltas)
            .await
            .map_err(ProviderStreamError::Normalize)?;
        Ok(Some(ProviderStreamChunk { deltas }))
    }

    pub(crate) async fn finish(&mut self) -> Result<Vec<AiStreamDelta>, ProviderStreamError> {
        let mut deltas = self.decoder.finish()?;
        self.reasoning.normalize(&mut deltas, true);
        self.adapter
            .normalize_stream_deltas(&mut deltas)
            .await
            .map_err(ProviderStreamError::Normalize)?;
        Ok(deltas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn ordinary_thinking_excludes_protected_state_and_closes_each_segment()
    -> anyhow::Result<()> {
        use crate::interaction_observation::{IngressStart, InteractionObservation, RunStart};

        let directory = tempfile::tempdir()?;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::raw_sql(include_str!(
            "../../../migrations/sqlite/0034_interaction_observation.sql"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../../migrations/sqlite/0040_interaction_input_preview.sql"
        ))
        .execute(&pool)
        .await?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            1,
            true,
        )
        .await;
        let observer = observation
            .observe_ingress(IngressStart {
                id: "ingress".into(),
                method: "POST".into(),
                path: "/responses".into(),
                protocol: "responses".into(),
            })
            .admit(RunStart {
                id: "run".into(),
                principal: "api-key:test".into(),
                api_key_id: None,
                api_key_name: None,
                generation_root_id: None,
                generation_parent_id: None,
                has_new_user: true,
                canonical_fingerprint: "thinking-test".into(),
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            });
        assert!(!observer.debug_enabled());
        observer.protect_secrets(["PLAIN_THINKING_SECRET_9"]);
        let make_attempt = |id: &str| AttemptObservation {
            observer: Some(observer.clone()),
            id: id.into(),
            model_turn_id: "turn".into(),
            transport: String::new(),
            protocol: String::new(),
            url: String::new(),
            started_at: Instant::now(),
            finished: AtomicBool::new(false),
            usage_confirmed: AtomicBool::new(false),
            thinking_active: AtomicBool::new(false),
        };
        let attempt = make_attempt("attempt");
        attempt.observe_delta(&AiStreamDelta::ThinkingDelta("readable PLAIN_THINK".into()));
        let interleaved = make_attempt("interleaved");
        interleaved.observe_delta(&AiStreamDelta::ThinkingDelta("isolated".into()));
        interleaved.finish("completed", None, None, None);
        drop(interleaved);
        attempt.observe_delta(&AiStreamDelta::ThinkingSignature(
            "protected-signature".into(),
        ));
        attempt.observe_delta(&AiStreamDelta::ProtectedThinkingStart { index: 0 });
        attempt.observe_delta(&AiStreamDelta::Usage(Default::default()));
        attempt.observe_delta(&AiStreamDelta::ResponseMetadata {
            metadata: serde_json::json!({"encrypted_content": "protected-ciphertext"}),
        });
        attempt.observe_delta(&AiStreamDelta::TextDelta(String::new()));
        attempt.observe_delta(&AiStreamDelta::ThinkingDeltaWithMetadata {
            text: "ING_SECRET_9 content".into(),
            obfuscation: Some("protected-padding".into()),
            output_index: Some(0),
            content_index: None,
        });
        attempt.observe_delta(&AiStreamDelta::ReasoningSummaryDelta {
            text: " summary".into(),
            obfuscation: Some("protected-padding".into()),
            output_index: Some(0),
            content_index: None,
        });
        attempt.observe_delta(&AiStreamDelta::TextDelta("answer".into()));
        attempt.observe_delta(&AiStreamDelta::Done {
            stop_reason: "stop".into(),
        });
        attempt.finish("completed", None, None, None);
        attempt.observe_delta(&AiStreamDelta::ThinkingDelta("too late".into()));
        drop(attempt);

        for (id, boundary) in [
            (
                "tool",
                AiStreamDelta::ToolCallStart {
                    index: 0,
                    id: "call".into(),
                    name: "tool".into(),
                },
            ),
            (
                "done",
                AiStreamDelta::Done {
                    stop_reason: "stop".into(),
                },
            ),
            ("eof", AiStreamDelta::UnexpectedEof),
            (
                "error",
                AiStreamDelta::StreamError {
                    error: crate::protocol::ir::AiError::new(
                        crate::protocol::ir::AiErrorKind::StreamMidError,
                        "unavailable",
                    ),
                },
            ),
        ] {
            let attempt = make_attempt(id);
            attempt.observe_delta(&AiStreamDelta::ThinkingDelta(id.into()));
            attempt.observe_delta(&boundary);
            drop(attempt);
        }
        let cancelled = make_attempt("cancelled");
        cancelled.observe_delta(&AiStreamDelta::ThinkingDelta("cancelled".into()));
        cancelled.finish("cancelled", None, None, None);
        drop(cancelled);
        let aborted = make_attempt("aborted");
        aborted.observe_delta(&AiStreamDelta::ThinkingDelta("aborted".into()));
        drop(aborted);
        drop(observer);
        observation.shutdown().await;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT kind, payload FROM observation_events WHERE kind LIKE 'model_thinking_%' ORDER BY sequence",
        ).fetch_all(&pool).await?;
        let events: Vec<(String, Value)> = rows
            .into_iter()
            .map(|(kind, payload)| (kind, serde_json::from_str(&payload).expect("event payload")))
            .collect();
        for (id, expected) in [
            ("attempt", "readable *** content summary"),
            ("interleaved", "isolated"),
            ("tool", "tool"),
            ("done", "done"),
            ("eof", "eof"),
            ("error", "error"),
            ("cancelled", "cancelled"),
            ("aborted", "aborted"),
        ] {
            let scoped: Vec<_> = events
                .iter()
                .filter(|(_, payload)| payload["attempt_id"] == id)
                .collect();
            assert_eq!(
                scoped.last().map(|event| event.0.as_str()),
                Some("model_thinking_finished")
            );
            assert_eq!(
                scoped
                    .iter()
                    .filter(|event| event.0 == "model_thinking_finished")
                    .count(),
                1
            );
            let text: String = scoped
                .iter()
                .filter_map(|event| event.1["text"].as_str())
                .collect();
            assert_eq!(text, expected);
            assert!(
                scoped
                    .iter()
                    .all(|event| event.1["model_turn_id"] == "turn")
            );
        }
        let serialized = serde_json::to_string(&events)?;
        for excluded in [
            "PLAIN_THINKING_SECRET_9",
            "protected-signature",
            "protected-ciphertext",
            "protected-padding",
            "answer",
            "too late",
        ] {
            assert!(
                !serialized.contains(excluded),
                "unexpected captured content: {excluded}"
            );
        }
        pool.close().await;
        Ok(())
    }

    #[test]
    fn stream_reasoning_normalization_handles_split_tags() {
        let mut normalizer = StreamReasoningNormalizer::default();
        let mut observed = Vec::new();

        for mut deltas in [
            vec![AiStreamDelta::TextDelta("<thi".into())],
            vec![AiStreamDelta::TextDelta("nk> step".into())],
            vec![
                AiStreamDelta::TextDelta("</think>answer".into()),
                AiStreamDelta::Done {
                    stop_reason: "stop".into(),
                },
            ],
        ] {
            normalizer.normalize(&mut deltas, false);
            observed.extend(deltas);
        }

        assert!(matches!(observed.as_slice(), [
            AiStreamDelta::ThinkingDelta(reasoning),
            AiStreamDelta::TextDelta(text),
            AiStreamDelta::Done { .. },
        ] if reasoning == "step" && text == "answer"));
    }

    #[test]
    fn stream_reasoning_normalization_preserves_unclosed_tag_as_text() {
        let mut normalizer = StreamReasoningNormalizer::default();
        let mut content = vec![AiStreamDelta::TextDelta("<think>incomplete".into())];
        normalizer.normalize(&mut content, false);
        assert!(content.is_empty());

        let mut terminal = vec![AiStreamDelta::Done {
            stop_reason: "stop".into(),
        }];
        normalizer.normalize(&mut terminal, false);
        assert!(matches!(
            terminal.as_slice(),
            [
                AiStreamDelta::TextDelta(text),
                AiStreamDelta::Done { .. }
            ] if text == "<think>incomplete"
        ));
    }

    #[tokio::test]
    async fn gitlab_401_refreshes_direct_access_token_and_retries_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind GitLab fixture");
        let address = listener.local_addr().expect("read GitLab fixture address");
        let server = tokio::spawn(async move {
            let responses = [
                (
                    "200 OK",
                    "application/json",
                    r#"{"token":"stale","headers":{"x-gitlab-old":"stale"}}"#,
                ),
                (
                    "401 Unauthorized",
                    "application/json",
                    r#"{"error":{"message":"expired"}}"#,
                ),
                (
                    "200 OK",
                    "application/json",
                    r#"{"token":"fresh","headers":{"x-gitlab-fresh":"enabled"}}"#,
                ),
                (
                    "200 OK",
                    "application/json",
                    r#"{"id":"chat_1","model":"gpt-test","choices":[{"index":0,"message":{"role":"assistant","content":"retried"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                ),
                (
                    "401 Unauthorized",
                    "application/json",
                    r#"{"error":{"message":"expired"}}"#,
                ),
                (
                    "200 OK",
                    "application/json",
                    r#"{"token":"fresh-stream","headers":{"x-gitlab-stream":"enabled"}}"#,
                ),
                ("200 OK", "text/event-stream", "data: [DONE]\n\n"),
            ];
            let mut requests = Vec::new();
            for (status, content_type, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("accept GitLab request");
                let mut request = vec![0_u8; 16 * 1024];
                let count = socket
                    .read(&mut request)
                    .await
                    .expect("read GitLab request");
                requests.push(String::from_utf8_lossy(&request[..count]).into_owned());
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write GitLab response");
            }
            requests
        });

        let base_url = format!("http://{address}");
        let provider = Provider {
            id: "gitlab-provider".into(),
            name: "GitLab".into(),
            vendor: Some("gitlab".into()),
            protocol: "openai-compatible".into(),
            base_url: base_url.clone(),
            preset_key: Some("gitlab".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "personal-token".into(),
            adapter_credentials: serde_json::json!({
                "apiKey": "personal-token",
                "instanceUrl": base_url,
                "aiGatewayUrl": base_url,
            })
            .to_string(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let gateway = Gateway::new(crate::config::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!(
                "stravia-gitlab-retry-test-{}",
                uuid::Uuid::new_v4()
            )),
            ..Default::default()
        })
        .await
        .expect("create test gateway");
        let adapter = ProviderAdapter::new(
            Arc::new(crate::provider::gitlab::GitLabVendor),
            ProviderBinding {
                provider: provider.clone(),
                protocol:
                    stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                egress_base_url: base_url.clone(),
                api_key: "personal-token".into(),
                actual_model: "gpt-test".into(),
                gateway: gateway.clone(),
                disable_default_auth: false,
                observer: None,
                model_turn_id: "test-turn".into(),
                target_id: "test-target".into(),
                provider_name: "GitLab".into(),
            },
        );
        let mut request = AiRequest::new(
            "gpt-test",
            vec![stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                    "hello".into(),
                ),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.meta.source_protocol =
            Some(stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        let outbound = adapter
            .build_request(&mut request)
            .await
            .expect("build initial GitLab request");
        assert_eq!(
            outbound
                .headers
                .get(reqwest::header::AUTHORIZATION)
                .unwrap(),
            "Bearer stale"
        );
        assert_eq!(outbound.headers.get("x-gitlab-old").unwrap(), "stale");

        let mut call = adapter.bind(ProxyClient::new(reqwest::Client::new()), outbound);
        let response = call.call_non_stream().await.expect("retry GitLab request");
        assert_eq!(response.status, 200);
        assert_eq!(response.canonical.unwrap().output_text(), "retried");

        request.stream.enabled = true;
        let stream_adapter = ProviderAdapter::new(
            Arc::new(crate::provider::gitlab::GitLabVendor),
            ProviderBinding {
                provider,
                protocol:
                    stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                egress_base_url: base_url,
                api_key: "personal-token".into(),
                actual_model: "gpt-test".into(),
                gateway,
                disable_default_auth: false,
                observer: None,
                model_turn_id: "test-turn-stream".into(),
                target_id: "test-target".into(),
                provider_name: "GitLab".into(),
            },
        );
        let stream_outbound = stream_adapter
            .build_request(&mut request)
            .await
            .expect("build GitLab stream request");
        assert_eq!(
            stream_outbound
                .headers
                .get(reqwest::header::AUTHORIZATION)
                .unwrap(),
            "Bearer fresh"
        );
        let mut stream_call =
            stream_adapter.bind(ProxyClient::new(reqwest::Client::new()), stream_outbound);
        assert!(matches!(
            stream_call
                .call_stream()
                .await
                .expect("retry GitLab stream"),
            ProviderStreamResponse::Stream(_)
        ));

        let requests = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("GitLab fixture completes")
            .expect("GitLab fixture succeeds");
        assert_eq!(requests.len(), 7);
        let requests = requests
            .iter()
            .map(|request| request.to_ascii_lowercase())
            .collect::<Vec<_>>();
        assert!(requests[0].starts_with("post /api/v4/ai/third_party_agents/direct_access "));
        assert!(requests[0].contains("authorization: bearer personal-token"));
        assert!(requests[1].starts_with("post /ai/v1/proxy/openai/v1/chat/completions "));
        assert!(requests[1].contains("authorization: bearer stale"));
        assert!(requests[1].contains("x-gitlab-old: stale"));
        assert!(requests[2].starts_with("post /api/v4/ai/third_party_agents/direct_access "));
        assert!(requests[3].starts_with("post /ai/v1/proxy/openai/v1/chat/completions "));
        assert!(requests[3].contains("authorization: bearer fresh"));
        assert!(requests[3].contains("x-gitlab-fresh: enabled"));
        assert!(!requests[3].contains("x-gitlab-old: stale"));
        assert!(requests[4].starts_with("post /ai/v1/proxy/openai/v1/chat/completions "));
        assert!(requests[4].contains("authorization: bearer fresh"));
        assert!(requests[5].starts_with("post /api/v4/ai/third_party_agents/direct_access "));
        assert!(requests[6].starts_with("post /ai/v1/proxy/openai/v1/chat/completions "));
        assert!(requests[6].contains("authorization: bearer fresh-stream"));
        assert!(requests[6].contains("x-gitlab-stream: enabled"));
        assert!(!requests[6].contains("x-gitlab-fresh: enabled"));
        assert!(requests[6].contains(r#""stream":true"#));
    }
}
