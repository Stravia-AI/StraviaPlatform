//! The readable canonical surface. Protocol identities and opaque payloads never enter it.
use std::collections::BTreeSet;

use serde_json::Value;

use super::{RedactionError, store::Mapping};
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, AiResponse, ContentBlock, DocumentSource, EmbeddingInput, MessageContent,
    ProtocolExt, ResponseFormat, TOOL_RESULT_CONTENT_KIND_META, ToolResultContentKind,
};

pub(super) const PREFIX: &str = "~stravia-secret:";
pub(super) const REFERENCE_LEN: usize = PREFIX.len() + 32 + 1;

pub(super) fn valid_reference(value: &str) -> bool {
    value.len() == REFERENCE_LEN
        && value.starts_with(PREFIX)
        && value.ends_with('~')
        && value.as_bytes()[PREFIX.len()..REFERENCE_LEN - 1]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

pub(super) fn active(mapping: &Mapping) -> bool {
    mapping.expires_at > chrono::Utc::now().timestamp_millis()
        && valid_reference(&mapping.reference)
}

/// Single pass over original bytes: inserted secrets/references are never scanned again.
pub(super) fn replace(
    text: &str,
    mappings: &[Mapping],
    restore: bool,
    used: &mut BTreeSet<String>,
) -> String {
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
    let mut result = String::with_capacity(text.len());
    let mut position = 0;
    while position < text.len() {
        if let Some(mapping) = candidates
            .iter()
            .find(|m| text[position..].starts_with(if restore { &m.reference } else { &m.secret }))
        {
            let (from, to) = if restore {
                (&mapping.reference, &mapping.secret)
            } else {
                (&mapping.secret, &mapping.reference)
            };
            result.push_str(to);
            position += from.len();
            used.insert(mapping.reference.clone());
        } else {
            let ch = text[position..].chars().next().expect("character boundary");
            result.push(ch);
            position += ch.len_utf8();
        }
    }
    result
}

type Visitor<'a> = dyn FnMut(&mut String, Option<&str>) -> Result<(), RedactionError> + 'a;

fn json_values(
    value: &mut Value,
    context: Option<&str>,
    visit: &mut Visitor<'_>,
) -> Result<(), RedactionError> {
    match value {
        Value::String(text) => visit(text, context)?,
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
    let _: Value = serde_json::from_str(arguments).map_err(|_| RedactionError::InvalidText)?;
    let bytes = arguments.as_bytes();
    let mut out = String::with_capacity(arguments.len());
    let mut cursor = 0;
    let mut copied = 0;
    let mut context: Option<String> = None;
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
        let mut decoded: String =
            serde_json::from_str(raw).map_err(|_| RedactionError::InvalidText)?;
        let after = arguments[cursor..].trim_start();
        if after.starts_with(':') {
            context = Some(decoded);
            continue;
        }
        let original = decoded.clone();
        visit(&mut decoded, context.as_deref())?;
        if decoded != original {
            out.push_str(&arguments[copied..start]);
            out.push_str(
                &serde_json::to_string(&decoded).map_err(|_| RedactionError::InvalidText)?,
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
            write_surface::wire_text(value, "tool_result", &mut |text, context, _| {
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
    let mut value: Value = serde_json::from_str(text).map_err(|_| RedactionError::InvalidText)?;
    let mut changed = false;
    tool_result(&mut value, Some(kind), true, &mut |text, context| {
        let original = text.clone();
        visit(text, context)?;
        changed |= original != *text;
        Ok(())
    })?;
    if changed {
        *text = serde_json::to_string(&value).map_err(|_| RedactionError::InvalidText)?;
    }
    Ok(())
}

// Both borrows expand from one field-selection definition. Collection never clones
// media, vendor metadata, opaque reasoning, or the request itself.
macro_rules! readable_surface {
    ($module:ident, $json:ident, $arguments:ident, $result:ident, $encoded:ident, $iter:ident, $get:ident, $values:ident, $slice:ident, $($qualifier:tt)*) => {
        mod $module {
            use super::*;
            fn json_values(value: & $($qualifier)* Value, context: Option<&str>, location: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$json(value, context, &mut |text, context| visit(text, context, location))
}
fn arguments_text(value: & $($qualifier)* String, _source: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$arguments(value, &mut |text, context| visit(text, context, "tool_arguments"))
}
fn tool_result(value: & $($qualifier)* Value, kind: Option<ToolResultContentKind>, reject: bool, _source: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$result(value, kind, reject, &mut |text, context| visit(text, context, "tool_result"))
}
fn encoded_tool_result(value: & $($qualifier)* String, kind: ToolResultContentKind, _source: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    super::$encoded(value, kind, &mut |text, context| visit(text, context, "tool_result"))
}
            type Visitor<'a> = dyn FnMut(& $($qualifier)* String, Option<&str>, &'static str) -> Result<(), RedactionError> + 'a;
pub(super) fn schema(value: & $($qualifier)* Value, location: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
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


pub(super) fn wire_text(value: & $($qualifier)* Value, location: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    match value {
        Value::Array(values) => for value in values { wire_text(value, location, visit)?; },
        Value::Object(values) => {
            let kind = values.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "image" | "image_url" | "input_image" | "audio" | "input_audio" | "video" | "file" | "input_file" | "redacted_thinking" | "compaction" | "compaction_trigger") { return Ok(()); }
            let document = kind == "document";
            let search = kind == "search_result";
            let embedded_resource = kind == "resource";
            let tool_input = matches!(kind, "tool_use" | "server_tool_use");
            let location = if tool_input { "tool_arguments" } else if matches!(kind, "tool_result" | "server_tool_result" | "codeExecutionResult") { "tool_result" } else { match values.get("role").and_then(Value::as_str) { Some("user") => "user_message", Some("tool") => "tool_result", Some("assistant" | "system" | "developer") => "system_or_history", _ => location } };
            if search && let Some(Value::String(source)) = values.$get("source") { visit(source, None, location)?; }
            if document && let Some(source) = values.$get("source") {
                match source.get("type").and_then(Value::as_str) {
                    Some("text" | "plain_text") => if let Some(Value::String(data)) = source.$get("data") { visit(data, None, location)?; },
                    Some("content" | "blocks") => if let Some(content) = source.$get("content") { wire_text(content, location, visit)?; },
                    _ => {}
                }
            }
            for (key, value) in values {
                match key.as_str() {
                    "text" | "refusal" | "thinking" | "cited_text" | "title" | "description" | "code" | "stdout" | "stderr" | "query" | "queries" | "prompt" => json_values(value, Some(key), location, visit)?,
                    "arguments" => if let Value::String(arguments) = value { arguments_text(arguments, location, visit)?; } else { json_values(value, None, "tool_arguments", visit)?; },
                    "input" if tool_input => json_values(value, None, location, visit)?,
                    "input" | "output" | "variables" | "json" => json_values(value, None, location, visit)?,
                    "functionCall" => if let Some(arguments) = value.$get("args") { json_values(arguments, None, "tool_arguments", visit)?; },
                    "functionResponse" => if let Some(result) = value.$get("response") { json_values(result, None, "tool_result", visit)?; },
                    "resource" if embedded_resource => if let Some(Value::String(text)) = value.$get("text") { visit(text, None, location)?; },
                    "format" | "json_schema" | "parts" | "functionDeclarations" | "executableCode" | "codeExecutionResult" | "citations" => wire_text(value, location, visit)?,
                    "content" | "summary" | "results" | "context" => if let Value::String(text) = value { visit(text, Some(key), location)?; } else { wire_text(value, location, visit)?; },
                    "schema" | "parameters" | "input_schema" | "responseSchema" | "responseJsonSchema" => schema(value, location, visit)?,
                    _ => {}
                }
            }
        },
        Value::String(text) => visit(text, None, location)?,
        _ => {}
    }
    Ok(())
}


pub(super) fn blocks(blocks: & $($qualifier)* [ContentBlock], reject_ambiguous: bool, location: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    for block in blocks {
        let location = match block {
            ContentBlock::ToolUse { .. } | ContentBlock::ServerToolUse { .. } => "tool_arguments",
            ContentBlock::ToolResult { .. } | ContentBlock::ServerToolResult { .. } | ContentBlock::CodeExecutionResult { .. } => "tool_result",
            _ => location,
        };
        match block {
            ContentBlock::Text { text, .. } => visit(text, None, location)?,
            ContentBlock::Thinking { thinking, .. } => visit(thinking, None, location)?,
            ContentBlock::Reasoning { summary, content, .. } => for text in summary.$iter().chain(content) { visit(text, None, location)?; },
            ContentBlock::ToolUse { input, .. } | ContentBlock::ServerToolUse { input, .. } => json_values(input, None, location, visit)?,
            ContentBlock::ToolResult { content, content_kind, .. } | ContentBlock::ServerToolResult { content, content_kind, .. } => tool_result(content, *content_kind, reject_ambiguous, location, visit)?,
            ContentBlock::Document { source, title, context, .. } => {
                for text in title.$iter().chain(context) { visit(text, None, location)?; }
                match source { DocumentSource::PlainText { data } => visit(data, None, location)?, DocumentSource::Blocks { content } => self::blocks(content, reject_ambiguous, location, visit)?, _ => {} }
            }
            ContentBlock::SearchResult { content, title, source, .. } => { visit(title, None, location)?; visit(source, None, location)?; self::blocks(content, reject_ambiguous, location, visit)?; }
            ContentBlock::Citation { cited_text, .. } => visit(cited_text, None, location)?,
            ContentBlock::ExecutableCode { code, .. } => visit(code, None, location)?,
            ContentBlock::CodeExecutionResult { stdout, stderr, .. } => { visit(stdout, None, location)?; visit(stderr, None, location)?; }
            ContentBlock::Refusal { refusal } => visit(refusal, None, location)?,
            ContentBlock::Unknown { raw } => wire_text(raw, location, visit)?,
            ContentBlock::Image { .. } | ContentBlock::Audio { .. } | ContentBlock::File { .. } | ContentBlock::Video { .. } | ContentBlock::RedactedThinking { .. } | ContentBlock::Compaction { .. } | ContentBlock::CompactionTrigger {} | ContentBlock::ContainerUpload { .. } => {}
        }
    }
    Ok(())
}


pub(super) fn item_text(item: & $($qualifier)* AiItem, reject_ambiguous: bool, _location: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    let location = match item.role {
        stravia_runtime_contract::protocol::ir::Role::User => "user_message",
        stravia_runtime_contract::protocol::ir::Role::Tool => "tool_result",
        _ => "system_or_history",
    };
    let encoded_kind = if item.role == stravia_runtime_contract::protocol::ir::Role::Tool {
        item.meta.as_ref().and_then(|meta| meta.get(TOOL_RESULT_CONTENT_KIND_META))
        .map(|kind| serde_json::from_value::<ToolResultContentKind>(kind.clone()).map_err(|_| RedactionError::InvalidText))
        .transpose()?
    } else { None };
    match & $($qualifier)* item.content {
        MessageContent::Text(text) if item.role == stravia_runtime_contract::protocol::ir::Role::Tool => match encoded_kind {
            Some(ToolResultContentKind::ContentBlocks) => encoded_tool_result(text, ToolResultContentKind::ContentBlocks, location, visit)?,
            Some(ToolResultContentKind::Json) => visit(text, None, location)?,
            None if encoded_compound_array(text) => if reject_ambiguous { return Err(RedactionError::AmbiguousToolResult); },
            None => visit(text, None, location)?,
        },
        MessageContent::Text(text) => visit(text, None, location)?,
        MessageContent::Blocks(content) if item.role == stravia_runtime_contract::protocol::ir::Role::Tool => for block in content {
            if let (Some(kind), ContentBlock::ToolResult { content: Value::String(text), .. }
                | ContentBlock::ServerToolResult { content: Value::String(text), .. }) = (encoded_kind, & $($qualifier)* *block) {
                match kind {
                    ToolResultContentKind::ContentBlocks => encoded_tool_result(text, kind, location, visit)?,
                    ToolResultContentKind::Json => visit(text, None, location)?,
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
    if let Some(calls) = & $($qualifier)* item.tool_calls { for call in calls { arguments_text(& $($qualifier)* call.arguments, location, visit)?; } }
    Ok(())
}


pub(super) fn request(request: & $($qualifier)* AiRequest, reject_ambiguous: bool, location: &'static str, visit: &mut Visitor<'_>) -> Result<(), RedactionError> {
    if let Some(text) = & $($qualifier)* request.instructions { visit(text, None, "system_or_history")?; }
    for item in & $($qualifier)* request.items { item_text(item, reject_ambiguous, location, visit)?; }
    if let Some(embedding) = & $($qualifier)* request.embedding { match & $($qualifier)* embedding.input { EmbeddingInput::Text(text) => visit(text, None, location)?, EmbeddingInput::Texts(texts) => for text in texts { visit(text, None, location)?; }, _ => {} } }
    if let Some(tools) = & $($qualifier)* request.tools { for tool in tools { if let Some(text) = & $($qualifier)* tool.description { visit(text, None, location)?; } schema(& $($qualifier)* tool.parameters, location, visit)?; } }
    if let Some(ResponseFormat::JsonSchema { schema: value, .. }) = & $($qualifier)* request.response_format { schema(value, location, visit)?; }
    // Encoders prefer these fidelity-preserving copies over canonical fields.
    // Select only model-readable carriers, never the arbitrary vendor bag:
    // authentication, identities, media and opaque signatures must stay intact.
    for (key, value) in & $($qualifier)* request.meta.vendor.ingress {
        match key.as_str() {
            "__anthropic_raw_system" | "__google_raw_system_instruction" => wire_text(value, "system_or_history", visit)?,
            "__anthropic_raw_messages" | "__anthropic_raw_tools"
            | "__google_raw_tools" | "__google_generation_config"
            | "prediction" | "response_format" => wire_text(value, location, visit)?,
            _ => {}
        }
    }
    if let Some(ext) = & $($qualifier)* request.ext {
        match ext {
            ProtocolExt::OpenAiChat(ext) => if let Some(value) = & $($qualifier)* ext.prediction { wire_text(value, location, visit)?; },
            ProtocolExt::OpenResponses(ext) => {
                if let Some(value) = & $($qualifier)* ext.text { wire_text(value, location, visit)?; }
                for value in & $($qualifier)* ext.passthrough_tools { wire_text(value, location, visit)?; }
                for (key, value) in & $($qualifier)* ext.passthrough_body { if matches!(key.as_str(), "prompt" | "context") { wire_text(value, location, visit)?; } }
            }
            ProtocolExt::Anthropic(ext) => {
                if let Some(value) = & $($qualifier)* ext.output_config { if let Some(format) = value.$get("format") { wire_text(format, location, visit)?; } }
                if let Some(tools) = & $($qualifier)* ext.server_tools { for tool in tools { wire_text(tool, location, visit)?; } }
            }
            ProtocolExt::Google(ext) => if let Some(value) = & $($qualifier)* ext.response_json_schema { schema(value, location, visit)?; },
        }
    }
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
    mut
);

type ReadVisitor<'a> = dyn FnMut(&String, Option<&str>) -> Result<(), RedactionError> + 'a;

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
        return visit(text, context);
    }
    let mut readable = String::new();
    decoded_json(value, context, &mut readable)?;
    if !readable.is_empty() {
        visit(&readable, None)?;
    }
    Ok(())
}

fn read_arguments(arguments: &String, visit: &mut ReadVisitor<'_>) -> Result<(), RedactionError> {
    if arguments.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(arguments).map_err(|_| RedactionError::InvalidText)?;
    read_json_values(&value, None, visit)
}

fn read_tool_result(
    value: &Value,
    kind: Option<ToolResultContentKind>,
    reject_ambiguous: bool,
    visit: &mut ReadVisitor<'_>,
) -> Result<(), RedactionError> {
    let mut readable = String::new();
    let mut collect =
        |text: &String, context: Option<&str>| append_context(&mut readable, text, context);
    match result_kind(value, kind, reject_ambiguous)? {
        Some(ToolResultContentKind::Json) => read_json_values(value, None, &mut collect)?,
        Some(ToolResultContentKind::ContentBlocks) => {
            read_surface::wire_text(value, "tool_result", &mut |text, context, _| {
                collect(text, context)
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
    text: &String,
    kind: ToolResultContentKind,
    visit: &mut ReadVisitor<'_>,
) -> Result<(), RedactionError> {
    let value: Value = serde_json::from_str(text).map_err(|_| RedactionError::InvalidText)?;
    read_tool_result(&value, Some(kind), true, visit)
}

pub fn request_texts(
    value: &AiRequest,
    reject_ambiguous: bool,
) -> Result<(Vec<String>, Vec<&'static str>), RedactionError> {
    let mut texts = Vec::new();
    let mut sources = Vec::new();
    read_surface::request(
        value,
        reject_ambiguous,
        "other_text",
        &mut |text, context, source| {
            sources.push(source);
            if let Some(key) = context {
                let mut readable = String::new();
                append_context(&mut readable, text, Some(key))?;
                texts.push(readable);
            } else {
                texts.push(text.clone());
            }
            Ok(())
        },
    )?;
    Ok((texts, sources))
}

pub fn redact_request(
    value: &mut AiRequest,
    mappings: &[Mapping],
) -> Result<Vec<String>, RedactionError> {
    let mut used = BTreeSet::new();
    write_surface::request(value, true, "other_text", &mut |text, _, _| {
        *text = replace(text, mappings, false, &mut used);
        Ok(())
    })?;
    if mappings
        .iter()
        .any(|mapping| used.contains(&mapping.reference) && !active(mapping))
    {
        return Err(RedactionError::InvalidText);
    }
    Ok(used.into_iter().collect())
}

pub(super) fn restore_item(
    item: &mut AiItem,
    mappings: &[Mapping],
    used: &mut BTreeSet<String>,
) -> Result<(), RedactionError> {
    write_surface::item_text(item, false, "other_text", &mut |text, _, _| {
        *text = replace(text, mappings, true, used);
        Ok(())
    })
}

pub(super) fn restore_arguments(
    arguments: &mut String,
    mappings: &[Mapping],
    used: &mut BTreeSet<String>,
) -> Result<(), RedactionError> {
    arguments_text(arguments, &mut |text, _| {
        *text = replace(text, mappings, true, used);
        Ok(())
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
    let mut changed = false;
    write_surface::wire_text(&mut value, "other_text", &mut |text, _, _| {
        let restored = replace(text, mappings, true, used);
        changed |= restored != *text;
        *text = restored;
        Ok(())
    })?;
    if changed {
        *raw = serde_json::to_string(&value).map_err(|_| RedactionError::InvalidText)?;
    }
    Ok(true)
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
            write_surface::wire_text(item, "other_text", &mut |text, _, _| {
                *text = replace(text, mappings, true, &mut used);
                Ok(())
            })?;
        }
    }
    Ok(used.into_iter().collect())
}

pub fn restore_response(
    value: &mut AiResponse,
    mappings: &[Mapping],
) -> Result<Vec<String>, RedactionError> {
    let mut used = BTreeSet::new();
    for item in &mut value.items {
        restore_item(item, mappings, &mut used)?;
    }
    Ok(used.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::hook::{ContextCompleteness, ContextSnapshot};

    #[test]
    fn encoded_tool_media_stays_opaque_after_context_rebuilds() {
        let mapping = Mapping {
            reference: format!("{PREFIX}{}~", "b".repeat(32)),
            secret: "Q8n4Vk7sT2p9X5a3Lc6D0h1R".into(),
            expires_at: i64::MAX,
        };
        let mut payload = serde_json::json!([
            {"type": "text", "text": mapping.secret},
            {"type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": mapping.secret
            }}
        ]);
        let mut item =
            AiItem::function_call_output("call-media", Value::String(payload.to_string()));
        item.meta = Some(serde_json::json!({
            (TOOL_RESULT_CONTENT_KIND_META): "content_blocks"
        }));
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
            reference: format!("{PREFIX}{}~", "c".repeat(32)),
            secret: "Q8n4Vk7sT2p9X5a3Lc6D0h1R".into(),
            expires_at: i64::MAX,
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
                MessageContent::Text(text) => text,
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
            reference: format!("{PREFIX}{}~", "d".repeat(32)),
            secret: "Q8n4Vk7sT2p9X5a3Lc6D0h1R".into(),
            expires_at: i64::MAX,
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
            reference: format!("{PREFIX}{}~", "a".repeat(32)),
            secret: "synthetic snapshot secret".into(),
            expires_at: 0,
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
            redact_request(&mut request, &[mapping]).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(request.instructions.as_deref(), Some("ordinary text"));
    }
}
