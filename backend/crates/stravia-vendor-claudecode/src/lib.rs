//! Claude 订阅（Claude Code OAuth）专用供应商插件。
//!
//! 使用 Claude Pro/Max 账号的 OAuth 令牌调用 Anthropic Messages API。请求以
//! 官方 2.1.319 与 2.1.323 的第一方交互流量为基准，并保留自定义工具的 `_` 名称隔离
//! （见 [`request`]）；客户端仍使用原始工具名。

mod allowance;
mod auth;
mod cache;
mod capabilities;
mod request;
mod session;
mod thinking;
mod tool_ledger;
mod xxhash;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::BTreeSet;

use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiRequest};
use stravia_vendor_common::common;
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::net::{Coding, Fields, Options, Request};
use stravia_vendor_sdk::{
    AuthCallback, AuthCallbackPort, AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType,
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, DataCompatibility, DiscoverRequest, DiscoverResponse, ErrorKind, GuestHost,
    NetworkDeclaration, Operation, OperationInput, OperationOutput, OriginDeclaration, PluginError,
    ProviderDescriptor, ProviderSnapshot, VendorDescriptor, VendorKind, read_http_body,
};

fn tools_used_header(names: &[String], truncated: bool) -> Result<String, PluginError> {
    const UNKNOWN: &str = "unreadable-tools-used-before-summary";
    const LAST: &[&str] = &[
        "Bash",
        "Read",
        "Edit",
        "Write",
        "Glob",
        "Grep",
        "NotebookEdit",
        "TodoWrite",
        "Agent",
        "Task",
        "ToolSearch",
        "EnterPlanMode",
        "ExitPlanMode",
    ];
    let mut names_in_order = Vec::new();
    for name in names {
        let name = name.as_str();
        let valid = !name.is_empty()
            && name.len() <= 256
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
        let name = if valid { name } else { UNKNOWN };
        if !names_in_order.contains(&name) {
            names_in_order.push(name);
        }
    }
    names_in_order.sort_by_key(|name| LAST.contains(name));
    let encode_prefix = |include_unknown: bool| {
        let mut out = String::from("[");
        let mut count = 0;
        if include_unknown {
            out.push_str("{\"name\":\"unreadable-tools-used-before-summary\"}");
            count += 1;
        }
        for name in &names_in_order {
            let item = format!("{{\"name\":\"{name}\"}}");
            let extra = item.len() + if count > 0 { 1 } else { 0 };
            if count == 64 || out.len() + extra + 1 > 7168 {
                break;
            }
            if count > 0 {
                out.push(',');
            }
            out.push_str(&item);
            count += 1;
        }
        out.push(']');
        (out, count - if include_unknown { 1 } else { 0 })
    };
    let (out, sent) = encode_prefix(false);
    if truncated || sent < names_in_order.len() {
        Ok(encode_prefix(true).0)
    } else {
        Ok(out)
    }
}

