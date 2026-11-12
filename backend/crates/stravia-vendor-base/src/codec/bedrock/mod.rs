//! Amazon Bedrock Converse API egress codec.
//!
//! Converse is JSON for unary calls and AWS Event Stream frames for streaming.
//! The frame decoder keeps bytes intact until it has validated one complete AWS
//! message; converting chunks to UTF-8 first corrupts the length-prefixed wire.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use http::header::HeaderMap;
use serde_json::{Map, Value, json};

use stravia_protocol_codec::transform::{
    BinaryWireStreamParser, ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use stravia_runtime_contract::protocol::ids::BEDROCK_CONVERSE_V1;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;
use stravia_runtime_contract::protocol::ids::StreamCaps;
use stravia_runtime_contract::protocol::ids::VendorFieldPolicy;
use stravia_runtime_contract::protocol::ir::AiError;
use stravia_runtime_contract::protocol::ir::AiErrorKind;
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
use stravia_runtime_contract::protocol::ir::ServiceTier;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::ToolChoice;
use stravia_runtime_contract::protocol::ir::ToolSpec;
use stravia_runtime_contract::protocol::ir::Usage;

pub struct BedrockConverseV1;

fn decode_output_config(request: &mut AiRequest, value: &Value) -> anyhow::Result<()> {
    let Some(format) = value.get("textFormat") else {
        return Ok(());
    };
    if format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return Ok(());
    }
    let definition = format
        .pointer("/structure/jsonSchema")
        .context("Bedrock JSON schema output is missing its definition")?;
    let schema = definition
        .get("schema")
        .and_then(Value::as_str)
        .context("Bedrock JSON schema output is missing its schema string")?;
    request.response_format = Some(ResponseFormat::JsonSchema {
        name: definition
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("response")
            .into(),
        description: definition
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        schema: serde_json::from_str(schema).context("Bedrock output schema is not valid JSON")?,
        strict: None,
    });
    Ok(())
}

