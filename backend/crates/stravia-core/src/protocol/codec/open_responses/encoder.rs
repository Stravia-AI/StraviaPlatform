//! Open Responses Protocol 2026-04-24 request encoder.

use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::ir::AiRequest;
use crate::protocol::ir::request::{
    ContentBlock, MediaSource, MessageContent, ResponseFormat, Role, ToolChoice,
};

/// Encoder for the dated Open Responses request contract.
pub struct ResponsesEncoder;

// Open Responses fields are emitted only from the canonical IR and dated extension.
impl ResponsesEncoder {
    pub(crate) fn encode_request(&self, req: &AiRequest) -> Result<(Value, HeaderMap)> {
        validate_target_thinking_control(req)?;
        let extension = match req.ext.as_ref() {
            Some(crate::protocol::ir::ProtocolExt::OpenResponses(extension)) => Some(extension),
            _ => None,
        };
        if matches!(req.response_format, Some(ResponseFormat::JsonObject)) {
            anyhow::bail!(
                "Open Responses 2026-04-24 cannot represent canonical json_object response format"
            );
        }
        let mut input: Vec<Value> = Vec::new();

        for item in &req.items {
            if let Some(reference_id) = item
                .meta
                .as_ref()
                .and_then(|meta| meta.get("__open_responses_item_reference"))
                .and_then(Value::as_str)
            {
                input.push(serde_json::json!({
                    "type": "item_reference",
                    "id": reference_id,
                }));
                continue;
            }
            if let Some((summary, content, signature)) = item.reasoning_ref() {
                let mut reasoning = serde_json::json!({
                    "type": "reasoning",
                    "summary": summary.iter().map(|text| serde_json::json!({
                        "type": "summary_text",
                        "text": text
                    })).collect::<Vec<_>>(),
                    "content": if content.is_empty() {
                        Value::Array(Vec::new())
                    } else {
                        Value::Array(content.iter().map(|text| serde_json::json!({
                            "type": "reasoning_text",
                            "text": text
                        })).collect())
                    }
                });
                insert_reasoning_metadata(&mut reasoning, item, signature.is_some());
                if let Some(signature) = signature {
                    reasoning["encrypted_content"] = Value::String(signature.to_owned());
                }
                input.push(reasoning);
                continue;
            }
            // Chat `reasoning_content` and tool calls share one assistant item. Handle that
            // combination before the single-Thinking fast path, which would omit the calls.
            if item
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
                && let Some(items) = encode_mixed_assistant_items(item)?
            {
                input.extend(items);
                continue;
            }
            if let Some((text, signature)) = item.thinking_ref() {
                let mut reasoning = serde_json::json!({
                    "type": "reasoning",
                    "summary": [{
                        "type": "summary_text",
                        "text": text,
                    }],
                    "content": [],
                });
                insert_reasoning_metadata(&mut reasoning, item, signature.is_some());
                if let Some(signature) = signature {
                    reasoning["encrypted_content"] = Value::String(signature.to_owned());
                }
                input.push(reasoning);
                continue;
            }
            if let Some(raw) = item.unknown_ref() {
                let item_type = raw.get("type").and_then(Value::as_str).unwrap_or_default();
                if !super::is_registered_extension_item(item_type) {
                    anyhow::bail!("unregistered Open Responses input extension: {item_type}");
                }
                input.push(raw.clone());
                continue;
            }
            if item.role == Role::Tool {
                let mut output = serde_json::json!({
                    "type": "function_call_output",
                    "call_id": item.tool_call_id.clone().unwrap_or_default(),
                    "output": encode_tool_output(&item.content)?,
                });
                insert_item_metadata(&mut output, item, true);
                input.push(output);
                continue;
            }

            if let Some(items) = encode_mixed_assistant_items(item)? {
                input.extend(items);
                continue;
            }

            if let Some(tool_calls) = &item.tool_calls {
                for tool_call in tool_calls {
                    let mut call = serde_json::json!({
                        "type": "function_call",
                        "call_id": tool_call.id,
                        "name": tool_call.name,
                        "arguments": tool_call.arguments,
                    });
                    if tool_calls.len() == 1 {
                        insert_item_metadata(&mut call, item, true);
                    }
                    input.push(call);
                }
            }

            let content = if let Some(refusal) = item.refusal_ref() {
                Some(vec![serde_json::json!({
                    "type": "refusal",
                    "refusal": refusal,
                })])
            } else {
                encode_message_content(&item.content, item.role)?
            };
            if let Some(content) = content {
                let role = match item.role {
                    Role::System => "system",
                    Role::Developer => "developer",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => unreachable!("tool items were emitted above"),
                };
                let mut message = serde_json::json!({
                    "type": "message",
                    "role": role,
                    "content": content,
                });
                insert_item_metadata(&mut message, item, true);
                if let Some(phase) = item.meta.as_ref().and_then(|meta| meta.get("phase")) {
                    message["phase"] = phase.clone();
                }
                input.push(message);
            }
        }

        if input.is_empty()
            && !extension.is_some_and(|extension| extension.previous_response_id.is_some())
        {
            anyhow::bail!("responses request requires input or previous_response_id");
        }

        let mut body = serde_json::json!({
            "model": req.model,
            "stream": req.stream.enabled,
        });
        if !input.is_empty() {
            body.as_object_mut()
                .expect("request body is an object")
                .insert("input".into(), Value::Array(input));
        }
        insert_request_control_fields(
            body.as_object_mut().expect("request body is an object"),
            req,
            extension,
        );

        Ok((body, HeaderMap::new()))
    }

