//! OpenAI Chat Completions API (`POST /v1/chat/completions`).
//!
//! `ProtocolAdapter` registration joins the endpoint's decoder, encoder, and
//! stream codecs behind the Protocol Conversion seam.

use crate::registry::EndpointRegistration;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;

use crate::transform::{ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder};
use http::header::HeaderMap;
use serde_json::Value;
use std::sync::Arc;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;

pub struct OpenAIChatCompletionsV1;

impl OpenAIChatCompletionsV1 {
    /// 仅从指定的 message/delta 字符串属性读取上游思考；缺失与 null 保持区别。
    /// 请求编码和客户端响应编码仍使用标准 Chat 契约。
    pub fn with_response_reasoning_field(field: impl Into<Arc<str>>) -> impl ProtocolAdapter {
        ResponseReasoningFieldAdapter {
            field: field.into(),
        }
    }
}

struct ResponseReasoningFieldAdapter {
    field: Arc<str>,
}

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[("POST", "/v1/chat/completions")],
    ..EndpointCapabilities::CHAT_STANDARD
};

impl ProtocolAdapter for OpenAIChatCompletionsV1 {
    fn id(&self) -> ProtocolEndpoint {
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        super::decoder::OpenAIDecoder.decode_request(body)
    }

    fn encode_request(&self, request: AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        super::encoder::OpenAIEncoder.encode_request(request)
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        super::encoder::OpenAIEncoder.egress_path(model, stream)
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        super::stream::OpenAIResponseParser.parse_response(body)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        super::stream::OpenAIResponseFormatter.format_response(response)
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::OpenAi(
            super::stream::OpenAIStreamParser::new(),
        ))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Ok(WireStreamEncoder::OpenAi(
            super::stream::OpenAIStreamFormatter::new(),
        ))
    }
}

impl ProtocolAdapter for ResponseReasoningFieldAdapter {
    fn id(&self) -> ProtocolEndpoint {
        OpenAIChatCompletionsV1.id()
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        OpenAIChatCompletionsV1.capabilities()
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        OpenAIChatCompletionsV1.decode_request(body)
    }

    fn encode_request(&self, request: AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        OpenAIChatCompletionsV1.encode_request(request)
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        OpenAIChatCompletionsV1.request_path(model, stream)
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        super::stream::OpenAIResponseParser
            .parse_response_with_reasoning_field(body, Some(&self.field))
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        OpenAIChatCompletionsV1.encode_response(response)
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::OpenAi(
            super::stream::OpenAIStreamParser::with_reasoning_field(Arc::clone(&self.field)),
        ))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        OpenAIChatCompletionsV1.stream_encoder()
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(OpenAIChatCompletionsV1) }
}
