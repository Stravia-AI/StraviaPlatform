//! Google Generative AI ingress decoder — produces `AiRequest` directly.
//!
//! `decode_with_model` accepts the model and stream flag extracted from the URL
//! path by the ingress shell handler, since Google embeds the model in the path
//! rather than the request body.

use anyhow::Result;
use serde_json::Value;

use crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use crate::protocol::ir::{
    AiItem, AiRequest, ContentBlock, GenerationConfig, GoogleExt, MediaSource, MessageContent,
    ProtocolExt, ReasoningConfig, Role, SafetySettings, StreamConfig, ToolCall, ToolChoice,
    ToolSpec,
};

use super::types::*;

pub struct GoogleDecoder;

fn is_plain_auto_tool_config(value: &Value) -> bool {
    let Some(config) = value.as_object() else {
        return false;
    };
    let Some(function_calling) = config
        .get("functionCallingConfig")
        .and_then(Value::as_object)
    else {
        return false;
    };

    config.len() == 1
        && function_calling.len() == 1
        && function_calling.get("mode").and_then(Value::as_str) == Some("AUTO")
}

impl GoogleDecoder {
    pub fn decode_with_model(&self, body: Value, model: &str, stream: bool) -> Result<AiRequest> {
        let req: GoogleRequest = serde_json::from_value(body)?;

        // ── System instruction ────────────────────────────────────────────────
        let needs_raw_system = req.system_instruction.as_ref().is_some_and(|si| {
            si.parts.len() > 1
                || si
                    .parts
                    .iter()
                    .any(|p| !matches!(p, GooglePart::Text { .. }))
        });
        let raw_system: Option<Value> = if needs_raw_system {
            req.system_instruction
                .as_ref()
                .and_then(|s| serde_json::to_value(s).ok())
        } else {
            None
        };

        // ── Tools: detect built-ins ───────────────────────────────────────────
        let has_builtin_tools = req.tools.as_ref().is_some_and(|ts| {
            ts.iter().any(|t| {
                t.google_search.is_some()
                    || t.code_execution.is_some()
                    || t.google_search_retrieval.is_some()
            })
        });
        let raw_tools: Option<Value> = if has_builtin_tools {
            req.tools
                .as_ref()
                .and_then(|t| serde_json::to_value(t).ok())
        } else {
            None
        };

        let instructions = req.system_instruction.as_ref().and_then(|instruction| {
            let text = instruction
                .parts
                .iter()
                .filter_map(|p| match p {
                    GooglePart::Text {
                        text,
                        thought: None | Some(false),
                        thought_signature: None,
                    } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        });

        // ── Messages ──────────────────────────────────────────────────────────
        let mut messages: Vec<AiItem> = Vec::new();

        for content in req.contents {
            messages.push(decode_content(content)?);
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        let tools = req.tools.as_ref().map(|entries| {
            let mut defs: Vec<ToolSpec> = Vec::new();
            for entry in entries {
                if let Some(decls) = &entry.function_declarations {
                    for fd in decls {
                        defs.push(ToolSpec {
                            name: fd.name.clone(),
                            description: fd.description.clone(),
                            parameters: fd
                                .parameters
                                .clone()
                                .unwrap_or(Value::Object(Default::default())),
                            strict: None,
                            cache_control: None,
                            meta: None,
                        });
                    }
                }
                if entry.google_search.is_some() {
                    defs.push(ToolSpec {
                        name: "__builtin__google_search".into(),
                        description: None,
                        parameters: Value::Object(Default::default()),
                        strict: None,
                        cache_control: None,
                        meta: None,
                    });
                }
                if entry.code_execution.is_some() {
                    defs.push(ToolSpec {
                        name: "__builtin__code_execution".into(),
                        description: None,
                        parameters: Value::Object(Default::default()),
                        strict: None,
                        cache_control: None,
                        meta: None,
                    });
                }
                if entry.google_search_retrieval.is_some() {
                    defs.push(ToolSpec {
                        name: "__builtin__google_search_retrieval".into(),
                        description: None,
                        parameters: Value::Object(Default::default()),
                        strict: None,
                        cache_control: None,
                        meta: None,
                    });
                }
            }
            defs
        });

        // ── generationConfig → IR fields + GoogleExt ──────────────────────────
        let gc = req.generation_config.as_ref();
        let max_tokens = gc.and_then(|c| c.max_output_tokens);
        let temperature = gc.and_then(|c| c.temperature);
        let top_p = gc.and_then(|c| c.top_p);
        let stop = gc.and_then(|c| c.stop_sequences.clone());
        let seed = gc.and_then(|c| c.seed.map(|s| s as i64));
        let frequency_penalty = gc.and_then(|c| c.frequency_penalty);
        let presence_penalty = gc.and_then(|c| c.presence_penalty);

        // Reasoning from thinkingConfig
        let reasoning = if let Some(tc) = gc.and_then(|c| c.thinking_config.as_ref()) {
            let effort_level = match tc.get("thinkingLevel") {
                None => None,
                Some(Value::String(value)) => Some(
                    crate::thinking::ThinkingLevel::from_wire(value).map_err(|_| {
                        anyhow::anyhow!("unsupported Gemini thinkingLevel: {value}")
                    })?,
                ),
                Some(_) => anyhow::bail!("Gemini thinkingLevel must be a string"),
            };
            let budget = tc
                .get("thinkingBudget")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let include_thoughts = tc
                .get("includeThoughts")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let level = effort_level
                .or_else(|| budget.map(crate::thinking::ThinkingLevel::from_budget))
                .or_else(|| {
                    tc.get("includeThoughts")
                        .and_then(Value::as_bool)
                        .map(|enabled| {
                            if enabled {
                                crate::thinking::ThinkingLevel::Medium
                            } else {
                                crate::thinking::ThinkingLevel::Off
                            }
                        })
                });
            ReasoningConfig {
                enabled: level.is_some_and(|level| level != crate::thinking::ThinkingLevel::Off)
                    || include_thoughts,
                budget_tokens: budget,
                level,
                ..Default::default()
            }
        } else {
            ReasoningConfig::default()
        };

        // Safety settings
        let safety_settings = req.safety_settings.as_ref().map(|ss| {
            ss.iter()
                .map(|s| SafetySettings {
                    category: s.category.clone(),
                    threshold: s.threshold.clone(),
                })
                .collect()
        });

        let plain_auto_tool_config = req
            .tool_config
            .as_ref()
            .is_some_and(is_plain_auto_tool_config);

        // GoogleExt
        let google_ext = GoogleExt {
            top_k: gc.and_then(|c| c.top_k.map(|v| v as u32)),
            candidate_count: gc.and_then(|c| c.candidate_count),
            response_logprobs: gc.and_then(|c| c.response_logprobs),
            logprobs: gc.and_then(|c| c.logprobs.map(|v| v as u32)),
            response_mime_type: gc.and_then(|c| c.response_mime_type.clone()),
            response_json_schema: gc.and_then(|c| c.response_schema.clone()),
            tool_config: if plain_auto_tool_config {
                None
            } else {
                req.tool_config.clone()
            },
            cached_content: req.cached_content.clone(),
            thinking_config: gc.and_then(|c| c.thinking_config.clone()),
            ..Default::default()
        };

        // ── Vendor ingress bag — backward compat for old Google encoder ────────
        let mut ingress = std::collections::HashMap::new();

        if let Some(ref gen_cfg) = req.generation_config
            && let Ok(v) = serde_json::to_value(gen_cfg)
        {
            ingress.insert("__google_generation_config".into(), v);
        }
        if let Some(v) = raw_system {
            ingress.insert("__google_raw_system_instruction".into(), v);
        }
        if let Some(v) = raw_tools {
            ingress.insert("__google_raw_tools".into(), v);
        }
        if let Some(ref ss) = req.safety_settings
            && let Ok(v) = serde_json::to_value(ss)
        {
            ingress.insert("__google_safety_settings".into(), v);
        }
        if let Some(ref tc) = req.tool_config {
            ingress.insert("__google_tool_config".into(), tc.clone());
        }
        if let Some(ref cc) = req.cached_content {
            ingress.insert("__google_cached_content".into(), Value::String(cc.clone()));
        }

        // ── Build AiRequest ───────────────────────────────────────────────────
        let tools_opt = tools.filter(|t| !t.is_empty());

        let mut ai_req = AiRequest::new(model.to_string(), messages);
        ai_req.instructions = instructions;
        ai_req.generation = GenerationConfig {
            temperature,
            max_tokens,
            top_p,
            seed,
            stop,
            frequency_penalty,
            presence_penalty,
        };
        ai_req.stream = StreamConfig {
            enabled: stream,
            include_usage: false,
        };
        ai_req.tools = tools_opt;
        ai_req.tool_choice = plain_auto_tool_config.then_some(ToolChoice::Auto);
        ai_req.reasoning = reasoning;
        ai_req.safety_settings = safety_settings;
        ai_req.ext = Some(ProtocolExt::Google(google_ext));
        ai_req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
        ai_req.meta.vendor.ingress = ingress;

        Ok(ai_req)
    }
}

impl GoogleDecoder {
    pub(crate) fn decode_request(&self, body: Value) -> Result<AiRequest> {
        self.decode_with_model(body, "gemini-2.0-flash", false)
    }
}

// ── Content decoding ──────────────────────────────────────────────────────────

fn decode_content(content: GoogleContent) -> Result<AiItem> {
    let mut role = match content.role.as_deref() {
        Some("user") | None => Role::User,
        Some("model") => Role::Assistant,
        Some(other) => anyhow::bail!("unknown Gemini role: {other}"),
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut has_function_response = false;
    let mut tool_result_ids = Vec::new();

    for part in content.parts {
        match part {
            GooglePart::Text {
                text,
                thought,
                thought_signature,
            } => {
                if thought.unwrap_or(false) || thought_signature.is_some() {
                    blocks.push(ContentBlock::Thinking {
                        thinking: text,
                        signature: thought_signature,
                    });
                } else {
                    text_parts.push(text.clone());
                    blocks.push(ContentBlock::Text {
                        text,
                        cache_control: None,
                    });
                }
            }
            GooglePart::InlineData { inline_data } => {
                let mime = inline_data.mime_type;
                let source = MediaSource::Base64 {
                    media_type: mime.clone(),
                    data: inline_data.data,
                };
                blocks.push(media_block(source, Some(mime)));
            }
            GooglePart::FileData { file_data } => {
                let mime = file_data.mime_type.filter(|value| !value.is_empty());
                let source = MediaSource::Url(file_data.file_uri);
                blocks.push(media_block(source, mime));
            }
            GooglePart::FunctionCall {
                function_call,
                thought_signature,
            } => {
                if let Some(signature) = thought_signature {
                    blocks.push(ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: Some(signature),
                    });
                }
                let id = function_call
                    .id
                    .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: function_call.name.clone(),
                    arguments: function_call.args.to_string(),
                });
                blocks.push(ContentBlock::ToolUse {
                    id,
                    name: function_call.name,
                    input: function_call.args,
                    cache_control: None,
                });
            }
            GooglePart::FunctionResponse { function_response } => {
                has_function_response = true;
                let tool_use_id = function_response.id.unwrap_or(function_response.name);
                tool_result_ids.push(tool_use_id.clone());
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id,
                    content: function_response.response,
                    is_error: None,
                    cache_control: None,
                });
            }
            GooglePart::ExecutableCode { executable_code } => {
                blocks.push(ContentBlock::ExecutableCode {
                    code: executable_code.code,
                    language: executable_code.language,
                    id: None,
                });
            }
            GooglePart::CodeExecutionResult {
                code_execution_result,
            } => {
                let output = code_execution_result.output.unwrap_or_default();
                blocks.push(ContentBlock::CodeExecutionResult {
                    return_code: 0,
                    stdout: output,
                    stderr: String::new(),
                    id: None,
                });
            }
            GooglePart::Other(v) => {
                // Detect thought parts (Gemini 2.5 extended thinking).
                let is_thought = v.get("thought").and_then(|t| t.as_bool()).unwrap_or(false);
                if is_thought {
                    let thinking = v
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    blocks.push(ContentBlock::Thinking {
                        thinking,
                        signature: v
                            .get("thoughtSignature")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                } else if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                    blocks.push(ContentBlock::Text {
                        text: text.to_string(),
                        cache_control: None,
                    });
                }
            }
        }
    }

