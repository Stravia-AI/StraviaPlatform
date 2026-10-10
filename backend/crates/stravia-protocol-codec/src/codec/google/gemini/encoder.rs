use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use http::header::HeaderMap;
use serde_json::Value;

use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::request::AiItem;
use stravia_runtime_contract::protocol::ir::request::ContentBlock;
use stravia_runtime_contract::protocol::ir::request::MediaSource;
use stravia_runtime_contract::protocol::ir::request::MessageContent;
use stravia_runtime_contract::protocol::ir::request::ResponseFormat;
use stravia_runtime_contract::protocol::ir::request::Role;
use stravia_runtime_contract::protocol::ir::request::ToolChoice;

pub struct GoogleEncoder;

impl GoogleEncoder {
    pub(crate) fn encode_request(&self, mut req: AiRequest) -> Result<(Value, HeaderMap)> {
        let ingress = &mut req.meta.vendor.ingress;
        if req
            .tool_choice
            .as_ref()
            .is_some_and(|choice| !matches!(choice, ToolChoice::Auto))
        {
            anyhow::bail!("unsupported tool_choice for Google Gemini");
        }

        // ── System instruction ────────────────────────────────────────────────
        let raw_system = ingress.remove("__google_raw_system_instruction");
        let mut system_parts = Vec::new();
        if raw_system.is_none()
            && let Some(text) = req.instructions.take()
        {
            system_parts.push(object([("text", Value::String(text))]));
        }

        // ── Contents ─────────────────────────────────────────────────────────
        let call_names = req
            .items
            .iter()
            .flat_map(|message| {
                let block_calls = match &message.content {
                    MessageContent::Blocks(blocks) => blocks.as_slice(),
                    MessageContent::Text(_) => &[],
                }
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, .. } => {
                        Some((id.as_str().to_owned(), name.clone()))
                    }
                    _ => None,
                });
                block_calls.chain(
                    message
                        .tool_calls
                        .iter()
                        .flatten()
                        .map(|call| (call.id.as_str().to_owned(), call.name.clone())),
                )
            })
            .collect::<HashMap<_, _>>();

        let mut contents: Vec<Value> = Vec::new();
        let mut previous_was_assistant = false;
        for msg in req.items {
            if matches!(msg.role, Role::System | Role::Developer) {
                if raw_system.is_none() {
                    system_parts.push(object([(
                        "text",
                        Value::String(content_into_text(msg.content)),
                    )]));
                }
                previous_was_assistant = false;
                continue;
            }
            let is_assistant = msg.role == Role::Assistant;
            let mut content = encode_content(msg, &call_names)?;
            if previous_was_assistant
                && let Some(previous) = contents.last_mut()
                && previous["role"] == "model"
                && content["role"] == "model"
                && let Some(parts) = previous["parts"].as_array_mut()
                && let Some(next_parts) = content["parts"].as_array_mut()
                && let (Some(last), Some(first)) = (parts.last_mut(), next_parts.first_mut())
                && attach_call_signature(last, first)
            {
                parts.pop();
                if parts.is_empty() {
                    contents.pop();
                }
            }
            // 乐观回放可能让条目只剩 Gemini 承载不了的受保护载荷（如 redacted）：
            // 编码后没有 part 的 model 条目整条跳过，不发出空 content。
            if is_assistant && content["parts"].as_array().is_some_and(Vec::is_empty) {
                previous_was_assistant = false;
                continue;
            }
            previous_was_assistant = is_assistant;
            contents.push(content);
        }

        let system_val = raw_system.or_else(|| {
            (!system_parts.is_empty()).then(|| object([("parts", Value::Array(system_parts))]))
        });
        let mut body = object([("contents", Value::Array(contents))]);
        let obj = body.as_object_mut().unwrap();

        if let Some(sv) = system_val {
            obj.insert("systemInstruction".into(), sv);
        }

        // ── generationConfig ──────────────────────────────────────────────────
        let mut gen_config: serde_json::Map<String, Value> =
            if let Some(Value::Object(m)) = ingress.remove("__google_generation_config") {
                m
            } else {
                serde_json::Map::new()
            };
        gen_config.remove("thinkingConfig");

        if let Some(t) = req.generation.temperature {
            gen_config.insert("temperature".into(), t.into());
        }
        if let Some(m) = req.generation.max_tokens {
            gen_config.insert("maxOutputTokens".into(), m.into());
        }
        if let Some(p) = req.generation.top_p {
            gen_config.insert("topP".into(), p.into());
        }
        match req.response_format {
            Some(ResponseFormat::JsonObject) => {
                gen_config.insert(
                    "responseMimeType".into(),
                    Value::String("application/json".into()),
                );
            }
            Some(ResponseFormat::JsonSchema { schema, .. }) => {
                gen_config.insert(
                    "responseMimeType".into(),
                    Value::String("application/json".into()),
                );
                gen_config.insert(
                    "responseSchema".into(),
                    sanitize_owned_gemini_schema(schema),
                );
            }
            Some(ResponseFormat::Text) | None => {}
        }
        let mut thinking_config = serde_json::json!({});
        if let Some(control) = req.reasoning.target_control.as_ref() {
            thinking_config = match control {
                stravia_runtime_contract::thinking::TargetThinkingControl::Budget { value } => {
                    serde_json::json!({"thinkingBudget": value})
                }
                stravia_runtime_contract::thinking::TargetThinkingControl::Enabled => {
                    serde_json::json!({})
                }
                stravia_runtime_contract::thinking::TargetThinkingControl::Disabled => {
                    serde_json::json!({"thinkingBudget": 0})
                }
                stravia_runtime_contract::thinking::TargetThinkingControl::Effort { value } => {
                    serde_json::json!({"thinkingLevel": value.to_ascii_uppercase()})
                }
                stravia_runtime_contract::thinking::TargetThinkingControl::Hidden => anyhow::bail!(
                    "Google Gemini cannot represent Target Thinking Control {control:?}"
                ),
            };
        }
        let summary_disabled = matches!(
            req.reasoning.display.as_deref(),
            Some("omitted" | "none" | "disabled" | "hidden")
        ) || req.reasoning.level
            == Some(stravia_runtime_contract::thinking::ThinkingLevel::Off)
            || matches!(
                req.reasoning.target_control,
                Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled)
            );
        thinking_config["includeThoughts"] = Value::Bool(!summary_disabled);
        gen_config.insert("thinkingConfig".into(), thinking_config);

        if !gen_config.is_empty() {
            obj.insert("generationConfig".into(), Value::Object(gen_config));
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        if let Some(raw) = ingress.remove("__google_raw_tools") {
            obj.insert("tools".into(), raw);
        } else if let Some(tools) = req.tools {
            let mut fn_decls: Vec<Value> = Vec::new();
            let mut builtin_entries: Vec<Value> = Vec::new();

            for t in tools {
                match t.name.as_str() {
                    "__builtin__google_search" => {
                        builtin_entries.push(serde_json::json!({"googleSearch": {}}));
                    }
                    "__builtin__code_execution" => {
                        builtin_entries.push(serde_json::json!({"codeExecution": {}}));
                    }
                    "__builtin__google_search_retrieval" => {
                        builtin_entries.push(serde_json::json!({"googleSearchRetrieval": {}}));
                    }
                    _ => {
                        let mut decl = object([("name", Value::String(t.name))]);
                        let d = decl.as_object_mut().unwrap();
                        if let Some(desc) = t.description {
                            d.insert("description".into(), Value::String(desc));
                        }
                        d.insert(
                            "parameters".into(),
                            sanitize_owned_gemini_schema(t.parameters),
                        );
                        fn_decls.push(decl);
                    }
                }
            }

            let mut tool_array: Vec<Value> = Vec::new();
            if !fn_decls.is_empty() {
                tool_array.push(object([("functionDeclarations", Value::Array(fn_decls))]));
            }
            tool_array.extend(builtin_entries);

            if !tool_array.is_empty() {
                obj.insert("tools".into(), Value::Array(tool_array));
            }
        }

        // ── Extra passthrough fields ───────────────────────────────────────────
        if let Some(v) = ingress.remove("__google_tool_config") {
            obj.insert("toolConfig".into(), v);
        }
        if let Some(v) = ingress.remove("__google_safety_settings") {
            obj.insert("safetySettings".into(), v);
        }
        if let Some(v) = ingress.remove("__google_cached_content") {
            obj.insert("cachedContent".into(), v);
        }

        Ok((body, HeaderMap::new()))
    }

    pub(crate) fn egress_path(&self, model: &str, stream: bool) -> String {
        if stream {
            format!("/v1beta/models/{}:streamGenerateContent?alt=sse", model)
        } else {
            format!("/v1beta/models/{}:generateContent", model)
        }
    }
}

