//! Adapter-agnostic upstream HTTP and Responses WebSocket transports.
//!
//! URL construction and authentication remain at the provider call site via
//! `VendorRegistry::resolve` and `VendorExtension`. `ProxyClient` receives
//! fully built URLs, headers, and canonical request bodies; this module owns
//! the network call plus process-local Responses WebSocket capability,
//! connection-affinity, and lifetime state.

use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;

pub(crate) const TRANSPORT_DIAGNOSTIC_MAX_CHARS: usize = 4096;
pub(crate) const TRANSPORT_DIAGNOSTIC_TRUNCATED: &str = "… [truncated]";

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct TransportDiagnostic {
    pub(crate) category: String,
    pub(crate) stage: String,
    pub(crate) has_received_response_event: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) websocket_close_code: Option<String>,
    pub(crate) cause: String,
}

impl TransportDiagnostic {
    pub(crate) fn from_reqwest(
        stage: &'static str,
        has_received_response_event: bool,
        http_status: Option<u16>,
        error: &reqwest::Error,
    ) -> Self {
        let category = if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connect"
        } else if error.is_redirect() {
            "redirect"
        } else if error.is_status() {
            "http_status"
        } else if error.is_body() {
            "body"
        } else if error.is_decode() {
            "decode"
        } else if error.is_request() {
            "request"
        } else {
            "transport"
        };
        Self::from_error(
            category,
            stage,
            has_received_response_event,
            http_status,
            None,
            error,
        )
    }

    pub(crate) fn from_error(
        category: impl Into<String>,
        stage: impl Into<String>,
        has_received_response_event: bool,
        http_status: Option<u16>,
        websocket_close_code: Option<String>,
        error: &(dyn std::error::Error + 'static),
    ) -> Self {
        let mut cause = error.to_string();
        let mut source = error.source();
        while let Some(next) = source {
            cause.push_str("; caused by: ");
            cause.push_str(&next.to_string());
            source = next.source();
        }
        Self::from_message(
            category,
            stage,
            has_received_response_event,
            http_status,
            websocket_close_code,
            cause,
        )
    }

    pub(crate) fn from_message(
        category: impl Into<String>,
        stage: impl Into<String>,
        has_received_response_event: bool,
        http_status: Option<u16>,
        websocket_close_code: Option<String>,
        cause: impl AsRef<str>,
    ) -> Self {
        let cause = crate::interaction_observation::redact_text(cause.as_ref());
        Self {
            category: category.into(),
            stage: stage.into(),
            has_received_response_event,
            http_status,
            websocket_close_code,
            cause: truncate_transport_diagnostic(cause),
        }
    }
}

impl std::fmt::Display for TransportDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut rendered = format!(
            "transport category={}; stage={}; has_received_response_event={}",
            self.category, self.stage, self.has_received_response_event
        );
        if let Some(status) = self.http_status {
            rendered.push_str(&format!("; http_status={status}"));
        }
        if let Some(code) = &self.websocket_close_code {
            rendered.push_str("; websocket_close_code=");
            rendered.push_str(code);
        }
        rendered.push_str("; cause: ");
        rendered.push_str(&self.cause);
        formatter.write_str(&truncate_transport_diagnostic(rendered))
    }
}

fn truncate_transport_diagnostic(value: String) -> String {
    if value.chars().count() <= TRANSPORT_DIAGNOSTIC_MAX_CHARS {
        return value;
    }
    let keep = TRANSPORT_DIAGNOSTIC_MAX_CHARS - TRANSPORT_DIAGNOSTIC_TRUNCATED.chars().count();
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push_str(TRANSPORT_DIAGNOSTIC_TRUNCATED);
    truncated
}

pub(crate) struct UpstreamTransportError {
    diagnostic: TransportDiagnostic,
    source: reqwest::Error,
}

impl UpstreamTransportError {
    pub(crate) fn diagnostic(&self) -> &TransportDiagnostic {
        &self.diagnostic
    }
}

impl std::fmt::Debug for UpstreamTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamTransportError")
            .field("diagnostic", &self.diagnostic)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for UpstreamTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.diagnostic.fmt(formatter)
    }
}

