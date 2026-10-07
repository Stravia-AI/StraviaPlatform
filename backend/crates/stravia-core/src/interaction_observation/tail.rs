use super::RunEvent;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::canonical;

pub(super) const MAX_CANDIDATES: usize = 128;
const MAX_UNITS: usize = 512;
const MAX_WINDOW_BYTES: usize = 512 * 1024;
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMPARISONS: usize = 65_536;
const MAX_VERIFIED_BYTES: usize = 8 * 1024 * 1024;
const MIN_MATCH_BYTES: usize = 256;
const MIN_ANSWER_BYTES: usize = 64;

#[derive(Clone)]
pub(super) struct Window {
    units: Vec<Unit>,
    bytes: usize,
    complete: bool,
    /// Canonical received-item identity, including the system/developer prefix
    /// and private controls deliberately projected away by tail diagnostics.
    /// Delivery/graph metadata is not client-input content: reconstructed
    /// ancestors retain server-output provenance, not client-replay provenance.
    /// Absent whenever a complete input cannot fit the capture limits.
    received_items: Option<Vec<([u8; 32], bool)>>,
}
#[derive(Clone)]
struct Unit {
    value: Value,
    hash: [u8; 32],
    bytes: usize,
}

impl Window {
    pub(super) fn capture(items: &[AiItem]) -> Option<Self> {
        let start = items
            .iter()
            .position(|item| {
                !matches!(
                    item.role,
                    stravia_runtime_contract::protocol::ir::Role::System
                        | stravia_runtime_contract::protocol::ir::Role::Developer
                )
            })
            .unwrap_or(items.len());
        // Keep the newest suffix that fits. Clients that replay a rewritten full
        // history after local compaction exceed 512 KiB; dropping the whole
        // window leaves no last_unit_hash, so the follow-up cannot find a source.
        // A single unit over the budget still fails closed.
        let mut units = Vec::new();
        let mut bytes = 0;
        for item in items[start..].iter().rev() {
            let Value::Array(values) = canonical::item_value(item) else {
                return None;
            };
            let mut projected = Vec::with_capacity(values.len());
            for mut value in values {
                if private_control(&value) && !public_thinking_projection(&value) {
                    // A nonmatching boundary preserves continuity without retaining private state.
                    value = serde_json::json!({"diagnostic_boundary": stravia_runtime_contract::identifier::new_id()});
                }
                let encoded = serde_json::to_vec(&value).ok()?;
                projected.push(Unit {
                    hash: Sha256::digest(&encoded).into(),
                    bytes: encoded.len(),
                    value,
                });
            }
            for unit in projected.into_iter().rev() {
                if units.len() == MAX_UNITS || bytes + unit.bytes > MAX_WINDOW_BYTES {
                    if units.is_empty() {
                        return None;
                    }
                    units.reverse();
                    return Some(Self {
                        units,
                        bytes,
                        complete: false,
                        received_items: None,
                    });
                }
                bytes += unit.bytes;
                units.push(unit);
            }
        }
        units.reverse();
        Some(Self {
            units,
            bytes,
            complete: true,
            received_items: None,
        })
    }
    pub(super) fn capture_received_input(items: &[AiItem]) -> Option<Self> {
        let mut window = Self::capture(items)?;
        // Delivered outputs and ordinary retained tails never compute this.
        if window.complete && window.current_tail_tool_ids().is_some() {
            window.received_items = Self::capture_received_items(items);
        }
        Some(window)
    }
    fn capture_received_items(items: &[AiItem]) -> Option<Vec<([u8; 32], bool)>> {
        if items.len() > MAX_UNITS {
            return None;
        }
        let mut received = Vec::with_capacity(items.len());
        let mut bytes = 0usize;
        for item in items {
            let mut value = canonical::item_value(item);
            value.sort_all_objects();
            let encoded = serde_json::to_vec(&value).ok()?;
            bytes = bytes.checked_add(encoded.len())?;
            if bytes > MAX_WINDOW_BYTES {
                return None;
            }
            let user_only = item.role == stravia_runtime_contract::protocol::ir::Role::User
                && value.as_array().is_some_and(|units| {
                    units.iter().all(|unit| {
                        unit.get("role").and_then(Value::as_str) == Some("user")
                            && unit.get("native_compaction").is_none()
                    })
                });
            received.push((Sha256::digest(&encoded).into(), user_only));
        }
        Some(received)
    }

