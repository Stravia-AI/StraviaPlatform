//! 把 codec 编码出的 Anthropic Messages 请求改写为 Claude Code CLI 的线上形态。
//!
//! Anthropic 对订阅 OAuth 令牌只放行「看起来像 Claude Code」的推理请求。本模块
//! 复刻 oh-my-pi v18.4.2 的 OAuth 塑形（`packages/ai/src/providers/anthropic.ts`
//! 与 `anthropic-identity.ts`）：`system[0]` 计费头 + `system[1]` 身份块、自定义
//! 工具名加 `_` 前缀、`metadata.user_id` JSON 身份、缓存 TTL 默认 1 小时，以及
//! 对最终请求体字节计算的 `cch` 校验值。这些值是与上游的线上约定，改动前应先用
//! 真实 Claude Code 或 oh-my-pi 抓包确认。

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::xxhash::xxh64;

/// 未配置覆盖值时上报的 Claude Code CLI 版本（与 oh-my-pi v18.4.2 的
/// `DEFAULT_CLAUDE_CODE_VERSION` 一致）。
pub(crate) const DEFAULT_CLIENT_VERSION: &str = "2.1.280";
/// Claude Code 当前版本捆绑的 `@anthropic-ai/sdk` 版本。
pub(crate) const SDK_VERSION: &str = "0.112.1";
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
const MAX_CACHE_BREAKPOINTS: usize = 4;
const LONG_CACHE_TTL: &str = "1h";

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

/// 已登录账号在请求元数据里的身份。
pub(crate) struct AccountIdentity<'a> {
    pub account_uuid: Option<&'a str>,
    pub device_id: Option<&'a str>,
}

pub(crate) struct ShapedRequest {
    pub body: Vec<u8>,
    pub session_id: String,
    pub betas: String,
}

/// 塑形 codec 产出的请求体并序列化为最终线上字节（已写入 `cch`）。
/// `fallback_session_id` 仅在客户端元数据没有可复用会话 ID 时调用。
pub(crate) fn shape(
    mut body: Value,
    client_version: &str,
    identity: &AccountIdentity<'_>,
    fallback_session_id: impl FnOnce() -> String,
) -> Result<ShapedRequest, String> {
    let object = body
        .as_object_mut()
        .ok_or("Anthropic request body must be a JSON object")?;
    // Claude Code 只走流式推理；流式同时避开非流式请求的长输出时限。
    object.insert("stream".into(), Value::Bool(true));
    prefix_tool_names(object);
    default_tool_result_errors(object);
    upgrade_cache_ttl(object);
    let session_id = client_session_id(object.get("metadata")).unwrap_or_else(fallback_session_id);
    let first_user_text = first_user_text(object.get("messages"));
    rebuild_system(object, &first_user_text, client_version)?;
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

/// Claude Code 推理请求头，按真实 CLI 抓包的字节序排列。宿主在插件协商
/// `Accept-Encoding` 时负责解码响应。
pub(crate) fn inference_headers(
    access_token: &str,
    client_version: &str,
    session_id: &str,
    betas: String,
) -> Vec<(String, String)> {
    vec![
        ("accept".into(), "application/json".into()),
        ("accept-encoding".into(), "gzip, deflate, br, zstd".into()),
        ("authorization".into(), format!("Bearer {access_token}")),
        ("connection".into(), "keep-alive".into()),
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), cli_user_agent(client_version)),
        ("x-claude-code-session-id".into(), session_id.into()),
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
    format!("claude-cli/{client_version} (external, cli)")
}

/// 响应侧还原：去掉请求时加上的一个 `_` 前缀。
pub(crate) fn decode_tool_name(name: &str) -> Option<String> {
    name.strip_prefix(TOOL_PREFIX).map(str::to_owned)
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

fn cache_control_slots(object: &mut Map<String, Value>) -> Vec<&mut Map<String, Value>> {
    let mut slots = Vec::new();
    for (key, value) in object.iter_mut() {
        match key.as_str() {
            "system" | "tools" => {
                if let Value::Array(entries) = value {
                    slots.extend(entries.iter_mut().filter_map(Value::as_object_mut));
                }
            }
            "messages" => {
                let Value::Array(messages) = value else {
                    continue;
                };
                for block in messages
                    .iter_mut()
                    .filter_map(|message| message.get_mut("content"))
                    .filter_map(Value::as_array_mut)
                    .flatten()
                    .filter_map(Value::as_object_mut)
                {
                    slots.push(block);
                }
            }
            _ => {}
        }
    }
    slots
}

/// Claude Code 对订阅用户默认使用 1 小时提示缓存；统一升级也避免 1h 与 5m
/// 断点混排时违反「长 TTL 必须在前」的上游约束。
fn upgrade_cache_ttl(object: &mut Map<String, Value>) {
    for slot in cache_control_slots(object) {
        let Some(Value::Object(cache_control)) = slot.get_mut("cache_control") else {
            continue;
        };
        if cache_control.get("type").and_then(Value::as_str) == Some("ephemeral")
            && !cache_control.contains_key("ttl")
        {
            cache_control.insert("ttl".into(), Value::String(LONG_CACHE_TTL.into()));
        }
    }
}

fn cache_breakpoints(object: &mut Map<String, Value>) -> usize {
    cache_control_slots(object)
        .into_iter()
        .filter(|slot| {
            slot.get("cache_control")
                .is_some_and(|value| !value.is_null())
        })
        .count()
}

/// 复用客户端元数据里的会话 ID：Claude Code JSON 形态取 `session_id`，旧版
/// `user_…_account_…_session_<uuid>` 形态取末段。
fn client_session_id(metadata: Option<&Value>) -> Option<String> {
    let user_id = metadata?.get("user_id")?.as_str()?.trim();
    if user_id.starts_with('{') {
        let parsed: Value = serde_json::from_str(user_id).ok()?;
        return parsed
            .get("session_id")?
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }
    let (_, session) = user_id.rsplit_once("_session_")?;
    (user_id.starts_with("user_") && session.len() == 36).then(|| session.to_owned())
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
        let mut identity = json!({ "type": "text", "text": IDENTITY_TEXT });
        // 身份块只在它是 system 尾块时承担缓存断点；客户端 system 已存在时，
        // 断点留给客户端自己的块，避免多占一个 4 断点上限内的名额。
        if client_blocks.is_empty() && cache_breakpoints(object) < MAX_CACHE_BREAKPOINTS {
            identity["cache_control"] = json!({ "type": "ephemeral", "ttl": LONG_CACHE_TTL });
        }
        system.push(identity);
    }
    system.extend(client_blocks);
    object.insert("system".into(), Value::Array(system));
    Ok(())
}

