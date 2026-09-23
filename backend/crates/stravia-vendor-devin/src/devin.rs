mod auth;
mod family;
mod selector;

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::devin_connect::request::SessionShape;
use crate::codec::devin_connect::{
    ASSIGN_MODEL_PATH, DevinClientPlatform, GET_CHAT_MESSAGE_PATH, ModelAssignment,
    decode_assign_model_response, decode_cli_model_configs,
    encode_assign_model_request_with_platform, encode_client_metadata_request_with_platform,
    encode_get_chat_message_request_with_platform, session_shape, wrap_request,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiStreamDelta};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AuthCallback, AuthCallbackPort, AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType,
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, DataCompatibility, DiscoverResponse, ErrorKind, GuestHost, NetworkDeclaration,
    Operation, OperationInput, OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor,
    ProviderSnapshot, VendorDescriptor, VendorKind, read_http_body,
};

const DEFAULT_BASE_URL: &str = "https://server.codeium.com";
const CATALOG_PATH: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
const MAX_UNARY_BODY: usize = 16 * 1024 * 1024;
const STATIC_MODELS: &[&str] = &[
    "swe-1-6-slow",
    "swe-1-7",
    "swe-1-7-lightning",
    "swe-2-medium",
    "swe-2-high",
    "swe-2-max",
    "claude-sonnet-5-medium",
    "claude-opus-5-medium",
    "claude-5-fable-medium",
    "gpt-5-4-medium",
    "gpt-5-5-medium",
    "gpt-5-6-luna-medium",
    "gemini-3-5-flash-medium",
    "gemini-3-1-pro-low",
    "glm-5-2",
    "kimi-k2-7",
    "deepseek-v4",
];

#[derive(Debug, Default, Serialize, Deserialize)]
struct PrivateState {
    #[serde(default)]
    assignments: BTreeMap<String, ModelAssignmentState>,
    #[serde(default)]
    next_steps: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelAssignmentState {
    jwt: String,
    model_uid: String,
}

pub(crate) fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
    ]);
    VendorDescriptor {
        vendor_id: "devin".into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "Devin".into(),
        description: Some("Devin Connect inference, OAuth, and live model discovery".into()),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: "devin".into(),
            catalog_id: Some("devin".into()),
            display_name: "Devin".into(),
            description: Some("Devin CLI OAuth and Connect-RPC API".into()),
            channels: vec![ChannelDescriptor {
                id: "devin".into(),
                name: crate::messages::channel_devin(),
                description: Some(crate::messages::channel_description()),
                auth: Some(AuthDescriptor {
                    flow: AuthFlow::AuthorizationCode,
                    callback: Some(AuthCallback {
                        bind_host: "127.0.0.1".into(),
                        redirect_host: "127.0.0.1".into(),
                        path: "/callback".into(),
                        port: AuthCallbackPort::Dynamic,
                        manual_redirect_uri: Some("chisel-show-auth-token".into()),
                        cancel_path: None,
                    }),
                    manual_input: Some(AuthManualInput {
                        input_type: AuthManualInputType::Text,
                        label: crate::messages::manual_token(),
                        description: Some(crate::messages::manual_token_description()),
                        secret: true,
                    }),
                }),
                protocol: Some("devin-connect".into()),
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
            config_groups: vec![ConfigGroup {
                id: "authentication".into(),
                label: crate::messages::authentication(),
            }],
            config_fields: vec![ConfigField {
                key: "apiKey".into(),
                label: crate::messages::session_token(),
                description: Some(crate::messages::session_token_description()),
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
            }],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: vec![
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "server.codeium.com".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "app.devin.ai".into(),
                        port: None,
                    },
                ],
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

pub(crate) fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if channel != "devin"
        || input.provider().provider_id != "devin"
        || input.provider().channel != channel
    {
        return Err(error(
            ErrorKind::Unsupported,
            "unsupported Devin provider or channel",
        ));
    }
    match (operation, input) {
        (Operation::Infer, OperationInput::Infer { provider, request }) => {
            infer(host, provider, request)
        }
        (Operation::Auth, OperationInput::Auth { provider, request }) => {
            auth::execute(host, &provider, request).map(OperationOutput::Auth)
        }
        (Operation::Discover, OperationInput::Discover { provider, .. }) => {
            discover(host, &provider).map(OperationOutput::Discover)
        }
        (Operation::Allowance, OperationInput::Allowance { provider, request }) => {
            crate::allowance::execute(host, provider, request).map(OperationOutput::Allowance)
        }
        (kind, input) if kind == input.operation() => Err(error(
            ErrorKind::Unsupported,
            format!("Devin does not implement {}", kind.as_str()),
        )),
        _ => Err(error(
            ErrorKind::Invalid,
            "operation and input do not match",
        )),
    }
}

fn infer(
    host: &GuestHost,
    provider: ProviderSnapshot,
    mut request: stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    let token = session_token(&provider)?;
    let platform = client_platform(&provider)?;
    let configured_model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(request.model.as_str());
    request.model = selector::selector_from_alias(configured_model);
    let table = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| selector::table_from_extensions(&metadata.extensions));
    if let Some(table) = &table {
        request.model =
            selector::resolve_selector(table, request.reasoning.target_control.as_ref());
    } else if let Some(selector) = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| metadata.selector.as_deref())
        .map(str::trim)
        .filter(|selector| !selector.is_empty())
    {
        request.model = selector.to_string();
        if let Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { value }) =
            &request.reasoning.target_control
        {
            request.model = selector::selector_with_level(&request.model, value);
        }
    } else if let Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
        value,
    }) = &request.reasoning.target_control
    {
        request.model = selector::selector_with_level(&request.model, value);
    }
    stravia_protocol_codec::codec::tool_correlation::normalize_request_tool_results(&mut request);
    request.stream.enabled = true;

    common::ensure_no_native_compaction(&request)?;
    crate::codec::devin_connect::validate_request(&request).map_err(|err| {
        error(
            ErrorKind::Invalid,
            format!("Devin request is not representable: {err}"),
        )
    })?;
    let mut state = read_state(host)?;
    let base_shape = session_shape(&request, token);
    let step_index = match state.next_steps.get_mut(&base_shape.trajectory_id) {
        None => {
            state.next_steps.insert(base_shape.trajectory_id.clone(), 2);
            0
        }
        Some(next) => {
            let value = *next;
            *next = next.saturating_add(1);
            value
        }
    };
    if state.next_steps.len() > 256 {
        state.next_steps.clear();
        state.next_steps.insert(base_shape.trajectory_id.clone(), 2);
    }
    let shape = SessionShape {
        trajectory_id: base_shape.trajectory_id,
        cascade_id: base_shape.cascade_id,
        step_index,
    };

    let router_known = table
        .as_ref()
        .and_then(|table| table.routers.as_ref())
        .map(|routers| routers.iter().any(|router| router == &request.model));
    let assignment = if router_known == Some(false) {
        None
    } else {
        resolve_assignment(
            host,
            &provider,
            token,
            (&request.model, &shape.cascade_id),
            platform,
            &mut state,
            router_known == Some(true),
        )?
    };
    if let Some(assignment) = &assignment {
        request.model = assignment.model_uid.clone();
    }
    write_state(host, &state)?;

    let proto = encode_get_chat_message_request_with_platform(
        &request,
        token,
        &shape,
        assignment
            .as_ref()
            .map(|assignment| assignment.jwt.as_str()),
        platform,
    )
    .map_err(|err| {
        error(
            ErrorKind::Invalid,
            format!("Devin request is not representable: {err}"),
        )
    })?;
    let body = wrap_request(&proto).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("frame Devin Connect request: {err}"),
        )
    })?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider, GET_CHAT_MESSAGE_PATH),
        headers: connect_headers(token, true),
        body,
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(&response, status, "Devin Connect"));
    }
    decode_inference(host, response)
}

