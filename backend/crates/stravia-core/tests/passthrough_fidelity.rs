//! Protocol-pair negotiation contract tests.
//!
//! Same-protocol pairs select `ProtocolMode::Native`; cross-protocol pairs
//! select a transform mode. `Native` still traverses canonical IR so inference
//! hooks observe every request and response.

use std::time::Duration;
use stravia_core::protocol::ProviderProtocols;
use stravia_core::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId,
};
use stravia_core::proxy::context::RequestContext;
use stravia_core::proxy::planner::{ProtocolMode, negotiate};

fn single_decl(proto: ProtocolId, url: &str) -> ProviderProtocols {
    ProviderProtocols {
        default: proto,
        base_url: url.to_string(),
    }
}

fn req_ctx(ingress: ProtocolId) -> RequestContext {
    RequestContext::new(ingress, Duration::from_secs(30))
}

// ── 4 diagonal cells: Native mode ─────────────────────────────────────────────

#[test]
fn diagonal_chat_chat_is_native() {
    let decl = single_decl(
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "https://api.openai.com",
    );
    let mut ctx = req_ctx(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    let plan = negotiate(
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        None,
        Some(&decl),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(plan.mode, ProtocolMode::Native, "chat→chat must be Native");
    assert_eq!(plan.egress, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert!(!plan.needs_conversion);
}

#[test]
fn diagonal_responses_responses_is_native() {
    let decl = single_decl(OPEN_RESPONSES_2026_04_24, "https://api.openai.com");
    let mut ctx = req_ctx(OPEN_RESPONSES_2026_04_24);
    let plan = negotiate(OPEN_RESPONSES_2026_04_24, None, Some(&decl), &mut ctx).unwrap();
    assert_eq!(
        plan.mode,
        ProtocolMode::Native,
        "responses→responses must be Native"
    );
    assert_eq!(plan.egress, OPEN_RESPONSES_2026_04_24);
    assert!(!plan.needs_conversion);
}

#[test]
fn diagonal_messages_messages_is_native() {
    let decl = single_decl(ANTHROPIC_MESSAGES_2023_06_01, "https://api.anthropic.com");
    let mut ctx = req_ctx(ANTHROPIC_MESSAGES_2023_06_01);
    let plan = negotiate(ANTHROPIC_MESSAGES_2023_06_01, None, Some(&decl), &mut ctx).unwrap();
    assert_eq!(
        plan.mode,
        ProtocolMode::Native,
        "messages→messages must be Native"
    );
    assert_eq!(plan.egress, ANTHROPIC_MESSAGES_2023_06_01);
    assert!(!plan.needs_conversion);
}

#[test]
fn diagonal_generate_generate_is_native() {
    let decl = single_decl(
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        "https://generativelanguage.googleapis.com",
    );
    let mut ctx = req_ctx(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    let plan = negotiate(
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        None,
        Some(&decl),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        plan.mode,
        ProtocolMode::Native,
        "generate→generate must be Native"
    );
    assert_eq!(plan.egress, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    assert!(!plan.needs_conversion);
}

// ── 12 off-diagonal cells: Transform mode ────────────────────────────────────

macro_rules! off_diagonal_test {
    ($name:ident, ingress = $ing:expr, egress = $eg:expr) => {
        #[test]
        fn $name() {
            let decl = single_decl($eg, "https://upstream.example.com");
            let mut ctx = req_ctx($ing);
            let plan = negotiate($ing, None, Some(&decl), &mut ctx).unwrap();
            assert_ne!(
                plan.mode,
                ProtocolMode::Native,
                "{} → {} should not be Native",
                $ing,
                $eg
            );
            assert!(
                plan.needs_conversion,
                "{} → {} must need conversion",
                $ing, $eg
            );
        }
    };
}

off_diagonal_test!(
    chat_to_responses_is_transform,
    ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    egress = OPEN_RESPONSES_2026_04_24
);
off_diagonal_test!(
    chat_to_messages_is_transform,
    ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    egress = ANTHROPIC_MESSAGES_2023_06_01
);
off_diagonal_test!(
    chat_to_generate_is_transform,
    ingress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    egress = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
);

off_diagonal_test!(
    responses_to_chat_is_transform,
    ingress = OPEN_RESPONSES_2026_04_24,
    egress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
);
off_diagonal_test!(
    responses_to_messages_is_transform,
    ingress = OPEN_RESPONSES_2026_04_24,
    egress = ANTHROPIC_MESSAGES_2023_06_01
);
off_diagonal_test!(
    responses_to_generate_is_transform,
    ingress = OPEN_RESPONSES_2026_04_24,
    egress = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
);

off_diagonal_test!(
    messages_to_chat_is_transform,
    ingress = ANTHROPIC_MESSAGES_2023_06_01,
    egress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
);
off_diagonal_test!(
    messages_to_responses_is_transform,
    ingress = ANTHROPIC_MESSAGES_2023_06_01,
    egress = OPEN_RESPONSES_2026_04_24
);
off_diagonal_test!(
    messages_to_generate_is_transform,
    ingress = ANTHROPIC_MESSAGES_2023_06_01,
    egress = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
);

off_diagonal_test!(
    generate_to_chat_is_transform,
    ingress = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    egress = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
);
off_diagonal_test!(
    generate_to_responses_is_transform,
    ingress = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    egress = OPEN_RESPONSES_2026_04_24
);
off_diagonal_test!(
    generate_to_messages_is_transform,
    ingress = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    egress = ANTHROPIC_MESSAGES_2023_06_01
);

// ── negotiate() with one provider per protocol ────────────────────────────────

#[test]
fn each_protocol_provider_selects_own_native() {
    for proto in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPEN_RESPONSES_2026_04_24,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        let decl = single_decl(proto, "https://upstream.example.com");
        let mut ctx = req_ctx(proto);
        let plan = negotiate(proto, None, Some(&decl), &mut ctx).unwrap();
        assert_eq!(
            plan.mode,
            ProtocolMode::Native,
            "provider for {} must get Native",
            proto
        );
        assert_eq!(plan.egress, proto);
    }
}