#[derive(Serialize)]
struct MetadataUserId<'a> {
    session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_uuid: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_id: Option<&'a str>,
}

/// `metadata.user_id` 由上游账号身份重建，只沿用客户端的会话 ID：客户端
/// 自带的账号/设备字段描述的是客户端自己的凭据，不是本连接的订阅账号。
fn metadata_user_id(session_id: &str, identity: &AccountIdentity<'_>) -> String {
    serde_json::to_string(&MetadataUserId {
        session_id,
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
        }
    }

    fn shaped(body: Value) -> (Value, ShapedRequest) {
        let shaped = shape(body, DEFAULT_CLIENT_VERSION, &identity(), || {
            "fallback-session".into()
        })
        .expect("request shapes");
        (
            serde_json::from_slice(&shaped.body).expect("shaped body is JSON"),
            shaped,
        )
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
    fn system_carries_billing_then_identity_and_keeps_client_breakpoints() {
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
        assert!(
            system[1].get("cache_control").is_none(),
            "client already anchors the system head"
        );
        assert_eq!(
            system[2]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn identity_anchors_cache_only_without_client_system_and_with_free_breakpoint() {
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            body["system"][1]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );

        let marked = json!({"type": "ephemeral"});
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "tools": [{"name": "a", "input_schema": {}, "cache_control": marked}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "1", "cache_control": marked},
                {"type": "text", "text": "2", "cache_control": marked},
                {"type": "text", "text": "3", "cache_control": marked}
            ]}],
        }));
        assert!(
            body["system"][1].get("cache_control").is_none(),
            "four breakpoints already used"
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
        assert!(texts[0].contains("cc_version=2.1.280."));
        assert_eq!(&texts[1..], [IDENTITY_TEXT, "prompt"]);
    }

    #[test]
    fn custom_tools_and_history_get_prefix_but_builtin_and_typed_tools_do_not() {
        let (body, _) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "tools": [
                {"name": "read", "input_schema": {}},
                {"name": "Web_Search", "input_schema": {}},
                {"type": "bash_20250124", "name": "bash"},
                {"type": "custom", "name": "grep", "input_schema": {}}
            ],
            "tool_choice": {"type": "tool", "name": "read"},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "bash", "input": {}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}
            ],
        }));
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["_read", "Web_Search", "bash", "_grep"]);
        assert_eq!(body["tool_choice"]["name"], "_read");
        assert_eq!(body["messages"][1]["content"][0]["name"], "_read");
        assert_eq!(body["messages"][1]["content"][1]["name"], "bash");
        assert_eq!(decode_tool_name("_read").as_deref(), Some("read"));
        assert_eq!(decode_tool_name("__private").as_deref(), Some("_private"));
        assert_eq!(decode_tool_name("bash"), None);
    }

    #[test]
    fn tool_results_without_is_error_are_sent_as_explicit_false() {
        let (_, shaped) = shaped(json!({
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
        let wire = String::from_utf8(shaped.body).unwrap();
        assert!(wire.contains(
            r#"{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}"#
        ));
        assert!(wire.contains(
            r#"{"type":"tool_result","tool_use_id":"t2","content":"boom","is_error":true}"#
        ));
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
    fn metadata_reuses_client_session_and_injects_account_identity() {
        let (body, shaped) = shaped(json!({
            "model": "m",
            "max_tokens": 1,
            "metadata": {"user_id": "{\"session_id\":\"s-1\",\"account_uuid\":\"client\"}"},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(shaped.session_id, "s-1");
        let user_id: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(user_id["session_id"], "s-1");
        assert_eq!(
            user_id["account_uuid"],
            "00000000-0000-4000-8000-0000000000aa"
        );
        assert_eq!(user_id["device_id"].as_str().unwrap().len(), 64);

        let (_, shaped) =
            shaped_with_metadata("user_abc_account_x_session_12345678-1234-1234-1234-123456789012");
        assert_eq!(shaped.session_id, "12345678-1234-1234-1234-123456789012");
        let (_, shaped) = shaped_with_metadata("opaque-client-user");
        assert_eq!(shaped.session_id, "fallback-session");
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
