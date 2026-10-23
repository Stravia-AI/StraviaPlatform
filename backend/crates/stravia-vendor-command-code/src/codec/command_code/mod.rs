//! Command Code `/alpha/generate` envelope and NDJSON stream codec.
//!
//! The CLI wire is not OpenAI Chat Completions. Request bodies wrap model
//! params in a CLI envelope; the upstream always streams NDJSON events named
//! like AI SDK (`text-delta`, `reasoning-delta`, `tool-call`, `finish`).
//! Request projection is private to this codec: readable document children and
//! JSON tool results use existing text carriers; unrepresentable blocks and
//! file-id images are omitted without dropping their representable siblings.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use stravia_protocol_codec::transform::TextWireStreamParser;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;

use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::ToolSpec;
use stravia_runtime_contract::protocol::ir::usage::Usage;

pub struct CommandCodeGenerateV1;

/// Command Code 设备档案的项目目录(workingDir / x-project-slug 同源)。
/// provider 侧的 `device_project_dir()` 复用此值,保持信封与指纹自洽。
pub const DEFAULT_WORKING_DIR: &str = r"C:\Users\dev\projects\app";
const DEFAULT_ENVIRONMENT: &str = "win32";
// 私有协议沿用 CLI 逆向得到的默认值，不套用标准协议的缺省透传规则。
const DEFAULT_MAX_TOKENS: u32 = 64_000;
const MAX_TOKENS_CAP: u32 = 200_000;

const TOOL_NAME_ALIASES: &[(&str, &str)] = &[
    ("bash_output", "shell_output"),
    ("task_output", "shell_output"),
    ("tool_search", "search_tools"),
    ("read_multiple_files", "read_file"),
];

impl CommandCodeGenerateV1 {
    pub(crate) fn encode_request(&self, request: AiRequest) -> anyhow::Result<Value> {
        let tool_names = tool_names(&request);
        let mut system = Vec::new();
        if let Some(instructions) = request.instructions
            && !instructions.trim().is_empty()
        {
            system.push(text_part("text", instructions));
        }
        let mut messages = Vec::new();
        for item in request.items {
            match item.role {
                Role::System | Role::Developer => system.extend(encode_system_blocks(item)?),
                _ => {
                    if let Some(message) = encode_message(item, &tool_names)? {
                        messages.push(message);
                    }
                }
            }
        }
        if system.is_empty() {
            // Upstream injects a ~7.5K default prompt when `params.system` is
            // omitted. A space placeholder keeps the conversation clean.
            system.push(json!({"type": "text", "text": " "}));
        } else {
            let last = system.len() - 1;
            for block in &mut system[..last] {
                if let Some(Value::String(text)) = block.get_mut("text") {
                    text.push('\n');
                }
            }
        }

        let max_tokens = request
            .generation
            .max_tokens
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .min(MAX_TOKENS_CAP);
        let mut params = Map::from_iter([
            ("model".into(), Value::String(request.model)),
            ("messages".into(), Value::Array(messages)),
            ("stream".into(), Value::Bool(true)),
            ("system".into(), Value::Array(system)),
            ("max_tokens".into(), max_tokens.into()),
            (
                "tools".into(),
                Value::Array(
                    request
                        .tools
                        .unwrap_or_default()
                        .into_iter()
                        .map(encode_tool)
                        .collect(),
                ),
            ),
        ]);
        insert_optional(&mut params, "temperature", request.generation.temperature);
        match request.reasoning.target_control {
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { value }) => {
                let value = if value == "none" { "off".into() } else { value };
                params.insert("reasoning_effort".into(), Value::String(value));
            }
            None => {
                if let Some(effort) = request.reasoning.level.map(|level| {
                    if level == stravia_runtime_contract::thinking::ThinkingLevel::Off {
                        "off"
                    } else {
                        level.as_str()
                    }
                }) {
                    params.insert("reasoning_effort".into(), Value::String(effort.into()));
                }
            }
            _ => {}
        }
        // command-code 1.82.1's shipped createModelClient emits no native
        // tool_choice or parallel_tool_calls controls.
        let mut body = json!({
            "config": {
                "workingDir": DEFAULT_WORKING_DIR,
                "date": chrono::Utc::now().format("%Y-%m-%d").to_string(),
                "environment": DEFAULT_ENVIRONMENT,
                "structure": [],
                "isGitRepo": false,
                "currentBranch": "",
                "mainBranch": "",
                "gitStatus": "",
                "recentCommits": [],
            },
            "memory": Value::Null,
            "taste": Value::Null,
            "skills": Value::Null,
            "permissionMode": "standard",
            "mode": "agent",
        });
        body.as_object_mut()
            .expect("envelope is an object")
            .insert("params".into(), Value::Object(params));

