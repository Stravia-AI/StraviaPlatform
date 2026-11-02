use serde_json::Value;

use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiItemAudience;
use stravia_runtime_contract::protocol::ir::AiItemProvenance;
use stravia_runtime_contract::protocol::ir::AiItemStatus;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;

pub struct ResponsesResponseFormatter;

impl ResponsesResponseFormatter {
    pub(crate) fn format_output_items(&self, items: &[AiItem], resp_id: &str) -> Vec<Value> {
        let mut output: Vec<Value> = Vec::new();

        for original in items {
            let projected = split_assistant_output(original);
            for item in projected
                .as_deref()
                .unwrap_or(std::slice::from_ref(original))
            {
                if let Some(native) = super::native_compaction_item(item) {
                    output.push(native);
                    continue;
                }
                if item.role == Role::Assistant
                    && item
                        .tool_calls
                        .as_ref()
                        .is_none_or(|calls| calls.is_empty())
                    && let Some(content) = wire_message_content(item)
                {
                    let mut message = serde_json::json!({
                        "type": "message",
                        "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| gateway_item_id("msg", resp_id, output.len())),
                        "status": item.status().map(|status| status.as_str()).unwrap_or("completed"),
                        "role": "assistant",
                        "content": content,
                    });
                    if let Some(phase) = item.meta.as_ref().and_then(|meta| meta.get("phase")) {
                        message["phase"] = phase.clone();
                    }
                    super::encoder::insert_item_metadata(&mut message, item, true);
                    output.push(message);
                    continue;
                }
                let output_start = output.len();
                if let Some((summary, content, signature)) = item.reasoning_ref() {
                    let mut reasoning = serde_json::json!({
                        "type": "reasoning",
                        "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| {
                            gateway_item_id("rs", resp_id, output.len())
                        }),
                        "summary": summary.iter().map(|text| serde_json::json!({
                            "type": "summary_text",
                            "text": text
                        })).collect::<Vec<_>>()
                    });
                    if !content.is_empty() {
                        reasoning["content"] = Value::Array(
                            content
                                .iter()
                                .map(|text| {
                                    serde_json::json!({
                                        "type": "reasoning_text",
                                        "text": text
                                    })
                                })
                                .collect(),
                        );
                    }
                    if let Some(signature) = signature {
                        reasoning["encrypted_content"] = Value::String(signature.to_owned());
                    }
                    output.push(reasoning);
                } else if let Some((text, _)) = item.thinking_ref() {
                    let mut reasoning = serde_json::json!({
                        "type": "reasoning",
                        "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| {
                            gateway_item_id("rs", resp_id, output.len())
                        }),
                        "summary": []
                    });
                    if !text.is_empty() {
                        reasoning["content"] = serde_json::json!([{
                            "type": "reasoning_text",
                            "text": text
                        }]);
                    }
                    output.push(reasoning);
                } else if let Some(call) = item.function_call_ref() {
                    output.push(serde_json::json!({
                        "type": "function_call",
                        "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| gateway_item_id("fc", resp_id, output.len())),
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                        "status": item.status().map(|status| status.as_str()).unwrap_or("completed")
                    }));
                } else if let Some((call_id, content)) = item.function_call_output_ref() {
                    let Some(output_value) = function_output_value(content) else {
                        continue;
                    };
                    output.push(serde_json::json!({
                        "type": "function_call_output",
                        "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| gateway_item_id("fco", resp_id, output.len())),
                        "call_id": call_id,
                        "output": output_value,
                        "status": item.status().map(|status| status.as_str()).unwrap_or("completed")
                    }));
                } else if let Some(raw) = item.unknown_ref() {
                    output.push(wire_extension_item(
                        raw,
                        resp_id,
                        output.len(),
                        item.id_ref(),
                    ));
                }
                if output.len() > output_start {
                    super::encoder::insert_item_metadata(&mut output[output_start], item, false);
                }
            }
        }

        output
    }

    pub fn format_response(&self, resp: &AiResponse) -> Value {
        let resp_id = if resp.id.is_empty() {
            stravia_runtime_contract::identifier::new_id()
        } else {
            resp.id.clone()
        };
        let output = self.format_output_items(&resp.items, &resp_id);
        let usage_known = resp.usage.required_components_known;
        let usage = if usage_known {
            let mut input_tokens_details = serde_json::json!({
                "cached_tokens": resp.usage.cache_read_tokens,
            });
            if let Some(cache_write_tokens) = resp.usage.cache_creation_tokens {
                input_tokens_details["cache_write_tokens"] = cache_write_tokens.into();
            }
            serde_json::json!({
                "input_tokens": resp.usage.prompt_tokens,
                "output_tokens": resp.usage.completion_tokens,
                "total_tokens": resp.usage.total_tokens,
                "input_tokens_details": input_tokens_details,
                "output_tokens_details": {
                    "reasoning_tokens": resp.usage.reasoning_tokens,
                },
            })
        } else {
            Value::Null
        };

        let terminal = resp
            .vendor
            .egress
            .get("__open_responses_terminal")
            .and_then(Value::as_object);
        let status = terminal
            .and_then(|terminal| terminal.get("status"))
            .and_then(Value::as_str)
            .filter(|status| {
                matches!(
                    *status,
                    "queued" | "in_progress" | "completed" | "incomplete" | "failed"
                )
            })
            .unwrap_or_else(|| {
                if matches!(
                    resp.stop_reason.as_deref(),
                    Some("length" | "max_tokens" | "content_filter")
                ) {
                    "incomplete"
                } else if resp.stop_reason.as_deref() == Some("failed") {
                    "failed"
                } else {
                    "completed"
                }
            });
        let mut formatted = response_resource_snapshot(
            &resp_id,
            &resp.model,
            status,
            output,
            Value::Null,
            Value::Null,
            usage,
        );
        if let Some(resource) = formatted.as_object_mut() {
            for key in [
                "__open_responses_effective_request",
                "__open_responses_response_profile",
            ] {
                if let Some(profile) = resp.vendor.ingress.get(key).and_then(Value::as_object) {
                    // Chat 字段存在性只供内部历史转换使用，不能穿透到 Responses 资源。
                    resource.extend(
                        profile
                            .iter()
                            .filter(|(key, _)| key.as_str() != CHAT_REASONING_FIELD_META)
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                }
            }
            if status == "failed" {
                resource.insert(
                    "error".into(),
                    serde_json::json!({
                        "type": "upstream_error",
                        "code": "upstream_response_failed",
                        "message": "The selected Provider failed the response.",
                        "param": null
                    }),
                );
            }
            if status == "incomplete"
                && let Some(details) = terminal
                    .and_then(|terminal| terminal.get("incomplete_details"))
                    .filter(|details| !details.is_null())
            {
                resource.insert("incomplete_details".into(), details.clone());
            }
        }
        if let Some(profile) = resp
            .vendor
            .ingress
            .get("__open_responses_effective_request")
            .and_then(Value::as_object)
            && let Some(resource) = formatted.as_object_mut()
        {
            for (field, value) in profile {
                if resource.contains_key(field) {
                    resource.insert(field.clone(), value.clone());
                }
            }
        }
        formatted
    }
}

