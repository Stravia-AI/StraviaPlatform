use std::collections::VecDeque;

use crate::protocol::ir::request::{
    AiItem, AiRequest, ContentBlock, MessageContent, Role, ToolCall,
};

pub fn normalize_request_tool_results(req: &mut AiRequest) {
    let inherits_tool_calls = matches!(
        req.ext.as_ref(),
        Some(crate::protocol::ir::ProtocolExt::OpenResponses(extension))
            if extension.previous_response_id.is_some()
    ) || req
        .meta
        .vendor
        .ingress
        .get(crate::protocol::ir::request::VERIFIED_HISTORY_REPLAY_META)
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let mut pending_calls: VecDeque<(String, String)> = VecDeque::new();
    let mut generated_id_seq: usize = 0;
    let mut normalized_messages: Vec<AiItem> = Vec::with_capacity(req.items.len());

    for mut msg in req.items.drain(..) {
        if msg.role == Role::Assistant {
            if let Some(tool_calls) = &mut msg.tool_calls {
                for tc in tool_calls.iter_mut() {
                    if tc.id.trim().is_empty() {
                        generated_id_seq += 1;
                        tc.id = format!("call_stravia_{generated_id_seq}");
                    }
                    pending_calls.push_back((tc.id.clone(), tc.name.clone()));
                }
            }
            normalized_messages.push(msg);
            continue;
        }

        if msg.role != Role::Tool {
            normalized_messages.push(msg);
            continue;
        }

        let existing_id = msg
            .tool_call_id
            .as_ref()
            .filter(|v| !v.trim().is_empty())
            .cloned();
        let has_existing_id = existing_id.is_some();

        let mut resolved_id: Option<String> = None;
        let mut has_linked_pending_call = false;

        if let Some(id) = existing_id.as_ref()
            && let Some(pos) = pending_calls
                .iter()
                .position(|(pending_id, _)| pending_id == id)
        {
            let _ = pending_calls.remove(pos);
            resolved_id = Some(id.clone());
            has_linked_pending_call = true;
        }

        let hinted_value = extract_tool_result_hint(&msg.content);

        if resolved_id.is_none()
            && let Some(hint) = hinted_value.clone()
            && let Some(pos) = pending_calls
                .iter()
                .position(|(pending_id, _)| pending_id == &hint)
            && let Some((call_id, _)) = pending_calls.remove(pos)
        {
            resolved_id = Some(call_id);
            has_linked_pending_call = true;
        }

        if resolved_id.is_none()
            && let Some(hint) = hinted_value.clone()
            && let Some(pos) = pending_calls
                .iter()
                .position(|(_, pending_name)| pending_name.eq_ignore_ascii_case(&hint))
            && let Some((call_id, _)) = pending_calls.remove(pos)
        {
            resolved_id = Some(call_id);
            has_linked_pending_call = true;
        }

        if resolved_id.is_none()
            && let Some((call_id, _name)) = pending_calls.pop_front()
        {
            resolved_id = Some(call_id);
            has_linked_pending_call = true;
        }

        if resolved_id.is_none() {
            resolved_id = existing_id;
        }

        if resolved_id.is_none() {
            generated_id_seq += 1;
            resolved_id = Some(format!("call_stravia_synth_{generated_id_seq}"));
        }

        let final_id = resolved_id.expect("final tool_call_id should always exist");
        if !has_linked_pending_call && !(inherits_tool_calls && has_existing_id) {
            let synth_name = hinted_value.unwrap_or_else(|| "unknown_tool".to_string());
            normalized_messages.push(AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                tool_calls: Some(vec![ToolCall {
                    id: final_id.clone(),
                    name: synth_name,
                    arguments: "{}".to_string(),
                }]),
                tool_call_id: None,
                meta: None,
            });
        }

        msg.tool_call_id = Some(final_id);
        normalized_messages.push(msg);
    }

    req.items = normalized_messages;
}

fn extract_tool_result_hint(content: &MessageContent) -> Option<String> {
    let MessageContent::Blocks(blocks) = content else {
        return None;
    };
    for block in blocks {
        if let ContentBlock::ToolResult { tool_use_id, .. } = block
            && !tool_use_id.trim().is_empty()
        {
            return Some(tool_use_id.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests;
