use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::SseEvent;
use crate::protocol::ids::{EndpointCapabilities, Protocol, ProtocolEndpoint};
use crate::protocol::ir::{AiRequest, AiResponse, AiStreamDelta, ProtocolExt};
use crate::protocol::registry::ProtocolRegistry;

pub(crate) trait ProtocolAdapter: Send + Sync + 'static {
    fn id(&self) -> ProtocolEndpoint;
    fn capabilities(&self) -> &'static EndpointCapabilities;
    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest>;
    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)>;
    fn request_path(&self, model: &str, stream: bool) -> String;
    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse>;
    fn encode_response(&self, response: &AiResponse) -> Value;
    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError>;
    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransformDirection {
    ClientRequest,
    UpstreamResponse,
    UpstreamStream,
    ClientResponse,
}

impl TransformDirection {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ClientRequest => "client_request",
            Self::UpstreamResponse => "upstream_response",
            Self::UpstreamStream => "upstream_stream",
            Self::ClientResponse => "client_response",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TransformError {
    #[error("protocol endpoint is not registered: {endpoint}")]
    Unsupported { endpoint: ProtocolEndpoint },
    #[error("{operation} is not supported for {endpoint}")]
    UnsupportedOperation {
        endpoint: ProtocolEndpoint,
        operation: &'static str,
    },
    #[error("invalid {direction} payload for {endpoint}: {source}")]
    Wire {
        endpoint: ProtocolEndpoint,
        direction: &'static str,
        #[source]
        source: anyhow::Error,
    },
    #[error(
        "{direction} from {ingress} to {egress} cannot preserve: {fields}",
        fields = .lost.join(", ")
    )]
    Unrepresentable {
        ingress: ProtocolEndpoint,
        egress: ProtocolEndpoint,
        direction: &'static str,
        lost: Vec<String>,
    },
    #[error("{direction} for {endpoint} is already closed")]
    StreamClosed {
        endpoint: ProtocolEndpoint,
        direction: &'static str,
    },
}

#[derive(Debug)]
pub(crate) struct EncodedRequest {
    pub(crate) body: Value,
    pub(crate) headers: HeaderMap,
    pub(crate) path: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProtocolPair {
    ingress: ProtocolEndpoint,
    egress: ProtocolEndpoint,
}

pub(crate) struct ProtocolTransform;

impl ProtocolTransform {
    pub(crate) fn global() -> &'static Self {
        static TRANSFORM: ProtocolTransform = ProtocolTransform;
        &TRANSFORM
    }

    pub(crate) fn bind(
        &self,
        ingress: ProtocolEndpoint,
        egress: ProtocolEndpoint,
    ) -> Result<ProtocolPair, TransformError> {
        let registry = ProtocolRegistry::global();
        for endpoint in [ingress, egress] {
            if registry.adapter(&endpoint).is_none() {
                return Err(TransformError::Unsupported { endpoint });
            }
        }
        Ok(ProtocolPair { ingress, egress })
    }

    /// Build only the upstream decoding side for an egress-only protocol.
    ///
    /// Bedrock Converse and Cohere Chat deliberately have no ingress routes.
    /// The proxy therefore must not require a client-side stream formatter just
    /// to decode their upstream streams.
    pub(crate) fn decode_stream(
        &self,
        endpoint: ProtocolEndpoint,
    ) -> Result<StreamDecodeStage, TransformError> {
        let adapter = adapter(endpoint)?;
        if !adapter.capabilities().streaming {
            return Err(TransformError::UnsupportedOperation {
                endpoint,
                operation: "stream",
            });
        }
        Ok(StreamDecodeStage {
            pair: ProtocolPair {
                ingress: endpoint,
                egress: endpoint,
            },
            decoder: adapter.stream_decoder()?,
            closed: false,
        })
    }

