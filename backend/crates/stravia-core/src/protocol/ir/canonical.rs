use sha2::{Digest, Sha256};

use super::{
    AiItem, AiRequest, ContentBlock, DocumentSource, MediaSource, MessageContent, ProtocolExt,
    ToolSpec,
};

/// Produces the stable Provider prompt material used by cache routing.
///
/// Cache directives and graph metadata are policy and delivery state, not
/// prompt material. Keeping this as a positive projection prevents new IR
/// fields from silently changing cache identity before their Provider
/// semantics are classified.
pub(crate) fn item_value(item: &AiItem) -> serde_json::Value {
    serde_json::Value::Array(history_item_values(item))
}

pub(crate) fn item_hash(item: &AiItem) -> [u8; 32] {
    hash_bytes(
        &serde_json::to_vec(&item_value(item)).expect("canonical AiItem must serialize as JSON"),
    )
}

pub(crate) fn item_hashes(items: &[AiItem]) -> Vec<[u8; 32]> {
    history_values(items)
        .into_iter()
        .map(|value| {
            hash_bytes(
                &serde_json::to_vec(&value)
                    .expect("canonical provider-context unit must serialize as JSON"),
            )
        })
        .collect()
}

pub(crate) fn history_unit_count(items: &[AiItem]) -> usize {
    items
        .iter()
        .map(|item| history_item_values(item).len())
        .sum()
}

fn history_item_values(item: &AiItem) -> Vec<serde_json::Value> {
    if item.role == super::Role::Assistant {
        return assistant_history_values(item);
    }
    if let Some(values) = tool_output_history_values(item) {
        return values;
    }
    vec![history_item_value(item)]
}

fn history_values(items: &[AiItem]) -> Vec<serde_json::Value> {
    items.iter().flat_map(history_item_values).collect()
}

fn history_item_value(item: &AiItem) -> serde_json::Value {
    let content = match &item.content {
        MessageContent::Text(text) => {
            serde_json::json!([{ "type": "text", "text": text }])
        }
        MessageContent::Blocks(blocks)
            if blocks
                .iter()
                .all(|block| matches!(block, ContentBlock::Text { .. })) =>
        {
            serde_json::json!([{
                "type": "text",
                "text": blocks.iter().filter_map(ContentBlock::as_text).collect::<String>()
            }])
        }
        MessageContent::Blocks(blocks) => {
            serde_json::Value::Array(blocks.iter().map(history_content_block_value).collect())
        }
    };
    let artifact_references = item
        .meta
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|meta| meta.get("__stravia_artifact_references"));
    serde_json::json!({
        "role": &item.role,
        "content": content,
        "tool_calls": &item.tool_calls,
        "tool_call_id": &item.tool_call_id,
        "artifact_references": artifact_references,
    })
}

fn assistant_history_values(item: &AiItem) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let mut text = String::new();
    let mut represented_tool_calls = Vec::new();

    let flush_text = |values: &mut Vec<serde_json::Value>, text: &mut String| {
        if !text.is_empty() {
            values.push(assistant_content_value(serde_json::json!({
                "type": "text",
                "text": std::mem::take(text),
            })));
        }
    };

    match &item.content {
        MessageContent::Text(value) => text.push_str(value),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text: value, .. } => text.push_str(value),
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        flush_text(&mut values, &mut text);
                        values.push(reasoning_value(thinking.clone(), signature.as_deref()));
                    }
                    ContentBlock::Reasoning {
                        summary,
                        content,
                        encrypted_content,
                    } => {
                        flush_text(&mut values, &mut text);
                        values.push(reasoning_value(
                            summary.iter().chain(content).cloned().collect(),
                            encrypted_content.as_deref(),
                        ));
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        flush_text(&mut values, &mut text);
                        represented_tool_calls.push(id.as_str());
                        values.push(tool_call_value(id, name, input.clone()));
                    }
                    other => {
                        flush_text(&mut values, &mut text);
                        values.push(assistant_content_value(history_content_block_value(other)));
                    }
                }
            }
        }
    }
    flush_text(&mut values, &mut text);

    for call in item.tool_calls.iter().flatten() {
        if represented_tool_calls.contains(&call.id.as_str()) {
            continue;
        }
        let arguments = serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
        values.push(tool_call_value(&call.id, &call.name, arguments));
    }

    if let Some(artifact_references) = item
        .meta
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|meta| meta.get("__stravia_artifact_references"))
    {
        values.push(serde_json::json!({
            "role": "assistant",
            "artifact_references": artifact_references,
        }));
    }
    if values.is_empty() {
        values.push(assistant_content_value(serde_json::json!({
            "type": "text",
            "text": "",
        })));
    }
    values
}

