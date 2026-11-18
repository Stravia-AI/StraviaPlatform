//! Claude Code diagnostic lineage and short-lived native thread state.
//! Only hashes, counts, timestamps and upstream IDs are persisted.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// 在私有状态根对象中占用的键；根对象的其他键（OAuth 状态等）原样保留。
const STATE_KEY: &str = "claude_threads";
const STATE_VERSION: u32 = 1;
/// 提示词谱系和诊断标识的保留期。
const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_ENTRIES: usize = 64;
const MAX_ID_LEN: usize = 128;
const THREAD_FRESH_MS: i64 = 270_000;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThreadMode {
    Off,
    Create,
    Continue {
        offset: usize,
        previous_message_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WireContext {
    pub session_id: String,
    pub prompt_id: String,
    pub prompt_index: usize,
    pub turn_index: usize,
    pub previous_message_id: Option<String>,
    pub previous_request_id: Option<String>,
}

/// 一次请求的规划结果；`commit` 在成功完成后据此落盘。
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub context: WireContext,
    pub mode: ThreadMode,
    scope: String,
    planned_ms: i64,
    fixed: Hash,
    input_chain: Hash,
    input_count: usize,
    turn_chain: Hash,
    turn_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThreadTip {
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
    prompt_id: String,
    turn_chain: String,
    turn_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_tip_v2: Option<ThreadTip>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_disabled_at_v2: Option<i64>,
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
    /// 本模块自己的键损坏或版本不符时只丢弃该节，其余键保留。
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

    /// `body` 是 prepare 之后的完整请求体；消息摘要用于识别同一提示词谱系。
    pub(crate) fn plan(
        &self,
        scope: &str,
        session_id: String,
        body: &Value,
        now_ms: i64,
        allow_threads: bool,
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
        let scope_entry = self.sessions.get(scope);
        let entry = scope_entry.filter(|e| {
            e.session_id == session_id && now_ms.saturating_sub(e.touched_ms) <= RETENTION_MS
        });
        let enabled = allow_threads
            && thread_compatible(object)
            && !scope_entry
                .and_then(|e| e.thread_disabled_at_v2)
                .is_some_and(|at| now_ms < at || now_ms.saturating_sub(at) < RETENTION_MS);
        let fixed = if enabled {
            fixed_digest(object)
        } else {
            [0; 32]
        };
        let thread = entry.and_then(|e| e.thread_tip_v2.as_ref()).filter(|t| {
            enabled
                && t.count > 0
                && t.count < messages.len()
                && now_ms >= t.at_ms
                && now_ms.saturating_sub(t.at_ms) < THREAD_FRESH_MS
                && t.fixed == hex(&fixed)
        });
        let mut chain = genesis();
        let mut prefix_matches = false;
        let mut turn_scoped = false;
        let mut turn_chain = chain;
        let mut turn_count = 0;
        for (index, message) in messages.iter().enumerate() {
            turn_scoped |= enabled
                && message.get("clear_at").and_then(Value::as_str) == Some("next_user_message");
            chain = chain_step(&chain, message);
            if thread.is_some_and(|t| t.count == index + 1 && t.chain == hex(&chain)) {
                prefix_matches = true;
            }
            if is_conversational_user(message) {
                turn_chain = chain;
                turn_count = index + 1;
            }
        }
        let mode = if !enabled || turn_scoped {
            ThreadMode::Off
        } else if let Some(t) = thread.filter(|t| {
            prefix_matches && messages.get(t.count).is_some_and(nonempty_user_or_system)
        }) {
            ThreadMode::Continue {
                offset: t.count,
                previous_message_id: t.message_id.clone(),
            }
        } else {
            ThreadMode::Create
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
                // 宿主在发送前提供持久逻辑谱系位置；截断历史不能推断序数。
                prompt_index: 0,
                turn_index: 0,
                previous_message_id: entry.and_then(|e| e.last_message_id.clone()),
            },
            scope: scope.to_owned(),
            mode,
            planned_ms: now_ms,
            fixed,
            input_chain: chain,
            input_count: messages.len(),
            turn_chain,
            turn_count,
        })
    }

    /// Record diagnostic and prompt lineage facts after a successful response.
    pub(crate) fn commit(
        &mut self,
        plan: &Plan,
        response_messages: &[Value],
        response_id: &str,
        request_id: Option<&str>,
        now_ms: i64,
    ) -> Result<(), String> {
        let Plan {
            context,
            scope,
            mode,
            fixed,
            input_chain,
            input_count,
            turn_chain,
            turn_count,
            ..
        } = plan;
        let old = self.sessions.remove(scope);
        if old
            .as_ref()
            .is_some_and(|o| o.session_id == context.session_id && o.touched_ms > now_ms)
        {
            if let Some(o) = old {
                self.sessions.insert(scope.clone(), o);
            }
            return Ok(());
        }
        let disabled_at = old
            .as_ref()
            .and_then(|o| o.thread_disabled_at_v2)
            .filter(|&at| now_ms < at || now_ms.saturating_sub(at) < RETENTION_MS);
        let tip = if *mode != ThreadMode::Off
            && disabled_at.is_none()
            && valid_message_id(response_id)
            && response_messages.len() == 1
            && response_messages[0].get("role").and_then(Value::as_str) == Some("assistant")
        {
            input_count.checked_add(1).map(|count| ThreadTip {
                fixed: hex(fixed),
                chain: hex(&chain_step(input_chain, &response_messages[0])),
                count,
                message_id: response_id.to_owned(),
                at_ms: now_ms,
            })
        } else if *mode == ThreadMode::Off && disabled_at.is_none() {
            old.as_ref()
                .filter(|e| e.session_id == context.session_id)
                .and_then(|e| e.thread_tip_v2.clone())
        } else {
            None
        };
        let entry = Entry {
            session_id: context.session_id.clone(),
            touched_ms: now_ms,
            prompt_id: context.prompt_id.clone(),
            turn_chain: hex(turn_chain),
            turn_count: *turn_count,
            last_request_id: request_id.filter(|r| valid_id(r)).map(str::to_owned),
            last_message_id: valid_id(response_id).then(|| response_id.to_owned()),
            thread_tip_v2: tip,
            thread_disabled_at_v2: disabled_at,
        };
        self.sessions
            .retain(|_, e| now_ms.saturating_sub(e.touched_ms) <= RETENTION_MS);
        self.evict_to(MAX_ENTRIES - 1);
        self.sessions.insert(scope.clone(), entry);
        Ok(())
    }

    /// Persist only explicit protocol rejection, without invalidating a newer tip.
    pub(crate) fn reject(&mut self, plan: &Plan, unsupported: bool) -> bool {
        if plan.mode == ThreadMode::Off {
            return false;
        }
        if unsupported {
            if let Some(entry) = self.sessions.get_mut(&plan.scope) {
                if entry
                    .thread_disabled_at_v2
                    .is_some_and(|at| at >= plan.planned_ms)
                {
                    return false;
                }
                entry.thread_disabled_at_v2 = Some(plan.planned_ms);
                entry.touched_ms = entry.touched_ms.max(plan.planned_ms);
                return true;
            }
            self.sessions
                .retain(|_, e| plan.planned_ms.saturating_sub(e.touched_ms) <= RETENTION_MS);
            self.evict_to(MAX_ENTRIES - 1);
            self.sessions.insert(
                plan.scope.clone(),
                Entry {
                    session_id: plan.context.session_id.clone(),
                    touched_ms: plan.planned_ms,
                    prompt_id: plan.context.prompt_id.clone(),
                    turn_chain: hex(&plan.turn_chain),
                    turn_count: plan.turn_count,
                    last_request_id: None,
                    last_message_id: None,
                    thread_tip_v2: None,
                    thread_disabled_at_v2: Some(plan.planned_ms),
                },
            );
            return true;
        }
        let ThreadMode::Continue {
            previous_message_id,
            ..
        } = &plan.mode
        else {
            return false;
        };
        if let Some(entry) = self.sessions.get_mut(&plan.scope)
            && entry.session_id == plan.context.session_id
            && entry
                .thread_tip_v2
                .as_ref()
                .is_some_and(|tip| tip.message_id == *previous_message_id)
        {
            entry.thread_tip_v2 = None;
            return true;
        }
        false
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
            if e.last_message_id.as_deref().is_some_and(|id| !valid_id(id)) {
                e.last_message_id = None;
            }
            if e.thread_tip_v2.as_ref().is_some_and(|t| {
                t.count == 0
                    || !valid_message_id(&t.message_id)
                    || !is_hex_digest(&t.fixed)
                    || !is_hex_digest(&t.chain)
            }) {
                e.thread_tip_v2 = None;
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

fn valid_message_id(s: &str) -> bool {
    s.starts_with("msg_") && s.len() > 4 && valid_id(s)
}

fn nonempty_user_or_system(message: &Value) -> bool {
    matches!(
        message.get("role").and_then(Value::as_str),
        Some("user" | "system")
    ) && match message.get("content") {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    }
}

fn thread_compatible(body: &Map<String, Value>) -> bool {
    if body
        .get("cache_control")
        .and_then(|cache| cache.get("scope"))
        .and_then(Value::as_str)
        == Some("turn")
    {
        return false;
    }
    match body.get("context_management") {
        None | Some(Value::Null) => true,
        Some(value) => value.as_object().is_some_and(|obj| {
            obj.len() == 1
                && obj
                    .get("edits")
                    .and_then(Value::as_array)
                    .is_some_and(|edits| {
                        edits.iter().all(|edit| {
                            edit.as_object().is_some_and(|e| {
                                e.len() == 2
                                    && e.get("type").and_then(Value::as_str)
                                        == Some("clear_thinking_20251015")
                                    && e.get("keep").and_then(Value::as_str) == Some("all")
                            })
                        })
                    })
        }),
    }
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

/// 原生 CLI 将含任一 tool_result 的 user 消息排除在新提示词之外。
fn is_conversational_user(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && match message.get("content") {
            Some(Value::String(_)) => true,
            Some(Value::Array(blocks)) => {
                !blocks.is_empty()
                    && !blocks
                        .iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            }
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

    #[test]
    fn lineage_and_diagnostics_survive_round_trip_without_native_threads() {
        let mut store = SessionStore::from_bytes(Some(br#"{"oauth":"kept"}"#)).unwrap();
        let first_body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let first = store
            .plan("scope", "session".into(), &first_body, 0, false)
            .unwrap();
        let prompt = first.context.prompt_id.clone();
        store
            .commit(&first, &[], "msg_1", Some("req_1"), 1)
            .unwrap();
        let bytes = store.to_bytes().unwrap();
        let state: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(state["oauth"], "kept");
        let entry = &state["claude_threads"]["sessions"]["scope"];
        assert!(entry.get("thread").is_none());
        assert!(entry.get("disabled").is_none());
        assert!(!String::from_utf8_lossy(&bytes).contains("hello"));
        let store = SessionStore::from_bytes(Some(&bytes)).unwrap();
        let next_body = json!({"messages": [{"role": "user", "content": "hello"}, {"role": "assistant", "content": "tool call"}, {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x", "content": "ok"}]}]});
        let next = store
            .plan("scope", "session".into(), &next_body, 2, false)
            .unwrap();
        assert_eq!(next.context.prompt_id, prompt);
        assert_eq!(next.context.previous_message_id.as_deref(), Some("msg_1"));
        assert_eq!(next.context.previous_request_id.as_deref(), Some("req_1"));
        let isolated = store
            .plan("other", "session".into(), &next_body, 2, false)
            .unwrap();
        assert_eq!(isolated.context.previous_message_id, None);
    }

    #[test]
    fn stale_completion_does_not_replace_newer_diagnostics() {
        let mut store = SessionStore::from_bytes(None).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "q"}]});
        let older = store
            .plan("scope", "session".into(), &body, 0, false)
            .unwrap();
        let newer = store
            .plan("scope", "session".into(), &body, 1, false)
            .unwrap();
        store
            .commit(&newer, &[], "msg_new", Some("req_new"), 10)
            .unwrap();
        store
            .commit(&older, &[], "msg_old", Some("req_old"), 5)
            .unwrap();
        let plan = store
            .plan("scope", "session".into(), &body, 11, false)
            .unwrap();
        assert_eq!(plan.context.previous_message_id.as_deref(), Some("msg_new"));
        assert_eq!(plan.context.previous_request_id.as_deref(), Some("req_new"));
    }

    #[test]
    fn thread_prefix_freshness_and_response_shape() {
        let mut store = SessionStore::from_bytes(None).unwrap();
        let first = json!({"messages":[{"role":"user","content":"q"}]});
        let reply = json!({"role":"assistant","content":"a"});
        let plan = store.plan("s", "session".into(), &first, 0, true).unwrap();
        assert_eq!(plan.mode, ThreadMode::Create);
        store
            .commit(&plan, std::slice::from_ref(&reply), "msg_1", None, 1)
            .unwrap();
        let next = json!({"messages":[{"role":"user","content":"q"}, reply, {"role":"user","content":"next"}]});
        let get = |body: &Value, now| {
            store
                .plan("s", "session".into(), body, now, true)
                .unwrap()
                .mode
        };
        assert_eq!(
            get(&next, 2),
            ThreadMode::Continue {
                offset: 2,
                previous_message_id: "msg_1".into()
            }
        );
        assert_eq!(get(&next, 270_001), ThreadMode::Create);
        assert_eq!(get(&next, 0), ThreadMode::Create);
        let mut changed = next.clone();
        changed["messages"][0]["content"] = json!("changed");
        assert_eq!(get(&changed, 2), ThreadMode::Create);
        changed = next.clone();
        changed["messages"][2]["content"] = json!("");
        assert_eq!(get(&changed, 2), ThreadMode::Create);
        changed = next.clone();
        changed["context_management"] =
            json!({"edits":[{"type":"clear_thinking_20251015","keep":"last"}]});
        assert_eq!(get(&changed, 2), ThreadMode::Off);
        changed["context_management"]["edits"][0]["keep"] = json!("all");
        assert_eq!(get(&changed, 2), ThreadMode::Create);
        let bad = store
            .plan("other", "session".into(), &first, 0, true)
            .unwrap();
        store
            .commit(
                &bad,
                &[json!({"role":"assistant"}), json!({"role":"assistant"})],
                "msg_2",
                None,
                1,
            )
            .unwrap();
        assert_eq!(
            store
                .plan("other", "session".into(), &next, 2, true)
                .unwrap()
                .mode,
            ThreadMode::Create
        );
    }

    #[test]
    fn rejection_is_scoped_sticky_and_does_not_clear_newer_tips() {
        let mut store = SessionStore::from_bytes(None).unwrap();
        let body = json!({"messages":[{"role":"user","content":"q"}]});
        let reply = json!({"role":"assistant","content":"a"});
        let first = store.plan("s", "session".into(), &body, 0, true).unwrap();
        store
            .commit(&first, std::slice::from_ref(&reply), "msg_1", None, 1)
            .unwrap();
        let next = json!({"messages":[{"role":"user","content":"q"}, reply, {"role":"user","content":"next"}]});
        let old = store.plan("s", "session".into(), &next, 2, true).unwrap();
        store
            .commit(
                &old,
                &[json!({"role":"assistant","content":"new"})],
                "msg_2",
                None,
                3,
            )
            .unwrap();
        assert!(!store.reject(&old, false));
        let mut replay = old.clone();
        replay.mode = ThreadMode::Off;
        store.commit(&replay, &[], "msg_replay", None, 4).unwrap();
        assert_eq!(
            store.sessions["s"]
                .thread_tip_v2
                .as_ref()
                .unwrap()
                .message_id,
            "msg_2"
        );
        assert!(store.reject(&old, true));
        store
            .commit(
                &first,
                &[json!({"role":"assistant","content":"a"})],
                "msg_3",
                None,
                5,
            )
            .unwrap();
        let saved = store.to_bytes().unwrap();
        let store = SessionStore::from_bytes(Some(&saved)).unwrap();
        assert_eq!(
            store
                .plan("s", "other-session".into(), &body, 6, true)
                .unwrap()
                .mode,
            ThreadMode::Off
        );
        assert_eq!(
            store
                .plan("s", "session".into(), &body, 2 + RETENTION_MS, true)
                .unwrap()
                .mode,
            ThreadMode::Create
        );
    }

    #[test]
    fn first_create_rejection_can_be_persisted() {
        let mut store = SessionStore::from_bytes(None).unwrap();
        let body = json!({"messages":[{"role":"user","content":"q"}]});
        let plan = store.plan("s", "session".into(), &body, 10, true).unwrap();
        assert!(store.reject(&plan, true));
        let saved = store.to_bytes().unwrap();
        let store = SessionStore::from_bytes(Some(&saved)).unwrap();
        assert_eq!(
            store
                .plan("s", "session".into(), &body, 11, true)
                .unwrap()
                .mode,
            ThreadMode::Off
        );
    }

    #[test]
    fn old_native_fields_are_discarded_without_losing_diagnostics() {
        let mut store = SessionStore::from_bytes(None).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "q"}]});
        let plan = store
            .plan("scope", "session".into(), &body, 0, false)
            .unwrap();
        store.commit(&plan, &[], "msg_1", Some("req_1"), 1).unwrap();
        let mut state: Value = serde_json::from_slice(&store.to_bytes().unwrap()).unwrap();
        state["claude_threads"]["sessions"]["scope"]["disabled"] = json!(true);
        state["claude_threads"]["sessions"]["scope"]["thread"] = json!({"legacy": true});
        let store = SessionStore::from_bytes(Some(&serde_json::to_vec(&state).unwrap())).unwrap();
        let plan = store
            .plan("scope", "session".into(), &body, 2, false)
            .unwrap();
        assert_eq!(plan.context.previous_message_id.as_deref(), Some("msg_1"));
        let saved: Value = serde_json::from_slice(&store.to_bytes().unwrap()).unwrap();
        assert!(
            saved["claude_threads"]["sessions"]["scope"]
                .get("thread")
                .is_none()
        );
        assert!(
            saved["claude_threads"]["sessions"]["scope"]
                .get("disabled")
                .is_none()
        );
    }
}