    pub(crate) fn inferred_ingress(request: &AiRequest) -> Option<ProtocolEndpoint> {
        if let Some(protocol) = request.meta.source_protocol {
            return Some(protocol);
        }
        match request.ext.as_ref() {
            Some(ProtocolExt::OpenResponses(_)) => {
                Some(crate::protocol::ids::OPEN_RESPONSES_2026_04_24)
            }
            Some(ProtocolExt::Anthropic(_)) => {
                Some(crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01)
            }
            Some(ProtocolExt::Google(_)) => {
                Some(crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
            }
            Some(ProtocolExt::OpenAiChat(_)) => {
                Some(crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
            }
            None => None,
        }
    }
}

impl ProtocolPair {
    pub(crate) fn decode_request(self, body: Value) -> Result<AiRequest, TransformError> {
        adapter(self.ingress)?
            .decode_request(body)
            .map_err(|source| TransformError::Wire {
                endpoint: self.ingress,
                direction: TransformDirection::ClientRequest.as_str(),
                source,
            })
    }

    pub(crate) fn encode_request(
        self,
        canonical: &AiRequest,
    ) -> Result<EncodedRequest, TransformError> {
        let lost = request_loss_paths(self, canonical);
        if !lost.is_empty() {
            return Err(unrepresentable(
                self,
                TransformDirection::ClientRequest,
                lost,
            ));
        }
        let target = adapter(self.egress)?;
        let path = target.request_path(&canonical.model, canonical.stream.enabled);
        let (body, headers) =
            target
                .encode_request(canonical)
                .map_err(|source| TransformError::Wire {
                    endpoint: self.egress,
                    direction: TransformDirection::ClientRequest.as_str(),
                    source,
                })?;
        Ok(EncodedRequest {
            body,
            headers,
            path,
        })
    }

    pub(crate) fn decode_response(self, body: Value) -> Result<AiResponse, TransformError> {
        validate_response_cardinality(self, &body)?;
        adapter(self.egress)?
            .decode_response(body)
            .map_err(|source| TransformError::Wire {
                endpoint: self.egress,
                direction: TransformDirection::UpstreamResponse.as_str(),
                source,
            })
    }

    pub(crate) fn encode_response(self, canonical: &AiResponse) -> Result<Value, TransformError> {
        let lost = response_loss_paths(self, canonical);
        if !lost.is_empty() {
            return Err(unrepresentable(
                self,
                TransformDirection::ClientResponse,
                lost,
            ));
        }
        Ok(adapter(self.ingress)?.encode_response(canonical))
    }

    pub(crate) fn stream(self) -> Result<StreamSession, TransformError> {
        if !adapter(self.egress)?.capabilities().streaming {
            return Err(TransformError::UnsupportedOperation {
                endpoint: self.egress,
                operation: "stream",
            });
        }
        let decoder = adapter(self.egress)?.stream_decoder()?;
        let encoder = adapter(self.ingress)?.stream_encoder()?;
        Ok(StreamSession {
            decoder: StreamDecodeStage {
                pair: self,
                decoder,
                closed: false,
            },
            encoder: StreamEncodeStage {
                pair: self,
                encoder,
                closed: false,
            },
        })
    }
}
pub(crate) struct StreamSession {
    decoder: StreamDecodeStage,
    encoder: StreamEncodeStage,
}

impl StreamSession {
    pub(crate) fn into_parts(self) -> (StreamDecodeStage, StreamEncodeStage) {
        (self.decoder, self.encoder)
    }
}

pub(crate) struct StreamDecodeStage {
    pair: ProtocolPair,
    decoder: WireStreamDecoder,
    closed: bool,
}

impl StreamDecodeStage {
    pub(crate) fn decode_chunk(
        &mut self,
        raw: &[u8],
    ) -> Result<Vec<AiStreamDelta>, TransformError> {
        if self.closed {
            return Err(stream_closed(
                self.pair.egress,
                TransformDirection::UpstreamStream,
            ));
        }
        self.decoder
            .parse_chunk(raw)
            .map_err(|source| TransformError::Wire {
                endpoint: self.pair.egress,
                direction: TransformDirection::UpstreamStream.as_str(),
                source,
            })
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<AiStreamDelta>, TransformError> {
        if self.closed {
            return Err(stream_closed(
                self.pair.egress,
                TransformDirection::UpstreamStream,
            ));
        }
        self.closed = true;
        self.decoder
            .finish()
            .map_err(|source| TransformError::Wire {
                endpoint: self.pair.egress,
                direction: TransformDirection::UpstreamStream.as_str(),
                source,
            })
    }
}

pub(crate) struct StreamEncodeStage {
    pair: ProtocolPair,
    encoder: WireStreamEncoder,
    closed: bool,
}

impl StreamEncodeStage {
    pub(crate) fn encode_deltas(
        &mut self,
        deltas: &[AiStreamDelta],
    ) -> Result<Vec<SseEvent>, TransformError> {
        if self.closed {
            return Err(stream_closed(
                self.pair.ingress,
                TransformDirection::ClientResponse,
            ));
        }
        let lost = stream_loss_paths(self.pair, deltas);
        if !lost.is_empty() {
            self.closed = true;
            return Err(unrepresentable(
                self.pair,
                TransformDirection::ClientResponse,
                lost,
            ));
        }
        Ok(self.encoder.format_deltas(deltas))
    }

    pub(crate) fn set_response_profile(
        &mut self,
        request: &AiRequest,
        previous_response_id: Option<&str>,
    ) {
        if let WireStreamEncoder::Responses(formatter) = &mut self.encoder {
            formatter.set_response_profile_from_request(request, previous_response_id);
        }
    }

    pub(crate) fn fail(&mut self, error: crate::protocol::ir::AiError) -> Vec<SseEvent> {
        self.closed = true;
        let mut events = self.encoder.format_deltas(&[
            AiStreamDelta::StreamError { error },
            AiStreamDelta::Done {
                stop_reason: "failed".into(),
            },
        ]);
        events.extend(self.encoder.format_done());
        events
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<SseEvent>, TransformError> {
        if self.closed {
            return Err(stream_closed(
                self.pair.ingress,
                TransformDirection::ClientResponse,
            ));
        }
        self.closed = true;
        Ok(self.encoder.format_done())
    }
}

pub(crate) enum WireStreamDecoder {
    OpenAi(crate::protocol::codec::openai::compatible::stream::OpenAIStreamParser),
    Responses(Box<crate::protocol::codec::open_responses::parser::ResponsesStreamParser>),
    Anthropic(crate::protocol::codec::anthropic::messages::stream::AnthropicStreamParser),
    Google(crate::protocol::codec::google::gemini::stream::GoogleStreamParser),
    Bedrock(crate::protocol::codec::bedrock::BedrockStreamParser),
    Cohere(crate::protocol::codec::cohere::CohereStreamParser),
    Gateway(crate::protocol::codec::gateway::GatewayStreamParser),
}

impl WireStreamDecoder {
    fn parse_chunk(&mut self, raw: &[u8]) -> anyhow::Result<Vec<AiStreamDelta>> {
        match self {
            Self::OpenAi(parser) => parser.parse_chunk(std::str::from_utf8(raw)?),
            Self::Responses(parser) => parser.parse_chunk(std::str::from_utf8(raw)?),
            Self::Anthropic(parser) => parser.parse_chunk(std::str::from_utf8(raw)?),
            Self::Google(parser) => parser.parse_chunk(std::str::from_utf8(raw)?),
            Self::Bedrock(parser) => parser.parse_chunk(raw),
            Self::Cohere(parser) => parser.parse_chunk(std::str::from_utf8(raw)?),
            Self::Gateway(parser) => parser.parse_chunk(std::str::from_utf8(raw)?),
        }
    }

    fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        match self {
            Self::OpenAi(parser) => parser.finish(),
            Self::Responses(parser) => parser.finish(),
            Self::Anthropic(parser) => parser.finish(),
            Self::Google(parser) => parser.finish(),
            Self::Bedrock(parser) => parser.finish(),
            Self::Cohere(parser) => parser.finish(),
            Self::Gateway(parser) => parser.finish(),
        }
    }
}

pub(crate) enum WireStreamEncoder {
    OpenAi(crate::protocol::codec::openai::compatible::stream::OpenAIStreamFormatter),
    Responses(Box<crate::protocol::codec::open_responses::stream::ResponsesStreamFormatter>),
    Anthropic(crate::protocol::codec::anthropic::messages::stream::AnthropicStreamFormatter),
    Google(crate::protocol::codec::google::gemini::stream::GoogleStreamFormatter),
}

impl WireStreamEncoder {
    fn format_deltas(&mut self, deltas: &[AiStreamDelta]) -> Vec<SseEvent> {
        match self {
            Self::OpenAi(formatter) => formatter.format_deltas(deltas),
            Self::Responses(formatter) => formatter.format_deltas(deltas),
            Self::Anthropic(formatter) => formatter.format_deltas(deltas),
            Self::Google(formatter) => formatter.format_deltas(deltas),
        }
    }

    fn format_done(&mut self) -> Vec<SseEvent> {
        match self {
            Self::OpenAi(formatter) => formatter.format_done(),
            Self::Responses(formatter) => formatter.format_done(),
            Self::Anthropic(formatter) => formatter.format_done(),
            Self::Google(formatter) => formatter.format_done(),
        }
    }
}

fn adapter(endpoint: ProtocolEndpoint) -> Result<&'static dyn ProtocolAdapter, TransformError> {
    ProtocolRegistry::global()
        .adapter(&endpoint)
        .map(|adapter| adapter.as_ref())
        .ok_or(TransformError::Unsupported { endpoint })
}

fn unrepresentable(
    pair: ProtocolPair,
    direction: TransformDirection,
    lost: Vec<String>,
) -> TransformError {
    TransformError::Unrepresentable {
        ingress: pair.ingress,
        egress: pair.egress,
        direction: direction.as_str(),
        lost,
    }
}

fn stream_closed(endpoint: ProtocolEndpoint, direction: TransformDirection) -> TransformError {
    TransformError::StreamClosed {
        endpoint,
        direction: direction.as_str(),
    }
}

fn validate_response_cardinality(pair: ProtocolPair, body: &Value) -> Result<(), TransformError> {
    let lost = match pair.egress.protocol {
        Protocol::OpenAICompatible => body
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| choices.len() > 1)
            .then(|| vec!["choices".to_string()]),
        Protocol::GoogleGemini => body
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| candidates.len() > 1)
            .then(|| vec!["candidates".to_string()]),
        _ => None,
    };
    if let Some(lost) = lost {
        return Err(unrepresentable(
            pair,
            TransformDirection::UpstreamResponse,
            lost,
        ));
    }
    Ok(())
}