fn tool_output_history_values(item: &AiItem) -> Option<Vec<serde_json::Value>> {
    if let MessageContent::Blocks(blocks) = &item.content {
        let tool_results = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => Some(tool_output_value(tool_use_id, content.clone(), *is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !tool_results.is_empty() && tool_results.len() == blocks.len() {
            return Some(tool_results);
        }
    }
    if item.role != super::Role::Tool {
        return None;
    }
    let call_id = item.tool_call_id.as_deref()?;
    let output = match &item.content {
        MessageContent::Text(text) => serde_json::Value::String(text.clone()),
        MessageContent::Blocks(blocks) => {
            let values = blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Unknown { raw } => raw.clone(),
                    other => history_content_block_value(other),
                })
                .collect::<Vec<_>>();
            match values.as_slice() {
                [value] => value.clone(),
                _ => serde_json::Value::Array(values),
            }
        }
    };
    Some(vec![tool_output_value(call_id, output, None)])
}

fn tool_output_value(
    call_id: &str,
    output: serde_json::Value,
    is_error: Option<bool>,
) -> serde_json::Value {
    serde_json::json!({
        "role": "tool",
        "tool_call_id": call_id,
        "output": output,
        "is_error": is_error,
    })
}

fn assistant_content_value(content: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "role": "assistant",
        "content": content,
    })
}

fn reasoning_value(text: String, encrypted_content: Option<&str>) -> serde_json::Value {
    assistant_content_value(serde_json::json!({
        "type": "reasoning",
        "text": text,
        "encrypted_content": encrypted_content,
    }))
}

fn tool_call_value(id: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "role": "assistant",
        "tool_call": {
            "id": id,
            "name": name,
            "arguments": arguments,
        },
    })
}

/// Compares only fields positively classified as Provider-context semantics.
///
/// Response delivery metadata and cache policy never enter this projection.
/// New IR fields remain non-semantic until explicitly added to
/// `history_item_value` or `history_content_block_value`.
pub(crate) fn history_items_equal(left: &[AiItem], right: &[AiItem]) -> bool {
    history_values(left) == history_values(right)
}

pub(crate) fn append_history_context_hash(previous: &[u8; 32], item: &AiItem) -> [u8; 32] {
    history_item_values(item)
        .into_iter()
        .fold(*previous, append_history_value_hash)
}

pub(crate) fn history_context_hash(items: &[AiItem]) -> [u8; 32] {
    let mut digest = hash_bytes(b"stravia-generation-chain-history-v1");
    for item in items {
        digest = append_history_context_hash(&digest, item);
    }
    digest
}

fn append_history_value_hash(previous: [u8; 32], value: serde_json::Value) -> [u8; 32] {
    let value = serde_json::to_vec(&value)
        .expect("model-visible provider-context unit must serialize as JSON");
    let mut hasher = Sha256::new();
    hasher.update(b"stravia-generation-chain-history-v1\0");
    hasher.update(previous);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
    hasher.finalize().into()
}

/// Fingerprints material and controls that can change Provider prompt-cache
/// matching. The projection is intentionally positive: new request fields do
/// not become cache identity until their Provider semantics are classified.
pub(crate) fn cache_controls_hash(request: &AiRequest) -> [u8; 32] {
    let controls = serde_json::json!({
        "model": &request.model,
        "instructions": &request.instructions,
        "tools": request.tools.as_ref().map(|tools| {
            tools.iter().map(history_tool_value).collect::<Vec<_>>()
        }),
        "tool_choice": &request.tool_choice,
        "parallel_tool_calls": request.parallel_tool_calls,
        "disable_parallel_tool_calls": request.disable_parallel_tool_calls,
        "reasoning": &request.reasoning,
        "response_format": &request.response_format,
        "safety_settings": &request.safety_settings,
        "protocol_controls": cache_protocol_controls_value(request.ext.as_ref()),
    });
    hash_bytes(
        &serde_json::to_vec(&controls).expect("prompt cache controls must serialize as JSON"),
    )
}

