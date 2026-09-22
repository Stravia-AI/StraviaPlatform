mod allowance;
mod auth;
mod codex;
mod media_generation;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::{common, thinking};
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    AuthCallback, AuthCallbackPort, AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType,
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigValidationResponse, DataCompatibility, DiscoverRequest, DiscoverResponse,
    DiscoveredModel, ErrorKind, GuestHost, NetworkDeclaration, Operation, OperationInput,
    OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor, ProviderSnapshot,
    TransportPreference, ValidationIssue, VendorDescriptor, VendorKind,
};

const VENDOR_ID: &str = "openai-codex";
const CATALOG_ID: &str = "openai";
const CHANNEL: &str = "codex";
const MAX_ERROR_BODY: usize = 256 * 1024;
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::Compact,
        Capability::Search,
        Capability::MediaImage,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::ConfigValidation,
    ]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "OpenAI Codex".into(),
        description: Some(
            "ChatGPT-backed Codex OAuth, Responses, hosted search, image generation, and usage."
                .into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: Some(CATALOG_ID.into()),
            display_name: "OpenAI Codex".into(),
            description: Some(
                "ChatGPT OAuth channel using Open Responses, hosted search, and image generation."
                    .into(),
            ),
            channels: vec![ChannelDescriptor {
                id: CHANNEL.into(),
                name: "Codex".into(),
                description: Some(
                    "ChatGPT OAuth channel using Open Responses, hosted search, and image generation."
                        .into(),
                ),
                auth: Some(AuthDescriptor {
                    flow: AuthFlow::AuthorizationCode,
                    callback: Some(AuthCallback {
                        bind_host: "127.0.0.1".into(),
                        redirect_host: "localhost".into(),
                        path: "/auth/callback".into(),
                        port: AuthCallbackPort::Fixed {
                            primary: 1455,
                            fallback: Some(1456),
                        },
                        manual_redirect_uri: Some(
                            "http://localhost:1457/auth/callback".into(),
                        ),
                        cancel_path: Some("/cancel".into()),
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
                protocol: Some("open-responses".into()),
                default_base_url: Some("https://chatgpt.com/backend-api/codex".into()),
                default_models_source: None,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
                search_model_required: true,
            }],
            capabilities,
            config_fields: vec![ConfigField {
                key: "websocket_url".into(),
                label: "Responses WebSocket URL".into(),
                description: Some(
                    "Optional full ws:// or wss:// Responses endpoint for a custom base URL."
                        .into(),
                ),
                kind: ConfigFieldKind::String { multiline: false },
                required: false,
                default_json: None,
                group: Some("Advanced".into()),
                secret: false,
                min: None,
                max: None,
                max_length: Some(4096),
                pattern: Some(r"^wss?://.+".into()),
                visible_when: None,
            }],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: vec![
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "auth.openai.com".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "chatgpt.com".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "wss".into(),
                        host: "chatgpt.com".into(),
                        port: None,
                    },
                ],
                field_origins: vec!["websocket_url".into()],
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}

pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    require_route(channel, provider)?;
    if matches!(operation, Operation::Infer | Operation::Compact) {
        Ok(OPEN_RESPONSES_2026_04_24.to_string())
    } else {
        Ok(provider.protocol.clone())
    }
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if input.operation() != operation {
        return Err(invalid("operation input does not match operation"));
    }
    require_route(channel, input.provider())?;
    match input {
        OperationInput::Infer { provider, request } => {
            infer(host, provider, request, Operation::Infer)
        }
        OperationInput::Compact { provider, request } => {
            infer(host, provider, request, Operation::Compact)
        }
        OperationInput::Search { provider, request } => {
            codex::search(host, &provider, request).map(OperationOutput::Search)
        }
        OperationInput::MediaImage { provider, request } => {
            media_generation::generate(host, &provider, request).map(OperationOutput::MediaImage)
        }
        OperationInput::Auth { provider, request } => {
            auth::execute(host, &provider, request).map(OperationOutput::Auth)
        }
        OperationInput::Discover { provider, request } => discover(host, provider, request),
        OperationInput::Allowance {
            provider,
            request: _,
        } => allowance::execute(host, &provider).map(OperationOutput::Allowance),
        OperationInput::ConfigValidation {
            provider: _,
            request,
        } => Ok(OperationOutput::ConfigValidation(
            ConfigValidationResponse {
                issues: validate_config(&request.options),
                proposed_base_url: None,
            },
        )),
    }
}

fn require_route(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != VENDOR_ID {
        return Err(unsupported(format!(
            "{VENDOR_ID} cannot execute provider `{}`",
            provider.provider_id
        )));
    }
    if channel != CHANNEL || provider.channel != CHANNEL {
        return Err(unsupported(format!(
            "{VENDOR_ID} only supports the `{CHANNEL}` channel"
        )));
    }
    Ok(())
}

fn infer(
    host: &GuestHost,
    provider: ProviderSnapshot,
    mut request: AiRequest,
    operation: Operation,
) -> Result<OperationOutput, PluginError> {
    let model = required_model(&provider)?;
    request.model = model.to_owned();
    require_codex_protocol(&provider)?;
    for item in &mut request.items {
        if item.role == stravia_runtime_contract::protocol::ir::Role::System {
            item.role = stravia_runtime_contract::protocol::ir::Role::Developer;
        }
    }
    let extension = request.ext.get_or_insert_with(|| {
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(Default::default())
    });
    if let stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(ext) = extension {
        ext.store = Some(false);
    }

    let protocol = OPEN_RESPONSES_2026_04_24;
    let preserve_upstream_errors = operation == Operation::Infer
        && stravia_protocol_codec::codec::compaction::native_compaction_requested(&request);
    let encoded = common::encode_inference_request(&protocol.to_string(), &request)?;
    let mut body = encoded.body;
    let codec_headers = encoded.headers;
    let egress_path = encoded.path;
    codex::prepare_body(&mut body)?;
    prepare_responses_body(&provider, &mut body)?;
    if operation == Operation::Infer
        && provider.transport_preference() != TransportPreference::HttpOnly
        && codex::websocket_available(&provider)
    {
        return codex::infer_websocket(host, &provider, &body, preserve_upstream_errors)
            .map(Box::new)
            .map(OperationOutput::Infer);
    }
    if operation == Operation::Compact {
        retain_compact_fields(&request, &mut body)?;
    }
    let path = if operation == Operation::Compact {
        "/responses/compact".to_owned()
    } else {
        strip_v1_prefix(&egress_path).to_owned()
    };
    let mut headers = common::header_pairs(&codec_headers)?;
    headers.retain(|(name, _)| {
        !name.eq_ignore_ascii_case("content-type") && !name.eq_ignore_ascii_case("accept")
    });
    headers.push(("content-type".into(), "application/json".into()));
    headers.push((
        "accept".into(),
        "text/event-stream, application/json".into(),
    ));
    codex::append_runtime_headers(&provider, &body, &mut headers)?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider.base_url, &path),
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("Codex request could not be encoded"))?,
    })?;
    match operation {
        Operation::Infer => {
            let decoded = if preserve_upstream_errors {
                common::decode_ai_response_preserving_upstream_errors(
                    host,
                    &protocol.to_string(),
                    response,
                    classify_responses_stream_error,
                )
            } else {
                ensure_success(&response, "Codex inference")?;
                common::decode_ai_response_with_error_classifier(
                    host,
                    &protocol.to_string(),
                    response,
                    classify_responses_stream_error,
                )
            };
            decoded.map(Box::new).map(OperationOutput::Infer)
        }
        Operation::Compact => common::decode_compaction_preserving_upstream_errors(host, response)
            .map(OperationOutput::Compact),
        _ => Err(invalid(
            "Codex codec returned an unexpected operation result",
        )),
    }
}

