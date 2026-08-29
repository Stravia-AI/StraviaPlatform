//! Distributed protocol adapter registration via the `inventory` crate.
//!
//! Each `protocol/codec/<protocol>/<endpoint>.rs` module emits one
//! `inventory::submit!` block. `ProtocolRegistry::global()` walks the
//! collected registrations once, indexes them by `ProtocolEndpoint` and ingress
//! route, and exposes alias resolution for human-friendly inputs.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, BEDROCK_CONVERSE_V1, COHERE_CHAT_V2, GATEWAY_LANGUAGE_MODEL_V4,
    GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPEN_RESPONSES_2026_04_24,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1, Protocol,
    ProtocolEndpoint, WATSONX_TEXT_CHAT_V1,
};
use crate::protocol::transform::ProtocolAdapter;

/// `inventory::submit!` payload. Each registered adapter ships one of these.
pub(crate) struct EndpointRegistration {
    pub(crate) make: fn() -> Box<dyn ProtocolAdapter>,
}

inventory::collect!(EndpointRegistration);

/// Global registry of protocol adapters, alias table, and ingress route index.
pub struct ProtocolRegistry {
    by_id: HashMap<ProtocolEndpoint, Arc<dyn ProtocolAdapter>>,
    endpoint_aliases: HashMap<&'static str, ProtocolEndpoint>,
    protocol_aliases: HashMap<&'static str, Protocol>,
    routes: Vec<RouteEntry>,
}

struct RouteEntry {
    method: &'static str,
    path: &'static str,
    id: ProtocolEndpoint,
}

