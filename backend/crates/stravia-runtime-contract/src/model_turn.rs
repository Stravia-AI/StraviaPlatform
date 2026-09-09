use crate::protocol::ir::{AiResponse, AiStreamDelta};
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
    pub upstream_body: Option<serde_json::Value>,
}

impl ModelTurnError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            upstream_status: None,
            upstream_body: None,
        }
    }
}

pub type CanonicalEventStream =
    Pin<Box<dyn Stream<Item = Result<CanonicalEvent, ModelTurnError>> + Send>>;
