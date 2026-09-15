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

#[derive(Clone)]
pub(super) struct Window {
    units: Vec<Unit>,
    bytes: usize,
}
#[derive(Clone)]
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
                if private_control(&value) && !public_thinking_projection(&value) {
                    // A nonmatching boundary preserves continuity without retaining private state.
                    value = serde_json::json!({"diagnostic_boundary": stravia_runtime_contract::identifier::new_id()});
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
    interactions: HashMap<String, String>,
    principals: HashMap<String, String>,
    last_hashes: HashMap<String, Vec<String>>,
    pending_tools: HashMap<String, Vec<String>>,
    expiries: HashMap<String, i64>,
    bytes: usize,
}
impl TailIndex {
    pub(super) fn sweep(&mut self, now: i64) {
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
        self.bytes = self.windows.values().map(|window| window.bytes).sum();
    }
    pub(super) fn insert(
        &mut self,
        run: String,
        window: Window,
        expires_at: i64,
        principal: impl Into<String>,
        interaction_id: impl Into<String>,
    ) {
        if self.windows.contains_key(&run) || self.bytes + window.bytes > MAX_INDEX_BYTES {
            return;
        }
        self.bytes += window.bytes;
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

    pub(super) fn current_tool_source(
        &self,
        input: &Window,
        principal: &str,
    ) -> Option<(String, String)> {
        let tail_ids = input.current_tail_tool_ids()?;
        let mut source = None;
        for id in &tail_ids {
            let Some(runs) = self.pending_tools.get(id) else {
                return None;
            };
            let matches: Vec<_> = runs
                .iter()
                .filter(|run| self.principals.get(*run).map(String::as_str) == Some(principal))
                .collect();
            if matches.len() != 1 {
                return None;
            }
            let run = matches[0];
            match &source {
                None => source = Some(run.clone()),
                Some(existing) if existing != run => return None,
                Some(_) => {}
            }
        }
        let run = source?;
        let pending = self.windows.get(&run)?.pending_tool_ids()?;
        if !tail_ids.iter().all(|id| pending.iter().any(|p| p == id)) {
            return None;
        }
        Some((run.clone(), self.interactions.get(&run)?.clone()))
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

    #[cfg(test)]
    pub(super) fn associate(
        &self,
        input: Option<&Window>,
        candidates: &[(String, String)],
    ) -> RunEvent {
        if candidates
            .iter()
            .any(|(run, _)| !self.windows.contains_key(run))
        {
            return RunEvent::RetainedTailAssociated {
                source_run_id: None,
                source_interaction_id: None,
                status: "index_unavailable".into(),
                candidate_count: 0,
                matched_units: 0,
                matched_bytes: 0,
                input_start: None,
            };
        }
        let loaded: Vec<(String, String, &Window)> = candidates
            .iter()
            .filter_map(|(run, interaction)| {
                self.windows
                    .get(run)
                    .map(|window| (run.clone(), interaction.clone(), window))
            })
            .collect();
        Self::associate_loaded(input, &loaded)
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

    fn association(thinking: AiItem, replayed_thinking: AiItem) -> RunEvent {
        let question = user("你好，你是什么模型");
        let answer = AiItem::output_text(
            "我是编程助手，可以帮助你阅读代码、运行命令、定位错误、处理文档，以及执行浏览器自动化和桌面操作。",
        );
        let mut old = Window::capture(&[question.clone()]).unwrap();
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
        index.associate(
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
        let mut first = Window::capture(&[first_user.clone()]).unwrap();
        assert!(first.append(Window::capture(&[first_answer.clone()]).unwrap()));
        let mut switched = Window::capture(&[
            first_user.clone(),
            first_answer.clone(),
            switch_user.clone(),
        ])
        .unwrap();
        assert!(switched.append(Window::capture(&[switch_answer.clone()]).unwrap()));
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
            index.associate(Some(&with_gpt), &candidates()),
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
            index.associate(Some(&without_gpt), &candidates()),
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
        let mut first = Window::capture(&[first_user.clone()]).unwrap();
        assert!(first.append(Window::capture(&[first_answer.clone()]).unwrap()));
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
            index.associate(
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

    #[test]
    fn current_tool_source_requires_unique_pending_ids() {
        let question = long_user("tool");
        let call = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call-1".into(),
            name: "probe".into(),
            arguments: "{}".into(),
        });
        let result: AiItem = serde_json::from_value(serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": long_answer("result").content.to_text(),
        }))
        .unwrap();
        let mut pending = Window::capture(&[question.clone()]).unwrap();
        assert!(pending.append(Window::capture(&[call.clone()]).unwrap()));
        let input = Window::capture(&[question, call, result, long_user("after")]).unwrap();
        let mut index = TailIndex::default();
        index.insert(
            "pending-run".into(),
            pending,
            i64::MAX,
            "principal",
            "pending-interaction",
        );
        assert_eq!(
            index.current_tool_source(&input, "principal"),
            Some(("pending-run".into(), "pending-interaction".into()))
        );
        assert_eq!(index.current_tool_source(&input, "other"), None);
    }
}