fn request_loss_paths(pair: ProtocolPair, request: &AiRequest) -> Vec<String> {
    if request
        .reasoning
        .target_control
        .as_ref()
        .is_some_and(|control| !thinking_control_representable(pair.egress.protocol, control))
    {
        return vec!["reasoning.target_control".to_string()];
    }
    if pair.ingress == pair.egress {
        return Vec::new();
    }

    let mut lost = Vec::new();
    let capabilities = adapter(pair.egress)
        .expect("bound egress adapter")
        .capabilities();

    if request.embedding.is_some() && !capabilities.embeddings {
        lost.push("embedding".to_string());
    }
    if request
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        && !capabilities.function_calling
    {
        lost.push("tools".to_string());
    }
    if let Some(tools) = &request.tools {
        for (index, tool) in tools.iter().enumerate() {
            if tool.strict == Some(true)
                && matches!(
                    pair.egress.protocol,
                    Protocol::AnthropicMessages | Protocol::GoogleGemini
                )
            {
                lost.push(format!("tools[{index}].strict"));
            }
            if pair.egress.protocol == Protocol::GoogleGemini
                && gemini_drops_schema_constraint(&tool.parameters)
            {
                lost.push(format!("tools[{index}].parameters"));
            }
        }
    }
    if let Some(tool_choice) = &request.tool_choice {
        let unsupported = match pair.egress.protocol {
            Protocol::AnthropicMessages => {
                matches!(tool_choice, crate::protocol::ir::ToolChoice::None)
            }
            Protocol::GoogleGemini => !matches!(tool_choice, crate::protocol::ir::ToolChoice::Auto),
            Protocol::BedrockConverse => {
                matches!(tool_choice, crate::protocol::ir::ToolChoice::None)
            }
            Protocol::OpenAICompatible
            | Protocol::OpenResponses
            | Protocol::CohereChat
            | Protocol::WatsonxTextChat
            | Protocol::GatewayLanguageModel => false,
        };
        push_if(&mut lost, unsupported, "tool_choice");
    }
    if (request.parallel_tool_calls.is_some() || request.disable_parallel_tool_calls.is_some())
        && !capabilities.parallel_tool_calls
    {
        lost.push("parallel_tool_calls".to_string());
    }
    if request.response_format.is_some() && !capabilities.structured_output {
        lost.push("response_format".to_string());
    }
    if request.generation.seed.is_some() && !capabilities.deterministic_seed {
        lost.push("seed".to_string());
    }
    if request.safety_settings.is_some() && pair.egress.protocol != Protocol::GoogleGemini {
        lost.push("safety_settings".to_string());
    }

    match request.ext.as_ref() {
        Some(ProtocolExt::OpenAiChat(extension)) => {
            push_if(&mut lost, extension.n.is_some_and(|n| n > 1), "n");
            push_if(&mut lost, extension.audio.is_some(), "audio");
            push_if(&mut lost, extension.logit_bias.is_some(), "logit_bias");
            push_if(&mut lost, extension.logprobs.is_some(), "logprobs");
            push_if(&mut lost, extension.top_logprobs.is_some(), "top_logprobs");
            push_if(&mut lost, extension.modalities.is_some(), "modalities");
            push_if(&mut lost, extension.prediction.is_some(), "prediction");
            push_if(
                &mut lost,
                extension.prompt_cache_retention.is_some(),
                "prompt_cache_retention",
            );
            push_if(&mut lost, extension.verbosity.is_some(), "verbosity");
            push_if(
                &mut lost,
                extension.web_search_options.is_some(),
                "web_search_options",
            );
        }
        Some(ProtocolExt::OpenResponses(extension)) => {
            push_if(&mut lost, extension.background == Some(true), "background");
            push_if(
                &mut lost,
                !extension.passthrough_tools.is_empty()
                    && matches!(
                        request.tool_choice,
                        Some(
                            crate::protocol::ir::ToolChoice::Required
                                | crate::protocol::ir::ToolChoice::Named { .. }
                                | crate::protocol::ir::ToolChoice::Raw(_)
                        )
                    ),
                "tools",
            );
            push_if(
                &mut lost,
                extension.max_tool_calls.is_some(),
                "max_tool_calls",
            );
            push_if(&mut lost, extension.truncation.is_some(), "truncation");
            let text_is_unrepresentable = extension.text.as_ref().is_some_and(|text| {
                let format = text.pointer("/format/type").and_then(Value::as_str);
                let format_supported = matches!(format, None | Some("text"))
                    || (pair.egress.protocol == Protocol::GoogleGemini
                        && format == Some("json_schema")
                        && text.pointer("/format/description").is_none()
                        && matches!(
                            &request.response_format,
                            Some(crate::protocol::ir::ResponseFormat::JsonSchema {
                                schema,
                                ..
                            }) if crate::protocol::codec::google::gemini::encoder::schema_is_losslessly_representable(schema)
                        ));
                !format_supported
            });
            push_if(&mut lost, text_is_unrepresentable, "text");
            push_if(
                &mut lost,
                extension.tool_choice_ext.is_some(),
                "tool_choice",
            );
        }
        Some(ProtocolExt::Anthropic(extension)) => {
            push_if(&mut lost, extension.top_k.is_some(), "top_k");
            push_if(&mut lost, extension.container.is_some(), "container");
            push_if(
                &mut lost,
                extension.inference_geo.is_some(),
                "inference_geo",
            );
            push_if(
                &mut lost,
                extension.output_config.is_some(),
                "output_config",
            );
            push_if(&mut lost, extension.service_tier.is_some(), "service_tier");
            push_if(&mut lost, extension.server_tools.is_some(), "server_tools");
        }
        Some(ProtocolExt::Google(extension)) => {
            push_if(
                &mut lost,
                extension.candidate_count.is_some_and(|count| count > 1),
                "candidate_count",
            );
            push_if(&mut lost, extension.top_k.is_some(), "top_k");
            push_if(
                &mut lost,
                extension.response_logprobs.is_some(),
                "response_logprobs",
            );
            push_if(&mut lost, extension.logprobs.is_some(), "logprobs");
            push_if(
                &mut lost,
                extension.response_mime_type.is_some(),
                "response_mime_type",
            );
            push_if(
                &mut lost,
                extension.response_json_schema.is_some(),
                "response_json_schema",
            );
            push_if(&mut lost, extension.tool_config.is_some(), "tool_config");
            push_if(
                &mut lost,
                extension.cached_content.is_some(),
                "cached_content",
            );
            push_if(
                &mut lost,
                extension.response_modalities.is_some(),
                "response_modalities",
            );
        }
        None => {}
    }

    for (message_index, message) in request.items.iter().enumerate() {
        let crate::protocol::ir::MessageContent::Blocks(blocks) = &message.content else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            if !request_block_representable(pair.egress.protocol, message.role, block) {
                lost.push(format!("messages[{message_index}].content[{block_index}]"));
            }
            if matches!(
                block,
                crate::protocol::ir::ContentBlock::Image {
                    detail: Some(_),
                    ..
                }
            ) && matches!(
                pair.egress.protocol,
                Protocol::AnthropicMessages | Protocol::GoogleGemini
            ) {
                lost.push(format!(
                    "messages[{message_index}].content[{block_index}].detail"
                ));
            }
        }
    }
    for field in request.meta.vendor.passthrough_safe.keys() {
        lost.push(format!("vendor.passthrough_safe.{field}"));
    }

    lost.sort();
    lost.dedup();
    lost
}

