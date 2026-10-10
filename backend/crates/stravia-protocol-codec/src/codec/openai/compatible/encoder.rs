use anyhow::Result;
use http::header::HeaderMap;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;

use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::request::AiItem;
use stravia_runtime_contract::protocol::ir::request::ContentBlock;
use stravia_runtime_contract::protocol::ir::request::MediaSource;
use stravia_runtime_contract::protocol::ir::request::MessageContent;
use stravia_runtime_contract::protocol::ir::request::Role;
use stravia_runtime_contract::protocol::ir::request::ToolChoice;
use stravia_runtime_contract::protocol::ir::request::ToolSpec;

pub struct OpenAIEncoder;

impl OpenAIEncoder {
    pub(crate) fn encode_request(&self, req: AiRequest) -> Result<(Value, HeaderMap)> {
        let tools = req.tools.as_deref().unwrap_or(&[]);
        let tools_opt: Option<&[ToolSpec]> = if tools.is_empty() { None } else { Some(tools) };

        let normalized_messages =
            normalize_messages_for_openai(req.items, req.instructions, tools_opt);
        let messages: Vec<Value> = normalized_messages
            .into_iter()
            .map(encode_message)
            .collect::<Result<Vec<_>>>()?;

        let mut ingress = req.meta.vendor.ingress;
        let responses_ingress = req.meta.source_protocol == Some(OPEN_RESPONSES_2026_04_24);

        let mut body = object([
            ("model", Value::String(req.model)),
            ("messages", Value::Array(messages)),
            ("stream", Value::Bool(req.stream.enabled)),
        ]);

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
        match req.reasoning.target_control {
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { value }) => {
                obj.insert("reasoning_effort".into(), Value::String(value));
            }
            None => {}
            Some(control) => {
                anyhow::bail!(
                    "OpenAI Chat Completions cannot represent Target Thinking Control {control:?}"
                );
            }
        }

        if let Some(tools) = req.tools.filter(|tools| !tools.is_empty()) {
            let tools_val: Vec<Value> = tools
                .into_iter()
                .map(|t| {
                    let mut f = object([
                        ("name", Value::String(t.name)),
                        ("parameters", t.parameters),
                    ]);
                    if let Some(desc) = t.description {
                        f.as_object_mut()
                            .unwrap()
                            .insert("description".into(), desc.into());
                    }
                    if let Some(strict) = t.strict {
                        f["strict"] = Value::Bool(strict);
                    }
                    object([("type", Value::String("function".into())), ("function", f)])
                })
                .collect();
            obj.insert("tools".into(), Value::Array(tools_val));
        }
        if let Some(tc) = req.tool_choice {
            obj.insert("tool_choice".into(), tool_choice_to_value(tc));
        }

        // Always include_usage when streaming.
        if req.stream.enabled {
            let stream_opts = ingress
                .remove("stream_options")
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
            if let Some(v) = ingress.remove(*key) {
                obj.entry(key.to_string()).or_insert(v);
            }
        }

        // Passthrough any remaining unknown extra fields.
        // Skip cross-protocol internal keys (e.g. __anthropic_*, __google_*)
        // that are only meaningful to their respective codecs, and gateway-owned
        // __stravia_* state that must never reach an upstream.
        for (k, v) in ingress {
            if k == "reasoning"
                || k == "reasoning_effort"
                || k.starts_with("__stravia_")
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
            obj.entry(k).or_insert(v);
        }

        Ok((body, HeaderMap::new()))
    }

    pub(crate) fn egress_path(&self, _model: &str, _stream: bool) -> String {
        "/v1/chat/completions".to_string()
    }
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn tool_choice_to_value(tc: ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Named { name } => object([
            ("type", Value::String("function".into())),
            ("function", object([("name", Value::String(name))])),
        ]),
        ToolChoice::Raw(v) => v,
    }
}

