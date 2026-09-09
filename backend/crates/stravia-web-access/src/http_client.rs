use std::{fmt, io, net::SocketAddr, pin::Pin, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use http::{header, Method};
use moli_cookie_jar::{BrowserCookieStore, NetworkCookieRequestContext};
use moli_stealth_net::{Transport, TransportConfig, TransportFingerprint, TransportRequest};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio_util::io::StreamReader;
use url::Url;

use crate::outbound::ResolvedProxy;

const MAX_REDIRECTS: usize = 10;

/// 可重放的 HTTP 请求语义，不包含连接策略。
pub type Request = http::Request<Vec<u8>>;
/// 响应元数据包含最终 URL；解压后的正文单独返回。
pub type Response = http::Response<Url>;

#[derive(Debug, thiserror::Error)]
#[error("response exceeded configured limit of {limit} bytes for {url}")]
pub struct ResponseTooLarge {
    pub limit: usize,
    pub url: Url,
}

/// 克隆共享 Moli 连接、搜索 Cookie 和构造期出站快照。
#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<HttpClientInner>,
}

struct HttpClientInner {
    transport: Transport,
    snapshot: ResolvedProxy,
    timeout: Duration,
    response_limit: Option<usize>,
    cookies: Option<Mutex<BrowserCookieStore>>,
    pin: Option<(String, Vec<SocketAddr>)>,
}

impl HttpClient {
    pub(crate) fn new(
        snapshot: ResolvedProxy,
        timeout: Duration,
        cookies: bool,
        response_limit: Option<usize>,
    ) -> Result<Self> {
        Self::build(snapshot, timeout, cookies, response_limit, None)
    }

