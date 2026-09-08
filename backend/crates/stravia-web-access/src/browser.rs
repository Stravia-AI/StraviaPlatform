use anyhow::Context;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify};

mod cdp;
mod egress;
mod process;
mod stealth;

use cdp::Cdp;
pub use process::{resolve_browser_executable, validate_browser_executable};

#[derive(Debug, Clone)]
pub(crate) struct ChromeLaunchConfig {
    pub proxy: crate::outbound::ResolvedProxy,
    pub browser_path: Option<std::path::PathBuf>,
}

#[derive(Clone)]
pub(crate) struct BrowserRuntime {
    inner: Arc<BrowserRuntimeInner>,
}

struct BrowserRuntimeInner {
    config: ChromeLaunchConfig,
    browser: Mutex<Option<Arc<Chrome>>>,
}

struct Chrome {
    cdp: Cdp,
    context: String,
    targets: Arc<Targets>,
    events: tokio::task::AbortHandle,
    process: Option<process::Process>,
}

#[derive(Default)]
struct Targets {
    sessions: parking_lot::Mutex<HashMap<String, Result<String, String>>>,
    documents: parking_lot::Mutex<HashMap<(String, String), Document>>,
    changed: Notify,
}

struct Document {
    loader: String,
    ready: bool,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        self.events.abort();
        if let Some(process) = self.process.take() {
            let cdp = self.cdp.clone();
            self.cdp.cleanup(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    cdp.call(None, "Browser.close", json!({})),
                )
                .await;
                drop(process);
            });
        }
    }
}

impl BrowserRuntime {
    pub(crate) fn new(config: ChromeLaunchConfig) -> Self {
        Self {
            inner: Arc::new(BrowserRuntimeInner {
                config,
                browser: Mutex::new(None),
            }),
        }
    }

    pub(crate) async fn require_available(&self) -> anyhow::Result<()> {
        resolve_browser_executable(self.inner.config.browser_path.as_deref()).await?;
        Ok(())
    }

    async fn browser(&self) -> anyhow::Result<Arc<Chrome>> {
        let mut slot = self.inner.browser.lock().await;
        if let Some(browser) = slot.as_ref() {
            if !browser.cdp.is_closed() {
                return Ok(browser.clone());
            }
        }
        *slot = None;
        let browser = Arc::new(Chrome::launch(self.inner.config.clone()).await?);
        *slot = Some(browser.clone());
        Ok(browser)
    }

    pub(crate) async fn render(&self, request: RenderRequest<'_>) -> anyhow::Result<RenderedPage> {
        validate_navigation(request.url)?;
        if let Some(url) = request.preflight_url {
            validate_navigation(url)?;
        }
        tokio::time::timeout(request.timeout, async {
            if let Some(guard) = request.request_guard {
                let url = request.url.to_owned();
                let preflight = request.preflight_url.map(str::to_owned);
                let allowed = tokio::task::spawn_blocking(move || {
                    guard(&url) && preflight.as_deref().is_none_or(guard)
                })
                .await?;
                anyhow::ensure!(allowed, "Chrome renderer rejected a non-public URL");
            }
            let browser = self.browser().await?;
            browser
                .render_page(request.url, request.ready_selector, request.preflight_url)
                .await
        })
        .await
        .context("Chrome rendering timed out")?
    }
}

fn validate_navigation(url: &str) -> anyhow::Result<()> {
    // about:blank 只用于无网络的初始页面，其余导航共用 HTTP SSRF 策略。
    if url == "about:blank" {
        return Ok(());
    }
    crate::fetch::policy::validate_url(url)
        .map(|_| ())
        .map_err(Into::into)
}

fn allowed_request(value: &str) -> bool {
    let Ok(mut url) = url::Url::parse(value) else {
        return false;
    };
    match url.scheme() {
        "data" | "blob" => return true,
        "ws" => {
            let _ = url.set_scheme("http");
        }
        "wss" => {
            let _ = url.set_scheme("https");
        }
        _ => {}
    }
    validate_navigation(url.as_str()).is_ok()
}

fn auto_attach() -> Value {
    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true, "filter": [{"type": "browser", "exclude": true}, {"type": "tab", "exclude": true}, {}]})
}

