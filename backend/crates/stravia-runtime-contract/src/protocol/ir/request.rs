//! `AiRequest` — the unified ingress IR for all supported protocols.
//!
//! Codec decoders produce `AiRequest`; encoders and the dispatcher consume it.
//! Text buffers are shared across snapshots. Mutations detach the buffer, while
//! serialization retains the existing plain-string JSON representation.

use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};
use serde_json::{Map, Value};
use std::sync::Arc;

use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::cache::CacheControl;
use crate::protocol::ir::envelope::RawEnvelope;
use crate::protocol::ir::ext::ProtocolExt;
use crate::protocol::ir::native_tools::NativeToolKind;
use crate::protocol::ir::vendor_ext::VendorExtensions;
use crate::thinking::{TargetThinkingControl, ThinkingLevel};

pub const VERIFIED_HISTORY_REPLAY_META: &str = "__stravia_verified_history_replay";

// ── Role ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

// ── Image source ─────────────────────────────────────────────────────────────

/// The data source for an image or audio content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    /// Inline base64-encoded data.
    Base64 { media_type: String, data: String },
    /// A URL pointing to the media.
    Url(#[serde(with = "url_source")] String),
    /// A provider-side file reference.
    FileId {
        file_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

// ── Document source ───────────────────────────────────────────────────────────

/// Source for a document content block (Anthropic).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DocumentSource {
    Base64Pdf {
        data: String,
    },
    PlainText {
        data: String,
    },
    Url(#[serde(with = "url_source")] String),
    /// Content already stored as content blocks.
    Blocks {
        content: Vec<ContentBlock>,
    },
}

// Internally tagged variants require an object payload, not a bare URL string.
mod url_source {
    use serde::{Deserialize, Deserializer, Serializer, ser::SerializeStruct};

    pub fn serialize<S: Serializer>(url: &str, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = serializer.serialize_struct("UrlSource", 1)?;
        value.serialize_field("url", url)?;
        value.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        #[derive(Deserialize)]
        struct UrlSource {
            url: String,
        }
        Ok(UrlSource::deserialize(deserializer)?.url)
    }
}

// ── Content blocks ────────────────────────────────────────────────────────────

/// Tool payload interpretation is assigned by its producer, never inferred from
/// business JSON fields. Persist it so restored history keeps the same semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultContentKind {
    Json,
    ContentBlocks,
}

pub const TOOL_RESULT_CONTENT_KIND_META: &str = "__stravia_tool_result_content_kind";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    // ── Text ─────────────────────────────────────────────────────────────────
    Text {
        text: Arc<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },

    // ── Multimodal ───────────────────────────────────────────────────────────
    Image {
        source: MediaSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Audio {
        source: MediaSource,
    },
    File {
        source: MediaSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
    Video {
        source: MediaSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },

    // ── Reasoning / thinking ─────────────────────────────────────────────────
    /// Extended thinking output (Anthropic `ThinkingBlockParam`, Google `thought=true`).
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Dated Open Responses reasoning item with summary and full content kept distinct.
    Reasoning {
        summary: Vec<String>,
        content: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// Native Responses opaque context state, distinct from protected reasoning.
    Compaction {
        encrypted_content: String,
    },
    /// Codex remote-v2 request control at its position in the input window.
    CompactionTrigger {},
    /// Redacted thinking block (Anthropic `RedactedThinkingBlockParam`).
    RedactedThinking {
        data: String,
    },

    // ── Tool calls ────────────────────────────────────────────────────────────
    ToolUse {
        id: ToolCallId,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: ToolCallId,
        content: Value,
        /// Absent only in history written before payload semantics were recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_kind: Option<ToolResultContentKind>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },

    // ── Server-side tools (Anthropic) ────────────────────────────────────────
    /// A server-executed tool call (Anthropic `ServerToolUseBlockParam`,
    /// Google `Part.toolCall`).
    ServerToolUse {
        id: ToolCallId,
        /// Tool name (e.g. `"web_search"`, `"code_execution"`).
        name: String,
        input: Value,
        /// Discriminator for the tool type (e.g. `"web_search"`, `"bash"`).
        #[serde(skip_serializing_if = "Option::is_none")]
        server_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Result from a server-executed tool.
    ServerToolResult {
        tool_use_id: ToolCallId,
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_kind: Option<ToolResultContentKind>,
        /// Discriminator matching the originating `ServerToolUse.server_type`.
        #[serde(skip_serializing_if = "Option::is_none")]
        server_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },

    // ── Documents / search ────────────────────────────────────────────────────
    /// A document block (Anthropic `DocumentBlockParam`).
    Document {
        source: DocumentSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// A search result block (Anthropic `SearchResultBlockParam`).
    SearchResult {
        content: Vec<ContentBlock>,
        source: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },

    // ── Citations ─────────────────────────────────────────────────────────────
    /// A citation block (Anthropic citations, OpenAI Responses annotations).
    Citation {
        cited_text: String,
        source: Value,
    },

    // ── Code execution ───────────────────────────────────────────────────────
    /// Executable code produced by the model (Google `Part.executableCode`).
    ExecutableCode {
        code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Code execution result (Google `Part.codeExecutionResult`,
    /// Anthropic `CodeExecutionResultBlockParam`).
    CodeExecutionResult {
        return_code: i32,
        stdout: String,
        stderr: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    // ── Container ────────────────────────────────────────────────────────────
    /// Container file upload (Anthropic `ContainerUploadBlockParam`).
    ContainerUpload {
        file_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },

    // ── Refusal ───────────────────────────────────────────────────────────────
    /// Model refusal (OpenAI `content_filter` / Anthropic `stop_reason = "refusal"`).
    Refusal {
        refusal: String,
    },

    // ── Fallback ─────────────────────────────────────────────────────────────
    /// A raw JSON block that the codec does not understand.  Preserved for
    /// pass-through and future extension.
    Unknown {
        raw: Value,
    },
}

impl ContentBlock {
    pub fn as_text(&self) -> Option<&str> {
        if let Self::Text { text, .. } = self {
            Some(text)
        } else {
            None
        }
    }

    pub fn is_tool_use(&self) -> bool {
        matches!(self, Self::ToolUse { .. } | Self::ServerToolUse { .. })
    }

    pub fn is_tool_result(&self) -> bool {
        matches!(
            self,
            Self::ToolResult { .. } | Self::ServerToolResult { .. }
        )
    }
}

/// Message content — shared plain text or a typed block list.
/// Use `Arc::make_mut` before editing shared text; JSON remains a string.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(Arc<String>),
    Blocks(Vec<ContentBlock>),
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;

        impl<'de> serde::de::Visitor<'de> for ContentVisitor {
            type Value = MessageContent;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a text string or an array of content blocks")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(MessageContent::Text(value.to_owned().into()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                // Untagged buffering borrows an owned Value string and copies it.
                Ok(MessageContent::Text(value.into()))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                sequence: A,
            ) -> Result<Self::Value, A::Error> {
                Vec::<ContentBlock>::deserialize(serde::de::value::SeqAccessDeserializer::new(
                    sequence,
                ))
                .map(MessageContent::Blocks)
            }
        }

        deserializer.deserialize_any(ContentVisitor)
    }
}

impl MessageContent {
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(t) => t.as_ref().clone(),
            Self::Blocks(bs) => bs
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

// ── Message ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalItemId(String);

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemReference(String);

macro_rules! identity {
    ($name:ident) => {
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}
identity!(CanonicalItemId);
identity!(ToolCallId);
identity!(ItemReference);

impl std::borrow::Borrow<str> for ItemReference {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl std::borrow::Borrow<str> for ToolCallId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl std::ops::Deref for ToolCallId {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<str> for ToolCallId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
impl From<String> for ToolCallId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for ToolCallId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}
impl ToolCallId {
    pub fn into_string(self) -> String {
        self.0
    }
}
impl PartialEq<str> for ToolCallId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for ToolCallId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl PartialEq<String> for ToolCallId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<ToolCallId> for String {
    fn eq(&self, other: &ToolCallId) -> bool {
        self == other.as_str()
    }
}

/// Lossless item metadata. Object-shaped metadata has typed graph fields; unknown
/// fields and even malformed legacy graph fields remain byte-for-byte equivalent
/// JSON values. Legacy non-objects are untouched until graph metadata is written.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AiItemMetadata {
    id: Option<CanonicalItemId>,
    reference: Option<ItemReference>,
    status: Option<AiItemStatus>,
    provenance: Option<AiItemProvenance>,
    audience: Option<AiItemAudience>,
    extensions: Map<String, Value>,
    opaque: Option<Value>,
}

impl From<Value> for AiItemMetadata {
    fn from(value: Value) -> Self {
        let Value::Object(mut fields) = value else {
            return Self {
                opaque: Some(value),
                ..Self::default()
            };
        };
        let id = if fields
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        {
            match fields.remove("id") {
                Some(Value::String(id)) => Some(CanonicalItemId(id)),
                _ => unreachable!(),
            }
        } else {
            None
        };
        let reference = if fields
            .get("__open_responses_item_reference")
            .and_then(Value::as_str)
            .is_some_and(|reference| !reference.is_empty())
        {
            match fields.remove("__open_responses_item_reference") {
                Some(Value::String(reference)) => Some(ItemReference(reference)),
                _ => unreachable!(),
            }
        } else {
            None
        };
        let status = fields
            .get("status")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "in_progress" => Some(AiItemStatus::InProgress),
                "completed" => Some(AiItemStatus::Completed),
                "incomplete" => Some(AiItemStatus::Incomplete),
                "failed" => Some(AiItemStatus::Failed),
                _ => None,
            });
        if status.is_some() {
            fields.remove("status");
        }
        let provenance = fields
            .get("provenance")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "client" => Some(AiItemProvenance::Client),
                "provider" => Some(AiItemProvenance::Provider),
                "platform" => Some(AiItemProvenance::Platform),
                _ => None,
            });
        if provenance.is_some() {
            fields.remove("provenance");
        }
        let audience = fields
            .get("audience")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "client" => Some(AiItemAudience::Client),
                "provider" => Some(AiItemAudience::Provider),
                "internal" => Some(AiItemAudience::Internal),
                _ => None,
            });
        if audience.is_some() {
            fields.remove("audience");
        }
        Self {
            id,
            reference,
            status,
            provenance,
            audience,
            extensions: fields,
            opaque: None,
        }
    }
}

impl Serialize for AiItemMetadata {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(opaque) = &self.opaque {
            return opaque.serialize(serializer);
        }
        let count = self.extensions.len()
            + usize::from(self.reference.is_some())
            + usize::from(self.id.is_some())
            + usize::from(self.status.is_some())
            + usize::from(self.provenance.is_some())
            + usize::from(self.audience.is_some());
        let mut map = serializer.serialize_map(Some(count))?;
        for (key, value) in &self.extensions {
            map.serialize_entry(key, value)?;
        }
        if let Some(id) = &self.id {
            map.serialize_entry("id", id)?;
        }
        if let Some(reference) = &self.reference {
            map.serialize_entry("__open_responses_item_reference", reference)?;
        }
        if let Some(status) = &self.status {
            map.serialize_entry("status", status)?;
        }
        if let Some(provenance) = &self.provenance {
            map.serialize_entry("provenance", provenance)?;
        }
        if let Some(audience) = &self.audience {
            map.serialize_entry("audience", audience)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for AiItemMetadata {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from(Value::deserialize(deserializer)?))
    }
}

impl AiItemMetadata {
    pub fn boxed(value: Value) -> Box<Self> {
        Box::new(Self::from(value))
    }

    pub fn id(&self) -> Option<&CanonicalItemId> {
        self.id.as_ref()
    }
    pub fn reference(&self) -> Option<&ItemReference> {
        self.reference.as_ref()
    }
    pub fn status(&self) -> Option<AiItemStatus> {
        self.status
    }
    pub fn provenance(&self) -> Option<AiItemProvenance> {
        self.provenance
    }
    pub fn audience(&self) -> Option<AiItemAudience> {
        self.audience
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        if self.opaque.is_some() {
            return None;
        }
        self.extensions.get(key)
    }

    pub fn insert_extension(
        &mut self,
        key: impl Into<String>,
        value: Value,
    ) -> Result<Option<Value>, &'static str> {
        let key = key.into();
        if Self::is_graph_field(&key) {
            return Err("reserved item metadata field");
        }
        if self.opaque.is_some() {
            return Err("opaque item metadata requires an explicit graph write");
        }
        Ok(self.extensions.insert(key, value))
    }

    /// Core graph annotations may promote legacy metadata without discarding it.
    /// Ordinary protocol extensions must not implicitly change its JSON shape.
    pub fn insert_graph_extension(
        &mut self,
        key: impl Into<String>,
        value: Value,
    ) -> Result<Option<Value>, &'static str> {
        let key = key.into();
        if Self::is_graph_field(&key) {
            return Err("reserved item metadata field");
        }
        self.promote_opaque();
        Ok(self.extensions.insert(key, value))
    }

    fn is_graph_field(key: &str) -> bool {
        matches!(
            key,
            "id" | "status" | "provenance" | "audience" | "__open_responses_item_reference"
        )
    }

    fn promote_opaque(&mut self) {
        if let Some(opaque) = self.opaque.take() {
            self.extensions.insert("vendor_meta".into(), opaque);
        }
    }

    pub fn object_extensions(&self) -> Option<&Map<String, Value>> {
        self.opaque.is_none().then_some(&self.extensions)
    }

    /// Borrow legacy non-object metadata without promoting it into graph fields.
    pub fn opaque_value(&self) -> Option<&Value> {
        self.opaque.as_ref()
    }

    /// Consume the extension map without exposing typed graph annotations.
    /// Malformed legacy graph fields stay in this map; opaque non-objects have
    /// no object extensions.
    pub fn into_object_extensions(self) -> Option<Map<String, Value>> {
        self.opaque.is_none().then_some(self.extensions)
    }

    pub fn remove_extension(&mut self, key: &str) -> Result<Option<Value>, &'static str> {
        if Self::is_graph_field(key) {
            return Err("reserved item metadata field");
        }
        Ok(self.extensions.remove(key))
    }

    pub fn set_graph(
        &mut self,
        id: Option<CanonicalItemId>,
        status: Option<AiItemStatus>,
        provenance: AiItemProvenance,
        audience: AiItemAudience,
    ) {
        self.promote_opaque();
        self.extensions.remove("id");
        self.extensions.remove("__open_responses_item_reference");
        self.extensions.remove("status");
        if let Some(id) = id.filter(|id| !id.0.is_empty()) {
            self.id = Some(id);
        }
        if let Some(status) = status {
            self.status = Some(status);
        }
        self.extensions.remove("provenance");
        self.extensions.remove("audience");
        self.provenance = Some(provenance);
        self.audience = Some(audience);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiItem {
    pub role: Role,
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// The `tool_call_id` this result answers.  Required for `Role::Tool` messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Provider-specific extras for this individual message (e.g. Anthropic
    /// `cache_control` on `system` array items).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_item_meta"
    )]
    pub meta: Option<Box<AiItemMetadata>>,
}