        Ok(body)
    }
}

fn decode_tool_call(value: &Value) -> anyhow::Result<ToolCall> {
    Ok(ToolCall {
        id: (required_string(value, "toolCallId")?).into(),
        name: required_string(value, "toolName")?,
        arguments: match value.get("input") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => serde_json::to_string(other)?,
            None => "{}".into(),
        },
    })
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn text_part(kind: &str, text: String) -> Value {
    object([
        ("type", Value::String(kind.into())),
        ("text", Value::String(text)),
    ])
}

fn message(role: &str, content: Vec<Value>) -> Value {
    object([
        ("role", Value::String(role.into())),
        ("content", Value::Array(content)),
    ])
}

fn encode_system_blocks(item: AiItem) -> anyhow::Result<Vec<Value>> {
    match item.content {
        MessageContent::Text(text) if !text.is_empty() => {
            Ok(vec![text_part("text", Arc::unwrap_or_clone(text))])
        }
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                encode_readable_block(block, false, &mut parts)?;
            }
            Ok(parts)
        }
        MessageContent::Text(_) => Ok(Vec::new()),
    }
}

/// 返回 None 表示整条消息跳过：编码后没有任何部件且不带 tool call 的
/// assistant 条目（如只剩无法承载的受保护思考载荷）不应发出空消息。
fn encode_message(
    item: AiItem,
    tool_names: &BTreeMap<String, String>,
) -> anyhow::Result<Option<Value>> {
    match item.role {
        Role::System | Role::Developer => unreachable!("system messages are encoded separately"),
        Role::User => {
            let content = encode_user_parts(item.content)?;
            Ok((!content.is_empty()).then(|| message("user", content)))
        }
        Role::Assistant => {
            let mut content = encode_assistant_parts(item.content)?;
            for call in item.tool_calls.unwrap_or_default() {
                content.push(owned_tool_call_value(call));
            }
            if content.is_empty() {
                return Ok(None);
            }
            Ok(Some(message("assistant", content)))
        }
        Role::Tool => {
            let tool_call_id = item
                .tool_call_id
                .context("Command Code tool result is missing tool_call_id")?;
            let tool_name = tool_names
                .get(tool_call_id.as_str())
                .map(String::as_str)
                .unwrap_or("");
            let is_error = matches!(&item.content, MessageContent::Blocks(blocks) if blocks.iter().any(|block| matches!(block, ContentBlock::ToolResult { is_error: Some(true), .. })));
            let Some(text) = tool_result_text(item.content)? else {
                return Ok(None);
            };
            Ok(Some(message(
                "tool",
                vec![object([
                    ("type", Value::String("tool-result".into())),
                    ("toolCallId", Value::String(tool_call_id.into_string())),
                    ("toolName", Value::String(tool_name.into())),
                    (
                        "output",
                        object([
                            (
                                "type",
                                Value::String(if is_error { "error-text" } else { "text" }.into()),
                            ),
                            ("value", Value::String(text)),
                        ]),
                    ),
                ])],
            )))
        }
    }
}

fn encode_user_parts(content: MessageContent) -> anyhow::Result<Vec<Value>> {
    match content {
        MessageContent::Text(text) => Ok(vec![text_part("text", Arc::unwrap_or_clone(text))]),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                encode_readable_block(block, true, &mut parts)?;
            }
            Ok(parts)
        }
    }
}

fn encode_assistant_parts(content: MessageContent) -> anyhow::Result<Vec<Value>> {
    match content {
        MessageContent::Text(text) if text.is_empty() => Ok(Vec::new()),
        MessageContent::Text(text) => Ok(vec![text_part("text", Arc::unwrap_or_clone(text))]),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            let mut reasoning = Vec::new();
            let mut rest = Vec::new();
            for block in blocks {
                match block {
                    // reasoning 部件只有 text：signature/encrypted_content/redacted
                    // 均无可承载的字段，按回放契约静默忽略，绝不写进可读正文。
                    ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                        reasoning.push(text_part("reasoning", thinking));
                    }
                    ContentBlock::Reasoning {
                        summary, content, ..
                    } => {
                        // Open Responses 明文顺序：summary 在前、content 在后，
                        // 每段非空文本一个 reasoning 部件。
                        for text in summary.into_iter().chain(content) {
                            if !text.is_empty() {
                                reasoning.push(text_part("reasoning", text));
                            }
                        }
                    }
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
                    ContentBlock::Text { text, .. } if !text.is_empty() => {
                        rest.push(text_part("text", Arc::unwrap_or_clone(text)));
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        rest.push(object([
                            ("type", Value::String("tool-call".into())),
                            ("toolCallId", Value::String(id.into_string())),
                            ("toolName", Value::String(name)),
                            ("input", input),
                        ]));
                    }
                    ContentBlock::Text { .. } => {}
                    other => encode_readable_block(other, false, &mut rest)?,
                }
            }
            parts.extend(reasoning);
            parts.extend(rest);
            Ok(parts)
        }
    }
}

