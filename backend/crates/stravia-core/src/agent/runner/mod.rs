use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use futures::{StreamExt, stream};
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use super::tool::{AgentToolContext, AgentToolRegistry};
use super::{
    AgentDefinitionRegistry, AgentTool, CapabilityModelAuthorization, ModelTurnExecutor, TurnInput,
};
use crate::hook::{HookRuntime, InferenceRun};
use crate::model_turn::ModelTurnAuthorization;
use stravia_runtime_contract::agent::{
    AgentCompletion, AgentDefinitionId, AgentDefinitionSpec, AgentEvent, AgentEventStream,
    AgentInput, AgentOutputValidationContext, AgentOutputValidator, AgentResult, AgentRunError,
    AgentRunLimits, AgentTurnId, ArtifactPolicy, VersionedToolId,
};
use stravia_runtime_contract::artifact::{ArtifactId, ArtifactSource, ArtifactStore};
use stravia_runtime_contract::hook::{
    ContextCompleteness, HookControl, PlatformToolResult, RequestKind, SessionContext, ToolId,
    TransportKind,
};
use stravia_runtime_contract::model_turn::CanonicalEvent;
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, AiResponse, ContentBlock, MediaSource, MessageContent, Role, ToolCall, Usage,
};
use stravia_runtime_contract::turn_chain::{TurnChainStore, TurnCommit, TurnNodeKind};
use stravia_runtime_contract::{CancellationToken, Principal};

mod context;
mod tools;
mod types;
use types::{AgentCommitPolicy, ResolvedAgentExecution, RunLimitStore};
pub(crate) use types::{AgentRunGuard, AgentRunLifecycle, AgentToolAuthorizer};

#[derive(Clone)]
pub struct AgentRunner {
    definitions: AgentDefinitionRegistry,
    model: Arc<dyn ModelTurnExecutor>,
    tools: AgentToolRegistry,
    turns: Arc<dyn TurnChainStore>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    run_lifecycles: Arc<[Arc<dyn AgentRunLifecycle>]>,
    tool_authorizer: Option<Arc<dyn AgentToolAuthorizer>>,
    run_limits: RunLimitStore,
    output_validators: Arc<HashMap<(AgentDefinitionId, u32), Arc<dyn AgentOutputValidator>>>,
    capability_model_authorizations:
        Arc<HashMap<(AgentDefinitionId, u32), CapabilityModelAuthorization>>,
    hooks: Option<HookRuntime>,
}

mod r#loop;
mod schema;

#[cfg(test)]
use r#loop::model_instructions;
use schema::*;

#[cfg(test)]
mod tests;
