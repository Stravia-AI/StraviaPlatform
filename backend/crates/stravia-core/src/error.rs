//! Unified error taxonomy for the Stravia AI Gateway.
//!
//! Every failure that crosses a layer boundary must be expressed as a
//! `GatewayError` variant. HTTP status codes, stable error codes for client
//! responses, and retryability are all derived from the variant — not scattered
//! across handler.rs ad-hoc strings.
//!
//! # Rendering
//! `GatewayError::render()` produces the canonical JSON error body:
//! ```json
//! { "error": { "message": "...", "type": "STRAVIA_xxx", "code": 4xx } }
//! ```
//! Optionally a `request_id` (from `RequestContext`) is injected once PR-02
//! lands. Until then, callers may pass `None`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

// ── Supporting types ──────────────────────────────────────────────────────────

/// Which phase of a request an upstream timeout occurred in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeoutPhase {
    Connect,
    FirstByte,
    Streaming,
    Total,
}

impl TimeoutPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeoutPhase::Connect => "connect",
            TimeoutPhase::FirstByte => "first_byte",
            TimeoutPhase::Streaming => "streaming",
            TimeoutPhase::Total => "total",
        }
    }
}

/// Why an API-key / token authentication attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    /// No credential was presented at all.
    Missing,
    /// The credential is syntactically present but does not match any key.
    Invalid,
    /// The credential is valid but expired.
    Expired,
}

impl AuthFailure {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthFailure::Missing => "missing_credential",
            AuthFailure::Invalid => "invalid_api_key",
            AuthFailure::Expired => "expired_api_key",
        }
    }
}

/// Why a request was denied after authentication passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDenial {
    /// The key is not authorised to access this model.
    ModelNotAllowed,
    /// IP / CIDR block deny-list hit.
    IpDenied,
    /// Custom reason (e.g. admin-set flag).
    Custom(String),
}

/// A dotted-path field reference, e.g. `"messages[0].content"`.
pub type FieldPath = String;

// ── The central error type ────────────────────────────────────────────────────

