//! Vercel AI Gateway v4 language-model wire codec.
//!
//! `@ai-sdk/gateway` does not forward OpenAI Chat Completions. It accepts the
//! AI SDK v4 language-model call object at `/language-model` and returns AI SDK
//! v4 result objects/events. Keeping that wire separate prevents an apparently
//! valid but incompatible OpenAI request from reaching the Gateway.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::protocol::ids::{
    EndpointCapabilities, GATEWAY_LANGUAGE_MODEL_V4, ProtocolEndpoint, StreamCaps,
    VendorFieldPolicy,
};
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, AiStreamDelta, ContentBlock, MediaSource, MessageContent, Role,
    ToolCall, ToolChoice, Usage,
};
use crate::protocol::registry::EndpointRegistration;
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};

pub struct GatewayLanguageModelV4;

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
    parallel_tool_calls: true,
    extended_reasoning: true,
    deterministic_seed: true,
    stream: StreamCaps {
        server_sent_events: true,
        usage_in_stream: true,
        requires_stream_flag: false,
    },
    unknown_field_policy: VendorFieldPolicy::Drop,
};

impl ProtocolAdapter for GatewayLanguageModelV4 {
    fn id(&self) -> ProtocolEndpoint {
        GATEWAY_LANGUAGE_MODEL_V4
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, _body: Value) -> anyhow::Result<AiRequest> {
        bail!("Vercel AI Gateway v4 is egress-only")
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        let tool_names = tool_names(request);
        let mut prompt = Vec::new();
        if let Some(instructions) = &request.instructions
            && !instructions.trim().is_empty()
        {
            prompt.push(json!({"role": "system", "content": instructions}));
        }
        prompt.extend(
            request
                .items
                .iter()
                .map(|item| encode_message(item, &tool_names))
                .collect::<anyhow::Result<Vec<_>>>()?,
        );

        let mut body = Map::from_iter([("prompt".into(), Value::Array(prompt))]);
        insert_optional(&mut body, "maxOutputTokens", request.generation.max_tokens);
        insert_optional(&mut body, "temperature", request.generation.temperature);
        insert_optional(&mut body, "topP", request.generation.top_p);
        insert_optional(
            &mut body,
            "presencePenalty",
            request.generation.presence_penalty,
        );
        insert_optional(
            &mut body,
            "frequencyPenalty",
            request.generation.frequency_penalty,
        );
        insert_optional(&mut body, "seed", request.generation.seed);
        if let Some(stop) = &request.generation.stop {
            body.insert("stopSequences".into(), json!(stop));
        }
        if let Some(response_format) = &request.response_format {
            body.insert(
                "responseFormat".into(),
                encode_response_format(response_format)?,
            );
        }
        if let Some(tools) = &request.tools {
            body.insert(
                "tools".into(),
                Value::Array(
                    tools
                        .iter()
                        .map(|tool| {
                            let mut value = Map::from_iter([
                                ("type".into(), Value::String("function".into())),
                                ("name".into(), Value::String(tool.name.clone())),
                                ("inputSchema".into(), tool.parameters.clone()),
                            ]);
                            if let Some(description) = &tool.description {
                                value.insert(
                                    "description".into(),
                                    Value::String(description.clone()),
                                );
                            }
                            if let Some(strict) = tool.strict {
                                value.insert("strict".into(), Value::Bool(strict));
                            }
                            Value::Object(value)
                        })
                        .collect(),
                ),
            );
        }
        if let Some(choice) = &request.tool_choice {
            body.insert("toolChoice".into(), encode_tool_choice(choice));
        }
        if request.reasoning.enabled {
            body.insert(
                "reasoning".into(),
                Value::String(
                    request
                        .reasoning
                        .effort
                        .as_ref()
                        .and_then(crate::protocol::ir::ReasoningEffort::as_openai_str)
                        .unwrap_or("provider-default")
                        .into(),
                ),
            );
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("ai-language-model-specification-version"),
            HeaderValue::from_static("4"),
        );
        headers.insert(
            HeaderName::from_static("ai-language-model-id"),
            HeaderValue::from_str(&request.model)
                .context("Gateway model ID cannot be encoded as an HTTP header")?,
        );
        headers.insert(
            HeaderName::from_static("ai-language-model-streaming"),
            HeaderValue::from_static(if request.stream.enabled {
                "true"
            } else {
                "false"
            }),
        );

        Ok((Value::Object(body), headers))
    }

