use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    fmt,
    ops::Deref,
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use futures::future::join_all;
use http_body_util::BodyExt;
use maud::PreEscaped;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info};
use url::{Host, Url};

use crate::browser::BrowserRuntime;
#[cfg(test)]
use crate::outbound::{direct_browser, direct_http_client};

mod macros;
mod ranking;
use crate::search::config::Config;
use crate::{engine_autocomplete_requests, engine_postsearch_requests, engine_requests, engines};

pub mod answer;
pub mod postsearch;
pub mod search;

engines! {
    // search
    Google = "google",
    GoogleScholar = "google_scholar",
    Baidu = "baidu",
    Bing = "bing",
    Brave = "brave",
    So360 = "360",
    SogouWeixin = "sogou_weixin",
    // answer
    Fend = "fend",
    Ip = "ip",
    Notepad = "notepad",
    ColorPicker = "colorpicker",
    Numbat = "numbat",
    Timezone = "timezone",
    Useragent = "useragent",
    Wikipedia = "wikipedia",
    // post-search
    DocsRs = "docs_rs",
    GitHub = "github",
    Mdn = "mdn",
    MinecraftWiki = "minecraft_wiki",
    StackExchange = "stackexchange",
}

engine_requests! {
    // search
    Baidu => search::baidu::request, parse_response,
    Bing => search::bing::request, parse_response,
    Brave => search::brave::request, parse_response,
    GoogleScholar => search::google_scholar::request, parse_response,
    Google => search::google::request, parse_response,
    So360 => search::so::request, parse_response,
    SogouWeixin => search::sogou_weixin::request, parse_response,
    // answer
    Fend => answer::fend::request, None,
    Ip => answer::ip::request, None,
    Notepad => answer::notepad::request, None,
    ColorPicker => answer::colorpicker::request, None,
    Numbat => answer::numbat::request, None,
    Timezone => answer::timezone::request, None,
    Useragent => answer::useragent::request, None,
    Wikipedia => answer::wikipedia::request, parse_response,
}

engine_autocomplete_requests! {
    Baidu => search::baidu::request_autocomplete, parse_autocomplete_response,
    Google => search::google::request_autocomplete, parse_autocomplete_response,
    Fend => answer::fend::request_autocomplete, None,
    Numbat => answer::numbat::request_autocomplete, None,
}

engine_postsearch_requests! {
    DocsRs => postsearch::docs_rs::request, parse_response,
    GitHub => postsearch::github::request, parse_response,
    Mdn => postsearch::mdn::request, parse_response,
    MinecraftWiki => postsearch::minecraft_wiki::request, parse_response,
    StackExchange => postsearch::stackexchange::request, parse_response,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id())
    }
}

impl<'de> Deserialize<'de> for Engine {
    fn deserialize<D>(deserializer: D) -> Result<Engine, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Engine::from_str(&s).map_err(|_| serde::de::Error::custom(format!("invalid engine '{s}'")))
    }
}

pub struct SearchQuery {
    pub query: String,
    /// Domains that must contain every returned search result. An empty list
    /// allows every domain.
    pub allowed_domains: Vec<AllowedDomain>,
    pub request_headers: HashMap<String, String>,
    pub ip: String,
    /// The config is part of the query so it's possible to make a query with a
    /// custom config.
    pub config: Arc<Config>,
    pub http: wreq::Client,
    pub(crate) browser: BrowserRuntime,
}

impl SearchQuery {
    #[cfg(test)]
    pub(crate) fn for_test(query: &str, allowed_domains: Vec<AllowedDomain>) -> Self {
        Self {
            query: query.to_string(),
            allowed_domains,
            request_headers: HashMap::new(),
            ip: String::new(),
            config: Arc::new(Config::default()),
            http: direct_http_client(),
            browser: direct_browser(),
        }
    }
}

/// A validated hostname restriction that matches the hostname itself and all
/// of its subdomains.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AllowedDomain(String);

impl AllowedDomain {
    /// Parses a hostname-only domain restriction.
    ///
    /// Schemes, ports, paths, query strings, fragments, credentials, and IP
    /// addresses are rejected because this type represents a DNS domain, not
    /// a URL prefix.
    pub fn parse(value: &str) -> eyre::Result<Self> {
        if value.is_empty() || value.trim() != value {
            eyre::bail!("allowed domain must be a non-empty hostname");
        }
        if value
            .chars()
            .any(|character| matches!(character, '/' | ':' | '?' | '#' | '@'))
        {
            eyre::bail!("allowed domain must be a hostname without URL components");
        }

        let url = Url::parse(&format!("https://{value}"))?;
        if url.path() != "/"
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            eyre::bail!("allowed domain must be a hostname without URL components");
        }