    pub(crate) fn egress_path(&self, _model: &str, _stream: bool) -> String {
        "/v1/responses".to_string()
    }
}

pub(crate) fn response_profile_from_request(req: &AiRequest) -> Value {
    let extension = match req.ext.as_ref() {
        Some(crate::protocol::ir::ProtocolExt::OpenResponses(extension)) => Some(extension),
        _ => None,
    };
    let mut profile = serde_json::Map::new();
    insert_request_control_fields(&mut profile, req, extension);
    profile.remove("include");
    profile.remove("stream_options");
    if let Some(extension) = extension {
        profile.insert(
            "metadata".into(),
            extension
                .metadata
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
        );
        profile.insert(
            "safety_identifier".into(),
            extension
                .safety_identifier
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(profile)
}
pub(crate) fn effective_response_profile_from_request(req: &AiRequest) -> Value {
    let Value::Object(mut profile) = response_profile_from_request(req) else {
        unreachable!("response profile is always an object");
    };
    for (key, value) in [
        ("temperature", serde_json::json!(1.0)),
        ("top_p", serde_json::json!(1.0)),
        ("presence_penalty", serde_json::json!(0.0)),
        ("frequency_penalty", serde_json::json!(0.0)),
        ("parallel_tool_calls", serde_json::json!(true)),
        ("tools", serde_json::json!([])),
        ("tool_choice", serde_json::json!("auto")),
        ("reasoning", serde_json::Value::Null),
        ("max_output_tokens", serde_json::Value::Null),
        ("max_tool_calls", serde_json::Value::Null),
        ("text", serde_json::json!({ "format": { "type": "text" } })),
        ("top_logprobs", serde_json::json!(0)),
        ("truncation", serde_json::json!("disabled")),
        ("service_tier", serde_json::json!("default")),
    ] {
        profile.entry(key).or_insert(value);
    }
    Value::Object(profile)
}

fn insert_request_control_fields(
    obj: &mut serde_json::Map<String, Value>,
    req: &AiRequest,
    extension: Option<&crate::protocol::ir::OpenResponsesExt>,
) {
    obj.insert(
        "store".into(),
        extension
            .and_then(|extension| extension.store)
            .unwrap_or(true)
            .into(),
    );
    if let Some(instructions) = &req.instructions {
        obj.insert("instructions".into(), Value::String(instructions.clone()));
    } else if extension.is_some_and(|extension| extension.instructions_present) {
        obj.insert("instructions".into(), Value::Null);
    }
    if let Some(value) = req.generation.temperature {
        obj.insert("temperature".into(), value.into());
    }
    if let Some(value) = req.generation.top_p {
        obj.insert("top_p".into(), value.into());
    }
    if let Some(value) = req.generation.max_tokens {
        obj.insert("max_output_tokens".into(), value.into());
    }
    if let Some(value) = req.generation.presence_penalty {
        obj.insert("presence_penalty".into(), value.into());
    }
    if let Some(value) = req.generation.frequency_penalty {
        obj.insert("frequency_penalty".into(), value.into());
    }
    if let Some(value) = req.parallel_tool_calls {
        obj.insert("parallel_tool_calls".into(), value.into());
    }
    let converted_to_responses = req
        .meta
        .source_protocol
        .is_some_and(|source| source != crate::protocol::ids::OPEN_RESPONSES_2026_04_24);
    if let Some(control) = req.reasoning.target_control.as_ref() {
        let mut reasoning = serde_json::Map::new();
        let effort = match control {
            crate::thinking::TargetThinkingControl::Effort { value } => value.as_str(),
            crate::thinking::TargetThinkingControl::Disabled => "none",
            _ => return,
        };
        reasoning.insert("effort".into(), Value::String(effort.into()));
        if req.meta.source_protocol == Some(crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01) {
            match req.reasoning.display.as_deref() {
                Some("omitted") => {}
                Some("summarized") | None => {
                    reasoning.insert("summary".into(), Value::String("auto".into()));
                }
                Some(summary) => {
                    reasoning.insert("summary".into(), Value::String(summary.into()));
                }
            }
        } else if let Some(summary) = &req.reasoning.display {
            reasoning.insert("summary".into(), Value::String(summary.clone()));
        } else if converted_to_responses {
            reasoning.insert("summary".into(), Value::String("auto".into()));
        }
        if !reasoning.is_empty() {
            obj.insert("reasoning".into(), Value::Object(reasoning));
        }
    }
    if let Some(tools) = &req.tools {
        let tools = tools
            .iter()
            .map(|tool| {
                if tool.name.starts_with("__builtin__") {
                    tool.parameters.clone()
                } else {
                    let mut encoded = serde_json::json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    });
                    if let Some(strict) = tool.strict {
                        encoded["strict"] = Value::Bool(strict);
                    }
                    encoded
                }
            })
            .collect();
        obj.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tool_choice) = &req.tool_choice {
        obj.insert("tool_choice".into(), tool_choice_to_value(tool_choice));
    }
    if let Some(format) = req.response_format.as_ref() {
        let format = match format {
            ResponseFormat::Text => serde_json::json!({"type": "text"}),
            ResponseFormat::JsonSchema {
                name,
                schema,
                strict,
            } => {
                let mut format = serde_json::json!({
                    "type": "json_schema",
                    "name": name,
                    "schema": schema,
                });
                if let Some(strict) = strict {
                    format["strict"] = Value::Bool(*strict);
                }
                format
            }
            ResponseFormat::JsonObject => Value::Null,
        };
        if !format.is_null() {
            obj.insert("text".into(), serde_json::json!({"format": format}));
        }
    }
    let Some(extension) = extension else {
        return;
    };
    if let Some(value) = extension.background {
        obj.insert("background".into(), value.into());
    }
    if let Some(value) = &extension.previous_response_id {
        obj.insert("previous_response_id".into(), Value::String(value.clone()));
    }
    if let Some(value) = &extension.include {
        obj.insert(
            "include".into(),
            Value::Array(value.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(value) = &extension.stream_options {
        obj.insert("stream_options".into(), value.clone());
    }
    if let Some(value) = extension.max_tool_calls {
        obj.insert("max_tool_calls".into(), value.into());
    }
    if let Some(value) = &extension.prompt_cache_key {
        obj.insert("prompt_cache_key".into(), Value::String(value.clone()));
    }
    if let Some(value) = extension.top_logprobs {
        obj.insert("top_logprobs".into(), value.into());
    }
    if let Some(value) = &extension.truncation {
        obj.insert("truncation".into(), Value::String(value.clone()));
    }
    if let Some(value) = &extension.text {
        obj.insert("text".into(), value.clone());
    }
    if let Some(value) = &extension.service_tier {
        obj.insert("service_tier".into(), Value::String(value.clone()));
    }
    if !extension.passthrough_tools.is_empty() {
        let tools = obj
            .entry("tools")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("encoded tools are an array");
        tools.extend(extension.passthrough_tools.iter().cloned());
    }
    for (key, value) in &extension.passthrough_body {
        obj.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn validate_target_thinking_control(req: &AiRequest) -> anyhow::Result<()> {
    let Some(control) = req.reasoning.target_control.as_ref() else {
        return Ok(());
    };
    match control {
        crate::thinking::TargetThinkingControl::Effort { .. }
        | crate::thinking::TargetThinkingControl::Disabled => Ok(()),
        _ => anyhow::bail!("Open Responses cannot represent Target Thinking Control {control:?}"),
    }
}

fn insert_item_metadata(encoded: &mut Value, item: &crate::protocol::ir::AiItem, status: bool) {
    let object = encoded
        .as_object_mut()
        .expect("encoded Open Responses item is an object");
    if let Some(id) = item.id_ref() {
        object.insert("id".into(), Value::String(id.to_owned()));
    }
    if status && let Some(status) = item.status() {
        object.insert("status".into(), Value::String(status.as_str().to_owned()));
    }
}

fn insert_reasoning_metadata(
    encoded: &mut Value,
    item: &crate::protocol::ir::AiItem,
    has_encrypted_content: bool,
) {
    insert_item_metadata(encoded, item, false);
    if has_encrypted_content {
        // Provider ciphertext can be bound to the original output item identity. A replayed
        // client projection may carry a different gateway ID, so the optional input ID is unsafe.
        encoded
            .as_object_mut()
            .expect("encoded Open Responses reasoning item is an object")
            .remove("id");
    }
}

fn encode_mixed_assistant_items(item: &crate::protocol::ir::AiItem) -> Result<Option<Vec<Value>>> {
    let MessageContent::Blocks(blocks) = &item.content else {
        return Ok(None);
    };
    if item.role != Role::Assistant
        || !blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking { .. }))
    {
        return Ok(None);
    }

    let mut items = Vec::with_capacity(blocks.len());
    let mut message_content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { .. } | ContentBlock::Refusal { .. } => {
                message_content.push(encode_responses_content_block(block, "output_text")?);
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                push_assistant_message(&mut items, &mut message_content, item);
                let summary = if thinking.is_empty() {
                    Vec::new()
                } else {
                    vec![serde_json::json!({
                        "type": "summary_text",
                        "text": thinking,
                    })]
                };
                let mut reasoning = serde_json::json!({
                    "type": "reasoning",
                    "summary": summary,
                    "content": [],
                });
                insert_reasoning_metadata(&mut reasoning, item, signature.is_some());
                if let Some(signature) = signature {
                    reasoning["encrypted_content"] = Value::String(signature.clone());
                }
                items.push(reasoning);
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                push_assistant_message(&mut items, &mut message_content, item);
                let mut call = serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                });
                if item
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.len() == 1)
                {
                    insert_item_metadata(&mut call, item, true);
                }
                items.push(call);
            }
            other => {
                anyhow::bail!(
                    "responses request cannot encode {} content block in assistant message",
                    content_block_kind(other)
                );
            }
        }
    }
    push_assistant_message(&mut items, &mut message_content, item);

    if let Some(tool_calls) = &item.tool_calls {
        for tool_call in tool_calls {
            let represented = blocks.iter().any(
                |block| matches!(block, ContentBlock::ToolUse { id, .. } if id == &tool_call.id),
            );
            if represented {
                continue;
            }
            items.push(serde_json::json!({
                "type": "function_call",
                "call_id": tool_call.id,
                "name": tool_call.name,
                "arguments": tool_call.arguments,
            }));
        }
    }

    Ok(Some(items))
}

