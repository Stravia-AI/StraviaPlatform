use std::collections::{BTreeMap, BTreeSet};
use stravia_vendor_common::common;

use serde_json::Value;
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_protocol_codec::transform::{ProtocolTransform, prepare_thinking_replay};
use stravia_runtime_contract::protocol::ids::{
    OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    OPENAI_COMPATIBLE_EMBEDDINGS_V1,
};
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_sdk::{
    Capability, ChannelDescriptor, ConfigField, ConfigFieldKind, ConfigGroup,
    ConfigValidationResponse, DataCompatibility, DiscoverRequest, DiscoverResponse,
    DiscoveredModel, ErrorKind, GuestHost, MODELS_SOURCE_CATALOG, NetworkDeclaration, Operation,
    OperationInput, OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor,
    ProviderSnapshot, TransportPreference, ValidationIssue,
};

const VENDOR_ID: &str = "openai";
const DEFAULT_CHANNEL: &str = "default";
const MAX_ERROR_BODY: usize = 256 * 1024;
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

pub(crate) fn descriptor(vendor_id: &str) -> Option<ProviderDescriptor> {
    (vendor_id == VENDOR_ID).then(openai_descriptor)
}

fn openai_descriptor() -> ProviderDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::Compact,
        Capability::ModelDiscovery,
        Capability::ConfigValidation,
    ]);
    ProviderDescriptor {
        provider_id: VENDOR_ID.into(),
        catalog_id: Some(VENDOR_ID.into()),
        display_name: "OpenAI".into(),
        description: Some("OpenAI API-key integration for ordinary API endpoints.".into()),
        channels: vec![ChannelDescriptor {
            id: DEFAULT_CHANNEL.into(),
            name: crate::messages::openai_api_channel(),
            description: Some(crate::messages::openai_api_channel_description()),
            auth: None,
            protocol: Some("openai-compatible".into()),
            protocols: Vec::new(),
            default_base_url: Some("https://api.openai.com/v1".into()),
            default_models_source: None,
            consumes_catalog_models: false,
            capabilities: capabilities.clone(),
            model_capabilities: BTreeSet::new(),
            search_model_required: false,
        }],
        capabilities,
        website: None,
        implementation: None,
        config_groups: vec![
            ConfigGroup {
                id: "authentication".into(),
                label: crate::messages::authentication_group(),
            },
            ConfigGroup {
                id: "advanced".into(),
                label: crate::messages::advanced_group(),
            },
        ],
        config_fields: vec![
            ConfigField {
                key: "apiKey".into(),
                label: crate::messages::api_key(),
                description: Some(crate::messages::openai_api_key_description()),
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
            },
            ConfigField {
                key: "websocket_url".into(),
                label: crate::messages::responses_websocket_url(),
                description: Some(crate::messages::responses_websocket_url_description()),
                kind: ConfigFieldKind::String { multiline: false },
                required: false,
                default_json: None,
                group: Some("advanced".into()),
                secret: false,
                min: None,
                max: None,
                max_length: Some(4096),
                pattern: Some(r"^wss?://.+".into()),
                visible_when: None,
            },
        ],
        network: NetworkDeclaration {
            base_url_field: None,
            extra_origins: vec![OriginDeclaration {
                scheme: "wss".into(),
                host: "api.openai.com".into(),
                port: None,
            }],
            field_origins: vec!["websocket_url".into()],
        },
        data_compat: DataCompatibility::default(),
    }
}

pub(crate) fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    request: &AiRequest,
) -> Result<String, PluginError> {
    if !matches!(operation, Operation::Infer | Operation::Compact) || channel != DEFAULT_CHANNEL {
        return Ok(provider.protocol.clone());
    }
    if request.embedding.is_some() {
        return Ok(
            if ProtocolRegistry::global()
                .resolve_alias(&provider.protocol)
                .is_some_and(|configured| {
                    configured.protocol == OPENAI_COMPATIBLE_EMBEDDINGS_V1.protocol
                })
            {
                OPENAI_COMPATIBLE_EMBEDDINGS_V1.to_string()
            } else {
                provider.protocol.clone()
            },
        );
    }

    let ingress = ProtocolTransform::inferred_ingress(request).unwrap_or(OPEN_RESPONSES_2026_04_24);
    let Ok(pair) = ProtocolTransform::global().bind(ingress, OPEN_RESPONSES_2026_04_24) else {
        return Ok(provider.protocol.clone());
    };
    if pair.encode_request(request).is_ok() {
        return Ok(OPEN_RESPONSES_2026_04_24.to_string());
    }

    // Protocol choice runs before Core prepares Target-owned thinking replay. Probe only the
    // codec's explicit thinking downgrade on a copy; its strict loss audit still rejects every
    // unrelated hard requirement, and the canonical request remains untouched for execution.
    let mut probe = request.clone();
    if prepare_thinking_replay(&mut probe, OPEN_RESPONSES_2026_04_24, |_| false)
        && pair.encode_request(&probe).is_ok()
    {
        return Ok(OPEN_RESPONSES_2026_04_24.to_string());
    }

    Ok(provider.protocol.clone())
}

