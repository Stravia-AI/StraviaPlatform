//! OpenCode Zen 免费层专属 Vendor 插件。
//!
//! Zen 的匿名免费模型在上游执行「opencode 客户端形态」门控：`stream: true`、
//! 非空 tools 且工具名须为 opencode 真实工具名、`x-opencode-session` 头形状
//! `ses_`+随机后缀、`User-Agent` 含 `opencode/<version>=1.17+`，缺一则
//! 403 `FreeTierError`；`tool_choice` 仅接受 `"auto"`。门控绑模型不绑凭证，
//! 自带 Zen key 调免费模型同样受限；付费模型不受此门控，本插件对付费模型
//! 只做标准 OpenAI-compatible 透传。
//! 依据与实测：docs/research/opencode-free-models.md §3.4。
//!
//! 本插件独立发布：不负责 `/zen/go/v1` 付费面（该面反向拒绝带 tools 的
//! 请求），也不接管 base 的 `opencode` Profile——免费通道是一个单独的
//! `opencode-free` Provider Profile，由管理员导入本插件后新增。

mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::canonical::{hash_bytes, hash_hex};
use stravia_runtime_contract::protocol::ir::request::{ToolChoice, ToolSpec};
use stravia_runtime_contract::protocol::ir::{AiRequest, ProtocolExt};
use stravia_vendor_common::common;
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, DataCompatibility, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost,
    HttpRequest, NetworkDeclaration, Operation, OperationInput, OperationOutput, PluginError,
    ProviderDescriptor, ProviderSnapshot, VendorDescriptor, VendorKind, read_http_body,
};

const VENDOR_ID: &str = "opencode-free";
const CHANNEL_ID: &str = "default";
/// 上游线协议：`openai-compatible/chat-completions/v1`（Zen 免费层唯一可桥接面）。
const PROTOCOL: &str = "openai-compatible/chat-completions/v1";
const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/v1";
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

const FREE_MODEL_SUFFIX: &str = "-free";
/// 不带 `-free` 后缀的免费模型。免费名单随上游轮换；轮换失效时上游返回
/// 明确的模型不可用错误，此处不扩列兜底。
const UNSUFFIXED_FREE_MODELS: &[&str] = &["big-pickle"];

/// 注入的占位工具名。上游按工具名集合校验，须为 opencode 客户端真实
/// 工具名，且实测单个名字不足。
const PLACEHOLDER_TOOL_NAMES: &[&str] = &["bash", "read"];

/// 官方客户端 UA 形态（`opencode/<version>` + ai-sdk/bun runtime 串）。
/// 可通过 Provider 配置的 `userAgent` 字段覆盖，应对上游抬高版本下限。
const CLIENT_USER_AGENT: &str = "opencode/1.18.31 ai-sdk/provider-utils/4.0.40 runtime/bun/1.3.14";

fn is_free_tier_model(model: &str) -> bool {
    model.ends_with(FREE_MODEL_SUFFIX) || UNSUFFIXED_FREE_MODELS.contains(&model)
}

fn placeholder_tool(name: &str) -> ToolSpec {
    // 最小合法 schema：上游要求 parameters 存在，description 可省略以省
    // token；「不可调用」语义由追加到 instructions 的禁令承担。
    ToolSpec {
        name: name.to_owned(),
        description: None,
        parameters: json!({"type": "object", "properties": {}}),
        strict: None,
        cache_control: None,
        meta: None,
    }
}