fn thinking_control_representable(
    protocol: Protocol,
    control: &crate::thinking::TargetThinkingControl,
) -> bool {
    use crate::thinking::TargetThinkingControl;
    match protocol {
        Protocol::OpenAICompatible => matches!(control, TargetThinkingControl::Effort { .. }),
        Protocol::OpenResponses => match control {
            TargetThinkingControl::Effort { .. } => true,
            TargetThinkingControl::Disabled => true,
            _ => false,
        },
        Protocol::AnthropicMessages | Protocol::GoogleGemini => {
            !matches!(control, TargetThinkingControl::Hidden)
        }
        _ => false,
    }
}
fn gemini_drops_schema_constraint(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "$schema" | "additionalProperties" | "$ref" | "ref" | "definitions" | "$defs"
                )
            }) || object.values().any(gemini_drops_schema_constraint)
        }
        Value::Array(values) => values.iter().any(gemini_drops_schema_constraint),
        _ => false,
    }
}

fn request_block_representable(
    target: Protocol,
    role: crate::protocol::ir::Role,
    block: &crate::protocol::ir::ContentBlock,
) -> bool {
    use crate::protocol::ir::ContentBlock;

    match target {
        Protocol::AnthropicMessages => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::ServerToolUse { .. }
                | ContentBlock::ServerToolResult { .. }
        ),
        Protocol::GoogleGemini => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::File { .. }
                | ContentBlock::Video { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::ExecutableCode { .. }
                | ContentBlock::CodeExecutionResult { .. }
        ),
        Protocol::OpenAICompatible => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Audio { .. }
                | ContentBlock::File { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Thinking {
                    signature: None,
                    ..
                }
        ),
        Protocol::OpenResponses => {
            matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::File { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. }
            ) || (role == crate::protocol::ir::Role::Assistant
                && matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::Reasoning { .. }
                ))
        }
        Protocol::BedrockConverse => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image {
                    source: crate::protocol::ir::MediaSource::Base64 { .. },
                    ..
                }
                | ContentBlock::Thinking {
                    signature: Some(_),
                    ..
                }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
        ),
        Protocol::CohereChat => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
        ),
        Protocol::WatsonxTextChat => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Thinking {
                    signature: None,
                    ..
                }
        ),
        Protocol::GatewayLanguageModel => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::File { .. }
                | ContentBlock::Video { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Thinking {
                    signature: None,
                    ..
                }
        ),
    }
}