fn encode_output_config(request: &AiRequest) -> anyhow::Result<Option<Value>> {
    let Some(format) = &request.response_format else {
        return Ok(None);
    };
    let (name, description, schema) = match format {
        ResponseFormat::Text | ResponseFormat::JsonObject => return Ok(None),
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            ..
        } => (name, description, schema),
    };
    let mut definition = Map::from_iter([
        ("schema".into(), serde_json::to_string(schema)?.into()),
        ("name".into(), name.clone().into()),
    ]);
    if let Some(description) = description {
        definition.insert("description".into(), description.clone().into());
    }
    Ok(Some(json!({"textFormat": {
        "type": "json_schema", "structure": {"jsonSchema": definition}
    }})))
}

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
        if let Some(system) = body.get("system").filter(|value| !value.is_null()) {
            request.items.insert(
                0,
                AiItem {
                    role: Role::System,
                    content: MessageContent::Blocks(decode_cached_blocks(system)?),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            );
        }
        request.parameters.metadata =
            body.get("requestMetadata")
                .and_then(Value::as_object)
                .map(|metadata| {
                    metadata
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_owned()))
                        })
                        .collect()
                });
        request.parameters.service_tier =
            match body.pointer("/serviceTier/type").and_then(Value::as_str) {
                Some("default") => Some(ServiceTier::Default),
                Some("flex") => Some(ServiceTier::Flex),
                Some("priority") => Some(ServiceTier::Priority),
                _ => None,
            };
        if let Some(value) = body.get("outputConfig") {
            decode_output_config(&mut request, value)?;
        }
        if let Some(native) = body.get("additionalModelRequestFields") {
            request.parameters.top_k = native
                .get("top_k")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            if let Some(effort) = native
                .pointer("/output_config/effort")
                .and_then(Value::as_str)
            {
                request.reasoning.level =
                    Some(stravia_runtime_contract::thinking::ThinkingLevel::from_wire(effort)?);
            }
            if let Some(thinking) = native.get("thinking").filter(|value| !value.is_null()) {
                let kind = thinking
                    .get("type")
                    .and_then(Value::as_str)
                    .context("Bedrock thinking is missing type")?;
                request.reasoning.target_control = Some(match kind {
                    "disabled" => {
                        stravia_runtime_contract::thinking::TargetThinkingControl::Disabled
                    }
                    "enabled" | "adaptive" => {
                        stravia_runtime_contract::thinking::TargetThinkingControl::Enabled
                    }
                    _ => bail!("unsupported Bedrock thinking type"),
                });
                request.reasoning.mode = Some(kind.into());
                request.reasoning.budget_tokens = thinking
                    .get("budget_tokens")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok());
            }
        }
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
            request.tools = tool_config
                .get("tools")
                .filter(|value| !value.is_null())
                .map(decode_tools)
                .transpose()?;
            request.tool_choice = tool_config
                .get("toolChoice")
                .filter(|value| !value.is_null())
                .map(decode_tool_choice)
                .transpose()?;
        }
        for (pointer, reset) in [
            ("/outputConfig", ParameterReset::ResponseFormat),
            ("/serviceTier", ParameterReset::ServiceTier),
            ("/inferenceConfig/temperature", ParameterReset::Temperature),
            ("/inferenceConfig/topP", ParameterReset::TopP),
            ("/inferenceConfig/maxTokens", ParameterReset::MaxTokens),
            (
                "/additionalModelRequestFields/thinking",
                ParameterReset::Reasoning,
            ),
        ] {
            if body.pointer(pointer).is_some_and(Value::is_null) {
                request.parameters.reset(reset);
            }
        }
        Ok(request)
    }

    fn encode_request(&self, mut request: AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        request.normalize_parameters();
        let claude = request.model.contains("anthropic.claude");
        let thinking = claude
            && match request.reasoning.target_control {
                Some(
                    stravia_runtime_contract::thinking::TargetThinkingControl::Disabled
                    | stravia_runtime_contract::thinking::TargetThinkingControl::Hidden,
                ) => false,
                Some(
                    stravia_runtime_contract::thinking::TargetThinkingControl::Enabled
                    | stravia_runtime_contract::thinking::TargetThinkingControl::Budget { .. }
                    | stravia_runtime_contract::thinking::TargetThinkingControl::Effort { .. },
                ) => true,
                None => request.reasoning.is_enabled(),
            };
        let budget = match request.reasoning.target_control {
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Budget { value }) => {
                Some(value)
            }
            _ => request.reasoning.budget_tokens,
        };
        if thinking {
            request.generation.temperature = None;
            request.generation.top_p = None;
        }
        let mut body =
            Map::from_iter([("messages".into(), Value::Array(encode_messages(&request)?))]);
        let mut system = Vec::new();
        if let Some(instructions) = &request.instructions
            && !instructions.trim().is_empty()
        {
            system.push(json!({"text": instructions}));
        }
        for item in &request.items {
            if matches!(item.role, Role::System | Role::Developer) {
                match &item.content {
                    MessageContent::Text(text) if !text.trim().is_empty() => {
                        system.push(json!({"text": text}));
                    }
                    MessageContent::Blocks(blocks) => {
                        for block in blocks {
                            if let ContentBlock::Text {
                                text,
                                cache_control,
                            } = block
                                && !text.trim().is_empty()
                            {
                                system.push(json!({"text": text}));
                                append_cache_point(&mut system, cache_control.as_ref());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !system.is_empty() {
            body.insert("system".into(), Value::Array(system));
        }
        let inference_config = encode_inference_config(&request);
        if !inference_config.is_empty() {
            body.insert("inferenceConfig".into(), Value::Object(inference_config));
        }
        if let Some(tool_config) = encode_tool_config(&request)? {
            body.insert("toolConfig".into(), tool_config);
        }
        if let Some(value) = &request.parameters.metadata {
            body.insert("requestMetadata".into(), json!(value));
        }
        if let Some(value) = request
            .parameters
            .service_tier
            .as_ref()
            .and_then(|tier| match tier {
                ServiceTier::Default => Some("default"),
                ServiceTier::Flex => Some("flex"),
                ServiceTier::Priority => Some("priority"),
                ServiceTier::Auto => None,
            })
        {
            body.insert("serviceTier".into(), json!({"type": value}));
        }
        if let Some(value) = encode_output_config(&request)? {
            body.insert("outputConfig".into(), value);
        }
        if claude {
            let mut native = Map::new();
            if let Some(top_k) = request.parameters.top_k.filter(|_| !thinking) {
                native.insert("top_k".into(), top_k.into());
            }
            if thinking {
                let config = if matches!(
                    request.reasoning.mode.as_deref(),
                    Some("auto" | "adaptive")
                ) || matches!(
                    request.reasoning.target_control,
                    Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { .. })
                ) {
                    json!({"type": "adaptive"})
                } else if let Some(budget) = budget {
                    if let Some(cap) = request.generation.max_tokens {
                        anyhow::ensure!(
                            budget < cap,
                            "Bedrock Claude thinking budget must be below the output token limit"
                        );
                    }
                    json!({"type": "enabled", "budget_tokens": budget})
                } else {
                    bail!(
                        "Bedrock Claude enabled thinking requires a token budget or adaptive mode"
                    );
                };
                native.insert("thinking".into(), config);
                let effort = match request.reasoning.target_control.as_ref() {
                    Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                        value,
                    }) => Some(value.as_str()),
                    _ => request.reasoning.level.map(|level| {
                        if level == stravia_runtime_contract::thinking::ThinkingLevel::Off {
                            "none"
                        } else {
                            level.as_str()
                        }
                    }),
                };
                if let Some(effort @ ("low" | "medium" | "high" | "max")) = effort {
                    native.insert("output_config".into(), json!({"effort": effort}));
                }
            } else if matches!(
                request.reasoning.target_control,
                Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled)
            ) || (request.reasoning.target_control.is_none()
                && request.reasoning.mode.as_deref() == Some("disabled"))
            {
                native.insert("thinking".into(), json!({"type": "disabled"}));
            }
            if !native.is_empty() {
                body.insert("additionalModelRequestFields".into(), Value::Object(native));
            }
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
                ContentBlock::Text { text, .. } => {
                    response.push_output_text(std::sync::Arc::unwrap_or_clone(text))
                }
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => response.push_reasoning(thinking, signature),
                // 密文块没有明文可读，作为独立 assistant 条目保留以便回放。
                ContentBlock::RedactedThinking { data } => response.items.push(AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking { data }]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                }),
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
        let mut content = Vec::new();
        for item in &response.items {
            match &item.content {
                MessageContent::Text(text) if item.role == Role::Assistant && !text.is_empty() => {
                    content.push(json!({"text": text}));
                }
                MessageContent::Blocks(blocks) if item.role == Role::Assistant => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text, .. } if !text.is_empty() => {
                                content.push(json!({"text": text}));
                            }
                            ContentBlock::Thinking {
                                thinking,
                                signature,
                            } => {
                                content.push(reasoning_text_block(thinking, signature.as_deref()));
                            }
                            ContentBlock::Reasoning {
                                summary,
                                content: segments,
                                encrypted_content,
                            } => {
                                content.push(reasoning_text_block(
                                    &reasoning_plaintext(summary, segments),
                                    encrypted_content.as_deref(),
                                ));
                            }
                            ContentBlock::RedactedThinking { data } => {
                                content.push(json!({
                                    "reasoningContent": {"redactedContent": data}
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            for call in item.tool_calls.as_deref().unwrap_or_default() {
                if let Ok(input @ Value::Object(_)) = serde_json::from_str::<Value>(&call.arguments)
                {
                    content.push(json!({"toolUse": {
                        "toolUseId": call.id, "name": call.name, "input": input,
                    }}));
                }
            }
        }
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
        Ok(WireStreamDecoder::custom_binary(BedrockStreamParser::new()))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: BEDROCK_CONVERSE_V1,
            operation: "ingress stream encoding",
        })
    }
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

    pub fn parse_chunk(&mut self, raw: &[u8]) -> anyhow::Result<Vec<AiStreamDelta>> {
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

    pub fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
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
                    id: (required_string(tool_use, "toolUseId")?).into(),
                    name: required_string(tool_use, "name")?,
                    arguments: String::new(),
                };
                deltas.push(AiStreamDelta::ToolCallStart {
                    index,
                    id: call.id.to_string(),
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
            } else if let Some(signature) = content
                .pointer("/reasoningContent/signature")
                .and_then(Value::as_str)
            {
                // 签名是回放必需的受保护载荷，丢了它下一轮只能剥离思考。
                deltas.push(AiStreamDelta::ThinkingSignature(signature.into()));
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
        // 异常帧保留原始 payload 在 StreamError.raw 里：validationException
        // 可能携带 Claude 的 thinking 签名拒绝文案，出口侧据此识别为
        // 受保护推理回放失败（仅限任何输出之前），其余仍是普通流内错误。
        if let Some(exception) = [
            "internalServerException",
            "modelStreamErrorException",
            "validationException",
            "throttlingException",
        ]
        .iter()
        .find_map(|key| payload.get(*key))
        {
            self.done = true;
            let message = exception
                .get("message")
                .or_else(|| exception.get("Message"))
                .and_then(Value::as_str)
                .unwrap_or("Bedrock streaming error");
            deltas.push(AiStreamDelta::StreamError {
                error: AiError::new(AiErrorKind::StreamMidError, message).with_raw(payload.clone()),
            });
            return Ok(());
        }
        deltas.push(AiStreamDelta::Unknown {
            raw: payload.to_string(),
        });
        Ok(())
    }
}

impl BinaryWireStreamParser for BedrockStreamParser {
    fn parse_binary_chunk(&mut self, raw: &[u8]) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.parse_chunk(raw)
    }

    fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.finish()
    }
}

