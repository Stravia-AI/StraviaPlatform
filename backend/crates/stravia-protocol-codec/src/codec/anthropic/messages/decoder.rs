//! Anthropic Messages ingress decoder — produces `AiRequest` directly.
//!
//! Cache-control and exotic blocks (Document, InputAudio) are now carried
//! natively in `ContentBlock`. Raw snapshots retain cache and exotic block
//! metadata; request controls have canonical typed ownership.

use anyhow::Result;
use serde_json::Value;

use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::CacheTtl;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::DocumentSource as IrDocumentSource;
use stravia_runtime_contract::protocol::ir::GenerationConfig;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::ReasoningConfig;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::StreamConfig;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::ToolChoice;
use stravia_runtime_contract::protocol::ir::ToolSpec;
use stravia_runtime_contract::protocol::ir::{
    CacheRetention, CompactionControl, NativeTool, NativeToolKind, ParameterReset,
    ReasoningSummary, ServiceTier, native_tool_from_anthropic,
};

use super::types::{
    AnthropicContent, AnthropicContentBlock, AnthropicImageSource, AnthropicMessage,
    AnthropicRequest, AnthropicSystem, CacheControl as WireCacheControl,
    DocumentSource as WireDocumentSource,
};
use crate::codec::compaction::{self, Envelope};
use crate::codec::open_responses::parser as open_responses_parser;

pub struct AnthropicDecoder;

impl AnthropicDecoder {
    pub(crate) fn decode_request(&self, body: Value) -> Result<AiRequest> {
        let mut resets = Vec::new();
        for (field, reset) in [
            ("system", ParameterReset::Instructions),
            ("tools", ParameterReset::NativeTools),
            ("temperature", ParameterReset::Temperature),
            ("top_p", ParameterReset::TopP),
            ("max_tokens", ParameterReset::MaxTokens),
            ("thinking", ParameterReset::Reasoning),
            ("service_tier", ParameterReset::ServiceTier),
            ("cache_control", ParameterReset::CacheRetention),
        ] {
            if body.get(field).is_some_and(Value::is_null) {
                resets.push(reset);
            }
        }
        if body
            .get("system")
            .is_some_and(|v| v == "" || v.as_array().is_some_and(Vec::is_empty))
        {
            resets.push(ParameterReset::Instructions);
        }
        if body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            resets.push(ParameterReset::NativeTools);
        }
        if body
            .pointer("/thinking/display")
            .is_some_and(Value::is_null)
        {
            resets.push(ParameterReset::ReasoningSummary);
        }
        if body
            .pointer("/cache_control/ttl")
            .is_some_and(Value::is_null)
        {
            resets.push(ParameterReset::CacheRetention);
        }
        if body
            .pointer("/output_config/effort")
            .is_some_and(Value::is_null)
        {
            resets.push(ParameterReset::ReasoningLevel);
        }
        if body.get("output_config").is_some_and(Value::is_null)
            || body
                .pointer("/output_config/format")
                .is_some_and(Value::is_null)
        {
            resets.push(ParameterReset::ResponseFormat);
        }
        if body.get("metadata").is_some_and(Value::is_null)
            || body
                .pointer("/metadata/user_id")
                .is_some_and(|v| v.is_null() || v == "")
        {
            resets.push(ParameterReset::EndUser);
        }
        if body
            .pointer("/tool_choice/disable_parallel_tool_use")
            .is_some_and(Value::is_null)
        {
            resets.push(ParameterReset::ParallelToolCalls);
        }
        let req: AnthropicRequest = serde_json::from_value(body)?;

        // ── Cache-control / exotic-block detection (for raw preservation) ──────
        let needs_raw_msgs = req.messages.iter().any(|m| {
            if let AnthropicContent::Blocks(blocks) = &m.content {
                blocks.iter().any(|b| {
                    b.cache_control().is_some()
                        || b.is_exotic()
                        || matches!(
                            b,
                            AnthropicContentBlock::ToolResult {
                                is_error: Some(_),
                                ..
                            }
                        )
                })
            } else {
                false
            }
        }) && !req.messages.iter().any(|m| {
            // A compaction block may need re-sealing for the selected Target, which the
            // raw wire snapshot would bypass.
            matches!(&m.content, AnthropicContent::Blocks(blocks)
                if blocks.iter().any(|b| matches!(b, AnthropicContentBlock::Compaction { .. })))
        });

