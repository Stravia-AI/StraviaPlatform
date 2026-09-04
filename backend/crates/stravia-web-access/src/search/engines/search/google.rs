use std::time::Duration;

use scraper::{ElementRef, Selector};
use url::Url;

use crate::{
    browser::RenderRequest,
    search::{
        engines::{EngineResponse, RequestResponse, SearchQuery},
        parse::{parse_html_response_with_opts, ParseOpts, QueryMethod},
    },
};

const GOOGLE_HOME_URL: &str = "https://www.google.com/";
const GOOGLE_RESULT_SELECTOR: &str = "a h3";
const BROWSER_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn request(search: &SearchQuery) -> anyhow::Result<RequestResponse> {
    Ok(search.http.get(search_url(search).as_str()).into())
}

pub(crate) fn requires_browser_render(body: &str) -> bool {
    body.contains("/httpservice/retry/enablejs") && !contains_result_heading(body)
}

pub(crate) async fn render_response(search: &SearchQuery) -> anyhow::Result<EngineResponse> {
    let url = search_url(search);
    let rendered = search
        .browser
        .render(RenderRequest {
            url: url.as_str(),
            preflight_url: Some(GOOGLE_HOME_URL),
            ready_selector: GOOGLE_RESULT_SELECTOR,
            timeout: BROWSER_RENDER_TIMEOUT,
            request_guard: None,
        })
        .await
        .map_err(|error| anyhow::anyhow!("Google browser renderer failed: {error}"))?;
    let body = rendered.html;

    if !rendered.ready {
        if requires_browser_render(&body) {
            anyhow::bail!("Google returned its JavaScript challenge after browser rendering");
        }
        if is_traffic_challenge(&body) {
            anyhow::bail!("Google blocked browser rendering with an automated-traffic challenge");
        }
        anyhow::bail!(
            "Google search results did not render from {} within {} seconds",
            rendered.url,
            BROWSER_RENDER_TIMEOUT.as_secs()
        );
    }
    if is_traffic_challenge(&body) {
        anyhow::bail!("Google blocked browser rendering with an automated-traffic challenge");
    }

    parse_response(&body)
}

fn search_url(search: &SearchQuery) -> Url {
    let query = search.query_with_allowed_domains();
    Url::parse_with_params(
        "https://www.google.com/search",
        &[
            ("q", query.as_ref()),
            // nfpr makes it not try to autocorrect
            ("nfpr", "1"),
            ("filter", "0"),
            ("start", "0"),
            ("hl", "en"),
            ("gl", "us"),
            ("udm", "14"),
            ("pws", "0"),
        ],
    )
    .expect("Google search URL is valid")
}

fn is_traffic_challenge(body: &str) -> bool {
    !contains_result_heading(body)
        && (body.contains("/sorry/")
            || body.contains("unusual traffic")
            || body.contains("detected unusual traffic")
            || body.contains("g-recaptcha"))
}

fn contains_result_heading(body: &str) -> bool {
    body.contains("<h3") || body.contains("<H3")
}

pub fn parse_response(body: &str) -> anyhow::Result<EngineResponse> {
    parse_html_response_with_opts(
        body,
        ParseOpts::new()
            // xpd is weird, some results have it but it's usually used for ads?
            // the :first-child filters out the ads though since for ads the first child is always a
            // span
            .result("[jscontroller=SC7lYd]")
            .title("h3")
            .href(QueryMethod::Manual(Box::new(|el: &ElementRef| {
                let url = el
                    .select(&Selector::parse("a[href]").unwrap())
                    .next()
                    .and_then(|n| n.value().attr("href"))
                    .unwrap_or_default();
                clean_url(url)
            })))
            .description(
                "div[data-sncf='2'], div[data-sncf='1,2'], div[style='-webkit-line-clamp:2']",
            )
            .featured_snippet("block-component")
            .featured_snippet_description(QueryMethod::Manual(Box::new(|el: &ElementRef| {
                let mut description = String::new();

                // role="heading"
                if let Some(heading_el) = el
                    .select(&Selector::parse("div[role='heading']").unwrap())
                    .next()
                {
                    description.push_str(&format!("{}\n\n", heading_el.text().collect::<String>()));
                }

                if let Some(description_container_el) = el
                    .select(&Selector::parse("div[data-attrid='wa:/description'] > span:first-child").unwrap())
                    .next()
                {
                    description.push_str(&iter_featured_snippet_children(&description_container_el));
                }
                else if let Some(description_list_el) = el
                    .select(&Selector::parse("ul").unwrap())
                    .next()
                {
                    // render as bullet points
                    for li in description_list_el.select(&Selector::parse("li").unwrap()) {
                        let text = li.text().collect::<String>();
                        description.push_str(&format!("• {text}\n"));
                    }
                }

                Ok(description)
            })))
            .featured_snippet_title(".g > div[lang] a h3, div[lang] > div[style='position:relative'] a h3")
            .featured_snippet_href(QueryMethod::Manual(Box::new(|el: &ElementRef| {
                let url = el
                    .select(&Selector::parse(".g > div[lang] a:has(h3), div[lang] > div[style='position:relative'] a:has(h3)").unwrap())
                    .next()
                    .and_then(|n| n.value().attr("href"))
                    .unwrap_or_default();
                clean_url(url)
            }))),
    )
}

