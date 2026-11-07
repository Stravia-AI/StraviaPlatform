use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use ::http::Method;
use url::Url;
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind};
use stravia_vendor_runtime::{HostFailure, HostHttpResponse, HostWebSocket, WebSocketMessage};
use stravia_vendor_sdk::{ErrorKind, net::{self, Field}};
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::Instrument as _;

use crate::interaction_observation::{RunEvent, RunObserver};

use super::lifecycle::VendorOperation;
use super::permissions::NetworkPolicy;
use crate::outbound_proxy::OutboundProxySettings;

pub(crate) mod shape;
use shape::ShapeRegistry;
mod fields;
mod options;
mod http;
pub(crate) mod transport;
mod websocket;
use http::UpstreamResponse;
use websocket::{Handshake, Message, WebSocket};

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
    continuation_id: Option<String>,
    requested_url: String,
    actual_url: String,
    requested_headers: [u8; 32],
    actual_headers: [u8; 32],
    compression_key: u8,
    partition: [u8; 32],
    handshake_status: u16,
    handshake_headers: Vec<net::Field>,
    socket: WebSocket,
    connected_at: tokio::time::Instant,
    reuse_expires_at: tokio::time::Instant,
    parked_at: tokio::time::Instant,
}

struct WebSocketPoolIdentity {
    scope_key: String,
    affinity: String,
    requested_url: String,
    actual_url: String,
    requested_headers: [u8; 32],
    actual_headers: [u8; 32],
    compression_key: u8,
    partition: [u8; 32],
}

/// Reuse lookup key; the pool matches every field exactly.
struct WebSocketPoolRequest<'a> {
    scope_key: &'a str,
    affinity: &'a str,
    requested_url: &'a str,
    requested_headers: &'a [u8; 32],
    compression_key: u8,
    partition: [u8; 32],
    continuation_id: Option<&'a str>,
}

impl VendorWebSocketPool {
    fn take(
        &self,
        request: &WebSocketPoolRequest<'_>,
        reuse_age: Duration,
    ) -> Option<PooledWebSocket> {
        let WebSocketPoolRequest {
            scope_key,
            affinity,
            requested_url,
            requested_headers,
            compression_key,
            partition,
            continuation_id,
        } = *request;
        let mut idle = self.idle.lock();
        let now = tokio::time::Instant::now();
        let matches = |entry: &PooledWebSocket| {
            entry.scope_key == scope_key
                && entry.affinity == affinity
                && continuation_id.is_none_or(|id| entry.continuation_id.as_deref() == Some(id))
                && entry.requested_url == requested_url
                && entry.actual_url == requested_url
                && entry.requested_headers == *requested_headers
                && entry.actual_headers == *requested_headers
                && entry.compression_key == compression_key
                && entry.partition == partition
                && (compression_key != 2 || entry.socket.compressed())
        };
        let mut expiration_changed = false;
        idle.retain_mut(|entry| {
            if matches(entry) {
                let expires_at = entry.reuse_expires_at.min(entry.connected_at + reuse_age);
                expiration_changed |= expires_at != entry.reuse_expires_at;
                entry.reuse_expires_at = expires_at;
            }
            entry.reuse_expires_at > now
        });
        if expiration_changed {
            self.expiration_changed.notify_one();
        }
        let index = idle.iter().position(matches)?;
        Some(idle.swap_remove(index))
    }