        let needs_raw_system = match &req.system {
            Some(AnthropicSystem::Blocks(blocks)) => {
                blocks.iter().any(|b| b.cache_control.is_some())
            }
            _ => false,
        };

        // Snapshot raw wire values before consuming req.
        // Inline "system" role messages are filtered out of the snapshot so that
        // the Anthropic encoder's raw-passthrough path (used when cache_control is
        // present) never forwards non-standard roles to strict downstream endpoints.
        let raw_messages: Option<Value> = if needs_raw_msgs {
            let wire_msgs: Vec<&AnthropicMessage> = req
                .messages
                .iter()
                .filter(|m| matches!(m.role.as_str(), "user" | "assistant"))
                .collect();
            serde_json::to_value(wire_msgs).ok()
        } else {
            None
        };
        let raw_system: Option<Value> = if needs_raw_system {
            req.system
                .as_ref()
                .and_then(|s| serde_json::to_value(s).ok())
        } else {
            None
        };
        // ── System prompt ─────────────────────────────────────────────────────
        let instructions = req
            .system
            .as_ref()
            .map(|system| match system {
                AnthropicSystem::Text(t) => t.clone(),
                AnthropicSystem::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| {
                        if b.kind == "text" {
                            Some(b.text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
            .filter(|text| !text.is_empty());

        let mut messages: Vec<AiItem> = Vec::new();

        for msg in req.messages {
            messages.extend(decode_message(msg)?);
        }

        // ── Tools ─────────────────────────────────────────────────────────────
        let mut user_tools: Vec<ToolSpec> = Vec::new();
        let mut native_tools = Vec::new();

        if let Some(tools) = req.tools {
            for t in tools {
                if t.tool_type.as_deref().is_some_and(|kind| kind != "custom") {
                    if let Some(tool) = native_tool_from_anthropic(&serde_json::to_value(t)?) {
                        native_tools.push(tool);
                    }
                } else if let Some(schema) = t.input_schema {
                    user_tools.push(ToolSpec {
                        name: t.name,
                        description: t.description,
                        parameters: schema,
                        strict: t.strict,
                        defer_loading: t.defer_loading,
                    });
                }
            }
        }

        // ── tool_choice ───────────────────────────────────────────────────────
        let disable_parallel_tool_calls = req
            .tool_choice
            .as_ref()
            .and_then(|choice| choice.get("disable_parallel_tool_use"))
            .filter(|value| !value.is_null())
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    anyhow::anyhow!("Anthropic disable_parallel_tool_use must be a boolean")
                })
            })
            .transpose()?;
        let tool_choice = req
            .tool_choice
            .and_then(parse_tool_choice)
            .map(|choice| match choice {
                ToolChoice::Named { ref name }
                    if name == "web_search"
                        && native_tools
                            .iter()
                            .any(|tool| matches!(tool, NativeTool::WebSearch(_))) =>
                {
                    ToolChoice::Native(NativeToolKind::WebSearch)
                }
                ToolChoice::Named { ref name }
                    if name == "code_execution"
                        && native_tools
                            .iter()
                            .any(|tool| matches!(tool, NativeTool::CodeExecution(_))) =>
                {
                    ToolChoice::Native(NativeToolKind::CodeExecution)
                }
                choice => choice,
            });
        let output_effort = parse_output_effort(req.output_config.as_ref())?;
        let response_format = parse_output_format(req.output_config)?;

        // ── Reasoning config ──────────────────────────────────────────────────
        let reasoning = match req.thinking.as_ref() {
            Some(thinking) if output_effort.is_some() => {
                if !matches!(thinking.kind.as_str(), "enabled" | "disabled" | "adaptive") {
                    anyhow::bail!("unsupported Anthropic thinking type: {}", thinking.kind);
                }
                let mut reasoning = reasoning_from_effort(
                    output_effort.as_deref().expect("explicit effort"),
                    parse_summary(thinking.display.as_deref()),
                )?;
                reasoning.mode = Some(if thinking.kind == "adaptive" {
                    "auto".into()
                } else {
                    thinking.kind.clone()
                });
                reasoning.budget_tokens = if thinking.kind == "enabled" {
                    thinking.budget_tokens
                } else {
                    None
                };
                reasoning
            }
            Some(thinking) if thinking.kind == "adaptive" => ReasoningConfig {
                mode: Some("auto".into()),
                summary: parse_summary(thinking.display.as_deref()),
                ..Default::default()
            },
            Some(thinking) => {
                let enabled = match thinking.kind.as_str() {
                    "enabled" => true,
                    "disabled" => false,
                    value => anyhow::bail!("unsupported Anthropic thinking type: {value}"),
                };
                ReasoningConfig {
                    mode: Some(thinking.kind.clone()),
                    budget_tokens: thinking.budget_tokens,
                    summary: parse_summary(thinking.display.as_deref()),
                    level: Some(if enabled {
                        thinking
                            .budget_tokens
                            .map(stravia_runtime_contract::thinking::ThinkingLevel::from_budget)
                            .unwrap_or(stravia_runtime_contract::thinking::ThinkingLevel::Medium)
                    } else {
                        stravia_runtime_contract::thinking::ThinkingLevel::Off
                    }),
                    ..Default::default()
                }
            }
            None => output_effort
                .as_deref()
                .map(|effort| reasoning_from_effort(effort, None))
                .transpose()?
                .unwrap_or_default(),
        };

        // ── Raw snapshots, never request-control aliases ─────────────────────
        let mut ingress = std::collections::HashMap::new();

        if let Some(v) = raw_messages {
            ingress.insert("__anthropic_raw_messages".into(), v);
        }
        if let Some(v) = raw_system {
            ingress.insert("__anthropic_raw_system".into(), v);
        }
        // ── Build AiRequest ───────────────────────────────────────────────────
        let tools_opt = if user_tools.is_empty() {
            None
        } else {
            Some(user_tools)
        };

        let mut ai_req = AiRequest::new(req.model, messages);
        ai_req.instructions = instructions;
        ai_req.generation = GenerationConfig {
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            top_p: req.top_p,
            stop: req.stop_sequences,
            ..Default::default()
        };
        ai_req.stream = StreamConfig {
            enabled: req.stream,
            include_usage: false,
        };
        ai_req.tools = tools_opt;
        ai_req.tool_choice = tool_choice;
        ai_req.parallel_tool_calls = disable_parallel_tool_calls.map(|disabled| !disabled);
        ai_req.response_format = response_format;
        ai_req.reasoning = reasoning;
        ai_req.parameters.top_k = req.top_k;
        let container = req.container.as_ref().and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("id").and_then(Value::as_str))
        });
        for tool in &mut native_tools {
            if let NativeTool::CodeExecution(tool) = tool {
                tool.container = container.map(str::to_owned);
            }
        }
        ai_req.parameters.compaction = req
            .context_management
            .as_ref()
            .and_then(|value| value.get("edits"))
            .and_then(Value::as_array)
            .and_then(|edits| {
                edits.iter().find(|edit| {
                    edit.get("type").and_then(Value::as_str) == Some("compact_20260112")
                })
            })
            .map(|edit| CompactionControl {
                threshold_tokens: edit
                    .get("trigger")
                    .filter(|trigger| {
                        trigger.get("type").and_then(Value::as_str) == Some("input_tokens")
                    })
                    .and_then(|trigger| trigger.get("value"))
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
                instructions: None,
            });
        if let Some(control) = req.compaction.filter(|control| !control.is_null()) {
            anyhow::ensure!(
                control.is_object(),
                "Anthropic `compaction` must be an object"
            );
            anyhow::ensure!(
                control.get("type").and_then(Value::as_str) == Some("summarize"),
                "Anthropic `compaction.type` must be summarize"
            );
            let instructions = match control.get("instructions") {
                Some(Value::String(instructions)) => Some(instructions.clone()),
                None | Some(Value::Null) => None,
                Some(_) => anyhow::bail!("Anthropic `compaction.instructions` must be a string"),
            };
            anyhow::ensure!(
                ai_req.parameters.compaction.is_none(),
                "Anthropic `compaction` cannot be combined with inline compaction"
            );
            ai_req.parameters.compaction = Some(CompactionControl {
                threshold_tokens: None,
                instructions,
            });
            ai_req.items.push(AiItem {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::CompactionTrigger {}]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            });
        }
        ai_req.parameters.cache.retention = req
            .cache_control
            .as_ref()
            .filter(|control| control.get("type").and_then(Value::as_str) == Some("ephemeral"))
            .and_then(|control| match control.get("ttl").and_then(Value::as_str) {
                Some("1h") => Some(CacheRetention::Extended),
                Some("5m") => Some(CacheRetention::Default),
                None if control.get("ttl").is_none() => Some(CacheRetention::Default),
                _ => None,
            });
        ai_req.parameters.end_user_id = req
            .metadata
            .and_then(|mut metadata| metadata.as_object_mut()?.remove("user_id"))
            .and_then(|user_id| match user_id {
                Value::String(user_id) if !user_id.is_empty() => Some(user_id),
                _ => None,
            });
        ai_req.parameters.service_tier = req.service_tier.and_then(|tier| match tier.as_str() {
            "auto" => Some(ServiceTier::Auto),
            "standard_only" => Some(ServiceTier::Default),
            _ => None,
        });
        ai_req.parameters.native_tools = native_tools;
        ai_req.parameters.resets = resets;
        ai_req.meta.source_protocol = Some(ANTHROPIC_MESSAGES_2023_06_01);
        ai_req.meta.vendor.ingress = ingress;
        ai_req.normalize_parameters();

        Ok(ai_req)
    }
}

