use anyhow::Result;
use http::header::HeaderMap;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::request::AiItem;
use stravia_runtime_contract::protocol::ir::request::ContentBlock;
use stravia_runtime_contract::protocol::ir::request::MediaSource;
use stravia_runtime_contract::protocol::ir::request::MessageContent;
use stravia_runtime_contract::protocol::ir::request::Role;
use stravia_runtime_contract::protocol::ir::request::ToolChoice;
use stravia_runtime_contract::protocol::ir::request::ToolSpec;
use stravia_runtime_contract::protocol::ir::response::RESPONSE_HISTORY_GROUP_META;

pub struct OpenAIEncoder;

impl OpenAIEncoder {
    pub(crate) fn encode_request(&self, mut req: AiRequest) -> Result<(Value, HeaderMap)> {
        req.normalize_parameters();
        let tools = req.tools.as_deref().unwrap_or(&[]);
        let tools_opt: Option<&[ToolSpec]> = if tools.is_empty() { None } else { Some(tools) };
        let normalized_messages =
            normalize_messages_for_openai(req.items, req.instructions, tools_opt);
        let mut messages: Vec<Value> = Vec::with_capacity(normalized_messages.len());
        let mut previous_group = None;
        let mut omitted_fields = Vec::new();
        for mut item in normalized_messages {
            let group = item.meta.as_mut().and_then(|meta| {
                meta.remove_extension(RESPONSE_HISTORY_GROUP_META)
                    .expect("response grouping is internal metadata")
            });
            if let Some(mut message) = encode_message(item)? {
                if group.is_some() {
                    // Chat has no item-level phase; it remains on the source IR.
                    message
                        .as_object_mut()
                        .expect("encoded message")
                        .remove("phase");
                }
                if group.is_some()
                    && group == previous_group
                    && let Some(previous) = messages.last_mut()
                    && previous.get("role").and_then(Value::as_str) == Some("assistant")
                    && message.get("role").and_then(Value::as_str) == Some("assistant")
                {
                    merge_response_assistant(previous, message, &mut omitted_fields);
                } else {
                    omitted_fields.clear();
                    messages.push(message);
                }
            }
            previous_group = group;
        }

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
        if let Some(stop) = req.generation.stop {
            obj.insert(
                "stop".into(),
                Value::Array(stop.into_iter().map(Value::String).collect()),
            );
        }
        if let Some(parallel) = req.parallel_tool_calls {
            obj.insert("parallel_tool_calls".into(), Value::Bool(parallel));
        }
        if let Some(format) = req.response_format {
            use stravia_runtime_contract::protocol::ir::ResponseFormat;

            let format = match format {
                ResponseFormat::Text => object([("type", Value::String("text".into()))]),
                ResponseFormat::JsonObject => {
                    object([("type", Value::String("json_object".into()))])
                }
                ResponseFormat::JsonSchema {
                    name,
                    description,
                    schema,
                    strict,
                } => {
                    let mut json_schema =
                        object([("name", Value::String(name)), ("schema", schema)]);
                    if let Some(strict) = strict {
                        json_schema["strict"] = Value::Bool(strict);
                    }
                    if let Some(description) = description {
                        json_schema["description"] = Value::String(description);
                    }
                    object([
                        ("type", Value::String("json_schema".into())),
                        ("json_schema", json_schema),
                    ])
                }
            };
            obj.insert("response_format".into(), format);
        }
        match req.reasoning.target_control {
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { value }) => {
                obj.insert("reasoning_effort".into(), Value::String(value));
            }
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled) => {
                obj.insert("reasoning_effort".into(), Value::String("none".into()));
            }
            None => {
                if req.reasoning.mode.as_deref() == Some("disabled") {
                    obj.insert("reasoning_effort".into(), "none".into());
                } else if let Some(level) = req.reasoning.level {
                    let effort = match level {
                        stravia_runtime_contract::thinking::ThinkingLevel::Off => "none",
                        stravia_runtime_contract::thinking::ThinkingLevel::Max => "xhigh",
                        other => other.as_str(),
                    };
                    obj.insert("reasoning_effort".into(), effort.into());
                }
            }
            _ => {}
        }

        if let Some(tools) = req.tools {
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
        if let Some(tc) = req.tool_choice.and_then(tool_choice_to_value) {
            obj.insert("tool_choice".into(), tc);
        }

        // Stream controls have a single canonical owner.
        if req.stream.enabled {
            let options = serde_json::json!({"include_usage": req.stream.include_usage});
            obj.insert("stream_options".into(), options);
        }
        if let Some(value) = req.generation.seed {
            obj.insert("seed".into(), value.into());
        }
        if let Some(value) = req.generation.frequency_penalty {
            obj.insert("frequency_penalty".into(), value.into());
        }
        if let Some(value) = req.generation.presence_penalty {
            obj.insert("presence_penalty".into(), value.into());
        }
        let parameters = req.parameters;
        if let Some(service) = parameters.service_tier {
            obj.insert("service_tier".into(), serde_json::to_value(service)?);
        }
        if let Some(logprobs) = parameters.logprobs {
            obj.insert("logprobs".into(), Value::Bool(true));
            obj.insert("top_logprobs".into(), logprobs.into());
        }
        if let Some(audio) = parameters.audio {
            let mut value = serde_json::Map::new();
            if let Some(voice) = audio.voice {
                value.insert("voice".into(), voice.into());
            }
            if let Some(format) = audio.format {
                value.insert("format".into(), format.into());
            }
            obj.insert("audio".into(), Value::Object(value));
        }
        for (name, value) in
            [
                (
                    "metadata",
                    parameters.metadata.map(|value| serde_json::json!(value)),
                ),
                (
                    "logit_bias",
                    parameters.logit_bias.map(|value| serde_json::json!(value)),
                ),
                (
                    "modalities",
                    parameters.modalities.map(|value| serde_json::json!(value)),
                ),
                ("n", parameters.candidate_count.map(Value::from)),
                ("store", parameters.store.map(Value::from)),
                (
                    "safety_identifier",
                    parameters.end_user_id.map(Value::String),
                ),
                (
                    "verbosity",
                    parameters.verbosity.map(|value| serde_json::json!(value)),
                ),
                ("prompt_cache_key", parameters.cache.key.map(Value::String)),
                (
                    "prompt_cache_retention",
                    parameters.cache.retention.map(|value| {
                        Value::String(match value {
                    stravia_runtime_contract::protocol::ir::CacheRetention::Default => "in_memory",
                    stravia_runtime_contract::protocol::ir::CacheRetention::Extended => "24h",
                }.into())
                    }),
                ),
            ]
        {
            if let Some(value) = value {
                obj.insert(name.into(), value);
            }
        }
        if let Some(options) = parameters.native_tools.iter().find_map(
            stravia_runtime_contract::protocol::ir::native_tool_to_chat_web_search_options,
        ) {
            obj.insert("web_search_options".into(), options);
        }

        crate::codec::control_projection::project_request(
            &mut body,
            crate::codec::control_projection::Family::Chat,
        );
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

fn tool_choice_to_value(tc: ToolChoice) -> Option<Value> {
    Some(match tc {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Named { name } => object([
            ("type", Value::String("function".into())),
            ("function", object([("name", Value::String(name))])),
        ]),
        ToolChoice::Allowed { .. } | ToolChoice::Native(_) => return None,
    })
}

/// The caller has verified the platform response-group identity; item order,
/// source protocol and phase are not used to guess message boundaries.
fn merge_response_assistant(previous: &mut Value, next: Value, omitted_fields: &mut Vec<String>) {
    let previous = previous.as_object_mut().expect("encoded assistant");
    let Value::Object(fields) = next else {
        unreachable!("encoded assistant")
    };
    for (key, value) in fields {
        if omitted_fields.contains(&key) {
            continue;
        }
        if key == "content" {
            append_chat_content(previous.entry(key).or_insert(Value::Null), value);
            continue;
        }
        let Some(existing) = previous.get_mut(&key) else {
            previous.insert(key, value);
            continue;
        };
        match (key.as_str(), existing, value) {
            ("reasoning_content", _, Value::Null) => {}
            ("reasoning_content", existing @ Value::Null, value) => *existing = value,
            ("reasoning_content", Value::String(existing), Value::String(text)) => {
                existing.push_str(&text)
            }
            ("tool_calls", Value::Array(existing), Value::Array(calls)) => existing.extend(calls),
            (_, existing, value) if *existing == value => {}
            _ => {
                // A single Chat message cannot represent conflicting item-level
                // extensions. Omit only that extension, never the response.
                previous.remove(&key);
                omitted_fields.push(key);
            }
        }
    }
}

fn append_chat_content(existing: &mut Value, content: Value) {
    match content {
        Value::Null => {}
        Value::String(text) if text.is_empty() => {}
        Value::String(text) => match existing {
            Value::String(current) => current.push_str(&text),
            Value::Array(parts) => {
                parts.push(object([("type", "text".into()), ("text", text.into())]))
            }
            _ => *existing = Value::String(text),
        },
        Value::Array(mut parts) => match existing {
            Value::Array(current) => current.append(&mut parts),
            Value::String(text) if !text.is_empty() => {
                parts.insert(
                    0,
                    object([
                        ("type", "text".into()),
                        ("text", std::mem::take(text).into()),
                    ]),
                );
                *existing = Value::Array(parts);
            }
            _ => *existing = Value::Array(parts),
        },
        _ => unreachable!("encoded Chat content is text, parts or null"),
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
            promote_reasoning_meta(&mut msg);
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

        // Results refer to an earlier call, not necessarily the immediately
        // preceding message. Moving calls here serializes parallel batches and
        // can move their reasoning past an already-observed result.
        if !seen_tool_call_ids.contains(&final_id) {
            out.push(AiItem {
                role: Role::Assistant,
                content: MessageContent::Text(String::new().into()),
                tool_calls: Some(vec![ToolCall {
                    id: final_id.clone(),
                    name: fallback_tool_name.clone(),
                    arguments: "{}".to_string(),
                }]),
                tool_call_id: None,
                meta: None,
            });
            seen_tool_call_ids.insert(final_id.clone());
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
        if matches!(&msg.content, MessageContent::Text(_)) {
            return true;
        }
        let has_calls = msg.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
        if has_calls {
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
        has_reasoning || matches!(&msg.content, MessageContent::Blocks(blocks) if blocks.iter().any(|block| !matches!(block,
            ContentBlock::Thinking { .. } | ContentBlock::Reasoning { .. }
            | ContentBlock::RedactedThinking { .. } | ContentBlock::ToolUse { .. }
        )))
    });

    out
}

fn prune_orphan_assistant_tool_calls(mut messages: Vec<AiItem>) -> Vec<AiItem> {
    let referenced_tool_ids: HashSet<stravia_runtime_contract::protocol::ir::ToolCallId> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .filter(|id| !id.trim().is_empty())
        .collect();

    messages.retain_mut(|msg| {
        if msg.role == Role::Assistant
            && let Some(calls) = msg.tool_calls.as_mut()
        {
            calls.retain(|tc| referenced_tool_ids.contains(&tc.id));
            if calls.is_empty() {
                msg.tool_calls = None;
                // Remove a shell created by orphan cleanup, not an originally
                // empty assistant message supplied by the caller.
                if msg.meta.is_none()
                    && matches!(&msg.content, MessageContent::Text(text) if text.is_empty())
                {
                    return false;
                }
            }
        }
        true
    });
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

fn encode_message(msg: AiItem) -> Result<Option<Value>> {
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
        let Some(tool_content) = tool_content else {
            return Ok(None);
        };
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
        return Ok(Some(obj));
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

    Ok(Some(obj))
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
        ContentBlock::Document { source, title, .. } => {
            let Some(source) = crate::codec::document_media_source(source) else {
                return Value::Null;
            };
            let mut encoded = encode_content_block_for_openai(ContentBlock::File {
                source,
                media_type: None,
            });
            if let Some(title) = title {
                encoded["file"]["filename"] = Value::String(title);
            }
            encoded
        }
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
        MediaSource::Base64 { media_type, data } => crate::codec::media_data_url(&media_type, data),
        MediaSource::Url(url) => url,
        MediaSource::FileId { file_id, .. } => file_id,
    }
}

pub(crate) fn tool_result_content_is_supported(
    content: &Value,
    kind: Option<stravia_runtime_contract::protocol::ir::ToolResultContentKind>,
) -> bool {
    if content.is_string()
        || kind
            != Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks)
    {
        return true;
    }
    content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block["type"].as_str() == Some("text") && block["text"].is_string())
    })
}

