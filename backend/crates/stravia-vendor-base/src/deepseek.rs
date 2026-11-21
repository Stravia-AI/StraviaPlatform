use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::{AiErrorKind, ServerToolUsage, Usage};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    Capability, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost, PluginError,
    ProviderDescriptor, ProviderSnapshot, SearchRequest, SearchResponse, SearchSource,
    read_http_body,
};

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_SOURCES: u32 = 20;
const SEARCH_INSTRUCTIONS: &str = "Perform speed-first Web Search. Distinguish verified facts, inference, disagreement, and uncertainty. Follow the requested language; otherwise follow the query language; use English when ambiguous.";

pub(crate) fn supports(provider_id: &str) -> bool {
    provider_id == "deepseek"
}

pub(crate) fn configure_descriptor(descriptor: &mut ProviderDescriptor) {
    if !supports(&descriptor.provider_id) {
        return;
    }
    if let Some(channel) = descriptor.channels.iter_mut().find(|c| c.id == "default") {
        channel.capabilities.insert(Capability::Search);
        descriptor.capabilities.insert(Capability::Search);
    }
}

pub(crate) fn decorate_discovery(mut response: DiscoverResponse) -> DiscoverResponse {
    for model in &mut response.models {
        if !model
            .capabilities
            .iter()
            .any(|cap| cap == Capability::Infer.as_str())
        {
            model.capabilities.push(Capability::Infer.as_str().into());
        }
        if is_search_model(model)
            && !model
                .capabilities
                .iter()
                .any(|cap| cap == Capability::Search.as_str())
        {
            model.capabilities.push(Capability::Search.as_str().into());
        }
    }
    response
}

fn is_search_model(model: &DiscoveredModel) -> bool {
    model.id.to_ascii_lowercase().contains("flash")
        || model
            .family
            .as_deref()
            .is_some_and(|family| family.to_ascii_lowercase().contains("flash"))
}

pub(crate) fn search(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: SearchRequest,
) -> Result<SearchResponse, PluginError> {
    if request.query.trim().is_empty() {
        return Err(invalid("DeepSeek Search query must not be empty"));
    }
    if request.continuation.is_some() {
        return Err(invalid("DeepSeek Search does not support continuation"));
    }
    if request.max_sources == Some(0) {
        return Err(invalid(
            "DeepSeek Search max_sources must be greater than zero",
        ));
    }
    let model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("DeepSeek Search requires a model"))?;

    let limit = request
        .max_sources
        .unwrap_or(DEFAULT_MAX_SOURCES)
        .clamp(1, DEFAULT_MAX_SOURCES);
    let body = build_search_body(model, provider, &request, limit);
    let encoded_body = serde_json::to_vec(&body)
        .map_err(|_| invalid("DeepSeek Search request could not be encoded"))?;

    let mut headers = vec![
        ("accept".into(), "text/event-stream".into()),
        ("content-type".into(), "application/json".into()),
    ];
    if let Some(key) = api_key(provider) {
        headers.push(("authorization".into(), format!("Bearer {key}")));
    }

    host.emit_started()?;
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: responses_endpoint(&provider.base_url),
            headers: crate::fields(headers),
            body: encoded_body,
        },
        &crate::net_options(),
    )?;

    let status = response.status()?;
    let response_headers = response.headers()?;
    if !(200..300).contains(&status) {
        let error_body = read_http_body(&response, MAX_ERROR_BYTES)?;
        return Err(common::upstream_error(
            status,
            &response_headers,
            &error_body,
        ));
    }

    let bytes = read_http_body(&response, MAX_RESPONSE_BYTES)?;
    let parsed = parse_responses_body(&bytes)?;
    let report_id = provider
        .operation_metadata
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("source");
    normalize_search_response(&parsed, request.max_sources, report_id)
}

