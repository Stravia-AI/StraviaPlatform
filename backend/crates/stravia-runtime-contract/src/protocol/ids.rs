//! Three-layer protocol identity: `Protocol` (suite) + `ProtocolEndpoint` (specific API endpoint).
//!
//! Canonical string form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: wire-format protocol suite.
//! - `name`: wire-format endpoint name (`chat-completions` / `responses` / `messages` / `generate-content` / `embeddings`).
//! - `version`: pinned wire-schema version (`v1`, `2026-04-24`, `2023-06-01`, `v1beta`).
//!
//! `ProtocolEndpoint` is `Copy` and stores `&'static str` slices — values must be const.
//! Host codec lookup still belongs to `ProtocolRegistry`; guest-selected identities use
//! [`ProtocolIdentity`] so an unregistered private protocol remains opaque and exact.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Top-level protocol suite (wire-format family).
///
/// A `Protocol` groups one or more `ProtocolEndpoint`s that share the same
/// request/response wire format. It is orthogonal to `Vendor` — multiple vendors
/// (e.g. OpenAI, Moonshot, DeepSeek) may implement the same `Protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    /// OpenAI Chat Completions-compatible protocol (`/v1/chat/completions`, `/v1/embeddings`).
    OpenAICompatible,
    /// Vendor-neutral Open Responses Protocol (`/v1/responses`).
    OpenResponses,
    /// Anthropic Messages protocol (`/v1/messages`).
    AnthropicMessages,
    /// Google Generative AI (Gemini) protocol.
    GoogleGemini,
    /// Amazon Bedrock Converse API.
    BedrockConverse,
    /// Cohere Chat API v2.
    CohereChat,
    /// IBM watsonx.ai Text Chat API.
    WatsonxTextChat,
    /// Vercel AI Gateway AI SDK v4 language-model wire.
    GatewayLanguageModel,
    /// Command Code `/alpha/generate` envelope and NDJSON stream.
    CommandCode,
    /// Devin Connect-RPC `ApiServerService` protobuf wire.
    DevinConnect,
}

impl Protocol {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAICompatible => "openai-compatible",
            Self::OpenResponses => "open-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGemini => "google-gemini",
            Self::BedrockConverse => "bedrock-converse",
            Self::CohereChat => "cohere-chat",
            Self::WatsonxTextChat => "watsonx-text-chat",
            Self::GatewayLanguageModel => "gateway-language-model",
            Self::CommandCode => "command-code",
            Self::DevinConnect => "devin-connect",
        }
    }

    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::OpenAICompatible => "OpenAI Compatible",
            Self::OpenResponses => "Open Responses",
            Self::AnthropicMessages => "Anthropic Messages",
            Self::GoogleGemini => "Google Gemini",
            Self::BedrockConverse => "Amazon Bedrock Converse",
            Self::CohereChat => "Cohere Chat",
            Self::WatsonxTextChat => "watsonx.ai Text Chat",
            Self::GatewayLanguageModel => "Vercel AI Gateway Language Model",
            Self::CommandCode => "Command Code Generate",
            Self::DevinConnect => "Devin Connect ApiServer",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Protocol {
    /// Resolve a known suite, endpoint, or legacy identifier without consulting
    /// a host codec registry. Unknown guest-selected identities remain opaque.
    pub fn from_identifier(value: &str) -> Option<Self> {
        value
            .trim()
            .parse()
            .ok()
            .or_else(|| ProtocolEndpoint::from_identifier(value).map(|endpoint| endpoint.protocol))
    }

    /// Whether the shared protocol semantics can carry a resolved Thinking
    /// control. Private wire encoding remains in the owning guest.
    pub fn represents_target_thinking_control(
        self,
        control: &crate::thinking::TargetThinkingControl,
    ) -> bool {
        use crate::thinking::TargetThinkingControl;

        if matches!(control, TargetThinkingControl::Hidden) {
            return true;
        }
        match self {
            Self::OpenResponses => matches!(
                control,
                TargetThinkingControl::Effort { .. } | TargetThinkingControl::Disabled
            ),
            Self::AnthropicMessages | Self::GoogleGemini => true,
            Self::OpenAICompatible => {
                matches!(control, TargetThinkingControl::Effort { .. })
            }
            Self::DevinConnect => matches!(
                control,
                TargetThinkingControl::Effort { .. }
                    | TargetThinkingControl::Enabled
                    | TargetThinkingControl::Disabled
            ),
            Self::BedrockConverse
            | Self::CohereChat
            | Self::WatsonxTextChat
            | Self::GatewayLanguageModel
            | Self::CommandCode => false,
        }
    }
}

