//! IBM watsonx.ai Text Chat wire adapter.
//!
//! watsonx preserves the OpenAI-compatible message and response shapes but
//! names the model field `model_id` and routes streaming requests separately.
//! Authentication and `project_id` injection remain vendor concerns.

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
use crate::protocol::ids::{EndpointCapabilities, ProtocolEndpoint, WATSONX_TEXT_CHAT_V1};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::protocol::registry::EndpointRegistration;
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};

pub struct WatsonxTextChatV1;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    reasoning: true,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[],
    ..EndpointCapabilities::CHAT_STANDARD
};

impl ProtocolAdapter for WatsonxTextChatV1 {
    fn id(&self) -> ProtocolEndpoint {
        WATSONX_TEXT_CHAT_V1
    }
    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, mut body: Value) -> anyhow::Result<AiRequest> {
        let object = body
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("watsonx request body must be an object"))?;
        if let Some(model) = object.remove("model_id") {
            object.insert("model".into(), model);
        }
        let mut request = OpenAIChatCompletionsV1.decode_request(body)?;
        request.meta.source_protocol = Some(WATSONX_TEXT_CHAT_V1);
        Ok(request)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        let (mut body, headers) = OpenAIChatCompletionsV1.encode_request(request)?;
        let object = body
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("OpenAI encoder returned a non-object"))?;
        if let Some(model) = object.remove("model") {
            object.insert("model_id".into(), model);
        }
        Ok((body, headers))
    }

    fn request_path(&self, _model: &str, stream: bool) -> String {
        if stream {
            "/ml/v1/text/chat_stream".into()
        } else {
            "/ml/v1/text/chat".into()
        }
    }

    fn decode_response(&self, mut body: Value) -> anyhow::Result<AiResponse> {
        if body.get("model").is_none()
            && let Some(model) = body.get("model_id").cloned()
            && let Some(object) = body.as_object_mut()
        {
            object.insert("model".into(), model);
        }
        OpenAIChatCompletionsV1.decode_response(body)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        let mut body = OpenAIChatCompletionsV1.encode_response(response);
        if let Some(object) = body.as_object_mut()
            && let Some(model) = object.remove("model")
        {
            object.insert("model_id".into(), model);
        }
        body
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        OpenAIChatCompletionsV1.stream_decoder()
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: WATSONX_TEXT_CHAT_V1,
            operation: "ingress stream encoding",
        })
    }
}

inventory::submit! { EndpointRegistration { make: || Box::new(WatsonxTextChatV1) } }

#[cfg(test)]
mod tests;