pub(crate) fn split_assistant_output(item: &AiItem) -> Option<Vec<AiItem>> {
    if item.role != Role::Assistant
        || item.unknown_ref().is_some()
        || item.is_compaction()
        || item.is_compaction_trigger()
    {
        return None;
    }
    let blocks = match &item.content {
        MessageContent::Blocks(blocks) => Some(blocks.as_slice()),
        MessageContent::Text(_) => None,
    };
    let has_calls = item
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    let has_mixed_blocks = blocks.is_some_and(|blocks| {
        let reasoning = blocks
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    ContentBlock::Reasoning { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::ToolUse { .. }
                )
            })
            .count();
        reasoning > 1
            || (reasoning > 0 && blocks.len() > 1)
            || blocks.iter().any(|block| {
                !matches!(
                    block,
                    ContentBlock::Text { .. }
                        | ContentBlock::Refusal { .. }
                        | ContentBlock::Reasoning { .. }
                        | ContentBlock::Thinking { .. }
                )
            })
    });
    if !has_mixed_blocks && !has_calls {
        return None;
    }
    // A single call with no message content already has a native carrier.
    if item.function_call_ref().is_some() && !has_mixed_blocks {
        return None;
    }
    let mut parts = Vec::new();
    let mut message = Vec::new();
    let mut represented_calls = Vec::new();
    let flush = |parts: &mut Vec<AiItem>, message: &mut Vec<ContentBlock>| {
        if !message.is_empty() {
            parts.push(AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(std::mem::take(message)),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            });
        }
    };
    if let Some(blocks) = blocks {
        for block in blocks {
            match block {
                ContentBlock::Text { .. } | ContentBlock::Refusal { .. } => {
                    message.push(block.clone())
                }
                ContentBlock::Reasoning { .. } | ContentBlock::Thinking { .. } => {
                    flush(&mut parts, &mut message);
                    parts.push(AiItem {
                        role: Role::Assistant,
                        content: MessageContent::Blocks(vec![block.clone()]),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: None,
                    });
                }
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    flush(&mut parts, &mut message);
                    if !represented_calls.contains(id) {
                        represented_calls.push(id.clone());
                        parts.push(AiItem::function_call(
                            stravia_runtime_contract::protocol::ir::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: input.to_string(),
                            },
                        ));
                    }
                }
                _ => {}
            }
        }
    } else if let MessageContent::Text(text) = &item.content
        && !text.is_empty()
    {
        message.push(ContentBlock::Text {
            text: text.clone(),
            cache_control: None,
        });
    }
    flush(&mut parts, &mut message);
    if let Some(calls) = &item.tool_calls {
        for call in calls {
            if !represented_calls.contains(&call.id) {
                represented_calls.push(call.id.clone());
                parts.push(AiItem::function_call(call.clone()));
            }
        }
    }
    for part in &mut parts {
        part.set_graph_metadata(
            None,
            item.status(),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        );
    }
    Some(parts)
}

