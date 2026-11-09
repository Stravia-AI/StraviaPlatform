//! Connection pooling for explicit routes and TLS identities.
//!
//! A lease holds an HTTP/1 sender exclusively. Dropping the lease waits for
//! the response body to release the connection before returning it to the
//! pool. HTTP/2 senders are cloned and may be used concurrently.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

use rama_core::ServiceInput;
use rama_http_core::client::conn::http1;
use rustls::ClientConfig;

use super::{
    body::RequestBody,
    connector::{self, Resolver, Route},
    h2::{self, H2PeerSettingsHandle, H2Settings},
};

const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Protocols to offer the destination, in preference order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AlpnPreference {
    /// Use the target TLS configuration's ALPN list, falling back to HTTP/1.1
    /// when the peer does not negotiate a protocol.
    Http2ThenHttp1,
    Http1Only,
    /// Require HTTP/2; for plaintext this uses HTTP/2 prior knowledge.
    Http2Only,
}

/// All inputs that determine how a connection is established.
#[derive(Clone)]
pub struct ConnectOptions {
    pub route: Route,
    pub host: String,
    pub port: u16,
    /// Either `http` or `https`.
    pub scheme: String,
    pub target_tls: Option<Arc<ClientConfig>>,
    pub tls_session_policy: super::super::shape::SessionPolicy,
    pub proxy_tls: Option<Arc<ClientConfig>>,
    pub resolver: Arc<dyn Resolver>,
    pub shape_identity: Arc<str>,
    pub alpn: AlpnPreference,
    pub h2_settings: H2Settings,
}

type H1Sender = http1::SendRequest<RequestBody>;

/// A sender for the negotiated HTTP version.
pub enum Sender {
    Http1(H1Sender),
    Http2(h2::H2Sender),
}

/// An acquired connection. For an HTTP forward proxy, the caller must use an
/// absolute-form URI and attach `proxy_authorization` only to that request.
/// All other HTTP/1 requests use origin-form. The peer settings handle is
/// present for HTTP/2, including extended CONNECT capability detection.
pub struct PooledConnection {
    pub sender: Option<Sender>,
    pub forward_proxy: bool,
    pub proxy_authorization: Option<String>,
    pub peer_settings: Option<H2PeerSettingsHandle>,
    guard: Option<EntryGuard>,
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        let Some(Sender::Http1(mut sender)) = self.sender.take() else {
            return;
        };
        let Some(guard) = self.guard.take() else {
            return;
        };
        let authorization = self.proxy_authorization.clone();
        let forward_proxy = self.forward_proxy;
        // Keep the entry alive until the body releases the HTTP/1 connection.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if sender.ready().await.is_ok()
                    && let Some(pool) = guard.pool.upgrade()
                {
                    let mut state = pool.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(entry) = state.entries.get_mut(&guard.key) {
                        entry.h1.push(IdleH1 {
                            sender,
                            authorization,
                            forward_proxy,
                            since: Instant::now(),
                        });
                        schedule_h1_expiry(&pool, guard.key.clone());
                    }
                }
            });
        }
    }
}

// Config identity is pointer identity, not its contents. Keeping the Arc in the
// key also prevents a freed configuration's address from being reused.
#[derive(Clone)]
struct ConfigIdentity(Option<Arc<ClientConfig>>);
impl PartialEq for ConfigIdentity {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}
impl Eq for ConfigIdentity {}
impl Hash for ConfigIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_ref().map(Arc::as_ptr).hash(state);
    }
}

#[derive(Clone)]
struct ResolverIdentity(Arc<dyn Resolver>);
impl PartialEq for ResolverIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ResolverIdentity {}
impl Hash for ResolverIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as *const ()).hash(state);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    route: Route,
    resolver: ResolverIdentity,
    host: String,
    port: u16,
    scheme: String,
    target_tls: ConfigIdentity,
    tls_session_policy: super::super::shape::SessionPolicy,
    proxy_tls: ConfigIdentity,
    shape_identity: Arc<str>,
    alpn: AlpnPreference,
}

struct IdleH1 {
    sender: H1Sender,
    authorization: Option<String>,
    forward_proxy: bool,
    since: Instant,
}

