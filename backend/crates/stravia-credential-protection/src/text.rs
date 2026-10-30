//! The readable canonical surface. Protocol identities and opaque payloads never enter it.
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use serde_json::Value;

use super::{RedactionError, marker, store::Mapping};
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, AiResponse, ContentBlock, CustomTool, DocumentSource, EmbeddingInput,
    MessageContent, NamespaceMember, NativeTool, ResponseFormat, TOOL_RESULT_CONTENT_KIND_META,
    ToolResultContentKind, ToolSpec,
};

/// 一段可读文本在请求中的位置：来源类别加块标识。
///
/// 只描述“在哪里”，不含文本、偏移或片段；块标识是客户端已知的工具调用 ID。
/// 偏移相对于被扫描文本，而请求在保护前已被历史还原改写，无法映射回客户端条目。
#[derive(Clone, Copy, Debug)]
pub(super) struct Origin<'a> {
    pub source: &'static str,
    /// 仅 `tool_arguments` 与 `tool_result` 携带。
    pub tool_id: Option<&'a str>,
    /// 仅 `user_message` 有意义。`None` 表示无法判定（例如供应商原始副本）。
    pub latest_input: Option<bool>,
    /// 本次请求新提交的内容；为假表示客户端重放的历史，或无法与历史区分的整请求副本。
    pub fresh: bool,
}

impl<'a> Origin<'a> {
    pub(super) const fn of(source: &'static str) -> Self {
        Self {
            source,
            tool_id: None,
            latest_input: None,
            fresh: true,
        }
    }

    /// 同一来源保留块标识；切换来源则丢弃，避免把一个工具的标识带到别处。
    pub(super) fn with_source(self, source: &'static str) -> Origin<'a> {
        Origin {
            source,
            tool_id: if self.source == source {
                self.tool_id
            } else {
                None
            },
            latest_input: self.latest_input,
            fresh: self.fresh,
        }
    }

    pub(super) fn with_tool<'b>(self, source: &'static str, id: &'b str) -> Origin<'b> {
        Origin {
            source,
            tool_id: Some(id),
            latest_input: self.latest_input,
            fresh: self.fresh,
        }
    }

    pub(super) fn with_tool_if_known<'b>(
        self,
        source: &'static str,
        id: Option<&'b str>,
    ) -> Origin<'b> {
        match id {
            Some(id) => self.with_tool(source, id),
            None => Origin {
                source,
                tool_id: None,
                latest_input: self.latest_input,
                fresh: self.fresh,
            },
        }
    }

    pub(super) fn with_latest_input(self, latest: bool) -> Origin<'a> {
        Origin {
            latest_input: Some(latest),
            ..self
        }
    }

    pub(super) fn with_fresh(self, fresh: bool) -> Origin<'a> {
        Origin { fresh, ..self }
    }

    /// 遍历回调里的块标识只在回调内有效；检测之后才用到它，所以转成自有数据保存。
    fn to_owned(self) -> TextOrigin {
        TextOrigin {
            source: self.source,
            tool_id: self.tool_id.map(str::to_owned),
            latest_input: self.latest_input,
            fresh: self.fresh,
        }
    }
}

/// [`Origin`] 的自有形式，与 `request_text_origins` 返回的文本一一对应。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TextOrigin {
    pub source: &'static str,
    pub tool_id: Option<String>,
    pub latest_input: Option<bool>,
    pub fresh: bool,
}

pub(super) fn active(mapping: &Mapping) -> bool {
    mapping.expires_at > chrono::Utc::now().timestamp_millis()
        && marker::valid_reference(&mapping.reference)
}

/// Single pass over original bytes: inserted secrets/references are never scanned again.
pub(super) fn replace<'a>(
    text: &'a str,
    mappings: &[Mapping],
    restore: bool,
    used: &mut BTreeSet<String>,
) -> Cow<'a, str> {
    if text.is_empty() || mappings.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut candidates: Vec<_> = mappings
        .iter()
        // Outbound protection uses the loaded snapshot even if it expires during
        // traversal. redact_request rejects expired used mappings before dispatch.
        .filter(|m| (!restore || active(m)) && !m.secret.is_empty())
        .collect();
    candidates.sort_by(|a, b| {
        let a_key = if restore { &a.reference } else { &a.secret };
        let b_key = if restore { &b.reference } else { &b.secret };
        b_key
            .len()
            .cmp(&a_key.len())
            .then_with(|| a_key.cmp(b_key))
            .then_with(|| a.reference.cmp(&b.reference))
    });
    if candidates.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut result = String::new();
    let mut copied = 0;
    let mut position = 0;
    while position < text.len() {
        // References are protocol atoms. Neither a broad detector nor a known
        // secret matching their identifier may nest another reference inside one.
        if !restore && let Some(reference) = marker::reference_prefix(&text[position..]) {
            position += reference.len();
            continue;
        }
        if let Some(mapping) = candidates
            .iter()
            .find(|m| text[position..].starts_with(if restore { &m.reference } else { &m.secret }))
        {
            let (from, to) = if restore {
                (&mapping.reference, &mapping.secret)
            } else {
                (&mapping.secret, &mapping.reference)
            };
            result.push_str(&text[copied..position]);
            result.push_str(to);
            position += from.len();
            copied = position;
            used.insert(mapping.reference.clone());
        } else {
            let ch = text[position..].chars().next().expect("character boundary");
            position += ch.len_utf8();
        }
    }
    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        result.push_str(&text[copied..]);
        Cow::Owned(result)
    }
}

type Visitor<'a> =
    dyn for<'text> FnMut(&'text str, Option<&str>) -> Result<Cow<'text, str>, RedactionError> + 'a;

#[derive(serde::Deserialize)]
struct DecodedString<'a>(#[serde(borrow)] Cow<'a, str>);

fn decode_string(raw: &str) -> Result<Cow<'_, str>, RedactionError> {
    serde_json::from_str::<DecodedString<'_>>(raw)
        .map(|decoded| decoded.0)
        .map_err(|_| RedactionError::InvalidText)
}

fn decode_unterminated(content: &str) -> Result<Cow<'_, str>, RedactionError> {
    if !content.contains('\\') {
        if content.bytes().any(|byte| byte < 0x20) {
            return Err(RedactionError::InvalidText);
        }
        return Ok(Cow::Borrowed(content));
    }
    serde_json::from_str::<String>(&format!("\"{content}\""))
        .map(Cow::Owned)
        .map_err(|_| RedactionError::InvalidText)
}

fn json_values(
    value: &mut Value,
    context: Option<&str>,
    visit: &mut Visitor<'_>,
) -> Result<(), RedactionError> {
    match value {
        Value::String(text) => {
            if let Cow::Owned(replaced) = visit(text, context)? {
                *text = replaced;
            }
        }
        Value::Array(values) => {
            for value in values {
                json_values(value, context, visit)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                json_values(value, Some(key), visit)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn arguments_text(arguments: &mut String, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    if arguments.is_empty() {
        return Ok(());
    }
    if serde_json::from_str::<serde::de::IgnoredAny>(arguments).is_err() {
        // 结构未闭合的入参：闭合且可解码的字符串 token 仍走解码-替换-重编码
        // 语义，字符串之外的字节只做字面扫描；转义损坏的 token 与无法解码
        // 的尾部可能藏有转义形式凭据或引用，任何方向都 fail-closed。
        return malformed_arguments_text(arguments, visit);
    }
    let bytes = arguments.as_bytes();
    let mut out = String::new();
    let mut cursor = 0;
    let mut copied = 0;
    let mut context: Option<Cow<'_, str>> = None;
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\\' => cursor += 2,
                b'"' => {
                    cursor += 1;
                    break;
                }
                _ => cursor += 1,
            }
        }
        let raw = &arguments[start..cursor];
        let decoded = decode_string(raw)?;
        let after = arguments[cursor..].trim_start();
        if after.starts_with(':') {
            context = Some(decoded);
            continue;
        }
        if let Cow::Owned(replaced) = visit(&decoded, context.as_deref())? {
            out.push_str(&arguments[copied..start]);
            out.push_str(
                &serde_json::to_string(&replaced).map_err(|_| RedactionError::InvalidText)?,
            );
            copied = cursor;
        }
    }
    if copied != 0 {
        out.push_str(&arguments[copied..]);
        *arguments = out;
    }
    Ok(())
}

type ClosedStringToken<'a> = (usize, usize, Option<Cow<'a, str>>);

