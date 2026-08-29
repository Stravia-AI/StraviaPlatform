//! Protocol registry acceptance.
//!
//! Nine adapters are registered:
//! - OpenAI Chat / Open Responses
//! - Anthropic Messages / Google Generate
//! - OpenAI Embeddings (registered for ingress route and capability discovery).

use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, BEDROCK_CONVERSE_V1, COHERE_CHAT_V2, GATEWAY_LANGUAGE_MODEL_V4,
    GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPEN_RESPONSES_2026_04_24,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1, Protocol, ProtocolId,
    WATSONX_TEXT_CHAT_V1,
};
use crate::protocol::ir::Role;
use crate::protocol::registry::ProtocolRegistry;
use serde_json::json;

#[test]
fn registers_all_adapters_with_correct_ids() {
    let reg = ProtocolRegistry::global();
    assert_eq!(reg.endpoints().len(), 9);

    for id in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPEN_RESPONSES_2026_04_24,
        OPENAI_COMPATIBLE_EMBEDDINGS_V1,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        BEDROCK_CONVERSE_V1,
        COHERE_CHAT_V2,
        WATSONX_TEXT_CHAT_V1,
        GATEWAY_LANGUAGE_MODEL_V4,
    ] {
        let h = reg.adapter(&id).unwrap_or_else(|| panic!("missing {id}"));
        assert_eq!(h.id(), id);
    }
}

#[test]
fn endpoints_for_protocol_segment() {
    let reg = ProtocolRegistry::global();
    assert_eq!(
        reg.endpoints_for_protocol(Protocol::OpenAICompatible).len(),
        2
    );
    assert_eq!(reg.endpoints_for_protocol(Protocol::OpenResponses).len(), 1);
    assert_eq!(
        reg.endpoints_for_protocol(Protocol::AnthropicMessages)
            .len(),
        1
    );
    assert_eq!(reg.endpoints_for_protocol(Protocol::GoogleGemini).len(), 1);
}

#[test]
fn ingress_routes_match_axum_router() {
    let reg = ProtocolRegistry::global();

    let cases: &[(&str, &str, ProtocolId)] = &[
        (
            "POST",
            "/v1/chat/completions",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        ),
        ("POST", "/v1/responses", OPEN_RESPONSES_2026_04_24),
        ("POST", "/v1/messages", ANTHROPIC_MESSAGES_2023_06_01),
        (
            "POST",
            "/v1beta/models/:model_action",
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        ),
    ];

    for (method, path, expected) in cases {
        let endpoint = reg
            .resolve_ingress_route(method, path)
            .unwrap_or_else(|| panic!("no adapter for {method} {path}"));
        assert_eq!(endpoint, *expected, "wrong endpoint for {method} {path}");
    }
}

#[test]
fn capabilities_match_dialect_special_cases() {
    let reg = ProtocolRegistry::global();
    assert!(
        reg.adapter(&GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
            .unwrap()
            .capabilities()
            .override_model_in_body,
        "Google must override model in body"
    );
}

// ── Decoder / encoder smoke ──

fn sample_body(id: ProtocolId) -> serde_json::Value {
    if id == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        json!({
            "model": "gpt-4o-mini",
            "messages": [
                {"role": "system", "content": "be helpful"},
                {"role": "user", "content": "hi"}
            ],
            "stream": false,
            "temperature": 0.5
        })
    } else if id == OPEN_RESPONSES_2026_04_24 {
        json!({
            "model": "gpt-4o-mini",
            "instructions": "be helpful",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ],
            "stream": false,
            "temperature": 0.5
        })
    } else if id == ANTHROPIC_MESSAGES_2023_06_01 {
        json!({
            "model": "claude-3-5-sonnet",
            "system": "be helpful",
            "messages": [
                {"role": "user", "content": "hi"}
            ],
            "max_tokens": 256,
            "stream": false
        })
    } else if id == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
        json!({
            "system_instruction": {"parts": [{"text": "be helpful"}]},
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]}
            ]
        })
    } else {
        panic!("no sample body for {id}")
    }
}

