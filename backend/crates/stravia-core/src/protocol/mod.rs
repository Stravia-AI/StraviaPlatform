//! Protocol layer.
//!
//! Wire codecs, the adapter registry, and canonical transforms live in the
//! shared `stravia-protocol-codec` crate (consumed by the host and Wasm vendor
//! plugins). This module keeps only the host-side provider negotiation built
//! on the DB `Provider` row.
//!
//! # Three-layer identity
//!
//! Canonical form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: closed `Protocol` enum (`openai-compatible` / `open-responses` / `anthropic-messages` / `google-gemini`).
//! - `name`: wire-format endpoint name (`chat-completions`, `responses`, `messages`, `generate-content`).
//! - `version`: dated or vendor schema version (`v1`, `2026-04-24`, `2023-06-01`, `v1beta`).
//!
//! See `stravia_runtime_contract::protocol::ids` and
//! `stravia_protocol_codec::registry` for the model.

use crate::db::models::Provider;
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;

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
    /// Build host codec declarations from a provider DB row.
    ///
    /// Guest-owned or third-party protocol identities deliberately return
    /// `None`: they remain opaque to the host instead of being presented as
    /// OpenAI-compatible.
    pub fn from_provider(provider: &Provider) -> Option<Self> {
        let default = ProtocolRegistry::global().resolve_alias(provider.protocol.trim())?;

        Some(Self {
            default,
            base_url: provider.base_url.trim().to_string(),
        })
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
