//! Cohere Chat API v2 (`POST /v2/chat`) egress codec.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

use crate::protocol::ids::{COHERE_CHAT_V2, EndpointCapabilities, ProtocolEndpoint};
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, AiStreamDelta, ContentBlock, MediaSource, MessageContent,
    ResponseFormat, Role, ToolCall, ToolChoice, Usage,
};
use crate::protocol::registry::EndpointRegistration;
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};

pub struct CohereChatV2;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[],
    multimodal: true,
    structured_output: true,
    function_calling: true,
    parallel_tool_calls: false,
    extended_reasoning: true,
    deterministic_seed: true,
    stream: crate::protocol::ids::StreamCaps {
        server_sent_events: true,
        usage_in_stream: true,
        requires_stream_flag: true,
    },
    unknown_field_policy: crate::protocol::ids::VendorFieldPolicy::Drop,
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
        request.generation.stop = body.get("stop_sequences").and_then(string_array);
        request.tools = body.get("tools").map(decode_tools).transpose()?;
        request.tool_choice = body
            .get("tool_choice")
            .map(decode_tool_choice)
            .transpose()?;
        Ok(request)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        let mut body = Map::new();
        body.insert("model".into(), Value::String(request.model.clone()));
        body.insert("messages".into(), Value::Array(encode_messages(request)?));
        body.insert("stream".into(), Value::Bool(request.stream.enabled));
        insert_number(&mut body, "temperature", request.generation.temperature);
        insert_u32(&mut body, "max_tokens", request.generation.max_tokens);
        insert_number(&mut body, "p", request.generation.top_p);
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
        if request.reasoning.enabled {
            let mut thinking = Map::from_iter([("type".into(), Value::String("enabled".into()))]);
            if let Some(budget) = request.reasoning.budget_tokens {
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

        let mut tools = encode_tools(request.tools.as_deref().unwrap_or(&[]));
        if let Some(choice) = &request.tool_choice {
            match choice {
                ToolChoice::Auto => {}
                ToolChoice::None => {
                    body.insert("tool_choice".into(), Value::String("NONE".into()));
                }
                ToolChoice::Required => {
                    body.insert("tool_choice".into(), Value::String("REQUIRED".into()));
                }
                ToolChoice::Named { name } => {
                    tools.retain(|tool| {
                        tool.pointer("/function/name")
                            .and_then(Value::as_str)
                            .is_some_and(|candidate| candidate == name)
                    });
                    if tools.is_empty() {
                        bail!("Cohere tool_choice names undeclared tool `{name}`");
                    }
                    body.insert("tool_choice".into(), Value::String("REQUIRED".into()));
                }
                ToolChoice::Raw(_) => bail!("Cohere Chat cannot represent raw tool_choice"),
            }
        }
        if !tools.is_empty() {
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
                id: required_string(tool, "id")?,
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
        json!({
            "generation_id": response.id,
            "message": {
                "role": "assistant",
                "content": response.output_texts().map(|text| json!({"type": "text", "text": text})).collect::<Vec<_>>(),
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
        Ok(WireStreamDecoder::Cohere(CohereStreamParser::new()))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: COHERE_CHAT_V2,
            operation: "ingress stream encoding",
        })
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(CohereChatV2) }
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

    pub(crate) fn parse_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let block = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            self.parse_event(&block, &mut deltas)?;
        }
        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
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
                    id: required_string(tool, "id")?,
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
                    id: call.id.clone(),
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

fn encode_messages(request: &AiRequest) -> anyhow::Result<Vec<Value>> {
    request.items.iter().map(encode_message).collect()
}

fn encode_message(item: &AiItem) -> anyhow::Result<Value> {
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
                let content = encode_text_content(&item.content)?;
                if !content.is_empty() {
                    message.insert("content".into(), Value::String(content));
                }
            } else {
                message.insert("tool_calls".into(), Value::Array(calls));
            }
        }
        Role::Tool => {
            message.insert(
                "content".into(),
                Value::String(encode_tool_result_content(&item.content)?),
            );
            let id = item
                .tool_call_id
                .as_deref()
                .context("Cohere tool message is missing tool_call_id")?;
            message.insert("tool_call_id".into(), Value::String(id.into()));
        }
        Role::User => {
            message.insert("content".into(), encode_user_content(&item.content)?);
        }
        Role::System | Role::Developer => {
            message.insert(
                "content".into(),
                Value::String(encode_text_content(&item.content)?),
            );
        }
    };
    Ok(Value::Object(message))
}

