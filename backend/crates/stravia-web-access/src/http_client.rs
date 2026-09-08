use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use http_body_util::BodyExt;
use url::Url;
use wreq::{header, Client, Method, Request, Response};

use crate::outbound::ResolvedProxy;

const MAX_REDIRECTS: usize = 10;

#[derive(Debug, thiserror::Error)]
#[error("response exceeded configured limit of {limit} bytes for {url}")]
/// 响应声明长度或解压后的实际正文超过客户端配置的上限。
pub struct ResponseTooLarge {
    pub limit: usize,
    pub url: Url,
}

#[derive(Clone)]
/// 原生 wreq 请求传输；克隆共享连接池、搜索 Cookie 和构造期出站快照。
pub struct HttpClient {
    inner: Arc<HttpClientInner>,
}

struct HttpClientInner {
    direct: Client,
    http: Client,
    https: Client,
    snapshot: ResolvedProxy,
    timeout: Duration,
    response_limit: Option<usize>,
    cookies: bool,
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

    /// 固定策略层已验证的全部地址，不允许连接时再次解析目标域名。
    pub(crate) fn pinned(
        hostname: &str,
        addresses: Vec<SocketAddr>,
        timeout: Duration,
        response_limit: usize,
    ) -> Result<Self> {
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|address| !crate::fetch::policy::is_public_ip(address.ip()))
        {
            bail!("direct HTTP requires public pinned addresses");
        }
        Self::build(
            ResolvedProxy::direct(),
            timeout,
            false,
            Some(response_limit),
            Some((hostname, addresses)),
        )
    }

    fn build(
        snapshot: ResolvedProxy,
        timeout: Duration,
        cookies: bool,
        response_limit: Option<usize>,
        pin: Option<(&str, Vec<SocketAddr>)>,
    ) -> Result<Self> {
        let jar = Arc::new(wreq::cookie::Jar::default());
        let make_client = |proxy: Option<&Url>| -> Result<Client> {
            let mut builder = Client::builder()
                .emulation(wreq_util::Profile::Chrome149)
                .no_proxy()
                .retry(wreq::retry::Policy::never())
                .redirect(wreq::redirect::Policy::none())
                .timeout(timeout);
            if cookies {
                builder = builder.cookie_provider(Arc::clone(&jar));
            }
            if let Some(proxy) = proxy {
                if !proxy.username().is_empty() || proxy.password().is_some() {
                    bail!("proxy URL must not include credentials");
                }
                builder = builder.proxy(wreq::Proxy::all(proxy.as_str())?);
            }
            if let Some((hostname, addresses)) = &pin {
                builder = builder.resolve_to_addrs((*hostname).to_owned(), addresses.clone());
            }
            Ok(builder.build()?)
        };
        let direct = make_client(None)?;
        let http = snapshot
            .http
            .as_ref()
            .map_or_else(|| Ok(direct.clone()), |proxy| make_client(Some(proxy)))?;
        let https = snapshot
            .https
            .as_ref()
            .map_or_else(|| Ok(direct.clone()), |proxy| make_client(Some(proxy)))?;
        Ok(Self {
            inner: Arc::new(HttpClientInner {
                direct,
                http,
                https,
                snapshot,
                timeout,
                response_limit,
                cookies,
            }),
        })
    }

    /// 总预算覆盖重定向、响应头和解压后正文；取消 future 会直接丢弃在途传输。
    /// 原生响应保留状态、最终 URI 和响应头，已读尽的正文由元组第二项唯一持有。
    pub async fn fetch(&self, request: Request) -> Result<(Response, Vec<u8>)> {
        self.fetch_with_redirects(request, true).await
    }

    /// 只执行当前跳，供 Fetch 策略逐跳检查目标和 Google 结果跳转使用。
    pub async fn fetch_once(&self, request: Request) -> Result<(Response, Vec<u8>)> {
        self.fetch_with_redirects(request, false).await
    }

    async fn fetch_with_redirects(
        &self,
        request: Request,
        follow: bool,
    ) -> Result<(Response, Vec<u8>)> {
        tokio::time::timeout(self.inner.timeout, self.fetch_inner(request, follow))
            .await
            .context("HTTP request timed out")?
    }

    async fn fetch_inner(&self, mut request: Request, follow: bool) -> Result<(Response, Vec<u8>)> {
        // 请求只携带 HTTP 语义，不能覆盖出站代理、Cookie、解压或超时策略。
        request.extensions_mut().clear();
        for count in 0..=MAX_REDIRECTS {
            let from = Url::parse(&request.uri().to_string())?;
            if !matches!(from.scheme(), "http" | "https")
                || !from.username().is_empty()
                || from.password().is_some()
            {
                bail!("HTTP transport requires an HTTP(S) URL without credentials");
            }
            if !self.inner.cookies {
                request.headers_mut().remove(header::COOKIE);
            }
            let next_request = if follow { request.try_clone() } else { None };
            let client = if self.inner.snapshot.pins_origin(&from) {
                &self.inner.direct
            } else if from.scheme() == "https" {
                &self.inner.https
            } else {
                &self.inner.http
            };
            // 禁止调用方覆盖逐跳重定向策略；每跳必须重新选择对应协议的代理。
            let outgoing = wreq::RequestBuilder::from_parts(client.clone(), request)
                .redirect(wreq::redirect::Policy::none())
                .build()?;
            let mut response = client.execute(outgoing).await?;
            let target =
                if follow && matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                    response
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
                request = next_request.context("redirect requires a replayable request body")?;
                let status = response.status().as_u16();
                if (matches!(status, 301 | 302) && request.method() == Method::POST)
                    || (status == 303
                        && request.method() != Method::GET
                        && request.method() != Method::HEAD)
                {
                    *request.method_mut() = Method::GET;
                    *request.body_mut() = None;
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
                // 未读取的重定向正文不得占用连接池或后台继续下载。
                response.forbid_recycle();
                continue;
            }
            let mut body = Vec::new();
            if let Some(limit) = self.inner.response_limit {
                if response
                    .content_length()
                    .is_some_and(|length| length > limit as u64)
                {
                    response.forbid_recycle();
                    return Err(ResponseTooLarge { limit, url: from }.into());
                }
            }
            while let Some(frame) = response.frame().await {
                if let Ok(data) = frame?.into_data() {
                    if let Some(limit) = self.inner.response_limit {
                        if body.len().saturating_add(data.len()) > limit {
                            response.forbid_recycle();
                            return Err(ResponseTooLarge { limit, url: from }.into());
                        }
                    }
                    body.extend_from_slice(&data);
                }
            }
            return Ok((response, body));
        }
        unreachable!("bounded redirect loop always returns")
    }
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("cookies", &self.inner.cookies)
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
            Reply::Redirect(location) => format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Reply::EchoCredentials => {
                let leaked = request.lines().any(|line| {
                    line.split_once(':').is_some_and(|(name, _)| {
                        name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("cookie")
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
        Request::new(Method::GET, url.parse().unwrap())
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
            Reply::Fixed(
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfinal",
            ),
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
        assert!(followed.0.uri().path().ends_with("/final"));
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
            response.0.uri().to_string(),
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