    fn park(
        self: &Arc<Self>,
        identity: WebSocketPoolIdentity,
        handshake_status: u16,
        handshake_headers: Vec<net::Field>,
        socket: WebSocket,
        connected_at: tokio::time::Instant,
        reuse_expires_at: tokio::time::Instant,
        continuation_id: Option<String>,
    ) -> bool {
        {
            let mut idle = self.idle.lock();
            let now = tokio::time::Instant::now();
            if reuse_expires_at <= now {
                return false;
            }
            idle.retain(|entry| entry.reuse_expires_at > now);
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
                continuation_id,
                requested_url: identity.requested_url,
                actual_url: identity.actual_url,
                requested_headers: identity.requested_headers,
                actual_headers: identity.actual_headers,
                compression_key: identity.compression_key,
                partition: identity.partition,
                handshake_status,
                handshake_headers,
                socket,
                connected_at,
                reuse_expires_at,
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
                        .map(|entry| entry.reuse_expires_at)
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
                                .retain(|entry| entry.reuse_expires_at > now);
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
    transport: Arc<transport::VendorTransport>,
    shapes: Arc<ShapeRegistry>,
    proxy: OutboundProxySettings,
    resolver: Arc<dyn transport::Resolver>,
    origins: NetworkPolicy,
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
    send_admission: Option<crate::rpm::SendAdmission>,
}

struct WireDiagnostic<'a> {
    direction: &'a str,
    transport: &'static str,
    message_type: &'a str,
    url: &'a Url,
    status_code: Option<u16>,
    headers: Option<&'a [Field]>,
}

impl VendorNetwork {
    /// Each redirect is authorized independently before dispatch.
    pub(crate) fn new(
        transport: Arc<transport::VendorTransport>,
        shapes: Arc<ShapeRegistry>,
        proxy: OutboundProxySettings,
        origins: NetworkPolicy,
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
            transport,
            shapes,
            proxy,
            resolver: Arc::new(transport::SystemResolver),
            origins,
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
            send_admission: None,
        }
    }

    pub(crate) fn with_observer(mut self, observer: Option<RunObserver>) -> Self {
        self.observer = observer;
        self
    }

    pub(crate) fn with_send_admission(
        mut self,
        admission: Option<crate::rpm::SendAdmission>,
    ) -> Self {
        self.send_admission = admission;
        self
    }

    async fn admit_send(&self) -> Result<(), HostFailure> {
        self.ensure_current()?;
        if let Some(admission) = &self.send_admission {
            admission
                .acquire()
                .instrument(tracing::info_span!(
                    target: "stravia::perf", "upstream.send_admission.wait"
                ))
                .await?;
        }
        if self.attempt_id.is_some()
            && let Some(observer) = &self.observer
        {
            observer.mark_upstream_started();
        }
        Ok(())
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
        if !self.origins.allows(&url) {
            return Err(invalid(
                "upstream origin is not authorized for this operation",
            ));
        }
        Ok(url)
    }

    fn protect_headers(&self, headers: &[Field]) {
        let Some(observer) = &self.observer else { return };
        observer.protect_secrets(headers.iter().filter_map(|field| {
            let name = field.name.to_ascii_lowercase();
            let sensitive = matches!(name.as_str(), "authorization" | "proxy-authorization" | "cookie" | "set-cookie")
                || name.contains("token") || name.contains("secret")
                || name.contains("api-key") || name.contains("api_key")
                || name.ends_with("-key") || name.starts_with("x-auth-");
            sensitive.then(|| std::str::from_utf8(&field.bytes).ok()).flatten().filter(|value| !value.is_empty())
        }));
    }