    fn request_path(&self, _model: &str, _stream: bool) -> String {
        "/language-model".into()
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        let mut response = AiResponse::new(
            body.pointer("/response/id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            body.pointer("/response/modelId")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        for content in body
            .get("content")
            .and_then(Value::as_array)
            .context("Gateway response is missing content")?
        {
            match content.get("type").and_then(Value::as_str) {
                Some("text") => response.push_output_text(required_string(content, "text")?),
                Some("reasoning") => {
                    response.push_reasoning(required_string(content, "text")?, None)
                }
                Some("tool-call") => response.push_tool_call(ToolCall {
                    id: required_string(content, "toolCallId")?,
                    name: required_string(content, "toolName")?,
                    arguments: serde_json::to_string(
                        content
                            .get("input")
                            .context("Gateway tool call is missing input")?,
                    )?,
                }),
                Some(other) => bail!("unsupported Gateway response content type `{other}`"),
                None => bail!("Gateway response content is missing type"),
            }
        }
        response.stop_reason = body
            .pointer("/finishReason/unified")
            .and_then(Value::as_str)
            .map(str::to_string);
        response.usage = gateway_usage(body.get("usage"));
        Ok(response)
    }

    fn encode_response(&self, _response: &AiResponse) -> Value {
        json!({"content": [], "finishReason": {"unified": "stop"}, "usage": gateway_usage_value(&Usage::default())})
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::Gateway(GatewayStreamParser::new()))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: GATEWAY_LANGUAGE_MODEL_V4,
            operation: "ingress stream encoding",
        })
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(GatewayLanguageModelV4) }
}

fn encode_message(item: &AiItem, tool_names: &BTreeMap<String, String>) -> anyhow::Result<Value> {
    match item.role {
        Role::System | Role::Developer => {
            Ok(json!({"role": "system", "content": text_content(&item.content)?}))
        }
        Role::User => {
            Ok(json!({"role": "user", "content": encode_content_parts(&item.content, false)?}))
        }
        Role::Assistant => {
            let mut content = encode_content_parts(&item.content, true)?;
            for call in item.tool_calls.as_deref().unwrap_or_default() {
                content.push(json!({
                    "type": "tool-call",
                    "toolCallId": call.id,
                    "toolName": call.name,
                    "input": serde_json::from_str::<Value>(&call.arguments)
                        .unwrap_or_else(|_| Value::String(call.arguments.clone())),
                }));
            }
            Ok(json!({"role": "assistant", "content": content}))
        }
        Role::Tool => {
            let tool_call_id = item
                .tool_call_id
                .as_deref()
                .context("Gateway tool result is missing tool_call_id")?;
            let tool_name = tool_names
                .get(tool_call_id)
                .context("Gateway tool result has no matching preceding tool call")?;
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

fn encode_content_parts(content: &MessageContent, assistant: bool) -> anyhow::Result<Vec<Value>> {
    let blocks = match content {
        MessageContent::Text(text) => return Ok(vec![json!({"type": "text", "text": text})]),
        MessageContent::Blocks(blocks) => blocks,
    };
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => Ok(json!({"type": "text", "text": text})),
            ContentBlock::Thinking { thinking, .. } if assistant => {
                Ok(json!({"type": "reasoning", "text": thinking}))
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } if assistant => {
                Ok(json!({"type": "tool-call", "toolCallId": id, "toolName": name, "input": input}))
            }
            ContentBlock::Image { source, .. }
            | ContentBlock::File {
                source,
                media_type: None,
            }
            | ContentBlock::Video {
                source,
                media_type: None,
            } => encode_file_part(source, "application/octet-stream"),
            ContentBlock::File {
                source,
                media_type: Some(media_type),
            }
            | ContentBlock::Video {
                source,
                media_type: Some(media_type),
            } => encode_file_part(source, media_type),
            ContentBlock::Audio { .. } => {
                bail!("Gateway v4 codec does not represent audio content")
            }
            other => bail!(
                "Gateway v4 cannot represent content block `{}`",
                content_block_name(other)
            ),
        })
        .collect()
}

fn encode_file_part(source: &MediaSource, media_type: &str) -> anyhow::Result<Value> {
    let data = match source {
        MediaSource::Url(url) => json!({"type": "url", "url": url}),
        MediaSource::Base64 { data, .. } => json!({"type": "data", "data": data}),
        MediaSource::FileId { file_id, .. } => {
            json!({"type": "reference", "reference": {"stravia": file_id}})
        }
    };
    Ok(json!({"type": "file", "mediaType": media_type, "data": data}))
}

fn text_content(content: &MessageContent) -> anyhow::Result<String> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => Ok(text.as_str()),
                ContentBlock::Thinking { thinking, .. } => Ok(thinking.as_str()),
                other => bail!(
                    "Gateway system message cannot represent `{}`",
                    content_block_name(other)
                ),
            })
            .collect(),
    }
}