/// 把发往免费模型的请求改写为契约形态。客户端已声明的同名工具原样保留
/// （视为客户端覆盖），只对实际注入的名字追加 instructions 禁令。
fn prepare_free_tier_request(request: &mut AiRequest) {
    request.stream.enabled = true;

    let tools = request.tools.get_or_insert_with(Vec::new);
    let injected: Vec<&'static str> = PLACEHOLDER_TOOL_NAMES
        .iter()
        .copied()
        .filter(|name| !tools.iter().any(|tool| tool.name == *name))
        .collect();
    for name in &injected {
        tools.push(placeholder_tool(name));
    }
    if injected.is_empty() {
        return;
    }

    // 上游对免费模型拒绝 `"auto"` 以外的 tool_choice（400）；占位的语义
    // 就是「声明但不可调用」，归一为 auto 是唯一可行值。
    request.tool_choice = Some(ToolChoice::Auto);

    let notice = format!(
        "The `{}` tool{} {} unavailable in this environment; never call {}.",
        injected.join("`, `"),
        if injected.len() > 1 { "s" } else { "" },
        if injected.len() > 1 { "are" } else { "is" },
        if injected.len() > 1 { "them" } else { "it" },
    );
    match &mut request.instructions {
        Some(instructions) if !instructions.trim().is_empty() => {
            instructions.push_str("\n\n");
            instructions.push_str(&notice);
        }
        instructions => *instructions = Some(notice),
    }
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.to_owned(), value));
}

fn setting<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .or_else(|| provider.options.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn random_suffix() -> String {
    // 26 位 hex 满足 `ses_`/`msg_` 形状校验；上游只查格式不查值，
    // 无客户端会话键时按请求随机即可（限流身份是公网 IP，不是 session 值）。
    uuid::Uuid::new_v4().simple().to_string()[..26].to_owned()
}

/// 从客户端显式会话键确定性派生 `ses_` 后缀：真实 opencode 客户端的
/// session 在一次会话内保持稳定，上游若按它做亲和路由（如前缀缓存），
/// 稳定值才有意义；随机化则会把会话打散。无键时退回随机。
fn session_suffix(request: &AiRequest, provider: &ProviderSnapshot) -> String {
    conversation_key(request, provider).map_or_else(random_suffix, |key| {
        hash_hex(&hash_bytes(format!("opencode-session\0{key}").as_bytes()))[..26].to_owned()
    })
}

/// 提取客户端显式声明的稳定会话键：宿主归一化结果
/// `__stravia_generation_session_id`（覆盖全部入站协议的 session 标头与
/// `prompt_cache_key`）→ Responses `prompt_cache_key`/`safety_identifier`
/// → 其它协议入站透传的同名 body 字段 → 宿主白名单内的 session 标头。
///
/// `__stravia_*` 前缀字段按约定不进上游 body；此处取用其值派生会话标头，
/// 正是该字段的存在目的。
fn conversation_key(request: &AiRequest, provider: &ProviderSnapshot) -> Option<String> {
    if let Some(key) = request
        .meta
        .vendor
        .ingress
        .get("__stravia_generation_session_id")
        .and_then(Value::as_str)
        .and_then(|value| sanitize_session_key(Some(value)))
    {
        return Some(key);
    }
    if let Some(ProtocolExt::OpenResponses(ext)) = request.ext.as_ref() {
        for value in [
            ext.prompt_cache_key.as_deref(),
            ext.safety_identifier.as_deref(),
        ] {
            if let Some(key) = sanitize_session_key(value) {
                return Some(key);
            }
        }
    }
    for field in ["prompt_cache_key", "safety_identifier"] {
        if let Some(key) = request
            .meta
            .vendor
            .ingress
            .get(field)
            .and_then(Value::as_str)
            .and_then(|value| sanitize_session_key(Some(value)))
        {
            return Some(key);
        }
    }
    for (name, value) in &provider.client_headers {
        if matches!(
            name.as_str(),
            "session-id" | "session_id" | "conversation_id" | "thread-id"
        ) && let Some(key) = sanitize_session_key(Some(value))
        {
            return Some(key);
        }
    }
    None
}

/// 会话键必须能安全进入标头值：可见 ASCII、非空、有界。
fn sanitize_session_key(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)))
    .then(|| value.to_owned())
}

/// 注入 opencode 客户端指纹头。先调用本函数再叠加 `client_headers`，
/// 使管理员显式配置的头优先于指纹缺省值。`request` 仅推理路径提供，
/// 用于派生稳定的 `x-opencode-session`；发现等无会话路径传 None。
fn apply_client_fingerprint(
    provider: &ProviderSnapshot,
    request: Option<&AiRequest>,
    headers: &mut Vec<(String, String)>,
) {
    let user_agent = setting(provider, "userAgent").unwrap_or(CLIENT_USER_AGENT);
    set_header(headers, "user-agent", user_agent.to_owned());
    set_header(headers, "x-opencode-client", "cli".into());
    set_header(headers, "x-opencode-project", "global".into());
    let session = request
        .map(|request| session_suffix(request, provider))
        .unwrap_or_else(random_suffix);
    set_header(headers, "x-opencode-session", format!("ses_{session}"));
    set_header(
        headers,
        "x-opencode-request",
        format!("msg_{}", random_suffix()),
    );
}

