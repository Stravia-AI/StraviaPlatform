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
    AgentTool, AgentToolContext, AgentToolError, AgentToolOutput, AgentTurnId, ArtifactId,
    VersionedToolId,
};
use crate::Gateway;
use crate::hook::tool::blocks_to_value;
use crate::mcp::{McpContext, McpTool, McpToolError, McpToolOutput};
use crate::proxy::security::Security;
use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::hook::ActionBatch;
use stravia_runtime_contract::hook::EventKind;
use stravia_runtime_contract::hook::Hook;
use stravia_runtime_contract::hook::HookAction;
use stravia_runtime_contract::hook::HookDescriptor;
use stravia_runtime_contract::hook::HookEvent;
use stravia_runtime_contract::hook::HookSession;
use stravia_runtime_contract::hook::PlatformTool;
use stravia_runtime_contract::hook::PlatformToolError;
use stravia_runtime_contract::hook::RequestKind;
use stravia_runtime_contract::hook::ResponsePatch;
use stravia_runtime_contract::hook::SessionContext;
use stravia_runtime_contract::hook::ToolExecutionContext;
use stravia_runtime_contract::hook::ToolId;
use stravia_runtime_contract::protocol::ir::ToolResultContentKind;

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
    ) -> Result<AgentToolOutput, AgentToolError> {
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
        let (content, content_kind) = blocks_to_value(output.content)
            .map_err(|error| AgentToolError::new("platform_tool_failed", error.message))?;
        if output.is_error {
            Err(AgentToolError::new(
                "platform_tool_error",
                content.to_string(),
            ))
        } else {
            Ok(AgentToolOutput {
                content,
                content_kind,
            })
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
    ) -> Result<AgentToolOutput, AgentToolError> {
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
            Ok(AgentToolOutput {
                content: output.structured_content,
                content_kind: ToolResultContentKind::Json,
            })
        }
    }
}
