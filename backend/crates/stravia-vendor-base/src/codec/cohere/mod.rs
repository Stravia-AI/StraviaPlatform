//! Cohere Chat API v2 (`POST /v2/chat`) egress codec.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use http::header::HeaderMap;
use serde_json::{Map, Value, json};

use stravia_protocol_codec::transform::{
    ProtocolAdapter, TextWireStreamParser, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use stravia_runtime_contract::protocol::ids::COHERE_CHAT_V2;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::DocumentSource;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::ParameterReset;
use stravia_runtime_contract::protocol::ir::ResponseFormat;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::ToolChoice;
use stravia_runtime_contract::protocol::ir::Usage;

pub struct CohereChatV2;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    override_model_in_body: false,
    ingress_routes: &[],
    multimodal: true,
    structured_output: true,
    function_calling: true,
    parallel_tool_calls: false,
    extended_reasoning: true,
    deterministic_seed: true,
    stream: stravia_runtime_contract::protocol::ids::StreamCaps {
        server_sent_events: true,
        usage_in_stream: true,
        requires_stream_flag: true,
    },
    unknown_field_policy: stravia_runtime_contract::protocol::ids::VendorFieldPolicy::Drop,
};

impl ProtocolAdapter for CohereChatV2 {
    fn id(&self) -> ProtocolEndpoint {
        COHERE_CHAT_V2
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        let model = required_string(&body, "model")?;
        let mut request = AiRequest::new(model, decode_messages(&body)?);
        request.stream.enabled = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        request.generation.temperature = body.get("temperature").and_then(Value::as_f64);
        request.generation.max_tokens = body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        request.generation.top_p = body.get("p").and_then(Value::as_f64);
        request.parameters.top_k = body
            .get("k")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        request.parameters.logprobs = body
            .get("logprobs")
            .and_then(Value::as_bool)
            .filter(|enabled| *enabled)
            .map(|_| 0);
        if body
            .get("logprobs")
            .is_some_and(|value| value.is_null() || value.as_bool() == Some(false))
        {
            request.parameters.reset(ParameterReset::Logprobs);
        }
        if body.get("thinking").is_some_and(Value::is_null) {
            request.parameters.reset(ParameterReset::Reasoning);
        }
        if body.get("response_format").is_some_and(Value::is_null) {
            request.parameters.reset(ParameterReset::ResponseFormat);
        }
        request.generation.frequency_penalty =
            body.get("frequency_penalty").and_then(Value::as_f64);
        request.generation.presence_penalty = body.get("presence_penalty").and_then(Value::as_f64);
        request.generation.seed = body.get("seed").and_then(Value::as_i64);
        let documents = decode_documents(&body);
        if !documents.is_empty() {
            request.items.push(AiItem {
                role: Role::User,
                content: MessageContent::Blocks(documents),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            });
        }
        if let Some(thinking) = body.get("thinking").filter(|value| !value.is_null()) {
            let enabled = match thinking.get("type").and_then(Value::as_str) {
                Some("enabled") => true,
                Some("disabled") => false,
                _ => bail!("unsupported Cohere thinking type"),
            };
            request.reasoning.mode = Some(if enabled { "enabled" } else { "disabled" }.into());
            request.reasoning.target_control = Some(if enabled {
                stravia_runtime_contract::thinking::TargetThinkingControl::Enabled
            } else {
                stravia_runtime_contract::thinking::TargetThinkingControl::Disabled
            });
            request.reasoning.budget_tokens = thinking
                .get("token_budget")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
        }
        request.generation.stop = body.get("stop_sequences").and_then(string_array);
        request.tools = body
            .get("tools")
            .filter(|value| !value.is_null())
            .map(decode_tools)
            .transpose()?;
        if let Some(strict) = body.get("strict_tools").and_then(Value::as_bool)
            && let Some(tools) = &mut request.tools
        {
            for tool in tools {
                tool.strict = Some(strict);
            }
        }
        request.tool_choice = body
            .get("tool_choice")
            .filter(|value| !value.is_null())
            .map(decode_tool_choice)
            .transpose()?;
        request.response_format = body
            .get("response_format")
            .filter(|value| !value.is_null())
            .map(|format| match format.get("type").and_then(Value::as_str) {
                Some("text") => Ok(ResponseFormat::Text),
                Some("json_object") => match format.get("json_schema") {
                    Some(schema) => Ok(ResponseFormat::JsonSchema {
                        name: "response".into(),
                        description: None,
                        schema: schema.clone(),
                        strict: None,
                    }),
                    None => Ok(ResponseFormat::JsonObject),
                },
                _ => bail!("unsupported Cohere response format"),
            })
            .transpose()?;
        Ok(request)
    }

