mod allowance;
mod auth;
mod connection;
mod models;
mod request;
mod stream;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::common;
use stravia_vendor_sdk::*;

const ORIGIN: &str = "https://api.kilo.ai";
const BASE: &str = "https://api.kilo.ai/api/gateway";
const CHAT: &str = "openai-compatible/chat-completions/v1";
const RESPONSES: &str = "open-responses/responses/2026-04-24";
const MAX_BODY: usize = 16 * 1024 * 1024;

fn fail(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    common::plugin_error(kind, message)
}
fn check(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != "kilo" || channel != "default" || provider.channel != channel {
        return Err(fail(ErrorKind::Invalid, "Kilo provider/channel mismatch"));
    }
    if provider.base_url.trim_end_matches('/') != BASE {
        return Err(fail(
            ErrorKind::Invalid,
            "Kilo requires https://api.kilo.ai/api/gateway",
        ));
    }
    Ok(())
}
fn key(provider: &ProviderSnapshot) -> Result<Option<&str>, PluginError> {
    if provider
        .credentials
        .get("expiresAtUnixMs")
        .and_then(Value::as_i64)
        .is_some_and(|t| t <= chrono::Utc::now().timestamp_millis())
    {
        return Err(fail(ErrorKind::Auth, "Kilo token expired; sign in again"));
    }
    Ok(provider
        .credentials
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty()))
}
fn token(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    key(provider)?.ok_or_else(|| fail(ErrorKind::Auth, "Kilo credentials missing; sign in again"))
}
fn organization(provider: &ProviderSnapshot) -> Option<&str> {
    provider
        .credentials
        .get("organizationId")
        .or_else(|| provider.options.get("organizationId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}
fn headers(provider: &ProviderSnapshot) -> Result<Vec<(String, String)>, PluginError> {
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
    ];
    if let Some(token) = key(provider)? {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    if let Some(org) = organization(provider) {
        if key(provider)?.is_none() {
            return Err(fail(
                ErrorKind::Auth,
                "Organization access requires authentication",
            ));
        }
        let verified_selection = provider
            .credentials
            .get("organizationId")
            .and_then(Value::as_str)
            .is_some_and(|selected| selected.trim() == org);
        let membership = provider
            .credentials
            .get("organizations")
            .and_then(Value::as_array)
            .is_some_and(|orgs| {
                orgs.iter()
                    .any(|entry| entry.get("id").and_then(Value::as_str) == Some(org))
            });
        if !verified_selection && !membership {
            return Err(fail(
                ErrorKind::Auth,
                "Selected Kilo organization requires verified membership; sign in again",
            ));
        }
        headers.push(("X-KiloCode-OrganizationId".into(), org.into()));
    }
    Ok(headers)
}
fn escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
fn http_json(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    method: &str,
    path: &str,
) -> Result<Value, PluginError> {
    let response = host.http_start(HttpRequest {
        method: method.into(),
        url: format!("{ORIGIN}{path}"),
        headers: headers(provider)?,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let bytes = read_http_body(&response, MAX_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &response_headers, &bytes));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| fail(ErrorKind::Invalid, format!("Invalid Kilo JSON: {e}")))
}
pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::AuthOauth,
        Capability::Allowance,
        Capability::ConnectionCheck,
    ]);
    let channel = ChannelDescriptor {
        id: "default".into(),
        name: messages::default_channel(),
        description: None,
        auth: Some(AuthDescriptor {
            flow: AuthFlow::DeviceCode,
            callback: None,
            manual_input: None,
        }),
        protocol: Some("openai-compatible".into()),
        protocols: Vec::new(),
        default_base_url: Some(BASE.into()),
        default_models_source: None,
        consumes_catalog_models: false,
        capabilities: capabilities.clone(),
        client_header_names: BTreeSet::new(),
        model_capabilities: BTreeSet::new(),
    };
    VendorDescriptor {
        vendor_id: "kilo".into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("valid version"),
        display_name: "Kilo".into(),
        description: Some(
            "Kilo Gateway: personal and organization accounts via device sign-in".into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: "kilo".into(),
            catalog_id: Some("kilo".into()),
            icon_svg: None,
            display_name: "Kilo".into(),
            description: None,
            channels: vec![channel],
            capabilities,
            website: Some("https://kilo.ai".into()),
            implementation: None,
            config_groups: vec![ConfigGroup {
                id: "organization".into(),
                label: messages::organization_group(),
            }],
            config_fields: vec![ConfigField {
                key: "organizationId".into(),
                label: messages::organization_id(),
                description: Some(messages::organization_description()),
                kind: ConfigFieldKind::String { multiline: false },
                required: false,
                default_json: None,
                group: Some("organization".into()),
                secret: false,
                min: None,
                max: None,
                max_length: Some(16384),
                pattern: None,
                visible_when: None,
            }],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: vec![OriginDeclaration {
                    scheme: "https".into(),
                    host: "api.kilo.ai".into(),
                    port: None,
                }],
                field_origins: vec![],
            },
            data_compat: DataCompatibility {
                config_fields_format: 1,
                private_state_format: 1,
                credentials_format: 1,
                model_metadata_format: 1,
            },
        }],
    }
}
pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    request: &AiRequest,
) -> Result<String, PluginError> {
    check(channel, provider)?;
    if operation != Operation::Infer {
        return Err(fail(
            ErrorKind::Unsupported,
            "Protocol selection is only available for inference",
        ));
    }
    Ok(if request::responses(provider, &request.model) {
        RESPONSES
    } else {
        CHAT
    }
    .into())
}
pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if operation != input.operation() {
        return Err(fail(ErrorKind::Invalid, "Operation/input mismatch"));
    }
    check(channel, input.provider())?;
    match input {
        OperationInput::Auth { provider, request } => {
            auth::execute(host, &provider, request.step).map(OperationOutput::Auth)
        }
        OperationInput::Discover { provider, .. } => {
            models::discover(host, &provider).map(OperationOutput::Discover)
        }
        OperationInput::Allowance { provider, .. } => {
            allowance::execute(host, &provider).map(OperationOutput::Allowance)
        }
        OperationInput::ConnectionCheck { provider, .. } => {
            connection::check(host, &provider).map(OperationOutput::ConnectionCheck)
        }
        OperationInput::Infer { provider, request } => request::infer(host, &provider, request),
        _ => Err(fail(ErrorKind::Unsupported, "Unsupported Kilo operation")),
    }
}
#[cfg(target_arch = "wasm32")]
struct KiloVendor;
#[cfg(target_arch = "wasm32")]
impl VendorGuest for KiloVendor {
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
stravia_vendor_sdk::export_vendor!(KiloVendor);

#[cfg(test)]
#[path = "../tests/behavior.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/stream_behavior.rs"]
mod stream_tests;
