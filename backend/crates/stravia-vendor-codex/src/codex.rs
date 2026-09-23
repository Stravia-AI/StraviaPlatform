use std::collections::HashMap;

use serde_json::{Value, json};
use stravia_runtime_contract::protocol::{
    ids::OPEN_RESPONSES_2026_04_24,
    ir::{AiResponse, ServerToolUsage, Usage},
};
use stravia_vendor_sdk::{
    GuestHost, ProviderSnapshot, SearchRequest, SearchResponse, SearchSource, WsMessage, WsRequest,
};

use super::auth::account_id_from_jwt;
use super::{
    CLIENT_VERSION, configured_websocket_url, endpoint, ensure_success, invalid,
    require_codex_protocol, required_model, retryable, secret,
};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const ORIGINATOR: &str = "codex_cli_rs";

pub(super) fn prepare_body(body: &mut Value) -> Result<(), stravia_vendor_sdk::PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    object.insert("store".into(), Value::Bool(false));
    for field in [
        "frequency_penalty",
        "presence_penalty",
        "temperature",
        "top_p",
        "top_logprobs",
        "truncation",
        "max_output_tokens",
        "max_tool_calls",
    ] {
        object.remove(field);
    }
    if object.get("service_tier").and_then(Value::as_str) == Some("auto") {
        object.remove("service_tier");
    }
    Ok(())
}

pub(super) fn append_runtime_headers(
    provider: &ProviderSnapshot,
    body: &Value,
    headers: &mut Vec<(String, String)>,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    append_forwarded_client_headers(provider, headers);
    append_identity_headers(provider, headers)?;
    set_header(
        headers,
        "openai-beta",
        "responses_websockets=2026-02-06".into(),
    );
    let request_id = request_id(body);
    set_header(headers, "x-client-request-id", request_id);
    set_header(headers, "x-codex-routing-hint", routing_hint(body)?);
    Ok(())
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.into(), value));
}

fn append_forwarded_client_headers(
    provider: &ProviderSnapshot,
    headers: &mut Vec<(String, String)>,
) {
    for (name, value) in &provider.client_headers {
        let name = name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "accept"
                | "accept-language"
                | "idempotency-key"
                | "openai-beta"
                | "session-id"
                | "session_id"
                | "conversation_id"
                | "thread-id"
                | "x-client-request-id"
                | "x-codex-beta-features"
                | "x-codex-installation-id"
                | "x-codex-turn-metadata"
                | "x-codex-turn-state"
                | "x-codex-window-id"
        ) && !value.contains(['\r', '\n'])
        {
            set_header(headers, &name, value.clone());
        }
    }
}

pub(super) fn append_identity_headers(
    provider: &ProviderSnapshot,
    headers: &mut Vec<(String, String)>,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    let access_token = secret(provider, "access_token")?;
    set_header(headers, "authorization", format!("Bearer {access_token}"));
    set_header(headers, "originator", ORIGINATOR.into());
    set_header(
        headers,
        "user-agent",
        format!("{ORIGINATOR}/{CLIENT_VERSION}"),
    );
    set_header(headers, "version", CLIENT_VERSION.into());
    if let Some(account_id) = provider
        .credentials
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| account_id_from_jwt(access_token))
    {
        set_header(headers, "chatgpt-account-id", account_id);
    }
    Ok(())
}

fn routing_hint(body: &Value) -> Result<String, stravia_vendor_sdk::PluginError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("Codex Responses request is missing model"))?;
    Ok(
        match body
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(tier) => format!("model={model};tier={tier}"),
            None => format!("model={model}"),
        },
    )
}

fn request_id(_body: &Value) -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(super) fn websocket_available(provider: &ProviderSnapshot) -> bool {
    configured_websocket_url(provider).is_some()
        || url::Url::parse(&provider.base_url).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("chatgpt.com")
                && url.port_or_known_default() == Some(443)
        })
}

