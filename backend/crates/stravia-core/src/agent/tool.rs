use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{AgentTurnId, VersionedToolId};
use crate::hook::Principal;
use crate::protocol::ir::ToolSpec;
use crate::proxy::context::CancellationToken;

#[derive(Clone)]
pub struct AgentToolContext {
    pub principal: Principal,
    pub turn_id: AgentTurnId,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct AgentToolError {
    pub code: String,
    pub message: String,
}

impl AgentToolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn id(&self) -> VersionedToolId;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn parallel_safe(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        context: AgentToolContext,
        input: Value,
    ) -> Result<Value, AgentToolError>;
}

#[derive(Clone, Default)]
pub(crate) struct AgentToolRegistry {
    tools: Arc<HashMap<VersionedToolId, Arc<dyn AgentTool>>>,
}

impl AgentToolRegistry {
    pub(crate) fn new(tools: Vec<Arc<dyn AgentTool>>) -> Result<Self, AgentToolError> {
        let mut by_id = HashMap::with_capacity(tools.len());
        for tool in tools {
            let id = tool.id();
            if id.id.trim().is_empty() || id.version == 0 {
                return Err(AgentToolError::new(
                    "invalid_tool",
                    "Agent Tool ID and version must be valid",
                ));
            }
            if by_id.insert(id.clone(), tool).is_some() {
                return Err(AgentToolError::new(
                    "duplicate_tool",
                    format!("duplicate Agent Tool {}@{}", id.id, id.version),
                ));
            }
        }
        Ok(Self {
            tools: Arc::new(by_id),
        })
    }

    pub(crate) fn resolve(&self, id: &VersionedToolId) -> Option<Arc<dyn AgentTool>> {
        self.tools.get(id).cloned()
    }

    pub(crate) fn resolve_model_name(
        &self,
        allowlist: &[VersionedToolId],
        model_name: &str,
    ) -> Option<(VersionedToolId, Arc<dyn AgentTool>)> {
        allowlist.iter().find_map(|id| {
            (model_tool_name(id) == model_name)
                .then(|| self.resolve(id).map(|tool| (id.clone(), tool)))
                .flatten()
        })
    }

    pub(crate) fn model_specs(
        &self,
        allowlist: &[VersionedToolId],
    ) -> Result<Vec<ToolSpec>, AgentToolError> {
        let mut names = HashSet::with_capacity(allowlist.len());
        allowlist
            .iter()
            .map(|id| {
                let tool = self.resolve(id).ok_or_else(|| {
                    AgentToolError::new(
                        "tool_unavailable",
                        format!("Agent Tool {}@{} is unavailable", id.id, id.version),
                    )
                })?;
                let name = model_tool_name(id);
                if !names.insert(name.clone()) {
                    return Err(AgentToolError::new(
                        "tool_name_collision",
                        format!(
                            "Agent Tool model name collides for {}@{}",
                            id.id, id.version
                        ),
                    ));
                }
                Ok(ToolSpec {
                    name,
                    description: Some(tool.description().to_owned()),
                    parameters: tool.input_schema(),
                    strict: Some(true),
                    cache_control: None,
                    meta: None,
                })
            })
            .collect()
    }
}

fn model_tool_name(id: &VersionedToolId) -> String {
    let already_safe = !id.id.is_empty()
        && id.id.len() <= 64
        && id
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if already_safe {
        return id.id.clone();
    }
    let digest = Sha256::digest(format!("{}@{}", id.id, id.version).as_bytes());
    let mut encoded = String::with_capacity(61);
    encoded.push_str("tool_");
    use std::fmt::Write;
    for byte in digest.iter().take(28) {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}
