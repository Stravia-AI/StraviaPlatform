use std::{
    fmt,
    sync::{mpsc, Arc, LazyLock},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use moli_cookie_jar::{advance_cookie_request_context, new_shared_browser_cookie_store};
use moli_fetch::{
    FetchCancelHandle, FetchClient, FetchClientHandle, FetchConfig, NetworkResponseExtraInfo,
    RawResponse, RedirectInfo, Request, RequestCredentialsMode,
};
use moli_stealth_net::TransportFingerprint;
use parking_lot::Mutex;
use url::Url;

const MAX_REDIRECTS: usize = 10;

static TRANSPORT_INITIALIZATION: LazyLock<std::result::Result<(), String>> = LazyLock::new(|| {
    moli_stealth_net::initialize_process_fingerprint(TransportFingerprint::chrome())
        .map_err(|error| error.to_string())
});

/// 初始化进程级 Chrome 传输指纹；浏览器和纯 HTTP 客户端必须在创建首个传输前共用此入口。
pub(crate) fn initialize_transport() -> Result<()> {
    TRANSPORT_INITIALIZATION
        .clone()
        .map_err(anyhow::Error::msg)
        .context("failed to initialize Moli Chrome transport fingerprint")
}

#[derive(Debug, thiserror::Error)]
#[error("response exceeded configured limit of {limit} bytes for {url}")]
/// 响应声明长度或解压后的实际正文超过客户端配置的上限。
pub struct ResponseTooLarge {
    pub limit: usize,
    pub url: Url,
}

#[derive(Clone)]
/// 使用 Moli 原生请求模型的并发 HTTP 客户端；克隆共享 Cookie 与传输生命周期。
pub struct HttpClient {
    inner: Arc<HttpClientInner>,
}

struct HttpClientInner {
    http: FetchClientHandle,
    https: FetchClientHandle,
    http_response_limit: Option<usize>,
    https_response_limit: Option<usize>,
    http_timeout_ms: u64,
    https_timeout_ms: u64,
    native_redirects: bool,
    cookies: bool,
    shutdown: mpsc::Sender<()>,
    owner_thread: Mutex<Option<JoinHandle<()>>>,
}

impl HttpClient {
    /// 快照 HTTP/HTTPS 配置，并在专属线程创建 Moli owner。
    ///
    /// `cookies` 为 false 时禁止发送和保存请求 Cookie；配置中的响应上限在流式读取时执行。
    /// 指纹初始化或 owner 线程启动失败会返回错误。最后一个克隆释放时同步关闭并回收 owner。
    pub fn new(
        mut http_config: FetchConfig,
        mut https_config: FetchConfig,
        cookies: bool,
    ) -> Result<Self> {
        initialize_transport()?;

        let native_redirects = http_config == https_config;
        if !cookies {
            remove_default_cookie_header(&mut http_config);
            remove_default_cookie_header(&mut https_config);
        }
        let http_response_limit = http_config.http_max_response_size();
        let https_response_limit = https_config.http_max_response_size();
        let http_timeout_ms = http_config.request_timeout_ms();
        let https_timeout_ms = https_config.request_timeout_ms();
        // 上游的实体大小错误目前只有字符串；关闭其上限，由本层流式读取产生可 downcast 的错误。
        http_config.set_connection_limits(
            http_config.http_max_concurrent(),
            http_config.http_max_host_open(),
            None,
        );
        https_config.set_connection_limits(
            https_config.http_max_concurrent(),
            https_config.http_max_host_open(),
            None,
        );

        let reuse_owner = http_config == https_config;
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let (initialized_tx, initialized_rx) = mpsc::sync_channel(1);
        let owner_thread = thread::Builder::new()
            .name("stravia-moli-http-owner".to_owned())
            .spawn(move || {
                let cookie_store = new_shared_browser_cookie_store();
                let http_owner = FetchClient::new(&http_config, Arc::clone(&cookie_store));
                let http = http_owner.handle();
                let https_owner = (!reuse_owner)
                    .then(|| FetchClient::new(&https_config, Arc::clone(&cookie_store)));
                let https = https_owner
                    .as_ref()
                    .map_or_else(|| http.clone(), FetchClient::handle);
                if initialized_tx.send((http, https)).is_err() {
                    return;
                }
                let _ = shutdown_rx.recv();
                drop(https_owner);
                drop(http_owner);
            })
            .context("failed to spawn Moli HTTP owner thread")?;

        let (http, https) = match initialized_rx.recv() {
            Ok(handles) => handles,
            Err(_) => {
                let panicked = owner_thread.join().is_err();
                bail!(if panicked {
                    "Moli HTTP owner thread panicked during initialization"
                } else {
                    "Moli HTTP owner thread exited during initialization"
                });
            }
        };

        Ok(Self {
            inner: Arc::new(HttpClientInner {
                http,
                https,
                http_response_limit,
                https_response_limit,
                http_timeout_ms,
                https_timeout_ms,
                native_redirects,
                cookies,
                shutdown: shutdown_tx,
                owner_thread: Mutex::new(Some(owner_thread)),
            }),
        })
    }

    /// 在 Tokio runtime 中执行请求，返回含 HTTP 错误状态的原始响应，不自动将 4xx/5xx 转成错误。
    ///
    /// 构造期超时覆盖完整重定向链；原生 Request 的单请求 override 不能放宽此客户端总预算。
    /// URL、传输、重定向、超时和正文大小错误会显式返回；取消 future 会取消尚未完成的传输。
    pub async fn fetch(&self, request: Request) -> Result<RawResponse> {
        let timeout_ms = match request.url.scheme() {
            "http" => self.inner.http_timeout_ms,
            "https" => self.inner.https_timeout_ms,
            scheme => bail!("Moli HTTP client does not support URL scheme `{scheme}`"),
        };
        if timeout_ms == 0 {
            return self.fetch_inner(request).await;
        }
        tokio::time::timeout(Duration::from_millis(timeout_ms), self.fetch_inner(request))
            .await
            .map_err(|_| anyhow::Error::new(moli_stealth_net::TransportError::Timeout))?
    }

    async fn fetch_inner(&self, mut request: Request) -> Result<RawResponse> {
        if !self.inner.cookies {
            request.credentials_mode = RequestCredentialsMode::Omit;
            request
                .request_headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
        }
        if !request.follow_redirects || self.inner.native_redirects {
            return self.fetch_once(request).await;
        }

        let initial_url = request.url.clone();
        let mut redirects = Vec::new();
        request.follow_redirects = false;

        for redirect_count in 0..=MAX_REDIRECTS {
            let response = self.fetch_once(request.clone()).await?;
            let Some(next_url) = redirect_target(&response)? else {
                return finish_redirect_chain(response, redirects);
            };
            if redirect_count == MAX_REDIRECTS {
                bail!("redirect limit exceeded for {}", response.final_url);
            }

            let from_url = response.final_url.clone();
            let status = response.status;
            let next_request_extra_info = response.network_request_extra_info().cloned();
            if let Some(last) = redirects.last_mut() {
                last.request_extra_info = next_request_extra_info.clone();
            }
            redirects.push(redirect_info(&response, next_url.clone()));

            request.cookie_context = advance_cookie_request_context(
                request.cookie_context.clone(),
                &initial_url,
                &next_url,
            );
            request.apply_redirect_status(status);
            sanitize_redirect_request(&mut request, &from_url, &next_url, self.inner.cookies);
            request.url = next_url;
        }

        unreachable!("bounded redirect loop always returns")
    }

    async fn fetch_once(&self, request: Request) -> Result<RawResponse> {
        let (handle, response_limit) = match request.url.scheme() {
            "http" => (&self.inner.http, self.inner.http_response_limit),
            "https" => (&self.inner.https, self.inner.https_response_limit),
            scheme => bail!("Moli HTTP client does not support URL scheme `{scheme}`"),
        };
        let cancel = FetchCancelHandle::new();
        let mut cancel_on_drop = CancelOnDrop(Some(cancel.clone()));
        let mut response = handle.fetch_raw_stream_with_cancel(request, cancel).await?;

        if let Some(limit) = response_limit {
            if declared_content_length(&response.headers).is_some_and(|length| length > limit) {
                return Err(ResponseTooLarge {
                    limit,
                    url: response.final_url.clone(),
                }
                .into());
            }
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.next_chunk().await {
            if let Some(limit) = response_limit {
                if body.len().saturating_add(chunk.len()) > limit {
                    return Err(ResponseTooLarge {
                        limit,
                        url: response.final_url.clone(),
                    }
                    .into());
                }
            }
            body.extend_from_slice(&chunk);
        }
        response.finish().await?;
        cancel_on_drop.0 = None;

        let extra_info = response.network_request_extra_info().cloned();
        let head = response.head();
        Ok(RawResponse::from_head_and_body(head, body).with_network_request_extra_info(extra_info))
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

impl Drop for HttpClientInner {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.owner_thread.get_mut().take() {
            let _ = thread.join();
        }
    }
}

struct CancelOnDrop(Option<FetchCancelHandle>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel.cancel();
        }
    }
}

fn redirect_target(response: &RawResponse) -> Result<Option<Url>> {
    if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.trim())
    else {
        return Ok(None);
    };
    let next = response
        .final_url
        .join(location)
        .or_else(|_| Url::parse(location))
        .with_context(|| {
            format!(
                "failed to resolve redirect location `{location}` from {}",
                response.final_url
            )
        })?;
    if !matches!(next.scheme(), "http" | "https") {
        bail!("network transport only supports HTTP(S) URLs: {next}");
    }
    Ok(Some(next))
}

