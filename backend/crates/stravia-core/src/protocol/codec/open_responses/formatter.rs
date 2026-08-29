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
mod tests {
    use super::*;
    use crate::protocol::ir::Usage;

    #[test]
    fn formats_cache_write_tokens_when_known() {
        let mut response = AiResponse::new("resp_usage", "model");
        response.usage = Usage {
            prompt_tokens: 12,
            completion_tokens: 3,
            total_tokens: 15,
            required_components_known: true,
            cache_read_tokens: Some(4),
            cache_creation_tokens: Some(5),
            ..Usage::default()
        };

        let formatted = ResponsesResponseFormatter.format_response(&response);
        assert_eq!(
            formatted["usage"]["input_tokens_details"],
            serde_json::json!({"cached_tokens": 4, "cache_write_tokens": 5})
        );
    }

    #[test]
    fn preserves_platform_owned_response_items() {
        let mut response = AiResponse::new("response-1", "model-1");
        response.items = vec![
            crate::protocol::ir::AiItem::unknown(serde_json::json!({
                "type": "stravia:agent_result",
                "turn_id": "aturn_1",
            })),
            crate::protocol::ir::AiItem::output_text("done"),
        ];

        let formatted = ResponsesResponseFormatter.format_response(&response);

        assert_eq!(formatted["output"][0]["type"], "stravia:agent_result");
        assert_eq!(formatted["output"][0]["turn_id"], "aturn_1");
        assert_eq!(formatted["output"][1]["content"][0]["text"], "done");
    }

    #[test]
    fn response_resource_has_dated_required_keys_and_effective_defaults() {
        let response = AiResponse::new("resp_gateway", "logical-model");
        let formatted = ResponsesResponseFormatter.format_response(&response);

        for key in [
            "id",
            "object",
            "created_at",
            "completed_at",
            "status",
            "incomplete_details",
            "model",
            "previous_response_id",
            "instructions",
            "output",
            "error",
            "tools",
            "tool_choice",
            "truncation",
            "parallel_tool_calls",
            "text",
            "top_p",
            "presence_penalty",
            "frequency_penalty",
            "top_logprobs",
            "temperature",
            "reasoning",
            "usage",
            "max_output_tokens",
            "max_tool_calls",
            "store",
            "background",
            "service_tier",
            "metadata",
            "safety_identifier",
            "prompt_cache_key",
        ] {
            assert!(formatted.get(key).is_some(), "missing required key {key}");
        }
        assert!(formatted.get("output_text").is_none());
        assert_eq!(formatted["id"], "resp_gateway");
        assert_eq!(formatted["model"], "logical-model");
        assert_eq!(formatted["temperature"], 1.0);
        assert_eq!(formatted["top_p"], 1.0);
        assert_eq!(formatted["presence_penalty"], 0.0);
        assert_eq!(formatted["frequency_penalty"], 0.0);
        assert_eq!(formatted["top_logprobs"], 0);
        assert_eq!(formatted["parallel_tool_calls"], true);
        assert_eq!(formatted["truncation"], "disabled");
        assert_eq!(formatted["service_tier"], "default");
        assert_eq!(formatted["tool_choice"], "auto");
        assert_eq!(
            formatted["text"],
            serde_json::json!({"format": {"type": "text"}})
        );
        assert_eq!(formatted["tools"], serde_json::json!([]));
        assert_eq!(formatted["metadata"], serde_json::json!({}));
        assert_eq!(formatted["store"], true);
        assert_eq!(formatted["background"], false);
        assert!(formatted["usage"].is_null());
    }

    #[test]
    fn response_resource_echoes_effective_request_and_incomplete_status() {
        let mut response = AiResponse::new("resp_gateway", "logical-model");
        response.stop_reason = Some("length".into());
        response.vendor.ingress.insert(
            "__open_responses_effective_request".into(),
            serde_json::json!({
                "previous_response_id": "resp_parent",
                "instructions": "Be concise.",
                "temperature": 0.2,
                "store": false,
                "metadata": {"tenant": "acme"},
                "safety_identifier": "safe-user"
            }),
        );

        let formatted = ResponsesResponseFormatter.format_response(&response);

        assert_eq!(formatted["status"], "incomplete");
        assert_eq!(formatted["previous_response_id"], "resp_parent");
        assert_eq!(formatted["instructions"], "Be concise.");
        assert_eq!(formatted["temperature"], 0.2);
        assert_eq!(formatted["store"], false);
        assert_eq!(formatted["metadata"]["tenant"], "acme");
        assert_eq!(formatted["safety_identifier"], "safe-user");
    }

