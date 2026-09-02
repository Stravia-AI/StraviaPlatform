use std::{net::IpAddr, sync::Arc, time::Duration};

use anyhow::{Context, anyhow};
use stravia_web_access::renderer::{PageRendererConfig, is_public_web_request};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};
use url::Url;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

trait ProxyStream: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite + ?Sized> ProxyStream for T {}
type BoxStream = Box<dyn ProxyStream + Unpin + Send>;

/// Tauri 的三个桌面 WebView 后端没有统一的子资源拦截接口，因此把渲染流量收口到
/// loopback proxy，在跨平台的同一位置执行公网请求策略与上游代理选择。
pub(crate) struct GuardedProxy {
    address: std::net::SocketAddr,
    accept_task: JoinHandle<()>,
}

impl GuardedProxy {
    pub(crate) async fn start(
        config: PageRendererConfig,
        enforce_public_web: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind the desktop render proxy")?;
        let address = listener
            .local_addr()
            .context("failed to read the desktop render proxy address")?;
        let tls_config = config
            .uses_https_proxy()
            .then(native_tls_config)
            .transpose()?;
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let config = config.clone();
                let tls_config = tls_config.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        handle_connection(stream, config, tls_config, enforce_public_web).await
                    {
                        tracing::debug!(error = ?error, "desktop render proxy connection failed");
                    }
                });
            }
        });
        Ok(Arc::new(Self {
            address,
            accept_task,
        }))
    }

    pub(crate) fn url(&self) -> Url {
        Url::parse(&format!("http://{}", self.address)).expect("loopback render proxy URL is valid")
    }
}