// 完整闭合的字符串 token：返回（含引号的字节区间, 解码值——转义损坏时为
// None，不凭空解码），以及第一个未闭合字符串的开引号位置；其后的字节
// 一律按字符串残段处理。
fn closed_string_tokens(raw: &str) -> (Vec<ClosedStringToken<'_>>, Option<usize>) {
    let bytes = raw.as_bytes();
    let mut tokens = Vec::new();
    let mut unterminated = None;
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\\' => cursor += 2,
                b'"' => {
                    cursor += 1;
                    break;
                }
                _ => cursor += 1,
            }
        }
        let end = cursor.min(bytes.len());
        let token = &raw[start..end];
        if token.len() < 2 || !token.ends_with('"') {
            unterminated = Some(start);
            break;
        }
        tokens.push((start, end, decode_string(token).ok()));
    }
    (tokens, unterminated)
}

fn malformed_arguments_text(
    arguments: &mut String,
    visit: &mut Visitor<'_>,
) -> Result<(), RedactionError> {
    let (tokens, unterminated_from) = closed_string_tokens(arguments);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut residuals: Vec<(usize, usize)> = Vec::new();
    let mut context: Option<Cow<'_, str>> = None;
    let mut last = 0;
    for (start, end, decoded) in tokens {
        if last < start {
            residuals.push((last, start));
        }
        last = end;
        // 闭合但转义损坏的 token 可能藏有转义形式凭据或引用，fail-closed。
        let Some(decoded) = decoded else {
            return Err(RedactionError::InvalidText);
        };
        if arguments[end..].trim_start().starts_with(':') {
            context = Some(decoded);
            continue;
        }
        if let Cow::Owned(replaced) = visit(&decoded, context.as_deref())? {
            edits.push((
                start,
                end,
                serde_json::to_string(&replaced).map_err(|_| RedactionError::InvalidText)?,
            ));
        }
    }
    if let Some(open) = unterminated_from {
        // 开引号本身属于结构字节；引号之后是未闭合的字符串内容。
        residuals.push((last, open + 1));
        let content = &arguments[open + 1..];
        // 转义序列完整则内容可判定解码；否则该段语义不可保证，fail-closed。
        let decoded = decode_unterminated(content)?;
        if let Cow::Owned(replaced) = visit(&decoded, context.as_deref())? {
            let encoded =
                serde_json::to_string(&replaced).map_err(|_| RedactionError::InvalidText)?;
            edits.push((
                open + 1,
                arguments.len(),
                encoded[1..encoded.len() - 1].to_string(),
            ));
        }
    } else {
        residuals.push((last, arguments.len()));
    }
    for (start, end) in residuals {
        if start >= end {
            continue;
        }
        // 字符串之外的残段没有转义语义，字面扫描与拼接即为原始字节。
        if let Cow::Owned(after) = visit(&arguments[start..end], None)? {
            edits.push((start, end, after));
        }
    }
    if edits.is_empty() {
        return Ok(());
    }
    edits.sort_by_key(|(start, _, _)| *start);
    let mut out = String::with_capacity(arguments.len());
    let mut pos = 0;
    for (start, end, replacement) in edits {
        out.push_str(&arguments[pos..start]);
        out.push_str(&replacement);
        pos = end;
    }
    out.push_str(&arguments[pos..]);
    *arguments = out;
    Ok(())
}

fn content_result_type(kind: Option<&str>) -> bool {
    kind.is_some_and(|kind| {
        matches!(
            kind,
            "text"
                | "input_text"
                | "output_text"
                | "image"
                | "image_url"
                | "input_image"
                | "audio"
                | "input_audio"
                | "video"
                | "file"
                | "input_file"
                | "redacted_thinking"
                | "thinking"
                | "reasoning"
                | "document"
                | "search_result"
        )
    })
}

fn compound_array(value: &Value) -> bool {
    value.as_array().is_some_and(|values| {
        values
            .iter()
            .any(|value| value.is_object() || value.is_array())
    })
}

fn encoded_compound_array(text: &str) -> bool {
    text.trim_start().starts_with('[')
        && serde_json::from_str::<Value>(text).is_ok_and(|value| compound_array(&value))
}

fn result_kind(
    value: &Value,
    kind: Option<ToolResultContentKind>,
    reject_ambiguous: bool,
) -> Result<Option<ToolResultContentKind>, RedactionError> {
    if let Some(kind) = kind {
        return Ok(Some(kind));
    }
    // Old compound arrays can be business JSON or protocol blocks. Their
    // contents cannot prove which interpretation the original producer intended.
    if compound_array(value) || value.as_str().is_some_and(encoded_compound_array) {
        // Protection rejects before egress. Restoration leaves opaque legacy
        // payloads untouched instead of making disabled requests fail.
        return if reject_ambiguous {
            Err(RedactionError::AmbiguousToolResult)
        } else {
            Ok(None)
        };
    }
    Ok(Some(ToolResultContentKind::Json))
}

fn tool_result(
    value: &mut Value,
    kind: Option<ToolResultContentKind>,
    reject_ambiguous: bool,
    visit: &mut Visitor<'_>,
) -> Result<(), RedactionError> {
    match result_kind(value, kind, reject_ambiguous)? {
        Some(ToolResultContentKind::Json) => json_values(value, None, visit),
        Some(ToolResultContentKind::ContentBlocks) => {
            write_surface::wire_text(value, Origin::of("tool_result"), &mut |text, context, _| {
                visit(text, context)
            })
        }
        None => Ok(()),
    }
}

fn encoded_tool_result(
    text: &mut String,
    kind: ToolResultContentKind,
    visit: &mut Visitor<'_>,
) -> Result<(), RedactionError> {
    if let Some(replaced) = encoded_tool_result_replacement(text, kind, visit)? {
        *text = replaced;
    }
    Ok(())
}

fn encoded_tool_result_replacement(
    text: &str,
    kind: ToolResultContentKind,
    visit: &mut Visitor<'_>,
) -> Result<Option<String>, RedactionError> {
    let mut value: Value = serde_json::from_str(text).map_err(|_| RedactionError::InvalidText)?;
    let mut changed = false;
    tool_result(&mut value, Some(kind), true, &mut |text, context| {
        let replaced = visit(text, context)?;
        changed |= matches!(replaced, Cow::Owned(_));
        Ok(replaced)
    })?;
    if changed {
        return serde_json::to_string(&value)
            .map(Some)
            .map_err(|_| RedactionError::InvalidText);
    }
    Ok(None)
}