impl FromStr for Protocol {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "openai-compatible" | "openai-compat" | "openai" => Ok(Self::OpenAICompatible),
            "open-responses" => Ok(Self::OpenResponses),
            "anthropic-messages" | "anthropic-msgs" | "anthropic" | "claude" => {
                Ok(Self::AnthropicMessages)
            }
            "google-gemini" | "google-genai" | "google-generative-ai" | "gemini" | "google" => {
                Ok(Self::GoogleGemini)
            }
            "bedrock-converse" | "bedrock" => Ok(Self::BedrockConverse),
            "cohere-chat" | "cohere" => Ok(Self::CohereChat),
            "watsonx-text-chat" | "watsonx" => Ok(Self::WatsonxTextChat),
            "gateway-language-model" | "gateway" => Ok(Self::GatewayLanguageModel),
            "command-code" | "commandcode" | "command-code-generate" => Ok(Self::CommandCode),
            "devin-connect" | "devin" | "windsurf-connect" => Ok(Self::DevinConnect),
            other => anyhow::bail!("unknown protocol: {other}"),
        }
    }
}

/// Specific API endpoint within a `Protocol`.
///
/// Canonical display: `{protocol}/{name}/{version}` (e.g. `openai-compatible/chat-completions/v1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProtocolEndpoint {
    pub protocol: Protocol,
    /// Endpoint name (kebab-case, matches the final path segment of the ingress route).
    pub name: &'static str,
    /// Wire-format version string as the vendor labels it.
    pub version: &'static str,
}

impl ProtocolEndpoint {
    pub const fn new(protocol: Protocol, name: &'static str, version: &'static str) -> Self {
        Self {
            protocol,
            name,
            version,
        }
    }
}

impl fmt::Display for ProtocolEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.protocol, self.name, self.version)
    }
}

/// The protocol identity selected by a vendor guest.
///
/// Unlike [`ProtocolEndpoint`], this value is intentionally open: third-party
/// guests may return identities unknown to the host. The original string is
/// retained for private-state affinity and protected-thinking replay.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolIdentity(String);

impl ProtocolIdentity {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Known suite semantics, when the identity is part of the shared contract.
    /// Unknown plugin identities deliberately return `None` without changing
    /// their stored identity.
    pub fn protocol(&self) -> Option<Protocol> {
        Protocol::from_identifier(&self.0)
    }

    pub fn matches_endpoint(&self, endpoint: ProtocolEndpoint) -> bool {
        self.protocol() == Some(endpoint.protocol)
    }
}

impl fmt::Display for ProtocolIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for ProtocolIdentity {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for ProtocolIdentity {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<ProtocolEndpoint> for ProtocolIdentity {
    fn from(value: ProtocolEndpoint) -> Self {
        Self::new(value.to_string())
    }
}

// ── Canonical const `ProtocolEndpoint` values ────────────────────────────────

pub const OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::OpenAICompatible, "chat-completions", "v1");

pub const OPENAI_COMPATIBLE_EMBEDDINGS_V1: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::OpenAICompatible, "embeddings", "v1");

pub const OPEN_RESPONSES_2026_04_24: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::OpenResponses, "responses", "2026-04-24");

pub const ANTHROPIC_MESSAGES_2023_06_01: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::AnthropicMessages, "messages", "2023-06-01");

pub const GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::GoogleGemini, "generate-content", "v1beta");

pub const BEDROCK_CONVERSE_V1: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::BedrockConverse, "converse", "v1");

pub const COHERE_CHAT_V2: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::CohereChat, "chat", "v2");

pub const WATSONX_TEXT_CHAT_V1: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::WatsonxTextChat, "chat", "v1");

pub const GATEWAY_LANGUAGE_MODEL_V4: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::GatewayLanguageModel, "language-model", "v4");

pub const COMMAND_CODE_GENERATE_V1: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::CommandCode, "generate", "v1");

pub const DEVIN_CONNECT_GET_CHAT_MESSAGE_V1: ProtocolEndpoint =
    ProtocolEndpoint::new(Protocol::DevinConnect, "get-chat-message", "v1");

