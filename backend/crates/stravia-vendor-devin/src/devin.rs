mod auth;
mod family;
mod selector;

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::devin_connect::request::{SessionShape, encode_prepared_request};
use crate::codec::devin_connect::stream::is_model_output;
use crate::codec::devin_connect::{
    ASSIGN_MODEL_PATH, DevinClientPlatform, GET_CHAT_MESSAGE_PATH, ModelAssignment,
    decode_assign_model_response, decode_cli_model_configs,
    encode_assign_model_request_with_platform, encode_client_metadata_request_with_platform,
    session_shape, wrap_request,
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
    // These are live upstream bindings, not disposable cache entries. No
    // lifecycle signal proves a scoped affinity dead, so capacity exhaustion
    // must reject a new write rather than silently reset another session.
    #[serde(default)]
    assignments: BTreeMap<String, ModelAssignmentState>,
    #[serde(default)]
    next_steps: BTreeMap<String, u64>,
}

impl PrivateState {
    fn record_assignment(&mut self, key: &str, assignment: &ModelAssignmentState) {
        self.assignments.insert(key.to_owned(), assignment.clone());
    }

    /// First native step is absent; index 1 belongs to CLI title generation.
    fn advance_session(&mut self, mut shape: SessionShape) -> SessionShape {
        shape.step_index = match self.next_steps.get_mut(&shape.trajectory_id) {
            None => {
                self.next_steps.insert(shape.trajectory_id.clone(), 2);
                0
            }
            Some(next) => {
                let value = *next;
                *next = next.saturating_add(1);
                value
            }
        };
        shape
    }
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
        egress_shapes: Vec::new(),
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
            icon_svg: Some(include_str!("assets/devin.svg").into()),
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
                        label: crate::messages::manual_code(),
                        description: Some(crate::messages::manual_code_description()),
                        secret: true,
                    }),
                }),
                protocol: Some("devin-connect".into()),
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
    crate::codec::devin_connect::prepare_request(&mut request);
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

    let affinity = provider
        .operation_metadata
        .get("transport_affinity")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    // Random, request-local identities can never be resumed. Persisting their
    // counters or cascade assignments only consumes capacity for dead records.
    let state = affinity.map(|_| read_state(host)).transpose()?;
    let shape = session_shape(affinity);

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
            state.as_ref().map(|(cell, state)| (cell, state)),
            router_known == Some(true),
        )?
    };
    if let Some(assignment) = &assignment {
        request.model = assignment.model_uid.clone();
    }
    let shape = match &state {
        Some((cell, _)) => advance_session(cell, shape)?,
        None => shape,
    };

    let proto = encode_prepared_request(
        &request,
        token,
        &shape,
        assignment
            .as_ref()
            .map(|assignment| assignment.jwt.as_str()),
        platform,
        &crate::fingerprint(&provider, token)?,
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
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: endpoint(&provider, GET_CHAT_MESSAGE_PATH),
            headers: crate::inference_fields(connect_headers(token, true)),
            body,
        },
        &crate::inference_options(),
    )?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(
            &response,
            status,
            "Devin Connect",
            protected_reasoning_untrusted(&provider),
        ));
    }
    decode_inference(host, response, protected_reasoning_untrusted(&provider))
}

fn protected_reasoning_untrusted(provider: &ProviderSnapshot) -> bool {
    provider
        .operation_metadata
        .get("protected_reasoning_untrusted")
        == Some(&Value::Bool(true))
}

fn decode_inference(
    host: &GuestHost,
    response: stravia_vendor_sdk::HttpResponse,
    protected_reasoning_untrusted: bool,
) -> Result<OperationOutput, PluginError> {
    let mut parser = crate::codec::devin_connect::DevinConnectStreamParser::new()
        .with_protected_reasoning_untrusted(protected_reasoning_untrusted);
    let mut accumulator = StreamResponseAccumulator::default();
    let mut produced_output = false;
    while let Some(chunk) = response.read_body()? {
        let deltas = parser.parse_chunk(&chunk).map_err(|err| {
            common::model_error(
                AiErrorKind::ServerError,
                format!("invalid Devin Connect stream: {err}"),
            )
        })?;
        emit_deltas(
            host,
            &mut accumulator,
            &deltas,
            &mut produced_output,
            parser.protected_reasoning_rejected(),
        )?;
    }
    let deltas = parser.finish().map_err(|err| {
        common::model_error(
            AiErrorKind::ServerError,
            format!("incomplete Devin Connect stream: {err}"),
        )
    })?;
    emit_deltas(
        host,
        &mut accumulator,
        &deltas,
        &mut produced_output,
        parser.protected_reasoning_rejected(),
    )?;
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(OperationOutput::Infer(Box::new(complete)))
}

