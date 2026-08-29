//! Amazon Bedrock Converse API egress codec.
//!
//! Converse is JSON for unary calls and AWS Event Stream frames for streaming.
//! The frame decoder keeps bytes intact until it has validated one complete AWS
//! message; converting chunks to UTF-8 first corrupts the length-prefixed wire.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

use crate::protocol::ids::{
    BEDROCK_CONVERSE_V1, EndpointCapabilities, ProtocolEndpoint, StreamCaps, VendorFieldPolicy,
};
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, AiStreamDelta, ContentBlock, MediaSource, MessageContent, Role,
    ToolCall, ToolChoice, ToolSpec, Usage,
};
use crate::protocol::registry::EndpointRegistration;
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};

pub struct BedrockConverseV1;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: false,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[],
    multimodal: true,
    structured_output: false,
    function_calling: true,
    parallel_tool_calls: false,
    extended_reasoning: false,
    deterministic_seed: false,
    stream: StreamCaps {
        server_sent_events: false,
        usage_in_stream: true,
        requires_stream_flag: false,
    },
    unknown_field_policy: VendorFieldPolicy::Drop,
};

impl ProtocolAdapter for BedrockConverseV1 {
    fn id(&self) -> ProtocolEndpoint {
        BEDROCK_CONVERSE_V1
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        let mut request = AiRequest::new(
            body.get("modelId")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            decode_messages(&body)?,
        );
        request.instructions = body.get("system").map(decode_system).transpose()?;
        if let Some(config) = body.get("inferenceConfig") {
            request.generation.temperature = config.get("temperature").and_then(Value::as_f64);
            request.generation.top_p = config.get("topP").and_then(Value::as_f64);
            request.generation.max_tokens = config
                .get("maxTokens")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            request.generation.stop = config.get("stopSequences").and_then(string_array);
        }
        if let Some(tool_config) = body.get("toolConfig") {
            request.tools = tool_config.get("tools").map(decode_tools).transpose()?;
            request.tool_choice = tool_config
                .get("toolChoice")
                .map(decode_tool_choice)
                .transpose()?;
        }
        Ok(request)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        if request.generation.seed.is_some()
            || request.generation.frequency_penalty.is_some()
            || request.generation.presence_penalty.is_some()
        {
            bail!("Bedrock Converse cannot represent seed, frequency_penalty, or presence_penalty");
        }
        if request.reasoning.enabled {
            bail!(
                "Bedrock Converse reasoning requires model-specific additionalModelRequestFields"
            );
        }
        if request.response_format.is_some() {
            bail!("Bedrock Converse does not expose a model-neutral response format");
        }
        let mut body =
            Map::from_iter([("messages".into(), Value::Array(encode_messages(request)?))]);
        if let Some(instructions) = &request.instructions
            && !instructions.trim().is_empty()
        {
            body.insert(
                "system".into(),
                Value::Array(vec![json!({"text": instructions})]),
            );
        }
        let inference_config = encode_inference_config(request);
        if !inference_config.is_empty() {
            body.insert("inferenceConfig".into(), Value::Object(inference_config));
        }
        if let Some(tool_config) = encode_tool_config(request)? {
            body.insert("toolConfig".into(), tool_config);
        }
        Ok((Value::Object(body), HeaderMap::new()))
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        let action = if stream {
            "converse-stream"
        } else {
            "converse"
        };
        format!("/model/{model}/{action}")
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        let mut response = AiResponse::new("", "");
        let message = body
            .pointer("/output/message")
            .context("Bedrock response is missing output.message")?;
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match decode_content_block(block)? {
                ContentBlock::Text { text, .. } => response.push_output_text(text),
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => response.push_reasoning(thinking, signature),
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => response.push_tool_call(ToolCall {
                    id,
                    name,
                    arguments: serde_json::to_string(&input)?,
                }),
                other => bail!(
                    "unsupported Bedrock output block `{}`",
                    content_block_name(&other)
                ),
            }
        }
        response.stop_reason = body
            .get("stopReason")
            .and_then(Value::as_str)
            .map(map_finish_reason)
            .map(str::to_string);
        response.usage = bedrock_usage(body.get("usage"));
        Ok(response)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        let mut content: Vec<Value> = response
            .output_texts()
            .map(|text| json!({"text": text}))
            .collect();
        content.extend(response.tool_calls().map(|call| json!({
            "toolUse": {
                "toolUseId": call.id,
                "name": call.name,
                "input": serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|_| Value::String(call.arguments.clone())),
            }
        })));
        json!({
            "output": {"message": {"role": "assistant", "content": content}},
            "stopReason": response.stop_reason.as_deref().unwrap_or("end_turn"),
            "usage": {
                "inputTokens": response.usage.prompt_tokens,
                "outputTokens": response.usage.completion_tokens,
                "totalTokens": response.usage.total_tokens,
            },
        })
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::Bedrock(BedrockStreamParser::new()))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: BEDROCK_CONVERSE_V1,
            operation: "ingress stream encoding",
        })
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(BedrockConverseV1) }
}

