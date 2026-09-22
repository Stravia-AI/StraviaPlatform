//! Command Code `/alpha/generate` envelope and NDJSON stream codec.
//!
//! The CLI wire is not OpenAI Chat Completions. Request bodies wrap model
//! params in a CLI envelope; the upstream always streams NDJSON events named
//! like AI SDK (`text-delta`, `reasoning-delta`, `tool-call`, `finish`).

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, bail};
use http::header::HeaderMap;
use serde_json::{Map, Value, json};

use stravia_protocol_codec::transform::{
    ProtocolAdapter, TextWireStreamParser, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use stravia_runtime_contract::protocol::ids::COMMAND_CODE_GENERATE_V1;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;
use stravia_runtime_contract::protocol::ids::StreamCaps;
use stravia_runtime_contract::protocol::ids::VendorFieldPolicy;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::ReasoningEffort;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::ToolChoice;
use stravia_runtime_contract::protocol::ir::ToolSpec;
use stravia_runtime_contract::protocol::ir::usage::Usage;

pub struct CommandCodeGenerateV1;

/// Command Code 设备档案的项目目录(workingDir / x-project-slug 同源)。
/// provider 侧的 `device_project_dir()` 复用此值,保持信封与指纹自洽。
pub const DEFAULT_WORKING_DIR: &str = r"C:\Users\dev\projects\app";
const DEFAULT_ENVIRONMENT: &str = "win32";
const DEFAULT_MAX_TOKENS: u32 = 64_000;
const MAX_TOKENS_CAP: u32 = 200_000;

const TOOL_NAME_ALIASES: &[(&str, &str)] = &[
    ("bash_output", "shell_output"),
    ("task_output", "shell_output"),
    ("tool_search", "search_tools"),
    ("read_multiple_files", "read_file"),
];

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[],
    multimodal: true,
    structured_output: false,
    function_calling: true,
    parallel_tool_calls: true,
    extended_reasoning: true,
    deterministic_seed: false,
    stream: StreamCaps {
        server_sent_events: false,
        usage_in_stream: true,
        requires_stream_flag: false,
    },
    unknown_field_policy: VendorFieldPolicy::Drop,
};