    fn encode_request(&self, mut request: AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        request.normalize_parameters();
        let mut body = Map::new();
        body.insert("model".into(), Value::String(request.model.clone()));
        body.insert("messages".into(), Value::Array(encode_messages(&request)?));
        body.insert("stream".into(), Value::Bool(request.stream.enabled));
        insert_number(&mut body, "temperature", request.generation.temperature);
        insert_u32(&mut body, "max_tokens", request.generation.max_tokens);
        insert_number(&mut body, "p", request.generation.top_p);
        insert_u32(&mut body, "k", request.parameters.top_k);
        if request.parameters.logprobs.is_some() {
            body.insert("logprobs".into(), Value::Bool(true));
        }
        let documents = encode_documents(&request);
        if !documents.is_empty() {
            body.insert("documents".into(), Value::Array(documents));
        }
        insert_number(
            &mut body,
            "presence_penalty",
            request.generation.presence_penalty,
        );
        insert_number(
            &mut body,
            "frequency_penalty",
            request.generation.frequency_penalty,
        );
        if let Some(seed) = request.generation.seed {
            body.insert("seed".into(), Value::from(seed));
        }
        if let Some(stop) = &request.generation.stop {
            body.insert(
                "stop_sequences".into(),
                Value::Array(stop.iter().cloned().map(Value::String).collect()),
            );
        }
        if matches!(
            request.reasoning.target_control,
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled)
        ) || (request.reasoning.target_control.is_none()
            && request.reasoning.mode.as_deref() == Some("disabled"))
        {
            body.insert("thinking".into(), json!({"type": "disabled"}));
        } else if request.reasoning.is_enabled()
            || matches!(
                request.reasoning.target_control,
                Some(
                    stravia_runtime_contract::thinking::TargetThinkingControl::Enabled
                        | stravia_runtime_contract::thinking::TargetThinkingControl::Budget { .. }
                )
            )
        {
            let mut thinking = Map::from_iter([("type".into(), Value::String("enabled".into()))]);
            let budget = match request.reasoning.target_control {
                Some(stravia_runtime_contract::thinking::TargetThinkingControl::Budget {
                    value,
                }) => Some(value),
                _ => request.reasoning.budget_tokens,
            };
            if let Some(budget) = budget {
                thinking.insert("token_budget".into(), Value::from(budget));
            }
            body.insert("thinking".into(), Value::Object(thinking));
        }
        if let Some(response_format) = &request.response_format {
            body.insert(
                "response_format".into(),
                encode_response_format(response_format)?,
            );
        }

