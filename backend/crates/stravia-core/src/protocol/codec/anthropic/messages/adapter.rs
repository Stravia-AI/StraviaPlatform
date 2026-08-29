//! Anthropic Messages API (`POST /v1/messages`).
//!
//! Wire version is the schema date `2023-06-01` (the `anthropic-version` header
//! the API requires), not the URL prefix `v1`.

use crate::protocol::ids::{ANTHROPIC_MESSAGES_2023_06_01, EndpointCapabilities, ProtocolEndpoint};
use crate::protocol::registry::EndpointRegistration;

use crate::protocol::ir::{AiRequest, AiResponse};
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use reqwest::header::HeaderMap;
use serde_json::Value;

pub struct AnthropicMessages2023;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[("POST", "/v1/messages")],
    extended_reasoning: true,
    ..EndpointCapabilities::CHAT_STANDARD
};

impl ProtocolAdapter for AnthropicMessages2023 {
    fn id(&self) -> ProtocolEndpoint {
        ANTHROPIC_MESSAGES_2023_06_01
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        super::decoder::AnthropicDecoder.decode_request(body)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        super::encoder::AnthropicEncoder.encode_request(request)
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        super::encoder::AnthropicEncoder.egress_path(model, stream)
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        super::stream::AnthropicResponseParser.parse_response(body)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        super::stream::AnthropicResponseFormatter.format_response(response)
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::Anthropic(
            super::stream::AnthropicStreamParser::new(),
        ))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Ok(WireStreamEncoder::Anthropic(
            super::stream::AnthropicStreamFormatter::new(),
        ))
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(AnthropicMessages2023) }
}
