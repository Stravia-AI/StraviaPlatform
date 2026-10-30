/// Render only typed native controls/state through the controlled same-protocol bag.
/// Canonical state wins over the stored wire snapshot, preventing stale ciphertext replay.
pub fn native_compaction_item(item: &crate::protocol::ir::AiItem) -> Option<serde_json::Value> {
    use crate::protocol::ir::{ContentBlock, MessageContent};
    let MessageContent::Blocks(blocks) = &item.content else {
        return None;
    };
    let (kind, encrypted_content) = match blocks.as_slice() {
        [ContentBlock::Compaction { encrypted_content }] => ("compaction", Some(encrypted_content)),
        [ContentBlock::CompactionTrigger {}] => ("compaction_trigger", None),
        _ => return None,
    };
    let mut wire = item
        .meta
        .as_ref()
        .and_then(|meta| meta.get("__open_responses_item"))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    wire.insert("type".into(), kind.into());
    if let Some(encrypted_content) = encrypted_content {
        wire.insert("encrypted_content".into(), encrypted_content.clone().into());
    }
    if let Some(id) = item.id_ref() {
        wire.insert("id".into(), id.into());
    }
    Some(serde_json::Value::Object(wire))
}

use sha2::{Digest, Sha256};

use super::{
    AiItem, AiRequest, ContentBlock, DocumentSource, MediaSource, MessageContent, ToolSpec,
};

mod retry;

/// Hash the complete canonical request except its per-attempt correlation ID.
///
/// Object ordering is immaterial; all session identity and future request fields
/// participate. This typed fingerprint is process-local, not a persisted wire format.
pub fn retry_request_hash(request: &AiRequest) -> Result<[u8; 32], serde_json::Error> {
    retry::request_hash(request)
}

struct HashWriter(Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_hash(value: &(impl serde::Serialize + ?Sized)) -> [u8; 32] {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value).expect("canonical JSON value must serialize");
    writer.0.finalize().into()
}

/// Produces the stable Provider prompt material used by cache routing.
///
/// Cache directives and graph metadata are policy and delivery state, not
/// prompt material. Keeping this as a positive projection prevents new IR
/// fields from silently changing cache identity before their Provider
/// semantics are classified.
pub fn item_value(item: &AiItem) -> serde_json::Value {
    serde_json::Value::Array(history_item_values(item))
}

pub fn item_hash(item: &AiItem) -> [u8; 32] {
    json_hash(&history_units(item))
}

pub fn item_hashes(items: &[AiItem]) -> Vec<[u8; 32]> {
    items
        .iter()
        .flat_map(history_units)
        .map(|value| json_hash(&value))
        .collect()
}

pub fn history_unit_count(items: &[AiItem]) -> usize {
    items.iter().map(|item| history_units(item).len()).sum()
}

enum HistoryUnit<'a> {
    Text { item: &'a AiItem, text: &'a str },
    Owned(serde_json::Value),
}

// A real JSON Map preserves the established order with or without
// serde_json's preserve_order feature, including feature unification.
enum HistoryObject {
    Text,
    Assistant,
    Tool,
    Message,
}

impl HistoryObject {
    fn keys(&self) -> &'static [String] {
        use std::sync::OnceLock;
        static TEXT: OnceLock<Vec<String>> = OnceLock::new();
        static ASSISTANT: OnceLock<Vec<String>> = OnceLock::new();
        static TOOL: OnceLock<Vec<String>> = OnceLock::new();
        static MESSAGE: OnceLock<Vec<String>> = OnceLock::new();
        let (cache, keys): (_, &[&str]) = match self {
            Self::Text => (&TEXT, &["type", "text"]),
            Self::Assistant => (&ASSISTANT, &["role", "content"]),
            Self::Tool => (&TOOL, &["role", "tool_call_id", "output", "is_error"]),
            Self::Message => (
                &MESSAGE,
                &[
                    "role",
                    "content",
                    "tool_calls",
                    "tool_call_id",
                    "artifact_references",
                ],
            ),
        };
        cache.get_or_init(|| {
            keys.iter()
                .map(|key| ((*key).to_owned(), serde_json::Value::Null))
                .collect::<serde_json::Map<_, _>>()
                .into_iter()
                .map(|(key, _)| key)
                .collect()
        })
    }
}

fn text_unit_kind(item: &AiItem) -> u8 {
    if item.role == super::Role::Assistant {
        0
    } else if item.role == super::Role::Tool && item.tool_call_id.is_some() {
        1
    } else {
        2
    }
}

impl PartialEq for HistoryUnit<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Owned(left), Self::Owned(right)) => left == right,
            (
                Self::Text {
                    item: left,
                    text: a,
                },
                Self::Text {
                    item: right,
                    text: b,
                },
            ) => {
                let kind = text_unit_kind(left);
                a == b
                    && kind == text_unit_kind(right)
                    && match kind {
                        0 => true,
                        1 => left.tool_call_id == right.tool_call_id,
                        _ => {
                            left.role == right.role
                                && match (&left.tool_calls, &right.tool_calls) {
                                    (None, None) => true,
                                    (Some(a), Some(b)) => {
                                        a.len() == b.len()
                                            && a.iter().zip(b).all(|(a, b)| {
                                                a.id == b.id
                                                    && a.name == b.name
                                                    && a.arguments == b.arguments
                                            })
                                    }
                                    _ => false,
                                }
                                && left.tool_call_id == right.tool_call_id
                                && left
                                    .meta
                                    .as_ref()
                                    .and_then(|meta| meta.get("__stravia_artifact_references"))
                                    .filter(|value| !value.is_null())
                                    == right
                                        .meta
                                        .as_ref()
                                        .and_then(|meta| meta.get("__stravia_artifact_references"))
                                        .filter(|value| !value.is_null())
                        }
                    }
            }
            // Mixed schemas still compare exact semantic values, not digests.
            (Self::Owned(value), borrowed) | (borrowed, Self::Owned(value)) => {
                *value
                    == serde_json::to_value(borrowed)
                        .expect("canonical history unit must serialize")
            }
        }
    }
}

struct HistoryText<'a>(&'a str);

impl serde::Serialize for HistoryText<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        for key in HistoryObject::Text.keys() {
            match key.as_str() {
                "type" => map.serialize_entry(key, "text")?,
                "text" => map.serialize_entry(key, self.0)?,
                _ => unreachable!(),
            }
        }
        map.end()
    }
}