        let all_tools = request.tools.as_deref().unwrap_or(&[]);
        let selected_tool = match &request.tool_choice {
            Some(ToolChoice::Named { name }) => all_tools.iter().find(|tool| tool.name == *name),
            _ => None,
        };
        let selected_tools = selected_tool.map(std::slice::from_ref).unwrap_or(all_tools);
        let tools = encode_tools(selected_tools);
        if let Some(choice) = &request.tool_choice {
            match choice {
                ToolChoice::Auto => {}
                ToolChoice::None => {
                    body.insert("tool_choice".into(), Value::String("NONE".into()));
                }
                ToolChoice::Required => {
                    body.insert("tool_choice".into(), Value::String("REQUIRED".into()));
                }
                ToolChoice::Named { .. } => {
                    if selected_tool.is_some() {
                        body.insert("tool_choice".into(), Value::String("REQUIRED".into()));
                    }
                }
                ToolChoice::Allowed { .. } | ToolChoice::Native(_) => {}
            }
        }
        if !tools.is_empty() {
            // Cohere 只有请求级开关，不能让未声明 strict 的工具被隐式收紧。
            if selected_tools.iter().any(|tool| tool.strict.is_some()) {
                body.insert(
                    "strict_tools".into(),
                    Value::Bool(selected_tools.iter().all(|tool| tool.strict == Some(true))),
                );
            }
            body.insert("tools".into(), Value::Array(tools));
        }

        Ok((Value::Object(body), HeaderMap::new()))
    }

    fn request_path(&self, _model: &str, _stream: bool) -> String {
        "/chat".into()
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        let id = body
            .get("generation_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut response = AiResponse::new(id, "");
        let message = body
            .get("message")
            .context("Cohere response is missing message")?;
        for content in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match content.get("type").and_then(Value::as_str) {
                Some("text") => response.push_output_text(required_string(content, "text")?),
                Some("thinking") => {
                    response.push_reasoning(required_string(content, "thinking")?, None)
                }
                Some(other) => bail!("unsupported Cohere response content type `{other}`"),
                None => bail!("Cohere response content is missing type"),
            }
        }
        for tool in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let function = tool
                .get("function")
                .context("Cohere tool call is missing function")?;
            response.push_tool_call(ToolCall {
                id: (required_string(tool, "id")?).into(),
                name: required_string(function, "name")?,
                arguments: cohere_tool_arguments(function)?,
            });
        }
        response.stop_reason = body
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(map_finish_reason)
            .map(str::to_string);
        response.usage = cohere_usage(body.pointer("/usage/tokens"));
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
                                content.push(json!({"type": "thinking", "thinking": thinking}));
                            }
                            ContentBlock::Reasoning {
                                summary,
                                content: segments,
                                ..
                            } => {
                                // 受保护的 encrypted_content 无载体，只输出明文段。
                                for text in summary
                                    .iter()
                                    .chain(segments)
                                    .filter(|text| !text.is_empty())
                                {
                                    content.push(json!({"type": "thinking", "thinking": text}));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        json!({
            "generation_id": response.id,
            "message": {
                "role": "assistant",
                "content": content,
                "tool_calls": response.tool_calls().map(|call| json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                })).collect::<Vec<_>>(),
            },
            "finish_reason": response.stop_reason.as_deref().unwrap_or("COMPLETE"),
            "usage": {"tokens": {
                "input_tokens": response.usage.prompt_tokens,
                "output_tokens": response.usage.completion_tokens,
            }},
        })
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::custom_text(CohereStreamParser::new()))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: COHERE_CHAT_V2,
            operation: "ingress stream encoding",
        })
    }
}

pub struct CohereStreamParser {
    buffer: String,
    done: bool,
    next_tool_index: usize,
    tools: BTreeMap<usize, ToolCall>,
}

