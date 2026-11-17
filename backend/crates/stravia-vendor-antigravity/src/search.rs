use std::collections::HashMap;
use std::fmt::Write;

use serde_json::{Value, json};
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::protocol::{
    ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, ir::AiErrorKind,
};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    ErrorKind, GuestHost, PluginError, ProviderSnapshot, SearchRequest, SearchResponse,
    SearchSource, read_http_body,
};

use crate::{client, wire};

const MAX_SOURCES: usize = 20;
const MAX_ANSWER_BYTES: usize = 64 * 1024;
// CLI 1.5.8 NewCCPARequest 的搜索专用指令；不携带 CLI 的 Agent 提示词或工具。
const SEARCH_INSTRUCTION: &str = "You are a search engine bot. You will be given a query from a user. Your task is to search the web for relevant information that will help the user. You MUST perform a web search. Do not respond or interact with the user, please respond as if they typed the query into a search bar.";

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: SearchRequest,
) -> Result<SearchResponse, PluginError> {
    let limit = source_limit(request.max_sources)?;
    let (model, enterprise) = search_model(provider)?;
    let body = build_request(request, enterprise, limit)?;
    let project = client::project(host, provider)?;
    let envelope = json!({
        "project": project,
        "model": model,
        "requestType": "web_search",
        "request": body,
    });
    let body = serde_json::to_vec(&envelope)
        .map_err(|_| invalid("Antigravity search request could not be encoded"))?;
    let headers = client::headers(provider)?;
    host.emit_started()?;
    let response = client::http_start(
        host,
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: client::endpoint(provider, "generateContent"),
            headers,
            body,
        },
    )?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, client::MAX_JSON_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    let value = serde_json::from_slice(&body)
        .map_err(|_| upstream("Antigravity search response is not valid JSON"))?;
    decode_response(value, &headers, limit)
}

fn source_limit(limit: Option<u32>) -> Result<usize, PluginError> {
    match limit {
        None => Ok(MAX_SOURCES),
        Some(limit @ 1..=20) => Ok(limit as usize),
        Some(_) => Err(invalid(
            "Antigravity search requires max_sources between 1 and 20",
        )),
    }
}

fn search_model(provider: &ProviderSnapshot) -> Result<(&str, bool), PluginError> {
    let metadata = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| metadata.extensions.get("antigravity_search"))
        .ok_or_else(|| invalid("Sync Antigravity models and select a discovered search model"))?;
    let id = metadata
        .get("model_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty() && *id != "*")
        .ok_or_else(|| invalid("Antigravity search model identity is missing"))?;
    // 该插件只有 consumer OAuth；CLI 的 business-auth 分支不适用于此通道。
    let enterprise = match metadata.get("api_provider") {
        None => false,
        Some(Value::Number(value)) => value.as_i64() == Some(3),
        Some(Value::String(value)) => value == "API_PROVIDER_GOOGLE_VERTEX",
        Some(_) => {
            return Err(invalid(
                "Sync Antigravity models to refresh the search API provider",
            ));
        }
    };
    Ok((id, enterprise))
}