// ── Message decoding helpers ──────────────────────────────────────────────────

fn decode_message(msg: AnthropicMessage) -> Result<Vec<AiItem>> {
    let msg = match msg.content {
        AnthropicContent::Blocks(blocks)
            if blocks
                .iter()
                .any(|block| matches!(block, AnthropicContentBlock::Compaction { .. })) =>
        {
            // A compaction block replaces everything before it, so it leads its message;
            // it expands to the state (and, for a Responses window, the retained items)
            // and the remaining blocks keep their own message.
            let mut items = Vec::new();
            let mut rest = Vec::new();
            for block in blocks {
                match block {
                    AnthropicContentBlock::Compaction {
                        content,
                        signature,
                        extra,
                    } => items.extend(decode_compaction_block(content, signature, extra)?),
                    other => rest.push(other),
                }
            }
            if rest.is_empty() {
                return Ok(items);
            }
            items.extend(decode_message(AnthropicMessage {
                role: msg.role,
                content: AnthropicContent::Blocks(rest),
            })?);
            return Ok(items);
        }
        content => AnthropicMessage {
            role: msg.role,
            content,
        },
    };
    let role = match msg.role.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        // Claude Code >=2.1.154 uses the official mid-conversation system messages feature,
        // embedding {"role":"system"} inside the messages array for skill/hook context.
        // See: https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages
        "system" => Role::System,
        other => anyhow::bail!("unknown Anthropic role: {other}"),
    };

    if role == Role::User
        && let AnthropicContent::Blocks(blocks) = msg.content
    {
        return decode_user_blocks(blocks);
    }

    let (content, tool_calls, tool_call_id) = match msg.content {
        AnthropicContent::Text(t) => (MessageContent::Text(t.into()), None, None),
        AnthropicContent::Blocks(blocks) => {
            let mut content_blocks: Vec<ContentBlock> = Vec::new();
            let mut tcs: Vec<ToolCall> = Vec::new();
            let mut tc_id: Option<String> = None;
            let mut thinking_texts: Vec<String> = Vec::new();

            for block in blocks {
                match block {
                    AnthropicContentBlock::Text {
                        text,
                        cache_control,
                        ..
                    } => {
                        content_blocks.push(ContentBlock::Text {
                            text: text.into(),
                            cache_control: cache_control.as_ref().map(map_cache_control),
                        });
                    }
                    AnthropicContentBlock::Thinking {
                        thinking,
                        signature,
                        ..
                    } => {
                        if role == Role::Assistant && !thinking.is_empty() {
                            thinking_texts.push(thinking.clone());
                        }
                        content_blocks.push(ContentBlock::Thinking {
                            thinking,
                            signature,
                        });
                    }
                    AnthropicContentBlock::RedactedThinking { data } => {
                        content_blocks.push(ContentBlock::RedactedThinking { data });
                    }
                    AnthropicContentBlock::Image {
                        source,
                        cache_control,
                        ..
                    } => {
                        content_blocks.push(ContentBlock::Image {
                            source: map_image_source(source)?,
                            detail: None,
                            cache_control: cache_control.as_ref().map(map_cache_control),
                        });
                    }
                    AnthropicContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control,
                        ..
                    } => {
                        tcs.push(ToolCall {
                            id: (id.clone()).into(),
                            name: name.clone(),
                            arguments: input.to_string(),
                        });
                        content_blocks.push(ContentBlock::ToolUse {
                            id: id.into(),
                            name,
                            input,
                            cache_control: cache_control.as_ref().map(map_cache_control),
                        });
                    }
                    AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        cache_control,
                        ..
                    } => {
                        tc_id = Some(tool_use_id.clone());
                        let content = content.unwrap_or(Value::Null);
                        let content_kind = if content.is_string() || content.is_array() {
                            stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks
                        } else {
                            stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json
                        };
                        content_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.into(),
                            content,
                            content_kind: Some(content_kind),
                            is_error: None,
                            cache_control: cache_control.as_ref().map(map_cache_control),
                        });
                    }
                    AnthropicContentBlock::Document {
                        source,
                        title,
                        context,
                        cache_control,
                        ..
                    } => {
                        content_blocks.push(map_document_block(
                            source,
                            title,
                            context,
                            cache_control,
                        )?);
                    }
                    AnthropicContentBlock::InputAudio { source, .. } => {
                        content_blocks.push(ContentBlock::Audio {
                            source: MediaSource::Base64 {
                                media_type: source.media_type,
                                data: source.data,
                            },
                        });
                    }
                    AnthropicContentBlock::Compaction { .. } => {
                        anyhow::bail!("compaction blocks are expanded before role decoding")
                    }
                }
            }

            let tool_calls_opt = if tcs.is_empty() { None } else { Some(tcs) };

            let meta = build_reasoning_meta(&thinking_texts);

            if content_blocks.len() == 1
                && let ContentBlock::Text { text, .. } = &content_blocks[0]
            {
                return Ok(vec![AiItem {
                    role,
                    content: MessageContent::Text(text.clone()),
                    tool_calls: tool_calls_opt,
                    tool_call_id: (tc_id).map(Into::into),
                    meta,
                }]);
            }

            return Ok(vec![AiItem {
                role,
                content: MessageContent::Blocks(content_blocks),
                tool_calls: tool_calls_opt,
                tool_call_id: (tc_id).map(Into::into),
                meta,
            }]);
        }
    };

    Ok(vec![AiItem {
        role,
        content,
        tool_calls,
        tool_call_id,
        meta: None,
    }])
}