fn prepare_responses_body(
    provider: &ProviderSnapshot,
    body: &mut Value,
) -> Result<(), PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    let include = object
        .entry("include")
        .or_insert_with(|| Value::Array(Vec::new()));
    if include.is_null() {
        *include = Value::Array(Vec::new());
    }
    let include = include
        .as_array_mut()
        .ok_or_else(|| invalid("Codex Responses include must be an array"))?;
    if !include
        .iter()
        .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
    {
        include.push(Value::String("reasoning.encrypted_content".into()));
    }
    if !object.contains_key("prompt_cache_key")
        && let Some(affinity) = provider
            .operation_metadata
            .get("transport_affinity")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        object.insert(
            "prompt_cache_key".into(),
            Value::String(affinity.to_owned()),
        );
    }
    Ok(())
}

fn retain_compact_fields(request: &AiRequest, body: &mut Value) -> Result<(), PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Open Responses compact request must be an object"))?;
    let explicit = request
        .meta
        .vendor
        .ingress
        .get("__stravia_compact_fields")
        .and_then(Value::as_array);
    object.retain(|key, _| {
        matches!(key.as_str(), "model" | "input" | "instructions")
            || explicit.is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field.as_str() == Some(key.as_str()))
            })
    });
    Ok(())
}

fn discover(
    host: &GuestHost,
    provider: ProviderSnapshot,
    request: DiscoverRequest,
) -> Result<OperationOutput, PluginError> {
    if let Some(response) = explicit_discovery(&provider)? {
        return Ok(OperationOutput::Discover(response));
    }
    if request.cursor.is_some() {
        return Err(unsupported(
            "Codex model discovery does not support cursor continuation",
        ));
    }
    require_codex_protocol(&provider)?;
    let mut url = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != "catalog")
        .map(str::to_owned)
        .unwrap_or_else(|| endpoint(&provider.base_url, "/models"));
    url.push(if url.contains('?') { '&' } else { '?' });
    url.push_str("client_version=0.153.0");
    let mut headers = vec![("accept".into(), "application/json".into())];
    codex::append_identity_headers(&provider, &mut headers)?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "GET".into(),
        url,
        headers,
        body: Vec::new(),
    })?;
    ensure_success(&response, "Codex model discovery")?;
    let body = stravia_vendor_sdk::read_http_body(&response, MAX_MODELS_BODY)?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| retryable("Codex model discovery returned invalid JSON"))?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| retryable("Codex model discovery response has no model list"))?;
    let models = entries
        .iter()
        .filter_map(discovered_model)
        .collect::<Vec<_>>();
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: None,
    }))
}

