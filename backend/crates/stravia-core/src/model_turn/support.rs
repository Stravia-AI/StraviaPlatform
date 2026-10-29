use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META;

pub(crate) fn explicit_chat_reasoning_field(resp: &AiResponse) -> Option<&serde_json::Value> {
    resp.vendor
        .ingress
        .get(CHAT_REASONING_FIELD_META)
        .filter(|value| value.is_null() || value.as_str().is_some_and(str::is_empty))
}

pub(crate) fn restore_response_history(
    items: &mut Vec<stravia_runtime_contract::protocol::ir::AiItem>,
    output_start: usize,
    response_group: &str,
    field: Option<&serde_json::Value>,
) {
    restore_chat_reasoning_field(items, output_start, field);
    // 调用方刚追加了一个完整响应；只记录这个事实，不从条目邻接关系推断。
    stravia_runtime_contract::protocol::ir::response::mark_response_history(
        &mut items[output_start..],
        response_group,
    );
}

fn restore_chat_reasoning_field(
    items: &mut Vec<stravia_runtime_contract::protocol::ir::AiItem>,
    output_start: usize,
    field: Option<&serde_json::Value>,
) {
    use stravia_runtime_contract::protocol::ir::{AiItem, Role};

    let Some(value) = field.filter(|value| value.is_null() || value.as_str() == Some("")) else {
        return;
    };
    let output = &items[output_start..];
    if output.iter().any(|item| {
        item.thinking_ref()
            .is_some_and(|(text, _)| !text.is_empty())
            || item.reasoning_ref().is_some_and(|(summary, content, _)| {
                summary.iter().chain(content).any(|text| !text.is_empty())
            })
            || item
                .meta
                .as_ref()
                .and_then(|meta| meta.get("reasoning_content"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty())
    }) {
        return;
    }
    // 字段随响应的可回放项保留；最终消息形状由目标协议编码器投影。
    // 不制造 Thinking，也不能把字段附到随后会被丢弃的受保护推理项。
    let mut restored = false;
    for item in &mut items[output_start..] {
        if item.role != Role::Assistant
            || item.thinking_ref().is_some()
            || item.reasoning_ref().is_some()
        {
            continue;
        }
        let meta = item.meta.get_or_insert_with(Default::default);
        if meta.get("reasoning_content").is_none() {
            meta.insert_graph_extension("reasoning_content", value.clone())
                .expect("reasoning_content is ordinary item metadata");
        }
        restored = true;
    }
    if !restored {
        let mut item = AiItem::output_text("");
        item.meta
            .get_or_insert_with(Default::default)
            .insert_graph_extension("reasoning_content", value.clone())
            .expect("reasoning_content is ordinary item metadata");
        items.push(item);
    }
}

pub(crate) fn ai_response_to_deltas(
    resp: &AiResponse,
) -> Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta> {
    use stravia_runtime_contract::protocol::ir::AiStreamDelta;
    let mut deltas = Vec::new();
    deltas.push(AiStreamDelta::MessageStart {
        id: if resp.id.is_empty() {
            stravia_runtime_contract::identifier::new_id()
        } else {
            resp.id.clone()
        },
        model: resp.model.clone(),
    });
    // 明确的空字段是消息形状，不是 Thinking；转流时仍须保留其存在性。
    if let Some(value) = explicit_chat_reasoning_field(resp) {
        deltas.push(AiStreamDelta::ResponseMetadata {
            metadata: serde_json::Value::Object(serde_json::Map::from_iter([(
                CHAT_REASONING_FIELD_META.to_owned(),
                value.clone(),
            )])),
        });
    }
    for (output_index, item) in resp.items.iter().enumerate() {
        if let Some(text) = item.output_text_ref()
            && !text.is_empty()
        {
            deltas.push(AiStreamDelta::TextDeltaWithMetadata {
                text: text.to_owned(),
                logprobs: Vec::new(),
                obfuscation: None,
                output_index: Some(output_index),
                content_index: Some(0),
            });
        } else if let Some(refusal) = item.refusal_ref()
            && !refusal.is_empty()
        {
            deltas.push(AiStreamDelta::RefusalDeltaWithIndex {
                text: refusal.to_owned(),
                output_index,
                content_index: 0,
            });
        } else if let Some((summary, content, _)) = item.reasoning_ref() {
            for (content_index, text) in summary.iter().enumerate() {
                deltas.push(AiStreamDelta::ReasoningSummaryDelta {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                });
            }
            for (content_index, text) in content.iter().enumerate() {
                deltas.push(AiStreamDelta::ThinkingDeltaWithMetadata {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                });
            }
        } else if let Some((text, signature)) = item.thinking_ref()
            && !text.is_empty()
        {
            deltas.push(AiStreamDelta::ThinkingDelta(text.to_owned()));
            if let Some(signature) = signature.filter(|value| !value.is_empty()) {
                deltas.push(AiStreamDelta::ThinkingSignature(signature.to_owned()));
            }
        } else if let Some(call) = item.function_call_ref() {
            deltas.push(AiStreamDelta::ToolCallStart {
                index: output_index,
                id: call.id.to_string(),
                name: call.name.clone(),
            });
            if !call.arguments.is_empty() {
                deltas.push(AiStreamDelta::ToolCallDelta {
                    index: output_index,
                    arguments: call.arguments.clone(),
                });
            }
        } else if let Some(raw) = item.unknown_ref() {
            deltas.push(AiStreamDelta::Unknown {
                raw: raw.to_string(),
            });
        }
        deltas.push(AiStreamDelta::ItemDone {
            index: output_index,
            item: item.clone(),
        });
    }

    deltas.push(AiStreamDelta::Usage(resp.usage.clone()));
    deltas.push(AiStreamDelta::Done {
        stop_reason: resp
            .stop_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string()),
    });
    deltas
}