fn wire_message_content(
    item: &stravia_runtime_contract::protocol::ir::AiItem,
) -> Option<Vec<Value>> {
    let generated = match &item.content {
        MessageContent::Text(text) if text.is_empty() => return None,
        MessageContent::Text(text) => vec![serde_json::json!({
            "type": "output_text",
            "text": text,
            "annotations": [],
            "logprobs": [],
        })],
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(serde_json::json!({
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                    "logprobs": [],
                })),
                ContentBlock::Refusal { refusal } => Some(serde_json::json!({
                    "type": "refusal",
                    "refusal": refusal,
                })),
                _ => None,
            })
            .collect::<Vec<_>>(),
    };
    if generated.is_empty() {
        return None;
    }
    let Some(original) = item
        .meta
        .as_ref()
        .and_then(|meta| meta.get("__open_responses_content"))
        .and_then(Value::as_array)
    else {
        return Some(generated);
    };
    if original.len() != generated.len()
        || original
            .iter()
            .zip(&generated)
            .any(|(original, generated)| original.get("type") != generated.get("type"))
    {
        return Some(generated);
    }
    let mut preserved = original.clone();
    for (wire, canonical) in preserved.iter_mut().zip(&generated) {
        match canonical.get("type").and_then(Value::as_str) {
            Some("output_text") => {
                if wire.get("text") != canonical.get("text") {
                    wire["annotations"] = Value::Array(Vec::new());
                    wire["logprobs"] = Value::Array(Vec::new());
                }
                wire["text"] = canonical["text"].clone();
            }
            Some("refusal") => wire["refusal"] = canonical["refusal"].clone(),
            _ => return Some(generated),
        }
    }
    Some(preserved)
}

pub(crate) fn function_output_value(content: &MessageContent) -> Option<Value> {
    super::encoder::encode_response_tool_output(content)
}

