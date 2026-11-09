mod allowance;
mod codec;
mod discovery;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::TextWireStreamParser;
use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiRequest, AiStreamDelta};
use stravia_vendor_common::common;
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, ConfigValidationResponse, DataCompatibility, ErrorKind, GuestHost,
    NetworkDeclaration, Operation, OperationInput, OperationOutput, OriginDeclaration, PluginError,
    ProviderDescriptor, ProviderSnapshot, ValidationIssue, VendorDescriptor, VendorKind,
    read_http_body,
};

use crate::codec::{CommandCodeGenerateV1, CommandCodeStreamParser, DEFAULT_WORKING_DIR};

const VENDOR_ID: &str = "command-code";
/// Directory identity shared with the catalog and Base naming; the package and
/// execution identity stay `command-code`.
const CATALOG_ID: &str = "commandcode";
const CHANNEL_ID: &str = "default";
const PROTOCOL_VERSION: &str = "1.82.1";
const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";
const FP_SALT: &str = "command-code:device-fingerprint:v1";
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

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::ConfigValidation,
    ]);
    VendorDescriptor {
        egress_shapes: Vec::new(),
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
            catalog_id: Some(CATALOG_ID.into()),
            icon_svg: Some(include_str!("assets/command-code.svg").into()),
            display_name: "CommandCode".into(),
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
                client_header_names: BTreeSet::new(),
                model_capabilities: BTreeSet::new(),
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
    Ok("command-code/generate/v1".into())
}

pub fn check_connection(
    host: &GuestHost,
    channel: &str,
    provider: ProviderSnapshot,
) -> Result<stravia_vendor_sdk::ConnectionCheckResponse, PluginError> {
    check_provider(channel, &provider)?;
    discovery::connection_check(host, &provider)
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
            discovery::execute(host, &provider).map(OperationOutput::Discover)
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

    let mut body = CommandCodeGenerateV1
        .encode_request(request)
        .map_err(|err| {
            error(
                ErrorKind::Invalid,
                format!("encode Command Code request: {err:#}"),
            )
        })?;
    let installation_id = client_installation_id(&provider)?;
    let session_id = inference_session_id(&provider)?;
    ensure_initialized(host, &provider, api_key, installation_id, &session_id)?;

    apply_device_envelope(&mut body, &session_id)?;
    let body = serde_json::to_vec(&body).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Command Code request: {err}"),
        )
    })?;
    host.emit_started()?;
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: format!("{}/alpha/generate", base_url(&provider)),
            headers: crate::net_fields(inference_headers(
                api_key,
                zdr_enabled(&provider),
                &session_id,
            )),
            body,
        },
        &crate::inference_net_options(),
    )?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(&response, status, "Command Code inference"));
    }
    decode_inference(host, response)
}

