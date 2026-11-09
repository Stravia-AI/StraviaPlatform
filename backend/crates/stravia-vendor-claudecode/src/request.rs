//! 把 codec 编码出的 Anthropic Messages 请求改写为官方 Claude Code 交互式 CLI 的线上形态。
//!
//! 形态依据官方 2.1.317 真正终端交互的 200 多轮抓包（默认 system 与自定义 system
//! 两种 profile）：计费头 `system[0]`、CLI 身份块、`metadata.user_id`
//! 内嵌 JSON、`diagnostics` 与最终字节 `cch`。业务内容、工具定义和
//! JSON schema 不按请求参数递归过滤；自定义工具额外使用 `_` 前缀与内建工具隔离。
//!
//! [`prepare`] 归一参数；[`encode`] 始终发送完整历史、system 和 tools。

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::cache;
use crate::capabilities::{self, ModelCapabilities};
use crate::session::WireContext;
use crate::thinking::{self, ModelProfile};
use crate::xxhash::Xxh64;

/// 未配置覆盖值时上报的 Claude Code CLI 正式发行版本。
pub(crate) const DEFAULT_CLIENT_VERSION: &str = "2.1.319";
/// 与该 Claude Code 发布包内的 `@anthropic-ai/sdk` 身份保持一致。
pub(crate) const SDK_VERSION: &str = "0.128.0";
const RUNTIME_VERSION: &str = "v26.12.1";
/// Stainless 平台头只描述「客户端运行环境」；插件在 Wasm 内拿不到宿主平台，
/// 固定为自托管服务端最常见的 Linux x64。
const STAINLESS_OS: &str = "Linux";
const STAINLESS_ARCH: &str = "x64";

pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
/// 普通交互式 CLI 的身份；非交互 SDK 的另一种身份不能混入同一个请求。
const IDENTITY_TEXT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const SDK_IDENTITY_TEXT: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const SDK_APPEND_IDENTITY_TEXT: &str = "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.";
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
const BILLING_ENTRYPOINT: &str = "cli";
const BILLING_TURN_ORIGIN: &str = "human";
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const FINGERPRINT_INDICES: [usize; 3] = [4, 7, 20];
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const CCH_MASK: u64 = 0xfffff;
const CCH_PLACEHOLDER: &[u8] = b"cch=00000";
const CCH_SYSTEM_PREFIX: &[u8] = b"[{\"type\":\"text\",\"text\":\"x-anthropic-billing-header:";
/// 官方只在 system 值起点之后这么多字节内寻找占位符。
const CCH_SEARCH_WINDOW: usize = 300;

/// CLI 2.1.319 first-party beta names; feature-dependent betas are selected below.
const TOOL_SEARCH_BETA: &str = "advanced-tool-use-2025-11-20";
const EFFORT_BETA: &str = "effort-2025-11-24";
const CACHE_DIAGNOSIS_BETA: &str = "cache-diagnosis-2026-04-07";

/// 线上 JSON 的对象键顺序。上游 SDK 按插入顺序序列化；Stravia 入口解析后
/// 原始顺序已丢失，这里按抓包里的字段构造顺序输出已知键，其余键按字典序排在
/// 后面。`cch` 只要求与实际发送字节一致，顺序用于贴近真实客户端。
/// `tool_use_id` 排在 `type` 之前：真实 `tool_result` 块是 `tool_use_id/type/content`。
const KEY_ORDER: &[&str] = &[
    "tool_use_id",
    "type",
    "role",
    "id",
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
    "thread",
    "diagnostics",
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
    "scope",
];

/// Closed first-party CLI contract: the public Messages parameters Claude Code
/// itself sends, checked against official 2.1.317 captures and the SDK's beta
/// message types. `metadata`, `thread` and `diagnostics` are intentionally not
/// admitted here: [`encode`] owns them, so client-supplied values never reach
/// the wire. CLI EXTRA_BODY / EXTRA_METADATA injection is not admitted either.
const CLAUDE_CODE_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
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
    pub betas: String,
}

/// 把 codec 产出的请求体归一为 Claude Code 的参数闭集，并保持历史完整：
///
/// - 剔除客户端的 `metadata` / `thread` / `diagnostics` 与全部缓存断点（可信的
///   metadata、diagnostics 和缓存断点由 [`encode`] 重建）；
/// - `system` 归一为客户端自有的块数组，剔除过期计费头和重复的身份块；
/// - 思考、effort、采样、prefill、上下文编辑按型号能力约束；
/// - 自定义工具声明、选择、排除列表及历史调用／引用统一加一层 `_` 前缀；
/// - `compact` 为真时保留 `compaction` 控制；与其他推理一样强制流式，原生压缩的
///   完整签名块和 `usage.iterations` 由 SSE 信封一次性交付。
///
/// `auto_output_limit` 表示 `max_tokens` 来自发现数据或编码器的必填回退，此时思考
/// 预算需让位于该上限。工具 ID、参数、schema 与 `tool_result.is_error` 保持原值。
pub(crate) fn prepare(
    mut body: Value,
    caps: &ModelCapabilities,
    auto_output_limit: bool,
    compact: bool,
) -> Result<Value, String> {
    let object = body
        .as_object_mut()
        .ok_or("Anthropic request body must be a JSON object")?;
    // `compaction` 不在 Claude Code 的闭合字段集内；字段限制期间暂存，随后原样放回。
    let compaction = if compact {
        Some(
            object
                .remove("compaction")
                .ok_or("compaction request is missing its compaction control")?,
        )
    } else {
        None
    };
    // 与 OMP 的原生压缩一致走流式，避免非流式请求的长输出时限。
    object.insert("stream".into(), Value::Bool(true));
    restrict_to_claude_code_fields(object);
    if let Some(compaction) = compaction {
        object.insert("compaction".into(), compaction);
    }
    // CLI 的自动策略先于 caller extra body；显式策略整体优先，原生压缩不混入编辑。
    if !object.contains_key("context_management")
        && caps.context_management != Some(false)
        && caps.thinking != Some(false)
        && caps.context_edits.get("clear_thinking_20251015") != Some(&false)
        && !object
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|model| model.contains("claude-3-"))
        && object
            .get("thinking")
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            != Some("disabled")
        && object
            .get("output_config")
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            != Some("none")
        && !carries_compaction(object)
    {
        object.insert(
            "context_management".into(),
            json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]}),
        );
    }
    let explicit_display = has_explicit_display(object);
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
    default_thinking_display(object, caps, explicit_display);
    capabilities::constrain(object, caps);
    drop_disallowed_sampling(object, caps);
    drop_unsupported_prefill(object, caps);
    normalize_system(object)?;
    if object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        object.remove("tools");
    }
    cache::strip(object);
    prefix_tool_names(object);
    Ok(body)
}

