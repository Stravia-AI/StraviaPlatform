mod extract;
mod http;
pub(crate) mod policy;
pub mod resource;

use std::{future::Future, net::IpAddr, pin::Pin, time::Duration};

use serde::Serialize;
use url::Url;

use crate::browser::{BrowserRuntime, RenderRequest};
use crate::outbound::LocalWeb;
use extract::{ContentKind, HtmlExtract};
use http::NetworkBackend;

const MARKDOWN_CHARACTER_CAP: usize = 500_000;
pub(crate) const DOWNLOAD_BYTE_CAP: usize = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
const RENDER_TIMEOUT: Duration = Duration::from_secs(15);
const LOW_QUALITY_LIMITATION: &str =
    "The extracted content may be a page shell, login wall, or challenge page.";
const LOSSY_DECODE_LIMITATION: &str = "The source encoding could not be decoded reliably; the returned text may contain substitutions. Use download=1 for the original bytes.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionPath {
    Static,
    Rendered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FetchedPage {
    pub requested_url: String,
    pub final_url: String,
    pub title: Option<String>,
    pub markdown: String,
    pub extraction_path: ExtractionPath,
    pub limitations: Vec<String>,
    pub truncated: bool,
}

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
/// HTML uses the same Readability plus Markdown pipeline as the provider fetch
/// path, with absolute links only when a trusted base URL is supplied. JSON,
/// XML, and text are returned verbatim. `raw` bypasses Readability, Markdown,
/// and reformatting entirely and strictly decodes the source bytes, failing on
/// unknown or invalid character sets instead of substituting replacement
/// characters.
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

pub(crate) async fn fetch_with_runtime(
    web: &LocalWeb,
    value: &str,
) -> Result<FetchedPage, FetchError> {
    fetch_with(
        value,
        &NetworkBackend::from_local_web(web),
        &MoliBackend {
            browser: web.fetch_browser(),
        },
    )
    .await
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
}

trait RenderBackend: Sync {
    fn render<'a>(
        &'a self,
        url: &'a Url,
    ) -> BackendFuture<'a, Result<RenderedResponse, FetchError>>;
}

struct HttpResponse {
    status: u16,
    content_type: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
}

struct RenderedResponse {
    final_url: String,
    html: String,
}

struct MoliBackend {
    browser: BrowserRuntime,
}

impl RenderBackend for MoliBackend {
    fn render<'a>(
        &'a self,
        url: &'a Url,
    ) -> BackendFuture<'a, Result<RenderedResponse, FetchError>> {
        Box::pin(async move {
            let rendered = self
                .browser
                .render(RenderRequest {
                    url: url.as_str(),
                    preflight_url: None,
                    ready_selector: "body",
                    failure_expression: None,
                    timeout: RENDER_TIMEOUT,
                    request_guard: Some(policy::is_public_browser_request),
                })
                .await
                .map_err(|error| {
                    FetchError::unavailable(format!("rendered extraction failed: {error}"))
                })?;
            if rendered.html.len() > DOWNLOAD_BYTE_CAP {
                return Err(FetchError::new(
                    FetchErrorCode::ResponseTooLarge,
                    format!("rendered HTML exceeds the {DOWNLOAD_BYTE_CAP}-byte safety cap"),
                ));
            }
            Ok(RenderedResponse {
                final_url: rendered.url,
                html: rendered.html,
            })
        })
    }
}

async fn fetch_with(
    value: &str,
    http: &impl HttpBackend,
    renderer: &impl RenderBackend,
) -> Result<FetchedPage, FetchError> {
    let requested_url = policy::validate_url(value)?;
    let (final_url, response) =
        get_with_redirects(requested_url.clone(), http, MAX_REDIRECTS).await?;
    if !(200..300).contains(&response.status) {
        return Err(FetchError::unavailable(format!(
            "HTTP request returned status {}",
            response.status
        )));
    }

    let content_type = response.content_type.as_deref().unwrap_or("");
    let decoded = extract::decode_lossy(&response.body, content_type);
    let mut page = match extract::classify(content_type, &decoded.text) {
        ContentKind::Html => {
            fetch_html(requested_url, final_url, decoded.text, http, renderer).await?
        }
        ContentKind::Markdown | ContentKind::Plain => {
            page_from_text(requested_url, final_url, decoded.text)
        }
        ContentKind::Json => page_from_text(
            requested_url,
            final_url,
            extract::json_markdown(&decoded.text),
        ),
        ContentKind::Xml => page_from_text(
            requested_url,
            final_url,
            extract::xml_markdown(&decoded.text),
        ),
        ContentKind::Unsupported => return Err(extract::unsupported(content_type)),
    };
    if decoded.lossy && page.extraction_path == ExtractionPath::Static {
        page.limitations.push(LOSSY_DECODE_LIMITATION.into());
    }
    Ok(page)
}

