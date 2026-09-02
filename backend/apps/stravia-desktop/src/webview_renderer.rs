use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, anyhow};
use serde::Deserialize;
#[cfg(target_os = "windows")]
use stravia_web_access::renderer::BROWSER_STEALTH_SCRIPT;
use stravia_web_access::renderer::{
    PageRenderer, PageRendererConfig, PageRendererFactory, RenderRequest, RenderRequestPolicy,
    RenderedPage, is_public_web_request,
};
use tauri::{
    AppHandle, WebviewUrl, WebviewWindow,
    webview::{NewWindowResponse, PageLoadEvent, WebviewWindowBuilder},
};
use tokio::sync::{mpsc, oneshot};
use url::Url;

use crate::guarded_proxy::GuardedProxy;

const SELECTOR_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const EVALUATION_TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_WINDOW_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) struct TauriPageRendererFactory {
    app: AppHandle,
    profile_root: PathBuf,
    renderers: Mutex<HashMap<PageRendererConfig, Arc<TauriPageRenderer>>>,
}

impl TauriPageRendererFactory {
    pub(crate) fn new(app: AppHandle, profile_root: PathBuf) -> Self {
        Self {
            app,
            profile_root,
            renderers: Mutex::new(HashMap::new()),
        }
    }
}

impl PageRendererFactory for TauriPageRendererFactory {
    fn build(&self, config: PageRendererConfig) -> Result<Arc<dyn PageRenderer>, String> {
        let mut renderers = self
            .renderers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(renderer) = renderers.get(&config) {
            return Ok(Arc::clone(renderer) as Arc<dyn PageRenderer>);
        }

        let renderer = Arc::new(TauriPageRenderer::new(
            self.app.clone(),
            self.profile_root.join(profile_key(&config)),
            config.clone(),
        ));
        renderers.insert(config, Arc::clone(&renderer));
        Ok(renderer)
    }
}

struct TauriPageRenderer {
    app: AppHandle,
    profile_dir: PathBuf,
    config: PageRendererConfig,
    guarded_proxy: tokio::sync::OnceCell<Arc<GuardedProxy>>,
    unrestricted_proxy: tokio::sync::OnceCell<Arc<GuardedProxy>>,
}

impl TauriPageRenderer {
    fn new(app: AppHandle, profile_dir: PathBuf, config: PageRendererConfig) -> Self {
        Self {
            app,
            profile_dir,
            config,
            guarded_proxy: tokio::sync::OnceCell::new(),
            unrestricted_proxy: tokio::sync::OnceCell::new(),
        }
    }

    async fn proxy(
        &self,
        request_policy: RenderRequestPolicy,
    ) -> anyhow::Result<&Arc<GuardedProxy>> {
        let (cell, enforce_public_web) = match request_policy {
            RenderRequestPolicy::PublicWeb => (&self.guarded_proxy, true),
            RenderRequestPolicy::Unrestricted => (&self.unrestricted_proxy, false),
        };
        cell.get_or_try_init(|| GuardedProxy::start(self.config.clone(), enforce_public_web))
            .await
    }

    fn create_window(
        &self,
        proxy: &GuardedProxy,
        request_policy: RenderRequestPolicy,
        load_tx: mpsc::UnboundedSender<PageEvent>,
    ) -> anyhow::Result<WebviewWindow> {
        let id = NEXT_WINDOW_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let label = format!("web-render-{}-{id}", std::process::id());
        // WebView2 要求共享 data directory 的窗口使用相同 browser arguments；
        // 两种请求策略使用不同 loopback proxy，因此必须隔离环境。
        let profile_dir = match request_policy {
            RenderRequestPolicy::PublicWeb => self.profile_dir.join("public-web"),
            RenderRequestPolicy::Unrestricted => self.profile_dir.join("unrestricted"),
        };
        let builder = WebviewWindowBuilder::new(
            &self.app,
            label,
            WebviewUrl::External(
                Url::parse("about:blank").expect("about:blank is a valid WebView URL"),
            ),
        )
        .visible(false)
        .focused(false)
        .focusable(false)
        .skip_taskbar(true)
        .inner_size(1365.0, 768.0)
        .data_directory(profile_dir)
        .incognito(true)
        .proxy_url(proxy.url())
        .devtools(false);
        #[cfg(target_os = "windows")]
        let builder = builder.initialization_script(BROWSER_STEALTH_SCRIPT);
        let window = builder
            .on_navigation(move |url| {
                matches!(url.scheme(), "about" | "blob" | "data")
                    || (matches!(url.scheme(), "http" | "https")
                        && (request_policy == RenderRequestPolicy::Unrestricted
                            || is_public_web_request(url.as_str())))
            })
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .on_download(|_, _| false)
            .on_page_load(move |_window, payload| {
                let _ = load_tx.send(PageEvent {
                    started: matches!(payload.event(), PageLoadEvent::Started),
                    url: payload.url().to_string(),
                });
            })
            .build()
            .context("failed to create the hidden render WebView")?;
        Ok(window)
    }
}

