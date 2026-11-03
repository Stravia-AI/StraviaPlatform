//! Claude Code 线程续接的短期会话状态。
//!
//! 官方 CLI 在同一会话内用 `thread = continue(previous_message_id)` 只上送新增消息。
//! 本模块只决定“本次请求能否续接”以及线上用的 ID，不触碰宿主：调用方负责读写
//! 私有状态字节，并把响应落盘前重新读取最新根对象后再 `commit`。
//!
//! 持久化内容仅限摘要/计数/时间/上游 ID，绝不保存消息、提示词、签名或凭据。续接依据是
//! 逐消息链式 SHA-256：请求前缀必须与上一次成功完成的转录完全一致。状态丢失或并发
//! 覆盖只会退化为完整 Create，不会授权错误的续接。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// 在私有状态根对象中占用的键；根对象的其他键（OAuth 状态等）原样保留。
const STATE_KEY: &str = "claude_threads";
const STATE_VERSION: u32 = 1;
/// 提示词谱系与“已禁用线程”标记的保留期。
const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_ENTRIES: usize = 64;
const MAX_ID_LEN: usize = 128;
/// 不参与固定参数摘要的顶层字段。
const VARIABLE_FIELDS: [&str; 7] = [
    "messages",
    "metadata",
    "max_tokens",
    "stream",
    "stop_sequences",
    "thread",
    "diagnostics",
];

type Hash = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadMode {
    Create,
    Continue,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WireContext {
    pub session_id: String,
    pub prompt_id: String,
    pub turn_index: usize,
    pub previous_message_id: Option<String>,
    pub previous_request_id: Option<String>,
    pub message_offset: usize,
    pub mode: ThreadMode,
}

/// 一次请求的规划结果；`commit` 在成功完成后据此落盘。
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub context: WireContext,
    scope: String,
    fixed: Hash,
    input_chain: Hash,
    input_count: usize,
    turn_chain: Hash,
    turn_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Thread {
    fixed: String,
    chain: String,
    count: usize,
    message_id: String,
    at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    session_id: String,
    touched_ms: i64,
    #[serde(default)]
    disabled: bool,
    prompt_id: String,
    turn_chain: String,
    turn_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread: Option<Thread>,
}

#[derive(Deserialize)]
struct Section {
    v: u32,
    sessions: BTreeMap<String, Entry>,
}

#[derive(Serialize)]
struct SectionRef<'a> {
    v: u32,
    sessions: &'a BTreeMap<String, Entry>,
}

#[derive(Serialize)]
struct StateRef<'a> {
    #[serde(flatten)]
    root: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_threads: Option<SectionRef<'a>>,
}

#[derive(Debug)]
pub(crate) struct SessionStore {
    /// 根对象中不属于本模块的键。
    root: Map<String, Value>,
    sessions: BTreeMap<String, Entry>,
}