fn normalize_messages_for_openai(
    messages: Vec<AiItem>,
    system: Option<String>,
    tools: Option<&[ToolSpec]>,
) -> Vec<AiItem> {
    let supplied_tool_ids = collect_supplied_tool_ids(&messages);
    let preprocessed = remap_duplicate_tool_call_ids(messages, system, &supplied_tool_ids);
    let mut unavailable_tool_ids = collect_supplied_tool_ids(&preprocessed);

    let mut out: Vec<AiItem> = Vec::with_capacity(preprocessed.len() + 2);
    let mut seen_tool_call_ids: HashSet<stravia_runtime_contract::protocol::ir::ToolCallId> =
        HashSet::new();
    let mut consumed_tool_result_ids: HashSet<stravia_runtime_contract::protocol::ir::ToolCallId> =
        HashSet::new();
    let mut pending_tool_call_ids: VecDeque<stravia_runtime_contract::protocol::ir::ToolCallId> =
        VecDeque::new();
    let mut generated_seq: usize = 0;
    let fallback_tool_name = tools
        .and_then(|defs| defs.first())
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "tool".to_string());

    for mut msg in preprocessed {
        if msg.role == Role::Assistant {
            if !is_standalone_reasoning_item(&msg) {
                promote_reasoning_meta(&mut msg);
            }
            if let Some(tool_calls) = &mut msg.tool_calls {
                for tc in tool_calls.iter_mut() {
                    if tc.id.trim().is_empty() {
                        tc.id = next_synthetic_tool_call_id(
                            &mut generated_seq,
                            &mut unavailable_tool_ids,
                        );
                    }
                    if tc.name.trim().is_empty() {
                        tc.name = fallback_tool_name.clone();
                    }
                    seen_tool_call_ids.insert(tc.id.clone());
                    pending_tool_call_ids.push_back(tc.id.clone());
                }
            }
            out.push(msg);
            continue;
        }

        if msg.role != Role::Tool {
            out.push(msg);
            continue;
        }

        let hinted_id = tool_message_hint(&msg);
        let mut resolved_id = msg
            .tool_call_id
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| hinted_id.clone().filter(|v| !v.trim().is_empty()));

        if let Some(id) = resolved_id.as_ref()
            && !consumed_tool_result_ids.contains(id)
            && let Some(pos) = pending_tool_call_ids
                .iter()
                .position(|pending_id| pending_id == id)
        {
            pending_tool_call_ids.remove(pos);
        } else if resolved_id.is_none() {
            resolved_id = pending_tool_call_ids.pop_front();
        }
        if resolved_id.is_none() {
            resolved_id = Some(next_synthetic_tool_call_id(
                &mut generated_seq,
                &mut unavailable_tool_ids,
            ));
        }
        let mut final_id = resolved_id.expect("tool_call_id should always exist");
        if consumed_tool_result_ids.contains(&final_id) {
            final_id = next_synthetic_tool_call_id(&mut generated_seq, &mut unavailable_tool_ids);
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
                    content: MessageContent::Text(String::new().into()),
                    tool_calls: Some(vec![tc]),
                    tool_call_id: None,
                    meta: source_meta,
                });
                seen_tool_call_ids.insert(final_id.clone());
            } else if !make_matching_call_adjacent(&mut out, &final_id) {
                if seen_tool_call_ids.contains(&final_id) {
                    final_id =
                        next_synthetic_tool_call_id(&mut generated_seq, &mut unavailable_tool_ids);
                }
                let synth_name = hinted_id
                    .as_deref()
                    .filter(|v| !v.trim().is_empty())
                    .map(|_| fallback_tool_name.clone())
                    .unwrap_or_else(|| fallback_tool_name.clone());
                out.push(AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Text(String::new().into()),
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
        if is_explicit_chat_reasoning_carrier(msg) {
            return true;
        }
        // 只有明文 reasoning 才有可回放的载体：受保护载荷（signature /
        // encrypted_content / redacted）在 chat 协议上无处可去，条目若只剩
        // 这些载荷就必须整条丢弃，否则会编码出既无 content 也无 tool_calls 的
        // 空 assistant 消息，被严格上游 400。
        let has_reasoning = matches!(
            &msg.content,
            MessageContent::Blocks(blocks)
                if blocks.iter().any(|block| match block {
                    ContentBlock::Thinking { thinking, .. } => !thinking.is_empty(),
                    ContentBlock::Reasoning { summary, content, .. } => {
                        summary.iter().chain(content).any(|text| !text.is_empty())
                    }
                    _ => false,
                })
        );
        has_reasoning || content_has_non_whitespace_text(&msg.content)
    });

    fold_standalone_reasoning_items(&mut out);

    out
}

