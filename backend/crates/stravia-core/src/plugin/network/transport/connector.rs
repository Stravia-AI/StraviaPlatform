//! Explicit TCP routing and TLS for outbound connections.
//!
//! This module never reads proxy environment variables. The caller must choose
//! `Route::Direct` when proxying is disabled or the destination matches the
//! application's outbound-proxy bypass policy, and otherwise parse the selected
//! proxy URL with `Proxy::from_url`. Bypass matching belongs to that policy, not
//! to the dialer. TLS configurations must use certificate verifiers appropriate
//! for their respective peers; the dialer supplies each peer's actual hostname
//! (or IP address) to rustls for verification.

use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use percent_encoding::percent_decode_str;
use rustls::{ClientConfig, pki_types::ServerName};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, lookup_host},
};
use tokio_rustls::TlsConnector;
use url::{Host, Url};

/// A stream suitable for passing to an asynchronous HTTP implementation.
pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}
pub type BoxStream = Box<dyn IoStream>;

/// Resolves locally dialed hosts. SOCKS5h destination names are deliberately
/// never passed to this resolver; the proxy resolves them instead.
pub trait Resolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>>;
}

/// The Tokio/system DNS resolver, usable when no custom override is needed.
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async move { Ok(lookup_host((host, port)).await?.collect()) })
    }
}

async fn resolve(resolver: &dyn Resolver, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    // Literal IPs need no DNS, but names always go through the supplied resolver.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addresses = resolver
        .resolve(host, port)
        .await
        .map_err(|e| stage(ErrorCategory::Dns, e))?;
    if addresses.is_empty() {
        return Err(stage(
            ErrorCategory::Dns,
            io::Error::new(io::ErrorKind::NotFound, "DNS lookup returned no addresses"),
        ));
    }
    Ok(addresses)
}

async fn dial(resolver: &dyn Resolver, host: &str, port: u16) -> io::Result<TcpStream> {
    TcpStream::connect(resolve(resolver, host, port).await?.as_slice())
        .await
        .map_err(|e| stage(ErrorCategory::Connect, e))
}

/// Routes compare and hash by their complete configuration, including credentials.
/// Neither routes nor proxies implement `Debug`, to avoid accidentally logging secrets.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Route {
    Direct,
    Proxy(Proxy),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ProxyKind {
    Http,
    Https,
    Socks5,
    Socks5h,
}

/// A selected proxy. Credentials are deliberately not exposed through `Debug`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Proxy {
    kind: ProxyKind,
    host: String,
    port: u16,
    credentials: Option<(String, String)>,
}

/// Stage at which a connection attempt failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCategory {
    Dns,
    Connect,
    Proxy,
    Tls,
}

#[derive(Debug)]
struct StageError {
    category: ErrorCategory,
    source: io::Error,
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, f)
    }
}
impl std::error::Error for StageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn stage(category: ErrorCategory, source: io::Error) -> io::Error {
    io::Error::new(source.kind(), StageError { category, source })
}

/// Classify a dial failure without inspecting error text. Invalid caller input
/// may be unclassified; the original error remains available as its source.
pub fn error_category(error: &io::Error) -> Option<ErrorCategory> {
    error
        .get_ref()?
        .downcast_ref::<StageError>()
        .map(|e| e.category)
}