fn api_key(provider: &ProviderSnapshot) -> Option<&str> {
    provider
        .credentials
        .get("apiKey")
        .or_else(|| provider.options.get("apiKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn responses_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/responses")
}

/// 根据绑定路由的默认思考等级映射 DeepSeek Responses API 的 `reasoning` 参数。
/// 参考 https://api-docs.deepseek.com/zh-cn/guides/thinking_mode ：
/// - `off` / `none` -> `{"effort": "none"}`（关闭思考，走联网搜索）
/// - `minimal` / `low` -> `{"effort": "low"}`（开启思考，走深度思考搜索）
/// - `medium` / `high` / `xhigh` -> `{"effort": "high"}`
/// - `max` -> `{"effort": "max"}`
/// - 未设置 -> 省略 `reasoning`，由 DeepSeek 默认决定
fn reasoning_config(provider: &ProviderSnapshot) -> Option<Value> {
    let level = provider
        .operation_metadata
        .get("default_thinking_level")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let effort = match level.to_ascii_lowercase().as_str() {
        "off" | "none" => "none",
        "minimal" | "low" => "low",
        "medium" | "high" | "xhigh" => "high",
        "max" => "max",
        _ => return None,
    };
    Some(json!({ "effort": effort }))
}

fn build_search_body(
    model: &str,
    provider: &ProviderSnapshot,
    request: &SearchRequest,
    limit: u32,
) -> Value {
    let mut instructions = String::from(SEARCH_INSTRUCTIONS);
    write!(instructions, " Cite at most {limit} distinct web sources.")
        .expect("writing to String cannot fail");
    if !request.allowed_domains.is_empty() {
        instructions.push_str(" Only search and cite sources from these allowed domains: ");
        instructions.push_str(&request.allowed_domains.join(", "));
        instructions.push('.');
    }

    let mut input_obj = serde_json::Map::new();
    input_obj.insert("query".into(), Value::String(request.query.clone()));
    if let Some(language) = request
        .language
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        input_obj.insert("language".into(), Value::String(language.to_owned()));
    }
    if !request.allowed_domains.is_empty() {
        input_obj.insert(
            "allowed_domains".into(),
            Value::Array(
                request
                    .allowed_domains
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }

    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": Value::Object(input_obj).to_string()}]
        }],
        "tools": [{"type": "deepseek_web_search"}],
        "tool_choice": "auto",
        "stream": true
    });
    if let Some(reasoning) = reasoning_config(provider) {
        body["reasoning"] = reasoning;
    }
    body
}

fn parse_responses_body(bytes: &[u8]) -> Result<Value, PluginError> {
    let (output_items, mut response) = scan_responses_body(bytes)?;
    if response
        .get("output")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
        && !output_items.is_empty()
    {
        response["output"] = Value::Array(output_items);
    }
    Ok(response)
}

fn scan_responses_body(bytes: &[u8]) -> Result<(Vec<Value>, Value), PluginError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| retryable("DeepSeek returned non-UTF-8 response data"))?;
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        let response: Value = serde_json::from_str(trimmed)
            .map_err(|_| retryable("DeepSeek returned malformed JSON"))?;
        check_terminal_json_response(&response)?;
        return Ok((Vec::new(), response));
    }

    let normalized = text.replace("\r\n", "\n");
    let mut output_items = Vec::new();
    for block in normalized.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if line.starts_with(':') {
                continue;
            }
            if let Some(value) = line.strip_prefix("event:") {
                event = Some(value.trim());
            } else if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.trim_start());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let payload: Value =
            serde_json::from_str(&data).map_err(|_| retryable("DeepSeek returned malformed SSE"))?;
        let event = event
            .or_else(|| payload.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        match event {
            "response.output_item.done" => {
                if let Some(item) = payload.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" | "response.done" => {
                let response = payload.get("response").cloned().unwrap_or(payload);
                check_terminal_json_response(&response)?;
                return Ok((output_items, response));
            }
            "error" | "response.failed" | "response.incomplete" => {
                return Err(common::model_error(
                    response_failure_kind(&payload),
                    response_failure_message(&payload),
                ));
            }
            _ => {}
        }
    }
    Err(common::model_error(
        AiErrorKind::UnexpectedEof,
        "DeepSeek stream ended without a terminal response",
    ))
}