fn decode_inference(
    host: &GuestHost,
    response: stravia_vendor_sdk::HttpResponse,
) -> Result<OperationOutput, PluginError> {
    let mut parser = crate::codec::devin_connect::DevinConnectStreamParser::new();
    let mut accumulator = StreamResponseAccumulator::default();
    while let Some(chunk) = response.read_body()? {
        let deltas = parser.parse_chunk(&chunk).map_err(|err| {
            common::model_error(
                AiErrorKind::ServerError,
                format!("invalid Devin Connect stream: {err}"),
            )
        })?;
        emit_deltas(host, &mut accumulator, &deltas)?;
    }
    let deltas = parser.finish().map_err(|err| {
        common::model_error(
            AiErrorKind::ServerError,
            format!("incomplete Devin Connect stream: {err}"),
        )
    })?;
    emit_deltas(host, &mut accumulator, &deltas)?;
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(OperationOutput::Infer(Box::new(complete)))
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

fn resolve_assignment(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    token: &str,
    (router_uid, cascade_id): (&str, &str),
    platform: DevinClientPlatform,
    state: &mut PrivateState,
    required: bool,
) -> Result<Option<ModelAssignmentState>, PluginError> {
    let key = format!("{router_uid}|{cascade_id}");
    if let Some(assignment) = state.assignments.get(&key) {
        return Ok(Some(assignment.clone()));
    }
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(provider, ASSIGN_MODEL_PATH),
        headers: connect_headers(token, false),
        body: encode_assign_model_request_with_platform(token, router_uid, cascade_id, platform),
    });
    let response = match response {
        Ok(response) => response,
        Err(_) if !required => return Ok(None),
        Err(err) => return Err(err),
    };
    let status = match response.status() {
        Ok(status) => status,
        Err(_) if !required => return Ok(None),
        Err(err) => return Err(err),
    };
    if !(200..300).contains(&status) {
        if required {
            return Err(http_error(&response, status, "Devin AssignModel"));
        }
        return Ok(None);
    }
    let bytes = match read_http_body(&response, MAX_UNARY_BODY) {
        Ok(bytes) => bytes,
        Err(_) if !required => return Ok(None),
        Err(err) => return Err(err),
    };
    let assignment = decode_assign_model_response(&bytes).map(|assignment: ModelAssignment| {
        ModelAssignmentState {
            jwt: assignment.jwt,
            model_uid: assignment.model_uid,
        }
    });
    if required && assignment.is_none() {
        return Err(common::model_error(
            stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError,
            format!("Devin AssignModel({router_uid}) returned an empty assignment"),
        ));
    }
    if let Some(assignment) = &assignment {
        if state.assignments.len() >= 64 {
            state.assignments.clear();
        }
        state.assignments.insert(key, assignment.clone());
    }
    Ok(assignment)
}

fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<DiscoverResponse, PluginError> {
    let token = session_token(provider)?;
    let platform = client_platform(provider)?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(provider, CATALOG_PATH),
        headers: catalog_headers(token),
        body: encode_client_metadata_request_with_platform(token, true, platform),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(&response, status, "Devin model discovery"));
    }
    let bytes = read_http_body(&response, MAX_UNARY_BODY)?;
    let entries = decode_cli_model_configs(&bytes).map_err(|err| {
        error(
            ErrorKind::upstream_unknown(),
            format!("Devin model discovery returned invalid protobuf: {err}"),
        )
    })?;

    let mut selectors: Vec<String> = entries.iter().map(|entry| entry.selector.clone()).collect();
    for selector in STATIC_MODELS {
        if !selectors.iter().any(|current| current == selector) {
            selectors.push((*selector).to_string());
        }
    }
    let models = family::group_families(&selectors, &entries)
        .iter()
        .map(family::discovered_model)
        .collect();
    Ok(DiscoverResponse {
        models,
        next_cursor: None,
    })
}

fn client_platform(provider: &ProviderSnapshot) -> Result<DevinClientPlatform, PluginError> {
    match provider
        .operation_metadata
        .get("host_platform")
        .and_then(Value::as_str)
    {
        Some("windows") => Ok(DevinClientPlatform::Windows),
        Some("macos") => Ok(DevinClientPlatform::Mac),
        Some("linux") => Ok(DevinClientPlatform::Linux),
        Some(other) => Err(error(
            ErrorKind::Invalid,
            format!("unsupported host_platform `{other}` for Devin"),
        )),
        None => Err(error(
            ErrorKind::Invalid,
            "Devin requires operation_metadata.host_platform",
        )),
    }
}

fn session_token(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    [
        "session_token",
        "sessionToken",
        "access_token",
        "accessToken",
        "apiKey",
        "api_key",
        "windsurf_api_key",
        "windsurfApiKey",
        "token",
    ]
    .into_iter()
    .find_map(|key| provider.credentials.get(key).and_then(Value::as_str))
    .map(str::trim)
    .filter(|token| !token.is_empty())
    .ok_or_else(|| error(ErrorKind::Auth, "Devin session token is missing"))
}

fn connect_headers(token: &str, streaming: bool) -> Vec<(String, String)> {
    let trace = uuid::Uuid::new_v4().simple().to_string();
    let span = uuid::Uuid::new_v4().simple().to_string();
    vec![
        (
            "content-type".into(),
            if streaming {
                "application/connect+proto".into()
            } else {
                "application/proto".into()
            },
        ),
        ("connect-protocol-version".into(), "1".into()),
        ("sentry-trace".into(), format!("{trace}-{}-1", &span[..16])),
        ("accept".into(), "*/*".into()),
        ("authorization".into(), format!("Basic {token}-{token}")),
    ]
}

fn catalog_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("content-type".into(), "application/proto".into()),
        ("connect-protocol-version".into(), "1".into()),
        ("accept".into(), "*/*".into()),
        ("authorization".into(), format!("Basic {token}-{token}")),
    ]
}

fn endpoint(provider: &ProviderSnapshot, path: &str) -> String {
    let base = provider.base_url.trim();
    let base = if base.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base
    };
    format!("{}{path}", base.trim_end_matches('/'))
}

fn read_state(host: &GuestHost) -> Result<PrivateState, PluginError> {
    match host.read_private_state()? {
        None => Ok(PrivateState::default()),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| error(ErrorKind::Invalid, "invalid Devin private state")),
    }
}

fn write_state(host: &GuestHost, state: &PrivateState) -> Result<(), PluginError> {
    let bytes = serde_json::to_vec(state).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Devin private state: {err}"),
        )
    })?;
    host.write_private_state(&bytes)
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
    let bytes = match read_http_body(response, MAX_UNARY_BODY) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let mut error = common::upstream_error(status, &headers, &bytes);
    error.message = format!("{label} returned HTTP {status}: {}", error.message);
    error
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}