impl Chrome {
    async fn launch(config: ChromeLaunchConfig) -> anyhow::Result<Self> {
        let proxy = egress::EgressProxy::start(config.proxy).await?;
        let process = process::Process::launch(proxy, config.browser_path.as_deref()).await?;
        let (cdp, mut receiver) = Cdp::connect(&process.endpoint).await?;
        let version = cdp.call(None, "Browser.getVersion", json!({})).await?;
        let ua = stealth::user_agent_override(
            version["product"].as_str().unwrap_or_default(),
            version["userAgent"].as_str().unwrap_or_default(),
        );
        // 显式持有会话上下文，让浏览器 Cookie 的寿命跟随运行时而非临时标签页。
        let context = cdp
            .call(
                None,
                "Target.createBrowserContext",
                json!({"disposeOnDetach": true}),
            )
            .await?;
        let context = context["browserContextId"]
            .as_str()
            .context("Chrome did not return browserContextId")?
            .to_owned();
        let targets = Arc::new(Targets::default());
        let event_cdp = cdp.clone();
        let event_targets = targets.clone();
        // 事件泵不等待 CDP 响应；独立任务处理初始化与拦截，JoinSet 随泵取消。
        let events = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    event = receiver.recv() => {
                        let Some(event) = event else { break };
                        match event["method"].as_str().unwrap_or_default() {
                            "Target.attachedToTarget" => {
                                let cdp = event_cdp.clone();
                                let targets = event_targets.clone();
                                let ua = ua.clone();
                                handlers.spawn(async move {
                                    let params = &event["params"];
                                    let Some(session) = params["sessionId"].as_str() else { return };
                                    let Some(target) = params["targetInfo"]["targetId"].as_str() else { return };
                                    let kind = params["targetInfo"]["type"].as_str().unwrap_or_default();
                                    let result = initialize_target(&cdp, session, kind, &ua).await.map(|()| session.to_owned()).map_err(|error| error.to_string());
                                    if result.is_err() { let _ = cdp.call(None, "Target.closeTarget", json!({"targetId": target})).await; }
                                    targets.sessions.lock().insert(target.to_owned(), result);
                                    targets.changed.notify_waiters();
                                });
                            }
                            "Target.detachedFromTarget" => {
                                if let Some(session) = event["params"]["sessionId"].as_str() {
                                    event_targets.sessions.lock().retain(|_, value| value.as_ref().is_ok_and(|id| id != session));
                                    event_targets.documents.lock().retain(|(id, _), _| id != session);
                                    event_targets.changed.notify_waiters();
                                }
                            }
                            "Page.lifecycleEvent" => {
                                let params = &event["params"];
                                if matches!(params["name"].as_str(), Some("init" | "DOMContentLoaded" | "load")) {
                                    if let (Some(session), Some(frame), Some(loader)) = (
                                        event["sessionId"].as_str(),
                                        params["frameId"].as_str(),
                                        params["loaderId"].as_str(),
                                    ) {
                                        let mut documents = event_targets.documents.lock();
                                        let document = documents.entry((session.to_owned(), frame.to_owned()))
                                            .or_insert_with(|| Document { loader: loader.to_owned(), ready: false });
                                        if params["name"] == "init" {
                                            document.loader.clear();
                                            document.loader.push_str(loader);
                                            document.ready = false;
                                        } else if document.loader == loader {
                                            document.ready = true;
                                        }
                                        drop(documents);
                                        event_targets.changed.notify_waiters();
                                    }
                                }
                            }
                            "Fetch.requestPaused" => {
                                let cdp = event_cdp.clone();
                                handlers.spawn(async move {
                                    let url = event["params"]["request"]["url"].as_str().unwrap_or_default();
                                    let allowed = allowed_request(url);
                                    let method = if allowed { "Fetch.continueRequest" } else { "Fetch.failRequest" };
                                    let mut params = json!({"requestId": event["params"]["requestId"]});
                                    if !allowed { params["errorReason"] = "BlockedByClient".into(); }
                                    let _ = cdp.call(event["sessionId"].as_str(), method, params).await;
                                });
                            }
                            _ => {}
                        }
                    }
                    _ = handlers.join_next(), if !handlers.is_empty() => {}
                }
            }
        });
        let browser = Self {
            cdp,
            context,
            targets,
            events: events.abort_handle(),
            process: Some(process),
        };
        browser
            .cdp
            .call(None, "Target.setAutoAttach", auto_attach())
            .await?;
        Ok(browser)
    }

    async fn page(&self) -> anyhow::Result<Page> {
        let cdp = self.cdp.clone();
        let context = self.context.clone();
        let targets = self.targets.clone();
        let (mut send, receive) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = async {
                let created = cdp
                    .call(
                        None,
                        "Target.createTarget",
                        json!({"url": "about:blank", "browserContextId": context}),
                    )
                    .await?;
                let target = created["targetId"]
                    .as_str()
                    .context("Chrome did not return targetId")?
                    .to_owned();
                let mut page = Page {
                    cdp,
                    target,
                    session: String::new(),
                    targets: targets.clone(),
                };
                loop {
                    let changed = targets.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    let result = targets.sessions.lock().get(&page.target).cloned();
                    if let Some(result) = result {
                        page.session = result.map_err(anyhow::Error::msg)?;
                        return Ok::<_, anyhow::Error>(page);
                    }
                    tokio::select! {
                        _ = changed => {},
                        _ = send.closed() => anyhow::bail!("Chrome page creation canceled"),
                    }
                }
            }
            .await;
            // 接收端取消时 send 返回的 Page 会立即 Drop 并关闭 target。
            let _ = send.send(result);
        });
        receive.await?
    }

    async fn render_page(
        &self,
        url: &str,
        selector: &str,
        preflight: Option<&str>,
    ) -> anyhow::Result<RenderedPage> {
        let page = self.page().await?;
        let result = async {
            if let Some(url) = preflight {
                page.navigate_and_render(url, "body")
                    .await
                    .context("preflight navigation failed")?;
            }
            page.navigate_and_render(url, selector).await
        }
        .await;
        page.close()
            .await
            .context("closing Chrome render target failed")?;
        result
    }
}

