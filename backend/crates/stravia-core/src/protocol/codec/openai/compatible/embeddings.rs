//! OpenAI Embeddings API (`POST /v1/embeddings`).
//!
//! Requests and responses pass through typed canonical IR so HookRuntime can
//! inspect and replace embedding inputs and vectors. Unknown wire fields remain
//! isolated in the vendor extension bag according to the endpoint policy.

use std::collections::HashMap;

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::ids::{
    EndpointCapabilities, OPENAI_COMPATIBLE_EMBEDDINGS_V1, ProtocolEndpoint,
};
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, EmbeddingData, EmbeddingInput, EmbeddingOutput,
    EmbeddingRequest, GenerationConfig, StreamConfig,
};
use crate::protocol::registry::EndpointRegistration;

use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};

/// OpenAI-spec field names for the embeddings endpoint.
const KNOWN_EMBEDDINGS_FIELDS: &[&str] =
    &["model", "input", "dimensions", "encoding_format", "user"];

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: false,
    tools: false,
    reasoning: false,
    embeddings: true,
    override_model_in_body: false,
    ingress_routes: &[("POST", "/v1/embeddings")],
    multimodal: false,
    structured_output: false,
    function_calling: false,
    parallel_tool_calls: false,
    extended_reasoning: false,
    deterministic_seed: false,
    stream: crate::protocol::ids::StreamCaps::DEFAULT,
    unknown_field_policy: crate::protocol::ids::VendorFieldPolicy::Drop,
};

pub struct OpenAIEmbeddingsV1;

impl ProtocolAdapter for OpenAIEmbeddingsV1 {
    fn id(&self) -> ProtocolEndpoint {
        OPENAI_COMPATIBLE_EMBEDDINGS_V1
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        EmbeddingsDecoder.decode_request(body)
    }

    fn encode_request(&self, request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        EmbeddingsEncoder.encode_request(request)
    }

    fn request_path(&self, model: &str, stream: bool) -> String {
        EmbeddingsEncoder.egress_path(model, stream)
    }

    fn decode_response(&self, body: Value) -> anyhow::Result<AiResponse> {
        EmbeddingsResponseParser.parse_response(body)
    }

    fn encode_response(&self, response: &AiResponse) -> Value {
        EmbeddingsResponseFormatter.format_response(response)
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: OPENAI_COMPATIBLE_EMBEDDINGS_V1,
            operation: "stream decode",
        })
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: OPENAI_COMPATIBLE_EMBEDDINGS_V1,
            operation: "stream encode",
        })
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(OpenAIEmbeddingsV1) }
}

// ── Decoder ───────────────────────────────────────────────────────────────────

struct EmbeddingsDecoder;

impl EmbeddingsDecoder {
    pub(crate) fn decode_request(&self, body: Value) -> anyhow::Result<AiRequest> {
        let obj = body
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("embeddings request must be a JSON object"))?;

        let model = obj
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow::anyhow!("model is required for embeddings"))?;

        let input = obj
            .get("input")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("input is required for embeddings"))
            .and_then(|value| {
                serde_json::from_value::<EmbeddingInput>(value)
                    .map_err(|error| anyhow::anyhow!("invalid embeddings input: {error}"))
            })?;
        let dimensions = obj
            .get("dimensions")
            .map(|value| serde_json::from_value::<u32>(value.clone()))
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid embeddings dimensions: {error}"))?;
        let encoding_format = obj
            .get("encoding_format")
            .map(|value| serde_json::from_value::<String>(value.clone()))
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid embeddings encoding_format: {error}"))?;
        let user = obj
            .get("user")
            .map(|value| serde_json::from_value::<String>(value.clone()))
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid embeddings user: {error}"))?;
        let mut ingress: HashMap<String, Value> = HashMap::new();

        // Collect unknown fields into __vendor_ingress.
        let mut vendor_ingress = serde_json::Map::new();
        for (k, v) in obj {
            if !KNOWN_EMBEDDINGS_FIELDS.contains(&k.as_str()) {
                vendor_ingress.insert(k.clone(), v.clone());
            }
        }
        if !vendor_ingress.is_empty() {
            ingress.insert("__vendor_ingress".into(), Value::Object(vendor_ingress));
        }

        let mut ai_req = AiRequest::new(model, Vec::<AiItem>::new());
        ai_req.generation = GenerationConfig::default();
        ai_req.embedding = Some(EmbeddingRequest {
            input,
            dimensions,
            encoding_format,
            user,
        });
        ai_req.stream = StreamConfig {
            enabled: false,
            include_usage: false,
        };
        ai_req.meta.source_protocol = Some(OPENAI_COMPATIBLE_EMBEDDINGS_V1);
        ai_req.meta.vendor.ingress = ingress;

        Ok(ai_req)
    }
}

