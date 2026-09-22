use stravia_runtime_contract::protocol::ir::{AiResponse, AiStreamDelta, NativeCompactionResponse};

use crate::bindings::stravia::vendor::{host, types};
use crate::envelope::{encode_payload, error};
use crate::guest::{ErrorKind, PluginError};

pub use host::{HttpResponse, WsConnection};
pub use types::WsMessage;

/// Operation-scoped access to controlled host services. It is intentionally
/// not cloneable into a background executor; imported resources are owned and
/// dropped before the exported operation returns.
pub struct GuestHost {
    _private: (),
}

impl GuestHost {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Admit an HTTP request and immediately return its pending response
    /// resource. Waiting happens only in `status`, `headers`, and `read_body`.
    pub fn http_start(&self, request: types::HttpRequest) -> Result<HttpResponse, PluginError> {
        host::http_start(&request)
    }

    pub fn ws_connect(&self, request: types::WsRequest) -> Result<WsConnection, PluginError> {
        host::ws_connect(&request)
    }

    pub fn read_private_state(&self) -> Result<Option<Vec<u8>>, PluginError> {
        host::read_private_state()
    }

    pub fn write_private_state(&self, bytes: &[u8]) -> Result<(), PluginError> {
        host::write_private_state(bytes)
    }

    /// Mark the exact boundary immediately before sending a model-generating
    /// HTTP request or WebSocket frame. Auxiliary requests must not call this.
    pub fn emit_started(&self) -> Result<(), PluginError> {
        host::emit_started()
    }

    /// Backpressured canonical stream emission.
    pub fn emit_delta(&self, delta: &AiStreamDelta) -> Result<(), PluginError> {
        host::emit_event(&types::CanonicalEvent::Delta(encode_payload(delta)))
    }

    pub fn emit_completed(&self, response: &AiResponse) -> Result<(), PluginError> {
        host::emit_event(&types::CanonicalEvent::Completed(encode_payload(response)))
    }

    pub fn emit_compacted(&self, response: &NativeCompactionResponse) -> Result<(), PluginError> {
        host::emit_event(&types::CanonicalEvent::Compacted(encode_payload(response)))
    }

    pub fn emit_failed(&self, failure: PluginError) -> Result<(), PluginError> {
        host::emit_event(&types::CanonicalEvent::Failed(failure))
    }

    pub fn log(&self, level: host::LogLevel, message: &str) {
        host::log(level, message)
    }
}

/// Read a complete body while preserving transport errors. An error from any
/// chunk is returned as an error, never translated into EOF. `limit` is an
/// additional guest-side bound; the host independently enforces its own cap.
pub fn read_http_body(response: &HttpResponse, limit: usize) -> Result<Vec<u8>, PluginError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.read_body()? {
        let next_len = body.len().checked_add(chunk.len()).ok_or_else(|| {
            error(
                ErrorKind::ResourceExhausted,
                "HTTP response body exceeds guest limit",
            )
        })?;
        if next_len > limit {
            return Err(error(
                ErrorKind::ResourceExhausted,
                "HTTP response body exceeds guest limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