    fn wire<P: Into<crate::interaction_observation::WirePayload>>(
        &self,
        diagnostic: WireDiagnostic<'_>,
        payload: impl FnOnce() -> P,
    ) {
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
            headers: diagnostic.headers.map(|headers| {
                if diagnostic.direction == "upstream_request" { headers_value(headers) }
                else { response_headers_value(headers) }
            }).unwrap_or(Value::Null),
            payload: payload().into(),
        });
    }

    pub(crate) fn validate_net_option(&self, key: &str, value: &str) -> Result<(), HostFailure> {
        options::validate_option(&self.shapes, key, value)
            .map_err(|error| HostFailure::new(ErrorKind::Unsupported, error.to_string()))
    }

    fn validate_options(&self, options: &net::Options) -> Result<(), HostFailure> {
        for (key, value) in options.iter() { self.validate_net_option(key, value)?; }
        Ok(())
    }

    fn route(&self, options: &net::Options, websocket: bool) -> Result<transport::Route, HostFailure> {
        self.validate_options(options)?;
        let name = option(options, net::keys::SHAPE).unwrap_or("default");
        let shape = self.shapes.get(name).ok_or_else(|| HostFailure::new(ErrorKind::Unsupported, "unknown network shape"))?;
        let mut proxy = self.proxy.clone();
        proxy.normalize();
        proxy.validate().map_err(|error| HostFailure::new(ErrorKind::Invalid, error.to_string()))?;
        let mut digest = Sha256::new();
        digest.update(proxy.url.as_bytes());
        digest.update([0]);
        digest.update(proxy.bypass.as_bytes());
        digest.update([proxy.force_http1 as u8]);
        let proxy_identity = format!("{:x}", digest.finalize());
        let preference = if websocket { option(options, net::keys::WS_TRANSPORT) } else { option(options, net::keys::HTTP_VERSION) };
        if proxy.force_http1 && matches!(preference, Some("http2" | "h2")) {
            return Err(HostFailure::new(ErrorKind::Unsupported, "HTTP/2 conflicts with the proxy HTTP/1 policy"));
        }
        if websocket && matches!(preference, Some("h2")) && !shape.ws.over_h2 {
            return Err(HostFailure::new(ErrorKind::Unsupported, "shape does not support WebSocket over HTTP/2"));
        }
        let version = match preference {
            Some("http1") => transport::HttpVersionPref::Http1,
            Some("http2" | "h2") => transport::HttpVersionPref::Http2,
            _ if proxy.force_http1 || (websocket && !shape.ws.over_h2) => transport::HttpVersionPref::Http1,
            _ => transport::HttpVersionPref::Auto,
        };
        Ok(transport::Route {
            shape_name: shape.name.clone(), shape_revision: shape.revision,
            tls: shape.tls_for(&proxy_identity, websocket), proxy_tls: self.shapes.proxy_tls(),
            proxy, resolver: self.resolver.clone(), version, h2: shape.http2.clone(),
            omit_h2_content_length: shape.http2_content_length == "omit",
        })
    }

    fn request_observer(&self, url: Url, transport_name: &'static str, message_type: &'static str, body: Bytes) -> Option<Box<dyn FnOnce(transport::RequestHead) + Send>> {
        if !self.observer.as_ref().is_some_and(RunObserver::debug_enabled) { return None; }
        let network = self.clone();
        Some(Box::new(move |head| {
            network.wire(WireDiagnostic { direction: "upstream_request", transport: transport_name, message_type, url: &url, status_code: None, headers: Some(&head.fields) }, || crate::interaction_observation::WirePayload::Bytes(body));
        }))
    }

    pub(crate) fn http_start(
            &self,
            request: net::Request,
            options: net::Options,
        ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
            let url = self.url(&request.url, false)?;
            self.validate_options(&options)?;
            let method = Method::from_bytes(request.method.as_bytes())
                .map_err(|_| invalid("invalid HTTP method"))?;
            if method == Method::CONNECT {
                return Err(invalid("HTTP CONNECT is not a vendor operation"));
            }
            http::prepare(&request.headers, &options)?;
            let route = self.route(&options, false)?;
            let network = self.clone();
            let sending = network.clone();
            let task = tokio::spawn(async move {
                let body = tokio::select! {
                    _ = sending.cancelled() => return Err(cancelled()),
                    body = http::encode(request.body, &options) => body?,
                };
                sending.send_http(method, url, request.headers.into_vec(), body, options, route).await
            }.in_current_span());
            let abort = task.abort_handle();
            Ok(Arc::new(PendingHttp { network, state: Mutex::new(HttpState::Pending(task)), abort }))
        }

    async fn send_http(
            &self,
            mut method: Method,
            mut url: Url,
            mut headers: Vec<Field>,
            mut body: Bytes,
            options: net::Options,
            route: transport::Route,
        ) -> Result<UpstreamResponse, HostFailure> {
            let shape = self.shapes.get(&route.shape_name)
                .ok_or_else(|| invalid("unknown network shape"))?;
            for redirects in 0..=MAX_REDIRECTS {
                self.ensure_current()?;
                let fields = fields::plan_http(headers.clone(), &url, method.as_str(), body.len(), shape)
                    .map_err(|error| HostFailure::new(ErrorKind::Trapped, error))?;
                self.admit_send().await?;
                let request = transport::RequestPlan {
                    method: method.as_str().to_owned(), uri: url.as_str().to_owned(),
                    fields, body: body.clone(),
                    observer: self.request_observer(url.clone(), "http", "request", body.clone()),
                };
                let response = async {
                    tokio::select! {
                        _ = self.cancelled() => Err(cancelled()),
                        result = self.transport.send(request, route.clone()) => result.map_err(transport_failure),
                    }
                }.instrument(tracing::info_span!(
                    target: "stravia::perf", "upstream.http.response_headers.wait"
                )).await?;
                self.ensure_current()?;
                let status = response.head.status;
                self.wire(WireDiagnostic {
                    direction: "upstream_response", transport: "http", message_type: "response_headers",
                    url: &url, status_code: Some(status), headers: Some(&response.head.fields),
                }, || Value::Null);
                if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                    return UpstreamResponse::new(response, url, method.as_str(), &options);
                }
                let Some(location) = response.head.fields.iter().find(|f| f.name.eq_ignore_ascii_case("location")) else {
                    return UpstreamResponse::new(response, url, method.as_str(), &options);
                };
                if redirects == MAX_REDIRECTS { return Err(invalid("upstream redirect limit exceeded")); }
                let location = std::str::from_utf8(&location.bytes).map_err(|_| invalid("invalid redirect location"))?;
                let next = url.join(location).map_err(|_| invalid("invalid redirect URL"))?;
                let next = self.url(next.as_str(), false)?;
                if matches!(status, 301..=303) && method != Method::GET && method != Method::HEAD {
                    method = Method::GET;
                    body = Bytes::new();
                    headers.retain(|f| !f.name.to_ascii_lowercase().starts_with("content-")
                        && !f.name.eq_ignore_ascii_case("transfer-encoding"));
                }
                if next.origin() != url.origin() {
                    if !body.is_empty() { return Err(invalid("cross-origin redirect cannot forward a request body")); }
                    if option(&options, net::keys::RESPONSE_DECODING).unwrap_or("none") == "none" {
                        headers.clear();
                    } else {
                        headers.retain(|f| f.name.eq_ignore_ascii_case("accept-encoding"));
                    }
                } else {
                    for field in &mut headers {
                        if field.name.eq_ignore_ascii_case("host") { field.bytes.clear(); }
                    }
                }
                url = next;
            }
            unreachable!("redirect loop returns at its bound")
        }

    pub(crate) async fn ws_connect(
        &self,
        request: net::ConnectRequest,
        options: net::Options,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        let route = self.route(&options, true)?;
        let shape = self.shapes.get(option(&options, net::keys::SHAPE).unwrap_or("default"))
            .ok_or_else(|| HostFailure::new(ErrorKind::Unsupported, "unknown network shape"))?;
        let compression = match option(&options, net::keys::WS_COMPRESSION).unwrap_or("disabled") {
            "preferred" => net::WsCompression::Preferred,
            "required" => net::WsCompression::Required,
            _ => net::WsCompression::Disabled,
        };
        let compression_key = match compression {
            net::WsCompression::Disabled => 0,
            net::WsCompression::Preferred => 1,
            net::WsCompression::Required => 2,
        };
        let reuse_age = option(&options, net::keys::REUSE_MAX_AGE)
            .and_then(|v| v.parse::<u64>().ok()).map(Duration::from_millis)
            .unwrap_or(MAX_WEBSOCKET_AGE).min(MAX_WEBSOCKET_AGE);
        let continuation_id = option(&options, net::keys::CONTINUATION);
        let mut proxy = route.proxy.clone();
        proxy.normalize();
        let mut partition_hash = Sha256::new();
        partition_hash.update(b"stravia-websocket-route-v1\0");
        partition_hash.update(route.shape_name.as_bytes());
        partition_hash.update(route.shape_revision.to_be_bytes());
        partition_hash.update(serde_json::to_vec(&proxy).expect("proxy settings serialize"));
        partition_hash.update(match route.version {
            transport::HttpVersionPref::Auto => b"auto".as_slice(),
            transport::HttpVersionPref::Http1 => b"http1".as_slice(),
            transport::HttpVersionPref::Http2 => b"http2".as_slice(),
        });
        let partition: [u8; 32] = partition_hash.finalize().into();
        let mut url = self.url(&request.url, true)?;
        let requested_url = url.as_str().to_owned();
        let mut headers = fields::plan_ws(request.headers.into_vec(), &url, shape)
            .map_err(|message| HostFailure::new(ErrorKind::Invalid, message))?;
        // Validate the handshake before checking out a pooled connection.
        let mut handshake = Some(Handshake::new(&url, headers.clone(), compression)?);
        let requested_headers = canonical_headers(&headers);

        if let (Some(pool), Some(scope_key), Some(affinity)) = (
            self.websocket_pool.as_ref(), self.websocket_scope_key.as_deref(), self.websocket_affinity.as_deref(),
        ) {
            let lookup = WebSocketPoolRequest {
                scope_key, affinity, requested_url: &requested_url, requested_headers: &requested_headers,
                compression_key, partition, continuation_id,
            };
            while let Some(mut entry) = pool.take(&lookup, reuse_age) {
                if self.url(&entry.actual_url, true).is_err() || !drain_idle_frames(&mut entry.socket) { continue; }
                return Ok(Arc::new(ScopedWebSocket {
                    network: self.clone(), socket: Mutex::new(Some(entry.socket)), wire_url: entry.actual_url.clone(),
                    handshake_status: entry.handshake_status, handshake_headers: entry.handshake_headers,
                    pool_identity: Some(WebSocketPoolIdentity {
                        scope_key: entry.scope_key, affinity: entry.affinity, requested_url: entry.requested_url,
                        actual_url: entry.actual_url, requested_headers: entry.requested_headers,
                        actual_headers: entry.actual_headers, compression_key: entry.compression_key, partition: entry.partition,
                    }),
                    connected_at: entry.connected_at, reuse_expires_at: entry.reuse_expires_at, reused: true,
                    first_send_admitted: AtomicBool::new(false), application_message_seen: AtomicBool::new(false),
                    reusable: AtomicBool::new(true), explicitly_released: AtomicBool::new(false),
                }));
            }
        }
        if continuation_id.is_some() {
            self.ensure_current()?;
            return Err(HostFailure::new(ErrorKind::ContinuationUnavailable, "requested WebSocket continuation is not available locally"));
        }
        for redirects in 0..=MAX_REDIRECTS {
            self.ensure_current()?;
            self.admit_send().await?;
            let handshake = match handshake.take() {
                Some(handshake) => handshake,
                None => Handshake::new(&url, headers.clone(), compression)?,
            };
            let observer = self.request_observer(url.clone(), "websocket", "handshake_request", Bytes::new());
            let response = tokio::select! {
                _ = self.cancelled() => return Err(cancelled()),
                result = handshake.send(&self.transport, route.clone(), observer) => result.map_err(websocket_transport_failure)?,
            };
            let (head, upgraded, mut rejected_body) = match response {
                transport::UpgradeResponse::Upgraded(head, io) => (head, Some(io), None),
                transport::UpgradeResponse::Rejected(response) => (response.head, None, Some(response.body)),
            };
            let status = head.status;
            self.wire(WireDiagnostic { direction: "upstream_response", transport: "websocket", message_type: "handshake_response", url: &url, status_code: Some(status), headers: Some(&head.fields) }, || Value::Null);
            if matches!(status, 301 | 302 | 303 | 307 | 308) && upgraded.is_none() {
                if redirects == MAX_REDIRECTS { return Err(invalid("upstream redirect limit exceeded")); }
                let location = head.fields.iter().find(|field| field.name.eq_ignore_ascii_case("location"))
                    .and_then(|field| std::str::from_utf8(&field.bytes).ok())
                    .ok_or_else(|| invalid("WebSocket redirect lacks a valid location"))?;
                let next = url.join(location).map_err(|_| invalid("invalid redirect URL"))?;
                let next = self.url(next.as_str(), true)?;
                if next.origin() != url.origin() {
                    headers = fields::plan_ws(Vec::new(), &next, shape)
                        .map_err(|message| HostFailure::new(ErrorKind::Invalid, message))?;
                }
                url = next;
                continue;
            }
            if let Some(mut body_stream) = rejected_body.take() {
                let retry_after = retry_after(&head.fields);
                let mut body = Vec::new();
                loop {
                    let chunk = tokio::select! {
                        _ = self.cancelled() => return Err(cancelled()),
                        result = body_stream.next_chunk() => result.map_err(|error| HostFailure::upstream_transport(
                            Some(kind_without_body(status)), Some(status), retry_after, "websocket".into(),
                            format!("WebSocket handshake body read failed: {}", transport_cause(&error)),
                        ))?,
                    };
                    let Some(chunk) = chunk else { break };
                    if chunk.len() > websocket::MAX_MESSAGE_BYTES - body.len() {
                        return Err(HostFailure::upstream_transport(Some(kind_without_body(status)), Some(status), retry_after, "websocket".into(), "WebSocket handshake body exceeds size limit"));
                    }
                    body.extend_from_slice(&chunk);
                }
                let body = Bytes::from(body);
                self.wire(WireDiagnostic { direction: "upstream_response", transport: "websocket", message_type: "handshake_body", url: &url, status_code: Some(status), headers: None }, || crate::interaction_observation::WirePayload::Bytes(body.clone()));
                return Err(match status {
                    401 | 403 | 408 | 429 | 500..=599 => {
                        let value = serde_json::from_slice::<Value>(&body).ok();
                        HostFailure::upstream(Some(AiError::kind_from_status(status, value.as_ref())), Some(status), retry_after, "upstream did not accept the WebSocket handshake")
                    }
                    200..=399 | 404 | 405 | 426 => HostFailure::upstream_transport(Some(AiErrorKind::ServiceUnavailable), Some(status), retry_after, "websocket".into(), "upstream did not accept the WebSocket handshake"),
                    _ => HostFailure { kind: ErrorKind::Invalid, message: "upstream did not accept the WebSocket handshake".into(), upstream_status: Some(status) },
                });
            }
            let socket = tokio::select! {
                _ = self.cancelled() => return Err(cancelled()),
                result = handshake.finish(&head, upgraded.expect("upgrade has IO")) => result?,
            };
            self.ensure_current()?;
            let connected_at = tokio::time::Instant::now();
            let actual_headers = canonical_headers(&headers);
            let pool_identity = match (self.websocket_pool.as_ref(), self.websocket_scope_key.as_ref(), self.websocket_affinity.as_ref()) {
                (Some(_), Some(scope_key), Some(affinity)) if url.as_str() == requested_url && actual_headers == requested_headers => Some(WebSocketPoolIdentity {
                    scope_key: scope_key.clone(), affinity: affinity.clone(), requested_url: requested_url.clone(), actual_url: url.as_str().to_owned(),
                    requested_headers, actual_headers, compression_key, partition,
                }),
                _ => None,
            };
            return Ok(Arc::new(ScopedWebSocket {
                network: self.clone(), socket: Mutex::new(Some(socket)), wire_url: url.as_str().to_owned(),
                handshake_status: head.status, handshake_headers: head.fields, pool_identity, connected_at,
                reuse_expires_at: connected_at + reuse_age, reused: false,
                first_send_admitted: AtomicBool::new(true), application_message_seen: AtomicBool::new(false),
                reusable: AtomicBool::new(true), explicitly_released: AtomicBool::new(false),
            }));
        }
        unreachable!("redirect loop returns at its bound")
    }
}

