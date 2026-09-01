mod agent_call;
mod hooks;
mod remote_mcp;

pub(crate) use agent_call::{AgentCallMcpTool, AgentCallPlatformTool};
pub(crate) use hooks::AgentDefinitionHook;
pub use remote_mcp::{RemoteMcpToolSource, discover_remote_mcp_tools};

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Value;

use super::{
    AgentDefinitionId, AgentDefinitionRegistry, AgentEvent, AgentInput, AgentRunError, AgentRunner,
    AgentTool, AgentToolContext, AgentToolError, AgentTurnId, ArtifactId, VersionedToolId,
};
use crate::Gateway;
use crate::hook::{
    ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookSession, PlatformTool,
    PlatformToolError, Principal, RequestKind, ResponsePatch, SessionContext, ToolExecutionContext,
    ToolId,
};
use crate::mcp::{McpContext, McpTool, McpToolError, McpToolOutput};
use crate::protocol::ir::ContentBlock;
use crate::proxy::context::CancellationToken;
use crate::proxy::security::Security;

pub struct PlatformToolAgentAdapter {
    tool: Arc<dyn PlatformTool>,
    id: VersionedToolId,
    description: String,
}

impl PlatformToolAgentAdapter {
    pub fn new(tool: Arc<dyn PlatformTool>, version: u32) -> Self {
        let description = tool
            .description()
            .unwrap_or(tool.external_name())
            .to_owned();
        Self {
            id: VersionedToolId {
                id: tool.external_name().to_owned(),
                version,
            },
            tool,
            description,
        }
    }

    pub(crate) fn with_id(tool: Arc<dyn PlatformTool>, id: VersionedToolId) -> Self {
        let description = tool
            .description()
            .unwrap_or(tool.external_name())
            .to_owned();
        Self {
            tool,
            id,
            description,
        }
    }
}

#[async_trait]
impl AgentTool for PlatformToolAgentAdapter {
    fn id(&self) -> VersionedToolId {
        self.id.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.tool.parameters()
    }
    fn parallel_safe(&self) -> bool {
        self.tool.parallel_safe()
    }

    async fn execute(
        &self,
        context: AgentToolContext,
        input: Value,
    ) -> Result<Value, AgentToolError> {
        let output = self
            .tool
            .execute_result(
                input,
                ToolExecutionContext {
                    request_id: context.turn_id.to_string(),
                    run_id: context.turn_id.to_string(),
                    principal: context.principal,
                    cancellation: context.cancellation,
                    progress: None,
                },
            )
            .await
            .map_err(|error| AgentToolError::new("platform_tool_failed", error.message))?;
        let content = blocks_to_value(output.content);
        if output.is_error {
            Err(AgentToolError::new(
                "platform_tool_error",
                content.to_string(),
            ))
        } else {
            Ok(content)
        }
    }
}

pub struct McpToolAgentAdapter {
    tool: Arc<dyn McpTool>,
    id: VersionedToolId,
    description: String,
}

impl McpToolAgentAdapter {
    pub fn new(tool: Arc<dyn McpTool>, version: u32) -> Self {
        Self {
            id: VersionedToolId {
                id: tool.name().to_owned(),
                version,
            },
            description: tool.description().unwrap_or(tool.name()).to_owned(),
            tool,
        }
    }
}

#[async_trait]
impl AgentTool for McpToolAgentAdapter {
    fn id(&self) -> VersionedToolId {
        self.id.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.tool.input_schema()
    }

    async fn execute(
        &self,
        context: AgentToolContext,
        input: Value,
    ) -> Result<Value, AgentToolError> {
        let mcp = McpContext::new(context.principal.api_key_id().to_owned());
        if !self
            .tool
            .available(&mcp)
            .await
            .map_err(|error| AgentToolError::new(error.code, error.message))?
        {
            return Err(AgentToolError::new(
                "mcp_tool_unavailable",
                "MCP Agent Tool is unavailable for this principal",
            ));
        }
        let output = self
            .tool
            .call(input, &mcp)
            .await
            .map_err(|error| AgentToolError::new(error.code, error.message))?;
        if output.is_error {
            Err(AgentToolError::new(
                "mcp_tool_error",
                output.structured_content.to_string(),
            ))
        } else {
            Ok(output.structured_content)
        }
    }
}

fn blocks_to_value(blocks: Vec<ContentBlock>) -> Value {
    if let [ContentBlock::Unknown { raw }] = blocks.as_slice() {
        raw.clone()
    } else if let [ContentBlock::Text { text, .. }] = blocks.as_slice() {
        Value::String(text.clone())
    } else {
        serde_json::to_value(blocks).unwrap_or(Value::Null)
    }
}
