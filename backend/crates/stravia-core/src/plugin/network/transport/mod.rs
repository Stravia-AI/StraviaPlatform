//! Outbound HTTP/1 and HTTP/2 transport with explicit routing.

mod body;
mod capture;
mod connector;
mod h1;
mod h2;
mod pool;
mod route;

#[cfg(test)]
mod h2_wire_tests;
#[cfg(test)]
mod keepalive_tests;
#[cfg(test)]
mod length_tests;
#[cfg(test)]
mod observer_tests;
#[cfg(test)]
mod proxy_tests;
#[cfg(test)]
mod proxy_tls_tests;
#[cfg(test)]
mod reuse_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod upgrade_tests;
#[cfg(test)]
mod wire_tests;

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use rama_core::extensions::ExtensionsRef;
use rama_http::io::upgrade::{Upgraded, handle_upgrade};
use rama_http_core::body::Incoming;
use rama_http_types::{
    Response, Version,
    body::util::BodyExt,
    header::{HeaderName, HeaderValue},
    proto::h2::ext::Protocol,
};
use rustls::ClientConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::outbound_proxy::OutboundProxySettings;
use pool::{AlpnPreference, ConnectOptions, Pool, Sender};

pub(crate) use connector::{Resolver, SystemResolver};
pub(crate) use h2::H2Settings;

pub(crate) use stravia_vendor_sdk::net::Field;

pub(crate) struct RequestPlan {
    pub method: String,
    pub uri: String,
    pub fields: Vec<Field>,
    pub body: Bytes,
    pub observer: Option<Box<dyn FnOnce(RequestHead) + Send>>,
}

/// Final protocol-specific request head, captured immediately before dispatch.
pub(crate) struct RequestHead {
    pub fields: Vec<Field>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum HttpVersionPref {
    Auto,
    Http1,
    Http2,
}

/// A resolved shape and explicit proxy policy. The caller owns authorization,
/// redirects and deadlines. TLS configurations must use the platform verifier.
#[derive(Clone)]
pub(crate) struct Route {
    pub shape_name: String,
    pub shape_revision: u64,
    pub tls: Arc<ClientConfig>,
    pub tls_session_policy: super::shape::SessionPolicy,
    pub proxy_tls: Arc<ClientConfig>,
    pub proxy: OutboundProxySettings,
    pub resolver: Arc<dyn Resolver>,
    pub version: HttpVersionPref,
    pub h2: H2Settings,
    pub omit_h2_content_length: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("proxy failed: {0}")]
    Proxy(String),
    #[error("TLS failed: {0}")]
    Tls(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("transport timed out")]
    Timeout,
    #[error("transport cancelled")]
    Cancelled,
}

#[derive(Debug)]
pub(crate) struct Head {
    pub status: u16,
    pub fields: Vec<Field>,
}

pub(crate) enum UpgradeResponse {
    Upgraded(Head, UpgradedIo),
    Rejected(UpstreamResponse),
}

pub(crate) struct UpstreamResponse {
    pub head: Head,
    pub body: UpstreamBody,
}

pub(crate) struct UpstreamBody(Incoming);
impl UpstreamBody {
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, TransportError> {
        while let Some(frame) = self.0.frame().await {
            let frame = frame.map_err(|e| TransportError::Protocol(e.to_string()))?;
            if let Ok(data) = frame.into_data() {
                return Ok(Some(data));
            }
        }
        Ok(None)
    }
}

/// Rama's upgraded IO retains any bytes read beyond the handshake.
pub(crate) struct UpgradedIo(Upgraded);
impl AsyncRead for UpgradedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for UpgradedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[derive(Clone, Default)]
pub(crate) struct VendorTransport {
    pool: Pool,
}

impl VendorTransport {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn send(
        &self,
        plan: RequestPlan,
        route: Route,
    ) -> Result<UpstreamResponse, TransportError> {
        let response = self.request(plan, route, false).await?;
        let head = response_head(&response);
        Ok(UpstreamResponse {
            head,
            body: UpstreamBody(response.into_body()),
        })
    }

    pub(crate) async fn upgrade(
        &self,
        plan: RequestPlan,
        route: Route,
    ) -> Result<UpgradeResponse, TransportError> {
        let response = self.request(plan, route, true).await?;
        let head = response_head(&response);
        let expected = if response.version() == Version::HTTP_2 {
            200
        } else {
            101
        };
        if head.status != expected {
            return Ok(UpgradeResponse::Rejected(UpstreamResponse {
                head,
                body: UpstreamBody(response.into_body()),
            }));
        }
        let io = handle_upgrade(&response)
            .await
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        Ok(UpgradeResponse::Upgraded(head, UpgradedIo(io)))
    }