/// DeepSeek and other strict chat-completions upstreams reject assistant
/// messages that have neither `content` nor `tool_calls`:
/// 400 "Invalid assistant message: content or tool_calls must be set".
/// Open Responses history gives native reasoning its own item ahead of the
/// sibling function-call items, so after the tool-call split such an item
/// sits right before the first assistant turn it belongs to. Fold its text
/// into that turn's `reasoning_content` (chronologically first) and drop the
/// carrier. Without a following assistant turn the text has no
/// `reasoning_content` host (strict upstreams reject content-less assistant
/// messages), so it falls back to the carrier's own `content`, like every
/// codec does when no native carrier exists. Carriers with no plaintext at
/// all (only signatures / ciphertext / redacted data) are dropped entirely.
fn fold_standalone_reasoning_items(out: &mut Vec<AiItem>) {
    for idx in (0..out.len()).rev() {
        if !is_standalone_reasoning_item(&out[idx]) {
            continue;
        }
        let mut carrier = out.remove(idx);
        let text = standalone_reasoning_text(&mut carrier);
        if out.get(idx).is_some_and(|next| {
            next.role == Role::Assistant
                && next
                    .meta
                    .as_ref()
                    .is_none_or(|meta| meta.object_extensions().is_some())
        }) {
            let next_meta = out[idx].meta.get_or_insert_with(Default::default);
            let existing = if next_meta
                .get("reasoning_content")
                .is_some_and(Value::is_string)
            {
                match next_meta
                    .remove_extension("reasoning_content")
                    .expect("reasoning content is not reserved")
                {
                    Some(Value::String(text)) => text,
                    _ => unreachable!("string extension checked above"),
                }
            } else {
                String::new()
            };
            let merged = match (text.is_empty(), existing.is_empty()) {
                (true, _) => existing,
                (false, true) => text,
                (false, false) => {
                    let mut text = text;
                    text.push('\n');
                    text.push_str(&existing);
                    text
                }
            };
            // 只含受保护载荷的 carrier 没有明文可合并，不应凭空写出空的
            // `reasoning_content` 字段。
            if !merged.is_empty() {
                next_meta
                    .insert_extension("reasoning_content", Value::String(merged))
                    .expect("reasoning content is not reserved");
            }
        } else if !text.is_empty() {
            // 明文降级为 content；清空 meta，避免来源侧扩展（如
            // `__open_responses_item_fields` 或推理签名副本）泄漏到消息字段。
            carrier.content = MessageContent::Text(text.into());
            carrier.meta = None;
            out.insert(idx, carrier);
        }
    }
}

/// An assistant item whose only payload is native reasoning: no tool calls,
/// no tool-call id, and no textual or non-reasoning content blocks.
fn is_standalone_reasoning_item(item: &AiItem) -> bool {
    item.role == Role::Assistant
        && !is_explicit_chat_reasoning_carrier(item)
        && item
            .tool_calls
            .as_ref()
            .is_none_or(|calls| calls.is_empty())
        && item
            .tool_call_id
            .as_ref()
            .is_none_or(|id| id.trim().is_empty())
        && match &item.content {
            MessageContent::Text(text) => text.trim().is_empty(),
            MessageContent::Blocks(blocks) => blocks.iter().all(|block| {
                matches!(
                    block,
                    ContentBlock::Thinking { .. }
                        | ContentBlock::Reasoning { .. }
                        | ContentBlock::RedactedThinking { .. }
                )
            }),
        }
}

fn is_explicit_chat_reasoning_carrier(item: &AiItem) -> bool {
    // 明确为空的 Chat 字段属于原消息，即使正文为空也不能当作独立推理项删除。
    matches!(&item.content, MessageContent::Text(_))
        && item
            .meta
            .as_ref()
            .and_then(|meta| meta.get("reasoning_content"))
            .is_some_and(|value| value.is_null() || value.as_str() == Some(""))
}