fn check_terminal_json_response(response: &Value) -> Result<(), PluginError> {
    if response.get("error").is_some_and(|value| !value.is_null()) {
        return Err(common::model_error(
            response_failure_kind(response),
            response_failure_message(response),
        ));
    }
    match response.get("status").and_then(Value::as_str) {
        Some("failed" | "incomplete") => Err(common::model_error(
            response_failure_kind(response),
            response_failure_message(response),
        )),
        _ => Ok(()),
    }
}

fn response_failure_kind(payload: &Value) -> AiErrorKind {
    let response = payload.get("response").unwrap_or(payload);
    if response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        == Some("content_filter")
    {
        return AiErrorKind::ContentFiltered;
    }
    let error = response
        .get("error")
        .or_else(|| payload.get("error"))
        .unwrap_or(payload);
    let body = error.to_string().to_ascii_lowercase();
    if ["insufficient_quota", "quota_exceeded", "billing_hard_limit"]
        .iter()
        .any(|marker| body.contains(marker))
    {
        return AiErrorKind::QuotaExceeded;
    }
    let discriminator = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match discriminator {
        "authentication_error" | "invalid_api_key" => AiErrorKind::AuthenticationError,
        "authorization_error" | "permission_error" | "permission_denied" => {
            AiErrorKind::AuthorizationError
        }
        "not_found" | "not_found_error" | "model_not_found" => AiErrorKind::NotFoundError,
        "rate_limit_error" | "too_many_requests" => AiErrorKind::RateLimitError,
        "invalid_request" | "invalid_request_error" => AiErrorKind::InvalidRequest,
        "server_error" | "model_error" | "api_error" => AiErrorKind::ServerError,
        "service_unavailable" => AiErrorKind::ServiceUnavailable,
        "timeout" => AiErrorKind::Timeout,
        "content_filtered" | "content_filter" => AiErrorKind::ContentFiltered,
        "context_length_exceeded" => AiErrorKind::ContextLengthExceeded,
        _ => AiErrorKind::StreamMidError,
    }
}

fn response_failure_message(payload: &Value) -> String {
    let response = payload.get("response").unwrap_or(payload);
    if let Some(message) = response
        .pointer("/error/message")
        .or_else(|| payload.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return format!("DeepSeek Search failed: {message}");
    }
    if let Some(reason) = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return format!("DeepSeek Search incomplete: {reason}");
    }
    "DeepSeek Search response stream failed".into()
}

#[derive(Debug, Default)]
struct SourceMetadata {
    title: Option<String>,
    snippet: Option<String>,
    published_at: Option<String>,
}

fn collect_source_metadata(response: &Value) -> HashMap<String, SourceMetadata> {
    let mut map: HashMap<String, SourceMetadata> = HashMap::new();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("type").and_then(Value::as_str) != Some("web_search_call") {
            continue;
        }
        if let Some(sources) = item.pointer("/action/sources").and_then(Value::as_array) {
            for source in sources {
                let Some(url) = source
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                else {
                    continue;
                };
                let entry = map.entry(url.to_owned()).or_default();
                if entry.title.is_none() {
                    entry.title = non_empty_str(source.get("title"));
                }
                if entry.snippet.is_none() {
                    entry.snippet = non_empty_str(source.get("snippet"));
                }
                if entry.published_at.is_none() {
                    entry.published_at = non_empty_str(source.get("published_at"));
                }
            }
        }
        if let Some(page) = item.get("page")
            && let Some(url) = page
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
        {
            let entry = map.entry(url.to_owned()).or_default();
            if entry.title.is_none() {
                entry.title = non_empty_str(page.get("title"));
            }
            if entry.published_at.is_none() {
                entry.published_at = non_empty_str(page.get("published_at"));
            }
        }
    }
    map
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[derive(Debug)]
struct Annotation {
    start: usize,
    end: usize,
    url: String,
    title: Option<String>,
}