// ── Encoder ───────────────────────────────────────────────────────────────────

struct EmbeddingsEncoder;

impl EmbeddingsEncoder {
    pub(crate) fn encode_request(&self, req: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        let mut obj = serde_json::Map::new();
        obj.insert("model".into(), Value::String(req.model.clone()));
        let embedding = req
            .embedding
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("canonical embedding request is missing"))?;
        obj.insert("input".into(), serde_json::to_value(&embedding.input)?);
        if let Some(dimensions) = embedding.dimensions {
            obj.insert("dimensions".into(), serde_json::json!(dimensions));
        }
        if let Some(encoding_format) = &embedding.encoding_format {
            obj.insert(
                "encoding_format".into(),
                Value::String(encoding_format.clone()),
            );
        }
        if let Some(user) = &embedding.user {
            obj.insert("user".into(), Value::String(user.clone()));
        }

        Ok((Value::Object(obj), HeaderMap::new()))
    }

    pub(crate) fn egress_path(&self, _model: &str, _stream: bool) -> String {
        "/v1/embeddings".to_string()
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingsWireResponse {
    #[serde(default)]
    object: Option<String>,
    data: Vec<EmbeddingData>,
    model: String,
    #[serde(default)]
    usage: EmbeddingsWireUsage,
    #[serde(flatten)]
    extensions: serde_json::Map<String, Value>,
}

#[derive(Default, serde::Deserialize)]
struct EmbeddingsWireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

struct EmbeddingsResponseParser;

impl EmbeddingsResponseParser {
    pub(crate) fn parse_response(&self, response: Value) -> anyhow::Result<AiResponse> {
        let wire: EmbeddingsWireResponse = serde_json::from_value(response)?;
        let mut canonical = AiResponse::new(
            format!("embedding-{}", uuid::Uuid::new_v4().simple()),
            wire.model,
        );
        canonical.usage.prompt_tokens = wire.usage.prompt_tokens;
        canonical.usage.total_tokens = wire.usage.total_tokens;
        canonical.embedding_output = Some(EmbeddingOutput {
            object: wire.object,
            data: wire.data,
            extensions: wire.extensions,
        });
        Ok(canonical)
    }
}

struct EmbeddingsResponseFormatter;

impl EmbeddingsResponseFormatter {
    pub(crate) fn format_response(&self, response: &AiResponse) -> Value {
        let Some(output) = &response.embedding_output else {
            return serde_json::json!({
                "error": {"message": "canonical embedding output is missing"}
            });
        };
        let mut object = output.extensions.clone();
        if let Some(kind) = &output.object {
            object.insert("object".into(), Value::String(kind.clone()));
        }
        object.insert(
            "data".into(),
            serde_json::to_value(&output.data).unwrap_or(Value::Array(Vec::new())),
        );
        object.insert("model".into(), Value::String(response.model.clone()));
        object.insert(
            "usage".into(),
            serde_json::json!({
                "prompt_tokens": response.usage.prompt_tokens,
                "total_tokens": response.usage.total_tokens,
            }),
        );
        Value::Object(object)
    }
}

#[cfg(test)]
mod tests;
