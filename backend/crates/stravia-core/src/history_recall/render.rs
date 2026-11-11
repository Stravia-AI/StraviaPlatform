//! 历史回看的纯渲染：目录行、交互转录与工具文件。
//!
//! 转录只追加：每个事件的渲染只依赖事件自身的不可变内容、所属 Run 的准入事实，
//! 以及此前事件已确定的计数（工具编号、上一个 Run）。凭据替换保持换行数，
//! 内联预览的行数只由原文决定，因此映射集合变化不会移动已发布的行号。

use std::{borrow::Cow, collections::HashMap};

use chrono::{DateTime, SecondsFormat};
use serde_json::Value;
use stravia_credential_protection::KnownSecrets;
use stravia_web_access_contract::read_path::HISTORY_ROOT;

use crate::interaction_observation::{
    ObservationEvent, RecallChain, RecallInteraction, RecallMember, RecallTool,
};

const INLINE_LINES: usize = 20;
const INLINE_BYTES: usize = 2 * 1024;
const PREVIEW_CHARS: usize = 80;

/// 带 `Z` 的 UTC 秒精度时间；毫秒值超出 chrono 范围时原样输出数字。
pub(super) fn timestamp(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |at| at.to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

pub(super) fn plural(count: u64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn model_name(model: Option<&str>) -> &str {
    model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or("unknown model")
}

/// 单行字段（工具名、状态、失败摘要）不允许换行改变行数。
fn single_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 先替换凭据，再取第一条非空行并截到 80 个字符；结果按 JSON 字符串加引号。
pub(super) fn preview(text: Option<&str>, secrets: &KnownSecrets) -> String {
    let replaced = text.map(|text| secrets.replace(text));
    let line = replaced
        .as_deref()
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()));
    let Some(line) = line else {
        return "(no text)".to_owned();
    };
    let mut preview: String = line.chars().take(PREVIEW_CHARS).collect();
    if line.chars().nth(PREVIEW_CHARS).is_some() {
        preview.push('…');
    }
    Value::String(preview).to_string()
}

pub(super) fn root_header(total: u64, retention_days: u32) -> String {
    format!(
        "# {HISTORY_ROOT} · {} · newest activity first · retention {}",
        plural(total, "chain", "chains"),
        plural(u64::from(retention_days), "day", "days"),
    )
}

pub(super) fn chain_line(chain: &RecallChain, preview: &str) -> String {
    format!(
        "- {} {HISTORY_ROOT}{}/ · {} · {} · {} · {preview}",
        timestamp(chain.last_active_at),
        chain.root_id,
        plural(chain.interactions, "interaction", "interactions"),
        single_line(&chain.status),
        single_line(model_name(chain.model.as_deref())),
    )
}

pub(super) fn chain_header(root_id: &str, total: u64, started_at: i64) -> String {
    format!(
        "# {HISTORY_ROOT}{root_id}/ · {} · started {}",
        plural(total, "interaction", "interactions"),
        timestamp(started_at),
    )
}

/// `previous` 是上一行（含上一页最后一行）的 Interaction；父 Interaction 不是上一行时才标分叉。
pub(super) fn member_line(
    root_id: &str,
    member: &RecallMember,
    previous: Option<&str>,
    preview: &str,
) -> String {
    let mut line = format!(
        "- {} {HISTORY_ROOT}{root_id}/{} · {} · {} · {preview}",
        timestamp(member.started_at),
        member.id,
        single_line(&member.status),
        single_line(model_name(member.model.as_deref())),
    );
    if let Some(parent) = member.parent_id.as_deref()
        && previous != Some(parent)
    {
        line.push_str(" · branch from ");
        line.push_str(parent);
        if !member.parent_listed {
            line.push_str(" (not listed)");
        }
    }
    line
}