/// Native Anthropic state is routed through the Responses compact-window decoder as a
/// sealed item, so it is byte-identical to the state a Responses client replays and to
/// what the upstream parser registered.
fn decode_compaction_block(
    content: Option<String>,
    signature: Option<String>,
    extra: serde_json::Map<String, Value>,
) -> Result<Vec<AiItem>> {
    let Some(signature) = signature.filter(|signature| !signature.is_empty()) else {
        anyhow::bail!("compaction block requires a signature");
    };
    if let Some(Envelope::Responses { output }) = compaction::open(&signature) {
        return open_responses_parser::decode_compaction_window(&output);
    }
    // `content: null` is a failed compaction the API treats as a no-op.
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    let mut block = extra;
    block.insert("type".into(), "compaction".into());
    block.insert("content".into(), content.into());
    block.insert("signature".into(), signature.clone().into());
    open_responses_parser::decode_compaction_window(&[serde_json::json!({
        "type": "compaction",
        "id": compaction::anthropic_state_id(&signature),
        "encrypted_content": compaction::seal_anthropic(&Value::Object(block)),
    })])
}

/// Build `meta = {"reasoning_content": ...}` so the OpenAI-compat encoder can
/// re-emit thinking text as a top-level `reasoning_content` field on assistant
/// messages (required by providers like Xiaomi Mimo / DeepSeek thinking mode).
fn build_reasoning_meta(
    thinking_texts: &[String],
) -> Option<Box<stravia_runtime_contract::protocol::ir::AiItemMetadata>> {
    if thinking_texts.is_empty() {
        return None;
    }
    let joined = thinking_texts.join("\n\n");
    if joined.is_empty() {
        return None;
    }
    Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
            serde_json::json!({ "reasoning_content": joined }),
        ),
    )
}