fn deserialize_item_meta<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<AiItemMetadata>>, D::Error> {
    Ok(Some(AiItemMetadata::boxed(Value::deserialize(
        deserializer,
    )?)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiItemStatus {
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiItemProvenance {
    Client,
    Provider,
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiItemAudience {
    Client,
    Provider,
    Internal,
}

impl AiItemProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Provider => "provider",
            Self::Platform => "platform",
        }
    }
}

impl AiItemAudience {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Provider => "provider",
            Self::Internal => "internal",
        }
    }
}

impl AiItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }
}

impl AiItem {
    pub fn is_compaction(&self) -> bool {
        matches!(&self.content, MessageContent::Blocks(blocks)
            if matches!(blocks.as_slice(), [ContentBlock::Compaction { .. }]))
    }

    pub fn is_compaction_trigger(&self) -> bool {
        matches!(&self.content, MessageContent::Blocks(blocks)
            if matches!(blocks.as_slice(), [ContentBlock::CompactionTrigger {}]))
    }

    pub fn canonical_id(&self) -> Option<&CanonicalItemId> {
        self.meta
            .as_ref()?
            .id()
            .filter(|id| !id.as_str().is_empty())
    }

    pub fn id_ref(&self) -> Option<&str> {
        self.canonical_id().map(CanonicalItemId::as_str)
    }

