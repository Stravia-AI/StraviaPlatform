//! Open Responses Protocol 2026-04-24 request encoder.

use anyhow::Result;
use http::header::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::request::ContentBlock;
use stravia_runtime_contract::protocol::ir::request::MediaSource;
use stravia_runtime_contract::protocol::ir::request::MessageContent;
use stravia_runtime_contract::protocol::ir::request::ResponseFormat;
use stravia_runtime_contract::protocol::ir::request::Role;
use stravia_runtime_contract::protocol::ir::request::ToolChoice;

/// Encoder for the dated Open Responses request contract.
pub struct ResponsesEncoder;

// Open Responses fields are emitted only from the canonical IR and dated extension.
impl ResponsesEncoder {
    pub fn encode_request(&self, mut req: AiRequest) -> Result<(Value, HeaderMap)> {
        req.normalize_parameters();
        validate_target_thinking_control(&req)?;
        let mut input: Vec<Value> = Vec::new();

        for mut item in std::mem::take(&mut req.items) {
            if let Some(native) = take_native_compaction_item(&mut item) {
                input.push(native);
                continue;
            }
            if let Some(reference_id) = item
                .item_reference()
                .map(stravia_runtime_contract::protocol::ir::ItemReference::as_str)
            {
                input.push(serde_json::json!({
                    "type": "item_reference",
                    "id": reference_id,
                }));
                continue;
            }
            // Chat `reasoning_content` 与 tool calls 共享同一条 assistant 条目，
            // Reasoning 也可能与 tool_calls / 普通块混合。混合拆分必须先于单块
            // 快路径，否则 tool_calls 会被丢掉。
            if item
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
                && let Some(items) = encode_mixed_assistant_items(&mut item)?
            {
                input.extend(items);
                continue;
            }
            if let Some((summary, content, encrypted_content)) = item.reasoning_ref() {
                let encrypted_content = encrypted_content.filter(|value| !value.is_empty());
                // reasoning 输入条目须携带上游 id 或 encrypted_content 才被接受；
                // 条目上的图 id 可能是网关本地 id，不能作为依据。无密文时明文降级为
                // assistant message 的 output_text。
                if encrypted_content.is_some() {
                    input.push(encode_native_reasoning_item(
                        &item,
                        summary,
                        encrypted_content,
                    ));
                    let mut degraded = degraded_reasoning_parts(content.iter());
                    push_derived_assistant_message(&mut input, &mut degraded);
                } else {
                    let mut degraded =
                        degraded_reasoning_parts(summary.iter().chain(content.iter()));
                    push_derived_assistant_message(&mut input, &mut degraded);
                }
                continue;
            }
            if let Some((text, _)) = item.thinking_ref() {
                let mut degraded = degraded_reasoning_parts(std::iter::once(text));
                push_derived_assistant_message(&mut input, &mut degraded);
                continue;
            }
            if let Some(raw) = item.unknown_ref() {
                let item_type = raw.get("type").and_then(Value::as_str).unwrap_or_default();
                if !super::is_registered_extension_item(item_type) {
                    continue;
                }
                let MessageContent::Blocks(mut blocks) = item.content else {
                    unreachable!("unknown item has block content");
                };
                let ContentBlock::Unknown { raw } = blocks.remove(0) else {
                    unreachable!("unknown item has an unknown block");
                };
                input.push(raw);
                continue;
            }
            if item.role == Role::Tool {
                let mut output = serde_json::json!({
                    "type": "function_call_output",
                });
                output["call_id"] =
                    Value::String(item.tool_call_id.take().unwrap_or_default().into_string());
                insert_owned_item_metadata(&mut output, &mut item, true);
                let Some(content) = encode_tool_output(std::mem::replace(
                    &mut item.content,
                    MessageContent::Blocks(Vec::new()),
                )) else {
                    continue;
                };
                output["output"] = content;
                input.push(output);
                continue;
            }

            if let Some(items) = encode_mixed_assistant_items(&mut item)? {
                input.extend(items);
                continue;
            }

            let content = if let Some(refusal) = item.refusal_ref() {
                Some(vec![serde_json::json!({
                    "type": "refusal",
                    "refusal": refusal,
                })])
            } else {
                encode_message_content(
                    std::mem::replace(&mut item.content, MessageContent::Blocks(Vec::new())),
                    item.role,
                )?
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
                });
                message["content"] = Value::Array(content);
                // 工具调用的原生身份仍归调用所有，拆出的前置正文不能继承它。
                if item.tool_calls.as_ref().is_none_or(Vec::is_empty) {
                    insert_owned_item_metadata(&mut message, &mut item, true);
                }
                if let Some(phase) = item
                    .meta
                    .as_mut()
                    .and_then(|meta| meta.remove_extension("phase").ok().flatten())
                {
                    message["phase"] = phase;
                }
                input.push(message);
            }