impl SessionStore {
    /// `None`/空白视为空状态；根不是 JSON 对象时显式报错（调用方不得据此覆写）。
    /// 本模块自己的键损坏或版本不符只丢弃缓存（最坏后果是完整 Create），其余键保留。
    pub(crate) fn from_bytes(bytes: Option<&[u8]>) -> Result<Self, String> {
        let Some(bytes) = bytes.filter(|b| !b.iter().all(u8::is_ascii_whitespace)) else {
            return Ok(Self {
                root: Map::new(),
                sessions: BTreeMap::new(),
            });
        };
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|e| format!("Claude session state is not valid JSON: {e}"))?;
        let Value::Object(mut root) = value else {
            return Err("Claude session state root must be a JSON object".into());
        };
        let sessions = root
            .remove(STATE_KEY)
            .and_then(|v| serde_json::from_value::<Section>(v).ok())
            .filter(|s| s.v == STATE_VERSION)
            .map(|s| s.sessions)
            .unwrap_or_default();
        let mut store = Self { root, sessions };
        store.sanitize();
        Ok(store)
    }

    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let state = StateRef {
            root: &self.root,
            claude_threads: (!self.sessions.is_empty()).then_some(SectionRef {
                v: STATE_VERSION,
                sessions: &self.sessions,
            }),
        };
        serde_json::to_vec(&state).map_err(|e| format!("Claude session state encode failed: {e}"))
    }

    /// `body` 是 prepare 之后的完整请求体；一趟遍历同时完成前缀校验与链摘要。
    pub(crate) fn plan(
        &self,
        scope: &str,
        session_id: String,
        body: &Value,
        now_ms: i64,
    ) -> Result<Plan, String> {
        if !valid_id(scope) || !valid_id(&session_id) {
            return Err("Claude session scope/session id is invalid".into());
        }
        let object = body
            .as_object()
            .ok_or("Claude request body must be an object")?;
        let messages = object
            .get("messages")
            .and_then(Value::as_array)
            .filter(|m| !m.is_empty())
            .ok_or("Claude request messages must be a non-empty array")?;
        let fixed = fixed_digest(object);
        let fixed_hex = hex(&fixed);

        let entry = self.sessions.get(scope).filter(|e| {
            e.session_id == session_id && now_ms.saturating_sub(e.touched_ms) <= RETENTION_MS
        });
        let thread = entry
            .filter(|e| !e.disabled)
            .and_then(|e| e.thread.as_ref())
            .filter(|t| {
                t.count > 0
                    && t.count < messages.len()
                    // 官方 2.1.317 未获得远端 lifetime 参数时，以服务端 404
                    // 判定过期；不能把文档的五分钟硬编码成另一种客户端行为。
                    && now_ms >= t.at_ms
                    && t.fixed == fixed_hex
            });
        let watch = thread.map(|t| t.count);

        let mut chain = genesis();
        let mut prefix_hex = None;
        let mut turn_chain = chain;
        let mut turn_count = 0;
        let mut user_turns = 0;
        for (index, message) in messages.iter().enumerate() {
            chain = chain_step(&chain, message);
            if is_conversational_user(message) {
                user_turns += 1;
                turn_chain = chain;
                turn_count = index + 1;
            }
            if watch == Some(index + 1) {
                prefix_hex = Some(hex(&chain));
            }
        }

        let continued = thread.filter(|t| {
            prefix_hex.as_deref() == Some(t.chain.as_str())
                && messages
                    .get(t.count)
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    .is_some_and(|role| role == "user" || role == "system")
        });
        let (mode, message_offset, previous_message_id) = if entry.is_some_and(|e| e.disabled) {
            (ThreadMode::Disabled, 0, None)
        } else if let Some(t) = continued {
            (ThreadMode::Continue, t.count, Some(t.message_id.clone()))
        } else {
            (ThreadMode::Create, 0, None)
        };
        let turn_hex = hex(&turn_chain);
        let prompt_id = entry
            .filter(|e| turn_count > 0 && e.turn_count == turn_count && e.turn_chain == turn_hex)
            .map(|e| e.prompt_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        Ok(Plan {
            context: WireContext {
                previous_request_id: entry.and_then(|e| e.last_request_id.clone()),
                session_id,
                prompt_id,
                turn_index: user_turns.max(1),
                previous_message_id,
                message_offset,
                mode,
            },
            scope: scope.to_owned(),
            fixed,
            input_chain: chain,
            input_count: messages.len(),
            turn_chain,
            turn_count,
        })
    }

    /// 仅在上游响应成功结束后调用。`plan.context.mode` 以调用方最终使用的为准：
    /// `Disabled` 对该会话粘性生效。`reusable=false`、无可用 ID 或最后一条不是
    /// assistant 时只记录谱系与请求 ID，不允许下次续接。
    pub(crate) fn commit(
        &mut self,
        plan: Plan,
        response_messages: &[Value],
        response_id: &str,
        request_id: Option<&str>,
        now_ms: i64,
        reusable: bool,
    ) -> Result<(), String> {
        let Plan {
            context,
            scope,
            fixed,
            input_chain,
            input_count,
            turn_chain,
            turn_count,
        } = plan;
        let disabled_now = context.mode == ThreadMode::Disabled;
        let old = self
            .sessions
            .remove(&scope)
            .filter(|e| e.session_id == context.session_id);
        if old.as_ref().is_some_and(|o| o.touched_ms > now_ms) {
            // 乱序到达的旧完成：不得覆盖更新的状态，只合并粘性的禁用标记。
            if let Some(mut o) = old {
                o.disabled |= disabled_now;
                self.sessions.insert(scope, o);
            }
            return Ok(());
        }

        let thread = (reusable
            && !disabled_now
            && valid_id(response_id)
            && response_messages.iter().all(Value::is_object)
            && response_messages
                .last()
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some("assistant"))
        .then(|| {
            let chain = response_messages
                .iter()
                .fold(input_chain, |c, m| chain_step(&c, m));
            Thread {
                fixed: hex(&fixed),
                chain: hex(&chain),
                count: input_count + response_messages.len(),
                message_id: response_id.to_owned(),
                at_ms: now_ms,
            }
        });
        let entry = Entry {
            session_id: context.session_id,
            touched_ms: now_ms,
            disabled: disabled_now || old.is_some_and(|o| o.disabled),
            prompt_id: context.prompt_id,
            turn_chain: hex(&turn_chain),
            turn_count,
            last_request_id: request_id.filter(|r| valid_id(r)).map(str::to_owned),
            thread,
        };
        self.sessions
            .retain(|_, e| now_ms.saturating_sub(e.touched_ms) <= RETENTION_MS);
        self.evict_to(MAX_ENTRIES - 1);
        self.sessions.insert(scope, entry);
        Ok(())
    }

    fn sanitize(&mut self) {
        self.sessions.retain(|scope, e| {
            if !valid_id(scope)
                || !valid_id(&e.session_id)
                || !uuid::Uuid::parse_str(&e.prompt_id).is_ok_and(|u| u.get_version_num() == 4)
                || !is_hex_digest(&e.turn_chain)
            {
                return false;
            }
            if e.last_request_id.as_deref().is_some_and(|r| !valid_id(r)) {
                e.last_request_id = None;
            }
            if e.thread.as_ref().is_some_and(|t| {
                t.count == 0
                    || !valid_id(&t.message_id)
                    || !is_hex_digest(&t.fixed)
                    || !is_hex_digest(&t.chain)
            }) {
                e.thread = None;
            }
            true
        });
        self.evict_to(MAX_ENTRIES);
    }

    /// 按 (touched, scope) 升序淘汰最旧项，结果确定。
    fn evict_to(&mut self, limit: usize) {
        while self.sessions.len() > limit {
            let oldest = self
                .sessions
                .iter()
                .min_by(|a, b| (a.1.touched_ms, a.0).cmp(&(b.1.touched_ms, b.0)))
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    self.sessions.remove(&key);
                }
                None => break,
            }
        }
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID_LEN && s.bytes().all(|b| b.is_ascii_graphic())
}