    pub(super) fn is_received_prefix_with_only_new_users(&self, input: &Self) -> bool {
        let (Some(old), Some(new)) = (&self.received_items, &input.received_items) else {
            return false;
        };
        !old.is_empty()
            && old.len() < new.len()
            && new.starts_with(old)
            && new[old.len()..].iter().all(|(_, user)| *user)
    }

    fn retained_bytes(&self) -> usize {
        self.bytes
            + self.received_items.as_ref().map_or(0, |items| {
                items.capacity() * std::mem::size_of::<([u8; 32], bool)>()
            })
    }

    pub(super) fn append(&mut self, mut output: Self) -> bool {
        // A delivered input+output tail is never a received-input proof.
        self.received_items = None;
        self.complete = false;
        output.received_items = None;
        output.complete = false;
        while !self.units.is_empty()
            && (self.bytes + output.bytes > MAX_WINDOW_BYTES
                || self.units.len() + output.units.len() > MAX_UNITS)
        {
            let dropped = self.units.remove(0);
            self.bytes = self.bytes.saturating_sub(dropped.bytes);
        }
        if self.bytes + output.bytes > MAX_WINDOW_BYTES
            || self.units.len() + output.units.len() > MAX_UNITS
        {
            if output.bytes > MAX_WINDOW_BYTES || output.units.len() > MAX_UNITS {
                return false;
            }
            *self = output;
            return true;
        }
        self.bytes += output.bytes;
        self.units.append(&mut output.units);
        true
    }
}