/// Every cross-layer failure in the gateway MUST be expressed as one of these
/// variants. No free-form `anyhow::Error` / string responses should escape a
/// layer boundary.
pub enum GatewayError {
    /// The client sent a malformed or logically invalid request.
    BadRequest { code: &'static str, msg: String },

    /// Authentication failed (missing / invalid / expired credential).
    Unauthorized { reason: AuthFailure },

    /// Authentication passed but the caller is not permitted to perform this
    /// action.
    Forbidden { reason: AccessDenial },

    /// The Principal has reached its active root-request limit.
    ConcurrencyLimitExceeded,

    /// No model matched the requested model / path.
    ModelNotFound { model: String },

    /// The ingress protocol cannot be translated to the target egress protocol.
    ProtocolUnsupported { ingress: String, egress: String },

    /// A lossless translation was required but the transform would drop fields.
    ProtocolLossyRejected { lost: Vec<FieldPath> },

    /// The upstream provider returned a non-2xx HTTP status.
    UpstreamStatus {
        provider: String,
        status: u16,
        body: Option<String>,
    },

    /// The upstream provider did not respond within the allowed time.
    UpstreamTimeout { phase: TimeoutPhase },

    /// A streaming SSE chunk could not be parsed.
    StreamParseError {
        provider: String,
        raw_chunk: Option<String>,
    },

    /// The client disconnected before the response was fully delivered.
    ClientCancelled,

    /// The requested provider is not registered or is misconfigured.
    ProviderUnavailable { provider: String, reason: String },

    /// An unexpected internal error. Use sparingly; prefer a specific variant.
    Internal { source: anyhow::Error },
}

impl GatewayError {
    /// Canonical HTTP status code for this error.
    pub fn http_status(&self) -> StatusCode {
        match self {
            GatewayError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            GatewayError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            GatewayError::Forbidden { .. } => StatusCode::FORBIDDEN,
            GatewayError::ConcurrencyLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            GatewayError::ModelNotFound { .. } => StatusCode::NOT_FOUND,
            GatewayError::ProtocolUnsupported { .. } => StatusCode::BAD_REQUEST,
            GatewayError::ProtocolLossyRejected { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            GatewayError::UpstreamStatus { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            GatewayError::UpstreamTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            GatewayError::StreamParseError { .. } => StatusCode::BAD_GATEWAY,
            GatewayError::ClientCancelled => {
                StatusCode::from_u16(499).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            }
            GatewayError::ProviderUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            GatewayError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable machine-readable error type string included in every error body.
    pub fn stable_code(&self) -> &'static str {
        match self {
            GatewayError::BadRequest { .. } => "STRAVIA_BAD_REQUEST",
            GatewayError::Unauthorized { .. } => "STRAVIA_AUTH_ERROR",
            GatewayError::Forbidden { .. } => "STRAVIA_FORBIDDEN",
            GatewayError::ConcurrencyLimitExceeded => "STRAVIA_CONCURRENCY_LIMIT",
            GatewayError::ModelNotFound { .. } => "STRAVIA_NOT_FOUND",
            GatewayError::ProtocolUnsupported { .. } => "STRAVIA_PROTOCOL_UNSUPPORTED",
            GatewayError::ProtocolLossyRejected { .. } => "STRAVIA_PROTOCOL_LOSSY_REJECTED",
            GatewayError::UpstreamStatus { .. } => "STRAVIA_UPSTREAM_ERROR",
            GatewayError::UpstreamTimeout { .. } => "STRAVIA_UPSTREAM_TIMEOUT",
            GatewayError::StreamParseError { .. } => "STRAVIA_STREAM_PARSE_ERROR",
            GatewayError::ClientCancelled => "STRAVIA_CLIENT_CANCELLED",
            GatewayError::ProviderUnavailable { .. } => "STRAVIA_SERVICE_UNAVAILABLE",
            GatewayError::Internal { .. } => "STRAVIA_INTERNAL_ERROR",
        }
    }

    /// Human-readable description suitable for the `message` field.
    pub fn message(&self) -> String {
        let message: String = match self {
            GatewayError::BadRequest { msg, .. } => msg.clone(),
            GatewayError::Unauthorized { reason } => {
                format!("authentication failed: {}", reason.as_str())
            }
            GatewayError::Forbidden { reason } => match reason {
                AccessDenial::ModelNotAllowed => "access to this model is not permitted".into(),
                AccessDenial::IpDenied => "request origin is blocked".into(),
                AccessDenial::Custom(msg) => msg.clone(),
            },
            GatewayError::ConcurrencyLimitExceeded => "Principal Concurrency Limit is full.".into(),
            GatewayError::ModelNotFound { model } => {
                format!("no model found for model: {model}")
            }
            GatewayError::ProtocolUnsupported { ingress, egress } => {
                format!("cannot translate {ingress} to {egress}")
            }
            GatewayError::ProtocolLossyRejected { lost } => {
                format!(
                    "translation would drop {} field(s): {}",
                    lost.len(),
                    lost.join(", ")
                )
            }
            GatewayError::UpstreamStatus {
                provider,
                status,
                body,
            } => {
                if let Some(b) = body {
                    format!(
                        "upstream {provider} returned {status}: {}",
                        redacted_payload(b)
                    )
                } else {
                    format!("upstream {provider} returned {status}")
                }
            }
            GatewayError::UpstreamTimeout { phase } => {
                format!("upstream timeout during {}", phase.as_str())
            }
            GatewayError::StreamParseError {
                provider,
                raw_chunk,
            } => {
                if let Some(chunk) = raw_chunk {
                    format!(
                        "stream parse error from {provider}: {}",
                        redacted_payload(chunk)
                    )
                } else {
                    format!("stream parse error from {provider}")
                }
            }
            GatewayError::ClientCancelled => "client disconnected before response completed".into(),
            GatewayError::ProviderUnavailable { provider, reason } => {
                format!("provider {provider} unavailable: {reason}")
            }
            GatewayError::Internal { source } => {
                format!("internal error: {source}")
            }
        };
        crate::interaction_observation::redaction::redact_text(&message)
    }

    /// Whether the gateway may safely retry this request with another target.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            GatewayError::UpstreamStatus { status, .. } if matches!(status, 429 | 500 | 502 | 503 | 504)
        ) || matches!(
            self,
            GatewayError::UpstreamTimeout { .. }
                | GatewayError::StreamParseError { .. }
                | GatewayError::ProviderUnavailable { .. }
        )
    }

    /// Render to an axum `Response` with the canonical JSON body.
    ///
    /// `request_id` is included when available.
    pub fn render(&self, request_id: Option<&str>) -> Response {
        let status = self.http_status();
        let numeric_status = status.as_u16();
        let mut error_obj = serde_json::json!({
            "message": self.message(),
            "type": self.stable_code(),
            "code": numeric_status,
        });
        if let Some(id) = request_id {
            error_obj["request_id"] = serde_json::Value::String(id.to_string());
        }
        (status, Json(serde_json::json!({ "error": error_obj }))).into_response()
    }

    /// Render with a `RequestContext` — injects `request_id` automatically.
    pub fn render_with_ctx(&self, ctx: &crate::proxy::context::RequestContext) -> Response {
        self.render(Some(&ctx.request_id))
    }

    /// Convenience: build an `Internal` variant from any `anyhow::Error`.
    pub fn internal(source: anyhow::Error) -> Self {
        GatewayError::Internal { source }
    }

    /// Convenience: build a `BadRequest` from a static code + dynamic message.
    pub fn bad_request(code: &'static str, msg: impl Into<String>) -> Self {
        GatewayError::BadRequest {
            code,
            msg: msg.into(),
        }
    }

    /// Convenience: build a `ProviderUnavailable` error.
    pub fn provider_unavailable(provider: impl Into<String>, reason: impl Into<String>) -> Self {
        GatewayError::ProviderUnavailable {
            provider: provider.into(),
            reason: reason.into(),
        }
    }

    /// Convenience: build an `UpstreamStatus` error.
    pub fn upstream_status(provider: impl Into<String>, status: u16, body: Option<String>) -> Self {
        GatewayError::UpstreamStatus {
            provider: provider.into(),
            status,
            body,
        }
    }
}

fn redacted_payload(payload: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(mut value) => {
            crate::interaction_observation::redaction::redact_value(&mut value);
            value.to_string()
        }
        Err(_) => crate::interaction_observation::redaction::redact_text(payload),
    }
}

impl std::fmt::Debug for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug 也会进入进程日志，不能绕过对上游正文与 URL 的统一脱敏。
        f.debug_struct("GatewayError")
            .field("code", &self.stable_code())
            .field("message", &self.message())
            .finish()
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.stable_code(), self.message())
    }
}