    #[test]
    fn response_resource_preserves_failed_terminal_state_without_raw_provider_error() {
        let mut response = AiResponse::new("resp_gateway", "logical-model");
        response.vendor.egress.insert(
            "__open_responses_terminal".into(),
            serde_json::json!({
                "status": "failed",
                "error": {"message": "secret provider payload"}
            }),
        );

        let formatted = ResponsesResponseFormatter.format_response(&response);

        assert_eq!(formatted["status"], "failed");
        assert_eq!(formatted["error"]["code"], "upstream_response_failed");
        assert_eq!(
            formatted["error"]["message"],
            "The selected Provider failed the response."
        );
        assert!(!formatted.to_string().contains("secret provider payload"));
    }
    #[test]
    fn clears_text_metadata_when_canonical_text_changes() {
        let mut response = AiResponse::new("resp_gateway", "logical-model");
        response.items = vec![crate::protocol::ir::AiItem {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::Text {
                text: "after".into(),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: Some(serde_json::json!({
                "id": "msg_provider",
                "status": "completed",
                "phase": "final_answer",
                "__open_responses_content": [{
                    "type": "output_text",
                    "text": "before",
                    "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                    "logprobs": [{"token": "before", "logprob": -0.1}]
                }]
            })),
        }];

        let formatted = ResponsesResponseFormatter.format_response(&response);

        assert_eq!(formatted["output"][0]["id"], "msg_provider");
        assert_eq!(formatted["output"][0]["phase"], "final_answer");
        assert_eq!(formatted["output"][0]["content"][0]["text"], "after");
        assert_eq!(
            formatted["output"][0]["content"][0]["annotations"],
            serde_json::json!([])
        );
        assert_eq!(
            formatted["output"][0]["content"][0]["logprobs"],
            serde_json::json!([])
        );
    }

    #[test]
    fn preserves_canonical_item_order_without_collapsing_messages() {
        let mut response = AiResponse::new("resp_gateway", "logical-model");
        response.items = vec![
            crate::protocol::ir::AiItem::output_text("answer"),
            crate::protocol::ir::AiItem::function_call(crate::protocol::ir::ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            }),
            crate::protocol::ir::AiItem::thinking("reasoning", Some("opaque".into())),
        ];

        let formatted = ResponsesResponseFormatter.format_response(&response);
        let types = formatted["output"]
            .as_array()
            .expect("output")
            .iter()
            .map(|item| item["type"].as_str().expect("type"))
            .collect::<Vec<_>>();

        assert_eq!(types, ["message", "function_call", "reasoning"]);
        assert_eq!(formatted["output"][2]["encrypted_content"], "opaque");
    }
    #[test]
    fn encodes_function_output_arrays_with_dated_content_shapes() {
        let mut response = AiResponse::new("resp_gateway", "logical-model");
        response.items = vec![crate::protocol::ir::AiItem {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "tool text".into(),
                    cache_control: None,
                },
                ContentBlock::Image {
                    source: crate::protocol::ir::MediaSource::Url(
                        "https://example.test/image.png".into(),
                    ),
                    detail: Some("high".into()),
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        }];

        let formatted = ResponsesResponseFormatter.format_response(&response);

        assert_eq!(
            formatted["output"][0]["output"][0],
            serde_json::json!({"type": "input_text", "text": "tool text"})
        );
        assert_eq!(
            formatted["output"][0]["output"][1],
            serde_json::json!({
                "type": "input_image",
                "image_url": "https://example.test/image.png",
                "detail": "high"
            })
        );
    }

    #[test]
    fn recovers_response_ids_from_function_call_output_item_ids() {
        assert_eq!(
            response_id_from_gateway_item_id("fco_abc_3"),
            Some("resp_abc".into())
        );
    }

    #[test]
    fn preserves_nonterminal_dated_response_states() {
        for status in ["queued", "in_progress"] {
            let mut response = AiResponse::new("resp_gateway", "logical-model");
            response.vendor.egress.insert(
                "__open_responses_terminal".into(),
                serde_json::json!({
                    "status": status,
                    "incomplete_details": null
                }),
            );

            let formatted = ResponsesResponseFormatter.format_response(&response);
            assert_eq!(formatted["status"], status);
            assert_eq!(formatted["completed_at"], Value::Null);
        }
    }
}