impl ProtocolAdapter for CommandCodeGenerateV1 {
    fn id(&self) -> ProtocolEndpoint {
        COMMAND_CODE_GENERATE_V1
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        let params = body.get("params").unwrap_or(&body);
        let model = params
            .get("model")
            .or_else(|| body.get("model"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut items = Vec::new();
        if let Some(system) = params.get("system").or_else(|| body.get("system")) {
            items.extend(decode_system(system)?);
        }
        let messages = params
            .get("messages")
            .or_else(|| body.get("messages"))
            .and_then(Value::as_array)
            .context("Command Code request is missing messages")?;
        for message in messages {
            items.push(decode_message(message)?);
        }
        let mut request = AiRequest::new(model, items);
        request.generation.max_tokens = params
            .get("max_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        request.generation.temperature = params.get("temperature").and_then(Value::as_f64);
        request.tools = params.get("tools").map(decode_tools).transpose()?;
        request.tool_choice = params
            .get("tool_choice")
            .map(decode_tool_choice)
            .transpose()?;
        if let Some(effort) = params.get("reasoning_effort").and_then(Value::as_str) {
            request.reasoning.enabled = true;
            request.reasoning.effort = ReasoningEffort::from_openai_str(effort).ok();
        }
        if let Some(parallel) = params.get("parallel_tool_calls").and_then(Value::as_bool) {
            request.parallel_tool_calls = Some(parallel);
        }
        request.stream.enabled = params
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        request.meta.source_protocol = Some(COMMAND_CODE_GENERATE_V1);
        Ok(request)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        let tool_names = tool_names(request);
        let mut system = Vec::new();
        if let Some(instructions) = &request.instructions
            && !instructions.trim().is_empty()
        {
            system.push(json!({"type": "text", "text": instructions}));
        }
        let mut messages = Vec::new();
        for item in &request.items {
            match item.role {
                Role::System | Role::Developer => system.extend(encode_system_blocks(item)?),
                _ => messages.push(encode_message(item, &tool_names)?),
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
            ("model".into(), Value::String(request.model.clone())),
            ("messages".into(), Value::Array(messages)),
            ("max_tokens".into(), json!(max_tokens)),
            ("stream".into(), Value::Bool(true)),
            ("system".into(), Value::Array(system)),
            (
                "tools".into(),
                Value::Array(
                    request
                        .tools
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(encode_tool)
                        .collect(),
                ),
            ),
        ]);
        insert_optional(&mut params, "temperature", request.generation.temperature);
        if let Some(effort) = request
            .reasoning
            .effort
            .as_ref()
            .and_then(ReasoningEffort::as_openai_str)
        {
            params.insert("reasoning_effort".into(), Value::String(effort.into()));
        }
        if let Some(choice) = &request.tool_choice {
            params.insert("tool_choice".into(), encode_tool_choice(choice));
        }
        if let Some(parallel) = request.parallel_tool_calls {
            params.insert("parallel_tool_calls".into(), Value::Bool(parallel));
        }

        let body = json!({
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
            "params": params,
        });
        Ok((body, HeaderMap::new()))
    }

    fn request_path(&self, _model: &str, _stream: bool) -> String {
        "/alpha/generate".into()
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        let mut response = AiResponse::new(
            body.get("id").and_then(Value::as_str).unwrap_or_default(),
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        for content in body
            .get("content")
            .and_then(Value::as_array)
            .context("Command Code response is missing content")?
        {
            match content.get("type").and_then(Value::as_str) {
                Some("text") => response.push_output_text(required_string(content, "text")?),
                Some("reasoning") => {
                    response.push_reasoning(required_string(content, "text")?, None)
                }
                Some("tool-call") => response.push_tool_call(decode_tool_call(content)?),
                Some(other) => bail!("unsupported Command Code response content type `{other}`"),
                None => bail!("Command Code response content is missing type"),
            }
        }
        response.stop_reason = body
            .get("finishReason")
            .and_then(Value::as_str)
            .map(map_finish_reason)
            .map(str::to_string);
        response.usage = command_code_usage(body.get("usage").or_else(|| body.get("totalUsage")));
        Ok(response)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        let mut content = Vec::new();
        for item in &response.items {
            match &item.content {
                MessageContent::Text(text) if item.role == Role::Assistant && !text.is_empty() => {
                    content.push(json!({"type": "text", "text": text}));
                }
                MessageContent::Blocks(blocks) if item.role == Role::Assistant => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text, .. } if !text.is_empty() => {
                                content.push(json!({"type": "text", "text": text}));
                            }
                            ContentBlock::Thinking { thinking, .. } => {
                                content.push(json!({"type": "reasoning", "text": thinking}));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            for call in item.tool_calls.as_deref().unwrap_or_default() {
                content.push(encode_tool_call_value(call));
            }
        }
        json!({
            "id": response.id,
            "model": response.model,
            "content": content,
            "finishReason": response.stop_reason.as_deref().unwrap_or("stop"),
            "usage": usage_value(&response.usage),
        })
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::custom_text(
            CommandCodeStreamParser::new(),
        ))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: COMMAND_CODE_GENERATE_V1,
            operation: "ingress stream encoding",
        })
    }
}

fn decode_system(value: &Value) -> anyhow::Result<Vec<AiItem>> {
    if let Some(text) = value.as_str() {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![AiItem {
            role: Role::System,
            content: MessageContent::Text(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }]);
    }
    let blocks = value
        .as_array()
        .context("Command Code system must be a string or block array")?
        .iter()
        .map(|block| {
            Ok(ContentBlock::Text {
                text: required_string(block, "text")?,
                cache_control: None,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if blocks.is_empty()
        || blocks.iter().all(|block| match block {
            ContentBlock::Text { text, .. } => text.trim().is_empty(),
            _ => false,
        })
    {
        return Ok(Vec::new());
    }
    Ok(vec![AiItem {
        role: Role::System,
        content: MessageContent::Blocks(blocks),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }])
}

fn decode_message(message: &Value) -> anyhow::Result<AiItem> {
    let role = match required_string(message, "role")?.as_str() {
        "system" | "developer" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => bail!("unsupported Command Code message role `{other}`"),
    };
    if matches!(role, Role::System) {
        // Content may be a plain string or the same block array shape the
        // top-level `system` field accepts.
        let text = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        return Ok(AiItem {
            role,
            content: MessageContent::Text(text),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        });
    }
    let parts = message
        .get("content")
        .and_then(Value::as_array)
        .context("Command Code message content must be an array")?;
    let mut blocks = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_call_id = None;
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => blocks.push(ContentBlock::Text {
                text: required_string(part, "text")?,
                cache_control: None,
            }),
            Some("reasoning") if role == Role::Assistant => {
                blocks.push(ContentBlock::Thinking {
                    thinking: required_string(part, "text")?,
                    signature: None,
                });
            }
            Some("tool-call") if role == Role::Assistant => {
                tool_calls.push(decode_tool_call(part)?);
            }
            Some("tool-result") if role == Role::Tool => {
                let id = required_string(part, "toolCallId")?;
                if tool_call_id.replace(id.clone()).is_some() {
                    bail!("Command Code tool message must contain exactly one tool result");
                }
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: tool_result_content(part)?,
                    content_kind: Some(
                        stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                    ),
                    is_error: None,
                    cache_control: None,
                });
            }
            Some("image") if role == Role::User => {
                let image = required_string(part, "image")?;
                blocks.push(ContentBlock::Image {
                    source: image_source(&image),
                    detail: None,
                    cache_control: None,
                });
            }
            Some(other) => {
                bail!("unsupported Command Code content type `{other}` for role `{role:?}`")
            }
            None => bail!("Command Code message content is missing type"),
        }
    }
    Ok(AiItem {
        role,
        content: MessageContent::Blocks(blocks),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id,
        meta: None,
    })
}

fn decode_tools(value: &Value) -> anyhow::Result<Vec<ToolSpec>> {
    value
        .as_array()
        .context("Command Code tools must be an array")?
        .iter()
        .map(|tool| {
            Ok(ToolSpec {
                name: required_string(tool, "name")?,
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: tool
                    .get("input_schema")
                    .or_else(|| tool.get("inputSchema"))
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                strict: None,
                cache_control: None,
                meta: None,
            })
        })
        .collect()
}

fn decode_tool_choice(value: &Value) -> anyhow::Result<ToolChoice> {
    match value.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(ToolChoice::Auto),
        Some("none") => Ok(ToolChoice::None),
        Some("any") | Some("required") => Ok(ToolChoice::Required),
        Some("tool") => Ok(ToolChoice::Named {
            name: required_string(value, "name")?,
        }),
        Some(other) => bail!("unsupported Command Code tool choice `{other}`"),
        None => bail!("Command Code tool choice is missing type"),
    }
}

fn decode_tool_call(value: &Value) -> anyhow::Result<ToolCall> {
    Ok(ToolCall {
        id: required_string(value, "toolCallId")?,
        name: required_string(value, "toolName")?,
        arguments: match value.get("input") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => serde_json::to_string(other)?,
            None => "{}".into(),
        },
    })
}