impl std::error::Error for GatewayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let GatewayError::Internal { source } = self {
            source.source()
        } else {
            None
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        self.render(None)
    }
}

// ── From impls for common error types ────────────────────────────────────────

impl From<anyhow::Error> for GatewayError {
    fn from(e: anyhow::Error) -> Self {
        GatewayError::Internal { source: e }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_request_renders_400() {
        let err = GatewayError::bad_request("model_required", "model is required");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.stable_code(), "STRAVIA_BAD_REQUEST");
        assert!(!err.retryable());
    }

    #[test]
    fn upstream_timeout_is_retryable() {
        let err = GatewayError::UpstreamTimeout {
            phase: TimeoutPhase::FirstByte,
        };
        assert!(err.retryable());
        assert_eq!(err.http_status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn upstream_429_is_retryable() {
        let err = GatewayError::upstream_status("openai", 429, None);
        assert!(err.retryable());
    }

    #[test]
    fn upstream_400_is_not_retryable() {
        let err = GatewayError::upstream_status("openai", 400, None);
        assert!(!err.retryable());
    }

    #[tokio::test]
    async fn render_includes_request_id() {
        let err = GatewayError::ModelNotFound {
            model: "gpt-4".into(),
        };
        let resp = err.render(Some("req-abc-123"));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["request_id"], "req-abc-123");
    }

    #[tokio::test]
    async fn upstream_error_credentials_do_not_escape_http_or_debug_rendering() {
        let error = GatewayError::upstream_status(
            "local-provider",
            429,
            Some(serde_json::json!({
                "error": {
                    "api_key": "nested-key-sentinel",
                    "password": "password with spaces sentinel",
                    "message": "request quota exhausted",
                },
                "url": "https://userinfo-sentinel:password-sentinel@example.test/path?access_token=query-sentinel&mode=safe"
            }).to_string()),
        );
        let debug = format!("{error:?}");
        let response = error.render(Some("request-redaction"));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(error.retryable());
        let body = axum::body::to_bytes(response.into_body(), 16_384)
            .await
            .unwrap();
        let http = String::from_utf8(body.to_vec()).unwrap();

        for rendered in [&http, &debug] {
            for credential in [
                "nested-key-sentinel",
                "password with spaces sentinel",
                "userinfo-sentinel",
                "password-sentinel",
                "query-sentinel",
            ] {
                assert!(
                    !rendered.contains(credential),
                    "credential escaped rendering"
                );
            }
            assert!(rendered.contains("request quota exhausted"));
            assert!(rendered.contains("mode=safe"));
        }
    }

    #[test]
    fn stable_codes_are_stable() {
        assert_eq!(
            GatewayError::Internal {
                source: anyhow::anyhow!("oops")
            }
            .stable_code(),
            "STRAVIA_INTERNAL_ERROR"
        );
    }

    #[test]
    fn concurrency_limit_exceeded_is_a_non_retryable_429_with_a_stable_code() {
        let response = GatewayError::ConcurrencyLimitExceeded.render(None);

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            GatewayError::ConcurrencyLimitExceeded.stable_code(),
            "STRAVIA_CONCURRENCY_LIMIT"
        );
        assert!(!GatewayError::ConcurrencyLimitExceeded.retryable());
        assert!(response.headers().get("retry-after").is_none());
    }
}
