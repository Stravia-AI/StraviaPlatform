use stravia_vendor_common::common;
mod auth;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_vendor_sdk::{
    AuthCallback, AuthCallbackPort, AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType,
    Capability, ChannelDescriptor, ConfigField, ConfigFieldKind, ConfigValidationResponse,
    DataCompatibility, DiscoverRequest, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost,
    NetworkDeclaration, Operation, OperationInput, OperationOutput, OriginDeclaration, PluginError,
    ProviderDescriptor, ProviderSnapshot, ValidationIssue,
};

const VENDOR_ID: &str = "anthropic";
const DEFAULT_CHANNEL: &str = "default";
const CLAUDE_CODE_CHANNEL: &str = "claude-code";
const MAX_ERROR_BODY: usize = 256 * 1024;
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;
const CLAUDE_CLI_USER_AGENT: &str = "claude-cli/2.1.222 (external, cli)";
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
const CLAUDE_CODE_MODELS: &[&str] = &[
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-4-5-20251101",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-20250514",
    "claude-opus-4-1-20250805",
    "claude-opus-4-20250514",
    "claude-haiku-4-5-20251001",
    "claude-3-5-haiku-20241022",
];

pub(crate) fn descriptor(vendor_id: &str) -> Option<ProviderDescriptor> {
    (vendor_id == VENDOR_ID).then(anthropic_descriptor)
}

fn anthropic_descriptor() -> ProviderDescriptor {
    let direct = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::ConfigValidation,
    ]);
    let claude_code = BTreeSet::from([
        Capability::Infer,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::ConfigValidation,
    ]);
    let capabilities = direct.union(&claude_code).copied().collect();
    ProviderDescriptor {
        provider_id: VENDOR_ID.into(),
        catalog_id: Some(VENDOR_ID.into()),
        display_name: "Anthropic".into(),
        description: Some("Anthropic Messages API and Claude Code OAuth channel.".into()),
        channels: vec![
            ChannelDescriptor {
                id: DEFAULT_CHANNEL.into(),
                name: "Anthropic API".into(),
                description: Some("Anthropic Messages API-key channel.".into()),
                auth: None,
                protocol: Some("anthropic-messages".into()),
                default_base_url: Some("https://api.anthropic.com".into()),
                default_models_source: None,
                capabilities: direct,
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            },
            ChannelDescriptor {
                id: CLAUDE_CODE_CHANNEL.into(),
                name: "Claude Code".into(),
                description: Some("Claude subscription OAuth channel.".into()),
                auth: Some(AuthDescriptor {
                    flow: AuthFlow::AuthorizationCode,
                    callback: Some(AuthCallback {
                        bind_host: "127.0.0.1".into(),
                        redirect_host: "localhost".into(),
                        path: "/callback".into(),
                        port: AuthCallbackPort::Dynamic,
                        manual_redirect_uri: Some(
                            "https://platform.claude.com/oauth/code/callback".into(),
                        ),
                        cancel_path: None,
                    }),
                    manual_input: Some(AuthManualInput {
                        input_type: AuthManualInputType::CallbackUrl,
                        label: "Callback URL".into(),
                        description: Some(
                            "Paste the full callback URL after completing authorization.".into(),
                        ),
                        secret: false,
                    }),
                }),
                protocol: Some("anthropic-messages".into()),
                default_base_url: Some("https://api.anthropic.com".into()),
                default_models_source: None,
                capabilities: claude_code,
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            },
        ],
        capabilities,
        config_fields: vec![ConfigField {
            key: "apiKey".into(),
            label: "API key".into(),
            description: Some("Anthropic API key for the default channel.".into()),
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
        }],
        network: NetworkDeclaration {
            base_url_field: None,
            extra_origins: vec![
                OriginDeclaration {
                    scheme: "https".into(),
                    host: "claude.com".into(),
                    port: None,
                },
                OriginDeclaration {
                    scheme: "https".into(),
                    host: "platform.claude.com".into(),
                    port: None,
                },
            ],
            field_origins: Vec::new(),
        },
        data_compat: DataCompatibility::default(),
    }
}