fn decode_user_blocks(blocks: Vec<AnthropicContentBlock>) -> Result<Vec<AiItem>> {
    let mut messages: Vec<AiItem> = Vec::new();
    let mut user_blocks: Vec<ContentBlock> = Vec::new();

    for block in blocks {
        match block {
            AnthropicContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                cache_control,
                ..
            } => {
                let content = content.unwrap_or_else(|| Value::String(String::new()));
                let content_kind = if content.is_array() {
                    "content_blocks"
                } else {
                    "json"
                };
                let meta = Some(
                    stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
                        serde_json::json!({
                            (stravia_runtime_contract::protocol::ir::TOOL_RESULT_CONTENT_KIND_META): content_kind
                        }),
                    ),
                );
                let content = match content {
                    Value::String(text) if is_error.is_none() && cache_control.is_none() => {
                        MessageContent::Text(text.into())
                    }
                    content => MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone().into(),
                        content,
                        content_kind: Some(if content_kind == "content_blocks" {
                            stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks
                        } else {
                            stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json
                        }),
                        is_error,
                        cache_control: cache_control.as_ref().map(map_cache_control),
                    }]),
                };
                messages.push(AiItem {
                    role: Role::Tool,
                    content,
                    tool_calls: None,
                    tool_call_id: Some((tool_use_id).into()),
                    meta,
                });
            }
            AnthropicContentBlock::Text {
                text,
                cache_control,
                ..
            } => {
                user_blocks.push(ContentBlock::Text {
                    text: text.into(),
                    cache_control: cache_control.as_ref().map(map_cache_control),
                });
            }
            AnthropicContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                user_blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature,
                });
            }
            AnthropicContentBlock::RedactedThinking { data } => {
                user_blocks.push(ContentBlock::RedactedThinking { data });
            }
            AnthropicContentBlock::Image {
                source,
                cache_control,
                ..
            } => {
                user_blocks.push(ContentBlock::Image {
                    source: map_image_source(source)?,
                    detail: None,
                    cache_control: cache_control.as_ref().map(map_cache_control),
                });
            }
            AnthropicContentBlock::ToolUse {
                id,
                name,
                input,
                cache_control,
                ..
            } => {
                user_blocks.push(ContentBlock::ToolUse {
                    id: id.into(),
                    name,
                    input,
                    cache_control: cache_control.as_ref().map(map_cache_control),
                });
            }
            AnthropicContentBlock::Document {
                source,
                title,
                context,
                cache_control,
                ..
            } => {
                user_blocks.push(map_document_block(source, title, context, cache_control)?);
            }
            AnthropicContentBlock::InputAudio { source, .. } => {
                user_blocks.push(ContentBlock::Audio {
                    source: MediaSource::Base64 {
                        media_type: source.media_type,
                        data: source.data,
                    },
                });
            }
            AnthropicContentBlock::Compaction { .. } => {
                anyhow::bail!("compaction blocks are expanded before role decoding")
            }
        }
    }

    if !user_blocks.is_empty() {
        let content = if user_blocks.len() == 1
            && let ContentBlock::Text { text, .. } = &user_blocks[0]
        {
            MessageContent::Text(text.clone())
        } else {
            MessageContent::Blocks(user_blocks)
        };
        messages.insert(
            0,
            AiItem {
                role: Role::User,
                content,
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        );
    }

    Ok(messages)
}

