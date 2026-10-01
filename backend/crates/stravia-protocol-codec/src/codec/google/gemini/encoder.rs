use std::collections::HashMap;

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
    pub(crate) fn encode_request(&self, req: &AiRequest) -> Result<(Value, HeaderMap)> {
        let ingress = &req.meta.vendor.ingress;
        if req
            .tool_choice
            .as_ref()
            .is_some_and(|choice| !matches!(choice, ToolChoice::Auto))
        {
            anyhow::bail!("unsupported tool_choice for Google Gemini");
        }

        // ── System instruction ────────────────────────────────────────────────
        let system_val: Option<Value> =
            if let Some(v) = ingress.get("__google_raw_system_instruction") {
                Some(v.clone())
            } else {
                let mut system_parts: Vec<Value> = req
                    .instructions
                    .iter()
                    .map(|text| serde_json::json!({"text": text}))
                    .collect();
                for msg in &req.items {
                    if matches!(msg.role, Role::System | Role::Developer) {
                        system_parts.push(serde_json::json!({"text": msg.content.to_text()}));
                    }
                }
                if system_parts.is_empty() {
                    None
                } else {
                    Some(serde_json::json!({"parts": system_parts}))
                }
            };

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
                    ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
                    _ => None,
                });
                block_calls.chain(
                    message
                        .tool_calls
                        .iter()
                        .flatten()
                        .map(|call| (call.id.as_str(), call.name.as_str())),
                )
            })
            .collect::<HashMap<_, _>>();

        let mut contents: Vec<Value> = Vec::new();
        let mut previous_was_assistant = false;
        for msg in &req.items {
            if matches!(msg.role, Role::System | Role::Developer) {
                previous_was_assistant = false;
                continue;
            }
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
            if msg.role == Role::Assistant && content["parts"].as_array().is_some_and(Vec::is_empty)
            {
                previous_was_assistant = false;
                continue;
            }
            previous_was_assistant = msg.role == Role::Assistant;
            contents.push(content);
        }

        let mut body = serde_json::json!({ "contents": contents });
        let obj = body.as_object_mut().unwrap();

        if let Some(sv) = system_val {
            obj.insert("systemInstruction".into(), sv);
        }

        // ── generationConfig ──────────────────────────────────────────────────
        let mut gen_config: serde_json::Map<String, Value> =
            if let Some(Value::Object(m)) = ingress.get("__google_generation_config") {
                m.clone()
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
        match req.response_format.as_ref() {
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
                gen_config.insert("responseSchema".into(), sanitize_gemini_schema(schema));
            }
            Some(ResponseFormat::Text) | None => {}
        }
        if let Some(control) = req.reasoning.target_control.as_ref() {
            let thinking_config = match control {
                stravia_runtime_contract::thinking::TargetThinkingControl::Budget { value } => {
                    serde_json::json!({"thinkingBudget": value})
                }
                stravia_runtime_contract::thinking::TargetThinkingControl::Enabled => {
                    serde_json::json!({"includeThoughts": true})
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
            gen_config.insert("thinkingConfig".into(), thinking_config);
        }

        if !gen_config.is_empty() {
            obj.insert("generationConfig".into(), Value::Object(gen_config));
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        if let Some(raw) = ingress.get("__google_raw_tools") {
            obj.insert("tools".into(), raw.clone());
        } else if let Some(ref tools) = req.tools {
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
                        let mut decl = serde_json::json!({"name": t.name});
                        let d = decl.as_object_mut().unwrap();
                        if let Some(ref desc) = t.description {
                            d.insert("description".into(), Value::String(desc.clone()));
                        }
                        d.insert("parameters".into(), sanitize_gemini_schema(&t.parameters));
                        fn_decls.push(decl);
                    }
                }
            }

            let mut tool_array: Vec<Value> = Vec::new();
            if !fn_decls.is_empty() {
                tool_array.push(serde_json::json!({"functionDeclarations": fn_decls}));
            }
            tool_array.extend(builtin_entries);

            if !tool_array.is_empty() {
                obj.insert("tools".into(), Value::Array(tool_array));
            }
        }

        // ── Extra passthrough fields ───────────────────────────────────────────
        if let Some(v) = ingress.get("__google_tool_config") {
            obj.insert("toolConfig".into(), v.clone());
        }
        if let Some(v) = ingress.get("__google_safety_settings") {
            obj.insert("safetySettings".into(), v.clone());
        }
        if let Some(v) = ingress.get("__google_cached_content") {
            obj.insert("cachedContent".into(), v.clone());
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

// ── Content encoding ──────────────────────────────────────────────────────────

fn encode_content(msg: &AiItem, call_names: &HashMap<&str, &str>) -> Result<Value> {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "model",
        Role::System | Role::Developer => unreachable!("instruction roles handled separately"),
    };

    let mut parts = match &msg.content {
        MessageContent::Text(t) => {
            if let Some(call_id) = msg.tool_call_id.as_deref() {
                let name = call_names.get(call_id).ok_or_else(|| {
                    anyhow::anyhow!("Gemini tool result references unknown call_id")
                })?;
                vec![serde_json::json!({
                    "functionResponse": {
                        "id": call_id,
                        "name": name,
                        "response": {"result": t}
                    }
                })]
            } else if let Some(ref tcs) = msg.tool_calls {
                let mut parts = Vec::new();
                if !t.is_empty() {
                    parts.push(serde_json::json!({"text": t}));
                }
                for tc in tcs {
                    let args: Value = serde_json::from_str(&tc.arguments).map_err(|error| {
                        anyhow::anyhow!(
                            "gemini functionCall args cannot represent arguments for tool call {}: {error}",
                            tc.id
                        )
                    })?;
                    parts.push(serde_json::json!({"functionCall": {
                        "id": tc.id,
                        "name": tc.name,
                        "args": args
                    }}));
                }
                parts
            } else {
                vec![serde_json::json!({"text": t})]
            }
        }
        MessageContent::Blocks(blocks) if msg.tool_call_id.is_some() => {
            let call_id = msg.tool_call_id.as_deref().expect("checked tool_call_id");
            let name = call_names
                .get(call_id)
                .ok_or_else(|| anyhow::anyhow!("Gemini tool result references unknown call_id"))?;
            let mut function_response = serde_json::json!({
                "id": call_id,
                "name": name,
                "response": {"result": msg.content.to_text()}
            });
            let mut parts = Vec::new();
            for block in blocks
                .iter()
                .filter(|block| !matches!(block, ContentBlock::Text { .. }))
            {
                append_content_parts_for_gemini(&mut parts, block, call_names);
            }
            if !parts.is_empty() {
                function_response["parts"] = Value::Array(parts);
            }
            vec![serde_json::json!({"functionResponse": function_response})]
        }
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                append_content_parts_for_gemini(&mut parts, block, call_names);
            }
            parts
        }
    };

    if let MessageContent::Blocks(blocks) = &msg.content
        && msg.tool_call_id.is_none()
        && let Some(calls) = &msg.tool_calls
    {
        for call in calls {
            if blocks.iter().any(|block| {
                matches!(block,
                ContentBlock::ToolUse { id, .. } if id == &call.id)
            }) {
                continue;
            }
            let args: Value = serde_json::from_str(&call.arguments).map_err(|error| {
                anyhow::anyhow!(
                    "gemini functionCall args cannot represent arguments for tool call {}: {error}",
                    call.id
                )
            })?;
            parts.push(serde_json::json!({"functionCall": {
                "id": call.id, "name": call.name, "args": args
            }}));
        }
    }
    pair_call_signatures(&mut parts);
    Ok(serde_json::json!({"role": role, "parts": parts}))
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
    block: &ContentBlock,
    call_names: &HashMap<&str, &str>,
) {
    if let ContentBlock::Reasoning {
        summary, content, ..
    } = block
    {
        parts.extend(
            summary
                .iter()
                .chain(content)
                .filter(|text| !text.is_empty())
                .map(|text| serde_json::json!({"text": text, "thought": true})),
        );
    } else if let Some(part) = encode_content_block_for_gemini(block, call_names) {
        parts.push(part);
    }
}