pub(super) fn infer_websocket(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    body: &Value,
    preserve_upstream_errors: bool,
) -> Result<AiResponse, stravia_vendor_sdk::PluginError> {
    let seed = request_id(body);
    let session_id = operation_id(provider, "session_id", &format!("session-{seed}"));
    let thread_id = operation_id(provider, "thread_id", &format!("thread-{seed}"));
    let window_id = operation_id(provider, "window_id", &format!("window-{seed}"));
    let turn_id = operation_id(provider, "turn_id", &format!("turn-{seed}"));

    let mut headers = Vec::new();
    append_runtime_headers(provider, body, &mut headers)?;
    set_header(&mut headers, "session-id", session_id.clone());
    set_header(&mut headers, "thread-id", thread_id.clone());
    set_header(&mut headers, "x-client-request-id", thread_id.clone());
    set_header(&mut headers, "x-codex-window-id", window_id.clone());

    let url = if let Some(url) = configured_websocket_url(provider) {
        url.to_owned()
    } else {
        let mut url = url::Url::parse(&endpoint(&provider.base_url, "/responses"))
            .map_err(|_| invalid("Codex base URL is invalid"))?;
        if url.scheme() != "https"
            || url.host_str() != Some("chatgpt.com")
            || url.port_or_known_default() != Some(443)
        {
            return Err(invalid(
                "custom Codex base URLs require an authorized websocket_url option",
            ));
        }
        url.set_scheme("wss")
            .map_err(|_| invalid("Codex WebSocket URL could not be constructed"))?;
        url.to_string()
    };
    let connection = host.ws_connect(WsRequest {
        url,
        headers,
        protocols: Vec::new(),
    })?;
    let mut frame = body.clone();
    let object = frame
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    object.remove("stream");
    object.remove("background");
    object.remove("service_tier");
    object.insert("type".into(), Value::String("response.create".into()));
    object.insert(
        "client_metadata".into(),
        json!({
            "session_id": session_id,
            "thread_id": thread_id,
            "x-codex-window-id": window_id,
            "turn_id": turn_id,
        }),
    );
    host.emit_started()?;
    connection
        .send(&WsMessage::Text(serde_json::to_string(&frame).map_err(
            |_| invalid("Codex WebSocket request could not be encoded"),
        )?))?;

    stravia_vendor_common::common::decode_codex_ai_response_websocket_with_error_classifier(
        host,
        &OPEN_RESPONSES_2026_04_24.to_string(),
        connection,
        preserve_upstream_errors,
        super::classify_responses_stream_error,
    )
}

fn operation_id(provider: &ProviderSnapshot, key: &str, fallback: &str) -> String {
    provider
        .operation_metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

pub(super) fn search(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: SearchRequest,
) -> Result<SearchResponse, stravia_vendor_sdk::PluginError> {
    if request.query.trim().is_empty() {
        return Err(invalid("Codex Search query must not be empty"));
    }
    if request.max_sources == Some(0) {
        return Err(invalid(
            "Codex Search max_sources must be greater than zero",
        ));
    }
    require_codex_protocol(provider)?;
    let model = required_model(provider)?;
    let mut tool = json!({
        "type": "web_search",
        "external_web_access": true
    });
    if !request.allowed_domains.is_empty() {
        tool["filters"] = json!({ "allowed_domains": request.allowed_domains });
    }
    let input = match request
        .language
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(language) => json!({"query": request.query, "language": language}),
        None => json!({"query": request.query}),
    };
    let mut body = json!({
        "model": model,
        "instructions": "Perform speed-first Web Search. Distinguish verified facts, inference, disagreement, and uncertainty. Follow the requested language; otherwise follow the query language; use English when ambiguous.",
        "input": [{
            "role": "user",
            "content": [{"type":"input_text", "text": input.to_string()}]
        }],
        "tools": [tool],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort":"medium", "summary":"auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content", "web_search_call.action.sources"]
    });
    prepare_body(&mut body)?;
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
    ];
    append_runtime_headers(provider, &body, &mut headers)?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider.base_url, "/responses"),
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("Codex Search request could not be encoded"))?,
    })?;
    ensure_success(&response, "Codex Search")?;
    let bytes = stravia_vendor_sdk::read_http_body(&response, MAX_RESPONSE_BYTES)?;
    let response = parse_responses_body(&bytes)?;
    let report_id = provider
        .operation_metadata
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("source");
    normalize_search_response(&response, request.max_sources, report_id)
}

