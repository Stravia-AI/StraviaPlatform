mod allowance;
mod codec;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::{ProtocolAdapter, ProtocolTransform, TextWireStreamParser};
use stravia_runtime_contract::protocol::ids::COMMAND_CODE_GENERATE_V1;
use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiRequest, AiStreamDelta};
use stravia_vendor_common::{common, thinking};
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, ConfigValidationResponse, DataCompatibility, DiscoverResponse, DiscoveredModel,
    ErrorKind, GuestHost, MODELS_SOURCE_CATALOG, NetworkDeclaration, Operation, OperationInput,
    OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor, ProviderSnapshot,
    ValidationIssue, VendorDescriptor, VendorKind, read_http_body,
};

use crate::codec::{CommandCodeGenerateV1, CommandCodeStreamParser, DEFAULT_WORKING_DIR};

const VENDOR_ID: &str = "command-code";
const CHANNEL_ID: &str = "default";
const PROTOCOL_VERSION: &str = "1.53.1";
const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";
const FP_SALT: &str = "command-code:device-fingerprint:v1";
const SESSION_TTL_MS: i64 = 12 * 60 * 60 * 1000;
const SESSION_JITTER_MS: u64 = 60 * 60 * 1000;
const INIT_TTL_MS: i64 = 8 * 60 * 60 * 1000;
const INIT_JITTER_MS: u64 = 2 * 60 * 60 * 1000;
const MAX_BODY: usize = 16 * 1024 * 1024;

const FINGERPRINT_CPUS: &[(&str, u8)] = &[
    ("12th Gen Intel(R) Core(TM) i7-12650H", 10),
    ("12th Gen Intel(R) Core(TM) i5-12400F", 6),
    ("12th Gen Intel(R) Core(TM) i9-12900K", 16),
    ("13th Gen Intel(R) Core(TM) i7-13700K", 16),
    ("13th Gen Intel(R) Core(TM) i5-13600K", 14),
    ("13th Gen Intel(R) Core(TM) i9-13900K", 24),
    ("Intel(R) Core(TM) Ultra 7 155H", 16),
    ("Intel(R) Core(TM) Ultra 9 285H", 16),
    ("Intel(R) Core(TM) i9-14900K", 24),
    ("Intel(R) Core(TM) i7-14700K", 20),
    ("AMD Ryzen 7 7800X3D", 8),
    ("AMD Ryzen 9 7950X", 16),
    ("AMD Ryzen 5 7600", 6),
    ("AMD Ryzen 9 7900X", 12),
    ("AMD Ryzen 7 5800X3D", 8),
];
const FINGERPRINT_MEMS: &[u8] = &[8, 16, 24, 32, 48, 64];
const FINGERPRINT_TZS: &[&str] = &[
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "America/Toronto",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Paris",
    "Europe/Moscow",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Singapore",
    "Asia/Seoul",
    "Asia/Hong_Kong",
    "Australia/Sydney",
    "Pacific/Auckland",
];
const FINGERPRINT_MAC_COUNTS: &[usize] = &[2, 3, 4, 5];
const FP_OS_USERS: &[&str] = &["dev", "user", "admin", "coder", "engineer", "work"];
const FP_MAIL_DOMAINS: &[&str] = &["gmail.com", "outlook.com", "qq.com", "163.com"];