    /// 固定策略层校验通过的地址，连接时不再解析源站。
    pub(crate) fn pinned(
        hostname: &str,
        addresses: Vec<SocketAddr>,
        timeout: Duration,
        response_limit: usize,
    ) -> Result<Self> {
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|address| !crate::address_policy::is_public_ip(address.ip()))
        {
            bail!("direct HTTP requires public pinned addresses");
        }
        Self::build(
            ResolvedProxy::direct(),
            timeout,
            false,
            Some(response_limit),
            Some((hostname.to_owned(), addresses)),
        )
    }

    fn build(
        snapshot: ResolvedProxy,
        timeout: Duration,
        cookies: bool,
        response_limit: Option<usize>,
        pin: Option<(String, Vec<SocketAddr>)>,
    ) -> Result<Self> {
        let transport = Transport::new(TransportConfig {
            fingerprint: TransportFingerprint::chrome(),
            ..TransportConfig::default()
        })?;
        Ok(Self {
            inner: Arc::new(HttpClientInner {
                transport,
                snapshot,
                timeout,
                response_limit,
                pin,
                cookies: cookies.then(|| Mutex::new(BrowserCookieStore::default())),
            }),
        })
    }

    pub async fn fetch(&self, request: Request) -> Result<(Response, Vec<u8>)> {
        self.fetch_with_redirects(request, true).await
    }

    /// 只执行一跳，让 Fetch 逐跳校验重定向并固定 DNS 结果。
    pub async fn fetch_once(&self, request: Request) -> Result<(Response, Vec<u8>)> {
        self.fetch_with_redirects(request, false).await
    }

    async fn fetch_with_redirects(
        &self,
        request: Request,
        follow: bool,
    ) -> Result<(Response, Vec<u8>)> {
        // 丢弃 future 或响应正文会取消 Moli 的在途传输。
        tokio::time::timeout(self.inner.timeout, self.fetch_inner(request, follow))
            .await
            .context("HTTP request timed out")?
    }

    async fn fetch_inner(&self, mut request: Request, follow: bool) -> Result<(Response, Vec<u8>)> {
        request.extensions_mut().clear();
        for count in 0..=MAX_REDIRECTS {
            let from = Url::parse(&request.uri().to_string())?;
            if !matches!(from.scheme(), "http" | "https")
                || !from.username().is_empty()
                || from.password().is_some()
            {
                bail!("HTTP transport requires an HTTP(S) URL without credentials");
            }
            let mut outgoing = TransportRequest::new(from.clone(), request.method().as_str());
            // 显式空值阻止 Moli 回读可变的进程代理环境。
            let proxy = if self.inner.snapshot.pins_origin(&from) {
                None
            } else if from.scheme() == "https" {
                self.inner.snapshot.https.as_ref()
            } else {
                self.inner.snapshot.http.as_ref()
            };
            outgoing.connection.proxy = Some(proxy.map_or_else(String::new, ToString::to_string));
            outgoing.connection.no_proxy = Some(String::new());
            outgoing.connection.connect_timeout = Some(self.inner.timeout);
            if let Some((hostname, addresses)) = &self.inner.pin {
                if from.host_str() != Some(hostname.as_str())
                    || addresses
                        .iter()
                        .any(|address| Some(address.port()) != from.port_or_known_default())
                {
                    bail!("pinned HTTP client cannot request a different origin");
                }
                outgoing.connection.resolved_addresses = Some(addresses.clone());
            }
            for (name, value) in request.headers() {
                if name == header::COOKIE && self.inner.cookies.is_none() {
                    continue;
                }
                outgoing
                    .headers
                    .push((name.as_str().to_owned(), value.to_str()?.to_owned()));
            }
            if !request.headers().contains_key(header::USER_AGENT) {
                outgoing.headers.push(("user-agent".into(),
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36".into()));
            }
            if !request.headers().contains_key(header::ACCEPT_ENCODING) {
                outgoing
                    .headers
                    .push(("accept-encoding".into(), "gzip, deflate, br, zstd".into()));
            }
            if let Some(jar) = &self.inner.cookies {
                if !request.headers().contains_key(header::COOKIE) {
                    let report = jar.lock().cookie_access_report_for_request(
                        &from,
                        NetworkCookieRequestContext::top_level_navigation(
                            request.method().as_str(),
                        ),
                    );
                    let mut value = String::new();
                    for entry in report.included_cookies {
                        if !value.is_empty() {
                            value.push_str("; ");
                        }
                        value.push_str(&entry.cookie.name);
                        value.push('=');
                        value.push_str(&entry.cookie.value);
                    }
                    if !value.is_empty() {
                        outgoing.headers.push(("cookie".into(), value));
                    }
                }
            }
            if !request.body().is_empty() {
                outgoing.body = Some(if follow {
                    request.body().clone()
                } else {
                    std::mem::take(request.body_mut())
                });
            }
            let response = self.inner.transport.execute(outgoing).await?;
            if let Some(jar) = &self.inner.cookies {
                jar.lock().store_response_headers_with_context_reports(
                    &from,
                    &response.headers,
                    &NetworkCookieRequestContext::top_level_navigation(request.method().as_str()),
                );
            }
            let mut metadata = http::Response::builder()
                .status(response.status)
                .version(response.version)
                .body(from.clone())?;
            for (name, value) in &response.headers {
                metadata.headers_mut().append(
                    http::header::HeaderName::from_bytes(name.as_bytes())?,
                    value.parse()?,
                );
            }
            let target = if follow && matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                metadata
                    .headers()
                    .get(header::LOCATION)
                    .map(|value| -> Result<Url> { Ok(from.join(value.to_str()?.trim())?) })
                    .transpose()?
            } else {
                None
            };
            if let Some(to) = target {
                if count == MAX_REDIRECTS {
                    bail!("redirect limit exceeded for {from}");
                }
                if (matches!(response.status, 301 | 302) && request.method() == Method::POST)
                    || (response.status == 303
                        && request.method() != Method::GET
                        && request.method() != Method::HEAD)
                {
                    *request.method_mut() = Method::GET;
                    request.body_mut().clear();
                    for name in [
                        header::CONTENT_LENGTH,
                        header::CONTENT_TYPE,
                        header::CONTENT_ENCODING,
                        header::CONTENT_LANGUAGE,
                        header::CONTENT_LOCATION,
                        header::TRANSFER_ENCODING,
                    ] {
                        request.headers_mut().remove(name);
                    }
                }
                if from.origin() != to.origin() {
                    for name in [
                        header::AUTHORIZATION,
                        header::COOKIE,
                        header::PROXY_AUTHORIZATION,
                        header::HOST,
                    ] {
                        request.headers_mut().remove(name);
                    }
                }
                if from.scheme() == "https" && to.scheme() == "http" {
                    request.headers_mut().remove(header::REFERER);
                }
                *request.uri_mut() = to.as_str().parse()?;
                // Moli 丢弃未读完的 H1 连接；H2 则只重置当前 stream。
                drop(response);
                continue;
            }
            if request.method() == Method::HEAD || matches!(response.status, 204 | 304) {
                return Ok((metadata, Vec::new()));
            }
            let encodings = metadata
                .headers()
                .get_all(header::CONTENT_ENCODING)
                .iter()
                .map(|value| value.to_str())
                .collect::<std::result::Result<Vec<_>, _>>()?
                .join(",");
            let compressed = encodings
                .split(',')
                .any(|value| !matches!(value.trim(), "" | "identity"));
            if !compressed {
                if let Some(limit) = self.inner.response_limit {
                    if metadata
                        .headers()
                        .get(header::CONTENT_LENGTH)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some_and(|length| length > limit as u64)
                    {
                        return Err(ResponseTooLarge { limit, url: from }.into());
                    }
                }
            }
            let stream = futures::stream::try_unfold(response.body, |mut body| async move {
                body.chunk()
                    .await
                    .map(|chunk| chunk.map(|chunk| (chunk, body)))
                    .map_err(io::Error::other)
            });
            let mut reader: Pin<Box<dyn AsyncRead + Send>> =
                Box::pin(StreamReader::new(Box::pin(stream)));
            for encoding in encodings.split(',').rev().map(str::trim) {
                reader = match encoding {
                    "" | "identity" => reader,
                    "gzip" | "x-gzip" => {
                        let mut decoder = GzipDecoder::new(BufReader::new(reader));
                        decoder.multiple_members(true);
                        Box::pin(decoder)
                    }
                    "deflate" => Box::pin(ZlibDecoder::new(BufReader::new(reader))),
                    "br" => Box::pin(BrotliDecoder::new(BufReader::new(reader))),
                    "zstd" => Box::pin(ZstdDecoder::new(BufReader::new(reader))),
                    _ => bail!("unsupported HTTP content encoding: {encoding}"),
                };
            }
            if compressed {
                metadata.headers_mut().remove(header::CONTENT_LENGTH);
                metadata.headers_mut().remove(header::CONTENT_ENCODING);
            }
            let mut body = Vec::new();
            let mut buffer = [0; 16 * 1024];
            loop {
                let capacity = self.inner.response_limit.map_or(buffer.len(), |limit| {
                    limit
                        .saturating_sub(body.len())
                        .saturating_add(1)
                        .min(buffer.len())
                });
                let count = reader.read(&mut buffer[..capacity]).await?;
                if count == 0 {
                    break;
                }
                if let Some(limit) = self.inner.response_limit {
                    if body.len().saturating_add(count) > limit {
                        return Err(ResponseTooLarge { limit, url: from }.into());
                    }
                }
                body.extend_from_slice(&buffer[..count]);
            }
            return Ok((metadata, body));
        }
        unreachable!("bounded redirect loop always returns")
    }
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("cookies", &self.inner.cookies.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };

    use parking_lot::{Condvar, Mutex};
    use std::thread::{self, JoinHandle};

    use super::*;

    #[derive(Clone)]
    enum Reply {
        Fixed(&'static str),
        Gzip(&'static [u8]),
        Redirect(String),
        EchoCookie,
        EchoCredentials,
        Concurrent(Arc<(Mutex<usize>, Condvar)>),
    }

    struct TestServer {
        base_url: String,
        shutdown: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn spawn_server(replies: Vec<Reply>) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            let mut handlers = Vec::new();
            for reply in replies {
                let (stream, _) = listener.accept().unwrap();
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                handlers.push(thread::spawn(move || serve(stream, reply)));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        TestServer {
            base_url: format!("http://{address}"),
            shutdown,
            thread: Some(thread),
        }
    }

    fn serve(mut stream: TcpStream, reply: Reply) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let request = String::from_utf8_lossy(&request);
        let response = match reply {
            Reply::Fixed(response) => response.to_owned(),
            Reply::Gzip(body) => {
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                // The client may intentionally cancel when the decoded limit is reached.
                let _ = stream.write_all(body);
                return;
            }
            Reply::Redirect(location) => format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Reply::EchoCredentials => {
                let leaked = request.lines().any(|line| {
                    line.split_once(':').is_some_and(|(name, _)| {
                        name.eq_ignore_ascii_case("authorization")
                            || name.eq_ignore_ascii_case("cookie")
                    })
                });
                ok(if leaked { "leaked" } else { "clean" })
            }
            Reply::EchoCookie => {
                let cookie = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                    .and_then(|line| line.split_once(':'))
                    .map_or("none", |(_, value)| value.trim());
                ok(cookie)
            }
            Reply::Concurrent(gate) => {
                let (count, ready) = &*gate;
                let mut count = count.lock();
                *count += 1;
                if *count == 2 {
                    ready.notify_all();
                } else {
                    let timeout = ready.wait_for(&mut count, Duration::from_secs(2));
                    assert!(!timeout.timed_out(), "requests were serialized");
                }
                drop(count);
                ok("parallel")
            }
        };
        stream.write_all(response.as_bytes()).unwrap();
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn client(limit: Option<usize>, cookies: bool) -> HttpClient {
        HttpClient::new(
            ResolvedProxy::direct(),
            Duration::from_secs(5),
            cookies,
            limit,
        )
        .unwrap()
    }

    fn get(url: &str) -> Request {
        http::Request::get(url).body(Vec::new()).unwrap()
    }

    #[tokio::test]
    async fn cookies_persist_per_client_and_remain_isolated() {
        let server = spawn_server(vec![
            Reply::Fixed(
                "HTTP/1.1 200 OK\r\nSet-Cookie: sid=one; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
            Reply::EchoCookie,
            Reply::EchoCookie,
        ]);
        let first = client(None, true);
        let second = client(None, true);
        first
            .fetch(get(&format!("{}/set", server.base_url)))
            .await
            .unwrap();
        let persisted = first
            .fetch(get(&format!("{}/echo", server.base_url)))
            .await
            .unwrap();
        let isolated = second
            .fetch(get(&format!("{}/echo", server.base_url)))
            .await
            .unwrap();
        assert_eq!(persisted.1, b"sid=one");
        assert_eq!(isolated.1, b"none");
    }

    #[tokio::test]
    async fn disabled_cookies_ignore_response_and_explicit_headers() {
        let server = spawn_server(vec![
            Reply::Fixed(
                "HTTP/1.1 200 OK\r\nSet-Cookie: sid=one; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
            Reply::EchoCookie,
        ]);
        let client = client(None, false);
        client.fetch(get(&server.base_url)).await.unwrap();
        let mut request = get(&server.base_url);
        request
            .headers_mut()
            .insert(header::COOKIE, "explicit=secret".parse().unwrap());
        let response = client.fetch(request).await.unwrap();
        assert_eq!(response.1, b"none");
    }

    #[tokio::test]
    async fn redirect_policy_preserves_manual_and_follows_when_requested() {
        let server = spawn_server(vec![
            Reply::Fixed(
                "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
            Reply::Fixed(
                "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
            Reply::Fixed("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfinal"),
        ]);
        let client = client(None, true);
        let manual = client
            .fetch_once(get(&format!("{}/start", server.base_url)))
            .await
            .unwrap();
        assert_eq!(manual.0.status(), 302);

        let followed = client
            .fetch(get(&format!("{}/start", server.base_url)))
            .await
            .unwrap();
        assert_eq!(followed.0.status(), 200);
        assert_eq!(followed.1, b"final");
        assert!(followed.0.body().path().ends_with("/final"));
    }

    #[tokio::test]
    async fn redirects_do_not_forward_cross_origin_credentials() {
        let destination = spawn_server(vec![Reply::EchoCredentials]);
        let origin = spawn_server(vec![Reply::Redirect(destination.base_url.clone())]);
        let client = client(None, true);
        let mut request = get(&origin.base_url);
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, "Bearer fixture".parse().unwrap());
        request
            .headers_mut()
            .insert(header::COOKIE, "explicit=fixture".parse().unwrap());
        let response = client.fetch(request).await.unwrap();
        assert_eq!(response.1, b"clean");
        assert_eq!(
            response.0.body().to_string(),
            format!("{}/", destination.base_url)
        );
    }

    #[tokio::test]
    async fn response_limit_is_typed_and_cancels_the_transfer() {
        let server = spawn_server(vec![Reply::Fixed(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nlarge",
        )]);
        let client = client(Some(4), false);
        let error = client.fetch(get(&server.base_url)).await.unwrap_err();
        let error = error.downcast_ref::<ResponseTooLarge>().unwrap();
        assert_eq!(error.limit, 4);
    }

    #[tokio::test]
    async fn chunked_body_cannot_bypass_the_response_limit() {
        let server = spawn_server(vec![Reply::Fixed(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n",
        )]);
        let client = client(Some(4), false);
        let error = client.fetch(get(&server.base_url)).await.unwrap_err();
        assert!(error.is::<ResponseTooLarge>());
    }

    #[tokio::test]
    async fn compressed_content_length_does_not_reject_a_body_at_the_decoded_limit() {
        let server = spawn_server(vec![Reply::Gzip(&[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 10, 203, 72, 205, 201, 201, 7, 0, 134, 166, 16, 54, 5, 0,
            0, 0,
        ])]);
        let response = client(Some(5), false)
            .fetch(get(&server.base_url))
            .await
            .unwrap();
        assert_eq!(response.1, b"hello");
    }

    #[tokio::test]
    async fn compressed_body_cannot_bypass_the_decoded_response_limit() {
        let server = spawn_server(vec![Reply::Gzip(&[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 10, 171, 168, 160, 12, 0, 0, 18, 172, 210, 58, 64, 0, 0,
            0,
        ])]);
        let error = client(Some(32), false)
            .fetch(get(&server.base_url))
            .await
            .unwrap_err();
        assert!(error.is::<ResponseTooLarge>());
    }

    #[tokio::test]
    async fn total_timeout_cancels_a_stalled_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nx")
                .await
                .unwrap();
            stream.read(&mut buffer).await.unwrap()
        });
        let client = HttpClient::new(
            ResolvedProxy::direct(),
            Duration::from_millis(100),
            false,
            Some(5),
        )
        .unwrap();
        let error = client
            .fetch(get(&format!("http://{address}/")))
            .await
            .unwrap_err();
        assert!(error.is::<tokio::time::error::Elapsed>());
        let remaining = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            remaining, 0,
            "cancellation must close the unread H1 connection"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloned_client_runs_requests_concurrently() {
        let gate = Arc::new((Mutex::new(0), Condvar::new()));
        let server = spawn_server(vec![
            Reply::Concurrent(Arc::clone(&gate)),
            Reply::Concurrent(gate),
        ]);
        let client = client(None, false);
        let clone = client.clone();
        let first = client.fetch(get(&format!("{}/one", server.base_url)));
        let second = clone.fetch(get(&format!("{}/two", server.base_url)));
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().1, b"parallel");
        assert_eq!(second.unwrap().1, b"parallel");
        drop(clone);
        drop(client);
    }
}