pub struct BedrockStreamParser {
    buffer: Vec<u8>,
    done: bool,
    started: bool,
    tools: BTreeMap<usize, ToolCall>,
}

impl BedrockStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            done: false,
            started: false,
            tools: BTreeMap::new(),
        }
    }

    pub(crate) fn parse_chunk(&mut self, raw: &[u8]) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.buffer.extend_from_slice(raw);
        let mut deltas = Vec::new();
        loop {
            let Some(frame) = take_event_frame(&mut self.buffer)? else {
                break;
            };
            self.parse_event(&frame, &mut deltas)?;
        }
        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        if !self.buffer.is_empty() {
            bail!("incomplete Bedrock Event Stream frame at end of response");
        }
        Ok((!self.done)
            .then_some(AiStreamDelta::UnexpectedEof)
            .into_iter()
            .collect())
    }

    fn parse_event(
        &mut self,
        payload: &Value,
        deltas: &mut Vec<AiStreamDelta>,
    ) -> anyhow::Result<()> {
        if !self.started && payload.get("messageStart").is_some() {
            self.started = true;
            deltas.push(AiStreamDelta::MessageStart {
                id: String::new(),
                model: String::new(),
            });
            return Ok(());
        }
        if let Some(start) = payload.get("contentBlockStart") {
            let index = required_usize(start, "contentBlockIndex")?;
            if let Some(tool_use) = start.pointer("/start/toolUse") {
                let call = ToolCall {
                    id: required_string(tool_use, "toolUseId")?,
                    name: required_string(tool_use, "name")?,
                    arguments: String::new(),
                };
                deltas.push(AiStreamDelta::ToolCallStart {
                    index,
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
                self.tools.insert(index, call);
            }
            return Ok(());
        }
        if let Some(delta) = payload.get("contentBlockDelta") {
            let index = required_usize(delta, "contentBlockIndex")?;
            let content = delta
                .get("delta")
                .context("Bedrock content delta is missing delta")?;
            if let Some(text) = content.get("text").and_then(Value::as_str) {
                deltas.push(AiStreamDelta::TextDelta(text.into()));
            } else if let Some(reasoning) = content
                .pointer("/reasoningContent/text")
                .and_then(Value::as_str)
            {
                deltas.push(AiStreamDelta::ThinkingDelta(reasoning.into()));
            } else if let Some(input) = content.pointer("/toolUse/input").and_then(Value::as_str) {
                let call = self
                    .tools
                    .get_mut(&index)
                    .context("Bedrock sent tool input before tool start")?;
                call.arguments.push_str(input);
                deltas.push(AiStreamDelta::ToolCallDelta {
                    index,
                    arguments: input.into(),
                });
            } else {
                bail!("unsupported Bedrock content delta");
            }
            return Ok(());
        }
        if let Some(stop) = payload.get("contentBlockStop") {
            let index = required_usize(stop, "contentBlockIndex")?;
            if let Some(call) = self.tools.remove(&index) {
                deltas.push(AiStreamDelta::ToolCallComplete {
                    index,
                    tool_call: call,
                });
            }
            return Ok(());
        }
        if let Some(stop) = payload.get("messageStop") {
            self.done = true;
            deltas.push(AiStreamDelta::Done {
                stop_reason: stop
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(map_finish_reason)
                    .unwrap_or("stop")
                    .into(),
            });
            return Ok(());
        }
        if let Some(metadata) = payload.get("metadata") {
            let usage = bedrock_usage(metadata.get("usage"));
            if usage.required_components_known {
                deltas.push(AiStreamDelta::Usage(usage));
            }
            return Ok(());
        }
        if payload.get("internalServerException").is_some()
            || payload.get("modelStreamErrorException").is_some()
            || payload.get("validationException").is_some()
            || payload.get("throttlingException").is_some()
        {
            bail!("Bedrock streaming error: {payload}");
        }
        deltas.push(AiStreamDelta::Unknown {
            raw: payload.to_string(),
        });
        Ok(())
    }
}

fn take_event_frame(buffer: &mut Vec<u8>) -> anyhow::Result<Option<Value>> {
    const PRELUDE_SIZE: usize = 12;
    const MESSAGE_CRC_SIZE: usize = 4;
    const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
    if buffer.len() < PRELUDE_SIZE {
        return Ok(None);
    }
    let total_length =
        u32::from_be_bytes(buffer[0..4].try_into().expect("four-byte frame length")) as usize;
    let header_length =
        u32::from_be_bytes(buffer[4..8].try_into().expect("four-byte header length")) as usize;
    if !(PRELUDE_SIZE + MESSAGE_CRC_SIZE..=MAX_FRAME_SIZE).contains(&total_length) {
        bail!("invalid Bedrock Event Stream frame length {total_length}");
    }
    if header_length > total_length.saturating_sub(PRELUDE_SIZE + MESSAGE_CRC_SIZE) {
        bail!("invalid Bedrock Event Stream header length {header_length}");
    }
    if buffer.len() < total_length {
        return Ok(None);
    }
    let frame: Vec<u8> = buffer.drain(..total_length).collect();
    let payload_start = PRELUDE_SIZE + header_length;
    let payload_end = total_length - MESSAGE_CRC_SIZE;
    let payload: Value = serde_json::from_slice(&frame[payload_start..payload_end])
        .context("parse Bedrock Event Stream JSON payload")?;
    Ok(Some(payload))
}

fn encode_messages(request: &AiRequest) -> anyhow::Result<Vec<Value>> {
    let mut messages = Vec::new();
    for item in &request.items {
        match item.role {
            Role::System => bail!("Bedrock system messages must be supplied through instructions"),
            Role::Developer => bail!("Bedrock Converse cannot represent developer messages"),
            Role::User => messages
                .push(json!({"role": "user", "content": encode_user_content(&item.content)?})),
            Role::Assistant => messages
                .push(json!({"role": "assistant", "content": encode_assistant_content(item)?})),
            Role::Tool => {
                messages.push(json!({"role": "user", "content": encode_tool_result(item)?}))
            }
        }
    }
    Ok(messages)
}

fn encode_user_content(content: &MessageContent) -> anyhow::Result<Vec<Value>> {
    match content {
        MessageContent::Text(text) => Ok(vec![json!({"text": text})]),
        MessageContent::Blocks(blocks) => blocks.iter().map(|block| match block {
            ContentBlock::Text { text, .. } => Ok(json!({"text": text})),
            ContentBlock::Image { source, .. } => match source {
                MediaSource::Base64 { media_type, data } => Ok(json!({"image": {"format": bedrock_image_format(media_type)?, "source": {"bytes": data}}})),
                MediaSource::Url(_) | MediaSource::FileId { .. } => bail!("Bedrock Converse accepts only inline image bytes"),
            },
            ContentBlock::ToolResult { tool_use_id, content, is_error, .. } => Ok(json!({"toolResult": {
                "toolUseId": tool_use_id,
                "content": [{"text": value_as_text(content)?}],
                "status": is_error.is_some_and(|error| error).then_some("error"),
            }})),
            other => bail!("Bedrock cannot represent user content block `{}`", content_block_name(other)),
        }).collect(),
    }
}

fn encode_assistant_content(item: &AiItem) -> anyhow::Result<Vec<Value>> {
    let mut content = match &item.content {
        MessageContent::Text(text) => vec![json!({"text": text})],
        MessageContent::Blocks(blocks) => blocks.iter().map(|block| match block {
            ContentBlock::Text { text, .. } => Ok(json!({"text": text})),
            ContentBlock::Thinking { thinking, signature: Some(signature) } => Ok(json!({"reasoningContent": {"reasoningText": {"text": thinking, "signature": signature}}})),
            ContentBlock::ToolUse { id, name, input, .. } => Ok(json!({"toolUse": {"toolUseId": id, "name": name, "input": input}})),
            other => bail!("Bedrock cannot represent assistant content block `{}`", content_block_name(other)),
        }).collect::<anyhow::Result<Vec<_>>>()?,
    };
    content.extend(item.tool_calls.as_deref().unwrap_or(&[]).iter().map(|call| json!({"toolUse": {
        "toolUseId": call.id,
        "name": call.name,
        "input": serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|_| Value::String(call.arguments.clone())),
    }})));
    Ok(content)
}