#[derive(Debug, Default, Serialize, Deserialize)]
struct PrivateState {
    session_id: Option<String>,
    session_expires_at_ms: Option<i64>,
    fingerprint: Option<Value>,
    next_init_at_ms: Option<i64>,
}

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::ConfigValidation,
    ]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "Command Code".into(),
        description: Some("Command Code CLI-compatible provider integration".into()),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: Some(VENDOR_ID.into()),
            display_name: "Command Code".into(),
            description: Some(
                "Command Code CLI-compatible inference, model discovery, and allowance".into(),
            ),
            channels: vec![ChannelDescriptor {
                id: CHANNEL_ID.into(),
                name: crate::messages::channel_default(),
                description: Some(crate::messages::channel_description()),
                auth: None,
                protocol: Some("command-code".into()),
                protocols: Vec::new(),
                default_base_url: Some(DEFAULT_BASE_URL.into()),
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
                    label: crate::messages::authentication(),
                },
                ConfigGroup {
                    id: "privacy".into(),
                    label: crate::messages::privacy(),
                },
            ],
            config_fields: vec![
                ConfigField {
                    key: "apiKey".into(),
                    label: crate::messages::api_key(),
                    description: Some(crate::messages::api_key_description()),
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
                },
                ConfigField {
                    key: "zdr".into(),
                    label: crate::messages::zdr(),
                    description: Some(crate::messages::zdr_description()),
                    kind: ConfigFieldKind::Bool,
                    required: false,
                    default_json: Some(Value::Bool(true)),
                    group: Some("privacy".into()),
                    secret: false,
                    min: None,
                    max: None,
                    max_length: None,
                    pattern: None,
                    visible_when: None,
                },
            ],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: vec![OriginDeclaration {
                    scheme: "https".into(),
                    host: "api.commandcode.ai".into(),
                    port: None,
                }],
                field_origins: Vec::new(),
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
    _request: &AiRequest,
) -> Result<String, PluginError> {
    check_provider(channel, provider)?;
    if operation != Operation::Infer {
        return Err(unsupported(operation));
    }
    Ok(COMMAND_CODE_GENERATE_V1.to_string())
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if input.operation() != operation {
        return Err(error(
            ErrorKind::Invalid,
            "operation kind does not match typed operation input",
        ));
    }
    check_provider(channel, input.provider())?;
    match (operation, input) {
        (Operation::Infer, OperationInput::Infer { provider, request }) => {
            infer(host, provider, request)
        }
        (Operation::Discover, OperationInput::Discover { provider, .. }) => {
            discover(host, &provider).map(OperationOutput::Discover)
        }
        (Operation::Allowance, OperationInput::Allowance { provider, .. }) => {
            allowance::execute(host, &provider).map(OperationOutput::Allowance)
        }
        (
            Operation::ConfigValidation,
            OperationInput::ConfigValidation {
                provider: _,
                request,
            },
        ) => Ok(OperationOutput::ConfigValidation(validate_config(
            &request.options,
        ))),
        (operation, _) => Err(unsupported(operation)),
    }
}

fn check_provider(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != VENDOR_ID {
        return Err(error(
            ErrorKind::Unsupported,
            format!(
                "Command Code guest cannot execute provider `{}`",
                provider.provider_id
            ),
        ));
    }
    if channel != CHANNEL_ID || provider.channel != CHANNEL_ID {
        return Err(error(
            ErrorKind::Unsupported,
            "unsupported Command Code channel",
        ));
    }
    Ok(())
}

fn unsupported(operation: Operation) -> PluginError {
    error(
        ErrorKind::Unsupported,
        format!("Command Code does not implement {}", operation.as_str()),
    )
}

fn validate_config(options: &BTreeMap<String, Value>) -> ConfigValidationResponse {
    let mut issues = Vec::new();
    if let Some(value) = options.get("zdr")
        && !value.is_boolean()
    {
        issues.push(ValidationIssue {
            field: Some("zdr".into()),
            code: "invalid_type".into(),
            message: crate::messages::invalid_zdr_type(),
        });
    }
    if let Some(value) = options.get("apiKey")
        && value.as_str().map(str::trim).is_none_or(str::is_empty)
    {
        issues.push(ValidationIssue {
            field: Some("apiKey".into()),
            code: "invalid_value".into(),
            message: crate::messages::invalid_api_key(),
        });
    }
    ConfigValidationResponse {
        issues,
        proposed_base_url: None,
    }
}

fn infer(
    host: &GuestHost,
    provider: ProviderSnapshot,
    mut request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    let api_key = api_key(&provider)?;
    if let Some(model) = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        request.model = model.to_string();
    }
    request.stream.enabled = true;

    common::ensure_no_native_compaction(&request)?;
    let adapter = CommandCodeGenerateV1;
    let encoded = ProtocolTransform::encode_request_with(&adapter, &request)
        .map_err(common::map_request_transform_error)?;
    let mut body = encoded.body;
    let mut state = read_state(host)?;
    let now_ms = Utc::now().timestamp_millis();
    let session_id = ensure_session(api_key, now_ms, &mut state);
    ensure_initialized(host, &provider, api_key, now_ms, &mut state)?;
    write_state(host, &state)?;

    apply_device_envelope(&mut body, &session_id)?;
    let body = serde_json::to_vec(&body).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Command Code request: {err}"),
        )
    })?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: format!("{}{}", base_url(&provider), encoded.path),
        headers: inference_headers(api_key, zdr_enabled(&provider), &session_id),
        body,
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(&response, status, "Command Code inference"));
    }
    decode_inference(host, response)
}