async fn initialize_target(cdp: &Cdp, session: &str, kind: &str, ua: &Value) -> anyhow::Result<()> {
    let page = matches!(kind, "page" | "iframe" | "webview");
    let worker = matches!(kind, "worker" | "shared_worker" | "service_worker");
    if page || worker || kind == "background_page" {
        cdp.call(Some(session), "Network.enable", json!({})).await?;
        cdp.call(Some(session), "Network.setUserAgentOverride", ua.clone())
            .await?;
        // Worker 的 CDP 域没有 Emulation；Network 覆盖仍然先于脚本恢复。
        if !worker {
            cdp.call(Some(session), "Emulation.setUserAgentOverride", ua.clone())
                .await?;
        }
        // Worker 没有 Fetch 域；其网络仍强制经过同一个校验出口。
        if page {
            cdp.call(
                Some(session),
                "Fetch.enable",
                json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
            )
            .await?;
        }
        cdp.call(Some(session), "Target.setAutoAttach", auto_attach())
            .await?;
    }
    if page {
        cdp.call(Some(session), "Page.enable", json!({})).await?;
        cdp.call(
            Some(session),
            "Page.setLifecycleEventsEnabled",
            json!({"enabled": true}),
        )
        .await?;
        // 子 frame 继承顶层 viewport；下载策略与设备尺寸命令仅支持顶层 target。
        if kind != "iframe" {
            cdp.call(
                Some(session),
                "Page.setDownloadBehavior",
                json!({"behavior": "deny"}),
            )
            .await?;
            cdp.call(
                Some(session),
                "Emulation.setDeviceMetricsOverride",
                json!({"width": 1365, "height": 768, "deviceScaleFactor": 1.25, "mobile": false}),
            )
            .await?;
        }
        cdp.call(
            Some(session),
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": stealth::script(), "runImmediately": true}),
        )
        .await?;
    }
    if worker {
        // 拉取 worker 默认上下文，不启用 Runtime 事件，也不添加 sourceURL。
        let global = cdp.call(Some(session), "Runtime.evaluate", json!({"expression": "globalThis", "serializationOptions": {"serialization": "idOnly"}})).await?;
        if let Some(object) = global["result"]["objectId"].as_str() {
            cdp.call(
                Some(session),
                "Runtime.releaseObject",
                json!({"objectId": object}),
            )
            .await?;
        }
    }
    cdp.call(Some(session), "Runtime.runIfWaitingForDebugger", json!({}))
        .await?;
    Ok(())
}

struct Page {
    cdp: Cdp,
    target: String,
    session: String,
    targets: Arc<Targets>,
}

