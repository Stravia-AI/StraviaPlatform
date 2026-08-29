use std::borrow::Cow;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures::{StreamExt, stream::BoxStream};
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::Gateway;
use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse, AiStreamDelta};
use crate::provider::inbound::InboundResponse;
use crate::provider::outbound::OutboundRequest;
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::proxy::client::{
    ProxyClient, ResponsesWebSocketAcquireError, ResponsesWebSocketLease,
    ResponsesWebSocketRegistry, ResponsesWebSocketRequest, ResponsesWebSocketTrace,
};

pub(crate) struct ProviderCall {
    adapter: ProviderAdapter,
    client: ProxyClient,
    outbound: OutboundRequest,
    websocket: Option<ResponsesWebSocketCall>,
}

#[derive(Clone)]
struct ResponsesWebSocketCall {
    registry: ResponsesWebSocketRegistry,
    namespace: String,
    provider_id: String,
    target_id: String,
    transport_attempt: String,
    full_outbound: OutboundRequest,
    require_affinity: bool,
    session_affinity: Option<String>,
}

pub(crate) struct ProviderUnaryResponse {
    pub raw: Value,
    pub canonical: Result<AiResponse, GatewayError>,
    pub status: u16,
    pub headers: HeaderMap,
}

pub(crate) enum ProviderStreamResponse {
    Error {
        status: u16,
        headers: HeaderMap,
        body: anyhow::Result<Value>,
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
    pub headers: HeaderMap,
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

struct ResponsesWebSocketStream {
    lease: ResponsesWebSocketLease,
    done_marker_sent: bool,
    done: bool,
    event_seen: bool,
    replayed_full_request: bool,
    full_request: Value,
    client: ProxyClient,
    fallback_outbound: OutboundRequest,
    http_fallback: Option<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>,
}

pub(crate) struct ProviderStreamChunk {
    pub raw: bytes::Bytes,
    pub deltas: Vec<AiStreamDelta>,
}

#[derive(Default)]
struct StreamReasoningNormalizer {
    pending: String,
    inside_think: bool,
    emitted_block: bool,
    current_block_started: bool,
    disabled: bool,
}

impl StreamReasoningNormalizer {
    fn normalize(&mut self, deltas: &mut Vec<AiStreamDelta>, finish: bool) {
        if self.disabled {
            return;
        }
        let needs_work = finish
            || !self.pending.is_empty()
            || self.inside_think
            || deltas.iter().any(|delta| {
                matches!(delta, AiStreamDelta::ThinkingDelta(_))
                    || matches!(delta, AiStreamDelta::TextDelta(text) if text.contains('<'))
            });
        if !needs_work {
            return;
        }

        let input = std::mem::take(deltas);
        let mut output = Vec::with_capacity(input.len());
        for delta in input {
            match delta {
                AiStreamDelta::TextDelta(text) if !self.disabled => {
                    self.pending.push_str(&text);
                    self.drain_pending(&mut output);
                }
                AiStreamDelta::ThinkingDelta(text) => {
                    self.flush_pending_literal(&mut output);
                    self.disabled = true;
                    output.push(AiStreamDelta::ThinkingDelta(text));
                }
                terminal
                    if matches!(
                        terminal,
                        AiStreamDelta::Done { .. }
                            | AiStreamDelta::StreamError { .. }
                            | AiStreamDelta::UnexpectedEof
                    ) =>
                {
                    self.flush_terminal(&mut output);
                    output.push(terminal);
                    self.disabled = true;
                }
                other => output.push(other),
            }
        }
        if finish {
            self.flush_terminal(&mut output);
            self.disabled = true;
        }
        *deltas = output;
    }

    fn drain_pending(&mut self, output: &mut Vec<AiStreamDelta>) {
        const OPEN: &str = "<think>";
        const CLOSE: &str = "</think>";
        loop {
            let marker = if self.inside_think { CLOSE } else { OPEN };
            if let Some(index) = self.pending.find(marker) {
                let content = if self.inside_think {
                    self.pending[..index].trim().to_string()
                } else {
                    self.pending[..index].to_string()
                };
                self.emit_content(output, content);
                self.pending.drain(..index + marker.len());
                self.inside_think = !self.inside_think;
                if !self.inside_think {
                    self.current_block_started = false;
                }
                continue;
            }
            if self.inside_think {
                break;
            }
            let retained = marker_prefix_suffix_len(&self.pending, marker);
            let emitted_len = self.pending.len() - retained;
            if emitted_len > 0 {
                let content = self.pending[..emitted_len].to_string();
                self.emit_content(output, content);
                self.pending.drain(..emitted_len);
            }
            break;
        }
    }

    fn emit_content(&mut self, output: &mut Vec<AiStreamDelta>, mut content: String) {
        if self.inside_think && !self.current_block_started {
            content = content.trim_start().to_string();
        }
        if content.is_empty() {
            return;
        }
        if self.inside_think {
            if !self.current_block_started {
                if self.emitted_block {
                    push_delta_text(output, true, "\n".into());
                }
                self.current_block_started = true;
                self.emitted_block = true;
            }
            push_delta_text(output, true, content);
        } else {
            push_delta_text(output, false, content);
        }
    }

    fn flush_pending_literal(&mut self, output: &mut Vec<AiStreamDelta>) {
        if self.pending.is_empty() {
            return;
        }
        let prefix = if self.inside_think { "<think>" } else { "" };
        let text = format!("{prefix}{}", std::mem::take(&mut self.pending));
        push_delta_text(output, false, text);
        self.inside_think = false;
        self.current_block_started = false;
    }

    fn flush_terminal(&mut self, output: &mut Vec<AiStreamDelta>) {
        if self.inside_think {
            let text = format!("<think>{}", std::mem::take(&mut self.pending));
            push_delta_text(output, false, text);
            self.inside_think = false;
            self.current_block_started = false;
        } else if !self.pending.is_empty() {
            let content = std::mem::take(&mut self.pending);
            push_delta_text(output, false, content);
        }
    }
}

fn marker_prefix_suffix_len(value: &str, marker: &str) -> usize {
    (1..marker.len())
        .rev()
        .find(|&length| value.ends_with(&marker[..length]))
        .unwrap_or(0)
}

fn push_delta_text(output: &mut Vec<AiStreamDelta>, reasoning: bool, content: String) {
    match (reasoning, output.last_mut()) {
        (true, Some(AiStreamDelta::ThinkingDelta(existing)))
        | (false, Some(AiStreamDelta::TextDelta(existing))) => existing.push_str(&content),
        (true, _) => output.push(AiStreamDelta::ThinkingDelta(content)),
        (false, _) => output.push(AiStreamDelta::TextDelta(content)),
    }
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
    #[cfg(debug_assertions)]
    pub(crate) wire_capture_id: Option<String>,
}

pub(crate) struct ResponsesWebSocketBinding {
    pub(crate) client: ProxyClient,
    pub(crate) outbound: OutboundRequest,
    pub(crate) full_outbound: OutboundRequest,
    pub(crate) registry: ResponsesWebSocketRegistry,
    pub(crate) namespace: String,
    pub(crate) provider_id: String,
    pub(crate) target_id: String,
    pub(crate) transport_attempt: String,
    pub(crate) require_affinity: bool,
    pub(crate) session_affinity: Option<String>,
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

    #[cfg(debug_assertions)]
    fn capture_upstream_request(
        &self,
        transport: crate::wire_capture::CaptureTransport,
        headers: &HeaderMap,
        body: &Value,
    ) {
        let Some(capture) = &self.binding.gateway.wire_capture else {
            return;
        };
        let Some(capture_id) = &self.binding.wire_capture_id else {
            return;
        };
        let body = serde_json::to_vec(body).unwrap_or_default();
        capture.record(
            capture_id,
            crate::wire_capture::CapturePeer::Upstream,
            crate::wire_capture::CapturePhase::Request,
            transport,
            self.binding.protocol.to_string(),
            None,
            crate::proxy::observability::reqwest_headers_to_json(headers),
            &body,
        );
    }

    #[cfg(debug_assertions)]
    fn capture_upstream_response(
        &self,
        transport: crate::wire_capture::CaptureTransport,
        representation: crate::wire_capture::CaptureRepresentation,
        status: u16,
        headers: Option<&HeaderMap>,
        body: &[u8],
    ) {
        let Some(capture) = &self.binding.gateway.wire_capture else {
            return;
        };
        let Some(capture_id) = &self.binding.wire_capture_id else {
            return;
        };
        let headers = headers.and_then(crate::proxy::observability::reqwest_headers_to_json);
        match representation {
            crate::wire_capture::CaptureRepresentation::Wire => capture.record(
                capture_id,
                crate::wire_capture::CapturePeer::Upstream,
                crate::wire_capture::CapturePhase::Response,
                transport,
                self.binding.protocol.to_string(),
                Some(status),
                headers,
                body,
            ),
            crate::wire_capture::CaptureRepresentation::Normalized => capture.record_normalized(
                capture_id,
                crate::wire_capture::CapturePeer::Upstream,
                crate::wire_capture::CapturePhase::Response,
                transport,
                self.binding.protocol.to_string(),
                Some(status),
                headers,
                body,
            ),
        }
    }

    pub(crate) fn bind(self, client: ProxyClient, outbound: OutboundRequest) -> ProviderCall {
        ProviderCall {
            adapter: self,
            client,
            outbound,
            websocket: None,
        }
    }

    pub(crate) fn bind_responses_websocket(
        self,
        request: ResponsesWebSocketBinding,
    ) -> ProviderCall {
        let ResponsesWebSocketBinding {
            client,
            outbound,
            full_outbound,
            registry,
            namespace,
            provider_id,
            target_id,
            transport_attempt,
            require_affinity,
            session_affinity,
        } = request;
        ProviderCall {
            adapter: self,
            client,
            outbound,
            websocket: Some(ResponsesWebSocketCall {
                registry,
                namespace,
                provider_id,
                target_id,
                transport_attempt,
                full_outbound,
                require_affinity,
                session_affinity,
            }),
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

    async fn refresh_auth_on_unauthorized(
        &self,
        outbound: &mut OutboundRequest,
    ) -> Result<bool, GatewayError> {
        self.vendor
            .refresh_auth_on_unauthorized(&self.provider_context(), outbound)
            .await
    }

    fn prepare_responses_websocket_headers(
        &self,
        headers: &mut HeaderMap,
        connection: crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<()> {
        crate::provider::common::pipeline::prepare_responses_websocket_headers(
            &self.provider_context(),
            headers,
            connection,
        )
    }
    fn build_responses_websocket_request(
        &self,
        body: &Value,
        connection: crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<Value> {
        crate::provider::common::pipeline::build_responses_websocket_request(
            self.vendor.as_ref(),
            &self.provider_context(),
            body,
            connection,
        )
    }
    fn normalize_responses_websocket_event(&self, event: &mut Value) -> anyhow::Result<()> {
        crate::provider::common::pipeline::normalize_responses_websocket_event(
            self.vendor.as_ref(),
            &self.provider_context(),
            event,
        )
    }
    fn retain_responses_websocket_event(&self, event: &Value) -> bool {
        crate::provider::common::pipeline::retain_responses_websocket_event(
            self.vendor.as_ref(),
            &self.provider_context(),
            event,
        )
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

impl ProviderCall {
    pub(crate) fn url(&self) -> &str {
        &self.outbound.url
    }

    pub(crate) fn request_headers_json(&self) -> Option<String> {
        crate::proxy::observability::reqwest_headers_to_json(&self.outbound.headers)
    }

    pub(crate) fn request_body_string(&self) -> Option<String> {
        serde_json::to_string(&self.outbound.body).ok()
    }

    pub(crate) async fn call_non_stream(&mut self) -> anyhow::Result<ProviderUnaryResponse> {
        #[cfg(debug_assertions)]
        self.adapter.capture_upstream_request(
            crate::wire_capture::CaptureTransport::Http,
            &self.outbound.headers,
            &self.outbound.body,
        );
        let (mut raw, mut status, mut headers) = self
            .client
            .call_non_stream(
                &self.outbound.url,
                self.outbound.headers.clone(),
                self.outbound.body.clone(),
            )
            .await?;
        #[cfg(debug_assertions)]
        self.adapter.capture_upstream_response(
            crate::wire_capture::CaptureTransport::Http,
            crate::wire_capture::CaptureRepresentation::Wire,
            status,
            Some(&headers),
            &serde_json::to_vec(&raw).unwrap_or_default(),
        );
        if status == 401
            && self
                .adapter
                .refresh_auth_on_unauthorized(&mut self.outbound)
                .await?
        {
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_request(
                crate::wire_capture::CaptureTransport::Http,
                &self.outbound.headers,
                &self.outbound.body,
            );
            (raw, status, headers) = self
                .client
                .call_non_stream(
                    &self.outbound.url,
                    self.outbound.headers.clone(),
                    self.outbound.body.clone(),
                )
                .await?;
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_response(
                crate::wire_capture::CaptureTransport::Http,
                crate::wire_capture::CaptureRepresentation::Wire,
                status,
                Some(&headers),
                &serde_json::to_vec(&raw).unwrap_or_default(),
            );
        }
        let canonical = self.adapter.parse_response(InboundResponse {
            status,
            body: raw.clone(),
        });
        Ok(ProviderUnaryResponse {
            raw,
            canonical: canonical.await,
            status,
            headers,
        })
    }

    pub(crate) async fn call_stream(&mut self) -> anyhow::Result<ProviderStreamResponse> {
        if let Some(websocket) = &self.websocket {
            let websocket_url = responses_websocket_url(&self.outbound.url)?;
            let previous_response_id = self
                .outbound
                .body
                .get("previous_response_id")
                .and_then(Value::as_str);
            let session_id = uuid::Uuid::new_v4().to_string();
            let thread_id = uuid::Uuid::new_v4().to_string();
            let window_id = uuid::Uuid::new_v4().to_string();
            let connection = crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata {
                session_id: &session_id,
                thread_id: &thread_id,
                window_id: &window_id,
            };
            let mut headers = self.outbound.headers.clone();
            self.adapter
                .prepare_responses_websocket_headers(&mut headers, connection)?;
            #[cfg(debug_assertions)]
            let capture_headers = headers.clone();
            let lease = websocket
                .registry
                .acquire(
                    &self.client.responses_websocket,
                    &websocket.namespace,
                    ResponsesWebSocketTrace {
                        provider_id: &websocket.provider_id,
                        target_id: &websocket.target_id,
                        transport_attempt: &websocket.transport_attempt,
                    },
                    ResponsesWebSocketRequest {
                        url: &websocket_url,
                        headers,
                    },
                    previous_response_id,
                    websocket.session_affinity.as_deref(),
                    websocket.require_affinity,
                )
                .await;
            match lease {
                Ok(mut lease) => {
                    let request_body =
                        if websocket.require_affinity && lease.previous_response_id().is_none() {
                            &websocket.full_outbound.body
                        } else {
                            &self.outbound.body
                        };
                    let connection = lease.connection_metadata();
                    let request = self
                        .adapter
                        .build_responses_websocket_request(request_body, connection)?;
                    let full_request = self.adapter.build_responses_websocket_request(
                        &websocket.full_outbound.body,
                        connection,
                    )?;
                    #[cfg(debug_assertions)]
                    self.adapter.capture_upstream_request(
                        crate::wire_capture::CaptureTransport::WebSocket,
                        &capture_headers,
                        &request,
                    );
                    if let Err(error) = lease.send(&request).await {
                        if lease.reused_connection() {
                            let trace = lease.trace();
                            tracing::warn!(
                                transport = "responses_websocket",
                                provider_id = trace.provider_id,
                                target_id = trace.target_id,
                                transport_attempt = trace.transport_attempt,
                                failure_stage = "send",
                                error = %error,
                                fallback_transport = "http_sse",
                                "reused upstream WebSocket failed before a response; retrying silently"
                            );
                            lease.terminal();
                            drop(lease);
                            let mut outbound = websocket.full_outbound.clone();
                            outbound.body["stream"] = Value::Bool(true);
                            return self.http_stream(outbound).await;
                        }
                        return Ok(ProviderStreamResponse::Uncertain {
                            message: error.to_string(),
                        });
                    }
                    let mut fallback_outbound = websocket.full_outbound.clone();
                    fallback_outbound.body["stream"] = Value::Bool(true);
                    return self.websocket_stream(lease, full_request, fallback_outbound);
                }
                Err(
                    error @ (ResponsesWebSocketAcquireError::Unsupported
                    | ResponsesWebSocketAcquireError::Cooldown
                    | ResponsesWebSocketAcquireError::Transport(_)),
                ) => {
                    let fallback_reason = match &error {
                        ResponsesWebSocketAcquireError::Unsupported => "unsupported",
                        ResponsesWebSocketAcquireError::Cooldown => "cooldown",
                        ResponsesWebSocketAcquireError::Transport(_) => "connect_failure",
                        ResponsesWebSocketAcquireError::Rejected(_) => unreachable!(),
                    };
                    tracing::debug!(
                        transport = "responses_websocket",
                        target_namespace = websocket.namespace,
                        provider_id = websocket.provider_id,
                        target_id = websocket.target_id,
                        transport_attempt = websocket.transport_attempt,
                        fallback_reason,
                        error = %error,
                        "falling back from Responses WebSocket to HTTP/SSE"
                    );
                    let mut outbound = if websocket.require_affinity {
                        websocket.full_outbound.clone()
                    } else {
                        self.outbound.clone()
                    };
                    outbound.body["stream"] = Value::Bool(true);
                    return self.http_stream(outbound).await;
                }
                Err(ResponsesWebSocketAcquireError::Rejected(status)) => {
                    tracing::debug!(
                        transport = "responses_websocket",
                        target_namespace = websocket.namespace,
                        provider_id = websocket.provider_id,
                        target_id = websocket.target_id,
                        transport_attempt = websocket.transport_attempt,
                        rejection_status = status,
                        "upstream WebSocket handshake was rejected"
                    );
                    return Ok(ProviderStreamResponse::Error {
                        status,
                        headers: HeaderMap::new(),
                        body: Ok(serde_json::json!({
                            "error": {
                                "message": "Responses WebSocket handshake was rejected"
                            }
                        })),
                    });
                }
            }
        }
        self.http_stream(self.outbound.clone()).await
    }

    fn websocket_stream(
        &self,
        lease: ResponsesWebSocketLease,
        full_request: Value,
        fallback_outbound: OutboundRequest,
    ) -> anyhow::Result<ProviderStreamResponse> {
        let response_continuation_available = Arc::new(AtomicBool::new(true));
        Ok(ProviderStreamResponse::Stream(Box::new(ProviderStream {
            adapter: self.adapter.clone(),
            decoder: crate::protocol::transform::ProtocolTransform::global()
                .decode_stream(self.adapter.binding.protocol)?,
            reasoning: StreamReasoningNormalizer::default(),
            source: ProviderStreamSource::ResponsesWebSocket(Box::new(ResponsesWebSocketStream {
                lease,
                done: false,
                done_marker_sent: false,
                event_seen: false,
                replayed_full_request: false,
                client: self.client.clone(),
                fallback_outbound,
                http_fallback: None,
                full_request,
            })),
            status: 200,
            headers: HeaderMap::new(),
            response_continuation_available,
        })))
    }

    async fn http_stream(
        &mut self,
        mut outbound: OutboundRequest,
    ) -> anyhow::Result<ProviderStreamResponse> {
        #[cfg(debug_assertions)]
        self.adapter.capture_upstream_request(
            crate::wire_capture::CaptureTransport::Sse,
            &outbound.headers,
            &outbound.body,
        );
        let (mut response, mut status) = self
            .client
            .call_stream(
                &outbound.url,
                outbound.headers.clone(),
                outbound.body.clone(),
            )
            .await?;
        #[cfg(debug_assertions)]
        self.adapter.capture_upstream_response(
            crate::wire_capture::CaptureTransport::Sse,
            crate::wire_capture::CaptureRepresentation::Wire,
            status,
            Some(response.headers()),
            &[],
        );
        if status == 401
            && self
                .adapter
                .refresh_auth_on_unauthorized(&mut outbound)
                .await?
        {
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_request(
                crate::wire_capture::CaptureTransport::Sse,
                &outbound.headers,
                &outbound.body,
            );
            (response, status) = self
                .client
                .call_stream(
                    &outbound.url,
                    outbound.headers.clone(),
                    outbound.body.clone(),
                )
                .await?;
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_response(
                crate::wire_capture::CaptureTransport::Sse,
                crate::wire_capture::CaptureRepresentation::Wire,
                status,
                Some(response.headers()),
                &[],
            );
        }
        self.outbound = outbound;
        let headers = response.headers().clone();
        if status >= 400 {
            let body = response.json().await.map_err(anyhow::Error::from);
            #[cfg(debug_assertions)]
            if let Ok(body) = &body {
                self.adapter.capture_upstream_response(
                    crate::wire_capture::CaptureTransport::Sse,
                    crate::wire_capture::CaptureRepresentation::Wire,
                    status,
                    Some(&headers),
                    &serde_json::to_vec(body).unwrap_or_default(),
                );
            }
            return Ok(ProviderStreamResponse::Error {
                status,
                headers,
                body,
            });
        }
        Ok(ProviderStreamResponse::Stream(Box::new(ProviderStream {
            adapter: self.adapter.clone(),
            decoder: crate::protocol::transform::ProtocolTransform::global()
                .decode_stream(self.adapter.binding.protocol)?,
            reasoning: StreamReasoningNormalizer::default(),
            source: ProviderStreamSource::Http(response.bytes_stream().boxed()),
            status,
            headers,
            response_continuation_available: Arc::new(AtomicBool::new(false)),
        })))
    }
}

impl ProviderStream {
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
                let raw = stream.next_raw(adapter, self.status).await?;
                if stream.http_fallback.is_some() {
                    self.response_continuation_available
                        .store(false, Ordering::Release);
                }
                let Some(raw) = raw else {
                    return Ok(None);
                };
                raw
            }
        };
        #[cfg(debug_assertions)]
        let (transport, representation) = match &self.source {
            ProviderStreamSource::Http(_) => (
                crate::wire_capture::CaptureTransport::Sse,
                crate::wire_capture::CaptureRepresentation::Wire,
            ),
            ProviderStreamSource::ResponsesWebSocket(_) => (
                crate::wire_capture::CaptureTransport::WebSocket,
                crate::wire_capture::CaptureRepresentation::Normalized,
            ),
        };
        #[cfg(debug_assertions)]
        self.adapter
            .capture_upstream_response(transport, representation, self.status, None, &raw);
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
        Ok(Some(ProviderStreamChunk { raw, deltas }))
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

impl ResponsesWebSocketStream {
    async fn next_raw(
        &mut self,
        adapter: &ProviderAdapter,
        status: u16,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        if let Some(stream) = &mut self.http_fallback {
            return match stream.next().await {
                Some(Ok(bytes)) => Ok(Some(bytes)),
                Some(Err(error)) => Err(ProviderStreamError::Transport(error.to_string())),
                None => Ok(None),
            };
        }
        if self.done {
            if self.done_marker_sent {
                return Ok(None);
            }
            self.done_marker_sent = true;
            return Ok(Some(bytes::Bytes::from_static(b"data: [DONE]\n\n")));
        }
        loop {
            let message = match self.lease.next().await {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    return self
                        .recover_reused_connection("receive", error.to_string())
                        .await;
                }
                None => {
                    return self
                        .recover_reused_connection(
                            "receive",
                            "Responses WebSocket closed before a terminal event".into(),
                        )
                        .await;
                }
            };
            let text = match message {
                reqwest_websocket::Message::Text(text) => {
                    #[cfg(debug_assertions)]
                    adapter.capture_upstream_response(
                        crate::wire_capture::CaptureTransport::WebSocket,
                        crate::wire_capture::CaptureRepresentation::Wire,
                        status,
                        None,
                        text.as_bytes(),
                    );
                    text
                }
                reqwest_websocket::Message::Ping(_) | reqwest_websocket::Message::Pong(_) => {
                    continue;
                }
                reqwest_websocket::Message::Close { .. } => {
                    return self
                        .recover_reused_connection(
                            "receive",
                            "Responses WebSocket closed before a terminal event".into(),
                        )
                        .await;
                }
                reqwest_websocket::Message::Binary(_) => {
                    return Err(ProviderStreamError::Uncertain(
                        "Responses WebSocket returned a binary event".into(),
                    ));
                }
            };
            let mut value: Value = serde_json::from_str(&text).map_err(|error| {
                ProviderStreamError::Uncertain(format!(
                    "Responses WebSocket returned invalid JSON: {error}"
                ))
            })?;
            adapter
                .normalize_responses_websocket_event(&mut value)
                .map_err(|error| ProviderStreamError::Uncertain(error.to_string()))?;
            if !adapter.retain_responses_websocket_event(&value) {
                continue;
            }
            let event_type = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if event_type == "error" {
                let code = value
                    .pointer("/error/code")
                    .or_else(|| value.get("code"))
                    .and_then(Value::as_str);
                let trace = self.lease.trace();
                tracing::debug!(
                    transport = "responses_websocket",
                    provider_id = trace.provider_id,
                    target_id = trace.target_id,
                    transport_attempt = trace.transport_attempt,
                    upstream_event = "error",
                    upstream_error_code = code.unwrap_or("unknown"),
                    "received upstream WebSocket error event"
                );
                if code == Some("websocket_connection_limit_reached") && !self.event_seen {
                    tracing::debug!(
                        transport = "responses_websocket",
                        provider_id = trace.provider_id,
                        target_id = trace.target_id,
                        transport_attempt = trace.transport_attempt,
                        fallback_reason = "connection_limit",
                        "falling back from Responses WebSocket to HTTP/SSE"
                    );
                    self.lease.connection_limit();
                    return self.switch_to_http_fallback().await;
                }
                if code == Some("previous_response_not_found")
                    && self.lease.previous_response_id().is_some()
                    && !self.event_seen
                    && !self.replayed_full_request
                {
                    self.lease.invalidate_previous();
                    self.lease
                        .send(&self.full_request)
                        .await
                        .map_err(|error| ProviderStreamError::Uncertain(error.to_string()))?;
                    self.replayed_full_request = true;
                    continue;
                }
                self.lease.invalidate_previous();
                self.lease.terminal();
                self.done = true;
            } else {
                self.event_seen = true;
                if responses_websocket_terminal(&event_type, &value) {
                    if response_completed(&event_type, &value) {
                        if let Some(response_id) = value
                            .pointer("/response/id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                        {
                            self.lease.completed(response_id);
                        } else {
                            self.lease.terminal();
                        }
                    } else {
                        let code = value
                            .pointer("/response/error/code")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        tracing::debug!(
                            transport = "responses_websocket",
                            upstream_event = event_type,
                            upstream_error_code = code,
                            "received unsuccessful upstream WebSocket terminal event"
                        );
                        self.lease.invalidate_previous();
                        self.lease.terminal();
                    }
                    self.done = true;
                }
            }
            let text = serde_json::to_string(&value)
                .map_err(|error| ProviderStreamError::Uncertain(error.to_string()))?;
            return Ok(Some(bytes::Bytes::from(format!(
                "event: {event_type}\ndata: {text}\n\n"
            ))));
        }
    }

    async fn recover_reused_connection(
        &mut self,
        failure_stage: &'static str,
        error: String,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        if self.event_seen || !self.lease.reused_connection() {
            return Err(ProviderStreamError::Uncertain(error));
        }
        let trace = self.lease.trace();
        tracing::warn!(
            transport = "responses_websocket",
            provider_id = trace.provider_id,
            target_id = trace.target_id,
            transport_attempt = trace.transport_attempt,
            failure_stage,
            error,
            fallback_transport = "http_sse",
            "reused upstream WebSocket failed before a response; retrying silently"
        );
        self.lease.terminal();
        self.switch_to_http_fallback().await
    }

    async fn switch_to_http_fallback(
        &mut self,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        let (response, status) = self
            .client
            .call_stream(
                &self.fallback_outbound.url,
                self.fallback_outbound.headers.clone(),
                self.fallback_outbound.body.clone(),
            )
            .await
            .map_err(|error| ProviderStreamError::Transport(error.to_string()))?;
        if status >= 400 {
            return Err(ProviderStreamError::Transport(format!(
                "HTTP/SSE fallback was rejected with status {status}"
            )));
        }
        self.http_fallback = Some(response.bytes_stream().boxed());
        let stream = self
            .http_fallback
            .as_mut()
            .expect("HTTP fallback stream was just installed");
        match stream.next().await {
            Some(Ok(bytes)) => Ok(Some(bytes)),
            Some(Err(error)) => Err(ProviderStreamError::Transport(error.to_string())),
            None => Ok(None),
        }
    }
}

fn responses_websocket_url(http_url: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(http_url)?;
    match url.scheme() {
        "http" => {
            url.set_scheme("ws")
                .map_err(|_| anyhow::anyhow!("invalid Responses WebSocket URL"))?;
        }
        "https" => {
            url.set_scheme("wss")
                .map_err(|_| anyhow::anyhow!("invalid Responses WebSocket URL"))?;
        }
        scheme => anyhow::bail!("unsupported Responses WebSocket URL scheme: {scheme}"),
    }
    Ok(url.to_string())
}

fn responses_websocket_terminal(event_type: &str, value: &Value) -> bool {
    matches!(
        event_type,
        "response.completed" | "response.failed" | "response.incomplete"
    ) || (event_type == "response.done"
        && value
            .pointer("/response/status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "completed" | "failed" | "incomplete")))
}

fn response_completed(event_type: &str, value: &Value) -> bool {
    event_type == "response.completed"
        || (event_type == "response.done"
            && value.pointer("/response/status").and_then(Value::as_str) == Some("completed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
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
                protocol: crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                egress_base_url: base_url.clone(),
                api_key: "personal-token".into(),
                actual_model: "gpt-test".into(),
                gateway: gateway.clone(),
                disable_default_auth: false,
                #[cfg(debug_assertions)]
                wire_capture_id: None,
            },
        );
        let mut request = AiRequest::new(
            "gpt-test",
            vec![crate::protocol::ir::AiItem {
                role: crate::protocol::ir::Role::User,
                content: crate::protocol::ir::MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.meta.source_protocol =
            Some(crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
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
                protocol: crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                egress_base_url: base_url,
                api_key: "personal-token".into(),
                actual_model: "gpt-test".into(),
                gateway,
                disable_default_auth: false,
                #[cfg(debug_assertions)]
                wire_capture_id: None,
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