/// 把 JSON 值中所有字符串（含对象键）的已知秘密替换为凭据标记。
fn redact_value(secrets: &KnownSecrets, value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(replaced) = secrets.replace(text) {
                *text = replaced;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(secrets, item);
            }
        }
        Value::Object(map) => {
            for (key, mut item) in std::mem::take(map) {
                redact_value(secrets, &mut item);
                map.insert(secrets.replace(&key).into_owned(), item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// 渲染替换后的值；只需要完整内容时不另外渲染原文。
fn redacted(value: &Value, secrets: &KnownSecrets, render: impl Fn(&Value) -> String) -> String {
    if secrets.is_empty() {
        return render(value);
    }
    let mut redacted = value.clone();
    redact_value(secrets, &mut redacted);
    render(&redacted)
}

fn redacted_lines(
    value: &Value,
    secrets: &KnownSecrets,
    render: impl Fn(&Value) -> String,
) -> Vec<String> {
    lines(&redacted(value, secrets, render))
}

/// 内联预览要同时用到原文与替换后文本：行数按原文计算，展示替换后的行。
/// `redacted` 为 `None` 表示替换没有改变文本。
struct Text {
    raw: String,
    redacted: Option<String>,
}

impl Text {
    fn plain(text: &str, secrets: &KnownSecrets) -> Self {
        Self {
            raw: text.to_owned(),
            redacted: match secrets.replace(text) {
                Cow::Owned(replaced) => Some(replaced),
                Cow::Borrowed(_) => None,
            },
        }
    }

    fn render(value: &Value, secrets: &KnownSecrets, render: impl Fn(&Value) -> String) -> Self {
        Self {
            raw: render(value),
            redacted: (!secrets.is_empty()).then(|| redacted(value, secrets, &render)),
        }
    }

    /// 前 20 行且不超过 2 KiB；第一行自身超长时截到 2 KiB。返回的布尔值表示是否截断。
    fn inline_lines(&self) -> (Vec<String>, bool) {
        let raw = lines(&self.raw);
        let mut count = 0;
        let mut bytes = 0;
        let mut cut_first = false;
        for line in raw.iter().take(INLINE_LINES) {
            if bytes + line.len() + 1 > INLINE_BYTES {
                if count == 0 {
                    count = 1;
                    cut_first = true;
                }
                break;
            }
            bytes += line.len() + 1;
            count += 1;
        }
        let mut shown: Vec<String> = lines(self.redacted.as_deref().unwrap_or(&self.raw))
            .into_iter()
            .take(count)
            .collect();
        if cut_first && let Some(first) = shown.first_mut() {
            let mut end = INLINE_BYTES.min(first.len());
            while !first.is_char_boundary(end) {
                end -= 1;
            }
            first.truncate(end);
        }
        (shown, cut_first || count < raw.len())
    }
}

/// 按 LF 切行；去掉末尾空行，空文本写作 `(no text)`。
fn lines(text: &str) -> Vec<String> {
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() {
        return vec!["(no text)".to_owned()];
    }
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

fn is_part(value: &Value) -> bool {
    value.get("type").is_some_and(Value::is_string)
}

/// 规范内容部分：文本取原文，媒体写占位行，思考不出现，其余部分写紧凑 JSON。
fn part_text(part: &Value) -> Option<String> {
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    if matches!(kind, "text" | "input_text" | "output_text")
        && let Some(text) = part.get("text").and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    if matches!(kind, "thinking" | "reasoning") {
        return None;
    }
    for media in ["image", "audio", "video", "file"] {
        if kind.contains(media) {
            return Some(format!("[{media}]"));
        }
    }
    Some(part.to_string())
}

fn content_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(parts) if !parts.is_empty() && parts.iter().all(is_part) => parts
            .iter()
            .filter_map(part_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) if is_part(value) => part_text(value).unwrap_or_default(),
        other => format!("{other:#}"),
    }
}

const NO_ARGUMENTS: &str = "(no arguments)";

enum Arguments<'a> {
    Missing,
    Plain(&'a str),
    Json(Cow<'a, Value>),
}

/// 工具参数：字符串里是 JSON 对象或数组时按 JSON 渲染，否则按纯文本。
fn parse_arguments(input: Option<&Value>) -> Arguments<'_> {
    match input {
        None | Some(Value::Null) => Arguments::Missing,
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => {
                Arguments::Json(Cow::Owned(parsed))
            }
            _ => Arguments::Plain(text),
        },
        Some(value) => Arguments::Json(Cow::Borrowed(value)),
    }
}

/// 转录内联预览用紧凑 JSON。
fn arguments_preview(input: Option<&Value>, secrets: &KnownSecrets) -> Text {
    match parse_arguments(input) {
        Arguments::Missing => Text::plain(NO_ARGUMENTS, secrets),
        Arguments::Plain(text) => Text::plain(text, secrets),
        Arguments::Json(value) => Text::render(&value, secrets, Value::to_string),
    }
}

/// 工具文件给出美化后的完整参数。
fn argument_lines(input: Option<&Value>, secrets: &KnownSecrets) -> Vec<String> {
    match parse_arguments(input) {
        Arguments::Missing => vec![NO_ARGUMENTS.to_owned()],
        Arguments::Plain(text) => lines(&secrets.replace(text)),
        Arguments::Json(value) => redacted_lines(&value, secrets, |value| format!("{value:#}")),
    }
}

fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// 用于建索引的未脱敏文本；字节区间相对于 `text`。
pub(crate) struct IndexableEventText {
    pub text: String,
    pub weight: i32,
}

fn visible_answer(payload: &Value) -> String {
    let text = str_field(payload, "text").unwrap_or_default();
    let placeholders: Vec<String> = payload
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| {
            !matches!(
                part.get("type").and_then(Value::as_str),
                Some("text" | "input_text" | "output_text")
            )
        })
        .filter_map(part_text)
        .collect();
    let mut result = if text.is_empty() && !placeholders.is_empty() {
        Vec::new()
    } else {
        lines(text)
    };
    result.extend(placeholders);
    result.join("\n")
}

