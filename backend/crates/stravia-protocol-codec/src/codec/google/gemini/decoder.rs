//! Google Generative AI ingress decoder — produces `AiRequest` directly.
//!
//! `decode_with_model` accepts the model and stream flag extracted from the URL
//! path by the ingress shell handler, since Google embeds the model in the path
//! rather than the request body.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::GenerationConfig;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::ReasoningConfig;
use stravia_runtime_contract::protocol::ir::ResponseFormat;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::StreamConfig;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::ToolChoice;
use stravia_runtime_contract::protocol::ir::ToolSpec;
use stravia_runtime_contract::protocol::ir::{
    AudioOutput, Modality, ParameterReset, ReasoningSummary, native_tool_from_gemini,
};

use super::types::*;

pub struct GoogleDecoder;

pub(super) fn decode_tool_choice(value: &Value) -> Option<ToolChoice> {
    let config = value.get("functionCallingConfig")?;
    let mode = config.get("mode")?.as_str()?;
    if matches!(mode, "AUTO" | "ANY")
        && let Some(names) = config.get("allowedFunctionNames").and_then(Value::as_array)
    {
        return Some(ToolChoice::Allowed {
            names: names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            required: mode == "ANY",
        });
    }
    match mode {
        "AUTO" => Some(ToolChoice::Auto),
        "NONE" => Some(ToolChoice::None),
        "ANY" => Some(ToolChoice::Required),
        _ => None,
    }
}