pub(super) fn parse_responses_body(bytes: &[u8]) -> Result<Value, stravia_vendor_sdk::PluginError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| retryable("Codex returned non-UTF-8 response data"))?;
    if !text.trim_start().starts_with("data:") && !text.contains("\nevent:") {
        return serde_json::from_str(text).map_err(|_| retryable("Codex returned malformed JSON"));
    }
    let normalized = text.replace("\r\n", "\n");
    let mut output_items = Vec::new();
    for block in normalized.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
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
            serde_json::from_str(&data).map_err(|_| retryable("Codex returned malformed SSE"))?;
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
                let mut response = payload.get("response").cloned().unwrap_or(payload);
                if response
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "completed")
                {
                    return Err(retryable("Codex response was incomplete"));
                }
                if response
                    .get("output")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty)
                    && !output_items.is_empty()
                {
                    response["output"] = Value::Array(output_items);
                }
                return Ok(response);
            }
            "error" | "response.failed" | "response.incomplete" => {
                return Err(stravia_vendor_common::common::model_error(
                    response_failure_kind(&payload),
                    "Codex response stream failed",
                ));
            }
            _ => {}
        }
    }
    Err(stravia_vendor_common::common::model_error(
        stravia_runtime_contract::protocol::ir::AiErrorKind::UnexpectedEof,
        "Codex stream ended without a terminal response",
    ))
}

fn response_failure_kind(payload: &Value) -> stravia_runtime_contract::protocol::ir::AiErrorKind {
    use stravia_runtime_contract::protocol::ir::AiErrorKind;

    let error = payload.get("error").unwrap_or(payload);
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
        "server_error" | "model_error" => AiErrorKind::ServerError,
        "service_unavailable" => AiErrorKind::ServiceUnavailable,
        "timeout" => AiErrorKind::Timeout,
        "content_filtered" => AiErrorKind::ContentFiltered,
        "context_length_exceeded" => AiErrorKind::ContextLengthExceeded,
        _ => AiErrorKind::StreamMidError,
    }
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
) -> Result<SearchResponse, stravia_vendor_sdk::PluginError> {
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
                .ok_or_else(|| retryable("Codex citation start is invalid"))?;
            let end_index = annotation
                .get("end_index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| retryable("Codex citation end is invalid"))?;
            if start_index >= end_index {
                return Err(retryable("Codex citation span is invalid"));
            }
            let (Some(start), Some(end)) = (
                character_offset_to_byte(text, start_index),
                character_offset_to_byte(text, end_index),
            ) else {
                return Err(retryable("Codex citation span is invalid"));
            };
            let url = annotation
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                .ok_or_else(|| retryable("Codex citation URL is invalid"))?;
            annotations.push(Annotation {
                start: base + start,
                end: base + end,
                url: url.to_owned(),
                title: annotation
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    if answer.is_empty() || annotations.is_empty() {
        return Err(retryable("Codex Search returned no cited answer"));
    }
    annotations.sort_by_key(|annotation| (annotation.start, annotation.end));
    let limit = requested_max_sources.unwrap_or(20).clamp(1, 20) as usize;
    let mut source_ids = HashMap::new();
    let mut sources = Vec::new();
    for annotation in &annotations {
        if !source_ids.contains_key(&annotation.url) {
            if sources.len() == limit {
                return Err(retryable(
                    "Codex Search returned more cited sources than allowed",
                ));
            }
            let id = format!("{report_id}:{}", sources.len() + 1);
            source_ids.insert(annotation.url.clone(), id.clone());
            sources.push(SearchSource {
                id,
                url: annotation.url.clone(),
                title: annotation.title.clone(),
                snippet: None,
                published_at: None,
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

pub(super) fn usage_from_response(response: &Value) -> Option<Usage> {
    let usage = response.get("usage")?;
    let prompt_tokens: u32 = usage.get("input_tokens")?.as_u64()?.try_into().ok()?;
    let completion_tokens: u32 = usage.get("output_tokens")?.as_u64()?.try_into().ok()?;
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    let web_search_requests = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("web_search_call"))
        .count()
        .min(u32::MAX as usize) as u32;
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
        cache_creation_tokens: None,
        server_tool_use: (web_search_requests > 0).then_some(ServerToolUsage {
            web_search_requests,
            web_fetch_requests: 0,
        }),
    })
}
