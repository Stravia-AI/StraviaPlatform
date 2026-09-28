mod rejections;
mod sql;
mod syntax;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::ToolCall;

pub(crate) use rejections::{ReasoningRejections, protected_payload_digests};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<stravia_runtime_contract::protocol::ids::ProtocolIdentity>,
    pub actual_model: String,
    pub target_id: String,
    /// 能验证受保护推理的签发方作用域指纹：只含协议、部署、凭据身份，以及协议
    /// 要求时的模型。代理、路由 Target、vendor 选项等不影响签名有效性，不参与比较。
    /// 旧记录没有该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
}

/// 历史条目的受保护推理能否回放给当前 Target。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingProvenance {
    /// 来源记录证明由同一签发作用域产生。
    Verified,
    /// 无来源记录（客户端提供或记录已丢失），或同一出口协议下签发作用域不同：
    /// 上游仍可能接受，乐观回放。
    Unknown,
    /// 当前签发作用域已拒绝过、出口协议不同、协议绑定的模型不同，或私有记录已损坏。
    Foreign,
}

/// 已知协议按协议族比较；插件自定义的协议标识按原字符串比较。
fn same_protocol(
    left: &stravia_runtime_contract::protocol::ids::ProtocolIdentity,
    right: &stravia_runtime_contract::protocol::ids::ProtocolIdentity,
) -> bool {
    match (left.protocol(), right.protocol()) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

impl ThinkingSource {
    const ITEM_META_KEY: &'static str = "__stravia_thinking_source";

    pub(crate) fn from_item(item: &stravia_runtime_contract::protocol::ir::AiItem) -> Option<Self> {
        serde_json::from_value(item.meta.as_ref()?.get(Self::ITEM_META_KEY)?.clone()).ok()
    }

    /// 按兼容性判定（ADR-0075）：只有已被当前签发作用域拒绝过，或能确定上游必然拒绝
    /// （出口协议不同、协议绑定的模型不同）时才判为 Foreign；签发作用域不同但仍可能
    /// 被接受的载荷按来源不明处理，交给上游校验与拒绝恢复。
    pub(crate) fn provenance(
        &self,
        item: &stravia_runtime_contract::protocol::ir::AiItem,
        rejections: &ReasoningRejections,
    ) -> ThinkingProvenance {
        if let Some(authority) = &self.authority
            && rejections.rejected(authority, item)
        {
            return ThinkingProvenance::Foreign;
        }
        let Some(stamp) = item
            .meta
            .as_ref()
            .and_then(|meta| meta.get(Self::ITEM_META_KEY))
        else {
            return ThinkingProvenance::Unknown;
        };
        let Ok(source) = serde_json::from_value::<Self>(stamp.clone()) else {
            return ThinkingProvenance::Foreign;
        };
        let same_scope = match &source.authority {
            Some(authority) => self.authority.as_ref() == Some(authority),
            // 旧记录只有整体 namespace：相等仍可证明，不等时拆不出签发作用域。
            None => source.namespace == self.namespace,
        };
        if same_scope {
            return ThinkingProvenance::Verified;
        }
        if let (Some(egress), Some(issuer)) = (&self.protocol, &source.protocol)
            && !same_protocol(egress, issuer)
        {
            return ThinkingProvenance::Foreign;
        }
        if self
            .protocol
            .as_ref()
            .and_then(|protocol| protocol.protocol())
            .is_some_and(stravia_protocol_codec::transform::protected_thinking_binds_model)
            && source.actual_model != self.actual_model
        {
            return ThinkingProvenance::Foreign;
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
        item.meta
            .get_or_insert_with(Default::default)
            .insert_graph_extension(
                Self::ITEM_META_KEY,
                serde_json::to_value(self).expect("thinking source is serializable"),
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