impl serde::Serialize for HistoryUnit<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let Self::Text { item, text } = self else {
            let Self::Owned(value) = self else {
                unreachable!()
            };
            return serde::Serialize::serialize(value, serializer);
        };
        let assistant = item.role == super::Role::Assistant;
        let tool = item.role == super::Role::Tool && item.tool_call_id.is_some();
        let keys = if assistant {
            HistoryObject::Assistant.keys()
        } else if tool {
            HistoryObject::Tool.keys()
        } else {
            HistoryObject::Message.keys()
        };
        let mut map = serializer.serialize_map(Some(keys.len()))?;
        for key in keys {
            match key.as_str() {
                "role" => map.serialize_entry(key, &item.role)?,
                "content" if assistant => map.serialize_entry(key, &HistoryText(text))?,
                "content" => map.serialize_entry(key, &[HistoryText(text)])?,
                // Struct serializers emit declaration order, whereas the old
                // Value projection uses JSON Map order for each tool call.
                "tool_calls" => map.serialize_entry(
                    key,
                    &serde_json::to_value(&item.tool_calls)
                        .expect("canonical tool calls must serialize"),
                )?,
                "tool_call_id" => map.serialize_entry(key, &item.tool_call_id)?,
                "artifact_references" => map.serialize_entry(
                    key,
                    &item
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("__stravia_artifact_references")),
                )?,
                "output" => map.serialize_entry(key, text)?,
                "is_error" => map.serialize_entry(key, &Option::<bool>::None)?,
                _ => unreachable!(),
            }
        }
        map.end()
    }
}

fn history_units(item: &AiItem) -> Vec<HistoryUnit<'_>> {
    // Complex content and native wrappers retain the canonical projection.
    if let MessageContent::Text(text) = &item.content
        && history_native_item_fields(item).is_none()
    {
        let mut units = Vec::new();
        if item.role == super::Role::Assistant {
            let mut tail = Vec::new();
            assistant_history_tail(item, &mut tail, &[]);
            if !text.is_empty() || tail.is_empty() {
                units.push(HistoryUnit::Text { item, text });
            }
            units.extend(tail.into_iter().map(HistoryUnit::Owned));
        } else {
            units.push(HistoryUnit::Text { item, text });
        }
        return units;
    }
    history_item_values(item)
        .into_iter()
        .map(HistoryUnit::Owned)
        .collect()
}

/// Canonical semantic units of one history item, in emission order. Callers
/// that only need a digest and count should use
/// [`append_history_context_hash_with_count`] to avoid owned text copies.
pub fn history_item_values(item: &AiItem) -> Vec<serde_json::Value> {
    let values = if let Some(mut native) = native_compaction_item(item) {
        if let Some(fields) = native.as_object_mut() {
            for key in [
                "id",
                "status",
                "metadata",
                "internal_chat_message_metadata_passthrough",
            ] {
                fields.remove(key);
            }
        }
        vec![serde_json::Value::Object(serde_json::Map::from_iter([
            ("role".into(), serde_json::json!(item.role)),
            ("native_compaction".into(), native),
        ]))]
    } else if item.role == super::Role::Assistant {
        assistant_history_values(item)
    } else if let Some(values) = tool_output_history_values(item) {
        values
    } else {
        vec![history_item_value(item)]
    };
    if let Some(fields) = history_native_item_fields(item) {
        return vec![serde_json::Value::Object(serde_json::Map::from_iter([
            ("items".into(), serde_json::Value::Array(values)),
            ("native_item_fields".into(), fields),
        ]))];
    }
    values
}

// This bag contains additive wire fields, not just metadata. Preserve every
// unclassified extension conservatively: it may carry model content. Only the
// protocol's application metadata and internal tracking carrier are non-semantic.
fn history_native_item_fields(item: &AiItem) -> Option<serde_json::Value> {
    let fields = item.meta.as_ref()?.get("__open_responses_item_fields")?;
    let Some(object) = fields.as_object() else {
        return Some(fields.clone());
    };
    let semantic = object
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "metadata" | "internal_chat_message_metadata_passthrough"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    (!semantic.is_empty()).then_some(serde_json::Value::Object(semantic))
}

fn history_item_value(item: &AiItem) -> serde_json::Value {
    // Move projected JSON into its parent: json! serializes Value expressions
    // again, recursively copying already-owned text and nested payloads.
    let content = match &item.content {
        MessageContent::Text(text) => {
            serde_json::json!([{ "type": "text", "text": text }])
        }
        MessageContent::Blocks(blocks)
            if blocks
                .iter()
                .all(|block| matches!(block, ContentBlock::Text { .. })) =>
        {
            serde_json::Value::Array(vec![serde_json::Value::Object(serde_json::Map::from_iter(
                [
                    ("type".into(), serde_json::json!("text")),
                    (
                        "text".into(),
                        serde_json::Value::String(
                            blocks.iter().filter_map(ContentBlock::as_text).collect(),
                        ),
                    ),
                ],
            ))])
        }
        MessageContent::Blocks(blocks) => {
            serde_json::Value::Array(blocks.iter().map(history_content_block_value).collect())
        }
    };
    let artifact_references = item
        .meta
        .as_ref()
        .and_then(|meta| meta.get("__stravia_artifact_references"));
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("role".into(), serde_json::json!(&item.role)),
        ("content".into(), content),
        ("tool_calls".into(), serde_json::json!(&item.tool_calls)),
        ("tool_call_id".into(), serde_json::json!(&item.tool_call_id)),
        (
            "artifact_references".into(),
            serde_json::json!(artifact_references),
        ),
    ]))
}

fn assistant_history_values(item: &AiItem) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let mut text = String::new();
    let mut represented_tool_calls = Vec::new();

    let flush_text = |values: &mut Vec<serde_json::Value>, text: &mut String| {
        if !text.is_empty() {
            values.push(assistant_content_value(serde_json::Value::Object(
                serde_json::Map::from_iter([
                    ("type".into(), serde_json::json!("text")),
                    (
                        "text".into(),
                        serde_json::Value::String(std::mem::take(text)),
                    ),
                ]),
            )));
        }
    };

    match &item.content {
        MessageContent::Text(value) => text.push_str(value),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text: value, .. } => text.push_str(value),
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        flush_text(&mut values, &mut text);
                        values.push(reasoning_value(thinking.clone(), signature.as_deref()));
                    }
                    ContentBlock::Reasoning {
                        summary,
                        content,
                        encrypted_content,
                    } => {
                        flush_text(&mut values, &mut text);
                        values.push(assistant_content_value(serde_json::json!({
                            "type": "reasoning",
                            "summary": summary,
                            "content": content,
                            "encrypted_content": encrypted_content,
                        })));
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        flush_text(&mut values, &mut text);
                        represented_tool_calls.push(id.as_str());
                        values.push(tool_call_value(id, name, input.clone()));
                    }
                    other => {
                        flush_text(&mut values, &mut text);
                        values.push(assistant_content_value(history_content_block_value(other)));
                    }
                }
            }
        }
    }
    flush_text(&mut values, &mut text);

    assistant_history_tail(item, &mut values, &represented_tool_calls);

    if values.is_empty() {
        values.push(assistant_content_value(serde_json::json!({
            "type": "text",
            "text": "",
        })));
    }
    values
}

