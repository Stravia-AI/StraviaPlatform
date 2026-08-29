use super::*;

pub type AgentTurnId = TurnNodeId;
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;
pub(super) type RunLimitKey = (AgentDefinitionId, u32);
pub(super) type RunLimitMap = HashMap<RunLimitKey, Arc<Semaphore>>;
pub(super) type RunLimitStore = Arc<Mutex<RunLimitMap>>;

#[derive(Debug, Clone)]
pub struct AgentInput {
    pub principal: Principal,
    pub definition_id: AgentDefinitionId,
    pub parent_turn_id: Option<AgentTurnId>,
    pub prompt: String,
    pub artifacts: Vec<ArtifactId>,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentCommitPolicy {
    CommitAgentTurn,
    Ephemeral,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentRunLimits {
    pub max_turns: u32,
    pub total_time: Duration,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedAgentExecution {
    pub(super) definition_revision: u32,
    pub(super) model_id: String,
    pub(super) limits: AgentRunLimits,
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
pub(crate) trait AgentRunGuard: Send {}

#[async_trait]
pub(crate) trait AgentRunLifecycle: Send + Sync {
    async fn start(
        &self,
        principal: &Principal,
        run_id: &AgentTurnId,
    ) -> Result<Box<dyn AgentRunGuard>, AgentRunError>;
}

/// Live authorization seam used immediately before an Agent Tool side effect.
///
/// The Gateway implementation revalidates the current Definition, bound Model,
/// and API-key grant through the canonical Security policy. Library users and
/// unit tests may omit it when they do not provide a security store.
#[async_trait]
pub(crate) trait AgentToolAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        principal: &Principal,
        definition_id: &AgentDefinitionId,
        model_id: &str,
    ) -> Result<(), AgentRunError>;
}