fn decode_inference(
    host: &GuestHost,
    response: stravia_vendor_sdk::HttpResponse,
) -> Result<OperationOutput, PluginError> {
    let headers = response.headers()?;
    let streaming = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.to_ascii_lowercase().contains("application/x-ndjson")
    });
    if !streaming {
        let body = read_http_body(&response, MAX_BODY)?;
        let value = serde_json::from_slice(&body).map_err(|err| {
            model_error(format!(
                "upstream returned invalid Command Code JSON: {err}"
            ))
        })?;
        let complete = CommandCodeGenerateV1
            .decode_response(value)
            .map_err(|err| model_error(format!("invalid Command Code response: {err}")))?;
        host.emit_completed(&complete)?;
        return Ok(OperationOutput::Infer(Box::new(complete)));
    }

    let mut parser = CommandCodeStreamParser::new();
    let mut accumulator = StreamResponseAccumulator::default();
    let mut pending_utf8 = Vec::new();
    while let Some(chunk) = response.read_body()? {
        pending_utf8.extend_from_slice(&chunk);
        let deltas = decode_utf8_chunk(&mut parser, &mut pending_utf8)?;
        emit_deltas(host, &mut accumulator, &deltas)?;
    }
    if !pending_utf8.is_empty() {
        return Err(model_error(
            "Command Code stream ended in the middle of a UTF-8 sequence",
        ));
    }
    let deltas = TextWireStreamParser::finish(&mut parser)
        .map_err(|err| model_error(format!("invalid Command Code stream: {err}")))?;
    emit_deltas(host, &mut accumulator, &deltas)?;
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(OperationOutput::Infer(Box::new(complete)))
}

fn decode_utf8_chunk(
    parser: &mut CommandCodeStreamParser,
    pending: &mut Vec<u8>,
) -> Result<Vec<AiStreamDelta>, PluginError> {
    match std::str::from_utf8(pending) {
        Ok(text) => {
            let deltas = TextWireStreamParser::parse_text_chunk(parser, text)
                .map_err(|err| model_error(format!("invalid Command Code stream: {err}")))?;
            pending.clear();
            Ok(deltas)
        }
        Err(err) if err.error_len().is_none() => {
            let valid_up_to = err.valid_up_to();
            if valid_up_to == 0 {
                return Ok(Vec::new());
            }
            let deltas = TextWireStreamParser::parse_text_chunk(
                parser,
                std::str::from_utf8(&pending[..valid_up_to])
                    .expect("bytes before valid_up_to are valid UTF-8"),
            )
            .map_err(|err| model_error(format!("invalid Command Code stream: {err}")))?;
            pending.drain(..valid_up_to);
            Ok(deltas)
        }
        Err(err) => Err(model_error(format!(
            "Command Code stream contained invalid UTF-8: {err}"
        ))),
    }
}

fn emit_deltas(
    host: &GuestHost,
    accumulator: &mut StreamResponseAccumulator,
    deltas: &[AiStreamDelta],
) -> Result<(), PluginError> {
    let terminal = deltas.iter().find_map(|delta| match delta {
        AiStreamDelta::StreamError { error } => Some((error.kind.clone(), error.status_code)),
        AiStreamDelta::UnexpectedEof => Some((AiErrorKind::UnexpectedEof, None)),
        _ => None,
    });
    for delta in deltas {
        host.emit_delta(delta)?;
        accumulator.apply(delta);
    }
    if let Some((kind, status)) = terminal {
        let failure = PluginError {
            kind: ErrorKind::upstream(Some(kind), None),
            message: "upstream inference stream ended with an error".into(),
            upstream_status: status,
        };
        host.emit_failed(PluginError {
            kind: failure.kind,
            message: failure.message.clone(),
            upstream_status: failure.upstream_status,
        })?;
        return Err(failure);
    }
    Ok(())
}

fn model_error(message: impl Into<String>) -> PluginError {
    error(
        ErrorKind::upstream(Some(AiErrorKind::ServerError), None),
        message,
    )
}

fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<DiscoverResponse, PluginError> {
    if let Some(response) = static_discovery(provider)? {
        return Ok(response);
    }
    let key = api_key(provider)?;
    let url = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != MODELS_SOURCE_CATALOG)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}/provider/v1/models", base_url(provider)));
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "GET".into(),
        url,
        headers: identity_headers(key),
        body: Vec::new(),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(
            &response,
            status,
            "Command Code model discovery",
        ));
    }
    let bytes = read_http_body(&response, MAX_BODY)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|err| {
        error(
            ErrorKind::Invalid,
            format!("invalid Command Code model catalog: {err}"),
        )
    })?;
    let mut models = BTreeMap::new();
    for item in value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        let display_name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&id)
            .to_string();
        let family = item
            .get("family")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|family| !family.is_empty())
            .map(str::to_string);
        let selector = item
            .get("selector")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|selector| !selector.is_empty())
            .map(str::to_string)
            .or_else(|| Some(id.clone()));
        let capabilities = item
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let metadata = item
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "id" | "name" | "family" | "selector" | "capabilities"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        models.insert(
            id.clone(),
            DiscoveredModel {
                id,
                display_name,
                family,
                selector,
                capabilities,
                metadata,
            },
        );
    }
    if models.is_empty() {
        return Err(error(
            ErrorKind::Invalid,
            "Command Code model discovery returned no model ids",
        ));
    }
    Ok(DiscoverResponse {
        models: models.into_values().collect(),
        next_cursor: None,
    })
}

fn static_discovery(provider: &ProviderSnapshot) -> Result<Option<DiscoverResponse>, PluginError> {
    let Some(static_models) = provider.operation_metadata.get("static_models") else {
        return Ok(None);
    };
    let values = static_models.as_array().ok_or_else(|| {
        error(
            ErrorKind::Invalid,
            "operation_metadata.static_models must be a JSON array",
        )
    })?;
    let mut models = BTreeMap::new();
    for (index, value) in values.iter().enumerate() {
        let id = value.as_str().ok_or_else(|| {
            error(
                ErrorKind::Invalid,
                format!("operation_metadata.static_models[{index}] must be a string"),
            )
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
        thinking::decorate_discovered_model(VENDOR_ID, &mut model);
        models.entry(id.to_owned()).or_insert(model);
    }
    Ok(Some(DiscoverResponse {
        models: models.into_values().collect(),
        next_cursor: None,
    }))
}

fn ensure_session(api_key: &str, now_ms: i64, state: &mut PrivateState) -> String {
    if let (Some(id), Some(expires_at)) = (&state.session_id, state.session_expires_at_ms)
        && now_ms < expires_at
    {
        return id.clone();
    }
    let jitter = jitter_ms(api_key, "session-jitter", SESSION_JITTER_MS);
    let id = uuid::Uuid::new_v4().to_string();
    state.session_id = Some(id.clone());
    state.session_expires_at_ms = Some(
        now_ms
            .saturating_add(SESSION_TTL_MS)
            .saturating_add(i64::try_from(jitter).unwrap_or(i64::MAX)),
    );
    id
}

fn ensure_initialized(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    api_key: &str,
    now_ms: i64,
    state: &mut PrivateState,
) -> Result<(), PluginError> {
    if state.next_init_at_ms.is_some_and(|next| now_ms < next) {
        return Ok(());
    }
    let fingerprint = state
        .fingerprint
        .get_or_insert_with(|| generate_fingerprint(api_key))
        .clone();
    let headers = init_headers(api_key, zdr_enabled(provider));
    let fingerprint_body = serde_json::to_vec(&fingerprint).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Command Code fingerprint: {err}"),
        )
    })?;
    let lifecycle_body = serde_json::to_vec(&json!({
        "eventType": "cli_session_exists",
        "metadata": {
            "sessionId": format!("sess_{}", hex_encode(&fp_digest(api_key, "lifecycle-session")[..8])),
            "cliVersion": PROTOCOL_VERSION,
            "mode": "interactive",
            "os": "win32-x64",
        }
    }))
    .map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Command Code lifecycle: {err}"),
        )
    })?;
    let fingerprint_response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: format!("{}/alpha/fingerprint/record", base_url(provider)),
        headers: headers.clone(),
        body: fingerprint_body,
    });
    let lifecycle_response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: format!("{}/alpha/lifecycle-events", base_url(provider)),
        headers,
        body: lifecycle_body,
    });
    for response in [fingerprint_response, lifecycle_response]
        .into_iter()
        .flatten()
    {
        if response.status().is_ok() {
            let _ = read_http_body(&response, MAX_BODY);
        }
    }
    let jitter = jitter_ms(api_key, "init-jitter", INIT_JITTER_MS);
    state.next_init_at_ms = Some(
        Utc::now()
            .timestamp_millis()
            .saturating_add(INIT_TTL_MS)
            .saturating_add(i64::try_from(jitter).unwrap_or(i64::MAX)),
    );
    Ok(())
}

