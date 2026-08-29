//! PR2A acceptance: VendorRegistry resolves (channel → vendor → family)
//! correctly, every registered extension produces auth/url output that
//! matches the legacy `ProviderAdapter` surface, and `list_metadata()`
//! is field-equivalent to `assets/providers.json` for the three
//! vendors migrated in PR2A (`openai`, `ollama`, plus the OpenAI/codex
//! channel).

use stravia_core::auth::types::StoredCredential;
use stravia_core::db::models::Provider;
use stravia_core::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId,
};
use stravia_core::provider::{VendorCtx, VendorRegistry, VendorScope};

fn make_provider(vendor: Option<&str>, channel: Option<&str>) -> Provider {
    Provider {
        id: "test".into(),
        name: "test".into(),
        vendor: vendor.map(str::to_string),
        protocol: "openai".into(),
        base_url: "https://api.example.com/v1".into(),
        preset_key: None,
        channel: channel.map(str::to_string),
        models_source: None,
        static_models: None,
        api_key: "sk-test".into(),
        adapter_credentials: r#"{"apiKey":"sk-test"}"#.into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn ctx<'a>(
    provider: &'a Provider,
    protocol_id: ProtocolId,
    api_key: &'a str,
    actual_model: &'a str,
    credential: Option<&'a StoredCredential>,
) -> VendorCtx<'a> {
    VendorCtx {
        provider,
        protocol_id,
        api_key,
        actual_model,
        credential,
    }
}

// ── 1. Three-tier resolution ──────────────────────────────────────────────

#[test]
fn resolve_channel_scope_takes_priority() {
    let reg = VendorRegistry::global();
    let p = make_provider(Some("openai"), Some("codex"));
    let ext = reg
        .resolve(&p, OPEN_RESPONSES_2026_04_24)
        .expect("codex channel ext");
    assert!(matches!(
        ext.scope(),
        VendorScope::Channel {
            vendor_id: "openai",
            channel_id: "codex",
        }
    ));
}