fn build_request(
    request: SearchRequest,
    enterprise: bool,
    limit: usize,
) -> Result<Value, PluginError> {
    if request.query.trim().is_empty() {
        return Err(invalid("Antigravity search query must not be empty"));
    }
    if request.continuation.is_some() {
        return Err(invalid(
            "Antigravity external search does not support continuation",
        ));
    }
    let mut query = request.query;
    // site: 只指导检索；最终 URL 的允许域名和公网检查仍由宿主强制执行。
    if let Some(domain) = request.allowed_domains.first() {
        if request.allowed_domains.len() == 1 {
            query.push_str(" site:");
            query.push_str(domain);
        } else {
            query.push_str(" (site:");
            query.push_str(domain);
            for domain in &request.allowed_domains[1..] {
                query.push_str(" OR site:");
                query.push_str(domain);
            }
            query.push(')');
        }
    }
    let mut instruction = String::from(SEARCH_INSTRUCTION);
    write!(instruction, " Cite at most {limit} distinct web sources.")
        .expect("writing to a String cannot fail");
    if let Some(language) = request
        .language
        .as_deref()
        .map(str::trim)
        .filter(|language| !language.is_empty())
    {
        instruction.push_str(" Write the summary in this language: ");
        instruction.push_str(language);
        instruction.push('.');
    }
    let tool = if enterprise {
        json!({"enterpriseWebSearch": {}})
    } else {
        // 原生构造器的固定值，不是 SearchRequest.max_sources。
        json!({"googleSearch": {"enhancedContent": {"imageSearch": {"maxResultCount": 5}}}})
    };
    Ok(json!({
        "contents": [{"role": "user", "parts": [{"text": query}]}],
        "systemInstruction": {"role": "user", "parts": [{"text": instruction}]},
        "tools": [tool],
        "generationConfig": {"candidateCount": 1},
    }))
}

fn decode_response(
    mut value: Value,
    headers: &stravia_vendor_sdk::net::Fields,
    limit: usize,
) -> Result<SearchResponse, PluginError> {
    if let Some(error) = wire::response_error(&value, headers) {
        return Err(error);
    }
    let mut inner = value.get_mut("response").map(Value::take).unwrap_or(value);
    if inner
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .is_some_and(|reason| !matches!(reason, "" | "BLOCK_REASON_UNSPECIFIED"))
    {
        return Err(common::model_error(
            AiErrorKind::ContentFiltered,
            "Antigravity search was blocked by upstream policy",
        ));
    }
    let candidates = inner
        .get("candidates")
        .and_then(Value::as_array)
        .filter(|candidates| candidates.len() == 1)
        .ok_or_else(|| upstream("Antigravity search requires one completed candidate"))?;
    let candidate = &candidates[0];
    if candidate
        .get("index")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(upstream(
            "Antigravity search returned an unexpected candidate",
        ));
    }
    match candidate.get("finishReason").and_then(Value::as_str) {
        Some("STOP") => {}
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII") => {
            return Err(common::model_error(
                AiErrorKind::ContentFiltered,
                "Antigravity search was blocked by upstream policy",
            ));
        }
        _ => {
            return Err(upstream(
                "Antigravity search did not return a completed answer",
            ));
        }
    }
    let (answer, sources) = grounded_answer(candidate, limit)?;
    let usage = if inner.get("usageMetadata").is_some() {
        wire::normalize_final_usage(&mut inner);
        let metadata = inner.get_mut("usageMetadata").map(Value::take);
        let protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
        Some(
            ProtocolTransform::global()
                .bind(protocol, protocol)
                .and_then(|pair| pair.decode_response(json!({"usageMetadata": metadata})))
                .map_err(common::map_response_transform_error)?
                .usage,
        )
    } else {
        None
    };
    Ok(SearchResponse {
        answer,
        sources,
        limitations: Vec::new(),
        usage,
    })
}