pub(crate) fn history_request_controls_hash(request: &AiRequest) -> [u8; 32] {
    let controls = serde_json::json!({
        "model": &request.model,
        "instructions": &request.instructions,
        "generation": {
            "temperature": request.generation.temperature,
            "top_p": request.generation.top_p,
            "seed": request.generation.seed,
            "stop": &request.generation.stop,
            "presence_penalty": request.generation.presence_penalty,
            "frequency_penalty": request.generation.frequency_penalty,
        },
        "embedding": &request.embedding,
        "tools": request.tools.as_ref().map(|tools| {
            tools.iter().map(history_tool_value).collect::<Vec<_>>()
        }),
        "tool_choice": &request.tool_choice,
        "parallel_tool_calls": request.parallel_tool_calls,
        "disable_parallel_tool_calls": request.disable_parallel_tool_calls,
        "reasoning": &request.reasoning,
        "response_format": &request.response_format,
        "safety_settings": &request.safety_settings,
        "protocol_controls": history_protocol_controls_value(request.ext.as_ref()),
    });
    hash_bytes(
        &serde_json::to_vec(&controls)
            .expect("Generation Chain history controls must serialize as JSON"),
    )
}

fn history_tool_value(tool: &ToolSpec) -> serde_json::Value {
    serde_json::json!({
        "name": &tool.name,
        "description": &tool.description,
        "parameters": &tool.parameters,
        "strict": tool.strict,
        "meta": &tool.meta,
    })
}

fn history_protocol_controls_value(extension: Option<&ProtocolExt>) -> serde_json::Value {
    normalize_empty_controls(match extension {
        None => serde_json::Value::Null,
        Some(ProtocolExt::OpenAiChat(extension)) => serde_json::json!({
            "openai_chat": {
                "audio": &extension.audio,
                "logit_bias": &extension.logit_bias,
                "logprobs": extension.logprobs,
                "top_logprobs": extension.top_logprobs,
                "modalities": &extension.modalities,
                "n": extension.n,
                "prediction": &extension.prediction,
                "verbosity": &extension.verbosity,
                "web_search_options": &extension.web_search_options,
            }
        }),
        Some(ProtocolExt::OpenResponses(extension)) => serde_json::json!({
            "open_responses": {
                "max_tool_calls": extension.max_tool_calls,
                "top_logprobs": extension.top_logprobs,
                "truncation": &extension.truncation,
                "text": &extension.text,
                "service_tier": &extension.service_tier,
                "native_web_search": &extension.native_web_search,
                "tool_choice_ext": &extension.tool_choice_ext,
            }
        }),
        Some(ProtocolExt::Anthropic(extension)) => serde_json::json!({
            "anthropic": {
                "top_k": extension.top_k,
                "container": &extension.container,
                "inference_geo": &extension.inference_geo,
                "output_config": &extension.output_config,
                "service_tier": &extension.service_tier,
                "server_tools": &extension.server_tools,
            }
        }),
        Some(ProtocolExt::Google(extension)) => serde_json::json!({
            "google": {
                "top_k": extension.top_k,
                "candidate_count": extension.candidate_count,
                "response_logprobs": extension.response_logprobs,
                "logprobs": extension.logprobs,
                "response_mime_type": &extension.response_mime_type,
                "response_json_schema": &extension.response_json_schema,
                "tool_config": &extension.tool_config,
                "response_modalities": &extension.response_modalities,
                "thinking_config": &extension.thinking_config,
            }
        }),
    })
}

