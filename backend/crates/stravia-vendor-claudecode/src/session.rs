//! Claude Code diagnostic identifiers and prompt lineage, scoped to a session.
//! Only hashes, counts, timestamps and upstream IDs are persisted; the request
//! always carries full history and never sends a native message thread.

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

type Hash = [u8; 32];

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
    scope: String,
    turn_chain: Hash,
    turn_count: usize,
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
        let entry = self.sessions.get(scope).filter(|e| {
            e.session_id == session_id && now_ms.saturating_sub(e.touched_ms) <= RETENTION_MS
        });
        let mut chain = genesis();
        let mut turn_chain = chain;
        let mut turn_count = 0;
        for (index, message) in messages.iter().enumerate() {
            chain = chain_step(&chain, message);
            if is_conversational_user(message) {
                turn_chain = chain;
                turn_count = index + 1;
            }
        }
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
            turn_chain,
            turn_count,
        })
    }

    /// Record diagnostic and prompt lineage facts after a successful response.
    pub(crate) fn commit(
        &mut self,
        plan: Plan,
        response_id: &str,
        request_id: Option<&str>,
        now_ms: i64,
    ) -> Result<(), String> {
        let Plan {
            context,
            scope,
            turn_chain,
            turn_count,
        } = plan;
        let old = self
            .sessions
            .remove(&scope)
            .filter(|e| e.session_id == context.session_id);
        if old.as_ref().is_some_and(|o| o.touched_ms > now_ms) {
            if let Some(o) = old {
                self.sessions.insert(scope, o);
            }
            return Ok(());
        }
        let entry = Entry {
            session_id: context.session_id,
            touched_ms: now_ms,
            prompt_id: context.prompt_id,
            turn_chain: hex(&turn_chain),
            turn_count,
            last_request_id: request_id.filter(|r| valid_id(r)).map(str::to_owned),
            last_message_id: valid_id(response_id).then(|| response_id.to_owned()),
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
            if e.last_message_id.as_deref().is_some_and(|id| !valid_id(id)) {
                e.last_message_id = None;
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
            .plan("scope", "session".into(), &first_body, 0)
            .unwrap();
        let prompt = first.context.prompt_id.clone();
        store.commit(first, "msg_1", Some("req_1"), 1).unwrap();
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
            .plan("scope", "session".into(), &next_body, 2)
            .unwrap();
        assert_eq!(next.context.prompt_id, prompt);
        assert_eq!(next.context.previous_message_id.as_deref(), Some("msg_1"));
        assert_eq!(next.context.previous_request_id.as_deref(), Some("req_1"));
        let isolated = store
            .plan("other", "session".into(), &next_body, 2)
            .unwrap();
        assert_eq!(isolated.context.previous_message_id, None);
    }

    #[test]
    fn stale_completion_does_not_replace_newer_diagnostics() {
        let mut store = SessionStore::from_bytes(None).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "q"}]});
        let older = store.plan("scope", "session".into(), &body, 0).unwrap();
        let newer = store.plan("scope", "session".into(), &body, 1).unwrap();
        store.commit(newer, "msg_new", Some("req_new"), 10).unwrap();
        store.commit(older, "msg_old", Some("req_old"), 5).unwrap();
        let plan = store.plan("scope", "session".into(), &body, 11).unwrap();
        assert_eq!(plan.context.previous_message_id.as_deref(), Some("msg_new"));
        assert_eq!(plan.context.previous_request_id.as_deref(), Some("req_new"));
    }

    #[test]
    fn old_native_fields_are_discarded_without_losing_diagnostics() {
        let mut store = SessionStore::from_bytes(None).unwrap();
        let body = json!({"messages": [{"role": "user", "content": "q"}]});
        let plan = store.plan("scope", "session".into(), &body, 0).unwrap();
        store.commit(plan, "msg_1", Some("req_1"), 1).unwrap();
        let mut state: Value = serde_json::from_slice(&store.to_bytes().unwrap()).unwrap();
        state["claude_threads"]["sessions"]["scope"]["disabled"] = json!(true);
        state["claude_threads"]["sessions"]["scope"]["thread"] = json!({"legacy": true});
        let store = SessionStore::from_bytes(Some(&serde_json::to_vec(&state).unwrap())).unwrap();
        let plan = store.plan("scope", "session".into(), &body, 2).unwrap();
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