fn apply_device_envelope(body: &mut Value, session_id: &str) -> Result<(), PluginError> {
    let object = body.as_object().cloned().ok_or_else(|| {
        error(
            ErrorKind::Trapped,
            "Command Code codec returned a non-object request",
        )
    })?;
    let mut ordered = Map::new();
    for key in ["config", "memory", "taste", "skills", "permissionMode"] {
        if let Some(value) = object.get(key) {
            ordered.insert(key.to_string(), value.clone());
        }
    }
    if let Some(config) = ordered.get_mut("config").and_then(Value::as_object_mut) {
        config.insert(
            "workingDir".into(),
            Value::String(DEFAULT_WORKING_DIR.into()),
        );
        config.insert("environment".into(), Value::String("win32".into()));
    }
    ordered.insert("threadId".into(), Value::String(session_id.to_string()));
    for key in ["mode", "promptCache", "params"] {
        if let Some(value) = object.get(key) {
            ordered.insert(key.to_string(), value.clone());
        }
    }
    *body = Value::Object(ordered);
    Ok(())
}

fn inference_headers(api_key: &str, zdr: bool, session_id: &str) -> Vec<(String, String)> {
    let mut headers = init_headers(api_key, zdr);
    headers.push((
        "x-project-slug".into(),
        slugify_project_path(DEFAULT_WORKING_DIR),
    ));
    headers.push(("x-taste-learning".into(), "false".into()));
    headers.push(("x-session-id".into(), session_id.into()));
    let trace = uuid::Uuid::new_v4().simple().to_string();
    let span = uuid::Uuid::new_v4().simple().to_string();
    headers.push((
        "traceparent".into(),
        format!("00-{trace}-{}-01", &span[..16]),
    ));
    headers
}

fn identity_headers(api_key: &str) -> Vec<(String, String)> {
    vec![
        ("user-agent".into(), "cli".into()),
        ("x-command-code-version".into(), PROTOCOL_VERSION.into()),
        ("x-cli-environment".into(), "production".into()),
        ("authorization".into(), format!("Bearer {api_key}")),
    ]
}

fn init_headers(api_key: &str, zdr: bool) -> Vec<(String, String)> {
    let mut headers = identity_headers(api_key);
    headers.push(("content-type".into(), "application/json".into()));
    if zdr {
        headers.push(("x-cmd-zdr".into(), "1".into()));
    }
    headers
}

fn zdr_enabled(provider: &ProviderSnapshot) -> bool {
    provider
        .options
        .get("zdr")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub(crate) fn api_key(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    ["apiKey", "api_key", "access_token", "accessToken", "token"]
        .into_iter()
        .find_map(|key| provider.credentials.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| error(ErrorKind::Auth, "Command Code API key is missing"))
}

pub(crate) fn base_url(provider: &ProviderSnapshot) -> String {
    let base = provider.base_url.trim();
    if base.is_empty() {
        DEFAULT_BASE_URL.into()
    } else {
        base.trim_end_matches('/').to_string()
    }
}

fn read_state(host: &GuestHost) -> Result<PrivateState, PluginError> {
    match host.read_private_state()? {
        None => Ok(PrivateState::default()),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| error(ErrorKind::Invalid, "invalid Command Code private state")),
    }
}

fn write_state(host: &GuestHost, state: &PrivateState) -> Result<(), PluginError> {
    let bytes = serde_json::to_vec(state).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Command Code private state: {err}"),
        )
    })?;
    host.write_private_state(&bytes)
}

