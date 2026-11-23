mod extract;
pub(crate) mod policy;
pub mod resource;

use std::{future::Future, net::IpAddr, pin::Pin, time::Duration};

use serde::Serialize;
use url::{Host, Url};

use crate::address_policy::is_public_ip;
use crate::OutboundProxyMode;
use extract::ContentKind;

pub(crate) const DOWNLOAD_BYTE_CAP: usize = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const LOSSY_DECODE_LIMITATION: &str = "The source encoding could not be decoded reliably; the returned text may contain substitutions. Use :download for the original bytes.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchErrorCode {
    InvalidUrl,
    Unavailable,
    UnsupportedMediaType,
    ResponseTooLarge,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FetchError {
    code: FetchErrorCode,
    message: String,
}

impl FetchError {
    #[must_use]
    pub fn code(&self) -> FetchErrorCode {
        self.code
    }

    fn new(code: FetchErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_url(value: &str) -> Self {
        Self::new(
            FetchErrorCode::InvalidUrl,
            format!("URL must be public HTTP(S): {value}"),
        )
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::new(FetchErrorCode::Unavailable, message)
    }
}

/// Text produced by the pure in-memory read conversion pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadText {
    pub text: String,
    pub representation: String,
    pub title: Option<String>,
    pub limitations: Vec<String>,
    pub source_truncated: bool,
}

/// Converts already-downloaded bytes into readable text without any network,
/// script, or secondary resource access.
///
/// HTML uses Readability plus Markdown conversion, with absolute links only
/// when a trusted base URL is supplied. JSON, XML, and text are returned
/// verbatim. `raw` bypasses Readability, Markdown, and reformatting entirely
/// and strictly decodes the source bytes, failing on unknown or invalid
/// character sets instead of substituting replacement characters.
pub fn convert_read_bytes(
    body: &[u8],
    content_type: &str,
    base_url: Option<&Url>,
    raw: bool,
) -> Result<ReadText, FetchError> {
    if raw {
        let decoded = extract::decode_strict(body, content_type)?;
        return match extract::classify(content_type, &decoded) {
            ContentKind::Unsupported => Err(extract::unsupported(content_type)),
            ContentKind::Html
            | ContentKind::Markdown
            | ContentKind::Plain
            | ContentKind::Json
            | ContentKind::Xml => Ok(ReadText {
                text: decoded,
                representation: "raw".into(),
                title: None,
                limitations: Vec::new(),
                source_truncated: false,
            }),
        };
    }
    let decoded = extract::decode_lossy(body, content_type);
    let limitations = decoded
        .lossy
        .then(|| LOSSY_DECODE_LIMITATION.to_string())
        .into_iter()
        .collect::<Vec<_>>();
    match extract::classify(content_type, &decoded.text) {
        ContentKind::Html => {
            let extract = extract::extract_html(&decoded.text, base_url)?;
            Ok(ReadText {
                text: extract.markdown,
                representation: "markdown".into(),
                title: extract.title,
                limitations,
                source_truncated: false,
            })
        }
        ContentKind::Markdown => Ok(ReadText {
            text: decoded.text,
            representation: "markdown".into(),
            title: None,
            limitations,
            source_truncated: false,
        }),
        ContentKind::Plain | ContentKind::Json | ContentKind::Xml => Ok(ReadText {
            text: decoded.text,
            representation: "text".into(),
            title: None,
            limitations,
            source_truncated: false,
        }),
        ContentKind::Unsupported => Err(extract::unsupported(content_type)),
    }
}

/// Resolve an HTTP redirect chain using per-hop public URL safety checks without reading bodies.
pub async fn resolve_final_url(value: &str, mode: OutboundProxyMode) -> Result<Url, FetchError> {
    let backend = RedirectBackend::from_mode(mode)?;
    let url = policy::validate_url(value).map_err(sanitize_resolution_error)?;
    let (url, response) = get_redirects(url, &backend, MAX_REDIRECTS, true)
        .await
        .map_err(sanitize_resolution_error)?;
    if !(200..300).contains(&response.status) {
        return Err(FetchError::unavailable(format!(
            "HTTP request returned status {}",
            response.status
        )));
    }
    Ok(url)
}

fn sanitize_resolution_error(error: FetchError) -> FetchError {
    let message = match error.message.as_str() {
        "HTTP redirect omitted Location" => "HTTP redirect omitted Location",
        "HTTP redirect limit exceeded" => "HTTP redirect limit exceeded",
        _ if error.code == FetchErrorCode::InvalidUrl => "Redirect URL is not public HTTP(S)",
        _ => "HTTP redirect resolution failed",
    };
    FetchError::new(error.code, message)
}