impl Drop for GuardedProxy {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

struct ParsedRequest {
    method: String,
    target: Url,
    connect: bool,
    header_end: usize,
}

async fn handle_connection(
    mut client: TcpStream,
    config: PageRendererConfig,
    tls_config: Option<Arc<ClientConfig>>,
    enforce_public_web: bool,
) -> anyhow::Result<()> {
    let buffer = read_request_header(&mut client).await?;
    let request = parse_request(&buffer)?;
    if enforce_public_web {
        let guard_url = request.target.to_string();
        let allowed = tokio::task::spawn_blocking(move || is_public_web_request(&guard_url))
            .await
            .context("desktop render request guard failed")?;
        if !allowed {
            client
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await?;
            return Ok(());
        }
    }

    let host = request
        .target
        .host_str()
        .ok_or_else(|| anyhow!("render proxy target has no host"))?;
    let port = request
        .target
        .port_or_known_default()
        .ok_or_else(|| anyhow!("render proxy target has no port"))?;
    let upstream = config.upstream_proxy_for(&request.target);
    let (mut remote, forwards_proxy_request) =
        connect_remote(upstream, host, port, tls_config).await?;

    if forwards_proxy_request {
        remote.write_all(&buffer).await?;
    } else if request.connect {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if buffer.len() > request.header_end {
            remote.write_all(&buffer[request.header_end..]).await?;
        }
    } else {
        remote
            .write_all(&origin_form_request(&buffer, &request)?)
            .await?;
    }

    tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

async fn read_request_header(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(4096);
    loop {
        if header_end(&buffer).is_some() {
            return Ok(buffer);
        }
        if buffer.len() >= MAX_HEADER_BYTES {
            return Err(anyhow!("render proxy request header exceeds safety limit"));
        }
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(anyhow!("render proxy client closed before sending headers"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn parse_request(buffer: &[u8]) -> anyhow::Result<ParsedRequest> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    let parsed = request
        .parse(buffer)
        .context("invalid render proxy request")?;
    let httparse::Status::Complete(header_end) = parsed else {
        return Err(anyhow!("incomplete render proxy request"));
    };
    let method = request
        .method
        .ok_or_else(|| anyhow!("render proxy request has no method"))?
        .to_string();
    let path = request
        .path
        .ok_or_else(|| anyhow!("render proxy request has no target"))?;
    let connect = method.eq_ignore_ascii_case("CONNECT");
    let target = if connect {
        Url::parse(&format!("https://{path}/")).context("invalid render proxy CONNECT target")?
    } else {
        Url::parse(path).context("render proxy requires an absolute request target")?
    };
    if !matches!(target.scheme(), "http" | "https") {
        return Err(anyhow!("render proxy only accepts HTTP(S) targets"));
    }
    Ok(ParsedRequest {
        method,
        target,
        connect,
        header_end,
    })
}

async fn connect_remote(
    upstream: Option<&Url>,
    target_host: &str,
    target_port: u16,
    tls_config: Option<Arc<ClientConfig>>,
) -> anyhow::Result<(BoxStream, bool)> {
    let Some(upstream) = upstream else {
        return Ok((
            Box::new(connect_tcp(target_host, target_port).await?),
            false,
        ));
    };
    let proxy_host = upstream
        .host_str()
        .ok_or_else(|| anyhow!("upstream proxy has no host"))?;
    let proxy_port = upstream
        .port_or_known_default()
        .ok_or_else(|| anyhow!("upstream proxy has no port"))?;

    match upstream.scheme() {
        "http" => Ok((Box::new(connect_tcp(proxy_host, proxy_port).await?), true)),
        "https" => {
            let tls_config = tls_config
                .ok_or_else(|| anyhow!("HTTPS proxy TLS configuration is unavailable"))?;
            let tcp = connect_tcp(proxy_host, proxy_port).await?;
            let server_name = ServerName::try_from(proxy_host.to_string())
                .map_err(|_| anyhow!("invalid HTTPS proxy server name"))?;
            let tls = tokio::time::timeout(
                CONNECT_TIMEOUT,
                TlsConnector::from(tls_config).connect(server_name, tcp),
            )
            .await
            .context("HTTPS proxy TLS handshake timed out")?
            .context("HTTPS proxy TLS handshake failed")?;
            Ok((Box::new(tls), true))
        }
        "socks5" => {
            let mut stream = connect_tcp(proxy_host, proxy_port).await?;
            socks5_connect(&mut stream, target_host, target_port).await?;
            Ok((Box::new(stream), false))
        }
        scheme => Err(anyhow!("unsupported desktop render proxy scheme: {scheme}")),
    }
}

async fn connect_tcp(host: &str, port: u16) -> anyhow::Result<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .context("render proxy connection timed out")?
        .with_context(|| format!("failed to connect render proxy route {host}:{port}"))
}

async fn socks5_connect(
    stream: &mut TcpStream,
    target_host: &str,
    target_port: u16,
) -> anyhow::Result<()> {
    stream.write_all(&[5, 1, 0]).await?;
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).await?;
    if greeting != [5, 0] {
        return Err(anyhow!("SOCKS5 proxy rejected unauthenticated access"));
    }

    let mut request = vec![5, 1, 0];
    if let Ok(address) = target_host.parse::<IpAddr>() {
        match address {
            IpAddr::V4(address) => {
                request.push(1);
                request.extend_from_slice(&address.octets());
            }
            IpAddr::V6(address) => {
                request.push(4);
                request.extend_from_slice(&address.octets());
            }
        }
    } else {
        let host = target_host.as_bytes();
        let length = u8::try_from(host.len()).context("SOCKS5 target hostname is too long")?;
        request.extend_from_slice(&[3, length]);
        request.extend_from_slice(host);
    }
    request.extend_from_slice(&target_port.to_be_bytes());
    stream.write_all(&request).await?;

    let mut response = [0_u8; 4];
    stream.read_exact(&mut response).await?;
    if response[0] != 5 || response[1] != 0 {
        return Err(anyhow!("SOCKS5 proxy failed with status {}", response[1]));
    }
    let address_bytes = match response[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0_u8; 1];
            stream.read_exact(&mut length).await?;
            usize::from(length[0])
        }
        value => return Err(anyhow!("SOCKS5 proxy returned address type {value}")),
    };
    let mut remainder = vec![0_u8; address_bytes + 2];
    stream.read_exact(&mut remainder).await?;
    Ok(())
}

fn origin_form_request(buffer: &[u8], request: &ParsedRequest) -> anyhow::Result<Vec<u8>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !parsed
        .parse(buffer)
        .context("invalid render proxy origin request")?
        .is_complete()
    {
        return Err(anyhow!("render proxy origin request is incomplete"));
    }
    let mut origin = request.target.path().to_string();
    if let Some(query) = request.target.query() {
        origin.push('?');
        origin.push_str(query);
    }
    let mut rewritten = format!("{} {origin} HTTP/1.1\r\n", request.method).into_bytes();
    for header in parsed.headers {
        if header.name.eq_ignore_ascii_case("connection")
            || header.name.eq_ignore_ascii_case("proxy-connection")
        {
            continue;
        }
        rewritten.extend_from_slice(header.name.as_bytes());
        rewritten.extend_from_slice(b": ");
        rewritten.extend_from_slice(header.value);
        rewritten.extend_from_slice(b"\r\n");
    }
    rewritten.extend_from_slice(b"Connection: close\r\n\r\n");
    rewritten.extend_from_slice(&buffer[request.header_end..]);
    Ok(rewritten)
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn native_tls_config() -> anyhow::Result<Arc<ClientConfig>> {
    let native = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    for certificate in native.certs {
        roots
            .add(certificate)
            .context("failed to load a native root certificate")?;
    }
    if roots.is_empty() {
        return Err(anyhow!(
            "no native root certificates are available for HTTPS proxies"
        ));
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_absolute_http_target_to_origin_form() {
        let request = b"GET http://example.com/path?q=1 HTTP/1.1\r\nHost: example.com\r\n\r\nbody";
        let parsed = parse_request(request).expect("proxy request");
        let rewritten = origin_form_request(request, &parsed).expect("origin request");
        assert_eq!(
            rewritten,
            b"GET /path?q=1 HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\nbody"
        );
    }

    #[tokio::test]
    async fn blocks_connect_requests_to_loopback_targets() {
        let target = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("target listener");
        let target_address = target.local_addr().expect("target address");
        let proxy = GuardedProxy::start(PageRendererConfig::direct(), true)
            .await
            .expect("guarded proxy");
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("proxy client");
        client
            .write_all(
                format!("CONNECT {target_address} HTTP/1.1\r\nHost: {target_address}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("CONNECT request");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("proxy response");
        assert!(response.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));

        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err(),
            "blocked target must not receive a connection"
        );
    }
}
