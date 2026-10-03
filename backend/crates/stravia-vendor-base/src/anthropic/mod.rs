use stravia_vendor_common::common;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_vendor_sdk::{
    Capability, ChannelDescriptor, ConfigField, ConfigFieldKind, ConfigGroup,
    ConfigValidationResponse, DataCompatibility, DiscoverRequest, DiscoverResponse,
    DiscoveredModel, ErrorKind, GuestHost, MODELS_SOURCE_CATALOG, NetworkDeclaration, Operation,
    OperationInput, OperationOutput, PluginError, ProviderDescriptor, ProviderSnapshot,
    ValidationIssue,
};

const VENDOR_ID: &str = "anthropic";
const DEFAULT_CHANNEL: &str = "default";
const MAX_ERROR_BODY: usize = 256 * 1024;
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

pub(crate) fn descriptor(vendor_id: &str) -> Option<ProviderDescriptor> {
    (vendor_id == VENDOR_ID).then(anthropic_descriptor)
}

fn anthropic_descriptor() -> ProviderDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::ConfigValidation,
    ]);
    ProviderDescriptor {
        provider_id: VENDOR_ID.into(),
        catalog_id: Some(VENDOR_ID.into()),
        display_name: "Anthropic".into(),
        description: Some("Anthropic Messages API.".into()),
        channels: vec![ChannelDescriptor {
            id: DEFAULT_CHANNEL.into(),
            name: crate::messages::anthropic_api_channel(),
            description: Some(crate::messages::anthropic_api_channel_description()),
            auth: None,
            protocol: Some("anthropic-messages".into()),
            protocols: Vec::new(),
            default_base_url: Some("https://api.anthropic.com".into()),
            default_models_source: None,
            consumes_catalog_models: false,
            capabilities: capabilities.clone(),
            model_capabilities: BTreeSet::new(),
        }],
        capabilities,
        website: None,
        implementation: None,
        config_groups: vec![ConfigGroup {
            id: "authentication".into(),
            label: crate::messages::authentication_group(),
        }],
        config_fields: vec![ConfigField {
            key: "apiKey".into(),
            label: crate::messages::api_key(),
            description: Some(crate::messages::anthropic_api_key_description()),
            kind: ConfigFieldKind::String { multiline: false },
            required: false,
            default_json: None,
            group: Some("authentication".into()),
            secret: true,
            min: None,
            max: None,
            max_length: Some(8192),
            pattern: None,
            visible_when: None,
        }],
        network: NetworkDeclaration {
            base_url_field: None,
            extra_origins: Vec::new(),
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
            infer(host, provider, request)
        }
        (DEFAULT_CHANNEL, OperationInput::Discover { provider, request }) => {
            discover(host, provider, request)
        }
        (
            DEFAULT_CHANNEL,
            OperationInput::ConfigValidation {
                provider: _,
                request,
            },
        ) => Ok(OperationOutput::ConfigValidation(
            ConfigValidationResponse {
                issues: validate_config(&request.options),
                proposed_base_url: None,
            },
        )),
        (DEFAULT_CHANNEL, _) => Err(unsupported(
            "operation is not supported by this Anthropic channel",
        )),
        _ => Err(unsupported("unknown Anthropic channel")),
    }
}

fn infer(
    host: &GuestHost,
    provider: ProviderSnapshot,
    mut request: stravia_runtime_contract::protocol::ir::AiRequest,
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
    crate::apply_session_affinity(&provider, &mut headers);
    set_header(&mut headers, "content-type", "application/json".into());
    set_header(
        &mut headers,
        "accept",
        "text/event-stream, application/json".into(),
    );
    set_header(&mut headers, "anthropic-version", "2023-06-01".into());
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("x-api-key"));
    if let Some(api_key) = credential(&provider, "apiKey") {
        headers.push(("x-api-key".into(), api_key.into()));
    }
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider.base_url, &egress_path),
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("Anthropic request could not be encoded"))?,
    })?;
    ensure_success(
        &response,
        "Anthropic inference",
        Some(crate::generic::classify_anthropic_error),
    )?;
    common::decode_ai_response_with_error_classifier(
        host,
        &ANTHROPIC_MESSAGES_2023_06_01.to_string(),
        response,
        crate::generic::classify_anthropic_error,
    )
    .map(Box::new)
    .map(OperationOutput::Infer)
}

fn discover(
    host: &GuestHost,
    provider: ProviderSnapshot,
    request: DiscoverRequest,
) -> Result<OperationOutput, PluginError> {
    let mut url = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != MODELS_SOURCE_CATALOG)
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
    ensure_success(&response, "Anthropic model discovery", None)?;
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
                .map(stravia_vendor_common::thinking::source_metadata)
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

fn validate_config(options: &BTreeMap<String, Value>) -> Vec<ValidationIssue> {
    if options.get("apiKey").is_none() {
        Vec::new()
    } else {
        vec![ValidationIssue {
            field: Some("apiKey".into()),
            code: "secret_in_options".into(),
            message: crate::messages::secret_in_options(),
        }]
    }
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

fn ensure_success(
    response: &stravia_vendor_sdk::HttpResponse,
    label: &str,
    classify: Option<fn(&Value, bool) -> Option<PluginError>>,
) -> Result<u16, PluginError> {
    let status = response.status()?;
    if (200..300).contains(&status) {
        return Ok(status);
    }
    let headers = response.headers()?;
    let bytes = stravia_vendor_sdk::read_http_body(response, MAX_ERROR_BODY)?;
    // 受保护推理回放被拒是可恢复错误：交给分类器标记为
    // ProtectedReasoningRejected，宿主剥离签名/密文后重试。
    if let Some(classify) = classify
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(mut error) = classify(&value, false)
    {
        error.upstream_status.get_or_insert(status);
        return Err(error);
    }
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