            if let Some(tool_calls) = item.tool_calls.take() {
                let single_call = tool_calls.len() == 1;
                for tool_call in tool_calls {
                    let mut call = serde_json::json!({
                        "type": "function_call",
                    });
                    call["call_id"] = Value::String(tool_call.id.into_string());
                    call["name"] = Value::String(tool_call.name);
                    call["arguments"] = Value::String(tool_call.arguments);
                    if single_call {
                        insert_owned_item_metadata(&mut call, &mut item, true);
                    }
                    input.push(call);
                }
            }
        }

        if input.is_empty() && req.parameters.continuation.is_none() {
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
        insert_owned_request_control_fields(
            body.as_object_mut().expect("request body is an object"),
            req,
        );

        crate::codec::control_projection::project_request(
            &mut body,
            crate::codec::control_projection::Family::Responses,
        );
        Ok((body, HeaderMap::new()))
    }

    pub(crate) fn egress_path(&self, _model: &str, _stream: bool) -> String {
        "/v1/responses".to_string()
    }
}

pub fn response_profile_from_request(req: &AiRequest) -> Value {
    let mut profile = serde_json::Map::new();
    insert_request_control_fields(&mut profile, req);
    profile.remove("include");
    profile.remove("stream_options");
    profile
        .entry("metadata")
        .or_insert_with(|| serde_json::json!({}));
    let mut profile = Value::Object(profile);
    crate::codec::control_projection::project_request(
        &mut profile,
        crate::codec::control_projection::Family::Responses,
    );
    profile
}
pub fn effective_response_profile_from_request(req: &AiRequest) -> Value {
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

fn insert_owned_request_control_fields(
    obj: &mut serde_json::Map<String, Value>,
    mut req: AiRequest,
) {
    let foreign_ingress = req.meta.source_protocol.is_some_and(|protocol| {
        protocol != stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
    });
    let tools = req.tools.take();
    let format = req.response_format.take();
    let tool_choice = req.tool_choice.take();
    let native_tools = std::mem::take(&mut req.parameters.native_tools);
    insert_request_control_fields(obj, &req);
    if let Some(tools) = tools {
        let encoded = tools
            .into_iter()
            .map(|tool| {
                let mut encoded = serde_json::json!({"type": "function"});
                encoded["name"] = Value::String(tool.name);
                if let Some(description) = tool.description {
                    encoded["description"] = Value::String(description);
                }
                encoded["parameters"] = tool.parameters;
                // Preserve foreign ingress's non-strict default: Responses
                // otherwise makes an omitted strict flag strict-by-default.
                if let Some(strict) = tool.strict.or(foreign_ingress.then_some(false)) {
                    encoded["strict"] = Value::Bool(strict);
                }
                if let Some(defer_loading) = tool.defer_loading {
                    encoded["defer_loading"] = Value::Bool(defer_loading);
                }
                encoded
            })
            .collect();
        obj.insert("tools".into(), Value::Array(encoded));
    }
    if let Some(choice) = tool_choice {
        let encoded = tool_choice_to_value(&choice, &native_tools);
        obj.insert("tool_choice".into(), encoded);
    }
    if let Some(format) = format {
        let format = match format {
            ResponseFormat::Text => serde_json::json!({"type": "text"}),
            ResponseFormat::JsonSchema {
                name,
                description,
                schema,
                strict,
            } => {
                let mut format = serde_json::json!({"type": "json_schema"});
                format["name"] = Value::String(name);
                format["schema"] = schema;
                if let Some(description) = description {
                    format["description"] = Value::String(description);
                }
                if let Some(strict) = strict {
                    format["strict"] = Value::Bool(strict);
                }
                format
            }
            ResponseFormat::JsonObject => Value::Null,
        };
        if !format.is_null() {
            if obj.get("text").is_some_and(Value::is_null) {
                obj.remove("text");
            }
            obj.entry("text").or_insert_with(|| serde_json::json!({}))["format"] = format;
        }
    }
    for tool in native_tools
        .iter()
        .filter_map(stravia_runtime_contract::protocol::ir::native_tool_to_responses)
    {
        obj.entry("tools")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("encoded tools are an array")
            .push(tool);
    }
}

fn insert_request_control_fields(obj: &mut serde_json::Map<String, Value>, req: &AiRequest) {
    obj.insert("store".into(), req.parameters.store.unwrap_or(true).into());
    if let Some(instructions) = &req.instructions {
        obj.insert("instructions".into(), Value::String(instructions.clone()));
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
    let mut reasoning = serde_json::Map::new();
    if let Some(control) = req.reasoning.target_control.as_ref() {
        let effort = match control {
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort { value } => {
                value.as_str()
            }
            stravia_runtime_contract::thinking::TargetThinkingControl::Disabled => "none",
            _ => "",
        };
        if !effort.is_empty() {
            reasoning.insert("effort".into(), Value::String(effort.into()));
        }
    } else if let Some(level) = req.reasoning.level {
        reasoning.insert(
            "effort".into(),
            Value::String(
                if level == stravia_runtime_contract::thinking::ThinkingLevel::Off {
                    "none"
                } else {
                    level.as_str()
                }
                .into(),
            ),
        );
    } else if req.reasoning.mode.as_deref() == Some("disabled") {
        reasoning.insert("effort".into(), Value::String("none".into()));
    }
    use stravia_runtime_contract::protocol::ir::ReasoningSummary;
    let summary = match req.reasoning.summary.as_ref() {
        Some(ReasoningSummary::None) => None,
        Some(summary) => Some(summary),
        None if req.reasoning.mode.as_deref() == Some("disabled")
            || req.reasoning.level
                == Some(stravia_runtime_contract::thinking::ThinkingLevel::Off)
            || matches!(
                req.reasoning.target_control,
                Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled)
            )
            || req
                .parameters
                .resets(stravia_runtime_contract::protocol::ir::ParameterReset::Reasoning)
            || req.parameters.resets(
                stravia_runtime_contract::protocol::ir::ParameterReset::ReasoningSummary,
            ) =>
        {
            None
        }
        None => Some(&ReasoningSummary::Auto),
    };
    if let Some(summary) = summary {
        reasoning.insert(
            "summary".into(),
            serde_json::to_value(summary).expect("summary serializes"),
        );
    }
    if !reasoning.is_empty() {
        obj.insert("reasoning".into(), Value::Object(reasoning));
    }
    if let Some(tools) = &req.tools {
        let foreign_ingress = req.meta.source_protocol.is_some_and(|protocol| {
            protocol != stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
        });
        let tools = tools
            .iter()
            .map(|tool| {
                let mut encoded = serde_json::json!({
                    "type": "function",
                    "name": tool.name,
                    "parameters": tool.parameters,
                });
                if let Some(description) = &tool.description {
                    encoded["description"] = Value::String(description.clone());
                }
                // Chat、Anthropic 等协议省略 `strict` 即非严格；Responses 省略时上游会自动
                // 严格化并把可选参数改成必填，所以非 Responses 入口必须显式写出 false。
                if let Some(strict) = tool.strict.or(foreign_ingress.then_some(false)) {
                    encoded["strict"] = Value::Bool(strict);
                }
                if let Some(defer_loading) = tool.defer_loading {
                    encoded["defer_loading"] = Value::Bool(defer_loading);
                }
                encoded
            })
            .collect();
        obj.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tool_choice) = &req.tool_choice {
        obj.insert(
            "tool_choice".into(),
            tool_choice_to_value(tool_choice, &req.parameters.native_tools),
        );
    }
    if let Some(format) = req.response_format.as_ref() {
        let format = match format {
            ResponseFormat::Text => serde_json::json!({"type": "text"}),
            ResponseFormat::JsonSchema {
                name,
                description,
                schema,
                strict,
            } => {
                let mut format = serde_json::json!({
                    "type": "json_schema",
                    "name": name,
                    "schema": schema,
                });
                if let Some(description) = description {
                    format["description"] = Value::String(description.clone());
                }
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
    let parameters = &req.parameters;
    if let Some(value) = parameters.generate {
        obj.insert("generate".into(), value.into());
    }
    if let Some(value) = parameters.background {
        obj.insert("background".into(), value.into());
    }
    if let Some(continuation) = &parameters.continuation {
        use stravia_runtime_contract::protocol::ir::Continuation;
        let (key, id) = match continuation {
            Continuation::PreviousResponse(id) => ("previous_response_id", id),
            Continuation::Conversation(id) => ("conversation", id),
        };
        obj.insert(key.into(), Value::String(id.clone()));
    }
    let reasoning_enabled = req.reasoning.mode.as_deref() != Some("disabled")
        && req.reasoning.level != Some(stravia_runtime_contract::thinking::ThinkingLevel::Off)
        && !matches!(
            req.reasoning.target_control,
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled)
        )
        && (req
            .reasoning
            .mode
            .as_deref()
            .is_some_and(|mode| matches!(mode, "enabled" | "auto"))
            || req.reasoning.level.is_some()
            || req.reasoning.target_control.is_some()
            || req.reasoning.budget_tokens.is_some());
    let mut include = Vec::new();
    if parameters.store == Some(false) && reasoning_enabled {
        include.push(Value::String("reasoning.encrypted_content".into()));
    }
    if parameters.logprobs.is_some() {
        include.push(Value::String("message.output_text.logprobs".into()));
    }
    if !include.is_empty() {
        obj.insert("include".into(), Value::Array(include));
    }
    if let Some(value) = parameters.max_tool_calls {
        obj.insert("max_tool_calls".into(), value.into());
    }
    if let Some(value) = &parameters.cache.key {
        obj.insert("prompt_cache_key".into(), Value::String(value.clone()));
    }
    if let Some(value) = &parameters.cache.retention {
        use stravia_runtime_contract::protocol::ir::CacheRetention;
        obj.insert(
            "prompt_cache_retention".into(),
            Value::String(
                match value {
                    CacheRetention::Default => "in_memory",
                    CacheRetention::Extended => "24h",
                }
                .into(),
            ),
        );
    }
    if let Some(value) = parameters.logprobs {
        obj.insert("top_logprobs".into(), value.into());
    }
    if let Some(value) = &parameters.truncation {
        obj.insert(
            "truncation".into(),
            serde_json::to_value(value).expect("truncation serializes"),
        );
    }
    if let Some(value) = &parameters.verbosity {
        obj.entry("text").or_insert_with(|| serde_json::json!({}))["verbosity"] =
            serde_json::to_value(value).expect("verbosity serializes");
    }
    if let Some(value) = &parameters.service_tier {
        obj.insert(
            "service_tier".into(),
            serde_json::to_value(value).expect("service tier serializes"),
        );
    }
    if let Some(value) = &parameters.metadata {
        obj.insert(
            "metadata".into(),
            serde_json::to_value(value).expect("string metadata serializes"),
        );
    }
    if let Some(value) = &parameters.end_user_id {
        obj.insert("safety_identifier".into(), Value::String(value.clone()));
    }
    for tool in parameters
        .native_tools
        .iter()
        .filter_map(stravia_runtime_contract::protocol::ir::native_tool_to_responses)
    {
        let tools = obj
            .entry("tools")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("encoded tools are an array");
        tools.push(tool);
    }
    if let Some(value) = &parameters.compaction {
        let mut control = serde_json::json!({"type": "compaction"});
        if let Some(threshold) = value.threshold_tokens {
            control["compact_threshold"] = threshold.into();
        }
        obj.insert("context_management".into(), Value::Array(vec![control]));
    }
}

fn validate_target_thinking_control(req: &AiRequest) -> anyhow::Result<()> {
    let Some(control) = req.reasoning.target_control.as_ref() else {
        return Ok(());
    };
    match control {
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort { .. }
        | stravia_runtime_contract::thinking::TargetThinkingControl::Disabled => Ok(()),
        _ => anyhow::bail!("Open Responses cannot represent Target Thinking Control {control:?}"),
    }
}

fn take_native_compaction_item(
    item: &mut stravia_runtime_contract::protocol::ir::AiItem,
) -> Option<Value> {
    if !item.is_compaction() && !item.is_compaction_trigger() {
        return None;
    }
    let mut wire = item
        .meta
        .as_mut()
        .and_then(|meta| {
            meta.remove_extension("__open_responses_item")
                .ok()
                .flatten()
        })
        .and_then(|value| match value {
            Value::Object(object) => Some(object),
            _ => None,
        })
        .unwrap_or_default();
    let MessageContent::Blocks(mut blocks) =
        std::mem::replace(&mut item.content, MessageContent::Blocks(Vec::new()))
    else {
        unreachable!("native compaction content was checked above");
    };
    match blocks.remove(0) {
        ContentBlock::Compaction { encrypted_content } => {
            wire.insert("type".into(), Value::String("compaction".into()));
            wire.insert("encrypted_content".into(), Value::String(encrypted_content));
        }
        ContentBlock::CompactionTrigger {} => {
            wire.insert("type".into(), Value::String("compaction_trigger".into()));
        }
        _ => unreachable!("native compaction block was checked above"),
    }
    if let Some(id) = item.id_ref() {
        wire.insert("id".into(), Value::String(id.into()));
    }
    Some(Value::Object(wire))
}

fn insert_owned_item_metadata(
    encoded: &mut Value,
    item: &mut stravia_runtime_contract::protocol::ir::AiItem,
    status: bool,
) {
    if let Some(Value::Object(fields)) = item.meta.as_mut().and_then(|meta| {
        meta.remove_extension("__open_responses_item_fields")
            .ok()
            .flatten()
    }) {
        let object = encoded
            .as_object_mut()
            .expect("encoded Open Responses item is an object");
        for (field, value) in fields {
            object.entry(field).or_insert(value);
        }
    }
    insert_item_metadata(encoded, item, status);
}

pub(super) fn insert_item_metadata(
    encoded: &mut Value,
    item: &stravia_runtime_contract::protocol::ir::AiItem,
    status: bool,
) {
    let object = encoded
        .as_object_mut()
        .expect("encoded Open Responses item is an object");
    if let Some(fields) = item
        .meta
        .as_ref()
        .and_then(|meta| meta.get("__open_responses_item_fields"))
        .and_then(Value::as_object)
    {
        for (field, value) in fields {
            object.entry(field.clone()).or_insert_with(|| value.clone());
        }
    }
    if let Some(id) = item.id_ref() {
        object.insert("id".into(), Value::String(id.to_owned()));
    }
    if status && let Some(status) = item.status() {
        object.insert("status".into(), Value::String(status.as_str().to_owned()));
    }
}

fn insert_reasoning_metadata(
    encoded: &mut Value,
    item: &stravia_runtime_contract::protocol::ir::AiItem,
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

/// 编码原生 reasoning 输入：摘要和密文保留，正文另作 assistant output_text。
/// 密文绑定来源条目身份，因此回放时移除本地图 id。
fn encode_native_reasoning_item(
    item: &stravia_runtime_contract::protocol::ir::AiItem,
    summary: &[String],
    encrypted_content: Option<&str>,
) -> Value {
    let mut reasoning = serde_json::json!({
        "type": "reasoning",
        "summary": summary.iter().map(|text| serde_json::json!({
            "type": "summary_text",
            "text": text
        })).collect::<Vec<_>>(),
        "content": [],
    });
    insert_reasoning_metadata(&mut reasoning, item, encrypted_content.is_some());
    if let Some(encrypted_content) = encrypted_content {
        reasoning["encrypted_content"] = Value::String(encrypted_content.to_owned());
    }
    reasoning
}

/// 无原生载体的明文推理降级为 assistant message 的 output_text 部件：
/// 每段非空文本一个部件，保持在原位置顺序。
fn degraded_reasoning_parts<S: AsRef<str>>(texts: impl Iterator<Item = S>) -> Vec<Value> {
    texts
        .filter(|text| !text.as_ref().is_empty())
        .map(|text| serde_json::json!({"type": "output_text", "text": text.as_ref()}))
        .collect()
}

fn encode_mixed_assistant_items(
    item: &mut stravia_runtime_contract::protocol::ir::AiItem,
) -> Result<Option<Vec<Value>>> {
    let MessageContent::Blocks(blocks) = &item.content else {
        return Ok(None);
    };
    if item.role != Role::Assistant
        || !blocks.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Thinking { .. }
                    | ContentBlock::Reasoning { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::ToolUse { .. }
            )
        })
    {
        return Ok(None);
    }

    let represented_calls: Vec<_> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let MessageContent::Blocks(blocks) =
        std::mem::replace(&mut item.content, MessageContent::Blocks(Vec::new()))
    else {
        unreachable!("mixed assistant content was checked above");
    };
    let mut items = Vec::with_capacity(blocks.len());
    let mut message_content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } if text.is_empty() => {}
            ContentBlock::Text { .. } | ContentBlock::Refusal { .. } => {
                if let Some(encoded) = encode_responses_content_block(block, "output_text") {
                    message_content.push(encoded);
                }
            }
            ContentBlock::Thinking { thinking, .. } => {
                message_content
                    .extend(degraded_reasoning_parts(std::iter::once(thinking.as_str())));
            }
            ContentBlock::Reasoning {
                summary,
                content,
                encrypted_content,
            } => {
                let encrypted_content = encrypted_content
                    .as_deref()
                    .filter(|value| !value.is_empty());
                if encrypted_content.is_some() {
                    push_derived_assistant_message(&mut items, &mut message_content);
                    items.push(encode_native_reasoning_item(
                        item,
                        &summary,
                        encrypted_content,
                    ));
                    message_content.extend(degraded_reasoning_parts(content.iter()));
                } else {
                    message_content.extend(degraded_reasoning_parts(
                        summary.iter().chain(content.iter()),
                    ));
                }
            }
            // redacted 数据没有 Responses 输入载体，静默忽略，绝不落到可读字段。
            ContentBlock::RedactedThinking { .. } => {}
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                push_derived_assistant_message(&mut items, &mut message_content);
                // 混合条目拆出的调用不拥有父条目的原生身份，call_id 仍关联其结果。
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                }));
            }
            _ => {}
        }
    }
    push_derived_assistant_message(&mut items, &mut message_content);

    if let Some(tool_calls) = &item.tool_calls {
        for tool_call in tool_calls {
            let represented = represented_calls.contains(&tool_call.id);
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

// 派生载体不接收父条目，避免在密文剥离或混合拆分后继承错误的原生身份。
fn push_derived_assistant_message(items: &mut Vec<Value>, content: &mut Vec<Value>) {
    if content.is_empty() {
        return;
    }
    let mut message = serde_json::json!({
        "type": "message",
        "role": "assistant",
    });
    message["content"] = Value::Array(std::mem::take(content));
    items.push(message);
}

fn encode_message_content(content: MessageContent, role: Role) -> Result<Option<Vec<Value>>> {
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
                Ok(Some(vec![text_part(text_type, Arc::unwrap_or_clone(text))]))
            }
        }
        MessageContent::Blocks(blocks) => {
            let mut encoded = Vec::with_capacity(blocks.len());
            for block in blocks {
                if matches!(&block, ContentBlock::Text { text, .. } if text.is_empty()) {
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
                        continue;
                    }
                }
                if let Some(part) = encode_responses_content_block(block, text_type) {
                    encoded.push(part);
                }
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
    match object.get("type").and_then(Value::as_str) {
        Some("text" | "input_text") => object.get("text").is_some_and(Value::is_string),
        Some("input_image") => object.get("image_url").is_some_and(Value::is_string),
        Some("input_file") => ["file_data", "file_url"]
            .iter()
            .any(|field| object.get(*field).is_some_and(Value::is_string)),
        Some("image" | "file") => object.get("source").is_some_and(|source| {
            match source.get("type").and_then(Value::as_str) {
                Some("base64") => {
                    source.get("data").is_some_and(Value::is_string)
                        && source.get("media_type").is_some_and(Value::is_string)
                }
                Some("url") => source.get("url").is_some_and(Value::is_string),
                _ => false,
            }
        }),
        _ => false,
    }
}

fn normalize_tool_output_part(mut part: Value, response: bool) -> Option<Value> {
    let kind = part.get("type").and_then(Value::as_str)?;
    if matches!(kind, "text" | "image" | "file") {
        let block: ContentBlock = serde_json::from_value(part).ok()?;
        part = encode_responses_content_block(block, "input_text")?;
    }
    let object = part.as_object_mut()?;
    let fields: &[&str] = match object.get("type").and_then(Value::as_str)? {
        "input_text" => &["type", "text"],
        "input_image" => &["type", "image_url", "detail"],
        "input_file" if response => &["type", "filename", "file_url"],
        "input_file" => &["type", "filename", "file_data", "file_url"],
        _ => return None,
    };
    object.retain(|key, _| fields.contains(&key.as_str()));
    if object.get("detail").is_some_and(|detail| {
        !detail
            .as_str()
            .is_some_and(|value| matches!(value, "low" | "high" | "auto"))
    }) {
        object.remove("detail");
    }
    if object
        .get("filename")
        .is_some_and(|filename| !filename.is_string())
    {
        object.remove("filename");
    }
    if response && object.get("type").and_then(Value::as_str) == Some("input_image") {
        object
            .entry("detail")
            .or_insert_with(|| Value::String("auto".into()));
    }
    let representable = if response {
        response_tool_output_part
    } else {
        request_tool_output_part
    };
    representable(&part).then_some(part)
}

fn normalize_tool_result_content(
    content: Value,
    kind: Option<stravia_runtime_contract::protocol::ir::ToolResultContentKind>,
) -> Option<Value> {
    if kind != Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks)
        && !content.is_string()
    {
        return Some(Value::String(content.to_string()));
    }
    match content {
        Value::String(_) => Some(content),
        Value::Array(items) => {
            let parts: Vec<_> = items
                .into_iter()
                .filter_map(|part| normalize_tool_output_part(part, false))
                .collect();
            (!parts.is_empty()).then_some(Value::Array(parts))
        }
        _ => None,
    }
}