// ── Schema sanitisation ───────────────────────────────────────────────────────

fn sanitize_gemini_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "$schema" | "additionalProperties" | "$ref" | "ref" | "definitions" | "$defs"
                ) {
                    continue;
                }
                out.insert(k.clone(), sanitize_gemini_schema(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_gemini_schema).collect()),
        _ => value.clone(),
    }
}

pub(crate) fn schema_is_losslessly_representable(schema: &Value) -> bool {
    sanitize_gemini_schema(schema) == *schema
}

fn sanitize_owned_gemini_schema(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "$schema"
                            | "additionalProperties"
                            | "$ref"
                            | "ref"
                            | "definitions"
                            | "$defs"
                    )
                })
                .map(|(key, value)| (key, sanitize_owned_gemini_schema(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(sanitize_owned_gemini_schema)
                .collect(),
        ),
        other => other,
    }
}

// ── Content encoding ──────────────────────────────────────────────────────────

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn content_into_text(content: MessageContent) -> String {
    match content {
        MessageContent::Text(text) => Arc::unwrap_or_clone(text),
        MessageContent::Blocks(blocks) => {
            let mut texts = blocks.into_iter().filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(Arc::unwrap_or_clone(text)),
                _ => None,
            });
            let mut text = texts.next().unwrap_or_default();
            for next in texts {
                text.push_str(&next);
            }
            text
        }
    }
}