impl GoogleDecoder {
    pub fn decode_with_model(&self, body: Value, model: &str, stream: bool) -> Result<AiRequest> {
        let mut resets = Vec::new();
        for (key, reset) in [
            ("systemInstruction", ParameterReset::Instructions),
            ("tools", ParameterReset::NativeTools),
        ] {
            if body
                .get(key)
                .is_some_and(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty))
            {
                resets.push(reset);
            }
        }
        if body
            .pointer("/systemInstruction/parts")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            resets.push(ParameterReset::Instructions);
        }
        if let Some(config) = body.get("generationConfig") {
            for (key, reset) in [
                ("temperature", ParameterReset::Temperature),
                ("topP", ParameterReset::TopP),
                ("presencePenalty", ParameterReset::PresencePenalty),
                ("frequencyPenalty", ParameterReset::FrequencyPenalty),
                ("maxOutputTokens", ParameterReset::MaxTokens),
                ("responseMimeType", ParameterReset::ResponseFormat),
                ("responseJsonSchema", ParameterReset::ResponseFormat),
                ("responseSchema", ParameterReset::ResponseFormat),
                ("responseLogprobs", ParameterReset::Logprobs),
                ("logprobs", ParameterReset::Logprobs),
                ("thinkingConfig", ParameterReset::Reasoning),
            ] {
                if config.get(key).is_some_and(|value| {
                    value.is_null()
                        || (key == "responseMimeType" && value.as_str() == Some(""))
                        || (key == "thinkingConfig"
                            && value.as_object().is_some_and(serde_json::Map::is_empty))
                }) {
                    resets.push(reset);
                }
            }
            if let Some(thinking) = config.get("thinkingConfig") {
                for (key, reset) in [
                    ("thinkingLevel", ParameterReset::ReasoningLevel),
                    ("includeThoughts", ParameterReset::ReasoningSummary),
                ] {
                    if thinking.get(key).is_some_and(Value::is_null) {
                        resets.push(reset);
                    }
                }
            }
        }
        let mut req: GoogleRequest = serde_json::from_value(body)?;

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

        let instructions = req.system_instruction.as_ref().map(|instruction| {
            instruction
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
                .join("\n")
        });

        // ── Messages ──────────────────────────────────────────────────────────
        let supplied_tool_ids: HashSet<String> = req
            .contents
            .iter()
            .flat_map(|content| &content.parts)
            .filter_map(|part| match part {
                GooglePart::FunctionCall { function_call, .. } => function_call.id.as_deref(),
                GooglePart::FunctionResponse { function_response } => {
                    function_response.id.as_deref()
                }
                _ => None,
            })
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        let mut generated_tool_id_seq = 0;
        let mut messages: Vec<AiItem> = Vec::new();

        for content in req.contents {
            decode_content(
                content,
                &mut generated_tool_id_seq,
                &supplied_tool_ids,
                &mut messages,
            )?;
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        let mut native_tools = Vec::new();
        let tools = req.tools.take().map(|entries| {
            let mut defs: Vec<ToolSpec> = Vec::new();
            for entry in entries {
                if let Some(decls) = entry.function_declarations {
                    for fd in decls {
                        defs.push(ToolSpec {
                            name: fd.name,
                            description: fd.description,
                            parameters: fd.parameters_json_schema.unwrap_or_else(|| {
                                super::types::normalize_json_schema(
                                    fd.parameters.unwrap_or(Value::Object(Default::default())),
                                )
                            }),
                            strict: None,
                            defer_loading: None,
                        });
                    }
                }
                for (key, value) in [
                    ("googleSearch", entry.google_search),
                    ("codeExecution", entry.code_execution),
                    ("googleSearchRetrieval", entry.google_search_retrieval),
                ] {
                    if let Some(value) = value
                        && let Some(tool) =
                            native_tool_from_gemini(&serde_json::json!({key: value}))
                    {
                        native_tools.push(tool);
                    }
                }
            }
            defs
        });

        // ── generationConfig → unified controls ───────────────────────────────
        let gc = req.generation_config.as_ref();
        let max_tokens = gc.and_then(|c| c.max_output_tokens);
        let temperature = gc.and_then(|c| c.temperature);
        let top_p = gc.and_then(|c| c.top_p);
        let seed = gc.and_then(|c| c.seed);
        let frequency_penalty = gc.and_then(|c| c.frequency_penalty);
        let presence_penalty = gc.and_then(|c| c.presence_penalty);

        // Reasoning from thinkingConfig
        let reasoning = if let Some(tc) = gc.and_then(|c| c.thinking_config.as_ref()) {
            let effort_level = match tc.get("thinkingLevel") {
                None => None,
                Some(Value::String(value)) => Some(
                    stravia_runtime_contract::thinking::ThinkingLevel::from_wire(value).map_err(
                        |_| anyhow::anyhow!("unsupported Gemini thinkingLevel: {value}"),
                    )?,
                ),
                Some(Value::Null) => None,
                Some(_) => anyhow::bail!("Gemini thinkingLevel must be a string"),
            };
            let budget = tc
                .get("thinkingBudget")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok());
            let include_thoughts = tc.get("includeThoughts").and_then(Value::as_bool);
            let level = effort_level.or_else(|| {
                budget.map(stravia_runtime_contract::thinking::ThinkingLevel::from_budget)
            });
            ReasoningConfig {
                mode: match tc.get("thinkingBudget").and_then(Value::as_i64) {
                    Some(-1) => Some("auto".into()),
                    Some(0) => Some("disabled".into()),
                    Some(value) if value > 0 => Some("enabled".into()),
                    _ => None,
                },
                budget_tokens: budget,
                level,
                summary: include_thoughts.map(|enabled| {
                    if enabled {
                        ReasoningSummary::Auto
                    } else {
                        ReasoningSummary::None
                    }
                }),
                ..Default::default()
            }
        } else {
            ReasoningConfig::default()
        };

        // Only content that cannot be represented by ordered items uses ingress.
        let mut ingress = std::collections::HashMap::new();
        if let Some(v) = raw_system {
            ingress.insert("__google_raw_system_instruction".into(), v);
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
            stop: req
                .generation_config
                .as_mut()
                .and_then(|config| config.stop_sequences.take()),
            frequency_penalty,
            presence_penalty,
        };
        ai_req.stream = StreamConfig {
            enabled: stream,
            include_usage: false,
        };
        ai_req.tools = tools_opt;
        ai_req.tool_choice = req.tool_config.as_ref().and_then(decode_tool_choice);
        ai_req.reasoning = reasoning;
        let parameters = &mut ai_req.parameters;
        parameters.resets = resets;
        if let Some(mut config) = req.generation_config.take() {
            parameters.top_k = config.top_k.map(|value| value as u32);
            parameters.candidate_count = config.candidate_count;
            parameters.logprobs = match config.response_logprobs {
                Some(false) => {
                    parameters.reset(ParameterReset::Logprobs);
                    None
                }
                Some(true) => Some(
                    config
                        .logprobs
                        .and_then(|value| u32::try_from(value).ok())
                        .unwrap_or(0),
                ),
                None => config.logprobs.and_then(|value| u32::try_from(value).ok()),
            };
            parameters.modalities = config.response_modalities.map(|values| {
                values
                    .into_iter()
                    .filter_map(|value| match value.to_ascii_lowercase().as_str() {
                        "text" => Some(Modality::Text),
                        "audio" => Some(Modality::Audio),
                        "image" => Some(Modality::Image),
                        _ => None,
                    })
                    .collect()
            });
            ai_req.response_format = config
                .response_json_schema
                .take()
                .filter(|schema| !schema.is_null())
                .or_else(|| {
                    config
                        .response_schema
                        .take()
                        .filter(|schema| !schema.is_null())
                        .map(super::types::normalize_json_schema)
                })
                .map(|schema| ResponseFormat::JsonSchema {
                    name: "response".into(),
                    description: None,
                    schema,
                    strict: None,
                });
            if ai_req.response_format.is_none() {
                ai_req.response_format = match config.response_mime_type.as_deref() {
                    Some("application/json") => Some(ResponseFormat::JsonObject),
                    Some("text/plain") => Some(ResponseFormat::Text),
                    _ => None,
                };
            }
            parameters.audio = config
                .speech_config
                .as_ref()
                .and_then(|speech| speech.pointer("/voiceConfig/prebuiltVoiceConfig/voiceName"))
                .and_then(Value::as_str)
                .map(|voice| AudioOutput {
                    voice: Some(voice.into()),
                    format: None,
                });
        }
        parameters.native_tools = native_tools;
        ai_req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
        ai_req.meta.vendor.ingress = ingress;
        ai_req.normalize_parameters();

        Ok(ai_req)
    }
}