impl Default for BedrockStreamParser {
    fn default() -> Self {
        Self::new()
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
            Role::System | Role::Developer => {}
            Role::User => {
                let content = encode_user_content(&item.content)?;
                if !content.is_empty() {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
            Role::Assistant => {
                let content = encode_assistant_content(item)?;
                // 只含无法承载的受保护推理载荷（且无 tool call）的条目编码为空；
                // 空 assistant 消息会被上游拒绝，整条跳过。tool 结果只依赖其它
                // 条目的 tool call，跳过不会破坏配对。
                if !content.is_empty() {
                    messages.push(json!({"role": "assistant", "content": content}));
                }
            }
            Role::Tool => {
                let content = encode_tool_result(item)?;
                if !content.is_empty() {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
        }
    }
    Ok(messages)
}

fn encode_user_content(content: &MessageContent) -> anyhow::Result<Vec<Value>> {
    match content {
        MessageContent::Text(text) => Ok(vec![json!({"text": text})]),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text {
                        text,
                        cache_control,
                    } => {
                        parts.push(json!({"text": text}));
                        append_cache_point(&mut parts, cache_control.as_ref());
                    }
                    ContentBlock::Image {
                        source: MediaSource::Base64 { media_type, data },
                        ..
                    } => {
                        if let Ok(format) = bedrock_image_format(media_type) {
                            parts.push(
                                json!({"image": {"format": format, "source": {"bytes": data}}}),
                            );
                        }
                    }
                    ContentBlock::Document {
                        source,
                        title,
                        context,
                        ..
                    } => {
                        let document = match source {
                            DocumentSource::Base64Pdf { data } if !data.is_empty() => {
                                Some(bedrock_document(
                                    "pdf",
                                    json!({"bytes": data}),
                                    title.as_deref(),
                                    context.as_deref(),
                                ))
                            }
                            DocumentSource::PlainText { data } => Some(bedrock_document(
                                "txt",
                                json!({"text": data}),
                                title.as_deref(),
                                context.as_deref(),
                            )),
                            // URLs require fetching or S3 access; Blocks are flattened upstream.
                            _ => None,
                        };
                        if let Some(document) = document {
                            parts.push(document);
                        }
                    }
                    ContentBlock::File {
                        source:
                            MediaSource::Base64 {
                                media_type: source_type,
                                data,
                            },
                        media_type,
                    } if !data.is_empty() => {
                        if let Some(format) =
                            bedrock_document_format(media_type.as_deref().unwrap_or(source_type))
                        {
                            parts.push(bedrock_document(
                                format,
                                json!({"bytes": data}),
                                None,
                                None,
                            ));
                        }
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        content_kind,
                        is_error,
                        ..
                    } => {
                        let content = bedrock_tool_result_content(content, *content_kind);
                        if !content.is_empty() {
                            parts.push(json!({"toolResult": {
                                "toolUseId": tool_use_id,
                                "content": content,
                                "status": is_error.is_some_and(|error| error).then_some("error"),
                            }}));
                        }
                    }
                    _ => {}
                }
            }
            // Converse requires a text sibling for documents. Its ContentBlock.text
            // shape is unconstrained String (unlike SystemContentBlock's NonEmptyString).
            // An empty carrier adds no instructions or third-party title/context text.
            if parts.iter().any(|part| part.get("document").is_some())
                && !parts.iter().any(|part| part.get("text").is_some())
            {
                parts.push(json!({"text": ""}));
            }
            Ok(parts)
        }
    }
}

