use scraper::{ElementRef, Selector};
use serde::Deserialize;
use url::Url;

use crate::search::{
    engines::{EngineResponse, RequestResponse, SearchQuery},
    parse::{parse_html_response_with_opts, ParseOpts, QueryMethod},
};

const BAIDU_SEARCH_URL: &str = "https://www.baidu.com/s";
const BAIDU_AUTOCOMPLETE_URL: &str = "https://www.baidu.com/sugrec";

pub async fn request(search: &SearchQuery) -> RequestResponse {
    search.http.get(search_url(search).as_str()).into()
}

fn search_url(search: &SearchQuery) -> Url {
    let query = search.query_with_allowed_domains();
    Url::parse_with_params(
        BAIDU_SEARCH_URL,
        &[("wd", query.as_ref()), ("ie", "utf-8"), ("rn", "10")],
    )
    .expect("Baidu search URL is valid")
}

pub fn parse_response(body: &str) -> anyhow::Result<EngineResponse> {
    parse_html_response_with_opts(
        body,
        ParseOpts::new()
            .result("#content_left > .result")
            .title("h3")
            // Baidu's title links are tracking redirects. Organic results expose the
            // destination in `mu`, so prefer it over the link URL.
            .href(QueryMethod::Manual(Box::new(source_url)))
            .description(".c-abstract, [class*='summary-gap']"),
    )
}

fn source_url(el: &ElementRef) -> anyhow::Result<String> {
    if let Some(url) = el.value().attr("mu").filter(|url| !url.is_empty()) {
        return Ok(url.to_string());
    }

    Ok(el
        .select(&Selector::parse("h3 a[href]").expect("Baidu title selector is valid"))
        .next()
        .and_then(|link| link.value().attr("href"))
        .unwrap_or_default()
        .to_string())
}

pub fn request_autocomplete(query: &str, client: &wreq::Client) -> wreq::RequestBuilder {
    let url = Url::parse_with_params(BAIDU_AUTOCOMPLETE_URL, &[("prod", "pc"), ("wd", query)])
        .expect("Baidu autocomplete URL is valid");
    client.get(url.as_str())
}

#[derive(Deserialize)]
struct AutocompleteResponse {
    #[serde(default)]
    g: Vec<AutocompleteItem>,
    #[serde(default)]
    s: Vec<String>,
}

#[derive(Deserialize)]
struct AutocompleteItem {
    q: String,
}

pub fn parse_autocomplete_response(body: &str) -> anyhow::Result<Vec<String>> {
    let response: AutocompleteResponse = serde_json::from_str(body)?;

    if response.g.is_empty() {
        Ok(response.s)
    } else {
        Ok(response
            .g
            .into_iter()
            .map(|suggestion| suggestion.q)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_autocomplete_response, parse_response, search_url};
    use crate::search::engines::{AllowedDomain, SearchQuery};

    fn search_with_allowed_domain() -> SearchQuery {
        SearchQuery::for_test(
            "Rust language",
            vec![AllowedDomain::parse("docs.rs").unwrap()],
        )
    }

    #[test]
    fn builds_a_baidu_search_url() {
        let url = search_url(&search_with_allowed_domain());
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("www.baidu.com"));
        assert_eq!(url.path(), "/s");
        assert_eq!(
            query.get("wd"),
            Some(&"Rust language (site:docs.rs)".to_string())
        );
        assert_eq!(query.get("ie"), Some(&"utf-8".to_string()));
        assert_eq!(query.get("rn"), Some(&"10".to_string()));
    }

    #[test]
    fn parses_current_baidu_organic_results() {
        let response = parse_response(
            r#"
                <div id="content_left">
                    <div class="result c-container" mu="http://example.com/article/">
                        <h3><a href="https://www.baidu.com/link?url=tracking">Example result</a></h3>
                        <div class="summary-gap_68jXq">A <em>search</em> result summary.</div>
                    </div>
                </div>
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
    fn skips_baidu_results_without_a_description() {
        let response = parse_response(
            r#"
                <div id="content_left">
                    <div class="result c-container" mu="https://example.com/no-snippet">
                        <h3><a href="https://www.baidu.com/link?url=tracking">No snippet</a></h3>
                    </div>
                </div>
            "#,
        )
        .unwrap();

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_empty_baidu_pages_as_no_results() {
        let response = parse_response("<html><body>No results</body></html>").unwrap();

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_baidu_autocomplete_suggestions() {
        let suggestions = parse_autocomplete_response(
            r#"{"q":"rust","g":[{"type":"sug","q":"rustling"},{"type":"sug","q":"rust语言"}]}"#,
        )
        .unwrap();

        assert_eq!(suggestions, ["rustling", "rust语言"]);
    }
}