// ── Type-mapping helpers ──────────────────────────────────────────────────────

fn map_cache_control(
    cc: &WireCacheControl,
) -> stravia_runtime_contract::protocol::ir::CacheControl {
    stravia_runtime_contract::protocol::ir::CacheControl {
        ttl: if cc.ttl.as_deref() == Some("1h") {
            CacheTtl::Ephemeral1h
        } else {
            CacheTtl::Ephemeral5m
        },
        breakpoint_priority: 0,
    }
}

fn map_image_source(src: AnthropicImageSource) -> Result<MediaSource> {
    Ok(match src.source_type.as_str() {
        "base64" => {
            let media_type = src
                .media_type
                .ok_or_else(|| anyhow::anyhow!("Anthropic base64 image requires media_type"))?;
            anyhow::ensure!(
                matches!(
                    media_type.as_str(),
                    "image/jpeg" | "image/png" | "image/gif" | "image/webp"
                ),
                "Unsupported Anthropic image MIME type: {media_type}"
            );
            MediaSource::Base64 {
                media_type,
                data: src
                    .data
                    .ok_or_else(|| anyhow::anyhow!("Anthropic base64 image requires data"))?,
            }
        }
        "url" => MediaSource::Url(
            src.url
                .ok_or_else(|| anyhow::anyhow!("Anthropic URL image requires url"))?,
        ),
        "file" => MediaSource::FileId {
            file_id: src
                .file_id
                .ok_or_else(|| anyhow::anyhow!("Anthropic file image requires file_id"))?,
            detail: None,
        },
        kind => anyhow::bail!("Unsupported Anthropic image source type: {kind}"),
    })
}