fn generate_fingerprint(api_key: &str) -> Value {
    let &(cpu_model, cpu_count) = fp_pick(api_key, "cpu", FINGERPRINT_CPUS, |(model, cores)| {
        format!("{model}|{cores}")
    });
    let &mem_gib = fp_pick(api_key, "mem", FINGERPRINT_MEMS, u8::to_string);
    let &timezone = fp_pick(api_key, "timezone", FINGERPRINT_TZS, |value| *value);
    let &mac_count = fp_pick(
        api_key,
        "macCount",
        FINGERPRINT_MAC_COUNTS,
        usize::to_string,
    );
    let &os_user = fp_pick(api_key, "osUser", FP_OS_USERS, |value| *value);
    let &mail_domain = fp_pick(api_key, "mailDomain", FP_MAIL_DOMAINS, |value| *value);
    let mid = hex_encode(&fp_digest(api_key, "machineId")[..16]);
    let machine_id = format!(
        "{}-{}-{}-{}-{}",
        &mid[0..8],
        &mid[8..12],
        &mid[12..16],
        &mid[16..20],
        &mid[20..32]
    );
    let mut macs = Vec::new();
    for index in 0..mac_count {
        let bytes = &fp_digest(api_key, &format!("mac{index}"))[..6];
        macs.push(
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(":"),
        );
    }
    macs.sort();
    let hostname = format!(
        "DESKTOP-{}",
        hex_encode(&fp_digest(api_key, "hostname")[..4]).to_ascii_uppercase()
    );
    let git_email = format!(
        "{os_user}.{}@{mail_domain}",
        hex_encode(&fp_digest(api_key, "gitEmail")[..3])
    );
    let thumbmark = fingerprint_hash_raw(&format!("machine\0{machine_id}|{}", macs.join(",")));
    json!({
        "thumbmark": thumbmark,
        "components": {
            "machineIdHash": fingerprint_hash(&machine_id),
            "macHashes": macs.iter().map(|mac| fingerprint_hash(mac)).collect::<Vec<_>>(),
            "osUserHash": fingerprint_hash(os_user),
            "hostnameHash": fingerprint_hash(&hostname),
            "gitEmailHash": fingerprint_hash(&git_email),
            "platform": "win32",
            "arch": "x64",
            "osRelease": "10.0.22631",
            "cpuModel": cpu_model,
            "cpuCount": cpu_count,
            "memGiB": mem_gib,
            "isContainer": false,
            "timezone": timezone,
            "runtime": "cli",
            "collectorVersion": 1,
        },
    })
}

fn fp_digest(api_key: &str, field: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0]);
    hasher.update(api_key.as_bytes());
    hasher.update([0]);
    hasher.update(field.as_bytes());
    hasher.finalize().into()
}

fn fp_pick<'a, T, S: AsRef<str>>(
    api_key: &str,
    field: &str,
    options: &'a [T],
    label: impl Fn(&T) -> S,
) -> &'a T {
    let score = |option: &T| fp_digest(api_key, &format!("{field}\0{}", label(option).as_ref()));
    let mut best = options.first().expect("fingerprint options are non-empty");
    let mut best_score = score(best);
    for option in &options[1..] {
        let option_score = score(option);
        if option_score > best_score {
            best = option;
            best_score = option_score;
        }
    }
    best
}

fn fingerprint_hash(value: &str) -> String {
    fingerprint_hash_raw(&value.trim().to_ascii_lowercase())
}

fn fingerprint_hash_raw(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FP_SALT.as_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    hex_encode(&hasher.finalize())
}

fn jitter_ms(api_key: &str, field: &str, maximum: u64) -> u64 {
    u64::from_le_bytes(
        fp_digest(api_key, field)[..8]
            .try_into()
            .expect("eight bytes"),
    ) % maximum
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn slugify_project_path(path: &str) -> String {
    let slug = path
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    let collapsed = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "root".into()
    } else {
        collapsed
    }
}

fn http_error(
    response: &stravia_vendor_sdk::HttpResponse,
    status: u16,
    label: &str,
) -> PluginError {
    let headers = match response.headers() {
        Ok(headers) => headers,
        Err(error) => return error,
    };
    let bytes = match read_http_body(response, MAX_BODY) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let mut failure = common::upstream_error(status, &headers, &bytes);
    if status == 402 {
        let retry_after = failure.kind.retry_after();
        failure.kind = ErrorKind::upstream(Some(AiErrorKind::QuotaExceeded), retry_after);
    }
    failure.message = format!("{label} returned HTTP {status}: {}", failure.message);
    failure
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(target_arch = "wasm32")]
struct CommandCodeVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for CommandCodeVendor {
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
stravia_vendor_sdk::export_vendor!(CommandCodeVendor);