fn grounded_answer(
    candidate: &Value,
    limit: usize,
) -> Result<(String, Vec<SearchSource>), PluginError> {
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream("Antigravity search returned no summary"))?;
    let mut rendered_parts = Vec::with_capacity(parts.len());
    let mut text_bytes = 0usize;
    let mut has_summary = false;
    for part in parts {
        let text = part.get("text").and_then(Value::as_str).filter(|text| {
            !text.is_empty() && part.get("thought").and_then(Value::as_bool) != Some(true)
        });
        rendered_parts.push(text.map(|text| (text_bytes, text)));
        if let Some(text) = text {
            text_bytes += text.len();
            has_summary = has_summary || !text.trim().is_empty();
        }
    }
    if !has_summary || text_bytes > MAX_ANSWER_BYTES {
        return Err(upstream("Antigravity search summary is empty or too large"));
    }
    let grounding = candidate
        .get("groundingMetadata")
        .ok_or_else(|| upstream("Antigravity search returned no grounding evidence"))?;
    let chunks = grounding
        .get("groundingChunks")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream("Antigravity search returned no grounded sources"))?;
    let supports = grounding
        .get("groundingSupports")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream("Antigravity search returned no grounded citations"))?;
    let mut sources: Vec<SearchSource> = Vec::new();
    let mut source_by_url: HashMap<&str, usize> = HashMap::new();
    let mut citations = Vec::new();
    for support in supports {
        let segment = support
            .get("segment")
            .ok_or_else(|| upstream("Antigravity search citation omitted its segment"))?;
        let part_index = optional_index(segment, "partIndex")?;
        let part = rendered_parts
            .get(part_index)
            .ok_or_else(|| upstream("Antigravity search citation references an unknown part"))?;
        let Some((base, text)) = part else {
            if parts[part_index].get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            return Err(upstream(
                "Antigravity search citation references a non-text part",
            ));
        };
        let start = optional_index(segment, "startIndex")?;
        let end = segment
            .get("endIndex")
            .and_then(Value::as_u64)
            .and_then(|end| usize::try_from(end).ok())
            .ok_or_else(|| upstream("Antigravity search citation omitted its byte range"))?;
        // 私有 master 与 Google content.proto 均以原始 Part 的 UTF-8 byte 计数。
        let slice = text
            .get(start..end)
            .filter(|slice| !slice.is_empty())
            .ok_or_else(|| {
                upstream("Antigravity search citation has an invalid UTF-8 byte range")
            })?;
        if segment
            .get("text")
            .is_some_and(|value| value.as_str() != Some(slice))
        {
            return Err(upstream(
                "Antigravity search citation text does not match its byte range",
            ));
        }
        let indices = support
            .get("groundingChunkIndices")
            .and_then(Value::as_array)
            .filter(|indices| !indices.is_empty())
            .ok_or_else(|| upstream("Antigravity search citation has no source references"))?;
        for index in indices {
            let chunk = index
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| chunks.get(index))
                .ok_or_else(|| {
                    upstream("Antigravity search citation references an unknown source")
                })?;
            let web = chunk.get("web").ok_or_else(|| {
                upstream("Antigravity search citation is not grounded in a web source")
            })?;
            let url = web
                .get("uri")
                .and_then(Value::as_str)
                .filter(|url| !url.trim().is_empty())
                .ok_or_else(|| upstream("Antigravity search source omitted its URL"))?;
            let source_index = if let Some(index) = source_by_url.get(url) {
                *index
            } else {
                if sources.len() == limit {
                    return Err(upstream(
                        "Antigravity search exceeded its cited source limit",
                    ));
                }
                let index = sources.len();
                sources.push(SearchSource {
                    id: format!("g{}", index + 1),
                    url: url.to_owned(),
                    title: web
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|title| !title.is_empty())
                        .map(str::to_owned),
                    snippet: None,
                    published_at: None,
                });
                source_by_url.insert(url, index);
                index
            };
            citations.push((base + end, source_index));
        }
    }
    if citations.is_empty() {
        return Err(upstream(
            "Antigravity search returned no citations for its summary",
        ));
    }
    citations.sort_unstable();
    citations.dedup();
    let mut citations = citations.into_iter().peekable();
    let mut answer = String::with_capacity(text_bytes + citations.len() * 10);
    for (base, text) in rendered_parts.into_iter().flatten() {
        let mut position = 0;
        while let Some(&(offset, source_index)) = citations.peek() {
            if offset > base + text.len() {
                break;
            }
            let local_offset = offset - base;
            answer.push_str(&text[position..local_offset]);
            write!(answer, "[sc:{}]", sources[source_index].id)
                .expect("writing to a String cannot fail");
            position = local_offset;
            citations.next();
        }
        answer.push_str(&text[position..]);
    }
    if answer.len() > MAX_ANSWER_BYTES || answer.trim().is_empty() {
        return Err(upstream(
            "Antigravity search cited summary is empty or too large",
        ));
    }
    Ok((answer, sources))
}