fn encode_system_blocks(item: &AiItem) -> anyhow::Result<Vec<Value>> {
    match &item.content {
        MessageContent::Text(text) if !text.is_empty() => {
            Ok(vec![json!({"type": "text", "text": text})])
        }
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => Ok(json!({"type": "text", "text": text})),
                other => bail!(
                    "Command Code system cannot represent `{}`",
                    content_block_name(other)
                ),
            })
            .collect(),
        MessageContent::Text(_) => Ok(Vec::new()),
    }
}

fn encode_message(item: &AiItem, tool_names: &BTreeMap<String, String>) -> anyhow::Result<Value> {
    match item.role {
        Role::System | Role::Developer => unreachable!("system messages are encoded separately"),
        Role::User => Ok(json!({
            "role": "user",
            "content": encode_user_parts(&item.content)?,
        })),
        Role::Assistant => {
            let mut content = encode_assistant_parts(&item.content)?;
            for call in item.tool_calls.as_deref().unwrap_or_default() {
                content.push(encode_tool_call_value(call));
            }
            Ok(json!({"role": "assistant", "content": content}))
        }
        Role::Tool => {
            let tool_call_id = item
                .tool_call_id
                .as_deref()
                .context("Command Code tool result is missing tool_call_id")?;
            let tool_name = tool_names
                .get(tool_call_id)
                .map(String::as_str)
                .unwrap_or("");
            Ok(json!({
                "role": "tool",
                "content": [{
                    "type": "tool-result",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "output": {"type": "text", "value": tool_result_text(&item.content)?},
                }],
            }))
        }
    }
}

