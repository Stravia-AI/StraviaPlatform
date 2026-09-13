use anyhow::Context;
use moli_core::runtime::{Browser, BrowserConfig, RenderedDomWaitUntil};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Mutex};

mod egress;

#[derive(Debug, Clone)]
pub(crate) struct BrowserLaunchConfig {
    pub proxy: crate::outbound::ResolvedProxy,
    pub profile_dir: Option<PathBuf>,
}

async fn profile_gate(path: &std::path::Path) -> Arc<Mutex<()>> {
    static GATES: std::sync::LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
        std::sync::LazyLock::new(Default::default);
    let mut gates = GATES.lock().await;
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(path).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(path.to_owned(), Arc::downgrade(&gate));
    gate
}

#[derive(Clone)]
pub(crate) struct BrowserRuntime {
    inner: Arc<BrowserRuntimeInner>,
}

struct BrowserRuntimeInner {
    config: BrowserLaunchConfig,
    owner: Mutex<Option<mpsc::Sender<RenderCommand>>>,
}

struct RenderCommand {
    url: String,
    preflight_url: Option<String>,
    ready_selector: String,
    failure_expression: Option<&'static str>,
    deadline: tokio::time::Instant,
    response: oneshot::Sender<anyhow::Result<RenderedPage>>,
}

impl BrowserRuntime {
    pub(crate) fn new(config: BrowserLaunchConfig) -> Self {
        Self {
            inner: Arc::new(BrowserRuntimeInner {
                config,
                owner: Mutex::new(None),
            }),
        }
    }