pub(crate) fn execute(
    vendor_id: &str,
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if vendor_id != VENDOR_ID {
        return Err(unsupported("vendor is not owned by the Anthropic module"));
    }
    if input.operation() != operation {
        return Err(invalid("operation input does not match operation"));
    }
    match (channel, input) {
        (DEFAULT_CHANNEL, OperationInput::Infer { provider, request }) => {
            infer(host, provider, request, false)
        }
        (DEFAULT_CHANNEL, OperationInput::Discover { provider, request }) => {
            discover(host, provider, request, false)
        }
        (
            DEFAULT_CHANNEL,
            OperationInput::ConfigValidation {
                provider: _,
                request,
            },
        ) => Ok(OperationOutput::ConfigValidation(
            ConfigValidationResponse {
                issues: validate_config(&request.options, false),
                proposed_base_url: None,
            },
        )),
        (CLAUDE_CODE_CHANNEL, OperationInput::Infer { provider, request }) => {
            infer(host, provider, request, true)
        }
        (CLAUDE_CODE_CHANNEL, OperationInput::Auth { provider, request }) => {
            auth::execute(host, &provider, request).map(OperationOutput::Auth)
        }
        (CLAUDE_CODE_CHANNEL, OperationInput::Discover { provider, request }) => {
            discover(host, provider, request, true)
        }
        (
            CLAUDE_CODE_CHANNEL,
            OperationInput::ConfigValidation {
                provider: _,
                request,
            },
        ) => Ok(OperationOutput::ConfigValidation(
            ConfigValidationResponse {
                issues: validate_config(&request.options, true),
                proposed_base_url: None,
            },
        )),
        (DEFAULT_CHANNEL | CLAUDE_CODE_CHANNEL, _) => Err(unsupported(
            "operation is not supported by this Anthropic channel",
        )),
        _ => Err(unsupported("unknown Anthropic channel")),
    }
}

fn infer(
    host: &GuestHost,
    provider: ProviderSnapshot,
    mut request: stravia_runtime_contract::protocol::ir::AiRequest,
    claude_code: bool,
) -> Result<OperationOutput, PluginError> {
    let model = required_model(&provider)?;
    request.model = model.into();
    let selected_protocol = ProtocolRegistry::global()
        .resolve_alias(&provider.protocol)
        .ok_or_else(|| unsupported("Anthropic provider protocol is not registered"))?;
    if selected_protocol != ANTHROPIC_MESSAGES_2023_06_01 {
        return Err(unsupported(
            "Anthropic requires the Anthropic Messages protocol",
        ));
    }
    let encoded =
        common::encode_inference_request(&ANTHROPIC_MESSAGES_2023_06_01.to_string(), &request)?;
    let body = encoded.body;
    let codec_headers = encoded.headers;
    let egress_path = encoded.path;
    let mut headers = common::header_pairs(&codec_headers)?;
    set_header(&mut headers, "content-type", "application/json".into());
    set_header(
        &mut headers,
        "accept",
        "text/event-stream, application/json".into(),
    );
    set_header(&mut headers, "anthropic-version", "2023-06-01".into());
    if claude_code {
        append_claude_code_headers(&provider, &mut headers)?;
    } else {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("x-api-key"));
        if let Some(api_key) = credential(&provider, "apiKey") {
            headers.push(("x-api-key".into(), api_key.into()));
        }
    }
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider.base_url, &egress_path),
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("Anthropic request could not be encoded"))?,
    })?;
    ensure_success(&response, "Anthropic inference")?;
    common::decode_ai_response(host, &ANTHROPIC_MESSAGES_2023_06_01.to_string(), response)
        .map(Box::new)
        .map(OperationOutput::Infer)
}

