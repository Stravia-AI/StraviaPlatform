mod allowance;
mod auth;
mod catalog;
mod client;
mod selector;
mod wire;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::BTreeSet;

use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::common;
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType, CANONICAL_FORMAT_VERSION,
    Capability, ChannelDescriptor, DataCompatibility, ErrorKind, GuestHost, NetworkDeclaration,
    Operation, OperationInput, OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor,
    ProviderSnapshot, VendorDescriptor, VendorKind,
};

const VENDOR_ID: &str = "antigravity";
const CHANNEL: &str = "oauth";
const BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const CLI_USER_AGENT: &str =
    "antigravity/cli/1.2.16 (aidev_client; os_type=linux; arch=amd64; auth_method=consumer)";

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
    ]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("valid package version"),
        display_name: "Antigravity".into(),
        description: Some("Antigravity CLI OAuth models and account quota monitoring.".into()),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: None,
            icon_svg: Some(include_str!("assets/antigravity.svg").into()),
            display_name: "Antigravity".into(),
            description: Some("Third-party OAuth access may violate Google's terms and lead to account suspension.".into()),
            channels: vec![ChannelDescriptor {
                id: CHANNEL.into(),
                name: messages::channel_oauth(),
                description: Some(messages::channel_description()),
                auth: Some(AuthDescriptor {
                    flow: AuthFlow::AuthorizationCode,
                    callback: None,
                    manual_input: Some(AuthManualInput {
                        input_type: AuthManualInputType::Text,
                        label: messages::authorization_code(),
                        description: Some(messages::authorization_description()),
                        secret: true,
                    }),
                }),
                protocol: Some("google-gemini".into()),
                protocols: Vec::new(),
                default_base_url: Some(BASE_URL.into()),
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
            }],
            capabilities,
            website: Some("https://antigravity.google".into()),
            implementation: None,
            config_groups: Vec::new(),
            config_fields: Vec::new(),
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: ["oauth2.googleapis.com"]
                    .into_iter()
                    .map(|host| OriginDeclaration {
                        scheme: "https".into(), host: host.into(), port: None,
                    })
                    .collect(),
                field_origins: Vec::new(),
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}

fn require_route(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != VENDOR_ID || channel != CHANNEL || provider.channel != CHANNEL {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            "Antigravity requires its OAuth channel",
        ));
    }
    Ok(())
}

pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    require_route(channel, provider)?;
    if operation != Operation::Infer {
        return Err(common::unsupported(operation.as_str(), VENDOR_ID, channel));
    }
    Ok(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string())
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    require_route(channel, input.provider())?;
    if input.operation() != operation {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "operation input does not match operation",
        ));
    }
    match input {
        OperationInput::Infer { provider, request } => wire::infer(host, provider, request)
            .map(Box::new)
            .map(OperationOutput::Infer),
        OperationInput::Auth { provider, request } => {
            auth::execute(host, &provider, request).map(OperationOutput::Auth)
        }
        OperationInput::Discover {
            provider,
            request: _,
        } => catalog::execute(host, &provider).map(OperationOutput::Discover),
        OperationInput::Allowance {
            provider,
            request: _,
        } => allowance::execute(host, &provider).map(OperationOutput::Allowance),
        _ => Err(common::unsupported(operation.as_str(), VENDOR_ID, channel)),
    }
}

#[cfg(target_arch = "wasm32")]
struct AntigravityVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for AntigravityVendor {
    fn descriptor() -> VendorDescriptor {
        descriptor()
    }
    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        request: &AiRequest,
    ) -> Result<String, PluginError> {
        select_protocol(operation, channel, provider, request)
    }
    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        execute(host, operation, channel, input)
    }
}

#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(AntigravityVendor);