async fn get_with_redirects(
    mut url: Url,
    http: &impl HttpBackend,
    redirect_limit: usize,
) -> Result<(Url, HttpResponse), FetchError> {
    for redirect_count in 0..=redirect_limit {
        policy::validate_parsed_url(&url)?;
        let addresses = if http.pins_origin(&url) {
            resolve_public_addresses(&url, http).await?
        } else {
            Vec::new()
        };
        let response = http.get(&url, &addresses).await?;
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
    if addresses
        .iter()
        .any(|address| !crate::address_policy::is_public_ip(*address))
    {
        return Err(FetchError::invalid_url(url.as_str()));
    }
    Ok(addresses)
}

async fn fetch_html(
    requested_url: Url,
    final_url: Url,
    html: String,
    http: &impl HttpBackend,
    renderer: &impl RenderBackend,
) -> Result<FetchedPage, FetchError> {
    let static_extract = extract::extract_html(&html, Some(&final_url))?;
    if !extract::is_low_quality(&static_extract.markdown) {
        return Ok(page_from_extract(
            requested_url,
            final_url,
            static_extract,
            ExtractionPath::Static,
            Vec::new(),
        ));
    }

    let rendered = match renderer.render(&final_url).await {
        Ok(rendered) => rendered,
        Err(error) => {
            if static_extract.markdown.trim().is_empty() {
                return Err(error);
            }
            return Ok(page_from_extract(
                requested_url,
                final_url,
                static_extract,
                ExtractionPath::Static,
                vec![format!(
                    "Rendered Extraction was unavailable: {error}. {LOW_QUALITY_LIMITATION}"
                )],
            ));
        }
    };

    let rendered_url = policy::validate_url(&rendered.final_url)?;
    resolve_public_addresses(&rendered_url, http).await?;
    let rendered_extract = extract::extract_html(&rendered.html, Some(&rendered_url))?;
    let rendered_low_quality = extract::is_low_quality(&rendered_extract.markdown);
    let (selected_url, selected_extract, selected_path) = if !rendered_low_quality
        || extract::score(&rendered_extract.markdown) > extract::score(&static_extract.markdown)
    {
        (rendered_url, rendered_extract, ExtractionPath::Rendered)
    } else {
        (final_url, static_extract, ExtractionPath::Static)
    };
    let limitations = rendered_low_quality
        .then(|| LOW_QUALITY_LIMITATION.to_string())
        .into_iter()
        .collect();
    Ok(page_from_extract(
        requested_url,
        selected_url,
        selected_extract,
        selected_path,
        limitations,
    ))
}

fn page_from_extract(
    requested_url: Url,
    final_url: Url,
    extract: HtmlExtract,
    extraction_path: ExtractionPath,
    limitations: Vec<String>,
) -> FetchedPage {
    let (markdown, truncated) = cap_markdown(extract.markdown);
    FetchedPage {
        requested_url: requested_url.into(),
        final_url: final_url.into(),
        title: extract.title,
        markdown,
        extraction_path,
        limitations,
        truncated,
    }
}

fn page_from_text(requested_url: Url, final_url: Url, markdown: String) -> FetchedPage {
    let (markdown, truncated) = cap_markdown(markdown);
    FetchedPage {
        requested_url: requested_url.into(),
        final_url: final_url.into(),
        title: None,
        markdown,
        extraction_path: ExtractionPath::Static,
        limitations: Vec::new(),
        truncated,
    }
}

fn cap_markdown(markdown: String) -> (String, bool) {
    if markdown.chars().count() <= MARKDOWN_CHARACTER_CAP {
        return (markdown, false);
    }
    (
        markdown.chars().take(MARKDOWN_CHARACTER_CAP).collect(),
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        net::{IpAddr, Ipv4Addr},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    use super::*;

    pub(super) struct StubBackend {
        pub(super) responses: Mutex<VecDeque<HttpResponse>>,
        pub(super) rendered: Mutex<Option<Result<RenderedResponse, FetchError>>>,
        pub(crate) requests: AtomicUsize,
        pub(crate) renders: AtomicUsize,
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
                rendered: Mutex::new(None),
                requests: AtomicUsize::new(0),
                renders: AtomicUsize::new(0),
                requested_urls: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn with_rendered(self, html: impl Into<String>) -> Self {
            *self.rendered.lock().expect("stub renderer lock") = Some(Ok(RenderedResponse {
                final_url: "https://example.com/article".into(),
                html: html.into(),
            }));
            self
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
            self.requested_urls
                .lock()
                .expect("stub requested URL lock")
                .push(url.as_str().to_string());
            Box::pin(async move {
                self.responses
                    .lock()
                    .expect("stub response lock")
                    .pop_front()
                    .ok_or_else(|| FetchError::unavailable("stub response exhausted"))
            })
        }
    }

    impl RenderBackend for StubBackend {
        fn render<'a>(
            &'a self,
            _url: &'a Url,
        ) -> BackendFuture<'a, Result<RenderedResponse, FetchError>> {
            self.renders.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                self.rendered
                    .lock()
                    .expect("stub renderer lock")
                    .take()
                    .unwrap_or_else(|| {
                        Err(FetchError::unavailable("browser renderer is unavailable"))
                    })
            })
        }
    }

    #[tokio::test]
    async fn rejects_non_public_urls_before_network_io() {
        for url in [
            "file:///etc/passwd",
            "http://user:password@example.com/",
            "http://localhost/",
            "http://service.local/",
            "http://home.arpa/",
            "http://127.0.0.1/",
            "http://192.168.1.1/",
            "http://127.0.0.1../",
            "http://192.168.1.1../",
            "http://[::1]/",
            "http://[2002:a00:100::1]/",
            "http://[3fff::1]/",
        ] {
            let backend = StubBackend::default();
            let error = fetch_with(url, &backend, &backend).await.unwrap_err();
            assert_eq!(error.code(), FetchErrorCode::InvalidUrl, "{url}");
            assert_eq!(backend.requests.load(Ordering::Relaxed), 0, "{url}");
        }
    }

    #[tokio::test]
    async fn rejects_unusable_dns_answers_before_http() {
        struct DnsBackend(Mutex<Option<Result<Vec<IpAddr>, FetchError>>>);
        impl HttpBackend for DnsBackend {
            fn resolve<'a>(
                &'a self,
                _url: &'a Url,
            ) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
                Box::pin(async move { self.0.lock().expect("stub DNS lock").take().unwrap() })
            }

            fn get<'a>(
                &'a self,
                _url: &'a Url,
                _addresses: &'a [IpAddr],
            ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
                Box::pin(async { unreachable!("unusable DNS answers must stop before HTTP") })
            }
        }
        let renderer = StubBackend::default();
        for (answers, code) in [
            (
                Ok(vec![
                    IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                    IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                ]),
                FetchErrorCode::InvalidUrl,
            ),
            (Ok(Vec::new()), FetchErrorCode::Unavailable),
            (
                Err(FetchError::unavailable("stub DNS lookup failed")),
                FetchErrorCode::Unavailable,
            ),
        ] {
            let backend = DnsBackend(Mutex::new(Some(answers)));
            let error = fetch_with("https://example.com/article", &backend, &renderer)
                .await
                .unwrap_err();

            assert_eq!(error.code(), code);
            assert_eq!(renderer.renders.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn rejects_trailing_dot_redirect_before_second_request() {
        for location in ["http://127.0.0.1../", "http://192.168.1.1../"] {
            let backend = StubBackend::default();
            *backend.responses.lock().expect("stub response lock") =
                VecDeque::from([HttpResponse {
                    status: 302,
                    content_type: None,
                    location: Some(location.into()),
                    body: Vec::new(),
                }]);

            let error = fetch_with("https://example.com/article", &backend, &backend)
                .await
                .unwrap_err();

            assert_eq!(error.code(), FetchErrorCode::InvalidUrl, "{location}");
            assert_eq!(backend.requests.load(Ordering::Relaxed), 1, "{location}");
            assert_eq!(backend.renders.load(Ordering::Relaxed), 0, "{location}");
        }
    }

    #[tokio::test]
    async fn rejects_non_public_rendered_final_url() {
        let backend = StubBackend::response(
            "text/html",
            "<html><body><main>Please enable JavaScript to continue to the requested article.</main></body></html>",
        );
        *backend.rendered.lock().expect("stub renderer lock") = Some(Ok(RenderedResponse {
            final_url: "http://127.0.0.1/".into(),
            html: "<html><body><main>Private content must not be returned.</main></body></html>"
                .into(),
        }));

        let error = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap_err();

        assert_eq!(error.code(), FetchErrorCode::InvalidUrl);
        assert_eq!(backend.requests.load(Ordering::Relaxed), 1);
        assert_eq!(backend.renders.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn extracts_article_markdown_without_page_navigation() {
        let paragraphs = (0..12)
            .map(|index| format!("<p>Paragraph {index} explains the complete local web fetch pipeline, including extraction quality, citations, and predictable Markdown output for callers.</p>"))
            .collect::<String>();
        let html = format!(
            "<html><head><title>Fallback title</title></head><body><nav>Products Pricing Sign in</nav><article><h1>Local Web Fetch</h1>{paragraphs}<a href='/source'>Source</a></article><footer>Legal links</footer></body></html>"
        );
        let backend = StubBackend::response("text/html; charset=utf-8", html);

        let page = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap();

        assert_eq!(page.title.as_deref(), Some("Fallback title"));
        assert_eq!(page.extraction_path, ExtractionPath::Static);
        assert!(page.markdown.contains("Local Web Fetch"));
        assert!(page
            .markdown
            .contains("[Source](https://example.com/source)"));
        assert!(!page.markdown.contains("Products Pricing"));
        assert_eq!(backend.renders.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn renders_a_javascript_gate_once_through_the_same_extractor() {
        let paragraphs = (0..12)
            .map(|index| format!("<p>Rendered paragraph {index} contains useful article detail, stable citations, and readable content after browser execution.</p>"))
            .collect::<String>();
        let rendered = format!(
            "<html><head><title>Rendered page</title></head><body><nav>Menu</nav><article><h1>Rendered article</h1>{paragraphs}</article></body></html>"
        );
        let backend = StubBackend::response(
            "text/html",
            "<html><body><main><p>Please enable JavaScript to continue. This page requires scripts before the requested content can load.</p></main></body></html>",
        )
        .with_rendered(rendered);

        let page = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap();

        assert_eq!(page.extraction_path, ExtractionPath::Rendered);
        assert_eq!(page.title.as_deref(), Some("Rendered page"));
        assert!(page.markdown.contains("Rendered paragraph"));
        assert!(!page.markdown.contains("Menu"));
        assert_eq!(backend.renders.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn keeps_the_better_low_quality_body_with_a_limitation() {
        let backend = StubBackend::response(
            "text/html",
            "<html><body><main>Please enable JavaScript to view this requested article and continue reading.</main></body></html>",
        )
        .with_rendered("<html><body><main>Sign in to continue to this protected article.</main></body></html>");

        let page = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap();

        assert!(!page.markdown.is_empty());
        assert_eq!(page.limitations.len(), 1);
        assert_eq!(backend.renders.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn default_html_fetch_reports_lossy_source_decoding() {
        let mut body = format!(
            "<html><body><article><p>{}",
            "This article explains how a gateway validates requests and preserves complete content. ".repeat(30)
        ).into_bytes();
        body.extend_from_slice(b"\xff</p></article></body></html>");
        let backend = StubBackend::response("text/html; charset=utf-8", body);
        let page = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap();
        assert_eq!(page.extraction_path, ExtractionPath::Static);
        assert!(page.markdown.contains('\u{fffd}'));
        assert!(!page.limitations.is_empty());
    }

    #[tokio::test]
    async fn passes_text_and_markdown_through_and_pretty_prints_json() {
        for content_type in ["text/plain", "text/markdown"] {
            let backend = StubBackend::response(content_type, "# Exact body\n\nKeep me.");
            let page = fetch_with("https://example.com/data", &backend, &backend)
                .await
                .unwrap();
            assert_eq!(page.markdown, "# Exact body\n\nKeep me.");
            assert_eq!(backend.renders.load(Ordering::Relaxed), 0);
        }

        let backend =
            StubBackend::response("application/json", br#"{"ok":true,"items":[1,2]}"#.to_vec());
        let page = fetch_with("https://example.com/data", &backend, &backend)
            .await
            .unwrap();
        assert!(page.markdown.contains("\"ok\": true"));
        assert!(page.markdown.starts_with("```json\n"));
    }

    #[tokio::test]
    async fn honors_a_basic_html_meta_charset() {
        let html = b"<html><head><meta charset=\"windows-1252\"><title>Caf\xe9</title></head><body><article><p>Caf\xe9 prices and details are available here.</p></article></body></html>";
        let backend = StubBackend::response("text/html", html.to_vec());

        let page = fetch_with("https://example.com/cafe", &backend, &backend)
            .await
            .unwrap();

        assert_eq!(page.title.as_deref(), Some("Café"));
        assert!(page.markdown.contains("Café"));
    }

    #[tokio::test]
    async fn rejects_unsupported_media_types() {
        for content_type in ["application/pdf", "image/png", "image/svg+xml"] {
            let backend = StubBackend::response(content_type, b"binary".to_vec());
            let error = fetch_with("https://example.com/file", &backend, &backend)
                .await
                .unwrap_err();
            assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);
        }
    }

    #[tokio::test]
    async fn caps_markdown_only_at_the_safety_ceiling() {
        let backend = StubBackend::response("text/plain", "x".repeat(MARKDOWN_CHARACTER_CAP + 1));
        let page = fetch_with("https://example.com/large.txt", &backend, &backend)
            .await
            .unwrap();

        assert_eq!(page.markdown.chars().count(), MARKDOWN_CHARACTER_CAP);
        assert!(page.truncated);
    }

    #[tokio::test]
    async fn missing_renderer_returns_static_markdown_with_a_limitation() {
        let backend = StubBackend::response(
            "text/html",
            "<html><head><title>Shell</title></head><body><main>Please enable JavaScript to continue to the requested article.</main></body></html>",
        );

        let page = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap();

        assert_eq!(page.extraction_path, ExtractionPath::Static);
        assert!(page.markdown.contains("enable JavaScript"));
        assert_eq!(page.limitations.len(), 1);
    }

    #[tokio::test]
    async fn rejects_a_private_redirect_before_the_second_request() {
        let backend = StubBackend {
            responses: Mutex::new(VecDeque::from([HttpResponse {
                status: 302,
                content_type: None,
                location: Some("http://127.0.0.1/admin".into()),
                body: Vec::new(),
            }])),
            rendered: Mutex::new(None),
            requests: AtomicUsize::new(0),
            renders: AtomicUsize::new(0),
            requested_urls: Mutex::new(Vec::new()),
        };

        let error = fetch_with("https://example.com/redirect", &backend, &backend)
            .await
            .unwrap_err();

        assert_eq!(error.code(), FetchErrorCode::InvalidUrl);
        assert_eq!(backend.requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn navigation_heavy_markdown_triggers_one_fallback() {
        let navigation = (0..12)
            .map(|index| format!("<a href='/item/{index}'>Menu {index}</a><br>"))
            .collect::<String>();
        let rendered = (0..12)
            .map(|index| format!("<p>Article paragraph {index} contains complete, useful rendered content for the fetched page contract.</p>"))
            .collect::<String>();
        let backend = StubBackend::response(
            "text/html",
            format!("<html><body><main>{navigation}</main></body></html>"),
        )
        .with_rendered(format!(
            "<html><head><title>Article</title></head><body><article>{rendered}</article></body></html>"
        ));

        let page = fetch_with("https://example.com/menu", &backend, &backend)
            .await
            .unwrap();

        assert_eq!(page.extraction_path, ExtractionPath::Rendered);
        assert_eq!(backend.renders.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn proxied_fetch_skips_origin_dns_and_still_rejects_private_urls() {
        struct ProxyBackend {
            resolved: AtomicUsize,
            requests: AtomicUsize,
        }
        impl HttpBackend for ProxyBackend {
            fn pins_origin(&self, _url: &Url) -> bool {
                false
            }

            fn resolve<'a>(
                &'a self,
                _url: &'a Url,
            ) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
                self.resolved.fetch_add(1, Ordering::Relaxed);
                Box::pin(async { unreachable!("proxied fetch must not resolve origin DNS") })
            }

            fn get<'a>(
                &'a self,
                _url: &'a Url,
                addresses: &'a [IpAddr],
            ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
                assert!(addresses.is_empty());
                self.requests.fetch_add(1, Ordering::Relaxed);
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        content_type: Some("text/plain".into()),
                        location: None,
                        body: b"proxied".to_vec(),
                    })
                })
            }
        }
        impl RenderBackend for ProxyBackend {
            fn render<'a>(
                &'a self,
                _url: &'a Url,
            ) -> BackendFuture<'a, Result<RenderedResponse, FetchError>> {
                Box::pin(async { unreachable!("plain text does not render") })
            }
        }

        let backend = ProxyBackend {
            resolved: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
        };
        let page = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap();
        assert_eq!(page.markdown, "proxied");
        assert_eq!(backend.resolved.load(Ordering::Relaxed), 0);
        assert_eq!(backend.requests.load(Ordering::Relaxed), 1);

        let error = fetch_with("http://127.0.0.1/", &backend, &backend)
            .await
            .unwrap_err();
        assert_eq!(error.code(), FetchErrorCode::InvalidUrl);
        assert_eq!(backend.requests.load(Ordering::Relaxed), 1);
    }

    fn utf16_bytes(text: &str, to_bytes: fn(u16) -> [u8; 2], bom: [u8; 2]) -> Vec<u8> {
        let mut bytes = bom.to_vec();
        bytes.extend(text.encode_utf16().flat_map(to_bytes));
        bytes
    }

    #[test]
    fn reads_utf8_text_with_bom_in_both_modes() {
        let text = "你好 🌆 plain utf8 text";
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(text.as_bytes());

        let readable = convert_read_bytes(&bytes, "text/plain; charset=utf-8", None, false)
            .expect("default utf8 read with BOM succeeds");
        assert_eq!(readable.text, text);
        assert_eq!(readable.representation, "text");
        assert!(readable.limitations.is_empty());
        assert!(!readable.source_truncated);

        let raw = convert_read_bytes(&bytes, "text/plain; charset=utf-8", None, true)
            .expect("raw utf8 read with BOM succeeds");
        assert_eq!(raw.text, text);
        assert_eq!(raw.representation, "raw");
        assert_eq!(raw.title, None);
    }

    #[test]
    fn bom_overrides_a_contradictory_declared_charset() {
        let text = "中文内容必须按字节序标记解码";
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(text.as_bytes());

        let raw = convert_read_bytes(&bytes, "text/plain; charset=iso-8859-1", None, true)
            .expect("BOM must win over the declared charset");
        assert_eq!(raw.text, text);
    }

    #[test]
    fn reads_utf16_with_bom_in_both_modes() {
        let text = "Grüße 中文 🌍";
        for (charset, to_bytes, bom) in [
            (
                "text/plain; charset=utf-16le",
                u16::to_le_bytes as fn(u16) -> [u8; 2],
                [0xFF, 0xFE],
            ),
            (
                "text/plain; charset=utf-16be",
                u16::to_be_bytes as fn(u16) -> [u8; 2],
                [0xFE, 0xFF],
            ),
        ] {
            let bytes = utf16_bytes(text, to_bytes, bom);
            for raw in [false, true] {
                let read = convert_read_bytes(&bytes, charset, None, raw)
                    .unwrap_or_else(|error| panic!("{charset} raw={raw}: {error}"));
                assert_eq!(read.text, text, "{charset} raw={raw}");
            }

            let bom_only =
                convert_read_bytes(&bytes, "text/plain", None, true).expect("BOM alone decodes");
            assert_eq!(bom_only.text, text);
        }
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

        for (charset, to_bytes) in [
            (
                "text/plain; charset=utf-16le",
                u16::to_le_bytes as fn(u16) -> [u8; 2],
            ),
            (
                "text/plain; charset=utf-16be",
                u16::to_be_bytes as fn(u16) -> [u8; 2],
            ),
        ] {
            let bytes: Vec<u8> = "Hi".encode_utf16().flat_map(to_bytes).collect();
            let raw = convert_read_bytes(&bytes, charset, None, true)
                .unwrap_or_else(|error| panic!("{charset}: {error}"));
            assert_eq!(raw.text, "Hi", "{charset}");
        }
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
        assert!(
            readable.text.contains("https://example.com/order"),
            "links must be absolutized against the trusted base: {}",
            readable.text
        );
        assert!(readable.limitations.is_empty());
    }

    #[test]
    fn html_without_a_trusted_base_keeps_relative_links() {
        let paragraphs = (0..6)
            .map(|index| {
                format!("<p>Detached paragraph {index} with enough words that the article stays the extracted main content without any origin.</p>")
            })
            .collect::<String>();
        let html = format!(
            "<html><head><title>Detached</title></head><body><article><h1>Detached doc</h1>{paragraphs}<a href='/source'>Source</a></article></body></html>"
        );

        let readable = convert_read_bytes(html.as_bytes(), "text/html", None, false)
            .expect("base-less HTML converts");
        assert!(
            readable.text.contains("](/source)"),
            "relative link must survive without a fabricated origin: {}",
            readable.text
        );
        assert!(!readable.text.contains("://"));
        assert!(!readable.text.contains("example.com"));
    }

    #[test]
    fn raw_html_returns_the_exact_source_without_decoration() {
        let html = "<html><head><title>Raw doc</title></head><body><nav>Nav stays</nav><article><p>Body</p></article></body></html>\n";

        let raw = convert_read_bytes(html.as_bytes(), "text/html; charset=utf-8", None, true)
            .expect("raw HTML strict decode");
        assert_eq!(raw.text, html);
        assert_eq!(raw.representation, "raw");
        assert_eq!(raw.title, None);
    }

    #[test]
    fn json_and_xml_are_read_verbatim_without_reformatting() {
        let json = r#"{"b":1,"a":[true,null]}"#;
        for raw in [false, true] {
            let read = convert_read_bytes(json.as_bytes(), "application/json", None, raw)
                .unwrap_or_else(|error| panic!("json raw={raw}: {error}"));
            assert_eq!(read.text, json, "json raw={raw}");
            assert_eq!(read.representation, if raw { "raw" } else { "text" });
            assert_eq!(read.title, None);
        }

        let xml = "<root><item>1</item>  <item>2</item></root>\n";
        for raw in [false, true] {
            let read = convert_read_bytes(xml.as_bytes(), "application/xml", None, raw)
                .unwrap_or_else(|error| panic!("xml raw={raw}: {error}"));
            assert_eq!(read.text, xml, "xml raw={raw}");
        }
    }

    #[test]
    fn markdown_and_explicit_text_subtypes_are_read_verbatim() {
        let markdown = "# Heading\n\n- item\n";
        let read = convert_read_bytes(markdown.as_bytes(), "text/markdown", None, false)
            .expect("markdown source reads");
        assert_eq!(read.text, markdown);
        assert_eq!(read.representation, "markdown");

        let csv = "a,b\r\n1,2\r\n";
        for content_type in ["text/csv", "text/tab-separated-values"] {
            let read = convert_read_bytes(csv.as_bytes(), content_type, None, false)
                .unwrap_or_else(|error| panic!("{content_type}: {error}"));
            assert_eq!(read.text, csv, "{content_type}");
            assert_eq!(read.representation, "text");
        }
    }

    #[test]
    fn empty_text_reads_successfully_in_both_modes() {
        for content_type in ["text/plain", ""] {
            for raw in [false, true] {
                let read = convert_read_bytes(b"", content_type, None, raw)
                    .unwrap_or_else(|error| panic!("{content_type} raw={raw}: {error}"));
                assert_eq!(read.text, "", "{content_type} raw={raw}");
                assert!(read.limitations.is_empty());
                assert!(!read.source_truncated);
            }
        }
    }

    #[test]
    fn raw_rejects_invalid_utf8_while_default_reads_lossily() {
        let bytes = b"valid start then \xc3\x28 invalid tail";

        let error = convert_read_bytes(bytes, "text/plain", None, true)
            .expect_err("raw must reject invalid utf-8");
        assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);

        let readable = convert_read_bytes(bytes, "text/plain", None, false)
            .expect("default mode degrades lossily");
        assert!(readable.text.contains("valid start then"));
        assert!(readable.text.contains("invalid tail"));
        assert!(!readable.limitations.is_empty());
    }

    #[test]
    fn raw_rejects_unknown_charsets_in_declaration_and_meta() {
        let error = convert_read_bytes(b"ascii only", "text/plain; charset=euc-kr", None, true)
            .expect_err("raw must reject unknown declared charsets");
        assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);

        let readable = convert_read_bytes(b"ascii only", "text/plain; charset=euc-kr", None, false)
            .expect("default mode still reads the ascii subset");
        assert_eq!(readable.text, "ascii only");
        assert!(!readable.limitations.is_empty());

        let html = b"<html><head><meta charset=\"shift_jis\"><title>t</title></head><body><article><p>ascii</p></article></body></html>";
        let error = convert_read_bytes(html, "text/html", None, true)
            .expect_err("raw must reject unknown meta charsets");
        assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);
    }

    #[test]
    fn raw_rejects_broken_utf16_payloads() {
        let odd: Vec<u8> = "hi"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .chain([0x41])
            .collect();
        let error = convert_read_bytes(&odd, "text/plain; charset=utf-16le", None, true)
            .expect_err("raw must reject an odd utf-16 length");
        assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);
        let readable = convert_read_bytes(&odd, "text/plain; charset=utf-16le", None, false)
            .expect("default mode drops the dangling unit lossily");
        assert!(readable.text.starts_with("hi"));
        assert!(!readable.limitations.is_empty());

        let mut lone_surrogate = Vec::new();
        for unit in [0xD800_u16, 0x0041] {
            lone_surrogate.extend_from_slice(&unit.to_le_bytes());
        }
        let error = convert_read_bytes(&lone_surrogate, "text/plain; charset=utf-16le", None, true)
            .expect_err("raw must reject a lone utf-16 surrogate");
        assert_eq!(error.code(), FetchErrorCode::UnsupportedMediaType);
        let readable =
            convert_read_bytes(&lone_surrogate, "text/plain; charset=utf-16le", None, false)
                .expect("default mode replaces the surrogate lossily");
        assert!(readable.text.ends_with('A'));
        assert!(!readable.limitations.is_empty());
    }

    #[test]
    fn binary_media_types_are_rejected_by_both_read_modes() {
        for content_type in ["image/png", "image/jpeg", "application/pdf"] {
            for raw in [false, true] {
                let error = convert_read_bytes(b"pretend bytes", content_type, None, raw)
                    .expect_err(&format!("{content_type} raw={raw} must be rejected"));
                assert_eq!(
                    error.code(),
                    FetchErrorCode::UnsupportedMediaType,
                    "{content_type} raw={raw}"
                );
            }
        }
    }

    #[tokio::test]
    async fn enforces_the_redirect_budget_passed_by_the_caller() {
        let redirect = |location: String| HttpResponse {
            status: 302,
            content_type: None,
            location: Some(location),
            body: Vec::new(),
        };
        let ok = HttpResponse {
            status: 200,
            content_type: Some("text/plain".into()),
            location: None,
            body: b"done".to_vec(),
        };
        let stub = |responses: VecDeque<HttpResponse>| StubBackend {
            responses: Mutex::new(responses),
            rendered: Mutex::new(None),
            requests: AtomicUsize::new(0),
            renders: AtomicUsize::new(0),
            requested_urls: Mutex::new(Vec::new()),
        };

        let mut within_budget: VecDeque<HttpResponse> = (0..MAX_REDIRECTS)
            .map(|index| redirect(format!("https://example.com/hop/{index}")))
            .collect();
        within_budget.push_back(ok);
        let backend = stub(within_budget);
        let page = fetch_with("https://example.com/start", &backend, &backend)
            .await
            .expect("exactly MAX_REDIRECTS hops fit the budget");
        assert_eq!(page.markdown, "done");
        assert_eq!(backend.requests.load(Ordering::Relaxed), MAX_REDIRECTS + 1);

        let over_budget: VecDeque<HttpResponse> = (0..=MAX_REDIRECTS)
            .map(|index| redirect(format!("https://example.com/hop/{index}")))
            .collect();
        let backend = stub(over_budget);
        let error = fetch_with("https://example.com/start", &backend, &backend)
            .await
            .expect_err("one hop over the budget fails");
        assert_eq!(error.code(), FetchErrorCode::Unavailable);
        assert_eq!(backend.requests.load(Ordering::Relaxed), MAX_REDIRECTS + 1);
    }
}
