use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{FutureExt, SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, LOCATION, RETRY_AFTER};
use reqwest::{Client, Method, Response, Url};
use reqwest_websocket::{Message, Upgrade, WebSocket};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostWebSocket, HttpRequest, WebSocketMessage,
};
use stravia_vendor_sdk::{ErrorKind, TransportFailure};
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::task::{AbortHandle, JoinHandle};

use crate::interaction_observation::{RunEvent, RunObserver};

use super::lifecycle::VendorOperation;

const MAX_REDIRECTS: usize = 10;
const MAX_IDLE_WEBSOCKETS: usize = 64;
const MAX_WEBSOCKET_AGE: Duration = Duration::from_secs(60 * 60);

/// Host-owned WebSocket transports retained between plugin operations. A guest
/// task is never retained: checked-in entries are inert sockets and every
/// checkout is re-admitted against the current operation's origin snapshot.
/// One pool-owned deadline task expires all idle entries; reuse never creates
/// another task or moves a connection's original deadline.
#[derive(Default)]
pub(crate) struct VendorWebSocketPool {
    idle: parking_lot::Mutex<Vec<PooledWebSocket>>,
    expiration_changed: Arc<Notify>,
    expiration_task: parking_lot::Mutex<Option<AbortHandle>>,
}

struct PooledWebSocket {
    scope_key: String,
    affinity: String,
    requested_url: String,
    actual_url: String,
    requested_headers: [u8; 32],
    actual_headers: [u8; 32],
    requested_protocols: Vec<String>,
    actual_protocols: Vec<String>,
    socket: WebSocket,
    connected_at: tokio::time::Instant,
    parked_at: tokio::time::Instant,
}

struct WebSocketPoolIdentity {
    scope_key: String,
    affinity: String,
    requested_url: String,
    actual_url: String,
    requested_headers: [u8; 32],
    actual_headers: [u8; 32],
    requested_protocols: Vec<String>,
    actual_protocols: Vec<String>,
}

impl VendorWebSocketPool {
    fn take(
        &self,
        scope_key: &str,
        affinity: &str,
        requested_url: &str,
        requested_headers: &[u8; 32],
        requested_protocols: &[String],
    ) -> Option<PooledWebSocket> {
        let now = tokio::time::Instant::now();
        let mut idle = self.idle.lock();
        idle.retain(|entry| entry.connected_at + MAX_WEBSOCKET_AGE > now);
        let index = idle.iter().position(|entry| {
            entry.scope_key == scope_key
                && entry.affinity == affinity
                && entry.requested_url == requested_url
                && entry.actual_url == requested_url
                && entry.requested_headers == *requested_headers
                && entry.actual_headers == *requested_headers
                && entry.requested_protocols == requested_protocols
                && entry.actual_protocols == requested_protocols
        })?;
        Some(idle.swap_remove(index))
    }

    fn park(
        self: &Arc<Self>,
        identity: WebSocketPoolIdentity,
        socket: WebSocket,
        connected_at: tokio::time::Instant,
    ) -> bool {
        let now = tokio::time::Instant::now();
        if connected_at + MAX_WEBSOCKET_AGE <= now {
            return false;
        }
        {
            let mut idle = self.idle.lock();
            idle.retain(|entry| entry.connected_at + MAX_WEBSOCKET_AGE > now);
            if idle.len() >= MAX_IDLE_WEBSOCKETS {
                let oldest = idle
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.parked_at)
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                idle.swap_remove(oldest);
            }
            idle.push(PooledWebSocket {
                scope_key: identity.scope_key,
                affinity: identity.affinity,
                requested_url: identity.requested_url,
                actual_url: identity.actual_url,
                requested_headers: identity.requested_headers,
                actual_headers: identity.actual_headers,
                requested_protocols: identity.requested_protocols,
                actual_protocols: identity.actual_protocols,
                socket,
                connected_at,
                parked_at: now,
            });
        }
        self.ensure_expiration_task();
        self.expiration_changed.notify_one();
        true
    }

    fn ensure_expiration_task(self: &Arc<Self>) {
        let mut task = self.expiration_task.lock();
        if task.is_some() {
            return;
        }
        let weak_pool = Arc::downgrade(self);
        let expiration_changed = Arc::clone(&self.expiration_changed);
        *task = Some(
            tokio::spawn(async move {
                loop {
                    let Some(pool) = weak_pool.upgrade() else {
                        return;
                    };
                    let expires_at = pool
                        .idle
                        .lock()
                        .iter()
                        .map(|entry| entry.connected_at + MAX_WEBSOCKET_AGE)
                        .min();
                    drop(pool);
                    let Some(expires_at) = expires_at else {
                        expiration_changed.notified().await;
                        continue;
                    };
                    tokio::select! {
                        _ = tokio::time::sleep_until(expires_at) => {
                            let Some(pool) = weak_pool.upgrade() else {
                                return;
                            };
                            let now = tokio::time::Instant::now();
                            pool.idle
                                .lock()
                                .retain(|entry| entry.connected_at + MAX_WEBSOCKET_AGE > now);
                        }
                        _ = expiration_changed.notified() => {}
                    }
                }
            })
            .abort_handle(),
        );
    }
}

