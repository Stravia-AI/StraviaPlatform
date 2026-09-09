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
pub use artifact::LocalArtifactStore;
pub use definition::AgentDefinitionRegistry;
pub(crate) use runner::{AgentRunGuard, AgentRunLifecycle, AgentRunner, AgentToolAuthorizer};
use stravia_runtime_contract::agent::*;
use stravia_runtime_contract::artifact::*;
pub use tool::{AgentTool, AgentToolContext, AgentToolError, AgentToolOutput};

#[cfg(test)]
pub(crate) use crate::model_turn::InMemoryModelTurnExecutor;
pub use crate::model_turn::ModelTurn;
pub use crate::model_turn::ModelTurnExecutor;
pub use crate::model_turn::TurnInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityModelAuthorization {
    MediaUnderstanding,
    WebSearch,
}
