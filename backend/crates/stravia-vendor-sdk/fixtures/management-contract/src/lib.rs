mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;
use stravia_vendor_sdk::{
    AiResponse, AllowanceAmount, AllowanceItem, AllowanceResponse, AuthCallback, AuthCallbackPort,
    AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType, AuthResponse, AuthStep,
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, DataCompatibility, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost,
    HttpRequest, NetworkDeclaration, Operation, OperationInput, OperationOutput, PluginError,
    ProviderDescriptor, ProviderSnapshot, VendorDescriptor, VendorGuest, VendorKind,
    read_http_body,
};

struct ManagementContractVendor;

#[derive(Clone, Copy)]
enum Profile {
    V1,
    V2,
    V3,
    Other,
}

impl Profile {
    fn current() -> Self {
        match option_env!("STRAVIA_FIXTURE_PROFILE").unwrap_or("v1") {
            "v1" => Self::V1,
            "v2" => Self::V2,
            "v3" => Self::V3,
            "other" => Self::Other,
            profile => panic!("unknown management fixture profile: {profile}"),
        }
    }

    fn vendor_id(self) -> &'static str {
        match self {
            Self::Other => "fixture.management.other",
            _ => "fixture.management",
        }
    }

    fn version(self) -> &'static str {
        match self {
            Self::V1 | Self::Other => "1.0.0",
            Self::V2 => "2.0.0",
            Self::V3 => "3.0.0",
        }
    }

    fn data_format(self) -> u32 {
        match self {
            Self::V3 => 2,
            _ => 1,
        }
    }
}

