use std::{path::PathBuf, sync::Arc, time::Duration};

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
    http: HttpClient,
    fetch_proxied: HttpClient,
    browser: BrowserRuntime,
    fetch_browser: BrowserRuntime,
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
}

impl LocalWeb {
    /// 创建固定出站代理快照的内嵌 Web Access 运行时。
    pub fn new(mode: OutboundProxyMode) -> Result<Self, LocalWebError> {
        Self::build(mode, None)
    }

    /// 在应用专用目录持久化搜索身份；Fetch 始终保持隔离。
    pub fn with_profile(
        mode: OutboundProxyMode,
        profile_dir: PathBuf,
    ) -> Result<Self, LocalWebError> {
        Self::build(mode, Some(profile_dir))
    }

    fn build(mode: OutboundProxyMode, profile_dir: Option<PathBuf>) -> Result<Self, LocalWebError> {
        let snapshot = resolve_mode(mode, |key| std::env::var(key).ok())?;
        let http = match &profile_dir {
            Some(path) => HttpClient::with_cookie_cache(
                snapshot.clone(),
                SEARCH_TIMEOUT,
                path.with_file_name("search-cookies.json"),
            )
            .map_err(|error| LocalWebError(format!("search Cookie store failed: {error}")))?,
            None => build_http_client(&snapshot, SEARCH_TIMEOUT, true)?,
        };
        let fetch_proxied = build_http_client(&snapshot, FETCH_TIMEOUT, false)?;
        let browser = BrowserRuntime::new(crate::browser::BrowserLaunchConfig {
            proxy: snapshot.clone(),
            profile_dir,
        });
        let fetch_browser = BrowserRuntime::new(crate::browser::BrowserLaunchConfig {
            proxy: snapshot.clone(),
            profile_dir: None,
        });
        Ok(Self {
            inner: Arc::new(LocalWebInner {
                snapshot,
                http,
                fetch_proxied,
                browser,
                fetch_browser,
            }),
        })
    }

    /// 返回共享搜索 Cookie 与构造期出站快照的 Moli HTTP 客户端克隆。
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

    pub(crate) fn fetch_browser(&self) -> BrowserRuntime {
        self.inner.fetch_browser.clone()
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
        query.http = self.http_client();
        query.browser = self.browser();
        crate::search::engines::search(&query, progress_tx).await
    }

    pub async fn autocomplete(
        &self,
        config: &crate::search::config::Config,
        query: &str,
    ) -> anyhow::Result<Vec<String>> {
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
    // SOCKS5 的默认模式 会本机解析；统一远端 DNS，保持 Local Web 的代理出站约束。
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
    BrowserRuntime::new(crate::browser::BrowserLaunchConfig {
        proxy: ResolvedProxy::direct(),
        profile_dir: None,
    })
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
        assert!(!snapshot.pins_origin(&Url::parse("https://example.com/").unwrap()));
        assert!(!snapshot.pins_origin(&Url::parse("http://example.com/").unwrap()));
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
            OutboundProxyMode::Explicit("http://127.0.0.1:7890".into())
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
        let web = LocalWeb::new(OutboundProxyMode::Explicit(format!("socks5://{addr}"))).unwrap();
        let request = http::Request::get("http://stravia-origin.invalid/")
            .body(Vec::new())
            .unwrap();
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
            .fetch(http::Request::get(&url).body(Vec::new()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.1, b"no-cookie");
        let second = client
            .fetch(http::Request::get(&url).body(Vec::new()).unwrap())
            .await
            .unwrap();
        assert_eq!(second.1, b"with-cookie");
    }
}