fn prompt_id_for_turn(turn: usize) -> Result<String, PluginError> {
    let cell = stravia_vendor_sdk::state::open(
        stravia_vendor_sdk::state::Scope::Conversation,
        "claude-prompt-v1",
    )
    .map_err(PluginError::from)?;
    let fresh = uuid::Uuid::new_v4().to_string();
    loop {
        let snapshot = cell.load().map_err(PluginError::from)?;
        if let Some(bytes) = snapshot.payload.as_deref() {
            let existing: Value = serde_json::from_slice(bytes)
                .map_err(|_| invalid("invalid Claude prompt state"))?;
            if existing.get("turn").and_then(Value::as_u64) == Some(turn as u64) {
                return existing
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("invalid Claude prompt identity"));
            }
        }
        let bytes = serde_json::to_vec(&serde_json::json!({"turn": turn, "id": fresh}))
            .map_err(|_| invalid("cannot encode Claude prompt state"))?;
        match cell.compare_exchange(snapshot.revision, Some(&bytes)) {
            Ok(()) => return Ok(fresh),
            Err(stravia_vendor_sdk::state::Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn net_options() -> Options {
    let mut options = Options::new();
    options.shape("claude-code").expect("valid shape");
    options
        .response_decoding(&[Coding::Gzip, Coding::Deflate, Coding::Br, Coding::Zstd])
        .expect("valid response codings");
    options
}

fn net_request(
    method: &str,
    url: impl Into<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Request {
    let mut fields = Fields::new();
    for (name, value) in headers {
        if value.is_empty()
            && (name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("content-length"))
        {
            fields.slot(name);
        } else {
            fields.append(name, value.into_bytes());
        }
    }
    if fields.get("accept").is_none() {
        fields.append("Accept", b"application/json".to_vec());
    }
    if fields.get("user-agent").is_none() {
        fields.append(
            "User-Agent",
            request::cli_user_agent(DEFAULT_CLIENT_VERSION).into_bytes(),
        );
    }
    if fields.get("accept-encoding").is_none() {
        fields.append("Accept-Encoding", b"gzip, deflate, br, zstd".to_vec());
    }
    Request::new(method, url).fields(fields).body(body)
}

use request::{AccountIdentity, DEFAULT_CLIENT_VERSION};
use session::SessionStore;

const VENDOR_ID: &str = "claude-code";
const CATALOG_ID: &str = "anthropic";
const CHANNEL: &str = "oauth";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const CLIENT_VERSION_FIELD: &str = "client_version";
/// oh-my-pi 模型发现使用的 beta 集合（`openai-compat.ts` 的 `ANTHROPIC_OAUTH_BETA`）。
const DISCOVERY_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,effort-2025-11-24,extended-cache-ttl-2025-04-11";
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::Compact,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
    ]);
    VendorDescriptor {
        egress_shapes: Vec::new(),
        vendor_id: VENDOR_ID.into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "Claude Code".into(),
        description: Some(
            "Claude Pro/Max subscription OAuth for the Anthropic Messages API, with usage limits."
                .into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: Some(CATALOG_ID.into()),
            icon_svg: Some(
                include_str!("../../../../frontend/stravia-webui/src/assets/icons/anthropic.svg")
                    .to_owned(),
            ),
            display_name: "Claude Code".into(),
            description: Some(
                "Claude subscription OAuth channel shaped as the Claude Code CLI.".into(),
            ),
            channels: vec![ChannelDescriptor {
                id: CHANNEL.into(),
                name: crate::messages::channel_oauth(),
                description: Some(crate::messages::channel_description()),
                auth: Some(AuthDescriptor {
                    flow: AuthFlow::AuthorizationCode,
                    callback: Some(AuthCallback {
                        bind_host: "127.0.0.1".into(),
                        redirect_host: "localhost".into(),
                        path: "/callback".into(),
                        port: AuthCallbackPort::Fixed {
                            primary: 54545,
                            fallback: Some(54546),
                        },
                        manual_redirect_uri: Some("http://localhost:54547/callback".into()),
                        cancel_path: Some("/cancel".into()),
                    }),
                    manual_input: Some(AuthManualInput {
                        input_type: AuthManualInputType::CallbackUrl,
                        label: crate::messages::callback_url(),
                        description: Some(crate::messages::callback_description()),
                        secret: false,
                    }),
                }),
                protocol: Some("anthropic-messages".into()),
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
                id: "advanced".into(),
                label: crate::messages::advanced_group(),
            }],
            config_fields: vec![ConfigField {
                key: CLIENT_VERSION_FIELD.into(),
                label: crate::messages::client_version(),
                description: Some(crate::messages::client_version_description()),
                kind: ConfigFieldKind::String { multiline: false },
                required: false,
                default_json: None,
                group: Some("advanced".into()),
                secret: false,
                min: None,
                max: None,
                max_length: Some(32),
                pattern: Some(r"^[0-9]+\.[0-9]+\.[0-9]+$".into()),
                visible_when: None,
            }],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: vec![
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "claude.ai".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "api.anthropic.com".into(),
                        port: None,
                    },
                ],
                field_origins: Vec::new(),
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}

pub fn select_protocol(
    _operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    require_route(channel, provider)?;
    Ok(ANTHROPIC_MESSAGES_2023_06_01.to_string())
}

pub fn check_connection(
    host: &GuestHost,
    channel: &str,
    provider: ProviderSnapshot,
) -> Result<stravia_vendor_sdk::ConnectionCheckResponse, PluginError> {
    require_route(channel, &provider)?;
    allowance::connection_check(host, &provider)
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
            infer(host, &provider, request, Operation::Infer)
        }
        OperationInput::Compact { provider, request } => {
            infer(host, &provider, request, Operation::Compact)
        }
        OperationInput::Auth { provider, request } => {
            auth::execute(host, &provider, request).map(OperationOutput::Auth)
        }
        OperationInput::Discover { provider, request } => {
            discover(host, &provider, request).map(OperationOutput::Discover)
        }
        OperationInput::Allowance {
            provider,
            request: _,
        } => allowance::execute(host, &provider).map(OperationOutput::Allowance),
        other => Err(common::unsupported(
            &format!("{:?}", other.operation()),
            VENDOR_ID,
            CHANNEL,
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
    provider: &ProviderSnapshot,
    mut request: AiRequest,
    operation: Operation,
) -> Result<OperationOutput, PluginError> {
    request.model = required_model(provider)?.to_owned();
    // `max_tokens` 是 Anthropic Messages 的必填线上字段。先归一化参数，区分客户端
    // 显式给出的上限与发现数据/编码器回退补出的上限：前者沿用既有抬高规则，
    // 后者不得被思考预算突破型号上限。
    request.normalize_parameters();
    let auto_output_limit = request.generation.max_tokens.is_none();
    common::apply_anthropic_output_limit(provider, &mut request);
    let protocol = ANTHROPIC_MESSAGES_2023_06_01.to_string();
    let display = request
        .reasoning
        .summary
        .as_ref()
        .map(thinking::summary_display);
    let compaction_control = (operation == Operation::Compact).then(|| {
        stravia_protocol_codec::codec::compaction::take_anthropic_compaction_control(&mut request)
    });
    let mut encoded = common::encode_inference_request(&protocol, request)?;
    // 通用 Anthropic 编码器不输出 `thinking.display`（部分兼容上游不认该字段）；
    // 本插件的上游是 Anthropic 本身，将统一的摘要意图映射为官方展示控制。
    if let Some(display) = display
        && let Some(thinking) = encoded
            .body
            .get_mut("thinking")
            .and_then(Value::as_object_mut)
        && thinking.get("type").and_then(Value::as_str) != Some("disabled")
    {
        thinking.insert("display".into(), Value::String(display.to_owned()));
    }
    let client_version = client_version(provider);
    let parent_session_id = parent_session_id(provider);
    let installation_id = provider
        .operation_metadata
        .get("client_installation_id")
        .and_then(Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| invalid("Claude inference requires host client_installation_id"))?;
    let mut device_hash = Sha256::new();
    device_hash.update(b"stravia:claude-code:virtual-installation-device-id:v1\0");
    device_hash.update(installation_id.as_bytes());
    let device_id =
        stravia_runtime_contract::protocol::ir::canonical::hash_hex(&device_hash.finalize().into());
    let identity = AccountIdentity {
        account_uuid: credential(provider, auth::ACCOUNT_UUID),
        device_id: Some(&device_id),
        parent_session_id: parent_session_id.as_deref(),
    };
    let caps = capabilities::ModelCapabilities::from_snapshot(provider);
    thinking::preserve_implicit_summary_intent(&mut encoded.body, &caps, display);
    let compact = operation == Operation::Compact;
    if compact {
        stravia_protocol_codec::codec::compaction::prepare_anthropic_compaction_body(
            &mut encoded.body,
            compaction_control,
        )
        .map_err(|error| invalid(error.to_string()))?;
    }
    let body =
        request::prepare(encoded.body, &caps, auto_output_limit, compact).map_err(invalid)?;
    let session_id = fallback_session_id(provider);
    let scope = diagnostics_scope(provider, &session_id);
    let cell = stravia_vendor_sdk::state::open(
        stravia_vendor_sdk::state::Scope::Conversation,
        "claude-session-v1",
    )
    .map_err(PluginError::from)?;
    let state = cell.load().map_err(PluginError::from)?;
    let store = SessionStore::from_bytes(state.payload.as_deref()).map_err(invalid)?;
    let mut plan = store
        .plan(&scope, session_id, &body, now_ms()?)
        .map_err(invalid)?;
    let position = |key: &str| -> Result<Option<usize>, PluginError> {
        provider
            .operation_metadata
            .get(key)
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| invalid(format!("Claude inference received invalid host {key}")))
            })
            .transpose()
    };
    let prompt_index = position("dialogue_prompt_index")?;
    let turn_index = position("user_turn_index")?;
    if turn_index == Some(0)
        || prompt_index
            .zip(turn_index)
            .is_some_and(|(prompt, turn)| prompt > turn)
    {
        return Err(invalid(
            "Claude inference received invalid host turn position",
        ));
    }
    // 未证明的历史没有累计序数。保留 SessionStore 的提示身份并省略计费头计数，
    // 不能因可选诊断缺失拒绝推理，也不能按压缩后窗口的消息数补造计数。
    if let (Some(prompt_index), Some(turn_index)) = (prompt_index, turn_index) {
        plan.context.prompt_index = prompt_index;
        plan.context.turn_index = turn_index;
        plan.context.prompt_id = prompt_id_for_turn(turn_index)?;
    }
    let url = format!(
        "{}?beta=true",
        common::endpoint_url(&provider.base_url, &encoded.path)?
    );
    let shaped = request::encode(
        &body,
        client_version,
        &identity,
        &plan.context,
        &caps,
        compact,
    )
    .map_err(invalid)?;
    let tool_cell = tool_ledger::open()?;
    let tools_used = tool_ledger::seed(&tool_cell, &body)?;
    let headers = request::inference_headers(
        access_token(provider)?,
        client_version,
        &plan.context,
        shaped.betas,
        tools_used,
    );
    // 每次实际模型请求之前都声明一次开始；宿主恢复重放时已经重置开始槽位。
    host.emit_started()?;
    let response = host.http_start(
        net_request("POST", url, headers, shaped.body),
        &net_options(),
    )?;
    if compact {
        return common::decode_anthropic_compaction_preserving_upstream_errors(host, response)
            .map(OperationOutput::Compact);
    }
    let status = response.status()?;
    let response_headers = response.headers()?;
    if !(200..300).contains(&status) {
        let error_body = read_http_body(&response, 256 * 1024)?;
        let error_value = serde_json::from_slice::<Value>(&error_body).ok();
        let mut error = error_value
            .as_ref()
            .and_then(|value| common::classify_anthropic_error(value, false))
            .unwrap_or_else(|| common::upstream_error(status, &response_headers, &error_body));
        error.upstream_status.get_or_insert(status);
        return Err(error);
    }
    let mut recorded_tools = BTreeSet::new();
    let complete = common::decode_ai_response_renaming_tools_with_callback(
        host,
        &protocol,
        response,
        common::classify_anthropic_error,
        request::strip_tool_prefix,
        &mut |name| {
            if recorded_tools.insert(name.to_owned()) {
                tool_ledger::handoff(&tool_cell, &body, name)?;
            }
            Ok(())
        },
    )?;
    if complete.error.is_none()
        && provider
            .operation_metadata
            .get("transport_affinity")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    {
        // Successful responses update scoped diagnostics without retaining content.
        let save = (|| {
            let request_id = response_headers
                .get("request-id")
                .and_then(|bytes| std::str::from_utf8(bytes).ok());
            let committed_at = now_ms()?;
            loop {
                let snapshot = cell.load().map_err(PluginError::from)?;
                let mut latest =
                    SessionStore::from_bytes(snapshot.payload.as_deref()).map_err(invalid)?;
                latest
                    .commit(plan.clone(), &complete.id, request_id, committed_at)
                    .map_err(invalid)?;
                let bytes = latest.to_bytes().map_err(invalid)?;
                match cell.compare_exchange(snapshot.revision, Some(&bytes)) {
                    Ok(()) => break Ok(()),
                    Err(stravia_vendor_sdk::state::Error::Conflict) => continue,
                    Err(error) => break Err(PluginError::from(error)),
                }
            }
        })();
        if let Err(error) = save {
            // 已完成的模型响应不因可丢弃的诊断状态失效而变成生成失败。
            host.log(
                stravia_vendor_sdk::LogLevel::Warn,
                &format!("Claude Code diagnostics state was not saved: {error}"),
            );
        }
    }
    Ok(OperationOutput::Infer(Box::new(complete)))
}