pub(crate) fn execute(
    vendor_id: &str,
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if vendor_id != VENDOR_ID || channel != DEFAULT_CHANNEL {
        return Err(unsupported(
            "provider channel is not owned by the OpenAI API profile",
        ));
    }
    if input.operation() != operation {
        return Err(invalid("operation input does not match operation"));
    }
    match input {
        OperationInput::Infer { provider, request } => {
            infer(host, provider, request, Operation::Infer)
        }
        OperationInput::Compact { provider, request } => {
            infer(host, provider, request, Operation::Compact)
        }
        OperationInput::Discover { provider, request } => discover(host, provider, request),
        OperationInput::ConfigValidation {
            provider: _,
            request,
        } => Ok(OperationOutput::ConfigValidation(
            ConfigValidationResponse {
                issues: validate_config(&request.options),
                proposed_base_url: None,
            },
        )),
        _ => Err(unsupported(
            "operation is not supported by the OpenAI API channel",
        )),
    }
}

pub(crate) fn execute_compact(
    host: &GuestHost,
    provider: ProviderSnapshot,
    request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    infer(host, provider, request, Operation::Compact)
}

fn infer(
    host: &GuestHost,
    provider: ProviderSnapshot,
    mut request: AiRequest,
    operation: Operation,
) -> Result<OperationOutput, PluginError> {
    request.model = required_model(&provider)?.to_owned();
    let protocol = ProtocolRegistry::global()
        .resolve_alias(&provider.protocol)
        .ok_or_else(|| unsupported("OpenAI provider protocol is not registered"))?;
    if protocol != OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
        && protocol != OPENAI_COMPATIBLE_EMBEDDINGS_V1
        && protocol != OPEN_RESPONSES_2026_04_24
    {
        return Err(unsupported(
            "protocol is not supported by the OpenAI vendor",
        ));
    }
    if operation == Operation::Compact && protocol != OPEN_RESPONSES_2026_04_24 {
        return Err(unsupported(
            "native compaction requires the Open Responses protocol",
        ));
    }
    let preserve_upstream_errors = operation == Operation::Infer
        && protocol == OPEN_RESPONSES_2026_04_24
        && stravia_protocol_codec::codec::compaction::native_compaction_requested(&request);
    let encoded = common::encode_inference_request(&protocol.to_string(), &request)?;
    let mut body = encoded.body;
    if protocol == OPEN_RESPONSES_2026_04_24 {
        prepare_responses_body(&provider, &mut body)?;
    }
    if operation == Operation::Infer
        && provider.transport_preference() != TransportPreference::HttpOnly
        && protocol == OPEN_RESPONSES_2026_04_24
        && (official_openai_origin(&provider.base_url)
            || configured_websocket_url(&provider).is_some())
    {
        return infer_responses_websocket(
            host,
            &provider,
            &body,
            &encoded.headers,
            preserve_upstream_errors,
        )
        .map(Box::new)
        .map(OperationOutput::Infer);
    }
    if operation == Operation::Compact {
        retain_compact_fields(&request, &mut body)?;
    }
    let path = if operation == Operation::Compact {
        "/v1/responses/compact".to_owned()
    } else {
        encoded.path
    };
    let url = endpoint(&provider.base_url, &path);
    let mut headers = common::header_pairs(&encoded.headers)?;
    append_client_headers(&provider, &mut headers);
    set_header(&mut headers, "content-type", "application/json".into());
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("accept"))
    {
        headers.push((
            "accept".into(),
            "text/event-stream, application/json".into(),
        ));
    }
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    if let Some(api_key) = credential(&provider, "apiKey") {
        headers.push(("authorization".into(), format!("Bearer {api_key}")));
    }
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("OpenAI request could not be encoded"))?,
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
                ensure_success(&response, "OpenAI inference")?;
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
            "OpenAI codec returned an unexpected operation result",
        )),
    }
}

fn prepare_responses_body(
    provider: &ProviderSnapshot,
    body: &mut Value,
) -> Result<(), PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("OpenAI Responses request body must be an object"))?;
    let include = object
        .entry("include")
        .or_insert_with(|| Value::Array(Vec::new()));
    if include.is_null() {
        *include = Value::Array(Vec::new());
    }
    let include = include
        .as_array_mut()
        .ok_or_else(|| invalid("OpenAI Responses include must be an array"))?;
    if !include
        .iter()
        .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
    {
        include.push(Value::String("reasoning.encrypted_content".into()));
    }
    if object.get("store").and_then(Value::as_bool) == Some(false)
        && !object.contains_key("prompt_cache_key")
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

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.into(), value));
}