/// 提取转录或工具文件渲染使用的未脱敏内容。
/// 工具名与完整参数之间以换行分隔，字节区间相对于拼接后的事件文本。
pub(crate) fn indexable_event_text(event: &ObservationEvent) -> Option<IndexableEventText> {
    let payload = &event.payload;
    let (text, weight) = match event.kind.as_str() {
        "user_input_recorded" => (
            lines(str_field(payload, "text").unwrap_or_default()).join("\n"),
            3,
        ),
        "client_visible_content"
            if payload.get("complete").and_then(Value::as_bool) == Some(true)
                && str_field(payload, "channel") == Some("visible") =>
        {
            (visible_answer(payload), 2)
        }
        "client_tool_handoff" | "platform_tool_started" => {
            let input = payload.get("input");
            let arguments = match parse_arguments(input) {
                Arguments::Missing => NO_ARGUMENTS.to_owned(),
                Arguments::Plain(text) => text.to_owned(),
                Arguments::Json(value) => format!("{value:#}"),
            };
            if arguments.contains("stravia://history/") {
                return None;
            }
            let name = single_line(str_field(payload, "name").unwrap_or("unknown tool"));
            (format!("{name}\n{}", lines(&arguments).join("\n")), 1)
        }
        _ => return None,
    };
    Some(IndexableEventText { text, weight })
}

/// 还原 `indexable_event_text` 写入段的角色权重；检索结果按它标注命中角色。
pub(crate) fn role_name(weight: i32) -> &'static str {
    match weight {
        3 => "user",
        2 => "assistant",
        _ => "tool call",
    }
}

/// 转录跨事件的已确定计数；游标续读时从持久化的工具编号与上一个 Run 恢复。
#[derive(Debug, Default, Clone)]
pub(super) struct TranscriptState {
    tools: HashMap<String, u32>,
    allocated: u32,
    run: Option<String>,
}

impl TranscriptState {
    /// `tool_ids` 是续读事件之前每个工具开始事件的 tool_id，按序号升序；缺失的 id 仍占编号。
    pub(super) fn resume(tool_ids: Vec<Option<String>>, run: Option<String>) -> Self {
        let allocated = u32::try_from(tool_ids.len()).unwrap_or(u32::MAX);
        let tools = tool_ids
            .into_iter()
            .zip(1..)
            .filter_map(|(tool_id, number)| Some((tool_id?, number)))
            .collect::<HashMap<String, u32>>();
        Self {
            tools,
            allocated,
            run,
        }
    }

    pub(super) fn allocated(&self) -> u32 {
        self.allocated
    }

    pub(super) fn run(&self) -> Option<&str> {
        self.run.as_deref()
    }
}

#[derive(Clone, Copy)]
pub(super) struct Transcript<'a> {
    interaction: &'a RecallInteraction,
    secrets: &'a KnownSecrets,
}

impl<'a> Transcript<'a> {
    pub(super) fn new(interaction: &'a RecallInteraction, secrets: &'a KnownSecrets) -> Self {
        Self {
            interaction,
            secrets,
        }
    }

    fn address(&self) -> String {
        format!(
            "{HISTORY_ROOT}{}/{}",
            self.interaction.root_id, self.interaction.id
        )
    }

    /// 父 Interaction 被清理后 `parent` 行写 `none`，行数不变。
    pub(super) fn header(&self) -> Vec<String> {
        let interaction = self.interaction;
        let parent = interaction.parent_id.as_deref().map_or_else(
            || "none".to_owned(),
            |parent| format!("{HISTORY_ROOT}{}/{parent}", interaction.root_id),
        );
        vec![
            format!("# Interaction {}", interaction.id),
            format!("- chain: {HISTORY_ROOT}{}/", interaction.root_id),
            format!("- parent: {parent}"),
            format!(
                "- started: {} · protocol: {}",
                timestamp(interaction.started_at),
                single_line(&interaction.protocol),
            ),
            String::new(),
        ]
    }