fn emit_deltas(
    host: &GuestHost,
    accumulator: &mut StreamResponseAccumulator,
    deltas: &[AiStreamDelta],
    produced_output: &mut bool,
    protected_reasoning_rejected: bool,
) -> Result<(), PluginError> {
    // 首个终止错误不能先提交成流事件，否则宿主已提交输出，无法恢复请求。
    // 同批的模型标识与用量仍先交给宿主暂存，失败尝试保留已报告用量。
    if let Some(error) =
        uncommitted_stream_error(deltas, *produced_output, protected_reasoning_rejected)
    {
        for delta in deltas.iter().filter(|delta| !is_terminal(delta)) {
            host.emit_delta(delta)?;
            accumulator.apply(delta);
        }
        return Err(error);
    }
    let terminal = deltas.iter().find_map(|delta| match delta {
        AiStreamDelta::StreamError { error } => Some((error.kind.clone(), error.status_code)),
        AiStreamDelta::UnexpectedEof => Some((AiErrorKind::UnexpectedEof, None)),
        _ => None,
    });
    for delta in deltas {
        host.emit_delta(delta)?;
        *produced_output |= is_model_output(delta);
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

fn is_terminal(delta: &AiStreamDelta) -> bool {
    matches!(
        delta,
        AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
    )
}

fn uncommitted_stream_error(
    deltas: &[AiStreamDelta],
    produced_output: bool,
    protected_reasoning_rejected: bool,
) -> Option<PluginError> {
    if produced_output || deltas.iter().any(is_model_output) {
        return None;
    }
    deltas.iter().find_map(|delta| {
        let (kind, status) = match delta {
            AiStreamDelta::StreamError { error } => (error.kind.clone(), error.status_code),
            AiStreamDelta::UnexpectedEof => (AiErrorKind::UnexpectedEof, None),
            _ => return None,
        };
        Some(PluginError {
            kind: if protected_reasoning_rejected {
                ErrorKind::ProtectedReasoningRejected
            } else {
                ErrorKind::upstream(Some(kind), None)
            },
            message: "upstream inference stream ended with an error".into(),
            upstream_status: status,
        })
    })
}

fn resolve_assignment(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    token: &str,
    (router_uid, cascade_id): (&str, &str),
    platform: DevinClientPlatform,
    state: Option<(&stravia_vendor_sdk::state::Cell, &PrivateState)>,
    required: bool,
) -> Result<Option<ModelAssignmentState>, PluginError> {
    let key = format!("{router_uid}|{cascade_id}");
    if let Some(assignment) = state.and_then(|(_, state)| state.assignments.get(&key)) {
        return Ok(Some(assignment.clone()));
    }
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: endpoint(provider, ASSIGN_MODEL_PATH),
            headers: crate::net_fields(connect_headers(token, false)),
            body: encode_assign_model_request_with_platform(
                token,
                router_uid,
                cascade_id,
                platform,
                &crate::fingerprint(provider, token)?,
            ),
        },
        &crate::net_options(),
    );
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
            return Err(http_error(&response, status, "Devin AssignModel", false));
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
    if let (Some(assignment), Some((cell, _))) = (&assignment, state) {
        update_state(cell, |state| {
            state.record_assignment(&key, assignment);
        })?;
    }
    Ok(assignment)
}

fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<DiscoverResponse, PluginError> {
    let token = session_token(provider)?;
    let platform = client_platform(provider)?;
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: endpoint(provider, CATALOG_PATH),
            headers: crate::net_fields(catalog_headers(token)),
            body: encode_client_metadata_request_with_platform(
                token,
                true,
                platform,
                &crate::fingerprint(provider, token)?,
            ),
        },
        &crate::net_options(),
    )?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(
            &response,
            status,
            "Devin model discovery",
            false,
        ));
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
    if streaming {
        return vec![
            ("authorization".into(), format!("Basic {token}-{token}")),
            ("sentry-trace".into(), format!("{trace}-{}-1", &span[..16])),
            ("content-type".into(), "application/connect+proto".into()),
            ("connect-protocol-version".into(), "1".into()),
            ("accept".into(), "*/*".into()),
            ("host".into(), String::new()),
            ("content-length".into(), String::new()),
        ];
    }
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