impl CohereStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            done: false,
            next_tool_index: 0,
            tools: BTreeMap::new(),
        }
    }

    pub fn parse_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let block = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            self.parse_event(&block, &mut deltas)?;
        }
        Ok(deltas)
    }

    pub fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        let mut deltas = Vec::new();
        if !self.buffer.trim().is_empty() {
            let block = std::mem::take(&mut self.buffer);
            self.parse_event(&block, &mut deltas)?;
        }
        if !self.done {
            deltas.push(AiStreamDelta::UnexpectedEof);
        }
        Ok(deltas)
    }

    fn parse_event(&mut self, block: &str, deltas: &mut Vec<AiStreamDelta>) -> anyhow::Result<()> {
        let event = block.lines().find_map(|line| line.strip_prefix("event: "));
        let payload = block
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect::<Vec<_>>()
            .join("\n");
        if payload.trim().is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_str(&payload).context("parse Cohere stream event")?;
        let event_type = event
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        match event_type {
            "message-start" => {
                let id = value.get("id").and_then(Value::as_str).unwrap_or_default();
                deltas.push(AiStreamDelta::MessageStart {
                    id: id.into(),
                    model: String::new(),
                });
            }
            "content-delta" => {
                let content = value
                    .pointer("/delta/message/content")
                    .context("Cohere content delta is missing content")?;
                match content.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        deltas.push(AiStreamDelta::TextDelta(required_string(content, "text")?))
                    }
                    Some("thinking") => deltas.push(AiStreamDelta::ThinkingDelta(required_string(
                        content, "thinking",
                    )?)),
                    Some(other) => bail!("unsupported Cohere stream content type `{other}`"),
                    None => bail!("Cohere stream content is missing type"),
                }
            }
            "tool-call-start" => {
                let tool = value
                    .pointer("/delta/message/tool_calls")
                    .context("Cohere tool start is missing tool call")?;
                let index = self.next_tool_index;
                self.next_tool_index += 1;
                let call = ToolCall {
                    id: (required_string(tool, "id")?).into(),
                    name: required_string(
                        tool.pointer("/function")
                            .context("Cohere tool start is missing function")?,
                        "name",
                    )?,
                    arguments: tool
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                };
                deltas.push(AiStreamDelta::ToolCallStart {
                    index,
                    id: call.id.to_string(),
                    name: call.name.clone(),
                });
                if !call.arguments.is_empty() {
                    deltas.push(AiStreamDelta::ToolCallDelta {
                        index,
                        arguments: call.arguments.clone(),
                    });
                }
                self.tools.insert(index, call);
            }
            "tool-call-delta" => {
                let tool = value
                    .pointer("/delta/message/tool_calls")
                    .context("Cohere tool delta is missing tool call")?;
                let delta = tool
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(index) = self.tools.last_key_value().map(|(index, _)| *index) else {
                    bail!("Cohere sent a tool delta before tool-call-start");
                };
                if !delta.is_empty() {
                    self.tools
                        .get_mut(&index)
                        .expect("existing Cohere tool")
                        .arguments
                        .push_str(delta);
                    deltas.push(AiStreamDelta::ToolCallDelta {
                        index,
                        arguments: delta.into(),
                    });
                }
            }
            "tool-call-end" => {
                let Some((index, call)) = self.tools.pop_last() else {
                    bail!("Cohere sent a tool end before tool-call-start");
                };
                deltas.push(AiStreamDelta::ToolCallComplete {
                    index,
                    tool_call: call,
                });
            }
            "message-end" => {
                let finish = value
                    .pointer("/delta/finish_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("COMPLETE");
                let usage = cohere_usage(value.pointer("/delta/usage/tokens"));
                if usage.required_components_known {
                    deltas.push(AiStreamDelta::Usage(usage));
                }
                self.done = true;
                deltas.push(AiStreamDelta::Done {
                    stop_reason: map_finish_reason(finish).into(),
                });
            }
            "citation-start" | "citation-end" | "content-start" | "content-end" => {}
            other => deltas.push(AiStreamDelta::Unknown {
                raw: format!("{other}: {payload}"),
            }),
        }
        Ok(())
    }
}

impl TextWireStreamParser for CohereStreamParser {
    fn parse_text_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.parse_chunk(raw)
    }

    fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.finish()
    }
}

impl Default for CohereStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

fn encode_messages(request: &AiRequest) -> anyhow::Result<Vec<Value>> {
    let mut messages = Vec::new();
    for item in &request.items {
        if let Some(message) = encode_message(item)? {
            messages.push(message);
        }
    }
    Ok(messages)
}

