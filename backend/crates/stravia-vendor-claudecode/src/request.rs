//! 把 codec 编码出的 Anthropic Messages 请求改写为 Claude Code CLI 的线上形态。
//!
//! 原生控制闭集按官方 Claude Code 2.1.300 发行包内的请求构造器、官方 SDK 类型
//! 及本地合成凭据 loopback 抓包维护；不接纳 CLI 的任意 EXTRA_BODY/EXTRA_METADATA。
//! 既有 OAuth 塑形包括 `system[0]` 计费头、`system[1]` 身份块、自定义工具名 `_`
//! 前缀、重建 `metadata.user_id`、缓存 TTL 与最终字节 `cch` 校验。业务内容、工具
//! 定义和 JSON schema 不按请求参数递归过滤。

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::cache;
use crate::capabilities::{self, ModelCapabilities};
use crate::thinking::{self, ModelProfile};
use crate::xxhash::xxh64;

/// 未配置覆盖值时上报的 Claude Code CLI 正式发行版本。
pub(crate) const DEFAULT_CLIENT_VERSION: &str = "2.1.300";
/// 与该 Claude Code 发布包内的 `@anthropic-ai/sdk` 身份保持一致。
pub(crate) const SDK_VERSION: &str = "0.128.0";
const RUNTIME_VERSION: &str = "v26.3.0";
/// Stainless 平台头只描述「客户端运行环境」；插件在 Wasm 内拿不到宿主平台，
/// 固定为自托管服务端最常见的 Linux x64。
const STAINLESS_OS: &str = "Linux";
const STAINLESS_ARCH: &str = "x64";

pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
const IDENTITY_TEXT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const FINGERPRINT_INDICES: [usize; 3] = [4, 7, 20];
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const CCH_PLACEHOLDER: &[u8] = b"cch=00000";
const CCH_MARKER: &[u8] = b"\"system\":[{\"type\":\"text\",\"text\":\"x-anthropic-billing-header:";
const CCH_SEARCH_WINDOW: usize = 150;
const TOOL_PREFIX: &str = "_";
/// 与 Anthropic 内置工具同名的自定义工具不加前缀，避免被上游当作内置工具冲突。
const BUILTIN_TOOL_NAMES: [&str; 4] = ["web_search", "code_execution", "text_editor", "computer"];

const EFFORT_BETA: &str = "effort-2025-11-24";
const FALLBACK_CREDIT_BETA: &str = "fallback-credit-2026-06-01";
const AGENT_BETAS: [&str; 7] = [
    "claude-code-20250219",
    OAUTH_BETA,
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "mid-conversation-system-2026-04-07",
];
const UTILITY_BETAS: [&str; 6] = [
    OAUTH_BETA,
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "structured-outputs-2025-12-15",
];

/// 线上 JSON 的对象键顺序。上游 SDK 按插入顺序序列化；Stravia 入口解析后
/// 原始顺序已丢失，这里按 Claude Code 的字段构造顺序输出已知键，其余键按
/// 字典序排在后面。`cch` 只要求与实际发送字节一致，顺序用于贴近真实客户端。
const KEY_ORDER: &[&str] = &[
    "type",
    "role",
    "id",
    "tool_use_id",
    "name",
    "description",
    "input_schema",
    "model",
    "messages",
    "system",
    "tools",
    "metadata",
    "max_tokens",
    "thinking",
    "context_management",
    "output_config",
    "stream",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "tool_choice",
    "content",
    "text",
    "signature",
    "data",
    "source",
    "media_type",
    "url",
    "input",
    "is_error",
    "budget_tokens",
    "display",
    "edits",
    "keep",
    "user_id",
    "eager_input_streaming",
    "strict",
    "defer_loading",
    "cache_control",
    "ttl",
];

/// Closed first-party CLI contract, checked against installed Claude Code
/// 2.1.248 loopback captures and the official npm 2.1.300 native package's
/// embedded queryModel / sideQuery request constructors. Generic SDK fields
/// and CLI EXTRA_BODY / EXTRA_METADATA injection are deliberately not admitted.
/// Nested parameter types follow anthropic-sdk-typescript beta/messages.
const CLAUDE_CODE_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "metadata",
    "max_tokens",
    "thinking",
    "context_management",
    "output_config",
    "stream",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "tool_choice",
    "speed",
    "cache_control",
    "thread",
    "diagnostics",
    "context_hint",
    "safeguards",
    "fallbacks",
];

/// 已登录账号在请求元数据里的身份。
pub(crate) struct AccountIdentity<'a> {
    pub account_uuid: Option<&'a str>,
    pub device_id: Option<&'a str>,
    pub parent_session_id: Option<&'a str>,
}

pub(crate) struct ShapedRequest {
    pub body: Vec<u8>,
    pub session_id: String,
    pub betas: String,
}

