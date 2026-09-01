use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;

use crate::protocol::ir::request::{
    AiItem, ContentBlock, MediaSource, MessageContent, Role, ToolChoice, ToolSpec,
};
use crate::protocol::ir::{AiRequest, ToolCall};

pub struct OpenAIEncoder;

impl OpenAIEncoder {
    pub(crate) fn encode_request(&self, req: &AiRequest) -> Result<(Value, HeaderMap)> {
        let tools = req.tools.as_deref().unwrap_or(&[]);
        let tools_opt: Option<&[ToolSpec]> = if tools.is_empty() { None } else { Some(tools) };

        let normalized_messages =
            normalize_messages_for_openai(&req.items, req.instructions.as_deref(), tools_opt);
        let messages: Vec<Value> = normalized_messages
            .iter()
            .map(encode_message)
            .collect::<Result<Vec<_>>>()?;

        let ingress = &req.meta.vendor.ingress;
        let responses_ingress = req.meta.source_protocol == Some(OPEN_RESPONSES_2026_04_24);

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "stream": req.stream.enabled,
        });

        let obj = body.as_object_mut().unwrap();

        if let Some(t) = req.generation.temperature {
            obj.insert("temperature".into(), t.into());
        }
        if let Some(m) = req.generation.max_tokens {
            obj.insert("max_tokens".into(), m.into());
        }
        if let Some(p) = req.generation.top_p {
            obj.insert("top_p".into(), p.into());
        }
        match req.reasoning.target_control.as_ref() {
            Some(crate::thinking::TargetThinkingControl::Effort { value }) => {
                obj.insert("reasoning_effort".into(), Value::String(value.clone()));
            }
            None => {}
            Some(control) => {
                anyhow::bail!(
                    "OpenAI Chat Completions cannot represent Target Thinking Control {control:?}"
                );
            }
        }

        if !tools.is_empty() {
            let tools_val: Vec<Value> = tools
                .iter()
                .map(|t| {
                    let mut f = serde_json::json!({
                        "name": t.name,
                        "parameters": t.parameters,
                    });
                    if let Some(ref desc) = t.description {
                        f.as_object_mut()
                            .unwrap()
                            .insert("description".into(), desc.clone().into());
                    }
                    serde_json::json!({
                        "type": "function",
                        "function": f,
                    })
                })
                .collect();
            obj.insert("tools".into(), Value::Array(tools_val));
        }
        if let Some(ref tc) = req.tool_choice {
            obj.insert("tool_choice".into(), tool_choice_to_value(tc));
        }

        // Always include_usage when streaming.
        if req.stream.enabled {
            let stream_opts = ingress
                .get("stream_options")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"include_usage": true}));
            obj.insert("stream_options".into(), stream_opts);
        }

        for key in &[
            "parallel_tool_calls",
            "prediction",
            "modalities",
            "audio",
            "response_format",
            "seed",
            "stop",
            "logit_bias",
            "service_tier",
            "frequency_penalty",
            "presence_penalty",
            "n",
            "user",
        ] {
            if let Some(v) = ingress.get(*key) {
                obj.entry(key.to_string()).or_insert_with(|| v.clone());
            }
        }

        // Passthrough any remaining unknown extra fields.
        // Skip cross-protocol internal keys (e.g. __anthropic_*, __google_*)
        // that are only meaningful to their respective codecs.
        for (k, v) in ingress {
            if k == "reasoning"
                || k == "reasoning_effort"
                || k.starts_with("__anthropic_")
                || k.starts_with("__google_")
                || (responses_ingress
                    && matches!(
                        k.as_str(),
                        "store" | "include" | "prompt_cache_key" | "client_metadata"
                    ))
            {
                continue;
            }
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }

        Ok((body, HeaderMap::new()))
    }

    pub(crate) fn egress_path(&self, _model: &str, _stream: bool) -> String {
        "/v1/chat/completions".to_string()
    }
}

fn tool_choice_to_value(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Named { name } => serde_json::json!({
            "type": "function",
            "function": {"name": name}
        }),
        ToolChoice::Raw(v) => v.clone(),
    }
}

