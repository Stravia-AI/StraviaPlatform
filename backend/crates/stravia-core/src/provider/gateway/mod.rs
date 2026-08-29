//! Vercel AI Gateway's AI SDK v4 language-model adapter.

const METADATA: crate::provider::metadata::VendorMetadata =
    crate::provider::metadata::VendorMetadata {
        id: "gateway",
        label: crate::provider::metadata::Label {
            zh: "Vercel AI Gateway",
            en: "Vercel AI Gateway",
        },
        icon: "vercel",
        default_protocol: "gateway-language-model",
        credential_fields: crate::provider::metadata::API_KEY_CREDENTIAL_FIELDS,
        channels: &[crate::provider::metadata::ChannelDef {
            id: "default",
            label: crate::provider::metadata::Label {
                zh: "默认",
                en: "Default",
            },
            base_urls: &[],
            api_key: None,
            models_source: None,
            capabilities_source: crate::provider::metadata::CapabilitiesSource::Auto,
            static_models: &[],
            auth_mode: crate::provider::metadata::AuthMode::ApiKey,
            oauth: None,
            runtime: None,
        }],
    };

crate::openai_compat_vendor!(
    GatewayVendor,
    "gateway",
    METADATA,
    [crate::protocol::ids::GATEWAY_LANGUAGE_MODEL_V4]
);