/// 把 [`prepare`] 产出的完整请求序列化为最终线上字节（已写入 `cch`）。
///
/// 全量发送历史和工具；普通请求携带上一次消息 ID 的诊断信息，压缩
/// 不携带诊断信息。缓存断点只复制承载它的那一条消息。
pub(crate) fn encode(
    body: &Value,
    client_version: &str,
    identity: &AccountIdentity<'_>,
    context: &WireContext,
    caps: &ModelCapabilities,
    suppress_diagnostics: bool,
) -> Result<ShapedRequest, String> {
    let object = body
        .as_object()
        .ok_or("Anthropic request body must be a JSON object")?;
    let history: &[Value] = match object.get("messages") {
        None => &[],
        Some(Value::Array(messages)) => messages,
        Some(_) => return Err("Anthropic messages must be an array".into()),
    };
    let messages = history;

    let mut system = vec![json!({
        "type": "text",
        "text": billing_header(
            client_version,
            &fingerprint(first_user_text(history), client_version),
            context,
        ),
    })];
    system.push(json!({ "type": "text", "text": IDENTITY_TEXT }));
    match object.get("system") {
        None => {}
        Some(Value::Array(blocks)) => system.extend(blocks.iter().cloned()),
        Some(_) => return Err("Anthropic system must be prepared into an array".into()),
    }
    cache::anchor_system(&mut system[1..]);

    let mut entries: Vec<(&str, Slot<'_>)> = Vec::with_capacity(object.len() + 4);
    for (key, value) in object {
        match key.as_str() {
            // 丢弃客户端提供的这些字段；本函数重建可信字段。
            "messages" | "system" | "metadata" | "thread" | "diagnostics" => {}
            _ => entries.push((key.as_str(), Slot::Borrowed(value))),
        }
    }
    entries.push((
        "messages",
        Slot::Messages {
            items: messages,
            anchor: cache::anchor_tail(messages),
        },
    ));
    entries.push(("system", Slot::Owned(Value::Array(system))));
    entries.push((
        "metadata",
        Slot::Owned(json!({ "user_id": metadata_user_id(&context.session_id, identity) })),
    ));
    if !suppress_diagnostics && !carries_compaction(object) {
        entries.push((
            "diagnostics",
            Slot::Owned(json!({
                "previous_message_id": context.previous_message_id.as_deref(),
            })),
        ));
    }

    let betas = beta_header(object, caps);
    let mut bytes = Vec::new();
    let system_at = write_body(entries, &mut bytes).map_err(|error| error.to_string())?;
    patch_cch(&mut bytes, system_at)?;
    Ok(ShapedRequest { body: bytes, betas })
}

/// Claude Code 推理请求头。压缩策略通过强类型契约交给宿主。
/// 第一方交互请求包含 prompt、request-class 与每次调用的新客户端请求 ID。
/// 自定义网关地址的行为不同，不能用于推断第一方请求。
pub(crate) fn inference_headers(
    access_token: &str,
    client_version: &str,
    context: &WireContext,
    betas: String,
    tools_used: String,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Accept".into(), "application/json".into()),
        ("Authorization".into(), format!("Bearer {access_token}")),
        ("Content-Type".into(), "application/json".into()),
        ("User-Agent".into(), cli_user_agent(client_version)),
        (
            "X-Claude-Code-Session-Id".into(),
            context.session_id.clone(),
        ),
        ("X-Stainless-Arch".into(), STAINLESS_ARCH.into()),
        ("X-Stainless-Lang".into(), "js".into()),
        ("X-Stainless-OS".into(), STAINLESS_OS.into()),
        ("X-Stainless-Package-Version".into(), SDK_VERSION.into()),
        ("X-Stainless-Retry-Count".into(), "0".into()),
        ("X-Stainless-Runtime".into(), "node".into()),
        ("X-Stainless-Runtime-Version".into(), RUNTIME_VERSION.into()),
        ("X-Stainless-Timeout".into(), "600".into()),
        ("anthropic-beta".into(), betas),
        (
            "anthropic-dangerous-direct-browser-access".into(),
            "true".into(),
        ),
        ("anthropic-version".into(), "2023-06-01".into()),
        ("x-app".into(), "cli".into()),
    ];
    if !context.prompt_id.is_empty() {
        headers.push(("x-claude-code-prompt-id".into(), context.prompt_id.clone()));
    }
    headers.push(("x-claude-code-request-class".into(), "main".into()));
    headers.push(("x-claude-code-sandy-plover".into(), tools_used));
    headers.push((
        "x-client-request-id".into(),
        uuid::Uuid::new_v4().to_string(),
    ));
    headers.push(("Connection".into(), "keep-alive".into()));
    headers.push(("Host".into(), String::new()));
    headers.push(("Accept-Encoding".into(), "gzip, deflate, br, zstd".into()));
    headers.push(("Content-Length".into(), String::new()));
    headers
}

pub(crate) fn cli_user_agent(client_version: &str) -> String {
    format!("claude-cli/{client_version} (external, cli)")
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
    // Custom tool wire shape (2.1.317 capture): `name/description/input_schema`
    // followed by `eager_input_streaming: true`; deferred tools carry
    // `defer_loading: true` instead and never the eager flag. Cache anchors are
    // generated by `encode`, never copied from the client tool definition.
    let custom = match tool.get("type").and_then(Value::as_str) {
        None | Some("custom") if tool.contains_key("input_schema") => true,
        Some("web_search_20250305") => false,
        _ => return false,
    };
    if custom {
        tool.retain(|key, _| {
            matches!(
                key.as_str(),
                "name" | "description" | "input_schema" | "strict" | "defer_loading"
            )
        });
        if tool.get("defer_loading").and_then(Value::as_bool) != Some(true) {
            tool.insert("eager_input_streaming".into(), Value::Bool(true));
        }
    } else {
        tool.retain(|key, _| {
            matches!(
                key.as_str(),
                "type"
                    | "name"
                    | "allowed_domains"
                    | "blocked_domains"
                    | "max_uses"
                    | "user_location"
            )
        });
        retain_control_fields(
            tool.get_mut("user_location"),
            &["type", "country", "region", "city", "timezone"],
        );
    }
    true
}

/// 与 OMP 一致：已带 `_` 的自定义名称仍加一层，避免 `read` 与 `_read` 碰撞。
/// 只豁免 Anthropic 内建名称，不能把 Claude Code 客户端的 Read/Bash 当成例外。
fn prefix_tool_name(field: Option<&mut Value>) {
    if let Some(Value::String(name)) = field
        && !["web_search", "code_execution", "text_editor", "computer"]
            .iter()
            .any(|builtin| name.eq_ignore_ascii_case(builtin))
    {
        name.insert(0, '_');
    }
}

fn prefix_tool_names(object: &mut Map<String, Value>) {
    if let Some(Value::Array(tools)) = object.get_mut("tools") {
        for tool in tools {
            prefix_tool_name(tool.get_mut("name"));
        }
    }
    if let Some(choice) = object.get_mut("tool_choice") {
        prefix_tool_name(choice.get_mut("name"));
    }
    if let Some(Value::Array(edits)) = object
        .get_mut("context_management")
        .and_then(|context| context.get_mut("edits"))
    {
        for edit in edits {
            if edit.get("type").and_then(Value::as_str) == Some("clear_tool_uses_20250919")
                && let Some(Value::Array(names)) = edit.get_mut("exclude_tools")
            {
                for name in names {
                    prefix_tool_name(Some(name));
                }
            }
        }
    }
    if let Some(Value::Array(messages)) = object.get_mut("messages") {
        prefix_message_tool_names(messages);
    }
}

/// 请求历史中的工具名称与工具声明一致。
fn prefix_message_tool_names(messages: &mut [Value]) {
    for message in messages {
        if let Some(Value::Array(content)) = message.get_mut("content") {
            prefix_content_tool_names(content);
        }
    }
}

fn prefix_content_tool_names(content: &mut [Value]) {
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => prefix_tool_name(block.get_mut("name")),
            Some("tool_reference") => prefix_tool_name(block.get_mut("tool_name")),
            Some("tool_result") => {
                if let Some(Value::Array(nested)) = block.get_mut("content") {
                    prefix_content_tool_names(nested);
                }
            }
            // 不递归改写 input、schema、正文或服务端工具的原生内容。
            _ => {}
        }
    }
}

pub(crate) fn strip_tool_prefix(name: &str) -> Option<String> {
    name.strip_prefix('_').map(str::to_owned)
}

fn retain_control_fields(field: Option<&mut Value>, fields: &[&str]) {
    if let Some(object) = field.and_then(Value::as_object_mut) {
        object.retain(|key, _| fields.contains(&key.as_str()));
    }
}

/// 客户端是否显式给了 `thinking.display`（含 `thinking::normalize` 会映射的同义取值）。
/// 显式意图（摘要、省略、updates）原样保留；只有未表态时才采用原生默认。
fn has_explicit_display(object: &Map<String, Value>) -> bool {
    matches!(
        object
            .get("thinking")
            .and_then(|thinking| thinking.get("display"))
            .and_then(Value::as_str),
        Some(
            "summarized"
                | "auto"
                | "concise"
                | "detailed"
                | "omitted"
                | "none"
                | "disabled"
                | "hidden"
                | "updates"
        )
    )
}