    async fn request(
        &self,
        mut plan: RequestPlan,
        route: Route,
        upgrade: bool,
    ) -> Result<Response<Incoming>, TransportError> {
        let mut uri =
            url::Url::parse(&plan.uri).map_err(|e| TransportError::Protocol(e.to_string()))?;
        let proxy = route::select_proxy(&route.proxy, &uri)?;
        match uri.scheme() {
            "ws" => uri
                .set_scheme("http")
                .map_err(|_| TransportError::Protocol("invalid URL scheme".into()))?,
            "wss" => uri
                .set_scheme("https")
                .map_err(|_| TransportError::Protocol("invalid URL scheme".into()))?,
            "http" | "https" => {}
            _ => return Err(TransportError::Protocol("unsupported URL scheme".into())),
        }
        uri.set_fragment(None);
        let host = uri
            .host_str()
            .ok_or_else(|| TransportError::Protocol("URL has no host".into()))?
            .to_owned();
        let port = uri
            .port_or_known_default()
            .ok_or_else(|| TransportError::Protocol("URL has no port".into()))?;
        plan.uri = uri.to_string();
        if route.proxy.force_http1 && matches!(route.version, HttpVersionPref::Http2) {
            return Err(TransportError::Proxy("proxy policy requires HTTP/1".into()));
        }
        let alpn = match (route.proxy.force_http1, route.version) {
            (true, _) | (_, HttpVersionPref::Http1) => AlpnPreference::Http1Only,
            (_, HttpVersionPref::Auto) => AlpnPreference::Http2ThenHttp1,
            (_, HttpVersionPref::Http2) => AlpnPreference::Http2Only,
        };
        let observer = plan.observer.take();
        let pseudo_order = observer
            .as_ref()
            .and_then(|_| route.h2.pseudo_header_order.clone());
        let mut connection = self
            .pool
            .acquire(ConnectOptions {
                route: proxy,
                host,
                port,
                scheme: uri.scheme().to_owned(),
                target_tls: Some(route.tls),
                tls_session_policy: route.tls_session_policy,
                proxy_tls: Some(route.proxy_tls),
                resolver: route.resolver,
                shape_name: route.shape_name,
                shape_revision: route.shape_revision,
                alpn,
                h2_settings: route.h2,
            })
            .await
            .map_err(|e| match connector::error_category(&e) {
                Some(connector::ErrorCategory::Proxy) => TransportError::Proxy(e.to_string()),
                Some(connector::ErrorCategory::Tls) => TransportError::Tls(e.to_string()),
                _ if e.kind() == std::io::ErrorKind::TimedOut => TransportError::Timeout,
                _ if e.kind() == std::io::ErrorKind::Interrupted => TransportError::Cancelled,
                _ => TransportError::Connect(e.to_string()),
            })?;
        let is_h2 = matches!(connection.sender.as_ref(), Some(Sender::Http2(_)));
        if upgrade && is_h2 {
            let peer = connection.peer_settings.as_ref().ok_or_else(|| {
                TransportError::Protocol("HTTP/2 peer settings unavailable".into())
            })?;
            if h2::peer_extended_connect_enabled(peer).await != Some(true) {
                return Err(TransportError::Protocol(
                    "HTTP/2 peer does not enable extended CONNECT".into(),
                ));
            }
            plan.method = "CONNECT".into();
            // RFC 8441 replaces the HTTP/1 key/accept challenge with the
            // extended CONNECT protocol identifier.
            plan.fields
                .retain(|field| !field.name.eq_ignore_ascii_case("sec-websocket-key"));
        }
        let mut request = h1::request(
            plan,
            if is_h2 {
                Version::HTTP_2
            } else {
                Version::HTTP_11
            },
            is_h2 && route.omit_h2_content_length,
        )?;
        if upgrade && is_h2 {
            request
                .extensions()
                .insert(Protocol::from_static("websocket"));
        }
        if connection.forward_proxy {
            // Rama's HTTP/1 encoder uses this extension to select absolute-form.
            // Credentials are carried only in the proxy authorization header.
            let mut address = url::Url::parse(&route.proxy.url)
                .map_err(|_| TransportError::Proxy("invalid proxy URL".into()))?;
            address
                .set_password(None)
                .map_err(|_| TransportError::Proxy("invalid proxy URL".into()))?;
            address
                .set_username("")
                .map_err(|_| TransportError::Proxy("invalid proxy URL".into()))?;
            let proxy =
                rama_net::address::ProxyAddress::try_from(address.as_str().trim_end_matches('/'))
                    .map_err(|_| TransportError::Proxy("invalid proxy address".into()))?;
            request
                .extensions()
                .insert(rama_net::client::ProxyRoute::Proxy(proxy));
        }
        if let Some(auth) = connection.proxy_authorization.as_ref() {
            request.headers_mut().append(
                HeaderName::from_static("proxy-authorization"),
                HeaderValue::from_str(auth).map_err(|e| TransportError::Proxy(e.to_string()))?,
            );
        }
        if let Some(observer) = observer {
            observer(capture::request_head(
                &request,
                pseudo_order.as_ref(),
                upgrade && is_h2,
            ));
        }
        match connection
            .sender
            .as_mut()
            .expect("new connection always has sender")
        {
            Sender::Http1(sender) => sender.send_request(request).await,
            Sender::Http2(sender) => sender.send_request(request).await,
        }
        .map_err(|e| TransportError::Protocol(e.to_string()))
    }
}

fn response_head(response: &Response<Incoming>) -> Head {
    Head {
        status: response.status().as_u16(),
        fields: response
            .headers()
            .ordered_iter()
            .map(|(name, value)| Field {
                name: name.to_string(),
                bytes: value.as_bytes().to_vec(),
            })
            .collect(),
    }
}