#[test]
fn resolve_falls_back_to_vendor_when_channel_unknown() {
    let reg = VendorRegistry::global();
    let p = make_provider(Some("openai"), Some("unknown-channel"));
    let ext = reg
        .resolve(&p, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .expect("openai vendor ext");
    assert!(matches!(
        ext.scope(),
        VendorScope::Vendor {
            vendor_id: "openai"
        }
    ));
}

#[test]
fn vertex_vendor_is_registered_with_native_and_openai_channels() {
    let reg = VendorRegistry::global();
    let meta = reg.metadata("google-vertex").expect("vertex metadata");
    assert_eq!(meta.default_protocol, "google-gemini");
    assert!(
        meta.channels
            .iter()
            .any(|c| c.base_urls.iter().any(|b| b.protocol == "google-gemini")),
        "vertex must expose native google-gemini endpoint"
    );
    assert!(
        meta.channels.iter().any(|c| c
            .base_urls
            .iter()
            .any(|b| b.protocol == "openai-compatible")),
        "vertex must expose OpenAI-compatible endpoint"
    );
}

#[test]
fn vertex_build_url_rewrites_google_generate_content_to_vertex_resource() {
    let reg = VendorRegistry::global();
    let mut p = make_provider(Some("google-vertex"), None);
    p.protocol = "google-gemini".into();
    p.base_url = "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global".into();
    p.api_key = r#"{"project_id":"demo-project"}"#.into();
    p.adapter_credentials = format!(r#"{{"credentials":{}}}"#, p.api_key);
    let ext = reg
        .resolve(&p, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        .expect("vertex vendor ext");
    let ctx = ctx(
        &p,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        &p.api_key,
        "gemini-2.5-flash",
        None,
    );

    let url = ext.build_url(
        &ctx,
        &p.base_url,
        "/v1beta/models/gemini-2.5-flash:generateContent",
    );

    assert_eq!(
        url,
        "https://aiplatform.googleapis.com/v1/projects/demo-project/locations/global/publishers/google/models/gemini-2.5-flash:generateContent"
    );
}

#[test]
fn vertex_build_url_rewrites_openai_compat_path_without_double_version() {
    let reg = VendorRegistry::global();
    let mut p = make_provider(Some("google-vertex"), None);
    p.protocol = "openai-compatible".into();
    p.base_url = "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/endpoints/openapi".into();
    p.api_key = r#"{"project_id":"demo-project"}"#.into();
    p.adapter_credentials = format!(r#"{{"credentials":{}}}"#, p.api_key);
    let ext = reg
        .resolve(&p, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .expect("vertex vendor ext");
    let ctx = ctx(
        &p,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        &p.api_key,
        "google/gemini-2.5-flash",
        None,
    );

    let url = ext.build_url(&ctx, &p.base_url, "/v1/chat/completions");

    assert_eq!(
        url,
        "https://aiplatform.googleapis.com/v1/projects/demo-project/locations/global/endpoints/openapi/chat/completions"
    );
}

#[test]
fn resolve_falls_back_to_protocol_default_vendor_when_vendor_unknown() {
    let reg = VendorRegistry::global();
    let p = make_provider(Some("unmapped-vendor"), None);
    let openai = reg
        .resolve(&p, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .expect("openai protocol default");
    let anthropic = reg
        .resolve(&p, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("anthropic protocol default");
    let google = reg
        .resolve(&p, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        .expect("google protocol default");

    // Resolves to the default vendor for each protocol suite
    assert!(matches!(
        openai.scope(),
        VendorScope::Vendor {
            vendor_id: "openai"
        }
    ));
    assert!(matches!(
        anthropic.scope(),
        VendorScope::Vendor {
            vendor_id: "anthropic"
        }
    ));
    assert!(matches!(
        google.scope(),
        VendorScope::Vendor {
            vendor_id: "google"
        }
    ));
}

#[test]
fn resolve_uses_protocol_default_vendor_when_vendor_field_blank() {
    let reg = VendorRegistry::global();
    let p = make_provider(None, None);
    let ext = reg
        .resolve(&p, ANTHROPIC_MESSAGES_2023_06_01)
        .expect("protocol default vendor fallback");
    assert!(matches!(
        ext.scope(),
        VendorScope::Vendor {
            vendor_id: "anthropic"
        }
    ));
}

#[test]
fn ollama_vendor_resolves_even_without_channel() {
    let reg = VendorRegistry::global();
    let p = make_provider(Some("ollama"), None);
    let ext = reg
        .resolve(&p, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .expect("ollama vendor");
    assert!(matches!(
        ext.scope(),
        VendorScope::Vendor {
            vendor_id: "ollama"
        }
    ));
}

// ── 2. auth_headers / build_url legacy parity ─────────────────────────────

#[test]
fn openai_family_default_emits_bearer() {
    let reg = VendorRegistry::global();
    let p = make_provider(None, None);
    let ext = reg
        .resolve(&p, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .unwrap();
    let h = ext.auth_headers(&ctx(
        &p,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "sk-abc",
        "gpt-4",
        None,
    ));
    assert_eq!(h.get("Authorization").unwrap(), "Bearer sk-abc");
}

#[test]
fn anthropic_family_default_emits_x_api_key_and_version() {
    let reg = VendorRegistry::global();
    let p = make_provider(None, None);
    let ext = reg.resolve(&p, ANTHROPIC_MESSAGES_2023_06_01).unwrap();
    let h = ext.auth_headers(&ctx(
        &p,
        ANTHROPIC_MESSAGES_2023_06_01,
        "sk-ant",
        "claude",
        None,
    ));
    assert_eq!(h.get("x-api-key").unwrap(), "sk-ant");
    assert_eq!(h.get("anthropic-version").unwrap(), "2023-06-01");
}

#[test]
fn google_family_default_appends_key_query_param() {
    let reg = VendorRegistry::global();
    let p = make_provider(None, None);
    let ext = reg
        .resolve(&p, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        .unwrap();
    let c = ctx(
        &p,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        "AIzaXYZ",
        "gemini-1.5",
        None,
    );

    let url1 = ext.build_url(
        &c,
        "https://generativelanguage.googleapis.com",
        "/v1beta/models",
    );
    assert_eq!(
        url1,
        "https://generativelanguage.googleapis.com/v1beta/models?key=AIzaXYZ"
    );

    let url2 = ext.build_url(
        &c,
        "https://generativelanguage.googleapis.com/v1beta",
        "/models?alt=sse",
    );
    assert_eq!(
        url2,
        "https://generativelanguage.googleapis.com/v1beta/models?alt=sse&key=AIzaXYZ"
    );
}

#[test]
fn openai_compat_strips_v1_when_base_already_has_path() {
    let reg = VendorRegistry::global();
    let p = make_provider(None, None);
    let ext = reg
        .resolve(&p, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .unwrap();
    let c = ctx(&p, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, "k", "m", None);

    let stripped = ext.build_url(&c, "https://api.deepseek.com/v1", "/v1/chat/completions");
    assert_eq!(stripped, "https://api.deepseek.com/v1/chat/completions");

    let preserved = ext.build_url(&c, "https://api.openai.com", "/v1/chat/completions");
    assert_eq!(preserved, "https://api.openai.com/v1/chat/completions");
}

// ── 3. Registered metadata matches the complete Vendor roster ────────────────

const PROVIDERS_JSON: &str = include_str!("fixtures/providers_registry.json");

fn expected_vendor_ids() -> std::collections::HashSet<String> {
    serde_json::from_str(PROVIDERS_JSON).expect("vendor fixture must be a JSON string array")
}

#[test]
fn list_metadata_matches_every_registered_vendor() {
    let reg = VendorRegistry::global();
    let registered: std::collections::HashSet<String> = reg
        .list_metadata()
        .into_iter()
        .map(|metadata| metadata.id.to_string())
        .collect();
    assert_eq!(registered, expected_vendor_ids());
}

#[test]
fn credential_field_labels_serialize_as_english_strings() {
    let metadata = VendorRegistry::global()
        .list_metadata()
        .into_iter()
        .find(|metadata| metadata.id == "azure")
        .expect("azure must be registered");
    let serialized = serde_json::to_value(metadata).expect("vendor metadata must serialize");

    assert_eq!(
        serialized["credentialFields"][0]["label"],
        serde_json::Value::String("Azure resource name".into())
    );
}

// ── 4. Unsupported placeholder vendors must NOT be registered ───────────────

#[test]
fn unsupported_placeholder_vendors_are_not_registered() {
    let reg = VendorRegistry::global();
    let registered: std::collections::HashSet<&str> =
        reg.list_metadata().into_iter().map(|m| m.id).collect();

    for placeholder in ["azure-foundry", "aws-bedrock"] {
        assert!(
            !registered.contains(placeholder),
            "placeholder vendor `{placeholder}` should not yet be registered"
        );
    }
}
