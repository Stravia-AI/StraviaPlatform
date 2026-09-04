use std::{sync::LazyLock, time::Duration};

use futures::future::join_all;
use regex::Regex;
use scraper::{ElementRef, Selector};
use url::Url;

use crate::{
    browser::RenderRequest,
    search::{
        engines::{EngineResponse, EngineSearchResult, RequestResponse, SearchQuery},
        parse::{parse_html_response_with_opts, ParseOpts, QueryMethod},
        urls::normalize_url,
    },
};

const SO_HOME_URL: &str = "https://www.so.com/";
const SO_SEARCH_URL: &str = "https://www.so.com/s";
const SO_RESULT_SELECTOR: &str = "li.res-list h3.res-title a[data-mdurl]";
const BROWSER_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

static LOCATION_REPLACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"window\.location\.replace\(["']([^"']+)["']\)"#)
        .expect("360 location.replace pattern is valid")
});

pub async fn request(search: &SearchQuery) -> eyre::Result<RequestResponse> {
    let response = search.http.get(search_url(search).as_str()).send().await?;
    let body = response.text().await?;
    let parsed = if requires_browser_render(&body) {
        render_response(search).await?
    } else {
        parse_response(&body)?
    };

    Ok(RequestResponse::Instant(Box::new(
        resolve_link_urls(parsed, &search.http).await,
    )))
}

pub(crate) fn requires_browser_render(body: &str) -> bool {
    !body.contains("res-list")
}

pub(crate) async fn render_response(search: &SearchQuery) -> eyre::Result<EngineResponse> {
    let url = search_url(search);
    let rendered = search
        .browser
        .render(RenderRequest {
            url: url.as_str(),
            preflight_url: Some(SO_HOME_URL),
            ready_selector: SO_RESULT_SELECTOR,
            timeout: BROWSER_RENDER_TIMEOUT,
            request_guard: None,
        })
        .await
        .map_err(|error| eyre::eyre!("360 browser renderer failed: {error}"))?;

    if !rendered.ready {
        eyre::bail!(
            "360 search results did not render from {} within {} seconds",
            rendered.url,
            BROWSER_RENDER_TIMEOUT.as_secs()
        );
    }

    parse_response(&rendered.html)
}

fn search_url(search: &SearchQuery) -> Url {
    let query = search.query_with_allowed_domains();
    Url::parse_with_params(SO_SEARCH_URL, &[("q", query.as_ref())])
        .expect("360 search URL is valid")
}

pub fn parse_response(body: &str) -> eyre::Result<EngineResponse> {
    parse_html_response_with_opts(
        body,
        ParseOpts::new()
            .result("li.res-list")
            .title("h3.res-title")
            // 自然结果用 data-mdurl；没有时留下 href，由 /link 二次解析或过滤。
            .href(QueryMethod::Manual(Box::new(source_url)))
            .description("p.res-desc"),
    )
}

fn source_url(el: &ElementRef) -> eyre::Result<String> {
    Ok(el
        .select(&Selector::parse("h3.res-title a").expect("360 title selector is valid"))
        .next()
        .and_then(|link| {
            link.value()
                .attr("data-mdurl")
                .filter(|url| !url.is_empty())
                .or_else(|| link.value().attr("href"))
        })
        .unwrap_or_default()
        .to_string())
}

async fn resolve_link_urls(mut response: EngineResponse, client: &wreq::Client) -> EngineResponse {
    let results = join_all(
        response
            .search_results
            .into_iter()
            .map(|result| resolve_link_url(result, client.clone())),
    )
    .await;

    response.search_results = results
        .into_iter()
        .filter_map(|result| match result {
            Ok(result) => Some(result),
            Err(error) => {
                tracing::warn!("360 destination URL resolution failed: {error}");
                None
            }
        })
        .collect();
    response
}

async fn resolve_link_url(
    mut result: EngineSearchResult,
    client: wreq::Client,
) -> eyre::Result<EngineSearchResult> {
    let url = Url::parse(&result.url)?;
    if is_360_ai_url(&url) {
        eyre::bail!("360 AI card has no destination URL");
    }
    if !is_so_tracking_link(&url) {
        return Ok(result);
    }

    // 不解密 m= 令牌；读 /link 返回页里的 location.replace，和搜狗微信同一类。
    let response = client
        .get(url.as_str())
        .header("Referer", SO_SEARCH_URL)
        .send()
        .await?;
    let body = response.text().await?;
    result.url = extract_so_link_destination(&body)
        .ok_or_else(|| eyre::eyre!("360 /link did not contain a destination URL"))?;
    Ok(result)
}