fn encode_tool_result(item: &AiItem) -> anyhow::Result<Vec<Value>> {
    let tool_use_id = item
        .tool_call_id
        .as_deref()
        .context("Bedrock tool message is missing tool_call_id")?;
    Ok(vec![json!({"toolResult": {
        "toolUseId": tool_use_id,
        "content": [{"text": value_as_text(&match &item.content { MessageContent::Text(text) => Value::String(text.clone()), MessageContent::Blocks(blocks) => Value::Array(blocks.iter().filter_map(ContentBlock::as_text).map(|text| Value::String(text.into())).collect()) })?}],
    }} )])
}

fn encode_inference_config(request: &AiRequest) -> Map<String, Value> {
    let mut config = Map::new();
    if let Some(value) = request.generation.max_tokens {
        config.insert("maxTokens".into(), Value::from(value));
    }
    if let Some(value) = request.generation.temperature {
        config.insert("temperature".into(), Value::from(value));
    }
    if let Some(value) = request.generation.top_p {
        config.insert("topP".into(), Value::from(value));
    }
    if let Some(value) = &request.generation.stop {
        config.insert(
            "stopSequences".into(),
            Value::Array(value.iter().cloned().map(Value::String).collect()),
        );
    }
    config
}

fn encode_tool_config(request: &AiRequest) -> anyhow::Result<Option<Value>> {
    let Some(tools) = request.tools.as_deref().filter(|tools| !tools.is_empty()) else {
        return Ok(None);
    };
    let mut tool_config = Map::from_iter([(
        "tools".into(),
        Value::Array(tools.iter().map(encode_tool).collect()),
    )]);
    if let Some(choice) = &request.tool_choice {
        let choice = match choice {
            ToolChoice::Auto => json!({"auto": {}}),
            ToolChoice::Required => json!({"any": {}}),
            ToolChoice::Named { name } => json!({"tool": {"name": name}}),
            ToolChoice::None => {
                bail!("Bedrock cannot represent tool_choice none while tools are present")
            }
            ToolChoice::Raw(_) => bail!("Bedrock cannot represent raw tool_choice"),
        };
        tool_config.insert("toolChoice".into(), choice);
    }
    Ok(Some(Value::Object(tool_config)))
}

