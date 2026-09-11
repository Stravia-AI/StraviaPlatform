mod sql;
mod syntax;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::ToolCall;

pub use sql::SqlHistoryMarkerStore;
#[cfg(test)]
pub(crate) use syntax::render_text_projection_span;
pub use syntax::{
    HISTORY_MARKER_PREFIX, MarkerResolution, PROJECTION_DELIMITER_PREFIX,
    history_marker_references, render_history_marker, resolve_request_markers,
};
pub(crate) use syntax::{
    new_reference, render_history_marker_reference, render_preview_projection_end,
    render_preview_projection_span, render_preview_projection_start, valid_reference,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMarkerKind {
    Platform,
    Thinking,
}

/// Actual provider identity for protected reasoning replay. Stored only in private history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingSource {
    pub namespace: String,
    #[serde(with = "thinking_source_protocol")]
    pub protocol: stravia_runtime_contract::protocol::ids::ProtocolEndpoint,
    pub actual_model: String,
    pub target_id: String,
}

impl ThinkingSource {
    pub(crate) fn from_item(item: &stravia_runtime_contract::protocol::ir::AiItem) -> Option<Self> {
        serde_json::from_value(
            item.meta
                .as_ref()?
                .get("__stravia_thinking_source")?
                .clone(),
        )
        .ok()
    }

    pub(crate) fn stamp_response(
        &self,
        response: &mut stravia_runtime_contract::protocol::ir::AiResponse,
    ) {
        for item in &mut response.items {
            if let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
                &item.content
            {
                if blocks.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Thinking { .. }
                            | ContentBlock::Reasoning { .. }
                            | ContentBlock::RedactedThinking { .. }
                    )
                }) && Self::from_item(item).is_none()
                {
                    self.stamp_item(item);
                }
            }
        }
    }

    pub(crate) fn stamp_item(&self, item: &mut stravia_runtime_contract::protocol::ir::AiItem) {
        let meta = item.meta.get_or_insert_with(|| serde_json::json!({}));
        if !meta.is_object() {
            *meta = serde_json::json!({ "vendor_meta": meta.take() });
        }
        meta.as_object_mut()
            .expect("item metadata is an object")
            .insert(
                "__stravia_thinking_source".into(),
                serde_json::to_value(self).expect("thinking source is serializable"),
            );
    }
}

mod thinking_source_protocol {
    use serde::{Deserialize, Deserializer, Serializer};
    use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;

    pub fn serialize<S: Serializer>(
        value: &ProtocolEndpoint,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ProtocolEndpoint, D::Error> {
        let value = String::deserialize(deserializer)?;
        crate::protocol::registry::ProtocolRegistry::global()
            .resolve_alias(&value)
            .ok_or_else(|| serde::de::Error::custom("unknown thinking source protocol"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HiddenHistorySegment {
    Platform {
        call: ToolCall,
        result: ContentBlock,
    },
    Thinking {
        block: ContentBlock,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<ThinkingSource>,
    },
}

impl HiddenHistorySegment {
    pub fn kind(&self) -> HistoryMarkerKind {
        match self {
            Self::Platform { .. } => HistoryMarkerKind::Platform,
            Self::Thinking { .. } => HistoryMarkerKind::Thinking,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMarker {
    pub reference: String,
    pub kind: HistoryMarkerKind,
    pub activity: String,
}

pub(crate) fn reserve_thinking_marker() -> HistoryMarker {
    HistoryMarker {
        reference: new_reference(),
        kind: HistoryMarkerKind::Thinking,
        activity: "Preserving protected reasoning".into(),
    }
}

#[derive(Debug, Clone)]
pub struct PlatformMarkerInput {
    pub tool_id: String,
    pub call: ToolCall,
    pub activity: String,
    pub execution_limit: Duration,
    pub pending_retention: Duration,
}

#[derive(Debug, Clone)]
pub struct ThinkingMarkerInput {
    pub block: ContentBlock,
    pub source: Option<ThinkingSource>,
    pub activity: String,
    pub pending_retention: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformExecutionState {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct ResolvedHistoryMarker {
    pub marker: HistoryMarker,
    pub execution_state: Option<PlatformExecutionState>,
    pub execution_deadline_unix_ms: Option<i64>,
    pub segment: Option<HiddenHistorySegment>,
    pub published: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed,
    Busy,
    Terminal,
    NotFound,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryMarkerError {
    #[error("history marker storage failed: {0}")]
    Storage(String),
    #[error("history marker terminal payload conflicts with its immutable result")]
    TerminalConflict,
    #[error("history marker payload does not match its protected unit")]
    InvalidPayload,
}

#[async_trait]
pub trait HistoryMarkerStore: Send + Sync {
    async fn create_platform(
        &self,
        principal: &Principal,
        input: PlatformMarkerInput,
    ) -> Result<HistoryMarker, HistoryMarkerError>;

    async fn create_thinking(
        &self,
        principal: &Principal,
        input: ThinkingMarkerInput,
    ) -> Result<HistoryMarker, HistoryMarkerError>;

    async fn create_reserved_thinking(
        &self,
        principal: &Principal,
        reserved: &HistoryMarker,
        input: ThinkingMarkerInput,
    ) -> Result<HistoryMarker, HistoryMarkerError>;

    async fn resolve(
        &self,
        principal: &Principal,
        reference: &str,
    ) -> Result<Option<ResolvedHistoryMarker>, HistoryMarkerError>;

    async fn claim_execution(
        &self,
        principal: &Principal,
        reference: &str,
        owner_id: &str,
        lease: Duration,
    ) -> Result<ClaimOutcome, HistoryMarkerError>;

    async fn finish_execution(
        &self,
        principal: &Principal,
        reference: &str,
        owner_id: &str,
        state: PlatformExecutionState,
        segment: HiddenHistorySegment,
    ) -> Result<(), HistoryMarkerError>;

    async fn wait_terminal(
        &self,
        principal: &Principal,
        reference: &str,
    ) -> Result<Option<ResolvedHistoryMarker>, HistoryMarkerError>;

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), HistoryMarkerError>;

    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), HistoryMarkerError>;

    async fn cleanup_expired(&self) -> Result<u64, HistoryMarkerError>;
}

#[cfg(test)]
mod tests;