impl std::error::Error for UpstreamTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone)]
pub struct ProxyClient {
    pub http: reqwest::Client,
    pub responses_websocket: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
#[error("error decoding response body: {source}")]
pub struct UpstreamResponseDecodeError {
    pub source: serde_json::Error,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
}

impl UpstreamResponseDecodeError {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl ProxyClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            responses_websocket: http.clone(),
            http,
        }
    }

    pub fn with_responses_websocket(
        http: reqwest::Client,
        responses_websocket: reqwest::Client,
    ) -> Self {
        Self {
            http,
            responses_websocket,
        }
    }

    pub async fn call_non_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Value,
    ) -> Result<(Value, u16, HeaderMap, bytes::Bytes)> {
        self.call_non_stream_raw(url, headers, bytes::Bytes::from(serde_json::to_vec(&body)?))
            .await
    }

    pub(crate) async fn call_non_stream_raw(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<(Value, u16, HeaderMap, bytes::Bytes)> {
        headers.entry(reqwest::header::CONTENT_TYPE).or_insert(
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|source| {
                let stage = if source.is_connect() {
                    "connect"
                } else {
                    "send"
                };
                UpstreamTransportError {
                    diagnostic: TransportDiagnostic::from_reqwest(stage, false, None, &source),
                    source,
                }
            })?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .map_err(|source| UpstreamTransportError {
                diagnostic: TransportDiagnostic::from_reqwest(
                    "receive",
                    false,
                    Some(status),
                    &source,
                ),
                source,
            })?;
        let json: Value =
            serde_json::from_slice(&bytes).map_err(|source| UpstreamResponseDecodeError {
                source,
                status,
                headers: resp_headers.clone(),
                body: bytes.clone(),
            })?;
        Ok((json, status, resp_headers, bytes))
    }

    pub async fn call_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Value,
    ) -> Result<(reqwest::Response, u16)> {
        self.call_stream_raw(url, headers, bytes::Bytes::from(serde_json::to_vec(&body)?))
            .await
    }

    pub(crate) async fn call_stream_raw(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<(reqwest::Response, u16)> {
        headers.entry(reqwest::header::CONTENT_TYPE).or_insert(
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|source| {
                let stage = if source.is_connect() {
                    "connect"
                } else {
                    "send"
                };
                UpstreamTransportError {
                    diagnostic: TransportDiagnostic::from_reqwest(stage, false, None, &source),
                    source,
                }
            })?;
        let status = resp.status().as_u16();
        Ok((resp, status))
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("outer transport wrapper")]
    struct WrappedError {
        #[source]
        source: CauseError,
    }

    #[derive(Debug, thiserror::Error)]
    #[error(
        "socket failed at https://nested-user:nested-password@example.test/v1?api_key=nested-secret"
    )]
    struct CauseError;

    #[test]
    fn transport_diagnostic_preserves_wrapped_cause_and_redacts_nested_urls() {
        let diagnostic = TransportDiagnostic::from_error(
            "connect",
            "connect",
            false,
            None,
            None,
            &WrappedError { source: CauseError },
        );
        let rendered = diagnostic.to_string();

        assert!(rendered.contains("outer transport wrapper"));
        assert!(rendered.contains("socket failed at"));
        assert!(rendered.contains("has_received_response_event=false"));
        assert!(rendered.contains("api_key=***"));
        for secret in ["nested-user", "nested-password", "nested-secret"] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
    }

    #[test]
    fn transport_diagnostic_redacts_complete_chain_before_explicit_truncation() {
        #[derive(Debug, thiserror::Error)]
        #[error("{prefix} https://example.test/v1?token={secret}")]
        struct LongCause {
            prefix: String,
            secret: String,
        }

        let secret = "secret-fragment-".repeat(64);
        let diagnostic = TransportDiagnostic::from_error(
            "receive",
            "receive",
            true,
            Some(200),
            None,
            &LongCause {
                prefix: "x".repeat(4000),
                secret: secret.clone(),
            },
        );
        let rendered = diagnostic.to_string();

        assert!(diagnostic.cause.contains("token=***"));
        assert!(!diagnostic.cause.ends_with(TRANSPORT_DIAGNOSTIC_TRUNCATED));
        assert!(!diagnostic.cause.contains(&secret));
        assert_eq!(rendered.chars().count(), TRANSPORT_DIAGNOSTIC_MAX_CHARS);
        assert!(rendered.ends_with(TRANSPORT_DIAGNOSTIC_TRUNCATED));
        assert!(!rendered.contains("secret-fragment-"));
    }

    #[tokio::test]
    async fn truncated_http_body_reports_receive_stage_status_without_url_secrets() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind disconnect fixture");
        let address = listener.local_addr().expect("disconnect fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\nconnection: close\r\n\r\n{\"partial\":true}",
                )
                .await
                .expect("write truncated response");
        });
        let url = format!("http://url-user:url-password@{address}/v1/responses?api_key=url-secret");

        let error = ProxyClient::new(reqwest::Client::new())
            .call_non_stream(&url, HeaderMap::new(), serde_json::json!({}))
            .await
            .expect_err("truncated body must fail");
        let transport = error
            .downcast_ref::<UpstreamTransportError>()
            .expect("body failure retains transport context");
        let diagnostic = transport.diagnostic();
        let rendered = diagnostic.to_string();

        assert!(diagnostic.cause.contains("caused by:"), "{rendered}");
        assert_eq!(diagnostic.stage, "receive");
        assert_eq!(diagnostic.http_status, Some(200));
        assert!(!diagnostic.has_received_response_event);
        for secret in ["url-user", "url-password", "url-secret"] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
    }
}

mod websocket;
pub(crate) use websocket::*;