impl ProtocolEndpoint {
    /// Resolve endpoint identities that are part of the shared runtime contract.
    /// This is identity parsing only; owning guest codecs remain responsible for
    /// all private wire encoding and decoding.
    pub fn from_identifier(value: &str) -> Option<Self> {
        Some(match value.trim() {
            "openai-compatible/chat-completions/v1"
            | "openai/chat/v1"
            | "openai-chat"
            | "openai-chat-completions" => OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "openai-compatible/embeddings/v1"
            | "openai/embeddings/v1"
            | "openai-embeddings"
            | "embeddings" => OPENAI_COMPATIBLE_EMBEDDINGS_V1,
            "open-responses/responses/2026-04-24" | "open-responses" => OPEN_RESPONSES_2026_04_24,
            "anthropic-messages/messages/2023-06-01" | "anthropic-messages" => {
                ANTHROPIC_MESSAGES_2023_06_01
            }
            "google-gemini/generate-content/v1beta"
            | "google-generate"
            | "google-generate-content" => GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            "bedrock-converse/converse/v1" => BEDROCK_CONVERSE_V1,
            "cohere-chat/chat/v2" => COHERE_CHAT_V2,
            "watsonx-text-chat/chat/v1" => WATSONX_TEXT_CHAT_V1,
            "gateway-language-model/language-model/v4" => GATEWAY_LANGUAGE_MODEL_V4,
            "command-code/generate/v1" => COMMAND_CODE_GENERATE_V1,
            "devin-connect/get-chat-message/v1" => DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            _ => return None,
        })
    }
}

impl<'de> Deserialize<'de> for ProtocolEndpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "openai-compatible/chat-completions/v1" => Ok(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            "openai-compatible/embeddings/v1" => Ok(OPENAI_COMPATIBLE_EMBEDDINGS_V1),
            "open-responses/responses/2026-04-24" => Ok(OPEN_RESPONSES_2026_04_24),
            "anthropic-messages/messages/2023-06-01" => Ok(ANTHROPIC_MESSAGES_2023_06_01),
            "google-gemini/generate-content/v1beta" => Ok(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA),
            "bedrock-converse/converse/v1" => Ok(BEDROCK_CONVERSE_V1),
            "cohere-chat/chat/v2" => Ok(COHERE_CHAT_V2),
            "watsonx-text-chat/chat/v1" => Ok(WATSONX_TEXT_CHAT_V1),
            "gateway-language-model/language-model/v4" => Ok(GATEWAY_LANGUAGE_MODEL_V4),
            "command-code/generate/v1" => Ok(COMMAND_CODE_GENERATE_V1),
            "devin-connect/get-chat-message/v1" => Ok(DEVIN_CONNECT_GET_CHAT_MESSAGE_V1),
            _ => Err(serde::de::Error::custom(format_args!(
                "unknown canonical protocol endpoint `{value}`"
            ))),
        }
    }
}

// ── Backward-compat type alias ────────────────────────────────────────────────

/// Backward-compat alias — prefer `ProtocolEndpoint`.
pub type ProtocolId = ProtocolEndpoint;

// ── Static capability types ───────────────────────────────────────────────────

/// Vendor field policy: what happens when the codec encounters a field
/// that the provider may or may not support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorFieldPolicy {
    /// The provider is known to support this field.
    Supported,
    /// The provider does not support this field; it MUST be dropped silently.
    Drop,
    /// Unknown — check at runtime via vendor extension.
    Unknown,
}

/// Stream capabilities for this endpoint.
#[derive(Debug, Clone, Copy)]
pub struct StreamCaps {
    /// Endpoint can produce SSE streaming responses.
    pub server_sent_events: bool,
    /// The `usage` object is present in the final stream chunk.
    pub usage_in_stream: bool,
    /// Provider requires the body to contain `"stream": true` to stream.
    pub requires_stream_flag: bool,
}

impl StreamCaps {
    pub const DEFAULT: Self = Self {
        server_sent_events: true,
        usage_in_stream: false,
        requires_stream_flag: true,
    };
}

/// Extended static capabilities of a protocol adapter.
///
/// Describes what a specific `ProtocolEndpoint` can represent.
#[derive(Debug, Clone, Copy)]
pub struct EndpointCapabilities {
    // ── Original fields (PR-01 through PR-06) ────────────────────────────────
    pub streaming: bool,
    pub tools: bool,
    pub reasoning: bool,
    pub embeddings: bool,
    /// The encoder writes the actual model name into the request body rather
    /// than the URL path. Currently only true for Google Generate.
    pub override_model_in_body: bool,
    /// Ingress routes this adapter claims, as `(method, path)` tuples.
    /// Used by `ProtocolRegistry` for declarative routing.
    pub ingress_routes: &'static [(&'static str, &'static str)],

