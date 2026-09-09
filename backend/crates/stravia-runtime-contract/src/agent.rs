use crate::artifact::ArtifactId;
use crate::protocol::ir::{AiItem, Usage};
use crate::turn_chain::TurnNodeId;
use crate::{CancellationToken, Principal};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentDefinitionId(String);

impl AgentDefinitionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSlug(String);

impl AgentSlug {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn tool_name(&self) -> String {
        format!("agent_{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionedToolId {
    pub id: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBudgets {
    pub total_wall_time: Duration,
    pub working_wall_time: Duration,
    pub model_turns: u32,
    pub tool_calls: Option<u32>,
    pub tool_parallelism: Option<u32>,
    pub concurrent_runs: Option<u32>,
    pub total_tokens: Option<u32>,
    pub finalization_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPolicy {
    pub max_artifacts: u32,
    pub max_bytes: u64,
    pub allowed_mime_types: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDefinitionExposure {
    #[default]
    Public,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinitionSpec {
    pub id: AgentDefinitionId,
    pub slug: AgentSlug,
    pub revision: u32,
    pub description: String,
    pub instructions: String,
    pub output_schema: Option<Value>,
    pub tools: Vec<VersionedToolId>,
    pub budgets: AgentBudgets,
    pub artifact_policy: ArtifactPolicy,
    pub repair_attempts: u32,
    #[serde(default)]
    pub exposure: AgentDefinitionExposure,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinitionConfig {
    pub enabled: bool,
    pub model_id: Option<String>,
    pub thinking_level: Option<crate::thinking::ThinkingLevel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinitionRecord {
    pub spec: AgentDefinitionSpec,
    pub spec_hash: String,
    pub config: AgentDefinitionConfig,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentDefinitionError {
    #[error("invalid Agent Definition: {0}")]
    Invalid(String),
    #[error("Agent Definition storage failed: {0}")]
    Storage(String),
    #[error("Agent Definition not found")]
    NotFound,
}

pub type AgentTurnId = TurnNodeId;
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;
#[derive(Debug, Clone)]
pub struct AgentInput {
    pub principal: Principal,
    pub definition_id: AgentDefinitionId,
    pub parent_turn_id: Option<AgentTurnId>,
    pub prompt: String,
    pub artifacts: Vec<ArtifactId>,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentRunLimits {
    pub max_turns: u32,
    pub total_time: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCompletion {
    #[serde(rename = "complete")]
    Completed,
    Partial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub turn_id: AgentTurnId,
    pub completion: AgentCompletion,
    pub output: Value,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStarted {
        turn_id: AgentTurnId,
    },
    ModelStepStarted {
        ordinal: u32,
    },
    PublicOutputDelta {
        text: String,
    },
    ToolStarted {
        tool: VersionedToolId,
        ordinal: u32,
    },
    ToolFinished {
        tool: VersionedToolId,
        ordinal: u32,
        is_error: bool,
    },
    UsageUpdated {
        usage: Usage,
    },
    Completed(AgentResult),
    Partial(AgentResult),
    Failed {
        error: AgentRunError,
    },
}

impl AgentEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed(_) | Self::Partial(_) | Self::Failed { .. }
        )
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
#[error("{message}")]
pub struct AgentRunError {
    pub code: String,
    pub message: String,
}

impl AgentRunError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentOutputValidationContext {
    pub principal: Principal,
    pub turn_id: AgentTurnId,
    pub definition_id: AgentDefinitionId,
    pub definition_revision: u32,
    pub completion: AgentCompletion,
}

#[async_trait]
pub trait AgentOutputValidator: Send + Sync {
    async fn validate(
        &self,
        context: &AgentOutputValidationContext,
        transcript: &[AiItem],
        output: Value,
    ) -> Result<Value, AgentRunError>;

    async fn before_commit(
        &self,
        _context: &AgentOutputValidationContext,
        _transcript: &[AiItem],
        _output: &Value,
    ) -> Result<(), AgentRunError> {
        Ok(())
    }
}