fn decode_state(bytes: Option<&[u8]>) -> Result<PrivateState, PluginError> {
    match bytes {
        None => Ok(PrivateState::default()),
        Some(bytes) => serde_json::from_slice(bytes)
            .map_err(|_| error(ErrorKind::Invalid, "invalid Devin business state")),
    }
}

fn encode_state(state: &PrivateState) -> Result<Vec<u8>, PluginError> {
    serde_json::to_vec(state).map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Devin business state: {err}"),
        )
    })
}

trait BusinessCell {
    fn load(&self)
    -> Result<stravia_vendor_sdk::state::Snapshot, stravia_vendor_sdk::state::Error>;
    fn compare_exchange(
        &self,
        revision: u64,
        bytes: &[u8],
    ) -> Result<(), stravia_vendor_sdk::state::Error>;
}

impl BusinessCell for stravia_vendor_sdk::state::Cell {
    fn load(
        &self,
    ) -> Result<stravia_vendor_sdk::state::Snapshot, stravia_vendor_sdk::state::Error> {
        stravia_vendor_sdk::state::Cell::load(self)
    }

    fn compare_exchange(
        &self,
        revision: u64,
        bytes: &[u8],
    ) -> Result<(), stravia_vendor_sdk::state::Error> {
        stravia_vendor_sdk::state::Cell::compare_exchange(self, revision, Some(bytes))
    }
}

fn read_state(
    host: &GuestHost,
) -> Result<(stravia_vendor_sdk::state::Cell, PrivateState), PluginError> {
    use stravia_vendor_sdk::state::{self, Scope};

    let cell = state::open(Scope::Provider, "devin-business-v1").map_err(PluginError::from)?;
    let state = initialize_state(&cell, || host.read_private_state())?;
    Ok((cell, state))
}

fn initialize_state(
    cell: &impl BusinessCell,
    legacy: impl FnOnce() -> Result<Option<Vec<u8>>, PluginError>,
) -> Result<PrivateState, PluginError> {
    let snapshot = cell.load().map_err(PluginError::from)?;
    if snapshot.revision == 0 && snapshot.payload.is_none() {
        // Publish the legacy snapshot once. After a competing initializer wins,
        // reload its value rather than importing stale private state again.
        // The obsolete source is not rewritten: OAuth also uses private state,
        // and its non-CAS API cannot safely clear only the old business fields.
        let legacy = legacy()?;
        let state = decode_state(legacy.as_deref())?;
        let bytes = encode_state(&state)?;
        match cell.compare_exchange(0, &bytes) {
            Ok(()) => return Ok(state),
            Err(stravia_vendor_sdk::state::Error::Conflict) => {}
            Err(error) => return Err(error.into()),
        }
        let snapshot = cell.load().map_err(PluginError::from)?;
        return decode_state(snapshot.payload.as_deref());
    }
    decode_state(snapshot.payload.as_deref())
}