fn assistant_history_tail(
    item: &AiItem,
    values: &mut Vec<serde_json::Value>,
    represented_tool_calls: &[&str],
) {
    for call in item.tool_calls.iter().flatten() {
        if represented_tool_calls.contains(&call.id.as_str()) {
            continue;
        }
        let arguments = serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
        values.push(tool_call_value(&call.id, &call.name, arguments));
    }

    if let Some(artifact_references) = item
        .meta
        .as_ref()
        .and_then(|meta| meta.get("__stravia_artifact_references"))
    {
        values.push(serde_json::json!({
            "role": "assistant",
            "artifact_references": artifact_references,
        }));
    }
}

fn tool_output_history_values(item: &AiItem) -> Option<Vec<serde_json::Value>> {
    if let MessageContent::Blocks(blocks) = &item.content
        && !blocks.is_empty()
        && blocks
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
    {
        return Some(
            blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => Some(tool_output_value(tool_use_id, content.clone(), *is_error)),
                    _ => None,
                })
                .collect(),
        );
    }
    if item.role != super::Role::Tool {
        return None;
    }
    let call_id = item.tool_call_id.as_deref()?;
    let output = match &item.content {
        MessageContent::Text(text) => serde_json::Value::String(text.as_ref().clone()),
        MessageContent::Blocks(blocks) => {
            let mut values = blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Unknown { raw } => raw.clone(),
                    other => history_content_block_value(other),
                })
                .collect::<Vec<_>>();
            if values.len() == 1 {
                values.pop().expect("one projected tool output block")
            } else {
                serde_json::Value::Array(values)
            }
        }
    };
    Some(vec![tool_output_value(call_id, output, None)])
}

fn tool_output_value(
    call_id: &str,
    output: serde_json::Value,
    is_error: Option<bool>,
) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("role".into(), serde_json::json!("tool")),
        ("tool_call_id".into(), serde_json::json!(call_id)),
        ("output".into(), output),
        ("is_error".into(), serde_json::json!(is_error)),
    ]))
}

fn assistant_content_value(content: serde_json::Value) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("role".into(), serde_json::json!("assistant")),
        ("content".into(), content),
    ]))
}

fn reasoning_value(text: String, encrypted_content: Option<&str>) -> serde_json::Value {
    assistant_content_value(serde_json::Value::Object(serde_json::Map::from_iter([
        ("type".into(), serde_json::json!("reasoning")),
        (
            "summary".into(),
            serde_json::Value::Array(vec![serde_json::Value::String(text)]),
        ),
        ("content".into(), serde_json::Value::Array(Vec::new())),
        (
            "encrypted_content".into(),
            serde_json::json!(encrypted_content),
        ),
    ])))
}

fn tool_call_value(id: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("role".into(), serde_json::json!("assistant")),
        (
            "tool_call".into(),
            serde_json::Value::Object(serde_json::Map::from_iter([
                ("id".into(), serde_json::json!(id)),
                ("name".into(), serde_json::json!(name)),
                ("arguments".into(), arguments),
            ])),
        ),
    ]))
}

/// Compares the same semantic projection used by history fingerprints.
///
/// Typed content, role order, media identity and native state are retained;
/// delivery metadata and cache policy are not. Unclassified additive wire
/// fields remain significant until explicitly classified as non-semantic.
pub fn history_items_equal(left: &[AiItem], right: &[AiItem]) -> bool {
    history_item_refs_equal(left, right)
}

/// Compare borrowed sequences with the same canonical units as the slice entry
/// point. Shared histories need not clone items into a contiguous owned slice.
pub fn history_item_refs_equal<'a>(
    left: impl IntoIterator<Item = &'a AiItem>,
    right: impl IntoIterator<Item = &'a AiItem>,
) -> bool {
    left.into_iter()
        .flat_map(history_units)
        .eq(right.into_iter().flat_map(history_units))
}

pub fn append_history_context_hash(previous: &[u8; 32], item: &AiItem) -> [u8; 32] {
    append_history_context_hash_with_count(previous, item).0
}

/// Internal workspace projection: count and digest share the same borrowed units.
pub fn append_history_context_hash_with_count(
    previous: &[u8; 32],
    item: &AiItem,
) -> ([u8; 32], usize) {
    let units = history_units(item);
    let count = units.len();
    let hash = units.into_iter().fold(*previous, |previous, value| {
        append_history_serialized_hash(previous, &value)
    });
    (hash, count)
}

pub fn history_context_hash(items: &[AiItem]) -> [u8; 32] {
    history_context_hash_with_count(items).0
}

/// Internal workspace projection for immutable history snapshots.
pub fn history_context_hash_with_count(items: &[AiItem]) -> ([u8; 32], usize) {
    let mut digest = hash_bytes(b"stravia-generation-chain-history-v1");
    let mut count = 0;
    for item in items {
        let (next, units) = append_history_context_hash_with_count(&digest, item);
        digest = next;
        count += units;
    }
    (digest, count)
}

/// Folds one already-owned canonical unit value into a running context hash.
/// For typed items, prefer [`append_history_context_hash_with_count`].
pub fn append_history_value_hash(previous: [u8; 32], value: serde_json::Value) -> [u8; 32] {
    append_history_serialized_hash(previous, &value)
}

fn append_history_serialized_hash(
    previous: [u8; 32],
    value: &(impl serde::Serialize + ?Sized),
) -> [u8; 32] {
    // 长度前缀属于既有持久化指纹契约；先计数，再直接散列相同 JSON 字节。
    let length = crate::json::serialized_len(&value)
        .expect("model-visible provider-context unit must serialize as JSON");
    let mut hasher = Sha256::new();
    hasher.update(b"stravia-generation-chain-history-v1\0");
    hasher.update(previous);
    hasher.update((length as u64).to_be_bytes());
    let mut writer = HashWriter(hasher);
    serde_json::to_writer(&mut writer, &value)
        .expect("model-visible provider-context unit must serialize as JSON");
    writer.0.finalize().into()
}

/// Fingerprints material and controls that can change Provider prompt-cache
/// matching. The projection is intentionally positive: new request fields do
/// not become cache identity until their Provider semantics are classified.
pub fn cache_controls_hash(request: &AiRequest) -> [u8; 32] {
    request_controls_hash(request, true)
}

