use base64::Engine;
use rand::RngExt;
use scraper::{ElementRef, Selector};
use url::Url;

use crate::search::{
    engines::{EngineResponse, SearchQuery},
    parse::{parse_html_response_with_opts, ParseOpts, QueryMethod},
};

pub async fn request(search: &SearchQuery) -> wreq::RequestBuilder {
    let cvid = generate_cvid();
    search
        .http
        .get(search_url(search, &cvid).as_str())
        .header("Cookie", &format!("SRCHHPGUSR=IG={}", cvid))
}

fn search_url(search: &SearchQuery, cvid: &str) -> Url {
    let query = search.query_with_allowed_domains();
    Url::parse_with_params(
        "https://www.bing.com/search",
        &[
            ("q", query.as_ref()),
            ("pq", query.as_ref()),
            ("cvid", cvid),
            ("filters", "rcrse:\"1\""), // filters=rcrse:"1" makes it not try to autocorrect
            ("FORM", "PERE"),
            ("ghc", "1"),
            ("lq", "0"),
            ("qs", "n"),
            ("sk", ""),
            ("sp", "-1"),
        ],
    )
    .expect("Bing search URL is valid")
}

fn generate_cvid() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{:02X}", b)).collect()
}

pub fn parse_response(body: &str) -> anyhow::Result<EngineResponse> {
    parse_html_response_with_opts(
        body,
        ParseOpts::new()
            .result("#b_results > li.b_algo")
            .title(".b_algo h2 > a")
            .href(QueryMethod::Manual(Box::new(|el: &ElementRef| {
                let url = el
                    .select(&Selector::parse("a[href]").unwrap())
                    .next()
                    .and_then(|n| n.value().attr("href"))
                    .unwrap_or_default();
                clean_url(url)
            })))
            .description(QueryMethod::Manual(Box::new(|el: &ElementRef| {
                let mut description = String::new();
                for inner_node in el
                    .select(
                        &Selector::parse(".b_caption > p, p.b_algoSlug, .b_caption .ipText")
                            .unwrap(),
                    )
                    .next()
                    .map(|n| n.children().collect::<Vec<_>>())
                    .unwrap_or_default()
                {
                    match inner_node.value() {
                        scraper::Node::Text(t) => {
                            description.push_str(&t.text);
                        }
                        scraper::Node::Element(inner_el) => {
                            if !inner_el
                                .has_class("algoSlug_icon", scraper::CaseSensitivity::CaseSensitive)
                            {
                                let element_ref = ElementRef::wrap(inner_node).unwrap();
                                description.push_str(&element_ref.text().collect::<String>());
                            }
                        }
                        _ => {}
                    }
                }

                Ok(description)
            }))),
    )
}

fn clean_url(url: &str) -> anyhow::Result<String> {
    // clean up bing's tracking urls
    if url.starts_with("https://www.bing.com/ck/a?") {
        // get the u param
        let url = Url::parse(url)?;
        let u = url
            .query_pairs()
            .find(|(key, _)| key == "u")
            .unwrap_or_default()
            .1;
        // cut off the "a1" and base64 decode
        let u = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&u[2..])
            .unwrap_or_default();
        // convert to utf8
        Ok(String::from_utf8_lossy(&u).to_string())
    } else {
        Ok(url.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{parse_response, search_url};
    use crate::search::engines::{AllowedDomain, SearchQuery};

    #[test]
    fn adds_allowed_domains_to_the_bing_query() {
        let search = SearchQuery::for_test(
            "Rust language",
            vec![AllowedDomain::parse("docs.rs").unwrap()],
        );
        let url = search_url(&search, "0123456789ABCDEF0123456789ABCDEF");

        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            query.get("q"),
            Some(&"Rust language (site:docs.rs)".to_string())
        );
        assert_eq!(query.get("pq"), query.get("q"));
    }

    #[test]
    fn parses_bing_organic_results_and_tracking_urls() {
        let response = parse_response(
            r#"
            <ol id="b_results">
              <li class="b_algo">
                <h2><a href="https://www.bing.com/ck/a?u=a1aHR0cHM6Ly93d3cucnVzdC1sYW5nLm9yZw">Rust Programming Language</a></h2>
                <div class="b_caption"><p>A language empowering everyone to build reliable software.</p></div>
              </li>
              <li class="b_algo">
                <h2><a href="https://doc.rust-lang.org/book/">The Rust Book</a></h2>
                <div class="b_caption"><p>Official book covering ownership and crates.</p></div>
              </li>
            </ol>
            "#,
        )
        .expect("Bing response parses");

        assert_eq!(response.search_results.len(), 2);
        assert_eq!(
            response.search_results[0].title,
            "Rust Programming Language"
        );
        assert_eq!(response.search_results[0].url, "https://www.rust-lang.org");
        assert_eq!(
            response.search_results[0].description,
            "A language empowering everyone to build reliable software."
        );
        assert_eq!(
            response.search_results[1].url,
            "https://doc.rust-lang.org/book"
        );
    }

    #[test]
    fn skips_bing_results_without_a_description() {
        let response = parse_response(
            r#"
            <ol id="b_results">
              <li class="b_algo">
                <h2><a href="https://example.com/no-snippet">No snippet</a></h2>
              </li>
            </ol>
            "#,
        )
        .expect("Bing response parses");

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_empty_bing_pages_as_no_results() {
        let response = parse_response(r#"<ol id="b_results"></ol>"#).expect("Bing page parses");

        assert!(response.search_results.is_empty());
    }
}