fn explicit_discovery(
    provider: &ProviderSnapshot,
) -> Result<Option<DiscoverResponse>, PluginError> {
    let Some(static_models) = provider.operation_metadata.get("static_models") else {
        return Ok(None);
    };
    let values = static_models
        .as_array()
        .ok_or_else(|| invalid("operation_metadata.static_models must be a JSON array"))?;
    let mut models = BTreeMap::new();
    for (index, value) in values.iter().enumerate() {
        let id = value.as_str().ok_or_else(|| {
            invalid(format!(
                "operation_metadata.static_models[{index}] must be a string"
            ))
        })?;
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let mut model = DiscoveredModel {
            id: id.to_owned(),
            display_name: id.to_owned(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        };
        decorate_discovered_model(&mut model);
        models.entry(id.to_owned()).or_insert(model);
    }
    Ok(Some(DiscoverResponse {
        models: models.into_values().collect(),
        next_cursor: None,
    }))
}

fn discovered_model(entry: &Value) -> Option<DiscoveredModel> {
    if entry
        .get("visibility")
        .and_then(Value::as_str)
        .is_some_and(|visibility| !visibility.eq_ignore_ascii_case("list"))
    {
        return None;
    }
    let id = entry
        .get("id")
        .or_else(|| entry.get("slug"))
        .or_else(|| entry.get("name"))
        .and_then(Value::as_str)?
        .trim();
    if id.is_empty() {
        return None;
    }
    let mut model = DiscoveredModel {
        id: id.to_owned(),
        display_name: entry
            .get("display_name")
            .or_else(|| entry.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned(),
        family: entry
            .get("family")
            .and_then(Value::as_str)
            .map(str::to_owned),
        selector: entry
            .get("selector")
            .and_then(Value::as_str)
            .map(str::to_owned),
        capabilities: entry
            .get("capabilities")
            .or_else(|| entry.get("supported_endpoints"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        metadata: entry
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    };
    decorate_discovered_model(&mut model);
    Some(model)
}

fn decorate_discovered_model(model: &mut DiscoveredModel) {
    let has_supplier_capabilities = model
        .capabilities
        .iter()
        .any(|capability| !capability.trim().is_empty());
    thinking::decorate_discovered_model(CATALOG_ID, model);
    if has_supplier_capabilities {
        return;
    }

    model.capabilities.push(Capability::Infer.as_str().into());
    model.capabilities.push(Capability::Search.as_str().into());
    if media_generation::supported_model(&model.id) {
        model
            .capabilities
            .push(Capability::MediaImage.as_str().into());
    }
}

fn validate_config(options: &BTreeMap<String, Value>) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if let Some(value) = options.get("websocket_url")
        && !value.as_str().is_some_and(|value| {
            value.trim().is_empty()
                || url::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "ws" | "wss"))
        })
    {
        issues.push(ValidationIssue {
            field: Some("websocket_url".into()),
            code: "invalid_websocket_url".into(),
            message: "Responses WebSocket URL must be an absolute ws:// or wss:// URL.".into(),
        });
    }
    issues
}

pub(crate) fn secret<'a>(
    provider: &'a ProviderSnapshot,
    key: &str,
) -> Result<&'a str, PluginError> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error(format!("missing credential `{key}`")))
}

