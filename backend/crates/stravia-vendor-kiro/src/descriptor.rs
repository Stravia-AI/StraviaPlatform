use std::collections::BTreeSet;

use stravia_vendor_sdk::{
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, DataCompatibility, NetworkDeclaration, OriginDeclaration, ProviderDescriptor,
    VendorDescriptor, VendorKind,
};

use crate::{CHANNEL_ID, DEFAULT_BASE_URL, PROTOCOL, VENDOR_ID, messages};

pub(super) fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::Allowance,
    ]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: env!("CARGO_PKG_VERSION")
            .parse()
            .expect("package version must be valid semver"),
        display_name: "Kiro".into(),
        description: Some("Native Kiro inference, account model discovery and credit usage".into()),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        egress_shapes: vec![
            serde_json::from_str(include_str!("../egress-shape.json"))
                .expect("valid Kiro CLI 3.4.0 egress shape"),
        ],
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: None,
            icon_svg: None,
            display_name: "Kiro".into(),
            description: None,
            website: Some("https://kiro.dev".into()),
            implementation: None,
            channels: vec![ChannelDescriptor {
                id: CHANNEL_ID.into(),
                name: messages::channel_default(),
                description: Some(messages::channel_description()),
                auth: None,
                protocol: Some(PROTOCOL.into()),
                protocols: Vec::new(),
                default_base_url: Some(DEFAULT_BASE_URL.into()),
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                client_header_names: BTreeSet::new(),
                model_capabilities: BTreeSet::new(),
            }],
            capabilities,
            config_groups: vec![ConfigGroup {
                id: "authentication".into(),
                label: messages::authentication(),
            }],
            config_fields: vec![ConfigField {
                key: "apiKey".into(),
                label: messages::api_key(),
                description: Some(messages::api_key_description()),
                kind: ConfigFieldKind::String { multiline: false },
                required: true,
                default_json: None,
                group: Some("authentication".into()),
                secret: true,
                min: None,
                max: None,
                max_length: Some(16_384),
                pattern: None,
                visible_when: None,
            }],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: [
                    "runtime.us-east-1.kiro.dev",
                    "runtime.eu-central-1.kiro.dev",
                    "management.us-east-1.kiro.dev",
                    "management.eu-central-1.kiro.dev",
                ]
                .into_iter()
                .map(|host| OriginDeclaration {
                    scheme: "https".into(),
                    host: host.into(),
                    port: None,
                })
                .collect(),
                field_origins: Vec::new(),
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}