fn encode_user_parts(content: &MessageContent) -> anyhow::Result<Vec<Value>> {
    match content {
        MessageContent::Text(text) => Ok(vec![json!({"type": "text", "text": text})]),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => Ok(json!({"type": "text", "text": text})),
                ContentBlock::Image { source, .. } => encode_image(source),
                other => bail!(
                    "Command Code user message cannot represent `{}`",
                    content_block_name(other)
                ),
            })
            .collect(),
    }
}

fn encode_assistant_parts(content: &MessageContent) -> anyhow::Result<Vec<Value>> {
    match content {
        MessageContent::Text(text) if text.is_empty() => Ok(Vec::new()),
        MessageContent::Text(text) => Ok(vec![json!({"type": "text", "text": text})]),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            let mut reasoning = Vec::new();
            let mut rest = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Thinking { thinking, .. } => {
                        reasoning.push(json!({"type": "reasoning", "text": thinking}));
                    }
                    ContentBlock::Text { text, .. } if !text.is_empty() => {
                        rest.push(json!({"type": "text", "text": text}));
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        rest.push(json!({
                            "type": "tool-call",
                            "toolCallId": id,
                            "toolName": name,
                            "input": input,
                        }));
                    }
                    ContentBlock::Text { .. } => {}
                    other => bail!(
                        "Command Code assistant message cannot represent `{}`",
                        content_block_name(other)
                    ),
                }
            }
            parts.extend(reasoning);
            parts.extend(rest);
            Ok(parts)
        }
    }
}

fn encode_image(source: &MediaSource) -> anyhow::Result<Value> {
    match source {
        MediaSource::Url(url) => {
            let mut image = json!({"type": "image", "image": url});
            if let Some(media_type) = data_url_media_type(url) {
                image
                    .as_object_mut()
                    .expect("image part is an object")
                    .insert("mimeType".into(), Value::String(media_type.into()));
            }
            Ok(image)
        }
        MediaSource::Base64 { media_type, data } => Ok(json!({
            "type": "image",
            "image": format!("data:{media_type};base64,{data}"),
            "mimeType": media_type,
        })),
        MediaSource::FileId { .. } => {
            bail!("Command Code cannot represent file-id image sources")
        }
    }
}

fn encode_tool(tool: &ToolSpec) -> Value {
    json!({
        "name": wire_tool_name(&tool.name),
        "description": tool.description.as_deref().unwrap_or(""),
        "input_schema": if tool.parameters.is_null() {
            json!({"type": "object", "properties": {}})
        } else {
            tool.parameters.clone()
        },
    })
}

fn encode_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Required => json!({"type": "any"}),
        ToolChoice::Named { name } => json!({"type": "tool", "name": name}),
        ToolChoice::Raw(value) => value.clone(),
    }
}

fn encode_tool_call_value(call: &ToolCall) -> Value {
    json!({
        "type": "tool-call",
        "toolCallId": call.id,
        "toolName": call.name,
        "input": serde_json::from_str::<Value>(&call.arguments)
            .unwrap_or_else(|_| Value::String(call.arguments.clone())),
    })
}

