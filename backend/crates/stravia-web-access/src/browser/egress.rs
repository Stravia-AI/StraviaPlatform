//! 内嵌 Moli 保留端到端 TLS；这里只校验目标并转发原始字节。
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_rustls::{rustls, TlsConnector};
use url::Url;

use crate::{address_policy::is_public_ip, fetch::policy::validate_url, outbound::ResolvedProxy};

const HEAD_LIMIT: usize = 32 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}
type Stream = Pin<Box<dyn Duplex>>;

pub(super) struct EgressProxy {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl EgressProxy {
    pub(super) async fn start(snapshot: ResolvedProxy) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let snapshot = Arc::new(snapshot);
        let task = tokio::spawn(async move {
            // JoinSet 被取消或丢弃时也取消所有正在运行的连接。
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let snapshot = snapshot.clone();
                            connections.spawn(async move { let _ = serve(stream, &snapshot).await; });
                        }
                        Err(_) => break,
                    },
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { address, task })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(super) async fn shutdown(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Request {
    method: String,
    url: Url,
    headers: Vec<(String, String)>,
    length: Option<u64>,
    chunked: bool,
    upgrade: bool,
    expect_continue: bool,
}

async fn read_head<S: AsyncRead + Unpin + ?Sized>(stream: &mut S) -> Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    loop {
        ensure!(head.len() < HEAD_LIMIT, "proxy header too large");
        head.push(stream.read_u8().await?);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
    }
}

fn parse_head(bytes: &[u8]) -> Result<Request> {
    let text = std::str::from_utf8(bytes).context("invalid proxy header encoding")?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let mut first = lines
        .next()
        .ok_or_else(|| anyhow!("missing request line"))?
        .split(' ');
    let method = first.next().unwrap_or_default();
    ensure!(
        !method.is_empty() && method.bytes().all(is_token),
        "invalid method"
    );
    let target = first.next().ok_or_else(|| anyhow!("missing target"))?;
    ensure!(
        first.next() == Some("HTTP/1.1") && first.next().is_none(),
        "invalid proxy request line"
    );
    let connect = method == "CONNECT";
    let url = if connect {
        authority_url(target)?
    } else {
        let url = validate_url(target).map_err(|_| anyhow!("blocked proxy target"))?;
        ensure!(
            url.scheme() == "http" && url.fragment().is_none(),
            "invalid forward target"
        );
        url
    };
    let mut headers = Vec::new();
    let mut host = None;
    let mut length = None;
    let mut chunked = false;
    let mut upgrade = false;
    let mut expect_continue = false;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid header"))?;
        ensure!(
            !name.is_empty() && name.bytes().all(is_token),
            "invalid header name"
        );
        ensure!(
            value.bytes().all(|b| b == b'\t' || (b >= 32 && b != 127)),
            "invalid header value"
        );
        let name = name.to_ascii_lowercase();
        let value = value.trim().to_owned();
        match name.as_str() {
            "host" => {
                ensure!(host.is_none(), "duplicate host");
                host = Some(value.clone());
            }
            "content-length" => {
                ensure!(
                    length.is_none()
                        && !value.is_empty()
                        && value.bytes().all(|b| b.is_ascii_digit()),
                    "invalid content length"
                );
                length = Some(value.parse::<u64>()?);
            }
            "transfer-encoding" => {
                ensure!(
                    !chunked && value.eq_ignore_ascii_case("chunked"),
                    "unsupported transfer encoding"
                );
                chunked = true;
            }
            "upgrade" => {
                ensure!(
                    value.eq_ignore_ascii_case("websocket") && !upgrade,
                    "unsupported upgrade"
                );
                upgrade = true;
            }
            "expect" => {
                ensure!(
                    value.eq_ignore_ascii_case("100-continue"),
                    "unsupported expectation"
                );
                expect_continue = true;
            }
            _ => {}
        }
        headers.push((name, value));
    }
    ensure!(!(chunked && length.is_some()), "ambiguous request framing");
    ensure!(
        url.port_or_known_default().is_some_and(|port| port != 0),
        "invalid target port"
    );
    for (_, value) in headers.iter().filter(|(name, _)| name == "connection") {
        ensure!(
            value.split(',').all(|token| {
                let token = token.trim();
                !token.is_empty()
                    && token.bytes().all(is_token)
                    && !["host", "content-length", "transfer-encoding"]
                        .iter()
                        .any(|name| token.eq_ignore_ascii_case(name))
            }),
            "invalid connection options"
        );
    }
    let host = host.ok_or_else(|| anyhow!("missing host"))?;
    let host_url = if connect {
        authority_url(&host)?
    } else {
        host_url(&host)?
    };
    ensure!(
        host_url.host() == url.host()
            && host_url.port_or_known_default() == url.port_or_known_default(),
        "host does not match target"
    );
    ensure!(
        !connect || (!chunked && length.unwrap_or(0) == 0 && !upgrade),
        "invalid CONNECT framing"
    );
    ensure!(
        !upgrade || (!chunked && length.unwrap_or(0) == 0),
        "invalid upgrade framing"
    );
    Ok(Request {
        method: method.to_owned(),
        url,
        headers,
        length,
        chunked,
        upgrade,
        expect_continue,
    })
}

fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn host_url(authority: &str) -> Result<Url> {
    ensure!(
        !authority.is_empty()
            && !authority
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b"/@?#\\".contains(&b)),
        "invalid authority"
    );
    validate_url(&format!("http://{authority}/")).map_err(|_| anyhow!("blocked proxy target"))
}

fn authority_url(authority: &str) -> Result<Url> {
    let (_, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("CONNECT requires port"))?;
    ensure!(
        !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>()? != 0,
        "invalid CONNECT port"
    );
    let mut url = host_url(authority)?;
    // 先按 HTTP 解析会消去显式的 80，切换 scheme 后必须恢复原端口。
    url.set_scheme("https")
        .map_err(|_| anyhow!("invalid CONNECT scheme"))?;
    url.set_port(Some(port.parse()?))
        .map_err(|_| anyhow!("invalid CONNECT port"))?;
    Ok(url)
}

async fn serve(client: TcpStream, snapshot: &ResolvedProxy) -> Result<()> {
    // 缓冲读取避免每字节一次系统调用；隧道继续消费同一 reader，保留预读的 TLS 字节。
    let mut client = BufReader::new(client);
    let established = timeout(HANDSHAKE_TIMEOUT, async {
        let head = read_head(&mut client).await?;
        let request = parse_head(&head)?;
        let (upstream, absolute) =
            connect_target(snapshot, &request.url, request.method == "CONNECT").await?;
        Ok::<_, anyhow::Error>((request, upstream, absolute))
    })
    .await;
    let (request, mut upstream, absolute) = match established {
        Ok(Ok(value)) => value,
        _ => {
            // 固定错误响应不包含目标、代理凭据或解析器错误信息。
            let _ = timeout(
                Duration::from_secs(1),
                client.write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ),
            )
            .await;
            return Ok(());
        }
    };
    if request.method == "CONNECT" {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }
    timeout(REQUEST_TIMEOUT, async {
        let target = if absolute {
            request.url.as_str()
        } else {
            &request.url[url::Position::BeforePath..]
        };
        let mut head = format!("{} {} HTTP/1.1\r\n", request.method, target);
        let connection_tokens: Vec<_> = request
            .headers
            .iter()
            .filter(|(name, _)| name == "connection")
            .flat_map(|(_, value)| value.split(','))
            .map(|value| value.trim().to_ascii_lowercase())
            .collect();
        // 重新生成 framing，避免 Connection 指定的 hop-by-hop 字段改变正文边界。
        for (name, value) in &request.headers {
            if matches!(
                name.as_str(),
                "connection"
                    | "proxy-connection"
                    | "proxy-authorization"
                    | "proxy-authenticate"
                    | "keep-alive"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "content-length"
                    | "expect"
                    | "upgrade"
            ) || connection_tokens.contains(name)
            {
                continue;
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        if let Some(length) = request.length {
            head.push_str(&format!("Content-Length: {length}\r\n"));
        }
        if request.chunked {
            head.push_str("Transfer-Encoding: chunked\r\n");
        }
        head.push_str(if request.upgrade {
            "Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
        } else {
            "Connection: close\r\n\r\n"
        });
        upstream.write_all(head.as_bytes()).await?;
        if request.expect_continue {
            client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        }
        upstream.flush().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    if request.upgrade {
        // 只有服务端明确接受升级后才允许双向传输，普通响应不开放第二条请求。
        loop {
            let head = timeout(HANDSHAKE_TIMEOUT, read_head(&mut upstream)).await??;
            let status = response_status(&head)?;
            client.write_all(&head).await?;
            if status == 101 {
                tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
                return Ok(());
            }
            if !(100..200).contains(&status) {
                break;
            }
        }
    }
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let upload = timeout(REQUEST_TIMEOUT, async {
        if request.chunked {
            copy_chunks(&mut client_read, &mut upstream_write).await?;
        } else if let Some(length) = request.length {
            copy_exact(&mut client_read, &mut upstream_write, length).await?;
        }
        upstream_write.flush().await?;
        Ok::<_, anyhow::Error>(())
    });
    let response = tokio::io::copy(&mut upstream_read, &mut client_write);
    tokio::pin!(upload, response);
    // 413 等提前响应不能被尚未完成的上传阻塞；正文之后绝不转发下一条请求。
    tokio::select! {
        result = &mut response => { result?; }
        result = &mut upload => { result??; response.await?; }
    }
    Ok(())
}

async fn copy_exact<R: AsyncRead + Unpin + ?Sized, W: AsyncWrite + Unpin + ?Sized>(
    reader: &mut R,
    writer: &mut W,
    length: u64,
) -> Result<()> {
    let copied = tokio::io::copy(&mut reader.take(length), writer).await?;
    ensure!(copied == length, "truncated request body");
    Ok(())
}

async fn copy_chunks<R: AsyncRead + Unpin + ?Sized, W: AsyncWrite + Unpin + ?Sized>(
    reader: &mut R,
    writer: &mut W,
) -> Result<()> {
    loop {
        let mut line = Vec::new();
        while !line.ends_with(b"\r\n") {
            ensure!(line.len() < 1024, "chunk line too large");
            line.push(reader.read_u8().await?);
        }
        let text = std::str::from_utf8(&line[..line.len() - 2])?;
        let size = text.split(';').next().unwrap_or_default();
        ensure!(
            !size.is_empty() && size.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid chunk size"
        );
        let size = u64::from_str_radix(size, 16)?;
        if size == 0 {
            // 丢弃 trailers，不能让它们覆盖 Host 或 framing 字段。
            let mut total = 0;
            loop {
                let mut trailer = Vec::new();
                while !trailer.ends_with(b"\r\n") {
                    ensure!(total < HEAD_LIMIT, "trailers too large");
                    trailer.push(reader.read_u8().await?);
                    total += 1;
                }
                if trailer == b"\r\n" {
                    break;
                }
                ensure!(
                    trailer[..trailer.len() - 2].contains(&b':'),
                    "invalid trailer"
                );
            }
            writer.write_all(b"0\r\n\r\n").await?;
            return Ok(());
        }
        writer.write_all(format!("{size:x}\r\n").as_bytes()).await?;
        copy_exact(reader, writer, size).await?;
        ensure!(
            reader.read_u8().await? == b'\r' && reader.read_u8().await? == b'\n',
            "invalid chunk ending"
        );
        writer.write_all(b"\r\n").await?;
    }
}

fn response_status(head: &[u8]) -> Result<u16> {
    let line = std::str::from_utf8(head)?
        .split("\r\n")
        .next()
        .unwrap_or_default();
    let mut parts = line.split(' ');
    ensure!(
        matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")),
        "invalid proxy response"
    );
    let status = parts
        .next()
        .ok_or_else(|| anyhow!("missing proxy status"))?;
    ensure!(
        status.len() == 3 && status.bytes().all(|b| b.is_ascii_digit()),
        "invalid proxy status"
    );
    Ok(status.parse()?)
}

async fn connect_target(
    snapshot: &ResolvedProxy,
    url: &Url,
    tunnel: bool,
) -> Result<(Stream, bool)> {
    let proxy = if snapshot
        .no_proxy
        .contains(url.host_str().unwrap_or_default())
    {
        None
    } else if url.scheme() == "https" {
        snapshot.https.as_ref()
    } else {
        snapshot.http.as_ref()
    };
    let Some(proxy) = proxy else {
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("missing target host"))?
            .trim_matches(['[', ']']);
        let port = url
            .port_or_known_default()
            .filter(|port| *port != 0)
            .ok_or_else(|| anyhow!("invalid target port"))?;
        let addresses: Vec<_> = tokio::net::lookup_host((host, port)).await?.collect();
        ensure!(
            !addresses.is_empty() && addresses.iter().all(|address| is_public_ip(address.ip())),
            "blocked target address"
        );
        for address in addresses {
            // 始终连接已验证的地址，不把域名交回连接器进行第二次 DNS 查询。
            if let Ok(stream) = TcpStream::connect(address).await {
                return Ok((Box::pin(BufReader::new(stream)), false));
            }
        }
        bail!("target connection failed");
    };
    ensure!(
        proxy.username().is_empty() && proxy.password().is_none(),
        "proxy credentials not supported"
    );
    let host = proxy
        .host_str()
        .ok_or_else(|| anyhow!("missing proxy host"))?
        .trim_matches(['[', ']']);
    let port = proxy
        .port_or_known_default()
        .or_else(|| (proxy.scheme() == "socks5h").then_some(1080))
        .ok_or_else(|| anyhow!("missing proxy port"))?;
    let socket = TcpStream::connect((host, port)).await?;
    let mut stream: Stream = if proxy.scheme() == "https" {
        // 工作区同时可能启用多个 crypto provider；不依赖进程级默认值或修改它。
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())?;
        Box::pin(BufReader::new(
            TlsConnector::from(Arc::new(config))
                .connect(name, socket)
                .await?,
        ))
    } else {
        Box::pin(BufReader::new(socket))
    };
    match proxy.scheme() {
        "socks5h" => {
            socks_connect(&mut stream, url).await?;
            Ok((stream, false))
        }
        "http" | "https" if tunnel => {
            let authority = target_authority(url)?;
            stream
                .write_all(
                    format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
                )
                .await?;
            let head = read_head(&mut stream).await?;
            ensure!(
                (200..300).contains(&response_status(&head)?),
                "upstream CONNECT rejected"
            );
            Ok((stream, false))
        }
        "http" | "https" => Ok((stream, true)),
        _ => bail!("unsupported proxy protocol"),
    }
}

fn target_authority(url: &Url) -> Result<String> {
    Ok(format!(
        "{}:{}",
        url.host_str().ok_or_else(|| anyhow!("missing host"))?,
        url.port_or_known_default()
            .ok_or_else(|| anyhow!("missing port"))?
    ))
}

async fn socks_connect<S: AsyncRead + AsyncWrite + Unpin + ?Sized>(
    stream: &mut S,
    url: &Url,
) -> Result<()> {
    stream.write_all(&[5, 1, 0]).await?;
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).await?;
    ensure!(greeting == [5, 0], "SOCKS authentication rejected");
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("missing SOCKS target"))?
        .trim_matches(['[', ']']);
    let mut request = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            ensure!(host.len() <= 255, "SOCKS hostname too long");
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(
        &url.port_or_known_default()
            .ok_or_else(|| anyhow!("missing SOCKS port"))?
            .to_be_bytes(),
    );
    stream.write_all(&request).await?;
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await?;
    ensure!(
        reply[0] == 5 && reply[1] == 0 && reply[2] == 0,
        "SOCKS connection rejected"
    );
    let count = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let count = stream.read_u8().await?;
            ensure!(count > 0, "invalid SOCKS bound hostname");
            usize::from(count)
        }
        _ => bail!("invalid SOCKS address type"),
    };
    let mut address = [0; 257];
    stream.read_exact(&mut address[..count + 2]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::{resolve_mode, OutboundProxyMode};

    fn snapshot(proxy: Option<Url>, bypass: bool) -> ResolvedProxy {
        resolve_mode(OutboundProxyMode::System, |key| match key {
            "HTTP_PROXY" | "HTTPS_PROXY" => proxy.as_ref().map(ToString::to_string),
            "NO_PROXY" if bypass => Some("*".to_owned()),
            _ => None,
        })
        .unwrap()
    }

    async fn exchange(proxy: &EgressProxy, bytes: &[u8]) -> Vec<u8> {
        timeout(Duration::from_secs(3), async {
            let mut stream = TcpStream::connect(proxy.address()).await.unwrap();
            stream.write_all(bytes).await.unwrap();
            let mut response = Vec::new();
            if let Err(error) = stream.read_to_end(&mut response).await {
                // 主动关闭仍带流水线输入的 socket 可产生 RST，但已完成响应必须可见。
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
                assert!(response.windows(4).any(|bytes| bytes == b"\r\n\r\n"));
            }
            response
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn egress_rejects_private_targets_and_ambiguous_requests() {
        let proxy = EgressProxy::start(snapshot(None, false)).await.unwrap();
        for request in [
            "GET http://127.0.0.1/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            "GET http://127.0.0.1../ HTTP/1.1\r\nHost: 127.0.0.1..\r\n\r\n",
            "CONNECT 192.168.1.1..:443 HTTP/1.1\r\nHost: 192.168.1.1..:443\r\n\r\n",
            "CONNECT localhost:443 HTTP/1.1\r\nHost: localhost:443\r\n\r\n",
            "GET http://example.com/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:80\r\n\r\n",
            "GET http://user:secret@example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
        ] {
            let response = exchange(&proxy, request.as_bytes()).await;
            assert!(response.starts_with(b"HTTP/1.1 502"));
            assert!(!String::from_utf8_lossy(&response).contains("secret"));
        }
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn egress_forward_body_redirect_and_pipeline_are_isolated() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let fixture = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = read_head(&mut stream).await.unwrap();
            assert!(head.starts_with(b"POST http://public.example/upload HTTP/1.1\r\n"));
            assert!(String::from_utf8_lossy(&head).contains("Connection: close\r\n"));
            let mut body = [0; 5];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"hello");
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            stream.shutdown().await.unwrap();
            let mut extra = Vec::new();
            stream.read_to_end(&mut extra).await.unwrap();
            assert!(extra.is_empty(), "pipelined request reached upstream");
        });
        let proxy = EgressProxy::start(snapshot(Some(endpoint), false))
            .await
            .unwrap();
        let response = exchange(&proxy, b"POST http://public.example/upload HTTP/1.1\r\nHost: public.example\r\nContent-Length: 5\r\n\r\nhelloGET http://127.0.0.1/private HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 302"));
        assert!(exchange(
            &proxy,
            b"GET http://127.0.0.1/private HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        )
        .await
        .starts_with(b"HTTP/1.1 502"));
        fixture.await.unwrap();
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn egress_no_proxy_pins_and_rejects_local_dns() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let snapshot = snapshot(Some(endpoint.clone()), true);
        // 单独覆盖连接阶段：NO_PROXY 必须绕开可信代理并校验解析所得地址。
        assert!(connect_target(&snapshot, &endpoint, false).await.is_err());
        assert!(timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn egress_connect_preserves_bytes_and_shutdown_closes_tunnels() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let fixture = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = read_head(&mut stream).await.unwrap();
            assert_eq!(
                head,
                b"CONNECT public.example:443 HTTP/1.1\r\nHost: public.example:443\r\n\r\n"
            );
            stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            let mut bytes = [0; 5];
            stream.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"\x16\x03\x01\x00\x00");
            stream.write_all(&bytes).await.unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        let proxy = EgressProxy::start(snapshot(Some(endpoint), false))
            .await
            .unwrap();
        let address = proxy.address();
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"CONNECT public.example:443 HTTP/1.1\r\nHost: public.example:443\r\n\r\n\x16\x03\x01\x00\x00").await.unwrap();
        assert!(read_head(&mut client)
            .await
            .unwrap()
            .starts_with(b"HTTP/1.1 200"));
        let mut bytes = [0; 5];
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"\x16\x03\x01\x00\x00");
        proxy.shutdown().await;
        timeout(Duration::from_secs(1), fixture)
            .await
            .unwrap()
            .unwrap();
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn egress_socks_uses_remote_domain_and_rejects_bad_reply() {
        for reply in [
            [5, 0, 0, 1],
            [5, 5, 0, 1],
            [4, 0, 0, 1],
            [5, 0, 1, 1],
            [5, 0, 0, 9],
        ] {
            let (mut client, mut server) = tokio::io::duplex(1024);
            let fixture = tokio::spawn(async move {
                let mut greeting = [0; 3];
                server.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 1, 0]);
                server.write_all(&[5, 0]).await.unwrap();
                let mut head = [0; 5];
                server.read_exact(&mut head).await.unwrap();
                assert_eq!(head, [5, 1, 0, 3, 14]);
                let mut target = [0; 16];
                server.read_exact(&mut target).await.unwrap();
                assert_eq!(&target[..14], b"public.example");
                assert_eq!(&target[14..], &443u16.to_be_bytes());
                server.write_all(&reply).await.unwrap();
                server.write_all(&[127, 0, 0, 1, 0, 80]).await.unwrap();
            });
            let result =
                socks_connect(&mut client, &Url::parse("https://public.example/").unwrap()).await;
            assert_eq!(result.is_ok(), reply == [5, 0, 0, 1]);
            fixture.await.unwrap();
        }
    }

    #[tokio::test]
    async fn egress_chunked_body_drops_trailers_and_leaves_next_request() {
        let mut source = &b"5;ext=ok\r\nhello\r\n0\r\nHost: 127.0.0.1\r\n\r\nNEXT"[..];
        let mut output = Vec::new();
        copy_chunks(&mut source, &mut output).await.unwrap();
        assert_eq!(output, b"5\r\nhello\r\n0\r\n\r\n");
        assert_eq!(source, b"NEXT");
    }
}
