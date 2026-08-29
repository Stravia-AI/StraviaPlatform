use futures::{StreamExt, stream};
use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};
use serde_json::{Value, json};

use super::engine::{
    AdapterSuccess, ProviderFailure, ProviderUsage, WebProviderAdapter, failed_fetch_result,
};
use super::{
    FetchRequest, FetchResult, FetchStatus, SearchMode, SearchRequest, SearchResponse,
    SearchResult, WebAccessErrorCode,
};

pub(super) enum AdapterConfig {
    Exa { id: String, api_key: String },
    Brave { id: String, api_key: String },
    Tavily { id: String, api_key: String },
    Zhipu { id: String, api_key: String },
}

const EXA_MAX_CONTENT_CHARACTERS: usize = 10_000;
const ZHIPU_SEARCH_MCP_ENDPOINT: &str = "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp";
const ZHIPU_READER_MCP_ENDPOINT: &str = "https://open.bigmodel.cn/api/mcp/web_reader/mcp";
const ZHIPU_SEARCH_TOOL: &str = "web_search_prime";
const ZHIPU_READER_TOOL: &str = "webReader";

pub(super) fn build_adapter(
    client: reqwest::Client,
    config: AdapterConfig,
) -> Arc<dyn WebProviderAdapter> {
    match config {
        AdapterConfig::Exa { id, api_key } => Arc::new(ExaAdapter {
            id,
            client,
            api_key,
        }),
        AdapterConfig::Brave { id, api_key } => Arc::new(BraveAdapter {
            id,
            client,
            api_key,
        }),
        AdapterConfig::Tavily { id, api_key } => Arc::new(TavilyAdapter {
            id,
            client,
            api_key,
        }),
        AdapterConfig::Zhipu { id, api_key } => Arc::new(ZhipuAdapter {
            id,
            client,
            api_key,
            search_endpoint: ZHIPU_SEARCH_MCP_ENDPOINT.into(),
            reader_endpoint: ZHIPU_READER_MCP_ENDPOINT.into(),
        }),
    }
}

struct ExaAdapter {
    id: String,
    client: reqwest::Client,
    api_key: String,
}

#[async_trait::async_trait]
impl WebProviderAdapter for ExaAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        true
    }

    fn supports_fetch(&self) -> bool {
        true
    }

    async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        let mut body = json!({
            "query": request.query,
            "numResults": request.max_results,
            "type": "auto",
            "contents": { "text": { "maxCharacters": 2_000 } }
        });
        if !request.allowed_domains.is_empty() {
            body["includeDomains"] = json!(request.allowed_domains);
        }
        if !request.blocked_domains.is_empty() {
            body["excludeDomains"] = json!(request.blocked_domains);
        }
        let payload = send_json(
            self.client
                .post("https://api.exa.ai/search")
                .header("x-api-key", &self.api_key)
                .json(&body),
        )
        .await?;
        let values = payload
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_response("Exa Search response is missing results"))?;
        let results = values
            .iter()
            .filter_map(|item| {
                Some(SearchResult {
                    url: http_url_field(item, "url")?.to_string(),
                    title: optional_string(item, "title"),
                    snippet: optional_string(item, "text").or_else(|| {
                        item.get("highlights")
                            .and_then(Value::as_array)
                            .and_then(|values| values.first())
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                    }),
                })
            })
            .take(request.max_results)
            .collect::<Vec<_>>();
        if !values.is_empty() && results.is_empty() {
            return Err(invalid_response("Exa Search results contain no valid URLs"));
        }
        Ok(AdapterSuccess::new(
            index_response(request, results),
            ProviderUsage::from_payload(&payload),
        ))
    }

    async fn fetch(
        &self,
        request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        let payload = send_json(
            self.client
                .post("https://api.exa.ai/contents")
                .header("x-api-key", &self.api_key)
                .json(&json!({
                    "ids": request.urls,
                    "text": { "maxCharacters": request.max_characters.min(EXA_MAX_CONTENT_CHARACTERS) }
                })),
        )
        .await?;
        let mut results = normalize_fetch_payload(
            request,
            payload.get("results").and_then(Value::as_array),
            "text",
        );
        if request.max_characters > EXA_MAX_CONTENT_CHARACTERS {
            for result in &mut results {
                if result.status == FetchStatus::Success
                    && result.content.as_ref().is_some_and(|content| {
                        content.chars().count() >= EXA_MAX_CONTENT_CHARACTERS
                    })
                {
                    result.truncated = true;
                }
            }
        }
        Ok(AdapterSuccess::new(
            results,
            ProviderUsage::from_payload(&payload),
        ))
    }
}