fn encode_user_content(content: &MessageContent) -> anyhow::Result<Value> {
    match content {
        MessageContent::Text(text) => Ok(Value::String(text.clone())),
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
                            MediaSource::FileId { .. } => {
                                bail!("Cohere cannot represent file-id images")
                            }
                        };
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                    other => bail!(
                        "Cohere cannot represent user content block `{}`",
                        content_block_name(other)
                    ),
                }
            }
            Ok(Value::Array(parts))
        }
    }
}

fn encode_text_content(content: &MessageContent) -> anyhow::Result<String> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => out.push_str(text),
                    ContentBlock::Thinking { thinking, .. } => out.push_str(thinking),
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {}
                    other => bail!(
                        "Cohere cannot represent content block `{}`",
                        content_block_name(other)
                    ),
                }
            }
            Ok(out)
        }
    }
}

fn encode_tool_result_content(content: &MessageContent) -> anyhow::Result<String> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Blocks(blocks) => {
            let mut values = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => values.push(text.clone()),
                    ContentBlock::ToolResult { content, .. } => values.push(match content {
                        Value::String(text) => text.clone(),
                        other => serde_json::to_string(other)?,
                    }),
                    other => bail!(
                        "Cohere cannot represent tool content block `{}`",
                        content_block_name(other)
                    ),
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

fn encode_tools(tools: &[crate::protocol::ir::ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
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
                Some(Value::String(text)) => MessageContent::Text(text.clone()),
                Some(Value::Array(parts)) => MessageContent::Blocks(
                    parts
                        .iter()
                        .map(|part| match part.get("type").and_then(Value::as_str) {
                            Some("text") => Ok(ContentBlock::Text {
                                text: required_string(part, "text")?,
                                cache_control: None,
                            }),
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
                None if role == Role::Assistant => MessageContent::Text(String::new()),
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
                                id: required_string(call, "id")?,
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
                tool_call_id: message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                meta: None,
            })
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

fn decode_tools(value: &Value) -> anyhow::Result<Vec<crate::protocol::ir::ToolSpec>> {
    value
        .as_array()
        .context("Cohere tools must be an array")?
        .iter()
        .map(|tool| {
            let function = tool
                .get("function")
                .context("Cohere tool is missing function")?;
            Ok(crate::protocol::ir::ToolSpec {
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
                cache_control: None,
                meta: None,
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
        ResponseFormat::Text => bail!("Cohere does not accept an explicit text response format"),
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

fn content_block_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::Image { .. } => "image",
        ContentBlock::Audio { .. } => "audio",
        ContentBlock::File { .. } => "file",
        ContentBlock::Video { .. } => "video",
        ContentBlock::Thinking { .. } => "thinking",
        ContentBlock::Reasoning { .. } => "reasoning",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_cohere_tool_schema_without_openai_shape() {
        let mut request = AiRequest::new("command-a", vec![AiItem::output_text("hello")]);
        request.items[0].role = Role::User;
        request.tools = Some(vec![crate::protocol::ir::ToolSpec {
            name: "weather".into(),
            description: Some("Get weather".into()),
            parameters: json!({"type": "object"}),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        let (body, _) = CohereChatV2.encode_request(&request).unwrap();
        assert_eq!(body["tools"][0]["function"]["name"], "weather");
        assert!(body.get("messages").is_some());
        assert!(body.get("choices").is_none());
    }

    #[test]
    fn normalizes_cohere_null_tool_arguments() {
        let response = CohereChatV2
            .decode_response(json!({
                "generation_id": "gen_1",
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "weather", "arguments": "null"}
                    }]
                },
                "finish_reason": "TOOL_CALL",
                "usage": {"tokens": {"input_tokens": 3, "output_tokens": 4}}
            }))
            .unwrap();

        assert_eq!(response.tool_calls().next().unwrap().arguments, "{}");
    }

    #[test]
    fn omits_assistant_text_when_replaying_cohere_tool_calls() {
        let item = AiItem {
            role: Role::Assistant,
            content: MessageContent::Text("I will call weather.".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                name: "weather".into(),
                arguments: "{}".into(),
            }]),
            tool_call_id: None,
            meta: None,
        };

        let message = encode_message(&item).unwrap();
        assert!(message.get("content").is_none());
        assert_eq!(message["tool_calls"][0]["function"]["name"], "weather");
    }
}