/// 原生 2.1.317 的自适应思考请求默认 `display: updates`。`thinking::normalize`
/// 为新型号补的默认摘要只是「没人表态」的占位，在这里换成原生默认；手动预算
/// （`enabled`）模式没有该证据，型号未识别时也不替客户端加字段。
fn default_thinking_display(
    object: &mut Map<String, Value>,
    caps: &ModelCapabilities,
    explicit_display: bool,
) {
    if explicit_display || ModelProfile::of_request(object, caps).is_none() {
        return;
    }
    if let Some(thinking) = object.get_mut("thinking").and_then(Value::as_object_mut)
        && thinking.get("type").and_then(Value::as_str) == Some("adaptive")
    {
        thinking.insert("display".into(), Value::String("updates".into()));
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

fn text_block(block: &Value) -> Option<&str> {
    (block.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| block.get("text").and_then(Value::as_str))
        .flatten()
}

/// `system` 归一为客户端自有的块数组：剔除过期计费头（例如直连 Claude Code 的请求体
/// 经网关改写后已失效）和开头重复的身份块；没有任何客户端 system 时省略该字段。
/// 计费头与 CLI 身份在 [`encode`] 里加入。
fn normalize_system(object: &mut Map<String, Value>) -> Result<(), String> {
    let mut blocks = match object.remove("system") {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::String(text)) if text.is_empty() => return Ok(()),
        Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
        Some(Value::Array(blocks)) => blocks,
        Some(_) => return Err("Anthropic system must be a string or an array of blocks".into()),
    };
    blocks.retain(|block| !text_block(block).is_some_and(|text| text.starts_with(BILLING_PREFIX)));
    let duplicated_identity = blocks
        .iter()
        .take_while(|block| {
            text_block(block).is_some_and(|text| {
                matches!(
                    text,
                    IDENTITY_TEXT | SDK_IDENTITY_TEXT | SDK_APPEND_IDENTITY_TEXT
                )
            })
        })
        .count();
    blocks.drain(..duplicated_identity);
    if !blocks.is_empty() {
        object.insert("system".into(), Value::Array(blocks));
    }
    Ok(())
}

/// 基于完整 canonical 历史的首条用户文本。
/// 原生 CLI 用独立 isMeta 标记排除自己注入的上下文；本插件不注入这些块，也不能
/// 凭用户文本以 `<system-reminder>` 开头就猜测其来源并忽略它。
fn first_user_text(messages: &[Value]) -> &str {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .find_map(|message| match message.get("content") {
            Some(Value::String(text)) => Some(text.as_str()),
            Some(Value::Array(blocks)) => blocks.iter().find_map(text_block),
            _ => None,
        })
        .map_or("", |text| text)
}

/// `SHA256(salt + msg[4] + msg[7] + msg[20] + version)` 前 3 个十六进制字符。
/// 下标按 JavaScript 字符串（UTF-16 码元）计算；落在代理对上的码元按 UTF-8
/// 编码孤立代理的方式替换为 U+FFFD，与 JS 运行时的哈希输入一致。
fn fingerprint(first_user_text: &str, client_version: &str) -> String {
    let mut sampled = [u16::from(b'0'); 3];
    for (index, unit) in first_user_text.encode_utf16().take(21).enumerate() {
        if let Some(slot) = FINGERPRINT_INDICES
            .iter()
            .position(|&wanted| wanted == index)
        {
            sampled[slot] = unit;
        }
    }
    let mut hash = Sha256::new();
    hash.update(FINGERPRINT_SALT.as_bytes());
    for character in char::decode_utf16(sampled) {
        let mut bytes = [0; 4];
        hash.update(
            character
                .unwrap_or(char::REPLACEMENT_CHARACTER)
                .encode_utf8(&mut bytes)
                .as_bytes(),
        );
    }
    hash.update(client_version.as_bytes());
    let digest = hash.finalize();
    format!("{:03x}", u16::from_be_bytes([digest[0], digest[1]]) >> 4)
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn is_request_id(text: &str) -> bool {
    text.strip_prefix("req_").is_some_and(|rest| {
        (1..=36).contains(&rest.len())
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

/// 计费头：`cc_version`、`cc_entrypoint`、`cch` 占位、`cc_prev_req`（上一次真实 HTTP
/// 响应的 request-id）、`cc_prompt_id`、`cc_turn_origin`、`cc_prompt_index`/`cc_turn_index`。
/// 可选字段和官方一样只在取值合法时出现。`cch=00000` 由 [`patch_cch`] 改写为校验值。
fn billing_header(client_version: &str, fingerprint: &str, context: &WireContext) -> String {
    let mut text = format!(
        "{BILLING_PREFIX} cc_version={client_version}.{fingerprint}; cc_entrypoint={BILLING_ENTRYPOINT}; cch=00000;"
    );
    if let Some(request_id) = context
        .previous_request_id
        .as_deref()
        .filter(|id| is_request_id(id))
    {
        text.push_str(&format!(" cc_prev_req={request_id};"));
    }
    if is_uuid(&context.prompt_id) {
        text.push_str(&format!(" cc_prompt_id={};", context.prompt_id));
    }
    text.push_str(&format!(" cc_turn_origin={BILLING_TURN_ORIGIN};"));
    if context.turn_index >= 1 {
        text.push_str(&format!(
            " cc_prompt_index={}; cc_turn_index={};",
            context.prompt_index, context.turn_index
        ));
    }
    text
}

/// 字段顺序与官方一致：`device_id`、`account_uuid`、`session_id`。
#[derive(Serialize)]
struct MetadataUserId<'a> {
    device_id: &'a str,
    account_uuid: &'a str,
    session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<&'a str>,
}

/// `metadata.user_id` 由上游账号身份和宿主 scoped 会话/父会话标签重建。
fn metadata_user_id(session_id: &str, identity: &AccountIdentity<'_>) -> String {
    serde_json::to_string(&MetadataUserId {
        device_id: identity.device_id.unwrap_or_default(),
        account_uuid: identity.account_uuid.unwrap_or_default(),
        session_id,
        parent_session_id: identity.parent_session_id,
    })
    .expect("metadata identity serializes")
}

/// Client beta selection with documented first-party defaults and discovered
/// capabilities. Remote feature assignments are intentionally not fetched.
fn beta_header(object: &Map<String, Value>, caps: &ModelCapabilities) -> String {
    let model = object.get("model").and_then(Value::as_str).unwrap_or("");
    let claude_3 = model.contains("claude-3-");
    let thinking = object
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "enabled" | "adaptive"));
    let has_deferred_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("defer_loading").and_then(Value::as_bool) == Some(true)
                    || tool.get("name").and_then(Value::as_str) == Some("ToolSearch")
            })
        });
    let has_effort = object
        .get("output_config")
        .and_then(|config| config.get("effort"))
        .is_some();
    let mut betas = Vec::new();
    if !model.contains("haiku") {
        betas.push("claude-code-20250219");
    }
    betas.push(OAUTH_BETA);
    if !claude_3 {
        betas.push("interleaved-thinking-2025-05-14");
    }
    if !claude_3 && caps.context_management != Some(false) {
        betas.push("context-management-2025-06-27");
    }
    betas.push("prompt-caching-scope-2026-01-05");
    // NBn 为主交互补回基础列表排除的 Haiku CLI beta，位置在基础列表末尾。
    if model.contains("haiku") {
        betas.push("claude-code-20250219");
    }
    if let Some(profile) = ModelProfile::new(model, caps) {
        if profile.per_turn_effort() {
            betas.push("per-turn-control-2026-07-01");
        }
        if profile.mid_conversation_tools() {
            betas.push("mid-conversation-tool-changes-2026-07-01");
        }
    }
    // The first-party default for the provenance feature is enabled, but remote
    // overrides and its accompanying provenance details are unavailable here.
    betas.push("tool-provenance-2026-10-26");
    // Native first-party defaults PBn and DIr both reject only Claude 3.
    if !claude_3 && caps.thinking != Some(false) {
        betas.push("thinking-token-count-2026-05-13");
    }
    if has_deferred_tools {
        betas.push(TOOL_SEARCH_BETA);
    }
    if has_effort {
        betas.push(EFFORT_BETA);
    }
    if object
        .get("output_config")
        .and_then(|v| v.get("task_budget"))
        .is_some()
    {
        betas.push("task-budgets-2026-03-13");
    }
    if object
        .get("output_config")
        .and_then(|v| v.get("format"))
        .is_some()
    {
        betas.push("structured-outputs-2025-12-15");
    }
    if thinking {
        betas.push("thinking-binding-controls-2026-08-01");
        if object
            .get("thinking")
            .and_then(|v| v.get("display"))
            .and_then(Value::as_str)
            == Some("updates")
        {
            betas.push("thinking-display-updates-2026-08-18");
        }
    }
    // This encoder places a one-hour cache marker on the identity block.
    betas.push("extended-cache-ttl-2025-04-11");
    betas.push(CACHE_DIAGNOSIS_BETA);
    for (present, beta) in [
        (
            object
                .get("cache_control")
                .and_then(|v| v.get("evict_on_complete"))
                .and_then(Value::as_bool)
                == Some(true),
            "prompt-caching-evict-2026-05-12",
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
            object.contains_key("fallbacks"),
            if object.get("fallbacks").and_then(Value::as_str) == Some("default") {
                "server-side-fallback-2026-07-01"
            } else {
                "server-side-fallback-2026-06-01"
            },
        ),
        // 压缩请求参数与回放的压缩块都要求该 beta，缺失时上游把 `compaction`
        // 当作未知内容块类型并以 400 拒绝。
        (
            carries_compaction(object),
            stravia_protocol_codec::codec::compaction::ANTHROPIC_COMPACTION_BETA,
        ),
    ] {
        if present && !betas.contains(&beta) {
            betas.push(beta);
        }
    }
    betas.join(",")
}