struct BraveAdapter {
    id: String,
    client: reqwest::Client,
    api_key: String,
}

#[async_trait::async_trait]
impl WebProviderAdapter for BraveAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        true
    }

    fn supports_fetch(&self) -> bool {
        false
    }

    async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        let query = brave_query(request)?;
        let mut url = reqwest::Url::parse("https://api.search.brave.com/res/v1/web/search")
            .map_err(|_| invalid_response("invalid Brave endpoint"))?;
        url.query_pairs_mut()
            .append_pair("q", &query)
            .append_pair("count", &request.max_results.to_string());
        let payload = send_json(
            self.client
                .get(url)
                .header("x-subscription-token", &self.api_key)
                .header("accept", "application/json"),
        )
        .await?;
        let values = payload
            .pointer("/web/results")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_response("Brave Search response is missing web.results"))?;
        let results = values
            .iter()
            .filter_map(|item| {
                Some(SearchResult {
                    url: http_url_field(item, "url")?.to_string(),
                    title: optional_string(item, "title"),
                    snippet: optional_string(item, "description"),
                })
            })
            .take(request.max_results)
            .collect::<Vec<_>>();
        if !values.is_empty() && results.is_empty() {
            return Err(invalid_response(
                "Brave Search results contain no valid URLs",
            ));
        }
        Ok(AdapterSuccess::new(
            index_response(request, results),
            ProviderUsage::from_payload(&payload),
        ))
    }

    async fn fetch(
        &self,
        _request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        Err(ProviderFailure::new(
            WebAccessErrorCode::Unsupported,
            "Brave does not support Web Fetch",
        ))
    }
}

fn brave_query(request: &SearchRequest) -> Result<String, ProviderFailure> {
    let mut query = request.query.clone();
    if !request.allowed_domains.is_empty() {
        query.push_str(" (");
        for (index, domain) in request.allowed_domains.iter().enumerate() {
            if index > 0 {
                query.push_str(" OR ");
            }
            query.push_str("site:");
            query.push_str(domain);
        }
        query.push(')');
    }
    for domain in &request.blocked_domains {
        query.push_str(" -site:");
        query.push_str(domain);
    }
    if query.chars().count() > 400 || query.split_whitespace().count() > 50 {
        return Err(ProviderFailure::new(
            WebAccessErrorCode::Unsupported,
            "Brave query limits exceeded",
        ));
    }
    Ok(query)
}

struct TavilyAdapter {
    id: String,
    client: reqwest::Client,
    api_key: String,
}

