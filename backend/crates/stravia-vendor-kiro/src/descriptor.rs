use std::collections::BTreeSet;

use stravia_vendor_sdk::{
    AuthDescriptor, AuthFlow, CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField,
    ConfigFieldKind, ConfigGroup, DataCompatibility, FieldCondition, NetworkDeclaration,
    OriginDeclaration, OriginHost, ProviderDescriptor, VendorDescriptor, VendorKind,
};

use crate::{
    BUILDER_ID_CHANNEL, DEFAULT_BASE_URL, GITHUB_CHANNEL, GOOGLE_CHANNEL, IDENTITY_CENTER_CHANNEL,
    IDENTITY_CENTER_PROTOCOL, OAUTH_PROTOCOL, VENDOR_ID, auth, messages,
};

fn oidc_origin() -> OriginDeclaration {
    const PREFIX: &str = r"oidc\.(?:";
    const SUFFIX: &str = r")\.amazonaws\.com";
    let regions = auth::OIDC_REGIONS;
    let mut regex = String::with_capacity(
        PREFIX.len()
            + SUFFIX.len()
            + regions.iter().map(|region| region.len()).sum::<usize>()
            + regions.len().saturating_sub(1),
    );
    regex.push_str(PREFIX);
    for (index, region) in regions.iter().enumerate() {
        if index != 0 {
            regex.push('|');
        }
        // 区域为写死的 ASCII 标签；不把用户输入拼进网络权限。
        regex.push_str(region);
    }
    regex.push_str(SUFFIX);
    OriginDeclaration {
        scheme: "https".into(),
        host: OriginHost::Regex { regex },
        port: None,
    }
}

pub(super) fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::AuthOauth,
    ]);
    let channel = |id: &str, name, description, protocol: &str| ChannelDescriptor {
        id: id.into(),
        name,
        description: Some(description),
        auth: Some(AuthDescriptor {
            flow: AuthFlow::DeviceCode,
            callback: None,
            manual_input: None,
        }),
        protocol: Some(protocol.into()),
        protocols: Vec::new(),
        default_base_url: Some(DEFAULT_BASE_URL.into()),
        default_models_source: None,
        consumes_catalog_models: false,
        capabilities: capabilities.clone(),
        client_header_names: BTreeSet::new(),
        model_capabilities: BTreeSet::new(),
    };
    let field = |key: &str, label, description, max_length| ConfigField {
        key: key.into(),
        label,
        description: Some(description),
        kind: ConfigFieldKind::String { multiline: false },
        required: true,
        default_json: None,
        group: Some("authentication".into()),
        secret: false,
        min: None,
        max: None,
        max_length: Some(max_length),
        pattern: None,
        visible_when: Some(FieldCondition {
            field: FieldCondition::PROTOCOL_CONTEXT.into(),
            equals: serde_json::json!(IDENTITY_CENTER_PROTOCOL),
        }),
    };
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
            channels: vec![
                channel(
                    GOOGLE_CHANNEL,
                    messages::channel_google(),
                    messages::channel_google_description(),
                    OAUTH_PROTOCOL,
                ),
                channel(
                    GITHUB_CHANNEL,
                    messages::channel_github(),
                    messages::channel_github_description(),
                    OAUTH_PROTOCOL,
                ),
                channel(
                    BUILDER_ID_CHANNEL,
                    messages::channel_builder_id(),
                    messages::channel_builder_id_description(),
                    OAUTH_PROTOCOL,
                ),
                channel(
                    IDENTITY_CENTER_CHANNEL,
                    messages::channel_identity_center(),
                    messages::channel_identity_center_description(),
                    IDENTITY_CENTER_PROTOCOL,
                ),
            ],
            capabilities,
            config_groups: vec![ConfigGroup {
                id: "authentication".into(),
                label: messages::authentication(),
            }],
            config_fields: vec![
                field(
                    "start_url",
                    messages::start_url(),
                    messages::start_url_description(),
                    512,
                ),
                field(
                    "region",
                    messages::region(),
                    messages::region_description(),
                    32,
                ),
            ],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: [
                    "runtime.us-east-1.kiro.dev",
                    "runtime.eu-central-1.kiro.dev",
                    "management.us-east-1.kiro.dev",
                    "management.eu-central-1.kiro.dev",
                    "prod.us-east-1.auth.desktop.kiro.dev",
                ]
                .into_iter()
                .map(|host| OriginDeclaration {
                    scheme: "https".into(),
                    host: host.into(),
                    port: None,
                })
                .chain(std::iter::once(oidc_origin()))
                .collect(),
                field_origins: Vec::new(),
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}