fn push_assistant_message(
    items: &mut Vec<Value>,
    content: &mut Vec<Value>,
    item: &crate::protocol::ir::AiItem,
) {
    if content.is_empty() {
        return;
    }
    let mut message = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": std::mem::take(content),
    });
    insert_item_metadata(&mut message, item, true);
    if let Some(phase) = item.meta.as_ref().and_then(|meta| meta.get("phase")) {
        message["phase"] = phase.clone();
    }
    items.push(message);
}

fn encode_message_content(content: &MessageContent, role: Role) -> Result<Option<Vec<Value>>> {
    let text_type = match role {
        Role::System | Role::Developer | Role::User => "input_text",
        Role::Assistant => "output_text",
        Role::Tool => unreachable!("tool results use function_call_output"),
    };

    match content {
        MessageContent::Text(text) => {
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(vec![serde_json::json!({
                    "type": text_type,
                    "text": text
                })]))
            }
        }
        MessageContent::Blocks(blocks) => {
            let mut encoded = Vec::with_capacity(blocks.len());
            for block in blocks {
                if matches!(block, ContentBlock::Text { text, .. } if text.is_empty()) {
                    continue;
                }
                if role == Role::Assistant {
                    if matches!(block, ContentBlock::ToolUse { .. }) {
                        // Canonical assistant tool use is emitted below from
                        // `message.tool_calls` as top-level function_call items.
                        continue;
                    }
                    if !matches!(
                        block,
                        ContentBlock::Text { .. } | ContentBlock::Refusal { .. }
                    ) {
                        anyhow::bail!(
                            "responses request cannot encode {} content block in assistant message",
                            content_block_kind(block)
                        );
                    }
                }
                encoded.push(encode_responses_content_block(block, text_type)?);
            }
            if encoded.is_empty() {
                Ok(None)
            } else {
                Ok(Some(encoded))
            }
        }
    }
}