fn normalize_messages_for_openai(
    messages: &[AiItem],
    system: Option<&str>,
    tools: Option<&[ToolSpec]>,
) -> Vec<AiItem> {
    let preprocessed = remap_duplicate_tool_call_ids(messages, system);

    let mut out: Vec<AiItem> = Vec::with_capacity(preprocessed.len() + 2);
    let mut seen_tool_call_ids: HashSet<String> = HashSet::new();
    let mut consumed_tool_result_ids: HashSet<String> = HashSet::new();
    let mut generated_seq: usize = 0;
    let fallback_tool_name = tools
        .and_then(|defs| defs.first())
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "tool".to_string());

    for msg in &preprocessed {
        let mut msg = msg.clone();

        if msg.role == Role::Assistant {
            promote_reasoning_meta(&mut msg);
            if let Some(tool_calls) = &mut msg.tool_calls {
                for tc in tool_calls.iter_mut() {
                    if tc.id.trim().is_empty() {
                        generated_seq += 1;
                        tc.id = format!("call_enc_{generated_seq}");
                    }
                    if tc.name.trim().is_empty() {
                        tc.name = fallback_tool_name.clone();
                    }
                    seen_tool_call_ids.insert(tc.id.clone());
                }
            }
            out.push(msg);
            continue;
        }

        if msg.role != Role::Tool {
            out.push(msg);
            continue;
        }

        let hinted_id = tool_message_payload(&msg).1;
        let mut resolved_id = msg
            .tool_call_id
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| hinted_id.clone().filter(|v| !v.trim().is_empty()));

        if resolved_id.is_none() {
            generated_seq += 1;
            resolved_id = Some(format!("call_enc_{generated_seq}"));
        }
        let mut final_id = resolved_id.expect("tool_call_id should always exist");
        if consumed_tool_result_ids.contains(&final_id) {
            generated_seq += 1;
            final_id = format!("call_enc_{generated_seq}");
        }

        let has_adjacent_matching_call = out
            .last()
            .is_some_and(|prev| assistant_has_tool_call_id(prev, &final_id));
        if !has_adjacent_matching_call {
            let extracted_call = take_matching_tool_call_from_history(&mut out, &final_id);
            if let Some((tc, source_idx)) = extracted_call {
                trim_trailing_assistant_text_after_index(&mut out, source_idx);
                let source_meta = out[source_idx].meta.clone();
                out.push(AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Text(String::new()),
                    tool_calls: Some(vec![tc]),
                    tool_call_id: None,
                    meta: source_meta,
                });
                seen_tool_call_ids.insert(final_id.clone());
            } else if !make_matching_call_adjacent(&mut out, &final_id) {
                if seen_tool_call_ids.contains(&final_id) {
                    generated_seq += 1;
                    final_id = format!("call_enc_{generated_seq}");
                }
                let synth_name = hinted_id
                    .as_deref()
                    .filter(|v| !v.trim().is_empty())
                    .map(|_| fallback_tool_name.clone())
                    .unwrap_or_else(|| fallback_tool_name.clone());
                out.push(AiItem {
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
                seen_tool_call_ids.insert(final_id.clone());
            }
        }

        msg.tool_call_id = Some(final_id.clone());
        consumed_tool_result_ids.insert(final_id);
        out.push(msg);
    }

    out = prune_orphan_assistant_tool_calls(out);

    out.retain(|msg| {
        if msg.role != Role::Assistant {
            return true;
        }
        let has_calls = msg.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
        if has_calls {
            return true;
        }
        let has_reasoning = matches!(
            &msg.content,
            MessageContent::Blocks(blocks)
                if blocks.iter().any(|block| matches!(
                    block,
                    ContentBlock::Thinking { .. }
                        | ContentBlock::Reasoning { .. }
                        | ContentBlock::RedactedThinking { .. }
                ))
        );
        has_reasoning || !msg.content.to_text().trim().is_empty()
    });

    out
}

fn prune_orphan_assistant_tool_calls(messages: Vec<AiItem>) -> Vec<AiItem> {
    let referenced_tool_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .filter(|id| !id.trim().is_empty())
        .collect();

    let mut out: Vec<AiItem> = Vec::with_capacity(messages.len());
    for mut msg in messages {
        if msg.role == Role::Assistant
            && let Some(calls) = msg.tool_calls.take()
        {
            let kept: Vec<ToolCall> = calls
                .into_iter()
                .filter(|tc| referenced_tool_ids.contains(&tc.id))
                .collect();
            if !kept.is_empty() {
                msg.tool_calls = Some(kept);
            }
        }
        out.push(msg);
    }
    out
}

fn assistant_has_tool_call_id(msg: &AiItem, tool_call_id: &str) -> bool {
    if msg.role != Role::Assistant {
        return false;
    }
    msg.tool_calls.as_ref().is_some_and(|calls| {
        calls
            .iter()
            .any(|tc| !tc.id.trim().is_empty() && tc.id == tool_call_id)
    })
}

