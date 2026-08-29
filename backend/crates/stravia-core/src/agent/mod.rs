mod adapters;
mod artifact;
mod definition;
mod definition_store;
mod runner;
mod tool;
pub(crate) use adapters::{AgentCallMcpTool, AgentCallPlatformTool, AgentDefinitionHook};
pub use adapters::{
    McpToolAgentAdapter, PlatformToolAgentAdapter, RemoteMcpToolSource, discover_remote_mcp_tools,
};
pub use artifact::{
    ArtifactByteStream, ArtifactError, ArtifactId, ArtifactReader, ArtifactRef, ArtifactSource,
    ArtifactStore, ArtifactUpload, ArtifactUploadRequest, LocalArtifactStore, MAX_ARTIFACT_BYTES,
    UploadedArtifactPart, bytes_stream,
};

pub use definition::{
    AgentBudgets, AgentDefinitionConfig, AgentDefinitionError, AgentDefinitionExposure,
    AgentDefinitionId, AgentDefinitionRecord, AgentDefinitionRegistry, AgentDefinitionSpec,
    AgentSlug, ArtifactPolicy, VersionedToolId,
};
pub use runner::{
    AgentCompletion, AgentEvent, AgentEventStream, AgentInput, AgentOutputValidationContext,
    AgentOutputValidator, AgentResult, AgentRunError, AgentRunLimits, AgentTurnId,
};
pub(crate) use runner::{AgentRunGuard, AgentRunLifecycle, AgentRunner, AgentToolAuthorizer};
pub use tool::{AgentTool, AgentToolContext, AgentToolError};

#[cfg(test)]
pub(crate) use crate::model_turn::InMemoryModelTurnExecutor;
pub use crate::model_turn::{
    CanonicalEvent, CanonicalEventStream, ModelTurn, ModelTurnError, ModelTurnExecutor, TurnInput,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityModelAuthorization {
    MediaUnderstanding,
    WebSearch,
}