impl Drop for VendorWebSocketPool {
    fn drop(&mut self) {
        if let Some(task) = self.expiration_task.get_mut().take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub(crate) struct VendorNetwork {
    http: Client,
    websocket: Client,
    origins: Arc<BTreeSet<String>>,
    operation: Arc<VendorOperation>,
    cancellation: CancellationToken,
    protocol: String,
    observer: Option<RunObserver>,
    model_turn_id: Option<String>,
    attempt_id: Option<String>,
    websocket_pool: Option<Arc<VendorWebSocketPool>>,
    websocket_scope_key: Option<String>,
    websocket_affinity: Option<String>,
    response_continuation_available: Arc<AtomicBool>,
}

struct WireDiagnostic<'a> {
    direction: &'a str,
    transport: &'static str,
    message_type: &'a str,
    url: &'a Url,
    status_code: Option<u16>,
    headers: Option<&'a HeaderMap>,
}

impl VendorNetwork {
    /// Clients must have automatic redirects disabled. Every redirect hop is
    /// independently authorized here before any bytes leave the process.
    pub(crate) fn new(
        http: Client,
        websocket: Client,
        origins: BTreeSet<String>,
        operation: Arc<VendorOperation>,
        cancellation: CancellationToken,
        mut protocol: String,
    ) -> Self {
        // Normalize registered aliases to stable wire protocol identities; custom protocols stay opaque.
        if let Some(endpoint) = ProtocolRegistry::global().resolve_alias(&protocol) {
            protocol.clear();
            let _ = write!(protocol, "{endpoint}");
        }
        Self {
            http,
            websocket,
            origins: Arc::new(origins),
            operation,
            cancellation,
            protocol,
            observer: None,
            model_turn_id: None,
            attempt_id: None,
            websocket_pool: None,
            websocket_scope_key: None,
            websocket_affinity: None,
            response_continuation_available: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn with_observer(mut self, observer: Option<RunObserver>) -> Self {
        self.observer = observer;
        self
    }

    pub(crate) fn with_observation_scope(
        mut self,
        model_turn_id: Option<String>,
        attempt_id: Option<String>,
    ) -> Self {
        self.model_turn_id = model_turn_id;
        self.attempt_id = attempt_id;
        self
    }

    pub(crate) fn with_websocket_pool(
        mut self,
        pool: Arc<VendorWebSocketPool>,
        scope_key: String,
        affinity: Option<String>,
    ) -> Self {
        self.websocket_pool = Some(pool);
        self.websocket_scope_key = Some(scope_key);
        self.websocket_affinity = affinity.filter(|value| !value.is_empty());
        self
    }

    pub(crate) fn with_response_continuation_available(
        mut self,
        response_continuation_available: Arc<AtomicBool>,
    ) -> Self {
        self.response_continuation_available = response_continuation_available;
        self
    }

    pub(crate) fn ensure_current(&self) -> Result<(), HostFailure> {
        if self.cancellation.is_cancelled() {
            return Err(cancelled());
        }
        self.operation.ensure_current().map_err(|_| cancelled())
    }

    async fn cancelled(&self) {
        tokio::select! {
            _ = self.cancellation.cancelled() => {},
            _ = self.operation.cancellation().cancelled() => {},
        }
    }

    fn url(&self, value: &str, websocket: bool) -> Result<Url, HostFailure> {
        self.ensure_current()?;
        let url = Url::parse(value).map_err(|_| invalid("invalid upstream URL"))?;
        let scheme_allowed = if websocket {
            matches!(url.scheme(), "ws" | "wss")
        } else {
            matches!(url.scheme(), "http" | "https")
        };
        if !scheme_allowed || !url.username().is_empty() || url.password().is_some() {
            return Err(invalid(
                "upstream URL has an unauthorized scheme or user information",
            ));
        }
        if !self.origins.contains(&url.origin().ascii_serialization()) {
            return Err(invalid(
                "upstream origin is not authorized for this operation",
            ));
        }
        Ok(url)
    }

    fn protect_headers(&self, headers: &HeaderMap) {
        let Some(observer) = &self.observer else {
            return;
        };
        observer.protect_secrets(headers.iter().filter_map(|(name, value)| {
            let name = name.as_str();
            let sensitive = matches!(
                name,
                "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
            ) || name.contains("token")
                || name.contains("secret")
                || name.contains("api-key")
                || name.contains("api_key")
                || name.ends_with("-key")
                || name.starts_with("x-auth-");
            sensitive
                .then(|| value.to_str().ok())
                .flatten()
                .filter(|value| !value.is_empty())
        }));
    }

    fn wire(&self, diagnostic: WireDiagnostic<'_>, payload: impl FnOnce() -> Value) {
        let Some(observer) = &self.observer else {
            return;
        };
        if let Some(headers) = diagnostic.headers {
            self.protect_headers(headers);
        }
        observer.record_debug(|| RunEvent::Wire {
            direction: diagnostic.direction.to_owned(),
            transport: diagnostic.transport.to_owned(),
            protocol: self.protocol.clone(),
            message_type: diagnostic.message_type.to_owned(),
            model_turn_id: self.model_turn_id.clone(),
            attempt_id: self.attempt_id.clone(),
            status_code: diagnostic.status_code,
            url: Some(diagnostic.url.as_str().to_owned()),
            headers: diagnostic.headers.map(headers_value).unwrap_or(Value::Null),
            payload: payload(),
        });
    }

    pub(crate) fn http_start(
        &self,
        request: HttpRequest,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let url = self.url(&request.url, false)?;
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|_| invalid("invalid HTTP method"))?;
        if method == Method::CONNECT {
            return Err(invalid("HTTP CONNECT is not a vendor operation"));
        }
        let headers = request_headers(request.headers, &url)?;
        let network = self.clone();
        let sending = network.clone();
        // The task owns the operation lease only while the imported response
        // resource exists. Resource drop aborts the task before version drain.
        let task = tokio::spawn(async move {
            sending
                .send_http(method, url, headers, Bytes::from(request.body))
                .await
        });
        let abort = task.abort_handle();
        Ok(Arc::new(PendingHttp {
            network,
            state: Mutex::new(HttpState::Pending(task)),
            abort,
        }))
    }

    async fn send_http(
        &self,
        mut method: Method,
        mut url: Url,
        mut headers: HeaderMap,
        mut body: Bytes,
    ) -> Result<Response, HostFailure> {
        for redirects in 0..=MAX_REDIRECTS {
            self.ensure_current()?;
            self.wire(
                WireDiagnostic {
                    direction: "upstream_request",
                    transport: "http",
                    message_type: "request",
                    url: &url,
                    status_code: None,
                    headers: Some(&headers),
                },
                || bytes_value(&body),
            );
            let response = tokio::select! {
                _ = self.cancelled() => return Err(cancelled()),
                result = self.http.request(method.clone(), url.clone())
                    .headers(headers.clone()).body(body.clone()).send() => result.map_err(transport_failure)?,
            };
            let status = response.status().as_u16();
            self.wire(
                WireDiagnostic {
                    direction: "upstream_response",
                    transport: "http",
                    message_type: "response_headers",
                    url: &url,
                    status_code: Some(status),
                    headers: Some(response.headers()),
                },
                || Value::Null,
            );
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response.headers().get(LOCATION) else {
                return Ok(response);
            };
            if redirects == MAX_REDIRECTS {
                return Err(invalid("upstream redirect limit exceeded"));
            }
            let next = url
                .join(
                    location
                        .to_str()
                        .map_err(|_| invalid("invalid redirect location"))?,
                )
                .map_err(|_| invalid("invalid redirect URL"))?;
            let next = self.url(next.as_str(), false)?;
            if matches!(status, 301..=303) && method != Method::GET && method != Method::HEAD {
                method = Method::GET;
                body = Bytes::new();
                headers.remove(reqwest::header::CONTENT_TYPE);
                headers.remove(reqwest::header::CONTENT_ENCODING);
            }
            if next.origin() != url.origin() {
                // Bodies and arbitrary custom headers can carry credentials,
                // not just Authorization and Cookie.
                if !body.is_empty() {
                    return Err(invalid(
                        "cross-origin redirect cannot forward a request body",
                    ));
                }
                headers.clear();
            }
            url = next;
        }
        unreachable!("redirect loop returns at its bound")
    }

    pub(crate) async fn ws_connect(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        protocols: Vec<String>,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        let mut url = self.url(&url, true)?;
        let requested_url = url.as_str().to_owned();
        let mut headers = request_headers(headers, &url)?;
        let requested_headers = canonical_headers(&headers);
        let requested_protocols = protocols.clone();
        let mut protocols = protocols;

        if let (Some(pool), Some(scope_key), Some(affinity)) = (
            self.websocket_pool.as_ref(),
            self.websocket_scope_key.as_deref(),
            self.websocket_affinity.as_deref(),
        ) {
            while let Some(mut entry) = pool.take(
                scope_key,
                affinity,
                &requested_url,
                &requested_headers,
                &requested_protocols,
            ) {
                if self.url(&entry.actual_url, true).is_err()
                    || !drain_idle_frames(&mut entry.socket)
                {
                    continue;
                }
                let wire_url = entry.actual_url.clone();
                return Ok(Arc::new(ScopedWebSocket {
                    network: self.clone(),
                    socket: Mutex::new(Some(entry.socket)),
                    wire_url,
                    pool_identity: Some(WebSocketPoolIdentity {
                        scope_key: entry.scope_key,
                        affinity: entry.affinity,
                        requested_url: entry.requested_url,
                        actual_url: entry.actual_url,
                        requested_headers: entry.requested_headers,
                        actual_headers: entry.actual_headers,
                        requested_protocols: entry.requested_protocols,
                        actual_protocols: entry.actual_protocols,
                    }),
                    connected_at: entry.connected_at,
                    reused: true,
                    application_message_seen: AtomicBool::new(false),
                    reusable: AtomicBool::new(true),
                    explicitly_released: AtomicBool::new(false),
                }));
            }
        }

        for redirects in 0..=MAX_REDIRECTS {
            self.ensure_current()?;
            self.wire(
                WireDiagnostic {
                    direction: "upstream_request",
                    transport: "websocket",
                    message_type: "handshake_request",
                    url: &url,
                    status_code: None,
                    headers: Some(&headers),
                },
                || Value::Null,
            );
            let response = tokio::select! {
                _ = self.cancelled() => return Err(cancelled()),
                result = self.websocket.get(url.clone()).version(reqwest::Version::HTTP_11)
                    .headers(headers.clone())
                    .upgrade()
                    .web_socket_config(
                        tungstenite::protocol::WebSocketConfig::default()
                            .max_message_size(None)
                            .max_frame_size(None),
                    )
                    .protocols(protocols.clone())
                    .send() => result.map_err(websocket_transport_failure)?,
            };
            let status = response.status().as_u16();
            self.wire(
                WireDiagnostic {
                    direction: "upstream_response",
                    transport: "websocket",
                    message_type: "handshake_response",
                    url: &url,
                    status_code: Some(status),
                    headers: Some(response.headers()),
                },
                || Value::Null,
            );
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if redirects == MAX_REDIRECTS {
                    return Err(invalid("upstream redirect limit exceeded"));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| invalid("WebSocket redirect lacks a valid location"))?;
                let next = url
                    .join(location)
                    .map_err(|_| invalid("invalid redirect URL"))?;
                let next = self.url(next.as_str(), true)?;
                if next.origin() != url.origin() {
                    headers.clear();
                    protocols.clear();
                }
                url = next;
                continue;
            }
            if status != 101 {
                let retry_after = retry_after(response.headers());
                let mut response = response.into_inner();
                let mut body = Vec::new();
                loop {
                    let chunk = tokio::select! {
                        _ = self.cancelled() => return Err(cancelled()),
                        result = response.chunk() => result.map_err(|error| {
                            tracing::debug!(
                                error = %crate::interaction_observation::redact_text(&error.to_string()),
                                "vendor WebSocket handshake body read failed"
                            );
                            HostFailure::upstream_transport(
                                Some(kind_without_body(status)),
                                Some(status),
                                retry_after,
                                TransportFailure::Websocket,
                                "WebSocket handshake body read failed",
                            )
                        })?,
                    };
                    let Some(chunk) = chunk else { break };
                    body.extend_from_slice(&chunk);
                }
                self.wire(
                    WireDiagnostic {
                        direction: "upstream_response",
                        transport: "websocket",
                        message_type: "handshake_body",
                        url: &url,
                        status_code: Some(status),
                        headers: None,
                    },
                    || bytes_value(&body),
                );
                return Err(match status {
                    401 | 403 | 408 | 429 | 500..=599 => {
                        let value = serde_json::from_slice::<Value>(&body).ok();
                        HostFailure::upstream(
                            Some(AiError::kind_from_status(status, value.as_ref())),
                            Some(status),
                            retry_after,
                            "upstream did not accept the WebSocket handshake",
                        )
                    }
                    200..=399 | 404 | 405 | 426 => HostFailure::upstream_transport(
                        Some(AiErrorKind::ServiceUnavailable),
                        Some(status),
                        retry_after,
                        TransportFailure::Websocket,
                        "upstream did not accept the WebSocket handshake",
                    ),
                    _ => HostFailure {
                        kind: ErrorKind::Invalid,
                        message: "upstream did not accept the WebSocket handshake".into(),
                        upstream_status: Some(status),
                    },
                });
            }
            let socket = tokio::select! {
                _ = self.cancelled() => return Err(cancelled()),
                result = response.into_websocket() => result.map_err(websocket_transport_failure)?,
            };
            let connected_at = tokio::time::Instant::now();
            let actual_headers = canonical_headers(&headers);
            let pool_identity = match (
                self.websocket_pool.as_ref(),
                self.websocket_scope_key.as_ref(),
                self.websocket_affinity.as_ref(),
            ) {
                (Some(_), Some(scope_key), Some(affinity))
                    if url.as_str() == requested_url.as_str()
                        && actual_headers == requested_headers
                        && protocols == requested_protocols =>
                {
                    Some(WebSocketPoolIdentity {
                        scope_key: scope_key.clone(),
                        affinity: affinity.clone(),
                        requested_url: requested_url.clone(),
                        actual_url: url.as_str().to_owned(),
                        requested_headers,
                        actual_headers,
                        requested_protocols: requested_protocols.clone(),
                        actual_protocols: protocols.clone(),
                    })
                }
                _ => None,
            };
            return Ok(Arc::new(ScopedWebSocket {
                network: self.clone(),
                socket: Mutex::new(Some(socket)),
                wire_url: url.as_str().to_owned(),
                pool_identity,
                connected_at,
                reused: false,
                application_message_seen: AtomicBool::new(false),
                reusable: AtomicBool::new(true),
                explicitly_released: AtomicBool::new(false),
            }));
        }
        unreachable!("redirect loop returns at its bound")
    }
}

fn request_headers(values: Vec<(String, String)>, url: &Url) -> Result<HeaderMap, HostFailure> {
    let mut headers = HeaderMap::with_capacity(values.len());
    for (name, value) in values {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid("invalid upstream header name"))?;
        if matches!(
            name.as_str(),
            "connection"
                | "transfer-encoding"
                | "content-length"
                | "proxy-authorization"
                | "proxy-connection"
                | "upgrade"
        ) {
            return Err(invalid(
                "upstream header is controlled by the host transport",
            ));
        }
        if name == reqwest::header::HOST {
            let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
            if !value.eq_ignore_ascii_case(authority) {
                return Err(invalid(
                    "Host header cannot override the authorized upstream",
                ));
            }
        }
        let value =
            HeaderValue::from_str(&value).map_err(|_| invalid("invalid upstream header value"))?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn canonical_headers(headers: &HeaderMap) -> [u8; 32] {
    let mut values = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .collect::<Vec<_>>();
    values.sort();
    let mut digest = Sha256::new();
    digest.update(b"stravia-vendor-websocket-headers-v1\0");
    for (name, value) in values {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    digest.finalize().into()
}

enum HttpState {
    Pending(JoinHandle<Result<Response, HostFailure>>),
    Ready(Response),
    Failed(HostFailure),
}

struct PendingHttp {
    network: VendorNetwork,
    state: Mutex<HttpState>,
    abort: AbortHandle,
}

impl Drop for PendingHttp {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

impl PendingHttp {
    async fn response(&self) -> Result<MutexGuard<'_, HttpState>, HostFailure> {
        self.network.ensure_current()?;
        let mut state = tokio::select! {
            _ = self.network.cancelled() => return Err(cancelled()),
            guard = self.state.lock() => guard,
        };
        if let HttpState::Pending(task) = &mut *state {
            let result = tokio::select! {
                _ = self.network.cancelled() => return Err(cancelled()),
                result = task => result.unwrap_or_else(|_| Err(HostFailure::new(ErrorKind::Trapped, "HTTP operation terminated"))),
            };
            *state = match result {
                Ok(response) => HttpState::Ready(response),
                Err(error) => HttpState::Failed(error),
            };
        }
        if let HttpState::Failed(error) = &*state {
            return Err(error.clone());
        }
        Ok(state)
    }
}

#[async_trait]
impl HostHttpResponse for PendingHttp {
    async fn status(&self) -> Result<u16, HostFailure> {
        let state = self.response().await?;
        let HttpState::Ready(response) = &*state else {
            unreachable!("resolved HTTP state")
        };
        Ok(response.status().as_u16())
    }

    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure> {
        let state = self.response().await?;
        let HttpState::Ready(response) = &*state else {
            unreachable!("resolved HTTP state")
        };
        response
            .headers()
            .iter()
            .map(|(name, value)| {
                let value = value
                    .to_str()
                    .map_err(|_| invalid("upstream header is not text"))?;
                Ok((name.as_str().to_owned(), value.to_owned()))
            })
            .collect()
    }

    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        let mut state = self.response().await?;
        let HttpState::Ready(response) = &mut *state else {
            unreachable!("resolved HTTP state")
        };
        let result = tokio::select! {
            _ = self.network.cancelled() => return Err(cancelled()),
            chunk = response.chunk() => chunk,
        };
        match result {
            Ok(chunk) => {
                if let Some(chunk) = &chunk {
                    self.network.wire(
                        WireDiagnostic {
                            direction: "upstream_response",
                            transport: "http",
                            message_type: "body_chunk",
                            url: response.url(),
                            status_code: Some(response.status().as_u16()),
                            headers: None,
                        },
                        || bytes_value(chunk),
                    );
                }
                Ok(chunk.map(|chunk| chunk.to_vec()))
            }
            Err(error) => {
                let error = transport_failure(error);
                *state = HttpState::Failed(error.clone());
                Err(error)
            }
        }
    }
}

struct ScopedWebSocket {
    network: VendorNetwork,
    socket: Mutex<Option<WebSocket>>,
    wire_url: String,
    pool_identity: Option<WebSocketPoolIdentity>,
    connected_at: tokio::time::Instant,
    reused: bool,
    application_message_seen: AtomicBool,
    reusable: AtomicBool,
    explicitly_released: AtomicBool,
}

impl ScopedWebSocket {
    fn wire_message(&self, direction: &str, message: &WebSocketMessage) {
        if let Ok(url) = Url::parse(&self.wire_url) {
            self.network.wire(
                WireDiagnostic {
                    direction,
                    transport: "websocket",
                    message_type: ws_message_type(message),
                    url: &url,
                    status_code: None,
                    headers: None,
                },
                || ws_payload(message),
            );
        }
    }

    fn invalidate(&self) {
        self.reusable.store(false, Ordering::Release);
        self.network
            .response_continuation_available
            .store(false, Ordering::Release);
    }

    fn expires_at(&self) -> tokio::time::Instant {
        self.connected_at + MAX_WEBSOCKET_AGE
    }

    fn expired(&self) -> HostFailure {
        self.invalidate();
        self.transport_failure(
            AiErrorKind::ServiceUnavailable,
            "upstream WebSocket reached its maximum age",
        )
    }

    fn transport_failure(&self, kind: AiErrorKind, message: &'static str) -> HostFailure {
        if self.reused && !self.application_message_seen.load(Ordering::Acquire) {
            HostFailure::upstream_transport(
                Some(kind),
                None,
                None,
                TransportFailure::Websocket,
                message,
            )
        } else {
            HostFailure::upstream(Some(kind), None, None, message)
        }
    }
}

impl Drop for ScopedWebSocket {
    fn drop(&mut self) {
        // Only `close()` after a guest-observed terminal can check a transport
        // back in. Cancellation, traps, partial streams, and forgotten handles
        // drop the socket here and therefore invalidate it.
        if !self.explicitly_released.load(Ordering::Acquire) {
            self.reusable.store(false, Ordering::Release);
        }
    }
}

#[async_trait]
impl HostWebSocket for ScopedWebSocket {
    async fn send(&self, message: WebSocketMessage) -> Result<(), HostFailure> {
        self.network.ensure_current()?;
        if tokio::time::Instant::now() >= self.expires_at() {
            let failure = self.expired();
            self.socket.lock().await.take();
            return Err(failure);
        }
        self.wire_message("upstream_request", &message);
        let mut guard = self.socket.lock().await;
        let socket = guard
            .as_mut()
            .ok_or_else(|| invalid("WebSocket is closed"))?;
        if matches!(message, WebSocketMessage::Close(_)) {
            self.invalidate();
        }
        let message = match message {
            WebSocketMessage::Text(text) => Message::Text(text),
            WebSocketMessage::Binary(bytes) => Message::Binary(bytes.into()),
            WebSocketMessage::Ping(bytes) => Message::Ping(bytes.into()),
            WebSocketMessage::Pong(bytes) => Message::Pong(bytes.into()),
            WebSocketMessage::Close(frame) => {
                let (code, reason) = frame.unwrap_or((1000, String::new()));
                Message::Close {
                    code: code.into(),
                    reason,
                }
            }
        };
        let (result, expired) = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(self.expires_at()) => (Err(self.expired()), true),
            _ = self.network.cancelled() => (Err(cancelled()), false),
            result = socket.send(message) => (result.map_err(|error| {
                log_transport_failure(&error);
                self.transport_failure(AiErrorKind::ServiceUnavailable, "upstream WebSocket send failed")
            }), false),
        };
        if result.is_err() {
            self.invalidate();
        }
        if expired {
            guard.take();
        }
        result
    }

    async fn next(&self) -> Result<Option<WebSocketMessage>, HostFailure> {
        self.network.ensure_current()?;
        if tokio::time::Instant::now() >= self.expires_at() {
            let failure = self.expired();
            self.socket.lock().await.take();
            return Err(failure);
        }
        let mut guard = self.socket.lock().await;
        let Some(socket) = guard.as_mut() else {
            return Ok(None);
        };
        let (message, expired) = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(self.expires_at()) => (Ok(None), true),
            _ = self.network.cancelled() => return Err(cancelled()),
            result = socket.next() => (result.transpose().map_err(|error| {
                log_transport_failure(&error);
                self.transport_failure(
                    AiErrorKind::ServiceUnavailable,
                    "upstream WebSocket receive failed",
                )
            }), false),
        };
        if expired {
            guard.take();
            return Err(self.expired());
        }
        let message = match message {
            Ok(message) => message,
            Err(failure) => {
                self.invalidate();
                return Err(failure);
            }
        };
        let Some(message) = message else {
            let failure = self.transport_failure(
                AiErrorKind::UnexpectedEof,
                "upstream WebSocket closed before a terminal response",
            );
            self.invalidate();
            return Err(failure);
        };
        let message = match message {
            Message::Text(text) => {
                self.application_message_seen.store(true, Ordering::Release);
                WebSocketMessage::Text(text)
            }
            Message::Binary(bytes) => {
                self.application_message_seen.store(true, Ordering::Release);
                WebSocketMessage::Binary(bytes.to_vec())
            }
            Message::Ping(bytes) => WebSocketMessage::Ping(bytes.to_vec()),
            Message::Pong(bytes) => WebSocketMessage::Pong(bytes.to_vec()),
            Message::Close { code, reason } => {
                if self.reused && !self.application_message_seen.load(Ordering::Acquire) {
                    let failure = self.transport_failure(
                        AiErrorKind::UnexpectedEof,
                        "reused upstream WebSocket closed before a response",
                    );
                    self.invalidate();
                    return Err(failure);
                }
                self.invalidate();
                WebSocketMessage::Close(Some((code.into(), reason)))
            }
        };
        self.wire_message("upstream_response", &message);
        Ok(Some(message))
    }