pub fn history_request_controls_hash(request: &AiRequest) -> [u8; 32] {
    request_controls_hash(request, false)
}

fn request_controls_hash(request: &AiRequest, cache: bool) -> [u8; 32] {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, &RequestControls { request, cache })
        .expect("request controls must serialize as JSON");
    writer.0.finalize().into()
}

struct RequestControls<'a> {
    request: &'a AiRequest,
    cache: bool,
}

impl serde::Serialize for RequestControls<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let r = self.request;
        let p = &r.parameters;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("model", &r.model)?;
        map.serialize_entry("instructions", &r.instructions)?;
        map.serialize_entry(
            "tools",
            &r.tools
                .as_deref()
                .filter(|tools| self.cache || !tools.is_empty())
                .map(ControlTools),
        )?;
        map.serialize_entry("tool_choice", &r.tool_choice)?;
        map.serialize_entry("parallel_tool_calls", &r.parallel_tool_calls)?;
        map.serialize_entry("reasoning", &r.reasoning)?;
        map.serialize_entry("response_format", &r.response_format)?;
        macro_rules! optional {
            ($name:literal, $value:expr) => {
                if let Some(value) = $value {
                    map.serialize_entry($name, value)?;
                }
            };
        }
        if !self.cache {
            optional!("temperature", &r.generation.temperature);
            optional!("top_p", &r.generation.top_p);
            optional!("seed", &r.generation.seed);
            optional!("stop", &r.generation.stop);
            optional!("presence_penalty", &r.generation.presence_penalty);
            optional!("frequency_penalty", &r.generation.frequency_penalty);
            optional!("embedding", &r.embedding);
            // Sampling and output scores affect history identity, not the
            // prompt prefix eligible for reuse on the same Target.
            optional!("top_k", &p.top_k);
            optional!("min_p", &p.min_p);
            optional!("repetition_penalty", &p.repetition_penalty);
            optional!("candidate_count", &p.candidate_count);
            if let Some(bias) = &p.logit_bias {
                // HashMap 的顺序不是契约；只排序借用，不复制参数正文。
                let sorted: std::collections::BTreeMap<_, _> = bias.iter().collect();
                map.serialize_entry("logit_bias", &sorted)?;
            }
            optional!("logprobs", &p.logprobs);
        }
        optional!("modalities", &p.modalities);
        optional!("audio", &p.audio);
        optional!("verbosity", &p.verbosity);
        optional!("max_tool_calls", &p.max_tool_calls);
        optional!("truncation", &p.truncation);
        optional!("service_tier", &p.service_tier);
        optional!("generate", &p.generate);
        if !p.native_tools.is_empty() {
            map.serialize_entry("native_tools", &p.native_tools)?;
        }
        optional!("compaction", &p.compaction);
        if self.cache {
            optional!("cache_key", &p.cache.key);
            optional!("cache_retention", &p.cache.retention);
        }
        map.end()
    }
}

struct ControlTools<'a>(&'a [ToolSpec]);

impl serde::Serialize for ControlTools<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        #[derive(serde::Serialize)]
        struct Tool<'a> {
            name: &'a str,
            description: &'a Option<String>,
            parameters: &'a serde_json::Value,
            strict: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            defer_loading: Option<bool>,
        }
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for tool in self.0 {
            sequence.serialize_element(&Tool {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
                strict: tool.strict,
                defer_loading: tool.defer_loading,
            })?;
        }
        sequence.end()
    }
}

pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn hash_hex(hash: &[u8; 32]) -> String {
    let mut output = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn history_content_block_value(block: &ContentBlock) -> serde_json::Value {
    match block {
        ContentBlock::Text { text, .. } => {
            serde_json::json!({ "type": "text", "text": text })
        }
        ContentBlock::Image { source, detail, .. } => {
            serde_json::Value::Object(serde_json::Map::from_iter([
                ("type".into(), serde_json::json!("image")),
                ("source".into(), media_source_value(source)),
                ("detail".into(), serde_json::json!(detail)),
            ]))
        }
        ContentBlock::Audio { source } => serde_json::Value::Object(serde_json::Map::from_iter([
            ("type".into(), serde_json::json!("audio")),
            ("source".into(), media_source_value(source)),
        ])),
        ContentBlock::File { source, media_type } => {
            serde_json::Value::Object(serde_json::Map::from_iter([
                ("type".into(), serde_json::json!("file")),
                ("source".into(), media_source_value(source)),
                ("media_type".into(), serde_json::json!(media_type)),
            ]))
        }
        ContentBlock::Video { source, media_type } => {
            serde_json::Value::Object(serde_json::Map::from_iter([
                ("type".into(), serde_json::json!("video")),
                ("source".into(), media_source_value(source)),
                ("media_type".into(), serde_json::json!(media_type)),
            ]))
        }
        ContentBlock::Thinking {
            thinking,
            signature,
        } => serde_json::json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature,
        }),
        ContentBlock::Reasoning {
            summary,
            content,
            encrypted_content,
        } => serde_json::json!({
            "type": "reasoning",
            "summary": summary,
            "content": content,
            "encrypted_content": encrypted_content,
        }),
        ContentBlock::Compaction { encrypted_content } => serde_json::json!({
            "type": "compaction", "encrypted_content": encrypted_content,
        }),
        ContentBlock::CompactionTrigger {} => serde_json::json!({"type": "compaction_trigger"}),
        ContentBlock::RedactedThinking { data } => serde_json::json!({
            "type": "redacted_thinking",
            "data": data,
        }),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::ServerToolUse {
            id,
            name,
            input,
            server_type,
            ..
        } => serde_json::json!({
            "type": "server_tool_use",
            "id": id,
            "name": name,
            "input": input,
            "server_type": server_type,
        }),
        ContentBlock::ServerToolResult {
            tool_use_id,
            content,
            server_type,
            ..
        } => serde_json::json!({
            "type": "server_tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "server_type": server_type,
        }),
        ContentBlock::Document {
            source,
            title,
            context,
            ..
        } => serde_json::Value::Object(serde_json::Map::from_iter([
            ("type".into(), serde_json::json!("document")),
            ("source".into(), history_document_source_value(source)),
            ("title".into(), serde_json::json!(title)),
            ("context".into(), serde_json::json!(context)),
        ])),
        ContentBlock::SearchResult {
            content,
            source,
            title,
            ..
        } => serde_json::Value::Object(serde_json::Map::from_iter([
            ("type".into(), serde_json::json!("search_result")),
            (
                "content".into(),
                serde_json::Value::Array(content.iter().map(history_content_block_value).collect()),
            ),
            ("source".into(), serde_json::json!(source)),
            ("title".into(), serde_json::json!(title)),
        ])),
        ContentBlock::ContainerUpload { file_id, .. } => serde_json::json!({
            "type": "container_upload",
            "file_id": file_id,
        }),
        ContentBlock::Citation { cited_text, source } => serde_json::json!({
            "type": "citation",
            "cited_text": cited_text,
            "source": source,
        }),
        ContentBlock::ExecutableCode { code, language, id } => serde_json::json!({
            "type": "executable_code",
            "code": code,
            "language": language,
            "id": id,
        }),
        ContentBlock::CodeExecutionResult {
            return_code,
            stdout,
            stderr,
            id,
        } => serde_json::json!({
            "type": "code_execution_result",
            "return_code": return_code,
            "stdout": stdout,
            "stderr": stderr,
            "id": id,
        }),
        ContentBlock::Refusal { refusal } => serde_json::json!({
            "type": "refusal",
            "refusal": refusal,
        }),
        ContentBlock::Unknown { raw } => serde_json::json!({
            "type": "unknown",
            "raw": raw,
        }),
    }
}

