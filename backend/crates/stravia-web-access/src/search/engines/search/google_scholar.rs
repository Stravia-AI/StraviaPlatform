use std::time::Duration;

use url::Url;

use crate::{
    renderer::{RenderRequest, RenderRequestPolicy},
    search::{
        engines::{EngineResponse, RequestResponse, SearchQuery},
        parse::{parse_html_response_with_opts, ParseOpts},
    },
};

const GOOGLE_SCHOLAR_HOME_URL: &str = "https://scholar.google.com/";
const GOOGLE_SCHOLAR_RESULT_SELECTOR: &str = "div.gs_r";
const BROWSER_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn request(search: &SearchQuery) -> RequestResponse {
    search.http.get(search_url(search).as_str()).into()
}

pub(crate) fn requires_browser_render(status: wreq::StatusCode) -> bool {
    status.is_redirection()
        || matches!(
            status,
            wreq::StatusCode::FORBIDDEN | wreq::StatusCode::TOO_MANY_REQUESTS
        )
}

pub(crate) async fn render_response(search: &SearchQuery) -> eyre::Result<EngineResponse> {
    let rendered = search
        .renderer
        .render(RenderRequest {
            url: search_url(search).as_str(),
            preflight_url: Some(GOOGLE_SCHOLAR_HOME_URL),
            ready_selector: GOOGLE_SCHOLAR_RESULT_SELECTOR,
            timeout: BROWSER_RENDER_TIMEOUT,
            request_policy: RenderRequestPolicy::Unrestricted,
        })
        .await
        .map_err(|error| eyre::eyre!("Google Scholar browser renderer failed: {error}"))?;
    if !rendered.ready {
        eyre::bail!(
            "Google Scholar search results did not render from {} within {} seconds",
            rendered.url,
            BROWSER_RENDER_TIMEOUT.as_secs()
        );
    }

    parse_response(&rendered.html)
}

fn search_url(search: &SearchQuery) -> Url {
    Url::parse_with_params(
        "https://scholar.google.com/scholar",
        &[
            ("hl", "en"),
            ("as_sdt", "0,5"),
            ("q", search.query.as_str()),
            ("btnG", ""),
        ],
    )
    .unwrap()
}

pub fn parse_response(body: &str) -> eyre::Result<EngineResponse> {
    parse_html_response_with_opts(
        body,
        ParseOpts::new()
            .result("div.gs_r")
            .title("h3")
            .href("h3 > a[href]")
            .description("div.gs_rs"),
    )
}

#[cfg(test)]
mod tests {
    use super::{parse_response, requires_browser_render};

    #[test]
    fn renders_google_scholar_after_http_blocking() {
        assert!(requires_browser_render(wreq::StatusCode::FOUND));
        assert!(requires_browser_render(wreq::StatusCode::FORBIDDEN));
        assert!(requires_browser_render(wreq::StatusCode::TOO_MANY_REQUESTS));
        assert!(!requires_browser_render(wreq::StatusCode::OK));
    }

    #[test]
    fn parses_google_scholar_organic_results() {
        let response = parse_response(
            r#"
            <div class="gs_r">
              <h3><a href="https://dl.acm.org/doi/10.1145/example">Ownership types for safe concurrency</a></h3>
              <div class="gs_rs">A paper about ownership and borrowing in systems languages.</div>
            </div>
            "#,
        )
        .expect("Google Scholar response parses");

        assert_eq!(response.search_results.len(), 1);
        assert_eq!(
            response.search_results[0].title,
            "Ownership types for safe concurrency"
        );
        assert_eq!(
            response.search_results[0].url,
            "https://dl.acm.org/doi/10.1145/example"
        );
        assert_eq!(
            response.search_results[0].description,
            "A paper about ownership and borrowing in systems languages."
        );
    }

    #[test]
    fn skips_google_scholar_results_without_a_description() {
        let response = parse_response(
            r#"
            <div class="gs_r">
              <h3><a href="https://example.com/no-abstract">No abstract</a></h3>
            </div>
            "#,
        )
        .expect("Google Scholar response parses");

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_empty_google_scholar_pages_as_no_results() {
        let response = parse_response("<html><body>No results</body></html>")
            .expect("Google Scholar page parses");

        assert!(response.search_results.is_empty());
    }
}
