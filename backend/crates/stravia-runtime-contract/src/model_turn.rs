use crate::protocol::ir::{AiErrorKind, AiResponse, AiStreamDelta};
use futures::Stream;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub enum CanonicalEvent {
    Delta(AiStreamDelta),
    Completed(Box<AiResponse>),
    Compacted(Box<crate::protocol::ir::NativeCompactionResponse>),
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct ModelTurnError {
    pub code: String,
    pub message: String,
    pub upstream_status: Option<u16>,
    /// 保留规范分类，避免把同为 HTTP 429 的额度耗尽重新解释为速率限制。
    pub upstream_error_kind: Option<AiErrorKind>,
    pub upstream_body: Option<Box<serde_json::Value>>,
}

impl ModelTurnError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            upstream_status: None,
            upstream_error_kind: None,
            upstream_body: None,
        }
    }
}

pub type CanonicalEventStream =
    Pin<Box<dyn Stream<Item = Result<CanonicalEvent, ModelTurnError>> + Send>>;