/// 返回 `None` 表示该块在 Gemini 上没有可承载的载体（如 redacted 数据），整块忽略。
pub(super) fn encode_content_block_for_gemini(
    b: &ContentBlock,
    call_names: &HashMap<&str, &str>,
) -> Option<Value> {
    Some(match b {
        ContentBlock::Text { text, .. } => serde_json::json!({"text": text}),
        ContentBlock::Image { source, .. } => match source {
            MediaSource::Base64 { media_type, data } => serde_json::json!({
                "inlineData": {
                    "mimeType": media_type,
                    "data": data,
                }
            }),
            MediaSource::Url(url) => serde_json::json!({"fileData": {"fileUri": url}}),
            MediaSource::FileId { file_id, .. } => {
                serde_json::json!({"fileData": {"fileUri": file_id}})
            }
        },
        ContentBlock::Audio { source } => encode_media_source(source, None),
        ContentBlock::File { source, media_type } | ContentBlock::Video { source, media_type } => {
            encode_media_source(source, media_type.as_deref())
        }
        ContentBlock::ToolUse {
            id, name, input, ..
        } => {
            serde_json::json!({"functionCall": {"id": id, "name": name, "args": input}})
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let name = call_names
                .get(tool_use_id.as_str())
                .copied()
                .unwrap_or(tool_use_id);
            serde_json::json!({
                "functionResponse": {
                    "id": tool_use_id,
                    "name": name,
                    "response": content
                }
            })
        }
        // Gemini 接受无 thoughtSignature 的 thought part（可能只是忽略它），所以
        // 无签名明文保持原生 thought part，不降级为正文：降级会让模型把自己的
        // 推理当成已经说出口的话。
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            let mut part = serde_json::json!({"text": thinking, "thought": true});
            if let Some(signature) = signature {
                part["thoughtSignature"] = Value::String(signature.clone());
            }
            part
        }
        // Request reasoning expands into paragraph parts before this single-part helper.
        ContentBlock::Reasoning { .. } => return None,
        // redacted 数据没有 Gemini 原生载体，静默忽略，绝不能落到可读文本里。
        ContentBlock::RedactedThinking { .. } => return None,
        ContentBlock::Unknown { raw } => raw.clone(),
        other => crate::codec::content_block_wire_value(other),
    })
}

fn encode_media_source(source: &MediaSource, media_type: Option<&str>) -> Value {
    match source {
        MediaSource::Url(url) => {
            let mut part = serde_json::json!({"fileData": {"fileUri": url}});
            if let Some(media_type) = media_type {
                part["fileData"]["mimeType"] = Value::String(media_type.to_owned());
            }
            part
        }
        MediaSource::FileId { file_id, .. } => {
            let mut part = serde_json::json!({"fileData": {"fileUri": file_id}});
            if let Some(media_type) = media_type {
                part["fileData"]["mimeType"] = Value::String(media_type.to_owned());
            }
            part
        }
        MediaSource::Base64 {
            media_type: source_media_type,
            data,
        } => serde_json::json!({
            "inlineData": {
                "mimeType": media_type.unwrap_or(source_media_type),
                "data": data,
            }
        }),
    }
}

#[cfg(test)]
mod tests;
