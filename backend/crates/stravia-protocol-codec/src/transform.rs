use http::header::HeaderMap;
use serde_json::Value;

use crate::SseEvent;
use crate::registry::ProtocolRegistry;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::Protocol;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;

mod replay;
pub use replay::prepare_thinking_replay;

pub trait ProtocolAdapter: Send + Sync + 'static {
    fn id(&self) -> ProtocolEndpoint;
    fn capabilities(&self) -> &'static EndpointCapabilities;
    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest>;
    fn encode_request(&self, request: AiRequest) -> anyhow::Result<(Value, HeaderMap)>;
    fn request_path(&self, model: &str, stream: bool) -> String;
    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse>;
    fn encode_response(&self, response: &AiResponse) -> Value;
    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError>;
    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformDirection {
    ClientRequest,
    UpstreamResponse,
    UpstreamStream,
    ClientResponse,
}

impl TransformDirection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClientRequest => "client_request",
            Self::UpstreamResponse => "upstream_response",
            Self::UpstreamStream => "upstream_stream",
            Self::ClientResponse => "client_response",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransformError {
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
    #[error("{direction} for {endpoint} is already closed")]
    StreamClosed {
        endpoint: ProtocolEndpoint,
        direction: &'static str,
    },
}

#[derive(Debug)]
pub struct EncodedRequest {
    pub body: Value,
    pub headers: HeaderMap,
    pub path: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ProtocolPair {
    ingress: ProtocolEndpoint,
    egress: ProtocolEndpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingCarrierFacts {
    pub indexed: bool,
    pub may_be_protected: bool,
    pub stream_unprotected_summaries: bool,
}

/// 受保护推理是否只能回放给签发它的同一模型。Anthropic 同一部署内跨模型的签名
/// 交由上游自行校验；Gemini thought signature 未承诺跨模型可用，保守按模型绑定。
pub fn protected_thinking_binds_model(protocol: Protocol) -> bool {
    matches!(protocol, Protocol::GoogleGemini)
}

pub struct ProtocolTransform;

impl ProtocolTransform {
    pub fn global() -> &'static Self {
        static TRANSFORM: ProtocolTransform = ProtocolTransform;
        &TRANSFORM
    }

    pub fn bind(
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

    /// Build only the upstream decoding side for a registered protocol.
    ///
    /// This avoids requiring a client-side stream formatter when only upstream
    /// decoding is needed. Plugin-owned adapters use [`Self::decode_stream_with`].
    pub fn decode_stream(
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
            pending_utf8: Vec::new(),
            closed: false,
        })
    }

    /// Encode a canonical request with a plugin-owned adapter that is not in
    /// the host's standard protocol registry.
    pub fn encode_request_with(
        adapter: &dyn ProtocolAdapter,
        mut canonical: AiRequest,
    ) -> Result<EncodedRequest, TransformError> {
        let endpoint = adapter.id();
        Self::prune_request_for(endpoint, adapter.capabilities(), &mut canonical);
        let path = adapter.request_path(&canonical.model, canonical.stream.enabled);
        let (body, headers) =
            adapter
                .encode_request(canonical)
                .map_err(|source| TransformError::Wire {
                    endpoint,
                    direction: TransformDirection::ClientRequest.as_str(),
                    source,
                })?;
        Ok(EncodedRequest {
            body,
            headers,
            path,
        })
    }

    /// 在此次编码的 canonical 副本上省略目标无法承载的数据。
    pub fn prune_request_for(
        endpoint: ProtocolEndpoint,
        capabilities: &EndpointCapabilities,
        canonical: &mut AiRequest,
    ) {
        let pair = ProtocolPair {
            ingress: Self::inferred_ingress(canonical).unwrap_or(endpoint),
            egress: endpoint,
        };
        prune_request(pair, canonical, capabilities)
    }

    /// Decode a unary response with a plugin-owned adapter.
    pub fn decode_response_with(
        adapter: &dyn ProtocolAdapter,
        body: Value,
    ) -> Result<AiResponse, TransformError> {
        let endpoint = adapter.id();
        let mut response =
            adapter
                .decode_response(body)
                .map_err(|source| TransformError::Wire {
                    endpoint,
                    direction: TransformDirection::UpstreamResponse.as_str(),
                    source,
                })?;
        crate::codec::reasoning::normalize_response_reasoning(&mut response);
        Ok(response)
    }

    /// Build a streaming response decoder with a plugin-owned adapter.
    pub fn decode_stream_with(
        adapter: &dyn ProtocolAdapter,
    ) -> Result<StreamDecodeStage, TransformError> {
        StreamDecodeStage::from_adapter(adapter)
    }

    pub fn inferred_ingress(request: &AiRequest) -> Option<ProtocolEndpoint> {
        request.meta.source_protocol
    }
}

impl ProtocolPair {
    pub fn thinking_carrier_facts(self) -> ThinkingCarrierFacts {
        let indexed = self.egress.protocol == Protocol::OpenResponses;
        let may_be_protected = matches!(
            self.egress.protocol,
            Protocol::OpenResponses | Protocol::AnthropicMessages | Protocol::GoogleGemini
        );
        ThinkingCarrierFacts {
            indexed,
            may_be_protected,
            // Open Responses summaries are the public display carrier.
            // Requesting `reasoning.encrypted_content` only asks for the
            // opaque replay blob; it must not delay those deltas until
            // item.done, where ciphertext may arrive.
            stream_unprotected_summaries: self.egress.protocol == Protocol::OpenResponses,
        }
    }

