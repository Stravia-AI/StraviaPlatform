use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use moli_core::runtime::{Browser, BrowserConfig, RenderedDomWaitUntil};
use tokio::sync::{mpsc, oneshot};

static NEXT_PROFILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct MoliLaunchConfig {
    pub proxy_server: Option<String>,
    pub no_proxy: Option<String>,
}

#[derive(Clone)]
pub(crate) struct BrowserRuntime {
    inner: Arc<BrowserRuntimeInner>,
}

struct BrowserRuntimeInner {
    config: MoliLaunchConfig,
    profile_dir: PathBuf,
    worker: OnceLock<Result<BrowserWorker, String>>,
}

struct BrowserWorker {
    commands: mpsc::UnboundedSender<BrowserCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

enum BrowserCommand {
    Render {
        request: OwnedRenderRequest,
        response: oneshot::Sender<Result<RenderedPage, String>>,
    },
    Shutdown,
}

struct OwnedRenderRequest {
    url: String,
    preflight_url: Option<String>,
    ready_selector: String,
    timeout: Duration,
}

impl BrowserRuntime {
    pub(crate) fn new(config: MoliLaunchConfig) -> Self {
        let profile_id = NEXT_PROFILE_ID.fetch_add(1, Ordering::Relaxed);
        let profile_dir =
            env::temp_dir().join(format!("stravia-moli-{}-{profile_id}", std::process::id()));
        Self {
            inner: Arc::new(BrowserRuntimeInner {
                config,
                profile_dir,
                worker: OnceLock::new(),
            }),
        }
    }

    fn worker(&self) -> anyhow::Result<&BrowserWorker> {
        self.inner
            .worker
            .get_or_init(|| {
                start_browser_worker(self.inner.config.clone(), &self.inner.profile_dir)
            })
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))
    }

    pub(crate) async fn render(&self, request: RenderRequest<'_>) -> anyhow::Result<RenderedPage> {
        if let Some(guard) = request.request_guard {
            if !guard(request.url) {
                anyhow::bail!("Moli renderer rejected non-public URL `{}`", request.url);
            }
        }

        let worker = self.worker()?;
        let request = OwnedRenderRequest {
            url: request.url.to_owned(),
            preflight_url: request.preflight_url.map(str::to_owned),
            ready_selector: request.ready_selector.to_owned(),
            timeout: request.timeout,
        };
        let (response_tx, response_rx) = oneshot::channel();
        worker
            .commands
            .send(BrowserCommand::Render {
                request,
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("Moli renderer thread is unavailable"))?;

        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("Moli renderer thread exited before returning a result"))?
            .map_err(anyhow::Error::msg)
    }

    #[cfg(test)]
    fn profile_dir(&self) -> &Path {
        &self.inner.profile_dir
    }
}

fn start_browser_worker(
    config: MoliLaunchConfig,
    profile_dir: &Path,
) -> Result<BrowserWorker, String> {
    crate::http_client::initialize_transport().map_err(|error| error.to_string())?;
    fs::create_dir_all(&profile_dir)
        .map_err(|error| format!("Moli profile creation failed: {error}"))?;

    let (commands, receiver) = mpsc::unbounded_channel();
    let (initialized_tx, initialized_rx) = std::sync::mpsc::sync_channel(1);
    let worker_profile_dir = profile_dir.to_owned();
    let worker_thread = thread::Builder::new()
        .name("stravia-moli-renderer".to_owned())
        .spawn(move || {
            run_browser_worker(config, &worker_profile_dir, receiver, initialized_tx);
        })
        .map_err(|error| format!("failed to start Moli renderer thread: {error}"))?;

    match initialized_rx.recv() {
        Ok(Ok(())) => Ok(BrowserWorker {
            commands,
            thread: Mutex::new(Some(worker_thread)),
        }),
        Ok(Err(error)) => {
            let _ = worker_thread.join();
            Err(error)
        }
        Err(_) => {
            let panic = worker_thread.join().is_err();
            Err(if panic {
                "Moli renderer thread panicked during initialization".to_owned()
            } else {
                "Moli renderer thread exited during initialization".to_owned()
            })
        }
    }
}

impl Drop for BrowserWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(BrowserCommand::Shutdown);
        if let Ok(thread) = self.thread.get_mut() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for BrowserRuntimeInner {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            drop(worker);
        }
        let _ = fs::remove_dir_all(&self.profile_dir);
    }
}

fn run_browser_worker(
    config: MoliLaunchConfig,
    profile_dir: &Path,
    mut receiver: mpsc::UnboundedReceiver<BrowserCommand>,
    initialized: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = initialized.send(Err(format!("failed to create Moli runtime: {error}")));
            return;
        }
    };

    runtime.block_on(async move {
        let browser = match Browser::new(browser_config(&config, profile_dir)) {
            Ok(browser) => browser,
            Err(error) => {
                let _ =
                    initialized.send(Err(format!("failed to initialize Moli browser: {error}")));
                return;
            }
        };
        if initialized.send(Ok(())).is_err() {
            return;
        }

        while let Some(command) = receiver.recv().await {
            match command {
                BrowserCommand::Render { request, response } => {
                    let result = render_request(&browser, request).await;
                    let _ = response.send(result);
                }
                BrowserCommand::Shutdown => break,
            }
        }
        drop(browser);
    });
}

