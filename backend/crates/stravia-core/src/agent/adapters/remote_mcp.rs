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
    ) -> Result<AgentToolOutput, AgentToolError> {
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
            let (output, content_kind) = match result.structured_content {
                Some(content) => (content, ToolResultContentKind::Json),
                None => (
                    serde_json::to_value(result.content).map_err(|error| {
                        AgentToolError::new("mcp_output_serialization_failed", error.to_string())
                    })?,
                    ToolResultContentKind::ContentBlocks,
                ),
            };
            if result.is_error.unwrap_or(false) {
                Err(AgentToolError::new("mcp_tool_error", output.to_string()))
            } else {
                Ok(AgentToolOutput {
                    content: output,
                    content_kind,
                })
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use stravia_runtime_contract::Principal;
    use stravia_runtime_contract::protocol::ir::AiItem;
    use stravia_runtime_contract::protocol::ir::AiRequest;
    use stravia_runtime_contract::protocol::ir::ContentBlock;
    use stravia_runtime_contract::protocol::ir::MessageContent;
    use stravia_runtime_contract::protocol::ir::Role;

    async fn mcp_reply(
        axum::extract::State(output): axum::extract::State<Value>,
        axum::Json(request): axum::Json<Value>,
    ) -> axum::response::Response {
        let result = match request["method"].as_str().unwrap() {
            "initialize" => serde_json::json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "local-redaction-fixture", "version": "1"},
            }),
            "notifications/initialized" => return axum::http::StatusCode::ACCEPTED.into_response(),
            "tools/list" => serde_json::json!({
                "tools": [{"name": "payload", "inputSchema": {"type": "object"}}]
            }),
            "tools/call" => output,
            method => panic!("unexpected MCP method: {method}"),
        };
        axum::Json(serde_json::json!({
            "jsonrpc": "2.0", "id": request["id"], "result": result
        }))
        .into_response()
    }

    #[tokio::test]
    async fn remote_mcp_content_preserves_media_and_protects_readable_resources() {
        const SECRET: &str = "Q8n4Vk7sT2p9X5a3Lc6D0h1R";
        let directory = tempfile::tempdir().unwrap();
        let gateway = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
        let owner = Principal::new("owner");
        gateway
            .admin()
            .set_setting(stravia_credential_protection::SETTING_KEY, "true")
            .await
            .unwrap();
        let mappings = gateway
            .redaction
            .mappings
            .intern(&owner, &[SECRET.into()])
            .await
            .unwrap()
            .mappings;
        for structured in [false, true] {
            let mut output = serde_json::json!({"content": [
                {"type": "text", "text": SECRET},
                {"type": "image", "data": SECRET, "mimeType": "image/png"},
                {"type": "audio", "data": SECRET, "mimeType": "audio/wav"},
                {"type": "resource", "resource": {"uri": "memory://text", "text": SECRET}},
                {"type": "resource", "resource": {"uri": "memory://blob", "blob": SECRET}},
            ]});
            if structured {
                output["structuredContent"] = serde_json::json!({
                    "type": "image", "data": SECRET,
                    "resource": {"text": SECRET, "blob": SECRET},
                });
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            let router = axum::Router::new()
                .route("/", axum::routing::post(mcp_reply))
                .with_state(output);
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let tools = discover_remote_mcp_tools(RemoteMcpToolSource {
                namespace: "fixture".into(),
                endpoint,
                bearer_token: None,
                version: 1,
            })
            .await
            .unwrap();
            let output = tools[0]
                .execute(
                    AgentToolContext {
                        principal: owner.clone(),
                        turn_id: AgentTurnId::agent(),
                        cancellation: stravia_runtime_contract::CancellationToken::new(),
                        deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
                    },
                    serde_json::json!({}),
                )
                .await
                .unwrap();
            let mut request = AiRequest::new(
                "model",
                vec![AiItem {
                    role: Role::Tool,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "call-remote".into(),
                        content: output.content,
                        content_kind: Some(output.content_kind),
                        is_error: Some(false),
                        cache_control: None,
                    }]),
                    tool_calls: None,
                    tool_call_id: Some("call-remote".into()),
                    meta: None,
                }],
            );
            gateway
                .redaction
                .protect(&owner, &mut request, None)
                .await
                .unwrap();
            let MessageContent::Blocks(blocks) = &request.items[0].content else {
                unreachable!()
            };
            let ContentBlock::ToolResult { content, .. } = &blocks[0] else {
                unreachable!()
            };
            let reference = &mappings[0].reference;
            if structured {
                assert_eq!(content["data"], reference.as_str());
                assert_eq!(content["resource"]["text"], reference.as_str());
                assert_eq!(content["resource"]["blob"], reference.as_str());
            } else {
                assert_eq!(content[0]["text"], reference.as_str());
                assert_eq!(content[1]["data"], SECRET);
                assert_eq!(content[2]["data"], SECRET);
                assert_eq!(content[3]["resource"]["text"], reference.as_str());
                assert_eq!(content[4]["resource"]["blob"], SECRET);
            }
            server.abort();
        }
    }
}