fn diagnostics_scope(provider: &ProviderSnapshot, session_id: &str) -> String {
    let mut hash = Sha256::new();
    for part in [
        "interactive-cli",
        session_id,
        credential(provider, auth::ACCOUNT_UUID).unwrap_or_default(),
        provider.base_url.as_str(),
        client_version(provider),
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    stravia_runtime_contract::protocol::ir::canonical::hash_hex(&hash.finalize().into())
}

fn now_ms() -> Result<i64, PluginError> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| common::plugin_error(ErrorKind::Trapped, error.to_string()))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|error| common::plugin_error(ErrorKind::Trapped, error.to_string()))
}

/// 仅从宿主 principal/target scoped 传输亲和键派生稳定的 UUID 形态会话 ID，
/// 使同一会话的请求共享 `X-Claude-Code-Session-Id`；无亲和键时每次请求新建。
fn fallback_session_id(provider: &ProviderSnapshot) -> String {
    let Some(affinity) = provider
        .operation_metadata
        .get("transport_affinity")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return uuid::Uuid::new_v4().to_string();
    };
    native_session_id(affinity)
}

/// A host-scoped parent label is lineage only, not proof that a parent session
/// exists and not response-continuation authority.
fn parent_session_id(provider: &ProviderSnapshot) -> Option<String> {
    provider
        .operation_metadata
        .get("parent_transport_affinity")
        .and_then(Value::as_str)
        .filter(|affinity| !affinity.is_empty())
        .map(native_session_id)
}

