use std::collections::HashMap;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::{
    BackendOutput, SearchBackend, SearchBackendInput, SearchCompletion, SearchEvidence,
    SearchEvidenceSet, SearchReport, SearchSource, WebSearchBackendKind, WebSearchError,
};
use crate::protocol::ir::Usage;

const MAX_CODEX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct CodexAgenticSearchBackend {
    gateway: crate::Gateway,
}

impl CodexAgenticSearchBackend {
    pub(crate) fn new(gateway: crate::Gateway) -> Self {
        Self { gateway }
    }
}

pub(crate) fn codex_provider_contract(provider: &crate::db::models::Provider) -> bool {
    provider.is_enabled
        && provider.channel.as_deref() == Some("codex")
        && provider.auth_mode == "oauth"
        && provider.protocol == "open-responses"
}
#[async_trait]
impl SearchBackend for CodexAgenticSearchBackend {
    fn kind(&self) -> WebSearchBackendKind {
        WebSearchBackendKind::Codex
    }

    async fn run(&self, input: SearchBackendInput) -> Result<BackendOutput, WebSearchError> {
        let super::ResolvedWebSearchBackend::Codex {
            provider_id,
            upstream_model,
        } = &input.binding
        else {
            return Err(codex_error(
                "invalid_binding",
                "Codex Search binding is invalid",
            ));
        };
        let provider = self
            .gateway
            .storage
            .providers()
            .get(provider_id)
            .await
            .map_err(|_| codex_error("provider_unavailable", "Codex Provider is unavailable"))?
            .ok_or_else(|| codex_error("provider_unavailable", "Codex Provider is unavailable"))?;
        if !codex_provider_contract(&provider) {
            return Err(codex_error(
                "provider_invalid",
                "Configured Codex Provider is incompatible with Web Search",
            ));
        }
        let model = self
            .gateway
            .storage
            .provider_models()
            .get(provider_id, upstream_model)
            .await
            .map_err(|_| codex_error("model_unavailable", "Codex model is unavailable"))?
            .filter(|model| model.effective_available())
            .ok_or_else(|| codex_error("model_unavailable", "Codex model is unavailable"))?;
        if model.model_id != *upstream_model {
            return Err(codex_error(
                "model_unavailable",
                "Codex model is unavailable",
            ));
        }
        let credential = self
            .gateway
            .storage
            .oauth_credentials()
            .get(provider_id)
            .await
            .map_err(|_| {
                codex_error("oauth_unavailable", "Codex OAuth credential is unavailable")
            })?;
        let runtime = self
            .gateway
            .admin()
            .resolve_provider_runtime_from_snapshot(&provider, credential.as_ref())
            .await
            .map_err(|_| {
                codex_error("oauth_unavailable", "Codex OAuth credential is unavailable")
            })?;
        let client = self
            .gateway
            .http_client_for_provider(provider.use_proxy)
            .await
            .map_err(|_| codex_error("transport_unavailable", "Codex transport is unavailable"))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", runtime.access_token)).map_err(|_| {
                codex_error("oauth_unavailable", "Codex OAuth credential is unavailable")
            })?,
        );
        for (name, value) in runtime.binding.extra_headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                codex_error("provider_invalid", "Codex Provider headers are invalid")
            })?;
            let value = HeaderValue::from_str(&value).map_err(|_| {
                codex_error("provider_invalid", "Codex Provider headers are invalid")
            })?;
            headers.insert(name, value);
        }
        let endpoint = runtime
            .binding
            .base_url_override
            .unwrap_or_else(|| provider.base_url.clone());
        let payload = codex_payload(&input, upstream_model);
        let request_bytes = serde_json::to_vec(&payload)
            .map(|bytes| bytes.len())
            .unwrap_or_default();
        let started_at = std::time::Instant::now();
        let response = match send_responses_sse(
            client
                .post(endpoint.trim_end_matches('/').to_owned() + "/responses")
                .headers(headers)
                .json(&payload),
        )
        .await
        {
            Ok(response) => {
                let usage = usage_from_response(&response);
                tracing::info!(
                    target: "stravia::audit",
                    event = "web_search_codex_request",
                    request_id = %input.turn_id,
                    provider_id,
                    model_id = upstream_model,
                    status = 200_u16,
                    outcome = "completed",
                    error_code = Option::<&str>::None,
                    request_bytes,
                    response_bytes = serde_json::to_vec(&response)
                        .map(|bytes| bytes.len())
                        .unwrap_or_default(),
                    prompt_tokens = usage.prompt_tokens,
                    completion_tokens = usage.completion_tokens,
                    total_tokens = usage.total_tokens,
                    elapsed_ms = started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    "Codex Web Search upstream request completed"
                );
                response
            }
            Err(error) => {
                tracing::info!(
                    target: "stravia::audit",
                    event = "web_search_codex_request",
                    request_id = %input.turn_id,
                    provider_id,
                    model_id = upstream_model,
                    status = Option::<u16>::None,
                    outcome = "failed",
                    error_code = error.code.as_str(),
                    request_bytes,
                    response_bytes = 0_usize,
                    prompt_tokens = 0_u32,
                    completion_tokens = 0_u32,
                    total_tokens = 0_u32,
                    elapsed_ms = started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    "Codex Web Search upstream request failed"
                );
                return Err(error);
            }
        };
        let (report, evidence) = normalize_response(&input.turn_id, &response)?;
        let usage = usage_from_response(&response);
        let tool_calls = response
            .get("output")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        item.get("type").and_then(Value::as_str) == Some("web_search_call")
                    })
                    .count()
                    .min(u32::MAX as usize) as u32
            })
            .unwrap_or_default();
        Ok(BackendOutput {
            completion: SearchCompletion::Complete,
            partial_cause: None,
            report,
            evidence,
            usage,
            model_turns: 1,
            tool_calls,
        })
    }
}

