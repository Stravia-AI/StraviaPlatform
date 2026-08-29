use std::collections::HashMap;

use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::ir::AiRequest;
use crate::protocol::ir::request::{
    AiItem, ContentBlock, MediaSource, MessageContent, ResponseFormat, Role, ToolChoice,
};

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
            .flat_map(|message| message.tool_calls.iter().flatten())
            .map(|call| (call.id.as_str(), call.name.as_str()))
            .collect::<HashMap<_, _>>();

        let mut contents: Vec<Value> = Vec::new();
        for msg in &req.items {
            if matches!(msg.role, Role::System | Role::Developer) {
                continue;
            }
            contents.push(encode_content(msg, &call_names)?);
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
                crate::thinking::TargetThinkingControl::Budget { value } => {
                    serde_json::json!({"thinkingBudget": value})
                }
                crate::thinking::TargetThinkingControl::Enabled => {
                    serde_json::json!({"includeThoughts": true})
                }
                crate::thinking::TargetThinkingControl::Disabled => {
                    serde_json::json!({"thinkingBudget": 0})
                }
                crate::thinking::TargetThinkingControl::Effort { value } => {
                    serde_json::json!({"thinkingLevel": value.to_ascii_uppercase()})
                }
                crate::thinking::TargetThinkingControl::Hidden => anyhow::bail!(
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

    let parts = match &msg.content {
        MessageContent::Text(t) => {
            if let Some(call_id) = msg.tool_call_id.as_deref() {
                let name = call_names.get(call_id).ok_or_else(|| {
                    anyhow::anyhow!("Gemini tool result references unknown call_id")
                })?;
                vec![serde_json::json!({
                    "functionResponse": {
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
                    let args: Value = serde_json::from_str(&tc.arguments)
                        .unwrap_or(Value::Object(Default::default()));
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
            let parts = blocks
                .iter()
                .filter(|block| !matches!(block, ContentBlock::Text { .. }))
                .map(|block| encode_content_block_for_gemini(block, call_names))
                .collect::<Vec<_>>();
            if !parts.is_empty() {
                function_response["parts"] = Value::Array(parts);
            }
            vec![serde_json::json!({"functionResponse": function_response})]
        }
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| encode_content_block_for_gemini(block, call_names))
            .collect(),
    };

    Ok(serde_json::json!({"role": role, "parts": parts}))
}

fn encode_content_block_for_gemini(b: &ContentBlock, call_names: &HashMap<&str, &str>) -> Value {
    match b {
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
        ContentBlock::Unknown { raw } => raw.clone(),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
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
mod tests {
    use super::*;
    #[test]
    fn rejects_unrepresentable_named_tool_choice() {
        let mut request = AiRequest::new(
            "model",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.tool_choice = Some(ToolChoice::Named {
            name: "lookup".into(),
        });

        let error = GoogleEncoder
            .encode_request(&request)
            .expect_err("Gemini encoder cannot silently drop named tool choice");
        assert!(error.to_string().contains("tool_choice"));
    }
    #[test]
    fn encodes_json_schema_response_format_in_generation_config() {
        let mut request = AiRequest::new(
            "model",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.response_format = Some(ResponseFormat::JsonSchema {
            name: "answer".into(),
            strict: Some(true),
            schema: serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"answer": {"type": "string"}}
            }),
        });

        let (body, _) = GoogleEncoder
            .encode_request(&request)
            .expect("Gemini supports structured JSON output");

        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            body["generationConfig"]["responseSchema"]["properties"]["answer"]["type"],
            "string"
        );
        assert!(
            body["generationConfig"]["responseSchema"]
                .get("$schema")
                .is_none()
        );
    }

    #[test]
    fn target_controls_replace_raw_gemini_thinking_config() {
        let mut request = AiRequest::new(
            "model",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.reasoning.target_control = Some(crate::thinking::TargetThinkingControl::Effort {
            value: "high".into(),
        });
        request.meta.vendor.ingress.insert(
            "__google_generation_config".into(),
            serde_json::json!({"thinkingConfig": {"thinkingBudget": 12}}),
        );

        let (body, _) = GoogleEncoder
            .encode_request(&request)
            .expect("Gemini Thinking Level");
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            serde_json::json!({"thinkingLevel": "HIGH"})
        );
    }
}