fn open_responses_output_item_representable(item: &crate::protocol::ir::AiItem) -> bool {
    if let Some(raw) = item.unknown_ref() {
        return raw
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(crate::protocol::codec::open_responses::is_registered_extension_item);
    }
    if item.function_call_ref().is_some() {
        return true;
    }
    if let Some((_, content)) = item.function_call_output_ref() {
        return crate::protocol::codec::open_responses::encoder::tool_output_representable(content);
    }
    match &item.content {
        crate::protocol::ir::MessageContent::Text(_) => {
            item.role == crate::protocol::ir::Role::Assistant
        }
        crate::protocol::ir::MessageContent::Blocks(blocks) => {
            (item.role == crate::protocol::ir::Role::Assistant
                && blocks.iter().all(|block| {
                    matches!(
                        block,
                        crate::protocol::ir::ContentBlock::Text { .. }
                            | crate::protocol::ir::ContentBlock::Refusal { .. }
                    )
                }))
                || (blocks.len() == 1
                    && matches!(
                        blocks.first(),
                        Some(
                            crate::protocol::ir::ContentBlock::Reasoning { .. }
                                | crate::protocol::ir::ContentBlock::Thinking { .. }
                        )
                    ))
        }
    }
}

fn canonicalized_google_stream_metadata(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.get("__google_response_metadata").cloned())
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|metadata| {
            metadata.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "usageMetadata" | "modelVersion" | "responseId"
                )
            })
        })
}

