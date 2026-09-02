use std::{sync::Arc, time::Duration};

use url::Url;

pub const BROWSER_STEALTH_SCRIPT: &str = include_str!("browser_stealth.js");

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageRendererConfig {
    http_proxy: Option<Url>,
    https_proxy: Option<Url>,
    bypass_hosts: Vec<String>,
}

impl PageRendererConfig {
    pub fn direct() -> Self {
        Self::new(None, None, Vec::new())
    }

    pub(crate) fn new(
        http_proxy: Option<Url>,
        https_proxy: Option<Url>,
        bypass_hosts: Vec<String>,
    ) -> Self {
        Self {
            http_proxy,
            https_proxy,
            bypass_hosts,
        }
    }

    pub fn upstream_proxy_for(&self, target: &Url) -> Option<&Url> {
        if target
            .host_str()
            .is_some_and(|host| self.bypasses_proxy(host))
        {
            return None;
        }
        match target.scheme() {
            "http" => self.http_proxy.as_ref(),
            "https" => self.https_proxy.as_ref(),
            _ => None,
        }
    }

    pub fn is_direct(&self) -> bool {
        self.http_proxy.is_none() && self.https_proxy.is_none()
    }

    pub fn uses_https_proxy(&self) -> bool {
        [&self.http_proxy, &self.https_proxy]
            .into_iter()
            .flatten()
            .any(|proxy| proxy.scheme() == "https")
    }

    #[cfg(any(feature = "chrome-renderer", test))]
    pub(crate) fn chromium_proxy_server(&self) -> Option<String> {
        match (&self.http_proxy, &self.https_proxy) {
            (None, None) => None,
            (Some(http), Some(https)) if http == https => Some(proxy_uri(http)),
            (Some(http), Some(https)) => Some(format!(
                "http={};https={}",
                proxy_uri(http),
                proxy_uri(https)
            )),
            (Some(http), None) => Some(proxy_uri(http)),
            (None, Some(https)) => Some(proxy_uri(https)),
        }
    }

    #[cfg(feature = "chrome-renderer")]
    pub(crate) fn chromium_args(&self) -> Vec<String> {
        if self.is_direct() {
            return vec!["--no-proxy-server".to_string()];
        }

        let mut rules = String::from("MAP * ~NOTFOUND");
        for host in self.proxy_hosts() {
            rules.push_str(", EXCLUDE ");
            rules.push_str(&host);
        }
        let mut args = vec![format!("--host-resolver-rules={rules}")];
        if !self.bypass_hosts.is_empty() {
            let mut items = Vec::new();
            for entry in &self.bypass_hosts {
                if entry == "*" {
                    items.push("*".to_string());
                } else if let Some(rest) = entry.strip_prefix('.') {
                    items.push(format!("*.{rest}"));
                } else {
                    items.push(entry.clone());
                    items.push(format!("*.{entry}"));
                }
            }
            args.push(format!("--proxy-bypass-list={}", items.join(";")));
        }
        args
    }

    fn bypasses_proxy(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.bypass_hosts.iter().any(|entry| match entry.as_str() {
            "*" => true,
            entry if entry == host => true,
            entry if entry.starts_with('.') => host.ends_with(entry),
            entry => host
                .strip_suffix(entry)
                .is_some_and(|prefix| prefix.ends_with('.')),
        })
    }

    #[cfg(any(feature = "chrome-renderer", test))]
    pub(crate) fn proxy_hosts(&self) -> Vec<String> {
        let mut hosts = Vec::new();
        for proxy in [&self.http_proxy, &self.https_proxy].into_iter().flatten() {
            let Some(host) = proxy.host_str() else {
                continue;
            };
            push_unique(&mut hosts, host.to_string());
            if matches!(host, "localhost" | "127.0.0.1" | "::1") {
                push_unique(&mut hosts, "127.0.0.1".into());
                push_unique(&mut hosts, "localhost".into());
                push_unique(&mut hosts, "::1".into());
            }
        }
        hosts
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderRequestPolicy {
    PublicWeb,
    Unrestricted,
}

pub struct RenderRequest<'a> {
    pub url: &'a str,
    pub preflight_url: Option<&'a str>,
    pub ready_selector: &'a str,
    pub timeout: Duration,
    pub request_policy: RenderRequestPolicy,
}

pub struct RenderedPage {
    pub html: String,
    pub url: String,
    pub ready: bool,
}

#[async_trait::async_trait]
pub trait PageRenderer: Send + Sync {
    async fn render(&self, request: RenderRequest<'_>) -> eyre::Result<RenderedPage>;
}

pub trait PageRendererFactory: Send + Sync {
    fn build(&self, config: PageRendererConfig) -> Result<Arc<dyn PageRenderer>, String>;
}

pub fn is_public_web_request(value: &str) -> bool {
    crate::fetch::policy::is_public_browser_request(value)
}

pub fn default_page_renderer_factory() -> Arc<dyn PageRendererFactory> {
    #[cfg(feature = "chrome-renderer")]
    {
        Arc::new(crate::browser::ChromeRendererFactory)
    }
    #[cfg(not(feature = "chrome-renderer"))]
    {
        Arc::new(UnavailableRendererFactory)
    }
}

#[cfg(not(feature = "chrome-renderer"))]
struct UnavailableRendererFactory;

#[cfg(not(feature = "chrome-renderer"))]
impl PageRendererFactory for UnavailableRendererFactory {
    fn build(&self, _config: PageRendererConfig) -> Result<Arc<dyn PageRenderer>, String> {
        Ok(Arc::new(UnavailableRenderer))
    }
}

#[cfg(not(feature = "chrome-renderer"))]
struct UnavailableRenderer;

#[cfg(not(feature = "chrome-renderer"))]
#[async_trait::async_trait]
impl PageRenderer for UnavailableRenderer {
    async fn render(&self, _request: RenderRequest<'_>) -> eyre::Result<RenderedPage> {
        eyre::bail!("rendered extraction is unavailable in this runtime")
    }
}

#[cfg(any(feature = "chrome-renderer", test))]
fn proxy_uri(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

#[cfg(any(feature = "chrome-renderer", test))]
fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_proxy_by_scheme_and_honors_bypass_hosts() {
        let config = PageRendererConfig::new(
            Some(Url::parse("http://http-proxy.example:8080").unwrap()),
            Some(Url::parse("socks5://socks-proxy.example:1080").unwrap()),
            vec!["corp.example".into()],
        );

        assert_eq!(
            config
                .upstream_proxy_for(&Url::parse("http://example.com/").unwrap())
                .map(Url::as_str),
            Some("http://http-proxy.example:8080/")
        );
        assert_eq!(
            config
                .upstream_proxy_for(&Url::parse("https://example.com/").unwrap())
                .map(Url::as_str),
            Some("socks5://socks-proxy.example:1080")
        );
        assert!(config
            .upstream_proxy_for(&Url::parse("https://app.corp.example/").unwrap())
            .is_none());
    }
}
