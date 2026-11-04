//! Claude 订阅（Claude Code OAuth）专用供应商插件。
//!
//! 使用 Claude Pro/Max 账号的 OAuth 令牌调用 Anthropic Messages API。请求以
//! 官方 2.1.317 的真实多轮流量为基准，并保留自定义工具的 `_` 名称隔离
//! （见 [`request`]）；客户端仍使用原始工具名。

mod allowance;
mod auth;
mod cache;
mod capabilities;
mod request;
mod session;
mod thinking;
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
use stravia_vendor_sdk::{
    AuthCallback, AuthCallbackPort, AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType,
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, DataCompatibility, DiscoverRequest, DiscoverResponse, ErrorKind, GuestHost,
    HttpRequest, NetworkDeclaration, Operation, OperationInput, OperationOutput, OriginDeclaration,
    PluginError, ProviderDescriptor, ProviderSnapshot, VendorDescriptor, VendorKind,
    read_http_body,
};

use request::{AccountIdentity, DEFAULT_CLIENT_VERSION};
use session::{SessionStore, ThreadMode};

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
        Capability::ConnectionCheck,
    ]);
    VendorDescriptor {
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
        OperationInput::ConnectionCheck {
            provider,
            request: _,
        } => allowance::connection_check(host, &provider).map(OperationOutput::ConnectionCheck),
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
    let compaction_control = (operation == Operation::Compact)
        .then(|| {
            stravia_protocol_codec::codec::compaction::take_anthropic_compaction_control(
                &mut request,
            )
        })
        .flatten();
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
    let identity = AccountIdentity {
        account_uuid: credential(provider, auth::ACCOUNT_UUID),
        device_id: credential(provider, auth::DEVICE_ID),
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
    let scope = thread_scope(provider, &session_id);
    let state = if compact {
        None
    } else {
        host.read_private_state()?
    };
    let store = SessionStore::from_bytes(state.as_deref()).map_err(invalid)?;
    let mut plan = store
        .plan(&scope, session_id, &body, now_ms()?)
        .map_err(invalid)?;
    // 原生压缩块只能作为完整窗口回放：独立压缩和携带压缩块的历史都必须发送全量
    // 历史，不能续接线程或按 offset 切片。无压缩历史的会话保持原有线程行为。
    if compact || body.as_object().is_some_and(request::carries_compaction) {
        plan.context.mode = ThreadMode::Disabled;
        plan.context.message_offset = 0;
        plan.context.previous_message_id = None;
    }
    let url = format!(
        "{}?beta=true",
        common::endpoint_url(&provider.base_url, &encoded.path)?
    );
    for attempt in 0..2 {
        let shaped = request::encode(&body, client_version, &identity, &plan.context, compact)
            .map_err(invalid)?;
        let headers = request::inference_headers(
            access_token(provider)?,
            client_version,
            &plan.context,
            shaped.betas,
        );
        // 每次实际模型请求之前都声明一次开始；恢复重放前宿主已经重置开始槽位。
        host.emit_started()?;
        let response = host.http_start_with_compression(
            HttpRequest {
                method: "POST".into(),
                url: url.clone(),
                headers,
                body: shaped.body,
            },
            stravia_vendor_sdk::HttpCompression {
                request: None,
                response: vec![
                    stravia_vendor_sdk::HttpContentCoding::Gzip,
                    stravia_vendor_sdk::HttpContentCoding::Deflate,
                    stravia_vendor_sdk::HttpContentCoding::Br,
                    stravia_vendor_sdk::HttpContentCoding::Zstd,
                ],
            },
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
            if attempt == 0
                && let Some(mode) = error_value
                    .as_ref()
                    .and_then(|value| thread_recovery(status, value, plan.context.mode))
            {
                // 这些拒绝发生在模型执行前。只做协议规定的一次全量重放；
                // 网络/配额/认证错误及已经发出的流式输出不进入此路径。
                // 该拒绝由本插件在内部消化：不发 Failed 事件。先向宿主声明
                // “已确认未执行”以重新装填单次发送门禁；被拒绝则作为最终错误返回。
                // 本插件已持有完整 canonical 历史，宿主返回的可选历史无需使用。
                host.replay_continuation()?;
                plan.context.mode = mode;
                plan.context.message_offset = 0;
                plan.context.previous_message_id = None;
                continue;
            }
            let mut error = error_value
                .as_ref()
                .and_then(|value| common::classify_anthropic_error(value, false))
                .unwrap_or_else(|| common::upstream_error(status, &response_headers, &error_body));
            error.upstream_status.get_or_insert(status);
            return Err(error);
        }
        let complete = common::decode_ai_response_renaming_tools(
            host,
            &protocol,
            response,
            common::classify_anthropic_error,
            request::strip_tool_prefix,
        )?;
        if provider
            .operation_metadata
            .get("transport_affinity")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        {
            // 只投影本轮输出，不复制完整历史；和下轮请求复用同一个 Anthropic
            // 编码器，避免签名、工具调用或内容块的另一套哈希表示。
            let save = (|| {
                let reusable = complete.error.is_none()
                    && matches!(
                        complete.stop_reason.as_deref(),
                        Some("stop" | "tool_calls" | "pause_turn")
                    );
                let mut response_history = reusable
                    .then(|| {
                        common::encode_inference_request(
                            &protocol,
                            AiRequest::new(&complete.model, complete.items.clone()),
                        )
                    })
                    .transpose()?;
                let response_messages = match &mut response_history {
                    Some(history) => {
                        let messages = history
                            .body
                            .get_mut("messages")
                            .and_then(Value::as_array_mut)
                            .ok_or_else(|| {
                                invalid("Claude Code response history is not an array")
                            })?;
                        request::prefix_message_tool_names(messages);
                        messages.as_slice()
                    }
                    None => &[],
                };
                // 网络调用期间其它会话可能更新连接私有状态；提交前重新读取，
                // 保留它们以及 OAuth 流程写入的非线程字段。
                let latest = host.read_private_state()?;
                let mut latest = SessionStore::from_bytes(latest.as_deref()).map_err(invalid)?;
                let request_id = response_headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("request-id"))
                    .map(|(_, value)| value.as_str());
                latest
                    .commit(
                        plan,
                        response_messages,
                        &complete.id,
                        request_id,
                        now_ms()?,
                        reusable,
                    )
                    .map_err(invalid)?;
                host.write_private_state(&latest.to_bytes().map_err(invalid)?)
            })();
            if let Err(error) = save {
                // 已完成的模型响应不因可丢弃的续接优化失效而变成生成失败。
                host.log(
                    stravia_vendor_sdk::LogLevel::Warn,
                    &format!("Claude Code thread state was not saved: {error}"),
                );
            }
        }
        return Ok(OperationOutput::Infer(Box::new(complete)));
    }
    unreachable!("the second attempt always returns; only the first may replay")
}

fn thread_scope(provider: &ProviderSnapshot, session_id: &str) -> String {
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

fn thread_recovery(status: u16, error: &Value, mode: ThreadMode) -> Option<ThreadMode> {
    if mode == ThreadMode::Disabled || !matches!(status, 400 | 404) {
        return None;
    }
    match error
        .pointer("/error/details/error_code")
        .and_then(Value::as_str)
    {
        Some(
            "thread_not_found"
            | "thread_already_continued"
            | "thread_continue_in_flight"
            | "thread_fingerprint_mismatch",
        ) if mode == ThreadMode::Continue => Some(ThreadMode::Create),
        Some("thread_unsupported_request") => Some(ThreadMode::Disabled),
        _ => {
            let message = error.pointer("/error/message").and_then(Value::as_str)?;
            (status == 400
                && message.contains("message-threads-2026-08-12")
                && (message.contains("Unsupported") || message.contains("invalid beta")))
            .then_some(ThreadMode::Disabled)
        }
    }
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
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url,
        headers: vec![
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
        body: Vec::new(),
    })?;
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