impl Drop for Page {
    fn drop(&mut self) {
        if self.target.is_empty() {
            return;
        }
        let cdp = self.cdp.clone();
        let target = std::mem::take(&mut self.target);
        self.cdp.cleanup(async move {
            let _ = cdp
                .call(None, "Target.closeTarget", json!({"targetId": target}))
                .await;
        });
    }
}

impl Page {
    async fn close(mut self) -> anyhow::Result<()> {
        self.cdp
            .call(None, "Target.closeTarget", json!({"targetId": self.target}))
            .await?;
        self.target.clear();
        Ok(())
    }

    async fn evaluate(
        &self,
        frame: &str,
        expression: &str,
        main_world: bool,
    ) -> anyhow::Result<Value> {
        // 每次按需拉取，避免导航期间取得的上下文缓存失效；同进程子 frame 同样按 frameId 选择。
        let isolated = self
            .cdp
            .call(
                Some(&self.session),
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": "stravia", "grantUniveralAccess": false}),
            )
            .await?;
        let mut context = isolated["executionContextId"]
            .as_i64()
            .context("missing isolated context")?;
        if main_world {
            let document = self.cdp.call(Some(&self.session), "Runtime.evaluate", json!({"expression": "document", "contextId": context, "serializationOptions": {"serialization": "idOnly"}})).await?;
            let object = document["result"]["objectId"]
                .as_str()
                .context("missing frame document")?;
            let node = self
                .cdp
                .call(
                    Some(&self.session),
                    "DOM.describeNode",
                    json!({"objectId": object}),
                )
                .await?;
            let resolved = self
                .cdp
                .call(
                    Some(&self.session),
                    "DOM.resolveNode",
                    json!({"backendNodeId": node["node"]["backendNodeId"]}),
                )
                .await?;
            let main_object = resolved["object"]["objectId"]
                .as_str()
                .context("missing main document")?;
            context = main_object
                .split('.')
                .nth(1)
                .context("invalid main context object")?
                .parse()?;
            self.cdp
                .call(
                    Some(&self.session),
                    "Runtime.releaseObject",
                    json!({"objectId": object}),
                )
                .await?;
            self.cdp
                .call(
                    Some(&self.session),
                    "Runtime.releaseObject",
                    json!({"objectId": main_object}),
                )
                .await?;
        }
        let result = self.cdp.call(Some(&self.session), "Runtime.evaluate", json!({"expression": expression, "contextId": context, "returnByValue": true, "awaitPromise": true})).await?;
        anyhow::ensure!(
            result.get("exceptionDetails").is_none(),
            "JavaScript evaluation failed: {}",
            result["exceptionDetails"]
        );
        Ok(result["result"]["value"].clone())
    }