fn map_document_block(
    source: WireDocumentSource,
    title: Option<String>,
    context: Option<String>,
    cache_control: Option<WireCacheControl>,
) -> Result<ContentBlock> {
    if source.kind == "file" {
        return Ok(ContentBlock::File {
            source: MediaSource::FileId {
                file_id: source
                    .file_id
                    .ok_or_else(|| anyhow::anyhow!("Anthropic file document requires file_id"))?,
                detail: None,
            },
            media_type: None,
        });
    }
    Ok(ContentBlock::Document {
        source: map_doc_source(source)?,
        title,
        context,
        cache_control: cache_control.as_ref().map(map_cache_control),
    })
}

fn map_doc_source(src: WireDocumentSource) -> Result<IrDocumentSource> {
    Ok(match src.kind.as_str() {
        "base64" => {
            anyhow::ensure!(
                src.media_type.as_deref() == Some("application/pdf"),
                "Anthropic base64 document requires application/pdf MIME type"
            );
            IrDocumentSource::Base64Pdf {
                data: src
                    .data
                    .ok_or_else(|| anyhow::anyhow!("Anthropic base64 document requires data"))?,
            }
        }
        "text" => {
            anyhow::ensure!(
                src.media_type.as_deref() == Some("text/plain"),
                "Anthropic text document requires text/plain MIME type"
            );
            IrDocumentSource::PlainText {
                data: src
                    .data
                    .ok_or_else(|| anyhow::anyhow!("Anthropic text document requires data"))?,
            }
        }
        "url" => IrDocumentSource::Url(
            src.url
                .ok_or_else(|| anyhow::anyhow!("Anthropic URL document requires url"))?,
        ),
        "content" => {
            let content = match src.content {
                Some(Value::String(text)) => vec![ContentBlock::Text {
                    text: text.into(),
                    cache_control: None,
                }],
                Some(Value::Array(values)) => {
                    let mut content = Vec::with_capacity(values.len());
                    for value in values {
                        if !matches!(
                            value.get("type").and_then(Value::as_str),
                            Some("text" | "image")
                        ) {
                            continue;
                        }
                        match serde_json::from_value::<AnthropicContentBlock>(value)? {
                            AnthropicContentBlock::Text {
                                text,
                                cache_control,
                            } => {
                                content.push(ContentBlock::Text {
                                    text: text.into(),
                                    cache_control: cache_control.as_ref().map(map_cache_control),
                                });
                            }
                            AnthropicContentBlock::Image {
                                source,
                                cache_control,
                            } => {
                                content.push(ContentBlock::Image {
                                    source: map_image_source(source)?,
                                    detail: None,
                                    cache_control: cache_control.as_ref().map(map_cache_control),
                                });
                            }
                            _ => unreachable!("document content was filtered to text and image"),
                        }
                    }
                    content
                }
                _ => anyhow::bail!("Anthropic document content must be a string or block array"),
            };
            IrDocumentSource::Blocks { content }
        }
        kind => anyhow::bail!("Unsupported Anthropic document source type: {kind}"),
    })
}