// Both borrows expand from one field-selection definition. Collection never clones
// media, vendor metadata, opaque reasoning, or the request itself.
macro_rules! readable_surface {
    ($module:ident, $json:ident, $arguments:ident, $result:ident, $encoded:ident, $iter:ident, $get:ident, $values:ident, $slice:ident, $shared_text:path, $shared_encoded:path, $apply:path, $text_type:ty, $($qualifier:tt)*) => {
        mod $module {
            use super::*;
            fn json_values(value: & $($qualifier)* Value, context: Option<&str>, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$json(value, context, &mut |text, context| visit(text, context, location))
}
fn arguments_text(value: $text_type, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$arguments(value, &mut |text, context| visit(text, context, location.with_source("tool_arguments")))
}
fn tool_result(value: & $($qualifier)* Value, kind: Option<ToolResultContentKind>, reject: bool, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$result(value, kind, reject, &mut |text, context| visit(text, context, location.with_source("tool_result")))
}
fn encoded_tool_result(value: $text_type, kind: ToolResultContentKind, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$encoded(value, kind, &mut |text, context| visit(text, context, location.with_source("tool_result")))
}
            pub(super) type Visitor<'a> = dyn for<'text> FnMut(&'text str, Option<&str>, Origin<'_>) -> Result<Cow<'text, str>, RedactionError> + 'a;
fn apply(text: $text_type, context: Option<&str>, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    $apply(text, context, location, visit)
}
pub(super) fn schema(value: & $($qualifier)* Value, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    match value {
        Value::Object(values) => for (key, value) in values {
            match key.as_str() {
                "description" | "title" | "$comment" | "default" | "examples" | "example" | "const" | "enum" => json_values(value, Some(key), location, visit)?,
                "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => if let Value::Object(entries) = value { for entry in entries.$values() { schema(entry, location, visit)?; } },
                _ => if value.is_object() || value.is_array() { schema(value, location, visit)?; },
            }
        },
        Value::Array(values) => for value in values { schema(value, location, visit)?; },
        _ => {}
    }
    Ok(())
}


pub(super) fn wire_text(value: & $($qualifier)* Value, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    match value {
        Value::Array(values) => for value in values { wire_text(value, location, visit)?; },
        Value::Object(values) => {
            let kind = values.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "image" | "image_url" | "input_image" | "audio" | "input_audio" | "video" | "file" | "input_file" | "redacted_thinking" | "compaction" | "compaction_trigger") { return Ok(()); }
            let document = kind == "document";
            let search = kind == "search_result";
            let embedded_resource = kind == "resource";
            let tool_input = matches!(kind, "tool_use" | "server_tool_use");
            let location = if tool_input { location.with_source("tool_arguments") } else if matches!(kind, "tool_result" | "server_tool_result" | "codeExecutionResult") { location.with_source("tool_result") } else { match values.get("role").and_then(Value::as_str) { Some("user") => location.with_source("user_message"), Some("tool") => location.with_source("tool_result"), Some("assistant" | "system" | "developer") => location.with_source("system_or_history"), _ => location } };
            if search && let Some(Value::String(source)) = values.$get("source") { apply(source, None, location, visit)?; }
            if document && let Some(source) = values.$get("source") {
                match source.get("type").and_then(Value::as_str) {
                    Some("text" | "plain_text") => if let Some(Value::String(data)) = source.$get("data") { apply(data, None, location, visit)?; },
                    Some("content" | "blocks") => if let Some(content) = source.$get("content") { wire_text(content, location, visit)?; },
                    _ => {}
                }
            }
            for (key, value) in values {
                match key.as_str() {
                    "text" | "refusal" | "thinking" | "cited_text" | "title" | "description" | "code" | "stdout" | "stderr" | "query" | "queries" | "prompt" => json_values(value, Some(key), location, visit)?,
                    "arguments" => if let Value::String(arguments) = value { arguments_text(arguments, location, visit)?; } else { json_values(value, None, location.with_source("tool_arguments"), visit)?; },
                    "input" if tool_input => json_values(value, None, location, visit)?,
                    "input" | "output" | "variables" | "json" => json_values(value, None, location, visit)?,
                    "functionCall" => if let Some(arguments) = value.$get("args") { json_values(arguments, None, location.with_source("tool_arguments"), visit)?; },
                    "functionResponse" => if let Some(result) = value.$get("response") { json_values(result, None, location.with_source("tool_result"), visit)?; },
                    "resource" if embedded_resource => if let Some(Value::String(text)) = value.$get("text") { apply(text, None, location, visit)?; },
                    "format" | "json_schema" | "parts" | "functionDeclarations" | "executableCode" | "codeExecutionResult" | "citations" => wire_text(value, location, visit)?,
                    "content" | "summary" | "results" | "context" => if let Value::String(text) = value { apply(text, Some(key), location, visit)?; } else { wire_text(value, location, visit)?; },
                    "schema" | "parameters" | "input_schema" | "responseSchema" | "responseJsonSchema" => schema(value, location, visit)?,
                    "input_examples" => json_values(value, None, location, visit)?,
                    _ => {}
                }
            }
        },
        Value::String(text) => apply(text, None, location, visit)?,
        _ => {}
    }
    Ok(())
}


pub(super) fn blocks(blocks: & $($qualifier)* [ContentBlock], reject_ambiguous: bool, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => $shared_text(text, None, location, visit)?,
            ContentBlock::Thinking { thinking, .. } => apply(thinking, None, location, visit)?,
            ContentBlock::Reasoning { summary, content, .. } => for text in summary.$iter().chain(content) { apply(text, None, location, visit)?; },
            ContentBlock::ToolUse { id, input, .. } | ContentBlock::ServerToolUse { id, input, .. } => json_values(input, None, location.with_tool("tool_arguments", id), visit)?,
            ContentBlock::ToolResult { tool_use_id, content, content_kind, .. } | ContentBlock::ServerToolResult { tool_use_id, content, content_kind, .. } => tool_result(content, *content_kind, reject_ambiguous, location.with_tool("tool_result", tool_use_id), visit)?,
            ContentBlock::Document { source, title, context, .. } => {
                for text in title.$iter().chain(context) { apply(text, None, location, visit)?; }
                match source { DocumentSource::PlainText { data } => apply(data, None, location, visit)?, DocumentSource::Blocks { content } => self::blocks(content, reject_ambiguous, location, visit)?, _ => {} }
            }
            ContentBlock::SearchResult { content, title, source, .. } => { apply(title, None, location, visit)?; apply(source, None, location, visit)?; self::blocks(content, reject_ambiguous, location, visit)?; }
            ContentBlock::Citation { cited_text, .. } => apply(cited_text, None, location, visit)?,
            ContentBlock::ExecutableCode { code, .. } => apply(code, None, location, visit)?,
            ContentBlock::CodeExecutionResult { stdout, stderr, .. } => { let location = location.with_source("tool_result"); apply(stdout, None, location, visit)?; apply(stderr, None, location, visit)?; }
            ContentBlock::Refusal { refusal } => apply(refusal, None, location, visit)?,
            ContentBlock::Unknown { raw } => wire_text(raw, location, visit)?,
            ContentBlock::Image { .. } | ContentBlock::Audio { .. } | ContentBlock::File { .. } | ContentBlock::Video { .. } | ContentBlock::RedactedThinking { .. } | ContentBlock::Compaction { .. } | ContentBlock::CompactionTrigger {} | ContentBlock::ContainerUpload { .. } => {}
        }
    }
    Ok(())
}


