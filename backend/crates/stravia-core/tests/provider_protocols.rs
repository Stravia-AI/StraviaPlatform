//! Acceptance tests for `ProviderProtocols`.
//!
//! Covers three guarantees:
//!
//! 1. Alias-aware provider protocol parsing.
//! 2. Single-provider `resolve_egress` — same protocol suite stays native,
//!    different protocol suites fall back to the provider default.
//! 3. The protocol registry contains one adapter for every canonical endpoint.

use stravia_core::db::models::Provider;
use stravia_core::protocol::ProviderProtocols;
use stravia_core::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
};
use stravia_core::protocol::registry::ProtocolRegistry;

fn provider_with_protocol(protocol: &str, base_url: &str) -> Provider {
    Provider {
        id: "p".to_string(),
        name: "p".to_string(),
        vendor: None,
        protocol: protocol.to_string(),
        base_url: base_url.to_string(),
        preset_key: None,
        channel: None,
        models_source: None,
        static_models: None,
        api_key: String::new(),
        adapter_credentials: "{}".to_string(),
        auth_mode: "apikey".to_string(),
        use_proxy: false,
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

#[test]
fn parses_legacy_protocol_keys() {
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);

    assert!(pp.supports(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
    assert!(!pp.supports(ANTHROPIC_MESSAGES_2023_06_01));
    assert!(!pp.supports(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA));
    assert!(!pp.supports(OPEN_RESPONSES_2026_04_24));
    assert_eq!(pp.default, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert_eq!(pp.base_url, "https://a.example/v1");
}

#[test]
fn parses_canonical_protocol_id() {
    let provider = provider_with_protocol("openai/chat/v1", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);

    assert!(pp.supports(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
    assert_eq!(pp.default, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
}

#[test]
fn parses_short_name_aliases() {
    let provider = provider_with_protocol("openai-chat", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);

    assert!(pp.supports(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
    assert_eq!(pp.default, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
}

#[test]
fn resolve_egress_exact_match_skips_conversion() {
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);
    let r = pp.resolve_egress(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

    assert_eq!(r.protocol, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert_eq!(r.base_url, "https://a.example/v1");
    assert!(!r.needs_conversion);
}

#[test]
fn resolve_egress_responses_falls_back_to_provider_default() {
    // Open Responses (`open-responses`) and OpenAI Compatible (`openai-compatible`) are
    // separate protocols; there is no same-protocol Tier-2 fallback between them.
    // An Open Responses client falls through to Tier 3 (provider default).
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);
    let r = pp.resolve_egress(OPEN_RESPONSES_2026_04_24);

    // No exact match, no same-protocol match (OpenResponses ≠ OpenAICompatible).
    // Tier 3: provider default = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.
    assert_eq!(r.protocol, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert_eq!(r.base_url, "https://a.example/v1");
    assert!(r.needs_conversion);
}

#[test]
fn resolve_egress_falls_back_to_global_default_when_family_missing() {
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);
    // Anthropic ingress, no Anthropic endpoint → fall back to default.
    let r = pp.resolve_egress(ANTHROPIC_MESSAGES_2023_06_01);

    assert_eq!(r.protocol, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert!(r.needs_conversion);
}

#[test]
fn protocol_adapter_resolves_for_every_canonical_id() {
    let reg = ProtocolRegistry::global();

    for id in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPEN_RESPONSES_2026_04_24,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        assert!(reg.contains(&id), "no protocol adapter registered for {id}");
    }
}