    pub fn decode_request(self, body: Value) -> Result<AiRequest, TransformError> {
        let mut request = adapter(self.ingress)?
            .decode_request(body)
            .map_err(|source| TransformError::Wire {
                endpoint: self.ingress,
                direction: TransformDirection::ClientRequest.as_str(),
                source,
            })?;
        // Response occurrence boundaries come from completed platform responses,
        // never from arbitrary extension fields supplied by a client.
        for item in &mut request.items {
            if let Some(meta) = item.meta.as_mut() {
                meta.remove_extension(
                    stravia_runtime_contract::protocol::ir::response::RESPONSE_HISTORY_GROUP_META,
                )
                .expect("response grouping is internal metadata");
            }
        }
        Ok(request)
    }

    pub fn encode_request(
        self,
        mut canonical: AiRequest,
    ) -> Result<EncodedRequest, TransformError> {
        let target = adapter(self.egress)?;
        prune_request(self, &mut canonical, target.capabilities());
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

    pub fn decode_response(self, body: Value) -> Result<AiResponse, TransformError> {
        let mut response = adapter(self.egress)?
            .decode_response(body)
            .map_err(|source| TransformError::Wire {
                endpoint: self.egress,
                direction: TransformDirection::UpstreamResponse.as_str(),
                source,
            })?;
        crate::codec::reasoning::normalize_response_reasoning(&mut response);
        Ok(response)
    }

    pub fn encode_response(self, canonical: &AiResponse) -> Result<Value, TransformError> {
        let target = adapter(self.ingress)?;
        let response = prune_response(self, canonical);
        Ok(target.encode_response(response.as_ref()))
    }

    pub fn stream(self) -> Result<StreamSession, TransformError> {
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
                pending_utf8: Vec::new(),
                closed: false,
            },
            encoder: StreamEncodeStage {
                pair: self,
                encoder,
                closed: false,
                anthropic_tools: std::collections::BTreeMap::new(),
            },
        })
    }
}
pub struct StreamSession {
    decoder: StreamDecodeStage,
    encoder: StreamEncodeStage,
}

impl StreamSession {
    pub fn into_parts(self) -> (StreamDecodeStage, StreamEncodeStage) {
        (self.decoder, self.encoder)
    }
}

pub struct StreamDecodeStage {
    pair: ProtocolPair,
    decoder: WireStreamDecoder,
    pending_utf8: Vec<u8>,
    closed: bool,
}

impl StreamDecodeStage {
    pub fn from_adapter(adapter: &dyn ProtocolAdapter) -> Result<Self, TransformError> {
        let endpoint = adapter.id();
        if !adapter.capabilities().streaming {
            return Err(TransformError::UnsupportedOperation {
                endpoint,
                operation: "stream",
            });
        }
        Ok(Self {
            pair: ProtocolPair {
                ingress: endpoint,
                egress: endpoint,
            },
            decoder: adapter.stream_decoder()?,
            pending_utf8: Vec::new(),
            closed: false,
        })
    }

    pub fn decode_chunk(&mut self, raw: &[u8]) -> Result<Vec<AiStreamDelta>, TransformError> {
        if self.closed {
            return Err(stream_closed(
                self.pair.egress,
                TransformDirection::UpstreamStream,
            ));
        }
        self.decoder
            .parse_chunk(raw, &mut self.pending_utf8)
            .map_err(|source| TransformError::Wire {
                endpoint: self.pair.egress,
                direction: TransformDirection::UpstreamStream.as_str(),
                source,
            })
    }

    pub fn finish(&mut self) -> Result<Vec<AiStreamDelta>, TransformError> {
        if self.closed {
            return Err(stream_closed(
                self.pair.egress,
                TransformDirection::UpstreamStream,
            ));
        }
        self.closed = true;
        if let Err(source) = std::str::from_utf8(&self.pending_utf8) {
            return Err(TransformError::Wire {
                endpoint: self.pair.egress,
                direction: TransformDirection::UpstreamStream.as_str(),
                source: source.into(),
            });
        }
        self.decoder
            .finish()
            .map_err(|source| TransformError::Wire {
                endpoint: self.pair.egress,
                direction: TransformDirection::UpstreamStream.as_str(),
                source,
            })
    }
}

pub struct StreamEncodeStage {
    pair: ProtocolPair,
    encoder: WireStreamEncoder,
    closed: bool,
    // Cross-protocol Anthropic inputs must be complete objects before opening a
    // tool block. Keep identities separate while upstream fragments interleave.
    anthropic_tools: std::collections::BTreeMap<usize, (String, String, String)>,
}