    /// 渲染一个事件；不渲染的事件（未完成的可见块、其余 kind）返回空且不改变状态。
    pub(super) fn event(
        &self,
        state: &mut TranscriptState,
        event: &ObservationEvent,
    ) -> Vec<String> {
        let payload = &event.payload;
        let mut body = Vec::new();
        match event.kind.as_str() {
            "user_input_recorded" => {
                body.push("## user".to_owned());
                body.extend(lines(
                    &self
                        .secrets
                        .replace(str_field(payload, "text").unwrap_or_default()),
                ));
            }
            "client_visible_content" => {
                if payload.get("complete").and_then(Value::as_bool) != Some(true) {
                    return Vec::new();
                }
                body.push("## assistant".to_owned());
                body.extend(redacted_lines(payload, self.secrets, visible_answer));
            }
            "client_tool_handoff" | "platform_tool_started" => {
                state.allocated = state.allocated.saturating_add(1);
                let number = state.allocated;
                if let Some(tool_id) = str_field(payload, "tool_id") {
                    state.tools.insert(tool_id.to_owned(), number);
                }
                let side = if event.kind == "client_tool_handoff" {
                    "client"
                } else {
                    "platform"
                };
                let name = self
                    .secrets
                    .replace(str_field(payload, "name").unwrap_or("unknown tool"))
                    .into_owned();
                body.push(format!(
                    "### tool call {number} · {} ({side})",
                    single_line(&name)
                ));
                self.inline(
                    &mut body,
                    arguments_preview(payload.get("input"), self.secrets),
                    Some(number),
                );
            }
            "client_tool_result" => {
                for result in payload
                    .get("results")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let status = if result.get("is_error").and_then(Value::as_bool) == Some(true) {
                        "error"
                    } else {
                        "ok"
                    };
                    let number = str_field(result, "tool_id")
                        .and_then(|tool_id| state.tools.get(tool_id))
                        .copied();
                    self.result(
                        &mut body,
                        number,
                        status,
                        result.get("content").unwrap_or(&Value::Null),
                    );
                }
            }
            "platform_tool_finished" => {
                let number = str_field(payload, "tool_id")
                    .and_then(|tool_id| state.tools.get(tool_id))
                    .copied();
                let status = str_field(payload, "status").unwrap_or("finished");
                self.result(
                    &mut body,
                    number,
                    &single_line(status),
                    payload.get("content").unwrap_or(&Value::Null),
                );
            }
            "observation_gap" => {
                let reason = self
                    .secrets
                    .replace(str_field(payload, "reason").unwrap_or("unknown"))
                    .into_owned();
                body.push(format!("> observation gap: {}", single_line(&reason)));
            }
            "request_failed" => {
                let error = payload.get("error").unwrap_or(&Value::Null);
                let summary = str_field(error, "message")
                    .or_else(|| str_field(error, "code"))
                    .unwrap_or("unknown failure");
                let mut summary = single_line(&self.secrets.replace(summary));
                if let (Some(code), Some(_)) =
                    (str_field(error, "code"), str_field(error, "message"))
                {
                    summary.push_str(&format!(" ({})", single_line(code)));
                }
                body.push(format!("> run failed: {summary}"));
            }
            _ => return Vec::new(),
        }
        let mut lines = Vec::with_capacity(body.len() + 1);
        if let Some(run_id) = event.run_id.as_deref()
            && state.run.as_deref() != Some(run_id)
        {
            let run = self.interaction.runs.get(run_id);
            lines.push(format!(
                "--- run · {} · {}",
                single_line(model_name(run.and_then(|run| run.model.as_deref()))),
                timestamp(run.map_or(event.occurred_at, |run| run.started_at)),
            ));
            state.run = Some(run_id.to_owned());
        }
        lines.extend(body);
        lines
    }

    fn result(&self, body: &mut Vec<String>, number: Option<u32>, status: &str, content: &Value) {
        match number {
            Some(number) => body.push(format!("### tool result {number} · {status}")),
            None => body.push(format!("### tool result (unmatched call) · {status}")),
        }
        self.inline(
            body,
            Text::render(content, self.secrets, content_text),
            number,
        );
    }

    /// 内联预览后总是给出工具文件路径；无法对应调用的结果没有工具文件。
    fn inline(&self, body: &mut Vec<String>, text: Text, number: Option<u32>) {
        let (shown, truncated) = text.inline_lines();
        body.extend(shown);
        if truncated {
            body.push("… (truncated)".to_owned());
        }
        if let Some(number) = number {
            body.push(format!("(full: {}/tools/{number})", self.address()));
        }
    }
}