fn proxy_error(error: io::Error) -> io::Error {
    if error_category(&error).is_some() {
        error
    } else {
        stage(ErrorCategory::Proxy, error)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn protocol_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl Proxy {
    /// Whether HTTP requests can be sent to this proxy in absolute-form.
    pub fn is_http_forward_proxy(&self) -> bool {
        matches!(self.kind, ProxyKind::Http | ProxyKind::Https)
    }

    /// Parse an explicitly selected HTTP, HTTPS, SOCKS5 or SOCKS5h proxy URL.
    pub fn from_url(value: &str) -> io::Result<Self> {
        let url = Url::parse(value).map_err(|_| invalid("invalid proxy URL"))?;
        let (kind, default_port) = match url.scheme() {
            "http" => (ProxyKind::Http, 80),
            "https" => (ProxyKind::Https, 443),
            "socks5" => (ProxyKind::Socks5, 1080),
            "socks5h" => (ProxyKind::Socks5h, 1080),
            _ => return Err(invalid("unsupported proxy URL scheme")),
        };
        if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
            return Err(invalid(
                "proxy URL must not contain a path, query or fragment",
            ));
        }
        let host = match url
            .host()
            .ok_or_else(|| invalid("proxy URL requires a host"))?
        {
            Host::Domain(host) => host.to_owned(),
            Host::Ipv4(host) => host.to_string(),
            Host::Ipv6(host) => host.to_string(),
        };
        let port = url.port().unwrap_or(default_port);
        if port == 0 {
            return Err(invalid("proxy port must be nonzero"));
        }
        let credentials = if !url.username().is_empty() || url.password().is_some() {
            let decode = |part: &str| {
                percent_decode_str(part)
                    .decode_utf8()
                    .map(|s| s.into_owned())
                    .map_err(|_| invalid("proxy credentials must be UTF-8"))
            };
            Some((
                decode(url.username())?,
                decode(url.password().unwrap_or(""))?,
            ))
        } else {
            None
        };
        Ok(Self {
            kind,
            host,
            port,
            credentials,
        })
    }
}

/// Connect directly to an HTTP(S) proxy for forwarding an HTTP request in
/// absolute-form. The returned optional value is the Proxy-Authorization header
/// value, and must be attached only to requests sent to this proxy, never to an
/// origin or a redirected request on another route. This function does not
/// select a route or apply bypass rules.
pub async fn connect_http_forward(
    proxy: &Proxy,
    proxy_tls: Option<Arc<ClientConfig>>,
    resolver: &dyn Resolver,
) -> io::Result<(BoxStream, Option<String>)> {
    if !matches!(proxy.kind, ProxyKind::Http | ProxyKind::Https) {
        return Err(invalid("HTTP forwarding requires an HTTP or HTTPS proxy"));
    }
    let authorization = basic_authorization(proxy.credentials.as_ref())?;
    let stream = connect_proxy(proxy, proxy_tls, resolver).await?;
    Ok((stream, authorization))
}

async fn connect_proxy(
    proxy: &Proxy,
    proxy_tls: Option<Arc<ClientConfig>>,
    resolver: &dyn Resolver,
) -> io::Result<BoxStream> {
    let tcp = dial(resolver, &proxy.host, proxy.port).await?;
    match proxy.kind {
        ProxyKind::Https => {
            let config =
                proxy_tls.ok_or_else(|| invalid("HTTPS proxy requires a TLS configuration"))?;
            // Both CONNECT and absolute-form forwarding below speak HTTP/1.1.
            let mut config = (*config).clone();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            let server_name = server_name(&proxy.host)?;
            Ok(Box::new(
                TlsConnector::from(Arc::new(config))
                    .connect(server_name, tcp)
                    .await
                    .map_err(|e| stage(ErrorCategory::Tls, e))?,
            ))
        }
        _ => Ok(Box::new(tcp)),
    }
}

/// Dial a direct TCP connection or a tunnel through the explicitly selected
/// proxy. HTTPS proxies require `proxy_tls`; other routes ignore it.
pub async fn connect(
    host: &str,
    port: u16,
    route: &Route,
    proxy_tls: Option<Arc<ClientConfig>>,
    resolver: &dyn Resolver,
) -> io::Result<BoxStream> {
    if port == 0 {
        return Err(invalid("target port must be nonzero"));
    }
    // Parse the host separately from the port to prevent CONNECT request
    // injection and to canonicalize domain names and IPv6 addresses.
    let target = parse_target(host)?;
    let target_host = match &target {
        Host::Domain(host) => host.clone(),
        Host::Ipv4(host) => host.to_string(),
        Host::Ipv6(host) => host.to_string(),
    };
    match route {
        Route::Direct => Ok(Box::new(dial(resolver, &target_host, port).await?)),
        Route::Proxy(proxy) => {
            let mut stream = connect_proxy(proxy, proxy_tls, resolver).await?;
            match proxy.kind {
                ProxyKind::Http | ProxyKind::Https => {
                    http_connect(&mut stream, &target, port, proxy.credentials.as_ref())
                        .await
                        .map_err(proxy_error)?;
                }
                ProxyKind::Socks5 | ProxyKind::Socks5h => {
                    socks_connect(
                        &mut stream,
                        &target,
                        port,
                        proxy.credentials.as_ref(),
                        matches!(proxy.kind, ProxyKind::Socks5h),
                        resolver,
                    )
                    .await
                    .map_err(proxy_error)?;
                }
            }
            Ok(stream)
        }
    }
}

/// Dial the selected route, then establish TLS to the target using its own
/// configuration and hostname. This also supports TLS nested inside HTTPS CONNECT.
pub async fn connect_tls(
    host: &str,
    port: u16,
    route: &Route,
    proxy_tls: Option<Arc<ClientConfig>>,
    target_tls: Arc<ClientConfig>,
    resolver: &dyn Resolver,
) -> io::Result<(BoxStream, Option<Vec<u8>>)> {
    let target = parse_target(host)?;
    let name = match target {
        Host::Domain(name) => name,
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    };
    let name = server_name(&name)?;
    let stream = connect(host, port, route, proxy_tls, resolver).await?;
    let stream = TlsConnector::from(target_tls)
        .connect(name, stream)
        .await
        .map_err(|e| stage(ErrorCategory::Tls, e))?;
    let alpn = stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    Ok((Box::new(stream), alpn))
}

fn parse_target(host: &str) -> io::Result<Host<String>> {
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        return Ok(match ip {
            IpAddr::V4(ip) => Host::Ipv4(ip),
            IpAddr::V6(ip) => Host::Ipv6(ip),
        });
    }
    Host::parse(host).map_err(|_| invalid("invalid target host"))
}