fn sanitize_redirect_request(request: &mut Request, from: &Url, to: &Url, cookies: bool) {
    let cross_origin = !same_origin(from, to);
    let https_downgrade = from.scheme() == "https" && to.scheme() == "http";
    request.request_headers.retain(|(name, _)| {
        if !cookies && name.eq_ignore_ascii_case("cookie") {
            return false;
        }
        if cross_origin
            && ["authorization", "cookie", "proxy-authorization"]
                .iter()
                .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
        {
            return false;
        }
        !(https_downgrade && name.eq_ignore_ascii_case("referer"))
    });
    if cross_origin {
        request.set_auth(None);
    }
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn remove_default_cookie_header(config: &mut FetchConfig) {
    let headers = config
        .default_request_headers()
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("cookie"))
        .cloned()
        .collect();
    config.set_default_request_headers(headers);
}

fn declared_content_length(headers: &[(String, String)]) -> Option<usize> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

fn redirect_info(response: &RawResponse, to_url: Url) -> RedirectInfo {
    let request_extra_info = response.network_request_extra_info().cloned();
    let has_extra = request_extra_info.is_some() && !response.from_cache;
    RedirectInfo {
        from_url: response.final_url.clone(),
        to_url,
        status: response.status,
        headers: response.headers.clone(),
        network_extra_info_available: has_extra,
        request_extra_info: None,
        response_extra_info: request_extra_info.map(|request_extra_info| {
            NetworkResponseExtraInfo {
                request_extra_info,
                status: response.status,
                headers: response.headers.clone(),
                cookie_set_reports: response.cookie_set_reports.clone(),
            }
        }),
        redirect_has_extra_info: has_extra,
        request_cookie_report: response.request_cookie_report.clone(),
        cookie_set_reports: response.cookie_set_reports.clone(),
        from_cache: response.from_cache,
        negotiated_http_version: response.negotiated_http_version,
    }
}

