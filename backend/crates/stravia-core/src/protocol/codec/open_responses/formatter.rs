use serde_json::Value;
use uuid::Uuid;

use crate::protocol::ir::{
    AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, AiResponse, ContentBlock,
    MessageContent, Role,
};

pub struct ResponsesResponseFormatter;

impl ResponsesResponseFormatter {
    pub(crate) fn format_response(&self, resp: &AiResponse) -> Value {
        let resp_id = if resp.id.is_empty() {
            format!("resp_{}", Uuid::new_v4().simple())
        } else {
            resp.id.clone()
        };

        let mut output: Vec<Value> = Vec::new();

        for item in &resp.items {
            if item.role == Role::Assistant
                && item.tool_calls.is_none()
                && let Some(content) = wire_message_content(item)
            {
                let mut message = serde_json::json!({
                    "type": "message",
                    "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| gateway_item_id("msg", &resp_id, output.len())),
                    "status": item.status().map(|status| status.as_str()).unwrap_or("completed"),
                    "role": "assistant",
                    "content": content,
                });
                if let Some(phase) = item.meta.as_ref().and_then(|meta| meta.get("phase")) {
                    message["phase"] = phase.clone();
                }
                output.push(message);
                continue;
            }
            if let Some((summary, content, signature)) = item.reasoning_ref() {
                let mut reasoning = serde_json::json!({
                    "type": "reasoning",
                    "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| {
                        gateway_item_id("rs", &resp_id, output.len())
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
            } else if let Some((text, signature)) = item.thinking_ref() {
                let mut reasoning = serde_json::json!({
                    "type": "reasoning",
                    "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| {
                        gateway_item_id("rs", &resp_id, output.len())
                    }),
                    "summary": [{
                        "type": "summary_text",
                        "text": text
                    }]
                });
                if let Some(signature) = signature {
                    reasoning["encrypted_content"] = Value::String(signature.to_owned());
                }
                output.push(reasoning);
            } else if let Some(call) = item.function_call_ref() {
                output.push(serde_json::json!({
                        "type": "function_call",
                        "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| gateway_item_id("fc", &resp_id, output.len())),
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                        "status": item.status().map(|status| status.as_str()).unwrap_or("completed")
                    }));
            } else if let Some((call_id, content)) = item.function_call_output_ref() {
                output.push(serde_json::json!({
                        "type": "function_call_output",
                        "id": item.id_ref().map(str::to_owned).unwrap_or_else(|| gateway_item_id("fco", &resp_id, output.len())),
                        "call_id": call_id,
                        "output": function_output_value(content),
                        "status": item.status().map(|status| status.as_str()).unwrap_or("completed")
                    }));
            } else if let Some(raw) = item.unknown_ref() {
                output.push(wire_extension_item(
                    raw,
                    &resp_id,
                    output.len(),
                    item.id_ref(),
                ));
            }
        }

        let usage_known = resp.usage.required_components_known;
        let usage = if usage_known {
            let mut input_tokens_details = serde_json::json!({
                "cached_tokens": resp.usage.cache_read_tokens.unwrap_or(0),
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
                    "reasoning_tokens": resp.usage.reasoning_tokens.unwrap_or(0),
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
                    resource.extend(profile.clone());
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

fn wire_message_content(item: &crate::protocol::ir::AiItem) -> Option<Vec<Value>> {
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
            .map(|block| match block {
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
            .collect::<Option<Vec<_>>>()?,
    };
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

pub(crate) fn function_output_value(content: &MessageContent) -> Value {
    super::encoder::encode_response_tool_output(content)
        .expect("function output passed the Open Responses representability gate")
}

pub(crate) fn gateway_item_id(prefix: &str, response_id: &str, output_index: usize) -> String {
    let response_id = response_id.strip_prefix("resp_").unwrap_or(response_id);
    format!("{prefix}_{response_id}_{output_index}")
}

pub(crate) fn stamp_output_graph_ids(resp: &AiResponse) -> Vec<AiItem> {
    let resp_id = if resp.id.is_empty() {
        format!("resp_{}", Uuid::new_v4().simple())
    } else {
        resp.id.clone()
    };
    let mut items = resp.items.clone();
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
    if item.role == Role::Assistant
        && item.tool_calls.is_none()
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

pub(crate) fn response_id_from_gateway_item_id(item_id: &str) -> Option<String> {
    let (prefix, remainder) = item_id.split_once('_')?;
    if !matches!(prefix, "msg" | "rs" | "fc" | "fco" | "item") {
        return None;
    }
    let (response_id, output_index) = remainder.rsplit_once('_')?;
    (!response_id.is_empty() && output_index.parse::<usize>().is_ok())
        .then(|| format!("resp_{response_id}"))
}

pub(crate) fn response_resource_snapshot(
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