/// 塑形 codec 产出的请求体并序列化为最终线上字节（已写入 `cch`）。
/// `auto_output_limit` 表示 `max_tokens` 来自发现数据或编码器的必填回退，
/// 此时思考预算需让位于该上限。会话仅由宿主的 principal/target scoped
/// operation metadata 派生。
pub(crate) fn shape(
    mut body: Value,
    client_version: &str,
    identity: &AccountIdentity<'_>,
    caps: &ModelCapabilities,
    auto_output_limit: bool,
    fallback_session_id: impl FnOnce() -> String,
) -> Result<ShapedRequest, String> {
    let object = body
        .as_object_mut()
        .ok_or("Anthropic request body must be a JSON object")?;
    // Claude Code 只走流式推理；流式同时避开非流式请求的长输出时限。
    object.insert("stream".into(), Value::Bool(true));
    restrict_to_claude_code_fields(object);
    let block_binding = object
        .get_mut("thinking")
        .and_then(Value::as_object_mut)
        .and_then(|thinking| thinking.remove("block_binding"));
    thinking::normalize(object, caps, auto_output_limit)?;
    if let Some(block_binding) = block_binding
        && let Some(thinking) = object.get_mut("thinking").and_then(Value::as_object_mut)
        && matches!(
            thinking.get("type").and_then(Value::as_str),
            Some("adaptive" | "enabled")
        )
    {
        thinking.insert("block_binding".into(), block_binding);
    }
    capabilities::constrain(object, caps);
    drop_disallowed_sampling(object, caps);
    drop_unsupported_prefill(object, caps);
    prefix_tool_names(object);
    default_tool_result_errors(object);
    let session_id = fallback_session_id();
    let first_user_text = first_user_text(object.get("messages"));
    rebuild_system(object, &first_user_text, client_version)?;
    cache::enforce(object);
    object.insert(
        "metadata".into(),
        json!({ "user_id": metadata_user_id(&session_id, identity) }),
    );
    let betas = beta_header(object);
    let mut bytes = Vec::new();
    write_ordered(&body, &mut bytes).map_err(|error| error.to_string())?;
    patch_cch(&mut bytes)?;
    Ok(ShapedRequest {
        body: bytes,
        session_id,
        betas,
    })
}

/// Claude Code 推理请求头。压缩策略通过强类型契约交给宿主。
pub(crate) fn inference_headers(
    access_token: &str,
    client_version: &str,
    session_id: &str,
    betas: String,
) -> Vec<(String, String)> {
    vec![
        ("accept".into(), "application/json".into()),
        ("authorization".into(), format!("Bearer {access_token}")),
        ("connection".into(), "keep-alive".into()),
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), cli_user_agent(client_version)),
        ("x-claude-code-session-id".into(), session_id.into()),
        (
            "x-client-request-id".into(),
            uuid::Uuid::new_v4().to_string(),
        ),
        ("x-stainless-arch".into(), STAINLESS_ARCH.into()),
        ("x-stainless-lang".into(), "js".into()),
        ("x-stainless-os".into(), STAINLESS_OS.into()),
        ("x-stainless-package-version".into(), SDK_VERSION.into()),
        ("x-stainless-retry-count".into(), "0".into()),
        ("x-stainless-runtime".into(), "node".into()),
        ("x-stainless-runtime-version".into(), RUNTIME_VERSION.into()),
        ("x-stainless-timeout".into(), "600".into()),
        ("anthropic-beta".into(), betas),
        (
            "anthropic-dangerous-direct-browser-access".into(),
            "true".into(),
        ),
        ("anthropic-version".into(), "2023-06-01".into()),
        ("x-app".into(), "cli".into()),
    ]
}

pub(crate) fn cli_user_agent(client_version: &str) -> String {
    // Official 2.1.300 non-interactive --print loopback capture.
    format!("claude-cli/{client_version} (external, sdk-cli)")
}

/// 响应侧还原：去掉请求时加上的一个 `_` 前缀。
pub(crate) fn decode_tool_name(name: &str) -> Option<String> {
    name.strip_prefix(TOOL_PREFIX).map(str::to_owned)
}

/// 丢弃真实 Claude Code 不会发送的顶层字段。
fn restrict_to_claude_code_fields(object: &mut Map<String, Value>) {
    object.retain(|key, _| CLAUDE_CODE_FIELDS.contains(&key.as_str()));
    let dropped_tools = object
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .is_some_and(|tools| {
            let original_count = tools.len();
            tools.retain_mut(restrict_claude_code_tool);
            tools.len() != original_count
        });
    if dropped_tools && let Some(tools) = object.get("tools").and_then(Value::as_array) {
        let unavailable_choice = object
            .get("tool_choice")
            .and_then(|choice| choice.get("name"))
            .and_then(Value::as_str)
            .is_some_and(|name| {
                !tools
                    .iter()
                    .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            });
        if tools.is_empty() || unavailable_choice {
            object.remove("tool_choice");
        }
    }
    retain_control_fields(
        object.get_mut("thinking"),
        &["type", "budget_tokens", "display", "block_binding"],
    );
    if let Some(thinking) = object.get_mut("thinking").and_then(Value::as_object_mut) {
        retain_control_fields(
            thinking.get_mut("block_binding"),
            &["prefix_mismatch_behavior"],
        );
    }
    retain_control_fields(
        object.get_mut("tool_choice"),
        &["type", "name", "disable_parallel_tool_use"],
    );
    retain_control_fields(
        object.get_mut("cache_control"),
        &["type", "ttl", "scope", "evict_on_complete"],
    );
    retain_control_fields(object.get_mut("thread"), &["type", "previous_message_id"]);
    retain_control_fields(object.get_mut("diagnostics"), &["previous_message_id"]);
    retain_control_fields(
        object.get_mut("context_hint"),
        &["enabled", "target_tokens_saved"],
    );
    if let Some(config) = object
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
    {
        config.retain(|key, _| matches!(key.as_str(), "effort" | "format" | "task_budget"));
        retain_control_fields(config.get_mut("format"), &["type", "schema"]);
        retain_control_fields(
            config.get_mut("task_budget"),
            &["type", "total", "remaining"],
        );
    }
    if let Some(context) = object
        .get_mut("context_management")
        .and_then(Value::as_object_mut)
    {
        context.retain(|key, _| key == "edits");
        if let Some(edits) = context.get_mut("edits").and_then(Value::as_array_mut) {
            edits.retain_mut(|edit| {
                let Some(edit) = edit.as_object_mut() else {
                    return false;
                };
                let fields: &[&str] = match edit.get("type").and_then(Value::as_str) {
                    Some("clear_thinking_20251015") => &["type", "keep"],
                    Some("clear_tool_uses_20250919") => &[
                        "type",
                        "clear_at_least",
                        "clear_tool_inputs",
                        "exclude_tools",
                        "keep",
                        "trigger",
                    ],
                    Some("compact_20260112") => {
                        &["type", "instructions", "pause_after_compaction", "trigger"]
                    }
                    _ => return false,
                };
                edit.retain(|key, _| fields.contains(&key.as_str()));
                for key in ["keep", "trigger", "clear_at_least"] {
                    retain_control_fields(edit.get_mut(key), &["type", "value"]);
                }
                true
            });
        }
    }
    if let Some(safeguards) = object.get_mut("safeguards").and_then(Value::as_array_mut) {
        for safeguard in safeguards {
            // classifier_context is the official classifier's business JSON,
            // not an open request-parameter namespace.
            retain_control_fields(Some(safeguard), &["type", "classifier_context"]);
        }
    }
    if let Some(fallbacks) = object.get_mut("fallbacks").and_then(Value::as_array_mut) {
        for fallback in fallbacks {
            retain_control_fields(Some(fallback), &["model"]);
        }
    }
}