fn server_name(host: &str) -> io::Result<ServerName<'static>> {
    ServerName::try_from(host.to_owned()).map_err(|_| invalid("invalid TLS server name"))
}

fn basic_authorization(credentials: Option<&(String, String)>) -> io::Result<Option<String>> {
    let Some((user, password)) = credentials else {
        return Ok(None);
    };
    if user.contains(':') {
        return Err(invalid("HTTP proxy username must not contain a colon"));
    }
    Ok(Some(format!(
        "Basic {}",
        STANDARD.encode(format!("{user}:{password}"))
    )))
}

async fn http_connect(
    stream: &mut BoxStream,
    target: &Host<String>,
    port: u16,
    credentials: Option<&(String, String)>,
) -> io::Result<()> {
    let authority = match target {
        Host::Ipv6(ip) => format!("[{ip}]:{port}"),
        _ => format!("{target}:{port}"),
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(authorization) = basic_authorization(credentials)? {
        request.push_str("Proxy-Authorization: ");
        request.push_str(&authorization);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    // Reading exactly through the header terminator avoids losing tunneled bytes
    // that can immediately follow the proxy response.
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        header.push(byte[0]);
        if header.len() > 16 * 1024 {
            return Err(protocol_error("proxy response headers too large"));
        }
        if header.ends_with(b"\r\n\r\n") {
            let line_end = header
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or_else(|| protocol_error("invalid proxy response"))?;
            let line = std::str::from_utf8(&header[..line_end])
                .map_err(|_| protocol_error("invalid proxy status line"))?;
            let mut parts = line.splitn(3, ' ');
            let version = parts.next().unwrap_or("");
            let code = parts.next().unwrap_or("");
            if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
                || code.len() != 3
                || !code.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(protocol_error("invalid proxy status line"));
            }
            let status: u16 = code
                .parse()
                .map_err(|_| protocol_error("invalid proxy status"))?;
            if (100..200).contains(&status) {
                header.clear();
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("proxy CONNECT failed with status {status}"),
                ));
            }
            return Ok(());
        }
    }
}

async fn socks_connect(
    stream: &mut BoxStream,
    target: &Host<String>,
    port: u16,
    credentials: Option<&(String, String)>,
    remote_dns: bool,
    resolver: &dyn Resolver,
) -> io::Result<()> {
    if credentials.is_some() {
        stream.write_all(&[5, 2, 0, 2]).await?;
    } else {
        stream.write_all(&[5, 1, 0]).await?;
    }
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 5 {
        return Err(protocol_error("invalid SOCKS version"));
    }
    match reply[1] {
        0 => {}
        2 if credentials.is_some() => {
            let (user, password) = credentials.unwrap();
            if user.is_empty() || user.len() > 255 || password.is_empty() || password.len() > 255 {
                return Err(invalid(
                    "SOCKS credentials must each contain 1 to 255 bytes",
                ));
            }
            let mut auth = Vec::with_capacity(3 + user.len() + password.len());
            auth.extend_from_slice(&[1, user.len() as u8]);
            auth.extend_from_slice(user.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await?;
            stream.read_exact(&mut reply).await?;
            if reply != [1, 0] {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "SOCKS authentication failed",
                ));
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SOCKS proxy rejected authentication methods",
            ));
        }
    }
    let mut request = vec![5, 1, 0];
    match target {
        Host::Domain(name) if remote_dns => {
            if name.is_empty() || name.len() > 255 {
                return Err(invalid("SOCKS target hostname must contain 1 to 255 bytes"));
            }
            request.extend_from_slice(&[3, name.len() as u8]);
            request.extend_from_slice(name.as_bytes());
        }
        _ => {
            let ip = match target {
                Host::Ipv4(ip) => IpAddr::V4(*ip),
                Host::Ipv6(ip) => IpAddr::V6(*ip),
                Host::Domain(name) => resolve(resolver, name, port).await?[0].ip(),
            };
            match ip {
                IpAddr::V4(ip) => {
                    request.push(1);
                    request.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    request.push(4);
                    request.extend_from_slice(&ip.octets());
                }
            }
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).await?;
    if prefix[0] != 5 || prefix[2] != 0 {
        return Err(protocol_error("invalid SOCKS response"));
    }
    if prefix[1] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("SOCKS proxy connect failed with code {}", prefix[1]),
        ));
    }
    let address_len = match prefix[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0];
            stream.read_exact(&mut len).await?;
            usize::from(len[0])
        }
        _ => return Err(protocol_error("invalid SOCKS address type")),
    };
    let mut address = [0u8; 257];
    stream.read_exact(&mut address[..address_len + 2]).await?;
    Ok(())
}