impl StreamEncodeStage {
    pub fn encode_deltas(
        &mut self,
        deltas: &[AiStreamDelta],
    ) -> Result<Vec<SseEvent>, TransformError> {
        if self.closed {
            return Err(stream_closed(
                self.pair.ingress,
                TransformDirection::ClientResponse,
            ));
        }
        if self.pair.ingress == self.pair.egress {
            return Ok(self.encoder.format_deltas(deltas));
        }
        if self.pair.ingress.protocol == Protocol::AnthropicMessages {
            let mut events = Vec::new();
            for delta in deltas {
                match delta {
                    AiStreamDelta::ToolCallStart { index, id, name } => {
                        self.anthropic_tools
                            .insert(*index, (id.clone(), name.clone(), String::new()));
                    }
                    AiStreamDelta::ToolCallDelta { index, arguments } => {
                        if let Some((_, _, buffered)) = self.anthropic_tools.get_mut(index) {
                            buffered.push_str(arguments);
                        }
                    }
                    AiStreamDelta::ToolCallComplete { index, tool_call } => {
                        self.anthropic_tools.remove(index);
                        self.emit_anthropic_tool(
                            *index,
                            &tool_call.id,
                            &tool_call.name,
                            &tool_call.arguments,
                            &mut events,
                        );
                    }
                    AiStreamDelta::Done { .. } => {
                        for (index, (id, name, arguments)) in
                            std::mem::take(&mut self.anthropic_tools)
                        {
                            self.emit_anthropic_tool(index, &id, &name, &arguments, &mut events);
                        }
                        events.extend(self.encoder.format_deltas(std::slice::from_ref(delta)));
                    }
                    AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof => {
                        // A failed/truncated stream cannot establish complete tool inputs.
                        self.anthropic_tools.clear();
                        events.extend(self.encoder.format_deltas(std::slice::from_ref(delta)));
                    }
                    _ => {
                        if let Some(projected) = prune_delta(self.pair, delta) {
                            events.extend(
                                self.encoder
                                    .format_deltas(std::slice::from_ref(projected.as_ref())),
                            );
                        }
                    }
                }
            }
            return Ok(events);
        }
        let mut events = Vec::new();
        let mut start = 0;
        for (index, delta) in deltas.iter().enumerate() {
            match prune_delta(self.pair, delta) {
                Some(std::borrow::Cow::Borrowed(_)) => continue,
                projected => {
                    if start < index {
                        events.extend(self.encoder.format_deltas(&deltas[start..index]));
                    }
                    if let Some(projected) = projected {
                        events.extend(
                            self.encoder
                                .format_deltas(std::slice::from_ref(projected.as_ref())),
                        );
                    }
                    start = index + 1;
                }
            }
        }
        if start == 0 {
            return Ok(self.encoder.format_deltas(deltas));
        }
        if start < deltas.len() {
            events.extend(self.encoder.format_deltas(&deltas[start..]));
        }
        Ok(events)
    }

    fn emit_anthropic_tool(
        &mut self,
        index: usize,
        id: &str,
        name: &str,
        arguments: &str,
        events: &mut Vec<SseEvent>,
    ) {
        // partial_json is a string carrier, but its final value must be an
        // object. Preserve valid raw JSON rather than synthesizing empty input.
        if !matches!(
            serde_json::from_str::<Value>(arguments),
            Ok(Value::Object(_))
        ) {
            return;
        }
        events.extend(self.encoder.format_deltas(&[
            AiStreamDelta::ToolCallStart {
                index,
                id: id.into(),
                name: name.into(),
            },
            AiStreamDelta::ToolCallDelta {
                index,
                arguments: arguments.into(),
            },
        ]));
    }

    pub fn set_response_profile(
        &mut self,
        request: &AiRequest,
        previous_response_id: Option<&str>,
    ) {
        if let WireStreamEncoder::Responses(formatter) = &mut self.encoder {
            formatter.set_response_profile_from_request(request, previous_response_id);
        }
    }