/// `None` 表示该条目编码后没有任何线上内容（如只含无法承载的受保护推理
/// 载荷、且无 tool call 的 assistant 条目）；上游不接受空 assistant 消息，
/// 必须整条跳过。被跳过的条目必然没有 tool call，不会破坏 tool 结果配对。
fn encode_message(item: &AiItem) -> anyhow::Result<Option<Value>> {
    let role = match item.role {
        Role::System | Role::Developer => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut message = Map::from_iter([("role".into(), Value::String(role.into()))]);
    match item.role {
        Role::Assistant => {
            let calls = encode_tool_calls(item);
            if calls.is_empty() {
                let Some(content) = encode_assistant_content(&item.content)? else {
                    return Ok(None);
                };
                message.insert("content".into(), content);
            } else {
                message.insert("tool_calls".into(), Value::Array(calls));
            }
        }
        Role::Tool => {
            let content = encode_tool_result_content(&item.content)?;
            if content.is_empty() {
                return Ok(None);
            }
            message.insert("content".into(), Value::String(content));
            let Some(id) = item.tool_call_id.as_deref() else {
                return Ok(None);
            };
            message.insert("tool_call_id".into(), Value::String(id.into()));
        }
        Role::User => {
            let content = encode_user_content(&item.content)?;
            if content.as_str().is_some_and(str::is_empty)
                || content.as_array().is_some_and(Vec::is_empty)
            {
                return Ok(None);
            }
            message.insert("content".into(), content);
        }
        Role::System | Role::Developer => {
            let content = encode_text_content(&item.content)?;
            if content.is_empty() {
                return Ok(None);
            }
            message.insert("content".into(), Value::String(content));
        }
    };
    Ok(Some(Value::Object(message)))
}

fn encode_assistant_content(content: &MessageContent) -> anyhow::Result<Option<Value>> {
    match content {
        MessageContent::Text(text) => {
            Ok((!text.is_empty()).then(|| Value::String(text.as_ref().clone())))
        }
        MessageContent::Blocks(blocks) => {
            let has_reasoning = blocks.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Thinking { .. }
                        | ContentBlock::Reasoning { .. }
                        | ContentBlock::RedactedThinking { .. }
                )
            });
            if !has_reasoning {
                let text = encode_text_content(content)?;
                return Ok((!text.is_empty()).then_some(Value::String(text)));
            }

            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } if !text.is_empty() => {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    // Cohere 的 thinking 部分只有明文字段：signature /
                    // encrypted_content 无原生载体，静默忽略而不是写进正文。
                    ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                        parts.push(json!({"type": "thinking", "thinking": thinking}));
                    }
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::Reasoning {
                        summary, content, ..
                    } => {
                        for text in summary
                            .iter()
                            .chain(content)
                            .filter(|text| !text.is_empty())
                        {
                            parts.push(json!({"type": "thinking", "thinking": text}));
                        }
                    }
                    // redacted_thinking 是纯密文载荷，Cohere 没有对应载体。
                    ContentBlock::RedactedThinking { .. } => {}
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {}
                    _ => {}
                }
            }
            Ok((!parts.is_empty()).then_some(Value::Array(parts)))
        }
    }
}

fn encode_user_content(content: &MessageContent) -> anyhow::Result<Value> {
    match content {
        MessageContent::Text(text) => Ok(Value::String(text.as_ref().clone())),
        MessageContent::Blocks(blocks) => {
            if blocks
                .iter()
                .all(|block| matches!(block, ContentBlock::Text { .. }))
            {
                return Ok(Value::String(
                    blocks.iter().filter_map(ContentBlock::as_text).collect(),
                ));
            }
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => {
                        parts.push(json!({"type": "text", "text": text}))
                    }
                    ContentBlock::Image { source, .. } => {
                        let url = match source {
                            MediaSource::Url(url) => url.clone(),
                            MediaSource::Base64 { media_type, data } => {
                                format!("data:{media_type};base64,{data}")
                            }
                            MediaSource::FileId { .. } => continue,
                        };
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                    _ => {}
                }
            }
            Ok(Value::Array(parts))
        }
    }
}