    let msg_content = if blocks.len() == 1 && text_parts.len() == 1 {
        MessageContent::Text(text_parts.into_iter().next().unwrap())
    } else {
        MessageContent::Blocks(blocks)
    };

    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    if has_function_response {
        role = Role::Tool;
    }

    Ok(AiItem {
        role,
        content: msg_content,
        tool_calls: tool_calls_opt,
        tool_call_id: (tool_result_ids.len() == 1).then(|| tool_result_ids.remove(0)),
        meta: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_thought_signature_and_function_ids() {
        let request = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [
                    {
                        "role": "model",
                        "parts": [
                            {
                                "text": "checked the repository",
                                "thought": true,
                                "thoughtSignature": "opaque-reasoning"
                            },
                            {
                                "functionCall": {
                                    "id": "call_read",
                                    "name": "read",
                                    "args": {"path": "Cargo.toml"}
                                }
                            }
                        ]
                    },
                    {
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "id": "call_read",
                                "name": "read",
                                "response": {"result": "workspace"}
                            }
                        }]
                    }
                ]
            }))
            .expect("Gemini request");

        let MessageContent::Blocks(assistant_blocks) = &request.items[0].content else {
            panic!("assistant blocks");
        };
        assert!(matches!(
            &assistant_blocks[0],
            ContentBlock::Thinking {
                thinking,
                signature: Some(signature)
            } if thinking == "checked the repository" && signature == "opaque-reasoning"
        ));
        assert!(matches!(
            &assistant_blocks[1],
            ContentBlock::ToolUse { id, name, .. }
                if id == "call_read" && name == "read"
        ));

        let MessageContent::Blocks(tool_blocks) = &request.items[1].content else {
            panic!("tool blocks");
        };
        assert!(matches!(
            &tool_blocks[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_read"
        ));
        assert_eq!(request.items[1].tool_call_id.as_deref(), Some("call_read"));
    }

    #[test]
    fn include_thoughts_enables_reasoning_without_budget() {
        let request = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
                "generationConfig": {
                    "thinkingConfig": {"includeThoughts": true}
                }
            }))
            .expect("Gemini request");

        assert!(request.reasoning.enabled);
    }

    #[test]
    fn thinking_level_decodes_case_insensitively() {
        let request = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
                "generationConfig": {
                    "thinkingConfig": {"thinkingLevel": "HIGH"}
                }
            }))
            .expect("Gemini request");

        assert_eq!(
            request.reasoning.level,
            Some(crate::thinking::ThinkingLevel::High)
        );
    }

    #[test]
    fn unknown_thinking_level_is_rejected() {
        let error = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
                "generationConfig": {
                    "thinkingConfig": {"thinkingLevel": "turbo"}
                }
            }))
            .expect_err("unknown thinkingLevel must fail");

        assert!(
            error
                .to_string()
                .contains("unsupported Gemini thinkingLevel")
        );
    }
}

fn media_block(source: MediaSource, media_type: Option<String>) -> ContentBlock {
    match media_type.as_deref() {
        Some(value) if value.starts_with("image/") => ContentBlock::Image {
            source,
            detail: None,
            cache_control: None,
        },
        Some(value) if value.starts_with("audio/") => ContentBlock::Audio { source },
        Some(value) if value.starts_with("video/") => ContentBlock::Video { source, media_type },
        _ => ContentBlock::File { source, media_type },
    }
}