fn encode_tool(tool: &ToolSpec) -> Value {
    json!({"toolSpec": {
        "name": tool.name,
        "description": tool.description,
        "inputSchema": {"json": tool.parameters},
    }})
}

fn decode_messages(body: &Value) -> anyhow::Result<Vec<AiItem>> {
    body.get("messages")
        .and_then(Value::as_array)
        .context("Bedrock request is missing messages")?
        .iter()
        .map(|message| {
            let role = match required_string(message, "role")?.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                other => bail!("unsupported Bedrock message role `{other}`"),
            };
            let blocks = message
                .get("content")
                .and_then(Value::as_array)
                .context("Bedrock message is missing content")?
                .iter()
                .map(decode_content_block)
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(AiItem {
                role,
                content: MessageContent::Blocks(blocks),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            })
        })
        .collect()
}

fn decode_system(value: &Value) -> anyhow::Result<String> {
    Ok(value
        .as_array()
        .context("Bedrock system must be an array")?
        .iter()
        .map(|part| required_string(part, "text"))
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("\n"))
}

fn decode_content_block(block: &Value) -> anyhow::Result<ContentBlock> {
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        return Ok(ContentBlock::Text {
            text: text.into(),
            cache_control: None,
        });
    }
    if let Some(tool) = block.get("toolUse") {
        return Ok(ContentBlock::ToolUse {
            id: required_string(tool, "toolUseId")?,
            name: required_string(tool, "name")?,
            input: tool.get("input").cloned().unwrap_or_else(|| json!({})),
            cache_control: None,
        });
    }
    if let Some(result) = block.get("toolResult") {
        return Ok(ContentBlock::ToolResult {
            tool_use_id: required_string(result, "toolUseId")?,
            content: result
                .get("content")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
            is_error: result
                .get("status")
                .and_then(Value::as_str)
                .map(|status| status == "error"),
            cache_control: None,
        });
    }
    if let Some(reasoning) = block.pointer("/reasoningContent/reasoningText") {
        return Ok(ContentBlock::Thinking {
            thinking: required_string(reasoning, "text")?,
            signature: reasoning
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    bail!("unsupported Bedrock content block")
}

fn decode_tools(value: &Value) -> anyhow::Result<Vec<ToolSpec>> {
    value
        .as_array()
        .context("Bedrock tools must be an array")?
        .iter()
        .map(|tool| {
            let spec = tool
                .get("toolSpec")
                .context("Bedrock tool is missing toolSpec")?;
            Ok(ToolSpec {
                name: required_string(spec, "name")?,
                description: spec
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: spec
                    .pointer("/inputSchema/json")
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
    if value.get("auto").is_some() {
        return Ok(ToolChoice::Auto);
    }
    if value.get("any").is_some() {
        return Ok(ToolChoice::Required);
    }
    if let Some(name) = value.pointer("/tool/name").and_then(Value::as_str) {
        return Ok(ToolChoice::Named { name: name.into() });
    }
    bail!("unsupported Bedrock toolChoice")
}

fn bedrock_usage(value: Option<&Value>) -> Usage {
    let prompt_tokens = value
        .and_then(|usage| usage.get("inputTokens"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    let completion_tokens = value
        .and_then(|usage| usage.get("outputTokens"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    let total_tokens = value
        .and_then(|usage| usage.get("totalTokens"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        required_components_known: value.is_some(),
        ..Usage::default()
    }
}

fn map_finish_reason(reason: &str) -> &str {
    match reason {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "content_filtered" | "guardrail_intervened" => "content_filter",
        _ => "other",
    }
}

fn value_as_text(value: &Value) -> anyhow::Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => serde_json::to_string(other).context("serialize structured Bedrock tool result"),
    }
}

fn bedrock_image_format(media_type: &str) -> anyhow::Result<&'static str> {
    match media_type {
        "image/jpeg" => Ok("jpeg"),
        "image/png" => Ok("png"),
        "image/gif" => Ok("gif"),
        "image/webp" => Ok("webp"),
        other => bail!("unsupported Bedrock image MIME type `{other}`"),
    }
}

fn required_string(value: &Value, key: &str) -> anyhow::Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("missing string `{key}`"))
}

fn required_usize(value: &Value, key: &str) -> anyhow::Result<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .with_context(|| format!("missing unsigned integer `{key}`"))
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
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
    fn encodes_converse_tool_config_not_chat_completions() {
        let mut request = AiRequest::new(
            "anthropic.claude",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("Hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.tools = Some(vec![ToolSpec {
            name: "weather".into(),
            description: None,
            parameters: json!({"type":"object"}),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        let (body, _) = BedrockConverseV1.encode_request(&request).unwrap();
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "weather"
        );
        assert!(body.get("messages").is_some());
        assert!(body.get("choices").is_none());
    }
}