fn request_tool_output_part(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let allowed = |fields: &[&str]| object.keys().all(|key| fields.contains(&key.as_str()));
    match object.get("type").and_then(Value::as_str) {
        Some("input_text") => {
            allowed(&["type", "text"]) && object.get("text").is_some_and(Value::is_string)
        }
        Some("input_image") => {
            allowed(&["type", "image_url", "detail"])
                && object.get("image_url").is_some_and(Value::is_string)
                && object.get("detail").is_none_or(|detail| {
                    detail.is_null()
                        || detail
                            .as_str()
                            .is_some_and(|detail| matches!(detail, "low" | "high" | "auto"))
                })
        }
        Some("input_file") => {
            allowed(&["type", "file_data", "file_url", "filename"])
                && ["file_data", "file_url"]
                    .iter()
                    .any(|field| object.get(*field).is_some_and(Value::is_string))
                && object
                    .get("filename")
                    .is_none_or(|filename| filename.is_null() || filename.is_string())
        }
        _ => false,
    }
}

fn normalize_tool_result_content(content: &Value) -> Result<Value> {
    match content {
        Value::String(_) => Ok(content.clone()),
        Value::Array(items) if items.iter().all(request_tool_output_part) => Ok(content.clone()),
        other => Ok(Value::String(serde_json::to_string(other)?)),
    }
}

