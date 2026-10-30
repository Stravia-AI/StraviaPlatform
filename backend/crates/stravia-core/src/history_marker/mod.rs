mod rejections;
mod sql;
mod syntax;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::ToolCall;

pub(crate) use rejections::{ReasoningRejections, RejectedReasoning, protected_payload_digest};
pub use sql::SqlHistoryMarkerStore;
pub use syntax::{
    HISTORY_MARKER_PREFIX, MarkerResolution, PROJECTION_DELIMITER_PREFIX,
    history_marker_references, render_history_marker, resolve_request_markers,
};
pub(crate) use syntax::{
    client_shaped_request, history_marker_references_iter, mark_current_run_projected,
    new_reference, render_preview_projection_end, render_preview_projection_span,
    render_preview_projection_start, request_needs_marker_resolution, valid_reference,
};
#[cfg(test)]
pub(crate) use syntax::{render_history_marker_reference, render_text_projection_span};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<stravia_runtime_contract::protocol::ids::ProtocolIdentity>,
    pub actual_model: String,
    pub target_id: String,
    /// 能验证受保护推理的签发方作用域指纹：只含协议、部署、凭据身份，以及协议
    /// 要求时的模型。代理、路由 Target、vendor 选项等不影响签名有效性，不参与比较。
    /// 旧记录没有该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    /// 本条目实际签发的载荷摘要；旧记录为空时沿用条目级来源。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issued: Vec<String>,
}

/// 单个受保护载荷相对当前签发作用域的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThinkingProvenance {
    Own,
    Foreign(String),
    Unknown,
}

impl ThinkingSource {
    const ITEM_META_KEY: &'static str = "__stravia_thinking_source";

    pub(crate) fn from_item(item: &stravia_runtime_contract::protocol::ir::AiItem) -> Option<Self> {
        serde_json::from_value(item.meta.as_ref()?.get(Self::ITEM_META_KEY)?.clone()).ok()
    }

    /// 来源跟随载荷而不是容器，插入已打标条目的其它载荷不能继承该条目的证明。
    pub(crate) fn provenance(
        &self,
        item: &stravia_runtime_contract::protocol::ir::AiItem,
        block: &ContentBlock,
    ) -> ThinkingProvenance {
        let Some(source) = Self::from_item(item) else {
            return ThinkingProvenance::Unknown;
        };
        if !source.issued.is_empty()
            && protected_payload_digest(block)
                .is_none_or(|digest| !source.issued.contains(&rejections::digest_hex(&digest)))
        {
            return ThinkingProvenance::Unknown;
        }
        let same_scope = match &source.authority {
            Some(authority) => self.authority.as_ref() == Some(authority),
            // 旧记录只有整体 namespace：相等仍可证明，不等时拆不出签发作用域。
            None => source.namespace == self.namespace,
        };
        if same_scope {
            return ThinkingProvenance::Own;
        }
        if let Some(authority) = source.authority {
            return ThinkingProvenance::Foreign(authority);
        }
        ThinkingProvenance::Unknown
    }

    pub(crate) fn stamp_response(
        &self,
        response: &mut stravia_runtime_contract::protocol::ir::AiResponse,
    ) {
        for item in &mut response.items {
            if let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
                &item.content
                && blocks.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Thinking { .. }
                            | ContentBlock::Reasoning { .. }
                            | ContentBlock::RedactedThinking { .. }
                    )
                })
                && Self::from_item(item).is_none()
            {
                self.stamp_item(item);
            }
        }
    }

    pub(crate) fn stamp_item(&self, item: &mut stravia_runtime_contract::protocol::ir::AiItem) {
        let mut source = self.clone();
        source.issued = match &item.content {
            stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(protected_payload_digest)
                .map(|digest| rejections::digest_hex(&digest))
                .collect(),
            _ => Vec::new(),
        };
        item.meta
            .get_or_insert_with(Default::default)
            .insert_graph_extension(
                Self::ITEM_META_KEY,
                serde_json::to_value(source).expect("thinking source is serializable"),
            )
            .expect("thinking source key is not reserved");
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
pub struct PublicExecutionInput {
    pub source_run_id: Option<String>,
    pub retention: Duration,
    /// Original acceptance deadline, captured before any asynchronous work.
    pub deadline_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub struct PublicExecution {
    pub id: String,
    pub marker_reference: String,
    pub state: PlatformExecutionState,
    pub deadline_unix_ms: i64,
    pub segment: Option<HiddenHistorySegment>,
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
    async fn create_public_execution(
        &self,
        principal: &Principal,
        marker_reference: &str,
        input: PublicExecutionInput,
    ) -> Result<String, HistoryMarkerError>;

    async fn resolve_public_execution(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<PublicExecution>, HistoryMarkerError>;

    /// Calls `before_commit` exactly once after a successful transactional update,
    /// immediately before COMMIT. A false result explicitly rolls the update back
    /// and returns `TerminalConflict`. A COMMIT error can be ambiguous: a confirmed
    /// persisted handoff succeeds; otherwise the storage error remains unresolved.
    async fn handoff_public_execution(
        &self,
        principal: &Principal,
        id: &str,
        before_commit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), HistoryMarkerError>;

    async fn recover_public_executions(&self) -> Result<(), HistoryMarkerError>;

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

    /// Resolve several markers in one pass; `resolved[i]` corresponds to
    /// `references[i]`. The default resolves serially; stores with real batch
    /// reads override it.
    async fn resolve_many(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<Vec<Option<ResolvedHistoryMarker>>, HistoryMarkerError> {
        let mut resolved = Vec::with_capacity(references.len());
        for reference in references {
            resolved.push(self.resolve(principal, reference).await?);
        }
        Ok(resolved)
    }

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