#[async_trait::async_trait]
impl WebProviderAdapter for TavilyAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        true
    }

    fn supports_fetch(&self) -> bool {
        true
    }

    async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        let mut body = json!({
            "query": request.query,
            "max_results": request.max_results,
            "search_depth": "basic",
            "include_answer": false,
            "include_raw_content": false
        });
        if !request.allowed_domains.is_empty() {
            body["include_domains"] = json!(request.allowed_domains);
        }
        if !request.blocked_domains.is_empty() {
            body["exclude_domains"] = json!(request.blocked_domains);
        }
        let payload = send_json(
            self.client
                .post("https://api.tavily.com/search")
                .bearer_auth(&self.api_key)
                .json(&body),
        )
        .await?;
        let values = payload
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_response("Tavily Search response is missing results"))?;
        let results = values
            .iter()
            .filter_map(|item| {
                Some(SearchResult {
                    url: http_url_field(item, "url")?.to_string(),
                    title: optional_string(item, "title"),
                    snippet: optional_string(item, "content"),
                })
            })
            .take(request.max_results)
            .collect::<Vec<_>>();
        if !values.is_empty() && results.is_empty() {
            return Err(invalid_response(
                "Tavily Search results contain no valid URLs",
            ));
        }
        Ok(AdapterSuccess::new(
            index_response(request, results),
            ProviderUsage::from_payload(&payload),
        ))
    }

    async fn fetch(
        &self,
        request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        let payload = send_json(
            self.client
                .post("https://api.tavily.com/extract")
                .bearer_auth(&self.api_key)
                .json(&json!({
                    "urls": request.urls,
                    "format": "markdown",
                    "include_images": false
                })),
        )
        .await?;
        Ok(AdapterSuccess::new(
            normalize_fetch_payload(
                request,
                payload.get("results").and_then(Value::as_array),
                "raw_content",
            ),
            ProviderUsage::from_payload(&payload),
        ))
    }
}

struct ZhipuAdapter {
    id: String,
    client: reqwest::Client,
    api_key: String,
    search_endpoint: String,
    reader_endpoint: String,
}

type ZhipuMcpService = RunningService<RoleClient, ClientInfo>;

#[async_trait::async_trait]
impl WebProviderAdapter for ZhipuAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        true
    }

    fn supports_fetch(&self) -> bool {
        true
    }

    async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        let service = connect_zhipu_mcp(&self.client, &self.search_endpoint, &self.api_key).await?;
        let arguments = zhipu_search_arguments(request);
        let result = service
            .call_tool(CallToolRequestParams::new(ZHIPU_SEARCH_TOOL).with_arguments(arguments))
            .await;
        let _ = service.cancel().await;
        let result = result.map_err(|error| {
            zhipu_transport_failure("Zhipu Search MCP tool call failed", &error.to_string())
        })?;
        let payload = zhipu_tool_payload(&result, "Zhipu Search")?;
        Ok(AdapterSuccess::new(
            normalize_zhipu_search(request, &payload)?,
            None,
        ))
    }

    async fn fetch(
        &self,
        request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        let service = connect_zhipu_mcp(&self.client, &self.reader_endpoint, &self.api_key).await?;
        let mut results = stream::iter(request.urls.iter().cloned().enumerate())
            .map(|(index, url)| {
                let service = &service;
                async move {
                    (
                        index,
                        call_zhipu_reader(service, url, request.max_characters).await,
                    )
                }
            })
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
        let _ = service.cancel().await;
        results.sort_unstable_by_key(|(index, _)| *index);
        Ok(AdapterSuccess::new(
            results.into_iter().map(|(_, result)| result).collect(),
            None,
        ))
    }
}

async fn connect_zhipu_mcp(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
) -> Result<ZhipuMcpService, ProviderFailure> {
    let config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_string())
        .auth_header(api_key.to_string());
    let transport = StreamableHttpClientTransport::with_client(client.clone(), config);
    let mut client_info = ClientInfo::default();
    client_info.protocol_version = ProtocolVersion::V_2024_11_05;
    client_info
        .serve_with_lifecycle(transport, ClientLifecycleMode::Initialize)
        .await
        .map_err(|error| zhipu_transport_failure("Zhipu MCP connection failed", &error.to_string()))
}

fn zhipu_search_arguments(request: &SearchRequest) -> serde_json::Map<String, Value> {
    let Value::Object(mut arguments) = json!({
        "search_query": request.query,
        "search_recency_filter": "noLimit",
        "content_size": "medium",
        "location": "us"
    }) else {
        unreachable!("Zhipu Search arguments are an object");
    };
    if let [domain] = request.allowed_domains.as_slice() {
        arguments.insert("search_domain_filter".into(), Value::String(domain.clone()));
    }
    arguments
}

