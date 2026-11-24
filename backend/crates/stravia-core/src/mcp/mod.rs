use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::FuturesUnordered};
use rmcp::model::Tool as RmTool;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

pub(crate) mod read;
mod server;

pub(crate) use server::router;

const TOOL_DEADLINE: Duration = Duration::from_secs(60);
pub(super) const SUBSCRIPTION_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct McpContext {
    pub api_key_id: String,
    execution: Option<McpExecutionContext>,
    request_cancellation: Option<stravia_runtime_contract::CancellationToken>,
}

#[derive(Clone)]
struct McpExecutionContext {
    cancellation: stravia_runtime_contract::CancellationToken,
    deadline: std::time::Instant,
}

impl McpContext {
    pub fn new(api_key_id: String) -> Self {
        Self {
            api_key_id,
            execution: None,
            request_cancellation: None,
        }
    }

    pub fn request_cancellation(&self) -> Option<stravia_runtime_contract::CancellationToken> {
        self.request_cancellation.clone()
    }

    fn for_call(
        &self,
        cancellation: stravia_runtime_contract::CancellationToken,
        deadline: std::time::Instant,
    ) -> Self {
        Self {
            api_key_id: self.api_key_id.clone(),
            execution: Some(McpExecutionContext {
                cancellation,
                deadline,
            }),
            request_cancellation: None,
        }
    }

