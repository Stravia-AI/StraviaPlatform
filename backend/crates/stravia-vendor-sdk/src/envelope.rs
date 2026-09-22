//! Versioned canonical envelope — the only canonical payload format allowed
//! across the WIT boundary. The host serializes/deserializes through
//! `stravia-runtime-contract` types; the guest does the same here, so both
//! ends share one canonical definition. The explicit `format` version keeps
//! internal IR evolution from silently becoming a wire-compat assumption.

use serde::{Serialize, de::DeserializeOwned};

use crate::bindings::stravia::vendor::types::{self, CanonicalPayload, PluginError};

/// Canonical envelope schema version emitted/consumed by this SDK.
/// Bumped whenever the serialized canonical shape changes incompatibly; the
/// host refuses plugins built against a version it does not support.
pub const CANONICAL_FORMAT_VERSION: u32 = 1;

/// A typed canonical body plus its format tag. `T` is a
/// `stravia-runtime-contract` type (e.g. `AiRequest`, `AiStreamDelta`,
/// `AiResponse`) or an SDK operation payload.
#[derive(Debug, Clone)]
pub struct CanonicalEnvelope<T> {
    pub format: u32,
    pub body: T,
}

/// Serialize `body` into a WIT [`CanonicalPayload`] tagged with
/// [`CANONICAL_FORMAT_VERSION`]. `serde_json` bytes — never vendor JSON.
pub fn encode_payload<T: Serialize>(body: &T) -> CanonicalPayload {
    let bytes = serde_json::to_vec(body).expect("canonical payloads must serialize");
    CanonicalPayload {
        format: CANONICAL_FORMAT_VERSION,
        body: bytes,
    }
}

/// Decode a [`CanonicalPayload`] into `T`, rejecting a mismatched `format`.
pub fn decode_payload<T: DeserializeOwned>(
    payload: &CanonicalPayload,
) -> Result<CanonicalEnvelope<T>, PluginError> {
    if payload.format != CANONICAL_FORMAT_VERSION {
        return Err(PluginError {
            kind: types::ErrorKind::Unsupported,
            message: format!(
                "canonical format {} unsupported (sdk expects {CANONICAL_FORMAT_VERSION})",
                payload.format
            ),
            upstream_status: None,
        });
    }
    let body = serde_json::from_slice::<T>(&payload.body).map_err(|e| PluginError {
        kind: types::ErrorKind::Invalid,
        message: format!("canonical payload decode failed: {e}"),
        upstream_status: None,
    })?;
    Ok(CanonicalEnvelope {
        format: payload.format,
        body,
    })
}

/// Host→guest direction helper: encode an error the SDK decided itself.
pub fn error(kind: types::ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}