struct IdleH2 {
    sender: h2::H2Sender,
    peer_settings: H2PeerSettingsHandle,
    since: Instant,
}

struct Entry {
    active: usize,
    h1: Vec<IdleH1>,
    h2: Option<IdleH2>,
    target_tls: Option<Arc<ClientConfig>>,
    proxy_tls: Option<Arc<ClientConfig>>,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
}

fn remove_unused(state: &mut State, key: &Key) {
    if state
        .entries
        .get(key)
        .is_some_and(|entry| entry.active == 0 && entry.h1.is_empty() && entry.h2.is_none())
    {
        state.entries.remove(key);
    }
}

// Held from the start of acquisition through the lease, and until an H1
// response releases its sender. Cancellation and errors release it too.
struct EntryGuard {
    pool: std::sync::Weak<Mutex<State>>,
    key: Key,
}
impl Drop for EntryGuard {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            let mut state = pool.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = state.entries.get_mut(&self.key) {
                entry.active -= 1;
            }
            remove_unused(&mut state, &self.key);
        }
    }
}

fn schedule_h1_expiry(pool: &Arc<Mutex<State>>, key: Key) {
    let pool = Arc::downgrade(pool);
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_TIMEOUT).await;
        if let Some(pool) = pool.upgrade() {
            let mut state = pool.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = state.entries.get_mut(&key) {
                entry
                    .h1
                    .retain(|idle| idle.since.elapsed() < IDLE_TIMEOUT && !idle.sender.is_closed());
            }
            remove_unused(&mut state, &key);
        }
    });
}

fn schedule_h2_expiry(pool: &Arc<Mutex<State>>, key: Key) {
    let pool = Arc::downgrade(pool);
    tokio::spawn(async move {
        let mut delay = IDLE_TIMEOUT;
        loop {
            tokio::time::sleep(delay).await;
            let Some(pool) = pool.upgrade() else { break };
            let mut state = pool.lock().unwrap_or_else(|e| e.into_inner());
            let Some(entry) = state.entries.get_mut(&key) else {
                break;
            };
            let Some(idle) = entry.h2.as_ref() else { break };
            if idle.sender.is_closed() || idle.since.elapsed() >= IDLE_TIMEOUT {
                // The h2 driver keeps active streams alive after the last sender
                // is dropped; this only removes the pool's reusable handle.
                entry.h2 = None;
                remove_unused(&mut state, &key);
                break;
            }
            delay = IDLE_TIMEOUT.saturating_sub(idle.since.elapsed());
        }
    });
}

/// A cloneable pool. Idle connections expire after 90 seconds;
/// there is no per-host idle connection limit.
#[derive(Clone, Default)]
pub struct Pool {
    state: Arc<Mutex<State>>,
}