impl ProtocolRegistry {
    pub fn global() -> &'static Self {
        static REG: OnceLock<ProtocolRegistry> = OnceLock::new();
        REG.get_or_init(Self::build)
    }

    fn build() -> Self {
        let mut by_id: HashMap<ProtocolEndpoint, Arc<dyn ProtocolAdapter>> = HashMap::new();
        let mut routes: Vec<RouteEntry> = Vec::new();

        for reg in inventory::iter::<EndpointRegistration> {
            let handler: Arc<dyn ProtocolAdapter> = Arc::from((reg.make)());
            let id = handler.id();

            for (method, path) in handler.capabilities().ingress_routes {
                routes.push(RouteEntry { method, path, id });
            }

            if by_id.insert(id, handler).is_some() {
                tracing::warn!(
                    target: "stravia_core::protocol",
                    "duplicate protocol adapter registration for {id}"
                );
            }
        }

        Self {
            by_id,
            endpoint_aliases: default_endpoint_aliases(),
            protocol_aliases: default_protocol_aliases(),
            routes,
        }
    }

    /// Look up an adapter by canonical endpoint id.
    pub(crate) fn adapter(&self, id: &ProtocolEndpoint) -> Option<&Arc<dyn ProtocolAdapter>> {
        self.by_id.get(id)
    }

    /// Return whether an endpoint has a registered adapter.
    pub fn contains(&self, id: &ProtocolEndpoint) -> bool {
        self.by_id.contains_key(id)
    }
    /// Return the static representability capabilities for an endpoint.
    pub fn capabilities(
        &self,
        id: &ProtocolEndpoint,
    ) -> Option<&'static crate::protocol::ids::EndpointCapabilities> {
        self.by_id.get(id).map(|adapter| adapter.capabilities())
    }

    /// Resolve a string into a registered `ProtocolEndpoint`.
    ///
    /// Accepts (in priority order):
    /// 1. New canonical `protocol/name/version` form (e.g. `openai-compat/chat-completions/v1`)
    /// 2. Old canonical `family/dialect/version` form (e.g. `openai/chat/v1`) — via alias table
    /// 3. Short alias from the alias table (e.g. `openai-chat-completions`)
    /// 4. Legacy enum string (e.g. `openai`, `gemini`, `openai_responses`)
    ///
    /// Returns `None` if no registered adapter matches.
    pub fn resolve_alias(&self, raw: &str) -> Option<ProtocolEndpoint> {
        let key = raw.trim();
        if key.is_empty() {
            return None;
        }

        if let Some(id) = self.parse_canonical(key) {
            return Some(id);
        }

        let lower = key.to_ascii_lowercase();
        if let Some(id) = self.endpoint_aliases.get(lower.as_str()) {
            return Some(*id);
        }

        None
    }

    fn parse_canonical(&self, raw: &str) -> Option<ProtocolEndpoint> {
        let parts: Vec<&str> = raw.splitn(3, '/').collect();
        if parts.len() != 3 {
            return None;
        }
        let protocol = parts[0].parse::<Protocol>().ok()?;
        self.by_id
            .keys()
            .find(|id| id.protocol == protocol && id.name == parts[1] && id.version == parts[2])
            .copied()
    }

    /// Resolve a string into a `Protocol` (suite-level, not endpoint-level).
    pub fn parse_protocol(&self, raw: &str) -> Option<Protocol> {
        let key = raw.trim().to_ascii_lowercase();
        if key.is_empty() {
            return None;
        }
        // Try as a registered protocol alias first
        if let Some(p) = self.protocol_aliases.get(key.as_str()) {
            return Some(*p);
        }
        // Try via endpoint alias → extract protocol from it
        if let Some(ep) = self.resolve_alias(raw) {
            return Some(ep.protocol);
        }
        None
    }

    pub(crate) fn protocol_supports_function_calling(&self, raw: &str) -> bool {
        self.parse_protocol(raw).is_some_and(|protocol| {
            self.by_id.iter().any(|(id, adapter)| {
                id.protocol == protocol && adapter.capabilities().function_calling
            })
        })
    }

    /// Registered endpoint identities for one protocol, sorted canonically.
    pub fn endpoints_for_protocol(&self, protocol: Protocol) -> Vec<ProtocolEndpoint> {
        let mut endpoints: Vec<_> = self
            .by_id
            .keys()
            .filter(|id| id.protocol == protocol)
            .copied()
            .collect();
        endpoints.sort();
        endpoints
    }

    /// Returns the `Protocol` for a registered `ProtocolEndpoint`, or `None` if not found.
    pub fn protocol_of(&self, id: &ProtocolEndpoint) -> Option<Protocol> {
        if self.by_id.contains_key(id) {
            Some(id.protocol)
        } else {
            None
        }
    }

    /// All distinct protocols that have at least one registered adapter.
    pub fn list_protocols(&self) -> Vec<Protocol> {
        let protocols: std::collections::BTreeSet<Protocol> =
            self.by_id.keys().map(|id| id.protocol).collect();
        protocols.into_iter().collect()
    }

    /// All registered endpoint identities, sorted canonically.
    pub fn endpoints(&self) -> Vec<ProtocolEndpoint> {
        let mut endpoints: Vec<_> = self.by_id.keys().copied().collect();
        endpoints.sort();
        endpoints
    }

    // ── Normalize helpers (migrated from protocol/normalize.rs) ──────────────

    /// Normalize a single protocol identifier string to its canonical
    /// `protocol/name/version` form.  Unknown strings are returned verbatim.
    pub fn normalize_string(&self, raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        match self.resolve_alias(trimmed) {
            Some(id) => id.to_string(),
            None => {
                tracing::warn!(
                    value = trimmed,
                    "leaving unrecognized protocol identifier unchanged"
                );
                trimmed.to_string()
            }
        }
    }

    /// Resolve an HTTP ingress method/path to its endpoint identity.
    ///
    /// Path matching is exact — axum-style `:param` segments are matched as
    /// literals because axum already extracts params before this is called.
    pub fn resolve_ingress_route(&self, method: &str, path: &str) -> Option<ProtocolEndpoint> {
        self.routes
            .iter()
            .find(|entry| entry.method.eq_ignore_ascii_case(method) && entry.path == path)
            .map(|entry| entry.id)
    }
}

