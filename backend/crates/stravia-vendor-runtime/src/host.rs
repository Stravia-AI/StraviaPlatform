use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_vendor_sdk::{ErrorKind, HttpCompression, WsCompression};

#[derive(Clone)]
#[doc(hidden)]
pub struct HttpResponseResource(pub Arc<dyn HostHttpResponse>);

#[derive(Clone)]
#[doc(hidden)]
pub struct WebSocketResource(pub Arc<dyn HostWebSocket>);

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// None preserves the legacy entry point's manual compression headers.
    /// Some opts into host-owned compression, including an identity-only policy.
    pub compression: Option<HttpCompression>,
}

#[derive(Debug, Clone)]
pub struct WsRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub protocols: Vec<String>,
    pub continuation_id: Option<String>,
    pub reuse_max_age_millis: Option<u64>,
    /// None is the legacy, non-negotiating entry point; Some is explicit policy.
    pub compression: Option<WsCompression>,
}

#[derive(Debug, Clone)]
pub enum WebSocketMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<(u16, String)>),
}

#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug)]
pub enum RuntimeEvent {
    UpstreamStarted,
    Delta(AiStreamDelta),
    Completed,
    Compacted,
    Failed {
        kind: ErrorKind,
        message: String,
        upstream_status: Option<u16>,
    },
}

/// Host-owned failure text may be sent back to the guest for control flow, but
/// it is still not automatically safe for end users. Runtime errors expose a
/// fixed summary selected from `kind`, never this text or guest text.
#[derive(Debug, Clone)]
pub struct HostFailure {
    pub kind: ErrorKind,
    pub message: String,
    pub upstream_status: Option<u16>,
}

impl HostFailure {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            upstream_status: None,
        }
    }

    pub fn upstream(
        kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
        status: Option<u16>,
        retry_after: Option<Duration>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: ErrorKind::upstream(kind, retry_after),
            message: message.into(),
            upstream_status: status,
        }
    }

    pub fn upstream_transport(
        kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
        status: Option<u16>,
        retry_after: Option<Duration>,
        transport_failure: stravia_vendor_sdk::TransportFailure,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: ErrorKind::upstream_transport(kind, retry_after, transport_failure),
            message: message.into(),
            upstream_status: status,
        }
    }
}

/// Pending HTTP exchange returned immediately after admission. `status` and
/// `headers` wait for response headers; `read_body` waits for the next body
/// chunk and MUST return transport failures as `Err`, never as `Ok(None)`.
#[async_trait]
pub trait HostHttpResponse: Send + Sync {
    async fn status(&self) -> Result<u16, HostFailure>;
    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure>;
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure>;
}

#[async_trait]
pub trait HostWebSocket: Send + Sync {
    async fn send(&self, message: WebSocketMessage) -> Result<(), HostFailure>;
    async fn next(&self) -> Result<Option<WebSocketMessage>, HostFailure>;
    async fn close(&self, continuation_id: Option<String>) -> Result<(), HostFailure>;
}

/// Services are constructed by Core for exactly one operation and one fixed
/// connection snapshot. Implementations own proxy/TLS, redirects, exact-origin
/// checks, credentials, diagnostic capture, and storage scope. They must not
/// expose provider/session/vendor identifiers to the guest.
#[async_trait]
pub trait HostServices: Send + Sync {
    /// Admit and start the request without waiting for response headers.
    /// Redirect targets must be checked again and credentials must not be
    /// forwarded across origins unless explicitly authorized.
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure>;

    /// Connect with a guest-declared maximum age for cross-request reuse.
    /// `None` uses the host default; zero disables cross-request reuse. Hosts
    /// may cap the requested age. Age starts at the first handshake, and later
    /// checkouts may shorten but never extend the deadline. This policy does
    /// not shorten the independent in-flight I/O deadline.
    async fn ws_connect(&self, request: WsRequest) -> Result<Arc<dyn HostWebSocket>, HostFailure>;

    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure>;
    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure>;

    /// Must apply bounded backpressure and return only after the event has been
    /// accepted or the operation was cancelled/fenced.
    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure>;

    fn log(&self, level: LogLevel, message: &str);

    /// Generation fence checked before every externally visible action and
    /// state write. False prevents late writes after an incompatible update.
    fn generation_is_current(&self, generation: u64) -> bool;
}