type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

trait HttpBackend: Sync {
    fn pins_origin(&self, _url: &Url) -> bool {
        true
    }

    fn resolve<'a>(&'a self, url: &'a Url) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>>;

    fn get<'a>(
        &'a self,
        url: &'a Url,
        addresses: &'a [IpAddr],
    ) -> BackendFuture<'a, Result<HttpResponse, FetchError>>;

    fn get_headers<'a>(
        &'a self,
        url: &'a Url,
        addresses: &'a [IpAddr],
    ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
        self.get(url, addresses)
    }
}

struct HttpResponse {
    status: u16,
    content_type: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
}

async fn get_redirects(
    mut url: Url,
    http: &impl HttpBackend,
    redirect_limit: usize,
    headers_only: bool,
) -> Result<(Url, HttpResponse), FetchError> {
    for redirect_count in 0..=redirect_limit {
        policy::validate_parsed_url(&url)?;
        let addresses = if http.pins_origin(&url) {
            resolve_public_addresses(&url, http).await?
        } else {
            Vec::new()
        };
        let response = if headers_only {
            http.get_headers(&url, &addresses).await?
        } else {
            http.get(&url, &addresses).await?
        };
        if !(300..400).contains(&response.status) {
            return Ok((url, response));
        }
        if redirect_count == redirect_limit {
            return Err(FetchError::unavailable("HTTP redirect limit exceeded"));
        }
        let location = response
            .location
            .as_deref()
            .ok_or_else(|| FetchError::unavailable("HTTP redirect omitted Location"))?;
        url = url
            .join(location)
            .map_err(|_| FetchError::invalid_url(location))?;
        policy::validate_parsed_url(&url)?;
    }
    unreachable!("redirect loop returns within its bound")
}

async fn resolve_public_addresses(
    url: &Url,
    http: &impl HttpBackend,
) -> Result<Vec<IpAddr>, FetchError> {
    let addresses = http.resolve(url).await?;
    if addresses.is_empty() {
        return Err(FetchError::unavailable(format!(
            "URL hostname had no DNS answers: {}",
            url.host_str().unwrap_or_default()
        )));
    }
    if addresses.iter().any(|address| !is_public_ip(*address)) {
        return Err(FetchError::invalid_url(url.as_str()));
    }
    Ok(addresses)
}

struct RedirectBackend {
    proxied: Option<(reqwest::Client, NoProxyList)>,
}

impl RedirectBackend {
    fn from_mode(mode: OutboundProxyMode) -> Result<Self, FetchError> {
        match mode {
            OutboundProxyMode::Direct => Ok(Self { proxied: None }),
            OutboundProxyMode::Explicit { url, bypass } => {
                let normalized = normalize_proxy_url(&url)?;
                let proxy = reqwest::Proxy::all(normalized.as_str())
                    .map_err(|_| FetchError::unavailable("Invalid outbound proxy configuration"))?;
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .proxy(proxy)
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(HTTP_TIMEOUT)
                    .build()
                    .map_err(|_| FetchError::unavailable("HTTP client initialization failed"))?;
                Ok(Self {
                    proxied: Some((client, NoProxyList::parse(&bypass))),
                })
            }
            OutboundProxyMode::System => {
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(HTTP_TIMEOUT)
                    .build()
                    .map_err(|_| FetchError::unavailable("HTTP client initialization failed"))?;
                Ok(Self {
                    proxied: Some((client, NoProxyList::default())),
                })
            }
        }
    }
}

fn normalize_proxy_url(raw: &str) -> Result<Url, FetchError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(FetchError::unavailable(
            "Invalid outbound proxy configuration",
        ));
    }
    let mut parsed = Url::parse(trimmed)
        .map_err(|_| FetchError::unavailable("Invalid outbound proxy configuration"))?;
    match parsed.scheme() {
        "http" | "https" | "socks5h" => {}
        "socks5" => {
            // 与既有出站代理语义一致，socks5 交给代理侧解析域名。
            let _ = parsed.set_scheme("socks5h");
        }
        _ => {
            return Err(FetchError::unavailable(
                "Invalid outbound proxy configuration",
            ));
        }
    }
    if parsed.host_str().is_none() {
        return Err(FetchError::unavailable(
            "Invalid outbound proxy configuration",
        ));
    }
    Ok(parsed)
}

impl HttpBackend for RedirectBackend {
    fn pins_origin(&self, url: &Url) -> bool {
        match &self.proxied {
            None => true,
            Some((_, no_proxy)) => url.host_str().is_some_and(|host| no_proxy.contains(host)),
        }
    }