fn encode_image(source: MediaSource) -> anyhow::Result<Option<Value>> {
    match source {
        MediaSource::Url(url) => {
            let media_type = data_url_media_type(&url).map(str::to_owned);
            let mut image = object([
                ("type", Value::String("image".into())),
                ("image", Value::String(url)),
            ]);
            if let Some(media_type) = media_type {
                image
                    .as_object_mut()
                    .expect("image part is an object")
                    .insert("mimeType".into(), Value::String(media_type));
            }
            Ok(Some(image))
        }
        MediaSource::Base64 { media_type, data } => {
            let image = format!("data:{media_type};base64,{data}");
            Ok(Some(object([
                ("type", Value::String("image".into())),
                ("image", Value::String(image)),
                ("mimeType", Value::String(media_type)),
            ])))
        }
        MediaSource::FileId { .. } => Ok(None),
    }
}

fn encode_tool(tool: ToolSpec) -> Value {
    let name = TOOL_NAME_ALIASES
        .iter()
        .find_map(|(from, to)| (*from == tool.name).then(|| (*to).to_owned()))
        .unwrap_or(tool.name);
    object([
        ("name", Value::String(name)),
        (
            "description",
            Value::String(tool.description.unwrap_or_default()),
        ),
        (
            "input_schema",
            if tool.parameters.is_null() {
                json!({"type": "object", "properties": {}})
            } else {
                tool.parameters
            },
        ),
    ])
}

fn owned_tool_call_value(call: ToolCall) -> Value {
    let input =
        serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::String(call.arguments));
    object([
        ("type", Value::String("tool-call".into())),
        ("toolCallId", Value::String(call.id.into_string())),
        ("toolName", Value::String(call.name)),
        ("input", input),
    ])
}

fn tool_names(request: &AiRequest) -> BTreeMap<String, String> {
    request
        .items
        .iter()
        .flat_map(|item| item.tool_calls.as_deref().unwrap_or_default())
        .map(|call| (call.id.to_string(), call.name.clone()))
        .chain(
            request
                .items
                .iter()
                .flat_map(|item| match &item.content {
                    MessageContent::Blocks(blocks) => blocks.as_slice(),
                    MessageContent::Text(_) => &[],
                })
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, .. } => Some((id.to_string(), name.clone())),
                    _ => None,
                }),
        )
        .collect()
}

// Private wire projection: documents expose their readable children, while
// unsupported media and protected payloads never become invented text.
fn encode_readable_block(
    block: ContentBlock,
    images: bool,
    parts: &mut Vec<Value>,
) -> anyhow::Result<()> {
    use stravia_runtime_contract::protocol::ir::{DocumentSource, ToolResultContentKind};
    match block {
        ContentBlock::Text { text, .. } => {
            parts.push(text_part("text", Arc::unwrap_or_clone(text)));
        }
        ContentBlock::Image { source, .. } if images => {
            parts.extend(encode_image(source)?);
        }
        ContentBlock::Document { source, .. } => match source {
            DocumentSource::PlainText { data } => parts.push(text_part("text", data)),
            DocumentSource::Blocks { content } => {
                for block in content {
                    encode_readable_block(block, images, parts)?;
                }
            }
            _ => {}
        },
        ContentBlock::ToolResult {
            content,
            content_kind,
            ..
        }
        | ContentBlock::ServerToolResult {
            content,
            content_kind,
            ..
        } => {
            if content_kind == Some(ToolResultContentKind::ContentBlocks) {
                let blocks: Vec<ContentBlock> = serde_json::from_value(content)
                    .context("invalid Command Code tool result content blocks")?;
                for block in blocks {
                    encode_readable_block(block, images, parts)?;
                }
            } else {
                let text = match content {
                    Value::String(text) => text,
                    other => serde_json::to_string(&other)?,
                };
                parts.push(text_part("text", text));
            }
        }
        _ => {}
    }
    Ok(())
}

