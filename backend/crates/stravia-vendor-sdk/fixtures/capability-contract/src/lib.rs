use std::collections::BTreeSet;
use std::time::Duration;

use stravia_vendor_sdk::{
    AiError, AiResponse, CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField,
    ConfigFieldKind, DataCompatibility, ErrorKind, GuestHost, HttpRequest, MediaImageResponse,
    NetworkDeclaration, Operation, OperationInput, OperationOutput, PluginError, ProviderDescriptor,
    SearchResponse, VendorDescriptor, VendorGuest, VendorKind, read_http_body,
};

struct CapabilityContractVendor;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    PureSearch,
    PureImage,
    Multi,
    Removed,
    Incompatible,
    BaseOlder,
    DedicatedDeepseek,
}

fn profile() -> Profile {
    match option_env!("STRAVIA_CAPABILITY_PROFILE").unwrap_or("multi") {
        "pure-search" => Profile::PureSearch,
        "pure-image" => Profile::PureImage,
        "multi" => Profile::Multi,
        "removed" => Profile::Removed,
        "incompatible" => Profile::Incompatible,
        "base-older" => Profile::BaseOlder,
        "dedicated-deepseek" => Profile::DedicatedDeepseek,
        value => panic!("unknown capability fixture profile: {value}"),
    }
}

impl Profile {
    fn vendor_id(self) -> &'static str {
        match self {
            Self::PureSearch => "fixture.capability-search",
            Self::PureImage => "fixture.capability-image",
            Self::Multi | Self::Removed | Self::Incompatible => "fixture.capability-contract",
            Self::BaseOlder => "base",
            Self::DedicatedDeepseek => "deepseek",
        }
    }

    fn provider_id(self) -> &'static str {
        match self {
            Self::BaseOlder => "openai",
            Self::DedicatedDeepseek => "deepseek",
            _ => self.vendor_id(),
        }
    }

    fn kind(self) -> VendorKind {
        if self == Self::BaseOlder {
            VendorKind::Fallback
        } else {
            VendorKind::Dedicated
        }
    }

    fn channel(self) -> &'static str {
        if self == Self::DedicatedDeepseek {
            "dedicated"
        } else {
            "default"
        }
    }

    fn version(self) -> semver::Version {
        match self {
            Self::BaseOlder => semver::Version::new(0, 0, 0),
            Self::PureSearch | Self::PureImage | Self::Multi | Self::DedicatedDeepseek => {
                semver::Version::new(1, 0, 0)
            }
            Self::Removed => semver::Version::new(2, 0, 0),
            Self::Incompatible => semver::Version::new(3, 0, 0),
        }
    }

    fn capabilities(self) -> BTreeSet<Capability> {
        match self {
            Self::PureSearch | Self::DedicatedDeepseek => {
                BTreeSet::from([Capability::Search])
            }
            Self::PureImage => BTreeSet::from([Capability::MediaImage]),
            Self::Multi | Self::Incompatible | Self::BaseOlder => BTreeSet::from([
                Capability::Infer,
                Capability::Search,
                Capability::MediaImage,
            ]),
            Self::Removed => BTreeSet::from([Capability::Infer]),
        }
    }
}