/// Reasoning text of a standalone carrier, in encode order. The blocks win:
/// Blocks are the single authoritative source; standalone carriers skip
/// metadata promotion so their plaintext can be consumed directly.
/// `meta.reasoning_content` is the fallback for meta-only carriers.
fn standalone_reasoning_text(item: &mut AiItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let MessageContent::Blocks(blocks) = &mut item.content {
        let blocks = std::mem::take(blocks);
        for block in blocks {
            match block {
                ContentBlock::Thinking { thinking, .. } => {
                    if !thinking.trim().is_empty() {
                        parts.push(thinking);
                    }
                }
                ContentBlock::Reasoning {
                    summary, content, ..
                } => {
                    parts.extend(
                        summary
                            .into_iter()
                            .chain(content)
                            .filter(|text| !text.trim().is_empty()),
                    );
                }
                ContentBlock::RedactedThinking { .. } => {}
                _ => {}
            }
        }
    }
    if parts.is_empty()
        && let Some(reasoning) = item
            .meta
            .as_mut()
            .and_then(|meta| meta.remove_extension("reasoning_content").ok().flatten())
            .and_then(|value| match value {
                Value::String(text) => Some(text),
                _ => None,
            })
    {
        return reasoning;
    }
    let mut parts = parts.into_iter();
    let mut text = parts.next().unwrap_or_default();
    for next in parts {
        text.push('\n');
        text.push_str(&next);
    }
    text
}

fn prune_orphan_assistant_tool_calls(mut messages: Vec<AiItem>) -> Vec<AiItem> {
    let referenced_tool_ids: HashSet<stravia_runtime_contract::protocol::ir::ToolCallId> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .filter(|id| !id.trim().is_empty())
        .collect();

    for msg in &mut messages {
        if msg.role == Role::Assistant
            && let Some(calls) = msg.tool_calls.as_mut()
        {
            calls.retain(|tc| referenced_tool_ids.contains(&tc.id));
            if calls.is_empty() {
                msg.tool_calls = None;
            }
        }
    }
    messages
}

fn collect_supplied_tool_ids(
    messages: &[AiItem],
) -> HashSet<stravia_runtime_contract::protocol::ir::ToolCallId> {
    let mut ids = HashSet::new();
    for msg in messages {
        if let Some(tool_calls) = &msg.tool_calls {
            ids.extend(
                tool_calls
                    .iter()
                    .map(|call| call.id.trim())
                    .filter(|id| !id.is_empty())
                    .map(stravia_runtime_contract::protocol::ir::ToolCallId::new),
            );
        }
        if let Some(id) = msg
            .tool_call_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            ids.insert(id.into());
        }
        if let MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                let id = match block {
                    ContentBlock::ToolUse { id, .. } | ContentBlock::ServerToolUse { id, .. } => {
                        Some(id.as_str())
                    }
                    ContentBlock::ToolResult { tool_use_id, .. }
                    | ContentBlock::ServerToolResult { tool_use_id, .. } => {
                        Some(tool_use_id.as_str())
                    }
                    _ => None,
                };
                if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
                    ids.insert(id.into());
                }
            }
        }
    }
    ids
}

