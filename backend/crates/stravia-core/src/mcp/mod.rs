use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::FuturesUnordered};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

mod server;

pub(crate) use server::router;

const TOOL_DEADLINE: Duration = Duration::from_secs(60);
pub(super) const SUBSCRIPTION_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct McpContext {
    pub api_key_id: String,
    execution: Option<McpExecutionContext>,
}

#[derive(Clone)]
struct McpExecutionContext {
    cancellation: crate::proxy::context::CancellationToken,
    deadline: std::time::Instant,
}

impl McpContext {
    pub(crate) fn new(api_key_id: String) -> Self {
        Self {
            api_key_id,
            execution: None,
        }
    }

    fn for_call(
        &self,
        cancellation: crate::proxy::context::CancellationToken,
        deadline: std::time::Instant,
    ) -> Self {
        Self {
            api_key_id: self.api_key_id.clone(),
            execution: Some(McpExecutionContext {
                cancellation,
                deadline,
            }),
        }
    }

    pub(crate) fn execution(
        &self,
    ) -> Option<(crate::proxy::context::CancellationToken, std::time::Instant)> {
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

    async fn available(&self, context: &McpContext) -> Vec<Arc<dyn McpTool>> {
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

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| McpToolError::new("tool_not_found", format!("unknown tool: {name}")))?;
        let tool_deadline = tool.deadline();
        let deadline = tokio::time::Instant::now() + tool_deadline;
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
        let cancellation = crate::proxy::context::CancellationToken::new();
        let call_context = context.for_call(cancellation.clone(), deadline.into_std());
        let mut call = Box::pin(tool.call(arguments, &call_context));
        tokio::select! {
            biased;
            result = &mut call => result,
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

#[cfg(test)]
mod tests;