fn restrict_claude_code_tool(tool: &mut Value) -> bool {
    let Some(tool) = tool.as_object_mut() else {
        return false;
    };
    // Official npm @anthropic-ai/claude-code-win32-x64@2.1.300, embedded
    // a$/x1t serializer: eager_input_streaming depends on the tengu_fgts
    // feature gate (or an environment override), so it is not an always-send
    // CLI field. Cache anchors are generated later by cache::enforce, never
    // copied from the client tool definition.
    let fields: &[&str] = match tool.get("type").and_then(Value::as_str) {
        None | Some("custom") if tool.contains_key("input_schema") => &[
            "name",
            "description",
            "input_schema",
            "strict",
            "defer_loading",
        ],
        Some("web_search_20250305") => &[
            "type",
            "name",
            "allowed_domains",
            "blocked_domains",
            "max_uses",
            "user_location",
        ],
        _ => return false,
    };
    tool.retain(|key, _| fields.contains(&key.as_str()));
    retain_control_fields(
        tool.get_mut("user_location"),
        &["type", "country", "region", "city", "timezone"],
    );
    true
}

fn retain_control_fields(value: Option<&mut Value>, fields: &[&str]) {
    if let Some(object) = value.and_then(Value::as_object_mut) {
        object.retain(|key, _| fields.contains(&key.as_str()));
    }
}

/// 不支持 prefill 时省略末尾 assistant，不把助手内容冒充用户，也不插入合成提示。
/// 前面的 assistant 历史及已完成的工具调用/结果保持原样。
fn drop_unsupported_prefill(object: &mut Map<String, Value>, caps: &ModelCapabilities) {
    let thinking_enabled = object
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "enabled" | "adaptive" | "between_tools"));
    let model_rejects = ModelProfile::of_request(object, caps)
        .is_some_and(|model| !model.accepts_prefill())
        || object.get("model").and_then(Value::as_str) == Some("claude-mythos-preview");
    if !thinking_enabled && !model_rejects {
        return;
    }
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    while messages
        .last()
        .is_some_and(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
    {
        messages.pop();
    }
}

/// 复刻 oh-my-pi `buildParams`：仅在模型支持自定义采样且思考未开启时才保留
/// `temperature` / `top_p` / `top_k`。开启思考（含 adaptive）时上游要求 `temperature`
/// 只能为 1，Opus ≥ 4.7 与 Sonnet/Fable/Mythos ≥ 5 则完全拒绝非默认采样参数。
/// 必须在思考控制改写之后调用，以最终发送的 `thinking` 为准。
fn drop_disallowed_sampling(object: &mut Map<String, Value>, caps: &ModelCapabilities) {
    let thinking_off = object
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "disabled");
    let model_rejects =
        ModelProfile::of_request(object, caps).is_some_and(|model| model.adaptive_only());
    if !thinking_off || model_rejects {
        for key in ["temperature", "top_p", "top_k"] {
            object.remove(key);
        }
    }
}

fn should_prefix(name: &str, typed_tool_names: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    !BUILTIN_TOOL_NAMES.contains(&lower.as_str())
        && !typed_tool_names.iter().any(|typed| typed == name)
}

fn prefix_name(object: &mut Map<String, Value>, typed_tool_names: &[String]) {
    if let Some(Value::String(name)) = object.get_mut("name")
        && should_prefix(name, typed_tool_names)
    {
        name.insert_str(0, TOOL_PREFIX);
    }
}

/// 自定义工具（无 `type` 或 `type: custom`）及其历史调用、`tool_choice` 统一加
/// 前缀；Anthropic 定义的类型化工具（`bash_20250124` 等）名字由上游校验，保持原样。
fn prefix_tool_names(object: &mut Map<String, Value>) {
    let tools = object
        .entry("tools")
        .or_insert_with(|| Value::Array(Vec::new()));
    let mut typed_tool_names = Vec::new();
    if let Value::Array(tools) = tools {
        for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
            let typed = tool
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind != "custom");
            if typed {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    typed_tool_names.push(name.to_owned());
                }
            } else {
                prefix_name(tool, &[]);
            }
        }
    }
    if let Some(Value::Array(messages)) = object.get_mut("messages") {
        for block in messages
            .iter_mut()
            .filter_map(|message| message.get_mut("content"))
            .filter_map(Value::as_array_mut)
            .flatten()
            .filter_map(Value::as_object_mut)
        {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                prefix_name(block, &typed_tool_names);
            }
        }
    }
    if let Some(Value::Object(choice)) = object.get_mut("tool_choice")
        && choice.get("type").and_then(Value::as_str) == Some("tool")
    {
        prefix_name(choice, &typed_tool_names);
    }
}