fn finish_redirect_chain(
    response: RawResponse,
    mut redirects: Vec<RedirectInfo>,
) -> Result<RawResponse> {
    let extra_info = response.network_request_extra_info().cloned();
    if let Some(last) = redirects.last_mut() {
        last.request_extra_info = extra_info.clone();
    }
    let (mut head, body) = response.into_parts();
    head.redirected = !redirects.is_empty();
    head.redirect_chain = redirects;
    RawResponse::from_head_and_materialized_body(head, body)
        .map(|response| response.with_network_request_extra_info(extra_info))
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

    fn config(limit: Option<usize>) -> FetchConfig {
        let mut config = FetchConfig::default();
        config.set_http_proxy(Some(String::new()));
        config.set_connection_limits(None, None, limit);
        config
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
        let first = HttpClient::new(config(None), config(None), true).unwrap();
        let second = HttpClient::new(config(None), config(None), true).unwrap();
        first
            .fetch(Request::get(&format!("{}/set", server.base_url)).unwrap())
            .await
            .unwrap();
        let persisted = first
            .fetch(Request::get(&format!("{}/echo", server.base_url)).unwrap())
            .await
            .unwrap();
        let isolated = second
            .fetch(Request::get(&format!("{}/echo", server.base_url)).unwrap())
            .await
            .unwrap();
        assert_eq!(persisted.body_bytes(), b"sid=one");
        assert_eq!(isolated.body_bytes(), b"none");
    }

    #[tokio::test]
    async fn disabled_cookies_ignore_response_request_and_default_headers() {
        let server = spawn_server(vec![
            Reply::Fixed(
                "HTTP/1.1 200 OK\r\nSet-Cookie: sid=one; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
            Reply::EchoCookie,
        ]);
        let mut settings = config(None);
        settings.push_default_request_header("Cookie", "default=secret");
        let client = HttpClient::new(settings.clone(), settings, false).unwrap();
        client
            .fetch(Request::get(&server.base_url).unwrap())
            .await
            .unwrap();
        let mut request = Request::get(&server.base_url).unwrap();
        request
            .request_headers
            .push(("Cookie".into(), "explicit=secret".into()));
        let response = client.fetch(request).await.unwrap();
        assert_eq!(response.body_bytes(), b"none");
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
        let client = HttpClient::new(config(None), config(None), true).unwrap();
        let manual = client
            .fetch(
                Request::get(&format!("{}/start", server.base_url))
                    .unwrap()
                    .with_follow_redirects(false),
            )
            .await
            .unwrap();
        assert_eq!(manual.status, 302);
        assert!(!manual.redirected);

        let followed = client
            .fetch(Request::get(&format!("{}/start", server.base_url)).unwrap())
            .await
            .unwrap();
        assert_eq!(followed.status, 200);
        assert_eq!(followed.body_bytes(), b"final");
        assert_eq!(followed.redirect_chain.len(), 1);
    }

    #[tokio::test]
    async fn split_configuration_redirects_do_not_forward_cross_origin_credentials() {
        let destination = spawn_server(vec![Reply::EchoCredentials]);
        let origin = spawn_server(vec![Reply::Redirect(destination.base_url.clone())]);
        let http = config(None);
        let mut https = http.clone();
        https.set_request_timeout_ms(http.request_timeout_ms() + 1);
        let client = HttpClient::new(http, https, true).unwrap();
        let mut request = Request::get(&origin.base_url).unwrap();
        request.request_headers = vec![
            ("Authorization".into(), "Bearer fixture".into()),
            ("Cookie".into(), "explicit=fixture".into()),
        ];
        let response = client.fetch(request).await.unwrap();
        assert_eq!(response.body_bytes(), b"clean");
        assert_eq!(
            response.final_url.as_str(),
            format!("{}/", destination.base_url)
        );
        assert_eq!(response.redirect_chain.len(), 1);
    }

    #[tokio::test]
    async fn response_limit_is_typed_and_cancels_the_transfer() {
        let server = spawn_server(vec![Reply::Fixed(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nlarge",
        )]);
        let client = HttpClient::new(config(Some(4)), config(Some(4)), false).unwrap();
        let error = client
            .fetch(Request::get(&server.base_url).unwrap())
            .await
            .unwrap_err();
        let error = error.downcast_ref::<ResponseTooLarge>().unwrap();
        assert_eq!(error.limit, 4);
    }

    #[tokio::test]
    async fn chunked_body_cannot_bypass_the_response_limit() {
        let server = spawn_server(vec![Reply::Fixed(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n",
        )]);
        let client = HttpClient::new(config(Some(4)), config(Some(4)), false).unwrap();
        let error = client
            .fetch(Request::get(&server.base_url).unwrap())
            .await
            .unwrap_err();
        assert!(error.is::<ResponseTooLarge>());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloned_client_runs_requests_concurrently() {
        let gate = Arc::new((Mutex::new(0), Condvar::new()));
        let server = spawn_server(vec![
            Reply::Concurrent(Arc::clone(&gate)),
            Reply::Concurrent(gate),
        ]);
        let client = HttpClient::new(config(None), config(None), false).unwrap();
        let clone = client.clone();
        let first = client.fetch(Request::get(&format!("{}/one", server.base_url)).unwrap());
        let second = clone.fetch(Request::get(&format!("{}/two", server.base_url)).unwrap());
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().body_bytes(), b"parallel");
        assert_eq!(second.unwrap().body_bytes(), b"parallel");
        drop(clone);
        drop(client);
    }
}
