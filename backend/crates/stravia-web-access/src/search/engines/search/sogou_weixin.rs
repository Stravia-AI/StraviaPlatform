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

const SOGOU_WECHAT_ORIGIN: &str = "https://weixin.sogou.com/";
const SOGOU_WECHAT_SEARCH_URL: &str = "https://weixin.sogou.com/weixin";
const SOGOU_WECHAT_RESULT_SELECTOR: &str =
    "ul.news-list > li[id^='sogou_vr_11002601_box_'] h3 a[id*='title']";
const BROWSER_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

static WECHAT_REDIRECT_URL_PARTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"url\s*\+=\s*'([^']*)';").expect("Sogou WeChat redirect pattern is valid")
});

pub async fn request(search: &SearchQuery) -> anyhow::Result<RequestResponse> {
    let response = search.http.get(search_url(search).as_str()).send().await?;
    let body = response.text().await?;
    if requires_browser_render(&body) {
        return Ok(RequestResponse::Instant(Box::new(
            render_response(search).await?,
        )));
    }
    let response = parse_response(&body)?;

    Ok(RequestResponse::Instant(Box::new(
        resolve_article_urls(response, &search.http).await,
    )))
}

fn search_url(query: &str) -> Url {
    Url::parse_with_params(
        SOGOU_WECHAT_SEARCH_URL,
        &[("type", "2"), ("query", query), ("ie", "utf8")],
    )
    .expect("Sogou WeChat search URL is valid")
}

pub(crate) fn requires_browser_render(body: &str) -> bool {
    !body.contains("sogou_vr_11002601_box_")
}

pub(crate) async fn render_response(search: &SearchQuery) -> anyhow::Result<EngineResponse> {
    let url = search_url(search);
    let rendered = search
        .browser
        .render(RenderRequest {
            url: url.as_str(),
            preflight_url: Some(SOGOU_WECHAT_ORIGIN),
            ready_selector: SOGOU_WECHAT_RESULT_SELECTOR,
            timeout: BROWSER_RENDER_TIMEOUT,
            request_guard: None,
        })
        .await
        .map_err(|error| anyhow::anyhow!("Sogou WeChat browser renderer failed: {error}"))?;

    if !rendered.ready {
        anyhow::bail!(
            "Sogou WeChat search results did not render from {} within {} seconds",
            rendered.url,
            BROWSER_RENDER_TIMEOUT.as_secs()
        );
    }

    Ok(resolve_article_urls(parse_response(&rendered.html)?, &search.http).await)
}

pub fn parse_response(body: &str) -> anyhow::Result<EngineResponse> {
    parse_html_response_with_opts(
        body,
        ParseOpts::new()
            .result("ul.news-list > li[id^='sogou_vr_11002601_box_']")
            .title("h3 a[id*='title']")
            .href(QueryMethod::Manual(Box::new(tracking_url)))
            .description("p[id*='summary']"),
    )
}

fn tracking_url(el: &ElementRef) -> anyhow::Result<String> {
    let href = el
        .select(
            &Selector::parse("h3 a[id*='title'][href]")
                .expect("Sogou WeChat title selector is valid"),
        )
        .next()
        .and_then(|link| link.value().attr("href"))
        .unwrap_or_default();
    let origin = Url::parse(SOGOU_WECHAT_ORIGIN).expect("Sogou WeChat origin is valid");

    Ok(Url::parse(href).or_else(|_| origin.join(href))?.to_string())
}

async fn resolve_article_urls(
    mut response: EngineResponse,
    client: &wreq::Client,
) -> EngineResponse {
    let results = join_all(
        response
            .search_results
            .into_iter()
            .map(|result| resolve_article_url(result, client.clone())),
    )
    .await;

    response.search_results = results
        .into_iter()
        .filter_map(|result| match result {
            Ok(result) => Some(result),
            Err(error) => {
                tracing::warn!("Sogou WeChat article URL resolution failed: {error}");
                None
            }
        })
        .collect();
    response
}

async fn resolve_article_url(
    mut result: EngineSearchResult,
    client: wreq::Client,
) -> anyhow::Result<EngineSearchResult> {
    // 依赖搜索 client 的 cookie jar，把结果页的 SNUID 带到这次 /link 请求。
    let response = client
        .get(&result.url)
        .header("Referer", SOGOU_WECHAT_SEARCH_URL)
        .send()
        .await?;
    let redirect_page = response.text().await?;
    let url = extract_wechat_article_url(&redirect_page).ok_or_else(|| {
        anyhow::anyhow!("Sogou WeChat redirect did not contain a direct article URL")
    })?;

    result.url = url;
    Ok(result)
}

fn extract_wechat_article_url(body: &str) -> Option<String> {
    let url = WECHAT_REDIRECT_URL_PARTS
        .captures_iter(body)
        .map(|captures| captures[1].to_string())
        .collect::<String>();
    let url = Url::parse(&url).ok()?;

    (url.scheme() == "https" && url.host_str() == Some("mp.weixin.qq.com") && url.path() == "/s")
        .then(|| normalize_url(url.as_str()))
}

#[cfg(test)]
mod tests {
    use super::{extract_wechat_article_url, parse_response, requires_browser_render, search_url};

    #[test]
    fn renders_sogou_wechat_pages_without_article_results() {
        assert!(requires_browser_render("<html><body>Loading</body></html>"));
    }

    #[test]
    fn does_not_render_sogou_wechat_pages_with_article_results() {
        assert!(!requires_browser_render(
            r#"<li id="sogou_vr_11002601_box_0"></li>"#,
        ));
    }

    #[test]
    fn builds_a_sogou_wechat_search_url() {
        let url = search_url("Rust language");
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("weixin.sogou.com"));
        assert_eq!(url.path(), "/weixin");
        assert_eq!(query.get("type"), Some(&"2".to_string()));
        assert_eq!(query.get("query"), Some(&"Rust language".to_string()));
        assert_eq!(query.get("ie"), Some(&"utf8".to_string()));
    }

    #[test]
    fn parses_empty_sogou_wechat_pages_as_no_results() {
        let response = parse_response("<html><body>Loading</body></html>").unwrap();

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_sogou_wechat_article_results() {
        let response = parse_response(
            r#"
                <ul class="news-list">
                    <li id="sogou_vr_11002601_box_0">
                        <h3><a id="sogou_vr_11002601_title_0" href="/link?url=tracking">Example article</a></h3>
                        <p id="sogou_vr_11002601_summary_0">A <em>WeChat</em> article summary.</p>
                    </li>
                </ul>
            "#,
        )
        .unwrap();

        assert_eq!(response.search_results.len(), 1);
        let result = &response.search_results[0];
        assert_eq!(result.title, "Example article");
        assert_eq!(result.url, "https://weixin.sogou.com/link?url=tracking");
        assert_eq!(result.description, "A WeChat article summary.");
    }

    #[test]
    fn extracts_direct_wechat_article_urls() {
        let url = extract_wechat_article_url(
            r#"
                <script>
                    var url = '';
                    url += 'https://mp.';
                    url += 'weixin.qq.com/s?src=11&timestamp=1787318782&ver=6918&signature=example&new=1';
                    window.location.replace(url);
                </script>
            "#,
        );

        assert_eq!(
            url.as_deref(),
            Some(
                "https://mp.weixin.qq.com/s?src=11&timestamp=1787318782&ver=6918&signature=example&new=1"
            )
        );
    }

    #[test]
    fn rejects_non_wechat_redirect_targets() {
        assert_eq!(
            extract_wechat_article_url(r#"url += 'https://example.com/article';"#),
            None
        );
    }
}