        let Host::Domain(host) = url
            .host()
            .ok_or_else(|| eyre::eyre!("allowed domain must contain a hostname"))?
        else {
            eyre::bail!("allowed domain must be a DNS hostname");
        };
        if host.starts_with('.') {
            eyre::bail!("allowed domain must not start with a dot");
        }

        Ok(Self(host.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn matches_host(&self, host: &str) -> bool {
        host == self.0
            || host
                .strip_suffix(&self.0)
                .is_some_and(|prefix| prefix.ends_with('.'))
    }
}

impl SearchQuery {
    pub(crate) fn query_with_allowed_domains(&self) -> Cow<'_, str> {
        if self.allowed_domains.is_empty() {
            return Cow::Borrowed(&self.query);
        }

        let site_filters = self
            .allowed_domains
            .iter()
            .map(|domain| format!("site:{}", domain.as_str()))
            .collect::<Vec<_>>()
            .join(" OR ");
        Cow::Owned(format!("{} ({site_filters})", self.query))
    }

    fn filter_allowed_domains(&self, response: &mut EngineResponse) {
        if self.allowed_domains.is_empty() {
            return;
        }

        response
            .search_results
            .retain(|result| self.allows_url(&result.url));
        response.featured_snippet = response
            .featured_snippet
            .take()
            .filter(|snippet| self.allows_url(&snippet.url));
    }

    fn allows_url(&self, value: &str) -> bool {
        let Ok(url) = Url::parse(value) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };

        self.allowed_domains
            .iter()
            .any(|domain| domain.matches_host(host))
    }
}

impl Deref for SearchQuery {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.query
    }
}

pub enum RequestResponse {
    None,
    Http(Box<wreq::RequestBuilder>),
    Instant(Box<EngineResponse>),
}
impl From<wreq::RequestBuilder> for RequestResponse {
    fn from(req: wreq::RequestBuilder) -> Self {
        Self::Http(Box::new(req))
    }
}

trait IntoRequestResponseResult {
    fn into_request_response_result(self) -> eyre::Result<RequestResponse>;
}

impl IntoRequestResponseResult for wreq::RequestBuilder {
    fn into_request_response_result(self) -> eyre::Result<RequestResponse> {
        Ok(RequestResponse::Http(Box::new(self)))
    }
}
impl IntoRequestResponseResult for EngineResponse {
    fn into_request_response_result(self) -> eyre::Result<RequestResponse> {
        Ok(RequestResponse::Instant(Box::new(self)))
    }
}
impl IntoRequestResponseResult for RequestResponse {
    fn into_request_response_result(self) -> eyre::Result<RequestResponse> {
        Ok(self)
    }
}
impl IntoRequestResponseResult for eyre::Result<RequestResponse> {
    fn into_request_response_result(self) -> eyre::Result<RequestResponse> {
        self
    }
}

pub enum RequestAutocompleteResponse {
    Http(Box<wreq::RequestBuilder>),
    Instant(Vec<String>),
}
impl From<wreq::RequestBuilder> for RequestAutocompleteResponse {
    fn from(req: wreq::RequestBuilder) -> Self {
        Self::Http(Box::new(req))
    }
}
impl From<Vec<String>> for RequestAutocompleteResponse {
    fn from(res: Vec<String>) -> Self {
        Self::Instant(res)
    }
}

pub struct HttpResponse {
    pub res: wreq::Response,
    pub body: String,
    pub config: Arc<Config>,
}

impl<'a> From<&'a HttpResponse> for &'a str {
    fn from(res: &'a HttpResponse) -> Self {
        &res.body
    }
}

