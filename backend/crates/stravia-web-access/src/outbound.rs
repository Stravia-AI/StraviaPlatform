use std::{net::IpAddr, sync::Arc, time::Duration};

use url::Url;

use crate::browser::BrowserRuntime;
use crate::http_client::HttpClient;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Local Web Provider 对全部出站流量的单一选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundProxyMode {
    Direct,
    System,
    Explicit { url: String, bypass: String },
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
    http: HttpClient,
    fetch_proxied: HttpClient,
    browser: BrowserRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedProxy {
    pub http: Option<Url>,
    pub https: Option<Url>,
    pub no_proxy: NoProxyList,
}

impl ResolvedProxy {
    pub(crate) fn direct() -> Self {
        Self {
            http: None,
            https: None,
            no_proxy: NoProxyList::default(),
        }
    }

    pub(crate) fn pins_origin(&self, url: &Url) -> bool {
        if (if url.scheme() == "https" {
            &self.https
        } else {
            &self.http
        })
        .is_none()
        {
            return true;
        }
        url.host_str()
            .is_some_and(|host| self.no_proxy.contains(host))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NoProxyList {
    entries: Vec<NoProxyEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NoProxyEntry {
    Address(IpAddr),
    Network { address: IpAddr, prefix: u8 },
    Domain(String),
}

impl NoProxyList {
    fn parse(value: &str) -> Self {
        Self {
            entries: value
                .split(|separator: char| {
                    separator == ',' || separator == ';' || separator.is_ascii_whitespace()
                })
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| {
                    parse_ip_network(entry)
                        .map(|(address, prefix)| NoProxyEntry::Network { address, prefix })
                        .or_else(|| entry.parse::<IpAddr>().ok().map(NoProxyEntry::Address))
                        .unwrap_or_else(|| NoProxyEntry::Domain(entry.to_owned()))
                })
                .collect(),
        }
    }

    pub(crate) fn contains(&self, host: &str) -> bool {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host)
            .trim_end_matches('.');
        if let Ok(address) = host.parse::<IpAddr>() {
            return self.entries.iter().any(|entry| match entry {
                NoProxyEntry::Address(expected) => *expected == address,
                NoProxyEntry::Network {
                    address: base,
                    prefix,
                } => network_contains(*base, *prefix, address),
                NoProxyEntry::Domain(domain) => domain == "*",
            });
        }
        self.entries.iter().any(|entry| match entry {
            NoProxyEntry::Domain(domain) => domain_matches(domain, host),
            NoProxyEntry::Address(_) | NoProxyEntry::Network { .. } => false,
        })
    }
}

fn parse_ip_network(value: &str) -> Option<(IpAddr, u8)> {
    let (address, prefix) = value.split_once('/')?;
    let address = address.parse::<IpAddr>().ok()?;
    if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let prefix = prefix.parse::<u8>().ok()?;
    let bits = match address {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    (prefix <= bits).then_some((address, prefix))
}

fn network_contains(base: IpAddr, prefix: u8, address: IpAddr) -> bool {
    match (base, address) {
        (IpAddr::V4(base), IpAddr::V4(address)) => {
            prefix == 0 || u32::from(base) >> (32 - prefix) == u32::from(address) >> (32 - prefix)
        }
        (IpAddr::V6(base), IpAddr::V6(address)) => {
            prefix == 0
                || u128::from(base) >> (128 - prefix) == u128::from(address) >> (128 - prefix)
        }
        _ => false,
    }
}

fn domain_matches(entry: &str, domain: &str) -> bool {
    if entry == "*" || entry.eq_ignore_ascii_case(domain) {
        return true;
    }
    if entry
        .strip_prefix('.')
        .is_some_and(|root| root.eq_ignore_ascii_case(domain))
    {
        return true;
    }
    if domain.len() <= entry.len()
        || !domain
            .get(domain.len().saturating_sub(entry.len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(entry))
    {
        return false;
    }
    if entry.starts_with('.') {
        return true;
    }
    domain.as_bytes().get(domain.len() - entry.len() - 1) == Some(&b'.')
}

impl LocalWeb {
    pub fn new(mode: OutboundProxyMode) -> Result<Self, LocalWebError> {
        Self::with_browser_path(mode, None)
    }

    /// 创建固定出站代理与浏览器路径快照的运行时；`None` 使用环境变量或本机检测。
    /// 此处只校验代理配置；每次本地搜索或抓取前校验浏览器路径，不启动浏览器。
    pub fn with_browser_path(
        mode: OutboundProxyMode,
        browser_path: Option<std::path::PathBuf>,
    ) -> Result<Self, LocalWebError> {
        let snapshot = resolve_mode(mode, |key| std::env::var(key).ok())?;
        let http = build_http_client(&snapshot, SEARCH_TIMEOUT, true)?;
        let fetch_proxied = build_http_client(&snapshot, FETCH_TIMEOUT, false)?;
        let browser = BrowserRuntime::new(crate::browser::ChromeLaunchConfig {
            proxy: snapshot.clone(),
            browser_path,
        });
        Ok(Self {
            inner: Arc::new(LocalWebInner {
                snapshot,
                http,
                fetch_proxied,
                browser,
            }),
        })
    }

    /// 返回共享搜索 Cookie 与构造期出站快照的 wreq HTTP 客户端克隆。
    pub fn http_client(&self) -> HttpClient {
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

    pub(crate) fn fetch_proxied_client(&self) -> HttpClient {
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
    ) -> anyhow::Result<()> {
        self.inner.browser.require_available().await?;
        query.http = self.http_client();
        query.browser = self.browser();
        crate::search::engines::search(&query, progress_tx).await
    }

    pub async fn autocomplete(
        &self,
        config: &crate::search::config::Config,
        query: &str,
    ) -> anyhow::Result<Vec<String>> {
        self.inner.browser.require_available().await?;
        crate::search::engines::autocomplete(config, query, &self.inner.http).await
    }
}

pub(crate) fn resolve_mode(
    mode: OutboundProxyMode,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedProxy, LocalWebError> {
    match mode {
        OutboundProxyMode::Direct => Ok(ResolvedProxy::direct()),
        OutboundProxyMode::Explicit { url, bypass } => {
            let proxy = parse_proxy_url(&url)?;
            Ok(ResolvedProxy {
                http: Some(proxy.clone()),
                https: Some(proxy),
                no_proxy: NoProxyList::parse(&bypass),
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
    let url =
        Url::parse(value).map_err(|_| LocalWebError::invalid_proxy("proxy URL is invalid"))?;
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
    // LocalWeb 的安全策略刻意把 socks5 归一为 socks5h：目标域名始终交给代理解析，
    // 不执行本地 DNS，也绝不放宽出站目的地校验；核心 reqwest 客户端保留 5/5h 语义。
    if url.scheme() == "socks5" {
        let _ = url.set_scheme("socks5h");
    }
    url
}

fn build_http_client(
    snapshot: &ResolvedProxy,
    timeout: Duration,
    cookies: bool,
) -> Result<HttpClient, LocalWebError> {
    // 搜索共享 Cookie；Fetch 使用独立、禁用 Cookie 且限制正文大小的客户端。
    HttpClient::new(
        snapshot.clone(),
        timeout,
        cookies,
        (!cookies).then_some(crate::fetch::DOWNLOAD_BYTE_CAP),
    )
    .map_err(|error| LocalWebError::invalid_proxy(format!("HTTP client failed: {error}")))
}

/// Shared by tests that only need a Direct HTTP client.
#[cfg(test)]
pub(crate) fn direct_http_client() -> HttpClient {
    static CLIENT: std::sync::LazyLock<HttpClient> = std::sync::LazyLock::new(|| {
        build_http_client(&ResolvedProxy::direct(), SEARCH_TIMEOUT, true)
            .expect("direct HTTP client")
    });
    CLIENT.clone()
}

#[cfg(test)]
pub(crate) fn direct_browser() -> BrowserRuntime {
    BrowserRuntime::new(crate::browser::ChromeLaunchConfig {
        proxy: ResolvedProxy::direct(),
        browser_path: None,
    })
}

pub fn parse_cli_proxy(value: &str) -> Result<OutboundProxyMode, LocalWebError> {
    match value {
        "direct" => Ok(OutboundProxyMode::Direct),
        "system" => Ok(OutboundProxyMode::System),
        other => Ok(OutboundProxyMode::Explicit {
            url: other.to_string(),
            bypass: String::new(),
        }),
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

    fn explicit(url: impl Into<String>, bypass: impl Into<String>) -> OutboundProxyMode {
        OutboundProxyMode::Explicit {
            url: url.into(),
            bypass: bypass.into(),
        }
    }

    #[test]
    fn explicit_http_proxy_applies_to_both_schemes() {
        let snapshot = resolve_mode(explicit("http://127.0.0.1:7890", ""), env(&[])).unwrap();
        assert!(!snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
        assert!(!snapshot.pins_origin(&Url::parse("http://example.com/").unwrap()));
    }

    #[test]
    fn explicit_rejects_userinfo_and_socks4() {
        let invalid = resolve_mode(explicit("http://user:pass@127.0.0.1:7890", ""), env(&[]))
            .unwrap_err()
            .to_string();
        assert_eq!(invalid, "proxy URL must not include credentials");
        let malformed = resolve_mode(explicit("http://user:pass@", ""), env(&[]))
            .unwrap_err()
            .to_string();
        assert_eq!(malformed, "proxy URL is invalid");
        assert!(!malformed.contains("user"));
        let unsupported = resolve_mode(explicit("socks4://127.0.0.1:1080", ""), env(&[]))
            .unwrap_err()
            .to_string();
        assert_eq!(unsupported, "unsupported proxy scheme: socks4");
    }

    #[test]
    fn explicit_bypass_matches_domains_ips_and_networks() {
        let snapshot = resolve_mode(
            explicit(
                "http://127.0.0.1:7890",
                "EXAMPLE.com;.Internal\t192.0.2.0/24 2001:DB8::/32",
            ),
            env(&[]),
        )
        .unwrap();
        for url in [
            "http://example.com/",
            "https://WWW.EXAMPLE.COM./",
            "http://service.internal/",
            "http://192.0.2.44/",
            "http://[2001:db8::44]/",
        ] {
            assert!(snapshot.pins_origin(&Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "http://notexample.com/",
            "http://internal.example.net/",
            "http://192.0.3.44/",
            "http://[2001:db9::44]/",
        ] {
            assert!(!snapshot.pins_origin(&Url::parse(url).unwrap()), "{url}");
        }

        let wildcard = resolve_mode(explicit("http://127.0.0.1:7890", " * "), env(&[])).unwrap();
        assert!(wildcard.pins_origin(&Url::parse("https://example.com/").unwrap()));
        assert!(wildcard.pins_origin(&Url::parse("https://192.0.2.1/").unwrap()));
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
        assert!(snapshot.pins_origin(&Url::parse("https://app.corp.example/").unwrap()));
        assert!(!snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
        assert!(!snapshot.pins_origin(&Url::parse("http://example.com/").unwrap()));
    }

    #[test]
    fn missing_scheme_proxy_requires_direct_origin_pinning() {
        let snapshot = resolve_mode(
            OutboundProxyMode::System,
            env(&[("HTTPS_PROXY", "http://https-proxy:8080")]),
        )
        .unwrap();
        assert!(snapshot.pins_origin(&Url::parse("http://example.com/").unwrap()));
        assert!(!snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
    }

    #[tokio::test]
    async fn removed_browser_rejects_local_execution_before_network() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chrome.exe");
        std::fs::write(&path, b"metadata fixture; never execute").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let web = LocalWeb::with_browser_path(
            explicit(format!("http://{}", listener.local_addr().unwrap()), ""),
            Some(path.clone()),
        )
        .unwrap();
        let adapter = crate::local::build_local_adapter(
            "local".into(),
            explicit(format!("http://{}", listener.local_addr().unwrap()), ""),
            [(
                "google".into(),
                crate::local::LocalSearchEngineSetting { enabled: true },
            )]
            .into_iter()
            .collect(),
            Some(path.clone()),
        )
        .unwrap();
        std::fs::remove_file(path).unwrap();
        let search = crate::SearchRequest {
            query: "Stravia".into(),
            max_results: 1,
            allowed_domains: vec![],
        };
        assert!(adapter.search(&search).await.is_err());
        let fetch = adapter
            .fetch(&crate::FetchRequest {
                urls: vec!["https://example.com/".into()],
                max_characters: 100,
            })
            .await
            .unwrap();
        assert_eq!(fetch.result[0].status, crate::FetchStatus::Error);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(web.search(web.search_query("Stravia"), tx).await.is_err());
        assert_eq!(
            web.fetch("https://example.com/").await.unwrap_err().code(),
            crate::fetch::FetchErrorCode::Unavailable
        );
        assert!(web
            .autocomplete(&crate::search::config::Config::default(), "Stravia")
            .await
            .is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn empty_system_env_is_direct() {
        let snapshot = resolve_mode(OutboundProxyMode::System, env(&[])).unwrap();
        assert!(snapshot.pins_origin(&Url::parse("http://example.com/").unwrap()));
        assert!(snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
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
            explicit("http://127.0.0.1:7890", "")
        );
    }

    #[tokio::test]
    async fn socks_proxy_receives_the_origin_hostname_without_local_dns() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 2];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0; greeting[1] as usize];
            stream.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&0));
            stream.write_all(&[5, 0]).await.unwrap();
            let mut connect = [0; 5];
            stream.read_exact(&mut connect).await.unwrap();
            assert_eq!(&connect[..4], &[5, 1, 0, 3]);
            let mut hostname = vec![0; connect[4] as usize];
            stream.read_exact(&mut hostname).await.unwrap();
            assert_eq!(hostname, b"stravia-origin.invalid");
            let mut port = [0; 2];
            stream.read_exact(&mut port).await.unwrap();
            assert_eq!(u16::from_be_bytes(port), 80);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nremote-dns",
                )
                .await
                .unwrap();
        });
        let web = LocalWeb::new(explicit(format!("socks5://{addr}"), "")).unwrap();
        let request = wreq::Request::new(
            wreq::Method::GET,
            "http://stravia-origin.invalid/".parse().unwrap(),
        );
        let response = web.http_client().fetch(request).await.unwrap();
        assert_eq!(response.1, b"remote-dns");
        server.await.unwrap();
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
        let first = client
            .fetch(wreq::Request::new(wreq::Method::GET, url.parse().unwrap()))
            .await
            .unwrap();
        assert_eq!(first.1, b"no-cookie");
        let second = client
            .fetch(wreq::Request::new(wreq::Method::GET, url.parse().unwrap()))
            .await
            .unwrap();
        assert_eq!(second.1, b"with-cookie");
    }
}