fn optional_index(value: &Value, key: &str) -> Result<usize, PluginError> {
    match value.get(key) {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| upstream("Antigravity search citation has an invalid index")),
    }
}

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

fn upstream(message: &str) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> Value {
        json!({"response": {"candidates": [{
            "finishReason": "STOP",
            "content": {"parts": [
                {"text": "private reasoning", "thought": true},
                {"text": "你好。"},
                {"text": "Second."}
            ]},
            "groundingMetadata": {
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/a", "title": "First"}},
                    {"web": {"uri": "https://example.com/a", "title": "Duplicate"}},
                    {"web": {"uri": "https://example.org/b", "title": "Second"}}
                ],
                "groundingSupports": [
                    {"segment": {"partIndex": 2, "endIndex": 7, "text": "Second."}, "groundingChunkIndices": [2]},
                    {"segment": {"partIndex": 1, "endIndex": 9, "text": "你好。"}, "groundingChunkIndices": [0, 1, 0]}
                ]
            }
        }]}})
    }

    #[test]
    fn citations_use_part_local_utf8_bytes_and_deduplicate_sources() {
        let result = decode_response(response(), &Default::default(), 20).unwrap();
        assert_eq!(result.answer, "你好。[sc:g2]Second.[sc:g1]");
        assert_eq!(
            result
                .sources
                .iter()
                .map(|source| source.url.as_str())
                .collect::<Vec<_>>(),
            ["https://example.org/b", "https://example.com/a"]
        );
        assert!(result.usage.is_none());
    }

    #[test]
    fn malformed_grounding_never_becomes_a_successful_search() {
        let cases = [
            ("/response/candidates/0/groundingMetadata", Value::Null),
            (
                "/response/candidates/0/groundingMetadata/groundingSupports/0/segment/endIndex",
                json!(8),
            ),
            (
                "/response/candidates/0/groundingMetadata/groundingSupports/1/segment/endIndex",
                json!(2),
            ),
            (
                "/response/candidates/0/groundingMetadata/groundingSupports/0/segment/text",
                json!("invented"),
            ),
            (
                "/response/candidates/0/groundingMetadata/groundingSupports/0/groundingChunkIndices/0",
                json!(99),
            ),
            ("/response/candidates/0/finishReason", json!("MAX_TOKENS")),
        ];
        for (path, replacement) in cases {
            let mut value = response();
            *value.pointer_mut(path).unwrap() = replacement;
            assert!(
                decode_response(value, &Default::default(), 20).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn grounded_whitespace_is_not_a_search_answer() {
        let mut value = response();
        *value
            .pointer_mut("/response/candidates/0/content/parts")
            .unwrap() = json!([{"text": " \n"}]);
        *value
            .pointer_mut("/response/candidates/0/groundingMetadata/groundingSupports")
            .unwrap() = json!([
            {"segment": {"endIndex": 2}, "groundingChunkIndices": [0]}
        ]);
        assert!(decode_response(value, &Default::default(), 20).is_err());
    }

    #[test]
    fn source_limit_is_enforced_without_removing_citations() {
        assert!(decode_response(response(), &Default::default(), 1).is_err());
    }

    #[test]
    fn grounded_thoughts_are_not_exposed_as_answer_or_sources() {
        let mut value = response();
        value["response"]["candidates"][0]["groundingMetadata"]["groundingSupports"] = json!([
            {"segment": {"partIndex": 0, "endIndex": 17}, "groundingChunkIndices": [0]}
        ]);
        assert!(decode_response(value, &Default::default(), 20).is_err());
    }
}