fn function_call(id: String, name: String, args: Value) -> Value {
    object([(
        "functionCall",
        object([
            ("id", Value::String(id)),
            ("name", Value::String(name)),
            ("args", args),
        ]),
    )])
}

fn encode_content(msg: AiItem, call_names: &HashMap<String, String>) -> Result<Value> {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "model",
        Role::System | Role::Developer => unreachable!("instruction roles handled separately"),
    };

    let block_call_ids = match &msg.content {
        MessageContent::Blocks(blocks)
            if msg.tool_call_id.is_none() && msg.tool_calls.is_some() =>
        {
            Some(
                blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, .. } => Some(id.as_str().to_owned()),
                        _ => None,
                    })
                    .collect::<std::collections::HashSet<_>>(),
            )
        }
        MessageContent::Text(_) | MessageContent::Blocks(_) => None,
    };
    let mut tool_calls = msg.tool_calls;
    let mut parts = match msg.content {
        MessageContent::Text(t) => {
            if let Some(call_id) = msg.tool_call_id.as_deref() {
                let name = call_names.get(call_id).ok_or_else(|| {
                    anyhow::anyhow!("Gemini tool result references unknown call_id")
                })?;
                vec![object([(
                    "functionResponse",
                    object([
                        ("id", Value::String(call_id.to_owned())),
                        ("name", Value::String(name.clone())),
                        (
                            "response",
                            object([("result", Value::String(Arc::unwrap_or_clone(t)))]),
                        ),
                    ]),
                )])]
            } else if let Some(tcs) = tool_calls.take() {
                let mut parts = Vec::new();
                if !t.is_empty() {
                    parts.push(object([("text", Value::String(Arc::unwrap_or_clone(t)))]));
                }
                for tc in tcs {
                    let args: Value = serde_json::from_str(&tc.arguments).map_err(|error| {
                        anyhow::anyhow!(
                            "gemini functionCall args cannot represent arguments for tool call {}: {error}",
                            tc.id
                        )
                    })?;
                    parts.push(function_call(tc.id.into_string(), tc.name, args));
                }
                parts
            } else {
                vec![object([("text", Value::String(Arc::unwrap_or_clone(t)))])]
            }
        }
        MessageContent::Blocks(blocks) if msg.tool_call_id.is_some() => {
            let call_id = msg.tool_call_id.as_deref().expect("checked tool_call_id");
            let name = call_names
                .get(call_id)
                .ok_or_else(|| anyhow::anyhow!("Gemini tool result references unknown call_id"))?;
            if blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                // 原生工具结果已携带 JSON 响应，不能再作为媒体嵌套进空文本响应。
                let mut parts = Vec::new();
                for block in blocks {
                    append_content_parts_for_gemini(&mut parts, block, call_names);
                }
                parts
            } else {
                let mut text = String::new();
                let mut parts = Vec::new();
                for block in blocks {
                    if let ContentBlock::Text { text: next, .. } = block {
                        let next = Arc::unwrap_or_clone(next);
                        if text.is_empty() {
                            text = next;
                        } else {
                            text.push_str(&next);
                        }
                    } else {
                        append_content_parts_for_gemini(&mut parts, block, call_names);
                    }
                }
                let mut function_response = object([
                    ("id", Value::String(call_id.to_owned())),
                    ("name", Value::String(name.clone())),
                    ("response", object([("result", Value::String(text))])),
                ]);
                if !parts.is_empty() {
                    function_response["parts"] = Value::Array(parts);
                }
                vec![object([("functionResponse", function_response)])]
            }
        }
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                append_content_parts_for_gemini(&mut parts, block, call_names);
            }
            parts
        }
    };

    if let Some(block_call_ids) = block_call_ids
        && msg.tool_call_id.is_none()
        && let Some(calls) = tool_calls
    {
        for call in calls {
            if block_call_ids.contains(call.id.as_str()) {
                continue;
            }
            let args: Value = serde_json::from_str(&call.arguments).map_err(|error| {
                anyhow::anyhow!(
                    "gemini functionCall args cannot represent arguments for tool call {}: {error}",
                    call.id
                )
            })?;
            parts.push(function_call(call.id.into_string(), call.name, args));
        }
    }
    pair_call_signatures(&mut parts);
    Ok(object([
        ("role", Value::String(role.into())),
        ("parts", Value::Array(parts)),
    ]))
}

