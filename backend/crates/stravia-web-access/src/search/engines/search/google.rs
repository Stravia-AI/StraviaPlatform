use std::time::Duration;

use futures::future::join_all;
use scraper::{ElementRef, Selector};
use url::Url;

use crate::{
    renderer::{RenderRequest, RenderRequestPolicy},
    search::{
        engines::{EngineResponse, RequestResponse, SearchQuery},
        parse::{parse_html_response_with_opts, ParseOpts, QueryMethod},
        urls::normalize_url,
    },
};

const GOOGLE_HOME_URL: &str = "https://www.google.com/";
const GOOGLE_READY_SELECTOR: &str = "a h3, div[role='heading'][aria-level='2']";
const GOOGLE_NO_RESULTS_MESSAGE: &str = "Your search did not match any documents";
const BROWSER_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn request(search: &SearchQuery) -> eyre::Result<RequestResponse> {
    Ok(search.http.get(search_url(search).as_str()).into())
}

pub(crate) fn requires_browser_render(body: &str) -> bool {
    body.contains("/httpservice/retry/enablejs")
        && !contains_result_heading(body)
        && !is_no_results_page(body)
}

pub(crate) async fn render_response(search: &SearchQuery) -> eyre::Result<EngineResponse> {
    let url = search_url(search);
    let rendered = search
        .renderer
        .render(RenderRequest {
            url: url.as_str(),
            preflight_url: Some(GOOGLE_HOME_URL),
            ready_selector: GOOGLE_READY_SELECTOR,
            timeout: BROWSER_RENDER_TIMEOUT,
            request_policy: RenderRequestPolicy::Unrestricted,
        })
        .await
        .map_err(|error| eyre::eyre!("Google browser renderer failed: {error}"))?;
    let body = rendered.html;

    if is_no_results_page(&body) {
        return parse_response(&body);
    }
    if !rendered.ready {
        if requires_browser_render(&body) {
            eyre::bail!("Google returned its JavaScript challenge after browser rendering");
        }
        if is_traffic_challenge(&body) {
            eyre::bail!("Google blocked browser rendering with an automated-traffic challenge");
        }
        eyre::bail!(
            "Google search results did not render from {} within {} seconds",
            rendered.url,
            BROWSER_RENDER_TIMEOUT.as_secs()
        );
    }
    if is_traffic_challenge(&body) {
        eyre::bail!("Google blocked browser rendering with an automated-traffic challenge");
    }

    let response = parse_response(&body)?;
    resolve_google_redirects(&search.http, response).await
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
        && !is_no_results_page(body)
        && (body.contains("/sorry/")
            || body.contains("unusual traffic")
            || body.contains("detected unusual traffic")
            || body.contains("g-recaptcha"))
}

fn is_no_results_page(body: &str) -> bool {
    !contains_result_heading(body) && body.contains(GOOGLE_NO_RESULTS_MESSAGE)
}

fn contains_result_heading(body: &str) -> bool {
    body.contains("<h3") || body.contains("<H3")
}

#[derive(Clone, Copy)]
enum RedirectSlot {
    SearchResult(usize),
    FeaturedSnippet,
}

async fn resolve_google_redirects(
    client: &wreq::Client,
    mut response: EngineResponse,
) -> eyre::Result<EngineResponse> {
    let mut redirects = response
        .search_results
        .iter()
        .enumerate()
        .filter(|(_, result)| is_google_goto_url(&result.url))
        .map(|(index, result)| (RedirectSlot::SearchResult(index), result.url.clone()))
        .collect::<Vec<_>>();
    if let Some(featured_snippet) = &response.featured_snippet {
        if is_google_goto_url(&featured_snippet.url) {
            redirects.push((RedirectSlot::FeaturedSnippet, featured_snippet.url.clone()));
        }
    }

    let resolutions = join_all(redirects.into_iter().map(|(slot, url)| async move {
        resolve_google_redirect(client, &url)
            .await
            .map(|resolved| (slot, resolved))
    }))
    .await;
    for resolution in resolutions {
        let (slot, resolved) = resolution?;
        match slot {
            RedirectSlot::SearchResult(index) => {
                response.search_results[index].url = resolved;
            }
            RedirectSlot::FeaturedSnippet => {
                response
                    .featured_snippet
                    .as_mut()
                    .expect("featured snippet redirect still has its result")
                    .url = resolved;
            }
        }
    }

    Ok(response)
}

fn is_google_goto_url(url: &str) -> bool {
    url.starts_with("https://www.google.com/goto?url=")
}

async fn resolve_google_redirect(client: &wreq::Client, url: &str) -> eyre::Result<String> {
    let response = client
        .get(url)
        .redirect(wreq::redirect::Policy::none())
        .send()
        .await
        .map_err(|error| eyre::eyre!("Google result redirect request failed: {error}"))?;
    if !response.status().is_redirection() {
        eyre::bail!("Google result redirect returned HTTP {}", response.status());
    }
    let location = response
        .headers()
        .get(wreq::header::LOCATION)
        .ok_or_else(|| eyre::eyre!("Google result redirect omitted Location"))?
        .to_str()
        .map_err(|error| eyre::eyre!("Google result redirect Location was invalid: {error}"))?;
    let target = Url::parse(location)
        .map_err(|error| eyre::eyre!("Google result redirect target was invalid: {error}"))?;
    if !matches!(target.scheme(), "http" | "https") {
        eyre::bail!(
            "Google result redirect used unsupported scheme {}",
            target.scheme()
        );
    }

    Ok(normalize_url(target.as_str()))
}

pub fn parse_response(body: &str) -> eyre::Result<EngineResponse> {
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
    use super::{
        clean_url, is_traffic_challenge, parse_response, requires_browser_render,
        resolve_google_redirect, search_url,
    };
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
    fn does_not_treat_rendered_zero_results_as_a_challenge() {
        let body = r#"
            <noscript>
              <meta http-equiv="refresh" content="0;url=/httpservice/retry/enablejs">
            </noscript>
            <script>const trafficPath = "/sorry/index";</script>
            <div role="heading" aria-level="2">Your search did not match any documents</div>
        "#;

        assert!(!requires_browser_render(body));
        assert!(!is_traffic_challenge(body));
    }

    #[test]
    fn makes_google_goto_urls_absolute_for_resolution() {
        assert_eq!(
            clean_url("/goto?url=opaque-token").unwrap(),
            "https://www.google.com/goto?url=opaque-token"
        );
    }

    #[tokio::test]
    async fn resolves_google_goto_without_requesting_the_destination() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let destination = format!("http://{addr}/destination");
        tokio::spawn({
            let destination = destination.clone();
            async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let destination = destination.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0; 1024];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]);
                        let response = if request.starts_with("GET /goto?") {
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_string()
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });

        let client = wreq::Client::new();
        let redirect = format!("http://{addr}/goto?url=opaque-token");
        let resolved = resolve_google_redirect(&client, &redirect).await.unwrap();

        assert_eq!(resolved, format!("https://{}/destination", addr));
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

pub fn parse_autocomplete_response(body: &str) -> eyre::Result<Vec<String>> {
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

fn clean_url(url: &str) -> eyre::Result<String> {
    if url.starts_with("/goto?url=") {
        Ok(format!("https://www.google.com{url}"))
    } else if url.starts_with("/url?q=") {
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