#[async_trait::async_trait]
impl PageRenderer for TauriPageRenderer {
    async fn render(&self, request: RenderRequest<'_>) -> eyre::Result<RenderedPage> {
        let proxy = self
            .proxy(request.request_policy)
            .await
            .map_err(|error| eyre::eyre!("{error:#}"))?;
        let (load_tx, mut load_rx) = mpsc::unbounded_channel();
        let window = self
            .create_window(proxy, request.request_policy, load_tx)
            .map_err(|error| eyre::eyre!("{error:#}"))?;
        let _window_guard = DestroyWindow(window.clone());

        if let Some(preflight_url) = request.preflight_url {
            let ready = navigate_and_wait(
                &window,
                &mut load_rx,
                preflight_url,
                "body",
                request.timeout,
            )
            .await
            .map_err(|error| eyre::eyre!("{error:#}"))?;
            if !ready {
                return Err(eyre::eyre!(
                    "preflight page did not load within {} seconds",
                    request.timeout.as_secs()
                ));
            }
        }

        let ready = navigate_and_wait(
            &window,
            &mut load_rx,
            request.url,
            request.ready_selector,
            request.timeout,
        )
        .await
        .map_err(|error| eyre::eyre!("{error:#}"))?;
        let extracted: ExtractedDocument = evaluate_json(
            &window,
            "JSON.stringify({html: document.documentElement.outerHTML, url: location.href})",
        )
        .await
        .context("rendered HTML extraction failed")
        .map_err(|error| eyre::eyre!("{error:#}"))?;

        Ok(RenderedPage {
            html: extracted.html,
            url: extracted.url,
            ready,
        })
    }
}

struct DestroyWindow(WebviewWindow);

impl Drop for DestroyWindow {
    fn drop(&mut self) {
        let _ = self.0.destroy();
    }
}

struct PageEvent {
    started: bool,
    url: String,
}

#[derive(Deserialize)]
struct ExtractedDocument {
    html: String,
    url: String,
}

async fn navigate_and_wait(
    window: &WebviewWindow,
    load_rx: &mut mpsc::UnboundedReceiver<PageEvent>,
    value: &str,
    selector: &str,
    timeout: Duration,
) -> anyhow::Result<bool> {
    while load_rx.try_recv().is_ok() {}
    let url = Url::parse(value).context("invalid render navigation URL")?;
    window
        .navigate(url)
        .context("render WebView navigation failed")?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let event = tokio::time::timeout(remaining, load_rx.recv())
            .await
            .context("render WebView navigation did not start")?
            .ok_or_else(|| anyhow!("render WebView page event channel closed"))?;
        if event.started && event.url != "about:blank" {
            break;
        }
    }

    let selector = serde_json::to_string(selector).context("failed to encode ready selector")?;
    let script = format!("Boolean(document.querySelector({selector}))");
    let mut interval = tokio::time::interval(SELECTOR_CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        interval.tick().await;
        match tokio::time::timeout(
            remaining.min(EVALUATION_TIMEOUT),
            evaluate_raw(window, &script),
        )
        .await
        {
            Ok(Ok(value)) if serde_json::from_str::<bool>(&value).unwrap_or(false) => {
                return Ok(true);
            }
            Ok(Ok(_) | Err(_)) | Err(_) => {}
        }
    }
}

async fn evaluate_json<T: for<'de> Deserialize<'de>>(
    window: &WebviewWindow,
    script: &str,
) -> anyhow::Result<T> {
    let raw = tokio::time::timeout(EVALUATION_TIMEOUT, evaluate_raw(window, script))
        .await
        .context("render WebView evaluation timed out")??;
    let json = serde_json::from_str::<String>(&raw).unwrap_or(raw);
    serde_json::from_str(&json).context("render WebView returned invalid JSON")
}

async fn evaluate_raw(window: &WebviewWindow, script: &str) -> anyhow::Result<String> {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    window
        .eval_with_callback(script, move |value| {
            if let Some(tx) = tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = tx.send(value);
            }
        })
        .context("render WebView evaluation failed")?;
    rx.await
        .map_err(|_| anyhow!("render WebView evaluation callback closed"))
}

fn profile_key(config: &PageRendererConfig) -> String {
    let mut hasher = DefaultHasher::new();
    config.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