/// Claude Code 的每个 `tool_result` 都显式携带 `is_error`。OpenAI、Gemini 等入口
/// 没有该字段，codec 编码时省略；这里补 `false`（上游缺省语义相同），已有值不变。
fn default_tool_result_errors(object: &mut Map<String, Value>) {
    if let Some(Value::Array(messages)) = object.get_mut("messages") {
        for block in messages
            .iter_mut()
            .filter_map(|message| message.get_mut("content"))
            .filter_map(Value::as_array_mut)
            .flatten()
            .filter_map(Value::as_object_mut)
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            block.entry("is_error").or_insert(Value::Bool(false));
        }
    }
}

fn first_user_text(messages: Option<&Value>) -> String {
    let Some(message) = messages.and_then(Value::as_array).and_then(|messages| {
        messages
            .iter()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    }) else {
        return String::new();
    };
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

/// `SHA256(salt + msg[4] + msg[7] + msg[20] + version)` 前 3 个十六进制字符。
/// 下标按 JavaScript 字符串（UTF-16 码元）计算；落在代理对上的码元按 UTF-8
/// 编码孤立代理的方式替换为 U+FFFD，与 JS 运行时的哈希输入一致。
fn fingerprint(first_user_text: &str, client_version: &str) -> String {
    let units: Vec<u16> = first_user_text.encode_utf16().collect();
    let mut seed = String::from(FINGERPRINT_SALT);
    for index in FINGERPRINT_INDICES {
        seed.push(match units.get(index) {
            Some(unit) => char::from_u32(u32::from(*unit)).unwrap_or('\u{FFFD}'),
            None => '0',
        });
    }
    seed.push_str(client_version);
    let digest = Sha256::digest(seed.as_bytes());
    format!("{:02x}{:02x}", digest[0], digest[1])[..3].to_owned()
}

fn text_block(block: &Value) -> Option<&str> {
    (block.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| block.get("text").and_then(Value::as_str))
        .flatten()
}

/// 组装 `[计费头, 身份块, 客户端 system...]`。客户端自带的计费头（例如直连
/// 的 Claude Code）对应的是它自己的请求体，经网关改写后已失效，一律替换。
fn rebuild_system(
    object: &mut Map<String, Value>,
    first_user_text: &str,
    client_version: &str,
) -> Result<(), String> {
    let mut client_blocks = match object.remove("system") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
        Some(Value::Array(blocks)) => blocks,
        Some(_) => return Err("Anthropic system must be a string or an array of blocks".into()),
    };
    client_blocks
        .retain(|block| !text_block(block).is_some_and(|text| text.starts_with(BILLING_PREFIX)));
    let mut system = Vec::with_capacity(client_blocks.len() + 2);
    system.push(json!({
        "type": "text",
        "text": format!(
            "{BILLING_PREFIX} cc_version={client_version}.{}; cc_entrypoint=cli; cch=00000;",
            fingerprint(first_user_text, client_version)
        ),
    }));
    if client_blocks.first().and_then(text_block) != Some(IDENTITY_TEXT) {
        system.push(json!({ "type": "text", "text": IDENTITY_TEXT }));
    }
    system.extend(client_blocks);
    object.insert("system".into(), Value::Array(system));
    Ok(())
}

#[derive(Serialize)]
struct MetadataUserId<'a> {
    session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_uuid: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_id: Option<&'a str>,
}

/// `metadata.user_id` 由上游账号身份和宿主 scoped 会话/父会话标签重建。
fn metadata_user_id(session_id: &str, identity: &AccountIdentity<'_>) -> String {
    serde_json::to_string(&MetadataUserId {
        session_id,
        parent_session_id: identity.parent_session_id,
        account_uuid: identity.account_uuid,
        device_id: identity.device_id,
    })
    .expect("metadata identity serializes")
}

fn beta_header(object: &Map<String, Value>) -> String {
    let thinking = object
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "enabled" | "adaptive"));
    let has_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let mut betas: Vec<&str> = if has_tools || thinking {
        let mut betas = AGENT_BETAS.to_vec();
        if thinking {
            betas.push(EFFORT_BETA);
        }
        betas.push(FALLBACK_CREDIT_BETA);
        betas
    } else {
        UTILITY_BETAS.to_vec()
    };
    // `output_config.effort` 需要 effort beta；关闭思考时 Claude Code 仍可能
    // 用最低 effort 代替 `thinking: disabled`。
    if object
        .get("output_config")
        .and_then(|config| config.get("effort"))
        .is_some()
        && !betas.contains(&EFFORT_BETA)
    {
        betas.push(EFFORT_BETA);
    }
    for (present, beta) in [
        (
            object
                .get("output_config")
                .and_then(|v| v.get("format"))
                .is_some(),
            "structured-outputs-2025-12-15",
        ),
        (
            object
                .get("output_config")
                .and_then(|v| v.get("task_budget"))
                .is_some(),
            "task-budgets-2026-03-13",
        ),
        (
            object
                .get("cache_control")
                .and_then(|v| v.get("evict_on_complete"))
                .and_then(Value::as_bool)
                == Some(true),
            "prompt-caching-evict-2026-05-12",
        ),
        (object.contains_key("thread"), "message-threads-2026-08-12"),
        (
            object.contains_key("diagnostics"),
            "cache-diagnosis-2026-04-07",
        ),
        (
            object.contains_key("context_hint"),
            "context-hint-2026-04-09",
        ),
        (
            object.get("speed").and_then(Value::as_str) == Some("fast"),
            "fast-mode-2026-02-01",
        ),
        (
            object.contains_key("safeguards"),
            "dangerous-tool-use-2026-09-03",
        ),
        (
            object
                .get("thinking")
                .and_then(|v| v.get("block_binding"))
                .is_some(),
            "thinking-binding-controls-2026-08-01",
        ),
        (
            object
                .get("thinking")
                .and_then(|v| v.get("display"))
                .and_then(Value::as_str)
                == Some("updates"),
            "thinking-display-updates-2026-08-18",
        ),
        (
            object.contains_key("fallbacks"),
            if object.get("fallbacks").and_then(Value::as_str) == Some("default") {
                "server-side-fallback-2026-07-01"
            } else {
                "server-side-fallback-2026-06-01"
            },
        ),
    ] {
        if present && !betas.contains(&beta) {
            betas.push(beta);
        }
    }
    betas.join(",")
}

