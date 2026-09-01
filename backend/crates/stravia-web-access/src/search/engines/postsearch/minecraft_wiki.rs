use maud::{html, PreEscaped};
use scraper::{Html, Selector};
use url::Url;

use crate::search::engines::{HttpResponse, Response};

pub async fn request(response: &Response) -> Option<wreq::RequestBuilder> {
    for search_result in response.search_results.iter().take(8) {
        if search_result
            .result
            .url
            .starts_with("https://minecraft.wiki/w/")
        {
            return Some(response.http.get(search_result.result.url.as_str()));
        }
    }

    None
}

pub fn parse_response(HttpResponse { res, body, .. }: &HttpResponse) -> Option<PreEscaped<String>> {
    let url = Url::parse(&res.uri().to_string()).ok()?;

    let dom = Html::parse_document(body);

    let page_title = dom
        .select(&Selector::parse("#firstHeading").unwrap())
        .next()?
        .text()
        .collect::<String>()
        .trim()
        .to_string();

    let doc_query = Selector::parse(".mw-parser-output > p").unwrap();

    let doc_html = dom
        .select(&doc_query)
        .next()
        .map(|doc| doc.html())
        .unwrap_or_default();

    let doc_html = ammonia::Builder::default()
        .link_rel(None)
        .add_allowed_classes("div", ["notaninfobox", "mcw-mainpage-icon"])
        .add_allowed_classes("pre", ["noexcerpt", "navigation-not-searchable"])
        .url_relative(ammonia::UrlRelative::RewriteWithBase(url.clone()))
        .clean(&doc_html)
        .to_string();

    Some(html! {
        h2 {
            a href=(url) { (page_title) }
        }
        div.infobox-minecraft_wiki-article {
            (PreEscaped(doc_html))
        }
    })
}