fn browser_config(config: &MoliLaunchConfig, profile_dir: &Path) -> BrowserConfig {
    let mut browser_config = BrowserConfig::default();
    browser_config.set_profile_dir(Some(profile_dir.to_owned()));
    let fetch = browser_config.fetch_mut();
    fetch.set_http_proxy(Some(config.proxy_server.clone().unwrap_or_default()));
    fetch.set_http_no_proxy(Some(config.no_proxy.clone().unwrap_or_default()));
    // Browser navigation can follow redirects and create subresource requests.
    // Enforce the egress policy inside Moli instead of validating only the URL
    // initially supplied by Stravia.
    fetch.set_network_blocking(true, Vec::new());
    browser_config
}

async fn render_request(
    browser: &Browser,
    request: OwnedRenderRequest,
) -> Result<RenderedPage, String> {
    if let Some(preflight_url) = request.preflight_url.as_deref() {
        render_page(browser, preflight_url, "body", request.timeout)
            .await
            .map_err(|error| format!("preflight navigation failed: {error}"))?;
    }
    render_page(
        browser,
        &request.url,
        &request.ready_selector,
        request.timeout,
    )
    .await
}

async fn render_page(
    browser: &Browser,
    url: &str,
    ready_selector: &str,
    timeout: Duration,
) -> Result<RenderedPage, String> {
    let started = Instant::now();
    let mut page = browser
        .fetch_allow_http_error_with_wait_until(url, RenderedDomWaitUntil::Done, timeout)
        .await
        .map_err(|error| format!("Moli fetch failed: {error}"))?;

    let remaining = timeout.saturating_sub(started.elapsed());
    let rendered = async {
        browser
            .wait_for_selector(&mut page, ready_selector, remaining)
            .await
            .map_err(|error| {
                format!("failed while waiting for selector `{ready_selector}`: {error}")
            })?;
        let final_url = page.final_url().to_string();
        let html = page
            .serialize_html_async()
            .await
            .map_err(|error| format!("failed to serialize rendered HTML: {error}"))?;
        Ok(RenderedPage {
            html,
            url: final_url,
            ready: true,
        })
    }
    .await;

    let close_result = page.close_async().await;
    if rendered.is_ok() {
        if let Err(error) = close_result {
            return Err(format!("failed to close rendered page: {error}"));
        }
    }
    rendered
}

pub(crate) struct RenderRequest<'a> {
    pub url: &'a str,
    pub preflight_url: Option<&'a str>,
    pub ready_selector: &'a str,
    pub timeout: Duration,
    pub request_guard: Option<fn(&str) -> bool>,
}

pub(crate) struct RenderedPage {
    pub html: String,
    pub url: String,
    pub ready: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_config() -> MoliLaunchConfig {
        MoliLaunchConfig {
            proxy_server: None,
            no_proxy: None,
        }
    }

    #[tokio::test]
    async fn worker_renders_and_cleans_up_on_its_owner_thread() {
        let runtime = BrowserRuntime::new(direct_config());
        let profile_dir = runtime.profile_dir().to_owned();

        let rendered = runtime
            .render(RenderRequest {
                url: "about:blank",
                preflight_url: None,
                ready_selector: "html",
                timeout: Duration::from_secs(5),
                request_guard: None,
            })
            .await
            .expect("about:blank should render");

        assert_eq!(rendered.url, "about:blank");
        assert!(rendered.ready);
        assert!(rendered.html.contains("<html"));
        drop(runtime);
        assert!(!profile_dir.exists());
    }

    #[tokio::test]
    async fn worker_executes_javascript_and_blocks_private_network_fetches() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let html = format!(
            r#"<html><body><script>
            document.body.dataset.computed = String(6 * 7);
            fetch("http://{addr}/private").then(
                () => {{ document.body.id = "leaked"; }},
                () => {{ document.body.id = "blocked"; }}
            );
            </script></body></html>"#
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = proxy.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let runtime = BrowserRuntime::new(MoliLaunchConfig {
            proxy_server: Some(format!("http://{proxy_addr}")),
            no_proxy: None,
        });
        let rendered = runtime
            .render(RenderRequest {
                url: "http://93.184.216.34/javascript",
                preflight_url: None,
                ready_selector: "body#blocked",
                timeout: Duration::from_secs(5),
                request_guard: None,
            })
            .await
            .expect("JavaScript should execute without private network access");
        assert!(rendered.html.contains("data-computed=\"42\""));
        assert!(rendered.html.contains("id=\"blocked\""));
        server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }
}
