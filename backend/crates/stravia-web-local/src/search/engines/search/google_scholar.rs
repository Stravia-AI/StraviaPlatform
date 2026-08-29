use url::Url;

use crate::search::{
    engines::{EngineResponse, RequestResponse, SearchQuery},
    parse::{parse_html_response_with_opts, ParseOpts},
};

pub async fn request(search: &SearchQuery) -> RequestResponse {
    let query: &str = search;
    let url = Url::parse_with_params(
        "https://scholar.google.com/scholar",
        &[("hl", "en"), ("as_sdt", "0,5"), ("q", query), ("btnG", "")],
    )
    .unwrap();
    search.http.get(url.as_str()).into()
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
    use super::parse_response;

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