pub(crate) fn carries_compaction(object: &Map<String, Value>) -> bool {
    object.contains_key("compaction")
        || object
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block.get("type").and_then(Value::as_str) == Some("compaction")
                            })
                        })
                })
            })
}

/// 顶层字段的来源：借用 prepare 的结果、本函数新建的小块，或只在尾部消息上带
/// 缓存断点的消息序列。
enum Slot<'a> {
    Borrowed(&'a Value),
    Owned(Value),
    Messages {
        items: &'a [Value],
        anchor: Option<(usize, Value)>,
    },
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

fn write_slot(slot: &Slot<'_>, out: &mut Vec<u8>) -> serde_json::Result<()> {
    match slot {
        Slot::Borrowed(value) => write_ordered(value, out),
        Slot::Owned(value) => write_ordered(value, out),
        Slot::Messages { items, anchor } => {
            out.push(b'[');
            for (index, message) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                match anchor {
                    Some((anchored, marked)) if *anchored == index => write_ordered(marked, out)?,
                    _ => write_ordered(message, out)?,
                }
            }
            out.push(b']');
            Ok(())
        }
    }
}

/// 返回顶层 `system` 值在 `out` 中的起始偏移；`cch` 只在自建计费块里改写，不会被
/// 消息正文里形似计费头的内容误导。
fn write_body(
    mut entries: Vec<(&str, Slot<'_>)>,
    out: &mut Vec<u8>,
) -> serde_json::Result<Option<usize>> {
    entries.sort_by(|(left, _), (right, _)| {
        key_rank(left)
            .cmp(&key_rank(right))
            .then_with(|| left.cmp(right))
    });
    let mut system_at = None;
    out.push(b'{');
    for (index, (key, slot)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut *out, key)?;
        out.push(b':');
        if *key == "system" {
            system_at = Some(out.len());
        }
        write_slot(slot, out)?;
    }
    out.push(b'}');
    Ok(system_at)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// `cch` 校验值的输入不是整个请求体：官方原生 fetch 层在哈希前排除下面这些字节
/// （2.1.317 的 18 条真实抓包逐条验证）。匹配的是紧凑 JSON 字面量，因此字符串内部被
/// 转义过的 `\"model\":\"` 不会误中。
///
/// - `"model":"VALUE"` 的 VALUE（保留键与两个引号）；
/// - 整个 `"max_tokens":DIGITS` 字段；
/// - 整个 `"fallbacks":[...]` 字段（数组内嵌套括号与转义引号按 JSON 字符串规则跳过）；
/// - 整个 `"fallback_credit_token":"..."` 字段。
///
/// 整字段排除时优先连同其后的逗号一起排除；字段是最后一个时改为排除其前的逗号。
const CCH_MODEL_KEY: &[u8] = b"\"model\":\"";
const CCH_MAX_TOKENS_KEY: &[u8] = b"\"max_tokens\":";
const CCH_FALLBACKS_KEY: &[u8] = b"\"fallbacks\":[";
const CCH_CREDIT_TOKEN_KEY: &[u8] = b"\"fallback_credit_token\":\"";

/// `from` 之后第一个未被转义的双引号的位置。
fn closing_quote(body: &[u8], from: usize) -> Option<usize> {
    let mut index = from;
    while index < body.len() {
        match body[index] {
            b'\\' => index += 2,
            b'"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

/// `open` 处 `[` 对应的 `]` 之后的位置；字符串内的括号与转义引号不计。
fn array_end(body: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < body.len() {
        match body[index] {
            b'"' => index = closing_quote(body, index + 1)?,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn field_span(
    body: &[u8],
    start: usize,
    end: usize,
    previous_end: usize,
) -> std::ops::Range<usize> {
    if body.get(end) == Some(&b',') {
        start..end + 1
    } else if start > previous_end && body[start - 1] == b',' {
        start - 1..end
    } else {
        start..end
    }
}

/// 顺序产生互不重叠的区间，不为长对话中的每一个 model/max_tokens 分配索引。
fn cch_excluded_spans(body: &[u8]) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let mut at = 0;
    let mut previous_end = 0;
    std::iter::from_fn(move || {
        loop {
            let offset = body.get(at..)?.iter().position(|byte| *byte == b'"')?;
            let start = at + offset;
            let rest = &body[start..];
            at = start + 1;
            let span = if rest.starts_with(CCH_MODEL_KEY) {
                let value = start + CCH_MODEL_KEY.len();
                // native fetch 对 model 值使用 memchr，而不是 JSON 字符串解码器。
                let end = value + body[value..].iter().position(|byte| *byte == b'"')?;
                at = end;
                (end > value).then_some(value..end)
            } else if rest.starts_with(CCH_MAX_TOKENS_KEY) {
                let digits = start + CCH_MAX_TOKENS_KEY.len();
                let end = digits
                    + body[digits..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_digit())
                        .count();
                if end > digits {
                    at = end;
                    Some(field_span(body, start, end, previous_end))
                } else {
                    None
                }
            } else if rest.starts_with(CCH_FALLBACKS_KEY) {
                let open = start + CCH_FALLBACKS_KEY.len() - 1;
                let end = array_end(body, open)?;
                at = end;
                Some(field_span(body, start, end, previous_end))
            } else if rest.starts_with(CCH_CREDIT_TOKEN_KEY) {
                let value = start + CCH_CREDIT_TOKEN_KEY.len();
                let end = value + body[value..].iter().position(|byte| *byte == b'"')? + 1;
                at = end;
                Some(field_span(body, start, end, previous_end))
            } else {
                None
            };
            if let Some(span) = span {
                previous_end = span.end;
                return Some(span);
            }
        }
    })
}

/// 对含 `cch=00000` 占位符的完整请求体计算 XXH64 低 20 位；排除区间直接从缓冲区
/// 上跳过，不复制请求体。
fn cch_digest(body: &[u8]) -> u64 {
    let mut hasher = Xxh64::new(CCH_SEED);
    let mut cursor = 0;
    for span in cch_excluded_spans(body) {
        hasher.update(&body[cursor..span.start]);
        cursor = span.end;
    }
    hasher.update(&body[cursor..]);
    hasher.finish() & CCH_MASK
}

/// 在含占位符的完整请求体上计算校验值并写回占位符（与 Claude Code 就地修补的行为
/// 一致）。
fn patch_cch(body: &mut [u8], system_at: Option<usize>) -> Result<(), String> {
    let system_at = system_at.ok_or("request has no system blocks")?;
    if !body[system_at..].starts_with(CCH_SYSTEM_PREFIX) {
        return Err("billing header is not the first system block".to_string());
    }
    let placeholder = find(body, CCH_PLACEHOLDER, system_at)
        .filter(|index| index - system_at <= CCH_SEARCH_WINDOW)
        .ok_or("billing header cch placeholder is missing")?;
    let cch = format!("{:05x}", cch_digest(body));
    body[placeholder + 4..placeholder + 9].copy_from_slice(cch.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";
    const PROMPT: &str = "22222222-2222-4222-8222-222222222222";

    fn identity() -> AccountIdentity<'static> {
        AccountIdentity {
            account_uuid: Some("00000000-0000-4000-8000-0000000000aa"),
            device_id: Some("d".repeat(64).leak()),
            parent_session_id: None,
        }
    }

    fn context() -> WireContext {
        WireContext {
            session_id: SESSION.into(),
            prompt_id: PROMPT.into(),
            prompt_index: 1,
            turn_index: 1,
            previous_message_id: None,
            previous_request_id: None,
        }
    }

    fn prior_context() -> WireContext {
        WireContext {
            previous_message_id: Some("msg_previous".into()),
            previous_request_id: Some("req_previous".into()),
            prompt_index: 2,
            turn_index: 2,
            ..context()
        }
    }

    fn prep_with(body: Value, caps: &ModelCapabilities, auto: bool) -> Value {
        prepare(body, caps, auto, false).expect("request prepares")
    }

    fn prep(body: Value) -> Value {
        prep_with(body, &ModelCapabilities::default(), false)
    }

    fn wire(prepared: &Value, context: &WireContext) -> (Value, ShapedRequest) {
        let shaped = encode(
            prepared,
            DEFAULT_CLIENT_VERSION,
            &identity(),
            context,
            &ModelCapabilities::default(),
            false,
        )
        .expect("request encodes");
        (
            serde_json::from_slice(&shaped.body).expect("wire body is JSON"),
            shaped,
        )
    }

    /// 完整创建请求。
    fn created(body: Value) -> (Value, ShapedRequest) {
        wire(&prep(body), &context())
    }

    fn betas_of(shaped: &ShapedRequest) -> Vec<&str> {
        shaped.betas.split(',').collect()
    }

    fn user_turns() -> Value {
        json!({
            "model": "m",
            "max_tokens": 1,
            "system": "client system",
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"name": "late", "input_schema": {}, "defer_loading": true}
            ],
            "messages": [
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                ]}
            ],
        })
    }

    #[test]
    fn prepare_keeps_history_whole_and_leaves_thread_and_cache_to_encode() {
        let marked = json!({"type": "ephemeral", "ttl": "5m"});
        let prepared = prep(json!({
            "model": "m",
            "max_tokens": 1,
            "metadata": {"user_id": "client"},
            "thread": {"type": "continue", "previous_message_id": "msg_client"},
            "diagnostics": {"previous_message_id": "msg_client"},
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=9.9.9.abc; cch=12345;"},
                {"type": "text", "text": IDENTITY_TEXT},
                {"type": "text", "text": "client prompt", "cache_control": marked}
            ],
            "tools": [{"name": "a", "input_schema": {}, "cache_control": marked}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "one", "cache_control": marked}]},
                {"role": "assistant", "content": [{"type": "text", "text": "two"}]},
                {"role": "user", "content": "three"}
            ],
        }));
        for field in ["metadata", "thread", "diagnostics"] {
            assert!(prepared.get(field).is_none(), "{field} is encode-owned");
        }
        assert_eq!(
            prepared["system"],
            json!([{"type": "text", "text": "client prompt"}])
        );
        assert!(prepared["tools"][0].get("cache_control").is_none());
        assert_eq!(prepared["messages"].as_array().unwrap().len(), 3);
        assert!(!prepared.to_string().contains("cache_control"));
        // 字符串内容保持原样，缓存升级只发生在 encode 的尾部消息副本上。
        assert_eq!(prepared["messages"][2]["content"], "three");
        assert_eq!(prepared["stream"], json!(true));
    }

    #[test]
    fn prepare_normalizes_system_forms_and_drops_empty_collections() {
        let prepared = prep(json!({
            "model": "m", "max_tokens": 1, "system": "", "tools": [],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert!(prepared.get("system").is_none() && prepared.get("tools").is_none());
        let prepared = prep(json!({
            "model": "m", "max_tokens": 1, "system": "plain",
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            prepared["system"],
            json!([{"type": "text", "text": "plain"}])
        );
        let error = prepare(
            json!({"model": "m", "system": 7, "messages": []}),
            &ModelCapabilities::default(),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.contains("system"), "{error}");
    }

    #[test]
    fn create_request_matches_the_measured_sdk_shape() {
        let prepared = prep(user_turns());
        let (body, shaped) = wire(&prepared, &context());
        let text = String::from_utf8(shaped.body.clone()).unwrap();
        assert!(text.starts_with("{\"model\":"));
        let positions: Vec<usize> = [
            "\"messages\":",
            "\"system\":",
            "\"tools\":",
            "\"metadata\":",
            "\"max_tokens\":",
            "\"diagnostics\":",
            "\"stream\":",
        ]
        .iter()
        .map(|key| text.find(key).unwrap_or_else(|| panic!("missing {key}")))
        .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{positions:?}"
        );

        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .starts_with(BILLING_PREFIX)
        );
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["text"], IDENTITY_TEXT);
        let one_hour = json!({"type": "ephemeral", "ttl": "1h"});
        assert_eq!(system[1]["cache_control"], one_hour);
        assert_eq!(system[2]["text"], "client system");
        assert_eq!(system[2]["cache_control"], one_hour);

        assert!(body.get("thread").is_none());
        assert_eq!(body["diagnostics"], json!({"previous_message_id": null}));
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        // 只有最新消息的最后一个可缓存块带断点；tools 从不带。
        assert_eq!(body["messages"][2]["content"][0]["cache_control"], one_hour);
        assert!(body["messages"][0]["content"].is_string());
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(text.matches("cache_control").count(), 3);
        for tool in body["tools"].as_array().unwrap() {
            assert!(tool.get("cache_control").is_none());
        }
        // 输入是借用的：编码不会给 prepare 的结果打断点。
        assert!(!prepared.to_string().contains("cache_control"));
    }

    #[test]
    fn metadata_user_id_embeds_identity_in_measured_order() {
        let (body, _) = created(user_turns());
        assert_eq!(body["metadata"].as_object().unwrap().len(), 1);
        let user_id = body["metadata"]["user_id"].as_str().unwrap();
        let device = user_id.find("\"device_id\"").unwrap();
        let account = user_id.find("\"account_uuid\"").unwrap();
        let session = user_id.find("\"session_id\"").unwrap();
        assert!(device < account && account < session, "{user_id}");
        let parsed: Value = serde_json::from_str(user_id).unwrap();
        assert_eq!(parsed["session_id"], SESSION);
        assert_eq!(
            parsed["account_uuid"],
            "00000000-0000-4000-8000-0000000000aa"
        );
        assert_eq!(parsed["device_id"].as_str().unwrap().len(), 64);
        assert!(parsed.get("parent_session_id").is_none());
    }

    #[test]
    fn metadata_ignores_client_supplied_identity() {
        let (body, _) = created(json!({
            "model": "m", "max_tokens": 1,
            "metadata": {"user_id": "{\"session_id\":\"client\",\"account_uuid\":\"client\"}"},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let parsed: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["session_id"], SESSION);
        assert_ne!(parsed["account_uuid"], "client");
    }

    #[test]
    fn later_request_keeps_full_history_and_diagnostics() {
        let prepared = prep(user_turns());
        let (body, shaped) = wire(&prepared, &prior_context());
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["system"].as_array().unwrap().len(), 3);
        assert!(body.get("thread").is_none());
        assert_eq!(
            body["diagnostics"],
            json!({"previous_message_id": "msg_previous"})
        );
        assert!(!betas_of(&shaped).contains(&"message-threads-2026-08-12"));
    }

    #[test]
    fn no_previous_message_still_sends_diagnostics() {
        let (body, _) = wire(&prep(user_turns()), &context());
        assert_eq!(body["diagnostics"], json!({"previous_message_id": null}));
    }

    #[test]
    fn recovery_suppresses_diagnostics_without_slicing() {
        let prepared = prep(user_turns());
        let shaped = encode(
            &prepared,
            DEFAULT_CLIENT_VERSION,
            &identity(),
            &prior_context(),
            &ModelCapabilities::default(),
            true,
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&shaped.body).unwrap();
        assert!(body.get("diagnostics").is_none());
        assert!(body.get("thread").is_none());
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn prior_message_is_diagnostic_only() {
        let (body, _) = wire(&prep(user_turns()), &prior_context());
        assert!(body.get("thread").is_none());
        assert_eq!(body["diagnostics"]["previous_message_id"], "msg_previous");
    }

    #[test]
    fn billing_header_fields_follow_the_wire_contract() {
        let parts = |context: &WireContext| {
            let (body, _) = wire(&prep(user_turns()), context);
            let text = body["system"][0]["text"].as_str().unwrap().to_owned();
            text.trim_end_matches(';')
                .split("; ")
                .skip(1)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let first = parts(&context());
        let keys: Vec<&str> = first
            .iter()
            .map(|part| part.split('=').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "cc_entrypoint",
                "cch",
                "cc_prompt_id",
                "cc_turn_origin",
                "cc_prompt_index",
                "cc_turn_index"
            ]
        );
        assert!(first[2].ends_with(PROMPT) && first[4].ends_with("=1") && first[5].ends_with("=1"));
        assert_eq!(first[0], "cc_entrypoint=cli");
        assert_eq!(first[3], "cc_turn_origin=human");

        let later = parts(&prior_context());
        let keys: Vec<&str> = later
            .iter()
            .map(|part| part.split('=').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "cc_entrypoint",
                "cch",
                "cc_prev_req",
                "cc_prompt_id",
                "cc_turn_origin",
                "cc_prompt_index",
                "cc_turn_index"
            ]
        );
        assert_eq!(later[2], "cc_prev_req=req_previous");
        assert_eq!(later[5], "cc_prompt_index=2");
        assert_eq!(later[6], "cc_turn_index=2");
    }

    #[test]
    fn default_cli_keeps_diagnostics_without_thread() {
        let prepared = prep(user_turns());
        let (body, shaped) = wire(&prepared, &context());
        assert!(body.get("thread").is_none());
        assert_eq!(body["diagnostics"], json!({"previous_message_id": null}));
        assert!(!betas_of(&shaped).contains(&"message-threads-2026-08-12"));
    }

    #[test]
    fn billing_header_omits_optional_fields_the_native_client_would_not_emit() {
        let invalid = WireContext {
            prompt_id: "not-a-uuid".into(),
            previous_request_id: Some("msg_wrong prefix; cch=1".into()),
            turn_index: 0,
            ..prior_context()
        };
        let (body, _) = wire(&prep(user_turns()), &invalid);
        let text = body["system"][0]["text"].as_str().unwrap();
        for absent in [
            "cc_prompt_id",
            "cc_prev_req",
            "cc_turn_index",
            "cc_prompt_index",
        ] {
            assert!(!text.contains(absent), "{text}");
        }
        assert!(text.contains("cc_turn_origin=human;"));
    }

    #[test]
    fn fingerprint_matches_measured_sdk_vectors() {
        // 抓包：人类输入第 4/7/20 个码元为 ' '、' '、'p'，计费头指纹 a8e。
        assert_eq!(fingerprint("abcd ef ghijklmnopqrp", "2.1.317"), "a8e");
        // 消息过短时缺位字符用 "0" 填充。
        assert_eq!(fingerprint("", "2.1.280"), "d7b");
        // 取出的两个码元可能重新配成代理对，须按 JS 拼接后再 UTF-8 编码。
        assert_eq!(fingerprint("abcd😀😀abcdefghijklmnop", "2.1.317"), "20e");
    }

    #[test]
    fn fingerprint_uses_the_first_human_prompt_of_the_full_history() {
        let messages = vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "abcd ef ghijklmnopqrp"}
            ]}),
            json!({"role": "assistant", "content": "answer"}),
            json!({"role": "user", "content": "a later prompt"}),
        ];
        assert_eq!(first_user_text(&messages), "abcd ef ghijklmnopqrp");
        assert_eq!(first_user_text(&messages[2..]), "a later prompt");
        let literal_markup = vec![json!({"role": "user", "content": [
            {"type":"text","text":"<system-reminder>literal user input"}
        ]})];
        assert_eq!(
            first_user_text(&literal_markup),
            "<system-reminder>literal user input"
        );
        let tool_first = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "x"}
            ]}),
            json!({"role": "user", "content": "real"}),
        ];
        assert_eq!(first_user_text(&tool_first), "real");
        assert_eq!(first_user_text(&[]), "");
    }

    #[test]
    fn cch_matches_independent_xxhash_reference() {
        // Bun.hash.xxHash64 对原生规则筛选后的字节计算，seed=0x4d659218e32a3268。
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cch=00000;"}],"model":"a","max_tokens":1,"messages":[{"role":"user","content":"hello"}],"stream":true}"#;
        assert_eq!(cch_digest(body), 0xdc10b);
    }

    /// 从 `body` 中去掉 `cch_excluded_spans` 排除的字节后的哈希输入。
    fn cch_input(body: &str) -> String {
        let mut kept = Vec::new();
        let mut cursor = 0;
        for span in cch_excluded_spans(body.as_bytes()) {
            kept.extend_from_slice(&body.as_bytes()[cursor..span.start]);
            cursor = span.end;
        }
        kept.extend_from_slice(&body.as_bytes()[cursor..]);
        String::from_utf8(kept).unwrap()
    }

    #[test]
    fn cch_input_excludes_model_name_max_tokens_and_fallback_fields() {
        let body = r#"{"model":"claude-x","messages":[{"content":"x\"model\":\"y \"max_tokens\":9,"}],"max_tokens":64,"fallbacks":[{"model":"m]\"["}],"stream":true}"#;
        assert_eq!(
            cch_input(body),
            r#"{"model":"","messages":[{"content":"x\"model\":\"y \"max_tokens\":9,"}],"stream":true}"#
        );
        // 排除的字段是最后一个时，改为带走其前面的逗号。
        assert_eq!(
            cch_input(r#"{"a":1,"fallback_credit_token":"token"}"#),
            r#"{"a":1}"#
        );
        // 原生字节扫描器对字符串取第一个引号，不按 JSON 反转义。
        assert_eq!(
            cch_input(r#"{"a":1,"fallback_credit_token":"tok\"en"}"#),
            r#"{"a":1en"}"#
        );
        assert_eq!(cch_input(r#"{"a":1,"max_tokens":5}"#), r#"{"a":1}"#);
        assert_eq!(cch_input(r#"{"max_tokens":5}"#), "{}");
        // 没有被排除内容的请求原样参与哈希；不是数字的 max_tokens 不匹配。
        let plain = r#"{"messages":[],"max_tokens":"x"}"#;
        assert_eq!(cch_input(plain), plain);
        // 空的 model 值没有可排除的字节。
        assert_eq!(cch_input(r#"{"model":"","a":1}"#), r#"{"model":"","a":1}"#);
    }

    #[test]
    fn cch_is_independent_of_excluded_fields_only() {
        let digest = |body: &str| cch_digest(body.as_bytes());
        let base = r#"{"model":"a","messages":["hello"],"max_tokens":1,"stream":true}"#;
        assert_eq!(
            digest(base),
            digest(&base.replace("\"a\"", "\"other-model\""))
        );
        assert_eq!(digest(base), digest(&base.replace(":1,", ":128000,")));
        assert_ne!(digest(base), digest(&base.replace("hello", "hullo")));
    }

    #[test]
    fn custom_tool_names_are_isolated_without_changing_tool_data_or_ids() {
        let schema = json!({"type":"object","properties":{"name":{"type":"string"}}});
        let input = json!({"type":"tool_use","name":"Read","input":{"name":"_Read"}});
        let (body, shaped) = created(json!({
            "model": "m",
            "max_tokens": 1,
            "tools": [
                {"name": "Read", "input_schema": schema},
                {"name": "_Read", "input_schema": {}},
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "Computer", "input_schema": {}}
            ],
            "tool_choice": {"type": "tool", "name": "Read"},
            "context_management": {"edits": [
                {"type": "clear_tool_uses_20250919", "exclude_tools": ["Read", "_Read", "web_search"]}
            ]},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": input},
                    {"type": "tool_use", "id": "t2", "name": "_Read", "input": {}},
                    {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                    {"type": "tool_result", "tool_use_id": "t2", "content": [
                        {"type": "tool_reference", "tool_name": "Read"},
                        {"type": "tool_reference", "tool_name": "_Read"},
                        {"type": "text", "text": "{\"name\":\"Read\"}"}
                    ], "is_error": true}
                ]}
            ],
        }));
        assert_eq!(body["tools"][0]["name"], "_Read");
        assert_eq!(body["tools"][1]["name"], "__Read");
        assert_eq!(body["tools"][2]["name"], "web_search");
        assert_eq!(body["tools"][3]["name"], "Computer");
        assert_eq!(body["tools"][0]["input_schema"], schema);
        assert_eq!(body["tool_choice"]["name"], "_Read");
        assert_eq!(
            body["context_management"]["edits"][0]["exclude_tools"],
            json!(["_Read", "__Read", "web_search"])
        );
        let calls = &body["messages"][1]["content"];
        assert_eq!(calls[0]["name"], "_Read");
        assert_eq!(calls[0]["id"], "t1");
        assert_eq!(calls[0]["input"], input);
        assert_eq!(calls[1]["name"], "__Read");
        assert_eq!(calls[2]["name"], "web_search");
        let results = &body["messages"][2]["content"];
        assert!(
            results[0].get("is_error").is_none(),
            "success results stay implicit"
        );
        assert_eq!(results[1]["is_error"], json!(true));
        assert_eq!(results[1]["tool_use_id"], "t2");
        assert_eq!(results[1]["content"][0]["tool_name"], "_Read");
        assert_eq!(results[1]["content"][1]["tool_name"], "__Read");
        assert_eq!(results[1]["content"][2]["text"], "{\"name\":\"Read\"}");
        // 真实 tool_result 键序：tool_use_id、type、content。
        let wire = String::from_utf8(shaped.body).unwrap();
        assert!(
            wire.contains("{\"tool_use_id\":\"t1\",\"type\":\"tool_result\",\"content\":\"ok\"}"),
            "{wire}"
        );
    }

    #[test]
    fn eager_input_streaming_marks_only_non_deferred_custom_tools() {
        let prepared = prep(json!({
            "model": "m", "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "plain", "description": "d", "input_schema": {}, "eager_input_streaming": false},
                {"name": "lazy", "input_schema": {}, "defer_loading": true, "eager_input_streaming": true},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
        }));
        let tools = prepared["tools"].as_array().unwrap();
        assert_eq!(tools[0]["eager_input_streaming"], json!(true));
        assert!(tools[1].get("eager_input_streaming").is_none());
        assert_eq!(tools[1]["defer_loading"], json!(true));
        assert!(tools[2].get("eager_input_streaming").is_none());
        // 线上键序：name、description、input_schema 之后才是 eager 标记。
        let (_, shaped) = wire(&prepared, &context());
        let text = String::from_utf8(shaped.body).unwrap();
        let tool = &text[text.find("\"tools\":").unwrap()..];
        assert!(
            tool.find("\"input_schema\"").unwrap()
                < tool.find("\"eager_input_streaming\"").unwrap()
        );
    }

    #[test]
    fn betas_follow_first_party_model_and_request_controls() {
        let request =
            |value: Value| beta_header(value.as_object().unwrap(), &ModelCapabilities::default());
        let normal = request(json!({
            "model": "claude-sonnet-4-5",
            "thinking": {"type": "enabled", "budget_tokens": 31999, "display": "updates"},
            "tools": [{"name": "ToolSearch"}]
        }));
        assert_eq!(
            normal,
            concat!(
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,",
                "context-management-2025-06-27,prompt-caching-scope-2026-01-05,",
                "tool-provenance-2026-10-26,thinking-token-count-2026-05-13,",
                "advanced-tool-use-2025-11-20,thinking-binding-controls-2026-08-01,",
                "thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,",
                "cache-diagnosis-2026-04-07"
            )
        );
        let bare = request(json!({"model": "claude-haiku-4-5"}));
        let betas: Vec<_> = bare.split(',').collect();
        for absent in [
            TOOL_SEARCH_BETA,
            EFFORT_BETA,
            "thinking-binding-controls-2026-08-01",
            "thinking-display-updates-2026-08-18",
            "per-turn-control-2026-07-01",
            "mid-conversation-tool-changes-2026-07-01",
            "mid-conversation-system-clear-at-2026-08-21",
        ] {
            assert!(!betas.contains(&absent), "{absent}");
        }
        assert!(!betas.contains(&"message-threads-2026-08-12"));
        assert!(betas.contains(&"claude-code-20250219"));
        assert!(betas.contains(&"thinking-token-count-2026-05-13"));
        let old = request(json!({"model": "claude-3-opus-20240229"}));
        for absent in [
            "interleaved-thinking-2025-05-14",
            "context-management-2025-06-27",
            "thinking-token-count-2026-05-13",
            "message-threads-2026-08-12",
        ] {
            assert!(!old.split(',').any(|beta| beta == absent), "{absent}");
        }
        let controls = request(json!({
            "model": "claude-sonnet-4-5",
            "tools": [{"name": "Read", "defer_loading": true}],
            "output_config": {"effort": "low", "format": {"type": "json_schema", "schema": {}}}
        }));
        for present in [
            TOOL_SEARCH_BETA,
            EFFORT_BETA,
            "structured-outputs-2025-12-15",
        ] {
            assert!(controls.split(',').any(|beta| beta == present), "{present}");
        }
    }

    #[test]
    fn discovered_capabilities_override_default_betas() {
        let caps = ModelCapabilities::from_wire(&json!({
            "thinking": {"supported": false},
            "context_management": {"supported": false}
        }));
        let body = json!({"model": "claude-sonnet-4-5"});
        let beta = beta_header(body.as_object().unwrap(), &caps);
        assert!(!beta.contains("thinking-token-count-2026-05-13"));
        assert!(!beta.contains("context-management-2025-06-27"));
        assert!(beta.contains("interleaved-thinking-2025-05-14"));
    }

    #[test]
    fn inference_headers_match_interactive_session_headers() {
        let context = prior_context();
        let headers = inference_headers(
            "tok",
            DEFAULT_CLIENT_VERSION,
            &context,
            "b1,b2".into(),
            "[]".into(),
        );
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(headers[0].0, "Accept");
        assert_eq!(get("accept-encoding"), Some("gzip, deflate, br, zstd"));
        assert_eq!(get("authorization"), Some("Bearer tok"));
        assert_eq!(get("x-claude-code-session-id"), Some(SESSION));
        assert_eq!(get("x-claude-code-prompt-id"), Some(PROMPT));
        assert_eq!(get("x-claude-code-request-class"), Some("main"));
        assert_eq!(get("x-claude-code-sandy-plover"), Some("[]"));
        assert!(get("x-client-request-id").is_some_and(is_uuid));
        assert_eq!(get("anthropic-beta"), Some("b1,b2"));
        assert_eq!(get("x-stainless-package-version"), Some(SDK_VERSION));
        assert_eq!(
            get("user-agent").map(str::to_owned),
            Some(cli_user_agent(DEFAULT_CLIENT_VERSION))
        );
        // 工具耗时、路由标记等无法如实提供的头不发送。
        for name in ["x-claude-code-prev-tool-durations", "anthropic-dispatch-id"] {
            assert!(get(name).is_none(), "{name}");
        }
        let names: Vec<_> = headers.iter().map(|(key, _)| key.as_str()).collect();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn identity_system_variants_never_duplicate_in_the_wire_system() {
        let (body, _) = created(json!({
            "model": "m", "max_tokens": 1,
            "system": [
                {"type": "text", "text": SDK_IDENTITY_TEXT},
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
        assert_eq!(&texts[1..], [IDENTITY_TEXT, "prompt"]);
        let (bare, _) = created(json!({
            "model": "m", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(bare["system"].as_array().unwrap().len(), 2);
        assert_eq!(
            bare["system"][1]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"}),
            "the identity block is the only system anchor without client system"
        );
    }

    #[test]
    fn compaction_history_cannot_be_sliced_or_threaded() {
        let prepared = prep(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "compaction", "content": "Summary", "signature": "sig-1"}
                ]},
                {"role": "user", "content": "What was the codeword?"}
            ]
        }));
        // 即使持久化计划认为可以续接，压缩块回放也必须全量、无线程控制。
        let (body, shaped) = wire(&prepared, &prior_context());
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["content"][0]["signature"], "sig-1");
        assert!(body.get("thread").is_none() && body.get("diagnostics").is_none());
        assert!(!betas_of(&shaped).contains(&"message-threads-2026-08-12"));
        assert!(betas_of(&shaped).contains(&"compact-2026-09-04"));
    }

    #[test]
    fn replayed_compaction_block_declares_its_beta_and_stays_byte_faithful() {
        let block = json!({"type": "compaction", "content": "Summary", "signature": "sig-1"});
        let (body, shaped) = created(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [
                {"role": "assistant", "content": [block.clone()]},
                {"role": "user", "content": "What was the codeword?"}
            ]
        }));
        assert!(
            betas_of(&shaped).contains(&"compact-2026-09-04"),
            "{}",
            shaped.betas
        );
        // 压缩块须逐字节回放：缓存断点或任何附加字段都会使上游拒绝签名。
        assert_eq!(body["messages"][0]["content"][0], block);
        assert!(
            body.get("thread").is_none() && body.get("diagnostics").is_none(),
            "signed compaction replay cannot be combined with a message thread"
        );
        assert!(!betas_of(&shaped).contains(&"message-threads-2026-08-12"));
    }

    #[test]
    fn inference_never_gains_the_compaction_beta_or_field() {
        let (body, shaped) = created(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
            "compaction": {"type": "summarize"}
        }));
        assert!(body.get("compaction").is_none());
        assert_eq!(body["stream"], json!(true));
        assert!(!betas_of(&shaped).contains(&"compact-2026-09-04"));
    }

    #[test]
    fn compaction_without_a_control_is_rejected_instead_of_sent_as_inference() {
        assert!(
            prepare(
                json!({"model": "m", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]}),
                &ModelCapabilities::default(),
                false,
                true,
            )
            .is_err()
        );
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
            let body = prep(json!({
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
            assert_eq!(messages[1]["content"], "completed earlier answer");
            assert_eq!(messages[2]["content"], "next turn");
        }
    }

    #[test]
    fn legacy_and_unknown_models_keep_native_prefill_unless_thinking_is_enabled() {
        for model in ["claude-sonnet-4-5", "claude-opus-4-5", "custom-model"] {
            let body = prep(json!({
                "model": model,
                "max_tokens": 4096,
                "messages": [
                    {"role": "user", "content": "first turn"},
                    {"role": "assistant", "content": "supported prefix"}
                ]
            }));
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 2, "{model}");
            assert_eq!(messages[1]["content"], "supported prefix");
        }
        let body = prep(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 8192,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "assistant", "content": "unsupported prefix"}
            ]
        }));
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["content"], "first turn");
    }

    #[test]
    fn explicit_output_limit_is_raised_only_for_the_thinking_budget() {
        let request = |max_tokens: u64, budget: u64, auto: bool| {
            prep_with(
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": max_tokens,
                    "thinking": {"type": "enabled", "budget_tokens": budget},
                    "messages": [{"role": "user", "content": "hi"}],
                }),
                &ModelCapabilities::default(),
                auto,
            )
        };
        // 显式上限：沿用既有规则，不足预算加正文空间时抬高。
        let body = request(2048, 16000, false);
        assert_eq!(body["thinking"]["budget_tokens"], 16000);
        assert_eq!(body["max_tokens"], 16000 + 4000);
        // 显式上限已足够时原样保留。
        assert_eq!(request(64000, 16000, false)["max_tokens"], 64000);
        // 自动补出的上限（发现数据或编码器必填回退）不能被预算抬高；预算让位。
        let body = request(32000, 63000, true);
        assert_eq!(body["max_tokens"], 32000);
        assert_eq!(body["thinking"]["budget_tokens"], 32000 - 4000);
    }

    #[test]
    fn adaptive_thinking_defaults_to_updates_unless_the_client_chose_a_display() {
        let thinking = |model: &str, thinking: Option<Value>| {
            let mut body = json!({
                "model": model, "max_tokens": 4096,
                "messages": [{"role": "user", "content": "hi"}],
            });
            if let Some(thinking) = thinking {
                body["thinking"] = thinking;
            }
            prep(body)["thinking"].clone()
        };
        let adaptive = json!({"type": "adaptive"});
        assert_eq!(
            thinking("claude-opus-5-5", Some(adaptive.clone())),
            json!({"type": "adaptive", "display": "updates"})
        );
        // 常开型号未要求思考时，补出的自适应思考同样默认 updates。
        assert_eq!(
            thinking("claude-opus-5-5", None),
            json!({"type": "adaptive", "display": "updates"})
        );
        for explicit in ["summarized", "omitted", "updates"] {
            assert_eq!(
                thinking(
                    "claude-opus-5-5",
                    Some(json!({"type": "adaptive", "display": explicit}))
                )["display"],
                explicit
            );
        }
        // 受 Open Responses 摘要意图映射而来的显式 summarized 不被改写。
        assert_eq!(
            thinking(
                "claude-opus-5-5",
                Some(json!({"type": "adaptive", "display": "auto"}))
            )["display"],
            "summarized"
        );
        // 手动预算与未识别型号没有原生 updates 证据，不加字段。
        assert!(
            thinking(
                "claude-sonnet-4-5",
                Some(json!({"type": "enabled", "budget_tokens": 2048}))
            )
            .get("display")
            .is_none()
        );
        assert_eq!(
            thinking("some-proxy-model", Some(adaptive)),
            json!({"type": "adaptive"})
        );
        assert!(
            thinking("claude-opus-5-5", Some(json!({"type": "disabled"})))
                .get("display")
                .is_none()
        );
    }

    #[test]
    fn sampling_params_are_dropped_when_thinking_is_on() {
        // 回归：adaptive 思考 + `temperature: 0` 被上游 400 拒绝。
        for thinking in [
            json!({"type": "adaptive"}),
            json!({"type": "enabled", "budget_tokens": 1024}),
        ] {
            let body = prep(json!({
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
            prep(body)
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
    fn open_responses_title_request_drops_temperature_on_opus_5_5() {
        // 回归抓包：omp 的标题请求（temperature 0、effort low）经 open-responses 入口
        // 转成 Anthropic 后，上游因 adaptive 思考加 temperature 返回 400。
        let body = prep(json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "temperature": 0,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "low"},
            "messages": [{"role": "user", "content": "测试"}],
        }));
        assert!(body.get("temperature").is_none());
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"], json!({"effort": "low"}));
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
        let (body, _) = created(json!({
            "model":"claude-sonnet-4-5",
            "max_tokens":1024,
            "tools":[{"name":"nullable_tool", "input_schema":schema}],
            "messages":[{"role":"user", "content":"hi"}]
        }));
        assert_eq!(body["tools"][0]["input_schema"], schema);
        assert!(body["tools"][0].get("strict").is_none());
    }

    #[test]
    fn fields_claude_code_never_sends_are_dropped() {
        let (body, _) = created(json!({
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
            assert!(
                CLAUDE_CODE_FIELDS.contains(key)
                    || ["metadata", "thread", "diagnostics"].contains(key),
                "unexpected field {key}"
            );
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
                assert_eq!(body["tools"][0]["name"], "available");
            } else {
                assert_eq!(body["tools"], json!([]));
            }
        }
    }

    #[test]
    fn official_nested_controls_survive_but_unknown_parameter_keys_do_not() {
        let schema = json!({"type":"object","properties":{"private_parameter":{"type":"string"}}});
        let body = prep(json!({
            "model":"claude-opus-4-6", "messages":[{"role":"user","content":"hi"}],
            "tools":[
                {"name":"business","input_schema":schema,"defer_loading":false,
                 "citations":{"enabled":true,"private_parameter":1},
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
        }));
        assert!(body.get("private_parameter").is_none());
        for owned in ["metadata", "thread", "diagnostics"] {
            assert!(body.get(owned).is_none(), "{owned}");
        }
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["tools"][0]["input_schema"], schema);
        assert_eq!(body["tools"][0]["defer_loading"], false);
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        for field in [
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
        let betas = beta_header(body.as_object().unwrap(), &ModelCapabilities::default());
        for beta in [
            "task-budgets-2026-03-13",
            "dangerous-tool-use-2026-09-03",
            "fast-mode-2026-02-01",
            "prompt-caching-evict-2026-05-12",
            "context-hint-2026-04-09",
        ] {
            assert!(betas.split(',').any(|actual| actual == beta), "{beta}");
        }
        // 端到端：账号元数据整体重建，客户端提供的身份与私有字段不会成为权威。
        let (wire_body, _) = wire(&body, &context());
        let user_id: Value =
            serde_json::from_str(wire_body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(user_id["session_id"], SESSION);
        assert_eq!(wire_body["metadata"].as_object().unwrap().len(), 1);
    }
}