    pub fn status(&self) -> Option<AiItemStatus> {
        self.meta.as_ref()?.status()
    }

    pub fn item_reference(&self) -> Option<&ItemReference> {
        self.meta.as_ref()?.reference()
    }

    pub fn with_graph_metadata(
        mut self,
        id: Option<String>,
        status: Option<AiItemStatus>,
        provenance: AiItemProvenance,
        audience: AiItemAudience,
    ) -> Self {
        self.set_graph_metadata(id, status, provenance, audience);
        self
    }

    pub fn set_graph_metadata(
        &mut self,
        id: Option<String>,
        status: Option<AiItemStatus>,
        provenance: AiItemProvenance,
        audience: AiItemAudience,
    ) {
        self.meta.get_or_insert_with(Default::default).set_graph(
            id.map(CanonicalItemId::new),
            status,
            provenance,
            audience,
        );
    }
    pub fn output_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Text(std::sync::Arc::new(text.into())),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    pub fn thinking(text: impl Into<String>, signature: Option<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::Thinking {
                thinking: text.into(),
                signature,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    pub fn reasoning(
        summary: Vec<String>,
        content: Vec<String>,
        encrypted_content: Option<String>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::Reasoning {
                summary,
                content,
                encrypted_content,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    pub fn refusal(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::Refusal {
                refusal: text.into(),
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    pub fn function_call(call: ToolCall) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Text(std::sync::Arc::new(String::new())),
            tool_calls: Some(vec![call]),
            tool_call_id: None,
            meta: None,
        }
    }

    /// Mark plain tool text only at a fresh producer boundary, never during replay.
    pub fn with_plain_tool_text_kind(mut self) -> Self {
        if self.role == Role::Tool && matches!(self.content, MessageContent::Text(_)) {
            self.meta
                .get_or_insert_with(Default::default)
                .insert_graph_extension(TOOL_RESULT_CONTENT_KIND_META, Value::String("json".into()))
                .expect("tool result kind key is not reserved");
        }
        self
    }

    pub fn function_call_output(call_id: impl Into<ToolCallId>, output: Value) -> Self {
        let call_id = call_id.into();
        let content = match output {
            Value::String(text) => MessageContent::Text(std::sync::Arc::new(text)),
            other => MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: call_id.clone(),
                content: other,
                content_kind: Some(ToolResultContentKind::Json),
                is_error: None,
                cache_control: None,
            }]),
        };
        Self {
            role: Role::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(call_id),
            meta: None,
        }
        .with_plain_tool_text_kind()
    }

    pub fn search_result(
        url: impl Into<String>,
        title: impl Into<String>,
        snippet: Option<String>,
    ) -> Self {
        let mut content = Vec::new();
        if let Some(snippet) = snippet {
            content.push(ContentBlock::Text {
                text: std::sync::Arc::new(snippet),
                cache_control: None,
            });
        }
        Self {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::SearchResult {
                content,
                source: url.into(),
                title: title.into(),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    pub fn unknown(raw: Value) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::Unknown { raw }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    pub fn output_text_ref(&self) -> Option<&str> {
        if self.tool_calls.is_some() {
            return None;
        }
        match &self.content {
            MessageContent::Text(text) if self.role == Role::Assistant => Some(text),
            MessageContent::Blocks(blocks) if self.role == Role::Assistant => {
                match blocks.as_slice() {
                    [ContentBlock::Text { text, .. }] => Some(text),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn thinking_ref(&self) -> Option<(&str, Option<&str>)> {
        let MessageContent::Blocks(blocks) = &self.content else {
            return None;
        };
        match blocks.as_slice() {
            [
                ContentBlock::Thinking {
                    thinking,
                    signature,
                },
            ] => Some((thinking, signature.as_deref())),
            _ => None,
        }
    }

    pub fn refusal_ref(&self) -> Option<&str> {
        let MessageContent::Blocks(blocks) = &self.content else {
            return None;
        };
        match blocks.as_slice() {
            [ContentBlock::Refusal { refusal }] => Some(refusal),
            _ => None,
        }
    }

    pub fn reasoning_ref(&self) -> Option<(&[String], &[String], Option<&str>)> {
        let MessageContent::Blocks(blocks) = &self.content else {
            return None;
        };
        match blocks.as_slice() {
            [
                ContentBlock::Reasoning {
                    summary,
                    content,
                    encrypted_content,
                },
            ] => Some((
                summary.as_slice(),
                content.as_slice(),
                encrypted_content.as_deref(),
            )),
            _ => None,
        }
    }

    pub fn set_reasoning_text(&mut self, text: &str) -> bool {
        let MessageContent::Blocks(blocks) = &mut self.content else {
            return false;
        };
        match blocks.as_mut_slice() {
            [ContentBlock::Thinking { thinking, .. }] => {
                thinking.clear();
                thinking.push_str(text);
                true
            }
            [ContentBlock::Reasoning { content, .. }] => {
                content.clear();
                if !text.is_empty() {
                    content.push(text.to_owned());
                }
                true
            }
            _ => false,
        }
    }
    pub fn function_call_ref(&self) -> Option<&ToolCall> {
        let is_empty = match &self.content {
            MessageContent::Text(text) => text.is_empty(),
            MessageContent::Blocks(blocks) => blocks.is_empty(),
        };
        match self.tool_calls.as_deref() {
            Some([call]) if self.role == Role::Assistant && is_empty => Some(call),
            _ => None,
        }
    }

    pub fn function_call_output_ref(&self) -> Option<(&str, &MessageContent)> {
        if self.role != Role::Tool {
            return None;
        }
        Some((self.tool_call_id.as_deref()?, &self.content))
    }

    pub fn unknown_ref(&self) -> Option<&Value> {
        let MessageContent::Blocks(blocks) = &self.content else {
            return None;
        };
        match blocks.as_slice() {
            [ContentBlock::Unknown { raw }] => Some(raw),
            _ => None,
        }
    }

    pub fn output_text_mut(&mut self) -> Option<&mut String> {
        if self.tool_calls.is_some() {
            return None;
        }
        match &mut self.content {
            MessageContent::Text(text) if self.role == Role::Assistant => Some(Arc::make_mut(text)),
            MessageContent::Blocks(blocks) if self.role == Role::Assistant => {
                match blocks.as_mut_slice() {
                    [ContentBlock::Text { text, .. }] => Some(Arc::make_mut(text)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn thinking_mut(&mut self) -> Option<&mut String> {
        let MessageContent::Blocks(blocks) = &mut self.content else {
            return None;
        };
        match blocks.as_mut_slice() {
            [ContentBlock::Thinking { thinking, .. }] => Some(thinking),
            _ => None,
        }
    }

    pub fn function_call_mut(&mut self) -> Option<&mut ToolCall> {
        let is_empty = match &self.content {
            MessageContent::Text(text) => text.is_empty(),
            MessageContent::Blocks(blocks) => blocks.is_empty(),
        };
        match self.tool_calls.as_deref_mut() {
            Some([call]) if self.role == Role::Assistant && is_empty => Some(call),
            _ => None,
        }
    }

    pub fn unknown_mut(&mut self) -> Option<&mut Value> {
        let MessageContent::Blocks(blocks) = &mut self.content else {
            return None;
        };
        match blocks.as_mut_slice() {
            [ContentBlock::Unknown { raw }] => Some(raw),
            _ => None,
        }
    }

    pub fn has_search_result(&self) -> bool {
        matches!(
            &self.content,
            MessageContent::Blocks(blocks)
                if matches!(blocks.as_slice(), [ContentBlock::SearchResult { .. }])
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: String,
}

// ── Tool spec ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's input parameters.
    pub parameters: Value,
    /// Whether to enforce strict JSON Schema validation (OpenAI + Anthropic).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Whether tool search must load this definition before it can be called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
}

/// `tool_choice` — how the model selects tools.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call a tool.
    Auto,
    /// Model must not call any tool.
    None,
    /// Model must call at least one tool.
    Required,
    /// Force a specific tool by name.
    Named {
        name: String,
    },
    Allowed {
        names: Vec<String>,
        required: bool,
    },
    Native(NativeToolKind),
}

// 旧 Raw 只在读取时归一化，无法表达的供应商选择不能重新进入公共契约。
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolChoiceWire {
    Auto,
    None,
    Required,
    Named { name: String },
    Allowed { names: Vec<String>, required: bool },
    Native(NativeToolKind),
    Raw(Value),
}

impl ToolChoiceWire {
    fn into_choice(self) -> Option<ToolChoice> {
        Some(match self {
            Self::Auto => ToolChoice::Auto,
            Self::None => ToolChoice::None,
            Self::Required => ToolChoice::Required,
            Self::Named { name } => ToolChoice::Named { name },
            Self::Allowed { names, required } => ToolChoice::Allowed { names, required },
            Self::Native(kind) => ToolChoice::Native(kind),
            Self::Raw(value) => return legacy_tool_choice(&value),
        })
    }
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ToolChoiceWire::deserialize(deserializer)?
            .into_choice()
            .ok_or_else(|| serde::de::Error::custom("unsupported legacy tool choice"))
    }
}

fn legacy_tool_choice(value: &Value) -> Option<ToolChoice> {
    let value = value.get("functionCallingConfig").unwrap_or(value);
    let kind = value
        .as_str()
        .or_else(|| value.get("type").and_then(Value::as_str))
        .or_else(|| value.get("mode").and_then(Value::as_str))?;
    match kind {
        "auto" | "AUTO" => {
            if let Some(names) = value.get("allowedFunctionNames").and_then(Value::as_array) {
                return Some(ToolChoice::Allowed {
                    names: names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    required: false,
                });
            }
            Some(ToolChoice::Auto)
        }
        "none" | "NONE" => Some(ToolChoice::None),
        "required" | "any" | "ANY" => {
            if let Some(names) = value.get("allowedFunctionNames").and_then(Value::as_array) {
                return Some(ToolChoice::Allowed {
                    names: names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    required: true,
                });
            }
            Some(ToolChoice::Required)
        }
        "function" | "tool" | "custom" => value
            .get("name")
            .or_else(|| {
                value
                    .get("function")
                    .and_then(|function| function.get("name"))
            })
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named { name: name.into() }),
        "allowed_tools" => {
            let tools = value.get("tools")?.as_array()?;
            let names = tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| {
                            tool.get("function")
                                .and_then(|function| function.get("name"))
                        })
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect();
            Some(ToolChoice::Allowed {
                names,
                required: value.get("mode").and_then(Value::as_str) == Some("required"),
            })
        }
        "web_search" | "web_search_preview" | "web_search_2025_08_26" => {
            Some(ToolChoice::Native(NativeToolKind::WebSearch))
        }
        "code_execution" | "code_interpreter" => {
            Some(ToolChoice::Native(NativeToolKind::CodeExecution))
        }
        "image_generation" => Some(ToolChoice::Native(NativeToolKind::ImageGeneration)),
        "tool_search" => Some(ToolChoice::Native(NativeToolKind::ToolSearch)),
        kind if kind.starts_with("web_search_") => {
            Some(ToolChoice::Native(NativeToolKind::WebSearch))
        }
        kind if kind.starts_with("code_execution_") => {
            Some(ToolChoice::Native(NativeToolKind::CodeExecution))
        }
        _ => None,
    }
}

// ── Generation config ─────────────────────────────────────────────────────────

/// Core generation parameters shared across all supported protocols.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
}

// ── Reasoning config ──────────────────────────────────────────────────────────

/// Effort level for reasoning / thinking models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    /// Budget in tokens (Anthropic `budget_tokens`).
    Budget(u32),
}

impl ReasoningEffort {
    pub fn as_openai_str(&self) -> Option<&str> {
        match self {
            Self::None => Some("none"),
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Xhigh => Some("xhigh"),
            Self::Max => Some("max"),
            Self::Budget(_) => None,
        }
    }
}

/// Reasoning / extended-thinking configuration.
///
/// Normalized from:
/// - OpenAI `reasoning.effort` + `reasoning.summary`
/// - Anthropic `thinking: { type: "enabled", budget_tokens, display }`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "ReasoningConfigWire")]
pub struct ReasoningConfig {
    /// 原生执行模式不是 effort 档位；adaptive 不应被还原为缺少预算的 enabled。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Token budget for thinking (Anthropic `budget_tokens`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// 跨协议只保留摘要意图，避免 display 与 summary 形成两个控制来源。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
    /// Canonical client-requested Thinking Level. `None` means unspecified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<ThinkingLevel>,
    /// Target-specific control resolved for one upstream attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_control: Option<TargetThinkingControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    None,
    Auto,
    Concise,
    Detailed,
}

impl ReasoningConfig {
    /// Derive client-requested activation without including per-attempt target controls.
    pub fn is_enabled(&self) -> bool {
        match self.mode.as_deref() {
            Some("disabled") => false,
            Some("auto" | "enabled" | "adaptive") => true,
            _ => self
                .level
                .map(|level| level != ThinkingLevel::Off)
                .unwrap_or_else(|| self.budget_tokens.is_some_and(|budget| budget > 0)),
        }
    }
}

// 格式 1 的旧字段仅在读取 DTO 中出现，不重新成为平台状态。
#[derive(Default, Deserialize)]
#[serde(default)]
struct ReasoningConfigWire {
    mode: Option<String>,
    enabled: bool,
    budget_tokens: Option<u32>,
    effort: Option<ReasoningEffort>,
    summary: Option<ReasoningSummary>,
    display: Option<String>,
    level: NullableControl<ThinkingLevel>,
    target_control: Option<TargetThinkingControl>,
}

impl From<ReasoningConfigWire> for ReasoningConfig {
    fn from(wire: ReasoningConfigWire) -> Self {
        // 兼容投影可同时携带 budget 与 effort，不能把副本重新解释成额外意图。
        let level = match wire.level {
            NullableControl::Value(level) => Some(level),
            NullableControl::Null => None,
            NullableControl::Missing if wire.budget_tokens.is_some() => None,
            NullableControl::Missing => wire.effort.as_ref().and_then(|effort| match effort {
                ReasoningEffort::Budget(value) => Some(ThinkingLevel::from_budget(*value)),
                value => value
                    .as_openai_str()
                    .and_then(|value| ThinkingLevel::from_wire(value).ok()),
            }),
        };
        let budget_tokens = wire.budget_tokens.or(match wire.effort {
            Some(ReasoningEffort::Budget(value)) => Some(value),
            _ => None,
        });
        let mode = wire.mode.or_else(|| {
            (wire.enabled && level.is_none() && budget_tokens.is_none()).then(|| "enabled".into())
        });
        Self {
            mode,
            budget_tokens,
            summary: wire.summary.or(match wire.display.as_deref() {
                Some("summarized") => Some(ReasoningSummary::Auto),
                Some("omitted") => Some(ReasoningSummary::None),
                _ => None,
            }),
            level,
            target_control: wire.target_control,
        }
    }
}

// ── Response format ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        schema: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

// 历史插件曾把 schema 包在 json_schema 中；读取兼容不能改变写出的唯一形态。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseFormatWire {
    Text,
    JsonObject,
    JsonSchema {
        #[serde(flatten)]
        shape: ResponseSchemaWire,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResponseSchemaWire {
    Direct(ResponseSchemaFields),
    Nested { json_schema: ResponseSchemaFields },
}

#[derive(Deserialize)]
struct ResponseSchemaFields {
    name: String,
    description: Option<String>,
    schema: Value,
    strict: Option<bool>,
}

impl<'de> Deserialize<'de> for ResponseFormat {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match ResponseFormatWire::deserialize(deserializer)? {
            ResponseFormatWire::Text => Self::Text,
            ResponseFormatWire::JsonObject => Self::JsonObject,
            ResponseFormatWire::JsonSchema { shape } => {
                let (ResponseSchemaWire::Direct(fields)
                | ResponseSchemaWire::Nested {
                    json_schema: fields,
                }) = shape;
                Self::JsonSchema {
                    name: fields.name,
                    description: fields.description,
                    schema: fields.schema,
                    strict: fields.strict,
                }
            }
        })
    }
}

// ── Stream config ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamConfig {
    pub enabled: bool,
    /// Whether the provider should include token usage in the final stream chunk.
    pub include_usage: bool,
}

// ── Request metadata ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaRoutingMode {
    Native,
    Bridge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRoutingPlan {
    pub mode: MediaRoutingMode,
    pub target_keys: Vec<String>,
    pub source_artifact_ids: Vec<String>,
}

fn serialize_optional_protocol<S>(
    protocol: &Option<ProtocolId>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    protocol
        .as_ref()
        .map(ToString::to_string)
        .serialize(serializer)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    /// The protocol the client spoke.
    #[serde(serialize_with = "serialize_optional_protocol")]
    pub source_protocol: Option<ProtocolId>,
    /// Raw envelope preserved for pass-through / audit, never part of canonical identity.
    #[serde(skip)]
    pub raw: Option<RawEnvelope>,
    /// Three-segment vendor extension bag.
    pub vendor: VendorExtensions,
    pub media_routing: Option<MediaRoutingPlan>,
    #[serde(skip)]
    pub redaction: crate::redaction::RedactionTrace,
}

// ── AiRequest ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Text(String),
    Texts(Vec<String>),
    Tokens(Vec<u32>),
    TokenBatches(Vec<Vec<u32>>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub input: EmbeddingInput,
    pub dimensions: Option<u32>,
    pub encoding_format: Option<String>,
    pub user: Option<String>,
}

/// Unified ingress IR consumed by all codec encoders and the dispatcher.
///
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "AiRequestWire")]
pub struct AiRequest {
    // ── Core ──────────────────────────────────────────────────────────────────
    /// [IR] The model identifier as received from the client.
    pub model: String,
    /// [IR] Conversation history.
    pub items: Vec<AiItem>,
    /// [IR] Request-level instructions. This remains separate from ordered
    /// message history so continuation can inherit it exactly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    // ── Generation ────────────────────────────────────────────────────────────
    /// [IR] Core generation parameters.
    pub generation: GenerationConfig,
    /// [IR] Embedding parameters. Present only for embedding requests.
    pub embedding: Option<EmbeddingRequest>,

    // ── Streaming ─────────────────────────────────────────────────────────────
    /// [IR] Streaming configuration.
    pub stream: StreamConfig,

    // ── Tools ─────────────────────────────────────────────────────────────────
    /// [IR] User-defined tool specifications.
    pub tools: Option<Vec<ToolSpec>>,
    /// [IR] Tool selection mode.
    pub tool_choice: Option<ToolChoice>,
    /// [IR] Whether the provider should call tools in parallel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    // ── Reasoning ─────────────────────────────────────────────────────────────
    /// [IR] Reasoning / extended-thinking configuration.
    pub reasoning: ReasoningConfig,

    // ── Output format ─────────────────────────────────────────────────────────
    /// [IR] Response format constraint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,

    // ── Protocol extensions ───────────────────────────────────────────────────
    /// 独立语义控制的唯一平台归属；扩展字段均有默认值以保持插件读取兼容。
    #[serde(default)]
    pub parameters: super::parameters::RequestParameters,

    // ── Metadata / vendor bag ─────────────────────────────────────────────────
    pub meta: RequestMetadata,
}

// 缺省与显式 null 只在读取边界区分，避免把续接重置状态扩散到公共字段。
#[derive(Default)]
enum NullableControl<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NullableControl<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

impl<T> NullableControl<T> {
    fn into_value(
        self,
        resets: &mut Vec<super::parameters::ParameterReset>,
        reset: super::parameters::ParameterReset,
    ) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Null => {
                if !resets.contains(&reset) {
                    resets.push(reset);
                }
                None
            }
            Self::Missing => None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GenerationConfigWire {
    temperature: NullableControl<f64>,
    max_tokens: NullableControl<u32>,
    top_p: NullableControl<f64>,
    seed: Option<i64>,
    stop: Option<Vec<String>>,
    presence_penalty: NullableControl<f64>,
    frequency_penalty: NullableControl<f64>,
}

impl GenerationConfigWire {
    fn into_config(self, resets: &mut Vec<super::parameters::ParameterReset>) -> GenerationConfig {
        use super::parameters::ParameterReset;
        GenerationConfig {
            temperature: self
                .temperature
                .into_value(resets, ParameterReset::Temperature),
            max_tokens: self
                .max_tokens
                .into_value(resets, ParameterReset::MaxTokens),
            top_p: self.top_p.into_value(resets, ParameterReset::TopP),
            seed: self.seed,
            stop: self.stop,
            presence_penalty: self
                .presence_penalty
                .into_value(resets, ParameterReset::PresencePenalty),
            frequency_penalty: self
                .frequency_penalty
                .into_value(resets, ParameterReset::FrequencyPenalty),
        }
    }
}

/// 仅用于读取已安装 format1 插件和历史快照；旧拼写不是运行时状态。
#[derive(Deserialize)]
struct AiRequestWire {
    model: String,
    items: Vec<AiItem>,
    #[serde(default)]
    instructions: NullableControl<String>,
    #[serde(default)]
    generation: Option<GenerationConfigWire>,
    #[serde(default)]
    embedding: Option<EmbeddingRequest>,
    #[serde(default)]
    stream: StreamConfig,
    #[serde(default)]
    tools: Option<Vec<ToolSpec>>,
    #[serde(default)]
    tool_choice: Option<ToolChoiceWire>,
    #[serde(default)]
    parallel_tool_calls: NullableControl<bool>,
    #[serde(default)]
    disable_parallel_tool_calls: Option<bool>,
    #[serde(default)]
    reasoning: NullableControl<ReasoningConfig>,
    #[serde(default)]
    response_format: NullableControl<ResponseFormat>,
    #[serde(default)]
    parameters: super::parameters::RequestParameters,
    #[serde(default)]
    ext: Option<ProtocolExt>,
    #[serde(default)]
    meta: RequestMetadata,
}

impl From<AiRequestWire> for AiRequest {
    fn from(wire: AiRequestWire) -> Self {
        use super::parameters::ParameterReset;
        let mut parameters = wire.parameters;
        let generation = wire
            .generation
            .unwrap_or_default()
            .into_config(&mut parameters.resets);
        let instructions = wire
            .instructions
            .into_value(&mut parameters.resets, ParameterReset::Instructions);
        let parallel_is_missing = matches!(&wire.parallel_tool_calls, NullableControl::Missing);
        let parallel_tool_calls = wire
            .parallel_tool_calls
            .into_value(&mut parameters.resets, ParameterReset::ParallelToolCalls)
            .or_else(|| {
                if parallel_is_missing {
                    wire.disable_parallel_tool_calls.map(|value| !value)
                } else {
                    None
                }
            });
        let reasoning = wire
            .reasoning
            .into_value(&mut parameters.resets, ParameterReset::Reasoning)
            .unwrap_or_default();
        let response_format = wire
            .response_format
            .into_value(&mut parameters.resets, ParameterReset::ResponseFormat);
        let mut request = Self {
            model: wire.model,
            items: wire.items,
            instructions,
            generation,
            embedding: wire.embedding,
            stream: wire.stream,
            tools: wire.tools,
            tool_choice: wire.tool_choice.and_then(ToolChoiceWire::into_choice),
            parallel_tool_calls,
            reasoning,
            response_format,
            parameters,
            meta: wire.meta,
        };
        if let Some(extension) = wire.ext {
            request.import_legacy_extension(extension);
        }
        request.normalize_parameters();
        request
    }
}

impl AiRequest {
    /// Convenience constructor with minimal required fields.
    pub fn new(model: impl Into<String>, items: Vec<AiItem>) -> Self {
        Self {
            model: model.into(),
            items,
            instructions: None,
            generation: GenerationConfig::default(),
            embedding: None,
            stream: StreamConfig::default(),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: ReasoningConfig::default(),
            response_format: None,
            parameters: Default::default(),
            meta: RequestMetadata::default(),
        }
    }

    /// Serialize the complete Debug checkpoint, including the raw ingress envelope.
    pub fn debug_value(&self) -> Result<Value, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        if let Some(raw) = &self.meta.raw
            && let Some(meta) = value.get_mut("meta").and_then(Value::as_object_mut)
        {
            meta.insert("raw".into(), serde_json::to_value(raw)?);
        }
        Ok(value)
    }

    /// 返回唯一平台输出模态；不从源协议的副本恢复。
    pub fn modalities(&self) -> Option<&Vec<super::parameters::Modality>> {
        self.parameters.modalities.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AiItem, AiItemAudience, AiItemMetadata, AiItemProvenance, AiItemStatus, DocumentSource,
        MediaSource, MessageContent,
    };
    use serde_json::{Value, json};

    #[test]
    fn legacy_request_snapshot_discards_removed_tool_and_safety_controls() {
        let request: super::AiRequest = serde_json::from_value(json!({
            "model": "model",
            "items": [],
            "tools": [{
                "name": "lookup",
                "description": "Find a record",
                "parameters": {"type": "object", "properties": {"opaque": {"vendor": true}}},
                "strict": false,
                "defer_loading": true,
                "eager_input_streaming": true,
                "allowed_callers": ["code_execution"],
                "input_examples": [{"opaque": 1}],
                "citations_enabled": true,
                "cache_control": {"type": "ephemeral"},
                "meta": {"vendor": "discard"}
            }],
            "safety_settings": [{"category": "harm", "threshold": "block"}],
            "disable_parallel_tool_calls": true,
            "reasoning": {"enabled": true, "display": "summarized"},
            "response_format": {"type": "json_schema", "name": "answer", "schema": {}}
        }))
        .unwrap();
        let stored = serde_json::to_value(&request).unwrap();
        assert_eq!(
            stored["tools"],
            json!([{
                "name": "lookup",
                "description": "Find a record",
                "parameters": {"type": "object", "properties": {"opaque": {"vendor": true}}},
                "strict": false,
                "defer_loading": true
            }])
        );
        assert!(stored.get("safety_settings").is_none());
        assert_eq!(stored["parallel_tool_calls"], false);
        assert_eq!(
            stored["reasoning"],
            json!({"mode": "enabled", "summary": "auto"})
        );
        assert_eq!(
            stored["response_format"],
            json!({
                "type": "json_schema", "name": "answer", "schema": {}
            })
        );
        let restored: super::AiRequest = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), stored);
    }

    #[test]
    fn legacy_raw_tool_choices_import_without_preserving_vendor_json() {
        for (raw, canonical) in [
            (json!("required"), json!("required")),
            (
                json!({"type": "function", "function": {"name": "lookup"}}),
                json!({"named": {"name": "lookup"}}),
            ),
            (
                json!({"type": "allowed_tools", "mode": "required",
                    "tools": [{"type": "function", "name": "lookup"}, {"type": "custom", "name": "script"}]}),
                json!({"allowed": {"names": ["lookup", "script"], "required": true}}),
            ),
            (
                json!({"mode": "AUTO", "allowedFunctionNames": ["lookup"]}),
                json!({"allowed": {"names": ["lookup"], "required": false}}),
            ),
            (
                json!({"type": "web_search_preview_2025_03_11"}),
                json!({"native": "web_search"}),
            ),
            (
                json!({"type": "code_interpreter"}),
                json!({"native": "code_execution"}),
            ),
            (
                json!({"type": "image_generation"}),
                json!({"native": "image_generation"}),
            ),
            (
                json!({"type": "tool_search"}),
                json!({"native": "tool_search"}),
            ),
            (
                json!({"type": "file_search", "private_control": true}),
                Value::Null,
            ),
        ] {
            let request: super::AiRequest = serde_json::from_value(json!({
                "model": "model", "items": [], "tool_choice": {"raw": raw}
            }))
            .unwrap();
            let stored = serde_json::to_value(&request).unwrap();
            assert_eq!(stored["tool_choice"], canonical);
            let restored: super::AiRequest = serde_json::from_value(stored).unwrap();
            assert_eq!(
                serde_json::to_value(restored.tool_choice).unwrap(),
                canonical
            );
        }
    }

    #[test]
    fn reasoning_summary_replaces_display_and_prefers_canonical_value() {
        for (wire, summary) in [
            (
                json!({"display": "summarized"}),
                Some(super::ReasoningSummary::Auto),
            ),
            (
                json!({"display": "omitted"}),
                Some(super::ReasoningSummary::None),
            ),
            (json!({"display": "unknown"}), None),
            (
                json!({"summary": "detailed", "display": "omitted"}),
                Some(super::ReasoningSummary::Detailed),
            ),
            (
                json!({"summary": "concise"}),
                Some(super::ReasoningSummary::Concise),
            ),
        ] {
            let reasoning: super::ReasoningConfig = serde_json::from_value(wire).unwrap();
            assert_eq!(reasoning.summary, summary);
            let stored = serde_json::to_value(&reasoning).unwrap();
            assert!(stored.get("display").is_none());
            let restored: super::ReasoningConfig = serde_json::from_value(stored).unwrap();
            assert_eq!(restored.summary, summary);
        }
    }

    #[test]
    fn canonical_reasoning_budget_and_level_override_legacy_effort_projection() {
        for wire in [
            json!({"budget_tokens":1024,"effort":{"budget":1024}}),
            json!({"budget_tokens":1024,"level":null,"effort":{"budget":1024}}),
            json!({"level":null,"effort":"high"}),
        ] {
            let reasoning: super::ReasoningConfig = serde_json::from_value(wire).unwrap();
            assert!(reasoning.level.is_none());
        }
        let legacy: super::ReasoningConfig =
            serde_json::from_value(json!({"effort":"high"})).unwrap();
        assert_eq!(legacy.level, Some(crate::thinking::ThinkingLevel::High));
    }

    #[test]
    fn response_format_description_round_trips_without_rewriting_business_schema() {
        let value = json!({
            "type": "json_schema",
            "name": "answer",
            "description": "Documented answer",
            "schema": {
                "type": "object",
                "description": "Business schema description",
                "properties": {"opaque": {"vendor_annotation": true}}
            },
            "strict": false
        });
        let format: super::ResponseFormat = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(format).unwrap(), value);
        let nested = json!({
            "type": "json_schema",
            "json_schema": {
                "name": "answer",
                "description": "Documented answer",
                "schema": value["schema"],
                "strict": false
            }
        });
        let legacy: super::ResponseFormat = serde_json::from_value(nested).unwrap();
        assert_eq!(serde_json::to_value(legacy).unwrap(), value);
    }

    #[test]
    fn explicit_null_request_controls_record_resets_without_roundtrip_growth() {
        use crate::protocol::ir::parameters::ParameterReset;
        let request: super::AiRequest = serde_json::from_value(json!({
            "model": "model",
            "items": [],
            "instructions": null,
            "reasoning": null,
            "response_format": null,
            "parallel_tool_calls": null,
            "disable_parallel_tool_calls": false,
            "generation": {
                "temperature": null,
                "top_p": null,
                "max_tokens": null,
                "presence_penalty": null,
                "frequency_penalty": null
            }
        }))
        .unwrap();
        assert!(request.instructions.is_none());
        assert!(request.parallel_tool_calls.is_none());
        assert!(request.response_format.is_none());
        for reset in [
            ParameterReset::Instructions,
            ParameterReset::Reasoning,
            ParameterReset::ResponseFormat,
            ParameterReset::ParallelToolCalls,
            ParameterReset::Temperature,
            ParameterReset::TopP,
            ParameterReset::MaxTokens,
            ParameterReset::PresencePenalty,
            ParameterReset::FrequencyPenalty,
        ] {
            assert!(request.parameters.resets.contains(&reset), "{reset:?}");
        }
        let stored = serde_json::to_value(&request).unwrap();
        for field in ["instructions", "response_format", "parallel_tool_calls"] {
            assert!(stored.get(field).is_none(), "{field}");
        }
        let restored: super::AiRequest = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), stored);
        let missing: super::AiRequest = serde_json::from_value(json!({
            "model": "model", "items": []
        }))
        .unwrap();
        assert!(missing.parameters.resets.is_empty());
    }

    #[test]
    fn legacy_bedrock_output_imports_single_canonical_response_format() {
        let output = json!({
            "textFormat": {
                "type": "json_schema",
                "structure": {"jsonSchema": {
                    "schema": "{\"type\":\"object\",\"properties\":{\"opaque\":{\"unknown\":true}}}",
                    "name": "answer",
                    "description": "documented answer",
                    "unknown": "ignored"
                }}
            }
        });
        for parameters in [
            json!({"bedrock_output_config": output}),
            json!({"extensions": {"bedrock": {"outputConfig": output}}}),
        ] {
            let request: super::AiRequest = serde_json::from_value(json!({
                "model": "model", "items": [], "parameters": parameters
            }))
            .unwrap();
            let Some(super::ResponseFormat::JsonSchema {
                name,
                description,
                schema,
                strict,
            }) = &request.response_format
            else {
                panic!("legacy Bedrock format was not imported");
            };
            assert_eq!(name, "answer");
            assert_eq!(schema["properties"]["opaque"]["unknown"], true);
            assert_eq!(*strict, None);
            assert_eq!(description.as_deref(), Some("documented answer"));
            let stored = serde_json::to_value(&request).unwrap();
            assert!(stored["parameters"].get("bedrock_output_config").is_none());
            assert!(stored["parameters"].get("extensions").is_none());
        }
    }

    #[test]
    fn legacy_native_controls_import_only_unified_fields() {
        let request: super::AiRequest = serde_json::from_value(json!({
            "model": "model", "items": [],
            "tools": [{"name": "lookup", "parameters": {}, "strict": null}],
            "parameters": {
                "cohere_strict_tools": false,
                "extensions": {
                    "cohere": {"strict_tools": true, "priority": 0, "documents": [], "unknown": 1},
                    "google": {"audioTimestamp": false, "speechConfig": null, "unknown": true},
                    "open-responses": {
                        "max_tool_calls": null,
                        "text": {"format": {"strict": null}, "verbosity": null},
                        "unknown": {"danger": true}
                    },
                    "unregistered": {"injected": true}
                },
                "future_optional_control": {"ignored": true}
            }
        }))
        .unwrap();
        assert_eq!(request.tools.as_ref().unwrap()[0].strict, Some(false));
        let stored = serde_json::to_value(&request).unwrap();
        for field in [
            "extensions",
            "future_optional_control",
            "cohere_strict_tools",
            "cohere_priority",
            "cohere_documents",
            "google_audio_timestamp",
            "google_speech_config",
            "response_presence",
        ] {
            assert!(stored["parameters"].get(field).is_none(), "{field}");
        }
        let restored: super::AiRequest = serde_json::from_value(stored).unwrap();
        assert_eq!(restored.tools.as_ref().unwrap()[0].strict, Some(false));
    }

    #[test]
    fn ignored_legacy_controls_do_not_change_canonical_history_identity() {
        let baseline = super::AiRequest::new("model", Vec::new());
        let mut legacy = serde_json::to_value(&baseline).unwrap();
        legacy["parameters"]["extensions"] = json!({"openai-chat": {"unknown": true}});
        legacy["meta"]["vendor"] = json!({
            "ingress": {"unknown": {"nested": true}},
            "egress": {},
            "passthrough_safe": {}
        });
        let ignored: super::AiRequest = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            crate::protocol::ir::canonical::history_request_controls_hash(&baseline),
            crate::protocol::ir::canonical::history_request_controls_hash(&ignored),
        );
    }

    #[test]
    fn legacy_reasoning_activation_is_only_a_missing_control_hint() {
        for (wire, mode, enabled) in [
            (json!({"enabled": true}), Some("enabled"), true),
            (json!({"enabled": false}), None, false),
            (json!({"enabled": true, "level": "off"}), None, false),
            (json!({"enabled": true, "level": "high"}), None, true),
            (json!({"enabled": true, "budget_tokens": 0}), None, false),
            (
                json!({"enabled": true, "mode": "disabled", "level": "max", "effort": "low"}),
                Some("disabled"),
                false,
            ),
        ] {
            let reasoning: super::ReasoningConfig = serde_json::from_value(wire).unwrap();
            assert_eq!(reasoning.mode.as_deref(), mode);
            assert_eq!(reasoning.is_enabled(), enabled);
            let canonical = serde_json::to_value(&reasoning).unwrap();
            assert!(canonical.get("enabled").is_none());
            assert!(canonical.get("effort").is_none());
            let restored: super::ReasoningConfig = serde_json::from_value(canonical).unwrap();
            assert_eq!(restored.mode, reasoning.mode);
            assert_eq!(restored.level, reasoning.level);
            assert_eq!(restored.is_enabled(), enabled);
        }
    }

    #[test]
    fn explicit_reasoning_mode_and_level_survive_legacy_effort_and_normalization() {
        let mut request = super::AiRequest::new("model", Vec::new());
        request.reasoning = serde_json::from_value(json!({
            "mode": "disabled", "level": "max", "effort": "low", "enabled": true
        }))
        .unwrap();
        request
            .meta
            .vendor
            .ingress
            .insert("reasoning_effort".into(), json!("medium"));
        request.normalize_parameters();
        assert_eq!(request.reasoning.mode.as_deref(), Some("disabled"));
        assert_eq!(
            request.reasoning.level,
            Some(crate::thinking::ThinkingLevel::Max)
        );
        assert!(!request.reasoning.is_enabled());
        request.reasoning.mode = Some("enabled".into());
        request.reasoning.level = Some(crate::thinking::ThinkingLevel::Off);
        request.normalize_parameters();
        assert!(request.reasoning.is_enabled());
        assert_eq!(
            request.reasoning.level,
            Some(crate::thinking::ThinkingLevel::Off)
        );
    }

    #[test]
    fn message_content_decoding_preserves_text_blocks_and_native_values() {
        let exact_number = json!(9_007_199_254_740_993_u64);
        for text in ["", "原文 🐟\n\"quote\"\\path\tend"] {
            let content: MessageContent =
                serde_json::from_value(json!(text)).expect("decode owned text");
            assert!(matches!(&content, MessageContent::Text(value) if value.as_str() == text));
            let restored: MessageContent =
                serde_json::from_slice(&serde_json::to_vec(&content).expect("encode text"))
                    .expect("decode escaped text");
            assert!(matches!(restored, MessageContent::Text(value) if value.as_str() == text));
        }
        for blocks in [
            json!([]),
            json!([
                {"type": "text", "text": "原文 🐟\n"},
                {"type": "thinking", "thinking": "保留思考", "signature": "opaque"},
                {
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": {"type": "text", "text": "business JSON", "nested": [null, false], "number": exact_number},
                    "content_kind": "json",
                    "is_error": true
                }
            ]),
        ] {
            let content: MessageContent =
                serde_json::from_value(blocks.clone()).expect("decode owned blocks");
            assert!(matches!(&content, MessageContent::Blocks(_)));
            assert_eq!(
                serde_json::to_value(&content).expect("encode blocks"),
                blocks
            );
            let restored: MessageContent =
                serde_json::from_slice(&serde_json::to_vec(&blocks).expect("encode wire blocks"))
                    .expect("decode wire blocks");
            assert_eq!(
                serde_json::to_value(restored).expect("encode restored"),
                blocks
            );
        }
    }

    #[test]
    fn message_content_wire_decoding_preserves_arbitrary_precision_numbers() {
        let raw = br#"[{"type":"tool_result","tool_use_id":"call_1","content":{"number":123456789012345678901234567890},"content_kind":"json"}]"#;
        let restored: MessageContent =
            serde_json::from_slice(raw).expect("decode native JSON number from wire");
        let MessageContent::Blocks(blocks) = restored else {
            panic!("expected tool-result blocks");
        };
        let Some(super::ContentBlock::ToolResult { content, .. }) = blocks.first() else {
            panic!("expected tool result");
        };
        assert!(content["number"].is_number());
        assert_eq!(
            content["number"].as_number().expect("number").to_string(),
            "123456789012345678901234567890"
        );
    }

    #[test]
    fn message_content_decoding_rejects_non_content_shapes() {
        for invalid in [
            Value::Null,
            json!(false),
            json!(42),
            json!({"type": "text", "text": "not an array"}),
            json!(["not a content block"]),
            json!([{"type": "text", "text": 42}]),
            json!([{"type": "unknown_block"}]),
        ] {
            assert!(serde_json::from_value::<MessageContent>(invalid.clone()).is_err());
            assert!(
                serde_json::from_slice::<MessageContent>(
                    &serde_json::to_vec(&invalid).expect("encode invalid wire")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn item_metadata_preserves_opaque_and_unknown_values_until_graph_mutation() {
        for raw in [json!("opaque"), json!([1, {"unknown": true}]), Value::Null] {
            let item: AiItem = serde_json::from_value(json!({
                "role": "assistant", "content": "answer", "meta": raw,
            }))
            .expect("deserialize legacy item");
            assert_eq!(serde_json::to_value(&item).expect("serialize")["meta"], raw);
            let mut item = item;
            assert!(
                item.meta
                    .as_mut()
                    .expect("legacy metadata")
                    .insert_extension("reasoning_content", json!(""))
                    .is_err()
            );
            assert_eq!(serde_json::to_value(&item).expect("serialize")["meta"], raw);
            item.set_graph_metadata(
                Some("msg_1".into()),
                Some(AiItemStatus::Completed),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            );
            let metadata = serde_json::to_value(&item).expect("serialize")["meta"].clone();
            assert_eq!(metadata["vendor_meta"], raw);
            assert_eq!(metadata["id"], "msg_1");
        }
        let raw = json!({"id": 41, "status": "future", "__open_responses_item_reference": 42, "extension": [1, 2]});
        let mut metadata = AiItemMetadata::from(raw.clone());
        assert_eq!(serde_json::to_value(&metadata).expect("serialize"), raw);
        metadata.set_graph(
            None,
            None,
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        );
        assert_eq!(
            serde_json::to_value(metadata).expect("serialize"),
            json!({"provenance": "provider", "audience": "client", "extension": [1, 2]})
        );
        let valid = json!({"id": "msg_1", "status": "completed", "provenance": "provider",
            "audience": "client", "__open_responses_item_reference": "msg_saved", "future": {"enabled": true}});
        let metadata = AiItemMetadata::from(valid.clone());
        assert_eq!(
            metadata.id().map(super::CanonicalItemId::as_str),
            Some("msg_1")
        );
        assert_eq!(
            metadata.reference().map(super::ItemReference::as_str),
            Some("msg_saved")
        );
        assert_eq!(serde_json::to_value(metadata).expect("serialize"), valid);
    }

    #[test]
    fn metadata_extension_mutator_protects_reserved_graph_fields() {
        let mut metadata = AiItemMetadata::from(json!({"future": {"field": true}}));
        for field in [
            "id",
            "status",
            "provenance",
            "audience",
            "__open_responses_item_reference",
        ] {
            assert!(metadata.insert_extension(field, json!("forged")).is_err());
            assert!(metadata.remove_extension(field).is_err());
        }
        assert_eq!(
            serde_json::to_value(metadata).expect("serialize"),
            json!({"future": {"field": true}})
        );
    }

    #[test]
    fn media_and_document_urls_survive_ir_serialization() {
        let url = "https://example.invalid/retained-reference";
        let media = serde_json::to_value(MediaSource::Url(url.into())).expect("serialize media");
        let restored: MediaSource = serde_json::from_value(media).expect("restore media");
        assert!(matches!(restored, MediaSource::Url(value) if value == url));

        let document =
            serde_json::to_value(DocumentSource::Url(url.into())).expect("serialize document");
        let restored: DocumentSource = serde_json::from_value(document).expect("restore document");
        assert!(matches!(restored, DocumentSource::Url(value) if value == url));
    }
}
