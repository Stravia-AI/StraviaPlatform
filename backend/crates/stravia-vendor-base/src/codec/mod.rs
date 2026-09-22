pub(crate) mod bedrock;
pub(crate) mod cohere;
pub(crate) mod gateway;
pub(crate) mod watsonx;

use stravia_protocol_codec::transform::ProtocolAdapter;

pub(crate) use bedrock::BedrockConverseV1;
pub(crate) use cohere::CohereChatV2;
pub(crate) use gateway::GatewayLanguageModelV4;
pub(crate) use watsonx::WatsonxTextChatV1;

pub(crate) fn adapter(protocol: &str) -> Option<&'static dyn ProtocolAdapter> {
    match protocol.trim().to_ascii_lowercase().as_str() {
        "bedrock" | "bedrock-converse" | "bedrock-converse/converse/v1" => Some(&BedrockConverseV1),
        "cohere" | "cohere-chat" | "cohere-chat/chat/v2" => Some(&CohereChatV2),
        "gateway" | "gateway-language-model" | "gateway-language-model/language-model/v4" => {
            Some(&GatewayLanguageModelV4)
        }
        "watsonx" | "watsonx-text-chat" | "watsonx-text-chat/chat/v1" => Some(&WatsonxTextChatV1),
        _ => None,
    }
}