    fn resolve<'a>(&'a self, url: &'a Url) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
        Box::pin(async move {
            match url.host() {
                Some(Host::Ipv4(address)) => return Ok(vec![IpAddr::V4(address)]),
                Some(Host::Ipv6(address)) => return Ok(vec![IpAddr::V6(address)]),
                _ => {}
            }
            let host = url
                .host_str()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?;
            let port = url
                .port_or_known_default()
                .ok_or_else(|| FetchError::invalid_url(url.as_str()))?;
            let addresses = tokio::net::lookup_host((host, port))
                .await
                .map_err(|error| {
                    FetchError::unavailable(format!(
                        "URL hostname could not be resolved: {host}: {error}"
                    ))
                })?
                .map(|address| address.ip())
                .collect::<Vec<_>>();
            Ok(addresses)
        })
    }

    fn get<'a>(
        &'a self,
        url: &'a Url,
        addresses: &'a [IpAddr],
    ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
        Box::pin(async move {
            let pinned = self.pins_origin(url);
            let client = if pinned {
                let host = url
                    .host_str()
                    .ok_or_else(|| FetchError::invalid_url(url.as_str()))?
                    .to_owned();
                let port = url
                    .port_or_known_default()
                    .ok_or_else(|| FetchError::invalid_url(url.as_str()))?;
                let addrs = addresses
                    .iter()
                    .map(|address| std::net::SocketAddr::new(*address, port))
                    .collect::<Vec<_>>();
                reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(HTTP_TIMEOUT)
                    .resolve_to_addrs(&host, &addrs)
                    .build()
                    .map_err(|error| {
                        FetchError::unavailable(format!("HTTP client failed: {error}"))
                    })?
            } else {
                self.proxied
                    .as_ref()
                    .expect("proxied client present when origin is not pinned")
                    .0
                    .clone()
            };
            let response = client.get(url.as_str()).send().await.map_err(|error| {
                FetchError::unavailable(format!("HTTP request failed: {error}"))
            })?;
            if pinned {
                let connected = response.remote_addr().ok_or_else(|| {
                    FetchError::unavailable("HTTP connection omitted its remote address")
                })?;
                if !is_public_ip(connected.ip())
                    || !addresses.iter().any(|address| *address == connected.ip())
                {
                    return Err(FetchError::invalid_url(url.as_str()));
                }
            }
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            Ok(HttpResponse {
                status: response.status().as_u16(),
                content_type: header("content-type"),
                location: header("location"),
                body: Vec::new(),
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct NoProxyList {
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

    fn contains(&self, host: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        net::{IpAddr, Ipv4Addr},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use parking_lot::Mutex;

    use super::*;

    pub(super) struct StubBackend {
        pub(super) responses: Mutex<VecDeque<HttpResponse>>,
        pub(crate) requests: AtomicUsize,
        pub(crate) requested_urls: Mutex<Vec<String>>,
    }

    impl StubBackend {
        pub(crate) fn response(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from([HttpResponse {
                    status: 200,
                    content_type: Some(content_type.into()),
                    location: None,
                    body: body.into(),
                }])),
                requests: AtomicUsize::new(0),
                requested_urls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Default for StubBackend {
        fn default() -> Self {
            Self::response("text/plain", "unused")
        }
    }

    impl HttpBackend for StubBackend {
        fn resolve<'a>(
            &'a self,
            _url: &'a Url,
        ) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
            Box::pin(async { Ok(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]) })
        }

        fn get<'a>(
            &'a self,
            url: &'a Url,
            _addresses: &'a [IpAddr],
        ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.requested_urls.lock().push(url.as_str().to_string());
            Box::pin(async move {
                self.responses
                    .lock()
                    .pop_front()
                    .ok_or_else(|| FetchError::unavailable("stub response exhausted"))
            })
        }
    }

    #[tokio::test]
    async fn headers_only_resolution_follows_relative_multihop_and_rejects_unsafe_targets() {
        let backend = StubBackend::default();
        let response = |status, location: Option<&str>| HttpResponse {
            status,
            content_type: None,
            location: location.map(str::to_owned),
            body: Vec::new(),
        };
        *backend.responses.lock() = VecDeque::from([
            response(302, Some("/next")),
            response(301, Some("https://publisher.example/final")),
            response(200, None),
        ]);
        let (url, _) = get_redirects(
            Url::parse("https://example.com/start").unwrap(),
            &backend,
            MAX_REDIRECTS,
            true,
        )
        .await
        .unwrap();
        assert_eq!(url.as_str(), "https://publisher.example/final");
        assert_eq!(
            *backend.requested_urls.lock(),
            [
                "https://example.com/start",
                "https://example.com/next",
                "https://publisher.example/final"
            ]
        );

        let backend = StubBackend::default();
        *backend.responses.lock() =
            VecDeque::from([response(302, Some("http://10.0.0.1/private?secret=value"))]);
        let error = get_redirects(
            Url::parse("https://example.com/start").unwrap(),
            &backend,
            MAX_REDIRECTS,
            true,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.code(), FetchErrorCode::InvalidUrl);
        assert_eq!(backend.requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn headers_only_resolution_fails_on_missing_location_and_redirect_loop() {
        let backend = StubBackend::default();
        let redirect = || HttpResponse {
            status: 302,
            content_type: None,
            location: Some("/loop".into()),
            body: Vec::new(),
        };
        *backend.responses.lock() = (0..=MAX_REDIRECTS).map(|_| redirect()).collect();
        let error = get_redirects(
            Url::parse("https://example.com/loop").unwrap(),
            &backend,
            MAX_REDIRECTS,
            true,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "HTTP redirect limit exceeded");
        assert_eq!(backend.requests.load(Ordering::Relaxed), MAX_REDIRECTS + 1);

        let backend = StubBackend::default();
        *backend.responses.lock() = VecDeque::from([HttpResponse {
            location: None,
            ..redirect()
        }]);
        let error = get_redirects(
            Url::parse("https://example.com/").unwrap(),
            &backend,
            MAX_REDIRECTS,
            true,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "HTTP redirect omitted Location");
    }

    #[test]
    fn reads_declared_charsets_without_a_bom() {
        for charset in ["iso-8859-1", "latin1", "latin-1"] {
            let content_type = format!("text/plain; charset={charset}");
            let raw = convert_read_bytes(b"Caf\xe9", &content_type, None, true)
                .unwrap_or_else(|error| panic!("{charset}: {error}"));
            assert_eq!(raw.text, "Café", "{charset}");
        }

        let raw = convert_read_bytes(
            b"\x80 and \x93quotes\x94",
            "text/plain; charset=windows-1252",
            None,
            true,
        )
        .expect("windows-1252 strict decode");
        assert!(raw.text.starts_with('€'));
        assert!(raw.text.contains('“'));

        let readable = convert_read_bytes(b"\x80", "text/plain; charset=cp1252", None, false)
            .expect("cp1252 alias decodes");
        assert_eq!(readable.text, "€");
    }

    #[test]
    fn converts_html_to_markdown_with_meta_charset_and_absolute_links() {
        let mut html = b"<html><head><meta charset=\"windows-1252\"><title>Caf\xe9 menu</title></head><body><nav>Nav</nav><article><h1>Caf\xe9 menu</h1>".to_vec();
        for index in 0..6 {
            html.extend_from_slice(b"<p>Caf");
            html.push(0xE9);
            html.extend_from_slice(
                format!(
                    " paragraph {index} keeps enough prose for the extractor to keep this article content.</p>"
                )
                .as_bytes(),
            );
        }
        html.extend_from_slice(b"<a href='/order'>Order</a></article></body></html>");
        let base = Url::parse("https://example.com/cafe").expect("base URL parses");

        let readable = convert_read_bytes(&html, "text/html", Some(&base), false)
            .expect("windows-1252 HTML converts");
        assert_eq!(readable.title.as_deref(), Some("Café menu"));
        assert_eq!(readable.representation, "markdown");
        assert!(readable.text.contains("Café paragraph"));
        assert!(readable.text.contains("https://example.com/order"));
        assert!(readable.limitations.is_empty());
    }

    #[test]
    fn raw_and_lossy_modes_preserve_read_semantics() {
        let html = "<html><head><title>Raw doc</title></head><body><article><p>Body</p></article></body></html>\n";
        let raw = convert_read_bytes(html.as_bytes(), "text/html; charset=utf-8", None, true)
            .expect("raw HTML strict decode");
        assert_eq!(raw.text, html);
        assert_eq!(raw.representation, "raw");
        assert_eq!(raw.title, None);

        let bytes = b"valid start then \xc3\x28 invalid tail";
        let error = convert_read_bytes(bytes, "text/plain", None, true)
            .expect_err("raw must reject invalid utf-8");
        assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);
        let readable = convert_read_bytes(bytes, "text/plain", None, false)
            .expect("default mode degrades lossily");
        assert!(!readable.limitations.is_empty());

        for content_type in ["image/png", "image/jpeg", "application/pdf"] {
            for raw in [false, true] {
                let error = convert_read_bytes(b"pretend bytes", content_type, None, raw)
                    .expect_err("binary must be rejected");
                assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);
            }
        }
    }
}