fn apply_auth(provider: &ProviderSnapshot, headers: &mut Vec<(String, String)>) {
    // Zen 官方客户端无 key 时发送字面量 `Bearer public`，上游对
    // `allowAnonymous` 免费模型放行并改用 IP 限流。
    let token = setting(provider, "apiKey").unwrap_or("public");
    set_header(headers, "authorization", format!("Bearer {token}"));
}

fn check_provider(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != VENDOR_ID {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            format!(
                "dedicated vendor `{VENDOR_ID}` cannot handle provider `{}`",
                provider.provider_id
            ),
        ));
    }
    if channel != CHANNEL_ID {
        return Err(common::unsupported("channel", VENDOR_ID, channel));
    }
    if provider.channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "provider snapshot channel does not match dispatch channel",
        ));
    }
    Ok(())
}

fn execute_inference(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    mut request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    let model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| common::plugin_error(ErrorKind::Invalid, "inference requires a model"))?;
    request.model = model.to_owned();
    if is_free_tier_model(model) {
        prepare_free_tier_request(&mut request);
    }
    let encoded = common::encode_inference_request(PROTOCOL, &request)?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    apply_client_fingerprint(provider, Some(&request), &mut headers);
    for (name, value) in &provider.client_headers {
        set_header(&mut headers, name, value.clone());
    }
    set_header(&mut headers, "content-type", "application/json".into());
    apply_auth(provider, &mut headers);
    let url = common::endpoint_url(&provider.base_url, &encoded.path)?;
    let body = serde_json::to_vec(&encoded.body).map_err(|error| {
        common::plugin_error(
            ErrorKind::Invalid,
            format!("failed to serialize codec request: {error}"),
        )
    })?;
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".to_owned(),
        url,
        headers,
        body,
    })?;
    common::decode_inference(host, PROTOCOL, response)
}

fn discover(host: &GuestHost, provider: &ProviderSnapshot) -> Result<OperationOutput, PluginError> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    apply_client_fingerprint(provider, None, &mut headers);
    apply_auth(provider, &mut headers);
    let response = host.http_start(HttpRequest {
        method: "GET".to_owned(),
        url,
        headers,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let body = read_http_body(&response, MAX_MODELS_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &response_headers, &body));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "model list is not valid JSON"))?;
    // 本插件只暴露免费模型：付费模型走 base 的 `opencode` Profile。
    let models = value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .filter(|id| is_free_tier_model(id))
        .map(|id| DiscoveredModel {
            id: id.to_owned(),
            display_name: id.to_owned(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        })
        .collect();
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: None,
    }))
}

pub fn select_protocol(
    _operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    check_provider(channel, provider)?;
    Ok(PROTOCOL.to_owned())
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    match input {
        OperationInput::Infer { provider, request } if operation == Operation::Infer => {
            check_provider(channel, &provider)?;
            execute_inference(host, &provider, request)
        }
        OperationInput::Discover { provider, .. } if operation == Operation::Discover => {
            check_provider(channel, &provider)?;
            discover(host, &provider)
        }
        other => Err(common::unsupported(
            other.operation().as_str(),
            VENDOR_ID,
            channel,
        )),
    }
}

