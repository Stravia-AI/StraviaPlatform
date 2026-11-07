//! Open Responses Protocol 2026-04-24 (`POST /v1/responses`).
//!
//! Streaming requirements belong to the resolved Target, not this protocol
//! adapter. Ordinary Open Responses targets support both buffered and streaming calls.

use crate::registry::EndpointRegistration;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;

use crate::transform::{
    EncodedRequest, ProtocolAdapter, ProtocolTransform, TransformDirection, TransformError,
    WireStreamDecoder, WireStreamEncoder,
};
use http::header::HeaderMap;
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;

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

impl OpenResponses20260424 {
    /// Encode with the normal ingress preparation and map every original
    /// canonical item gap to its native `input` offset. There are `items.len()+1`
    /// offsets, including the final gap. Removed items collapse to a gap;
    /// expanded assistant/tool items advance by their actual encoded size.
    /// The request is consumed once; ordinary encoding allocates no gap map.
    pub fn encode_request_with_input_gaps(
        &self,
        mut request: AiRequest,
    ) -> Result<(EncodedRequest, Vec<usize>), TransformError> {
        let ingress =
            ProtocolTransform::inferred_ingress(&request).unwrap_or(OPEN_RESPONSES_2026_04_24);
        ProtocolTransform::global().bind(ingress, OPEN_RESPONSES_2026_04_24)?;
        let mut gaps = Vec::with_capacity(request.items.len() + 1);
        let removed = ProtocolTransform::prune_request_for_with_removals(
            OPEN_RESPONSES_2026_04_24,
            &CAPS,
            &mut request,
        );
        let mut removed = removed.into_iter().peekable();
        let path = self.request_path(&request.model, request.stream.enabled);
        let (body, headers) = super::encoder::ResponsesEncoder
            .encode_request_observing_gaps(request, |offset| {
                while removed.peek() == Some(&gaps.len()) {
                    gaps.push(offset);
                    removed.next();
                }
                gaps.push(offset);
            })
            .map_err(|source| TransformError::Wire {
                endpoint: OPEN_RESPONSES_2026_04_24,
                direction: TransformDirection::ClientRequest.as_str(),
                source,
            })?;
        Ok((
            EncodedRequest {
                body,
                headers,
                path,
            },
            gaps,
        ))
    }
}

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

    fn encode_request(&self, request: AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
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