impl From<HttpResponse> for wreq::Response {
    fn from(res: HttpResponse) -> Self {
        res.res
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineSearchResult {
    pub url: String,
    pub title: String,
    pub description: String,
}

#[derive(Debug)]
pub struct EngineFeaturedSnippet {
    pub url: String,
    pub title: String,
    pub description: String,
}

#[derive(Debug, Default)]
pub struct EngineResponse {
    pub search_results: Vec<EngineSearchResult>,
    pub featured_snippet: Option<EngineFeaturedSnippet>,
    pub answer_html: Option<PreEscaped<String>>,
    pub infobox_html: Option<PreEscaped<String>>,
}

impl EngineResponse {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn answer_html(html: PreEscaped<String>) -> Self {
        Self {
            answer_html: Some(html),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn infobox_html(html: PreEscaped<String>) -> Self {
        Self {
            infobox_html: Some(html),
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub enum EngineProgressUpdate {
    Requesting,
    Downloading,
    Parsing,
    Done,
    Error(String),
}

#[derive(Debug)]
pub enum ProgressUpdateData {
    Engine {
        engine: Engine,
        update: EngineProgressUpdate,
    },
    Response(Response),
    PostSearchInfobox(Infobox),
}

#[derive(Debug)]
pub struct ProgressUpdate {
    pub data: ProgressUpdateData,
    pub time_ms: u64,
}

impl ProgressUpdate {
    #[must_use]
    pub fn new(data: ProgressUpdateData, start_time: Instant) -> Self {
        Self {
            data,
            time_ms: start_time.elapsed().as_millis() as u64,
        }
    }
}

async fn make_request(
    request: wreq::RequestBuilder,
    engine: Engine,
    query: &SearchQuery,
    send_engine_progress_update: impl Fn(Engine, EngineProgressUpdate),
) -> eyre::Result<HttpResponse> {
    send_engine_progress_update(engine, EngineProgressUpdate::Requesting);

    let mut res = request.send().await?;

    send_engine_progress_update(engine, EngineProgressUpdate::Downloading);

    let mut body_bytes = Vec::new();
    while let Some(frame) = res.frame().await {
        if let Ok(chunk) = frame?.into_data() {
            body_bytes.extend_from_slice(&chunk);
        }
    }
    let body = String::from_utf8_lossy(&body_bytes).to_string();

    send_engine_progress_update(engine, EngineProgressUpdate::Parsing);

    let http_response = HttpResponse {
        res,
        body,
        config: query.config.clone(),
    };
    Ok(http_response)
}

async fn make_requests(
    query: &SearchQuery,
    progress_tx: &mpsc::UnboundedSender<ProgressUpdate>,
    start_time: Instant,
    send_engine_progress_update: &impl Fn(Engine, EngineProgressUpdate),
) -> eyre::Result<()> {
    let mut requests = Vec::new();
    for &engine in Engine::all() {
        let engine_config = query.config.engines.get(engine);
        if !engine_config.enabled {
            continue;
        }

        requests.push(async move {
            let request_response = match engine.request(query).await {
                Ok(r) => r,
                Err(e) => {
                    error!("request error for {engine}: {e}");
                    send_engine_progress_update(engine, EngineProgressUpdate::Error(e.to_string()));
                    return Err(e);
                }
            };

            let mut response = match request_response {
                RequestResponse::Http(request) => {
                    let http_response =
                        match make_request(*request, engine, query, send_engine_progress_update)
                            .await
                        {
                            Ok(http_response) => http_response,
                            Err(e) => {
                                send_engine_progress_update(
                                    engine,
                                    EngineProgressUpdate::Error(e.to_string()),
                                );
                                return Err(e);
                            }
                        };

                    let response = match match engine {
                        Engine::Google
                            if search::google::requires_browser_render(&http_response.body) =>
                        {
                            search::google::render_response(query).await
                        }
                        _ => engine.parse_response(&http_response),
                    } {
                        Ok(response) => response,
                        Err(e) => {
                            error!("parse error for {engine}: {e}");
                            send_engine_progress_update(
                                engine,
                                EngineProgressUpdate::Error(e.to_string()),
                            );
                            return Err(e);
                        }
                    };

                    send_engine_progress_update(engine, EngineProgressUpdate::Done);

                    response
                }
                RequestResponse::Instant(response) => *response,
                RequestResponse::None => EngineResponse::new(),
            };
            query.filter_allowed_domains(&mut response);

            Ok((engine, response))
        });
    }

    let mut response_futures = Vec::new();
    for request in requests {
        response_futures.push(request);
    }

    let mut responses = HashMap::new();
    for response_result in join_all(response_futures).await {
        let response_result: eyre::Result<_> = response_result; // this line is necessary to make type inference work
        if let Ok((engine, response)) = response_result {
            responses.insert(engine, response);
        }
    }

    let response =
        ranking::merge_engine_responses(query.config.clone(), responses, query.http.clone());
    let has_infobox = response.infobox.is_some();
    progress_tx.send(ProgressUpdate::new(
        ProgressUpdateData::Response(response.clone()),
        start_time,
    ))?;

    if !has_infobox {
        // post-search

        let mut postsearch_requests = Vec::new();
        for &engine in Engine::all() {
            let engine_config = query.config.engines.get(engine);
            if !engine_config.enabled {
                continue;
            }

            if let Some(request) = engine.postsearch_request(&response).await {
                postsearch_requests.push(async move {
                    let response = match request.send().await {
                        Ok(mut res) => {
                            let mut body_bytes = Vec::new();
                            while let Some(frame) = res.frame().await {
                                if let Ok(chunk) = frame?.into_data() {
                                    body_bytes.extend_from_slice(&chunk);
                                }
                            }
                            let body = String::from_utf8_lossy(&body_bytes).to_string();

                            let http_response = HttpResponse {
                                res,
                                body,
                                config: query.config.clone(),
                            };
                            engine.postsearch_parse_response(&http_response)
                        }
                        Err(e) => {
                            error!("postsearch request error: {e}");
                            None
                        }
                    };
                    Ok((engine, response))
                });
            }
        }

        let mut postsearch_response_futures = Vec::new();
        for request in postsearch_requests {
            postsearch_response_futures.push(request);
        }

        let postsearch_responses_result: eyre::Result<HashMap<_, _>> =
            join_all(postsearch_response_futures)
                .await
                .into_iter()
                .collect();
        let postsearch_responses = postsearch_responses_result?;

        for (engine, response) in postsearch_responses {
            if let Some(html) = response {
                progress_tx.send(ProgressUpdate::new(
                    ProgressUpdateData::PostSearchInfobox(Infobox { html, engine }),
                    start_time,
                ))?;
                // break so we don't send multiple infoboxes
                break;
            }
        }
    }

    Ok(())
}

#[tracing::instrument(fields(query = %query.query), skip(progress_tx))]
pub async fn search(
    query: &SearchQuery,
    progress_tx: mpsc::UnboundedSender<ProgressUpdate>,
) -> eyre::Result<()> {
    let start_time = Instant::now();

    info!("Doing search");

    let progress_tx = &progress_tx;
    let send_engine_progress_update = |engine: Engine, update: EngineProgressUpdate| {
        let _ = progress_tx.send(ProgressUpdate::new(
            ProgressUpdateData::Engine { engine, update },
            start_time,
        ));
    };

    make_requests(query, progress_tx, start_time, &send_engine_progress_update).await?;

    Ok(())
}

pub async fn autocomplete(
    config: &Config,
    query: &str,
    client: &wreq::Client,
) -> eyre::Result<Vec<String>> {
    let mut requests = Vec::new();
    for &engine in Engine::all() {
        let config = config.engines.get(engine);
        if !config.enabled {
            continue;
        }

        if let Some(request) = engine.request_autocomplete(query, client) {
            requests.push(async move {
                let response = match request {
                    RequestAutocompleteResponse::Http(request) => {
                        let res = request.send().await?;
                        let body = res.text().await?;
                        engine.parse_autocomplete_response(&body)?
                    }
                    RequestAutocompleteResponse::Instant(response) => response,
                };
                Ok((engine, response))
            });
        }
    }

    let mut autocomplete_futures = Vec::new();
    for request in requests {
        autocomplete_futures.push(request);
    }

    let autocomplete_results_result: eyre::Result<HashMap<_, _>> =
        join_all(autocomplete_futures).await.into_iter().collect();
    let autocomplete_results = autocomplete_results_result?;

    Ok(ranking::merge_autocomplete_responses(
        config,
        autocomplete_results,
    ))
}

#[derive(Clone, Serialize)]
pub struct Response {
    pub search_results: Vec<SearchResult<EngineSearchResult>>,
    pub featured_snippet: Option<FeaturedSnippet>,
    pub answer: Option<Answer>,
    pub infobox: Option<Infobox>,
    #[serde(skip)]
    pub config: Arc<Config>,
    #[serde(skip)]
    pub http: wreq::Client,
}

impl fmt::Debug for Response {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Response")
            .field("search_results", &self.search_results)
            .field("featured_snippet", &self.featured_snippet)
            .field("answer", &self.answer)
            .field("infobox", &self.infobox)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult<R: Serialize> {
    pub result: R,
    pub engines: BTreeSet<Engine>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeaturedSnippet {
    pub url: String,
    pub title: String,
    pub description: String,
    pub engine: Engine,
}

#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    #[serde(serialize_with = "serialize_markup")]
    pub html: PreEscaped<String>,
    pub engine: Engine,
}

#[derive(Debug, Clone, Serialize)]
pub struct Infobox {
    #[serde(serialize_with = "serialize_markup")]
    pub html: PreEscaped<String>,
    pub engine: Engine,
}

pub struct AutocompleteResult {
    pub query: String,
    pub score: f64,
}

fn serialize_markup<S>(markup: &PreEscaped<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&markup.0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use url::Url;

    use super::{
        AllowedDomain, Engine, EngineFeaturedSnippet, EngineProgressUpdate, EngineResponse,
        EngineSearchResult, ProgressUpdateData, SearchQuery,
    };
    use crate::search::config::{Config, EngineConfig};
    use crate::{LocalWeb, OutboundProxyMode};

    const LIVE_SEARCH_QUERY: &str = "Rust programming language";
    const LIVE_FETCH_URL: &str = "https://www.rust-lang.org/";

    fn search_with_allowed_domains(domains: &[&str]) -> SearchQuery {
        SearchQuery::for_test(
            "Rust language",
            domains
                .iter()
                .map(|domain| AllowedDomain::parse(domain).unwrap())
                .collect(),
        )
    }

    fn config_with_only(engine: Engine) -> Config {
        let mut config = Config::default();
        let mut engines = (*config.engines).clone();
        for &candidate in Engine::all() {
            engines
                .map
                .entry(candidate)
                .or_insert_with(EngineConfig::new)
                .enabled = candidate == engine;
        }
        config.engines = Arc::new(engines);
        config
    }

    fn is_tracking_url(url: &Url) -> bool {
        match url.host_str() {
            Some("www.google.com") if url.path() == "/url" => true,
            Some("www.bing.com") if url.path().starts_with("/ck/") => true,
            Some("www.so.com" | "so.com") if url.path() == "/link" => true,
            Some("weixin.sogou.com") if url.path() == "/link" => true,
            Some("www.baidu.com") if url.path() == "/link" => true,
            _ => false,
        }
    }

    async fn live_engine_results(
        web: &LocalWeb,
        engine: Engine,
    ) -> Result<Vec<(String, String)>, String> {
        let mut search = web.search_query(LIVE_SEARCH_QUERY);
        search.config = Arc::new(config_with_only(engine));
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let search_web = web.clone();
        let handle = tokio::spawn(async move { search_web.search(search, progress_tx).await });

        let mut response = None;
        let mut errors = Vec::new();
        while let Some(update) = progress_rx.recv().await {
            match update.data {
                ProgressUpdateData::Engine {
                    engine: update_engine,
                    update: EngineProgressUpdate::Error(error),
                } if update_engine == engine => errors.push(error),
                ProgressUpdateData::Response(value) => response = Some(value),
                _ => {}
            }
        }
        handle
            .await
            .map_err(|error| format!("{engine} search task failed: {error}"))?
            .map_err(|error| format!("{engine} search failed: {error}"))?;

        let response =
            response.ok_or_else(|| format!("{engine} did not emit a search response"))?;
        if response.search_results.is_empty() {
            if errors.is_empty() {
                return Err(format!("{engine} returned no organic results"));
            }
            return Err(format!("{engine} failed: {}", errors.join("; ")));
        }

        let mut parsed = Vec::new();
        for result in response.search_results {
            let url = Url::parse(&result.result.url).map_err(|error| {
                format!(
                    "{engine} returned an unparsable URL {}: {error}",
                    result.result.url
                )
            })?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(format!(
                    "{engine} returned a non-http URL {}",
                    result.result.url
                ));
            }
            if is_tracking_url(&url) {
                return Err(format!(
                    "{engine} returned a tracking URL {}",
                    result.result.url
                ));
            }
            if result.result.title.trim().is_empty() {
                return Err(format!(
                    "{engine} returned an empty title for {}",
                    result.result.url
                ));
            }
            if engine == Engine::SogouWeixin && url.host_str() != Some("mp.weixin.qq.com") {
                return Err(format!(
                    "{engine} returned a non-WeChat URL {}",
                    result.result.url
                ));
            }
            parsed.push((result.result.url, result.result.title));
        }
        Ok(parsed)
    }

    #[test]
    fn allowed_domains_match_the_domain_and_subdomains() {
        let domain = AllowedDomain::parse("example.com").unwrap();

        assert_eq!(domain.as_str(), "example.com");
        assert!(domain.matches_host("example.com"));
        assert!(domain.matches_host("docs.example.com"));
        assert!(!domain.matches_host("notexample.com"));
        assert!(AllowedDomain::parse("https://example.com").is_err());
        assert!(AllowedDomain::parse("example.com/docs").is_err());
        assert!(AllowedDomain::parse("example.com:443").is_err());
    }

    #[test]
    fn query_with_allowed_domains_uses_site_filters() {
        let search = search_with_allowed_domains(&["docs.rs", "example.com"]);

        assert_eq!(
            search.query_with_allowed_domains(),
            "Rust language (site:docs.rs OR site:example.com)"
        );
    }

    #[test]
    fn filters_results_and_featured_snippets_to_allowed_domains() {
        let search = search_with_allowed_domains(&["example.com"]);
        let mut response = EngineResponse {
            search_results: vec![
                EngineSearchResult {
                    url: "https://docs.example.com/rust".to_string(),
                    title: "Allowed".to_string(),
                    description: String::new(),
                },
                EngineSearchResult {
                    url: "https://notexample.com/rust".to_string(),
                    title: "Blocked".to_string(),
                    description: String::new(),
                },
            ],
            featured_snippet: Some(EngineFeaturedSnippet {
                url: "https://notexample.com/featured".to_string(),
                title: "Blocked featured result".to_string(),
                description: String::new(),
            }),
            ..Default::default()
        };

        search.filter_allowed_domains(&mut response);

        assert_eq!(response.search_results.len(), 1);
        assert_eq!(
            response.search_results[0].url,
            "https://docs.example.com/rust"
        );
        assert!(response.featured_snippet.is_none());
    }

    #[tokio::test]
    #[ignore = "hits live search engines; run with --ignored"]
    async fn live_search_engines_return_parsable_destination_urls() {
        let web = LocalWeb::new(OutboundProxyMode::System).expect("local web runtime");
        let mut failures = Vec::new();
        let mut fetchable_url = None;

        for engine in [
            Engine::Google,
            Engine::Bing,
            Engine::Brave,
            Engine::Baidu,
            Engine::So360,
            Engine::SogouWeixin,
            Engine::GoogleScholar,
        ] {
            match live_engine_results(&web, engine).await {
                Ok(results) => {
                    eprintln!(
                        "{engine}: {} results, first={}",
                        results.len(),
                        results[0].0
                    );
                    if fetchable_url.is_none() && engine != Engine::SogouWeixin {
                        fetchable_url = Some(results[0].0.clone());
                    }
                }
                Err(error) => failures.push(error),
            }
        }

        assert!(
            failures.is_empty(),
            "live search engines failed: {}",
            failures.join(" | ")
        );

        let fetch_url = fetchable_url.expect("at least one non-WeChat search result");
        let page = web
            .fetch(&fetch_url)
            .await
            .unwrap_or_else(|error| panic!("fetching search result {fetch_url} failed: {error}"));
        assert!(
            !page.markdown.trim().is_empty(),
            "fetched search result {fetch_url} had no markdown"
        );
    }

    #[tokio::test]
    #[ignore = "hits a live public page; run with --ignored"]
    async fn live_fetch_extracts_a_public_page() {
        let web = LocalWeb::new(OutboundProxyMode::System).expect("local web runtime");
        let page = web
            .fetch(LIVE_FETCH_URL)
            .await
            .unwrap_or_else(|error| panic!("fetching {LIVE_FETCH_URL} failed: {error}"));

        assert_eq!(page.requested_url, LIVE_FETCH_URL);
        let final_url = Url::parse(&page.final_url).expect("fetched final URL parses");
        assert!(
            matches!(
                final_url.host_str(),
                Some("www.rust-lang.org" | "rust-lang.org")
            ),
            "unexpected final URL {}",
            page.final_url
        );
        assert!(
            !page.markdown.trim().is_empty(),
            "fetched page had no markdown"
        );
        assert!(
            page.markdown.to_ascii_lowercase().contains("rust"),
            "fetched markdown did not contain page content: {}",
            page.markdown.chars().take(200).collect::<String>()
        );
    }
}