fn tool_result_text(content: MessageContent) -> anyhow::Result<Option<String>> {
    match content {
        MessageContent::Text(text) => Ok(Some(Arc::unwrap_or_clone(text))),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                encode_readable_block(block, false, &mut parts)?;
            }
            if parts.is_empty() {
                return Ok(None);
            }
            let text = parts
                .into_iter()
                .filter_map(|mut part| match part.get_mut("text").map(Value::take) {
                    Some(Value::String(text)) => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Some(text))
        }
    }
}

fn data_url_media_type(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("data:")?;
    let media = rest.split([';', ',']).next()?;
    (!media.is_empty()).then_some(media)
}

fn command_code_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    let mut prompt_tokens = as_u32(value.get("inputTokens")).unwrap_or(0);
    let completion_tokens = as_u32(value.get("outputTokens")).unwrap_or(0);
    let cache_read = as_u32(
        value
            .get("cachedInputTokens")
            .or_else(|| value.pointer("/inputTokenDetails/cacheReadTokens")),
    );
    let cache_write = as_u32(value.pointer("/inputTokenDetails/cacheWriteTokens"));
    if completion_tokens == 0 {
        // Zero-output responses are treated as empty to avoid false billing.
        return Usage {
            required_components_known: true,
            ..Usage::default()
        };
    }
    if let Some(no_cache) = as_u32(value.pointer("/inputTokenDetails/noCacheTokens")) {
        prompt_tokens = no_cache
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_write.unwrap_or(0));
    }
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
        required_components_known: true,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_write,
        ..Usage::default()
    }
}

fn map_finish_reason(reason: &str) -> &str {
    let reason = reason.trim();
    match reason.to_ascii_lowercase().as_str() {
        "tool-calls" | "tool_calls" | "tool_use" => "tool_calls",
        "length" | "max_tokens" | "max_output_tokens" | "model_context_window_exceeded" => "length",
        "" => "stop",
        _ => reason,
    }
}

fn insert_optional<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(key.into(), json!(value));
    }
}

fn required_string(value: &Value, key: &str) -> anyhow::Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("Command Code payload is missing string `{key}`"))
}