fn canonical_headers(headers: &[Field]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"stravia-vendor-websocket-fields-v2\0");
    for field in headers {
        digest.update((field.name.len() as u64).to_be_bytes());
        digest.update(field.name.as_bytes());
        digest.update((field.bytes.len() as u64).to_be_bytes());
        digest.update(&field.bytes);
    }
    digest.finalize().into()
}

enum HttpState {
    Pending(JoinHandle<Result<UpstreamResponse, HostFailure>>),
    Ready(UpstreamResponse),
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
        Ok(response.status())
    }

    async fn headers(&self) -> Result<Vec<Field>, HostFailure> {
        let state = self.response().await?;
        let HttpState::Ready(response) = &*state else {
            unreachable!("resolved HTTP state")
        };
        Ok(response.headers().to_vec())
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
                            message_type: response.body_message_type(),
                            url: response.url(),
                            status_code: Some(response.status()),
                            headers: None,
                        },
                        || crate::interaction_observation::WirePayload::Bytes(chunk.clone()),
                    );
                }
                Ok(chunk.map(|chunk| chunk.to_vec()))
            }
            Err(error) => {
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
    handshake_status: u16,
    handshake_headers: Vec<net::Field>,
    pool_identity: Option<WebSocketPoolIdentity>,
    connected_at: tokio::time::Instant,
    // Retirement only controls checkout/check-in; in-flight I/O keeps the
    // existing 60-minute hard limit measured from the same connection start.
    reuse_expires_at: tokio::time::Instant,
    reused: bool,
    first_send_admitted: AtomicBool,
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

    fn transport_failure(&self, kind: AiErrorKind, message: impl Into<String>) -> HostFailure {
        if self.reused && !self.application_message_seen.load(Ordering::Acquire) {
            HostFailure::upstream_transport(
                Some(kind),
                None,
                None,
                "websocket".to_owned(),
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
    fn handshake_status(&self) -> u16 {
        self.handshake_status
    }

    fn handshake_headers(&self) -> Vec<net::Field> {
        self.handshake_headers.clone()
    }

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
        if matches!(
            &message,
            WebSocketMessage::Text(_) | WebSocketMessage::Binary(_)
        ) && !self.first_send_admitted.swap(false, Ordering::AcqRel)
        {
            self.network.admit_send().await?;
        }
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
                Message::Close { code, reason }
            }
        };
        let (result, expired) = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(self.expires_at()) => (Err(self.expired()), true),
            _ = self.network.cancelled() => (Err(cancelled()), false),
            result = socket.send(message) => (result.map_err(|error| {
                let cause = transport_cause(&error);
                tracing::debug!(error = %cause, "vendor transport failed");
                self.transport_failure(
                    AiErrorKind::ServiceUnavailable,
                    format!("upstream WebSocket send failed: {cause}"),
                )
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
                let cause = transport_cause(&error);
                tracing::debug!(error = %cause, "vendor transport failed");
                self.transport_failure(
                    AiErrorKind::ServiceUnavailable,
                    format!("upstream WebSocket receive failed: {cause}"),
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
        let binary_captured = matches!(&message, Message::Binary(_));
        let message = match message {
            Message::Text(text) => {
                self.application_message_seen.store(true, Ordering::Release);
                WebSocketMessage::Text(text)
            }
            Message::Binary(bytes) => {
                self.application_message_seen.store(true, Ordering::Release);
                if let Ok(url) = Url::parse(&self.wire_url) {
                    self.network.wire(
                        WireDiagnostic {
                            direction: "upstream_response",
                            transport: "websocket",
                            message_type: "binary",
                            url: &url,
                            status_code: None,
                            headers: None,
                        },
                        || crate::interaction_observation::WirePayload::Bytes(bytes.clone()),
                    );
                }
                WebSocketMessage::Binary(bytes.to_vec())
            }
            Message::Ping(bytes) => WebSocketMessage::Ping(bytes.to_vec()),
            Message::Pong(bytes) => WebSocketMessage::Pong(bytes.to_vec()),
            Message::Close { code, reason } => {
                let message = WebSocketMessage::Close(Some((code, reason)));
                // 实际收到的 Close 必须先捕获；复用连接的分类失败不能吞掉线上的帧。
                self.wire_message("upstream_response", &message);
                if self.reused && !self.application_message_seen.load(Ordering::Acquire) {
                    let failure = self.transport_failure(
                        AiErrorKind::UnexpectedEof,
                        "reused upstream WebSocket closed before a response",
                    );
                    self.invalidate();
                    return Err(failure);
                }
                self.invalidate();
                return Ok(Some(message));
            }
        };
        if !binary_captured {
            self.wire_message("upstream_response", &message);
        }
        Ok(Some(message))
    }

    async fn close(&self, continuation_id: Option<String>) -> Result<(), HostFailure> {
        self.network
            .response_continuation_available
            .store(false, Ordering::Release);
        self.network.ensure_current()?;
        let Some(mut socket) = self.socket.lock().await.take() else {
            return Ok(());
        };
        let has_continuation = continuation_id.is_some();
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
                    compression_key: identity.compression_key,
                    partition: identity.partition,
                },
                self.handshake_status,
                self.handshake_headers.clone(),
                socket,
                self.connected_at,
                self.reuse_expires_at,
                continuation_id,
            ) {
                self.explicitly_released.store(true, Ordering::Release);
                if has_continuation {
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
            result = socket.close(1000, None) => result.map_err(transport_failure),
        }
    }
}

fn headers_value(headers: &[Field]) -> Value {
    Value::Array(headers.iter().map(|field| {
        let bytes = if field.name.eq_ignore_ascii_case("authorization") || field.name.eq_ignore_ascii_case("proxy-authorization") {
            b"***".as_slice()
        } else {
            field.bytes.as_slice()
        };
        serde_json::json!({ "name": field.name, "bytes": bytes })
    }).collect())
}

fn response_headers_value(headers: &[Field]) -> Value {
    let mut values = serde_json::Map::new();
    for field in headers {
        let name = field.name.to_ascii_lowercase();
        let value = if name == "authorization" { Value::String("***".into()) } else { bytes_value(&field.bytes) };
        match values.entry(name) {
            serde_json::map::Entry::Vacant(entry) => { entry.insert(value); }
            serde_json::map::Entry::Occupied(mut entry) => match entry.get_mut() {
                Value::Array(existing) => existing.push(value),
                existing => { let first = std::mem::replace(existing, Value::Null); *existing = Value::Array(vec![first, value]); }
            }
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
        WebSocketMessage::Binary(bytes) => bytes_value(bytes),
        // Trace policy replaces control-frame payloads with omission metadata.
        WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) => Value::Null,
        WebSocketMessage::Close(Some((code, reason))) => {
            serde_json::json!({ "code": code, "reason": reason })
        }
        WebSocketMessage::Close(None) => Value::Null,
    }
}

fn option<'a>(options: &'a net::Options, key: &str) -> Option<&'a str> {
    options.iter().find_map(|(name, value)| (name == key).then_some(value))
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

fn retry_after(headers: &[Field]) -> Option<Duration> {
    let field = headers.iter().find(|field| field.name.eq_ignore_ascii_case("retry-after"))?;
    let value = std::str::from_utf8(&field.bytes).ok()?.trim();
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

/// 包装错误的 Display 可能只有类别名；保留 source 链后统一脱敏，
/// 让日志与最终失败请求记录都能区分真实传输原因。
fn transport_cause(error: &(impl std::error::Error + ?Sized)) -> String {
    let mut cause = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        let _ = write!(cause, ": {error}");
        source = error.source();
    }
    crate::interaction_observation::redact_text(&cause)
}

fn transport_failure(error: impl std::error::Error) -> HostFailure {
    let cause = transport_cause(&error);
    tracing::debug!(error = %cause, "vendor transport failed");
    HostFailure::upstream(
        Some(AiErrorKind::ServiceUnavailable),
        None,
        None,
        format!("upstream transport failed: {cause}"),
    )
}

fn websocket_transport_failure(error: impl std::error::Error) -> HostFailure {
    let cause = transport_cause(&error);
    tracing::debug!(error = %cause, "vendor transport failed");
    HostFailure::upstream_transport(
        Some(AiErrorKind::ServiceUnavailable),
        None,
        None,
        "websocket".to_owned(),
        format!("upstream WebSocket transport failed: {cause}"),
    )
}

#[cfg(test)]
mod tests;