impl VendorGuest for CapabilityContractVendor {
    fn descriptor() -> VendorDescriptor {
        let profile = profile();
        let capabilities = profile.capabilities();
        VendorDescriptor {
            vendor_id: profile.vendor_id().into(),
            version: profile.version(),
            display_name: format!("Capability Contract ({})", profile_name(profile)),
            description: Some(
                "Exercises full-search, image generation, capability removal, and update fences"
                    .into(),
            ),
            authors: vec!["Stravia test fixture".into()],
            canonical_format_version: CANONICAL_FORMAT_VERSION,
            kind: profile.kind(),
            providers: vec![ProviderDescriptor {
                provider_id: profile.provider_id().into(),
                catalog_id: (profile == Profile::DedicatedDeepseek).then(|| "deepseek".into()),
                display_name: format!("Capability Contract ({})", profile_name(profile)),
                description: None,
                channels: vec![ChannelDescriptor {
                id: profile.channel().into(),
                name: if profile == Profile::DedicatedDeepseek {
                    "Dedicated Search"
                } else {
                    "Default"
                }
                .into(),
                description: None,
                auth: None,
                protocol: (profile == Profile::BaseOlder).then(|| "openai-compatible".into()),
                default_base_url: None,
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            }],
            capabilities,
            config_fields: if profile == Profile::BaseOlder {
                vec![ConfigField {
                    key: "api_key".into(),
                    label: "API key".into(),
                    description: Some("OpenAI-compatible test credential".into()),
                    kind: ConfigFieldKind::String { multiline: false },
                    required: false,
                    default_json: None,
                    group: Some("Authentication".into()),
                    secret: true,
                    min: None,
                    max: None,
                    max_length: Some(8192),
                    pattern: None,
                    visible_when: None,
                }]
            } else {
                Vec::new()
            },
            network: NetworkDeclaration::default(),
            data_compat: DataCompatibility {
                private_state_format: if profile == Profile::Incompatible {
                    2
                } else {
                    1
                },
                ..DataCompatibility::default()
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
        let state_before = read_state(host)?;
        match input {
            OperationInput::Search { provider, request } => {
                ensure_capability(Capability::Search)?;
                let response = post_json::<_, SearchResponse>(
                    host,
                    &provider.base_url,
                    "/search",
                    &request,
                    state_before,
                )?;
                write_next_state(host, state_before)?;
                Ok(OperationOutput::Search(response))
            }
            OperationInput::MediaImage { provider, request } => {
                ensure_capability(Capability::MediaImage)?;
                let response = post_json::<_, MediaImageResponse>(
                    host,
                    &provider.base_url,
                    "/image",
                    &request,
                    state_before,
                )?;
                write_next_state(host, state_before)?;
                Ok(OperationOutput::MediaImage(response))
            }
            OperationInput::Infer { provider, request } => {
                ensure_capability(Capability::Infer)?;
                let response = post_json::<_, AiResponse>(
                    host,
                    &provider.base_url,
                    "/infer",
                    &request,
                    state_before,
                )?;
                write_next_state(host, state_before)?;
                host.emit_completed(&response)?;
                Ok(OperationOutput::Infer(Box::new(response)))
            }
            _ => Err(unsupported(operation)),
        }
    }
}

fn post_json<T, R>(
    host: &GuestHost,
    base_url: &str,
    path: &str,
    request: &T,
    state_before: u64,
) -> Result<R, PluginError>
where
    T: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let body = serde_json::to_vec(request).map_err(invalid)?;
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: format!("{}{path}", base_url.trim_end_matches('/')),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("x-state-before".into(), state_before.to_string()),
        ],
        body,
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, 8 * 1024 * 1024)?;
    if !(200..300).contains(&status) {
        let payload = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let retry_after = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(PluginError {
            kind: ErrorKind::upstream(
                Some(AiError::kind_from_status(status, Some(&payload))),
                retry_after,
            ),
            message: "capability fixture upstream rejected the request".into(),
            upstream_status: Some(status),
        });
    }
    serde_json::from_slice(&body).map_err(invalid)
}

fn read_state(host: &GuestHost) -> Result<u64, PluginError> {
    if !tracks_private_state() {
        return Ok(0);
    }
    host.read_private_state()?
        .as_deref()
        .map(|bytes| {
            std::str::from_utf8(bytes)
                .map_err(invalid)?
                .parse()
                .map_err(invalid)
        })
        .transpose()
        .map(|value| value.unwrap_or_default())
}

fn write_next_state(host: &GuestHost, state_before: u64) -> Result<(), PluginError> {
    if !tracks_private_state() {
        return Ok(());
    }
    let next = state_before
        .checked_add(1)
        .ok_or_else(|| invalid("capability fixture state exhausted"))?;
    host.write_private_state(next.to_string().as_bytes())
}

fn tracks_private_state() -> bool {
    matches!(profile(), Profile::Multi | Profile::Incompatible)
}

fn ensure_capability(capability: Capability) -> Result<(), PluginError> {
    if profile().capabilities().contains(&capability) {
        Ok(())
    } else {
        Err(unsupported(match capability {
            Capability::Infer => Operation::Infer,
            Capability::Search => Operation::Search,
            Capability::MediaImage => Operation::MediaImage,
            _ => Operation::Infer,
        }))
    }
}

fn unsupported(operation: Operation) -> PluginError {
    PluginError {
        kind: ErrorKind::Unsupported,
        message: format!("fixture profile does not implement {}", operation.as_str()),
        upstream_status: None,
    }
}

fn invalid(error: impl std::fmt::Display) -> PluginError {
    PluginError {
        kind: ErrorKind::Invalid,
        message: format!("capability fixture payload error: {error}"),
        upstream_status: None,
    }
}

fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::PureSearch => "pure-search",
        Profile::PureImage => "pure-image",
        Profile::Multi => "multi",
        Profile::Removed => "removed",
        Profile::Incompatible => "incompatible",
        Profile::BaseOlder => "base-older",
        Profile::DedicatedDeepseek => "dedicated-deepseek",
    }
}

stravia_vendor_sdk::export_vendor!(CapabilityContractVendor);