fn discover(
    host: &GuestHost,
    provider: ProviderSnapshot,
    request: DiscoverRequest,
    claude_code: bool,
) -> Result<OperationOutput, PluginError> {
    if claude_code {
        if request.cursor.is_some() {
            return Err(unsupported(
                "Claude Code's curated model inventory is not paginated",
            ));
        }
        return Ok(OperationOutput::Discover(DiscoverResponse {
            models: CLAUDE_CODE_MODELS
                .iter()
                .map(|id| DiscoveredModel {
                    id: (*id).into(),
                    display_name: human_model_name(id),
                    family: Some("claude".into()),
                    selector: None,
                    capabilities: Vec::new(),
                    metadata: BTreeMap::new(),
                })
                .collect(),
            next_cursor: None,
        }));
    }
    let mut url = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != "catalog")
        .map(str::to_owned)
        .unwrap_or_else(|| endpoint(&provider.base_url, "/v1/models"));
    if let Some(cursor) = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("after_id=");
        url.push_str(&percent_encode(cursor));
    }
    let mut headers = vec![
        ("accept".into(), "application/json".into()),
        ("anthropic-version".into(), "2023-06-01".into()),
    ];
    if let Some(api_key) = credential(&provider, "apiKey") {
        headers.push(("x-api-key".into(), api_key.into()));
    }
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "GET".into(),
        url,
        headers,
        body: Vec::new(),
    })?;
    ensure_success(&response, "Anthropic model discovery")?;
    let bytes = stravia_vendor_sdk::read_http_body(&response, MAX_MODELS_BODY)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| retryable("Anthropic model discovery returned invalid JSON"))?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| retryable("Anthropic model discovery response has no model list"))?;
    let models = entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.trim();
            if id.is_empty() {
                return None;
            }
            let display_name = entry
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_owned();
            let metadata = entry
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let capabilities = entry
                .get("capabilities")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            Some(DiscoveredModel {
                id: id.into(),
                display_name,
                family: entry
                    .get("family")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Some("claude".into())),
                selector: entry
                    .get("selector")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                capabilities,
                metadata,
            })
        })
        .collect();
    let next_cursor = value
        .get("has_more")
        .and_then(Value::as_bool)
        .filter(|has_more| *has_more)
        .and_then(|_| value.get("last_id").and_then(Value::as_str))
        .map(str::to_owned);
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor,
    }))
}

fn validate_config(options: &BTreeMap<String, Value>, claude_code: bool) -> Vec<ValidationIssue> {
    if claude_code || options.get("apiKey").is_none() {
        Vec::new()
    } else {
        vec![ValidationIssue {
            field: Some("apiKey".into()),
            code: "secret_in_options".into(),
            message: "API keys must be stored as credentials, not options.".into(),
        }]
    }
}

pub(super) fn append_claude_code_headers(
    provider: &ProviderSnapshot,
    headers: &mut Vec<(String, String)>,
) -> Result<(), PluginError> {
    set_header(
        headers,
        "authorization",
        format!("Bearer {}", secret(provider, "access_token")?),
    );
    set_header(headers, "user-agent", CLAUDE_CLI_USER_AGENT.into());
    set_header(headers, "referer", "https://claude.ai/".into());
    set_header(headers, "origin", "https://claude.ai".into());
    set_header(headers, "anthropic-beta", ANTHROPIC_OAUTH_BETA.into());
    set_header(headers, "anthropic-version", "2023-06-01".into());
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("x-api-key"));
    Ok(())
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.into(), value));
}

fn endpoint(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn human_model_name(id: &str) -> String {
    id.split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn required_model(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "*")
        .ok_or_else(|| invalid("Anthropic inference requires an upstream model"))
}

fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn secret<'a>(
    provider: &'a ProviderSnapshot,
    key: &str,
) -> Result<&'a str, PluginError> {
    credential(provider, key).ok_or_else(|| auth_error(format!("missing credential `{key}`")))
}

fn ensure_success(
    response: &stravia_vendor_sdk::HttpResponse,
    label: &str,
) -> Result<u16, PluginError> {
    let status = response.status()?;
    if (200..300).contains(&status) {
        return Ok(status);
    }
    let headers = response.headers()?;
    let bytes = stravia_vendor_sdk::read_http_body(response, MAX_ERROR_BODY)?;
    let mut error = common::upstream_error(status, &headers, &bytes);
    error.message = format!("{label} failed: {}", error.message);
    Err(error)
}

pub(super) fn invalid(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Invalid,
        message: message.into(),
        upstream_status: None,
    }
}

pub(super) fn auth_error(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Auth,
        message: message.into(),
        upstream_status: None,
    }
}

fn retryable(message: impl Into<String>) -> PluginError {
    common::model_error(
        stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError,
        message,
    )
}

pub(super) fn unsupported(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Unsupported,
        message: message.into(),
        upstream_status: None,
    }
}