fn extract_so_link_destination(body: &str) -> Option<String> {
    let url = LOCATION_REPLACE
        .captures(body)
        .and_then(|captures| Url::parse(&captures[1]).ok())?;
    is_final_360_url(&url).then(|| normalize_url(url.as_str()))
}

fn is_so_tracking_link(url: &Url) -> bool {
    matches!(url.host_str(), Some("www.so.com") | Some("so.com")) && url.path() == "/link"
}

fn is_360_ai_url(url: &Url) -> bool {
    url.host_str() == Some("ai.so.com")
}

fn is_final_360_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && !is_so_tracking_link(url)
        && !is_360_ai_url(url)
}

#[cfg(test)]
mod tests {
    use super::{
        extract_so_link_destination, is_360_ai_url, is_so_tracking_link, parse_response,
        requires_browser_render, search_url,
    };
    use crate::search::engines::{AllowedDomain, SearchQuery};
    use url::Url;

    fn search_with_allowed_domain() -> SearchQuery {
        SearchQuery::for_test(
            "Rust language",
            vec![AllowedDomain::parse("docs.rs").unwrap()],
        )
    }

    #[test]
    fn renders_360_pages_without_result_items() {
        assert!(requires_browser_render("<html><body>Loading</body></html>"));
    }

    #[test]
    fn does_not_render_360_pages_with_result_items() {
        assert!(!requires_browser_render(
            r#"<li class="res-list"><h3 class="res-title">Result</h3></li>"#,
        ));
    }

    #[test]
    fn builds_a_360_search_url() {
        let url = search_url(&search_with_allowed_domain());

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("www.so.com"));
        assert_eq!(url.path(), "/s");
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "q").unwrap().1,
            "Rust language (site:docs.rs)"
        );
    }

    #[test]
    fn parses_empty_360_pages_as_no_results() {
        let response = parse_response("<html><body>Loading</body></html>").unwrap();

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_360_organic_results_with_canonical_urls() {
        let response = parse_response(
            r#"
                <li class="res-list">
                    <h3 class="res-title"><a href="https://www.so.com/link?m=tracking" data-mdurl="http://example.com/article/">Example result</a></h3>
                    <p class="res-desc">A <em>search</em> result summary.</p>
                </li>
            "#,
        )
        .unwrap();

        assert_eq!(response.search_results.len(), 1);
        let result = &response.search_results[0];
        assert_eq!(result.title, "Example result");
        assert_eq!(result.url, "https://example.com/article");
        assert_eq!(result.description, "A search result summary.");
    }

    #[test]
    fn keeps_tracking_href_when_data_mdurl_is_missing() {
        let response = parse_response(
            r#"
                <li class="res-list">
                    <h3 class="res-title"><a href="https://www.so.com/link?m=tracking">No mdurl</a></h3>
                    <p class="res-desc">Needs a second request.</p>
                </li>
            "#,
        )
        .unwrap();

        assert_eq!(
            response.search_results[0].url,
            "https://www.so.com/link?m=tracking"
        );
        assert!(is_so_tracking_link(
            &Url::parse(&response.search_results[0].url).unwrap()
        ));
    }

    #[test]
    fn extracts_destination_from_so_link_page() {
        let url = extract_so_link_destination(
            r#"
                <script>window.location.replace("https://blog.csdn.net/inthat/article/details/121491401")</script>
                <noscript>
                    <meta http-equiv="refresh" content="0;URL='https://blog.csdn.net/inthat/article/details/121491401'">
                </noscript>
            "#,
        );

        assert_eq!(
            url.as_deref(),
            Some("https://blog.csdn.net/inthat/article/details/121491401")
        );
    }

    #[test]
    fn rejects_ai_and_nested_tracking_destinations() {
        assert!(is_360_ai_url(
            &Url::parse("https://ai.so.com/search?search=rust").unwrap()
        ));
        assert_eq!(
            extract_so_link_destination(
                r#"<script>window.location.replace("https://www.so.com/link?m=nested")</script>"#
            ),
            None
        );
        assert_eq!(
            extract_so_link_destination(
                r#"<script>window.location.replace("https://ai.so.com/search?q=rust")</script>"#
            ),
            None
        );
    }
}