fn wire_tool_name(name: &str) -> &str {
    TOOL_NAME_ALIASES
        .iter()
        .find_map(|(from, to)| (*from == name).then_some(*to))
        .unwrap_or(name)
}

fn tool_names(request: &AiRequest) -> BTreeMap<String, String> {
    request
        .items
        .iter()
        .flat_map(|item| item.tool_calls.as_deref().unwrap_or_default())
        .map(|call| (call.id.clone(), call.name.clone()))
        .collect()
}

fn tool_result_text(content: &MessageContent) -> anyhow::Result<String> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Blocks(blocks) => Ok(blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                ContentBlock::ToolResult { content, .. } => content
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| serde_json::to_string(content).ok()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")),
    }
}

fn tool_result_content(part: &Value) -> anyhow::Result<Value> {
    let output = part
        .get("output")
        .context("Command Code tool result is missing output")?;
    if let Some(text) = output.get("value").and_then(Value::as_str) {
        return Ok(Value::String(text.to_string()));
    }
    if let Some(text) = output.as_str() {
        return Ok(Value::String(text.to_string()));
    }
    Ok(output.clone())
}

fn image_source(image: &str) -> MediaSource {
    if let Some(rest) = image.strip_prefix("data:")
        && let Some(semi) = rest.find(';')
    {
        let media_type = rest[..semi].to_string();
        if let Some(data) = rest[semi + 1..].strip_prefix("base64,") {
            return MediaSource::Base64 {
                media_type,
                data: data.to_string(),
            };
        }
    }
    MediaSource::Url(image.to_string())
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

fn usage_value(usage: &Usage) -> Value {
    json!({
        "inputTokens": usage.prompt_tokens,
        "outputTokens": usage.completion_tokens,
        "cachedInputTokens": usage.cache_read_tokens.unwrap_or(0),
        "inputTokenDetails": {
            "cacheReadTokens": usage.cache_read_tokens.unwrap_or(0),
            "cacheWriteTokens": usage.cache_creation_tokens.unwrap_or(0),
        },
    })
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

fn content_block_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::Image { .. } => "image",
        ContentBlock::Audio { .. } => "audio",
        ContentBlock::File { .. } => "file",
        ContentBlock::Video { .. } => "video",
        ContentBlock::Thinking { .. } => "thinking",
        ContentBlock::Reasoning { .. } => "reasoning",
        ContentBlock::Compaction { .. } => "compaction",
        ContentBlock::CompactionTrigger {} => "compaction_trigger",
        ContentBlock::RedactedThinking { .. } => "redacted_thinking",
        ContentBlock::ToolUse { .. } => "tool_use",
        ContentBlock::ToolResult { .. } => "tool_result",
        ContentBlock::ServerToolUse { .. } => "server_tool_use",
        ContentBlock::ServerToolResult { .. } => "server_tool_result",
        ContentBlock::Document { .. } => "document",
        ContentBlock::SearchResult { .. } => "search_result",
        ContentBlock::Citation { .. } => "citation",
        ContentBlock::ExecutableCode { .. } => "executable_code",
        ContentBlock::CodeExecutionResult { .. } => "code_execution_result",
        ContentBlock::ContainerUpload { .. } => "container_upload",
        ContentBlock::Refusal { .. } => "refusal",
        ContentBlock::Unknown { .. } => "unknown",
    }
}

pub struct CommandCodeStreamParser {
    buffer: String,
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
                    id: id.clone(),
                    name: required_string(&value, "toolName")?,
                    arguments: String::new(),
                };
                deltas.push(AiStreamDelta::ToolCallStart {
                    index,
                    id: call.id.clone(),
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
                if self.completed_tools.contains(&call.id) {
                    return Ok(());
                }
                let index = if let Some((index, _)) = self.tools.remove(&call.id) {
                    index
                } else {
                    let index = self.next_index;
                    self.next_index += 1;
                    deltas.push(AiStreamDelta::ToolCallStart {
                        index,
                        id: call.id.clone(),
                        name: call.name.clone(),
                    });
                    index
                };
                self.completed_tools.insert(call.id.clone());
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