// Google autocomplete responses sometimes include clickable links that include
// text that we shouldn't show.
// We can filter for these by removing any elements matching
// [data-ved]:not([data-send-open-event])
fn iter_featured_snippet_children(el: &ElementRef) -> String {
    let mut description = String::new();
    recursive_iter_featured_snippet_children(&mut description, el);
    description
}
fn recursive_iter_featured_snippet_children(description: &mut String, el: &ElementRef) {
    for inner_node in el.children() {
        match inner_node.value() {
            scraper::Node::Text(t) => {
                description.push_str(&t.text);
            }
            scraper::Node::Element(inner_el) => {
                if inner_el.attr("data-ved").is_none()
                    || inner_el.attr("data-send-open-event").is_some()
                {
                    recursive_iter_featured_snippet_children(
                        description,
                        &ElementRef::wrap(inner_node).unwrap(),
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_traffic_challenge, parse_response, requires_browser_render, search_url};
    use crate::search::engines::{AllowedDomain, SearchQuery};

    fn search_with_allowed_domain() -> SearchQuery {
        SearchQuery::for_test(
            "Rust language",
            vec![AllowedDomain::parse("docs.rs").unwrap()],
        )
    }

    #[test]
    fn adds_allowed_domains_to_the_google_query() {
        let url = search_url(&search_with_allowed_domain());

        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "q").unwrap().1,
            "Rust language (site:docs.rs)"
        );
    }

    #[test]
    fn recognizes_google_enablejs_challenge_without_results() {
        assert!(requires_browser_render(
            r#"<noscript><meta http-equiv="refresh" content="0;url=/httpservice/retry/enablejs"></noscript>"#,
        ));
    }

    #[test]
    fn does_not_render_normal_google_results() {
        assert!(!requires_browser_render(
            r#"<div jscontroller="SC7lYd"><a href="https://example.com"><h3>Result</h3></a></div>"#,
        ));
    }

    #[test]
    fn does_not_discard_results_that_contain_a_sorry_url() {
        assert!(!is_traffic_challenge(
            r#"<a href="/sorry/"><h3>Search result</h3></a>"#,
        ));
    }

    #[test]
    fn parses_google_organic_results_and_tracking_urls() {
        let response = parse_response(
            r#"
            <div jscontroller="SC7lYd">
              <a href="/url?q=https://www.rust-lang.org/&amp;sa=U">
                <h3>Rust Programming Language</h3>
              </a>
              <div data-sncf="2">A language empowering everyone to build reliable software.</div>
            </div>
            <div jscontroller="SC7lYd">
              <a href="https://doc.rust-lang.org/book/">
                <h3>The Rust Programming Language</h3>
              </a>
              <div data-sncf="1,2">The official book covering ownership, borrowing, and crates.</div>
            </div>
            "#,
        )
        .expect("Google response parses");

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
    fn skips_google_results_without_a_description() {
        let response = parse_response(
            r#"
            <div jscontroller="SC7lYd">
              <a href="https://example.com/no-snippet"><h3>No snippet</h3></a>
            </div>
            "#,
        )
        .expect("Google response parses");

        assert!(response.search_results.is_empty());
    }

    #[test]
    fn parses_google_challenge_pages_as_no_results() {
        let response = parse_response(
            r#"<noscript><meta http-equiv="refresh" content="0;url=/httpservice/retry/enablejs"></noscript>"#,
        )
        .expect("Google challenge page parses");

        assert!(response.search_results.is_empty());
        assert!(response.featured_snippet.is_none());
    }
}

pub fn request_autocomplete(query: &str, client: &wreq::Client) -> wreq::RequestBuilder {
    let url = Url::parse_with_params(
        "https://suggestqueries.google.com/complete/search",
        &[
            ("output", "firefox"),
            ("client", "firefox"),
            ("hl", "US-en"),
            ("q", query),
        ],
    )
    .unwrap();
    client.get(url.as_str())
}

pub fn parse_autocomplete_response(body: &str) -> anyhow::Result<Vec<String>> {
    let res = serde_json::from_str::<Vec<serde_json::Value>>(body)?;
    Ok(res
        .into_iter()
        .nth(1)
        .unwrap_or_default()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect())
}

fn clean_url(url: &str) -> anyhow::Result<String> {
    if url.starts_with("/url?q=") {
        // get the q param
        let url = Url::parse(format!("https://www.google.com{url}").as_str())?;
        let q = url
            .query_pairs()
            .find(|(key, _)| key == "q")
            .unwrap_or_default()
            .1;
        Ok(q.to_string())
    } else {
        Ok(url.to_string())
    }
}