fn tool_message_payload(
    content: MessageContent,
) -> (
    Option<String>,
    Option<stravia_runtime_contract::protocol::ir::ToolCallId>,
) {
    match content {
        MessageContent::Text(t) => (Some(Arc::unwrap_or_clone(t)), None),
        MessageContent::Blocks(blocks) => {
            if !blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                let mut texts = blocks.into_iter().filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(Arc::unwrap_or_clone(text)),
                    _ => None,
                });
                let mut text = texts.next();
                for next in texts {
                    text.as_mut().expect("first text exists").push_str(&next);
                }
                return (text, None);
            }
            for block in blocks {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    content_kind,
                    ..
                } = block
                {
                    let text = match content {
                        Value::String(s) => Some(s),
                        Value::Array(blocks)
                            if content_kind
                                == Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks) =>
                        {
                            let mut text: Option<String> = None;
                            for block in blocks {
                                if let Ok(ContentBlock::Text { text: next, .. }) =
                                    serde_json::from_value::<ContentBlock>(block)
                                {
                                    let next = Arc::unwrap_or_clone(next);
                                    if let Some(text) = &mut text {
                                        text.push_str(&next);
                                    } else {
                                        text = Some(next);
                                    }
                                }
                            }
                            text
                        }
                        _ if content_kind
                            == Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks) => None,
                        Value::Null if content_kind.is_none() => Some(String::new()),
                        other => Some(other.to_string()),
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