    async fn navigate_and_render(&self, url: &str, selector: &str) -> anyhow::Result<RenderedPage> {
        let previous = self
            .cdp
            .call(Some(&self.session), "Page.getFrameTree", json!({}))
            .await?;
        let previous_loader = previous["frameTree"]["frame"]["loaderId"].as_str();
        let navigation = self
            .cdp
            .call(Some(&self.session), "Page.navigate", json!({"url": url}))
            .await?;
        anyhow::ensure!(
            navigation.get("errorText").is_none(),
            "Chrome navigation failed: {}",
            navigation["errorText"]
        );
        let frame = navigation["frameId"]
            .as_str()
            .context("missing navigation frame")?;
        let key = (self.session.clone(), frame.to_owned());
        // 与导航监听器一致，以当前文档相对导航前的变化为准；JS 跳转可能替换 navigate 返回的 loader。
        let expression = format!(
            r#"new Promise((resolve, reject) => {{
            const observer = new MutationObserver(check);
            function check() {{
                try {{
                    if (document.readyState === 'loading' || !document.querySelector({selector})) return;
                    observer.disconnect();
                    document.removeEventListener('DOMContentLoaded', check);
                    resolve({{
                        html: (document.doctype ? new XMLSerializer().serializeToString(document.doctype) + '\n' : '') + document.documentElement.outerHTML,
                        url: location.href
                    }});
                }} catch (error) {{ observer.disconnect(); reject(error); }}
            }}
            observer.observe(document, {{childList: true, subtree: true, attributes: true}});
            document.addEventListener('DOMContentLoaded', check, {{once: true}});
            check();
        }})"#,
            selector = serde_json::to_string(selector)?
        );
        let value = 'document: loop {
            let changed = self.targets.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let loader = self
                .targets
                .documents
                .lock()
                .get(&key)
                .filter(|document| {
                    document.ready
                        && (navigation.get("loaderId").is_none()
                            || Some(document.loader.as_str()) != previous_loader)
                })
                .map(|document| document.loader.clone());
            let Some(loader) = loader else {
                changed.await;
                continue;
            };
            let evaluation = self.evaluate(frame, &expression, false);
            tokio::pin!(evaluation);
            loop {
                let changed = self.targets.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self
                    .targets
                    .documents
                    .lock()
                    .get(&key)
                    .is_some_and(|document| document.ready && document.loader == loader)
                {
                    continue 'document;
                }
                tokio::select! {
                    _ = changed => {},
                    result = &mut evaluation => match result {
                        Ok(value) => break 'document value,
                        Err(error) => {
                            // 上下文失效响应可能先于生命周期事件抵达；只在确认换文档后重新绑定。
                            let current = self.cdp.call(Some(&self.session), "Page.getFrameTree", json!({})).await;
                            if current.as_ref().is_ok_and(|tree|
                                tree["frameTree"]["frame"]["loaderId"].as_str().is_some_and(|id| id != loader))
                            {
                                continue 'document;
                            }
                            return Err(error);
                        }
                    },
                }
            }
        };
        let url = value["url"].as_str().context("missing rendered URL")?;
        validate_navigation(url)?;
        Ok(RenderedPage {
            html: value["html"]
                .as_str()
                .context("missing rendered HTML")?
                .to_owned(),
            url: url.to_owned(),
            ready: true,
        })
    }
}