// DocumentBlock: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_DocumentBlock.html
// DocumentSource bytes are already base64 on the JSON wire; text is a native source.
fn bedrock_document(
    format: &str,
    source: Value,
    title: Option<&str>,
    context: Option<&str>,
) -> Value {
    let name = title
        .filter(|name| bedrock_document_name_valid(name))
        .unwrap_or("document");
    let mut document = json!({"format": format, "name": name, "source": source});
    if let Some(context) = context {
        document["context"] = json!(context);
    }
    json!({"document": document})
}

fn bedrock_document_name_valid(name: &str) -> bool {
    let mut previous_whitespace = false;
    let mut length = 0;
    for character in name.chars() {
        length += 1;
        if length > 200 {
            return false;
        }
        let whitespace = character.is_whitespace();
        if (whitespace && previous_whitespace)
            || !(character.is_alphanumeric()
                || whitespace
                || matches!(character, '-' | '(' | ')' | '[' | ']'))
        {
            return false;
        }
        previous_whitespace = whitespace;
    }
    length > 0
}

fn bedrock_document_format(media_type: &str) -> Option<&'static str> {
    match media_type {
        "application/pdf" => Some("pdf"),
        "text/csv" | "application/csv" => Some("csv"),
        "application/msword" => Some("doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.ms-excel" => Some("xls"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        "text/html" => Some("html"),
        "text/plain" => Some("txt"),
        "text/markdown" | "text/x-markdown" => Some("md"),
        _ => None,
    }
}

fn encode_assistant_content(item: &AiItem) -> anyhow::Result<Vec<Value>> {
    let mut content = match &item.content {
        MessageContent::Text(text) => vec![json!({"text": text})],
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text {
                        text,
                        cache_control,
                    } => {
                        parts.push(json!({"text": text}));
                        append_cache_point(&mut parts, cache_control.as_ref());
                    }
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => match signature
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                    {
                        // 有签名的思考走原生 reasoningText 载体，签名原样透传。
                        Some(signature) => {
                            parts.push(reasoning_text_block(thinking, Some(signature)))
                        }
                        // Claude on Bedrock 拒绝无签名 reasoningContent；
                        // 无签名明文只能降级为普通 text 块，否则整个请求被拒。
                        None if !thinking.is_empty() => parts.push(json!({"text": thinking})),
                        None => {}
                    },
                    ContentBlock::Reasoning {
                        summary,
                        content: segments,
                        encrypted_content,
                    } => match encrypted_content
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                    {
                        // encrypted_content 是来源上游的不透明载荷；Converse 的
                        // reasoningText.signature 能原生承载这类密文，乐观回放，
                        // 被上游拒绝时由宿主剥离重试。
                        Some(encrypted) => parts.push(reasoning_text_block(
                            &reasoning_plaintext(summary, segments),
                            Some(encrypted),
                        )),
                        // 与无签名 Thinking 同理降级：每段非空文本一个 text 块，
                        // 保持在原位置顺序。
                        None => {
                            for text in summary
                                .iter()
                                .chain(segments)
                                .filter(|text| !text.is_empty())
                            {
                                parts.push(json!({"text": text}));
                            }
                        }
                    },
                    // redactedContent 是 Converse 原生载体（blob，JSON 中为 base64
                    // 字符串）；IR 的 data 已是 base64，原样透传。
                    ContentBlock::RedactedThinking { data } if !data.is_empty() => {
                        parts.push(json!({"reasoningContent": {"redactedContent": data}}))
                    }
                    ContentBlock::RedactedThinking { .. } => {}
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control,
                    } if input.is_object() => {
                        parts.push(
                            json!({"toolUse": {"toolUseId": id, "name": name, "input": input}}),
                        );
                        append_cache_point(&mut parts, cache_control.as_ref());
                    }
                    _ => {}
                }
            }
            parts
        }
    };
    for call in item.tool_calls.as_deref().unwrap_or_default() {
        if let Ok(input @ Value::Object(_)) = serde_json::from_str::<Value>(&call.arguments) {
            content.push(json!({"toolUse": {
                "toolUseId": call.id, "name": call.name, "input": input,
            }}));
        }
    }
    Ok(content)
}