fn hash_hex(hash: &[u8; 32]) -> String {
    hash.iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn tool_call_id(value: &Value) -> Option<&str> {
    value
        .get("tool_call")
        .and_then(|call| call.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn tool_result_id(value: &Value) -> Option<&str> {
    (value.get("role").and_then(Value::as_str) == Some("tool"))
        .then(|| value.get("tool_call_id").and_then(Value::as_str))
        .flatten()
        .filter(|id| !id.is_empty())
}

impl Window {
    pub(super) fn last_hash_hex(&self) -> Option<String> {
        self.units.last().map(|unit| hash_hex(&unit.hash))
    }

    pub(super) fn unit_hash_hexes(&self) -> Vec<String> {
        self.units.iter().map(|unit| hash_hex(&unit.hash)).collect()
    }

    pub(super) fn pending_tool_ids(&self) -> Option<Vec<String>> {
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        for unit in &self.units {
            if let Some(id) = tool_call_id(&unit.value) {
                if !seen.insert(id) {
                    return None;
                }
                pending.push(id.to_owned());
            } else if let Some(id) = tool_result_id(&unit.value) {
                pending.retain(|pending_id| pending_id != id);
            }
        }
        Some(pending)
    }

    pub(super) fn current_tail_tool_ids(&self) -> Option<Vec<String>> {
        let tail_start = self
            .units
            .iter()
            .rposition(|unit| unit.value.get("role").and_then(Value::as_str) == Some("assistant"))
            .map_or(0, |index| index + 1);
        let tail = &self.units[tail_start..];
        if !tail
            .iter()
            .any(|unit| tool_result_id(&unit.value).is_some())
        {
            return None;
        }
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        for unit in tail {
            if let Some(id) = tool_result_id(&unit.value) {
                if !seen.insert(id) {
                    return None;
                }
                ids.push(id.to_owned());
            }
        }
        Some(ids)
    }

    pub(super) fn user_after_match(&self, start: usize, units: usize) -> bool {
        let end = start.saturating_add(units);
        self.units
            .get(end..)
            .into_iter()
            .flatten()
            .any(|unit| unit.value.get("role").and_then(Value::as_str) == Some("user"))
    }
}

#[derive(Default)]
pub(super) struct TailIndex {
    windows: HashMap<String, Window>,
    received_inputs: HashMap<String, Window>,
    received_expiries: HashMap<String, i64>,
    /// Insertion order breaks equal-expiry ties during receipt-priority eviction.
    cache_order: VecDeque<(bool, String)>,
    interactions: HashMap<String, String>,
    principals: HashMap<String, String>,
    last_hashes: HashMap<String, Vec<String>>,
    pending_tools: HashMap<String, Vec<String>>,
    expiries: HashMap<String, i64>,
    bytes: usize,
}
impl TailIndex {
    pub(super) fn sweep(&mut self, now: i64) {
        self.received_expiries.retain(|_, expiry| *expiry > now);
        self.received_inputs
            .retain(|id, _| self.received_expiries.contains_key(id));
        self.expiries.retain(|_, expiry| *expiry > now);
        self.windows.retain(|id, _| self.expiries.contains_key(id));
        self.interactions
            .retain(|id, _| self.expiries.contains_key(id));
        self.principals
            .retain(|id, _| self.expiries.contains_key(id));
        self.last_hashes.retain(|_, runs| {
            runs.retain(|id| self.expiries.contains_key(id));
            !runs.is_empty()
        });
        self.pending_tools.retain(|_, runs| {
            runs.retain(|id| self.expiries.contains_key(id));
            !runs.is_empty()
        });
        self.cache_order.retain(|(received, run)| {
            if *received {
                self.received_inputs.contains_key(run)
            } else {
                self.windows.contains_key(run)
            }
        });
        self.bytes = self
            .windows
            .values()
            .chain(self.received_inputs.values())
            .map(Window::retained_bytes)
            .sum();
    }
    pub(super) fn cache_received_input(&mut self, run: String, window: Window, expires_at: i64) {
        if window.received_items.as_ref().is_none_or(Vec::is_empty)
            || self.received_inputs.contains_key(&run)
        {
            return;
        }
        // New receipts must not lose their proof merely because old delivered
        // tails filled the cache. Persisted sources remain reconstructible.
        while self.bytes + window.retained_bytes() > MAX_INDEX_BYTES {
            let Some(position) = self
                .cache_order
                .iter()
                .enumerate()
                .min_by_key(|(_, (received, run))| {
                    if *received {
                        self.received_expiries.get(run)
                    } else {
                        self.expiries.get(run)
                    }
                })
                .map(|(position, _)| position)
            else {
                return;
            };
            let Some((received, oldest)) = self.cache_order.remove(position) else {
                return;
            };
            if received {
                if let Some(window) = self.received_inputs.remove(&oldest) {
                    self.bytes = self.bytes.saturating_sub(window.retained_bytes());
                }
                self.received_expiries.remove(&oldest);
            } else {
                if let Some(window) = self.windows.remove(&oldest) {
                    self.bytes = self.bytes.saturating_sub(window.retained_bytes());
                }
                self.expiries.remove(&oldest);
                self.principals.remove(&oldest);
                self.interactions.remove(&oldest);
                self.last_hashes.retain(|_, runs| {
                    runs.retain(|run| run != &oldest);
                    !runs.is_empty()
                });
                self.pending_tools.retain(|_, runs| {
                    runs.retain(|run| run != &oldest);
                    !runs.is_empty()
                });
            }
        }
        self.bytes += window.retained_bytes();
        self.cache_order.push_back((true, run.clone()));
        self.received_expiries.insert(run.clone(), expires_at);
        self.received_inputs.insert(run, window);
    }

    pub(super) fn received_input(&self, run: &str, now: i64) -> Option<&Window> {
        (self.received_expiries.get(run).copied()? > now)
            .then(|| self.received_inputs.get(run))
            .flatten()
    }
    pub(super) fn insert(
        &mut self,
        run: String,
        window: Window,
        expires_at: i64,
        principal: impl Into<String>,
        interaction_id: impl Into<String>,
    ) {
        if self.windows.contains_key(&run) || self.bytes + window.retained_bytes() > MAX_INDEX_BYTES
        {
            return;
        }
        self.bytes += window.retained_bytes();
        self.cache_order.push_back((false, run.clone()));
        if let Some(hash) = window.last_hash_hex() {
            let runs = self.last_hashes.entry(hash).or_default();
            if !runs.iter().any(|id| id == &run) {
                runs.push(run.clone());
            }
        }
        if let Some(ids) = window.pending_tool_ids() {
            for id in ids {
                let runs = self.pending_tools.entry(id).or_default();
                if !runs.iter().any(|existing| existing == &run) {
                    runs.push(run.clone());
                }
            }
        }
        self.expiries.insert(run.clone(), expires_at);
        self.principals.insert(run.clone(), principal.into());
        self.interactions.insert(run.clone(), interaction_id.into());
        self.windows.insert(run, window);
    }

    pub(super) fn window(&self, run: &str) -> Option<&Window> {
        self.windows.get(run)
    }

    pub(super) fn fingerprint_runs(&self, input: &Window, principal: &str) -> Vec<String> {
        let mut runs = Vec::new();
        let mut seen = HashSet::new();
        for hash in input.unit_hash_hexes() {
            for run in self.last_hashes.get(&hash).into_iter().flatten() {
                if self.principals.get(run).map(String::as_str) == Some(principal)
                    && seen.insert(run.clone())
                {
                    runs.push(run.clone());
                }
            }
        }
        runs
    }

    pub(super) fn interaction(&self, run: &str) -> Option<&str> {
        self.interactions.get(run).map(String::as_str)
    }

    pub(super) fn pending_runs(&self, tool_id: &str, principal: &str) -> Vec<(String, String)> {
        self.pending_tools
            .get(tool_id)
            .into_iter()
            .flatten()
            .filter(|run| self.principals.get(*run).map(String::as_str) == Some(principal))
            .filter_map(|run| {
                self.interactions
                    .get(run)
                    .map(|interaction| (run.clone(), interaction.clone()))
            })
            .collect()
    }

    pub(super) fn associate_loaded(
        input: Option<&Window>,
        candidates: &[(String, String, &Window)],
    ) -> RunEvent {
        let result =
            |status: &str, source: Option<&(String, String)>, count, units, bytes, start| {
                RunEvent::RetainedTailAssociated {
                    source_run_id: source.map(|s| s.0.clone()),
                    source_interaction_id: source.map(|s| s.1.clone()),
                    status: status.into(),
                    candidate_count: count,
                    matched_units: units,
                    matched_bytes: bytes,
                    input_start: start,
                }
            };
        let Some(input) = input else {
            return result("resource_limit", None, 0, 0, 0, None);
        };
        if candidates.len() > MAX_CANDIDATES {
            return result("resource_limit", None, 0, 0, 0, None);
        }
        let mut positions: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
        for (index, unit) in input.units.iter().enumerate() {
            positions.entry(unit.hash).or_default().push(index);
        }
        let mut accepted = Vec::new();
        let mut comparisons = 0;
        let mut verified_bytes = 0;
        for source in candidates {
            let old = source.2;
            let Some(last) = old.units.last() else {
                continue;
            };
            let mut best = None;
            for &end in positions.get(&last.hash).into_iter().flatten() {
                let mut length = 0;
                while length < old.units.len() && length <= end {
                    comparisons += 1;
                    if comparisons > MAX_COMPARISONS {
                        return result("resource_limit", None, 0, 0, 0, None);
                    }
                    let left = &old.units[old.units.len() - 1 - length];
                    let right = &input.units[end - length];
                    if left.hash != right.hash {
                        break;
                    }
                    verified_bytes += left.bytes;
                    if verified_bytes > MAX_VERIFIED_BYTES {
                        return result("resource_limit", None, 0, 0, 0, None);
                    }
                    if left.value != right.value {
                        break;
                    }
                    length += 1;
                }
                if length == 0 {
                    continue;
                }
                let start = end + 1 - length;
                let mut bytes: usize = input.units[start..=end].iter().map(|unit| unit.bytes).sum();
                for begin in start..=end {
                    let units = end + 1 - begin;
                    if bytes < MIN_MATCH_BYTES || best.is_some_and(|(n, _, _)| units <= n) {
                        break;
                    }
                    comparisons += units;
                    if comparisons > MAX_COMPARISONS {
                        return result("resource_limit", None, 0, 0, 0, None);
                    }
                    if strong(&input.units[begin..=end], bytes) {
                        best = Some((units, bytes, begin));
                        break;
                    }
                    bytes -= input.units[begin].bytes;
                }
            }
            if let Some(best) = best {
                accepted.push((source, best));
            }
        }
        match accepted.as_slice() {
            [] => result("no_match", None, 0, 0, 0, None),
            [(source, (units, bytes, start))] => {
                let pair = (source.0.clone(), source.1.clone());
                result("inferred", Some(&pair), 1, *units, *bytes, Some(*start))
            }
            _ => {
                let Some((source, (units, bytes, start))) = accepted
                    .iter()
                    .max_by_key(|(_, (units, bytes, _))| (*units, *bytes))
                else {
                    return result("ambiguous", None, accepted.len(), 0, 0, None);
                };
                if accepted
                    .iter()
                    .filter(|(_, (match_units, match_bytes, _))| {
                        *match_units == *units && *match_bytes == *bytes
                    })
                    .count()
                    == 1
                {
                    let pair = (source.0.clone(), source.1.clone());
                    result("inferred", Some(&pair), 1, *units, *bytes, Some(*start))
                } else {
                    result("ambiguous", None, accepted.len(), 0, 0, None)
                }
            }
        }
    }
}

fn public_thinking_projection(value: &Value) -> bool {
    if value.get("role").and_then(Value::as_str) != Some("assistant")
        || value.pointer("/content/type").and_then(Value::as_str) != Some("reasoning")
        || !value
            .pointer("/content/encrypted_content")
            .is_some_and(Value::is_null)
    {
        return false;
    }
    // Marker carrier 是已交付的公开投影，不是隐藏 reasoning；仍精确比较全部预览与标记字节，
    // 不解析隐藏内容，也不允许仅凭 Marker 绕过完整交互、唯一候选及 Principal 隔离。
    ["/content/summary", "/content/content"]
        .into_iter()
        .filter_map(|path| value.pointer(path).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .any(|text| {
            text.contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                && !crate::history_marker::history_marker_references(&[AiItem::thinking(
                    text, None,
                )])
                .is_empty()
        })
}

fn private_control(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "reasoning"
                            | "thinking"
                            | "redacted_thinking"
                            | "compaction"
                            | "compaction_trigger"
                    )
                })
                || object
                    .keys()
                    .any(|key| matches!(key.as_str(), "encrypted_content" | "signature"))
                || object.values().any(private_control)
        }
        Value::Array(values) => values.iter().any(private_control),
        _ => false,
    }
}

