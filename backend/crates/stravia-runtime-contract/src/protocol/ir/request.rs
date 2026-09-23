//! `AiRequest` — the unified ingress IR for all supported protocols.
//!
//! Codec decoders (PR-2) produce `AiRequest`; codec encoders (PR-3) and the
//! dispatcher (PR-5) consume it.  Until PR-2 lands, `compat.rs` provides
//! lossless `From` conversions from/to the old `InternalRequest`.

use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};
use serde_json::{Map, Value};

use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::cache::CacheControl;
use crate::protocol::ir::envelope::RawEnvelope;
use crate::protocol::ir::ext::ProtocolExt;
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
        text: String,
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

/// Message content — either a plain string or a typed block list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
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
            content: MessageContent::Text(text.into()),
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
            content: MessageContent::Text(String::new()),
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
            Value::String(text) => MessageContent::Text(text),
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
                text: snippet,
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
            MessageContent::Text(text) if self.role == Role::Assistant => Some(text),
            MessageContent::Blocks(blocks) if self.role == Role::Assistant => {
                match blocks.as_mut_slice() {
                    [ContentBlock::Text { text, .. }] => Some(text),
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
    /// Per-tool cache breakpoint (Anthropic).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// Vendor-specific extra fields not covered by the IR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// `tool_choice` — how the model selects tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call a tool.
    Auto,
    /// Model must not call any tool.
    None,
    /// Model must call at least one tool.
    Required,
    /// Force a specific tool by name.
    Named { name: String },
    /// Pass-through raw value for protocol-specific options.
    Raw(Value),
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
pub enum ReasoningEffort {
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
    pub fn from_openai_str(value: &str) -> anyhow::Result<Self> {
        let effort = match value {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            _ => anyhow::bail!("invalid OpenAI reasoning effort"),
        };
        Ok(effort)
    }

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
pub struct ReasoningConfig {
    /// Whether extended reasoning / thinking is requested.
    pub enabled: bool,
    /// Token budget for thinking (Anthropic `budget_tokens`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// Effort level (OpenAI `reasoning.effort`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Display mode for thinking content (Anthropic `display: "summarized" | "omitted"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Canonical client-requested Thinking Level. `None` means unspecified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<ThinkingLevel>,
    /// Target-specific control resolved for one upstream attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_control: Option<TargetThinkingControl>,
}

// ── Response format ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

// ── Stream config ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamConfig {
    pub enabled: bool,
    /// Whether the provider should include token usage in the final stream chunk.
    pub include_usage: bool,
}

// ── Safety settings ───────────────────────────────────────────────────────────

/// Google SafetySettings — important enough to have a first-class home in the IR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetySettings {
    pub category: String,
    pub threshold: String,
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
/// Fields are annotated with the FIELD_HOMING.md category that they belong to:
/// `[IR]` = core, `[OAIChat]` = OpenAIChatExt, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiRequest {
    // ── Core ──────────────────────────────────────────────────────────────────
    /// [IR] The model identifier as received from the client.
    pub model: String,
    /// [IR] Conversation history.
    pub items: Vec<AiItem>,
    /// [IR] Request-level instructions. This remains separate from ordered
    /// message history so continuation can inherit it exactly.
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
    pub parallel_tool_calls: Option<bool>,
    /// [IR] Disable parallel tool use (Anthropic `disable_parallel_tool_use`,
    /// equivalent to `parallel_tool_calls = false` for OpenAI).
    pub disable_parallel_tool_calls: Option<bool>,

    // ── Reasoning ─────────────────────────────────────────────────────────────
    /// [IR] Reasoning / extended-thinking configuration.
    pub reasoning: ReasoningConfig,

    // ── Output format ─────────────────────────────────────────────────────────
    /// [IR] Response format constraint.
    pub response_format: Option<ResponseFormat>,

    // ── Safety ────────────────────────────────────────────────────────────────
    /// [IR] Google SafetySettings (ignored by other encoders).
    pub safety_settings: Option<Vec<SafetySettings>>,

    // ── Protocol extensions ───────────────────────────────────────────────────
    /// Protocol-domain Ext carrying fields specific to the source protocol.
    /// Populated by the ingress decoder (PR-2); consumed by the egress encoder (PR-3).
    pub ext: Option<ProtocolExt>,

    // ── Metadata / vendor bag ─────────────────────────────────────────────────
    pub meta: RequestMetadata,
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
            disable_parallel_tool_calls: None,
            reasoning: ReasoningConfig::default(),
            response_format: None,
            safety_settings: None,
            ext: None,
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

    /// Return the modalities from `OpenAIChatExt` if present.
    pub fn modalities(&self) -> Option<&Vec<String>> {
        if let Some(ProtocolExt::OpenAiChat(ref e)) = self.ext {
            e.modalities.as_ref()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AiItem, AiItemAudience, AiItemMetadata, AiItemProvenance, AiItemStatus, DocumentSource,
        MediaSource,
    };
    use serde_json::{Value, json};

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