pub(crate) fn tool_output_representable(content: &MessageContent) -> bool {
    match content {
        MessageContent::Text(_) => true,
        MessageContent::Blocks(blocks) => blocks.iter().all(|block| match block {
            ContentBlock::Text { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::ServerToolResult { .. } => true,
            ContentBlock::Image { source, detail, .. } => {
                matches!(source, MediaSource::Base64 { .. } | MediaSource::Url(_))
                    && detail
                        .as_deref()
                        .is_none_or(|detail| matches!(detail, "low" | "high" | "auto"))
            }
            ContentBlock::File { source, .. } => matches!(source, MediaSource::Url(_)),
            _ => false,
        }),
    }
}

pub(crate) fn encode_tool_output(content: &MessageContent) -> Result<Value> {
    match content {
        MessageContent::Text(text) => Ok(Value::String(text.clone())),
        MessageContent::Blocks(blocks) => {
            if let [
                ContentBlock::ToolResult { content, .. }
                | ContentBlock::ServerToolResult { content, .. },
            ] = blocks.as_slice()
            {
                return normalize_tool_result_content(content);
            }
            let mut output = Vec::with_capacity(blocks.len());
            for block in blocks {
                match block {
                    ContentBlock::ToolResult { content, .. }
                    | ContentBlock::ServerToolResult { content, .. } => {
                        match normalize_tool_result_content(content)? {
                            Value::Array(items) => output.extend(items),
                            Value::String(text) => {
                                output
                                    .push(serde_json::json!({"type": "input_text", "text": text}));
                            }
                            _ => unreachable!("normalized tool output is string or array"),
                        }
                    }
                    _ => output.push(encode_responses_content_block(block, "input_text")?),
                }
            }
            Ok(Value::Array(output))
        }
    }
}

fn response_tool_output_part(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let allowed = |fields: &[&str]| object.keys().all(|key| fields.contains(&key.as_str()));
    match object.get("type").and_then(Value::as_str) {
        Some("input_text") => {
            allowed(&["type", "text"]) && object.get("text").is_some_and(Value::is_string)
        }
        Some("input_image") => {
            allowed(&["type", "image_url", "detail"])
                && object.contains_key("image_url")
                && object
                    .get("image_url")
                    .is_some_and(|url| url.is_null() || url.is_string())
                && object
                    .get("detail")
                    .and_then(Value::as_str)
                    .is_some_and(|detail| matches!(detail, "low" | "high" | "auto"))
        }
        Some("input_file") => {
            allowed(&["type", "filename", "file_url"])
                && object.get("filename").is_none_or(Value::is_string)
                && object.get("file_url").is_none_or(Value::is_string)
        }
        _ => false,
    }
}

pub(crate) fn encode_response_tool_output(content: &MessageContent) -> Result<Value> {
    match content {
        MessageContent::Text(text) => Ok(Value::String(text.clone())),
        MessageContent::Blocks(blocks) => {
            if let [
                ContentBlock::ToolResult { content, .. }
                | ContentBlock::ServerToolResult { content, .. },
            ] = blocks.as_slice()
            {
                return match content {
                    Value::String(_) => Ok(content.clone()),
                    Value::Array(items) if items.iter().all(response_tool_output_part) => {
                        Ok(content.clone())
                    }
                    other => Ok(Value::String(serde_json::to_string(other)?)),
                };
            }
            let mut output = Vec::with_capacity(blocks.len());
            for block in blocks {
                match block {
                    ContentBlock::ToolResult { content, .. }
                    | ContentBlock::ServerToolResult { content, .. } => match content {
                        Value::Array(items) if items.iter().all(response_tool_output_part) => {
                            output.extend(items.iter().cloned());
                        }
                        other => output.push(serde_json::json!({
                            "type": "input_text",
                            "text": match other {
                                Value::String(text) => text.clone(),
                                value => serde_json::to_string(value)?,
                            }
                        })),
                    },
                    _ => {
                        let mut encoded = encode_responses_content_block(block, "input_text")?;
                        if encoded.get("type").and_then(Value::as_str) == Some("input_image")
                            && encoded.get("detail").is_none()
                        {
                            encoded["detail"] = Value::String("auto".into());
                        }
                        if !response_tool_output_part(&encoded) {
                            anyhow::bail!(
                                "responses output cannot encode {} tool-result content block",
                                content_block_kind(block)
                            );
                        }
                        output.push(encoded);
                    }
                }
            }
            Ok(Value::Array(output))
        }
    }
}

fn encode_responses_content_block(block: &ContentBlock, text_type: &str) -> Result<Value> {
    match block {
        ContentBlock::Text { text, .. } => Ok(serde_json::json!({
            "type": text_type,
            "text": text
        })),
        ContentBlock::Image { source, detail, .. } => {
            let mut encoded = serde_json::json!({"type": "input_image"});
            match source {
                MediaSource::Base64 { media_type, data } => {
                    encoded["image_url"] =
                        Value::String(format!("data:{media_type};base64,{data}"));
                }
                MediaSource::Url(url) => {
                    encoded["image_url"] = Value::String(url.clone());
                }
                MediaSource::FileId { file_id, detail } => {
                    encoded["file_id"] = Value::String(file_id.clone());
                    if let Some(detail) = detail {
                        encoded["detail"] = Value::String(detail.clone());
                    }
                }
            }
            if let Some(detail) = detail {
                encoded["detail"] = Value::String(detail.clone());
            }
            Ok(encoded)
        }
        ContentBlock::Refusal { refusal } => Ok(serde_json::json!({
            "type": "refusal",
            "refusal": refusal
        })),
        ContentBlock::File { source, media_type } => {
            let mut encoded = serde_json::json!({"type": "input_file"});
            match source {
                MediaSource::Base64 {
                    media_type: source_media_type,
                    data,
                } => {
                    let media_type = media_type
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .unwrap_or(source_media_type);
                    encoded["file_data"] =
                        Value::String(format!("data:{media_type};base64,{data}"));
                }
                MediaSource::Url(url) => {
                    encoded["file_url"] = Value::String(url.clone());
                }
                MediaSource::FileId { file_id, .. } => {
                    encoded["file_id"] = Value::String(file_id.clone());
                }
            }
            Ok(encoded)
        }
        ContentBlock::Video { source, media_type } => {
            let video_url = match source {
                MediaSource::Base64 {
                    media_type: source_media_type,
                    data,
                } => {
                    let media_type = media_type
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .unwrap_or(source_media_type);
                    format!("data:{media_type};base64,{data}")
                }
                MediaSource::Url(url) => url.clone(),
                MediaSource::FileId { .. } => {
                    anyhow::bail!(
                        "responses request cannot encode a file-id video: dated input_video requires video_url"
                    )
                }
            };
            Ok(serde_json::json!({
                "type": "input_video",
                "video_url": video_url
            }))
        }
        ContentBlock::Audio { .. } => anyhow::bail!(
            "responses request cannot encode {} content block: Responses API has no supported wire mapping",
            content_block_kind(block)
        ),
        _ => anyhow::bail!(
            "responses request cannot encode {} content block",
            content_block_kind(block)
        ),
    }
}

fn content_block_kind(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::Image { .. } => "image",
        ContentBlock::Audio { .. } => "audio",
        ContentBlock::File { .. } => "file",
        ContentBlock::Video { .. } => "video",
        ContentBlock::Thinking { .. } => "thinking",
        ContentBlock::Reasoning { .. } => "reasoning",
        ContentBlock::RedactedThinking { .. } => "redacted_thinking",
        ContentBlock::ToolUse { .. } => "tool_use",
        ContentBlock::ToolResult { .. } => "tool_result",
        ContentBlock::ServerToolUse { .. } => "server_tool_use",
        ContentBlock::ServerToolResult { .. } => "server_tool_result",
        ContentBlock::Document { .. } => "document",
        ContentBlock::SearchResult { .. } => "search_result",
        ContentBlock::Citation { .. } => "citation",
        ContentBlock::ExecutableCode { .. } => "executable_code",
        ContentBlock::CodeExecutionResult { .. } => "code_execution_result",
        ContentBlock::ContainerUpload { .. } => "container_upload",
        ContentBlock::Refusal { .. } => "refusal",
        ContentBlock::Unknown { .. } => "unknown",
    }
}

fn tool_choice_to_value(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Named { name } => serde_json::json!({
            "type": "function",
            "name": name
        }),
        ToolChoice::Raw(v) => v.clone(),
    }
}

#[cfg(test)]
mod tests;