fn string_field(
    key: &str,
    label: stravia_vendor_sdk::LocalizedText,
    description: Option<stravia_vendor_sdk::LocalizedText>,
    secret: bool,
    group: &str,
) -> ConfigField {
    ConfigField {
        key: key.to_owned(),
        label,
        description,
        kind: ConfigFieldKind::String { multiline: false },
        required: false,
        default_json: None::<Value>,
        group: Some(group.to_owned()),
        secret,
        min: None,
        max: None,
        max_length: Some(16_384),
        pattern: None,
        visible_when: None,
    }
}

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([Capability::Infer, Capability::ModelDiscovery]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "OpenCode Zen Free".into(),
        description: Some(
            "OpenCode Zen free-tier models (anonymous or Zen key) over the chat-completions surface."
                .into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            // 目录 ID 复用 `opencode` 以共享目录图标；本插件不消费目录模型。
            catalog_id: Some("opencode".into()),
            display_name: "OpenCode Zen Free".into(),
            description: Some(
                "Free-tier models on OpenCode Zen. Works without an API key; anonymous use is IP-rate-limited upstream."
                    .into(),
            ),
            channels: vec![ChannelDescriptor {
                id: CHANNEL_ID.into(),
                name: messages::default_channel(),
                description: None,
                auth: None,
                protocol: Some("openai-compatible".into()),
                protocols: Vec::new(),
                default_base_url: Some(DEFAULT_BASE_URL.into()),
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            }],
            capabilities,
            website: Some("https://opencode.ai".into()),
            implementation: None,
            config_groups: vec![
                ConfigGroup {
                    id: "credentials".into(),
                    label: messages::credentials_group(),
                },
                ConfigGroup {
                    id: "advanced".into(),
                    label: messages::advanced_group(),
                },
            ],
            config_fields: vec![
                string_field(
                    "apiKey",
                    messages::api_key(),
                    Some(messages::api_key_description()),
                    true,
                    "credentials",
                ),
                string_field(
                    "userAgent",
                    messages::user_agent(),
                    Some(messages::user_agent_description()),
                    false,
                    "advanced",
                ),
            ],
            network: NetworkDeclaration::default(),
            data_compat: DataCompatibility::default(),
        }],
    }
}

