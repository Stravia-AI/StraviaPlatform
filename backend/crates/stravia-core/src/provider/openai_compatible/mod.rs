//! Catalog-driven adapter for ordinary OpenAI-compatible npm providers.

use crate::provider::metadata::{AuthMode, CapabilitiesSource, ChannelDef, Label, VendorMetadata};

const METADATA: VendorMetadata = VendorMetadata {
    id: "openai-compatible",
    label: Label {
        zh: "OpenAI 兼容",
        en: "OpenAI Compatible",
    },
    icon: "openai",
    default_protocol: "openai-compatible",
    credential_fields: crate::provider::metadata::API_KEY_CREDENTIAL_FIELDS,
    channels: &[ChannelDef {
        id: "default",
        label: Label {
            zh: "默认",
            en: "Default",
        },
        base_urls: &[],
        api_key: None,
        models_source: None,
        capabilities_source: CapabilitiesSource::Auto,
        static_models: &[],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
    }],
};

crate::openai_compat_vendor!(
    OpenAICompatibleVendor,
    "openai-compatible",
    METADATA,
    [crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
);