fn native_session_id(affinity: &str) -> String {
    // Preserve the published namespace and bytes; changing it would rename
    // existing sessions. In particular, whitespace belongs to the identity.
    let mut hash = Sha256::new();
    hash.update(VENDOR_ID.as_bytes());
    hash.update(b":");
    hash.update(affinity.as_bytes());
    let digest = hash.finalize();
    let bytes: [u8; 16] = digest[..16].try_into().expect("digest has 16 bytes");
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: DiscoverRequest,
) -> Result<DiscoverResponse, PluginError> {
    let mut url = common::endpoint_url(&provider.base_url, "/v1/models")?;
    if let Some(cursor) = request
        .cursor
        .as_deref()
        .filter(|cursor| !cursor.is_empty())
    {
        url.push_str("?after_id=");
        url.push_str(cursor);
    }
    let response = host.http_start(
        net_request(
            "GET",
            url,
            vec![
                ("Accept".into(), "application/json".into()),
                ("anthropic-version".into(), "2023-06-01".into()),
                (
                    "anthropic-dangerous-direct-browser-access".into(),
                    "true".into(),
                ),
                ("anthropic-beta".into(), DISCOVERY_BETAS.into()),
                (
                    "authorization".into(),
                    format!("Bearer {}", access_token(provider)?),
                ),
            ],
            Vec::new(),
        ),
        &net_options(),
    )?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_MODELS_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| retryable("Claude model discovery returned invalid JSON"))?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| retryable("Claude model discovery response has no model list"))?;
    let models = entries
        .iter()
        .filter_map(capabilities::discovered_model)
        .collect();
    let next_cursor = (value.get("has_more").and_then(Value::as_bool) == Some(true))
        .then(|| value.get("last_id").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned);
    Ok(DiscoverResponse {
        models,
        next_cursor,
    })
}