    pub(crate) fn execution(
        &self,
    ) -> Option<(
        stravia_runtime_contract::CancellationToken,
        std::time::Instant,
    )> {
        self.execution
            .as_ref()
            .map(|execution| (execution.cancellation.clone(), execution.deadline))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct McpToolError {
    pub code: &'static str,
    pub message: String,
}

impl McpToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpToolOutput {
    pub structured_content: Value,
    pub is_error: bool,
    pub content: Vec<rmcp::model::ContentBlock>,
}

impl McpToolOutput {
    pub fn success(structured_content: Value) -> Self {
        Self {
            structured_content,
            content: Vec::new(),
            is_error: false,
        }
    }

    pub fn execution_error(structured_content: Value) -> Self {
        Self {
            structured_content,
            is_error: true,
            content: Vec::new(),
        }
    }

    pub fn success_with_content(
        structured_content: Value,
        content: Vec<rmcp::model::ContentBlock>,
    ) -> Self {
        Self {
            structured_content,
            is_error: false,
            content,
        }
    }
}

#[async_trait]
pub trait McpTool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> Option<&str> {
        None
    }
    async fn description_for(&self, _context: &McpContext) -> Option<String> {
        self.description().map(str::to_owned)
    }
    fn input_schema(&self) -> Value;
    async fn input_schema_for(&self, _context: &McpContext) -> Value {
        self.input_schema()
    }
    fn deadline(&self) -> Duration {
        TOOL_DEADLINE
    }
    fn output_schema(&self) -> Option<Value> {
        None
    }
    fn await_cancellation_cleanup(&self) -> bool {
        false
    }
    /// Whether the underlying platform capability backing this built-in MCP tool is currently
    /// configured and enabled at the platform level (independent of any single API key's scope).
    async fn platform_capability_available(&self) -> bool {
        true
    }
    async fn available(&self, context: &McpContext) -> Result<bool, McpToolError>;
    async fn call(
        &self,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError>;
}

#[derive(Clone, Default)]
pub struct McpToolRegistry {
    tools: Arc<HashMap<String, Arc<dyn McpTool>>>,
}

impl McpToolRegistry {
    pub fn new(tools: Vec<Arc<dyn McpTool>>) -> anyhow::Result<Self> {
        let mut registered = HashMap::with_capacity(tools.len());
        for tool in tools {
            let raw_name = tool.name();
            let name = raw_name.trim();
            if name.is_empty() {
                anyhow::bail!("MCP tool name cannot be empty");
            }
            if name != raw_name {
                anyhow::bail!("MCP tool name cannot have surrounding whitespace: {raw_name}");
            }
            if !tool.input_schema().is_object() {
                anyhow::bail!("MCP tool input schema must be an object: {name}");
            }
            if tool
                .output_schema()
                .is_some_and(|schema| !schema.is_object())
            {
                anyhow::bail!("MCP tool output schema must be an object: {name}");
            }
            let name = name.to_owned();
            if registered.insert(name.clone(), tool).is_some() {
                anyhow::bail!("duplicate MCP tool name: {name}");
            }
        }
        Ok(Self {
            tools: Arc::new(registered),
        })
    }

    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn internal_tools(&self) -> Vec<Arc<dyn McpTool>> {
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort_unstable();
        names
            .into_iter()
            .map(|name| Arc::clone(&self.tools[name]))
            .collect()
    }

    pub async fn available(&self, context: &McpContext) -> Vec<Arc<dyn McpTool>> {
        let deadline = tokio::time::Instant::now() + TOOL_DEADLINE;
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort_unstable();

        let mut checks = FuturesUnordered::new();
        for (index, name) in names.into_iter().enumerate() {
            let tool = Arc::clone(&self.tools[name]);
            checks.push(async move {
                let available = tool.available(context).await;
                (index, tool, available)
            });
        }

        let mut available_tools = Vec::with_capacity(checks.len());
        while !checks.is_empty() {
            match tokio::time::timeout_at(deadline, checks.next()).await {
                Ok(Some((index, tool, Ok(true)))) => available_tools.push((index, tool)),
                Ok(Some((_, _, Ok(false)))) => {}
                Ok(Some((_, tool, Err(error)))) => tracing::warn!(
                    tool = tool.name(),
                    error_code = error.code,
                    "MCP tool availability could not be resolved"
                ),
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!("MCP tool listing exceeded the availability deadline");
                    break;
                }
            }
        }
        available_tools.sort_unstable_by_key(|(index, _)| *index);
        available_tools.into_iter().map(|(_, tool)| tool).collect()
    }

    pub async fn call(
        &self,
        name: &str,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        let accepted = tokio::time::Instant::now();
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| McpToolError::new("tool_not_found", format!("unknown tool: {name}")))?;
        let tool_deadline = tool.deadline();
        let deadline = tokio::time::Instant::now() + tool_deadline;
        let deadline = context
            .execution
            .as_ref()
            .map(|execution| deadline.min(execution.deadline.into()))
            .unwrap_or(deadline);
        let available = tokio::time::timeout_at(deadline, tool.available(context))
            .await
            .map_err(|_| {
                McpToolError::new(
                    "timeout",
                    format!(
                        "tool exceeded the {} second deadline",
                        tool_deadline.as_secs()
                    ),
                )
            })??;
        if !available {
            return Err(McpToolError::new(
                "tool_unavailable",
                format!("tool is not available: {name}"),
            ));
        }
        let cancellation = context
            .execution
            .as_ref()
            .map(|execution| execution.cancellation.child_token())
            .or_else(|| context.request_cancellation.clone())
            .unwrap_or_default();
        let call_context = context.for_call(cancellation.clone(), deadline.into_std());
        let mut call = Box::pin(crate::tool_execution::scope_execution(
            accepted,
            deadline.into_std(),
            tool.call(arguments, &call_context),
        ));
        tokio::select! {
            biased;
            result = &mut call => result,
            _ = cancellation.cancelled() => {
                if tool.await_cancellation_cleanup() {
                    return call.await;
                }
                Err(McpToolError::new("cancelled", "tool request cancelled"))
            }
            _ = tokio::time::sleep_until(deadline) => {
                cancellation.cancel();
                if tool.await_cancellation_cleanup()
                    && let Ok(output) = call.await
                    && !output.is_error
                {
                    return Ok(output);
                }
                Err(McpToolError::new(
                    "timeout",
                    format!(
                        "tool exceeded the {} second deadline",
                        tool_deadline.as_secs()
                    ),
                ))
            }
        }
    }
}