#[cfg(target_arch = "wasm32")]
struct OpencodeFreeVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for OpencodeFreeVendor {
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
stravia_vendor_sdk::export_vendor!(OpencodeFreeVendor);

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role};

    fn provider(model: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: VENDOR_ID.into(),
            channel: CHANNEL_ID.into(),
            base_url: DEFAULT_BASE_URL.into(),
            protocol: "openai-compatible".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: Some(model.into()),
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    fn request(model: &str) -> AiRequest {
        AiRequest::new(
            model,
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("say ok".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        )
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: Some("client tool".into()),
            parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            strict: None,
            cache_control: None,
            meta: None,
        }
    }

    fn tool_names(request: &AiRequest) -> Vec<String> {
        request
            .tools
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }

    #[test]
    fn free_model_gets_stream_tools_and_instructions() {
        let mut request = request("big-pickle");
        prepare_free_tier_request(&mut request);
        assert!(request.stream.enabled);
        assert_eq!(tool_names(&request), vec!["bash", "read"]);
        assert!(matches!(request.tool_choice, Some(ToolChoice::Auto)));
        let instructions = request.instructions.unwrap();
        assert!(instructions.contains("`bash`"));
        assert!(instructions.contains("`read`"));
        let bash = &request.tools.as_ref().unwrap()[0];
        assert_eq!(bash.description, None);
        assert_eq!(bash.parameters, json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn client_tool_same_name_overrides_injection_and_ban() {
        let mut request = request("mimo-v2.5-free");
        request.instructions = Some("Be brief.".into());
        request.tools = Some(vec![tool("bash"), tool("custom_tool")]);
        prepare_free_tier_request(&mut request);
        assert_eq!(tool_names(&request), vec!["bash", "custom_tool", "read"]);
        // 客户端的 bash 保留原 schema，禁令只针对注入的 read。
        let bash = &request.tools.as_ref().unwrap()[0];
        assert_eq!(bash.description.as_deref(), Some("client tool"));
        let instructions = request.instructions.unwrap();
        assert!(instructions.starts_with("Be brief."));
        assert!(instructions.contains("`read`"));
        assert!(!instructions.contains("`bash`"));
    }

    #[test]
    fn client_with_both_names_gets_no_injection() {
        let mut request = request("nemotron-3-ultra-free");
        request.tools = Some(vec![tool("bash"), tool("read")]);
        request.tool_choice = Some(ToolChoice::Required);
        prepare_free_tier_request(&mut request);
        assert_eq!(tool_names(&request), vec!["bash", "read"]);
        // 仍需 stream:true，但 tool_choice 与 instructions 保持客户端原值。
        assert!(request.stream.enabled);
        assert!(matches!(request.tool_choice, Some(ToolChoice::Required)));
        assert!(request.instructions.is_none());
    }

    #[test]
    fn fingerprint_and_auth_headers() {
        let mut headers = Vec::new();
        let provider = provider("big-pickle");
        apply_client_fingerprint(&provider, None, &mut headers);
        apply_auth(&provider, &mut headers);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert!(get("user-agent").unwrap().starts_with("opencode/1."));
        let session = get("x-opencode-session").unwrap();
        assert!(session.starts_with("ses_") && session.len() == 30);
        assert!(session[4..].chars().all(|c| c.is_ascii_hexdigit()));
        assert!(get("x-opencode-request").unwrap().starts_with("msg_"));
        assert_eq!(get("x-opencode-client").as_deref(), Some("cli"));
        assert_eq!(get("authorization").as_deref(), Some("Bearer public"));
    }

    fn session_header(provider: &ProviderSnapshot, request: &AiRequest) -> String {
        let mut headers = Vec::new();
        apply_client_fingerprint(provider, Some(request), &mut headers);
        headers
            .into_iter()
            .find(|(n, _)| n == "x-opencode-session")
            .map(|(_, v)| v)
            .expect("session header")
    }

    #[test]
    fn session_is_stable_per_client_key() {
        // 同一客户端会话键必须派生同一 ses_ 值；不同键不同值。
        // 上游若按 session 做亲和路由，稳定性是缓存命中的前提。
        let provider = provider("big-pickle");
        let mut req = request("big-pickle");
        req.ext = Some(ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                prompt_cache_key: Some("conv-a".into()),
                ..Default::default()
            },
        ));
        let first = session_header(&provider, &req);
        let second = session_header(&provider, &req);
        assert_eq!(first, second);
        assert!(first.starts_with("ses_") && first.len() == 30);

        let mut other = request("big-pickle");
        other.ext = Some(ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                prompt_cache_key: Some("conv-b".into()),
                ..Default::default()
            },
        ));
        assert_ne!(session_header(&provider, &other), first);
    }

    #[test]
    fn session_prefers_host_resolved_key_and_falls_back_to_random() {
        let provider = provider("big-pickle");
        let mut req = request("big-pickle");
        req.meta.vendor.ingress.insert(
            "__stravia_generation_session_id".into(),
            Value::String("host-session".into()),
        );
        req.ext = Some(ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                prompt_cache_key: Some("other".into()),
                ..Default::default()
            },
        ));
        let expected = format!(
            "ses_{}",
            &hash_hex(&hash_bytes(b"opencode-session\0host-session"))[..26]
        );
        assert_eq!(session_header(&provider, &req), expected);

        // 无键：随机 ses_，两次调用不同——上游只查形状时语义等价。
        let plain = request("big-pickle");
        let a = session_header(&provider, &plain);
        let b = session_header(&provider, &plain);
        assert!(a.starts_with("ses_") && b.starts_with("ses_"));
        assert_ne!(a, b);
    }

    #[test]
    fn api_key_and_user_agent_override() {
        let mut provider = provider("big-pickle");
        provider
            .credentials
            .insert("apiKey".into(), Value::String("zen-key".into()));
        provider
            .options
            .insert("userAgent".into(), Value::String("opencode/9.9.9".into()));
        let mut headers = Vec::new();
        apply_client_fingerprint(&provider, None, &mut headers);
        apply_auth(&provider, &mut headers);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("user-agent").as_deref(), Some("opencode/9.9.9"));
        assert_eq!(get("authorization").as_deref(), Some("Bearer zen-key"));
    }

    #[test]
    fn non_free_model_decision() {
        assert!(is_free_tier_model("mimo-v2.5-free"));
        assert!(is_free_tier_model("big-pickle"));
        assert!(!is_free_tier_model("gpt-5.5"));
        assert!(!is_free_tier_model("freedom"));
    }
}