fn as_u32(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

pub struct CommandCodeStreamParser {
    buffer: String,
    started: bool,
    done: bool,
    saw_finish_signal: bool,
    finish_reason: String,
    usage: Usage,
    tools: BTreeMap<String, (usize, ToolCall)>,
    /// Monotonic slot counter. `tools.len()` is not a valid index source:
    /// `tool-input-end` removes entries, so a later bare `tool-call` would
    /// reuse the slot of a still-open tool call.
    next_index: usize,
    /// Tool call ids already finalized. The upstream emits the streamed
    /// `tool-input-*` events *and* a bare `tool-call` echo with the complete
    /// input for every call; without this set each call completes twice.
    completed_tools: HashSet<String>,
}

impl CommandCodeStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            started: false,
            done: false,
            saw_finish_signal: false,
            finish_reason: "stop".into(),
            usage: Usage::default(),
            tools: BTreeMap::new(),
            next_index: 0,
            completed_tools: HashSet::new(),
        }
    }

    pub fn parse_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let line = self.buffer[..index].to_string();
            self.buffer.drain(..=index);
            self.parse_line(&line, &mut deltas)?;
        }
        Ok(deltas)
    }

    pub fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        let mut deltas = Vec::new();
        if !self.buffer.trim().is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.parse_line(&line, &mut deltas)?;
        }
        if !self.done {
            if self.saw_finish_signal {
                deltas.push(AiStreamDelta::Usage(self.usage.clone()));
                deltas.push(AiStreamDelta::Done {
                    stop_reason: self.finish_reason.clone(),
                });
                self.done = true;
            } else {
                deltas.push(AiStreamDelta::UnexpectedEof);
            }
        }
        Ok(deltas)
    }

    fn parse_line(&mut self, line: &str, deltas: &mut Vec<AiStreamDelta>) -> anyhow::Result<()> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
            return Ok(());
        }
        let value: Value = serde_json::from_str(trimmed).context("parse Command Code NDJSON")?;
        if !self.started {
            self.started = true;
            // 原生信封没有响应 ID；宿主通过 MessageStart 绑定续接节点身份。
            deltas.push(AiStreamDelta::MessageStart {
                id: String::new(),
                model: String::new(),
            });
        }
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text-delta" => {
                let text = value
                    .get("text")
                    .or_else(|| value.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    deltas.push(AiStreamDelta::TextDelta(text.to_string()));
                }
            }
            "reasoning-delta" => {
                let text = value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    deltas.push(AiStreamDelta::ThinkingDelta(text.to_string()));
                }
            }
            "tool-input-start" => {
                let id = required_string(&value, "id")?;
                if self.completed_tools.contains(&id) {
                    // Duplicate start for an already-finalized call — ignore.
                    return Ok(());
                }
                let index = self.next_index;
                self.next_index += 1;
                let call = ToolCall {
                    id: (id.clone()).into(),
                    name: required_string(&value, "toolName")?,
                    arguments: String::new(),
                };
                deltas.push(AiStreamDelta::ToolCallStart {
                    index,
                    id: call.id.to_string(),
                    name: call.name.clone(),
                });
                self.tools.insert(id, (index, call));
            }
            "tool-input-delta" => {
                let id = required_string(&value, "id")?;
                let Some((index, call)) = self.tools.get_mut(&id) else {
                    // Deltas trailing a bare `tool-call` that already
                    // finalized this id are redundant, not protocol errors.
                    if self.completed_tools.contains(&id) {
                        return Ok(());
                    }
                    bail!("Command Code emitted a tool input delta before start");
                };
                let delta = required_string(&value, "delta")?;
                call.arguments.push_str(&delta);
                deltas.push(AiStreamDelta::ToolCallDelta {
                    index: *index,
                    arguments: delta,
                });
            }
            "tool-input-end" => {
                let id = required_string(&value, "id")?;
                let Some((index, call)) = self.tools.remove(&id) else {
                    if self.completed_tools.contains(&id) {
                        return Ok(());
                    }
                    bail!("Command Code emitted a tool input end before start");
                };
                self.completed_tools.insert(id);
                deltas.push(AiStreamDelta::ToolCallComplete {
                    index,
                    tool_call: call,
                });
            }
            "tool-call" => {
                // The upstream emits this echo after the streamed
                // `tool-input-*` events for the same call. Only treat it as a
                // new call when no streamed form was seen.
                let call = decode_tool_call(&value)?;
                if self.completed_tools.contains(call.id.as_str()) {
                    return Ok(());
                }
                let index = if let Some((index, _)) = self.tools.remove(call.id.as_str()) {
                    index
                } else {
                    let index = self.next_index;
                    self.next_index += 1;
                    deltas.push(AiStreamDelta::ToolCallStart {
                        index,
                        id: call.id.to_string(),
                        name: call.name.clone(),
                    });
                    index
                };
                self.completed_tools.insert(call.id.to_string());
                deltas.push(AiStreamDelta::ToolCallComplete {
                    index,
                    tool_call: call,
                });
            }
            "finish-step" => {
                self.saw_finish_signal = true;
                if let Some(reason) = value.get("finishReason").and_then(Value::as_str) {
                    self.finish_reason = map_finish_reason(reason).to_string();
                }
                if value.get("usage").is_some() {
                    self.usage = command_code_usage(value.get("usage"));
                }
            }
            "finish" => {
                self.saw_finish_signal = true;
                if let Some(reason) = value.get("finishReason").and_then(Value::as_str) {
                    self.finish_reason = map_finish_reason(reason).to_string();
                }
                if value.get("totalUsage").is_some() || value.get("usage").is_some() {
                    self.usage =
                        command_code_usage(value.get("totalUsage").or_else(|| value.get("usage")));
                }
                deltas.push(AiStreamDelta::Usage(self.usage.clone()));
                self.done = true;
                deltas.push(AiStreamDelta::Done {
                    stop_reason: self.finish_reason.clone(),
                });
            }
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown Command Code error");
                bail!("Command Code stream error: {message}");
            }
            "text-start" | "text-end" | "reasoning-start" | "reasoning-end" | "start"
            | "start-step" | "provider-metadata" | "tool-error" => {}
            other => deltas.push(AiStreamDelta::Unknown {
                raw: format!("{other}: {trimmed}"),
            }),
        }
        Ok(())
    }
}

impl TextWireStreamParser for CommandCodeStreamParser {
    fn parse_text_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.parse_chunk(raw)
    }

    fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.finish()
    }
}

impl Default for CommandCodeStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