pub(super) fn attach_call_signature(carrier: &mut Value, call: &mut Value) -> bool {
    if carrier["text"].as_str() != Some("")
        || carrier["thought"] != true
        || carrier["thoughtSignature"].as_str().is_none()
        || call.get("functionCall").is_none()
        || call.get("thoughtSignature").is_some()
    {
        return false;
    }
    let signature = carrier
        .as_object_mut()
        .expect("thought part is an object")
        .remove("thoughtSignature")
        .expect("checked signature");
    call["thoughtSignature"] = signature;
    true
}

pub(super) fn pair_call_signatures(parts: &mut Vec<Value>) {
    let mut index = 0;
    while index + 1 < parts.len() {
        let (before, after) = parts.split_at_mut(index + 1);
        if attach_call_signature(&mut before[index], &mut after[0]) {
            parts.remove(index);
        } else {
            index += 1;
        }
    }
}

fn append_content_parts_for_gemini(
    parts: &mut Vec<Value>,
    block: ContentBlock,
    call_names: &HashMap<String, String>,
) {
    if let ContentBlock::Reasoning {
        summary, content, ..
    } = block
    {
        parts.extend(
            summary
                .into_iter()
                .chain(content)
                .filter(|text| !text.is_empty())
                .map(|text| {
                    object([
                        ("text", Value::String(text)),
                        ("thought", Value::Bool(true)),
                    ])
                }),
        );
    } else if let Some(part) = encode_content_block_for_gemini(block, call_names) {
        parts.push(part);
    }
}

/// 返回 `None` 表示该块在 Gemini 上没有可承载的载体（如 redacted 数据），整块忽略。
pub(super) fn encode_content_block_for_gemini(
    b: ContentBlock,
    call_names: &HashMap<String, String>,
) -> Option<Value> {
    Some(match b {
        ContentBlock::Text { text, .. } => {
            object([("text", Value::String(Arc::unwrap_or_clone(text)))])
        }
        ContentBlock::Image { source, .. } => encode_media_source(source, None),
        ContentBlock::Audio { source } => encode_media_source(source, None),
        ContentBlock::File { source, media_type } | ContentBlock::Video { source, media_type } => {
            encode_media_source(source, media_type)
        }
        ContentBlock::ToolUse {
            id, name, input, ..
        } => function_call(id.into_string(), name, input),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let name = call_names
                .get(tool_use_id.as_str())
                .cloned()
                .unwrap_or_else(|| tool_use_id.as_str().to_owned());
            object([(
                "functionResponse",
                object([
                    ("id", Value::String(tool_use_id.into_string())),
                    ("name", Value::String(name)),
                    (
                        "response",
                        if content.is_object() {
                            content
                        } else {
                            object([("result", content)])
                        },
                    ),
                ]),
            )])
        }
        // Gemini 接受无 thoughtSignature 的 thought part（可能只是忽略它），所以
        // 无签名明文保持原生 thought part，不降级为正文：降级会让模型把自己的
        // 推理当成已经说出口的话。
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            let mut part = object([
                ("text", Value::String(thinking)),
                ("thought", Value::Bool(true)),
            ]);
            if let Some(signature) = signature {
                part["thoughtSignature"] = Value::String(signature);
            }
            part
        }
        // Request reasoning expands into paragraph parts before this single-part helper.
        ContentBlock::Reasoning { .. } => return None,
        // redacted 数据没有 Gemini 原生载体，静默忽略，绝不能落到可读文本里。
        ContentBlock::RedactedThinking { .. } => return None,
        ContentBlock::Unknown { raw } => raw,
        other => crate::codec::content_block_wire_value(&other),
    })
}

fn encode_media_source(source: MediaSource, media_type: Option<String>) -> Value {
    match source {
        MediaSource::Url(url) => {
            let mut part = object([("fileData", object([("fileUri", Value::String(url))]))]);
            if let Some(media_type) = media_type {
                part["fileData"]["mimeType"] = Value::String(media_type);
            }
            part
        }
        MediaSource::FileId { file_id, .. } => {
            let mut part = object([("fileData", object([("fileUri", Value::String(file_id))]))]);
            if let Some(media_type) = media_type {
                part["fileData"]["mimeType"] = Value::String(media_type);
            }
            part
        }
        MediaSource::Base64 {
            media_type: source_media_type,
            data,
        } => object([(
            "inlineData",
            object([
                (
                    "mimeType",
                    Value::String(media_type.unwrap_or(source_media_type)),
                ),
                ("data", Value::String(data)),
            ]),
        )]),
    }
}

#[cfg(test)]
mod tests;