fn response_loss_paths(pair: ProtocolPair, response: &AiResponse) -> Vec<String> {
    if pair.ingress == pair.egress {
        return Vec::new();
    }
    let mut lost = Vec::new();
    if response
        .protected_reasoning_signatures()
        .any(|signature| signature.is_some())
        && !matches!(
            pair.ingress.protocol,
            Protocol::AnthropicMessages
                | Protocol::GoogleGemini
                | Protocol::OpenAICompatible
                | Protocol::OpenResponses
        )
    {
        lost.push("reasoning_signature".to_string());
    }
    if pair.ingress.protocol == Protocol::OpenResponses {
        for (index, item) in response.items.iter().enumerate() {
            if !open_responses_output_item_representable(item) {
                lost.push(format!("items[{index}]"));
            }
        }
    }
    if pair.ingress.protocol != Protocol::OpenResponses {
        for (index, item) in response.items.iter().enumerate() {
            if matches!(
                &item.content,
                crate::protocol::ir::MessageContent::Blocks(blocks)
                    if blocks.iter().any(|block| matches!(
                        block,
                        crate::protocol::ir::ContentBlock::Refusal { .. }
                    ))
            ) {
                lost.push(format!("items[{index}].refusal"));
            }
            if pair.ingress.protocol != Protocol::OpenAICompatible
                && matches!(
                &item.content,
                crate::protocol::ir::MessageContent::Blocks(blocks)
                    if blocks.iter().any(|block| matches!(
                        block,
                        crate::protocol::ir::ContentBlock::Reasoning { .. }
                    ))
                )
            {
                lost.push(format!("items[{index}].reasoning_structure"));
            }
            if let Some(content) = item
                .meta
                .as_ref()
                .and_then(|meta| meta.get("__open_responses_content"))
                .and_then(Value::as_array)
            {
                if content.iter().any(|block| {
                    block
                        .get("annotations")
                        .and_then(Value::as_array)
                        .is_some_and(|annotations| !annotations.is_empty())
                }) {
                    lost.push(format!("items[{index}].annotations"));
                }
                if content.iter().any(|block| {
                    block
                        .get("logprobs")
                        .and_then(Value::as_array)
                        .is_some_and(|logprobs| !logprobs.is_empty())
                }) {
                    lost.push(format!("items[{index}].logprobs"));
                }
            }
            if item.function_call_output_ref().is_some()
                || item.has_search_result()
                || item.unknown_ref().is_some()
            {
                lost.push(format!("items[{index}]"));
            }
        }
    }
    for field in response.vendor.passthrough_safe.keys() {
        lost.push(format!("vendor.passthrough_safe.{field}"));
    }
    lost.sort();
    lost.dedup();
    lost
}