fn zhipu_reader_arguments(url: &str) -> serde_json::Map<String, Value> {
    let Value::Object(arguments) = json!({
        "url": url,
        "timeout": 20,
        "no_cache": false,
        "return_format": "markdown",
        "retain_images": false,
        "no_gfm": false,
        "keep_img_data_url": false,
        "with_images_summary": false,
        "with_links_summary": false
    }) else {
        unreachable!("Zhipu Reader arguments are an object");
    };
    arguments
}

fn normalize_zhipu_search(
    request: &SearchRequest,
    payload: &Value,
) -> Result<SearchResponse, ProviderFailure> {
    let values = payload
        .as_array()
        .ok_or_else(|| invalid_response("Zhipu Search response is not an array"))?;
    let results = values
        .iter()
        .filter_map(|item| {
            Some(SearchResult {
                url: http_url_field(item, "link")?.to_string(),
                title: optional_string(item, "title"),
                snippet: optional_string(item, "content"),
            })
        })
        .take(request.max_results)
        .collect::<Vec<_>>();
    if !values.is_empty() && results.is_empty() {
        return Err(invalid_response(
            "Zhipu Search results contain no valid URLs",
        ));
    }
    Ok(index_response(request, results))
}

async fn call_zhipu_reader(
    service: &ZhipuMcpService,
    url: String,
    max_characters: usize,
) -> FetchResult {
    let arguments = zhipu_reader_arguments(&url);
    let result = service
        .call_tool(CallToolRequestParams::new(ZHIPU_READER_TOOL).with_arguments(arguments))
        .await
        .map_err(|error| {
            zhipu_transport_failure("Zhipu Reader MCP tool call failed", &error.to_string())
        })
        .and_then(|result| {
            let payload = zhipu_tool_payload(&result, "Zhipu Reader")?;
            normalize_zhipu_fetch(&url, max_characters, &payload)
        });
    match result {
        Ok(result) => result,
        Err(failure) => failed_fetch_result(url, failure.code, Some(failure.message)),
    }
}

fn normalize_zhipu_fetch(
    url: &str,
    max_characters: usize,
    payload: &Value,
) -> Result<FetchResult, ProviderFailure> {
    let content = string_field(payload, "content")
        .ok_or_else(|| invalid_response("Zhipu Reader response is missing content"))?;
    let truncated = content.chars().count() > max_characters;
    let content = if truncated {
        content.chars().take(max_characters).collect()
    } else {
        content.to_string()
    };
    Ok(FetchResult {
        url: url.to_string(),
        status: FetchStatus::Success,
        content: Some(content),
        format: Some("markdown".into()),
        title: optional_string(payload, "title"),
        truncated,
        error: None,
    })
}

fn zhipu_tool_payload(result: &CallToolResult, operation: &str) -> Result<Value, ProviderFailure> {
    let text = result
        .content
        .iter()
        .find_map(|content| content.as_text())
        .map(|content| content.text.trim())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid_response(format!("{operation} response contains no text")))?;
    if result.is_error.unwrap_or(false) {
        return Err(zhipu_transport_failure(
            &format!("{operation} failed"),
            text,
        ));
    }
    let mut payload = serde_json::from_str::<Value>(text)
        .map_err(|_| invalid_response(format!("{operation} returned invalid JSON")))?;
    for _ in 0..2 {
        let Some(encoded) = payload.as_str() else {
            break;
        };
        payload = serde_json::from_str(encoded)
            .map_err(|_| invalid_response(format!("{operation} returned invalid nested JSON")))?;
    }
    Ok(payload)
}