pub(crate) struct RenderRequest<'a> {
    pub url: &'a str,
    pub preflight_url: Option<&'a str>,
    pub ready_selector: &'a str,
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
        config: ChromeLaunchConfig,
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
            let task = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (mut stream, _) = accepted.unwrap();
                            connections.spawn(async move {
                                let mut request = Vec::new();
                                while !request.ends_with(b"\r\n\r\n") && request.len() < 32768 {
                                    let Ok(byte) = stream.read_u8().await else { return };
                                    request.push(byte);
                                }
                                let request = String::from_utf8_lossy(&request);
                                let raw_url = request.split_whitespace().nth(1).unwrap_or_default();
                                let Ok(url) = url::Url::parse(raw_url) else { return };
                                if url.path() == "/pending.js" {
                                    std::future::pending::<()>().await;
                                }
                                let (content_type, cookie, body) = match url.path() {
                                    "/redirect" => ("text/html", "", "<html><body><script>location.replace('/worlds')</script><script defer src='/pending.js'></script></body></html>".to_owned()),
                                    "/preflight" => ("text/html", "Set-Cookie: gate=passed; Path=/\r\n", "<html><body>cookie ready</body></html>".to_owned()),
                                    "/worlds" => ("text/html", "", "<html><body><script>globalThis.frameMarker='top';const probe=new Error();Object.defineProperty(probe,'stack',{get(){document.body.dataset.stackRead='true';return 'probe'}});console.debug(probe)</script><iframe src='/frame' onload=\"document.body.id='ready'\"></iframe></body></html>".to_owned()),
                                    "/worker.js" => ("text/javascript", "", "postMessage({ua:navigator.userAgent, language:navigator.language});".to_owned()),
                                    "/frame" => ("text/html", "", "<html><body><script>globalThis.frameMarker='frame';parent.postMessage({frame:true,ua:navigator.userAgent,webdriver:navigator.webdriver},'*')</script></body></html>".to_owned()),
                                    "/dynamic" => {
                                        let cookie = request.to_ascii_lowercase().contains("cookie: gate=passed");
                                        ("text/html", "", format!(r#"<!doctype html><html><body data-cookie="{cookie}"><script>
                                            globalThis.pageMarker='main';
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
                config: ChromeLaunchConfig {
                    browser_path: Some(
                        process::resolve_browser_executable(None)
                            .await
                            .expect("installed Chrome/Chromium"),
                    ),
                    proxy: crate::outbound::ResolvedProxy {
                        http: Some(proxy.clone()),
                        https: Some(proxy),
                        no_proxy: Default::default(),
                    },
                },
                task,
            }
        }
    }

    fn request<'a>(url: &'a str, selector: &'a str) -> RenderRequest<'a> {
        RenderRequest {
            url,
            preflight_url: None,
            ready_selector: selector,
            timeout: Duration::from_secs(20),
            request_guard: None,
        }
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome/Chromium; set STRAVIA_CHROME_PATH when not on a standard path"]
    async fn chrome_javascript_redirect_rebinds_readiness() {
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

    async fn wait_for_no_pages(browser: &Chrome) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let targets = browser
                    .cdp
                    .call(None, "Target.getTargets", json!({}))
                    .await
                    .unwrap();
                if targets["targetInfos"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|target| target["type"] != "page")
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("canceled page must close");
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome/Chromium; set STRAVIA_CHROME_PATH when not on a standard path"]
    async fn chrome_dynamic_cookie_workers_frames_and_network_policy() {
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(private.local_addr().unwrap()).await;
        let runtime = BrowserRuntime::new(fixture.config.clone());
        let mut input = request("http://93.184.216.34/dynamic", "body#ready[data-computed]");
        input.preflight_url = Some("http://93.184.216.34/preflight");
        let rendered = runtime.render(input).await.unwrap();
        assert!(rendered.html.contains("data-cookie=\"true\""));
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
    #[ignore = "requires installed Chrome/Chromium; set STRAVIA_CHROME_PATH when not on a standard path"]
    async fn chrome_worlds_timeout_cancel_and_profile_cleanup() {
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::start(private.local_addr().unwrap()).await;
        let runtime = BrowserRuntime::new(fixture.config.clone());
        let browser = runtime.browser().await.unwrap();
        let profile = browser.process.as_ref().unwrap().profile.clone();
        let page = browser.page().await.unwrap();
        page.navigate_and_render("http://93.184.216.34/worlds", "body#ready")
            .await
            .unwrap();
        let tree = page
            .cdp
            .call(Some(&page.session), "Page.getFrameTree", json!({}))
            .await
            .unwrap();
        let frame = tree["frameTree"]["frame"]["id"].as_str().unwrap();
        assert_eq!(
            page.evaluate(frame, "globalThis.frameMarker", true)
                .await
                .unwrap(),
            "top"
        );
        let child = tree["frameTree"]["childFrames"][0]["frame"]["id"]
            .as_str()
            .unwrap();
        assert_eq!(
            page.evaluate(child, "globalThis.frameMarker", true)
                .await
                .unwrap(),
            "frame"
        );
        assert_eq!(
            page.evaluate(child, "typeof globalThis.frameMarker", false)
                .await
                .unwrap(),
            "undefined"
        );
        assert_eq!(
            page.evaluate(frame, "typeof globalThis.frameMarker", false)
                .await
                .unwrap(),
            "undefined"
        );
        assert_eq!(
            page.evaluate(frame, "navigator.webdriver", true)
                .await
                .unwrap(),
            false
        );
        assert_eq!(
            page.evaluate(
                frame,
                "navigator.userAgent.includes('HeadlessChrome')",
                true
            )
            .await
            .unwrap(),
            false
        );
        assert_eq!(
            page.evaluate(frame, "devicePixelRatio", true)
                .await
                .unwrap(),
            1.25
        );
        assert_eq!(
            page.evaluate(frame, "document.body.dataset.stackRead === 'true'", true)
                .await
                .unwrap(),
            false
        );
        let stack = page
            .evaluate(frame, "new Error().stack", false)
            .await
            .unwrap();
        assert!(!stack.as_str().unwrap().contains("pptr:"));
        assert!(!stack
            .as_str()
            .unwrap()
            .contains("__puppeteer_evaluation_script__"));
        page.close().await.unwrap();
        let mut timeout = request("http://93.184.216.34/wait", "#never");
        timeout.timeout = Duration::from_millis(300);
        assert!(runtime
            .render(timeout)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        wait_for_no_pages(&browser).await;
        let clone = runtime.clone();
        let pending = tokio::spawn(async move {
            clone
                .render(request("http://93.184.216.34/wait", "#never"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        pending.abort();
        let _ = pending.await;
        wait_for_no_pages(&browser).await;
        drop(browser);
        drop(runtime);
        tokio::time::timeout(Duration::from_secs(10), async {
            while tokio::fs::try_exists(&profile).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("last runtime owner must reap Chrome and remove profile");
    }
}
