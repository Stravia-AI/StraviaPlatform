use super::*;

// ── Log builder ───────────────────────────────────────────────────────────────

/// Fluent builder for `LogEntry`. Eliminates the long flat parameter list at
/// call sites.
///
/// Create via `LogBuilder::from_dispatch`, chain setter methods for the
/// per-call fields, then call `emit` to enqueue the entry.
#[derive(Clone)]
pub(super) struct LogBuilder {
    gw: Gateway,
    client_protocol: String,
    upstream_protocol: String,
    client_model: String,
    upstream_model: String,
    api_key_id: Option<String>,
    api_key_name: Option<String>,
    provider_id: String,
    provider_name: String,
    model_id: Option<String>,
    model_name: Option<String>,
    is_stream: bool,
    redact_payloads: bool,
    start: Instant,
    client_status_code: i32,
    usage: Usage,
    thinking_level: Option<crate::thinking::ThinkingLevel>,
    extras: LogExtras,
}

impl LogBuilder {
    /// Build from dispatch-pipeline context before a provider is selected.
    /// `upstream_protocol` defaults to `client_protocol`; `upstream_model` and
    /// `provider_id` default to empty strings.
    pub(super) fn from_dispatch(
        gw: &Gateway,
        ingress: &str,
        request_model: &str,
        thinking_level: Option<crate::thinking::ThinkingLevel>,
        api_key_id: Option<&str>,
        start: Instant,
    ) -> Self {
        Self {
            gw: gw.clone(),
            client_protocol: ingress.to_string(),
            upstream_protocol: ingress.to_string(),
            client_model: request_model.to_string(),
            upstream_model: String::new(),
            api_key_id: api_key_id.map(ToString::to_string),
            api_key_name: None,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: None,
            model_name: None,
            is_stream: false,
            redact_payloads: false,
            start,
            client_status_code: 200,
            usage: Usage::default(),
            thinking_level,
            extras: LogExtras::default(),
        }
    }

    pub(super) fn stream_flag(mut self, v: bool) -> Self {
        self.is_stream = v;
        self
    }

    pub(super) fn model_turn(
        mut self,
        route: &crate::hook::RouteContext,
        target: &crate::model_turn::TargetIdentity,
    ) -> Self {
        self.upstream_protocol = route.egress.to_string();
        self.provider_id = target.provider_id.clone();
        self.provider_name = target.provider_name.clone();
        self.model_id = Some(route.model_id.clone());
        self.model_name = Some(target.route_name.clone());
        self.upstream_model = target.actual_model.clone();
        self
    }

    pub(super) fn upstream_protocol(mut self, protocol: &str) -> Self {
        self.upstream_protocol = protocol.to_string();
        self
    }

    pub(super) fn status(mut self, code: u16) -> Self {
        self.client_status_code = code as i32;
        self
    }

    pub(super) fn usage(mut self, u: Usage) -> Self {
        self.usage = u;
        self
    }
    /// Pre-fill the client request-side `LogExtras` fields (method, path,
    /// headers, body) from a `RequestExtras`.
    pub(super) fn with_req_extras(mut self, req: &RequestExtras) -> Self {
        self.extras.method = Some(req.method.clone());
        self.extras.path = Some(req.path.clone());
        self.extras.client_request_headers = req.headers.clone();
        self.extras.client_request_body = req.body.clone();
        self
    }

    /// Set the upstream request wire (headers + body encoded for upstream).
    pub(super) fn with_upstream_request(
        mut self,
        headers: Option<String>,
        body: Option<String>,
    ) -> Self {
        self.extras.upstream_request_headers = headers;
        self.extras.upstream_request_body = body;
        self
    }

    pub(super) fn upstream_url(mut self, url: &str) -> Self {
        self.extras.upstream_url = Some(crate::proxy::observability::redact_url_credentials(url));
        self
    }

    /// Set the upstream response wire.
    pub(super) fn with_upstream_response(
        mut self,
        status: i32,
        headers: Option<String>,
        body: Option<String>,
        latency_ms: Option<i64>,
    ) -> Self {
        self.extras.upstream_status_code = Some(status);
        self.extras.upstream_response_headers = headers;
        self.extras.upstream_response_body = body;
        self.extras.latency_upstream_ms = latency_ms;
        self
    }

    /// Set the client response wire.
    pub(super) fn with_client_response(
        mut self,
        headers: Option<String>,
        body: Option<String>,
    ) -> Self {
        self.extras.client_response_headers = headers;
        self.extras.client_response_body = body;
        self
    }

    pub(super) fn stream_metrics(mut self, chunks: i32, first_chunk_ms: Option<i64>) -> Self {
        self.extras.stream_chunks_count = chunks;
        self.extras.stream_first_chunk_ms = first_chunk_ms;
        self
    }

    // ── Legacy shim ────────────────────────────────────────────────────────

    /// Maps `response_body` → `client_response_body`.
    pub(super) fn resp_body(mut self, b: Option<String>) -> Self {
        self.extras.client_response_body = b;
        self
    }

    pub(super) fn emit(mut self) {
        if self.redact_payloads {
            self.extras.client_request_body = None;
            self.extras.client_response_body = None;
            self.extras.upstream_request_body = None;
            self.extras.upstream_response_body = None;
        }
        use crate::logging::LogEntry;
        let latency_total_ms = self.start.elapsed().as_millis() as i64;
        let entry = LogEntry {
            api_key_id: self.api_key_id,
            api_key_name: self.api_key_name,
            created_at: chrono::Utc::now().timestamp_millis(),
            client_protocol: self.client_protocol,
            upstream_protocol: self.upstream_protocol,
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            model_id: self.model_id,
            model_name: self.model_name,
            upstream_url: self.extras.upstream_url,
            client_model: self.client_model,
            upstream_model: self.upstream_model,
            method: self.extras.method,
            path: self.extras.path,
            client_request_headers: self.extras.client_request_headers,
            client_request_body: self.extras.client_request_body,
            client_response_headers: self.extras.client_response_headers,
            client_response_body: self.extras.client_response_body,
            upstream_request_headers: self.extras.upstream_request_headers,
            upstream_request_body: self.extras.upstream_request_body,
            upstream_response_headers: self.extras.upstream_response_headers,
            upstream_response_body: self.extras.upstream_response_body,
            upstream_status_code: self.extras.upstream_status_code,
            client_status_code: self.client_status_code,
            latency_total_ms,
            latency_upstream_ms: self.extras.latency_upstream_ms,
            usage: self.usage,
            thinking_level: self.thinking_level,
            is_stream: self.is_stream,
            stream_chunks_count: self.extras.stream_chunks_count,
            stream_first_chunk_ms: self.extras.stream_first_chunk_ms,
        };
        send_log(&self.gw, entry);
    }
}