    async fn close(&self, response_continuation: bool) -> Result<(), HostFailure> {
        self.network
            .response_continuation_available
            .store(false, Ordering::Release);
        self.network.ensure_current()?;
        let Some(socket) = self.socket.lock().await.take() else {
            return Ok(());
        };
        if self.reusable.load(Ordering::Acquire)
            && self.network.ensure_current().is_ok()
            && let (Some(pool), Some(identity)) = (
                self.network.websocket_pool.as_ref(),
                self.pool_identity.as_ref(),
            )
        {
            if pool.park(
                WebSocketPoolIdentity {
                    scope_key: identity.scope_key.clone(),
                    affinity: identity.affinity.clone(),
                    requested_url: identity.requested_url.clone(),
                    actual_url: identity.actual_url.clone(),
                    requested_headers: identity.requested_headers,
                    actual_headers: identity.actual_headers,
                    requested_protocols: identity.requested_protocols.clone(),
                    actual_protocols: identity.actual_protocols.clone(),
                },
                socket,
                self.connected_at,
            ) {
                self.explicitly_released.store(true, Ordering::Release);
                if response_continuation {
                    // 只有guest声明“本响应来自此连接”且宿主确实完成归池，
                    // 才允许后续沿用该响应身份；请求允许WS或握手成功都不够。
                    self.network
                        .response_continuation_available
                        .store(true, Ordering::Release);
                }
            } else {
                self.invalidate();
            }
            return Ok(());
        }
        self.invalidate();
        let close_message = WebSocketMessage::Close(Some((1000, String::new())));
        self.wire_message("upstream_request", &close_message);
        tokio::select! {
            _ = self.network.cancelled() => Err(cancelled()),
            result = socket.close(1000.into(), None) => result.map_err(transport_failure),
        }
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

fn drain_idle_frames(socket: &mut WebSocket) -> bool {
    loop {
        let Some(frame) = socket.next().now_or_never() else {
            return true;
        };
        match frame {
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
            Some(Ok(_)) | Some(Err(_)) | None => return false,
        }
    }
}

fn ws_message_type(message: &WebSocketMessage) -> &'static str {
    match message {
        WebSocketMessage::Text(_) => "text",
        WebSocketMessage::Binary(_) => "binary",
        WebSocketMessage::Ping(_) => "ping",
        WebSocketMessage::Pong(_) => "pong",
        WebSocketMessage::Close(_) => "close",
    }
}

fn ws_payload(message: &WebSocketMessage) -> Value {
    match message {
        WebSocketMessage::Text(text) => Value::String(text.clone()),
        WebSocketMessage::Binary(bytes)
        | WebSocketMessage::Ping(bytes)
        | WebSocketMessage::Pong(bytes) => bytes_value(bytes),
        WebSocketMessage::Close(Some((code, reason))) => {
            serde_json::json!({ "code": code, "reason": reason })
        }
        WebSocketMessage::Close(None) => Value::Null,
    }
}

fn cancelled() -> HostFailure {
    HostFailure::new(ErrorKind::Cancelled, "vendor operation cancelled")
}

fn invalid(message: &'static str) -> HostFailure {
    // 此处检查 guest 生成的传输请求与上游响应，不应归责为客户端 canonical 输入错误。
    HostFailure::new(ErrorKind::Trapped, message)
}

fn kind_without_body(status: u16) -> AiErrorKind {
    if status == 429 {
        AiErrorKind::Unknown
    } else {
        AiError::kind_from_status(status, None)
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    let milliseconds = deadline
        .signed_duration_since(Utc::now())
        .num_milliseconds()
        .max(0);
    Some(Duration::from_millis(milliseconds as u64))
}

fn log_transport_failure(error: &impl std::fmt::Display) {
    tracing::debug!(
        error = %crate::interaction_observation::redact_text(&error.to_string()),
        "vendor transport failed"
    );
}

fn transport_failure(error: impl std::fmt::Display) -> HostFailure {
    log_transport_failure(&error);
    HostFailure::upstream(
        Some(AiErrorKind::ServiceUnavailable),
        None,
        None,
        "upstream transport failed",
    )
}

fn websocket_transport_failure(error: impl std::fmt::Display) -> HostFailure {
    log_transport_failure(&error);
    HostFailure::upstream_transport(
        Some(AiErrorKind::ServiceUnavailable),
        None,
        None,
        TransportFailure::Websocket,
        "upstream WebSocket transport failed",
    )
}

#[cfg(test)]
mod tests;