/// 一个 `reasoningContent.reasoningText` 块；signature 仅在调用方确认非空时给出。
fn reasoning_text_block(text: &str, signature: Option<&str>) -> Value {
    let mut reasoning = Map::from_iter([("text".into(), Value::String(text.to_owned()))]);
    if let Some(signature) = signature {
        reasoning.insert("signature".into(), Value::String(signature.to_owned()));
    }
    json!({"reasoningContent": {"reasoningText": Value::Object(reasoning)}})
}

/// Reasoning 的明文按 summary → content 顺序、跳过空串后拼接为单段文本。
fn reasoning_plaintext(summary: &[String], content: &[String]) -> String {
    summary
        .iter()
        .chain(content)
        .filter(|text| !text.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

fn bedrock_tool_result_content(
    content: &Value,
    kind: Option<stravia_runtime_contract::protocol::ir::ToolResultContentKind>,
) -> Vec<Value> {
    use stravia_runtime_contract::protocol::ir::ToolResultContentKind;
    if kind != Some(ToolResultContentKind::ContentBlocks) {
        return vec![json!({"json": content})];
    }
    if let Some(text) = content.as_str() {
        return (!text.is_empty())
            .then(|| json!({"text": text}))
            .into_iter()
            .collect();
    }
    let mut parts = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        if let Some(text) = super::tool_result_block_text(block) {
            if !text.is_empty() {
                parts.push(json!({"text": text}));
            }
            continue;
        }
        match block.get("type").and_then(Value::as_str) {
            None => {
                if let Some(value) = block.get("json") {
                    parts.push(json!({"json": value}));
                } else if let Some(image) = block.get("image")
                    && matches!(
                        image.get("format").and_then(Value::as_str),
                        Some("png" | "jpeg" | "gif" | "webp")
                    )
                    && image
                        .pointer("/source/bytes")
                        .and_then(Value::as_str)
                        .is_some()
                {
                    parts.push(json!({"image": image}));
                }
            }
            Some("image") => {
                if let Some(source) = block.get("source")
                    && source.get("type").and_then(Value::as_str) == Some("base64")
                    && let (Some(media_type), Some(data)) = (
                        source.get("media_type").and_then(Value::as_str),
                        source.get("data").and_then(Value::as_str),
                    )
                    && let Ok(format) = bedrock_image_format(media_type)
                {
                    parts.push(json!({"image": {"format": format, "source": {"bytes": data}}}));
                }
            }
            _ => {}
        }
    }
    parts
}

fn encode_tool_result(item: &AiItem) -> anyhow::Result<Vec<Value>> {
    let Some(tool_use_id) = item.tool_call_id.as_deref() else {
        return Ok(Vec::new());
    };
    let mut is_error = false;
    let content = match &item.content {
        MessageContent::Text(text) => vec![json!({"text": text})],
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => parts.push(json!({"text": text})),
                    ContentBlock::ToolResult {
                        content,
                        content_kind,
                        is_error: error,
                        ..
                    } => {
                        let projected = bedrock_tool_result_content(content, *content_kind);
                        if !projected.is_empty() {
                            is_error |= *error == Some(true);
                            parts.extend(projected);
                        }
                    }
                    _ => {}
                }
            }
            parts
        }
    };
    Ok((!content.is_empty())
        .then(|| {
            let mut result = json!({"toolUseId": tool_use_id, "content": content});
            if is_error {
                result["status"] = json!("error");
            }
            json!({"toolResult": result})
        })
        .into_iter()
        .collect())
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
    let mut definitions = Vec::with_capacity(tools.len());
    for tool in tools {
        definitions.push(encode_tool(tool));
    }
    let mut tool_config = Map::from_iter([("tools".into(), Value::Array(definitions))]);
    if let Some(choice) = &request.tool_choice {
        let choice = match choice {
            ToolChoice::Auto => json!({"auto": {}}),
            ToolChoice::Required => json!({"any": {}}),
            ToolChoice::Named { name } => json!({"tool": {"name": name}}),
            ToolChoice::None | ToolChoice::Allowed { .. } | ToolChoice::Native(_) => {
                return Ok(Some(Value::Object(tool_config)));
            }
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
            let blocks = decode_cached_blocks(
                message
                    .get("content")
                    .context("Bedrock message is missing content")?,
            )?;
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

fn append_cache_point(
    parts: &mut Vec<Value>,
    cache: Option<&stravia_runtime_contract::protocol::ir::CacheControl>,
) {
    if let Some(cache) = cache {
        let ttl = match cache.ttl {
            stravia_runtime_contract::protocol::ir::CacheTtl::Ephemeral5m => "5m",
            stravia_runtime_contract::protocol::ir::CacheTtl::Ephemeral1h => "1h",
        };
        parts.push(json!({"cachePoint": {"type": "default", "ttl": ttl}}));
    }
}

fn decode_cached_blocks(value: &Value) -> anyhow::Result<Vec<ContentBlock>> {
    let mut blocks = Vec::new();
    for part in value
        .as_array()
        .context("Bedrock content must be an array")?
    {
        if let Some(point) = part.get("cachePoint") {
            let cache = decode_cache_point(point)?;
            match blocks.last_mut() {
                Some(
                    ContentBlock::Text { cache_control, .. }
                    | ContentBlock::ToolUse { cache_control, .. },
                ) => *cache_control = Some(cache),
                _ => bail!("Bedrock cache point must follow a supported cacheable block"),
            }
        } else {
            blocks.push(decode_content_block(part)?);
        }
    }
    Ok(blocks)
}

fn decode_cache_point(
    point: &Value,
) -> anyhow::Result<stravia_runtime_contract::protocol::ir::CacheControl> {
    anyhow::ensure!(
        point.get("type").and_then(Value::as_str) == Some("default"),
        "unsupported Bedrock cache point type"
    );
    Ok(match point.get("ttl").and_then(Value::as_str) {
        None | Some("5m") => stravia_runtime_contract::protocol::ir::CacheControl::ephemeral(),
        Some("1h") => stravia_runtime_contract::protocol::ir::CacheControl::ephemeral_1h(),
        Some(_) => bail!("unsupported Bedrock cache point TTL"),
    })
}

fn decode_content_block(block: &Value) -> anyhow::Result<ContentBlock> {
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        return Ok(ContentBlock::Text {
            text: text.to_owned().into(),
            cache_control: None,
        });
    }
    if let Some(tool) = block.get("toolUse") {
        return Ok(ContentBlock::ToolUse {
            id: (required_string(tool, "toolUseId")?).into(),
            name: required_string(tool, "name")?,
            input: tool.get("input").cloned().unwrap_or_else(|| json!({})),
            cache_control: None,
        });
    }
    if let Some(result) = block.get("toolResult") {
        return Ok(ContentBlock::ToolResult {
            tool_use_id: (required_string(result, "toolUseId")?).into(),
            content_kind: Some(
                stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
            ),
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
    // redactedContent 是 blob：JSON 线上即 base64 字符串，与 IR 的 data 字段对应。
    if let Some(data) = block
        .pointer("/reasoningContent/redactedContent")
        .and_then(Value::as_str)
    {
        return Ok(ContentBlock::RedactedThinking {
            data: data.to_string(),
        });
    }
    bail!("unsupported Bedrock content block")
}

fn decode_tools(value: &Value) -> anyhow::Result<Vec<ToolSpec>> {
    let mut tools: Vec<ToolSpec> = Vec::new();
    for tool in value.as_array().context("Bedrock tools must be an array")? {
        if tool.get("cachePoint").is_none() {
            let spec = tool
                .get("toolSpec")
                .context("Bedrock tool is missing toolSpec")?;
            tools.push(ToolSpec {
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
                defer_loading: None,
            });
        }
    }
    Ok(tools)
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

#[cfg(test)]
mod tests;
