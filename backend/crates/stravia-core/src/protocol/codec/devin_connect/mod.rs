//! Devin Connect-RPC egress codec — `ApiServerService/GetChatMessage`.
//!
//! Egress-only, stream-only: the upstream is a server-streaming Connect-RPC
//! endpoint that exchanges protobuf frames (`application/connect+proto`),
//! not JSON. Request encoding is byte-oriented and embeds the session
//! credential inside `ClientMetadata`, so it is owned by the vendor's
//! `build_request`; this adapter supplies the endpoint identity,
//! capabilities, and the binary stream decoder.

pub(crate) mod connect;
pub(crate) mod proto;
pub(crate) mod request;
pub(crate) mod sanitize;
pub(crate) mod stream;

pub(crate) use connect::wrap_request;
pub(crate) use request::ASSIGN_MODEL_PATH;
pub(crate) use request::DevinModelConfig;
pub(crate) use request::GET_CHAT_MESSAGE_PATH;
pub(crate) use request::ModelAssignment;
pub(crate) use request::decode_assign_model_response;
pub(crate) use request::decode_cli_model_configs;
pub(crate) use request::devin_upstream_provider_name;
pub(crate) use request::encode_assign_model_request;
pub(crate) use request::encode_client_metadata_request;
pub(crate) use request::encode_get_chat_message_request;
pub(crate) use request::session_shape;
pub(crate) use stream::DevinConnectStreamParser;

/// 签名类型与输出身份必须一起回放；使用现有 opaque signature carrier，
/// 避免在跨协议历史中把 Devin 私有字段伪装成通用推理参数。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct ThinkingReplay {
    pub signature: String,
    pub signature_type: String,
    pub output_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub redacted_text: String,
}

impl ThinkingReplay {
    const PREFIX: &str = "devin-thinking-v1:";

    fn encode(&self) -> String {
        format!(
            "{}{}",
            Self::PREFIX,
            serde_json::to_string(self).expect("string-only thinking state")
        )
    }

    fn decode(value: &str) -> anyhow::Result<Self> {
        match value.strip_prefix(Self::PREFIX) {
            Some(json) => Ok(serde_json::from_str(json)?),
            None => Ok(Self {
                signature: value.to_owned(),
                ..Self::default()
            }),
        }
    }
}

const CUSTOM_TOOL_META: &str = "__devin_custom_tool";

use anyhow::bail;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::protocol::registry::EndpointRegistration;
use crate::protocol::transform::{
    ProtocolAdapter, TransformError, WireStreamDecoder, WireStreamEncoder,
};
use stravia_runtime_contract::protocol::ids::DEVIN_CONNECT_GET_CHAT_MESSAGE_V1;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;
use stravia_runtime_contract::protocol::ids::StreamCaps;
use stravia_runtime_contract::protocol::ids::VendorFieldPolicy;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;

pub struct DevinConnectGetChatMessageV1;

const CAPS: EndpointCapabilities = EndpointCapabilities {
    streaming: true,
    tools: true,
    // Thinking deltas stream natively (#9); there is no request-side
    // thinking control on the wire.
    reasoning: true,
    embeddings: false,
    override_model_in_body: false,
    ingress_routes: &[],
    multimodal: true,
    structured_output: false,
    function_calling: true,
    // #11/#12 (disable_parallel_tool_calls / tool_choice) are unconfirmed
    // tags the reference client deliberately omits.
    parallel_tool_calls: false,
    extended_reasoning: false,
    deterministic_seed: false,
    stream: StreamCaps {
        server_sent_events: false,
        usage_in_stream: true,
        requires_stream_flag: false,
    },
    unknown_field_policy: VendorFieldPolicy::Drop,
};

impl ProtocolAdapter for DevinConnectGetChatMessageV1 {
    fn id(&self) -> ProtocolEndpoint {
        DEVIN_CONNECT_GET_CHAT_MESSAGE_V1
    }

    fn capabilities(&self) -> &'static EndpointCapabilities {
        &CAPS
    }

    fn decode_request(&self, _body: Value) -> anyhow::Result<AiRequest> {
        bail!("Devin Connect is an egress-only protocol")
    }

    fn encode_request(&self, _request: &AiRequest) -> anyhow::Result<(Value, HeaderMap)> {
        bail!("Devin Connect request encoding is binary and vendor-owned")
    }

    fn request_path(&self, _model: &str, _stream: bool) -> String {
        GET_CHAT_MESSAGE_PATH.to_string()
    }

    fn decode_response(&self, _body: Value) -> anyhow::Result<AiResponse> {
        bail!("Devin Connect is stream-only; unary responses are not produced")
    }

    fn encode_response(&self, _response: &AiResponse) -> Value {
        Value::Null
    }

    fn stream_decoder(&self) -> Result<WireStreamDecoder, TransformError> {
        Ok(WireStreamDecoder::DevinConnect(
            DevinConnectStreamParser::new(),
        ))
    }

    fn stream_encoder(&self) -> Result<WireStreamEncoder, TransformError> {
        Err(TransformError::UnsupportedOperation {
            endpoint: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            operation: "ingress stream encoding",
        })
    }
}

inventory::submit! {
    EndpointRegistration { make: || Box::new(DevinConnectGetChatMessageV1) }
}