fn tool_result_text(content: &MessageContent) -> anyhow::Result<String> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => Ok(text.clone()),
                ContentBlock::ToolResult { content, .. } => Ok(serde_json::to_string(content)?),
                other => bail!(
                    "Gateway tool result cannot represent `{}`",
                    content_block_name(other)
                ),
            })
            .collect(),
    }
}

fn tool_names(request: &AiRequest) -> BTreeMap<String, String> {
    request
        .items
        .iter()
        .flat_map(|item| item.tool_calls.as_deref().unwrap_or_default())
        .map(|call| (call.id.clone(), call.name.clone()))
        .collect()
}

fn encode_response_format(value: &crate::protocol::ir::ResponseFormat) -> anyhow::Result<Value> {
    match value {
        crate::protocol::ir::ResponseFormat::Text => Ok(json!({"type": "text"})),
        crate::protocol::ir::ResponseFormat::JsonObject => Ok(json!({"type": "json"})),
        crate::protocol::ir::ResponseFormat::JsonSchema { schema, name, .. } => {
            let mut result = Map::from_iter([
                ("type".into(), Value::String("json".into())),
                ("schema".into(), schema.clone()),
            ]);
            result.insert("name".into(), Value::String(name.clone()));
            Ok(Value::Object(result))
        }
    }
}

fn encode_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Required => json!({"type": "required"}),
        ToolChoice::Named { name } => json!({"type": "tool", "toolName": name}),
        ToolChoice::Raw(value) => value.clone(),
    }
}

fn insert_optional<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(key.into(), json!(value));
    }
}