fn is_hex_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn hex(hash: &Hash) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in hash {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

/// 带 tool_result 以外内容（或字符串内容）的 user 消息才是新的用户轮次。
fn is_conversational_user(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && match message.get("content") {
            Some(Value::String(_)) => true,
            Some(Value::Array(blocks)) => blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) != Some("tool_result")),
            _ => false,
        }
}

fn sha_out(hasher: Sha256) -> Hash {
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest[..]);
    out
}

fn genesis() -> Hash {
    let mut h = Sha256::new();
    h.update(b"stravia.claude.thread.v1");
    sha_out(h)
}

/// `H(prev || canonical(message))`：可在响应消息上继续延伸，无需重读历史。
fn chain_step(prev: &Hash, message: &Value) -> Hash {
    let mut h = Sha256::new();
    h.update(b"stravia.claude.thread.msg.v1");
    h.update(prev);
    feed_message(&mut h, message);
    sha_out(h)
}

fn fixed_digest(body: &Map<String, Value>) -> Hash {
    let mut entries: Vec<_> = body
        .iter()
        .filter(|(k, _)| !VARIABLE_FIELDS.contains(&k.as_str()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = Sha256::new();
    h.update(b"stravia.claude.thread.fixed.v1");
    feed_len(&mut h, entries.len());
    for (key, value) in entries {
        feed_str(&mut h, key);
        feed_value(&mut h, value);
    }
    sha_out(h)
}

fn feed_len(h: &mut Sha256, n: usize) {
    h.update(u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes());
}

fn feed_str(h: &mut Sha256, s: &str) {
    h.update(b"s");
    feed_len(h, s.len());
    h.update(s.as_bytes());
}

/// 键序无关的规范化哈希（不依赖 serde_json 是否启用 preserve_order）。
fn feed_value(h: &mut Sha256, value: &Value) {
    match value {
        Value::Null => h.update(b"n"),
        Value::Bool(true) => h.update(b"t"),
        Value::Bool(false) => h.update(b"f"),
        Value::Number(n) => {
            let text = n.to_string();
            h.update(b"d");
            feed_len(h, text.len());
            h.update(text.as_bytes());
        }
        Value::String(s) => feed_str(h, s),
        Value::Array(items) => {
            h.update(b"a");
            feed_len(h, items.len());
            for item in items {
                feed_value(h, item);
            }
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            h.update(b"o");
            feed_len(h, entries.len());
            for (k, v) in entries {
                feed_str(h, k);
                feed_value(h, v);
            }
        }
    }
}

/// 消息的规范形：字符串 content 等同单个 text 块；内容块顶层的 `cache_control`
/// 是缓存锚点而非数据，不参与摘要（块内 `input` 等数据字段原样参与）。
fn feed_message(h: &mut Sha256, message: &Value) {
    let Value::Object(map) = message else {
        feed_value(h, message);
        return;
    };
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    h.update(b"o");
    feed_len(h, entries.len());
    for (k, v) in entries {
        feed_str(h, k);
        if k == "content" {
            feed_content(h, v);
        } else {
            feed_value(h, v);
        }
    }
}

fn feed_content(h: &mut Sha256, content: &Value) {
    match content {
        Value::String(text) => {
            h.update(b"a");
            feed_len(h, 1);
            h.update(b"o");
            feed_len(h, 2);
            feed_str(h, "text");
            feed_str(h, text);
            feed_str(h, "type");
            feed_str(h, "text");
        }
        Value::Array(blocks) => {
            h.update(b"a");
            feed_len(h, blocks.len());
            for block in blocks {
                feed_block(h, block);
            }
        }
        other => feed_value(h, other),
    }
}

fn feed_block(h: &mut Sha256, block: &Value) {
    let Value::Object(map) = block else {
        feed_value(h, block);
        return;
    };
    let mut entries: Vec<_> = map.iter().filter(|(k, _)| *k != "cache_control").collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    h.update(b"o");
    feed_len(h, entries.len());
    for (k, v) in entries {
        feed_str(h, k);
        if k == "content" {
            feed_content(h, v);
        } else {
            feed_value(h, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCOPE: &str = "scope-a";
    const SID: &str = "71b8ddf0-6d2b-4fe7-af43-96c39ed5b0c3";

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn assistant(text: &str) -> Value {
        json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
    }

    fn tool_call(id: &str) -> Value {
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Read", "input": {"cache_control": "data"}}
        ]})
    }

    fn tool_result(id: &str) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "ok"}
        ]})
    }

    fn body(messages: &[Value]) -> Value {
        json!({
            "model": "model-a",
            "system": [{"type": "text", "text": "sys"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
            "max_tokens": 64,
            "stream": true,
            "messages": messages,
        })
    }

    fn plan_at(store: &SessionStore, messages: &[Value], now: i64) -> Plan {
        store
            .plan(SCOPE, SID.into(), &body(messages), now)
            .expect("plan")
    }

    fn commit_ok(
        store: &mut SessionStore,
        plan: Plan,
        reply: &Value,
        id: &str,
        req: &str,
        now: i64,
    ) {
        store
            .commit(plan, std::slice::from_ref(reply), id, Some(req), now, true)
            .expect("commit");
    }

    fn empty() -> SessionStore {
        SessionStore::from_bytes(None).expect("empty store")
    }

    /// q1 -> tool call -> tool result，返回已提交（t=1000）的存储与相关消息。
    fn after_first_call() -> (SessionStore, Plan, Vec<Value>) {
        let mut store = empty();
        let first = plan_at(&store, &[user("q1")], 0);
        let reply = tool_call("toolu_1");
        commit_ok(&mut store, first.clone(), &reply, "msg_1", "req_1", 1000);
        let messages = vec![user("q1"), reply, tool_result("toolu_1")];
        (store, first, messages)
    }

    #[test]
    fn first_request_creates_with_fresh_v4_prompt() {
        let store = empty();
        let plan = plan_at(&store, &[user("q1")], 0);
        let ctx = &plan.context;
        assert_eq!(ctx.mode, ThreadMode::Create);
        assert_eq!(ctx.message_offset, 0);
        assert_eq!(ctx.session_id, SID);
        assert_eq!(ctx.turn_index, 1);
        assert!(ctx.previous_message_id.is_none() && ctx.previous_request_id.is_none());
        let id = uuid::Uuid::parse_str(&ctx.prompt_id).expect("uuid");
        assert_eq!(id.get_version_num(), 4);
    }

    #[test]
    fn tool_loop_continues_with_same_prompt_and_new_user_turn_rotates_it() {
        let (mut store, first, messages) = after_first_call();
        let second = plan_at(&store, &messages, 2000);
        assert_eq!(second.context.mode, ThreadMode::Continue);
        assert_eq!(second.context.message_offset, 2);
        assert_eq!(second.context.previous_message_id.as_deref(), Some("msg_1"));
        assert_eq!(second.context.previous_request_id.as_deref(), Some("req_1"));
        assert_eq!(second.context.prompt_id, first.context.prompt_id);
        assert_eq!(second.context.turn_index, 1);

        let done = assistant("done");
        commit_ok(&mut store, second, &done, "msg_2", "req_2", 3000);
        let mut next = messages;
        next.extend([done, user("q2")]);
        let third = plan_at(&store, &next, 4000);
        assert_eq!(third.context.mode, ThreadMode::Continue);
        assert_eq!(third.context.message_offset, 4);
        assert_eq!(third.context.previous_message_id.as_deref(), Some("msg_2"));
        assert_eq!(third.context.previous_request_id.as_deref(), Some("req_2"));
        assert_ne!(third.context.prompt_id, first.context.prompt_id);
        assert_eq!(third.context.turn_index, 2);
    }

    #[test]
    fn user_message_with_text_next_to_tool_result_is_a_new_turn() {
        let (store, first, mut messages) = after_first_call();
        messages.pop();
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
            {"type": "text", "text": "also this"}
        ]}));
        let plan = plan_at(&store, &messages, 2000);
        assert_eq!(plan.context.turn_index, 2);
        assert_ne!(plan.context.prompt_id, first.context.prompt_id);
    }

    #[test]
    fn server_expiry_does_not_reset_client_prompt_lineage() {
        let (store, first, messages) = after_first_call();
        // 真实 CLI 在空闲 649 秒后先继续，收到 thread_not_found 才全量重放。
        // 本层保留关联；一次重放由执行层的 HTTP 合同测试覆盖。
        for idle_ms in [299_999, 300_000, 649_000] {
            let plan = plan_at(&store, &messages, 1000 + idle_ms);
            assert_eq!(plan.context.mode, ThreadMode::Continue);
            assert_eq!(plan.context.message_offset, 2);
            assert_eq!(plan.context.previous_message_id.as_deref(), Some("msg_1"));
            assert_eq!(plan.context.previous_request_id.as_deref(), Some("req_1"));
            assert_eq!(plan.context.prompt_id, first.context.prompt_id);
        }

        // 时钟回拨也不能授权续接。
        let skewed = plan_at(&store, &messages, 999);
        assert_eq!(skewed.context.mode, ThreadMode::Create);
    }

    #[test]
    fn edited_forked_or_replayed_history_recreates() {
        let (mut store, _, messages) = after_first_call();
        let done = assistant("done");
        let second = plan_at(&store, &messages, 2000);
        commit_ok(&mut store, second, &done, "msg_2", "req_2", 3000);
        let mut full = messages.clone();
        full.extend([done, user("q2")]);

        let mut edited = full.clone();
        edited[0] = user("q1 edited");
        assert_eq!(
            plan_at(&store, &edited, 4000).context.mode,
            ThreadMode::Create
        );

        let mut forked = full.clone();
        forked[3] = assistant("other");
        assert_eq!(
            plan_at(&store, &forked, 4000).context.mode,
            ThreadMode::Create
        );

        let mut tool_edited = full.clone();
        tool_edited[1]["content"][0]["input"] = json!({"cache_control": "changed"});
        assert_eq!(
            plan_at(&store, &tool_edited, 4000).context.mode,
            ThreadMode::Create
        );

        // 回到更早分支、或原样重放（没有新增消息）。
        assert_eq!(
            plan_at(&store, &full[..3], 4000).context.mode,
            ThreadMode::Create
        );
        assert_eq!(
            plan_at(&store, &full[..4], 4000).context.mode,
            ThreadMode::Create
        );
        // 新增消息以 assistant 开头不是合法续接。
        let mut bad_next = full[..3].to_vec();
        bad_next.push(assistant("done"));
        bad_next.push(assistant("again"));
        assert_eq!(
            plan_at(&store, &bad_next, 4000).context.mode,
            ThreadMode::Create
        );
        assert_eq!(
            plan_at(&store, &full, 4000).context.mode,
            ThreadMode::Continue
        );
    }

    #[test]
    fn cache_anchors_and_string_content_do_not_break_the_prefix() {
        let (store, _, mut messages) = after_first_call();
        messages[0] = json!({"role": "user", "content": [
            {"type": "text", "text": "q1", "cache_control": {"type": "ephemeral"}}
        ]});
        assert_eq!(
            plan_at(&store, &messages, 2000).context.mode,
            ThreadMode::Continue
        );
    }

    #[test]
    fn fixed_parameters_gate_continuation_but_variable_fields_do_not() {
        let (store, _, messages) = after_first_call();
        let plan_with = |edit: &dyn Fn(&mut Value)| {
            let mut b = body(&messages);
            edit(&mut b);
            store
                .plan(SCOPE, SID.into(), &b, 2000)
                .expect("plan")
                .context
                .mode
        };
        assert_eq!(
            plan_with(&|b| b["model"] = json!("model-b")),
            ThreadMode::Create
        );
        assert_eq!(
            plan_with(&|b| b["system"][0]["text"] = json!("changed")),
            ThreadMode::Create
        );
        assert_eq!(plan_with(&|b| b["tools"] = json!([])), ThreadMode::Create);
        assert_eq!(
            plan_with(&|b| b["thinking"] = json!({"type": "enabled"})),
            ThreadMode::Create
        );
        assert_eq!(
            plan_with(&|b| b["tool_choice"] = json!({"type": "any"})),
            ThreadMode::Create
        );

        assert_eq!(
            plan_with(&|b| b["max_tokens"] = json!(9)),
            ThreadMode::Continue
        );
        assert_eq!(
            plan_with(&|b| b["stream"] = json!(false)),
            ThreadMode::Continue
        );
        assert_eq!(
            plan_with(&|b| b["stop_sequences"] = json!(["x"])),
            ThreadMode::Continue
        );
        assert_eq!(
            plan_with(&|b| b["metadata"] = json!({"user_id": "u"})),
            ThreadMode::Continue
        );
        assert_eq!(
            plan_with(&|b| {
                b["thread"] = json!({"type": "create"});
                b["diagnostics"] = json!({"previous_message_id": null});
            }),
            ThreadMode::Continue
        );
    }

    #[test]
    fn scope_and_session_are_isolated() {
        let (store, _, messages) = after_first_call();
        let other_scope = store
            .plan("scope-b", SID.into(), &body(&messages), 2000)
            .expect("plan");
        assert_eq!(other_scope.context.mode, ThreadMode::Create);
        assert!(other_scope.context.previous_request_id.is_none());

        let other_session = store
            .plan(
                SCOPE,
                "11111111-1111-4111-8111-111111111111".into(),
                &body(&messages),
                2000,
            )
            .expect("plan");
        assert_eq!(other_session.context.mode, ThreadMode::Create);
        assert!(other_session.context.previous_request_id.is_none());
    }

    #[test]
    fn unusable_responses_never_authorize_continuation() {
        let reply = tool_call("toolu_1");
        let messages = [user("q1"), reply.clone(), tool_result("toolu_1")];
        let cases: [(&str, bool, Value); 5] = [
            ("msg_1", false, reply.clone()),
            ("", true, reply.clone()),
            ("msg 1", true, reply.clone()),
            ("msg_1", true, user("not assistant")),
            ("msg_1", true, json!("not an object")),
        ];
        for (id, reusable, response) in cases {
            let mut store = empty();
            let first = plan_at(&store, &[user("q1")], 0);
            store
                .commit(
                    first.clone(),
                    &[response],
                    id,
                    Some("req_1"),
                    1000,
                    reusable,
                )
                .expect("commit");
            let next = plan_at(&store, &messages, 2000);
            assert_eq!(next.context.mode, ThreadMode::Create, "{id:?} {reusable}");
            // 谱系与请求 ID 仍然保留。
            assert_eq!(next.context.prompt_id, first.context.prompt_id);
            assert_eq!(next.context.previous_request_id.as_deref(), Some("req_1"));
        }
        let mut store = empty();
        let first = plan_at(&store, &[user("q1")], 0);
        store
            .commit(first, &[reply], "msg_1", None, 1000, true)
            .expect("commit");
        assert!(
            plan_at(&store, &messages, 2000)
                .context
                .previous_request_id
                .is_none()
        );
    }

    #[test]
    fn disabled_mode_is_sticky_and_persists() {
        let (mut store, _, messages) = after_first_call();
        let mut plan = plan_at(&store, &messages, 2000);
        plan.context.mode = ThreadMode::Disabled;
        plan.context.message_offset = 0;
        plan.context.previous_message_id = None;
        commit_ok(&mut store, plan, &assistant("done"), "msg_2", "req_2", 3000);

        let mut next = messages.clone();
        next.extend([assistant("done"), user("q2")]);
        let reloaded =
            SessionStore::from_bytes(Some(&store.to_bytes().expect("bytes"))).expect("reload");
        for s in [&store, &reloaded] {
            let plan = plan_at(s, &next, 4000);
            assert_eq!(plan.context.mode, ThreadMode::Disabled);
            assert_eq!(plan.context.message_offset, 0);
            assert!(plan.context.previous_message_id.is_none());
        }

        // 之后即使以 Create 提交也不会解除禁用；换会话/作用域不受影响。
        let mut again = plan_at(&store, &next, 4000);
        again.context.mode = ThreadMode::Create;
        commit_ok(&mut store, again, &assistant("x"), "msg_3", "req_3", 5000);
        assert_eq!(
            plan_at(&store, &next, 6000).context.mode,
            ThreadMode::Disabled
        );
        let other = store
            .plan("scope-b", SID.into(), &body(&next), 6000)
            .expect("plan");
        assert_eq!(other.context.mode, ThreadMode::Create);
        let other_session = store
            .plan(
                SCOPE,
                "11111111-1111-4111-8111-111111111111".into(),
                &body(&next),
                6000,
            )
            .expect("plan");
        assert_eq!(other_session.context.mode, ThreadMode::Create);
    }

    #[test]
    fn protocol_recovery_to_full_create_recommits_the_full_transcript() {
        let (mut store, _, messages) = after_first_call();
        let mut plan = plan_at(&store, &messages, 2000);
        assert_eq!(plan.context.mode, ThreadMode::Continue);
        plan.context.mode = ThreadMode::Create;
        plan.context.message_offset = 0;
        plan.context.previous_message_id = None;
        let done = assistant("done");
        commit_ok(&mut store, plan, &done, "msg_2", "req_2", 3000);

        let mut next = messages;
        next.extend([done, user("q2")]);
        let plan = plan_at(&store, &next, 4000);
        assert_eq!(plan.context.mode, ThreadMode::Continue);
        assert_eq!(plan.context.message_offset, 4);
        assert_eq!(plan.context.previous_message_id.as_deref(), Some("msg_2"));
    }

    #[test]
    fn stale_out_of_order_commit_cannot_overwrite_newer_state() {
        let (mut store, _, messages) = after_first_call();
        let late = plan_at(&store, &[user("other conversation")], 500);
        commit_ok(
            &mut store,
            late,
            &assistant("zzz"),
            "msg_old",
            "req_old",
            900,
        );
        let plan = plan_at(&store, &messages, 2000);
        assert_eq!(plan.context.mode, ThreadMode::Continue);
        assert_eq!(plan.context.previous_message_id.as_deref(), Some("msg_1"));
    }

    #[test]
    fn serialization_preserves_foreign_root_fields_and_stores_no_content() {
        let root = json!({
            "version": 1, "code_verifier": "verifier-xyz", "state": "st",
            "extra": {"a": [1, 2]}
        });
        let raw = serde_json::to_vec(&root).expect("json");
        let mut store = SessionStore::from_bytes(Some(&raw)).expect("load");
        let reply = json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "SECRET-THOUGHT", "signature": "SIGSECRET"},
            {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}
        ]});
        let first = store
            .plan(SCOPE, SID.into(), &body(&[user("PRIVATE-PROMPT")]), 0)
            .expect("plan");
        commit_ok(&mut store, first, &reply, "msg_1", "req_1", 1000);

        let bytes = store.to_bytes().expect("bytes");
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        for forbidden in ["PRIVATE-PROMPT", "SECRET-THOUGHT", "SIGSECRET", "toolu_1"] {
            assert!(!text.contains(forbidden), "{forbidden} leaked");
        }
        let saved: Value = serde_json::from_slice(&bytes).expect("json");
        for key in ["version", "code_verifier", "state", "extra"] {
            assert_eq!(saved[key], root[key]);
        }
        assert!(saved[STATE_KEY].is_object());

        let reloaded = SessionStore::from_bytes(Some(&bytes)).expect("reload");
        let messages = [user("PRIVATE-PROMPT"), reply, tool_result("toolu_1")];
        let plan = plan_at(&reloaded, &messages, 2000);
        assert_eq!(plan.context.mode, ThreadMode::Continue);
        assert_eq!(plan.context.previous_message_id.as_deref(), Some("msg_1"));
    }

    #[test]
    fn root_and_section_errors_are_explicit_or_self_healing() {
        assert!(
            SessionStore::from_bytes(Some(b""))
                .expect("empty")
                .sessions
                .is_empty()
        );
        assert!(
            SessionStore::from_bytes(Some(b" \n"))
                .expect("blank")
                .sessions
                .is_empty()
        );
        assert!(SessionStore::from_bytes(Some(b"not json")).is_err());
        assert!(SessionStore::from_bytes(Some(b"[]")).is_err());
        assert!(SessionStore::from_bytes(Some(b"null")).is_err());

        for corrupt in [
            json!({"version": 1, (STATE_KEY): "garbage"}),
            json!({"version": 1, (STATE_KEY): {"v": 99, "sessions": {}}}),
            json!({"version": 1, (STATE_KEY): {"v": 1, "sessions": {"s": {"session_id": 7}}}}),
        ] {
            let raw = serde_json::to_vec(&corrupt).expect("json");
            let store = SessionStore::from_bytes(Some(&raw)).expect("self-heals");
            assert!(store.sessions.is_empty());
            let saved: Value =
                serde_json::from_slice(&store.to_bytes().expect("bytes")).expect("json");
            assert_eq!(saved, json!({"version": 1}));
        }
    }

    #[test]
    fn invalid_entries_are_dropped_on_load() {
        let (store, _, _) = after_first_call();
        let mut saved: Value =
            serde_json::from_slice(&store.to_bytes().expect("bytes")).expect("json");
        saved[STATE_KEY]["sessions"][SCOPE]["prompt_id"] = json!("not-a-uuid");
        let raw = serde_json::to_vec(&saved).expect("json");
        assert!(
            SessionStore::from_bytes(Some(&raw))
                .expect("load")
                .sessions
                .is_empty()
        );

        let mut saved: Value =
            serde_json::from_slice(&store.to_bytes().expect("bytes")).expect("json");
        saved[STATE_KEY]["sessions"][SCOPE]["thread"]["chain"] = json!("zz");
        let raw = serde_json::to_vec(&saved).expect("json");
        let loaded = SessionStore::from_bytes(Some(&raw)).expect("load");
        assert!(loaded.sessions[SCOPE].thread.is_none());
    }

    #[test]
    fn entries_are_bounded_oldest_first_and_expire() {
        let mut store = empty();
        for i in 0..(MAX_ENTRIES + 20) {
            let scope = format!("scope-{i:03}");
            let now = i64::try_from(i).expect("small") * 1000;
            let plan = store
                .plan(&scope, SID.into(), &body(&[user("q")]), now)
                .expect("plan");
            store
                .commit(plan, &[assistant("a")], "msg_x", Some("req_x"), now, true)
                .expect("commit");
        }
        assert_eq!(store.sessions.len(), MAX_ENTRIES);
        assert!(!store.sessions.contains_key("scope-000"));
        assert!(
            store
                .sessions
                .contains_key(&format!("scope-{:03}", MAX_ENTRIES + 19))
        );
        let reloaded =
            SessionStore::from_bytes(Some(&store.to_bytes().expect("bytes"))).expect("reload");
        assert_eq!(reloaded.sessions.len(), MAX_ENTRIES);

        let late = RETENTION_MS + 100_000;
        let plan = store
            .plan("scope-late", SID.into(), &body(&[user("q")]), late)
            .expect("plan");
        store
            .commit(plan, &[], "", None, late, false)
            .expect("commit");
        assert_eq!(store.sessions.len(), 1);
        assert!(store.sessions.contains_key("scope-late"));
    }

    #[test]
    fn plan_rejects_malformed_inputs() {
        let store = empty();
        let ok = body(&[user("q")]);
        assert!(store.plan("", SID.into(), &ok, 0).is_err());
        assert!(store.plan("bad scope", SID.into(), &ok, 0).is_err());
        assert!(store.plan(SCOPE, String::new(), &ok, 0).is_err());
        assert!(store.plan(SCOPE, SID.into(), &json!([]), 0).is_err());
        assert!(
            store
                .plan(SCOPE, SID.into(), &json!({"model": "m"}), 0)
                .is_err()
        );
        assert!(
            store
                .plan(SCOPE, SID.into(), &json!({"messages": []}), 0)
                .is_err()
        );
    }

    #[test]
    fn digest_is_independent_of_key_order() {
        let a = json!({"role": "assistant", "content": [{"type": "text", "text": "x"}]});
        let b: Value =
            serde_json::from_str(r#"{"content":[{"text":"x","type":"text"}],"role":"assistant"}"#)
                .expect("json");
        let g = genesis();
        assert_eq!(chain_step(&g, &a), chain_step(&g, &b));
        assert_ne!(chain_step(&g, &a), chain_step(&g, &assistant("y")));
    }
}