fn strong(units: &[Unit], bytes: usize) -> bool {
    if units.len() < 2 || bytes < MIN_MATCH_BYTES {
        return false;
    }
    let mut user = false;
    let mut answer = false;
    let mut calls = HashSet::new();
    let mut resolved = HashSet::new();
    for unit in units {
        match unit.value.get("role").and_then(Value::as_str) {
            Some("user") => user = true,
            Some("assistant") => {
                if let Some(call) = unit.value.get("tool_call") {
                    let Some(id) = call.get("id").and_then(Value::as_str) else {
                        return false;
                    };
                    if !calls.insert(id) {
                        return false;
                    }
                } else if unit.value.pointer("/content/type").and_then(Value::as_str)
                    == Some("text")
                {
                    answer |= (user || !resolved.is_empty())
                        && unit
                            .value
                            .pointer("/content/text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.len() >= MIN_ANSWER_BYTES);
                }
            }
            Some("tool") => {
                let Some(id) = unit.value.get("tool_call_id").and_then(Value::as_str) else {
                    return false;
                };
                if !calls.contains(id) || !resolved.insert(id) {
                    return false;
                }
            }
            // Leading instructions may differ, but never erase internal control units.
            Some("system" | "developer") => return false,
            _ => return false,
        }
    }
    calls == resolved && ((user && answer) || (!calls.is_empty() && answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> AiItem {
        AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    #[test]
    fn received_prefix_proof_is_exact_and_requires_only_new_users() {
        let mut system = user("instructions");
        system.role = stravia_runtime_contract::protocol::ir::Role::System;
        let old_items = vec![
            system,
            user("task"),
            AiItem::thinking("private", None),
            AiItem::function_call_output("call", serde_json::json!("result")),
        ];
        let old = Window::capture_received_input(&old_items).unwrap();
        let mut replay = old_items.clone();
        replay.push(user("reminder"));
        assert!(old.is_received_prefix_with_only_new_users(
            &Window::capture_received_input(&replay).unwrap()
        ));
        assert!(!old.is_received_prefix_with_only_new_users(&old));
        for index in 0..old_items.len() {
            let mut changed = replay.clone();
            changed[index].content =
                stravia_runtime_contract::protocol::ir::MessageContent::Text("changed".into());
            assert!(!old.is_received_prefix_with_only_new_users(
                &Window::capture_received_input(&changed).unwrap()
            ));
        }
        replay.push(AiItem::output_text("new output"));
        assert!(!old.is_received_prefix_with_only_new_users(
            &Window::capture_received_input(&replay).unwrap()
        ));
    }

    #[test]
    fn received_prefix_proof_declines_truncation_and_instruction_overflow() {
        let mut items = vec![user("bounded"); MAX_UNITS - 1];
        items.push(AiItem::function_call_output(
            "call",
            serde_json::json!("result"),
        ));
        let old = Window::capture_received_input(&items).unwrap();
        let mut overflow = items.clone();
        overflow.push(user("reminder"));
        let truncated = Window::capture_received_input(&overflow).unwrap();
        assert!(!old.is_received_prefix_with_only_new_users(&truncated));
        assert!(truncated.received_items.is_none());

        let mut instructions = user(&"s".repeat(MAX_WINDOW_BYTES));
        instructions.role = stravia_runtime_contract::protocol::ir::Role::System;
        let result = AiItem::function_call_output("call", serde_json::json!("result"));
        let old =
            Window::capture_received_input(&[instructions.clone(), user("task"), result.clone()])
                .unwrap();
        let new =
            Window::capture_received_input(&[instructions, user("task"), result, user("reminder")])
                .unwrap();
        assert!(!old.is_received_prefix_with_only_new_users(&new));
    }

    #[test]
    fn received_inputs_share_tail_budget_and_expire_without_indexing_tool_sources() {
        let window = Window::capture_received_input(&[
            user("task"),
            AiItem::function_call_output("call", serde_json::json!("result")),
        ])
        .unwrap();
        let mut index = TailIndex::default();
        index.cache_received_input("received".into(), window.clone(), 10);
        assert!(index.received_input("received", 9).is_some());
        assert!(index.received_input("received", 10).is_none());
        assert!(index.fingerprint_runs(&window, "owner").is_empty());
        assert_eq!(index.bytes, window.retained_bytes());
        index.sweep(10);
        assert_eq!(index.bytes, 0);
    }

    #[test]
    fn new_received_input_evicts_oldest_equal_expiry_tail_under_shared_budget() {
        let window = Window::capture_received_input(&[
            user(&"x".repeat(400 * 1024)),
            AiItem::function_call_output("call", serde_json::json!("result")),
        ])
        .unwrap();
        let mut index = TailIndex::default();
        let count = MAX_INDEX_BYTES / window.retained_bytes();
        for entry in 0..count {
            index.insert(
                format!("tail-{entry}"),
                window.clone(),
                20,
                "owner",
                "interaction",
            );
        }
        assert!(index.window("tail-0").is_some());
        index.cache_received_input("new-receipt".into(), window.clone(), 20);
        assert!(index.received_input("new-receipt", 11).is_some());
        assert!(index.window("tail-0").is_none());
        assert!(index.window("tail-1").is_some());
        assert!(
            !index
                .fingerprint_runs(&window, "owner")
                .contains(&"tail-0".into())
        );
        assert!(index.bytes <= MAX_INDEX_BYTES);
    }

    /// Loads candidate windows through the index exactly as Run Attribution's
    /// discover step does, then runs the production matcher.
    fn associate(
        index: &TailIndex,
        input: Option<&Window>,
        candidates: &[(String, String)],
    ) -> RunEvent {
        let loaded: Vec<(String, String, &Window)> = candidates
            .iter()
            .filter_map(|(run, interaction)| {
                index
                    .window(run)
                    .map(|window| (run.clone(), interaction.clone(), window))
            })
            .collect();
        TailIndex::associate_loaded(input, &loaded)
    }

    fn association(thinking: AiItem, replayed_thinking: AiItem) -> RunEvent {
        let question = user("你好，你是什么模型");
        let answer = AiItem::output_text(
            "我是编程助手，可以帮助你阅读代码、运行命令、定位错误、处理文档，以及执行浏览器自动化和桌面操作。",
        );
        let mut old = Window::capture(std::slice::from_ref(&question)).unwrap();
        assert!(old.append(Window::capture(&[thinking, answer.clone()]).unwrap()));
        let input = Window::capture(&[
            question,
            replayed_thinking,
            answer,
            user("我当前是什么电脑"),
        ])
        .unwrap();
        let mut index = TailIndex::default();
        index.insert(
            "previous-run".into(),
            old,
            i64::MAX,
            "principal",
            "previous-interaction",
        );
        associate(
            &index,
            Some(&input),
            &[("previous-run".into(), "previous-interaction".into())],
        )
    }

    #[test]
    fn projected_thinking_preserves_complete_interaction_tail() {
        let reference = "abcdefghijklmnopqrstuvwxyzab";
        let projected = format!(
            "{}{}",
            crate::history_marker::render_preview_projection_span(reference, 0, "公开思考预览"),
            crate::history_marker::render_history_marker_reference(reference),
        );
        let thinking = AiItem::thinking(projected.clone(), None);
        assert!(matches!(
            association(thinking.clone(), thinking.clone()),
            RunEvent::RetainedTailAssociated {
                status,
                source_interaction_id: Some(source),
                ..
            } if status == "inferred" && source == "previous-interaction"
        ));
        for replay in [
            AiItem::thinking(projected.replace("公开思考预览", "修改后的预览"), None),
            AiItem::thinking(
                projected.replace(reference, "abcdefghijklmnopqrstuvwxyzac"),
                None,
            ),
        ] {
            assert!(matches!(
                association(thinking.clone(), replay),
                RunEvent::RetainedTailAssociated { status, .. } if status == "no_match"
            ));
        }
    }

    fn long_user(tag: &str) -> AiItem {
        user(&format!("{tag} {}", "用户问题内容。".repeat(8)))
    }

    fn long_answer(tag: &str) -> AiItem {
        AiItem::output_text(format!("{tag} {}", "助手回答内容。".repeat(8)))
    }

    fn nested_sources() -> (TailIndex, Window, Window) {
        let first_user = long_user("first");
        let first_answer = long_answer("glm");
        let switch_user = long_user("switch");
        let switch_answer = long_answer("gpt");
        let resume_user = long_user("resume");
        let mut first = Window::capture(std::slice::from_ref(&first_user)).unwrap();
        assert!(first.append(Window::capture(std::slice::from_ref(&first_answer)).unwrap()));
        let mut switched = Window::capture(&[
            first_user.clone(),
            first_answer.clone(),
            switch_user.clone(),
        ])
        .unwrap();
        assert!(switched.append(Window::capture(std::slice::from_ref(&switch_answer)).unwrap()));
        let with_gpt = Window::capture(&[
            first_user.clone(),
            first_answer.clone(),
            switch_user,
            switch_answer,
            resume_user.clone(),
        ])
        .unwrap();
        let without_gpt = Window::capture(&[first_user, first_answer, resume_user]).unwrap();
        let mut index = TailIndex::default();
        index.insert(
            "run-first".into(),
            first,
            i64::MAX,
            "principal",
            "glm-first",
        );
        index.insert(
            "run-switch".into(),
            switched,
            i64::MAX,
            "principal",
            "gpt-switch",
        );
        (index, with_gpt, without_gpt)
    }

    fn candidates() -> Vec<(String, String)> {
        vec![
            ("run-first".into(), "glm-first".into()),
            ("run-switch".into(), "gpt-switch".into()),
        ]
    }

    #[test]
    fn unique_longest_nested_source_is_the_later_turn() {
        let (index, with_gpt, _) = nested_sources();
        assert!(matches!(
            associate(&index, Some(&with_gpt), &candidates()),
            RunEvent::RetainedTailAssociated {
                status,
                source_interaction_id: Some(source),
                ..
            } if status == "inferred" && source == "gpt-switch"
        ));
    }

    #[test]
    fn omitting_the_switched_turn_keeps_the_earlier_source() {
        let (index, _, without_gpt) = nested_sources();
        assert!(matches!(
            associate(&index, Some(&without_gpt), &candidates()),
            RunEvent::RetainedTailAssociated {
                status,
                source_interaction_id: Some(source),
                ..
            } if status == "inferred" && source == "glm-first"
        ));
    }

    #[test]
    fn equal_length_independent_sources_stay_ambiguous() {
        let first_user = long_user("first");
        let first_answer = long_answer("glm");
        let resume_user = long_user("resume");
        let mut first = Window::capture(std::slice::from_ref(&first_user)).unwrap();
        assert!(first.append(Window::capture(std::slice::from_ref(&first_answer)).unwrap()));
        let duplicate = Window::capture(&[first_user.clone(), first_answer.clone()]).unwrap();
        let input = Window::capture(&[first_user, first_answer, resume_user]).unwrap();
        let mut index = TailIndex::default();
        index.insert(
            "run-a".into(),
            first,
            i64::MAX,
            "principal",
            "interaction-a",
        );
        index.insert(
            "run-b".into(),
            duplicate,
            i64::MAX,
            "principal",
            "interaction-b",
        );
        assert!(matches!(
            associate(
                &index,
                Some(&input),
                &[
                    ("run-a".into(), "interaction-a".into()),
                    ("run-b".into(), "interaction-b".into()),
                ]
            ),
            RunEvent::RetainedTailAssociated { status, candidate_count: 2, .. }
                if status == "ambiguous"
        ));
    }

    #[test]
    fn private_thinking_remains_an_unmatchable_boundary() {
        for thinking in [
            AiItem::thinking("未投影的原始思考", None),
            AiItem::thinking(
                crate::history_marker::render_history_marker_reference(
                    "abcdefghijklmnopqrstuvwxyzab",
                ),
                Some("protected-signature".into()),
            ),
            AiItem::thinking("<!--sh:invalid-->", None),
        ] {
            assert!(matches!(
                association(thinking.clone(), thinking),
                RunEvent::RetainedTailAssociated { status, .. } if status == "no_match"
            ));
        }
    }

    fn bulky(tag: &str, bytes: usize) -> AiItem {
        user(&format!("{tag} {}", "a".repeat(bytes)))
    }

    #[test]
    fn oversized_received_history_keeps_newest_suffix() {
        let prefix = bulky("old", 280_000);
        let question = long_user("keep");
        let answer = long_answer("keep");
        let captured = Window::capture(&[
            prefix.clone(),
            bulky("older", 280_000),
            question.clone(),
            answer.clone(),
        ])
        .unwrap();
        let expected = Window::capture(&[question, answer]).unwrap();
        assert_eq!(captured.last_hash_hex(), expected.last_hash_hex());
        assert!(captured.units.len() < 4);
        assert!(captured.units.len() >= expected.units.len());
        assert!(Window::capture(&[bulky("too-big", MAX_WINDOW_BYTES)]).is_none());
    }

    #[test]
    fn unit_count_overflow_keeps_the_last_max_units() {
        let items: Vec<_> = (0..=MAX_UNITS)
            .map(|index| user(&format!("turn-{index}")))
            .collect();
        let captured = Window::capture(&items).unwrap();
        assert_eq!(captured.units.len(), MAX_UNITS);
        assert_eq!(
            captured.last_hash_hex(),
            Window::capture(std::slice::from_ref(items.last().unwrap()))
                .unwrap()
                .last_hash_hex()
        );
    }

    #[test]
    fn append_drops_oldest_units_to_keep_delivered_output() {
        let items: Vec<_> = (0..MAX_UNITS)
            .map(|index| user(&format!("prefix-{index}")))
            .collect();
        let mut window = Window::capture(&items).unwrap();
        assert_eq!(window.units.len(), MAX_UNITS);
        let output = Window::capture(&[long_answer("out")]).unwrap();
        let out_hash = output.last_hash_hex();
        assert!(window.append(output));
        assert_eq!(window.last_hash_hex(), out_hash);
        assert_eq!(window.units.len(), MAX_UNITS);
    }

    #[test]
    fn compacted_resume_matches_source_indexed_from_oversized_history() {
        let prefix = bulky("dropped", 280_000);
        let question = long_user("retained");
        let answer = long_answer("retained");
        let mut source = Window::capture(&[
            prefix,
            bulky("also-dropped", 280_000),
            question.clone(),
            answer.clone(),
        ])
        .unwrap();
        assert!(source.append(Window::capture(&[long_answer("final")]).unwrap()));
        let input = Window::capture(&[
            long_user("summary of earlier turns"),
            question,
            answer,
            long_answer("final"),
            long_user("resume after compaction"),
        ])
        .unwrap();
        let mut index = TailIndex::default();
        index.insert(
            "source-run".into(),
            source,
            i64::MAX,
            "principal",
            "source-interaction",
        );
        assert!(matches!(
            associate(
                &index,
                Some(&input),
                &[("source-run".into(), "source-interaction".into())]
            ),
            RunEvent::RetainedTailAssociated {
                status,
                source_interaction_id: Some(source),
                ..
            } if status == "inferred" && source == "source-interaction"
        ));
    }

    #[test]
    fn pending_tools_survive_suffix_capture() {
        let call = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call-tail".into(),
            name: "probe".into(),
            arguments: "{}".into(),
        });
        let window = Window::capture(&[
            bulky("old", 280_000),
            bulky("older", 280_000),
            long_user("tool"),
            call,
        ])
        .unwrap();
        assert_eq!(
            window.pending_tool_ids().as_deref(),
            Some(["call-tail".to_owned()].as_slice())
        );
    }
}