pub(super) fn item_text(item: & $($qualifier)* AiItem, reject_ambiguous: bool, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    let location = match item.role {
        stravia_runtime_contract::protocol::ir::Role::User => location.with_source("user_message"),
        stravia_runtime_contract::protocol::ir::Role::Tool => location.with_tool_if_known("tool_result", item.tool_call_id.as_deref()),
        _ => location.with_source("system_or_history"),
    };
    let encoded_kind = if item.role == stravia_runtime_contract::protocol::ir::Role::Tool {
        item.meta.as_ref().and_then(|meta| meta.get(TOOL_RESULT_CONTENT_KIND_META))
        .map(|kind| serde_json::from_value::<ToolResultContentKind>(kind.clone()).map_err(|_| RedactionError::InvalidText))
        .transpose()?
    } else { None };
    match & $($qualifier)* item.content {
        MessageContent::Text(text) if item.role == stravia_runtime_contract::protocol::ir::Role::Tool => match encoded_kind {
            Some(ToolResultContentKind::ContentBlocks) => $shared_encoded(text, ToolResultContentKind::ContentBlocks, location, visit)?,
            Some(ToolResultContentKind::Json) => $shared_text(text, None, location, visit)?,
            None if encoded_compound_array(text) => if reject_ambiguous { return Err(RedactionError::AmbiguousToolResult); },
            None => $shared_text(text, None, location, visit)?,
        },
        MessageContent::Text(text) => $shared_text(text, None, location, visit)?,
        MessageContent::Blocks(content) if item.role == stravia_runtime_contract::protocol::ir::Role::Tool => for block in content {
            if let (Some(kind), ContentBlock::ToolResult { content: Value::String(text), .. }
                | ContentBlock::ServerToolResult { content: Value::String(text), .. }) = (encoded_kind, & $($qualifier)* *block) {
                match kind {
                    ToolResultContentKind::ContentBlocks => encoded_tool_result(text, kind, location, visit)?,
                    ToolResultContentKind::Json => apply(text, None, location, visit)?,
                }
            } else if let ContentBlock::Unknown { raw } = block {
                if content_result_type(raw.get("type").and_then(Value::as_str)) {
                    if reject_ambiguous { return Err(RedactionError::AmbiguousToolResult); }
                    continue;
                }
                tool_result(raw, None, reject_ambiguous, location, visit)?;
            }
            else { blocks(std::slice::$slice(block), reject_ambiguous, location, visit)?; }
        },
        MessageContent::Blocks(content) => blocks(content, reject_ambiguous, location, visit)?,
    }
    if let Some(calls) = & $($qualifier)* item.tool_calls { for call in calls { arguments_text(& $($qualifier)* call.arguments, location.with_tool("tool_arguments", &call.id), visit)?; } }
    Ok(())
}


pub(super) fn request(request: & $($qualifier)* AiRequest, reject_ambiguous: bool, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    // 本次提交的内容是最后一条 Assistant 之后的条目（新用户消息或工具结果）；更早的是客户端重放的历史。
    // 末尾若只剩 Assistant（预填），其前的条目才是本次输入，故边界在最后一条非 Assistant 条目之前找。
    let history_end = request.items.iter().rposition(|item| item.role != stravia_runtime_contract::protocol::ir::Role::Assistant)
        .and_then(|end| request.items[..end].iter().rposition(|item| item.role == stravia_runtime_contract::protocol::ir::Role::Assistant))
        .map(|boundary| boundary + 1);
    // 指令、工具定义、元数据和供应商整请求副本每轮都会重发，无法单独判定新旧：有历史就一律不算新输入。
    let location = location.with_fresh(history_end.is_none());
    if let Some(text) = & $($qualifier)* request.instructions { apply(text, None, location.with_source("system_or_history"), visit)?; }
    // 只有请求窗口内最后一条 User 条目才是本次输入；其余 User 条目是客户端重放的历史。
    let latest_input = request.items.iter().rposition(|item| item.role == stravia_runtime_contract::protocol::ir::Role::User);
    for (index, item) in (& $($qualifier)* request.items).into_iter().enumerate() {
        let origin = location.with_latest_input(Some(index) == latest_input).with_fresh(index >= history_end.unwrap_or(0));
        item_text(item, reject_ambiguous, origin, visit)?;
    }
    if let Some(embedding) = & $($qualifier)* request.embedding { match & $($qualifier)* embedding.input { EmbeddingInput::Text(text) => apply(text, None, location, visit)?, EmbeddingInput::Texts(texts) => for text in texts { apply(text, None, location, visit)?; }, _ => {} } }
    if let Some(tools) = & $($qualifier)* request.tools { for tool in tools { function_tool(tool, location, visit)?; } }
    if let Some(ResponseFormat::JsonSchema { schema: value, description, .. }) = & $($qualifier)* request.response_format {
        schema(value, location, visit)?;
        if let Some(description) = description { apply(description, None, location, visit)?; }
    }
    // Encoders prefer these fidelity-preserving copies over canonical fields.
    // Select only model-readable carriers, never the arbitrary vendor bag:
    // authentication, identities, media and opaque signatures must stay intact.
    for (key, value) in & $($qualifier)* request.meta.vendor.ingress {
        match key.as_str() {
            "__anthropic_raw_system" | "__google_raw_system_instruction" => wire_text(value, location.with_source("system_or_history"), visit)?,
            "__anthropic_raw_messages" | "__anthropic_raw_tools"
            | "__google_raw_tools" | "__google_generation_config"
            | "prediction" | "response_format" => wire_text(value, location, visit)?,
            _ => {}
        }
    }
    if let Some(metadata) = & $($qualifier)* request.parameters.metadata {
        for (key, text) in metadata { apply(text, Some(key), location, visit)?; }
    }
    for tool in & $($qualifier)* request.parameters.native_tools {
        match tool {
            NativeTool::ToolSearch(tool) => {
                if let Some(text) = & $($qualifier)* tool.description { apply(text, None, location, visit)?; }
                if let Some(parameters) = & $($qualifier)* tool.parameters { schema(parameters, location, visit)?; }
            }
            NativeTool::Custom(tool) => custom_tool(tool, location, visit)?,
            NativeTool::Namespace(namespace) => {
                if let Some(text) = & $($qualifier)* namespace.description { apply(text, None, location, visit)?; }
                for member in & $($qualifier)* namespace.tools {
                    match member {
                        NamespaceMember::Function(tool) => function_tool(tool, location, visit)?,
                        NamespaceMember::Custom(tool) => custom_tool(tool, location, visit)?,
                    }
                }
            }
            NativeTool::WebSearch(_) | NativeTool::CodeExecution(_) | NativeTool::ImageGeneration(_) => {}
        }
    }
    Ok(())
}

fn function_tool(tool: & $($qualifier)* ToolSpec, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    if let Some(text) = & $($qualifier)* tool.description { apply(text, None, location, visit)?; }
    schema(& $($qualifier)* tool.parameters, location, visit)
}

fn custom_tool(tool: & $($qualifier)* CustomTool, location: Origin<'_>, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    if let Some(text) = & $($qualifier)* tool.description { apply(text, None, location, visit)?; }
    if let Some(grammar) = & $($qualifier)* tool.grammar { apply(& $($qualifier)* grammar.definition, None, location, visit)?; }
    Ok(())
}

        }
    };
}

readable_surface!(
    read_surface,
    read_json_values,
    read_arguments,
    read_tool_result,
    read_encoded_tool_result,
    iter,
    get,
    values,
    from_ref,
    read_shared,
    read_shared_encoded,
    apply_read,
    &str,
);
readable_surface!(
    write_surface,
    json_values,
    arguments_text,
    tool_result,
    encoded_tool_result,
    iter_mut,
    get_mut,
    values_mut,
    from_mut,
    write_shared,
    write_shared_encoded,
    apply_write,
    &mut String,
    mut
);

fn apply_read(
    text: &str,
    context: Option<&str>,
    location: Origin<'_>,
    visit: &mut read_surface::Visitor<'_>,
) -> Result<(), RedactionError> {
    visit(text, context, location)?;
    Ok(())
}

fn apply_write(
    text: &mut String,
    context: Option<&str>,
    location: Origin<'_>,
    visit: &mut write_surface::Visitor<'_>,
) -> Result<(), RedactionError> {
    if let Cow::Owned(replaced) = visit(text, context, location)? {
        *text = replaced;
    }
    Ok(())
}

