mod extract;
mod http;
mod policy;

use std::{future::Future, net::IpAddr, pin::Pin, time::Duration};

use serde::Serialize;
use url::Url;

use crate::browser::{BrowserRuntime, RenderRequest};
use crate::outbound::LocalWeb;
use extract::{ContentKind, HtmlExtract};
use http::NetworkBackend;

const MARKDOWN_CHARACTER_CAP: usize = 500_000;
const DOWNLOAD_BYTE_CAP: usize = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
const RENDER_TIMEOUT: Duration = Duration::from_secs(15);
const LOW_QUALITY_LIMITATION: &str =
    "The extracted content may be a page shell, login wall, or challenge page.";

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

pub(crate) async fn fetch_with_runtime(
    web: &LocalWeb,
    value: &str,
) -> Result<FetchedPage, FetchError> {
    fetch_with(
        value,
        &NetworkBackend::from_local_web(web),
        &ChromeBackend {
            browser: web.browser(),
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

struct ChromeBackend {
    browser: BrowserRuntime,
}

impl RenderBackend for ChromeBackend {
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
    let (final_url, response) = get_with_redirects(requested_url.clone(), http).await?;
    if !(200..300).contains(&response.status) {
        return Err(FetchError::unavailable(format!(
            "HTTP request returned status {}",
            response.status
        )));
    }

    let content_type = response.content_type.as_deref().unwrap_or("");
    let decoded = extract::decode(&response.body, content_type);
    match extract::classify(content_type, &decoded) {
        ContentKind::Html => fetch_html(requested_url, final_url, decoded, http, renderer).await,
        ContentKind::Markdown | ContentKind::Plain => {
            Ok(page_from_text(requested_url, final_url, decoded))
        }
        ContentKind::Json => Ok(page_from_text(
            requested_url,
            final_url,
            extract::json_markdown(&decoded),
        )),
        ContentKind::Xml => Ok(page_from_text(
            requested_url,
            final_url,
            extract::xml_markdown(&decoded),
        )),
        ContentKind::Unsupported => Err(extract::unsupported(content_type)),
    }
}

async fn get_with_redirects(
    mut url: Url,
    http: &impl HttpBackend,
) -> Result<(Url, HttpResponse), FetchError> {
    for redirect_count in 0..=MAX_REDIRECTS {
        policy::validate_url(url.as_str())?;
        let addresses = if http.pins_origin(&url) {
            resolve_public_addresses(&url, http).await?
        } else {
            Vec::new()
        };
        let response = http.get(&url, &addresses).await?;
        if !(300..400).contains(&response.status) {
            return Ok((url, response));
        }
        if redirect_count == MAX_REDIRECTS {
            return Err(FetchError::unavailable("HTTP redirect limit exceeded"));
        }
        let location = response
            .location
            .as_deref()
            .ok_or_else(|| FetchError::unavailable("HTTP redirect omitted Location"))?;
        url = url
            .join(location)
            .map_err(|_| FetchError::invalid_url(location))?;
        policy::validate_url(url.as_str())?;
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
        .any(|address| !policy::is_public_ip(*address))
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
    let static_extract = extract::extract_html(&html, &final_url)?;
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
    let rendered_extract = extract::extract_html(&rendered.html, &rendered_url)?;
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

    struct StubBackend {
        responses: Mutex<VecDeque<HttpResponse>>,
        rendered: Mutex<Option<Result<RenderedResponse, FetchError>>>,
        requests: AtomicUsize,
        renders: AtomicUsize,
    }

    impl StubBackend {
        fn response(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
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
            }
        }

        fn with_rendered(self, html: impl Into<String>) -> Self {
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
            _url: &'a Url,
            _addresses: &'a [IpAddr],
        ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
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
    async fn rejects_a_hostname_when_any_dns_answer_is_non_public() {
        struct MixedDnsBackend(AtomicUsize);
        impl HttpBackend for MixedDnsBackend {
            fn resolve<'a>(
                &'a self,
                _url: &'a Url,
            ) -> BackendFuture<'a, Result<Vec<IpAddr>, FetchError>> {
                Box::pin(async {
                    Ok(vec![
                        IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                    ])
                })
            }

            fn get<'a>(
                &'a self,
                _url: &'a Url,
                _addresses: &'a [IpAddr],
            ) -> BackendFuture<'a, Result<HttpResponse, FetchError>> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Box::pin(async { unreachable!("mixed DNS answers must stop before HTTP") })
            }
        }
        impl RenderBackend for MixedDnsBackend {
            fn render<'a>(
                &'a self,
                _url: &'a Url,
            ) -> BackendFuture<'a, Result<RenderedResponse, FetchError>> {
                Box::pin(async { unreachable!("mixed DNS answers must stop before rendering") })
            }
        }
        let backend = MixedDnsBackend(AtomicUsize::new(0));

        let error = fetch_with("https://example.com/article", &backend, &backend)
            .await
            .unwrap_err();

        assert_eq!(error.code(), FetchErrorCode::InvalidUrl);
        assert_eq!(backend.0.load(Ordering::Relaxed), 0);
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
}
