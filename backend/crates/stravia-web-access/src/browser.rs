use std::{
    ffi::{OsStr, OsString},
    sync::{Arc, OnceLock},
    time::Duration,
};

use headless_chrome::{
    browser::tab::RequestPausedDecision,
    protocol::cdp::{
        Emulation::{
            SetUserAgentOverride as SetEmulationUserAgentOverride, UserAgentBrandVersion,
            UserAgentMetadata,
        },
        Fetch::{events::RequestPausedEvent, FailRequest, RequestPattern, RequestStage},
        Network::{ErrorReason, SetUserAgentOverride},
        Page,
    },
    Browser, LaunchOptionsBuilder, Tab,
};

const STEALTH_SCRIPT: &str = include_str!("browser_stealth.js");

#[derive(Debug, Clone)]
pub(crate) struct ChromeLaunchConfig {
    pub proxy_server: Option<String>,
    pub args: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct BrowserRuntime {
    inner: Arc<BrowserInner>,
}

struct BrowserInner {
    config: ChromeLaunchConfig,
    browser: OnceLock<Result<Browser, String>>,
}

impl BrowserRuntime {
    pub(crate) fn new(config: ChromeLaunchConfig) -> Self {
        Self {
            inner: Arc::new(BrowserInner {
                config,
                browser: OnceLock::new(),
            }),
        }
    }

    pub(crate) async fn render(&self, request: RenderRequest<'_>) -> eyre::Result<RenderedPage> {
        let runtime = self.clone();
        let url = request.url.to_string();
        let preflight_url = request.preflight_url.map(str::to_string);
        let ready_selector = request.ready_selector.to_string();
        let request_guard = request.request_guard;
        let timeout = request.timeout;

        tokio::task::spawn_blocking(move || {
            runtime.render_blocking(
                &url,
                preflight_url.as_deref(),
                &ready_selector,
                timeout,
                request_guard,
            )
        })
        .await
        .map_err(|error| eyre::eyre!("browser renderer task failed: {error}"))?
    }

