use std::{net::IpAddr, str::FromStr, sync::Arc, time::Duration};

use url::Url;
use wreq_util::Emulation;

use crate::browser::BrowserRuntime;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Local Web Provider 对全部出站流量的单一选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundProxyMode {
    Direct,
    System,
    Explicit(String),
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LocalWebError(pub(crate) String);

impl LocalWebError {
    fn invalid_proxy(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Clone)]
pub struct LocalWeb {
    inner: Arc<LocalWebInner>,
}

struct LocalWebInner {
    snapshot: ResolvedProxy,
    http: wreq::Client,
    fetch_proxied: wreq::Client,
    browser: BrowserRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedProxy {
    pub http: Option<Url>,
    pub https: Option<Url>,
    pub no_proxy: NoProxyList,
}

impl ResolvedProxy {
    fn direct() -> Self {
        Self {
            http: None,
            https: None,
            no_proxy: NoProxyList::default(),
        }
    }

    fn is_direct(&self) -> bool {
        self.http.is_none() && self.https.is_none()
    }

    pub(crate) fn pins_origin(&self, url: &Url) -> bool {
        if self.is_direct() {
            return true;
        }
        url.host_str()
            .is_some_and(|host| self.no_proxy.contains(host))
    }

    fn chrome_proxy_server(&self) -> Option<String> {
        match (&self.http, &self.https) {
            (None, None) => None,
            (Some(http), Some(https)) if http == https => Some(chrome_proxy_uri(http)),
            (Some(http), Some(https)) => Some(format!(
                "http={};https={}",
                chrome_proxy_uri(http),
                chrome_proxy_uri(https)
            )),
            (Some(http), None) => Some(chrome_proxy_uri(http)),
            (None, Some(https)) => Some(chrome_proxy_uri(https)),
        }
    }

    fn chrome_exclude_hosts(&self) -> Vec<String> {
        let mut hosts = Vec::new();
        for proxy in [&self.http, &self.https].into_iter().flatten() {
            if let Some(host) = proxy.host_str() {
                push_unique(&mut hosts, host.to_string());
                if matches!(host, "localhost" | "127.0.0.1" | "::1") {
                    push_unique(&mut hosts, "127.0.0.1".into());
                    push_unique(&mut hosts, "localhost".into());
                    push_unique(&mut hosts, "::1".into());
                }
            }
        }
        hosts
    }

    fn chrome_bypass_list(&self) -> Option<String> {
        self.no_proxy.chrome_bypass_list()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NoProxyList {
    entries: Vec<String>,
}

impl NoProxyList {
    fn parse(value: &str) -> Self {
        Self {
            entries: value
                .split(|character| matches!(character, ',' | ' ' | ';'))
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| entry.to_ascii_lowercase())
                .collect(),
        }
    }

    pub(crate) fn contains(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.entries.iter().any(|entry| match entry.as_str() {
            "*" => true,
            entry if entry == host => true,
            entry if entry.starts_with('.') => host.ends_with(entry),
            entry => host == *entry || host.ends_with(&format!(".{entry}")),
        })
    }

    fn chrome_bypass_list(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut items = Vec::new();
        for entry in &self.entries {
            if entry == "*" {
                items.push("*".to_string());
                continue;
            }
            if let Some(rest) = entry.strip_prefix('.') {
                items.push(format!("*.{rest}"));
                continue;
            }
            items.push(entry.clone());
            items.push(format!("*.{entry}"));
        }
        Some(items.join(";"))
    }

    fn as_wreq(&self) -> Option<wreq::NoProxy> {
        if self.entries.is_empty() {
            return None;
        }
        wreq::NoProxy::from_string(&self.entries.join(","))
    }
}

impl LocalWeb {
    pub fn new(mode: OutboundProxyMode) -> Result<Self, LocalWebError> {
        Self::from_env(mode, |key| std::env::var(key).ok())
    }

    fn from_env(
        mode: OutboundProxyMode,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, LocalWebError> {
        let snapshot = resolve_mode(mode, env)?;
        let http = build_http_client(&snapshot, SEARCH_TIMEOUT, false, true)?;
        let fetch_proxied = build_http_client(&snapshot, FETCH_TIMEOUT, true, false)?;
        let browser = BrowserRuntime::new(chrome_launch_config(&snapshot));
        Ok(Self {
            inner: Arc::new(LocalWebInner {
                snapshot,
                http,
                fetch_proxied,
                browser,
            }),
        })
    }

    pub fn http_client(&self) -> wreq::Client {
        self.inner.http.clone()
    }

    pub fn search_query(&self, query: impl Into<String>) -> crate::search::engines::SearchQuery {
        crate::search::engines::SearchQuery {
            query: query.into(),
            allowed_domains: Vec::new(),
            request_headers: std::collections::HashMap::new(),
            ip: String::new(),
            config: std::sync::Arc::new(crate::search::config::Config::default()),
            http: self.http_client(),
            browser: self.browser(),
        }
    }

    pub(crate) fn browser(&self) -> BrowserRuntime {
        self.inner.browser.clone()
    }

    pub(crate) fn snapshot(&self) -> &ResolvedProxy {
        &self.inner.snapshot
    }

    pub(crate) fn fetch_proxied_client(&self) -> wreq::Client {
        self.inner.fetch_proxied.clone()
    }

    pub async fn fetch(
        &self,
        url: &str,
    ) -> Result<crate::fetch::FetchedPage, crate::fetch::FetchError> {
        crate::fetch::fetch_with_runtime(self, url).await
    }

    pub async fn search(
        &self,
        mut query: crate::search::engines::SearchQuery,
        progress_tx: tokio::sync::mpsc::UnboundedSender<crate::search::engines::ProgressUpdate>,
    ) -> eyre::Result<()> {
        query.http = self.http_client();
        query.browser = self.browser();
        crate::search::engines::search(&query, progress_tx).await
    }

    pub async fn autocomplete(
        &self,
        config: &crate::search::config::Config,
        query: &str,
    ) -> eyre::Result<Vec<String>> {
        crate::search::engines::autocomplete(config, query, &self.inner.http).await
    }
}

pub(crate) fn resolve_mode(
    mode: OutboundProxyMode,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedProxy, LocalWebError> {
    match mode {
        OutboundProxyMode::Direct => Ok(ResolvedProxy::direct()),
        OutboundProxyMode::Explicit(value) => {
            let proxy = parse_proxy_url(&value)?;
            Ok(ResolvedProxy {
                http: Some(proxy.clone()),
                https: Some(proxy),
                no_proxy: NoProxyList::default(),
            })
        }
        OutboundProxyMode::System => {
            let https = env_first(
                &env,
                &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"],
            )?;
            let http = env_first(
                &env,
                &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"],
            )?;
            let no_proxy = env_text(&env, &["NO_PROXY", "no_proxy"])
                .map(|value| NoProxyList::parse(&value))
                .unwrap_or_default();
            if http.is_none() && https.is_none() {
                return Ok(ResolvedProxy::direct());
            }
            Ok(ResolvedProxy {
                http,
                https,
                no_proxy,
            })
        }
    }
}

fn env_first(
    env: &impl Fn(&str) -> Option<String>,
    keys: &[&str],
) -> Result<Option<Url>, LocalWebError> {
    match env_text(env, keys) {
        Some(value) => Ok(Some(parse_proxy_url(&value)?)),
        None => Ok(None),
    }
}

fn env_text(env: &impl Fn(&str) -> Option<String>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = env(key) {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                continue;
            }
            return Some(trimmed.to_string());
        }
    }
    None
}

fn parse_proxy_url(value: &str) -> Result<Url, LocalWebError> {
    let url = Url::parse(value)
        .map_err(|_| LocalWebError::invalid_proxy(format!("proxy URL is invalid: {value}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(LocalWebError::invalid_proxy(
            "proxy URL must not include credentials",
        ));
    }
    match url.scheme() {
        "http" | "https" | "socks5" | "socks5h" => Ok(normalize_socks(url)),
        scheme => Err(LocalWebError::invalid_proxy(format!(
            "unsupported proxy scheme: {scheme}"
        ))),
    }
}

fn normalize_socks(mut url: Url) -> Url {
    if url.scheme() == "socks5h" {
        let _ = url.set_scheme("socks5");
    }
    url
}

fn chrome_proxy_uri(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

fn chrome_launch_config(snapshot: &ResolvedProxy) -> crate::browser::ChromeLaunchConfig {
    let mut args = Vec::new();
    let proxy_server = snapshot.chrome_proxy_server();
    if proxy_server.is_some() {
        let mut rules = String::from("MAP * ~NOTFOUND");
        for host in snapshot.chrome_exclude_hosts() {
            rules.push_str(", EXCLUDE ");
            rules.push_str(&host);
        }
        args.push(format!("--host-resolver-rules={rules}"));
        if let Some(bypass) = snapshot.chrome_bypass_list() {
            args.push(format!("--proxy-bypass-list={bypass}"));
        }
    } else {
        args.push("--no-proxy-server".to_string());
    }
    crate::browser::ChromeLaunchConfig { proxy_server, args }
}

fn build_http_client(
    snapshot: &ResolvedProxy,
    timeout: Duration,
    disable_redirects: bool,
    cookies: bool,
) -> Result<wreq::Client, LocalWebError> {
    let mut builder = wreq::Client::builder()
        .local_address(IpAddr::from_str("0.0.0.0").expect("IPv4 any address"))
        .emulation(Emulation::Firefox139)
        .timeout(timeout);
    // 搜狗微信解 /link 跟踪跳转时需要搜索页下发的 SNUID；Fetch 使用独立 client。
    if cookies {
        builder = builder.cookie_store(true);
    }
    if disable_redirects {
        builder = builder.redirect(wreq::redirect::Policy::none());
    }
    builder = apply_proxy(builder, snapshot)?;
    builder
        .build()
        .map_err(|error| LocalWebError::invalid_proxy(format!("HTTP client failed: {error}")))
}

fn apply_proxy(
    mut builder: wreq::ClientBuilder,
    snapshot: &ResolvedProxy,
) -> Result<wreq::ClientBuilder, LocalWebError> {
    match (&snapshot.http, &snapshot.https) {
        (None, None) => Ok(builder.no_proxy()),
        (Some(http), Some(https)) if http == https => {
            Ok(builder.proxy(wreq_proxy(wreq_all(http)?, snapshot)))
        }
        (http, https) => {
            if let Some(http) = http {
                builder = builder.proxy(wreq_proxy(wreq_http(http)?, snapshot));
            }
            if let Some(https) = https {
                builder = builder.proxy(wreq_proxy(wreq_https(https)?, snapshot));
            }
            Ok(builder)
        }
    }
}

fn wreq_proxy(proxy: wreq::Proxy, snapshot: &ResolvedProxy) -> wreq::Proxy {
    proxy.no_proxy(snapshot.no_proxy.as_wreq())
}

fn wreq_all(url: &Url) -> Result<wreq::Proxy, LocalWebError> {
    wreq::Proxy::all(url.as_str()).map_err(proxy_build_error)
}

fn wreq_http(url: &Url) -> Result<wreq::Proxy, LocalWebError> {
    wreq::Proxy::http(url.as_str()).map_err(proxy_build_error)
}

fn wreq_https(url: &Url) -> Result<wreq::Proxy, LocalWebError> {
    wreq::Proxy::https(url.as_str()).map_err(proxy_build_error)
}

fn proxy_build_error(error: wreq::Error) -> LocalWebError {
    LocalWebError::invalid_proxy(format!("proxy configuration failed: {error}"))
}

fn push_unique(items: &mut Vec<String>, value: String) {
    if !items.iter().any(|item| item == &value) {
        items.push(value);
    }
}

/// Shared by tests that only need a Direct HTTP client.
#[cfg(test)]
pub(crate) fn direct_http_client() -> wreq::Client {
    static CLIENT: std::sync::LazyLock<wreq::Client> = std::sync::LazyLock::new(|| {
        build_http_client(&ResolvedProxy::direct(), SEARCH_TIMEOUT, false, true)
            .expect("direct HTTP client")
    });
    CLIENT.clone()
}

#[cfg(test)]
pub(crate) fn direct_browser() -> BrowserRuntime {
    BrowserRuntime::new(chrome_launch_config(&ResolvedProxy::direct()))
}

pub fn parse_cli_proxy(value: &str) -> Result<OutboundProxyMode, LocalWebError> {
    match value {
        "direct" => Ok(OutboundProxyMode::Direct),
        "system" => Ok(OutboundProxyMode::System),
        other => Ok(OutboundProxyMode::Explicit(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn explicit_http_proxy_applies_to_both_schemes() {
        let snapshot = resolve_mode(
            OutboundProxyMode::Explicit("http://127.0.0.1:7890".into()),
            env(&[]),
        )
        .unwrap();
        assert_eq!(
            snapshot.chrome_proxy_server().as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert!(!snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
        assert!(snapshot
            .chrome_exclude_hosts()
            .contains(&"127.0.0.1".into()));
    }

    #[test]
    fn explicit_socks5h_is_rewritten_to_socks5() {
        let snapshot = resolve_mode(
            OutboundProxyMode::Explicit("socks5h://127.0.0.1:1080".into()),
            env(&[]),
        )
        .unwrap();
        assert_eq!(
            snapshot.chrome_proxy_server().as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
    }

    #[test]
    fn explicit_rejects_userinfo_and_socks4() {
        assert!(resolve_mode(
            OutboundProxyMode::Explicit("http://user:pass@127.0.0.1:7890".into()),
            env(&[]),
        )
        .is_err());
        assert!(resolve_mode(
            OutboundProxyMode::Explicit("socks4://127.0.0.1:1080".into()),
            env(&[]),
        )
        .is_err());
        assert!(resolve_mode(OutboundProxyMode::Explicit("not a url".into()), env(&[]),).is_err());
    }

    #[test]
    fn system_prefers_uppercase_and_scheme_specific_values() {
        let snapshot = resolve_mode(
            OutboundProxyMode::System,
            env(&[
                ("HTTPS_PROXY", "http://https-proxy:8080"),
                ("https_proxy", "http://ignored:1"),
                ("HTTP_PROXY", "http://http-proxy:8080"),
                ("NO_PROXY", "localhost,.corp.example"),
            ]),
        )
        .unwrap();
        assert_eq!(
            snapshot.https.as_ref().map(Url::as_str),
            Some("http://https-proxy:8080/")
        );
        assert_eq!(
            snapshot.http.as_ref().map(Url::as_str),
            Some("http://http-proxy:8080/")
        );
        assert_eq!(
            snapshot.chrome_proxy_server().as_deref(),
            Some("http=http://http-proxy:8080;https=http://https-proxy:8080")
        );
        assert!(snapshot.pins_origin(&Url::parse("https://app.corp.example/").unwrap()));
        assert!(!snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
    }

    #[test]
    fn empty_system_env_is_direct() {
        let snapshot = resolve_mode(OutboundProxyMode::System, env(&[])).unwrap();
        assert!(snapshot.is_direct());
        assert!(snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
        assert_eq!(snapshot.chrome_proxy_server(), None);
    }

    #[test]
    fn parse_cli_proxy_accepts_modes_and_urls() {
        assert_eq!(
            parse_cli_proxy("direct").unwrap(),
            OutboundProxyMode::Direct
        );
        assert_eq!(
            parse_cli_proxy("system").unwrap(),
            OutboundProxyMode::System
        );
        assert_eq!(
            parse_cli_proxy("http://127.0.0.1:7890").unwrap(),
            OutboundProxyMode::Explicit("http://127.0.0.1:7890".into())
        );
    }

    #[tokio::test]
    async fn search_client_resends_cookies_from_earlier_responses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};

                    let mut buf = vec![0; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let has_cookie = request.lines().any(|line| {
                        line.to_ascii_lowercase().starts_with("cookie:")
                            && line.contains("SNUID=test")
                    });
                    let extra_headers = if has_cookie {
                        ""
                    } else {
                        "Set-Cookie: SNUID=test; Path=/\r\n"
                    };
                    let body = if has_cookie {
                        "with-cookie"
                    } else {
                        "no-cookie"
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        let client = LocalWeb::new(OutboundProxyMode::Direct)
            .unwrap()
            .http_client();
        let url = format!("http://127.0.0.1:{}/search", addr.port());
        let first = client.get(&url).send().await.unwrap().text().await.unwrap();
        assert_eq!(first, "no-cookie");
        let second = client.get(&url).send().await.unwrap().text().await.unwrap();
        assert_eq!(second, "with-cookie");
    }
}
