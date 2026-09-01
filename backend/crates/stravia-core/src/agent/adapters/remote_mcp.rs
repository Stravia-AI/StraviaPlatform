use super::*;

#[derive(Clone)]
pub struct RemoteMcpToolSource {
    pub namespace: String,
    pub endpoint: String,
    pub bearer_token: Option<String>,
    pub version: u32,
}

pub async fn discover_remote_mcp_tools(
    source: RemoteMcpToolSource,
) -> Result<Vec<Arc<dyn AgentTool>>, AgentToolError> {
    if source.namespace.trim().is_empty()
        || source.endpoint.trim().is_empty()
        || source.version == 0
    {
        return Err(AgentToolError::new(
            "invalid_mcp_source",
            "MCP namespace, endpoint, and version must be valid",
        ));
    }
    let mut client = connect_remote_mcp(&source).await?;
    let remote_tools = client
        .peer()
        .list_all_tools()
        .await
        .map_err(|error| AgentToolError::new("mcp_discovery_failed", error.to_string()))?;
    let _ = client.close().await;
    Ok(remote_tools
        .into_iter()
        .map(|tool| {
            Arc::new(RemoteMcpAgentTool {
                id: VersionedToolId {
                    id: format!("{}.{}", source.namespace, tool.name),
                    version: source.version,
                },
                remote_name: tool.name.into_owned(),
                description: tool
                    .description
                    .map(|description| description.into_owned())
                    .unwrap_or_else(|| "Remote MCP Tool".into()),
                input_schema: Value::Object(tool.input_schema.as_ref().clone()),
                source: source.clone(),
            }) as Arc<dyn AgentTool>
        })
        .collect())
}

struct RemoteMcpAgentTool {
    id: VersionedToolId,
    remote_name: String,
    description: String,
    input_schema: Value,
    source: RemoteMcpToolSource,
}

#[async_trait]
impl AgentTool for RemoteMcpAgentTool {
    fn id(&self) -> VersionedToolId {
        self.id.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(
        &self,
        context: AgentToolContext,
        input: Value,
    ) -> Result<Value, AgentToolError> {
        let arguments = input.as_object().cloned().ok_or_else(|| {
            AgentToolError::new(
                "invalid_mcp_arguments",
                "MCP Tool arguments must be an object",
            )
        })?;
        let operation = async {
            let mut client = connect_remote_mcp(&self.source).await?;
            let result = client
                .call_tool(
                    CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments),
                )
                .await
                .map_err(|error| AgentToolError::new("mcp_call_failed", error.to_string()))?;
            let _ = client.close().await;
            let output = result
                .structured_content
                .unwrap_or_else(|| serde_json::to_value(result.content).unwrap_or(Value::Null));
            if result.is_error.unwrap_or(false) {
                Err(AgentToolError::new("mcp_tool_error", output.to_string()))
            } else {
                Ok(output)
            }
        };
        tokio::select! {
            _ = context.cancellation.cancelled() => {
                Err(AgentToolError::new("cancelled", "Remote MCP Tool cancelled"))
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(context.deadline)) => {
                context.cancellation.cancel();
                Err(AgentToolError::new("deadline_exceeded", "Remote MCP Tool deadline exceeded"))
            }
            result = operation => result,
        }
    }
}

async fn connect_remote_mcp(
    source: &RemoteMcpToolSource,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, AgentToolError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(source.endpoint.clone());
    if let Some(token) = source.bearer_token.as_ref() {
        config = config.auth_header(token.clone());
    }
    ().serve(StreamableHttpClientTransport::from_config(config))
        .await
        .map_err(|error| AgentToolError::new("mcp_connect_failed", error.to_string()))
}
