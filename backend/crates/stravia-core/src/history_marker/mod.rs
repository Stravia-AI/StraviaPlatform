mod sql;
mod syntax;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::hook::Principal;
use crate::protocol::ir::{ContentBlock, ToolCall};

pub use sql::SqlHistoryMarkerStore;
pub use syntax::{
    HISTORY_MARKER_PREFIX, MarkerResolution, PROJECTION_DELIMITER_PREFIX,
    history_marker_references, render_history_marker, resolve_request_markers,
};
pub(crate) use syntax::{
    render_history_marker_reference, render_preview_projection_span, render_text_projection_end,
    render_text_projection_span, render_text_projection_start,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMarkerKind {
    Platform,
    Thinking,
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