fn update_state<T>(
    cell: &impl BusinessCell,
    mut update: impl FnMut(&mut PrivateState) -> T,
) -> Result<T, PluginError> {
    loop {
        let snapshot = cell.load().map_err(PluginError::from)?;
        let mut state = decode_state(snapshot.payload.as_deref())?;
        let result = update(&mut state);
        let bytes = encode_state(&state)?;
        match cell.compare_exchange(snapshot.revision, &bytes) {
            Ok(()) => return Ok(result),
            Err(stravia_vendor_sdk::state::Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn advance_session(
    cell: &impl BusinessCell,
    shape: SessionShape,
) -> Result<SessionShape, PluginError> {
    update_state(cell, |state| {
        state.advance_session(SessionShape {
            trajectory_id: shape.trajectory_id.clone(),
            cascade_id: shape.cascade_id.clone(),
            step_index: shape.step_index,
        })
    })
}

fn http_error(
    response: &stravia_vendor_sdk::HttpResponse,
    status: u16,
    label: &str,
    protected_reasoning_untrusted: bool,
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
    if let Some(kind) = connect_http_error_kind(&bytes, protected_reasoning_untrusted) {
        error.kind = kind;
    }
    error.message = format!("{label} returned HTTP {status}: {}", error.message);
    error
}

fn connect_http_error_kind(bytes: &[u8], untrusted: bool) -> Option<ErrorKind> {
    let body: Value = serde_json::from_slice(bytes).ok()?;
    let code = body.get("code").and_then(Value::as_str)?;
    // invalid_argument 本身不构成推理拒绝证据，必须结合宿主的来源声明。
    (code == "invalid_argument").then(|| {
        if untrusted {
            ErrorKind::ProtectedReasoningRejected
        } else {
            ErrorKind::upstream(Some(AiErrorKind::InvalidRequest), None)
        }
    })
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    struct TestCell {
        snapshot: std::cell::RefCell<stravia_vendor_sdk::state::Snapshot>,
        concurrent_payload: std::cell::RefCell<Option<Vec<u8>>>,
    }

    impl TestCell {
        fn new(revision: u64, payload: Option<Vec<u8>>) -> Self {
            Self {
                snapshot: std::cell::RefCell::new(stravia_vendor_sdk::state::Snapshot {
                    revision,
                    payload,
                }),
                concurrent_payload: std::cell::RefCell::new(None),
            }
        }
    }

    impl BusinessCell for TestCell {
        fn load(
            &self,
        ) -> Result<stravia_vendor_sdk::state::Snapshot, stravia_vendor_sdk::state::Error> {
            Ok(self.snapshot.borrow().clone())
        }

        fn compare_exchange(
            &self,
            revision: u64,
            bytes: &[u8],
        ) -> Result<(), stravia_vendor_sdk::state::Error> {
            let mut snapshot = self.snapshot.borrow_mut();
            if let Some(payload) = self.concurrent_payload.borrow_mut().take() {
                snapshot.revision += 1;
                snapshot.payload = Some(payload);
            }
            if snapshot.revision != revision {
                return Err(stravia_vendor_sdk::state::Error::Conflict);
            }
            snapshot.revision += 1;
            snapshot.payload = Some(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn business_state_imports_legacy_only_before_first_publication() {
        let cell = TestCell::new(0, None);
        let imported =
            initialize_state(&cell, || Ok(Some(br#"{"next_steps":{"old":9}}"#.to_vec()))).unwrap();
        assert_eq!(imported.next_steps["old"], 9);
        let reopened = initialize_state(&cell, || panic!("legacy must not be read again")).unwrap();
        assert_eq!(reopened.next_steps["old"], 9);
        let deleted = TestCell::new(8, None);
        let state =
            initialize_state(&deleted, || panic!("tombstone must not reimport legacy")).unwrap();
        assert!(state.next_steps.is_empty());
        assert!(state.assignments.is_empty());
    }

    #[test]
    fn competing_initializer_wins_without_reimporting_legacy() {
        let cell = TestCell::new(0, None);
        *cell.concurrent_payload.borrow_mut() = Some(br#"{"next_steps":{"published":4}}"#.to_vec());
        let state = initialize_state(&cell, || {
            Ok(Some(br#"{"next_steps":{"stale":2}}"#.to_vec()))
        })
        .unwrap();
        assert_eq!(state.next_steps["published"], 4);
        assert!(!state.next_steps.contains_key("stale"));
    }

    #[test]
    fn concurrent_affinity_update_preserves_other_steps_and_assignments() {
        let cell = TestCell::new(1, Some(encode_state(&PrivateState::default()).unwrap()));
        let mut other = PrivateState::default();
        let other_shape = other.advance_session(session_shape(Some("other")));
        let key = format!("router|{}", other_shape.cascade_id);
        other.record_assignment(
            &key,
            &ModelAssignmentState {
                jwt: "other-jwt".into(),
                model_uid: "other-model".into(),
            },
        );
        *cell.concurrent_payload.borrow_mut() = Some(encode_state(&other).unwrap());
        let current = advance_session(&cell, session_shape(Some("current"))).unwrap();
        assert_eq!(current.step_index, 0);
        let mut state = decode_state(cell.load().unwrap().payload.as_deref()).unwrap();
        assert_eq!(state.assignments[&key].jwt, "other-jwt");
        assert_eq!(
            state
                .advance_session(session_shape(Some("other")))
                .step_index,
            2
        );
        assert_eq!(
            state
                .advance_session(session_shape(Some("current")))
                .step_index,
            2
        );
    }

    #[test]
    fn interactive_inference_headers_match_cli_order() {
        let headers = connect_headers("token", true);
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "authorization",
                "sentry-trace",
                "content-type",
                "connect-protocol-version",
                "accept",
                "host",
                "content-length"
            ]
        );
        assert_eq!(headers[0].1, "Basic token-token");
        assert_eq!(headers[2].1, "application/connect+proto");
        assert!(headers[5].1.is_empty());
        assert!(headers[6].1.is_empty());
    }

    #[test]
    fn connect_rejection_requires_json_true_declaration() {
        let mut provider = ProviderSnapshot {
            provider_id: "devin".into(),
            channel: "devin".into(),
            base_url: DEFAULT_BASE_URL.into(),
            protocol: "devin-connect".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: None,
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        };
        let body = br#"{"code":"invalid_argument","message":"invalid request"}"#;
        assert!(!protected_reasoning_untrusted(&provider));
        for value in [
            Value::Null,
            Value::Bool(false),
            Value::String("true".into()),
            Value::from(1),
            Value::Bool(true),
        ] {
            let expected_rejection = value == Value::Bool(true);
            provider
                .operation_metadata
                .insert("protected_reasoning_untrusted".into(), value);
            let kind =
                connect_http_error_kind(body, protected_reasoning_untrusted(&provider)).unwrap();
            assert_eq!(
                matches!(kind, ErrorKind::ProtectedReasoningRejected),
                expected_rejection
            );
            if !expected_rejection {
                assert_eq!(kind.model_error_kind(), Some(AiErrorKind::InvalidRequest));
            }
        }
        assert_eq!(
            connect_http_error_kind(body, false)
                .unwrap()
                .model_error_kind(),
            Some(AiErrorKind::InvalidRequest)
        );
        assert!(connect_http_error_kind(br#"{"code":"permission_denied"}"#, true).is_none());
    }

    #[test]
    fn errors_before_stream_output_return_without_committing_terminal_events() {
        use stravia_runtime_contract::protocol::ir::AiError;
        for rejected in [false, true] {
            let deltas = [AiStreamDelta::StreamError {
                error: AiError::new(AiErrorKind::InvalidRequest, "rejected"),
            }];
            let failure = uncommitted_stream_error(&deltas, false, rejected).unwrap();
            assert_eq!(
                matches!(failure.kind, ErrorKind::ProtectedReasoningRejected),
                rejected
            );
            if !rejected {
                assert_eq!(
                    failure.kind.model_error_kind(),
                    Some(AiErrorKind::InvalidRequest)
                );
            }
            assert!(uncommitted_stream_error(&deltas, true, rejected).is_none());
        }
        assert!(uncommitted_stream_error(&[AiStreamDelta::UnexpectedEof], false, false).is_some());
        assert!(uncommitted_stream_error(&[], false, false).is_none());
        let error = AiStreamDelta::StreamError {
            error: AiError::new(AiErrorKind::InvalidRequest, "rejected"),
        };
        let metadata = AiStreamDelta::MessageStart {
            id: String::new(),
            model: "resolved".into(),
        };
        let failure = uncommitted_stream_error(&[metadata, error.clone()], false, true).unwrap();
        assert!(matches!(
            failure.kind,
            ErrorKind::ProtectedReasoningRejected
        ));
        let partial = [AiStreamDelta::TextDelta("partial".into()), error];
        assert!(uncommitted_stream_error(&partial, false, true).is_none());
    }

    #[test]
    fn persisted_state_preserves_native_steps_across_restart_and_principal_boundaries() {
        let affinity = Some("principal-a/target-a/session");
        let mut state = PrivateState::default();
        let first = state.advance_session(session_shape(affinity));
        assert_eq!(first.step_index, 0);
        assert_eq!(wire_step(&first), None);
        // These are the same serde bytes written/read by production private state.
        let bytes = serde_json::to_vec(&state).unwrap();
        let mut restored: PrivateState = serde_json::from_slice(&bytes).unwrap();
        let second = restored.advance_session(session_shape(affinity));
        assert_eq!(second.step_index, 2);
        assert_eq!(wire_step(&second), Some(2));
        assert_eq!(second.cascade_id, first.cascade_id);
        let bytes = serde_json::to_vec(&restored).unwrap();
        let mut restored: PrivateState = serde_json::from_slice(&bytes).unwrap();
        let third = restored.advance_session(session_shape(affinity));
        assert_eq!(third.step_index, 3);
        assert_eq!(wire_step(&third), Some(3));
        assert_eq!(third.trajectory_id, first.trajectory_id);
        assert_eq!(
            restored
                .advance_session(session_shape(Some("principal-b/target-a/session")))
                .step_index,
            0
        );
        assert_eq!(restored.advance_session(session_shape(None)).step_index, 0);
        assert_eq!(restored.advance_session(session_shape(None)).step_index, 0);
    }

    fn wire_step(shape: &SessionShape) -> Option<u64> {
        use crate::codec::devin_connect::proto::parse_fields;
        let request = stravia_runtime_contract::protocol::ir::AiRequest::new("swe-2", Vec::new());
        let body = encode_prepared_request(
            &request,
            "test-credential",
            shape,
            None,
            DevinClientPlatform::Linux,
            "test-fingerprint",
        )
        .unwrap();
        let fields = parse_fields(&body).unwrap();
        let trajectory = fields.iter().find(|field| field.number == 15).unwrap();
        parse_fields(trajectory.bytes)
            .unwrap()
            .iter()
            .find(|field| field.number == 2)
            .map(|field| field.scalar)
    }

    #[test]
    fn new_sessions_do_not_reset_existing_native_steps() {
        let mut state = PrivateState::default();
        for index in 0..256 {
            let affinity = format!("scoped/{index}");
            assert_eq!(
                state
                    .advance_session(session_shape(Some(&affinity)))
                    .step_index,
                0
            );
        }
        let resumed = state.advance_session(session_shape(Some("scoped/0")));
        assert_eq!(resumed.step_index, 2);
        let fresh = state.advance_session(session_shape(Some("scoped/new")));
        assert_eq!(fresh.step_index, 0);
        assert_eq!(
            state
                .advance_session(session_shape(Some("scoped/0")))
                .step_index,
            3
        );
        assert_eq!(
            state
                .advance_session(session_shape(Some("scoped/new")))
                .step_index,
            2
        );
    }

    #[test]
    fn assignment_growth_preserves_cascade_bindings_and_native_steps() {
        let mut state = PrivateState::default();
        let first = state.advance_session(session_shape(Some("active-session")));
        let first_key = format!("router|{}", first.cascade_id);
        state.record_assignment(
            &first_key,
            &ModelAssignmentState {
                jwt: "active-jwt".into(),
                model_uid: "active-model".into(),
            },
        );
        for index in 0..80 {
            state.record_assignment(
                &format!("router|cascade-{index}"),
                &ModelAssignmentState {
                    jwt: format!("jwt-{index}"),
                    model_uid: format!("model-{index}"),
                },
            );
        }
        let mut restored = decode_state(Some(&encode_state(&state).unwrap())).unwrap();
        assert_eq!(restored.assignments[&first_key].jwt, "active-jwt");
        assert_eq!(restored.assignments[&first_key].model_uid, "active-model");
        let resumed = restored.advance_session(session_shape(Some("active-session")));
        assert_eq!(resumed.cascade_id, first.cascade_id);
        assert_eq!(resumed.step_index, 2);
    }

    #[test]
    fn legacy_auth_fields_are_not_imported_into_business_state() {
        let legacy = br#"{
            "state":"pending-auth",
            "redirect_uri":"http://localhost/callback",
            "code_verifier":"pending-verifier",
            "manual":false,
            "assignments":{"router|cascade":{"jwt":"bound-jwt","model_uid":"resolved"}},
            "next_steps":{"trajectory":7}
        }"#;
        let state = decode_state(Some(legacy)).unwrap();
        let migrated: Value = serde_json::from_slice(&encode_state(&state).unwrap()).unwrap();
        assert_eq!(
            migrated["assignments"]["router|cascade"]["jwt"],
            "bound-jwt"
        );
        assert_eq!(migrated["next_steps"]["trajectory"], 7);
        assert!(migrated.get("code_verifier").is_none());
        assert!(migrated.get("state").is_none());
    }
}
