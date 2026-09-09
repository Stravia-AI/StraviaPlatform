use super::RunEvent;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
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

pub(super) struct Window {
    units: Vec<Unit>,
    bytes: usize,
}
struct Unit {
    value: Value,
    hash: [u8; 32],
    bytes: usize,
}

impl Window {
    pub(super) fn capture(items: &[AiItem]) -> Option<Self> {
        if items.len() > MAX_UNITS {
            return None;
        }
        // Bound serialization before constructing semantic projections; no payload reaches SQL.
        struct Bound(usize);
        impl std::io::Write for Bound {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .filter(|n| *n <= MAX_WINDOW_BYTES)
                    .ok_or_else(|| std::io::Error::other("diagnostic window limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(&mut Bound(0), items).ok()?;
        let mut units = Vec::new();
        let mut bytes = 0;
        for item in items.iter().skip_while(|item| {
            matches!(
                item.role,
                stravia_runtime_contract::protocol::ir::Role::System
                    | stravia_runtime_contract::protocol::ir::Role::Developer
            )
        }) {
            let Value::Array(values) = canonical::item_value(item) else {
                return None;
            };
            for mut value in values {
                if private_control(&value) {
                    // A nonmatching boundary preserves continuity without retaining private state.
                    value = serde_json::json!({"diagnostic_boundary": uuid::Uuid::new_v4().to_string()});
                }
                let encoded = serde_json::to_vec(&value).ok()?;
                bytes += encoded.len();
                if bytes > MAX_WINDOW_BYTES || units.len() == MAX_UNITS {
                    return None;
                }
                units.push(Unit {
                    hash: Sha256::digest(&encoded).into(),
                    bytes: encoded.len(),
                    value,
                });
            }
        }
        Some(Self { units, bytes })
    }
    pub(super) fn append(&mut self, mut output: Self) -> bool {
        if self.bytes + output.bytes > MAX_WINDOW_BYTES
            || self.units.len() + output.units.len() > MAX_UNITS
        {
            return false;
        }
        self.bytes += output.bytes;
        self.units.append(&mut output.units);
        true
    }
}

#[derive(Default)]
pub(super) struct TailIndex {
    windows: HashMap<String, Window>,
    expiries: HashMap<String, i64>,
    bytes: usize,
}
impl TailIndex {
    pub(super) fn sweep(&mut self, now: i64) {
        self.expiries.retain(|_, expiry| *expiry > now);
        self.windows.retain(|id, _| self.expiries.contains_key(id));
        self.bytes = self.windows.values().map(|window| window.bytes).sum();
    }
    pub(super) fn insert(&mut self, run: String, window: Window, expires_at: i64) {
        if self.windows.contains_key(&run)
            || self.windows.len() >= MAX_CANDIDATES
            || self.bytes + window.bytes > MAX_INDEX_BYTES
        {
            return;
        }
        self.bytes += window.bytes;
        self.expiries.insert(run.clone(), expires_at);
        self.windows.insert(run, window);
    }
    pub(super) fn associate(
        &self,
        input: Option<&Window>,
        candidates: &[(String, String)],
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
        if candidates
            .iter()
            .any(|(run, _)| !self.windows.contains_key(run))
        {
            return result("index_unavailable", None, 0, 0, 0, None);
        }
        let mut positions: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
        for (index, unit) in input.units.iter().enumerate() {
            positions.entry(unit.hash).or_default().push(index);
        }
        let mut accepted = Vec::new();
        let mut comparisons = 0;
        let mut verified_bytes = 0;
        for source in candidates {
            let old = &self.windows[&source.0];
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
                result("inferred", Some(source), 1, *units, *bytes, Some(*start))
            }
            _ => result("ambiguous", None, accepted.len(), 0, 0, None),
        }
    }
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