#[derive(Debug, serde::Serialize, Deserialize)]
struct PendingAuthorization {
    state: String,
    redirect_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenWire {
    access_token: String,
    refresh_token: String,
    expires_at_unix_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ModelsWire {
    models: Vec<ModelWire>,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelWire {
    id: String,
    display_name: String,
    family: Option<String>,
    selector: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AllowanceWire {
    remaining: String,
}

impl VendorGuest for ManagementContractVendor {
    fn descriptor() -> VendorDescriptor {
        let profile = Profile::current();
        let capabilities = BTreeSet::from([
            Capability::Infer,
            Capability::AuthOauth,
            Capability::ModelDiscovery,
            Capability::Allowance,
        ]);
        VendorDescriptor {
            vendor_id: profile.vendor_id().into(),
            version: semver::Version::parse(profile.version())
                .expect("management fixture version must be semver"),
            display_name: "Management Lifecycle Fixture".into(),
            description: Some(
                "Exercises OAuth, refresh, model discovery, allowance, and update fencing".into(),
            ),
            authors: vec!["Unverified management fixture author".into()],
            canonical_format_version: CANONICAL_FORMAT_VERSION,
            kind: VendorKind::Dedicated,
            providers: vec![ProviderDescriptor {
                provider_id: profile.vendor_id().into(),
                catalog_id: None,
                display_name: "Management Lifecycle Fixture".into(),
                description: None,
                channels: vec![ChannelDescriptor {
                    id: "default".into(),
                    name: crate::messages::channel_default(),
                    description: None,
                    auth: Some(AuthDescriptor {
                        flow: AuthFlow::AuthorizationCode,
                        callback: Some(AuthCallback {
                            bind_host: "127.0.0.1".into(),
                            redirect_host: "127.0.0.1".into(),
                            path: "/callback".into(),
                            port: AuthCallbackPort::Dynamic,
                            manual_redirect_uri: Some("http://127.0.0.1:18765/callback".into()),
                            cancel_path: None,
                        }),
                        manual_input: Some(AuthManualInput {
                            input_type: AuthManualInputType::CallbackUrl,
                            label: crate::messages::authorization_callback(),
                            description: None,
                            secret: false,
                        }),
                    }),
                    protocol: Some("fixture-management".into()),
                    protocols: Vec::new(),
                    default_base_url: None,
                    default_models_source: None,
                    consumes_catalog_models: false,
                    capabilities: capabilities.clone(),
                    model_capabilities: BTreeSet::new(),
                    search_model_required: false,
                }],
                capabilities,
                website: None,
                implementation: None,
                config_groups: vec![ConfigGroup {
                    id: "connection".into(),
                    label: crate::messages::connection(),
                }],
                config_fields: vec![ConfigField {
                    key: "workspace".into(),
                    label: crate::messages::workspace(),
                    description: Some(crate::messages::workspace_description()),
                    kind: ConfigFieldKind::String { multiline: false },
                    required: true,
                    default_json: None,
                    group: Some("connection".into()),
                    secret: false,
                    min: None,
                    max: None,
                    max_length: Some(128),
                    pattern: None,
                    visible_when: None,
                }],
                network: NetworkDeclaration {
                    base_url_field: None,
                    extra_origins: Vec::new(),
                    field_origins: Vec::new(),
                },
                data_compat: DataCompatibility {
                    config_fields_format: profile.data_format(),
                    private_state_format: profile.data_format(),
                    credentials_format: profile.data_format(),
                    model_metadata_format: profile.data_format(),
                },
            }],
        }
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        _channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        match (operation, input) {
            (Operation::Auth, OperationInput::Auth { provider, request }) => {
                auth(host, &provider, request.step).map(OperationOutput::Auth)
            }
            (Operation::Discover, OperationInput::Discover { provider, request }) => {
                discover(host, &provider, request.cursor).map(OperationOutput::Discover)
            }
            (Operation::Allowance, OperationInput::Allowance { provider, .. }) => {
                allowance(host, &provider).map(OperationOutput::Allowance)
            }
            (Operation::Infer, OperationInput::Infer { provider, request }) => {
                infer(host, &provider, &request)
            }
            (kind, input) if kind == input.operation() => Err(error(
                ErrorKind::Unsupported,
                format!("management fixture does not implement {}", kind.as_str()),
            )),
            _ => Err(error(
                ErrorKind::Invalid,
                "management fixture operation/input mismatch",
            )),
        }
    }
}

fn auth(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    step: AuthStep,
) -> Result<AuthResponse, PluginError> {
    match step {
        AuthStep::Start {
            redirect_uri,
            state,
        } => {
            if state.trim().is_empty() || redirect_uri.trim().is_empty() {
                return Err(error(
                    ErrorKind::Invalid,
                    "OAuth state and redirect URI are required",
                ));
            }
            host.write_private_state(
                &serde_json::to_vec(&PendingAuthorization {
                    state: state.clone(),
                    redirect_uri: redirect_uri.clone(),
                })
                .map_err(invalid)?,
            )?;
            Ok(AuthResponse::Authorization {
                url: format!(
                    "{}/authorize?state={}&redirect_uri={}",
                    provider.base_url.trim_end_matches('/'),
                    state,
                    redirect_uri
                ),
                user_code: None,
                verification_uri: Some(format!(
                    "{}/authorize",
                    provider.base_url.trim_end_matches('/')
                )),
                interval_seconds: None,
            })
        }
        AuthStep::Exchange { callback_url } => {
            let pending: PendingAuthorization = serde_json::from_slice(
                &host
                    .read_private_state()?
                    .ok_or_else(|| error(ErrorKind::Invalid, "missing pending OAuth state"))?,
            )
            .map_err(invalid)?;
            if !callback_url.contains(&format!("state={}", pending.state)) {
                return Err(error(ErrorKind::Invalid, "OAuth callback state mismatch"));
            }
            let wire: TokenWire = post_json(
                host,
                provider,
                "oauth_exchange",
                "/oauth/exchange",
                serde_json::json!({
                    "callback_url": callback_url,
                    "redirect_uri": pending.redirect_uri,
                }),
                false,
            )?;
            host.write_private_state(b"exchange-complete")?;
            Ok(credentials(wire))
        }
        AuthStep::Refresh => {
            let refresh_token = credential(provider, "refresh_token")?;
            let wire: TokenWire = post_json(
                host,
                provider,
                "oauth_refresh",
                "/oauth/refresh",
                serde_json::json!({ "refresh_token": refresh_token }),
                true,
            )?;
            advance_state(host)?;
            Ok(credentials(wire))
        }
        AuthStep::ManualInput { .. } | AuthStep::Poll | AuthStep::Revoke => Err(error(
            ErrorKind::Unsupported,
            "management fixture only supports authorization-code completion and refresh",
        )),
    }
}

fn credentials(wire: TokenWire) -> AuthResponse {
    AuthResponse::Credentials {
        values: BTreeMap::from([
            ("access_token".into(), Value::String(wire.access_token)),
            ("refresh_token".into(), Value::String(wire.refresh_token)),
            ("subject_id".into(), Value::String("fixture-account".into())),
        ]),
        expires_at_unix_ms: wire.expires_at_unix_ms,
    }
}

fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    cursor: Option<String>,
) -> Result<DiscoverResponse, PluginError> {
    require_connection(provider)?;
    let wire: ModelsWire = post_json(
        host,
        provider,
        "model_discovery",
        "/models",
        serde_json::json!({ "cursor": cursor }),
        true,
    )?;
    let version = Profile::current().version();
    let ModelsWire {
        models,
        next_cursor,
    } = wire;
    let models = models
        .into_iter()
        .map(|model| DiscoveredModel {
            id: model.id,
            display_name: model.display_name,
            family: model.family,
            selector: model.selector,
            capabilities: vec!["tools".into()],
            metadata: BTreeMap::from([
                (
                    "provider".into(),
                    serde_json::json!({ "fixture_version": version }),
                ),
                ("fixture_version".into(), Value::String(version.into())),
            ]),
        })
        .collect();
    advance_state(host)?;
    Ok(DiscoverResponse {
        models,
        next_cursor,
    })
}

fn allowance(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    require_connection(provider)?;
    let wire: AllowanceWire = post_json(
        host,
        provider,
        "allowance",
        "/allowance",
        serde_json::json!({}),
        true,
    )?;
    advance_state(host)?;
    Ok(AllowanceResponse {
        allowances: vec![AllowanceItem {
            key: "requests".into(),
            label: "Requests".into(),
            kind: "request_allowance".into(),
            used: None,
            remaining: Some(AllowanceAmount {
                value: wire.remaining,
                unit: "requests".into(),
                currency: None,
            }),
            limit: Some(AllowanceAmount {
                value: "100".into(),
                unit: "requests".into(),
                currency: None,
            }),
            used_percent: None,
            window_seconds: Some(3600),
            resets_at_unix_ms: None,
            condition: None,
        }],
        models: Vec::new(),
        plan_label: Some(format!("fixture-{}", Profile::current().version())),
    })
}

fn infer(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &stravia_vendor_sdk::AiRequest,
) -> Result<OperationOutput, PluginError> {
    require_connection(provider)?;
    let metadata = provider
        .model_metadata
        .as_ref()
        .ok_or_else(|| error(ErrorKind::Invalid, "discovered model metadata is required"))?;
    if !metadata.extensions.contains_key("fixture_version") {
        return Err(error(
            ErrorKind::Invalid,
            "discovered model metadata must be restored before inference",
        ));
    }
    host.emit_started()?;
    let response: AiResponse = post_json(host, provider, "infer", "/infer", request, true)?;
    advance_state(host)?;
    host.emit_completed(&response)?;
    Ok(OperationOutput::Infer(Box::new(response)))
}

fn post_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    operation: &str,
    path: &str,
    body: T,
    authenticated: bool,
) -> Result<R, PluginError> {
    let state_before = state_label(host)?;
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        (
            "x-management-version".into(),
            Profile::current().version().into(),
        ),
        (
            "x-management-vendor".into(),
            Profile::current().vendor_id().into(),
        ),
        ("x-management-operation".into(), operation.into()),
        ("x-state-before".into(), state_before),
        (
            "x-workspace".into(),
            provider
                .options
                .get("workspace")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
        ),
    ];
    if authenticated {
        headers.push((
            "authorization".into(),
            format!("Bearer {}", credential(provider, "access_token")?),
        ));
    }
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: format!("{}{}", provider.base_url.trim_end_matches('/'), path),
        headers,
        body: serde_json::to_vec(&body).map_err(invalid)?,
    })?;
    let status = response.status()?;
    let bytes = read_http_body(&response, 1024 * 1024)?;
    if !(200..300).contains(&status) {
        return Err(PluginError {
            kind: ErrorKind::upstream_unknown(),
            message: format!("management fixture upstream returned HTTP {status}"),
            upstream_status: Some(status),
        });
    }
    serde_json::from_slice(&bytes).map_err(invalid)
}

fn require_connection(provider: &ProviderSnapshot) -> Result<(), PluginError> {
    let workspace = provider
        .options
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error(ErrorKind::Invalid, "workspace configuration is required"))?;
    if workspace.len() > 128 {
        return Err(error(
            ErrorKind::Invalid,
            "workspace configuration is too long",
        ));
    }
    credential(provider, "access_token")?;
    Ok(())
}

fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Result<&'a str, PluginError> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error(ErrorKind::Auth, format!("missing {key}")))
}

fn state_label(host: &GuestHost) -> Result<String, PluginError> {
    Ok(match host.read_private_state()? {
        None => "empty".into(),
        Some(bytes) => std::str::from_utf8(&bytes)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.to_string())
            .unwrap_or_else(|| "present".into()),
    })
}

fn advance_state(host: &GuestHost) -> Result<(), PluginError> {
    let next = host
        .read_private_state()?
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    host.write_private_state(next.to_string().as_bytes())
}

fn invalid(error: impl std::fmt::Display) -> PluginError {
    PluginError {
        kind: ErrorKind::Invalid,
        message: format!("management fixture payload error: {error}"),
        upstream_status: None,
    }
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

stravia_vendor_sdk::export_vendor!(ManagementContractVendor);