fn read_shared(
    text: &std::sync::Arc<String>,
    context: Option<&str>,
    location: Origin<'_>,
    visit: &mut read_surface::Visitor<'_>,
) -> Result<(), RedactionError> {
    apply_read(text, context, location, visit)
}

fn write_shared(
    text: &mut std::sync::Arc<String>,
    context: Option<&str>,
    location: Origin<'_>,
    visit: &mut write_surface::Visitor<'_>,
) -> Result<(), RedactionError> {
    if let Cow::Owned(replaced) = visit(text, context, location)? {
        set_shared(text, replaced);
    }
    Ok(())
}

fn set_shared(text: &mut std::sync::Arc<String>, replaced: String) {
    if let Some(text) = std::sync::Arc::get_mut(text) {
        *text = replaced;
    } else {
        *text = std::sync::Arc::new(replaced);
    }
}

fn read_shared_encoded(
    text: &std::sync::Arc<String>,
    kind: ToolResultContentKind,
    location: Origin<'_>,
    visit: &mut read_surface::Visitor<'_>,
) -> Result<(), RedactionError> {
    read_encoded_tool_result(text, kind, &mut |text, context| {
        visit(text, context, location.with_source("tool_result"))
    })
}

fn write_shared_encoded(
    text: &mut std::sync::Arc<String>,
    kind: ToolResultContentKind,
    location: Origin<'_>,
    visit: &mut write_surface::Visitor<'_>,
) -> Result<(), RedactionError> {
    if let Some(replaced) = encoded_tool_result_replacement(text, kind, &mut |text, context| {
        visit(text, context, location.with_source("tool_result"))
    })? {
        set_shared(text, replaced);
    }
    Ok(())
}

type ReadVisitor<'a> =
    dyn for<'text> FnMut(&'text str, Option<&str>) -> Result<Cow<'text, str>, RedactionError> + 'a;

fn append_context(
    output: &mut String,
    text: &str,
    context: Option<&str>,
) -> Result<(), RedactionError> {
    if let Some(key) = context {
        output.push_str(&serde_json::to_string(key).map_err(|_| RedactionError::InvalidText)?);
        output.push_str(" = ");
    }
    output.push_str(text);
    output.push('\n');
    Ok(())
}