fn normalize_search_response(
    response: &Value,
    requested_max_sources: Option<u32>,
    report_id: &str,
) -> Result<SearchResponse, PluginError> {
    let source_meta = collect_source_metadata(response);
    let mut answer = String::new();
    let mut annotations = Vec::new();
    for content in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
    {
        let Some(text) = content.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !answer.is_empty() {
            answer.push('\n');
        }
        let base = answer.len();
        answer.push_str(text);
        for annotation in content
            .get("annotations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !matches!(
                annotation.get("type").and_then(Value::as_str),
                Some("url_citation") | Some("url_annotation")
            ) {
                continue;
            }
            let start_index = annotation
                .get("start_index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| retryable("DeepSeek citation start is invalid"))?;
            let end_index = annotation
                .get("end_index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| retryable("DeepSeek citation end is invalid"))?;
            if start_index >= end_index {
                return Err(retryable("DeepSeek citation span is invalid"));
            }
            let (Some(start), Some(end)) = (
                character_offset_to_byte(text, start_index),
                character_offset_to_byte(text, end_index),
            ) else {
                return Err(retryable("DeepSeek citation span is invalid"));
            };
            let url = annotation
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                .ok_or_else(|| retryable("DeepSeek citation URL is invalid"))?;
            annotations.push(Annotation {
                start: base + start,
                end: base + end,
                url: url.to_owned(),
                title: non_empty_str(annotation.get("title")),
            });
        }
    }
    if answer.is_empty() || annotations.is_empty() {
        return Err(retryable("DeepSeek Search returned no cited answer"));
    }

    annotations.sort_by_key(|annotation| (annotation.start, annotation.end));
    let limit = requested_max_sources
        .unwrap_or(DEFAULT_MAX_SOURCES)
        .clamp(1, DEFAULT_MAX_SOURCES) as usize;
    let mut source_ids = HashMap::new();
    let mut sources = Vec::new();
    for annotation in &annotations {
        if !source_ids.contains_key(&annotation.url) {
            if sources.len() == limit {
                return Err(retryable(
                    "DeepSeek Search returned more cited sources than allowed",
                ));
            }
            let id = format!("{report_id}:{}", sources.len() + 1);
            source_ids.insert(annotation.url.clone(), id.clone());
            let meta = source_meta.get(&annotation.url);
            sources.push(SearchSource {
                id,
                url: annotation.url.clone(),
                title: annotation
                    .title
                    .clone()
                    .or_else(|| meta.and_then(|m| m.title.clone())),
                snippet: meta.and_then(|m| m.snippet.clone()),
                published_at: meta.and_then(|m| m.published_at.clone()),
            });
        }
    }

    let mut insertions = annotations
        .iter()
        .map(|annotation| {
            (
                annotation.end,
                format!(" [sc:{}]", source_ids[&annotation.url]),
            )
        })
        .collect::<Vec<_>>();
    insertions.sort_by_key(|(offset, _)| std::cmp::Reverse(*offset));
    for (offset, marker) in insertions {
        answer.insert_str(offset, &marker);
    }

    Ok(SearchResponse {
        answer,
        sources,
        limitations: Vec::new(),
        usage: usage_from_response(response),
    })
}

fn character_offset_to_byte(value: &str, offset: usize) -> Option<usize> {
    value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .nth(offset)
}

fn usage_from_response(response: &Value) -> Option<Usage> {
    let usage = response.get("usage")?;
    let prompt_tokens: u32 = usage.get("input_tokens")?.as_u64()?.try_into().ok()?;
    let completion_tokens: u32 = usage.get("output_tokens")?.as_u64()?.try_into().ok()?;
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));

    let server_tool_use = if let Some(reported) = usage.get("server_tool_use") {
        let tool_search = reported
            .get("tool_search")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let tool_image_search = reported
            .get("tool_image_search")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let tool_fetch = reported
            .get("tool_fetch")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let web_search_requests = tool_search
            .saturating_add(tool_image_search)
            .min(u32::MAX as u64) as u32;
        let web_fetch_requests = tool_fetch.min(u32::MAX as u64) as u32;
        (web_search_requests > 0 || web_fetch_requests > 0).then_some(ServerToolUsage {
            web_search_requests,
            web_fetch_requests,
        })
    } else {
        let mut search_calls = 0u32;
        let mut fetch_calls = 0u32;
        for item in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if item.get("type").and_then(Value::as_str) != Some("web_search_call") {
                continue;
            }
            match item.pointer("/action/type").and_then(Value::as_str) {
                Some("open_page") => fetch_calls = fetch_calls.saturating_add(1),
                _ => search_calls = search_calls.saturating_add(1),
            }
        }
        (search_calls > 0 || fetch_calls > 0).then_some(ServerToolUsage {
            web_search_requests: search_calls,
            web_fetch_requests: fetch_calls,
        })
    };

    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        required_components_known: true,
        cache_read_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        cache_creation_tokens: usage
            .pointer("/input_tokens_details/cache_write_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        server_tool_use,
    })
}

