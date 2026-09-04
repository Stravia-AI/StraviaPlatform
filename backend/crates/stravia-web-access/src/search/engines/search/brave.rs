use url::Url;

use crate::search::{
    engines::{EngineResponse, RequestResponse, SearchQuery},
    parse::{parse_html_response_with_opts, ParseOpts},
};

pub async fn request(search: &SearchQuery) -> RequestResponse {
    search.http.get(search_url(search).as_str()).into()
}

fn search_url(search: &SearchQuery) -> Url {
    let query = search.query_with_allowed_domains();
    Url::parse_with_params("https://search.brave.com/search", &[("q", query.as_ref())])
        .expect("Brave search URL is valid")
}

pub fn parse_response(body: &str) -> anyhow::Result<EngineResponse> {
    parse_html_response_with_opts(
        body,
        ParseOpts::new()
            .result(".snippet[data-pos]:not(.standalone)")
            .title(".title")
            .href("a")
            .description(".generic-snippet, .video-snippet > .snippet-description"),
    )
}

#[cfg(test)]
mod tests {
    use super::{parse_response, search_url};
    use crate::search::engines::{AllowedDomain, SearchQuery};

    #[test]
    fn adds_allowed_domains_to_the_brave_query() {
        let search = SearchQuery::for_test(
            "Rust language",
            vec![AllowedDomain::parse("docs.rs").unwrap()],
        );
        let url = search_url(&search);

        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "q").unwrap().1,
            "Rust language (site:docs.rs)"
        );
    }

    #[test]
    fn parses_results_without_removed_results_container() {
        let response = parse_response(
            r#"
            <div class="snippet" data-pos="1">
              <a href="https://www.rust-lang.org/">
                <div class="title">Rust Programming Language</div>
              </a>
              <div class="generic-snippet">A language empowering everyone.</div>
            </div>
            "#,
        )
        .expect("Brave response parses");

        assert_eq!(response.search_results.len(), 1);
        assert_eq!(
            response.search_results[0].title,
            "Rust Programming Language"
        );
        assert_eq!(response.search_results[0].url, "https://www.rust-lang.org");
        assert_eq!(
            response.search_results[0].description,
            "A language empowering everyone."
        );
    }

    #[test]
    fn skips_brave_standalone_and_undescribed_snippets() {
        let response = parse_response(
            r#"
            <div class="snippet standalone" data-pos="1">
              <a href="https://example.com/ad">
                <div class="title">Advertisement</div>
              </a>
              <div class="generic-snippet">Promoted result.</div>
            </div>
            <div class="snippet" data-pos="2">
              <a href="https://example.com/no-snippet">
                <div class="title">No snippet</div>
              </a>
            </div>
            "#,
        )
        .expect("Brave response parses");

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_empty_brave_pages_as_no_results() {
        let response =
            parse_response("<html><body>No results</body></html>").expect("Brave page parses");

        assert!(response.search_results.is_empty());
    }
}