fn key_rank(key: &str) -> usize {
    KEY_ORDER
        .iter()
        .position(|candidate| *candidate == key)
        .unwrap_or(KEY_ORDER.len())
}

fn write_ordered(value: &Value, out: &mut Vec<u8>) -> serde_json::Result<()> {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| {
                key_rank(left)
                    .cmp(&key_rank(right))
                    .then_with(|| left.cmp(right))
            });
            out.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_ordered(value, out)?;
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_ordered(item, out)?;
            }
            out.push(b']');
        }
        scalar => serde_json::to_writer(&mut *out, scalar)?,
    }
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// 在含占位符的完整请求体上计算 XXH64，取低 20 位写回占位符（与 Claude Code
/// 就地修补的行为一致）。
fn patch_cch(body: &mut [u8]) -> Result<(), String> {
    let marker = find(body, CCH_MARKER, 0).ok_or("billing header is not the first system block")?;
    let search_from = marker + CCH_MARKER.len();
    let placeholder = find(body, CCH_PLACEHOLDER, search_from)
        .filter(|index| index - search_from <= CCH_SEARCH_WINDOW)
        .ok_or("billing header cch placeholder is missing")?;
    let cch = format!("{:05x}", xxh64(body, CCH_SEED) & 0xfffff);
    body[placeholder + 4..placeholder + 9].copy_from_slice(cch.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> AccountIdentity<'static> {
        AccountIdentity {
            account_uuid: Some("00000000-0000-4000-8000-0000000000aa"),
            device_id: Some("d".repeat(64).leak()),
            parent_session_id: None,
        }
    }

    fn shaped(body: Value) -> (Value, ShapedRequest) {
        shaped_with(body, &ModelCapabilities::default())
    }

    fn shaped_with(body: Value, caps: &ModelCapabilities) -> (Value, ShapedRequest) {
        shaped_with_limit_origin(body, caps, false)
    }

    fn shaped_auto(body: Value) -> (Value, ShapedRequest) {
        shaped_with_limit_origin(body, &ModelCapabilities::default(), true)
    }

    fn shaped_with_limit_origin(
        body: Value,
        caps: &ModelCapabilities,
        auto_output_limit: bool,
    ) -> (Value, ShapedRequest) {
        let shaped = shape(
            body,
            DEFAULT_CLIENT_VERSION,
            &identity(),
            caps,
            auto_output_limit,
            || "fallback-session".into(),
        )
        .expect("request shapes");
        (
            serde_json::from_slice(&shaped.body).expect("shaped body is JSON"),
            shaped,
        )
    }

    #[test]
    fn unsupported_prefill_is_dropped_only_at_the_tail() {
        for model in [
            "claude-sonnet-4-6",
            "claude-sonnet-5-5",
            "claude-opus-4-6",
            "claude-opus-5-5",
            "claude-mythos-preview",
        ] {
            let (body, _) = shaped(json!({
                "model": model,
                "max_tokens": 4096,
                "messages": [
                    {"role": "user", "content": "first turn"},
                    {"role": "assistant", "content": "completed earlier answer"},
                    {"role": "user", "content": "next turn"},
                    {"role": "assistant", "content": "prefix"},
                    {"role": "assistant", "content": "more prefix"}
                ]
            }));
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 3, "{model}");
            assert_eq!(messages[1]["role"], "assistant");
            assert_eq!(
                messages[1]["content"][0]["text"],
                "completed earlier answer"
            );
            assert_eq!(messages[2]["role"], "user");
            assert_eq!(messages[2]["content"][0]["text"], "next turn");
        }
    }

    #[test]
    fn legacy_and_unknown_models_keep_native_prefill_unless_thinking_is_enabled() {
        for model in ["claude-sonnet-4-5", "claude-opus-4-5", "custom-model"] {
            let (body, _) = shaped(json!({
                "model": model,
                "max_tokens": 4096,
                "messages": [
                    {"role": "user", "content": "first turn"},
                    {"role": "assistant", "content": "supported prefix"}
                ]
            }));
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 2, "{model}");
            assert_eq!(messages[1]["content"][0]["text"], "supported prefix");
        }
        let (body, _) = shaped(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 8192,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "assistant", "content": "unsupported prefix"}
            ]
        }));
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["content"][0]["text"], "first turn");
    }

    #[test]
    fn explicit_output_limit_is_raised_only_for_the_thinking_budget() {
        // 显式上限：沿用既有规则，不足预算加正文空间时抬高。
        let (body, _) = shaped(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 2048,
            "thinking": {"type": "enabled", "budget_tokens": 16000},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(body["thinking"]["budget_tokens"], 16000);
        assert_eq!(body["max_tokens"], 16000 + 4000);

        // 显式上限已足够时原样保留。
        let (body, _) = shaped(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 64000,
            "thinking": {"type": "enabled", "budget_tokens": 16000},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(body["max_tokens"], 64000);

        // 自动补出的上限（发现数据或编码器必填回退）不能被预算抬高；预算让位。
        let (body, _) = shaped_auto(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "thinking": {"type": "enabled", "budget_tokens": 63000},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(body["max_tokens"], 32000);
        assert_eq!(body["thinking"]["budget_tokens"], 32000 - 4000);
    }

    #[test]
    fn fingerprint_matches_oh_my_pi_capture() {
        // oh-my-pi v18.4.2 真实抓包：首条用户消息以 system-reminder 开头，
        // 下标 4/7/20 取到 "t-d"，计费头为 cc_version=2.1.280.096。
        let text = "<system-reminder>\nToday: 2026-09-29; current working directory";
        assert_eq!(fingerprint(text, "2.1.280"), "096");
        // 消息过短时缺位字符用 "0" 填充。
        assert_eq!(fingerprint("", "2.1.280"), "d7b");
    }

    #[test]
    fn cch_is_recomputable_over_sent_bytes() {
        let (_, shaped) = shaped(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello"}],
        }));
        let text = String::from_utf8(shaped.body.clone()).unwrap();
        let start = text.find("cch=").unwrap() + 4;
        let sent = &text[start..start + 5];
        assert_ne!(sent, "00000");
        let mut zeroed = shaped.body.clone();
        zeroed[start..start + 5].copy_from_slice(b"00000");
        assert_eq!(sent, format!("{:05x}", xxh64(&zeroed, CCH_SEED) & 0xfffff));
        assert!(
            text.starts_with("{\"model\":"),
            "top-level keys follow Claude Code order"
        );
    }

    #[test]
    fn system_carries_billing_then_identity_then_client_blocks() {
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "system": [
                {"type": "text", "text": "client prompt", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "volatile tail"}
            ],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 4);
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .starts_with(BILLING_PREFIX)
        );
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["text"], IDENTITY_TEXT);
        assert_eq!(system[2]["text"], "client prompt");
        assert!(
            system[1].get("cache_control").is_none() && system[2].get("cache_control").is_none(),
            "client breakpoint on a middle block is dropped"
        );
        assert_eq!(
            system[3]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"}),
            "the anchor sits on the last system block"
        );
    }

    #[test]
    fn requests_without_client_breakpoints_get_default_anchors() {
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "system": "client prompt",
            "tools": [{"name": "a", "input_schema": {}}],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let ttl = json!({"type": "ephemeral", "ttl": "1h"});
        assert_eq!(body["tools"][0]["cache_control"], ttl);
        assert_eq!(body["system"][2]["cache_control"], ttl);
        assert!(body["system"][0].get("cache_control").is_none());
        assert_eq!(body["messages"][0]["content"][0]["cache_control"], ttl);
    }

    #[test]
    fn client_breakpoints_never_reach_upstream_and_total_stays_within_limit() {
        let marked = json!({"type": "ephemeral", "ttl": "5m"});
        let (body, shaped) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "tools": [{"name": "a", "input_schema": {}, "cache_control": marked}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "1", "cache_control": marked},
                    {"type": "text", "text": "2", "cache_control": marked},
                    {"type": "text", "text": "3", "cache_control": marked}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "a"}]},
                {"role": "user", "content": [{"type": "text", "text": "4"}]}
            ],
        }));
        let wire = String::from_utf8(shaped.body).unwrap();
        assert!(!wire.contains("5m"), "client TTL is overwritten");
        assert_eq!(wire.matches("cache_control").count(), 4);
        assert!(
            body["messages"][0]["content"][2]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["messages"][2]["content"][0]
                .get("cache_control")
                .is_some()
        );
    }

    #[test]
    fn identity_block_is_the_system_anchor_when_client_has_no_system() {
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            body["system"][1]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn stale_client_billing_header_is_replaced_and_identity_not_duplicated() {
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=9.9.9.abc; cc_entrypoint=cli; cch=12345;"},
                {"type": "text", "text": IDENTITY_TEXT},
                {"type": "text", "text": "prompt"}
            ],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let texts: Vec<&str> = body["system"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts.len(), 3);
        assert!(!texts[0].contains("9.9.9.abc"));
        assert_eq!(&texts[1..], [IDENTITY_TEXT, "prompt"]);
    }

    #[test]
    fn tool_results_without_is_error_are_sent_as_explicit_false() {
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                    {"type": "tool_result", "tool_use_id": "t2", "content": "boom", "is_error": true}
                ]}
            ],
        }));
        let results = &body["messages"][2]["content"];
        assert_eq!(results[0]["is_error"], json!(false));
        assert_eq!(results[1]["is_error"], json!(true));
    }

    #[test]
    fn empty_tool_list_is_sent_and_selects_utility_betas() {
        let (body, shaped) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(shaped.betas, UTILITY_BETAS.join(","));
    }

    #[test]
    fn beta_profile_follows_tools_thinking_and_effort() {
        let base = |extra: Value| {
            let mut body = json!({
                "model": "m",
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}],
            });
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            shaped(body).1.betas
        };
        let agent = AGENT_BETAS.join(",");
        assert_eq!(
            base(json!({"thinking": {"type": "enabled", "budget_tokens": 1024}})),
            format!("{agent},{EFFORT_BETA},{FALLBACK_CREDIT_BETA}")
        );
        assert_eq!(
            base(
                json!({"tools": [{"name": "a", "input_schema": {}}], "thinking": {"type": "disabled"}})
            ),
            format!("{agent},{FALLBACK_CREDIT_BETA}")
        );
        assert_eq!(
            base(json!({"output_config": {"effort": "low"}})),
            format!("{},{EFFORT_BETA}", UTILITY_BETAS.join(","))
        );
    }

    #[test]
    fn metadata_ignores_client_session_and_injects_account_identity() {
        let (body, shaped) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "metadata": {"user_id": "{\"session_id\":\"s-1\",\"account_uuid\":\"client\"}"},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(shaped.session_id, "fallback-session");
        let user_id: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(user_id["session_id"], "fallback-session");
        assert_eq!(
            user_id["account_uuid"],
            "00000000-0000-4000-8000-0000000000aa"
        );
        assert_eq!(user_id["device_id"].as_str().unwrap().len(), 64);

        let (_, shaped) =
            shaped_with_metadata("user_abc_account_x_session_12345678-1234-1234-1234-123456789012");
        assert_eq!(shaped.session_id, "fallback-session");
        let (_, shaped) = shaped_with_metadata("opaque-client-user");
        assert_eq!(shaped.session_id, "fallback-session");
    }

    #[test]
    fn non_strict_nullable_tool_schema_is_preserved_without_schema_surgery() {
        let properties: Map<String, Value> = (0..39)
            .map(|index| {
                (
                    format!("field_{index}"),
                    json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
                )
            })
            .collect();
        let schema = json!({"type":"object", "properties":properties});
        let (body, _) = shaped(json!({
            "model":"claude-sonnet-4-5",
            "max_tokens":1024,
            "tools":[{"name":"nullable_tool", "input_schema":schema}],
            "messages":[{"role":"user", "content":"hi"}]
        }));
        assert_eq!(body["tools"][0]["name"], "_nullable_tool");
        assert_eq!(body["tools"][0]["input_schema"], schema);
        assert!(body["tools"][0].get("strict").is_none());
    }

    #[test]
    fn sampling_params_are_dropped_when_thinking_is_on() {
        // 回归：adaptive 思考 + `temperature: 0` 被上游 400 拒绝。
        for thinking in [
            json!({"type": "adaptive"}),
            json!({"type": "enabled", "budget_tokens": 1024}),
        ] {
            let (body, _) = shaped(json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 2048,
                "temperature": 0,
                "top_p": 0.9,
                "top_k": 5,
                "thinking": thinking,
                "messages": [{"role": "user", "content": "hi"}],
            }));
            for key in ["temperature", "top_p", "top_k"] {
                assert!(body.get(key).is_none(), "{key} must not be sent");
            }
        }
    }

    #[test]
    fn sampling_params_survive_only_when_model_and_thinking_allow() {
        let body = |model: &str, thinking: Option<Value>| {
            let mut body = json!({
                "model": model,
                "max_tokens": 64,
                "temperature": 0.2,
                "messages": [{"role": "user", "content": "hi"}],
            });
            if let Some(thinking) = thinking {
                body["thinking"] = thinking;
            }
            shaped(body).0
        };
        assert_eq!(body("claude-sonnet-4-5", None)["temperature"], json!(0.2));
        assert_eq!(
            body("claude-opus-4-6", Some(json!({"type": "disabled"})))["temperature"],
            json!(0.2)
        );
        for model in [
            "claude-opus-4-7",
            "claude-opus-4.8",
            "claude-opus-5-5",
            "claude-sonnet-5",
            "claude-fable-5",
        ] {
            assert!(
                body(model, Some(json!({"type": "disabled"})))
                    .get("temperature")
                    .is_none(),
                "{model} rejects custom sampling"
            );
        }
        // 日期后缀不是版本号。
        assert!(
            body("claude-opus-4-1-20250805", None)
                .get("temperature")
                .is_some()
        );
        assert!(
            body("claude-3-opus-20240229", None)
                .get("temperature")
                .is_some()
        );
    }

    #[test]
    fn open_responses_title_request_matches_claude_code_wire_on_opus_5_5() {
        // 回归抓包：omp 的标题请求（temperature 0、effort low）经 open-responses 入口
        // 转成 Anthropic 后，上游因 adaptive 思考加 temperature 返回 400。
        let (body, _) = shaped(json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "temperature": 0,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "low"},
            "messages": [{"role": "user", "content": "测试"}],
        }));
        assert!(body.get("temperature").is_none());
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": "low"}));
    }

    #[test]
    fn fields_claude_code_never_sends_are_dropped() {
        let (body, _) = shaped(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 64,
            "service_tier": "auto",
            "container": "c1",
            "presence_penalty": 0.1,
            "stop_sequences": ["x"],
            "tool_choice": {"type": "auto"},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in &keys {
            assert!(CLAUDE_CODE_FIELDS.contains(key), "unexpected field {key}");
        }
        assert!(keys.contains(&"stop_sequences") && keys.contains(&"tool_choice"));
    }

    #[test]
    fn unsupported_tool_projection_does_not_leave_an_invalid_forced_choice() {
        for supported_tool in [false, true] {
            let mut body = json!({
                "tools": [{"type":"code_execution_20260120","name":"API-only"}],
                "tool_choice": {"type":"tool","name":"API-only"}
            });
            if supported_tool {
                body["tools"].as_array_mut().unwrap().push(json!({
                    "name":"available","input_schema":{"type":"object"}
                }));
            }
            restrict_to_claude_code_fields(body.as_object_mut().unwrap());
            assert!(body.get("tool_choice").is_none());
            assert!(
                !body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "API-only")
            );
            if supported_tool {
                assert_eq!(
                    body["tools"],
                    json!([{"name":"available","input_schema":{"type":"object"}}])
                );
            } else {
                assert_eq!(body["tools"], json!([]));
            }
        }
    }

    #[test]
    fn official_nested_controls_survive_but_unknown_parameter_keys_do_not() {
        let schema = json!({"type":"object","properties":{"private_parameter":{"type":"string"}}});
        let mut body = json!({
            "model":"claude-opus-4-6", "messages":[{"role":"user","content":"hi"}],
            "tools":[
                {"name":"business","input_schema":schema,"defer_loading":false,
                 "eager_input_streaming":true,"citations":{"enabled":true,"private_parameter":1},
                 "cache_control":{"type":"ephemeral","ttl":"5m"},
                 "allowed_callers":["direct"],"input_examples":[{"query":"SDK-only"}],
                 "private_parameter":1},
                {"type":"web_search_20250305","name":"web_search","max_uses":8,
                 "user_location":{"type":"approximate","city":"Paris","private_parameter":1},
                 "search_profile":"official-profile","allowed_domains":["example.com"],"private_parameter":1},
                {"type":"code_execution_20260120","name":"API-only"}
            ],
            "metadata":{"user_id":"spoofed","private_parameter":1},
            "max_tokens":4096,"private_parameter":1,
            "thinking":{"type":"adaptive","display":"omitted","private_parameter":1,
                "block_binding":{"prefix_mismatch_behavior":"error","private_parameter":1}},
            "output_config":{"effort":"high","private_parameter":1,
                "format":{"type":"json_schema","schema":schema,"private_parameter":1},
                "task_budget":{"type":"tokens","total":100000,"remaining":90000,"private_parameter":1}},
            "context_management":{"private_parameter":1,"edits":[
                {"type":"clear_thinking_20251015","keep":"all","private_parameter":1},
                {"type":"clear_tool_uses_20250919","trigger":{"type":"input_tokens","value":1000,"private_parameter":1}},
                {"type":"unknown_private_edit","private_parameter":1}]},
            "tool_choice":{"type":"auto","disable_parallel_tool_use":true,"private_parameter":1},
            "speed":"fast",
            "thread":{"type":"continue","previous_message_id":"msg_previous","private_parameter":1},
            "diagnostics":{"previous_message_id":"msg_previous","private_parameter":1},
            "cache_control":{"type":"ephemeral","ttl":"1h","evict_on_complete":true,"private_parameter":1},
            "context_hint":{"enabled":true,"target_tokens_saved":1000,"private_parameter":1},
            "safeguards":[{"type":"dangerous_tool_use","classifier_context":{"business_key":1},"private_parameter":1}],
            "fallbacks":[{"model":"claude-sonnet-4-5","private_parameter":1}]
        });
        restrict_to_claude_code_fields(body.as_object_mut().unwrap());
        assert!(body.get("private_parameter").is_none());
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["tools"][0]["input_schema"], schema);
        assert_eq!(body["tools"][0]["defer_loading"], false);
        for field in [
            "eager_input_streaming",
            "citations",
            "cache_control",
            "allowed_callers",
            "input_examples",
            "private_parameter",
        ] {
            assert!(body["tools"][0].get(field).is_none(), "{field}");
        }
        assert!(body["tools"][1].get("search_profile").is_none());
        assert_eq!(
            body["tools"][1]["user_location"],
            json!({"type":"approximate","city":"Paris"})
        );
        assert!(body["tools"][1].get("private_parameter").is_none());
        assert_eq!(body["output_config"]["format"]["schema"], schema);
        assert_eq!(
            body["thinking"]["block_binding"],
            json!({"prefix_mismatch_behavior":"error"})
        );
        assert_eq!(
            body["output_config"]["task_budget"],
            json!({"type":"tokens","total":100000,"remaining":90000})
        );
        assert_eq!(
            body["context_management"]["edits"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            body["context_management"]["edits"][1]["trigger"],
            json!({"type":"input_tokens","value":1000})
        );
        for field in [
            "thinking",
            "output_config",
            "tool_choice",
            "thread",
            "diagnostics",
            "cache_control",
            "context_hint",
        ] {
            assert!(body[field].get("private_parameter").is_none(), "{field}");
        }
        assert_eq!(
            body["safeguards"][0],
            json!({"type":"dangerous_tool_use","classifier_context":{"business_key":1}})
        );
        assert_eq!(body["fallbacks"], json!([{"model":"claude-sonnet-4-5"}]));
        let betas = beta_header(body.as_object().unwrap());
        for beta in [
            "task-budgets-2026-03-13",
            "message-threads-2026-08-12",
            "dangerous-tool-use-2026-09-03",
            "fast-mode-2026-02-01",
        ] {
            assert!(betas.split(',').any(|actual| actual == beta));
        }
        // End-to-end shaping replaces the entire metadata object; neither
        // user-supplied identity nor private metadata can become an authority.
        let (shaped, request) = shaped(body);
        assert_eq!(
            shaped["thinking"]["block_binding"],
            json!({"prefix_mismatch_behavior":"error"})
        );
        let identity: Value =
            serde_json::from_str(shaped["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(identity["session_id"], request.session_id);
        assert!(shaped["tools"][0].get("eager_input_streaming").is_none());
        assert!(shaped["tools"][0].get("cache_control").is_none());
        assert_eq!(shaped["tools"][1]["name"], "web_search");
        assert_eq!(
            shaped["tools"][1]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert_eq!(shaped["metadata"].as_object().unwrap().len(), 1);
        assert!(identity.get("private_parameter").is_none());
    }

    fn shaped_with_metadata(user_id: &str) -> (Value, ShapedRequest) {
        shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "metadata": {"user_id": user_id},
            "messages": [{"role": "user", "content": "hi"}],
        }))
    }
}