    // ── PR-07 additions ───────────────────────────────────────────────────────
    /// Whether multimodal (vision) input is accepted.
    pub multimodal: bool,
    /// Whether the provider accepts structured output / JSON-mode requests.
    pub structured_output: bool,
    /// Whether the provider supports named function tools.
    pub function_calling: bool,
    /// Whether the provider supports parallel tool calls.
    pub parallel_tool_calls: bool,
    /// Whether the provider exposes extended reasoning / thinking.
    pub extended_reasoning: bool,
    /// Whether the provider honours the `seed` parameter for determinism.
    pub deterministic_seed: bool,
    /// Stream capabilities for this endpoint.
    pub stream: StreamCaps,
    /// Default policy for unrecognised vendor fields in the egress body.
    pub unknown_field_policy: VendorFieldPolicy,
}

impl EndpointCapabilities {
    pub const EMPTY: Self = Self {
        streaming: false,
        tools: false,
        reasoning: false,
        embeddings: false,
        override_model_in_body: false,
        ingress_routes: &[],
        multimodal: false,
        structured_output: false,
        function_calling: false,
        parallel_tool_calls: false,
        extended_reasoning: false,
        deterministic_seed: false,
        stream: StreamCaps::DEFAULT,
        unknown_field_policy: VendorFieldPolicy::Drop,
    };

    /// The standard set of capabilities for a typical chat-completions endpoint.
    pub const CHAT_STANDARD: Self = Self {
        streaming: true,
        tools: true,
        reasoning: false,
        embeddings: false,
        override_model_in_body: false,
        ingress_routes: &[],
        multimodal: true,
        structured_output: true,
        function_calling: true,
        parallel_tool_calls: true,
        extended_reasoning: false,
        deterministic_seed: true,
        stream: StreamCaps {
            server_sent_events: true,
            usage_in_stream: true,
            requires_stream_flag: true,
        },
        unknown_field_policy: VendorFieldPolicy::Drop,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_canonical_form() {
        assert_eq!(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            "openai-compatible/chat-completions/v1"
        );
        assert_eq!(
            OPEN_RESPONSES_2026_04_24.to_string(),
            "open-responses/responses/2026-04-24"
        );
        assert_eq!(
            ANTHROPIC_MESSAGES_2023_06_01.to_string(),
            "anthropic-messages/messages/2023-06-01"
        );
        assert_eq!(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string(),
            "google-gemini/generate-content/v1beta"
        );
        assert_eq!(
            OPENAI_COMPATIBLE_EMBEDDINGS_V1.to_string(),
            "openai-compatible/embeddings/v1"
        );
    }

    #[test]
    fn rejects_obsolete_responses_protocol_names() {
        assert!("openai-responses".parse::<Protocol>().is_err());
        assert!("openai-resps".parse::<Protocol>().is_err());
        assert!("responses".parse::<Protocol>().is_err());
    }

    #[test]
    fn protocol_round_trip() {
        for p in [
            Protocol::OpenAICompatible,
            Protocol::OpenResponses,
            Protocol::AnthropicMessages,
            Protocol::GoogleGemini,
            Protocol::BedrockConverse,
            Protocol::CohereChat,
            Protocol::WatsonxTextChat,
            Protocol::GatewayLanguageModel,
            Protocol::CommandCode,
            Protocol::DevinConnect,
        ] {
            assert_eq!(p.as_str().parse::<Protocol>().unwrap(), p);
        }
    }

    #[test]
    fn protocol_endpoint_is_copy_and_hashable() {
        use std::collections::HashSet;
        let id = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
        let copied = id;
        let mut set = HashSet::new();
        set.insert(id);
        set.insert(copied);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn selected_protocol_identity_preserves_unknown_plugins() {
        let identity = ProtocolIdentity::new("acme/private-inference-v7");
        assert_eq!(identity.as_str(), "acme/private-inference-v7");
        assert_eq!(identity.protocol(), None);
        assert!(!identity.matches_endpoint(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
        assert_eq!(
            serde_json::to_string(&identity).unwrap(),
            r#""acme/private-inference-v7""#
        );
    }

    #[test]
    fn selected_protocol_identity_exposes_known_semantics_without_a_codec() {
        let private = ProtocolIdentity::new("devin-connect");
        assert_eq!(private.protocol(), Some(Protocol::DevinConnect));
        assert!(Protocol::DevinConnect.represents_target_thinking_control(
            &crate::thinking::TargetThinkingControl::Effort {
                value: "high".into()
            }
        ));
        let standard = ProtocolIdentity::new("openai/chat/v1");
        assert!(standard.matches_endpoint(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
        assert!(standard.matches_endpoint(OPENAI_COMPATIBLE_EMBEDDINGS_V1));
    }
}