/// Three-tier endpoint alias table.
///
/// Tier 1 — Old canonical strings (backward compatibility for DB / yaml data).
/// Tier 2 — Canonical short names (preferred for new configs).
/// Tier 3 — Legacy brand names (human-friendly shortcuts).
fn default_endpoint_aliases() -> HashMap<&'static str, ProtocolEndpoint> {
    let mut m = HashMap::new();

    // ── Tier 1: Old canonical (backward compat) ───────────────────────────────
    m.insert("openai/chat/v1", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    m.insert("openai/embeddings/v1", OPENAI_COMPATIBLE_EMBEDDINGS_V1);
    m.insert(
        "anthropic/messages/2023-06-01",
        ANTHROPIC_MESSAGES_2023_06_01,
    );
    m.insert(
        "google/generate/v1beta",
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    );

    // ── Tier 2: Canonical short names ─────────────────────────────────────────
    m.insert("openai-chat", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    m.insert(
        "openai-chat-completions",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    );
    m.insert("open-responses", OPEN_RESPONSES_2026_04_24);
    m.insert("openai-embeddings", OPENAI_COMPATIBLE_EMBEDDINGS_V1);
    m.insert("anthropic-messages", ANTHROPIC_MESSAGES_2023_06_01);
    m.insert("google-generate", GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    m.insert(
        "google-generate-content",
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    );
    m.insert("bedrock-converse", BEDROCK_CONVERSE_V1);
    m.insert("cohere-chat", COHERE_CHAT_V2);
    m.insert("watsonx-text-chat", WATSONX_TEXT_CHAT_V1);
    m.insert("gateway-language-model", GATEWAY_LANGUAGE_MODEL_V4);

    // ── Tier 3: Legacy brand / friendly aliases ────────────────────────────────
    m.insert("openai", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    m.insert("embeddings", OPENAI_COMPATIBLE_EMBEDDINGS_V1);
    m.insert("anthropic", ANTHROPIC_MESSAGES_2023_06_01);
    m.insert("claude", ANTHROPIC_MESSAGES_2023_06_01);
    m.insert("gemini", GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    m.insert("bedrock", BEDROCK_CONVERSE_V1);
    m.insert("cohere", COHERE_CHAT_V2);
    m.insert("watsonx", WATSONX_TEXT_CHAT_V1);
    m.insert("gateway", GATEWAY_LANGUAGE_MODEL_V4);

    m
}

/// Protocol-level alias table.
fn default_protocol_aliases() -> HashMap<&'static str, Protocol> {
    let mut m = HashMap::new();

    // Canonical (new)
    m.insert("openai-compatible", Protocol::OpenAICompatible);
    m.insert("open-responses", Protocol::OpenResponses);
    m.insert("anthropic-messages", Protocol::AnthropicMessages);
    m.insert("google-gemini", Protocol::GoogleGemini);
    m.insert("bedrock-converse", Protocol::BedrockConverse);
    m.insert("cohere-chat", Protocol::CohereChat);
    m.insert("watsonx-text-chat", Protocol::WatsonxTextChat);
    m.insert("gateway-language-model", Protocol::GatewayLanguageModel);

    // Short names
    m.insert("openai", Protocol::OpenAICompatible);
    m.insert("anthropic", Protocol::AnthropicMessages);
    m.insert("claude", Protocol::AnthropicMessages);
    m.insert("gemini", Protocol::GoogleGemini);
    m.insert("google", Protocol::GoogleGemini);
    m.insert("bedrock", Protocol::BedrockConverse);
    m.insert("cohere", Protocol::CohereChat);
    m.insert("watsonx", Protocol::WatsonxTextChat);
    m.insert("gateway", Protocol::GatewayLanguageModel);

    // Deprecated aliases (old canonical slugs, backward compat only)
    m.insert("openai-compat", Protocol::OpenAICompatible);
    m.insert("anthropic-msgs", Protocol::AnthropicMessages);
    m.insert("google-genai", Protocol::GoogleGemini);
    m.insert("google-generative-ai", Protocol::GoogleGemini);

    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_all_nine_adapters() {
        let reg = ProtocolRegistry::global();
        assert!(reg.contains(&OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
        assert!(reg.contains(&OPEN_RESPONSES_2026_04_24));
        assert!(reg.contains(&OPENAI_COMPATIBLE_EMBEDDINGS_V1));
        assert!(reg.contains(&ANTHROPIC_MESSAGES_2023_06_01));
        assert!(reg.contains(&GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA));
        assert!(reg.contains(&BEDROCK_CONVERSE_V1));
        assert!(reg.contains(&COHERE_CHAT_V2));
        assert!(reg.contains(&WATSONX_TEXT_CHAT_V1));
        assert!(reg.contains(&GATEWAY_LANGUAGE_MODEL_V4));
        assert_eq!(reg.endpoints().len(), 9);
    }

    #[test]
    fn alias_table_resolves_new_canonical() {
        let reg = ProtocolRegistry::global();
        // New canonical form
        assert_eq!(
            reg.resolve_alias("openai-compatible/chat-completions/v1"),
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        );
        assert_eq!(
            reg.resolve_alias("open-responses/responses/2026-04-24"),
            Some(OPEN_RESPONSES_2026_04_24)
        );
        assert_eq!(
            reg.resolve_alias("anthropic-messages/messages/2023-06-01"),
            Some(ANTHROPIC_MESSAGES_2023_06_01)
        );
        assert_eq!(
            reg.resolve_alias("google-gemini/generate-content/v1beta"),
            Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        );
    }

    #[test]
    fn alias_table_resolves_old_canonical_and_short() {
        let reg = ProtocolRegistry::global();
        // Old canonical (tier 1)
        assert_eq!(
            reg.resolve_alias("openai/chat/v1"),
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        );
        assert_eq!(
            reg.resolve_alias("anthropic/messages/2023-06-01"),
            Some(ANTHROPIC_MESSAGES_2023_06_01)
        );
        assert_eq!(
            reg.resolve_alias("google/generate/v1beta"),
            Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        );

        assert_eq!(reg.resolve_alias("openai-responses/responses/v1"), None);
        assert_eq!(reg.resolve_alias("openai-resps/responses/v1"), None);
        assert_eq!(reg.resolve_alias("openai/responses/v1"), None);
        assert_eq!(
            reg.resolve_alias("anthropic-msgs/messages/2023-06-01"),
            Some(ANTHROPIC_MESSAGES_2023_06_01)
        );
        assert_eq!(
            reg.resolve_alias("google-genai/generate-content/v1beta"),
            Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        );

        // Canonical short (tier 2)
        assert_eq!(
            reg.resolve_alias("openai-chat"),
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        );
        assert_eq!(
            reg.resolve_alias("openai-chat-completions"),
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        );
        assert_eq!(
            reg.resolve_alias("open-responses"),
            Some(OPEN_RESPONSES_2026_04_24)
        );
        assert_eq!(reg.resolve_alias("openai-responses"), None);
        assert_eq!(reg.resolve_alias("openai_responses"), None);
        assert_eq!(reg.resolve_alias("responses"), None);
        assert_eq!(
            reg.resolve_alias("anthropic-messages"),
            Some(ANTHROPIC_MESSAGES_2023_06_01)
        );
        assert_eq!(
            reg.resolve_alias("google-generate"),
            Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        );
        assert_eq!(
            reg.resolve_alias("google-generate-content"),
            Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        );

        // Legacy brand (tier 3)
        assert_eq!(
            reg.resolve_alias("openai"),
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        );
        assert_eq!(
            reg.resolve_alias("anthropic"),
            Some(ANTHROPIC_MESSAGES_2023_06_01)
        );
        assert_eq!(
            reg.resolve_alias("claude"),
            Some(ANTHROPIC_MESSAGES_2023_06_01)
        );
        assert_eq!(
            reg.resolve_alias("gemini"),
            Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        );
    }

    #[test]
    fn alias_resolution_is_case_insensitive_and_trims() {
        let reg = ProtocolRegistry::global();
        assert_eq!(
            reg.resolve_alias("  OpenAI  "),
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        );
        assert_eq!(
            reg.resolve_alias("GEMINI"),
            Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        );
    }

    #[test]
    fn unknown_returns_none() {
        let reg = ProtocolRegistry::global();
        assert_eq!(reg.resolve_alias(""), None);
        assert_eq!(reg.resolve_alias("nope"), None);
        assert_eq!(reg.resolve_alias("openai/nope/v1"), None);
    }

    #[test]
    fn endpoints_for_protocol_group_correctly() {
        let reg = ProtocolRegistry::global();
        let openai_compat = reg.endpoints_for_protocol(Protocol::OpenAICompatible);
        assert_eq!(openai_compat.len(), 2);
        assert!(openai_compat.contains(&OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
        assert!(openai_compat.contains(&OPENAI_COMPATIBLE_EMBEDDINGS_V1));

        assert_eq!(reg.endpoints_for_protocol(Protocol::OpenResponses).len(), 1);
        assert_eq!(
            reg.endpoints_for_protocol(Protocol::AnthropicMessages)
                .len(),
            1
        );
        assert_eq!(reg.endpoints_for_protocol(Protocol::GoogleGemini).len(), 1);
    }

    #[test]
    fn parse_protocol_resolves_aliases() {
        let reg = ProtocolRegistry::global();
        assert_eq!(
            reg.parse_protocol("openai-compatible"),
            Some(Protocol::OpenAICompatible)
        );
        assert_eq!(
            reg.parse_protocol("openai"),
            Some(Protocol::OpenAICompatible)
        );
        assert_eq!(
            reg.parse_protocol("claude"),
            Some(Protocol::AnthropicMessages)
        );
        assert_eq!(reg.parse_protocol("gemini"), Some(Protocol::GoogleGemini));
        assert_eq!(
            reg.parse_protocol("google-gemini"),
            Some(Protocol::GoogleGemini)
        );
        // Deprecated aliases still resolve
        assert_eq!(
            reg.parse_protocol("openai-compat"),
            Some(Protocol::OpenAICompatible)
        );
        assert_eq!(
            reg.parse_protocol("anthropic-msgs"),
            Some(Protocol::AnthropicMessages)
        );
        assert_eq!(
            reg.parse_protocol("google-genai"),
            Some(Protocol::GoogleGemini)
        );
    }

    #[test]
    fn list_protocols_returns_all_eight() {
        let reg = ProtocolRegistry::global();
        let protocols = reg.list_protocols();
        assert_eq!(protocols.len(), 8);
        assert!(protocols.contains(&Protocol::OpenAICompatible));
        assert!(protocols.contains(&Protocol::OpenResponses));
        assert!(protocols.contains(&Protocol::AnthropicMessages));
        assert!(protocols.contains(&Protocol::GoogleGemini));
        assert!(protocols.contains(&Protocol::BedrockConverse));
        assert!(protocols.contains(&Protocol::CohereChat));
        assert!(protocols.contains(&Protocol::WatsonxTextChat));
        assert!(protocols.contains(&Protocol::GatewayLanguageModel));
    }

    #[test]
    fn ingress_route_matches_method_and_path() {
        let reg = ProtocolRegistry::global();
        assert_eq!(
            reg.resolve_ingress_route("POST", "/v1/chat/completions"),
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        );
        assert_eq!(
            reg.resolve_ingress_route("POST", "/v1/responses"),
            Some(OPEN_RESPONSES_2026_04_24)
        );
        assert_eq!(
            reg.resolve_ingress_route("POST", "/v1/messages"),
            Some(ANTHROPIC_MESSAGES_2023_06_01)
        );
        assert_eq!(
            reg.resolve_ingress_route("POST", "/v1/embeddings"),
            Some(OPENAI_COMPATIBLE_EMBEDDINGS_V1)
        );
        assert!(
            reg.resolve_ingress_route("GET", "/v1/chat/completions")
                .is_none()
        );
    }

    #[test]
    fn capabilities_match_endpoint_special_cases() {
        let reg = ProtocolRegistry::global();
        let chat = reg.adapter(&OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1).unwrap();
        let google = reg.adapter(&GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA).unwrap();

        assert!(google.capabilities().override_model_in_body);
        assert!(!chat.capabilities().override_model_in_body);
    }
}
