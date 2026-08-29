//! Three-layer protocol identity: `Protocol` (suite) + `ProtocolEndpoint` (specific API endpoint).
//!
//! Canonical string form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: wire-format protocol suite.
//! - `name`: wire-format endpoint name (`chat-completions` / `responses` / `messages` / `generate-content` / `embeddings`).
//! - `version`: pinned wire-schema version (`v1`, `2026-04-24`, `2023-06-01`, `v1beta`).
//!
//! `ProtocolEndpoint` is `Copy` and stores `&'static str` slices — values must be const.
//! Runtime parsing of arbitrary strings into a `ProtocolEndpoint` is the responsibility of
//! `ProtocolRegistry::resolve_alias`, which returns one of the registered const ids.

use std::fmt;
use std::str::FromStr;

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
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
}
