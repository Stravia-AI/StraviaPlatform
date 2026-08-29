use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use super::tool::{AgentToolContext, AgentToolRegistry};
use super::{
    AgentDefinitionId, AgentDefinitionRegistry, AgentDefinitionSpec, AgentTool, ArtifactId,
    ArtifactPolicy, ArtifactSource, ArtifactStore, CanonicalEvent, CapabilityModelAuthorization,
    ModelTurnExecutor, TurnInput, VersionedToolId,
};
use crate::hook::{
    ContextCompleteness, HookControl, HookRuntime, InferenceRun, PlatformToolResult, Principal,
    RequestKind, SessionContext, ToolId, TransportKind,
};
use crate::model_turn::ModelTurnAuthorization;
use crate::protocol::ir::{
    AiItem, AiRequest, AiResponse, ContentBlock, MediaSource, MessageContent, Role, ToolCall, Usage,
};
use crate::proxy::context::CancellationToken;
use crate::turn_chain::{TurnChainStore, TurnCommit, TurnNodeId, TurnNodeKind};

mod types;
pub use types::*;
use types::{AgentCommitPolicy, ResolvedAgentExecution, RunLimitStore};

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