fn isolated(
    config: &Option<Arc<ClientConfig>>,
    policy: super::super::shape::SessionPolicy,
) -> Option<Arc<ClientConfig>> {
    config.as_ref().map(|config| {
        let mut config = (**config).clone();
        config.resumption = policy.resumption();
        Arc::new(config)
    })
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl Pool {
    pub async fn acquire(&self, options: ConnectOptions) -> io::Result<PooledConnection> {
        if !matches!(options.scheme.as_str(), "http" | "https") || options.port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid destination scheme or port",
            ));
        }
        let key = Key {
            route: options.route.clone(),
            resolver: ResolverIdentity(options.resolver.clone()),
            host: options.host.clone(),
            port: options.port,
            scheme: options.scheme.clone(),
            target_tls: ConfigIdentity(options.target_tls.clone()),
            tls_session_policy: options.tls_session_policy,
            proxy_tls: ConfigIdentity(options.proxy_tls.clone()),
            shape_identity: options.shape_identity.clone(),
            alpn: options.alpn,
        };
        let (target_tls, proxy_tls, guard) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let entry = state.entries.entry(key.clone()).or_insert_with(|| Entry {
                active: 0,
                h1: Vec::new(),
                h2: None,
                target_tls: isolated(&options.target_tls, options.tls_session_policy),
                proxy_tls: isolated(
                    &options.proxy_tls,
                    super::super::shape::SessionPolicy::Standard,
                ),
            });
            entry.active += 1;
            let guard = EntryGuard {
                pool: Arc::downgrade(&self.state),
                key: key.clone(),
            };
            entry
                .h1
                .retain(|idle| idle.since.elapsed() < IDLE_TIMEOUT && !idle.sender.is_closed());
            if let Some(h2) = entry.h2.as_mut() {
                if h2.since.elapsed() < IDLE_TIMEOUT && !h2.sender.is_closed() {
                    h2.since = Instant::now();
                    return Ok(PooledConnection {
                        sender: Some(Sender::Http2(h2.sender.clone())),
                        peer_settings: Some(h2.peer_settings.clone()),
                        forward_proxy: false,
                        proxy_authorization: None,
                        guard: Some(guard),
                    });
                }
                entry.h2 = None;
            }
            if let Some(idle) = entry.h1.pop() {
                return Ok(PooledConnection {
                    sender: Some(Sender::Http1(idle.sender)),
                    peer_settings: None,
                    forward_proxy: idle.forward_proxy,
                    proxy_authorization: idle.authorization,
                    guard: Some(guard),
                });
            }
            (entry.target_tls.clone(), entry.proxy_tls.clone(), guard)
        };

        let forward_proxy = options.scheme == "http"
            && matches!(&options.route, Route::Proxy(proxy) if proxy.is_http_forward_proxy());
        let (stream, authorization, negotiated) = if forward_proxy {
            if options.alpn == AlpnPreference::Http2Only {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "HTTP forwarding requires HTTP/1.1",
                ));
            }
            let Route::Proxy(proxy) = &options.route else {
                unreachable!()
            };
            let (stream, authorization) =
                connector::connect_http_forward(proxy, proxy_tls, options.resolver.as_ref())
                    .await?;
            (stream, authorization, None)
        } else if options.scheme == "https" {
            let config = target_tls.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "HTTPS requires target TLS configuration",
                )
            })?;
            let mut config = (*config).clone();
            match options.alpn {
                AlpnPreference::Http2ThenHttp1 => {}
                AlpnPreference::Http1Only => config
                    .alpn_protocols
                    .retain(|protocol| protocol == b"http/1.1"),
                AlpnPreference::Http2Only => config.alpn_protocols = vec![b"h2".to_vec()],
            }
            let (stream, negotiated) = connector::connect_tls(
                &options.host,
                options.port,
                &options.route,
                proxy_tls,
                Arc::new(config),
                options.resolver.as_ref(),
            )
            .await?;
            (stream, None, negotiated)
        } else {
            (
                connector::connect(
                    &options.host,
                    options.port,
                    &options.route,
                    proxy_tls,
                    options.resolver.as_ref(),
                )
                .await?,
                None,
                None,
            )
        };
        let use_h2 = if options.scheme == "https" {
            match negotiated.as_deref() {
                Some(b"h2") => true,
                None | Some(b"http/1.1") if options.alpn != AlpnPreference::Http2Only => false,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unsupported or missing negotiated ALPN",
                    ));
                }
            }
        } else {
            !forward_proxy && options.alpn == AlpnPreference::Http2Only
        };
        let io = ServiceInput::new(stream);
        let (sender, peer_settings) = if use_h2 {
            let (sender, peer_settings) =
                h2::connect(io, options.h2_settings).await.map_err(other)?;
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.h2 = Some(IdleH2 {
                    sender: sender.clone(),
                    peer_settings: peer_settings.clone(),
                    since: Instant::now(),
                });
                schedule_h2_expiry(&self.state, key.clone());
            }
            (Sender::Http2(sender), Some(peer_settings))
        } else {
            let (sender, connection) = http1::handshake::<_, RequestBody>(io)
                .await
                .map_err(other)?;
            tokio::spawn(async move {
                if let Err(error) = connection.with_upgrades().await {
                    tracing::debug!(%error, "HTTP/1 connection closed with an error");
                }
            });
            (Sender::Http1(sender), None)
        };
        Ok(PooledConnection {
            sender: Some(sender),
            forward_proxy,
            proxy_authorization: authorization,
            peer_settings,
            guard: Some(guard),
        })
    }
}
