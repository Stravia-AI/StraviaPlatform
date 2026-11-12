//! Devin Connect-RPC egress codec — `ApiServerService/GetChatMessage`.
//!
//! Egress-only, stream-only: the upstream is a server-streaming Connect-RPC
//! endpoint that exchanges protobuf frames (`application/connect+proto`),
//! not JSON. Request encoding is byte-oriented and embeds the session
//! credential inside `ClientMetadata`. The vendor owns the binary encoder
//! and stream decoder; the plugin projects its private wire capabilities before I/O.

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
#[cfg(test)]
pub use request::DevinModelCost;
pub use request::GET_CHAT_MESSAGE_PATH;
pub use request::ModelAssignment;
pub use request::decode_assign_model_response;
pub use request::decode_cli_model_configs;
pub use request::devin_upstream_provider_name;
pub use request::encode_assign_model_request_with_platform;
pub use request::encode_client_metadata_request_with_platform;
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

use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::thinking::TargetThinkingControl;

/// 私有协议能力只在插件中投影；正文编码器逐项省略不支持的内容。
pub(crate) fn prepare_request(request: &mut AiRequest) {
    // 私有 wire 没有这些控制；正文与受保护思考由本插件的编码器投影。
    request.generation.presence_penalty = None;
    request.generation.frequency_penalty = None;
    request.response_format = None;
    // Effort is consumed by the provider's model selector, not sent as a wire control.
    if request
        .reasoning
        .target_control
        .as_ref()
        .is_some_and(|control| !matches!(control, TargetThinkingControl::Effort { .. }))
    {
        request.reasoning.target_control = None;
    }
    // The encoder allowlists native fields from canonical parameters; foreign
    // controls remain source history, never an arbitrary protobuf passthrough.
}