fn encode_text_content(content: &MessageContent) -> anyhow::Result<String> {
    match content {
        MessageContent::Text(text) => Ok(text.as_ref().clone()),
        MessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => out.push_str(text),
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {}
                    _ => {}
                }
            }
            Ok(out)
        }
    }
}

fn encode_tool_result_content(content: &MessageContent) -> anyhow::Result<String> {
    match content {
        MessageContent::Text(text) => Ok(text.as_ref().clone()),
        MessageContent::Blocks(blocks) => {
            let mut values = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => values.push(text.as_ref().clone()),
                    ContentBlock::ToolResult {
                        content,
                        content_kind,
                        ..
                    } => {
                        values.push(super::tool_result_text_value(content, *content_kind)?);
                    }
                    _ => {}
                }
            }
            Ok(values.join(""))
        }
    }
}

fn encode_tool_calls(item: &AiItem) -> Vec<Value> {
    item.tool_calls
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|call| {
            json!({
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": call.arguments},
            })
        })
        .collect()
}

fn decode_documents(body: &Value) -> Vec<ContentBlock> {
    body.get("documents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|document| {
            let data = document.get("data")?;
            let text = data.as_str().or_else(|| data.get("text")?.as_str())?;
            Some(ContentBlock::Document {
                source: DocumentSource::PlainText { data: text.into() },
                title: data.get("title").and_then(Value::as_str).map(str::to_owned),
                context: None,
                cache_control: None,
            })
        })
        .collect()
}

fn encode_documents(request: &AiRequest) -> Vec<Value> {
    request
        .items
        .iter()
        .filter(|item| item.role == Role::User)
        .flat_map(|item| match &item.content {
            MessageContent::Blocks(blocks) => blocks.as_slice(),
            MessageContent::Text(_) => &[],
        })
        .filter_map(|block| {
            let ContentBlock::Document {
                source: DocumentSource::PlainText { data },
                title,
                ..
            } = block
            else {
                return None;
            };
            let mut document = Map::from_iter([("text".into(), Value::String(data.clone()))]);
            if let Some(title) = title {
                document.insert("title".into(), Value::String(title.clone()));
            }
            Some(json!({"data": document}))
        })
        .collect()
}

fn encode_tools(tools: &[stravia_runtime_contract::protocol::ir::ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut function = Map::from_iter([
                ("name".into(), Value::String(tool.name.clone())),
                ("parameters".into(), tool.parameters.clone()),
            ]);
            if let Some(description) = &tool.description {
                function.insert("description".into(), Value::String(description.clone()));
            }
            json!({
                "type": "function",
                "function": function,
            })
        })
        .collect()
}

