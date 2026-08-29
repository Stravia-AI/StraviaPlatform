//! Cohere Chat API v2 adapter.

use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, CredentialFieldDef, CredentialInputKind, Label,
    VendorMetadata,
};

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[CredentialFieldDef {
    key: "apiKey",
    label: "API key",
    secret: true,
    required: true,
    input: CredentialInputKind::Password,
}];

const METADATA: VendorMetadata = VendorMetadata {
    id: "cohere",
    label: Label {
        zh: "Cohere",
        en: "Cohere",
    },
    icon: "cohere",
    default_protocol: "cohere-chat",
    credential_fields: CREDENTIAL_FIELDS,
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
    CohereVendor,
    "cohere",
    METADATA,
    [crate::protocol::ids::COHERE_CHAT_V2]
);