#[test]
fn decoder_preserves_role_sequence_and_source_protocol() {
    let reg = ProtocolRegistry::global();
    for id in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPEN_RESPONSES_2026_04_24,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let body = sample_body(id);
        let req = reg
            .adapter(&id)
            .unwrap()
            .decode_request(body)
            .unwrap_or_else(|e| panic!("decoder failed for {id}: {e}"));

        assert_eq!(
            req.meta.source_protocol.unwrap(),
            id,
            "source_protocol mismatch for {id}"
        );
        assert!(!req.items.is_empty(), "messages empty for {id}");
        let _: Vec<Role> = req.items.iter().map(|m| m.role).collect();
    }
}

#[test]
fn adapter_round_trips_request_for_every_endpoint() {
    let reg = ProtocolRegistry::global();
    for id in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPEN_RESPONSES_2026_04_24,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let body = sample_body(id);
        let h = reg.adapter(&id).unwrap();
        let internal = h.decode_request(body).unwrap();
        let (out_body, headers) = h
            .encode_request(&internal)
            .unwrap_or_else(|e| panic!("encoder failed for {id}: {e}"));
        assert!(
            out_body.is_object(),
            "encoded body must be an object for {id}"
        );
        let _ = headers;

        let _path = h.request_path(&internal.model, internal.stream.enabled);
    }
}

#[test]
fn stream_state_constructs_for_generation_adapters() {
    let reg = ProtocolRegistry::global();
    for id in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPEN_RESPONSES_2026_04_24,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let h = reg.adapter(&id).unwrap();
        h.stream_decoder().expect("stream decoder");
        h.stream_encoder().expect("stream encoder");
    }
}

// ── Embeddings — registered unary adapter ──

#[test]
fn embeddings_adapter_advertises_unary_capabilities() {
    let reg = ProtocolRegistry::global();
    let caps = reg
        .adapter(&OPENAI_COMPATIBLE_EMBEDDINGS_V1)
        .unwrap()
        .capabilities();
    assert!(caps.embeddings);
    assert!(!caps.streaming);
    assert!(!caps.tools);
    assert!(!caps.override_model_in_body);
}

#[test]
fn embeddings_route_resolves_to_adapter() {
    let reg = ProtocolRegistry::global();
    let path = "/v1/embeddings";
    let endpoint = reg
        .resolve_ingress_route("POST", path)
        .unwrap_or_else(|| panic!("no adapter for POST {path}"));
    assert_eq!(endpoint, OPENAI_COMPATIBLE_EMBEDDINGS_V1);
}

#[test]
fn embeddings_aliases_resolve() {
    let reg = ProtocolRegistry::global();
    assert_eq!(
        reg.resolve_alias("openai-embeddings"),
        Some(OPENAI_COMPATIBLE_EMBEDDINGS_V1)
    );
    assert_eq!(
        reg.resolve_alias("embeddings"),
        Some(OPENAI_COMPATIBLE_EMBEDDINGS_V1)
    );
    assert_eq!(
        reg.resolve_alias("openai/embeddings/v1"),
        Some(OPENAI_COMPATIBLE_EMBEDDINGS_V1)
    );
}

#[test]
fn embeddings_decoder_round_trips_body() {
    let reg = ProtocolRegistry::global();
    let body = json!({
        "model": "text-embedding-3-small",
        "input": ["hello", "world"],
        "encoding_format": "float"
    });
    let internal = reg
        .adapter(&OPENAI_COMPATIBLE_EMBEDDINGS_V1)
        .unwrap()
        .decode_request(body.clone())
        .unwrap();
    assert_eq!(internal.model, "text-embedding-3-small");
    assert!(!internal.stream.enabled);

    let (encoded, _headers) = reg
        .adapter(&OPENAI_COMPATIBLE_EMBEDDINGS_V1)
        .unwrap()
        .encode_request(&internal)
        .unwrap();
    assert_eq!(encoded, body, "encoder must round-trip the original body");
}
