//! `AiRequest` — the unified ingress IR for all supported protocols.
//!
//! Codec decoders (PR-2) produce `AiRequest`; codec encoders (PR-3) and the
//! dispatcher (PR-5) consume it.  Until PR-2 lands, `compat.rs` provides
//! lossless `From` conversions from/to the old `InternalRequest`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::cache::CacheControl;
use crate::protocol::ir::envelope::RawEnvelope;
use crate::protocol::ir::ext::ProtocolExt;
use crate::protocol::ir::vendor_ext::VendorExtensions;
use crate::thinking::{TargetThinkingControl, ThinkingLevel};

pub(crate) const VERIFIED_HISTORY_REPLAY_META: &str = "__stravia_verified_history_replay";

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
    Url(String),
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
    Url(String),
    /// Content already stored as content blocks.
    Blocks {
        content: Vec<ContentBlock>,
    },
}

// ── Content blocks ────────────────────────────────────────────────────────────

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
    /// Redacted thinking block (Anthropic `RedactedThinkingBlockParam`).
    RedactedThinking {
        data: String,
    },

    // ── Tool calls ────────────────────────────────────────────────────────────
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },

    // ── Server-side tools (Anthropic) ────────────────────────────────────────
    /// A server-executed tool call (Anthropic `ServerToolUseBlockParam`,
    /// Google `Part.toolCall`).
    ServerToolUse {
        id: String,
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
        tool_use_id: String,
        content: Value,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiItem {
    pub role: Role,
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// The `tool_call_id` this result answers.  Required for `Role::Tool` messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Provider-specific extras for this individual message (e.g. Anthropic
    /// `cache_control` on `system` array items).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
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
    pub fn id_ref(&self) -> Option<&str> {
        self.meta
            .as_ref()?
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    }

    pub fn status(&self) -> Option<AiItemStatus> {
        match self.meta.as_ref()?.get("status")?.as_str()? {
            "in_progress" => Some(AiItemStatus::InProgress),
            "completed" => Some(AiItemStatus::Completed),
            "incomplete" => Some(AiItemStatus::Incomplete),
            "failed" => Some(AiItemStatus::Failed),
            _ => None,
        }
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
        let mut meta = self
            .meta
            .take()
            .map(|value| match value {
                Value::Object(object) => object,
                other => serde_json::Map::from_iter([("vendor_meta".into(), other)]),
            })
            .unwrap_or_default();
        if let Some(id) = id.filter(|value| !value.is_empty()) {
            meta.insert("id".into(), Value::String(id));
        }
        if let Some(status) = status {
            meta.insert("status".into(), Value::String(status.as_str().into()));
        }
        meta.insert(
            "provenance".into(),
            Value::String(
                match provenance {
                    AiItemProvenance::Client => "client",
                    AiItemProvenance::Provider => "provider",
                    AiItemProvenance::Platform => "platform",
                }
                .into(),
            ),
        );
        meta.insert(
            "audience".into(),
            Value::String(
                match audience {
                    AiItemAudience::Client => "client",
                    AiItemAudience::Provider => "provider",
                    AiItemAudience::Internal => "internal",
                }
                .into(),
            ),
        );
        self.meta = Some(Value::Object(meta));
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

    pub fn function_call_output(call_id: impl Into<String>, output: Value) -> Self {
        let content = match output {
            Value::String(text) => MessageContent::Text(text),
            Value::Array(items) => MessageContent::Blocks(
                items
                    .into_iter()
                    .map(|raw| ContentBlock::Unknown { raw })
                    .collect(),
            ),
            other => MessageContent::Blocks(vec![ContentBlock::Unknown { raw: other }]),
        };
        Self {
            role: Role::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            meta: None,
        }
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
    pub id: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaRoutingMode {
    Native,
    Bridge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaRoutingPlan {
    pub mode: MediaRoutingMode,
    pub target_keys: Vec<String>,
    pub source_artifact_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestMetadata {
    /// The protocol the client spoke.
    pub source_protocol: Option<ProtocolId>,
    /// Raw envelope preserved for pass-through / audit.
    pub raw: Option<RawEnvelope>,
    /// Three-segment vendor extension bag.
    pub vendor: VendorExtensions,
    pub(crate) media_routing: Option<MediaRoutingPlan>,
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
#[derive(Debug, Clone)]
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

    /// Return the modalities from `OpenAIChatExt` if present.
    pub fn modalities(&self) -> Option<&Vec<String>> {
        if let Some(ProtocolExt::OpenAiChat(ref e)) = self.ext {
            e.modalities.as_ref()
        } else {
            None
        }
    }
}