pub(crate) fn configured_websocket_url(provider: &ProviderSnapshot) -> Option<&str> {
    provider
        .options
        .get("websocket_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| value.starts_with("ws://") || value.starts_with("wss://"))
}

pub(crate) fn require_codex_protocol(provider: &ProviderSnapshot) -> Result<(), PluginError> {
    let protocol = stravia_protocol_codec::registry::ProtocolRegistry::global()
        .resolve_alias(&provider.protocol)
        .ok_or_else(|| unsupported("Codex provider protocol is not registered"))?;
    if protocol != OPEN_RESPONSES_2026_04_24 {
        return Err(unsupported("Codex requires the Open Responses protocol"));
    }
    Ok(())
}

pub(crate) fn required_model(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "*")
        .ok_or_else(|| invalid("Codex operation requires an upstream model"))
}

pub(crate) fn endpoint(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let mut path = path.trim_start_matches('/');
    if base.ends_with("/v1") {
        path = path.strip_prefix("v1/").unwrap_or(path);
    }
    format!("{base}/{path}")
}

fn strip_v1_prefix(path: &str) -> &str {
    path.strip_prefix("/v1/")
        .unwrap_or(path)
        .trim_start_matches('/')
}

pub(crate) fn ensure_success(
    response: &stravia_vendor_sdk::HttpResponse,
    label: &str,
) -> Result<u16, PluginError> {
    let status = response.status()?;
    if (200..300).contains(&status) {
        return Ok(status);
    }
    let headers = response.headers()?;
    let body = stravia_vendor_sdk::read_http_body(response, MAX_ERROR_BODY)?;
    if protected_reasoning_rejected(&body) {
        return Err(PluginError {
            kind: ErrorKind::ProtectedReasoningRejected,
            message: "Codex rejected protected reasoning replay".into(),
            upstream_status: Some(status),
        });
    }
    let mut error = common::upstream_error(status, &headers, &body);
    error.message = format!("{label} failed: {}", error.message);
    Err(error)
}

fn protected_reasoning_rejected(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .as_ref()
        .is_some_and(protected_reasoning_rejection_event)
}

fn protected_reasoning_rejection_event(value: &Value) -> bool {
    value
        .pointer("/error/code")
        .or_else(|| value.pointer("/response/error/code"))
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        == Some("invalid_encrypted_content")
}

pub(crate) fn classify_responses_stream_error(
    value: &Value,
    saw_response_event: bool,
) -> Option<PluginError> {
    (!saw_response_event && protected_reasoning_rejection_event(value)).then(|| PluginError {
        kind: ErrorKind::ProtectedReasoningRejected,
        message: "Codex rejected protected reasoning replay".into(),
        upstream_status: None,
    })
}

pub(crate) fn invalid(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Invalid,
        message: message.into(),
        upstream_status: None,
    }
}

pub(crate) fn auth_error(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Auth,
        message: message.into(),
        upstream_status: None,
    }
}

pub(crate) fn retryable(message: impl Into<String>) -> PluginError {
    common::model_error(
        stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError,
        message,
    )
}

pub(crate) fn unsupported(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Unsupported,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(target_arch = "wasm32")]
struct CodexVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for CodexVendor {
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
stravia_vendor_sdk::export_vendor!(CodexVendor);
