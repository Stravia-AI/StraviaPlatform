//! Open Responses Protocol 2026-04-24 (`POST /v1/responses`).
//!
//! Streaming requirements belong to the resolved Target, not this protocol
//! adapter. Ordinary Open Responses targets support both buffered and streaming calls.

use crate::protocol::ids::{EndpointCapabilities, OPEN_RESPONSES_2026_04_24, ProtocolEndpoint};
use crate::protocol::registry::EndpointRegistration;

use crate::protocol::ir::{AiRequest, AiResponse};
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use reqwest::header::HeaderMap;
use serde_json::Value;

pub struct OpenResponses20260424;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[("POST", "/v1/responses")],
    ..EndpointCapabilities::CHAT_STANDARD
};

impl ProtocolAdapter for OpenResponses20260424 {
    fn id(&self) -> ProtocolEndpoint {
        OPEN_RESPONSES_2026_04_24
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        super::decoder::ResponsesDecoder.decode_request(body)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        super::encoder::ResponsesEncoder.encode_request(request)
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        super::encoder::ResponsesEncoder.egress_path(model, stream)
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        super::parser::ResponsesResponseParser.parse_response(body)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        super::formatter::ResponsesResponseFormatter.format_response(response)
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::Responses(Box::default()))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Ok(WireStreamEncoder::Responses(Box::default()))
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(OpenResponses20260424) }
}