impl GoogleDecoder {
    pub(crate) fn decode_request(&self, body: Value) -> Result<AiRequest> {
        self.decode_with_model(body, "gemini-2.0-flash", false)
    }
}

// ── Content decoding ──────────────────────────────────────────────────────────

fn decode_content(
    content: GoogleContent,
    generated_tool_id_seq: &mut usize,
    supplied_tool_ids: &HashSet<String>,
    messages: &mut Vec<AiItem>,
) -> Result<()> {
    let role = match content.role.as_deref() {
        Some("user") | None => Role::User,
        Some("model") => Role::Assistant,
        Some(other) => anyhow::bail!("unknown Gemini role: {other}"),
    };

    let start = messages.len();
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

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
                    blocks.push(ContentBlock::Text {
                        text: text.into(),
                        cache_control: None,
                    });
                }
            }
            part @ (GooglePart::InlineData { .. } | GooglePart::FileData { .. }) => {
                blocks.push(decode_media_part(part).expect("matched media part"));
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
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or_else(|| {
                        next_synthetic_tool_call_id(generated_tool_id_seq, supplied_tool_ids)
                    });
                tool_calls.push(ToolCall {
                    id: (id.clone()).into(),
                    name: function_call.name.clone(),
                    arguments: function_call.args.to_string(),
                });
                blocks.push(ContentBlock::ToolUse {
                    id: id.into(),
                    name: function_call.name,
                    input: function_call.args,
                    cache_control: None,
                });
            }
            GooglePart::FunctionResponse { function_response } => {
                if !blocks.is_empty() {
                    messages.push(decoded_content(
                        role,
                        std::mem::take(&mut blocks),
                        std::mem::take(&mut tool_calls),
                    ));
                }
                let tool_use_id = function_response.id.unwrap_or(function_response.name);
                let mut result = Vec::with_capacity(1 + function_response.parts.len());
                result.push(ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone().into(),
                    content: function_response.response,
                    content_kind: Some(
                        stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                    ),
                    is_error: None,
                    cache_control: None,
                });
                result.extend(
                    function_response
                        .parts
                        .into_iter()
                        .filter_map(decode_media_part),
                );
                // 一个 Item 只属于一次调用，避免并行结果的附件串到另一个 call_id。
                messages.push(AiItem {
                    role: Role::Tool,
                    content: MessageContent::Blocks(result),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id.into()),
                    meta: None,
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
                    blocks.push(ContentBlock::Text {
                        text: text.to_string().into(),
                        cache_control: None,
                    });
                }
            }
        }
    }

    if !blocks.is_empty() || messages.len() == start {
        messages.push(decoded_content(role, blocks, tool_calls));
    }
    Ok(())
}

fn decode_media_part(part: GooglePart) -> Option<ContentBlock> {
    match part {
        GooglePart::InlineData { inline_data } => {
            let mime = inline_data.mime_type;
            let source = MediaSource::Base64 {
                media_type: mime.clone(),
                data: inline_data.data,
            };
            Some(media_block(source, Some(mime)))
        }
        GooglePart::FileData { file_data } => {
            let mime = file_data.mime_type.filter(|value| !value.is_empty());
            Some(media_block(MediaSource::Url(file_data.file_uri), mime))
        }
        _ => None,
    }
}

fn decoded_content(role: Role, mut blocks: Vec<ContentBlock>, tool_calls: Vec<ToolCall>) -> AiItem {
    let msg_content = if matches!(blocks.as_slice(), [ContentBlock::Text { .. }]) {
        let Some(ContentBlock::Text { text, .. }) = blocks.pop() else {
            unreachable!();
        };
        MessageContent::Text(text)
    } else {
        MessageContent::Blocks(blocks)
    };

    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    AiItem {
        role,
        content: msg_content,
        tool_calls: tool_calls_opt,
        tool_call_id: None,
        meta: None,
    }
}

fn next_synthetic_tool_call_id(sequence: &mut usize, supplied_ids: &HashSet<String>) -> String {
    loop {
        *sequence += 1;
        let id = format!("tc_{}", *sequence);
        if !supplied_ids.contains(&id) {
            return id;
        }
    }
}

#[cfg(test)]
mod tests;

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