fn decode_messages(body: &Value) -> anyhow::Result<Vec<AiItem>> {
    body.get("messages")
        .and_then(Value::as_array)
        .context("Cohere request is missing messages")?
        .iter()
        .map(|message| {
            let role = match required_string(message, "role")?.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                other => bail!("unsupported Cohere message role `{other}`"),
            };
            let content = match message.get("content") {
                Some(Value::String(text)) => MessageContent::Text(text.clone().into()),
                Some(Value::Array(parts)) => MessageContent::Blocks(
                    parts
                        .iter()
                        .map(|part| match part.get("type").and_then(Value::as_str) {
                            Some("text") => Ok(ContentBlock::Text {
                                text: required_string(part, "text")?.into(),
                                cache_control: None,
                            }),
                            Some("thinking") if role == Role::Assistant => {
                                Ok(ContentBlock::Thinking {
                                    thinking: required_string(part, "thinking")?,
                                    signature: None,
                                })
                            }
                            Some("image_url") => Ok(ContentBlock::Image {
                                source: MediaSource::Url(required_string(
                                    part.pointer("/image_url")
                                        .context("Cohere image is missing image_url")?,
                                    "url",
                                )?),
                                detail: part
                                    .pointer("/image_url/detail")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                cache_control: None,
                            }),
                            Some(other) => {
                                bail!("unsupported Cohere message content type `{other}`")
                            }
                            None => bail!("Cohere message content is missing type"),
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?,
                ),
                None if role == Role::Assistant => MessageContent::Text(String::new().into()),
                _ => bail!("Cohere message has invalid content"),
            };
            let tool_calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .map(|call| {
                            let function = call
                                .get("function")
                                .context("Cohere tool call is missing function")?;
                            Ok(ToolCall {
                                id: (required_string(call, "id")?).into(),
                                name: required_string(function, "name")?,
                                arguments: cohere_tool_arguments(function)?,
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()
                })
                .transpose()?;
            Ok(AiItem {
                role,
                content,
                tool_calls,
                tool_call_id: (message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string))
                .map(Into::into),
                meta: None,
            }
            .with_plain_tool_text_kind())
        })
        .collect()
}

fn cohere_tool_arguments(function: &Value) -> anyhow::Result<String> {
    match function.get("arguments") {
        Some(Value::Null) => Ok("{}".into()),
        Some(Value::String(value)) if value == "null" => Ok("{}".into()),
        Some(Value::String(value)) => Ok(value.clone()),
        _ => bail!("Cohere tool call is missing string `arguments`"),
    }
}

fn decode_tools(
    value: &Value,
) -> anyhow::Result<Vec<stravia_runtime_contract::protocol::ir::ToolSpec>> {
    value
        .as_array()
        .context("Cohere tools must be an array")?
        .iter()
        .map(|tool| {
            let function = tool
                .get("function")
                .context("Cohere tool is missing function")?;
            Ok(stravia_runtime_contract::protocol::ir::ToolSpec {
                name: required_string(function, "name")?,
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                strict: None,
                defer_loading: None,
            })
        })
        .collect()
}

fn decode_tool_choice(value: &Value) -> anyhow::Result<ToolChoice> {
    match value.as_str() {
        None => bail!("Cohere tool_choice must be a string"),
        Some("NONE") => Ok(ToolChoice::None),
        Some("REQUIRED") => Ok(ToolChoice::Required),
        Some("AUTO") => Ok(ToolChoice::Auto),
        Some(other) => bail!("unsupported Cohere tool_choice `{other}`"),
    }
}

fn encode_response_format(value: &ResponseFormat) -> anyhow::Result<Value> {
    match value {
        ResponseFormat::Text => Ok(json!({"type": "text"})),
        ResponseFormat::JsonObject => Ok(json!({"type": "json_object"})),
        ResponseFormat::JsonSchema { schema, .. } => {
            Ok(json!({"type": "json_object", "json_schema": schema}))
        }
    }
}

fn cohere_usage(value: Option<&Value>) -> Usage {
    let prompt_tokens = value
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    let completion_tokens = value
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
        required_components_known: value.is_some(),
        ..Usage::default()
    }
}

fn map_finish_reason(reason: &str) -> &str {
    match reason {
        "COMPLETE" | "STOP_SEQUENCE" => "stop",
        "MAX_TOKENS" => "length",
        "TOOL_CALL" => "tool_calls",
        "ERROR" => "error",
        _ => "other",
    }
}

fn required_string(value: &Value, key: &str) -> anyhow::Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("missing string `{key}`"))
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

fn insert_number(object: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(value) = value {
        object.insert(key.into(), Value::from(value));
    }
}

fn insert_u32(object: &mut Map<String, Value>, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        object.insert(key.into(), Value::from(value));
    }
}

#[cfg(test)]
mod tests;
