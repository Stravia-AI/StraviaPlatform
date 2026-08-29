//! Google Generative AI (`POST /v1beta/models/:model:generateContent`).
//!
//! Wire version `v1beta` matches Google's URL versioning.
//!
//! `override_model_in_body` is true: the encoder embeds the actual model name
//! in the request body / URL path rather than a top-level `model` field.

use crate::protocol::ids::{
    EndpointCapabilities, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, ProtocolEndpoint,
};
use crate::protocol::registry::EndpointRegistration;

use crate::protocol::ir::{AiRequest, AiResponse};
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use reqwest::header::HeaderMap;
use serde_json::Value;

pub struct GoogleGenerateContentV1Beta;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    embeddings: false,
    override_model_in_body: true,
    ingress_routes: &[("POST", "/v1beta/models/:model_action")],
    ..EndpointCapabilities::CHAT_STANDARD
};

impl ProtocolAdapter for GoogleGenerateContentV1Beta {
    fn id(&self) -> ProtocolEndpoint {
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        super::decoder::GoogleDecoder.decode_request(body)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        super::encoder::GoogleEncoder.encode_request(request)
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        super::encoder::GoogleEncoder.egress_path(model, stream)
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        super::stream::GoogleResponseParser.parse_response(body)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        super::stream::GoogleResponseFormatter.format_response(response)
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::Google(
            super::stream::GoogleStreamParser::new(),
        ))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Ok(WireStreamEncoder::Google(
            super::stream::GoogleStreamFormatter::new(),
        ))
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(GoogleGenerateContentV1Beta) }
}