fn stream_loss_paths(pair: ProtocolPair, deltas: &[AiStreamDelta]) -> Vec<String> {
    if pair.ingress == pair.egress {
        return Vec::new();
    }
    let mut lost = Vec::new();
    for (index, delta) in deltas.iter().enumerate() {
        match delta {
            AiStreamDelta::ThinkingSignature(_)
                if !matches!(
                    pair.ingress.protocol,
                    Protocol::AnthropicMessages
                        | Protocol::GoogleGemini
                        | Protocol::OpenAICompatible
                        | Protocol::OpenResponses
                ) =>
            {
                lost.push(format!("deltas[{index}].thinking_signature"));
            }
            AiStreamDelta::RefusalDelta(_)
            | AiStreamDelta::RefusalDeltaWithIndex { .. }
                if pair.ingress.protocol != Protocol::OpenResponses =>
            {
                lost.push(format!("deltas[{index}].refusal"));
            }
            AiStreamDelta::TextDeltaWithMetadata { logprobs, .. } => {
                if !logprobs.is_empty() {
                    lost.push(format!("deltas[{index}].logprobs"));
                }
            }
            AiStreamDelta::ReasoningSummaryDelta { .. }
                if !matches!(
                    pair.ingress.protocol,
                    Protocol::AnthropicMessages
                        | Protocol::GoogleGemini
                        | Protocol::OpenAICompatible
                ) =>
            {
                lost.push(format!("deltas[{index}].reasoning_summary"));
            }
            AiStreamDelta::Unknown { raw }
                if pair.ingress.protocol == Protocol::OpenResponses
                    && serde_json::from_str::<serde_json::Value>(raw)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .map(crate::protocol::codec::open_responses::is_registered_extension_item)
                        })
                        .unwrap_or(false) => {}
            AiStreamDelta::Unknown { raw }
                if pair.egress.protocol == Protocol::GoogleGemini
                    && canonicalized_google_stream_metadata(raw) => {}
            AiStreamDelta::Unknown { .. } => lost.push(format!("deltas[{index}].unknown")),
            _ => {}
        }
    }
    lost
}

fn push_if(fields: &mut Vec<String>, condition: bool, field: &str) {
    if condition {
        fields.push(field.to_string());
    }
}

#[cfg(test)]
mod tests;