fn zhipu_transport_failure(message: &str, detail: &str) -> ProviderFailure {
    let normalized = detail.to_ascii_lowercase();
    let code = if normalized.contains("429")
        || normalized.contains("rate limit")
        || normalized.contains("quota")
        || normalized.contains("余额")
    {
        WebAccessErrorCode::RateLimited
    } else if normalized.contains("timeout") || normalized.contains("timed out") {
        WebAccessErrorCode::Timeout
    } else {
        WebAccessErrorCode::Unavailable
    };
    ProviderFailure::new(code, message)
}

async fn send_json(builder: reqwest::RequestBuilder) -> Result<Value, ProviderFailure> {
    let response = builder.send().await.map_err(classify_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProviderFailure::new(
            if status.as_u16() == 429 {
                WebAccessErrorCode::RateLimited
            } else {
                WebAccessErrorCode::Unavailable
            },
            format!("Web Provider returned HTTP {}", status.as_u16()),
        ));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| invalid_response("Web Provider returned invalid JSON"))
}

fn classify_transport_error(error: reqwest::Error) -> ProviderFailure {
    ProviderFailure::new(
        if error.is_timeout() {
            WebAccessErrorCode::Timeout
        } else {
            WebAccessErrorCode::Unavailable
        },
        "Web Provider request failed",
    )
}

fn invalid_response(message: impl Into<String>) -> ProviderFailure {
    ProviderFailure::new(WebAccessErrorCode::Unavailable, message)
}

fn index_response(request: &SearchRequest, results: Vec<SearchResult>) -> SearchResponse {
    SearchResponse {
        mode: SearchMode::Index,
        query: request.query.clone(),
        results,
        answer: None,
        citations: None,
    }
}

fn normalize_fetch_payload(
    request: &FetchRequest,
    values: Option<&Vec<Value>>,
    content_field: &str,
) -> Vec<FetchResult> {
    let mut by_url: HashMap<String, Vec<&Value>> = HashMap::new();
    if let Some(values) = values {
        for item in values {
            let Some(url) = string_field(item, "url").or_else(|| string_field(item, "id")) else {
                continue;
            };
            by_url.entry(url.to_string()).or_default().push(item);
        }
    }
    let mut positions = HashMap::<String, usize>::new();
    request
        .urls
        .iter()
        .map(|url| {
            let position = {
                let next = positions.entry(url.clone()).or_insert(0);
                let position = *next;
                *next += 1;
                position
            };
            let Some(items) = by_url.get(url) else {
                return failed_fetch_result(url.clone(), WebAccessErrorCode::Unavailable, None);
            };
            let Some(item) = items.get(position).or_else(|| items.last()) else {
                return failed_fetch_result(url.clone(), WebAccessErrorCode::Unavailable, None);
            };
            let Some(content) = string_field(item, content_field) else {
                return failed_fetch_result(url.clone(), WebAccessErrorCode::Unavailable, None);
            };
            FetchResult {
                url: url.clone(),
                status: FetchStatus::Success,
                content: Some(content.to_string()),
                format: Some("markdown".into()),
                title: optional_string(item, "title"),
                truncated: false,
                error: None,
            }
        })
        .collect()
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn http_url_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    string_field(value, field).and_then(strict_http_url)
}

fn strict_http_url(url: &str) -> Option<&str> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(url)
}