/// Tool metadata resolved for an authenticated MCP endpoint scope.
#[derive(Debug, Clone)]
pub struct ResolvedMcpTool {
    pub exposed_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

impl ResolvedMcpTool {
    pub fn to_rmcp_tool(&self) -> RmTool {
        let input_obj = Arc::new(self.input_schema.as_object().cloned().unwrap_or_default());
        let mut definition = RmTool::new_with_raw(
            self.exposed_name.clone(),
            self.description.clone().map(std::borrow::Cow::Owned),
            input_obj,
        );
        if let Some(output_schema) = self
            .output_schema
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
        {
            definition = definition.with_raw_output_schema(Arc::new(output_schema));
        }
        definition
    }
}

/// Endpoint-level access failure returned by [`McpCatalogProvider::authorize_endpoint`].
#[derive(Debug)]
pub enum McpEndpointAccessError {
    NotFound(String),
    Forbidden { code: &'static str, message: String },
    Internal(String),
}

/// Extension seam resolving `/mcp*` endpoint scopes, visible tools, and tool execution.
///
/// Community Edition installs [`BuiltinMcpCatalogProvider`], which exposes only built-in
/// platform MCP tools on `/mcp`. Commercial or custom hosts can inject an alternative
/// provider via [`crate::GatewayExtensionFactory`] to govern external MCP servers,
/// HTTP-to-MCP bridges, groups, and fine-grained scopes.
#[async_trait]
pub trait McpCatalogProvider: Send + Sync + 'static {
    /// Parses a request URI path into an opaque normalized scope key, or returns `None`
    /// when the path is not served by this provider (404).
    fn parse_scope(&self, path: &str) -> Option<String>;

    /// Authorizes an authenticated API key for the resolved endpoint scope.
    async fn authorize_endpoint(
        &self,
        gateway: &crate::Gateway,
        api_key_id: &str,
        scope: &str,
    ) -> Result<(), McpEndpointAccessError>;

    /// Lists all MCP tools visible to `context` within `scope`.
    async fn list_visible_tools(
        &self,
        gateway: &crate::Gateway,
        context: &McpContext,
        scope: &str,
    ) -> Result<Vec<ResolvedMcpTool>, McpToolError>;

    /// Executes a visible MCP tool within `scope`.
    async fn call_visible_tool(
        &self,
        gateway: &crate::Gateway,
        name: &str,
        arguments: Value,
        context: &McpContext,
        scope: &str,
    ) -> Result<McpToolOutput, McpToolError>;
}

/// Default Community Edition MCP catalog exposing only built-in tools on `/mcp`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinMcpCatalogProvider;

#[async_trait]
impl McpCatalogProvider for BuiltinMcpCatalogProvider {
    fn parse_scope(&self, path: &str) -> Option<String> {
        let trimmed = path.trim_end_matches('/');
        (trimmed == "/mcp").then(|| "unified".to_owned())
    }

    async fn authorize_endpoint(
        &self,
        _gateway: &crate::Gateway,
        _api_key_id: &str,
        _scope: &str,
    ) -> Result<(), McpEndpointAccessError> {
        Ok(())
    }

    async fn list_visible_tools(
        &self,
        gateway: &crate::Gateway,
        context: &McpContext,
        _scope: &str,
    ) -> Result<Vec<ResolvedMcpTool>, McpToolError> {
        let available = gateway.mcp_registry.available(context).await;
        let mut resolved = Vec::with_capacity(available.len());
        for tool in available {
            let input_schema = tool.input_schema_for(context).await;
            let description = tool.description_for(context).await;
            let output_schema = tool.output_schema();
            resolved.push(ResolvedMcpTool {
                exposed_name: tool.name().to_owned(),
                description,
                input_schema,
                output_schema,
            });
        }
        Ok(resolved)
    }

    async fn call_visible_tool(
        &self,
        gateway: &crate::Gateway,
        name: &str,
        arguments: Value,
        context: &McpContext,
        _scope: &str,
    ) -> Result<McpToolOutput, McpToolError> {
        gateway.mcp_registry.call(name, arguments, context).await
    }
}

#[cfg(test)]
mod tests;