fn invalid(message: impl Into<String>) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

fn retryable(message: impl Into<String>) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn provider_snapshot(default_thinking_level: Option<&str>) -> ProviderSnapshot {
        let mut operation_metadata = BTreeMap::new();
        if let Some(level) = default_thinking_level {
            operation_metadata.insert("default_thinking_level".into(), json!(level));
        }
        ProviderSnapshot {
            provider_id: "deepseek".into(),
            channel: "default".into(),
            base_url: "https://api.deepseek.com".into(),
            protocol: "openai-compatible".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: Some("deepseek-flash".into()),
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata,
        }
    }

    fn search_request(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            continuation: None,
            allowed_domains: Vec::new(),
            language: None,
            max_sources: Some(10),
        }
    }

    #[test]
    fn responses_endpoint_normalizes_optional_v1_suffix() {
        assert_eq!(
            responses_endpoint("https://api.deepseek.com"),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            responses_endpoint("https://api.deepseek.com/"),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            responses_endpoint("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            responses_endpoint("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/responses"
        );
    }

    #[test]
    fn reasoning_follows_bound_route_thinking_level() {
        let none_body = build_search_body(
            "deepseek-flash",
            &provider_snapshot(None),
            &search_request("test"),
            10,
        );
        assert!(none_body.get("reasoning").is_none());

        for (level, expected) in [
            ("off", "none"),
            ("none", "none"),
            ("minimal", "low"),
            ("low", "low"),
            ("medium", "high"),
            ("high", "high"),
            ("xhigh", "high"),
            ("max", "max"),
        ] {
            let body = build_search_body(
                "deepseek-flash",
                &provider_snapshot(Some(level)),
                &search_request("test"),
                10,
            );
            assert_eq!(
                body["reasoning"],
                json!({ "effort": expected }),
                "level={level}"
            );
        }
    }

    #[test]
    fn search_body_uses_deepseek_web_search_without_unsupported_tool_filters() {
        let mut req = search_request("Iceland population");
        req.allowed_domains = vec!["example.com".into()];
        req.language = Some("zh-CN".into());

        let body = build_search_body("deepseek-flash", &provider_snapshot(Some("off")), &req, 5);
        assert_eq!(body["model"], "deepseek-flash");
        assert_eq!(body["tools"], json!([{"type": "deepseek_web_search"}]));
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"], json!({"effort": "none"}));
        assert!(
            body["instructions"]
                .as_str()
                .unwrap()
                .contains("example.com")
        );
    }

    #[test]
    fn decorate_discovery_marks_only_flash_models_for_search() {
        let response = decorate_discovery(DiscoverResponse {
            models: vec![
                DiscoveredModel {
                    id: "deepseek-flash".into(),
                    display_name: "DeepSeek Flash".into(),
                    family: Some("deepseek-flash".into()),
                    selector: None,
                    capabilities: Vec::new(),
                    metadata: BTreeMap::new(),
                },
                DiscoveredModel {
                    id: "deepseek-chat".into(),
                    display_name: "DeepSeek Chat".into(),
                    family: Some("deepseek".into()),
                    selector: None,
                    capabilities: Vec::new(),
                    metadata: BTreeMap::new(),
                },
                DiscoveredModel {
                    id: "deepseek-v4-pro".into(),
                    display_name: "DeepSeek V4 Pro".into(),
                    family: Some("deepseek-thinking".into()),
                    selector: None,
                    capabilities: Vec::new(),
                    metadata: BTreeMap::new(),
                },
            ],
            next_cursor: None,
        });

        assert_eq!(response.models[0].capabilities, vec!["infer", "search"]);
        assert_eq!(response.models[1].capabilities, vec!["infer"]);
        assert_eq!(response.models[2].capabilities, vec!["infer"]);
    }

    #[test]
    fn parse_sse_ignores_keepalive_and_normalizes_citations_with_source_metadata() {
        let sse = concat!(
            ": keep-alive\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"srvtoolu_s_00\",\"status\":\"completed\",\"kind\":\"search\",\"action\":{\"type\":\"search\",\"queries\":[\"冰岛人口\"],\"sources\":[{\"type\":\"url\",\"url\":\"https://example.com/iceland\",\"title\":\"冰岛统计局\",\"snippet\":\"冰岛人口约39.4万\",\"published_at\":\"2026-01-02T03:04:05+00:00\"}]}}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"web_search_call\",\"id\":\"srvtoolu_s_00\",\"status\":\"completed\",\"kind\":\"search\",\"action\":{\"type\":\"search\",\"queries\":[\"冰岛人口\"],\"sources\":[{\"type\":\"url\",\"url\":\"https://example.com/iceland\",\"title\":\"冰岛统计局\",\"snippet\":\"冰岛人口约39.4万\",\"published_at\":\"2026-01-02T03:04:05+00:00\"}]}},{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"冰岛人口约为39.4万。\",\"annotations\":[{\"type\":\"url_citation\",\"url\":\"https://example.com/iceland\",\"title\":\"\",\"start_index\":0,\"end_index\":11}]}]}],\"usage\":{\"input_tokens\":100,\"input_tokens_details\":{\"cached_tokens\":20},\"output_tokens\":30,\"output_tokens_details\":{\"reasoning_tokens\":10},\"total_tokens\":130,\"server_tool_use\":{\"tool_search\":2,\"tool_fetch\":1,\"tool_image_search\":0}}}}\n\n"
        );

        let parsed = parse_responses_body(sse.as_bytes()).expect("valid sse");
        let normalized =
            normalize_search_response(&parsed, Some(5), "turn-1").expect("normalized response");
        assert_eq!(normalized.answer, "冰岛人口约为39.4万 [sc:turn-1:1]。");
        assert_eq!(normalized.sources.len(), 1);
        assert_eq!(normalized.sources[0].id, "turn-1:1");
        assert_eq!(normalized.sources[0].url, "https://example.com/iceland");
        assert_eq!(normalized.sources[0].title.as_deref(), Some("冰岛统计局"));
        assert_eq!(
            normalized.sources[0].snippet.as_deref(),
            Some("冰岛人口约39.4万")
        );
        assert_eq!(
            normalized.sources[0].published_at.as_deref(),
            Some("2026-01-02T03:04:05+00:00")
        );
        let usage = normalized.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.total_tokens, 130);
        assert_eq!(usage.cache_read_tokens, Some(20));
        assert_eq!(usage.reasoning_tokens, Some(10));
        let server_tool_use = usage.server_tool_use.expect("server_tool_use present");
        assert_eq!(server_tool_use.web_search_requests, 2);
        assert_eq!(server_tool_use.web_fetch_requests, 1);
    }

    #[test]
    fn non_stream_content_filter_incomplete_maps_to_content_filtered() {
        let body = json!({
            "status": "incomplete",
            "incomplete_details": { "reason": "content_filter" },
            "output": []
        });
        let err = parse_responses_body(body.to_string().as_bytes()).expect_err("must fail");
        assert!(matches!(
            err.kind,
            ErrorKind::Upstream(failure)
                if matches!(
                    failure.model_error_kind,
                    Some(stravia_vendor_sdk::ModelErrorKind::ContentFiltered)
                )
        ));
    }
}