fn history_document_source_value(source: &DocumentSource) -> serde_json::Value {
    match source {
        DocumentSource::Base64Pdf { data } => {
            serde_json::json!({ "type": "base64_pdf", "data": data })
        }
        DocumentSource::PlainText { data } => {
            serde_json::json!({ "type": "plain_text", "data": data })
        }
        DocumentSource::Url(url) => serde_json::json!({ "type": "url", "url": url }),
        DocumentSource::Blocks { content } => {
            serde_json::Value::Object(serde_json::Map::from_iter([
                ("type".into(), serde_json::json!("blocks")),
                (
                    "content".into(),
                    serde_json::Value::Array(
                        content.iter().map(history_content_block_value).collect(),
                    ),
                ),
            ]))
        }
    }
}

fn media_source_value(source: &MediaSource) -> serde_json::Value {
    match source {
        MediaSource::Base64 { media_type, data } => serde_json::json!({
            "type": "base64",
            "media_type": media_type,
            "data": data,
        }),
        MediaSource::Url(url) => serde_json::json!({
            "type": "url",
            "url": url,
        }),
        MediaSource::FileId { file_id, detail } => serde_json::json!({
            "type": "file_id",
            "file_id": file_id,
            "detail": detail,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{
        AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, ContentBlock, MessageContent, Role,
        ToolCall,
    };

    #[test]
    fn legacy_activation_hint_is_not_an_independent_request_control() {
        let mut canonical = AiRequest::new("model", Vec::new());
        canonical.reasoning.level = Some(crate::thinking::ThinkingLevel::High);
        let mut legacy = canonical.clone();
        legacy.reasoning = serde_json::from_value(serde_json::json!({
            "level": "high", "enabled": true, "effort": "low"
        }))
        .unwrap();
        assert_eq!(
            cache_controls_hash(&canonical),
            cache_controls_hash(&legacy)
        );
        assert_eq!(
            history_request_controls_hash(&canonical),
            history_request_controls_hash(&legacy)
        );
        legacy.reasoning.mode = Some("disabled".into());
        assert_ne!(
            cache_controls_hash(&canonical),
            cache_controls_hash(&legacy)
        );
    }

    fn assert_borrowed_history_matches_owned(items: &[AiItem]) {
        let mut expected = hash_bytes(b"stravia-generation-chain-history-v1");
        let mut count = 0;
        let mut hashes = Vec::new();
        for item in items {
            let owned = history_item_values(item);
            assert_eq!(
                serde_json::to_vec(&history_units(item)).unwrap(),
                serde_json::to_vec(&owned).unwrap(),
            );
            assert_eq!(
                item_hash(item),
                json_hash(&serde_json::Value::Array(owned.clone()))
            );
            for value in owned {
                // Original persisted contract: hash the length-prefixed owned
                // projection bytes, independently of the new streaming writer.
                let bytes = serde_json::to_vec(&value).unwrap();
                let mut hasher = Sha256::new();
                hasher.update(b"stravia-generation-chain-history-v1\0");
                hasher.update(expected);
                hasher.update((bytes.len() as u64).to_be_bytes());
                hasher.update(&bytes);
                expected = hasher.finalize().into();
                hashes.push(json_hash(&value));
                count += 1;
            }
        }
        assert_eq!(history_context_hash(items), expected);
        assert_eq!(history_context_hash_with_count(items), (expected, count));
        let mut incremental = hash_bytes(b"stravia-generation-chain-history-v1");
        let mut incremental_count = 0;
        for item in items {
            let (next, units) = append_history_context_hash_with_count(&incremental, item);
            incremental = next;
            incremental_count += units;
        }
        assert_eq!((incremental, incremental_count), (expected, count));
        assert_eq!(history_unit_count(items), count);
        assert_eq!(item_hashes(items), hashes);
    }

    #[test]
    fn counted_history_matches_owned_bytes_for_empty_and_large_snapshots() {
        assert_borrowed_history_matches_owned(&[]);
        for length in [0, 255, 256, 65_536] {
            let item = AiItem::output_text("\"\\\n中文🙂".repeat(length));
            assert_borrowed_history_matches_owned(std::slice::from_ref(&item));
            let mut changed = item.clone();
            changed.content = MessageContent::Text("rewritten".to_owned().into());
            assert_borrowed_history_matches_owned(std::slice::from_ref(&changed));
            if length != 0 {
                assert_ne!(
                    history_context_hash_with_count(std::slice::from_ref(&item)).0,
                    history_context_hash_with_count(std::slice::from_ref(&changed)).0,
                );
            }
        }
    }

    #[test]
    fn borrowed_text_history_matches_owned_bytes_for_roles_and_metadata() {
        for role in [
            Role::System,
            Role::Developer,
            Role::User,
            Role::Assistant,
            Role::Tool,
        ] {
            for text in [
                "",
                " \t\r\n ",
                "中文🙂 quote:\" slash:\\ controls:\0\u{8}\u{c}",
            ] {
                for meta in [
                    None,
                    Some(serde_json::json!({"metadata": {"ignored": true}})),
                    Some(serde_json::json!({"__stravia_artifact_references": [{"id": "a"}]})),
                    Some(
                        serde_json::json!({"__open_responses_item_fields": {"metadata": {"ignored": true}}}),
                    ),
                    Some(
                        serde_json::json!({"__open_responses_item_fields": {"z": "native", "a": [1, null]}}),
                    ),
                    Some(serde_json::json!({"__open_responses_item_fields": "opaque"})),
                ] {
                    let plain = AiItem {
                        role,
                        content: MessageContent::Text(text.to_owned().into()),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: meta.map(|value| Box::new(value.into())),
                    };
                    let mut calls = plain.clone();
                    calls.tool_calls = Some(vec![
                        ToolCall {
                            id: "call_z".into(),
                            name: "z".into(),
                            arguments: "{\"z\":1,\"a\":2}".into(),
                        },
                        ToolCall {
                            id: "call_a".into(),
                            name: "a".into(),
                            arguments: "invalid\n🙂".into(),
                        },
                    ]);
                    calls.tool_call_id = Some("output".into());
                    let mut blocks = plain.clone();
                    blocks.content = MessageContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "".to_owned().into(),
                            cache_control: None,
                        },
                        ContentBlock::Text {
                            text: text.to_owned().into(),
                            cache_control: None,
                        },
                    ]);
                    assert_borrowed_history_matches_owned(&[plain.clone(), calls, blocks.clone()]);
                    assert_eq!(
                        history_items_equal(
                            std::slice::from_ref(&plain),
                            std::slice::from_ref(&blocks)
                        ),
                        history_item_values(&plain) == history_item_values(&blocks),
                    );
                    let mut changed = plain.clone();
                    changed.content = MessageContent::Text(format!("{text}!").into());
                    assert!(!history_items_equal(
                        std::slice::from_ref(&plain),
                        std::slice::from_ref(&changed)
                    ));
                    assert!(history_items_equal(
                        std::slice::from_ref(&plain),
                        std::slice::from_ref(&plain)
                    ));
                }
            }
        }
    }

    #[test]
    fn history_equality_normalizes_missing_and_null_artifact_references() {
        for role in [Role::System, Role::Developer, Role::User, Role::Tool] {
            let absent = AiItem {
                role,
                content: MessageContent::Text("text🙂\n\"".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            };
            let mut null = absent.clone();
            null.meta = Some(Box::new(
                serde_json::json!({"__stravia_artifact_references": null}).into(),
            ));
            let mut blocks = null.clone();
            blocks.content = MessageContent::Blocks(vec![ContentBlock::Text {
                text: "text🙂\n\"".to_owned().into(),
                cache_control: None,
            }]);
            assert_eq!(item_hash(&absent), item_hash(&null));
            assert!(history_items_equal(
                std::slice::from_ref(&absent),
                std::slice::from_ref(&null),
            ));
            assert!(history_items_equal(
                std::slice::from_ref(&absent),
                std::slice::from_ref(&blocks),
            ));
            let mut referenced = null.clone();
            referenced.meta = Some(Box::new(
                serde_json::json!({"__stravia_artifact_references": ["artifact"]}).into(),
            ));
            assert!(!history_items_equal(&[absent], &[referenced]));
        }
    }

    #[test]
    fn borrowed_history_retains_complex_fallback_and_unit_order() {
        let mut assistant = item(ContentBlock::Thinking {
            thinking: "thought🙂".to_owned(),
            signature: Some("signed".into()),
        });
        assistant.role = Role::Assistant;
        assistant.content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "before ".to_owned().into(),
                cache_control: None,
            },
            ContentBlock::Thinking {
                thinking: "thought🙂".to_owned(),
                signature: Some("signed".into()),
            },
            ContentBlock::Text {
                text: " after".to_owned().into(),
                cache_control: None,
            },
        ]);
        let media = item(ContentBlock::Image {
            source: MediaSource::Base64 {
                media_type: "image/png".into(),
                data: "payload".into(),
            },
            detail: Some("high".into()),
            cache_control: None,
        });
        let native = item(ContentBlock::Compaction {
            encrypted_content: "opaque-state".into(),
        });
        let tool =
            AiItem::function_call_output("call", serde_json::json!({"nested": ["🙂", null]}));
        assert_borrowed_history_matches_owned(&[assistant, media, native, tool]);
    }

    #[test]
    fn persisted_history_hash_preserves_json_byte_contract() {
        let previous = history_context_hash(&[]);
        for (value, expected) in [
            (
                serde_json::Value::Null,
                "50ad83999d56ad8c839dd3ff6536fc7cfb1a4d87ca8f7ef6b993099f85465b11",
            ),
            (
                serde_json::Value::String("\n🙂".repeat(16_384)),
                "931b9183b5e5db73b8982f96bb77df3ae0970d10fe864dc8d16d84a7ae53633f",
            ),
            (
                serde_json::Value::String(
                    "quote:\" slash:\\ controls:\u{8}\u{c}\r\t\0 中文".into(),
                ),
                "e5e34a9c7c0a532c566f892b4638382501a80f51034f91d025a90334d5debaff",
            ),
        ] {
            assert_eq!(
                hash_hex(&append_history_value_hash(previous, value)),
                expected
            );
        }
    }

    fn item(content: ContentBlock) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![content]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    #[test]
    fn nested_projection_preserves_native_fields_and_json_bytes() {
        let mut item = item(ContentBlock::Document {
            source: DocumentSource::Blocks {
                content: vec![ContentBlock::Image {
                    source: MediaSource::Base64 {
                        media_type: "image/png".into(),
                        data: "payload\n🙂".into(),
                    },
                    detail: Some("high".into()),
                    cache_control: None,
                }],
            },
            title: Some("title".into()),
            context: Some("context".into()),
            cache_control: None,
        });
        item.meta = Some(Box::new(
            serde_json::json!({
                "__open_responses_item_fields": {
                    "metadata": {"ignored": true},
                    "internal_chat_message_metadata_passthrough": {"ignored": true},
                    "extension": {"z": [null, "opaque"], "a": 1},
                },
                "__stravia_artifact_references": [{"id": "artifact"}],
            })
            .into(),
        ));
        let expected = serde_json::json!([{
            "items": [{
                "role": "user",
                "content": [{
                    "type": "document",
                    "source": {
                        "type": "blocks",
                        "content": [{
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "payload\n🙂",
                            },
                            "detail": "high",
                        }],
                    },
                    "title": "title",
                    "context": "context",
                }],
                "tool_calls": null,
                "tool_call_id": null,
                "artifact_references": [{"id": "artifact"}],
            }],
            "native_item_fields": {"extension": {"z": [null, "opaque"], "a": 1}},
        }]);
        assert_eq!(item_value(&item), expected);
        assert_eq!(item_hash(&item), json_hash(&expected));
        assert_eq!(
            history_context_hash(std::slice::from_ref(&item)),
            append_history_value_hash(history_context_hash(&[]), expected[0].clone()),
        );
    }

    #[test]
    fn text_projection_preserves_empty_blocks_role_and_escaping() {
        for role in [Role::User, Role::Assistant] {
            for text in ["", "quote:\" slash:\\\n🙂"] {
                let plain = AiItem {
                    role,
                    content: MessageContent::Text(text.to_owned().into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                };
                let mut blocks = plain.clone();
                blocks.content = MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "".to_owned().into(),
                        cache_control: None,
                    },
                    ContentBlock::Text {
                        text: text.to_owned().into(),
                        cache_control: None,
                    },
                ]);
                assert_eq!(item_value(&plain), item_value(&blocks));
                assert_eq!(item_hash(&plain), item_hash(&blocks));
                assert_eq!(
                    history_context_hash(std::slice::from_ref(&plain)),
                    history_context_hash(std::slice::from_ref(&blocks)),
                );
                if text.is_empty() {
                    blocks.content = MessageContent::Blocks(Vec::new());
                    assert_eq!(item_value(&plain), item_value(&blocks));
                }
            }
        }
    }

    #[test]
    fn tool_projection_preserves_single_multiple_and_invalid_arguments() {
        let output = serde_json::json!({"z": ["nested", null], "a": {"escaped": "\n🙂"}});
        let single = AiItem::function_call_output("call_1", output.clone());
        assert_eq!(
            history_item_values(&single),
            vec![serde_json::json!({
                "role": "tool", "tool_call_id": "call_1", "output": output, "is_error": null,
            })],
        );
        let mut unknown = single.clone();
        unknown.content = MessageContent::Blocks(vec![ContentBlock::Unknown {
            raw: output.clone(),
        }]);
        assert_eq!(history_item_values(&single), history_item_values(&unknown));
        let mut multiple = single.clone();
        multiple.content = MessageContent::Blocks(vec![
            ContentBlock::Unknown {
                raw: serde_json::json!({"a": 1}),
            },
            ContentBlock::Unknown {
                raw: serde_json::json!({"b": 2}),
            },
        ]);
        assert_eq!(
            history_item_values(&multiple),
            vec![serde_json::json!({
                "role": "tool", "tool_call_id": "call_1",
                "output": [{"a": 1}, {"b": 2}], "is_error": null,
            })],
        );
        let call = AiItem::function_call(ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{not-json\n🙂".into(),
        });
        assert_eq!(
            history_item_values(&call),
            vec![serde_json::json!({
                "role": "assistant",
                "tool_call": {"id": "call_1", "name": "lookup", "arguments": "{not-json\n🙂"},
            })],
        );
    }

    #[test]
    fn media_identity_changes_item_hash() {
        let left = item(ContentBlock::Image {
            source: MediaSource::Url("https://example.test/a.png".into()),
            detail: None,
            cache_control: None,
        });
        let right = item(ContentBlock::Image {
            source: MediaSource::Url("https://example.test/b.png".into()),
            detail: None,
            cache_control: None,
        });
        assert_ne!(item_hash(&left), item_hash(&right));
    }

    #[test]
    fn url_media_variants_have_stable_history_hashes() {
        for block in [
            ContentBlock::Audio {
                source: MediaSource::Url("https://example.test/audio.mp3".into()),
            },
            ContentBlock::File {
                source: MediaSource::Url("https://example.test/document.txt".into()),
                media_type: Some("text/plain".into()),
            },
            ContentBlock::Video {
                source: MediaSource::Url("https://example.test/video.mp4".into()),
                media_type: Some("video/mp4".into()),
            },
        ] {
            let value = history_item_value(&item(block));
            assert!(value["content"][0]["source"]["url"].is_string());
        }
    }

    #[test]
    fn canonical_sampling_controls_change_history_but_not_cache_identity() {
        let baseline = AiRequest::new("model", Vec::new());
        let mutations: [fn(&mut crate::protocol::ir::RequestParameters); 6] = [
            |p| p.top_k = Some(8),
            |p| p.min_p = Some(0.1),
            |p| p.repetition_penalty = Some(1.2),
            |p| p.candidate_count = Some(2),
            |p| p.logit_bias = Some([("token".into(), 0.5)].into_iter().collect()),
            |p| p.logprobs = Some(3),
        ];
        for mutate in mutations {
            let mut changed = baseline.clone();
            mutate(&mut changed.parameters);
            assert_eq!(
                cache_controls_hash(&baseline),
                cache_controls_hash(&changed)
            );
            assert_ne!(
                history_request_controls_hash(&baseline),
                history_request_controls_hash(&changed)
            );
        }
    }

    #[test]
    fn cache_key_is_cache_policy_not_history_identity() {
        let request_with = |key: Option<&str>| {
            let mut request = AiRequest::new("model", Vec::new());
            request.parameters.cache.key = key.map(str::to_owned);
            request
        };
        let without_cache = request_with(None);
        let with_cache = request_with(Some("cache-bucket"));

        assert_eq!(
            history_request_controls_hash(&without_cache),
            history_request_controls_hash(&with_cache)
        );
        assert_ne!(
            cache_controls_hash(&without_cache),
            cache_controls_hash(&with_cache)
        );
    }

    #[test]
    fn unified_native_and_output_controls_change_canonical_identity() {
        use crate::protocol::ir::{
            AudioOutput, CodeExecutionTool, CompactionControl, Modality, NativeTool, ServiceTier,
            Truncation, Verbosity,
        };

        let baseline = AiRequest::new("model", Vec::new());
        let mutations: [fn(&mut crate::protocol::ir::RequestParameters); 9] = [
            |p| p.modalities = Some(vec![Modality::Audio]),
            |p| {
                p.audio = Some(AudioOutput {
                    voice: Some("voice".into()),
                    format: Some("pcm".into()),
                })
            },
            |p| p.verbosity = Some(Verbosity::High),
            |p| p.max_tool_calls = Some(2),
            |p| p.truncation = Some(Truncation::Auto),
            |p| p.service_tier = Some(ServiceTier::Priority),
            |p| p.generate = Some(false),
            |p| {
                p.native_tools = vec![NativeTool::CodeExecution(CodeExecutionTool {
                    container: Some("container".into()),
                })]
            },
            |p| {
                p.compaction = Some(CompactionControl {
                    threshold_tokens: Some(1_000),
                })
            },
        ];
        for mutate in mutations {
            let mut changed = baseline.clone();
            mutate(&mut changed.parameters);
            assert_ne!(
                cache_controls_hash(&baseline),
                cache_controls_hash(&changed)
            );
            assert_ne!(
                history_request_controls_hash(&baseline),
                history_request_controls_hash(&changed)
            );
        }
    }

    #[test]
    fn unified_reasoning_summary_and_schema_description_change_identity() {
        use crate::protocol::ir::{ReasoningConfig, ReasoningSummary, ResponseFormat};

        let mut baseline = AiRequest::new("model", Vec::new());
        baseline.reasoning = ReasoningConfig {
            mode: Some("enabled".into()),
            budget_tokens: None,
            summary: Some(ReasoningSummary::Auto),
            level: None,
            target_control: None,
        };
        baseline.response_format = Some(ResponseFormat::JsonSchema {
            name: "answer".into(),
            description: Some("Original description".into()),
            schema: serde_json::json!({"type": "object"}),
            strict: Some(true),
        });
        let mut changed_summary = baseline.clone();
        changed_summary.reasoning.summary = Some(ReasoningSummary::None);
        let mut changed_description = baseline.clone();
        if let Some(ResponseFormat::JsonSchema { description, .. }) =
            &mut changed_description.response_format
        {
            *description = Some("Different description".into());
        }
        for changed in [&changed_summary, &changed_description] {
            assert_ne!(cache_controls_hash(&baseline), cache_controls_hash(changed));
            assert_ne!(
                history_request_controls_hash(&baseline),
                history_request_controls_hash(changed)
            );
        }
    }

    #[test]
    fn appended_context_hash_matches_the_complete_prefix() {
        let items = [
            item(ContentBlock::Text {
                text: std::sync::Arc::new("first".into()),
                cache_control: None,
            }),
            item(ContentBlock::Text {
                text: std::sync::Arc::new("second".into()),
                cache_control: None,
            }),
        ];

        assert_eq!(
            append_history_context_hash(&history_context_hash(&items[..1]), &items[1]),
            history_context_hash(&items),
        );
    }

    #[test]
    fn provider_context_projection_whitelists_only_model_semantics() {
        let response_items = vec![
            AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("encrypted".into()))
                .with_graph_metadata(
                    Some("rs_provider".into()),
                    Some(AiItemStatus::Completed),
                    AiItemProvenance::Provider,
                    AiItemAudience::Client,
                ),
            AiItem::output_text("answer").with_graph_metadata(
                Some("msg_provider".into()),
                Some(AiItemStatus::Completed),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            ),
            AiItem::function_call(ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            })
            .with_graph_metadata(
                Some("fc_provider".into()),
                Some(AiItemStatus::Completed),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            ),
            AiItem::function_call_output("call_1", serde_json::Value::String("result".into()))
                .with_graph_metadata(
                    Some("fco_provider".into()),
                    Some(AiItemStatus::Completed),
                    AiItemProvenance::Client,
                    AiItemAudience::Provider,
                ),
        ];
        let replay_items = vec![
            AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("encrypted".into())),
            AiItem::output_text("answer"),
            AiItem::function_call(ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            }),
            AiItem::function_call_output("call_1", serde_json::Value::String("result".into())),
        ];

        assert!(history_items_equal(&response_items, &replay_items));

        for changed in [
            vec![
                AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("different".into())),
                replay_items[1].clone(),
                replay_items[2].clone(),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                AiItem::output_text("different"),
                replay_items[2].clone(),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                AiItem::function_call(ToolCall {
                    id: "call_2".into(),
                    name: "lookup".into(),
                    arguments: "{\"value\":1}".into(),
                }),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                AiItem::function_call(ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{\"value\":2}".into(),
                }),
                replay_items[3].clone(),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                replay_items[2].clone(),
                AiItem::function_call_output("call_2", serde_json::Value::String("result".into())),
            ],
            vec![
                replay_items[0].clone(),
                replay_items[1].clone(),
                replay_items[2].clone(),
                AiItem::function_call_output(
                    "call_1",
                    serde_json::Value::String("different".into()),
                ),
            ],
        ] {
            assert!(!history_items_equal(&response_items, &changed));
        }

        assert!(!history_items_equal(
            &[item(ContentBlock::Text {
                text: "question".to_owned().into(),
                cache_control: None,
            })],
            &[item(ContentBlock::Text {
                text: "different".to_owned().into(),
                cache_control: None,
            })]
        ));
        assert!(history_items_equal(
            &[item(ContentBlock::Text {
                text: "question".to_owned().into(),
                cache_control: Some(crate::protocol::ir::CacheControl::ephemeral()),
            })],
            &[item(ContentBlock::Text {
                text: "question".to_owned().into(),
                cache_control: None,
            })]
        ));
    }

    #[test]
    fn anthropic_and_responses_outputs_share_history_and_cache_identity() {
        let anthropic_output =
            |reasoning: &str, signature: &str, text: &str, call_id: &str, input| {
                vec![AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Thinking {
                            thinking: reasoning.into(),
                            signature: Some(signature.into()),
                        },
                        ContentBlock::Text {
                            text: text.to_owned().into(),
                            cache_control: None,
                        },
                        ContentBlock::ToolUse {
                            id: call_id.into(),
                            name: "lookup".into(),
                            input,
                            cache_control: None,
                        },
                    ]),
                    tool_calls: Some(vec![ToolCall {
                        id: call_id.into(),
                        name: "lookup".into(),
                        arguments: "{\"value\":1}".into(),
                    }]),
                    tool_call_id: None,
                    meta: None,
                }]
            };
        let responses = vec![
            AiItem::thinking("summaryreasoning", Some("opaque".into())),
            AiItem::output_text("answer"),
            AiItem::function_call(ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            }),
        ];
        let anthropic = anthropic_output(
            "summaryreasoning",
            "opaque",
            "answer",
            "call_1",
            serde_json::json!({"value": 1}),
        );

        assert!(history_items_equal(&responses, &anthropic));
        assert_eq!(item_hashes(&responses), item_hashes(&anthropic));

        for changed in [
            anthropic_output(
                "different",
                "opaque",
                "answer",
                "call_1",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "different",
                "answer",
                "call_1",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "opaque",
                "different",
                "call_1",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "opaque",
                "answer",
                "call_2",
                serde_json::json!({"value": 1}),
            ),
            anthropic_output(
                "summaryreasoning",
                "opaque",
                "answer",
                "call_1",
                serde_json::json!({"value": 2}),
            ),
        ] {
            assert!(!history_items_equal(&responses, &changed));
            assert_ne!(item_hashes(&responses), item_hashes(&changed));
        }

        let responses_output = vec![AiItem::function_call_output(
            "call_1",
            serde_json::json!("result"),
        )];
        let anthropic_output = vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: serde_json::json!("result"),
                content_kind: Some(crate::protocol::ir::ToolResultContentKind::ContentBlocks),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            meta: None,
        }];
        assert!(history_items_equal(&responses_output, &anthropic_output));
        assert_eq!(
            item_hashes(&responses_output),
            item_hashes(&anthropic_output)
        );

        for changed in [
            AiItem::function_call_output("call_2", serde_json::json!("result")),
            AiItem::function_call_output("call_1", serde_json::json!("different")),
        ] {
            assert!(!history_items_equal(
                &anthropic_output,
                std::slice::from_ref(&changed)
            ));
        }
    }
}