fn append_client_headers(provider: &ProviderSnapshot, headers: &mut Vec<(String, String)>) {
    for (name, value) in &provider.client_headers {
        set_header(headers, name, value.clone());
    }
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

fn infer_responses_websocket(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    body: &Value,
    codec_headers: &http::HeaderMap,
    preserve_upstream_errors: bool,
) -> Result<stravia_runtime_contract::protocol::ir::AiResponse, PluginError> {
    let mut headers = common::header_pairs(codec_headers)?;
    append_client_headers(provider, &mut headers);
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    if let Some(api_key) = credential(provider, "apiKey") {
        headers.push(("authorization".into(), format!("Bearer {api_key}")));
    }
    let url = if let Some(url) = configured_websocket_url(provider) {
        url.to_owned()
    } else {
        let url = endpoint(provider.base_url.as_str(), "/v1/responses");
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(invalid(
                "official OpenAI Responses WebSocket requires an https base URL",
            ));
        };
        format!("wss://{rest}")
    };
    let connection = host.ws_connect(stravia_vendor_sdk::WsRequest {
        url,
        headers,
        protocols: Vec::new(),
    })?;
    let mut frame = body.clone();
    let object = frame
        .as_object_mut()
        .ok_or_else(|| invalid("OpenAI Responses request body must be an object"))?;
    object.remove("stream");
    object.remove("background");
    object.insert("type".into(), Value::String("response.create".into()));
    host.emit_started()?;
    connection.send(&stravia_vendor_sdk::WsMessage::Text(
        serde_json::to_string(&frame)
            .map_err(|_| invalid("OpenAI WebSocket request could not be encoded"))?,
    ))?;
    common::decode_ai_response_websocket_with_error_classifier(
        host,
        &OPEN_RESPONSES_2026_04_24.to_string(),
        connection,
        preserve_upstream_errors,
        classify_responses_stream_error,
    )
}

fn classify_responses_stream_error(value: &Value, saw_response_event: bool) -> Option<PluginError> {
    (!saw_response_event && protected_reasoning_rejection_event(value)).then(|| PluginError {
        kind: ErrorKind::ProtectedReasoningRejected,
        message: "OpenAI rejected protected reasoning replay".into(),
        upstream_status: None,
    })
}

fn discover(
    host: &GuestHost,
    provider: ProviderSnapshot,
    request: DiscoverRequest,
) -> Result<OperationOutput, PluginError> {
    if request.cursor.is_some() {
        return Err(unsupported(
            "OpenAI model discovery does not support cursor continuation",
        ));
    }
    let url = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != MODELS_SOURCE_CATALOG)
        .map(str::to_owned)
        .map_or_else(|| common::model_discovery_url(&provider.base_url), Ok)?;
    let mut headers = vec![("accept".into(), "application/json".into())];
    if let Some(api_key) = credential(&provider, "apiKey") {
        headers.push(("authorization".into(), format!("Bearer {api_key}")));
    }
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "GET".into(),
        url,
        headers,
        body: Vec::new(),
    })?;
    ensure_success(&response, "OpenAI model discovery")?;
    let body = stravia_vendor_sdk::read_http_body(&response, MAX_MODELS_BODY)?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| retryable("OpenAI model discovery returned invalid JSON"))?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| retryable("OpenAI model discovery response has no model list"))?;
    let models = entries
        .iter()
        .filter_map(|entry| {
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
            let display_name = entry
                .get("display_name")
                .or_else(|| entry.get("name"))
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
                .or_else(|| entry.get("supported_endpoints"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            Some(DiscoveredModel {
                id: id.to_owned(),
                display_name,
                family: entry
                    .get("family")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                selector: entry
                    .get("selector")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                capabilities,
                metadata,
            })
        })
        .collect();
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: None,
    }))
}

fn validate_config(options: &BTreeMap<String, Value>) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if options.get("apiKey").is_some() {
        issues.push(ValidationIssue {
            field: Some("apiKey".into()),
            code: "secret_in_options".into(),
            message: crate::messages::secret_in_options(),
        });
    }
    if let Some(value) = options.get("websocket_url")
        && !value.as_str().is_some_and(|value| {
            value.trim().is_empty()
                || url::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "ws" | "wss"))
        })
    {
        issues.push(ValidationIssue {
            field: Some("websocket_url".into()),
            code: "invalid_websocket_url".into(),
            message: crate::messages::invalid_websocket_url(),
        });
    }
    issues
}

fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn configured_websocket_url(provider: &ProviderSnapshot) -> Option<&str> {
    provider
        .options
        .get("websocket_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| value.starts_with("ws://") || value.starts_with("wss://"))
}

fn official_openai_origin(base_url: &str) -> bool {
    url::Url::parse(base_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.openai.com")
            && url.port_or_known_default() == Some(443)
    })
}

pub(super) fn required_model(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "*")
        .ok_or_else(|| invalid("OpenAI operation requires an upstream model"))
}

pub(super) fn endpoint(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let mut path = path.trim_start_matches('/');
    if base.ends_with("/v1") {
        path = path.strip_prefix("v1/").unwrap_or(path);
    }
    format!("{base}/{path}")
}

pub(super) fn ensure_success(
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
            message: "OpenAI rejected protected reasoning replay".into(),
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

pub(super) fn invalid(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Invalid,
        message: message.into(),
        upstream_status: None,
    }
}

pub(super) fn retryable(message: impl Into<String>) -> PluginError {
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