/// The upstream always answers `/alpha/generate` with an NDJSON event stream,
/// regardless of how it labels `Content-Type`, so the body is decoded as a
/// stream unconditionally.
fn decode_inference(
    host: &GuestHost,
    response: stravia_vendor_sdk::HttpResponse,
) -> Result<OperationOutput, PluginError> {
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
            kind: failure.kind.clone(),
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

fn native_session_id(affinity: &str) -> String {
    // The official threadId validator is z.uuid(); newSession uses randomUUID().
    // Hash the complete trusted host scope, never credentials or client headers.
    let mut hash = Sha256::new();
    hash.update(b"stravia:command-code:session:v1\0");
    hash.update(affinity.as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn client_installation_id(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    let id = provider
        .operation_metadata
        .get("client_installation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| error(ErrorKind::Invalid, "missing client_installation_id"))?;
    let parsed = uuid::Uuid::parse_str(id)
        .map_err(|_| error(ErrorKind::Invalid, "invalid client_installation_id"))?;
    if parsed.to_string() != id {
        return Err(error(
            ErrorKind::Invalid,
            "client_installation_id must be a canonical UUID",
        ));
    }
    Ok(id)
}

fn inference_session_id(provider: &ProviderSnapshot) -> Result<String, PluginError> {
    let scope = common::session_affinity(provider)
        .or_else(|| {
            provider
                .operation_metadata
                .get("client_conversation_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        })
        .ok_or_else(|| error(ErrorKind::Invalid, "missing trusted conversation identity"))?;
    Ok(native_session_id(scope))
}

fn ensure_initialized(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    api_key: &str,
    installation_id: &str,
    session_id: &str,
) -> Result<(), PluginError> {
    use stravia_vendor_sdk::state::{self, Error as StateError, Scope};

    // The cell is bound by the host to this conversation checkpoint; it never
    // stores credentials, and its fixed-size payload does not grow with turns.
    let cell = state::open(Scope::Conversation, "command-code-startup-attempted-v1")
        .map_err(PluginError::from)?;
    let identity = format!("{installation_id}:{session_id}");
    loop {
        let snapshot = cell.load().map_err(PluginError::from)?;
        if snapshot.payload.as_deref() == Some(identity.as_bytes()) {
            return Ok(());
        }
        match cell.compare_exchange(snapshot.revision, Some(identity.as_bytes())) {
            Ok(()) => break,
            Err(StateError::Conflict) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    // Network I/O is deliberately outside the CAS reducer. The CLI likewise
    // marks fingerprint initialization before its best-effort request.
    let fingerprint = generate_fingerprint(installation_id);
    let fingerprint_body = serde_json::to_vec(&fingerprint).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Command Code fingerprint: {err}"),
        )
    })?;
    let lifecycle_body = serde_json::to_vec(&json!({
        "eventType": "cli_session_exists",
        "metadata": {
            "sessionId": format!("sess_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]),
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
    let fingerprint_response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: format!("{}/alpha/fingerprint/record", base_url(provider)),
            headers: crate::net_fields(init_headers(api_key, zdr_enabled(provider), true)),
            body: fingerprint_body,
        },
        &crate::inference_net_options(),
    );
    let lifecycle_response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: format!("{}/alpha/lifecycle-events", base_url(provider)),
            headers: crate::net_fields(init_headers(api_key, zdr_enabled(provider), false)),
            body: lifecycle_body,
        },
        &crate::inference_net_options(),
    );
    for (stage, response) in [
        ("fingerprint", fingerprint_response),
        ("lifecycle", lifecycle_response),
    ] {
        let outcome = response.and_then(|response| {
            let status = response.status()?;
            read_http_body(&response, MAX_BODY)?;
            if !(200..300).contains(&status) {
                host.log(
                    stravia_vendor_sdk::LogLevel::Warn,
                    &format!("Command Code optional {stage} startup returned HTTP {status}"),
                );
            }
            Ok(())
        });
        if let Err(err) = outcome {
            host.log(
                stravia_vendor_sdk::LogLevel::Warn,
                &format!(
                    "Command Code optional {stage} startup failed: {:?}",
                    err.kind
                ),
            );
        }
    }
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
    let mut headers = vec![
        ("host".into(), String::new()),
        ("connection".into(), "keep-alive".into()),
        (
            "content-type".into(),
            "application/json, application/json".into(),
        ),
        ("User-Agent".into(), "cli".into()),
        ("x-command-code-version".into(), PROTOCOL_VERSION.into()),
        ("x-cli-environment".into(), "production".into()),
    ];
    headers.push((
        "x-project-slug".into(),
        slugify_project_path(DEFAULT_WORKING_DIR),
    ));
    headers.push(("x-taste-learning".into(), "false".into()));
    headers.push(("x-session-id".into(), session_id.into()));
    headers.push(("Authorization".into(), format!("Bearer {api_key}")));
    if zdr {
        headers.push(("x-cmd-zdr".into(), "1".into()));
    }
    let trace = uuid::Uuid::new_v4().simple().to_string();
    let span = uuid::Uuid::new_v4().simple().to_string();
    headers.push((
        "traceparent".into(),
        format!("00-{trace}-{}-01", &span[..16]),
    ));
    headers.extend([
        ("accept".into(), "*/*".into()),
        ("accept-language".into(), "*".into()),
        ("sec-fetch-mode".into(), "cors".into()),
        ("accept-encoding".into(), "br, gzip, deflate".into()),
        ("content-length".into(), String::new()),
    ]);
    headers
}

fn init_headers(api_key: &str, zdr: bool, fingerprint: bool) -> Vec<(String, String)> {
    let mut headers = vec![
        ("host".into(), String::new()),
        ("connection".into(), "keep-alive".into()),
        (
            "content-type".into(),
            if fingerprint {
                "application/json"
            } else {
                "application/json, application/json"
            }
            .into(),
        ),
    ];
    if fingerprint {
        headers.push(("Authorization".into(), format!("Bearer {api_key}")));
        headers.push(("x-cli-environment".into(), "production".into()));
        headers.push(("x-command-code-version".into(), PROTOCOL_VERSION.into()));
        headers.push(("User-Agent".into(), "cli".into()));
    } else {
        headers.push(("x-cli-environment".into(), "production".into()));
        headers.push(("Authorization".into(), format!("Bearer {api_key}")));
        headers.push(("User-Agent".into(), "cli".into()));
        headers.push(("x-command-code-version".into(), PROTOCOL_VERSION.into()));
    }
    if zdr {
        headers.push(("x-cmd-zdr".into(), "1".into()));
    }
    headers.extend([
        ("accept".into(), "*/*".into()),
        ("accept-language".into(), "*".into()),
        ("sec-fetch-mode".into(), "cors".into()),
        ("accept-encoding".into(), "br, gzip, deflate".into()),
        ("content-length".into(), String::new()),
    ]);
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

    fn check_connection(
        host: &GuestHost,
        channel: &str,
        provider: ProviderSnapshot,
    ) -> Result<stravia_vendor_sdk::ConnectionCheckResponse, PluginError> {
        check_connection(host, channel, provider)
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

fn net_fields(headers: Vec<(String, String)>) -> stravia_vendor_sdk::net::Fields {
    let mut fields = stravia_vendor_sdk::net::Fields::new();
    for (name, value) in headers {
        fields.append(name, value.into_bytes());
    }
    if fields.get("accept").is_none() {
        fields.append("accept", b"*/*".to_vec());
    }
    if fields.get("accept-encoding").is_none() {
        fields.append("accept-encoding", b"identity".to_vec());
    }
    if fields.get("user-agent").is_none() {
        fields.append("user-agent", b"Stravia".to_vec());
    }
    fields
}

fn net_options() -> stravia_vendor_sdk::net::Options {
    let mut options = stravia_vendor_sdk::net::Options::new();
    options
        .shape("command-code")
        .expect("command-code shape is valid");
    options
}

fn inference_net_options() -> stravia_vendor_sdk::net::Options {
    use stravia_vendor_sdk::net::Coding;
    let mut options = net_options();
    options
        .response_decoding(&[Coding::Br, Coding::Gzip, Coding::Deflate])
        .expect("valid response codings");
    options
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn provider(affinity: Option<&str>) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: VENDOR_ID.into(),
            channel: CHANNEL_ID.into(),
            base_url: "http://127.0.0.1:18080".into(),
            protocol: "command-code".into(),
            options: BTreeMap::from([("session_id".into(), json!("injected"))]),
            credentials: BTreeMap::from([("apiKey".into(), json!("synthetic-key"))]),
            model: Some("synthetic-model".into()),
            model_metadata: None,
            client_headers: vec![("x-session-id".into(), "injected".into())],
            operation_metadata: affinity
                .map(|value| BTreeMap::from([("session_affinity".into(), json!(value))]))
                .unwrap_or_default(),
        }
    }

    #[test]
    fn wire_identity_uses_trusted_scope() {
        let provider = provider(Some(" 会话🚀\0A "));
        let id = inference_session_id(&provider).unwrap();
        let headers = inference_headers("synthetic-key", true, &id);
        let second = inference_headers("synthetic-key", true, &id);
        assert!(headers.contains(&("x-session-id".into(), id.clone())));
        assert!(headers.contains(&("Authorization".into(), "Bearer synthetic-key".into())));
        let trace = |values: &Vec<(String, String)>| {
            values
                .iter()
                .find(|(name, _)| name == "traceparent")
                .unwrap()
                .1
                .clone()
        };
        assert_ne!(trace(&headers), trace(&second));
        let mut body = json!({"config": {}, "params": {"messages": []}});
        apply_device_envelope(&mut body, &id).unwrap();
        assert_eq!(body["threadId"], id);
        assert_ne!(id, native_session_id("会话🚀\0A"));
        assert_ne!(id, native_session_id(" 会话🚀\0a "));
        assert_ne!(id, native_session_id(" 会话🚀"));
        let long = "会".repeat(4096);
        assert_ne!(
            native_session_id(&long),
            native_session_id(&format!("{long}尾"))
        );
    }

    #[test]
    fn inference_headers_follow_interactive_wire_order() {
        let headers = inference_headers("synthetic-key", false, "synthetic-session");
        assert_eq!(
            headers
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [
                "host",
                "connection",
                "content-type",
                "User-Agent",
                "x-command-code-version",
                "x-cli-environment",
                "x-project-slug",
                "x-taste-learning",
                "x-session-id",
                "Authorization",
                "traceparent",
                "accept",
                "accept-language",
                "sec-fetch-mode",
                "accept-encoding",
                "content-length"
            ]
        );
        assert!(headers.contains(&(
            "content-type".into(),
            "application/json, application/json".into()
        )));
        assert!(headers.contains(&("accept-encoding".into(), "br, gzip, deflate".into())));
        let with_zdr = inference_headers("synthetic-key", true, "synthetic-session");
        let names: Vec<_> = with_zdr.iter().map(|(name, _)| name.as_str()).collect();
        let authorization = names
            .iter()
            .position(|name| *name == "Authorization")
            .unwrap();
        assert_eq!(names[authorization + 1], "x-cmd-zdr");
        assert_eq!(names[authorization + 2], "traceparent");
    }

    #[test]
    fn installation_identity_is_required_and_canonical() {
        let mut snapshot = provider(None);
        assert!(client_installation_id(&snapshot).is_err());
        snapshot
            .operation_metadata
            .insert("client_installation_id".into(), json!("invalid"));
        assert!(client_installation_id(&snapshot).is_err());
        snapshot.operation_metadata.insert(
            "client_installation_id".into(),
            json!("01234567-89ab-4cde-8fab-0123456789ab"),
        );
        assert_eq!(
            client_installation_id(&snapshot).unwrap(),
            "01234567-89ab-4cde-8fab-0123456789ab"
        );
        assert_eq!(
            generate_fingerprint(client_installation_id(&snapshot).unwrap()),
            generate_fingerprint("01234567-89ab-4cde-8fab-0123456789ab")
        );
        assert_ne!(
            generate_fingerprint("01234567-89ab-4cde-8fab-0123456789ab"),
            generate_fingerprint("01234567-89ab-4cde-8fab-0123456789ac")
        );
    }

    #[test]
    fn fixed_vector_and_conversation_session_remain_stable() {
        assert_eq!(
            native_session_id(&"0123456789abcdef".repeat(4)),
            "29776a7f-1e39-483f-b0be-28beeb2d9f3a"
        );
        let mut provider = provider(None);
        assert!(inference_session_id(&provider).is_err());
        provider.operation_metadata.insert(
            "client_conversation_id".into(),
            json!("01234567-89ab-4cde-8fab-0123456789ab"),
        );
        let first = inference_session_id(&provider).unwrap();
        assert_eq!(first, inference_session_id(&provider).unwrap());
        assert_eq!(uuid::Uuid::parse_str(&first).unwrap().get_version_num(), 4);
    }
}