pub(crate) fn tool_output_representable(content: &MessageContent) -> bool {
    match content {
        MessageContent::Text(_) => true,
        MessageContent::Blocks(blocks) => blocks.iter().any(|block| match block {
            ContentBlock::Text { .. } => true,
            ContentBlock::ToolResult { content, content_kind, .. }
            | ContentBlock::ServerToolResult { content, content_kind, .. } => match content {
                _ if *content_kind != Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks) => true,
                Value::String(_) => true,
                Value::Array(items) => items.iter().any(request_tool_output_part),
                _ => false,
            },
            ContentBlock::Image { source, detail, .. } => {
                matches!(source, MediaSource::Base64 { .. } | MediaSource::Url(_))
                    && detail
                        .as_deref()
                        .is_none_or(|detail| matches!(detail, "low" | "high" | "auto"))
            }
            ContentBlock::File { source, .. } => {
                matches!(source, MediaSource::Base64 { .. } | MediaSource::Url(_))
            }
            _ => false,
        }),
    }
}

pub(crate) fn encode_tool_output(content: MessageContent) -> Option<Value> {
    match content {
        MessageContent::Text(text) => Some(Value::String(Arc::unwrap_or_clone(text))),
        MessageContent::Blocks(mut blocks) => {
            if let [ContentBlock::ToolResult { .. } | ContentBlock::ServerToolResult { .. }] =
                blocks.as_slice()
            {
                let (ContentBlock::ToolResult {
                    content,
                    content_kind,
                    ..
                }
                | ContentBlock::ServerToolResult {
                    content,
                    content_kind,
                    ..
                }) = blocks.remove(0)
                else {
                    unreachable!("single tool result checked above");
                };
                return normalize_tool_result_content(content, content_kind);
            }
            let mut output = Vec::with_capacity(blocks.len());
            for block in blocks {
                match block {
                    ContentBlock::ToolResult {
                        content,
                        content_kind,
                        ..
                    }
                    | ContentBlock::ServerToolResult {
                        content,
                        content_kind,
                        ..
                    } => match normalize_tool_result_content(content, content_kind) {
                        Some(Value::Array(items)) => output.extend(items),
                        Some(Value::String(text)) => {
                            output.push(text_part("input_text", text));
                        }
                        None => {}
                        _ => unreachable!("normalized tool output is string or array"),
                    },
                    _ => {
                        if let Some(part) = encode_responses_content_block(block, "input_text")
                            && request_tool_output_part(&part)
                        {
                            output.push(part);
                        }
                    }
                }
            }
            (!output.is_empty()).then_some(Value::Array(output))
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

pub(crate) fn encode_response_tool_output(content: &MessageContent) -> Option<Value> {
    match content {
        MessageContent::Text(text) => Some(Value::String(text.as_ref().clone())),
        MessageContent::Blocks(blocks) => {
            if let [
                ContentBlock::ToolResult {
                    content,
                    content_kind,
                    ..
                }
                | ContentBlock::ServerToolResult {
                    content,
                    content_kind,
                    ..
                },
            ] = blocks.as_slice()
            {
                return match content {
                    _ if *content_kind != Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks)
                        && !content.is_string() => Some(Value::String(content.to_string())),
                    Value::String(_) => Some(content.clone()),
                    Value::Array(items) => {
                        let parts: Vec<_> = items
                            .iter()
                            .filter_map(normalize_response_tool_output_part)
                            .collect();
                        (!parts.is_empty()).then_some(Value::Array(parts))
                    }
                    _ => None,
                };
            }
            let mut output = Vec::with_capacity(blocks.len());
            for block in blocks {
                match block {
                    ContentBlock::ToolResult { content, content_kind, .. }
                    | ContentBlock::ServerToolResult { content, content_kind, .. } => match content {
                        _ if *content_kind != Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks)
                            && !content.is_string() => output.push(text_part("input_text", content.to_string())),
                        Value::Array(items) => {
                            output.extend(
                                items
                                    .iter()
                                    .filter_map(normalize_response_tool_output_part),
                            );
                        }
                        Value::String(text) => output.push(text_part("input_text", text.clone())),
                        _ => {}
                    },
                    ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::File { .. } => {
                        let Some(mut encoded) =
                            encode_responses_content_block(block.clone(), "input_text")
                        else {
                            continue;
                        };
                        if encoded.get("type").and_then(Value::as_str) == Some("input_image")
                            && encoded.get("detail").is_none()
                        {
                            encoded["detail"] = Value::String("auto".into());
                        }
                        if response_tool_output_part(&encoded) {
                            output.push(encoded);
                        }
                    }
                    _ => {}
                }
            }
            (!output.is_empty()).then_some(Value::Array(output))
        }
    }
}

fn normalize_response_tool_output_part(part: &Value) -> Option<Value> {
    let object = part.as_object()?;
    let kind = object.get("type").and_then(Value::as_str)?;
    let fields: &[&str] = match kind {
        "text" | "input_text" => &["type", "text"],
        "input_image" => &["type", "image_url", "detail"],
        "input_file" => &["type", "filename", "file_url"],
        "image" | "file" => {
            let source = object.get("source")?;
            let (field, value) = match source.get("type").and_then(Value::as_str)? {
                "url" => (
                    if kind == "image" {
                        "image_url"
                    } else {
                        "file_url"
                    },
                    Value::String(source.get("url")?.as_str()?.to_owned()),
                ),
                "base64" if kind == "image" => (
                    "image_url",
                    Value::String(format!(
                        "data:{};base64,{}",
                        source.get("media_type")?.as_str()?,
                        source.get("data")?.as_str()?
                    )),
                ),
                _ => return None,
            };
            let mut normalized = serde_json::json!({"type": if kind == "image" { "input_image" } else { "input_file" }});
            normalized[field] = value;
            for key in ["detail", "filename"] {
                if let Some(value) = object.get(key) {
                    normalized[key] = value.clone();
                }
            }
            return normalize_tool_output_part(normalized, true);
        }
        _ => return None,
    };
    let normalized = Value::Object(
        object
            .iter()
            .filter(|(key, _)| fields.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    normalize_tool_output_part(normalized, true)
}

fn text_part(text_type: &str, text: String) -> Value {
    let mut part = serde_json::Map::new();
    part.insert("type".into(), Value::String(text_type.into()));
    part.insert("text".into(), Value::String(text));
    Value::Object(part)
}

fn encode_responses_content_block(block: ContentBlock, text_type: &str) -> Option<Value> {
    match block {
        ContentBlock::Document { source, title, .. } => {
            let source = crate::codec::document_media_source(source)?;
            let mut encoded = encode_responses_content_block(
                ContentBlock::File {
                    source,
                    media_type: None,
                },
                text_type,
            )?;
            if let Some(title) = title {
                encoded["filename"] = Value::String(title);
            }
            Some(encoded)
        }
        ContentBlock::Text { text, .. } => Some(text_part(text_type, Arc::unwrap_or_clone(text))),
        ContentBlock::Image { source, detail, .. } => {
            let mut encoded = serde_json::json!({"type": "input_image"});
            match source {
                MediaSource::Base64 { media_type, data } => {
                    encoded["image_url"] =
                        Value::String(crate::codec::media_data_url(&media_type, data));
                }
                MediaSource::Url(url) => {
                    encoded["image_url"] = Value::String(url);
                }
                MediaSource::FileId { file_id, detail } => {
                    encoded["file_id"] = Value::String(file_id);
                    if let Some(detail) = detail {
                        encoded["detail"] = Value::String(detail);
                    }
                }
            }
            if let Some(detail) = detail {
                encoded["detail"] = Value::String(detail);
            }
            Some(encoded)
        }
        ContentBlock::Refusal { refusal } if text_type == "output_text" => {
            let mut encoded = serde_json::json!({"type": "refusal"});
            encoded["refusal"] = Value::String(refusal);
            Some(encoded)
        }
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
                        .unwrap_or(&source_media_type);
                    encoded["file_data"] =
                        Value::String(crate::codec::media_data_url(media_type, data));
                }
                MediaSource::Url(url) => {
                    encoded["file_url"] = Value::String(url);
                }
                MediaSource::FileId { file_id, .. } => {
                    encoded["file_id"] = Value::String(file_id);
                }
            }
            Some(encoded)
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
                        .unwrap_or(&source_media_type);
                    crate::codec::media_data_url(media_type, data)
                }
                MediaSource::Url(url) => url,
                MediaSource::FileId { .. } => return None,
            };
            let mut encoded = serde_json::json!({"type": "input_video"});
            encoded["video_url"] = Value::String(video_url);
            Some(encoded)
        }
        _ => None,
    }
}

fn tool_choice_to_value(
    tc: &ToolChoice,
    native_tools: &[stravia_runtime_contract::protocol::ir::NativeTool],
) -> Value {
    use stravia_runtime_contract::protocol::ir::{NativeTool, NativeToolKind};
    let named = |name: &str| {
        let custom = native_tools
            .iter()
            .any(|tool| matches!(tool, NativeTool::Custom(tool) if tool.name == name));
        serde_json::json!({"type": if custom { "custom" } else { "function" }, "name": name})
    };
    match tc {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Named { name } => named(name),
        ToolChoice::Allowed { names, required } => serde_json::json!({
            "type": "allowed_tools",
            "mode": if *required { "required" } else { "auto" },
            "tools": names.iter().map(|name| named(name)).collect::<Vec<_>>(),
        }),
        ToolChoice::Native(kind) => serde_json::json!({
            "type": match kind {
                NativeToolKind::WebSearch => "web_search",
                NativeToolKind::CodeExecution => "code_interpreter",
                NativeToolKind::ImageGeneration => "image_generation",
                NativeToolKind::ToolSearch => "tool_search",
            }
        }),
    }
}

#[cfg(test)]
mod tests;
