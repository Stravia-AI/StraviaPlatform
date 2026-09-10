use crate::protocol::ir::{ContentBlock, ToolResultContentKind, ToolSpec};
use crate::{CancellationToken, Principal};
use async_trait::async_trait;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_PLATFORM_TOOL_EXECUTION_LIMIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolId(String);

impl ToolId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Immutable, platform-selected capabilities for one StraviaRead execution.
/// This is deliberately not deserializable from tool arguments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadExposureScope {
    networking: bool,
    media: bool,
}

impl ReadExposureScope {
    pub const NONE: Self = Self::new(false, false);
    pub const FULL: Self = Self::new(true, true);

    pub const fn new(networking: bool, media: bool) -> Self {
        Self { networking, media }
    }

    pub const fn networking(self) -> bool {
        self.networking
    }

    pub const fn media(self) -> bool {
        self.media
    }

    pub const fn union(self, other: Self) -> Self {
        Self::new(
            self.networking || other.networking,
            self.media || other.media,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StraviaReadDomain {
    Query,
    WebPage,
    Media,
}

#[derive(Clone)]
pub struct ToolExecutionContext {
    pub request_id: String,
    pub run_id: String,
    pub principal: Principal,
    pub read_scope: ReadExposureScope,
    pub cancellation: CancellationToken,
    pub progress: Option<Arc<dyn ToolProgressSink>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProgress {
    pub call_id: String,
    pub phase: String,
    pub ordinal: u32,
    pub payload: Option<Value>,
}

pub trait ToolProgressSink: Send + Sync {
    fn emit(&self, progress: ToolProgress);
}

#[derive(Debug, Clone)]
pub struct PlatformToolOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub metadata: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlatformToolResult {
    pub tool_id: ToolId,
    pub call_id: String,
    pub content: Value,
    pub content_kind: ToolResultContentKind,
    pub is_error: bool,
    pub metadata: serde_json::Map<String, Value>,
}
impl PlatformToolResult {
    pub fn content_block(&self) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: self.call_id.clone(),
            content: self.content.clone(),
            content_kind: Some(self.content_kind),
            is_error: Some(self.is_error),
            cache_control: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlatformToolError {
    pub message: String,
}

impl PlatformToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[async_trait]
pub trait PlatformTool: Send + Sync + 'static {
    fn id(&self) -> ToolId;
    fn external_name(&self) -> &str;
    fn read_domain(&self) -> Option<StraviaReadDomain> {
        None
    }
    fn description(&self) -> Option<&str> {
        None
    }
    fn activity_label(&self) -> &str {
        "Running a platform tool"
    }
    fn execution_limit(&self) -> Option<Duration> {
        None
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn parameters(&self) -> Value;

    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError>;

    async fn execute_blocks(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Vec<ContentBlock>, PlatformToolError> {
        let content = self.execute(arguments, context).await?;
        Ok(vec![ContentBlock::Unknown { raw: content }])
    }
    async fn execute_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        Ok(PlatformToolOutput {
            content: self.execute_blocks(arguments, context).await?,
            is_error: false,
            metadata: serde_json::Map::new(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ExposedPlatformTool {
    pub id: ToolId,
    pub provider_name: String,
    pub spec: ToolSpec,
}
