//! OpenAI Chat Completions API (`POST /v1/chat/completions`).
//!
//! `ProtocolAdapter` registration joins the endpoint's decoder, encoder, and
//! stream codecs behind the Protocol Conversion seam.

use crate::protocol::ids::{
    EndpointCapabilities, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolEndpoint,
};
use crate::protocol::registry::EndpointRegistration;

use crate::protocol::ir::{AiRequest, AiResponse};
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use reqwest::header::HeaderMap;
use serde_json::Value;

pub struct OpenAIChatCompletionsV1;

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

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
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

inventory::submit! {
    EndpointRegistration { make: || Box::new(OpenAIChatCompletionsV1) }
}