fn client_version(provider: &ProviderSnapshot) -> &str {
    provider
        .options
        .get(CLIENT_VERSION_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_CLIENT_VERSION)
}

fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn access_token(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    credential(provider, "access_token")
        .ok_or_else(|| auth_error("missing credential `access_token`"))
}

fn required_model(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "*")
        .ok_or_else(|| invalid("Claude Code inference requires an upstream model"))
}

pub(crate) fn invalid(message: impl Into<String>) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

pub(crate) fn auth_error(message: impl Into<String>) -> PluginError {
    common::plugin_error(ErrorKind::Auth, message)
}

pub(crate) fn unsupported(message: impl Into<String>) -> PluginError {
    common::plugin_error(ErrorKind::Unsupported, message)
}

pub(crate) fn retryable(message: impl Into<String>) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
}

#[cfg(target_arch = "wasm32")]
struct ClaudeCodeVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for ClaudeCodeVendor {
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
stravia_vendor_sdk::export_vendor!(ClaudeCodeVendor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_passes_sdk_validation() {
        descriptor().validate().expect("valid descriptor");
    }

    #[test]
    fn native_session_uses_only_host_scoped_affinity() {
        let mut provider = ProviderSnapshot {
            provider_id: VENDOR_ID.into(),
            channel: CHANNEL.into(),
            base_url: "https://api.anthropic.com".into(),
            protocol: "anthropic-messages".into(),
            options: Default::default(),
            credentials: Default::default(),
            model: Some("claude-sonnet-4-5".into()),
            model_metadata: None,
            client_headers: vec![(
                "x-claude-code-session-id".into(),
                "raw-client-session".into(),
            )],
            operation_metadata: Default::default(),
        };
        provider
            .operation_metadata
            .insert("transport_affinity".into(), "principal-target-a".into());
        let session = fallback_session_id(&provider);
        assert_eq!(fallback_session_id(&provider), session);
        assert!(parent_session_id(&provider).is_none());
        provider.operation_metadata.insert(
            "parent_transport_affinity".into(),
            "principal-target-parent".into(),
        );
        let parent = parent_session_id(&provider).unwrap();
        let mut earlier_parent_provider = provider.clone();
        earlier_parent_provider.operation_metadata.insert(
            "transport_affinity".into(),
            "principal-target-parent".into(),
        );
        assert_eq!(parent, fallback_session_id(&earlier_parent_provider));
        assert_ne!(parent, session);
        provider.client_headers[0].1 = "other-raw-client-session".into();
        assert_eq!(fallback_session_id(&provider), session);
        let body = request::prepare(
            serde_json::json!({
                "model":"claude-sonnet-4-5", "max_tokens":1024,
                "messages":[{"role":"user", "content":"hi"}],
                "metadata":{"user_id":"{\"session_id\":\"spoofed\",\"account_uuid\":\"spoofed\",\"device_id\":\"spoofed\"}"}
            }),
            &capabilities::ModelCapabilities::default(),
            false,
            false,
        ).unwrap();
        let plan = SessionStore::from_bytes(None)
            .unwrap()
            .plan("test", fallback_session_id(&provider), &body, 0)
            .unwrap();
        let shaped = request::encode(
            &body,
            DEFAULT_CLIENT_VERSION,
            &AccountIdentity {
                account_uuid: Some("native-account"),
                device_id: Some("native-device"),
                parent_session_id: Some(&parent),
            },
            &plan.context,
            &capabilities::ModelCapabilities::default(),
            false,
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&shaped.body).unwrap();
        let identity: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        let headers = request::inference_headers(
            "test-token",
            DEFAULT_CLIENT_VERSION,
            &plan.context,
            shaped.betas,
            "[]".into(),
        );
        let header_session = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-claude-code-session-id"))
            .unwrap();
        assert_eq!(
            identity["session_id"].as_str(),
            Some(header_session.1.as_str())
        );
        assert_eq!(identity["session_id"], session);
        assert_eq!(identity["parent_session_id"], parent);
        assert_eq!(identity["account_uuid"], "native-account");
        assert_eq!(identity["device_id"], "native-device");
        provider
            .operation_metadata
            .insert("transport_affinity".into(), "principal-target-b".into());
        assert_ne!(fallback_session_id(&provider), session);
        provider.operation_metadata.clear();
        assert_ne!(
            fallback_session_id(&provider),
            fallback_session_id(&provider)
        );
    }

    #[test]
    fn session_hash_preserves_full_utf8_and_legacy_vectors() {
        assert_eq!(
            native_session_id("principal-target-scope"),
            "a107661d-a7a9-43db-809b-2dc9dcd37ada"
        );
        let source = " 空白\0\r\n会话😀 ";
        assert_eq!(
            native_session_id(source),
            "0a00c8c6-848d-463f-94f9-b8c3e6ce1a14"
        );
        let mut provider = ProviderSnapshot {
            provider_id: VENDOR_ID.into(),
            channel: CHANNEL.into(),
            base_url: DEFAULT_BASE_URL.into(),
            protocol: "anthropic-messages".into(),
            options: Default::default(),
            credentials: Default::default(),
            model: Some("claude-sonnet-4-5".into()),
            model_metadata: None,
            client_headers: vec![],
            operation_metadata: Default::default(),
        };
        for input in [source, " ", "\0", "会话😀", "会话😁"] {
            provider
                .operation_metadata
                .insert("transport_affinity".into(), input.into());
            provider
                .operation_metadata
                .insert("parent_transport_affinity".into(), input.into());
            let session = fallback_session_id(&provider);
            assert_eq!(session, native_session_id(input));
            assert_eq!(session, fallback_session_id(&provider.clone()));
            assert_eq!(Some(session), parent_session_id(&provider));
        }
        assert_ne!(native_session_id(source), native_session_id(source.trim()));
        let prefix = "会话😀".repeat(4096);
        assert_ne!(
            native_session_id(&format!("{prefix}a")),
            native_session_id(&format!("{prefix}b"))
        );
    }
}