fn remap_duplicate_tool_call_ids(messages: &[AiItem], system: Option<&str>) -> Vec<AiItem> {
    let mut out = Vec::with_capacity(messages.len() + usize::from(system.is_some()));
    if let Some(system) = system {
        out.push(AiItem {
            role: Role::System,
            content: MessageContent::Text(system.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        });
    }
    out.extend_from_slice(messages);
    let mut seen_counts: HashMap<String, usize> = HashMap::new();
    let mut pending_by_original: HashMap<String, Vec<String>> = HashMap::new();
    let mut generated_seq: usize = 0;

    for msg in &mut out {
        if msg.role == Role::Assistant {
            if let Some(tool_calls) = &mut msg.tool_calls {
                for tc in tool_calls.iter_mut() {
                    let original = if tc.id.trim().is_empty() {
                        generated_seq += 1;
                        format!("call_enc_{generated_seq}")
                    } else {
                        tc.id.clone()
                    };

                    let count = seen_counts.entry(original.clone()).or_insert(0);
                    *count += 1;
                    let unique = if *count == 1 {
                        original.clone()
                    } else {
                        format!("{}_dup{}", original, *count)
                    };
                    tc.id = unique.clone();
                    pending_by_original
                        .entry(original)
                        .or_default()
                        .push(unique);
                }
            }
            continue;
        }

        if msg.role != Role::Tool {
            continue;
        }

        let Some(original_id) = msg
            .tool_call_id
            .as_ref()
            .filter(|v| !v.trim().is_empty())
            .cloned()
        else {
            continue;
        };

        if let Some(stack) = pending_by_original.get_mut(&original_id)
            && let Some(unique_id) = stack.pop()
        {
            msg.tool_call_id = Some(unique_id);
        }
    }

    out
}

fn make_matching_call_adjacent(out: &mut Vec<AiItem>, tool_call_id: &str) -> bool {
    if out.is_empty() {
        return false;
    }

    loop {
        let Some(last) = out.last() else {
            return false;
        };
        if assistant_has_tool_call_id(last, tool_call_id) {
            return true;
        }

        let drop_candidate = last.role == Role::Assistant
            && last
                .tool_calls
                .as_ref()
                .is_none_or(|calls| calls.is_empty())
            && last
                .tool_call_id
                .as_ref()
                .is_none_or(|id| id.trim().is_empty());
        if drop_candidate {
            let _ = out.pop();
            continue;
        }
        return false;
    }
}

fn take_matching_tool_call_from_history(
    out: &mut [AiItem],
    tool_call_id: &str,
) -> Option<(ToolCall, usize)> {
    for (idx, msg) in out.iter_mut().enumerate().rev() {
        if msg.role != Role::Assistant {
            continue;
        }
        let Some(calls) = msg.tool_calls.as_mut() else {
            continue;
        };
        if let Some(pos) = calls.iter().position(|tc| tc.id == tool_call_id) {
            let tc = calls.remove(pos);
            if calls.is_empty() {
                msg.tool_calls = None;
            }
            return Some((tc, idx));
        }
    }
    None
}

fn trim_trailing_assistant_text_after_index(out: &mut Vec<AiItem>, source_idx: usize) {
    while out.len() > source_idx + 1 {
        let Some(last) = out.last() else {
            break;
        };
        let drop_candidate = last.role == Role::Assistant
            && last
                .tool_calls
                .as_ref()
                .is_none_or(|calls| calls.is_empty())
            && last
                .tool_call_id
                .as_ref()
                .is_none_or(|id| id.trim().is_empty());
        if drop_candidate {
            let _ = out.pop();
            continue;
        }
        break;
    }
}

fn promote_reasoning_meta(message: &mut AiItem) {
    let MessageContent::Blocks(blocks) = &message.content else {
        return;
    };
    let mut reasoning = String::new();
    for block in blocks {
        if let ContentBlock::Thinking { thinking, .. } = block
            && !thinking.is_empty()
        {
            if !reasoning.is_empty() {
                reasoning.push('\n');
            }
            reasoning.push_str(thinking);
        }
    }
    if reasoning.is_empty() {
        return;
    }
    let meta = message
        .meta
        .get_or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Value::Object(meta) = meta {
        meta.insert("reasoning_content".into(), Value::String(reasoning));
    }
}

fn encode_message(msg: &AiItem) -> Result<Value> {
    let role = match msg.role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    let mut obj = serde_json::json!({ "role": role });
    let map = obj.as_object_mut().unwrap();

    if msg.role == Role::Tool {
        let (tool_content, hinted_tool_call_id) = tool_message_payload(msg);
        map.insert("content".into(), Value::String(tool_content));
        let resolved_tool_call_id = msg
            .tool_call_id
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| hinted_tool_call_id.filter(|v| !v.trim().is_empty()));
        if let Some(tool_call_id) = resolved_tool_call_id {
            map.insert("tool_call_id".into(), Value::String(tool_call_id));
        }
        return Ok(obj);
    }

    match &msg.content {
        MessageContent::Text(t) => {
            map.insert("content".into(), Value::String(t.clone()));
        }
        MessageContent::Blocks(blocks) => {
            // For assistant messages, strip blocks that are already expressed
            // elsewhere in the OpenAI shape:
            //   - Thinking / RedactedThinking: surfaced via the top-level
            //     `reasoning_content` field carried in `meta` (see the Anthropic
            //     messages decoder). Emitting them as plain text would duplicate
            //     reasoning and break strict thinking-mode upstreams.
            //   - ToolUse: already expressed via the `tool_calls` array below.
            //     Encoding it into `content` would produce `{type:"function"}`,
            //     which OpenAI chat/completions rejects with
            //     "unknown variant `function`, expected `text`". This is the
            //     root cause of tool calls failing in Anthropic Messages →
            //     OpenAI cross-protocol conversion (the Anthropic decoder
            //     carries `tool_use` in BOTH `content` and `tool_calls`).
            let strip_for_assistant = msg.role == Role::Assistant;
            let parts: Vec<Value> = blocks
                .iter()
                .filter(|b| {
                    !(strip_for_assistant
                        && matches!(
                            b,
                            ContentBlock::Thinking { .. }
                                | ContentBlock::RedactedThinking { .. }
                                | ContentBlock::ToolUse { .. }
                        ))
                })
                .map(encode_content_block_for_openai)
                .collect();
            if !parts.is_empty() {
                map.insert("content".into(), Value::Array(parts));
            }
            // An assistant turn that carries only tool calls / thinking has no
            // textual content — leave `content` unset (OpenAI accepts its
            // absence when `tool_calls` is present) rather than emitting `[]`,
            // which some strict upstreams reject.
        }
    }

    if let Some(ref tcs) = msg.tool_calls {
        let arr: Vec<Value> = tcs
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }
                })
            })
            .collect();
        map.insert("tool_calls".into(), Value::Array(arr));
    }
    if let Some(ref tid) = msg.tool_call_id {
        map.insert("tool_call_id".into(), Value::String(tid.clone()));
    }

    // Internal canonical metadata participates in local semantics but must never
    // become an upstream vendor field.
    if let Some(Value::Object(extra)) = &msg.meta {
        for (key, value) in extra
            .iter()
            .filter(|(key, _)| !key.starts_with("__stravia_"))
        {
            map.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    Ok(obj)
}

fn encode_content_block_for_openai(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text, .. } => {
            serde_json::json!({"type": "text", "text": text})
        }
        ContentBlock::Image { source, detail, .. } => {
            let url = media_source_to_url(source);
            let mut image_url = serde_json::json!({"url": url});
            if let Some(detail) = detail {
                image_url["detail"] = Value::String(detail.clone());
            }
            serde_json::json!({
                "type": "image_url",
                "image_url": image_url
            })
        }
        ContentBlock::Audio { source } => {
            let url = media_source_to_url(source);
            serde_json::json!({"type": "input_audio", "input_audio": {"data": url}})
        }
        ContentBlock::File { source, .. } => {
            let url = media_source_to_url(source);
            serde_json::json!({"type": "file", "file": {"url": url}})
        }
        ContentBlock::ToolUse {
            id, name, input, ..
        } => {
            serde_json::json!({
                "type": "function",
                "id": id,
                "function": {"name": name, "arguments": input.to_string()}
            })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            serde_json::json!({
                "type": "text",
                "text": match content {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                },
                "tool_call_id": tool_use_id,
            })
        }
        ContentBlock::Thinking { thinking, .. } => {
            // OpenAI does not support thinking blocks; pass as plain text
            serde_json::json!({"type": "text", "text": thinking})
        }
        ContentBlock::RedactedThinking { .. } => {
            serde_json::json!({"type": "text", "text": ""})
        }
        ContentBlock::Unknown { raw } => raw.clone(),
        other => {
            // Other block types (Document, SearchResult, etc.) not supported
            // by OpenAI chat/completions; serialise raw as fallback.
            serde_json::to_value(other).unwrap_or(Value::Null)
        }
    }
}

fn media_source_to_url(source: &MediaSource) -> String {
    match source {
        MediaSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
        MediaSource::Url(url) => url.clone(),
        MediaSource::FileId { file_id, .. } => file_id.clone(),
    }
}

fn tool_message_payload(msg: &AiItem) -> (String, Option<String>) {
    match &msg.content {
        MessageContent::Text(t) => (t.clone(), None),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                {
                    let text = match content {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    let hinted_id = if tool_use_id.trim().is_empty() {
                        None
                    } else {
                        Some(tool_use_id.clone())
                    };
                    return (text, hinted_id);
                }
            }
            (msg.content.to_text(), None)
        }
    }
}

#[cfg(test)]
mod tests;