fn parse_tool_choice(v: Value) -> Option<ToolChoice> {
    match v.get("type").and_then(|t| t.as_str()) {
        Some("auto") => Some(ToolChoice::Auto),
        Some("any") => Some(ToolChoice::Required),
        Some("none") => Some(ToolChoice::None),
        Some("tool") => v
            .get("name")
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named {
                name: name.to_string(),
            }),
        _ => None,
    }
}

fn parse_output_format(
    output_config: Option<Value>,
) -> Result<Option<stravia_runtime_contract::protocol::ir::ResponseFormat>> {
    let Some(mut format) = output_config
        .and_then(|mut config| config.as_object_mut()?.remove("format"))
        .filter(|format| !format.is_null())
    else {
        return Ok(None);
    };
    if format.get("type").and_then(Value::as_str) != Some("json_schema") {
        anyhow::bail!("unsupported Anthropic output_config.format type");
    }
    let schema = format
        .as_object_mut()
        .and_then(|format| format.remove("schema"))
        .filter(|schema| schema.is_object())
        .ok_or_else(|| {
            anyhow::anyhow!("Anthropic output_config.format.schema must be an object")
        })?;
    Ok(Some(
        stravia_runtime_contract::protocol::ir::ResponseFormat::JsonSchema {
            name: "response".into(),
            description: None,
            schema,
            strict: Some(true),
        },
    ))
}

fn parse_output_effort(output_config: Option<&Value>) -> Result<Option<String>> {
    let Some(output_config) = output_config else {
        return Ok(None);
    };
    let output_config = output_config
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Anthropic output_config must be an object"))?;
    output_config
        .get("effort")
        .filter(|effort| !effort.is_null())
        .map(|effort| {
            effort
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Anthropic output_config.effort must be a string"))
        })
        .transpose()
}

fn parse_summary(display: Option<&str>) -> Option<ReasoningSummary> {
    match display {
        Some("summarized") => Some(ReasoningSummary::Auto),
        Some("omitted") => Some(ReasoningSummary::None),
        _ => None,
    }
}

fn reasoning_from_effort(
    effort: &str,
    summary: Option<ReasoningSummary>,
) -> Result<ReasoningConfig> {
    let level = stravia_runtime_contract::thinking::ThinkingLevel::from_wire(effort)?;
    Ok(ReasoningConfig {
        summary,
        level: Some(level),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests;