fn cache_protocol_controls_value(extension: Option<&ProtocolExt>) -> serde_json::Value {
    normalize_empty_controls(match extension {
        None => serde_json::Value::Null,
        Some(ProtocolExt::OpenAiChat(extension)) => serde_json::json!({
            "openai_chat": {
                "prompt_cache_retention": &extension.prompt_cache_retention,
                "prediction": &extension.prediction,
                "web_search_options": &extension.web_search_options,
            }
        }),
        Some(ProtocolExt::OpenResponses(extension)) => serde_json::json!({
            "open_responses": {
                "prompt_cache_key": &extension.prompt_cache_key,
                "truncation": &extension.truncation,
                "text": &extension.text,
                "native_web_search": &extension.native_web_search,
                "tool_choice_ext": &extension.tool_choice_ext,
            }
        }),
        Some(ProtocolExt::Anthropic(extension)) => serde_json::json!({
            "anthropic": {
                "container": &extension.container,
                "inference_geo": &extension.inference_geo,
                "output_config": &extension.output_config,
                "service_tier": &extension.service_tier,
                "server_tools": &extension.server_tools,
            }
        }),
        Some(ProtocolExt::Google(extension)) => serde_json::json!({
            "google": {
                "cached_content": &extension.cached_content,
                "thinking_config": &extension.thinking_config,
                "tool_config": &extension.tool_config,
                "response_mime_type": &extension.response_mime_type,
                "response_json_schema": &extension.response_json_schema,
            }
        }),
    })
}

fn normalize_empty_controls(value: serde_json::Value) -> serde_json::Value {
    fn is_empty(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Null => true,
            serde_json::Value::Array(values) => values.iter().all(is_empty),
            serde_json::Value::Object(values) => values.values().all(is_empty),
            _ => false,
        }
    }

    if is_empty(&value) {
        serde_json::Value::Null
    } else {
        value
    }
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(crate) fn hash_hex(hash: &[u8; 32]) -> String {
    let mut output = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn history_content_block_value(block: &ContentBlock) -> serde_json::Value {
    match block {
        ContentBlock::Text { text, .. } => {
            serde_json::json!({ "type": "text", "text": text })
        }
        ContentBlock::Image { source, detail, .. } => serde_json::json!({
            "type": "image",
            "source": media_source_value(source),
            "detail": detail,
        }),
        ContentBlock::Audio { source } => serde_json::json!({
            "type": "audio",
            "source": media_source_value(source),
        }),
        ContentBlock::File { source, media_type } => serde_json::json!({
            "type": "file",
            "source": media_source_value(source),
            "media_type": media_type,
        }),
        ContentBlock::Video { source, media_type } => serde_json::json!({
            "type": "video",
            "source": media_source_value(source),
            "media_type": media_type,
        }),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => serde_json::json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature,
        }),
        ContentBlock::Reasoning {
            summary,
            content,
            encrypted_content,
        } => serde_json::json!({
            "type": "reasoning",
            "summary": summary,
            "content": content,
            "encrypted_content": encrypted_content,
        }),
        ContentBlock::RedactedThinking { data } => serde_json::json!({
            "type": "redacted_thinking",
            "data": data,
        }),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::ServerToolUse {
            id,
            name,
            input,
            server_type,
            ..
        } => serde_json::json!({
            "type": "server_tool_use",
            "id": id,
            "name": name,
            "input": input,
            "server_type": server_type,
        }),
        ContentBlock::ServerToolResult {
            tool_use_id,
            content,
            server_type,
            ..
        } => serde_json::json!({
            "type": "server_tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "server_type": server_type,
        }),
        ContentBlock::Document {
            source,
            title,
            context,
            ..
        } => serde_json::json!({
            "type": "document",
            "source": history_document_source_value(source),
            "title": title,
            "context": context,
        }),
        ContentBlock::SearchResult {
            content,
            source,
            title,
            ..
        } => serde_json::json!({
            "type": "search_result",
            "content": content.iter().map(history_content_block_value).collect::<Vec<_>>(),
            "source": source,
            "title": title,
        }),
        ContentBlock::ContainerUpload { file_id, .. } => serde_json::json!({
            "type": "container_upload",
            "file_id": file_id,
        }),
        ContentBlock::Citation { cited_text, source } => serde_json::json!({
            "type": "citation",
            "cited_text": cited_text,
            "source": source,
        }),
        ContentBlock::ExecutableCode { code, language, id } => serde_json::json!({
            "type": "executable_code",
            "code": code,
            "language": language,
            "id": id,
        }),
        ContentBlock::CodeExecutionResult {
            return_code,
            stdout,
            stderr,
            id,
        } => serde_json::json!({
            "type": "code_execution_result",
            "return_code": return_code,
            "stdout": stdout,
            "stderr": stderr,
            "id": id,
        }),
        ContentBlock::Refusal { refusal } => serde_json::json!({
            "type": "refusal",
            "refusal": refusal,
        }),
        ContentBlock::Unknown { raw } => serde_json::json!({
            "type": "unknown",
            "raw": raw,
        }),
    }
}