fn codex_payload(input: &SearchBackendInput, model: &str) -> Value {
    let mut tool = json!({
        "type": "web_search",
        "external_web_access": true
    });
    if !input.policy.allowed_domains.is_empty() {
        tool["filters"] = json!({
            "allowed_domains": input.policy.allowed_domains
        });
    }
    let context = json!({
        "ancestors": input.ancestors.iter().map(|ancestor| json!({
            "turn_id": ancestor.turn_id,
            "query": ancestor.query,
            "completion": ancestor.completion,
            "report": ancestor.report,
        })).collect::<Vec<_>>(),
        "query": input.query,
        "policy": {
            "blocked_domains": input.policy.blocked_domains,
        },
    });
    json!({
        "model": model,
        "instructions": "Perform speed-first Web Search. Distinguish verified facts, inference, disagreement, and uncertainty. Follow the requested language; otherwise follow the query language; use English when ambiguous.",
        "input": [{
            "role": "user",
            "content": [{ "type": "input_text", "text": context.to_string() }]
        }],
        "tools": [tool],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": { "effort": "medium", "summary": "auto" },
        "store": false,
        "stream": true,
        "include": [
            "reasoning.encrypted_content",
            "web_search_call.action.sources"
        ]
    })
}

async fn send_responses_sse(builder: reqwest::RequestBuilder) -> Result<Value, WebSearchError> {
    let response = builder.send().await.map_err(|error| {
        if error.is_timeout() {
            codex_error("timeout", "Codex Search request timed out")
        } else {
            codex_error("upstream_unavailable", "Codex Search request failed")
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(codex_error(
            if status.as_u16() == 429 {
                "rate_limited"
            } else {
                "upstream_unavailable"
            },
            if status.as_u16() == 429 {
                "Codex Search was rate limited"
            } else {
                "Codex Search upstream returned an error"
            },
        ));
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk =
            chunk.map_err(|_| codex_error("stream_failed", "Codex Search stream failed"))?;
        if body.len().saturating_add(chunk.len()) > MAX_CODEX_RESPONSE_BYTES {
            return Err(codex_error(
                "response_too_large",
                "Codex Search response exceeded the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    parse_responses_sse(&body)
}

fn parse_responses_sse(body: &[u8]) -> Result<Value, WebSearchError> {
    let body = std::str::from_utf8(body)
        .map_err(|_| codex_error("malformed_sse", "Codex Search returned malformed SSE"))?;
    let normalized = body.replace("\r\n", "\n");
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
        let payload: Value = serde_json::from_str(&data)
            .map_err(|_| codex_error("malformed_sse", "Codex Search returned malformed SSE"))?;
        let event = event
            .or_else(|| payload.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        match event {
            "response.output_item.done" => {
                if let Some(item) = payload.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                let mut response = payload.get("response").cloned().unwrap_or(payload);
                if response
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "completed")
                {
                    return Err(codex_error(
                        "incomplete_response",
                        "Codex Search response was incomplete",
                    ));
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
                return Err(codex_error("stream_failed", "Codex Search stream failed"));
            }
            _ => {}
        }
    }
    Err(codex_error(
        "incomplete_response",
        "Codex Search stream ended without completion",
    ))
}

#[derive(Debug, Clone)]
struct Annotation {
    start: usize,
    end: usize,
    url: String,
    title: Option<String>,
}

fn normalize_response(
    turn_id: &super::SearchTurnId,
    response: &Value,
) -> Result<(SearchReport, SearchEvidenceSet), WebSearchError> {
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
            let Some(start_index) = annotation
                .get("start_index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
            else {
                return Err(codex_error(
                    "invalid_annotation",
                    "Codex citation span is invalid",
                ));
            };
            let Some(end_index) = annotation
                .get("end_index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
            else {
                return Err(codex_error(
                    "invalid_annotation",
                    "Codex citation span is invalid",
                ));
            };
            let (Some(start), Some(end)) = (
                character_offset_to_byte(text, start_index),
                character_offset_to_byte(text, end_index),
            ) else {
                return Err(codex_error(
                    "invalid_annotation",
                    "Codex citation span is invalid",
                ));
            };
            if start_index >= end_index {
                return Err(codex_error(
                    "invalid_annotation",
                    "Codex citation span is invalid",
                ));
            }
            let url = annotation
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    codex_error("invalid_annotation", "Codex citation URL is invalid")
                })?;
            annotations.push(Annotation {
                start: base + start,
                end: base + end,
                url: super::validator::normalize_public_url(url)?,
                title: annotation
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    if answer.is_empty() || annotations.is_empty() {
        return Err(codex_error(
            "missing_annotations",
            "Codex Search returned no cited answer",
        ));
    }
    annotations.sort_by_key(|annotation| (annotation.start, annotation.end));
    let mut source_ids = HashMap::new();
    let mut sources = Vec::new();
    for annotation in &annotations {
        if !source_ids.contains_key(&annotation.url) {
            if sources.len() == 20 {
                return Err(codex_error(
                    "source_limit",
                    "Codex Search returned too many cited sources",
                ));
            }
            let id = format!("source-{}-{}", turn_id, sources.len() + 1);
            source_ids.insert(annotation.url.clone(), id.clone());
            sources.push(SearchSource {
                id,
                url: annotation.url.clone(),
                title: annotation.title.clone(),
            });
        }
    }
    let mut insertions = annotations
        .iter()
        .map(|annotation| {
            (
                annotation.end,
                format!(" [{}]", source_ids[&annotation.url]),
            )
        })
        .collect::<Vec<_>>();
    insertions.sort_by_key(|(offset, _)| std::cmp::Reverse(*offset));
    for (offset, marker) in insertions {
        answer.insert_str(offset, &marker);
    }
    let evidence = sources
        .iter()
        .map(|source| SearchEvidence {
            url: source.url.clone(),
            title: source.title.clone(),
        })
        .collect();
    Ok((
        SearchReport {
            answer,
            sources,
            limitations: Vec::new(),
        },
        evidence,
    ))
}

fn character_offset_to_byte(value: &str, offset: usize) -> Option<usize> {
    value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .nth(offset)
}

fn usage_from_response(response: &Value) -> Usage {
    let usage = response.get("usage").unwrap_or(&Value::Null);
    let prompt_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    let completion_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
        ..Usage::default()
    }
}

fn codex_error(code: impl Into<String>, message: impl Into<String>) -> WebSearchError {
    WebSearchError::backend(WebSearchBackendKind::Codex, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_insert_turn_scoped_markers_and_ignore_action_only_sources() {
        let response = json!({
            "status": "completed",
            "output": [
                {
                    "type": "web_search_call",
                    "action": { "sources": [{ "url": "https://9.9.9.9/consulted" }] }
                },
                {
                    "type": "message",
                    "content": [{
                        "type": "output_text",
                        "text": "First fact and second fact",
                        "annotations": [
                            {
                                "type": "url_citation",
                                "start_index": 0,
                                "end_index": 10,
                                "url": "https://8.8.8.8/source",
                                "title": "Primary"
                            },
                            {
                                "type": "url_citation",
                                "start_index": 15,
                                "end_index": 26,
                                "url": "https://8.8.8.8/source",
                                "title": "Primary"
                            }
                        ]
                    }]
                }
            ]
        });

        let (report, _) =
            normalize_response(&super::super::SearchTurnId::new("wst_codex"), &response)
                .expect("normalized report");

        assert_eq!(report.sources.len(), 1);
        assert_eq!(report.sources[0].url, "https://8.8.8.8/source");
        assert!(!report.answer.contains("consulted"));
        assert_eq!(report.answer.matches("[source-wst_codex-1]").count(), 2);
    }

    #[test]
    fn payload_maps_allowed_domains_and_advises_blocked_domains() {
        let input = SearchBackendInput {
            turn_id: super::super::SearchTurnId::new("wst_codex"),
            principal: crate::hook::Principal::new("owner"),
            query: "Search the claim".into(),
            policy: super::super::WebSearchRunPolicy {
                allowed_domains: vec!["allowed.example".into()],
                blocked_domains: vec!["blocked.example".into()],
            },
            ancestors: Vec::new(),
            binding: super::super::ResolvedWebSearchBackend::Codex {
                provider_id: "codex-provider".into(),
                upstream_model: "gpt-5".into(),
            },
            definition_revision: None,
            local_limits: None,
            cancellation: crate::proxy::context::CancellationToken::new(),
        };

        let payload = codex_payload(&input, "gpt-5");
        assert_eq!(
            payload["tools"][0]["filters"]["allowed_domains"],
            json!(["allowed.example"])
        );
        let context: Value =
            serde_json::from_str(payload["input"][0]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            context["policy"]["blocked_domains"],
            json!(["blocked.example"])
        );
    }

    #[test]
    fn malformed_annotation_span_is_rejected() {
        let response = json!({
            "output": [{
                "content": [{
                    "text": "short",
                    "annotations": [{
                        "type": "url_citation",
                        "start_index": 0,
                        "end_index": 99,
                        "url": "https://8.8.8.8/source"
                    }]
                }]
            }]
        });

        let error = normalize_response(&super::super::SearchTurnId::new("wst_codex"), &response)
            .expect_err("invalid span must fail");

        assert_eq!(error.code, "invalid_annotation");
    }

    #[test]
    fn annotation_indices_are_unicode_character_offsets() {
        let response = json!({
            "output": [{
                "content": [{
                    "text": "研究结论",
                    "annotations": [{
                        "type": "url_citation",
                        "start_index": 0,
                        "end_index": 2,
                        "url": "https://8.8.8.8/source"
                    }]
                }]
            }]
        });

        let (report, _) =
            normalize_response(&super::super::SearchTurnId::new("wst_unicode"), &response)
                .expect("Unicode annotation");

        assert_eq!(report.answer, "研究 [source-wst_unicode-1]结论");
    }
}