/// 工具文件：头部、完整参数，再按出现顺序追加每个结果段。
pub(super) fn tool_file(
    interaction: &RecallInteraction,
    index: u32,
    tool: &RecallTool,
    secrets: &KnownSecrets,
) -> Vec<String> {
    let call = &tool.call;
    let side = if call.kind == "client_tool_handoff" {
        "client"
    } else {
        "platform"
    };
    let name = secrets
        .replace(str_field(&call.payload, "name").unwrap_or("unknown tool"))
        .into_owned();
    let mut lines = vec![
        format!("# Tool call {index} · {} · {side}", single_line(&name)),
        format!(
            "- interaction: {HISTORY_ROOT}{}/{}",
            interaction.root_id, interaction.id
        ),
        format!("- started: {}", timestamp(call.occurred_at)),
        String::new(),
        "## arguments".to_owned(),
    ];
    lines.extend(argument_lines(call.payload.get("input"), secrets));
    let tool_id = str_field(&call.payload, "tool_id");
    let mut ordinal = 0;
    for event in &tool.results {
        let segments: Vec<(String, String, &Value)> = match event.kind.as_str() {
            "platform_tool_finished" => {
                let status = str_field(&event.payload, "status").unwrap_or("finished");
                let duration = event
                    .payload
                    .get("duration_ms")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(|| event.occurred_at.saturating_sub(call.occurred_at));
                vec![(
                    single_line(status),
                    format!("{duration} ms"),
                    event.payload.get("content").unwrap_or(&Value::Null),
                )]
            }
            "client_tool_result" => event
                .payload
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|result| tool_id.is_some() && str_field(result, "tool_id") == tool_id)
                .map(|result| {
                    let status = if result.get("is_error").and_then(Value::as_bool) == Some(true) {
                        "error"
                    } else {
                        "ok"
                    };
                    (
                        status.to_owned(),
                        format!("{} ms", event.occurred_at.saturating_sub(call.occurred_at)),
                        result.get("content").unwrap_or(&Value::Null),
                    )
                })
                .collect(),
            _ => Vec::new(),
        };
        for (status, duration, content) in segments {
            ordinal += 1;
            lines.push(String::new());
            lines.push(format!("## result {ordinal} · {status} · {duration}"));
            lines.extend(redacted_lines(content, secrets, content_text));
        }
    }
    lines
}

#[cfg(test)]
mod indexable_tests {
    use super::indexable_event_text;
    use crate::interaction_observation::ObservationEvent;
    use serde_json::{Value, json};

    fn event(kind: &str, payload: Value) -> ObservationEvent {
        ObservationEvent {
            sequence: 1,
            occurred_at: 0,
            interaction_id: None,
            run_id: None,
            rejection_id: None,
            kind: kind.to_owned(),
            payload,
        }
    }

    #[test]
    fn indexes_only_visible_content_and_tool_calls() {
        let user = indexable_event_text(&event(
            "user_input_recorded",
            json!({"text": "first\r\nsecond\n"}),
        ))
        .unwrap();
        assert_eq!((user.text.as_str(), user.weight), ("first\nsecond", 3));
        let answer = indexable_event_text(&event(
            "client_visible_content",
            json!({
                "complete": true, "channel": "visible", "text": "answer", "parts": [
                    {"type": "thinking", "text": "private"}, {"type": "image"}
                ]
            }),
        ))
        .unwrap();
        assert_eq!(
            (answer.text.as_str(), answer.weight),
            ("answer\n[image]", 2)
        );
        assert!(
            indexable_event_text(&event(
                "client_visible_content",
                json!({"complete": false, "text": "partial"})
            ))
            .is_none()
        );
        assert!(
            indexable_event_text(&event(
                "client_visible_content",
                json!({"complete": true, "channel": "thinking", "text": "private"})
            ))
            .is_none()
        );
        assert!(
            indexable_event_text(&event(
                "platform_tool_finished",
                json!({"content": "result"})
            ))
            .is_none()
        );
        let tool = indexable_event_text(&event(
            "client_tool_handoff",
            json!({"name": "lookup", "input": "{\"query\":\"alpha\"}"}),
        ))
        .unwrap();
        assert_eq!(
            (tool.text.as_str(), tool.weight),
            ("lookup\n{\n  \"query\": \"alpha\"\n}", 1)
        );
    }

    #[test]
    fn excludes_history_calls_in_either_tool_kind() {
        for kind in ["client_tool_handoff", "platform_tool_started"] {
            for input in [
                json!({"path": "stravia://history/chain"}),
                json!("stravia://history/"),
            ] {
                assert!(
                    indexable_event_text(&event(kind, json!({"name": "read", "input": input})))
                        .is_none()
                );
            }
        }
    }
}