fn history_document_source_value(source: &DocumentSource) -> serde_json::Value {
    match source {
        DocumentSource::Base64Pdf { data } => {
            serde_json::json!({ "type": "base64_pdf", "data": data })
        }
        DocumentSource::PlainText { data } => {
            serde_json::json!({ "type": "plain_text", "data": data })
        }
        DocumentSource::Url(url) => serde_json::json!({ "type": "url", "url": url }),
        DocumentSource::Blocks { content } => serde_json::json!({
            "type": "blocks",
            "content": content.iter().map(history_content_block_value).collect::<Vec<_>>(),
        }),
    }
}

fn media_source_value(source: &MediaSource) -> serde_json::Value {
    match source {
        MediaSource::Base64 { media_type, data } => serde_json::json!({
            "type": "base64",
            "media_type": media_type,
            "data": data,
        }),
        MediaSource::Url(url) => serde_json::json!({
            "type": "url",
            "url": url,
        }),
        MediaSource::FileId { file_id, detail } => serde_json::json!({
            "type": "file_id",
            "file_id": file_id,
            "detail": detail,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{
        AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, ContentBlock, MessageContent, Role,
        ToolCall,
    };

    fn item(content: ContentBlock) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![content]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    #[test]
    fn media_identity_changes_item_hash() {
        let left = item(ContentBlock::Image {
            source: MediaSource::Url("https://example.test/a.png".into()),
            detail: None,
            cache_control: None,
        });
        let right = item(ContentBlock::Image {
            source: MediaSource::Url("https://example.test/b.png".into()),
            detail: None,
            cache_control: None,
        });
        assert_ne!(item_hash(&left), item_hash(&right));
    }

    #[test]
    fn url_media_variants_have_stable_history_hashes() {
        for block in [
            ContentBlock::Audio {
                source: MediaSource::Url("https://example.test/audio.mp3".into()),
            },
            ContentBlock::File {
                source: MediaSource::Url("https://example.test/document.txt".into()),
                media_type: Some("text/plain".into()),
            },
            ContentBlock::Video {
                source: MediaSource::Url("https://example.test/video.mp4".into()),
                media_type: Some("video/mp4".into()),
            },
        ] {
            let value = history_item_value(&item(block));
            assert!(value["content"][0]["source"]["url"].is_string());
        }
    }

    #[test]
    fn google_cached_content_is_cache_policy_not_history_identity() {
        let request_with = |cached_content: Option<&str>| {
            let mut request = AiRequest::new("model", Vec::new());
            request.ext = Some(ProtocolExt::Google(super::super::GoogleExt {
                cached_content: cached_content.map(str::to_owned),
                ..Default::default()
            }));
            request
        };
        let without_cache = request_with(None);
        let with_cache = request_with(Some("cachedContents/example"));

        assert_eq!(
            history_request_controls_hash(&without_cache),
            history_request_controls_hash(&with_cache)
        );
        assert_ne!(
            cache_controls_hash(&without_cache),
            cache_controls_hash(&with_cache)
        );
    }

    #[test]
    fn appended_context_hash_matches_the_complete_prefix() {
        let items = [
            item(ContentBlock::Text {
                text: "first".into(),
                cache_control: None,
            }),
            item(ContentBlock::Text {
                text: "second".into(),
                cache_control: None,
            }),
        ];

        assert_eq!(
            append_history_context_hash(&history_context_hash(&items[..1]), &items[1]),
            history_context_hash(&items),
        );
    }

    #[test]
    fn provider_context_projection_whitelists_only_model_semantics() {
        let response_items = vec![
            AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("encrypted".into()))
                .with_graph_metadata(
                    Some("rs_provider".into()),
                    Some(AiItemStatus::Completed),
                    AiItemProvenance::Provider,
                    AiItemAudience::Client,
                ),
            AiItem::output_text("answer").with_graph_metadata(
                Some("msg_provider".into()),
                Some(AiItemStatus::Completed),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            ),
            AiItem::function_call(ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            })
            .with_graph_metadata(
                Some("fc_provider".into()),
                Some(AiItemStatus::Completed),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            ),
            AiItem::function_call_output("call_1", serde_json::Value::String("result".into()))
                .with_graph_metadata(
                    Some("fco_provider".into()),
                    Some(AiItemStatus::Completed),
                    AiItemProvenance::Client,
                    AiItemAudience::Provider,
                ),
        ];
        let replay_items = vec![
            AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("encrypted".into())),
            AiItem::output_text("answer"),
            AiItem::function_call(ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            }),
            AiItem::function_call_output("call_1", serde_json::Value::String("result".into())),
        ];

        assert!(history_items_equal(&response_items, &replay_items));

        for changed in [
            vec![
                AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("different".into())),
                replay_items[1].clone(),
                replay_items[2].clone(),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                AiItem::output_text("different"),
                replay_items[2].clone(),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                AiItem::function_call(ToolCall {
                    id: "call_2".into(),
                    name: "lookup".into(),
                    arguments: "{\"value\":1}".into(),
                }),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                AiItem::function_call(ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{\"value\":2}".into(),
                }),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                replay_items[2].clone(),
                AiItem::function_call_output("call_2", serde_json::Value::String("result".into())),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                replay_items[2].clone(),
                AiItem::function_call_output(
                    "call_1",
                    serde_json::Value::String("different".into()),
                ),
            ],
        ] {
            assert!(!history_items_equal(&response_items, &changed));
        }

        assert!(!history_items_equal(
            &[item(ContentBlock::Text {
                text: "question".into(),
                cache_control: None,
            })],
            &[item(ContentBlock::Text {
                text: "different".into(),
                cache_control: None,
            })]
        ));
        assert!(history_items_equal(
            &[item(ContentBlock::Text {
                text: "question".into(),
                cache_control: Some(crate::protocol::ir::CacheControl::ephemeral()),
            })],
            &[item(ContentBlock::Text {
                text: "question".into(),
                cache_control: None,
            })]
        ));
    }

    #[test]
    fn anthropic_and_responses_outputs_share_history_and_cache_identity() {
        let anthropic_output =
            |reasoning: &str, signature: &str, text: &str, call_id: &str, input| {
                vec![AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Thinking {
                            thinking: reasoning.into(),
                            signature: Some(signature.into()),
                        },
                        ContentBlock::Text {
                            text: text.into(),
                            cache_control: None,
                        },
                        ContentBlock::ToolUse {
                            id: call_id.into(),
                            name: "lookup".into(),
                            input,
                            cache_control: None,
                        },
                    ]),
                    tool_calls: Some(vec![ToolCall {
                        id: call_id.into(),
                        name: "lookup".into(),
                        arguments: "{\"value\":1}".into(),
                    }]),
                    tool_call_id: None,
                    meta: None,
                }]
            };
        let responses = vec![
            AiItem::reasoning(
                vec!["summary".into()],
                vec!["reasoning".into()],
                Some("opaque".into()),
            ),
            AiItem::output_text("answer"),
            AiItem::function_call(ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            }),
        ];
        let anthropic = anthropic_output(
            "summaryreasoning",
            "opaque",
            "answer",
            "call_1",
            serde_json::json!({"value": 1}),
        );

        assert!(history_items_equal(&responses, &anthropic));
        assert_eq!(item_hashes(&responses), item_hashes(&anthropic));

        for changed in [
            anthropic_output(
                "different",
                "opaque",
                "answer",
                "call_1",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "different",
                "answer",
                "call_1",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "opaque",
                "different",
                "call_1",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "opaque",
                "answer",
                "call_2",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "opaque",
                "answer",
                "call_1",
                serde_json::json!({"value": 2}),
            ),
        ] {
            assert!(!history_items_equal(&responses, &changed));
            assert_ne!(item_hashes(&responses), item_hashes(&changed));
        }

        let responses_output = vec![AiItem::function_call_output(
            "call_1",
            serde_json::json!("result"),
        )];
        let anthropic_output = vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: serde_json::json!("result"),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        }];
        assert!(history_items_equal(&responses_output, &anthropic_output));
        assert_eq!(
            item_hashes(&responses_output),
            item_hashes(&anthropic_output)
        );

        for changed in [
            AiItem::function_call_output("call_2", serde_json::json!("result")),
            AiItem::function_call_output("call_1", serde_json::json!("different")),
        ] {
            assert!(!history_items_equal(
                &anthropic_output,
                std::slice::from_ref(&changed)
            ));
        }
    }
}