    async fn owner(&self) -> anyhow::Result<mpsc::Sender<RenderCommand>> {
        let mut owner = self.inner.owner.lock().await;
        if let Some(sender) = owner.as_ref().filter(|sender| !sender.is_closed()) {
            return Ok(sender.clone());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (sender, receiver) = mpsc::channel(16);
        let config = self.inner.config.clone();
        // Browser 的 Rc 所有者具有线程亲和性，创建、使用和销毁必须在同一线程。
        std::thread::Builder::new()
            .name("stravia-moli".into())
            .spawn(move || {
                let local = tokio::task::LocalSet::new();
                runtime.block_on(local.run_until(serve(config, receiver)));
            })?;
        *owner = Some(sender.clone());
        Ok(sender)
    }

    pub(crate) async fn render(&self, request: RenderRequest<'_>) -> anyhow::Result<RenderedPage> {
        validate_navigation(request.url)?;
        if let Some(url) = request.preflight_url {
            validate_navigation(url)?;
        }
        let deadline = tokio::time::Instant::now() + request.timeout;
        tokio::time::timeout_at(deadline, async {
            if let Some(guard) = request.request_guard {
                let url = request.url.to_owned();
                let preflight = request.preflight_url.map(str::to_owned);
                let allowed = tokio::task::spawn_blocking(move || {
                    guard(&url) && preflight.as_deref().is_none_or(guard)
                })
                .await?;
                anyhow::ensure!(allowed, "Moli renderer rejected a non-public URL");
            }
            let (response, result) = oneshot::channel();
            self.owner()
                .await?
                .send(RenderCommand {
                    url: request.url.to_owned(),
                    preflight_url: request.preflight_url.map(str::to_owned),
                    ready_selector: request.ready_selector.to_owned(),
                    failure_expression: request.failure_expression,
                    deadline,
                    response,
                })
                .await
                .context("Moli owner stopped")?;
            // 丢弃接收端也会取消所有者线程上的在途操作。
            result.await.context("Moli owner stopped")?
        })
        .await
        .context("Moli rendering timed out")?
    }
}

async fn serve(config: BrowserLaunchConfig, mut commands: mpsc::Receiver<RenderCommand>) {
    let mut state = None;
    while let Some(mut command) = commands.recv().await {
        if command.response.is_closed() {
            continue;
        }
        let mut profile_guard = None;
        let operation = async {
            if let Some(path) = config.profile_dir.as_ref() {
                std::fs::create_dir_all(path).context("creating Moli profile directory")?;
                let path =
                    std::fs::canonicalize(path).context("resolving Moli profile directory")?;
                profile_guard = Some(profile_gate(&path).await.lock_owned().await);
            }
            if state.is_none() {
                // 内嵌 API 不执行 Moli CLI 的进程级初始化，必须显式选择同一传输指纹。
                moli_stealth_net::initialize_process_fingerprint(
                    moli_stealth_net::TransportFingerprint::chrome(),
                )?;
                let proxy = egress::EgressProxy::start(config.proxy.clone()).await?;
                let profile_dir = config.profile_dir.clone();
                let mut config = BrowserConfig::default();
                config.set_profile_dir(profile_dir);
                config.set_subframe_loading_enabled(true);
                config
                    .set_optional_resource_fetch_mask(moli_core::OptionalResourceFetchMask::all());
                let fetch = config.fetch_mut();
                fetch.set_http_proxy(Some(format!("http://{}", proxy.address())));
                // 空字符串禁用所有绕过项，包括继承的 NO_PROXY 环境变量。
                fetch.set_http_no_proxy(Some(String::new()));
                fetch.set_network_blocking(true, Vec::new());
                fetch.set_obey_robots(false);
                state = Some((Browser::new(config)?, proxy));
            }
            let (browser, _) = state.as_ref().expect("initialized Moli browser");
            render_page(
                browser,
                &command.url,
                command.preflight_url.as_deref(),
                &command.ready_selector,
                command.failure_expression,
                command.deadline,
            )
            .await
        };
        let result = tokio::select! {
            biased;
            _ = command.response.closed() => Err(anyhow::anyhow!("Moli rendering cancelled")),
            result = tokio::time::timeout_at(command.deadline, operation) => {
                result.context("Moli rendering timed out").and_then(|result| result)
            }
        };
        // 持久分区只有一个写入者；取消也必须先结束生产者并 flush，再交给下一请求。
        // 不把 profile 锁绑到 adapter 生命周期，旧代理快照仍可与新快照交替使用。
        if config.profile_dir.is_some() {
            if let Some((browser, proxy)) = state.take() {
                drop(browser);
                proxy.shutdown().await;
            }
        }
        drop(profile_guard);
        let _ = command.response.send(result);
    }
    // 先停止渲染生产者、回收资源所有者并刷新私有存储分区，再关闭出口监听。
    if let Some((browser, proxy)) = state {
        drop(browser);
        proxy.shutdown().await;
    }
}

async fn render_page(
    browser: &Browser,
    url: &str,
    preflight: Option<&str>,
    selector: &str,
    failure_expression: Option<&str>,
    deadline: tokio::time::Instant,
) -> anyhow::Result<RenderedPage> {
    let preflight = if preflight.is_some() {
        let target = url::Url::parse(url)?;
        let mut cookies = moli_cookie_jar::BrowserCookieStore::default();
        for cookie in browser.cookies()? {
            if !cookie.is_expired() && cookie.matches(&target) {
                cookies.upsert_with_request_url_report(
                    cookie,
                    None,
                    moli_cookie_jar::CookieSource::Cdp,
                );
            }
        }
        // 查询网络 Cookie 而非 document.cookie：HttpOnly 有效，分区与 SameSite 仍由上游判定。
        let context = moli_cookie_jar::NetworkCookieRequestContext::top_level_navigation("GET")
            .with_initiator_url(&target, &target);
        let has_cookie = !cookies
            .observe_cookie_access_report_for_request(&target, context)
            .included_cookies
            .is_empty();
        if has_cookie {
            None
        } else {
            preflight
        }
    } else {
        None
    };
    let mut page = browser
        .fetch_allow_http_error_with_wait_until(
            preflight.unwrap_or(url),
            RenderedDomWaitUntil::DomContentLoaded,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await?;
    let result = async {
        if preflight.is_some() {
            wait_for_document(&mut page, "body", failure_expression)
                .await
                .context("preflight navigation failed")?;
            let context = page.create_isolated_world_async("stravia", false).await?;
            let navigation = page
                .evaluate_runtime_expression_in_execution_context_with_await_async(
                    context,
                    &format!("location.assign({})", serde_json::to_string(url)?),
                    false,
                )
                .await?;
            anyhow::ensure!(
                navigation.get("exception").is_none(),
                "Moli navigation failed: {}",
                navigation
            );
        }
        wait_for_document(&mut page, selector, failure_expression).await
    }
    .await;
    let closed = page.close_async().await;
    let rendered = result?;
    closed?;
    Ok(rendered)
}

async fn wait_for_document(
    page: &mut moli_core::page::Page,
    selector: &str,
    failure_expression: Option<&str>,
) -> anyhow::Result<RenderedPage> {
    // 在隔离世界判定就绪并序列化，避免页面覆盖 document/JSON；导航后重新绑定。
    // 终止条件先于就绪条件，已被拦截的页面不必等待结果节点或文档加载完成。
    let expression = format!(
        "(() => {{ const failure = ({}); if (failure) return JSON.stringify({{failure}}); if (document.readyState === 'loading' || !document.querySelector({})) return null; return JSON.stringify({{html: (document.doctype ? new XMLSerializer().serializeToString(document.doctype) + '\\n' : '') + document.documentElement.outerHTML, url: location.href}}); }})()",
        failure_expression.unwrap_or("null"),
        serde_json::to_string(selector)?,
    );
    loop {
        // 先由高层 Interface 跟随待处理的 JS 导航，旧文档不能满足新页面的就绪条件。
        page.evaluate_runtime_expression_async("void 0").await?;
        let context = page.create_isolated_world_async("stravia", false).await?;
        let evaluated = page.evaluate_runtime_expression_in_execution_context_without_navigation_follow_with_await_async(
                context, &expression, false,
            ).await;
        if page.has_pending_location_navigation().await?
            || !page
                .has_isolated_execution_context_id_async(context)
                .await?
        {
            continue;
        }
        let evaluated = evaluated?;
        anyhow::ensure!(
            evaluated.get("exception").is_none(),
            "Moli JavaScript evaluation failed: {}",
            evaluated
        );
        if let Some(serialized) = evaluated["value"].as_str() {
            let value: serde_json::Value = serde_json::from_str(serialized)?;
            if let Some(failure) = value["failure"].as_str() {
                anyhow::bail!("{failure}");
            }
            let url = value["url"].as_str().context("missing rendered URL")?;
            validate_navigation(url)?;
            return Ok(RenderedPage {
                html: value["html"]
                    .as_str()
                    .context("missing rendered HTML")?
                    .to_owned(),
                url: url.to_owned(),
                ready: true,
            });
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn validate_navigation(url: &str) -> anyhow::Result<()> {
    if url == "about:blank" {
        return Ok(());
    }
    crate::fetch::policy::validate_url(url)
        .map(|_| ())
        .map_err(Into::into)
}

pub(crate) struct RenderRequest<'a> {
    pub url: &'a str,
    pub preflight_url: Option<&'a str>,
    pub ready_selector: &'a str,
    pub failure_expression: Option<&'static str>,
    pub timeout: Duration,
    pub request_guard: Option<fn(&str) -> bool>,
}

#[derive(Debug)]
pub(crate) struct RenderedPage {
    pub html: String,
    pub url: String,
    pub ready: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Fixture {
        config: BrowserLaunchConfig,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Fixture {
        async fn start(private: std::net::SocketAddr) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy =
                url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let task = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (mut stream, _) = accepted.unwrap();
                            let recorded = recorded.clone();
                            connections.spawn(async move {
                                let mut request = Vec::new();
                                while !request.ends_with(b"\r\n\r\n") && request.len() < 32768 {
                                    let Ok(byte) = stream.read_u8().await else { return };
                                    request.push(byte);
                                }
                                let request = String::from_utf8_lossy(&request);
                                let raw_url = request.split_whitespace().nth(1).unwrap_or_default();
                                let Ok(url) = url::Url::parse(raw_url) else { return };
                                recorded.lock().await.push(url.as_str().to_owned());
                                if url.path() == "/pending.js" {
                                    std::future::pending::<()>().await;
                                }
                                let (content_type, cookie, body) = match url.path() {
                                    "/identity-home" => ("text/html", "Set-Cookie: identity=retained; Path=/; HttpOnly; Max-Age=3600\r\n", "<html><body><script>fetch('/identity-observed')</script>identity ready</body></html>".to_owned()),
                                    "/identity-expire" => ("text/html", "Set-Cookie: identity=retained; Path=/; HttpOnly; Max-Age=1\r\n", "<html><body>expires shortly</body></html>".to_owned()),
                                    "/identity-path" => ("text/html", "Set-Cookie: identity=retained; Path=/other; HttpOnly; Max-Age=3600\r\n", "<html><body>unrelated path</body></html>".to_owned()),
                                    "/identity-secure" => ("text/html", "Set-Cookie: identity=retained; Path=/; Secure; HttpOnly; Max-Age=3600\r\n", "<html><body>secure only</body></html>".to_owned()),
                                    "/identity-results" => {
                                        let cookie = request.to_ascii_lowercase().contains("cookie: identity=retained");
                                        ("text/html", "", format!("<html><body data-cookie=\"{cookie}\">results</body></html>"))
                                    }
                                    "/challenge" => ("text/html", "", "<html><body><script>setTimeout(()=>{document.body.innerHTML='<p>Our systems have detected unusual traffic from your computer network.</p><div class=\"g-recaptcha\"></div>'},30)</script></body></html>".to_owned()),
                                    "/challenge-redirect" => ("text/html", "", "<html><body><script>location.replace('/challenge')</script></body></html>".to_owned()),
                                    "/challenge-preflight" => ("text/html", "", "<html><body>Our systems have detected unusual traffic from your computer network.</body></html>".to_owned()),
                                    "/traffic-results" => ("text/html", "", "<html><body><a href='https://example.com/sorry/'><h3>Understanding unusual traffic and g-recaptcha</h3></a></body></html>".to_owned()),
                                    "/redirect" => ("text/html", "", "<html><body><script>location.replace('/worlds')</script><script defer src='/pending.js'></script></body></html>".to_owned()),
                                    "/preflight" => ("text/html", "Set-Cookie: gate=passed; Path=/\r\n", "<html><body><script>sessionStorage.setItem('gate','passed')</script>cookie ready</body></html>".to_owned()),
                                    "/worlds" => ("text/html", "", "<html><body><script>globalThis.frameMarker='top';const probe=new Error();Object.defineProperty(probe,'stack',{get(){document.body.dataset.stackRead='true';return 'probe'}});console.debug(probe)</script><iframe src='/frame' onload=\"document.body.id='ready'\"></iframe></body></html>".to_owned()),
                                    "/worker.js" => ("text/javascript", "", "postMessage({ua:navigator.userAgent, language:navigator.language});".to_owned()),
                                    "/frame" => ("text/html", "", "<html><body><script>globalThis.frameMarker='frame';parent.postMessage({frame:true,ua:navigator.userAgent,webdriver:navigator.webdriver},'*')</script></body></html>".to_owned()),
                                    "/dynamic" => {
                                        let cookie = request.to_ascii_lowercase().contains("cookie: gate=passed");
                                        ("text/html", "", format!(r#"<!doctype html><html><body data-cookie="{cookie}"><script>
                                            globalThis.pageMarker='main';
                                            document.body.dataset.session=String(sessionStorage.getItem('gate'));
                                            let frames=0,worker=false,blocked=false;
                                            function ready(){{if(frames===2&&worker&&blocked)document.body.id='ready'}}
                                            addEventListener('message',e=>{{if(e.data.frame){{if(e.data.webdriver||e.data.ua.includes('Headless'))throw Error('frame fingerprint');frames++;ready()}}}});
                                            const w=new Worker('/worker.js');w.onmessage=e=>{{document.body.dataset.worker=e.data.ua;worker=!e.data.ua.includes('Headless')&&e.data.language==='en-US';w.terminate();ready()}};
                                            fetch('http://{private}/private').then(()=>document.body.id='leaked',()=>{{blocked=true;ready()}});
                                            setTimeout(()=>document.body.dataset.computed=String(6*7),20);
                                            </script><iframe src='/frame'></iframe><iframe src='http://93.184.216.35/frame'></iframe></body></html>"#))
                                    }
                                    _ => ("text/html", "", "<html><body>waiting forever</body></html>".to_owned()),
                                };
                                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                let _ = stream.write_all(response.as_bytes()).await;
                            });
                        }
                        _ = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            });
            Self {
                config: BrowserLaunchConfig {
                    profile_dir: None,
                    proxy: crate::outbound::ResolvedProxy {
                        http: Some(proxy.clone()),
                        https: Some(proxy),
                        no_proxy: Default::default(),
                    },
                },
                requests,
                task,
            }
        }
    }

    fn request<'a>(url: &'a str, selector: &'a str) -> RenderRequest<'a> {
        RenderRequest {
            url,
            preflight_url: None,
            ready_selector: selector,
            failure_expression: None,
            timeout: Duration::from_secs(20),
            request_guard: None,
        }
    }

    struct TempProfile(PathBuf);

    impl TempProfile {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "stravia-browser-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            )))
        }
    }

    impl Drop for TempProfile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn identity_search(runtime: &BrowserRuntime) -> RenderedPage {
        let mut input = request("http://93.184.216.34/identity-results", "body");
        input.preflight_url = Some("http://93.184.216.34/identity-home");
        runtime.render(input).await.unwrap()
    }

    async fn home_visits(fixture: &Fixture) -> usize {
        fixture
            .requests
            .lock()
            .await
            .iter()
            .filter(|url| url.as_str() == "http://93.184.216.34/identity-home")
            .count()
    }

    #[tokio::test]
    async fn moli_preflight_reuses_httponly_cookie_after_profile_rebuild() {
        let fixture = Fixture::start("127.0.0.1:1".parse().unwrap()).await;
        let profile = TempProfile::new();
        let mut config = fixture.config.clone();
        config.profile_dir = Some(profile.0.clone());
        let runtime = BrowserRuntime::new(config.clone());
        assert!(identity_search(&runtime)
            .await
            .html
            .contains("data-cookie=\"true\""));
        assert_eq!(home_visits(&fixture).await, 1);
        assert!(identity_search(&runtime)
            .await
            .html
            .contains("data-cookie=\"true\""));
        assert_eq!(home_visits(&fixture).await, 1);
        drop(runtime);
        let rebuilt = BrowserRuntime::new(config);
        assert!(identity_search(&rebuilt)
            .await
            .html
            .contains("data-cookie=\"true\""));
        assert_eq!(home_visits(&fixture).await, 1);

        // Fetch 的临时分区不能看见搜索身份，即使访问完全相同的 URL。
        let fetch = BrowserRuntime::new(fixture.config.clone());
        let fetched = fetch
            .render(request("http://93.184.216.34/identity-results", "body"))
            .await
            .unwrap();
        assert!(fetched.html.contains("data-cookie=\"false\""));
    }

    #[tokio::test]
    async fn moli_preflight_ignores_expired_and_nonmatching_cookies() {
        let fixture = Fixture::start("127.0.0.1:1".parse().unwrap()).await;
        for seed in [
            "http://93.184.216.34/identity-expire",
            "http://93.184.216.34/identity-path",
            "http://93.184.216.35/identity-home",
            "http://93.184.216.34/identity-secure",
        ] {
            let runtime = BrowserRuntime::new(fixture.config.clone());
            runtime.render(request(seed, "body")).await.unwrap();
            if seed.ends_with("identity-expire") {
                tokio::time::sleep(Duration::from_millis(1100)).await;
            }
            let before = home_visits(&fixture).await;
            assert!(
                identity_search(&runtime)
                    .await
                    .html
                    .contains("data-cookie=\"true\""),
                "{seed}"
            );
            assert_eq!(home_visits(&fixture).await, before + 1, "{seed}");
        }
    }

    #[tokio::test]
    async fn moli_profile_cancellation_flushes_before_next_owner() {
        let fixture = Fixture::start("127.0.0.1:1".parse().unwrap()).await;
        let profile = TempProfile::new();
        let mut config = fixture.config.clone();
        config.profile_dir = Some(profile.0.clone());
        let runtime = BrowserRuntime::new(config.clone());
        let pending = tokio::spawn(async move {
            runtime
                .render(request("http://93.184.216.34/identity-home", "#never"))
                .await
        });
        // 等页面消费设置 Cookie 的响应后再取消，避免只测到启动前取消。
        tokio::time::timeout(Duration::from_secs(10), async {
            while !fixture
                .requests
                .lock()
                .await
                .iter()
                .any(|url| url.ends_with("/identity-observed"))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        pending.abort();
        let _ = pending.await;
        let next = BrowserRuntime::new(config);
        assert!(identity_search(&next)
            .await
            .html
            .contains("data-cookie=\"true\""));
        assert_eq!(home_visits(&fixture).await, 1);
    }

    #[tokio::test]
    async fn moli_google_challenge_stops_before_deadline() {
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(private.local_addr().unwrap()).await;
        let runtime = BrowserRuntime::new(fixture.config.clone());
        for (url, preflight) in [
            ("http://93.184.216.34/challenge", None),
            (
                "http://93.184.216.34/challenge-redirect",
                Some("http://93.184.216.34/preflight"),
            ),
            (
                "http://93.184.216.34/traffic-results",
                Some("http://93.184.216.34/challenge-preflight"),
            ),
        ] {
            let runtime = BrowserRuntime::new(fixture.config.clone());
            let mut input = request(url, "a h3");
            input.preflight_url = preflight;
            input.failure_expression =
                Some(crate::search::engines::search::google::GOOGLE_FAILURE_EXPRESSION);
            let error = tokio::time::timeout(Duration::from_secs(5), runtime.render(input))
                .await
                .expect("challenge must terminate before the render deadline")
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("automated-traffic challenge"),
                "{error:#}"
            );
        }
        let mut input = request("http://93.184.216.34/traffic-results", "a h3");
        input.failure_expression =
            Some(crate::search::engines::search::google::GOOGLE_FAILURE_EXPRESSION);
        let page = runtime.render(input).await.unwrap();
        assert!(page.html.contains("Understanding unusual traffic"));
    }

    #[tokio::test]
    async fn moli_javascript_redirect_rebinds_readiness() {
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(private.local_addr().unwrap()).await;
        let runtime = BrowserRuntime::new(fixture.config.clone());
        let page = runtime
            .render(request("http://93.184.216.34/redirect", "body#ready"))
            .await
            .unwrap();
        assert_eq!(page.url, "http://93.184.216.34/worlds");
        assert!(page.html.contains("id=\"ready\""));
    }

    #[tokio::test]
    async fn moli_dynamic_cookie_workers_frames_and_network_policy() {
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(private.local_addr().unwrap()).await;
        let runtime = BrowserRuntime::new(fixture.config.clone());
        let mut input = request("http://93.184.216.34/dynamic", "body#ready[data-computed]");
        input.preflight_url = Some("http://93.184.216.34/preflight");
        let rendered = runtime.render(input).await.unwrap();
        assert!(rendered.html.contains("data-cookie=\"true\""));
        assert!(rendered.html.contains("data-session=\"passed\""));
        assert!(rendered.html.contains("data-computed=\"42\""));
        assert!(!rendered.html.contains("HeadlessChrome"));
        let repeated = runtime
            .render(request(
                "http://93.184.216.34/dynamic",
                "body#ready[data-computed]",
            ))
            .await
            .unwrap();
        assert!(repeated.html.contains("data-cookie=\"true\""));
        let isolated = BrowserRuntime::new(fixture.config.clone());
        let isolated_page = isolated
            .render(request(
                "http://93.184.216.34/dynamic",
                "body#ready[data-computed]",
            ))
            .await
            .unwrap();
        assert!(isolated_page.html.contains("data-cookie=\"false\""));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), private.accept())
                .await
                .is_err()
        );
        assert!(runtime
            .render(request("file:///etc/passwd", "body"))
            .await
            .is_err());
        assert!(runtime
            .render(request("http://127.0.0.1/", "body"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn moli_timeout_and_cancellation_release_owner_for_next_render() {
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(private.local_addr().unwrap()).await;
        let runtime = BrowserRuntime::new(fixture.config.clone());
        let mut input = request("http://93.184.216.34/wait", "#never");
        input.timeout = Duration::from_millis(300);
        assert!(runtime
            .render(input)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        let clone = runtime.clone();
        let pending = tokio::spawn(async move {
            clone
                .render(request("http://93.184.216.34/wait", "#never"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        pending.abort();
        let _ = pending.await;
        let rendered = runtime
            .render(request("http://93.184.216.34/worlds", "body#ready"))
            .await
            .unwrap();
        assert!(rendered.html.contains("id=\"ready\""));
        assert!(!rendered.html.contains("data-stack-read=\"true\""));
    }
}