    pub fn fail(
        &mut self,
        error: stravia_runtime_contract::protocol::ir::AiError,
    ) -> Vec<SseEvent> {
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

    pub fn finish(&mut self) -> Result<Vec<SseEvent>, TransformError> {
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

/// Text-oriented upstream stream parser supplied by a vendor plugin.
///
/// The shared codec owns UTF-8 boundary handling while the plugin owns the
/// proprietary event grammar. This keeps vendor wire implementations out of
/// host builds without duplicating the common stream stage.
pub trait TextWireStreamParser: Send {
    fn parse_text_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>>;
    fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>>;
}

/// Byte-oriented upstream stream parser supplied by a vendor plugin.
///
/// Binary protocols receive transport chunks unchanged and must handle their
/// own frame and field boundaries.
pub trait BinaryWireStreamParser: Send {
    fn parse_binary_chunk(&mut self, raw: &[u8]) -> anyhow::Result<Vec<AiStreamDelta>>;
    fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>>;
}

pub enum WireStreamDecoder {
    OpenAi(crate::codec::openai::compatible::stream::OpenAIStreamParser),
    Responses(Box<crate::codec::open_responses::parser::ResponsesStreamParser>),
    Anthropic(crate::codec::anthropic::messages::stream::AnthropicStreamParser),
    Google(crate::codec::google::gemini::stream::GoogleStreamParser),
    CustomText(Box<dyn TextWireStreamParser>),
    CustomBinary(Box<dyn BinaryWireStreamParser>),
}

impl WireStreamDecoder {
    pub fn custom_text(parser: impl TextWireStreamParser + 'static) -> Self {
        Self::CustomText(Box::new(parser))
    }

    pub fn custom_binary(parser: impl BinaryWireStreamParser + 'static) -> Self {
        Self::CustomBinary(Box::new(parser))
    }

    fn parse_chunk(
        &mut self,
        raw: &[u8],
        pending_utf8: &mut Vec<u8>,
    ) -> anyhow::Result<Vec<AiStreamDelta>> {
        if let Self::CustomBinary(parser) = self {
            debug_assert!(pending_utf8.is_empty());
            return parser.parse_binary_chunk(raw);
        }

        let mut deltas = Vec::new();
        let mut remaining = raw;
        while !pending_utf8.is_empty() {
            let Some((&next, rest)) = remaining.split_first() else {
                return Ok(deltas);
            };
            pending_utf8.push(next);
            remaining = rest;
            match std::str::from_utf8(pending_utf8) {
                Ok(completed) => {
                    deltas.extend(self.parse_text_chunk(completed)?);
                    pending_utf8.clear();
                }
                Err(error) if error.error_len().is_none() => {}
                Err(error) => return Err(error.into()),
            }
        }

        match std::str::from_utf8(remaining) {
            Ok(text) => deltas.extend(self.parse_text_chunk(text)?),
            Err(error) if error.error_len().is_none() => {
                let (valid, incomplete) = remaining.split_at(error.valid_up_to());
                if !valid.is_empty() {
                    deltas.extend(
                        self.parse_text_chunk(
                            std::str::from_utf8(valid)
                                .expect("bytes before valid_up_to must be valid UTF-8"),
                        )?,
                    );
                }
                pending_utf8.extend_from_slice(incomplete);
            }
            Err(error) => return Err(error.into()),
        }
        Ok(deltas)
    }

    fn parse_text_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<AiStreamDelta>> {
        match self {
            Self::OpenAi(parser) => parser.parse_chunk(raw),
            Self::Responses(parser) => parser.parse_chunk(raw),
            Self::Anthropic(parser) => parser.parse_chunk(raw),
            Self::Google(parser) => parser.parse_chunk(raw),
            Self::CustomText(parser) => parser.parse_text_chunk(raw),
            Self::CustomBinary(_) => {
                unreachable!("binary stream decoding bypasses UTF-8 framing")
            }
        }
    }

    fn finish(&mut self) -> anyhow::Result<Vec<AiStreamDelta>> {
        match self {
            Self::OpenAi(parser) => parser.finish(),
            Self::Responses(parser) => parser.finish(),
            Self::Anthropic(parser) => parser.finish(),
            Self::Google(parser) => parser.finish(),
            Self::CustomText(parser) => parser.finish(),
            Self::CustomBinary(parser) => parser.finish(),
        }
    }
}

pub enum WireStreamEncoder {
    OpenAi(crate::codec::openai::compatible::stream::OpenAIStreamFormatter),
    Responses(Box<crate::codec::open_responses::stream::ResponsesStreamFormatter>),
    Anthropic(crate::codec::anthropic::messages::stream::AnthropicStreamFormatter),
    Google(crate::codec::google::gemini::stream::GoogleStreamFormatter),
}

impl WireStreamEncoder {
    pub fn format_deltas(&mut self, deltas: &[AiStreamDelta]) -> Vec<SseEvent> {
        match self {
            Self::OpenAi(formatter) => formatter.format_deltas(deltas),
            Self::Responses(formatter) => formatter.format_deltas(deltas),
            Self::Anthropic(formatter) => formatter.format_deltas(deltas),
            Self::Google(formatter) => formatter.format_deltas(deltas),
        }
    }

    pub fn format_done(&mut self) -> Vec<SseEvent> {
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

fn stream_closed(endpoint: ProtocolEndpoint, direction: TransformDirection) -> TransformError {
    TransformError::StreamClosed {
        endpoint,
        direction: direction.as_str(),
    }
}

fn prune_request(pair: ProtocolPair, request: &mut AiRequest, capabilities: &EndpointCapabilities) {
    use stravia_runtime_contract::protocol::ir::{ResponseFormat, ToolChoice};
    // Normalize before dropping foreign protocol fidelity state.
    request.normalize_parameters();
    if request
        .reasoning
        .target_control
        .as_ref()
        .is_some_and(|control| !thinking_control_representable(pair.egress.protocol, control))
    {
        request.reasoning.target_control = None;
    }
    if !capabilities.embeddings {
        request.embedding = None;
    }
    if !capabilities.function_calling {
        request.tools = None;
        request.tool_choice = None;
    }
    if !capabilities.parallel_tool_calls {
        request.parallel_tool_calls = None;
    }
    if !capabilities.structured_output {
        request.response_format = None;
    }
    if !capabilities.deterministic_seed {
        request.generation.seed = None;
    }
    if pair.egress.protocol == Protocol::BedrockConverse {
        request.generation.frequency_penalty = None;
        request.generation.presence_penalty = None;
        request.reasoning = Default::default();
    } else if !capabilities.reasoning {
        request.reasoning = Default::default();
    }
    if matches!(
        pair.egress.protocol,
        Protocol::OpenResponses | Protocol::AnthropicMessages
    ) && matches!(request.response_format, Some(ResponseFormat::JsonObject))
    {
        request.response_format = None;
    }
    if request
        .tool_choice
        .as_ref()
        .is_some_and(|choice| match pair.egress.protocol {
            Protocol::AnthropicMessages | Protocol::OpenAICompatible => {
                matches!(choice, ToolChoice::Allowed { .. } | ToolChoice::Native(_))
            }
            Protocol::BedrockConverse => matches!(
                choice,
                ToolChoice::None | ToolChoice::Allowed { .. } | ToolChoice::Native(_)
            ),
            Protocol::CohereChat => {
                matches!(choice, ToolChoice::Allowed { .. } | ToolChoice::Native(_))
            }
            Protocol::GoogleGemini => matches!(choice, ToolChoice::Native(_)),
            _ => false,
        })
    {
        request.tool_choice = None;
    }
    if pair.ingress != pair.egress {
        if !request.parameters.native_tools.is_empty()
            && !request.parameters.native_tools.iter().any(|tool| {
                use stravia_runtime_contract::protocol::ir::NativeTool;
                match pair.egress.protocol {
                    Protocol::OpenResponses => true,
                    Protocol::AnthropicMessages | Protocol::GoogleGemini => {
                        matches!(
                            tool,
                            NativeTool::WebSearch(_) | NativeTool::CodeExecution(_)
                        )
                    }
                    _ => false,
                }
            })
            && request.tools.as_ref().is_none_or(|tools| tools.is_empty())
        {
            request.tool_choice = None;
        }
        request.meta.vendor.passthrough_safe.clear();
        request.items.retain(|item| {
            item.unknown_ref().is_none()
                && (pair.egress.protocol == Protocol::OpenResponses
                    || (!item.is_compaction() && !item.is_compaction_trigger()))
        });
    }
    prune_items(
        pair.egress.protocol,
        &mut request.items,
        capabilities.function_calling,
        false,
        pair.ingress == pair.egress,
    );
}

/// 对象入参无法表达的调用和它关联的结果必须一起省略；绝不补空对象。
fn prune_items(
    target: Protocol,
    items: &mut Vec<stravia_runtime_contract::protocol::ir::AiItem>,
    tools_supported: bool,
    output: bool,
    preserve_blocks: bool,
) {
    let mut dropped_calls = std::collections::HashSet::new();
    for item in items.iter_mut() {
        prune_item(
            target,
            item,
            tools_supported,
            output,
            preserve_blocks,
            &mut dropped_calls,
        );
    }
    items.retain_mut(|item| retain_pruned_item(target, item, tools_supported, &dropped_calls));
}

fn prune_item(
    target: Protocol,
    item: &mut stravia_runtime_contract::protocol::ir::AiItem,
    tools_supported: bool,
    output: bool,
    preserve_blocks: bool,
    dropped_calls: &mut std::collections::HashSet<
        stravia_runtime_contract::protocol::ir::ToolCallId,
    >,
) {
    use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent};
    {
        if target == Protocol::OpenResponses
            && (item.is_compaction()
                || item.is_compaction_trigger()
                || item.unknown_ref().is_some()
                || item.function_call_output_ref().is_some())
        {
            return;
        }
        if let Some(calls) = &mut item.tool_calls {
            calls.retain(|call| {
                let keep = tool_call_representable(target, &call.arguments, tools_supported);
                if !keep {
                    dropped_calls.insert(call.id.clone());
                }
                keep
            });
            if calls.is_empty() {
                item.tool_calls = None;
            }
        }
        if let MessageContent::Blocks(blocks) = &mut item.content {
            if output || target != Protocol::AnthropicMessages {
                flatten_document_blocks(blocks);
            }
            blocks.retain_mut(|block| {
                if output && let ContentBlock::Document {
                    source: stravia_runtime_contract::protocol::ir::DocumentSource::PlainText { data }, ..
                } = block {
                    *block = ContentBlock::Text {
                        text: std::mem::take(data).into(), cache_control: None,
                    };
                }
                let keep = !(target == Protocol::BedrockConverse
                    && item.role != stravia_runtime_contract::protocol::ir::Role::User
                    && matches!(block, ContentBlock::Document { .. } | ContentBlock::File { .. }))
                    && item_block_representable(target, block, tools_supported, output, preserve_blocks);
                if !keep {
                    match block {
                        ContentBlock::ToolUse { id, .. } | ContentBlock::ServerToolUse { id, .. } => {
                            dropped_calls.insert(id.clone());
                        }
                        _ => {}
                    }
                }
                keep
            });
        }
    }
}

fn flatten_document_blocks(blocks: &mut Vec<stravia_runtime_contract::protocol::ir::ContentBlock>) {
    use stravia_runtime_contract::protocol::ir::{ContentBlock, DocumentSource};
    if !blocks.iter().any(|block| {
        matches!(
            block,
            ContentBlock::Document {
                source: DocumentSource::Blocks { .. },
                ..
            }
        )
    }) {
        return;
    }
    fn append(parts: &mut Vec<ContentBlock>, block: ContentBlock) {
        match block {
            ContentBlock::Document {
                source: DocumentSource::Blocks { content },
                ..
            } => content.into_iter().for_each(|block| append(parts, block)),
            block => parts.push(block),
        }
    }
    let mut parts = Vec::with_capacity(blocks.len());
    for block in std::mem::take(blocks) {
        append(&mut parts, block);
    }
    *blocks = parts;
}

fn retain_pruned_item(
    target: Protocol,
    item: &mut stravia_runtime_contract::protocol::ir::AiItem,
    tools_supported: bool,
    dropped_calls: &std::collections::HashSet<stravia_runtime_contract::protocol::ir::ToolCallId>,
) -> bool {
    use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent};
    if target == Protocol::OpenResponses
        && (item.is_compaction()
            || item.is_compaction_trigger()
            || item.unknown_ref().is_some()
            || item.function_call_output_ref().is_some())
    {
        return true;
    }
    if item
        .tool_call_id
        .as_ref()
        .is_some_and(|id| !tools_supported || dropped_calls.contains(id))
    {
        return false;
    }
    if let MessageContent::Blocks(blocks) = &mut item.content {
        blocks.retain(|block| {
            !matches!(block,
                ContentBlock::ToolResult { tool_use_id, .. } |
                ContentBlock::ServerToolResult { tool_use_id, .. }
                if dropped_calls.contains(tool_use_id))
        });
        if blocks.is_empty() && item.tool_calls.is_none() {
            return false;
        }
    }
    true
}

fn tool_call_representable(target: Protocol, arguments: &str, tools_supported: bool) -> bool {
    if !tools_supported {
        return false;
    }
    let object_arguments = matches!(
        target,
        Protocol::GoogleGemini | Protocol::AnthropicMessages | Protocol::BedrockConverse
    );
    // Only object-valued wire carriers (or the JSON-valued gateway carrier)
    // require parsing; string-valued carriers preserve the original arguments.
    !object_arguments && target != Protocol::GatewayLanguageModel
        || serde_json::from_str::<Value>(arguments)
            .is_ok_and(|args| !object_arguments || args.is_object())
}

fn item_block_representable(
    target: Protocol,
    block: &stravia_runtime_contract::protocol::ir::ContentBlock,
    tools_supported: bool,
    output: bool,
    preserve_blocks: bool,
) -> bool {
    use stravia_runtime_contract::protocol::ir::ContentBlock;
    let native = preserve_blocks
        && target == Protocol::AnthropicMessages
        && tools_supported
        && matches!(
            block,
            ContentBlock::ServerToolUse { .. } | ContentBlock::ServerToolResult { .. }
        );
    if native {
        return true;
    }
    let keep = match block {
        ContentBlock::ToolResult {
            content,
            content_kind,
            ..
        } if target == Protocol::OpenAICompatible => {
            tools_supported
                && crate::codec::openai::compatible::encoder::tool_result_content_is_supported(
                    content,
                    *content_kind,
                )
        }
        ContentBlock::ToolResult {
            content,
            content_kind,
            ..
        } if target == Protocol::GoogleGemini => {
            tools_supported
                && crate::codec::google::gemini::encoder::tool_result_content_is_supported(
                    content,
                    *content_kind,
                )
        }
        ContentBlock::ToolUse { input, .. } => {
            tools_supported
                && (!matches!(
                    target,
                    Protocol::GoogleGemini
                        | Protocol::AnthropicMessages
                        | Protocol::BedrockConverse
                ) || input.is_object())
        }
        ContentBlock::ServerToolUse { .. } | ContentBlock::ServerToolResult { .. }
            if target == Protocol::AnthropicMessages =>
        {
            false
        }
        ContentBlock::ToolResult { .. } | ContentBlock::ServerToolResult { .. } => {
            tools_supported && request_block_representable(target, block)
        }
        _ => {
            request_block_representable(target, block)
                || output
                    && target == Protocol::OpenResponses
                    && matches!(block, ContentBlock::Refusal { .. })
        }
    };
    keep && (!output
        || target != Protocol::OpenResponses
        || matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Refusal { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::Reasoning { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::Compaction { .. }
                | ContentBlock::CompactionTrigger { .. }
        ))
}

fn item_needs_pruning(
    target: Protocol,
    item: &stravia_runtime_contract::protocol::ir::AiItem,
    tools_supported: bool,
    output: bool,
    preserve_blocks: bool,
) -> bool {
    use stravia_runtime_contract::protocol::ir::MessageContent;
    let passthrough = target == Protocol::OpenResponses
        && (item.is_compaction()
            || item.is_compaction_trigger()
            || item.unknown_ref().is_some()
            || item.function_call_output_ref().is_some());
    if item.tool_call_id.is_some() && !tools_supported {
        return true;
    }
    if !passthrough
        && item.tool_calls.as_ref().is_some_and(|calls| {
            calls.is_empty()
                || calls
                    .iter()
                    .any(|call| !tool_call_representable(target, &call.arguments, tools_supported))
        })
    {
        return true;
    }
    matches!(&item.content, MessageContent::Blocks(blocks)
    if blocks.is_empty() && item.tool_calls.is_none()
        || !passthrough && blocks.iter().any(|block| {
            !item_block_representable(target, block, tools_supported, output, preserve_blocks)
                || output && matches!(block,
                    stravia_runtime_contract::protocol::ir::ContentBlock::Document {
                        source: stravia_runtime_contract::protocol::ir::DocumentSource::PlainText { .. }
                            | stravia_runtime_contract::protocol::ir::DocumentSource::Blocks { .. },
                        ..
                    })
        }))
}

fn thinking_control_representable(
    protocol: Protocol,
    control: &stravia_runtime_contract::thinking::TargetThinkingControl,
) -> bool {
    use stravia_runtime_contract::thinking::TargetThinkingControl;
    match protocol {
        Protocol::OpenAICompatible => matches!(control, TargetThinkingControl::Effort { .. }),
        Protocol::OpenResponses => matches!(
            control,
            TargetThinkingControl::Effort { .. } | TargetThinkingControl::Disabled
        ),
        Protocol::AnthropicMessages | Protocol::GoogleGemini => {
            !matches!(control, TargetThinkingControl::Hidden)
        }
        Protocol::BedrockConverse
        | Protocol::CohereChat
        | Protocol::WatsonxTextChat
        | Protocol::GatewayLanguageModel => false,
    }
}

fn request_block_representable(
    target: Protocol,
    block: &stravia_runtime_contract::protocol::ir::ContentBlock,
) -> bool {
    use stravia_runtime_contract::protocol::ir::{ContentBlock, MediaSource};

    // 思考块由 thinking replay 按来源剥离受保护载荷，再由出口 codec 决定原生承载或
    // 降级为正文；无法承载的受保护载荷按契约忽略，不属于意外丢失。
    if matches!(block, ContentBlock::RedactedThinking { .. }) {
        return target == Protocol::AnthropicMessages;
    }
    if matches!(
        block,
        ContentBlock::Thinking { .. } | ContentBlock::Reasoning { .. }
    ) {
        return true;
    }
    match target {
        Protocol::AnthropicMessages => match block {
            ContentBlock::Text { .. }
            | ContentBlock::Document { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => true,
            ContentBlock::Image { source, .. } => match source {
                MediaSource::Url(_) | MediaSource::FileId { .. } => true,
                MediaSource::Base64 { media_type, .. } => matches!(
                    media_type.as_str(),
                    "image/jpeg" | "image/png" | "image/gif" | "image/webp"
                ),
            },
            ContentBlock::File { source, media_type } => match source {
                MediaSource::FileId { .. } => true,
                MediaSource::Url(_) => media_type
                    .as_deref()
                    .is_none_or(|mime| mime == "application/pdf"),
                MediaSource::Base64 {
                    media_type: source_type,
                    ..
                } => {
                    matches!(source_type.as_str(), "application/pdf" | "text/plain")
                        && media_type.as_ref().is_none_or(|mime| mime == source_type)
                }
            },
            _ => false,
        },
        Protocol::GoogleGemini => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Audio { .. }
                | ContentBlock::Document { .. }
                | ContentBlock::File { .. }
                | ContentBlock::Video { .. }
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
                | ContentBlock::Document { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
        ),
        Protocol::OpenResponses => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::File { .. }
                | ContentBlock::Document { .. }
                | ContentBlock::Video {
                    source: stravia_runtime_contract::protocol::ir::MediaSource::Url { .. }
                        | stravia_runtime_contract::protocol::ir::MediaSource::Base64 { .. },
                    ..
                }
                | ContentBlock::Refusal { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
        ),
        Protocol::BedrockConverse => {
            matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image {
                        source: stravia_runtime_contract::protocol::ir::MediaSource::Base64 { .. },
                        ..
                    }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. }
            ) || matches!(
                block,
                ContentBlock::Document {
                    source: stravia_runtime_contract::protocol::ir::DocumentSource::PlainText { .. },
                    ..
                }
            ) || matches!(block, ContentBlock::Document {
            source: stravia_runtime_contract::protocol::ir::DocumentSource::Base64Pdf { data }, ..
        } if !data.is_empty())
                || matches!(block, ContentBlock::File {
            source: stravia_runtime_contract::protocol::ir::MediaSource::Base64 { media_type: source_type, data },
            media_type,
        } if !data.is_empty() && matches!(media_type.as_deref().unwrap_or(source_type),
            "application/pdf" | "text/csv" | "application/csv" | "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.ms-excel" | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "text/html" | "text/plain" | "text/markdown" | "text/x-markdown"))
        }
        Protocol::CohereChat => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
        ),
        Protocol::WatsonxTextChat => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
        ),
        Protocol::GatewayLanguageModel => matches!(
            block,
            ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::File { .. }
                | ContentBlock::Video { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
        ),
    }
}

fn open_responses_output_item_representable(
    item: &stravia_runtime_contract::protocol::ir::AiItem,
) -> bool {
    if item.is_compaction() || item.is_compaction_trigger() {
        return true;
    }
    if let Some(raw) = item.unknown_ref() {
        return raw
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(crate::codec::open_responses::is_registered_extension_item);
    }
    if item.function_call_ref().is_some() {
        return true;
    }
    if let Some((_, content)) = item.function_call_output_ref() {
        return crate::codec::open_responses::encoder::tool_output_representable(content);
    }
    match &item.content {
        stravia_runtime_contract::protocol::ir::MessageContent::Text(_) => {
            item.role == stravia_runtime_contract::protocol::ir::Role::Assistant
        }
        stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) => item.role
            == stravia_runtime_contract::protocol::ir::Role::Assistant
            && (item
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
                || blocks.iter().any(|block| {
                    matches!(
                        block,
                        stravia_runtime_contract::protocol::ir::ContentBlock::Text { .. }
                            | stravia_runtime_contract::protocol::ir::ContentBlock::Refusal { .. }
                            | stravia_runtime_contract::protocol::ir::ContentBlock::ToolUse { .. }
                            | stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning { .. }
                            | stravia_runtime_contract::protocol::ir::ContentBlock::Thinking { .. }
                    )
                })),
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

fn prune_response(pair: ProtocolPair, response: &AiResponse) -> std::borrow::Cow<'_, AiResponse> {
    use std::borrow::Cow;
    let target = pair.ingress.protocol;
    let native = pair.ingress == pair.egress;
    let changed = response.items.iter().any(|item| {
        item_needs_pruning(target, item, true, true, native)
            || !native && !response_item_representable(target, item)
    }) || !native
        && (!response.vendor.passthrough_safe.is_empty()
            || response
                .vendor
                .egress
                .contains_key("__openai_native_choices")
            || response
                .vendor
                .egress
                .contains_key("__openai_native_choice_index")
            || response
                .vendor
                .egress
                .contains_key("__google_native_candidate_index")
            || response
                .vendor
                .egress
                .contains_key("__google_native_candidates"));
    if !changed {
        return Cow::Borrowed(response);
    }
    let mut response = response.clone();
    prune_items(target, &mut response.items, true, true, native);
    if !native {
        response.vendor.passthrough_safe.clear();
        response.vendor.egress.remove("__openai_native_choices");
        response
            .vendor
            .egress
            .remove("__openai_native_choice_index");
        response
            .vendor
            .egress
            .remove("__google_native_candidate_index");
        response.vendor.egress.remove("__google_native_candidates");
        response
            .items
            .retain(|item| response_item_representable(target, item));
    }
    Cow::Owned(response)
}

fn response_item_representable(
    target: Protocol,
    item: &stravia_runtime_contract::protocol::ir::AiItem,
) -> bool {
    if target == Protocol::OpenResponses {
        open_responses_output_item_representable(item)
    } else {
        !item.is_compaction() && !item.is_compaction_trigger() && item.unknown_ref().is_none()
    }
}

fn prune_delta(
    pair: ProtocolPair,
    delta: &AiStreamDelta,
) -> Option<std::borrow::Cow<'_, AiStreamDelta>> {
    use std::borrow::Cow::{Borrowed, Owned};
    let target = pair.ingress.protocol;
    match delta {
        AiStreamDelta::ItemDone { index, item } => {
            if !response_item_representable(target, item) {
                return None;
            }
            if !item_needs_pruning(target, item, true, true, false) {
                return Some(Borrowed(delta));
            }
            let mut item = item.clone();
            let mut dropped_calls = std::collections::HashSet::new();
            prune_item(target, &mut item, true, true, false, &mut dropped_calls);
            if !retain_pruned_item(target, &mut item, true, &dropped_calls)
                || !response_item_representable(target, &item)
            {
                return None;
            }
            Some(Owned(AiStreamDelta::ItemDone {
                index: *index,
                item,
            }))
        }
        AiStreamDelta::ThinkingSignature(_)
            if !matches!(
                target,
                Protocol::AnthropicMessages
                    | Protocol::GoogleGemini
                    | Protocol::OpenAICompatible
                    | Protocol::OpenResponses
            ) =>
        {
            None
        }
        AiStreamDelta::RefusalDelta(_) | AiStreamDelta::RefusalDeltaWithIndex { .. }
            if target != Protocol::OpenResponses =>
        {
            None
        }
        AiStreamDelta::TextDeltaWithMetadata { text, .. }
            if !matches!(target, Protocol::OpenResponses | Protocol::OpenAICompatible) =>
        {
            Some(Owned(AiStreamDelta::TextDelta(text.clone())))
        }
        AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
            if target != Protocol::OpenResponses =>
        {
            Some(Owned(AiStreamDelta::ThinkingDelta(text.clone())))
        }
        AiStreamDelta::Unknown { raw }
            if target == Protocol::OpenResponses
                && serde_json::from_str::<Value>(raw)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("type")
                            .and_then(Value::as_str)
                            .map(crate::codec::open_responses::is_registered_extension_item)
                    })
                    .unwrap_or(false) =>
        {
            Some(Borrowed(delta))
        }
        AiStreamDelta::Unknown { raw }
            if pair.egress.protocol == Protocol::GoogleGemini
                && canonicalized_google_stream_metadata(raw) =>
        {
            Some(Borrowed(delta))
        }
        AiStreamDelta::Unknown { .. } => None,
        _ => Some(Borrowed(delta)),
    }
}

#[cfg(test)]
mod tests;