pub fn gateway_item_id(prefix: &str, response_id: &str, output_index: usize) -> String {
    format!("{prefix}_{response_id}_{output_index}")
}

pub fn stamp_output_graph_ids(resp: &AiResponse) -> Vec<AiItem> {
    let resp_id = if resp.id.is_empty() {
        stravia_runtime_contract::identifier::new_id()
    } else {
        resp.id.clone()
    };
    let mut items = Vec::with_capacity(resp.items.len());
    for item in &resp.items {
        if let Some(parts) = split_assistant_output(item) {
            items.extend(parts);
        } else {
            items.push(item.clone());
        }
    }
    let mut output_index = 0usize;
    for item in &mut items {
        let Some(prefix) = output_item_prefix(item) else {
            continue;
        };
        if item.id_ref().is_none() {
            let id = gateway_item_id(prefix, &resp_id, output_index);
            item.set_graph_metadata(
                Some(id),
                item.status().or(Some(AiItemStatus::Completed)),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            );
        }
        output_index += 1;
    }
    items
}

fn output_item_prefix(item: &AiItem) -> Option<&'static str> {
    if item.redacted_thinking_ref().is_some() {
        return Some("rs");
    }
    if item.role == Role::Assistant
        && item
            .tool_calls
            .as_ref()
            .is_none_or(|calls| calls.is_empty())
        && wire_message_content(item).is_some()
    {
        return Some("msg");
    }
    if item.reasoning_ref().is_some() || item.thinking_ref().is_some() {
        return Some("rs");
    }
    if item.function_call_ref().is_some() {
        return Some("fc");
    }
    if item.function_call_output_ref().is_some() {
        return Some("fco");
    }
    if item.unknown_ref().is_some() {
        return Some("item");
    }
    None
}

pub fn response_id_from_gateway_item_id(item_id: &str) -> Option<String> {
    let (prefix, remainder) = item_id.split_once('_')?;
    if !matches!(prefix, "msg" | "rs" | "fc" | "fco" | "item") {
        return None;
    }
    let (response_id, output_index) = remainder.rsplit_once('_')?;
    (!response_id.is_empty() && output_index.parse::<usize>().is_ok())
        .then(|| response_id.to_owned())
}

pub fn response_resource_snapshot(
    id: &str,
    model: &str,
    status: &str,
    output: Vec<Value>,
    incomplete_details: Value,
    error: Value,
    usage: Value,
) -> Value {
    let now = chrono::Utc::now().timestamp();
    let completed_at = matches!(status, "completed" | "failed" | "incomplete").then_some(now);
    serde_json::json!({
        "id": id,
        "object": "response",
        "created_at": now,
        "completed_at": completed_at,
        "status": status,
        "incomplete_details": incomplete_details,
        "model": model,
        "previous_response_id": null,
        "instructions": null,
        "output": output,
        "error": error,
        "tools": [],
        "tool_choice": "auto",
        "truncation": "disabled",
        "parallel_tool_calls": true,
        "text": {"format": {"type": "text"}},
        "top_p": 1.0,
        "presence_penalty": 0.0,
        "frequency_penalty": 0.0,
        "top_logprobs": 0,
        "temperature": 1.0,
        "reasoning": null,
        "usage": usage,
        "max_output_tokens": null,
        "max_tool_calls": null,
        "store": true,
        "background": false,
        "service_tier": "default",
        "metadata": {},
        "safety_identifier": null,
        "prompt_cache_key": null
    })
}

fn wire_extension_item(
    raw: &Value,
    response_id: &str,
    output_index: usize,
    preferred_id: Option<&str>,
) -> Value {
    let mut item = raw.clone();
    if let Some(object) = item.as_object_mut() {
        object.insert(
            "id".into(),
            Value::String(
                preferred_id
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| gateway_item_id("item", response_id, output_index)),
            ),
        );
        object.remove("stravia_artifact_id");
        object.remove("stravia_partial_images");
    }
    item
}

#[cfg(test)]
mod tests;
