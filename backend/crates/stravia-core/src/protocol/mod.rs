//! Protocol layer.
//!
//! # Three-layer identity
//!
//! Canonical form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: closed `Protocol` enum (`openai-compatible` / `open-responses` / `anthropic-messages` / `google-gemini`).
//! - `name`: wire-format endpoint name (`chat-completions`, `responses`, `messages`, `generate-content`).
//! - `version`: dated or vendor schema version (`v1`, `2026-04-24`, `2023-06-01`, `v1beta`).
//!
//! See [`ids`], [`registry`], and the crate-private Protocol Conversion module
//! for the model.
//!
//! ## Codec layout
//!
//! Each `codec/<vendor>/<protocol>/` directory co-locates one wire adapter.
//!
//! - `codec/openai/compatible/chat_completions.rs` — `OpenAICompatibleChatCompletionsV1`
//! - `codec/openai/compatible/embeddings.rs` — `OpenAICompatibleEmbeddingsV1`
//! - `codec/open_responses/adapter.rs` — `OpenResponses20260424`
//! - `codec/anthropic/messages/adapter.rs` — `AnthropicMessages2023`
//! - `codec/google/gemini/generate_content.rs` — `GoogleGeminiGenerateContentV1Beta`
//!
//! Shared semantic utilities live in `codec/reasoning.rs` and
//! `codec/tool_correlation.rs`.
//!
//! ## Alias table
//!
//! See [`registry::ProtocolRegistry`] for three-tier resolution of endpoint aliases
//! and [`registry::ProtocolRegistry::parse_protocol`] for Protocol-level resolution.

pub(crate) mod codec;

#[cfg(test)]
mod conversion;
pub mod ids;
pub mod ir;
pub mod registry;
#[cfg(test)]
mod registry_tests;
pub(crate) mod transform;

use crate::db::models::Provider;
use crate::protocol::ids::{OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolEndpoint};

// ── SSE helper ──

#[derive(Debug, Clone)]
pub(crate) struct SseEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

impl SseEvent {
    pub(crate) fn new(event: Option<&str>, data: impl Into<String>) -> Self {
        Self {
            event: event.map(|e| e.to_string()),
            data: data.into(),
        }
    }

    pub(crate) fn to_sse_string(&self) -> String {
        let mut s = String::new();
        if let Some(ref event) = self.event {
            s.push_str(&format!("event: {event}\n"));
        }
        s.push_str(&format!("data: {}\n\n", self.data));
        s
    }
}

// ── Provider protocol negotiation ──

/// Declared protocol capabilities of a single provider, built from the DB row.
#[derive(Debug, Clone)]
pub struct ProviderProtocols {
    pub default: ProtocolEndpoint,
    pub base_url: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedEgress {
    pub protocol: ProtocolEndpoint,
    pub base_url: String,
    pub needs_conversion: bool,
}

impl ProviderProtocols {
    /// Best-effort string → [`ProtocolEndpoint`] resolver.
    pub fn parse_protocol_key(s: &str) -> Option<ProtocolEndpoint> {
        let reg = registry::ProtocolRegistry::global();
        reg.resolve_alias(s).or_else(|| {
            let protocol = reg.parse_protocol(s)?;
            reg.endpoints_for_protocol(protocol).first().copied()
        })
    }

    /// Build from a provider DB row.
    pub fn from_provider(provider: &Provider) -> Self {
        let default = Self::parse_protocol_key(provider.protocol.trim())
            .unwrap_or(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

        Self {
            default,
            base_url: provider.base_url.trim().to_string(),
        }
    }

    /// Returns `true` if the provider declares support for `protocol`.
    pub fn supports(&self, protocol: ProtocolEndpoint) -> bool {
        self.default.protocol == protocol.protocol
    }

    /// Deterministic two-tier egress resolution:
    ///
    /// 1. **Same protocol suite** — use the ingress endpoint and provider base URL.
    /// 2. **Provider default** — last resort with conversion.
    pub fn resolve_egress(&self, ingress: ProtocolEndpoint) -> ResolvedEgress {
        if self.supports(ingress) {
            return ResolvedEgress {
                protocol: ingress,
                base_url: self.base_url.clone(),
                needs_conversion: false,
            };
        }

        ResolvedEgress {
            protocol: self.default,
            base_url: self.base_url.clone(),
            needs_conversion: true,
        }
    }
}
