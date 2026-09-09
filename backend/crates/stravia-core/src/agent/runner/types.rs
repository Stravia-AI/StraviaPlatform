use super::*;

pub(super) type RunLimitKey = (AgentDefinitionId, u32);
pub(super) type RunLimitMap = HashMap<RunLimitKey, Arc<Semaphore>>;
pub(super) type RunLimitStore = Arc<Mutex<RunLimitMap>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentCommitPolicy {
    CommitAgentTurn,
    Ephemeral,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedAgentExecution {
    pub(super) definition_revision: u32,
    pub(super) model_id: String,
    pub(super) limits: AgentRunLimits,
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