fn gateway_usage(value: Option<&Value>) -> Usage {
    let prompt_tokens = value
        .and_then(|value| value.pointer("/inputTokens/total"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    let completion_tokens = value
        .and_then(|value| value.pointer("/outputTokens/total"))
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

fn gateway_usage_value(usage: &Usage) -> Value {
    json!({
        "inputTokens": {"total": usage.prompt_tokens},
        "outputTokens": {"total": usage.completion_tokens},
    })
}

fn required_string(value: &Value, key: &str) -> anyhow::Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("Gateway payload is missing string `{key}`"))
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

pub struct GatewayStreamParser {
    buffer: String,
    done: bool,
    tools: BTreeMap<String, (usize, ToolCall)>,
}

impl GatewayStreamParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            done: false,
            tools: BTreeMap::new(),
        }
    }

    pub(crate) fn parse_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
        self.buffer.push_str(raw);
        let mut deltas = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let event = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            self.parse_event(&event, &mut deltas)?;
        }
        Ok(deltas)
    }

    pub(crate) fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        let mut deltas = Vec::new();
        if !self.buffer.trim().is_empty() {
            let event = std::mem::take(&mut self.buffer);
            self.parse_event(&event, &mut deltas)?;
        }
        if !self.done {
            deltas.push(AiStreamDelta::UnexpectedEof);
        }
        Ok(deltas)
    }

    fn parse_event(&mut self, event: &str, deltas: &mut Vec<AiStreamDelta>) -> anyhow::Result<()> {
        let payload = event
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect::<Vec<_>>()
            .join("\n");
        if payload.trim().is_empty() {
            return Ok(());
        }
        let value: Value =
            serde_json::from_str(&payload).context("parse Gateway v4 stream event")?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response-metadata" => deltas.push(AiStreamDelta::MessageStart {
                id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                model: value
                    .get("modelId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            }),
            "text-delta" => {
                deltas.push(AiStreamDelta::TextDelta(required_string(&value, "delta")?))
            }
            "reasoning-delta" => deltas.push(AiStreamDelta::ThinkingDelta(required_string(
                &value, "delta",
            )?)),
            "tool-input-start" => {
                let id = required_string(&value, "id")?;
                let index = self.tools.len();
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
                let (index, call) = self
                    .tools
                    .get_mut(&id)
                    .context("Gateway emitted a tool input delta before start")?;
                let delta = required_string(&value, "delta")?;
                call.arguments.push_str(&delta);
                deltas.push(AiStreamDelta::ToolCallDelta {
                    index: *index,
                    arguments: delta,
                });
            }
            "tool-input-end" => {
                let id = required_string(&value, "id")?;
                let (index, call) = self
                    .tools
                    .remove(&id)
                    .context("Gateway emitted a tool input end before start")?;
                deltas.push(AiStreamDelta::ToolCallComplete {
                    index,
                    tool_call: call,
                });
            }
            "tool-call" => deltas.push(AiStreamDelta::ToolCallComplete {
                index: self.tools.len(),
                tool_call: ToolCall {
                    id: required_string(&value, "toolCallId")?,
                    name: required_string(&value, "toolName")?,
                    arguments: serde_json::to_string(
                        value
                            .get("input")
                            .context("Gateway tool call is missing input")?,
                    )?,
                },
            }),
            "finish" => {
                deltas.push(AiStreamDelta::Usage(gateway_usage(value.get("usage"))));
                self.done = true;
                deltas.push(AiStreamDelta::Done {
                    stop_reason: value
                        .pointer("/finishReason/unified")
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .into(),
                });
            }
            "error" => bail!(
                "Gateway stream error: {}",
                value.get("error").unwrap_or(&Value::Null)
            ),
            "stream-start" | "text-start" | "text-end" | "reasoning-start" | "reasoning-end"
            | "raw" => {}
            other => deltas.push(AiStreamDelta::Unknown {
                raw: format!("{other}: {payload}"),
            }),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{AiRequest, MessageContent};

    #[test]
    fn encodes_ai_sdk_v4_language_model_wire() {
        let request = AiRequest::new(
            "ignored-by-gateway",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let (body, headers) = GatewayLanguageModelV4.encode_request(&request).unwrap();

        assert_eq!(
            GatewayLanguageModelV4.request_path("anything", false),
            "/language-model"
        );
        assert_eq!(
            body["prompt"][0],
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]})
        );
        assert!(body.get("model").is_none());
        assert!(body.get("messages").is_none());
        assert_eq!(
            headers
                .get("ai-language-model-specification-version")
                .unwrap(),
            "4"
        );
        assert_eq!(
            headers.get("ai-language-model-id").unwrap(),
            "ignored-by-gateway"
        );
        assert_eq!(headers.get("ai-language-model-streaming").unwrap(), "false");

        let mut streaming_request = request;
        streaming_request.stream.enabled = true;
        let (_, streaming_headers) = GatewayLanguageModelV4
            .encode_request(&streaming_request)
            .unwrap();
        assert_eq!(
            streaming_headers
                .get("ai-language-model-streaming")
                .unwrap(),
            "true"
        );
    }

    #[test]
    fn decodes_ai_sdk_v4_response() {
        let response = GatewayLanguageModelV4
            .decode_response(json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "tool-call", "toolCallId": "call_1", "toolName": "weather", "input": {"city": "Paris"}}
                ],
                "finishReason": {"unified": "tool-calls"},
                "usage": {
                    "inputTokens": {"total": 3},
                    "outputTokens": {"total": 5}
                },
                "response": {"id": "resp_1", "modelId": "openai/gpt-5"}
            }))
            .unwrap();

        assert_eq!(response.id, "resp_1");
        assert_eq!(response.model, "openai/gpt-5");
        assert_eq!(response.output_text(), "hello");
        assert_eq!(response.tool_calls().count(), 1);
        assert_eq!(response.stop_reason.as_deref(), Some("tool-calls"));
        assert_eq!(response.usage.total_tokens, 8);
    }

    #[test]
    fn parses_ai_sdk_v4_stream_events() {
        let mut parser = GatewayStreamParser::new();
        let deltas = parser
            .parse_chunk(
                "data: {\"type\":\"response-metadata\",\"id\":\"resp_1\",\"modelId\":\"openai/gpt-5\"}\n\n\
                 data: {\"type\":\"text-delta\",\"id\":\"text_1\",\"delta\":\"hello\"}\n\n\
                 data: {\"type\":\"finish\",\"finishReason\":{\"unified\":\"stop\"},\"usage\":{\"inputTokens\":{\"total\":3},\"outputTokens\":{\"total\":5}}}\n\n",
            )
            .unwrap();

        assert!(matches!(deltas[0], AiStreamDelta::MessageStart { .. }));
        assert!(matches!(&deltas[1], AiStreamDelta::TextDelta(text) if text == "hello"));
        assert!(matches!(
            deltas[2],
            AiStreamDelta::Usage(Usage {
                total_tokens: 8,
                ..
            })
        ));
        assert!(matches!(&deltas[3], AiStreamDelta::Done { stop_reason } if stop_reason == "stop"));
    }
}