    fn render_blocking(
        &self,
        url: &str,
        preflight_url: Option<&str>,
        ready_selector: &str,
        timeout: Duration,
        request_guard: Option<fn(&str) -> bool>,
    ) -> eyre::Result<RenderedPage> {
        let browser = match self
            .inner
            .browser
            .get_or_init(|| launch_browser(&self.inner.config))
        {
            Ok(browser) => browser,
            Err(error) => eyre::bail!("browser renderer is unavailable: {error}"),
        };
        let tab = browser
            .new_tab()
            .map_err(|error| eyre::eyre!("tab creation failed: {error}"))?;
        let rendered = render_tab(
            &tab,
            url,
            preflight_url,
            ready_selector,
            timeout,
            request_guard,
        );
        let _ = tab.close(true);
        rendered
    }
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

fn launch_browser(config: &ChromeLaunchConfig) -> Result<Browser, String> {
    let mut args: Vec<OsString> = vec![OsString::from(
        "--disable-blink-features=AutomationControlled",
    )];
    args.extend(config.args.iter().map(OsString::from));
    let arg_refs: Vec<&OsStr> = args.iter().map(|arg| arg.as_os_str()).collect();
    let mut launch_options = LaunchOptionsBuilder::default();
    launch_options
        .headless(true)
        .window_size(Some((1365, 768)))
        .args(arg_refs)
        .ignore_default_args(vec![OsStr::new("--enable-automation")]);
    if let Some(proxy_server) = &config.proxy_server {
        launch_options.proxy_server(Some(proxy_server.as_str()));
    }
    let launch_options = launch_options
        .build()
        .map_err(|error| format!("launch configuration failed: {error}"))?;

    Browser::new(launch_options).map_err(|error| format!("launch failed: {error}"))
}

fn render_tab(
    tab: &Tab,
    url: &str,
    preflight_url: Option<&str>,
    ready_selector: &str,
    timeout: Duration,
    request_guard: Option<fn(&str) -> bool>,
) -> eyre::Result<RenderedPage> {
    apply_omp_stealth(tab)?;
    if let Some(request_guard) = request_guard {
        tab.enable_fetch(
            Some(&[RequestPattern {
                url_pattern: None,
                resource_Type: None,
                request_stage: Some(RequestStage::Request),
            }]),
            None,
        )
        .map_err(|error| eyre::eyre!("request guard setup failed: {error}"))?;
        tab.enable_request_interception(Arc::new(move |_, _, intercepted: RequestPausedEvent| {
            if request_guard(&intercepted.params.request.url) {
                RequestPausedDecision::Continue(None)
            } else {
                RequestPausedDecision::Fail(FailRequest {
                    request_id: intercepted.params.request_id,
                    error_reason: ErrorReason::BlockedByClient,
                })
            }
        }))
        .map_err(|error| eyre::eyre!("request guard setup failed: {error}"))?;
    }
    if let Some(preflight_url) = preflight_url {
        tab.navigate_to(preflight_url)
            .map_err(|error| eyre::eyre!("preflight navigation failed: {error}"))?;
        tab.wait_for_element_with_custom_timeout("body", timeout)
            .map_err(|error| eyre::eyre!("preflight page did not load: {error}"))?;
    }

    tab.navigate_to(url)
        .map_err(|error| eyre::eyre!("navigation failed: {error}"))?;
    let ready = tab
        .wait_for_element_with_custom_timeout(ready_selector, timeout)
        .is_ok();
    let html = tab
        .get_content()
        .map_err(|error| eyre::eyre!("rendered HTML extraction failed: {error}"))?;

    Ok(RenderedPage {
        html,
        url: tab.get_url(),
        ready,
    })
}

fn apply_omp_stealth(tab: &Tab) -> eyre::Result<()> {
    let user_agent = tab
        .evaluate("navigator.userAgent", true)
        .map_err(|error| eyre::eyre!("user-agent read failed: {error}"))?
        .value
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| eyre::eyre!("browser did not provide a user agent"))?
        .replace("HeadlessChrome/", "Chrome/");
    let full_version = user_agent
        .split("Chrome/")
        .nth(1)
        .and_then(|version| version.split_whitespace().next())
        .unwrap_or("0")
        .to_string();
    let major_version = full_version
        .split('.')
        .next()
        .and_then(|version| version.parse::<usize>().ok())
        .unwrap_or_default();
    let order = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ][major_version % 6];
    let escaped_chars = [" ", " ", ";"];
    let greasey_brand = format!(
        "{}Not{}A{}Brand",
        escaped_chars[order[0]], escaped_chars[order[1]], escaped_chars[order[2]]
    );
    let mut brands = vec![None; 3];
    brands[order[0]] = Some(UserAgentBrandVersion {
        brand: greasey_brand.clone(),
        version: "99".to_string(),
    });
    brands[order[1]] = Some(UserAgentBrandVersion {
        brand: "Chromium".to_string(),
        version: major_version.to_string(),
    });
    brands[order[2]] = Some(UserAgentBrandVersion {
        brand: "Google Chrome".to_string(),
        version: major_version.to_string(),
    });
    let brands: Vec<_> = brands.into_iter().flatten().collect();
    let full_version_list = brands
        .iter()
        .map(|brand| UserAgentBrandVersion {
            brand: brand.brand.clone(),
            version: if brand.brand == greasey_brand {
                "99.0.0.0".to_string()
            } else {
                full_version.clone()
            },
        })
        .collect();

    let user_agent_metadata = UserAgentMetadata {
        brands: Some(brands),
        full_version_list: Some(full_version_list),
        full_version: Some(full_version),
        platform: "Windows".to_string(),
        platform_version: "10.0.0".to_string(),
        architecture: "x86".to_string(),
        model: String::new(),
        mobile: false,
        bitness: Some("64".to_string()),
        wow_64: None,
        form_factors: None,
    };
    let accept_language = Some("en-US,en".to_string());
    let platform = Some("Win32".to_string());
    tab.call_method(SetUserAgentOverride {
        user_agent: user_agent.clone(),
        accept_language: accept_language.clone(),
        platform: platform.clone(),
        user_agent_metadata: Some(user_agent_metadata.clone()),
    })
    .map_err(|error| eyre::eyre!("user-agent override failed: {error}"))?;
    tab.call_method(SetEmulationUserAgentOverride {
        user_agent,
        accept_language,
        platform,
        user_agent_metadata: Some(user_agent_metadata),
    })
    .map_err(|error| eyre::eyre!("user-agent emulation failed: {error}"))?;
    tab.call_method(Page::AddScriptToEvaluateOnNewDocument {
        source: STEALTH_SCRIPT.to_string(),
        world_name: None,
        include_command_line_api: None,
        run_immediately: None,
    })
    .map_err(|error| eyre::eyre!("script injection failed: {error}"))?;

    Ok(())
}