fn optional_string(value: &Value, field: &str) -> Option<String> {
    string_field(value, field).map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brave_rewrite_refuses_over_limit_filters() {
        let request = SearchRequest {
            query: "Rust".into(),
            max_results: 5,
            allowed_domains: (0..20)
                .map(|index| format!("very-long-domain-{index}.example.com"))
                .collect(),
            blocked_domains: vec![],
        };
        assert!(matches!(
            brave_query(&request),
            Err(ProviderFailure {
                code: WebAccessErrorCode::Unsupported,
                ..
            })
        ));
    }

    #[test]
    fn fetch_normalization_reuses_deduplicated_upstream_results() {
        let url = "https://example.com/".to_string();
        let request = FetchRequest {
            urls: vec![url.clone(), url.clone()],
            max_characters: 1_000,
        };
        let payload = vec![json!({
            "url": url,
            "text": "body",
        })];

        let results = normalize_fetch_payload(&request, Some(&payload), "text");

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| {
            result.status == FetchStatus::Success
                && result.content.as_deref() == Some("body")
                && !result.truncated
        }));
    }

    #[test]
    fn search_url_normalization_accepts_only_http_urls() {
        assert!(http_url_field(&json!({"url": "not-a-url"}), "url").is_none());
        assert!(http_url_field(&json!({"url": "ftp://example.com/"}), "url").is_none());
        assert_eq!(
            http_url_field(&json!({"url": "https://example.com/"}), "url"),
            Some("https://example.com/")
        );
    }
    #[test]
    fn zhipu_arguments_match_current_remote_mcp_tools() {
        assert_eq!(ZHIPU_SEARCH_TOOL, "web_search_prime");
        assert_eq!(ZHIPU_READER_TOOL, "webReader");
        let request = SearchRequest {
            query: "Rust".into(),
            max_results: 3,
            allowed_domains: vec!["rust-lang.org".into()],
            blocked_domains: vec!["private.rust-lang.org".into()],
        };
        let arguments = zhipu_search_arguments(&request);
        assert_eq!(arguments["search_query"], "Rust");
        assert_eq!(arguments["search_domain_filter"], "rust-lang.org");
        assert_eq!(arguments["search_recency_filter"], "noLimit");
        assert_eq!(arguments["content_size"], "medium");
        assert_eq!(arguments["location"], "us");
        assert!(!arguments.contains_key("blocked_domains"));

        let mut multiple_domains = request;
        multiple_domains.allowed_domains.push("docs.rs".into());
        assert!(
            !zhipu_search_arguments(&multiple_domains).contains_key("search_domain_filter"),
            "a singular upstream filter must not narrow a multi-domain union"
        );

        let reader = zhipu_reader_arguments("https://example.com/");
        assert_eq!(reader["url"], "https://example.com/");
        assert_eq!(reader["return_format"], "markdown");
        assert_eq!(reader["retain_images"], false);
    }

    #[test]
    fn zhipu_normalization_decodes_nested_mcp_json() {
        let upstream = json!([{
            "title": "Rust",
            "link": "https://www.rust-lang.org/",
            "content": "A language empowering everyone."
        }]);
        let nested = serde_json::to_string(
            &serde_json::to_string(&upstream).expect("encoded upstream payload"),
        )
        .expect("nested MCP payload");
        let result = CallToolResult::success(vec![rmcp::model::ContentBlock::text(nested)]);
        let payload = zhipu_tool_payload(&result, "Zhipu Search").expect("decoded payload");
        let response = normalize_zhipu_search(
            &SearchRequest {
                query: "Rust".into(),
                max_results: 1,
                allowed_domains: vec![],
                blocked_domains: vec![],
            },
            &payload,
        )
        .expect("normalized Search response");

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].url, "https://www.rust-lang.org/");
        assert_eq!(response.results[0].title.as_deref(), Some("Rust"));
        assert_eq!(
            response.results[0].snippet.as_deref(),
            Some("A language empowering everyone.")
        );
    }

    #[test]
    fn zhipu_reader_normalization_enforces_character_limit() {
        let result = normalize_zhipu_fetch(
            "https://example.com/",
            5,
            &json!({
                "title": "Example",
                "url": "https://example.com/",
                "content": "123456789"
            }),
        )
        .expect("normalized Fetch response");

        assert_eq!(result.status, FetchStatus::Success);
        assert_eq!(result.content.as_deref(), Some("12345"));
        assert_eq!(result.title.as_deref(), Some("Example"));
        assert!(result.truncated);
    }
}
