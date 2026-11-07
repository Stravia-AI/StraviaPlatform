use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_vendor_sdk::{
    ErrorKind,
    net::{self, ConnectRequest, Field, Options, Request},
};

#[derive(Clone, Default)]
#[doc(hidden)]
pub struct NetOptionsResource(pub Options);

#[derive(Clone)]
#[doc(hidden)]
pub struct HttpResponseResource(pub Arc<dyn HostHttpResponse>);

#[derive(Clone)]
#[doc(hidden)]
pub struct StateCellResource(pub Arc<dyn HostStateCell>);

#[derive(Clone)]
#[doc(hidden)]
pub struct JournalEntryResource(pub Arc<dyn HostJournalEntry>);

#[derive(Clone)]
#[doc(hidden)]
pub struct WebSocketResource(pub Arc<dyn HostWebSocket>);

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
        transport_failure: String,
        message: impl Into<String>,
    ) -> Self {
        let mut kind = ErrorKind::upstream(kind, retry_after);
        if let ErrorKind::Upstream(failure) = &mut kind {
            failure.transport_failure = Some(transport_failure);
        }
        Self {
            kind,
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
    async fn headers(&self) -> Result<Vec<Field>, HostFailure>;
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure>;
}

#[async_trait]
pub trait HostWebSocket: Send + Sync {
    fn handshake_status(&self) -> u16;
    fn handshake_headers(&self) -> Vec<Field>;
    /// True only when this checkout performed a new WebSocket handshake.
    fn is_fresh_handshake(&self) -> bool;
    async fn send(&self, message: WebSocketMessage) -> Result<(), HostFailure>;
    async fn next(&self) -> Result<Option<WebSocketMessage>, HostFailure>;
    async fn close(&self, continuation_id: Option<String>) -> Result<(), HostFailure>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateScope {
    Provider,
    Conversation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub revision: u64,
    pub payload: Option<Vec<u8>>,
}

/// Safe, guest-visible failure. Never include private state payloads in the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError {
    pub code: String,
    pub message: String,
}

impl StateError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// An immutable operation-bound journal node. Hosts revalidate its owner,
/// lease and epoch on every read and before accepting it for an update.
#[async_trait]
pub trait HostJournalEntry: Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;
    async fn read(&self) -> Result<JournalEntryData, StateError>;
}

pub struct JournalEntryData {
    pub text: String,
    pub previous: Option<Arc<dyn HostJournalEntry>>,
}

pub struct JournalSnapshot {
    pub revision: u64,
    pub payload: Option<Vec<u8>>,
    pub head: Option<Arc<dyn HostJournalEntry>>,
}

/// A cell is bound to the operation that opened it; implementations must fence
/// every access when that operation is cancelled or otherwise invalidated.
#[async_trait]
pub trait HostStateCell: Send + Sync {
    async fn load(&self) -> Result<StateSnapshot, StateError>;
    async fn compare_exchange(
        &self,
        expected_revision: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, StateError>;

    /// Atomically read the payload, revision and journal head. Only admitted
    /// conversation cells support journals.
    async fn journal_load(&self) -> Result<JournalSnapshot, StateError> {
        Err(StateError::new(
            "unsupported",
            "journal is unavailable in this context",
        ))
    }
    async fn journal_append(
        &self,
        _text: String,
        _previous: Option<Arc<dyn HostJournalEntry>>,
    ) -> Result<Arc<dyn HostJournalEntry>, StateError> {
        Err(StateError::new(
            "unsupported",
            "journal is unavailable in this context",
        ))
    }
    async fn journal_compare_exchange(
        &self,
        _expected_revision: u64,
        _payload: Option<Vec<u8>>,
        _head: Option<Arc<dyn HostJournalEntry>>,
    ) -> Result<bool, StateError> {
        Err(StateError::new(
            "unsupported",
            "journal is unavailable in this context",
        ))
    }
}

/// Services are constructed by Core for exactly one operation and one fixed
/// connection snapshot. Implementations own proxy/TLS, redirects, exact-origin
/// checks, credentials, diagnostic capture, and storage scope. They must not
/// expose provider/session/vendor identifiers to the guest.
#[async_trait]
pub trait HostServices: Send + Sync {
    /// Admit and start the request without waiting for response headers.
    /// Redirect targets must be checked again and credentials must not be
    /// forwarded across origins unless explicitly authorized. TLS, proxy,
    /// network permission, and failure handling remain host-owned.
    fn http_start(
        &self,
        request: Request,
        options: Options,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure>;

    /// Connect with the supplied transport options and host-owned reuse policy.
    async fn ws_connect(
        &self,
        request: ConnectRequest,
        options: Options,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure>;

    /// Validate every option independently before storing it in the resource.
    /// Hosts with a shape registry override this to validate its membership.
    fn validate_net_option(&self, key: &str, value: &str) -> Result<(), net::OptionError> {
        net::validate_host_option(key, value)?;
        if key == net::keys::SHAPE && value != "default" {
            return Err(net::OptionError {
                key: key.into(),
                value: value.into(),
            });
        }
        Ok(())
    }

    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure>;
    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure>;

    /// Open a cell in the operation's admitted context. Unsupported contexts
    /// must fail rather than silently provide ephemeral state.
    async fn open_state(
        &self,
        _scope: StateScope,
        _key: String,
    ) -> Result<Arc<dyn HostStateCell>, StateError> {
        Err(StateError::new(
            "unsupported",
            "state is unavailable in this context",
        ))
    }

    /// Open a provider-bound, process-volatile cell. Unsupported contexts
    /// must fail explicitly; durable storage is never a fallback.
    async fn open_volatile_state(
        &self,
        _key: String,
    ) -> Result<Arc<dyn HostStateCell>, StateError> {
        Err(StateError::new(
            "unsupported",
            "volatile state is unavailable in this context",
        ))
    }

    /// Must apply bounded backpressure and return only after the event has been
    /// accepted or the operation was cancelled/fenced.
    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure>;

    fn log(&self, level: LogLevel, message: &str);

    /// Generation fence checked before every externally visible action and
    /// state write. False prevents late writes after an incompatible update.
    fn generation_is_current(&self, generation: u64) -> bool;
}