fn next_synthetic_tool_call_id(
    sequence: &mut usize,
    unavailable_ids: &mut HashSet<stravia_runtime_contract::protocol::ir::ToolCallId>,
) -> stravia_runtime_contract::protocol::ir::ToolCallId {
    loop {
        *sequence += 1;
        let id =
            stravia_runtime_contract::protocol::ir::ToolCallId::new(format!("tc_{}", *sequence));
        if unavailable_ids.insert(id.clone()) {
            return id;
        }
    }
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

fn remap_duplicate_tool_call_ids(
    mut messages: Vec<AiItem>,
    system: Option<String>,
    supplied_tool_ids: &HashSet<stravia_runtime_contract::protocol::ir::ToolCallId>,
) -> Vec<AiItem> {
    if let Some(system) = system {
        messages.insert(
            0,
            AiItem {
                role: Role::System,
                content: MessageContent::Text(system.into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        );
    }
    let mut seen_counts: HashMap<stravia_runtime_contract::protocol::ir::ToolCallId, usize> =
        HashMap::new();
    let mut pending_by_original: HashMap<
        stravia_runtime_contract::protocol::ir::ToolCallId,
        Vec<stravia_runtime_contract::protocol::ir::ToolCallId>,
    > = HashMap::new();
    let mut unavailable_tool_ids = supplied_tool_ids.clone();
    let mut generated_seq: usize = 0;

    for msg in &mut messages {
        if msg.role == Role::Assistant {
            if let Some(tool_calls) = &mut msg.tool_calls {
                for tc in tool_calls.iter_mut() {
                    let original = if tc.id.trim().is_empty() {
                        next_synthetic_tool_call_id(&mut generated_seq, &mut unavailable_tool_ids)
                    } else {
                        tc.id.clone()
                    };

                    let count = seen_counts.entry(original.clone()).or_insert(0);
                    *count += 1;
                    let unique = if *count == 1 {
                        original.clone()
                    } else {
                        next_synthetic_tool_call_id(&mut generated_seq, &mut unavailable_tool_ids)
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

    messages
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
    if message
        .meta
        .as_ref()
        .is_some_and(|meta| meta.object_extensions().is_none())
    {
        return;
    }
    let MessageContent::Blocks(blocks) = &message.content else {
        return;
    };
    // Thinking 与 Reasoning 的明文都进入 `reasoning_content`，按块顺序拼接；
    // signature / encrypted_content / redacted 是受保护载荷，绝不进正文。
    let mut reasoning = String::new();
    let push_segment = |reasoning: &mut String, text: &str| {
        if text.is_empty() {
            return;
        }
        if !reasoning.is_empty() {
            reasoning.push('\n');
        }
        reasoning.push_str(text);
    };
    for block in blocks {
        match block {
            ContentBlock::Thinking { thinking, .. } => push_segment(&mut reasoning, thinking),
            ContentBlock::Reasoning {
                summary, content, ..
            } => {
                for text in summary.iter().chain(content) {
                    push_segment(&mut reasoning, text);
                }
            }
            _ => {}
        }
    }
    if reasoning.is_empty() {
        return;
    }
    message
        .meta
        .get_or_insert_with(Default::default)
        .insert_extension("reasoning_content", Value::String(reasoning))
        .expect("reasoning content is not reserved");
}

fn content_has_non_whitespace_text(content: &MessageContent) -> bool {
    match content {
        MessageContent::Text(text) => !text.trim().is_empty(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(ContentBlock::as_text)
            .any(|text| !text.trim().is_empty()),
    }
}

fn tool_message_hint(msg: &AiItem) -> Option<stravia_runtime_contract::protocol::ir::ToolCallId> {
    let MessageContent::Blocks(blocks) = &msg.content else {
        return None;
    };
    for block in blocks {
        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
            return (!tool_use_id.trim().is_empty()).then(|| tool_use_id.clone());
        }
    }
    None
}

fn encode_message(msg: AiItem) -> Result<Value> {
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
        let (tool_content, hinted_tool_call_id) = tool_message_payload(msg.content);
        map.insert("content".into(), Value::String(tool_content));
        let resolved_tool_call_id = msg
            .tool_call_id
            .filter(|v| !v.trim().is_empty())
            .or_else(|| hinted_tool_call_id.filter(|v| !v.trim().is_empty()));
        if let Some(tool_call_id) = resolved_tool_call_id {
            map.insert(
                "tool_call_id".into(),
                Value::String(tool_call_id.into_string()),
            );
        }
        return Ok(obj);
    }

    match msg.content {
        MessageContent::Text(t) => {
            map.insert("content".into(), Value::String(Arc::unwrap_or_clone(t)));
        }
        MessageContent::Blocks(blocks) => {
            // For assistant messages, strip blocks that are already expressed
            // elsewhere in the OpenAI shape:
            //   - Thinking / Reasoning / RedactedThinking: plaintext is surfaced
            //     via the top-level `reasoning_content` field carried in `meta`
            //     (see `promote_reasoning_meta`). Emitting them as plain text
            //     would duplicate reasoning and break strict thinking-mode
            //     upstreams; their protected payloads have no chat carrier.
            //   - ToolUse: already expressed via the `tool_calls` array below.
            //     Encoding it into `content` would produce `{type:"function"}`,
            //     which OpenAI chat/completions rejects with
            //     "unknown variant `function`, expected `text`". This is the
            //     root cause of tool calls failing in Anthropic Messages →
            //     OpenAI cross-protocol conversion (the Anthropic decoder
            //     carries `tool_use` in BOTH `content` and `tool_calls`).
            let strip_for_assistant = msg.role == Role::Assistant;
            let mut visible = blocks.into_iter().filter(|b| {
                !(strip_for_assistant
                    && matches!(
                        b,
                        ContentBlock::Thinking { .. }
                            | ContentBlock::Reasoning { .. }
                            | ContentBlock::RedactedThinking { .. }
                            | ContentBlock::ToolUse { .. }
                    ))
            });
            let first = visible.next();
            let second = visible.next();
            match (first, second) {
                // 单一文本优先使用两种合法载体中的字符串形式，
                // 避免要求兼容上游同时支持多模态 content 数组。
                (Some(ContentBlock::Text { text, .. }), None) => {
                    map.insert("content".into(), Value::String(Arc::unwrap_or_clone(text)));
                }
                (Some(first), second) => {
                    let parts = std::iter::once(first)
                        .chain(second)
                        .chain(visible)
                        .map(encode_content_block_for_openai)
                        .collect();
                    map.insert("content".into(), Value::Array(parts));
                }
                (None, _) => {}
            }
            // An assistant turn that carries only tool calls / thinking has no
            // textual content — leave `content` unset (OpenAI accepts its
            // absence when `tool_calls` is present) rather than emitting `[]`,
            // which some strict upstreams reject.
        }
    }

    if let Some(tcs) = msg.tool_calls {
        let arr: Vec<Value> = tcs
            .into_iter()
            .map(|tc| {
                object([
                    ("id", Value::String(tc.id.into_string())),
                    ("type", Value::String("function".into())),
                    (
                        "function",
                        object([
                            ("name", Value::String(tc.name)),
                            ("arguments", Value::String(tc.arguments)),
                        ]),
                    ),
                ])
            })
            .collect();
        map.insert("tool_calls".into(), Value::Array(arr));
    }
    if let Some(tid) = msg.tool_call_id {
        map.insert("tool_call_id".into(), Value::String(tid.into_string()));
    }

    // Internal canonical metadata participates in local semantics but must never
    // become an upstream vendor field.
    if let Some(extra) = msg.meta.and_then(|meta| meta.into_object_extensions()) {
        for (key, value) in extra {
            if !key.starts_with("__stravia_")
                // `reasoning_signature` 是受保护载荷的来源侧载体，chat 协议无法
                // 承载，静默丢弃而不是作为可读字段透传。
                && key.as_str() != "reasoning_signature"
            {
                map.entry(key).or_insert(value);
            }
        }
    }

    // Strict chat-completions upstreams (e.g. DeepSeek) reject assistant
    // messages that have neither `content` nor `tool_calls`. Every normal
    // path yields one of the two; this net guarantees the invariant even for
    // unexpected block combinations.
    if msg.role == Role::Assistant
        && !map.contains_key("tool_calls")
        && map.get("content").is_none_or(Value::is_null)
    {
        map.insert("content".into(), Value::String(String::new()));
    }

    Ok(obj)
}

fn encode_content_block_for_openai(b: ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text, .. } => object([
            ("type", Value::String("text".into())),
            ("text", Value::String(Arc::unwrap_or_clone(text))),
        ]),
        ContentBlock::Image { source, detail, .. } => {
            let url = media_source_to_url(source);
            let mut image_url = object([("url", Value::String(url))]);
            if let Some(detail) = detail {
                image_url["detail"] = Value::String(detail);
            }
            object([
                ("type", Value::String("image_url".into())),
                ("image_url", image_url),
            ])
        }
        ContentBlock::Audio { source } => match source {
            MediaSource::Base64 { media_type, data } => {
                let format = if matches!(
                    media_type.as_str(),
                    "audio/wav" | "audio/x-wav" | "audio/wave"
                ) {
                    "wav"
                } else {
                    "mp3"
                };
                object([
                    ("type", Value::String("input_audio".into())),
                    (
                        "input_audio",
                        object([
                            ("data", Value::String(data)),
                            ("format", Value::String(format.into())),
                        ]),
                    ),
                ])
            }
            _ => object([
                ("type", Value::String("input_audio".into())),
                (
                    "input_audio",
                    object([("data", Value::String(media_source_to_url(source)))]),
                ),
            ]),
        },
        ContentBlock::File { source, .. } => {
            let file = match source {
                MediaSource::Base64 { .. } => object([
                    ("filename", Value::String("attachment".into())),
                    ("file_data", Value::String(media_source_to_url(source))),
                ]),
                MediaSource::FileId { file_id, .. } => {
                    object([("file_id", Value::String(file_id))])
                }
                MediaSource::Url(url) => object([("file_url", Value::String(url))]),
            };
            object([("type", Value::String("file".into())), ("file", file)])
        }
        ContentBlock::ToolUse {
            id, name, input, ..
        } => object([
            ("type", Value::String("function".into())),
            ("id", Value::String(id.into_string())),
            (
                "function",
                object([
                    ("name", Value::String(name)),
                    ("arguments", Value::String(input.to_string())),
                ]),
            ),
        ]),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => object([
            ("type", Value::String("text".into())),
            (
                "text",
                Value::String(match content {
                    Value::String(s) => s,
                    Value::Null => String::new(),
                    other => other.to_string(),
                }),
            ),
            ("tool_call_id", Value::String(tool_use_id.into_string())),
        ]),
        ContentBlock::Thinking { thinking, .. } => {
            // OpenAI does not support thinking blocks; pass as plain text
            object([
                ("type", Value::String("text".into())),
                ("text", Value::String(thinking)),
            ])
        }
        // 非 assistant 角色的 Reasoning 没有 reasoning_content 载体，明文降级为
        // text；encrypted_content 是受保护载荷，一并丢弃。
        ContentBlock::Reasoning {
            summary, content, ..
        } => object([
            ("type", Value::String("text".into())),
            (
                "text",
                Value::String(summary.into_iter().chain(content).collect::<String>()),
            ),
        ]),
        ContentBlock::RedactedThinking { .. } => {
            serde_json::json!({"type": "text", "text": ""})
        }
        ContentBlock::Unknown { raw } => raw,
        other => {
            // Other block types (Document, SearchResult, etc.) not supported
            // by OpenAI chat/completions; serialise raw as fallback.
            crate::codec::content_block_wire_value(&other)
        }
    }
}

fn media_source_to_url(source: MediaSource) -> String {
    match source {
        MediaSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
        MediaSource::Url(url) => url,
        MediaSource::FileId { file_id, .. } => file_id,
    }
}

fn tool_message_payload(
    content: MessageContent,
) -> (
    String,
    Option<stravia_runtime_contract::protocol::ir::ToolCallId>,
) {
    match content {
        MessageContent::Text(t) => (Arc::unwrap_or_clone(t), None),
        MessageContent::Blocks(blocks) => {
            if !blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                let mut texts = blocks.into_iter().filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(Arc::unwrap_or_clone(text)),
                    _ => None,
                });
                let mut text = texts.next().unwrap_or_default();
                for next in texts {
                    text.push_str(&next);
                }
                return (text, None);
            }
            for block in blocks {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                {
                    let text = match content {
                        Value::String(s) => s,
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    let hinted_id = if tool_use_id.trim().is_empty() {
                        None
                    } else {
                        Some(tool_use_id)
                    };
                    return (text, hinted_id);
                }
            }
            unreachable!("tool result existence checked above")
        }
    }
}

#[cfg(test)]
mod tests;