fn decoded_json(
    value: &Value,
    context: Option<&str>,
    output: &mut String,
) -> Result<(), RedactionError> {
    match value {
        Value::String(text) => append_context(output, text, context)?,
        Value::Array(values) => {
            for value in values {
                decoded_json(value, context, output)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                decoded_json(value, Some(key), output)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// One input per JSON tree keeps sibling credentials together for composite rules.
/// Values remain decoded, so a detected secret is exactly the string later replaced.
fn read_json_values(
    value: &Value,
    context: Option<&str>,
    visit: &mut ReadVisitor<'_>,
) -> Result<(), RedactionError> {
    if let Value::String(text) = value {
        return visit(text, context).map(|_| ());
    }
    let mut readable = String::new();
    decoded_json(value, context, &mut readable)?;
    if !readable.is_empty() {
        visit(&readable, None)?;
    }
    Ok(())
}

fn read_arguments(arguments: &str, visit: &mut ReadVisitor<'_>) -> Result<(), RedactionError> {
    if arguments.is_empty() {
        return Ok(());
    }
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        // 检测必须完整覆盖：完整 token 解码后送检，其余字节原文送检；
        // 无法安全解码的转义区域可能藏有转义形式凭据，一律 fail-closed。
        let (tokens, unterminated_from) = closed_string_tokens(arguments);
        let mut readable = String::new();
        let mut last = 0;
        for (start, end, decoded) in tokens {
            readable.push_str(&arguments[last..start]);
            let Some(decoded) = decoded else {
                return Err(RedactionError::InvalidText);
            };
            readable.push_str(&decoded);
            last = end;
        }
        if let Some(open) = unterminated_from {
            readable.push_str(&arguments[last..open]);
            let content = &arguments[open + 1..];
            let decoded = decode_unterminated(content)?;
            readable.push_str(&decoded);
        } else {
            readable.push_str(&arguments[last..]);
        }
        return visit(&readable, None).map(|_| ());
    };
    read_json_values(&value, None, visit)
}

fn read_tool_result(
    value: &Value,
    kind: Option<ToolResultContentKind>,
    reject_ambiguous: bool,
    visit: &mut ReadVisitor<'_>,
) -> Result<(), RedactionError> {
    let mut readable = String::new();
    match result_kind(value, kind, reject_ambiguous)? {
        Some(ToolResultContentKind::Json) => {
            read_json_values(value, None, &mut |text, context| {
                append_context(&mut readable, text, context)?;
                Ok(Cow::Borrowed(text))
            })?
        }
        Some(ToolResultContentKind::ContentBlocks) => {
            read_surface::wire_text(value, Origin::of("tool_result"), &mut |text, context, _| {
                append_context(&mut readable, text, context)?;
                Ok(Cow::Borrowed(text))
            })?
        }
        None => {}
    }
    if !readable.is_empty() {
        visit(&readable, None)?;
    }
    Ok(())
}

fn read_encoded_tool_result(
    text: &str,
    kind: ToolResultContentKind,
    visit: &mut ReadVisitor<'_>,
) -> Result<(), RedactionError> {
    let value: Value = serde_json::from_str(text).map_err(|_| RedactionError::InvalidText)?;
    read_tool_result(&value, Some(kind), true, visit)
}

/// 检测与引用核对读到的全部文本，顺序与 [`request_text_origins`] 一致。
pub fn request_texts(
    value: &AiRequest,
    reject_ambiguous: bool,
) -> Result<Vec<String>, RedactionError> {
    collect_texts(value, reject_ambiguous, None)
}

/// 同 [`request_texts`]，并按同样的下标返回每段文本的来源；只有需要定位命中时才付出复制工具 ID 的代价。
pub fn request_text_origins(
    value: &AiRequest,
    reject_ambiguous: bool,
) -> Result<(Vec<String>, Vec<TextOrigin>), RedactionError> {
    let mut origins = Vec::new();
    let texts = collect_texts(value, reject_ambiguous, Some(&mut origins))?;
    Ok((texts, origins))
}

fn collect_texts(
    value: &AiRequest,
    reject_ambiguous: bool,
    mut origins: Option<&mut Vec<TextOrigin>>,
) -> Result<Vec<String>, RedactionError> {
    let mut texts = Vec::new();
    read_surface::request(
        value,
        reject_ambiguous,
        Origin::of("other_text"),
        &mut |text, context, origin| {
            if let Some(origins) = origins.as_deref_mut() {
                origins.push(origin.to_owned());
            }
            if let Some(key) = context {
                let mut readable = String::new();
                append_context(&mut readable, text, Some(key))?;
                texts.push(readable);
            } else {
                texts.push(text.to_owned());
            }
            Ok(Cow::Borrowed(text))
        },
    )?;
    Ok(texts)
}

/// 出站改写的结果。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Redacted {
    /// 请求里被替换过的全部引用。
    pub references: Vec<String>,
    /// 每个引用在哪些本次新提交的位置被替换；客户端重放的历史不在其中。
    pub placements: BTreeMap<String, BTreeSet<TextOrigin>>,
}

pub fn redact_request(
    value: &mut AiRequest,
    mappings: &[Mapping],
) -> Result<Redacted, RedactionError> {
    let mut used = BTreeSet::new();
    let mut placements: BTreeMap<String, BTreeSet<TextOrigin>> = BTreeMap::new();
    write_surface::request(
        value,
        true,
        Origin::of("other_text"),
        &mut |text, _, origin| {
            let mut here = BTreeSet::new();
            let replaced = replace(text, mappings, false, &mut here);
            if origin.fresh {
                for reference in &here {
                    placements
                        .entry(reference.clone())
                        .or_default()
                        .insert(origin.to_owned());
                }
            }
            used.extend(here);
            Ok(replaced)
        },
    )?;
    if mappings
        .iter()
        .any(|mapping| used.contains(&mapping.reference) && !active(mapping))
    {
        return Err(RedactionError::InvalidText);
    }
    Ok(Redacted {
        references: used.into_iter().collect(),
        placements,
    })
}

// 还原与出站保护共用同一遍历：转义损坏区域对引用同样不可判定（引用也可能
// 被上游 unicode 转义后藏入），故所有方向统一 fail-closed。
pub(super) fn restore_item(
    item: &mut AiItem,
    mappings: &[Mapping],
    used: &mut BTreeSet<String>,
) -> Result<(), RedactionError> {
    write_surface::item_text(item, false, Origin::of("other_text"), &mut |text, _, _| {
        Ok(replace(text, mappings, true, used))
    })
}

pub(super) fn restore_arguments(
    arguments: &mut String,
    mappings: &[Mapping],
    used: &mut BTreeSet<String>,
) -> Result<(), RedactionError> {
    arguments_text(arguments, &mut |text, _| {
        Ok(replace(text, mappings, true, used))
    })
}

pub(super) fn restore_unknown(
    raw: &mut String,
    mappings: &[Mapping],
    used: &mut BTreeSet<String>,
) -> Result<bool, RedactionError> {
    // Return whether the accumulator considers this a new canonical item.
    let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
        return Ok(false);
    };
    if value.get("__open_responses_event").is_some() {
        return Ok(false);
    }
    // Gemini emits response metadata between text deltas. Like the stream
    // accumulator, keep it outside canonical item boundaries: flushing here
    // would expose an unfinished reference instead of restoring its next delta.
    let response_metadata = value
        .get("__google_response_metadata")
        .is_some_and(Value::is_object);
    let mut changed = false;
    write_surface::wire_text(&mut value, Origin::of("other_text"), &mut |text, _, _| {
        let replaced = replace(text, mappings, true, used);
        changed |= matches!(replaced, Cow::Owned(_));
        Ok(replaced)
    })?;
    if changed {
        *raw = serde_json::to_string(&value).map_err(|_| RedactionError::InvalidText)?;
    }
    Ok(!response_metadata)
}

pub(super) fn restore_compaction(
    value: &mut stravia_runtime_contract::protocol::ir::NativeCompactionResponse,
    mappings: &[Mapping],
) -> Result<Vec<String>, RedactionError> {
    let mut used = BTreeSet::new();
    for item in &mut value.items {
        restore_item(item, mappings, &mut used)?;
    }
    if let Some(output) = value.wire.get_mut("output").and_then(Value::as_array_mut) {
        for item in output {
            write_surface::wire_text(item, Origin::of("other_text"), &mut |text, _, _| {
                Ok(replace(text, mappings, true, &mut used))
            })?;
        }
    }
    Ok(used.into_iter().collect())
}

/// 还原模型响应的结果。
pub(super) struct Restored {
    /// 响应里被还原过的全部引用。
    pub references: Vec<String>,
    /// 工具调用 ID 到该调用参数里被还原的引用。
    pub tool_calls: BTreeMap<String, BTreeSet<String>>,
}

pub(super) fn restore_response(
    value: &mut AiResponse,
    mappings: &[Mapping],
) -> Result<Restored, RedactionError> {
    let mut used = BTreeSet::new();
    let mut tool_calls: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for item in &mut value.items {
        write_surface::item_text(
            item,
            false,
            Origin::of("other_text"),
            &mut |text, _, origin| {
                let mut here = BTreeSet::new();
                let restored = replace(text, mappings, true, &mut here);
                if let (Some(id), "tool_arguments") = (origin.tool_id, origin.source) {
                    tool_calls
                        .entry(id.to_owned())
                        .or_default()
                        .extend(here.iter().cloned());
                }
                used.extend(here);
                Ok(restored)
            },
        )?;
    }
    Ok(Restored {
        references: used.into_iter().collect(),
        tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marker::{IDENTIFIER_LEN, PREFIX, SUFFIX};
    use stravia_runtime_contract::hook::{ContextCompleteness, ContextSnapshot};

    #[test]
    fn replacements_keep_longest_matches_unicode_and_inserted_values_literal() {
        let first = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "a".repeat(IDENTIFIER_LEN)),
            secret: "密钥-long".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let second = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "b".repeat(IDENTIFIER_LEN)),
            secret: "密钥".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let mappings = [second, first];
        let mut used = BTreeSet::new();
        let input = "开始 密钥-long / 密钥 结束";
        assert_eq!(
            replace(input, &mappings, false, &mut used),
            format!(
                "开始 {} / {} 结束",
                mappings[1].reference, mappings[0].reference
            ),
        );
        assert_eq!(used.len(), 2);
        let nested = [
            Mapping {
                secret: mappings[1].reference.clone(),
                ..mappings[0].clone()
            },
            mappings[1].clone(),
        ];
        assert_eq!(
            replace(&nested[0].reference, &nested, true, &mut BTreeSet::new()),
            nested[1].reference,
        );
    }

    #[test]
    fn untouched_argument_spelling_survives_valid_and_malformed_json() {
        let mappings = [Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "a".repeat(IDENTIFIER_LEN)),
            secret: "not-present".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        }];
        for original in [
            " { \"key\" : \"前缀\\u003c-no-reference\", \"number\": 1.00e+02 } ",
            " { \"key\" : \"前缀\\u003c-no-reference\", bare-tail",
            " { \"key\" : \"前缀\\u003c-no-reference",
        ] {
            let mut arguments = original.to_owned();
            let mut used = BTreeSet::new();
            restore_arguments(&mut arguments, &mappings, &mut used).unwrap();
            assert_eq!(arguments, original);
            assert!(used.is_empty());
        }
    }

    #[test]
    fn outbound_replacement_preserves_reference_atoms_when_identifiers_are_known_secrets() {
        let identifier = "abcdefghijklmnopqrstuvwxyzab";
        let reference = format!("{PREFIX}{identifier}{SUFFIX}");
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "b".repeat(IDENTIFIER_LEN)),
            secret: identifier.into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let mut used = BTreeSet::new();
        let input = format!("{reference} {identifier}");
        let replaced = replace(&input, std::slice::from_ref(&mapping), false, &mut used);
        assert_eq!(replaced, format!("{reference} {}", mapping.reference));
        assert_eq!(used, BTreeSet::from([mapping.reference.clone()]));
        assert_eq!(
            replace(&replaced, &[mapping], true, &mut BTreeSet::new()),
            format!("{reference} {identifier}")
        );
    }

    #[test]
    fn encoded_tool_media_stays_opaque_after_context_rebuilds() {
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "b".repeat(IDENTIFIER_LEN)),
            secret: "Q8n4Vk7sT2p9X5a3Lc6D0h1R".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let mut payload = serde_json::json!([
            {"type": "text", "text": mapping.secret},
            {"type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": mapping.secret
            }}
        ]);
        let mut item =
            AiItem::function_call_output("call-media", Value::String(payload.to_string()));
        item.meta = Some(
            stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                (TOOL_RESULT_CONTENT_KIND_META): "content_blocks"
            })),
        );
        let mut request = AiRequest::new("model", vec![item]);
        for _ in 0..2 {
            ContextSnapshot::from_request(&request, ContextCompleteness::Full)
                .write_to_request(&mut request);
        }
        redact_request(&mut request, std::slice::from_ref(&mapping)).unwrap();
        payload[0]["text"] = Value::String(mapping.reference.clone());
        let MessageContent::Blocks(blocks) = &request.items[0].content else {
            panic!("expected a rebuilt tool result");
        };
        let ContentBlock::ToolResult {
            content: Value::String(content),
            ..
        } = &blocks[0]
        else {
            panic!("expected the original encoded payload");
        };
        assert_eq!(serde_json::from_str::<Value>(content).unwrap(), payload);
    }

    #[test]
    fn legacy_encoded_tool_results_fail_closed_and_restore_without_guessing() {
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "c".repeat(IDENTIFIER_LEN)),
            secret: "Q8n4Vk7sT2p9X5a3Lc6D0h1R".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let payload = serde_json::json!([
            {"type": "text", "text": mapping.secret},
            {"type": "image", "source": {
                "type": "url", "url": format!("https://opaque.invalid/{}", mapping.reference)
            }}
        ])
        .to_string();
        let mut item = AiItem::function_call_output("call-legacy", Value::String(payload.clone()));
        item.meta = None;
        let mut request = AiRequest::new("model", vec![item]);
        for _ in 0..2 {
            assert!(matches!(
                redact_request(&mut request, std::slice::from_ref(&mapping)),
                Err(RedactionError::AmbiguousToolResult)
            ));
            restore_item(
                &mut request.items[0],
                std::slice::from_ref(&mapping),
                &mut BTreeSet::new(),
            )
            .unwrap();
            let actual = match &request.items[0].content {
                MessageContent::Text(text) => text.as_ref(),
                MessageContent::Blocks(blocks) => {
                    let ContentBlock::ToolResult {
                        content: Value::String(text),
                        ..
                    } = &blocks[0]
                    else {
                        panic!("expected the original legacy representation");
                    };
                    text
                }
            };
            assert_eq!(actual, &payload);
            ContextSnapshot::from_request(&request, ContextCompleteness::Full)
                .write_to_request(&mut request);
        }
    }

    #[test]
    fn fresh_tool_text_is_not_reinterpreted_as_media() {
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "d".repeat(IDENTIFIER_LEN)),
            secret: "Q8n4Vk7sT2p9X5a3Lc6D0h1R".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let payload = serde_json::json!([{"type": "image", "value": mapping.secret}]).to_string();
        let item = AiItem::function_call_output("call-json", Value::String(payload.clone()));
        let mut request = AiRequest::new("model", vec![item]);
        ContextSnapshot::from_request(&request, ContextCompleteness::Full)
            .write_to_request(&mut request);
        redact_request(&mut request, std::slice::from_ref(&mapping)).unwrap();
        let MessageContent::Blocks(blocks) = &request.items[0].content else {
            panic!("expected a rebuilt tool result");
        };
        let ContentBlock::ToolResult {
            content: Value::String(text),
            ..
        } = &blocks[0]
        else {
            panic!("expected ordinary tool text");
        };
        assert_eq!(text, &payload.replace(&mapping.secret, &mapping.reference));
    }

    #[test]
    fn expired_outbound_snapshot_fails_closed_but_restore_stays_literal() {
        // A loaded mapping can expire after detection/intern and before replacement.
        // Pin that boundary instead of racing a wall-clock sleep.
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "a".repeat(IDENTIFIER_LEN)),
            secret: "synthetic snapshot secret".into(),
            expires_at: 0,
            rule_ids: Default::default(),
        };
        let mut request = AiRequest::new("model", Vec::new());
        request.instructions = Some(mapping.secret.clone());
        assert!(matches!(
            redact_request(&mut request, std::slice::from_ref(&mapping)),
            Err(RedactionError::InvalidText)
        ));
        let mut used = BTreeSet::new();
        assert_eq!(
            replace(
                &mapping.reference,
                std::slice::from_ref(&mapping),
                true,
                &mut used
            ),
            mapping.reference
        );
        assert!(used.is_empty());
        request.instructions = Some("ordinary text".into());
        assert_eq!(
            redact_request(&mut request, &[mapping]).unwrap().references,
            Vec::<String>::new()
        );
        assert_eq!(request.instructions.as_deref(), Some("ordinary text"));
    }

    #[test]
    fn malformed_tool_arguments_still_sweep_credentials_and_pass_through() {
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "b".repeat(IDENTIFIER_LEN)),
            secret: "sk-live-7f3a9c".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let malformed = format!("{{\"key\":\"{}\"", mapping.secret);

        // 未闭合字符串残段中的字面凭据仍被检测并替换成引用原子。
        let mut request = AiRequest::new(
            "model",
            vec![AiItem::function_call(
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "call-1".into(),
                    name: "local_probe".into(),
                    arguments: malformed.clone(),
                },
            )],
        );
        let texts = request_texts(&request, true)
            .expect("malformed arguments are still readable for detection");
        assert!(texts.iter().any(|text| text.contains(&mapping.secret)));
        redact_request(&mut request, std::slice::from_ref(&mapping))
            .expect("malformed arguments must not fail redaction");
        let redacted = &request.items[0].tool_calls.as_ref().unwrap()[0].arguments;
        assert!(!redacted.contains(&mapping.secret));
        assert!(redacted.contains(&mapping.reference));
        assert!(redacted.starts_with("{\"key\":\""));

        // 还原方向把引用原子按原样字节放回残段。
        let mut item = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call-1".into(),
            name: "local_probe".into(),
            arguments: redacted.clone(),
        });
        let mut used = BTreeSet::new();
        restore_item(&mut item, std::slice::from_ref(&mapping), &mut used)
            .expect("malformed arguments must not fail restoration");
        assert_eq!(item.tool_calls.as_ref().unwrap()[0].arguments, malformed);
    }

    #[test]
    fn malformed_tool_arguments_keep_decoded_credential_and_escape_semantics() {
        // 转义形式的凭据：闭合字符串 token 解码后可检测；裸原始字节扫描
        // 反而匹配不到 "\u002d" 形态。
        let escaped = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "b".repeat(IDENTIFIER_LEN)),
            secret: "sk-live-7f3a9c".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let malformed = "{\"k\":\"sk\\u002dlive\\u002d7f3a9c\",\"t\":".to_string();
        let mut request = AiRequest::new(
            "model",
            vec![AiItem::function_call(
                stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "call-1".into(),
                    name: "local_probe".into(),
                    arguments: malformed.clone(),
                },
            )],
        );
        let texts = request_texts(&request, true).expect("readable");
        assert!(texts.iter().any(|text| text.contains(&escaped.secret)));
        redact_request(&mut request, std::slice::from_ref(&escaped)).expect("redact");
        let redacted = &request.items[0].tool_calls.as_ref().unwrap()[0].arguments;
        assert!(redacted.contains(&escaped.reference));
        assert!(!redacted.contains(&escaped.secret));
        let mut item = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call-1".into(),
            name: "local_probe".into(),
            arguments: redacted.clone(),
        });
        let mut used = BTreeSet::new();
        restore_item(&mut item, std::slice::from_ref(&escaped), &mut used).expect("restore");
        assert!(
            item.tool_calls.as_ref().unwrap()[0]
                .arguments
                .contains(&escaped.secret),
            "decoded credential must restore inside the same token"
        );

        // 需要 JSON 转义的 secret 在闭合 token 内往返：还原必须重新转义，
        // 而不是把原始引号字节直接插入字符串。
        let quoting = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "c".repeat(IDENTIFIER_LEN)),
            secret: "a\"b".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let malformed = "{\"k\":\"a\\\"b\",\"t\":".to_string();
        let mut item = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call-1".into(),
            name: "local_probe".into(),
            arguments: format!("{{\"k\":\"{}\",\"t\":", quoting.reference),
        });
        let mut used = BTreeSet::new();
        restore_item(&mut item, std::slice::from_ref(&quoting), &mut used).expect("restore");
        assert_eq!(
            item.tool_calls.as_ref().unwrap()[0].arguments,
            malformed,
            "restored secret must be re-escaped as JSON string content"
        );
    }

    #[test]
    fn typed_native_prompt_carriers_protect_text_without_mutating_identities() {
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "d".repeat(IDENTIFIER_LEN)),
            secret: "sk-native-prompt-secret".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        let mut request = AiRequest::new("model", Vec::new());
        request.parameters.session.id = Some(mapping.secret.clone());
        request.parameters.metadata =
            Some([(mapping.secret.clone(), mapping.secret.clone())].into());
        request.parameters.native_tools = serde_json::from_value(serde_json::json!([
            {"type": "tool_search", "description": mapping.secret,
                "parameters": {"type": "object", "description": mapping.secret}},
            {"type": "custom", "name": mapping.secret, "description": mapping.secret,
                "grammar": {"syntax": "lark", "definition": mapping.secret}},
            {"type": "namespace", "name": mapping.secret, "description": mapping.secret,
                "tools": [
                    {"type": "function", "name": mapping.secret,
                        "description": mapping.secret,
                        "parameters": {"type": "object", "description": mapping.secret}},
                    {"type": "custom", "name": mapping.secret,
                        "grammar": {"syntax": "regex", "definition": mapping.secret}}
                ]}
        ]))
        .unwrap();
        let texts = request_texts(&request, true).unwrap();
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.contains(&mapping.secret))
                .count(),
            9
        );
        redact_request(&mut request, std::slice::from_ref(&mapping)).unwrap();
        assert_eq!(
            request.parameters.session.id.as_deref(),
            Some(mapping.secret.as_str())
        );
        assert_eq!(
            request.parameters.metadata.as_ref().unwrap()[&mapping.secret],
            mapping.reference
        );
        let tools = serde_json::to_value(&request.parameters.native_tools).unwrap();
        assert_eq!(tools[0]["description"], mapping.reference);
        assert_eq!(tools[0]["parameters"]["description"], mapping.reference);
        assert_eq!(tools[1]["name"], mapping.secret);
        assert_eq!(tools[1]["grammar"]["definition"], mapping.reference);
        assert_eq!(tools[2]["name"], mapping.secret);
        assert_eq!(tools[2]["tools"][0]["name"], mapping.secret);
        assert_eq!(
            tools[2]["tools"][0]["parameters"]["description"],
            mapping.reference
        );
        assert_eq!(
            tools[2]["tools"][1]["grammar"]["definition"],
            mapping.reference
        );
        read_surface::request(&request, true, Origin::of("request"), &mut |text, _, _| {
            assert!(!text.contains(&mapping.secret));
            Ok(Cow::Borrowed(text))
        })
        .unwrap();
    }

    #[test]
    fn undecodable_escape_regions_fail_closed_in_all_directions() {
        // \l、\q 不是合法转义：对应区域可能藏有转义形式凭据，
        // 检测与出站保护不能返回成功。
        let mapping = Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", "b".repeat(IDENTIFIER_LEN)),
            secret: "sk-live-7f3a9c".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        };
        for malformed in ["{\"k\":\"sk\\live", "{\"k\":\"a\\qb\",\"t\":"] {
            let request = |malformed: &str| {
                AiRequest::new(
                    "model",
                    vec![AiItem::function_call(
                        stravia_runtime_contract::protocol::ir::ToolCall {
                            id: "call-1".into(),
                            name: "local_probe".into(),
                            arguments: malformed.to_string(),
                        },
                    )],
                )
            };
            assert!(
                matches!(
                    request_texts(&request(malformed), true),
                    Err(RedactionError::InvalidText)
                ),
                "detection must fail closed on undecodable escapes: {malformed}"
            );
            let mut request = request(malformed);
            assert!(matches!(
                redact_request(&mut request, std::slice::from_ref(&mapping)),
                Err(RedactionError::InvalidText)
            ));

            // 还原方向同样 fail-closed：引用也可能被上游 unicode 转义后藏入
            // 损坏区域，字面扫描无法保证完整覆盖。
            let mut item =
                AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
                    id: "call-1".into(),
                    name: "local_probe".into(),
                    arguments: malformed.to_string(),
                });
            let mut used = BTreeSet::new();
            assert!(matches!(
                restore_item(&mut item, std::slice::from_ref(&mapping), &mut used),
                Err(RedactionError::InvalidText)
            ));
        }
    }

    fn known_mapping(letter: &str, rule_ids: &[&str]) -> Mapping {
        Mapping {
            reference: format!("{PREFIX}{}{SUFFIX}", letter.repeat(IDENTIFIER_LEN)),
            secret: "sk-known-9f2c41".into(),
            expires_at: i64::MAX,
            rule_ids: rule_ids
                .iter()
                .map(|rule| (*rule).to_owned())
                .collect::<Vec<_>>()
                .into(),
        }
    }

    #[test]
    fn redaction_places_a_known_secret_only_where_the_request_submits_it() {
        use stravia_runtime_contract::protocol::ir::{Role, ToolCall};

        let mapping = known_mapping("e", &["github-pat"]);
        let user = |text: &str| AiItem {
            role: Role::User,
            content: MessageContent::Text(std::sync::Arc::new(text.into())),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let call = AiItem::function_call(ToolCall {
            id: "call-9".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        let result = AiItem::function_call_output("call-9", Value::String(mapping.secret.clone()));

        // 密钥只在客户端重放的历史里：替换了，但没有本次提交的位置。
        let mut replayed = AiRequest::new(
            "model",
            vec![
                user(&mapping.secret),
                AiItem::output_text("ok"),
                user("thanks"),
            ],
        );
        let outcome = redact_request(&mut replayed, std::slice::from_ref(&mapping)).unwrap();
        assert_eq!(outcome.references, vec![mapping.reference.clone()]);
        assert!(outcome.placements.is_empty());

        // 同一密钥出现在最后一条 Assistant 之后的工具结果里：那是本次提交的位置，不是裸文本位置。
        let mut submitted = AiRequest::new("model", vec![user(&mapping.secret), call, result]);
        let outcome = redact_request(&mut submitted, std::slice::from_ref(&mapping)).unwrap();
        let places = &outcome.placements[&mapping.reference];
        assert_eq!(places.len(), 1);
        let place = places.iter().next().unwrap();
        assert_eq!(place.source, "tool_result");
        assert_eq!(place.tool_id.as_deref(), Some("call-9"));
    }

    #[test]
    fn restoring_a_response_attributes_references_to_the_tool_call_that_carried_them() {
        use stravia_runtime_contract::protocol::ir::ToolCall;

        let mapping = known_mapping("f", &["github-pat"]);
        let mut response = AiResponse::new("resp", "model");
        response
            .items
            .push(AiItem::output_text(format!("saw {}", mapping.reference)));
        response.items.push(AiItem::function_call(ToolCall {
            id: "call-a".into(),
            name: "run".into(),
            arguments: format!("{{\"token\":\"{}\"}}", mapping.reference),
        }));
        response.items.push(AiItem::function_call(ToolCall {
            id: "call-b".into(),
            name: "run".into(),
            arguments: "{}".into(),
        }));

        let restored = restore_response(&mut response, std::slice::from_ref(&mapping)).unwrap();

        assert_eq!(restored.references, vec![mapping.reference.clone()]);
        // 助手文本里的还原不是工具调用；没有引用的调用不出现。
        assert_eq!(
            restored.tool_calls,
            BTreeMap::from([(
                "call-a".to_owned(),
                BTreeSet::from([mapping.reference.clone()])
            )])
        );
        let calls = response.items[1].tool_calls.as_ref().unwrap();
        assert!(calls[0].arguments.contains(&mapping.secret));
    }
}
