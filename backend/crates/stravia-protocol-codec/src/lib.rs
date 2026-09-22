//! Standard wire codecs for Stravia.
//!
//! This crate owns the four public protocol families, their adapter registry,
//! and canonical request/response/stream transforms. Proprietary upstream wire
//! codecs live in their owning Wasm vendor crates and use the public transform
//! extension points without being linked into host builds.
//!
//! # Three-layer identity
//!
//! Canonical form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: closed `Protocol` enum (`openai-compatible` / `open-responses` / `anthropic-messages` / `google-gemini`).
//! - `name`: wire-format endpoint name (`chat-completions`, `responses`, `messages`, `generate-content`).
//! - `version`: dated or vendor schema version (`v1`, `2026-04-24`, `2023-06-01`, `v1beta`).
//!
//! See `stravia_runtime_contract::protocol::ids`, [`registry`], and the
//! crate-private protocol conversion tests for the model.
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

pub mod accumulator;
pub mod codec;

#[cfg(test)]
mod conversion;
pub mod registry;
#[cfg(test)]
mod registry_tests;
pub mod transform;

// ── SSE helper ──

/// One Server-Sent Events frame: optional `event:` name plus `data:` payload.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn new(event: Option<&str>, data: impl Into<String>) -> Self {
        Self {
            event: event.map(|e| e.to_string()),
            data: data.into(),
        }
    }

    pub fn to_sse_string(&self) -> String {
        let mut s = String::new();
        if let Some(event) = &self.event {
            s.push_str(&format!("event: {event}\n"));
        }
        s.push_str(&format!("data: {}\n\n", self.data));
        s
    }
}
