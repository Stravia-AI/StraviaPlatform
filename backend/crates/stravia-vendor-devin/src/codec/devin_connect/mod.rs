//! Devin Connect-RPC egress codec — `ApiServerService/GetChatMessage`.
//!
//! Egress-only, stream-only: the upstream is a server-streaming Connect-RPC
//! endpoint that exchanges protobuf frames (`application/connect+proto`),
//! not JSON. Request encoding is byte-oriented and embeds the session
//! credential inside `ClientMetadata`. The vendor owns the binary encoder
//! and stream decoder; shared representability checks run before upstream I/O.

pub(crate) mod connect;
pub mod proto;
pub mod request;
pub(crate) mod sanitize;
pub(crate) mod stream;
mod tool_description;

pub use connect::wrap_request;
pub use request::ASSIGN_MODEL_PATH;
pub use request::DevinClientPlatform;
pub use request::DevinModelConfig;
pub use request::GET_CHAT_MESSAGE_PATH;
pub use request::ModelAssignment;
pub use request::decode_assign_model_response;
pub use request::decode_cli_model_configs;
pub use request::devin_upstream_provider_name;
pub use request::encode_assign_model_request_with_platform;
pub use request::encode_client_metadata_request_with_platform;
pub use request::encode_get_chat_message_request_with_platform;
pub use request::session_shape;
pub use stream::DevinConnectStreamParser;

/// 使用现有 opaque signature carrier 保留 Devin 响应状态；
/// output_id 仅随历史保留，不回填到原生请求的 prompt。
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

use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::protocol::ids::DEVIN_CONNECT_GET_CHAT_MESSAGE_V1;
use stravia_runtime_contract::protocol::ids::EndpointCapabilities;
use stravia_runtime_contract::protocol::ids::StreamCaps;
use stravia_runtime_contract::protocol::ids::VendorFieldPolicy;
use stravia_runtime_contract::protocol::ir::AiRequest;

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

pub fn validate_request(request: &AiRequest) -> anyhow::Result<()> {
    ProtocolTransform::validate_request_for(DEVIN_CONNECT_GET_CHAT_MESSAGE_V1, &CAPS, request)?;
    request::validate_request_fields(request)
}
