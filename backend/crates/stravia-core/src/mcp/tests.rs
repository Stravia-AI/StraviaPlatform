use super::*;
use crate::Gateway;
use base64::Engine;
use bytes::Bytes;
use rmcp::model::{CallToolRequestParams, ClientInfo, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct EchoTool;

#[async_trait]
impl McpTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> Option<&str> {
        Some("Echo an integer")
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "integer", "x-mcp-header": "Value" }
            },
            "required": ["value"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } },
            "required": ["value"],
            "additionalProperties": false
        }))
    }

    async fn available(&self, _context: &McpContext) -> Result<bool, McpToolError> {
        Ok(true)
    }

    async fn call(
        &self,
        arguments: Value,
        _context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        match arguments.get("value").and_then(Value::as_i64) {
            Some(-1) => Ok(McpToolOutput::execution_error(json!({
                "value": -1
            }))),
            Some(-2) => Err(McpToolError::new("echo_failed", "echo execution failed")),
            _ => Ok(McpToolOutput::success(arguments)),
        }
    }
}

struct BlockingMcpTool {
    first_call: AtomicBool,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl McpTool for BlockingMcpTool {
    fn name(&self) -> &str {
        "blocking"
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    async fn available(&self, _context: &McpContext) -> Result<bool, McpToolError> {
        Ok(true)
    }

    async fn call(
        &self,
        _arguments: Value,
        _context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        if self.first_call.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(McpToolOutput::success(json!({ "complete": true })))
    }
}

struct CooperativeCleanupTool {
    cleanups: Arc<AtomicUsize>,
}

#[async_trait]
impl McpTool for CooperativeCleanupTool {
    fn name(&self) -> &str {
        "cooperative_cleanup"
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    fn deadline(&self) -> Duration {
        Duration::from_millis(1)
    }

    fn await_cancellation_cleanup(&self) -> bool {
        true
    }

    async fn available(&self, _context: &McpContext) -> Result<bool, McpToolError> {
        Ok(true)
    }

    async fn call(
        &self,
        _arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        let (cancellation, _) = context.execution().expect("execution context");
        cancellation.cancelled().await;
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        Ok(McpToolOutput::execution_error(json!({
            "error": "cancelled"
        })))
    }
}

#[tokio::test]
async fn cooperative_tool_cleanup_completes_before_timeout_returns() {
    let cleanups = Arc::new(AtomicUsize::new(0));
    let registry = McpToolRegistry::new(vec![Arc::new(CooperativeCleanupTool {
        cleanups: Arc::clone(&cleanups),
    })])
    .expect("registry");

    let error = registry
        .call(
            "cooperative_cleanup",
            json!({}),
            &McpContext::new("key".into()),
        )
        .await
        .expect_err("tool timeout");

    assert_eq!(error.code, "timeout");
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
}

struct InvalidSchemaTool {
    output: bool,
}

#[async_trait]
impl McpTool for InvalidSchemaTool {
    fn name(&self) -> &str {
        if self.output {
            "invalid_output"
        } else {
            "invalid_input"
        }
    }

    fn input_schema(&self) -> Value {
        if self.output {
            json!({ "type": "object" })
        } else {
            json!(true)
        }
    }

    fn output_schema(&self) -> Option<Value> {
        self.output.then_some(json!(false))
    }
    async fn available(&self, _context: &McpContext) -> Result<bool, McpToolError> {
        unreachable!("invalid schemas are rejected during registration")
    }

    async fn call(
        &self,
        _arguments: Value,
        _context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        unreachable!("invalid schemas are rejected during registration")
    }
}

struct TestApp {
    _data_dir: tempfile::TempDir,
    gateway: Gateway,
    key_id: String,
    token: String,
    endpoint: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

type SdkClient = RunningService<RoleClient, ClientInfo>;

async fn test_app() -> TestApp {
    test_app_with_tools(vec![Arc::new(EchoTool)]).await
}

async fn test_app_with_tools(mcp_tools: Vec<Arc<dyn McpTool>>) -> TestApp {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let builder = mcp_tools
        .into_iter()
        .fold(crate::Gateway::builder(config), |builder, tool| {
            builder.mcp_tool(tool)
        });
    let (gateway, _logs) = builder.build().await.expect("gateway");
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "MCP key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            inject_web_search: true,
            model_ids: vec![],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let app = crate::proxy::server::create_router(gateway.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("MCP test server");
    });

    TestApp {
        _data_dir: data_dir,
        gateway,
        key_id: key.id,
        token: key.token,
        endpoint: format!("http://{address}/mcp"),
        server,
    }
}

async fn set_concurrency_limit(app: &TestApp, limit: i32) {
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            crate::db::models::UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: Some(Some(limit)),
                is_enabled: None,
                mcp_access_enabled: None,
                transparent_injection_enabled: None,
                inject_web_search: None,
                expires_at: None,
                model_ids: None,
                inject_media_understanding: None,
            },
        )
        .await
        .expect("set concurrency limit");
}

async fn serve_media_report(source_id: crate::agent::ArtifactId) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("Media provider listener");
    let address = listener.local_addr().expect("Media provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("Media provider request");
        let mut request = vec![0_u8; 32 * 1024];
        let read = socket.read(&mut request).await.expect("read Media request");
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(
            String::from_utf8_lossy(&request[..read]).contains("data:image/jpeg;base64"),
            "Media Model must receive the JPEG derivative"
        );
        let report = json!({
            "answer": format!("Direct MCP understood the image [artifact:{}]", source_id.as_str()),
            "artifacts": [{"artifact_id": source_id}],
            "limitations": []
        })
        .to_string();
        let body = json!({
            "id": "chatcmpl-media",
            "object": "chat.completion",
            "created": 1,
            "model": "vision",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": report},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write Media response");
    });
    (format!("http://{address}/v1"), calls)
}

async fn media_test_app() -> (TestApp, crate::agent::ArtifactId, Arc<AtomicUsize>) {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, _logs) = crate::Gateway::new(config).await.expect("Gateway");
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Media MCP key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let principal = crate::hook::Principal::new(key.id.clone());
    let source = gateway
            .media_derivatives
            .as_ref()
            .expect("Media store")
            .create_source(
                &principal,
                "image/png",
                Bytes::from(
                    base64::engine::general_purpose::STANDARD
                        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
                        .expect("PNG fixture"),
                ),
                Duration::from_secs(60 * 60),
            )
            .await
            .expect("source Artifact");
    let (provider_url, calls) = serve_media_report(source.id.clone()).await;
    let provider = gateway
        .admin()
        .create_provider(crate::db::models::CreateProvider {
            name: Some("MCP Vision".into()),
            source: crate::db::models::ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: provider_url,
                models_source: None,
                static_models: None,
            },
            credential: crate::db::models::ProviderCredentialInput::ApiKey {
                value: "test-key".into(),
            },
            use_proxy: false,
        })
        .await
        .expect("Media Provider");
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "vision",
            crate::provider_models::CreateManualProviderModel {
                metadata: json!({
                    "id": "vision",
                    "modalities": {"input": ["text", "image"], "output": ["text"]}
                }),
            },
        )
        .await
        .expect("Media Provider Model");
    let model = gateway
        .admin()
        .create_model(crate::db::models::CreateModel {
            name: "mcp-vision".into(),
            balance: None,
            target_provider: provider.id,
            target_model: "vision".into(),
            targets: vec![],
        })
        .await
        .expect("Media Model");
    gateway
        .admin()
        .update_media_understanding_config(crate::admin::MediaUnderstandingConfigUpdate {
            enabled: true,
            model_id: Some(model.id),
        })
        .await
        .expect("enable Media Understanding");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("MCP listener");
    let address = listener.local_addr().expect("MCP address");
    let app = crate::proxy::server::create_router(gateway.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("MCP test server");
    });
    (
        TestApp {
            _data_dir: data_dir,
            gateway,
            key_id: key.id,
            token: key.token,
            endpoint: format!("http://{address}/mcp"),
            server,
        },
        source.id,
        calls,
    )
}
async fn connect(app: &TestApp) -> SdkClient {
    let config = StreamableHttpClientTransportConfig::with_uri(app.endpoint.clone())
        .auth_header(app.token.clone());
    let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
    ClientInfo::default()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("RMCP client connection")
}

fn arguments(value: i64) -> serde_json::Map<String, Value> {
    json!({ "value": value })
        .as_object()
        .expect("arguments object")
        .clone()
}

fn assert_server_metadata(meta: Option<&rmcp::model::MetaObject>) {
    let server = meta
        .and_then(|meta| meta.get("io.modelcontextprotocol/serverInfo"))
        .expect("server metadata");
    assert_eq!(server["name"], "stravia");
    assert_eq!(server["version"], env!("CARGO_PKG_VERSION"));
}

async fn assert_auth_error(
    request: reqwest::RequestBuilder,
    status: reqwest::StatusCode,
    code: &str,
    message: &str,
) {
    let response = request
        .json(&json!({}))
        .send()
        .await
        .expect("MCP auth request");
    assert_eq!(response.status(), status);
    let body: Value = response.json().await.expect("MCP auth error");
    assert_eq!(body["error"]["code"], code);
    assert_eq!(body["error"]["message"], message);
}

#[test]
fn registry_rejects_non_object_schemas() {
    for output in [false, true] {
        let error = McpToolRegistry::new(vec![Arc::new(InvalidSchemaTool { output })])
            .err()
            .expect("invalid schema rejection");
        assert!(error.to_string().contains("schema must be an object"));
    }
}

#[tokio::test]
async fn official_client_discovers_lists_and_calls_tools() {
    let app = test_app().await;
    let http = reqwest::Client::new();
    let unauthorized = http
        .post(&app.endpoint)
        .json(&json!({}))
        .send()
        .await
        .expect("unauthorized request");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let invalid_origin = http
        .post(&app.endpoint)
        .bearer_auth(&app.token)
        .header("origin", "https://attacker.example")
        .json(&json!({}))
        .send()
        .await
        .expect("invalid-origin request");
    assert_eq!(invalid_origin.status(), reqwest::StatusCode::FORBIDDEN);

    let client = connect(&app).await;
    let peer = client.peer_info().expect("discovered server information");
    assert_eq!(peer.protocol_version, ProtocolVersion::V_2026_07_28);
    assert_eq!(
        peer.server_info.as_ref().map(|info| info.name.as_str()),
        Some("stravia")
    );
    assert!(peer.capabilities.tools.is_some());

    let listed = client.list_tools(None).await.expect("tools/list");
    assert_server_metadata(listed.meta.as_ref());
    assert_eq!(listed.cache_scope, Some(rmcp::model::CacheScope::Private));
    assert_eq!(listed.ttl_ms, Some(0));
    assert_eq!(listed.tools.len(), 1);
    let tool = &listed.tools[0];
    assert_eq!(tool.name, "echo");
    assert_eq!(tool.description.as_deref(), Some("Echo an integer"));
    let encoded = serde_json::to_value(tool).expect("tool schema");
    assert_eq!(
        encoded["inputSchema"]["properties"]["value"]["x-mcp-header"],
        "Value"
    );
    assert!(encoded["outputSchema"].is_object());

    let success = client
        .call_tool(CallToolRequestParams::new("echo").with_arguments(arguments(42)))
        .await
        .expect("successful tools/call");
    assert_eq!(success.structured_content, Some(json!({ "value": 42 })));
    assert_ne!(success.is_error, Some(true));
    assert_server_metadata(success.meta.as_ref());

    let execution_error = client
        .call_tool(CallToolRequestParams::new("echo").with_arguments(arguments(-1)))
        .await
        .expect("structured execution error");
    assert_eq!(execution_error.is_error, Some(true));
    assert_eq!(
        execution_error.structured_content,
        Some(json!({ "value": -1 }))
    );
    assert_server_metadata(execution_error.meta.as_ref());

    let provider_error = client
        .call_tool(CallToolRequestParams::new("echo").with_arguments(arguments(-2)))
        .await
        .expect("provider execution error");
    assert_eq!(provider_error.is_error, Some(true));
    assert_eq!(
        provider_error
            .meta
            .as_ref()
            .and_then(|meta| meta.get("stravia/errorCode"))
            .and_then(Value::as_str),
        Some("echo_failed")
    );
    assert_server_metadata(provider_error.meta.as_ref());
}

#[tokio::test]
async fn mcp_tool_call_and_proxy_run_share_principal_concurrency_limit() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let app = test_app_with_tools(vec![Arc::new(BlockingMcpTool {
        first_call: AtomicBool::new(true),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    })])
    .await;
    set_concurrency_limit(&app, 1).await;

    let client = connect(&app).await;
    let first_call = tokio::spawn(async move {
        client
            .call_tool(CallToolRequestParams::new("blocking"))
            .await
            .expect("first tools/call")
    });
    entered.notified().await;

    let http = reqwest::Client::new();
    let rejected_mcp = http
        .post(&app.endpoint)
        .bearer_auth(&app.token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "blocking", "arguments": {} }
        }))
        .send()
        .await
        .expect("rejected MCP tools/call");
    assert_eq!(
        rejected_mcp.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS
    );
    assert!(
        rejected_mcp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .is_none()
    );
    let body: Value = rejected_mcp.json().await.expect("MCP limit response");
    assert_eq!(body["error"]["type"], "STRAVIA_CONCURRENCY_LIMIT");
    assert_eq!(
        body["error"]["message"],
        "Principal Concurrency Limit is full."
    );

    let proxy_endpoint = app.endpoint.strip_suffix("/mcp").expect("MCP endpoint");
    let rejected_proxy = http
        .post(format!("{proxy_endpoint}/v1/chat/completions"))
        .bearer_auth(&app.token)
        .json(&json!({ "model": "unconfigured", "messages": [] }))
        .send()
        .await
        .expect("rejected Proxy Inference Run");
    assert_eq!(
        rejected_proxy.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS
    );
    assert!(
        rejected_proxy
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .is_none()
    );
    let body: Value = rejected_proxy.json().await.expect("Proxy limit response");
    assert_eq!(body["error"]["type"], "STRAVIA_CONCURRENCY_LIMIT");

    release.notify_one();
    let completed = first_call.await.expect("first tools/call task");
    assert_eq!(
        completed.structured_content,
        Some(json!({ "complete": true }))
    );

    let client = connect(&app).await;
    let resumed = client
        .call_tool(CallToolRequestParams::new("blocking"))
        .await
        .expect("tools/call after lease release");
    assert_eq!(
        resumed.structured_content,
        Some(json!({ "complete": true }))
    );
}

#[tokio::test]
async fn codex_protocol_client_initializes_and_lists_tools() {
    let app = test_app().await;
    let config = StreamableHttpClientTransportConfig::with_uri(app.endpoint.clone())
        .auth_header(app.token.clone());
    let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
    let client = ClientInfo::default()
        .with_protocol_version(ProtocolVersion::V_2025_06_18)
        .serve_with_lifecycle(transport, ClientLifecycleMode::Initialize)
        .await
        .expect("Codex-compatible MCP client connection");

    let peer = client.peer_info().expect("negotiated server information");
    assert_eq!(peer.protocol_version, ProtocolVersion::V_2025_06_18);
    assert_eq!(
        client
            .list_tools(None)
            .await
            .expect("tools/list")
            .tools
            .len(),
        1
    );
}

#[tokio::test]
async fn official_client_calls_media_with_a_principal_owned_artifact() {
    let (app, source_id, media_calls) = media_test_app().await;
    let client = connect(&app).await;
    let listed = client.list_tools(None).await.expect("tools/list");
    assert!(
        listed
            .tools
            .iter()
            .any(|tool| tool.name == "understand_media")
    );
    let arguments = json!({
        "prompt": "Describe the image",
        "artifacts": [{"artifact_id": source_id}]
    })
    .as_object()
    .expect("Media arguments")
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("understand_media").with_arguments(arguments))
        .await
        .expect("Media tools/call");

    assert_ne!(
        result.is_error,
        Some(true),
        "{:?}",
        result.structured_content
    );
    let structured = result.structured_content.expect("structured Media result");
    assert_eq!(
        structured["report"]["artifacts"][0]["artifact_id"],
        source_id.as_str()
    );
    assert!(
        structured["report"]["answer"]
            .as_str()
            .is_some_and(|answer| answer.contains("Direct MCP understood"))
    );
    assert_eq!(media_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn media_platform_gate_removes_the_tool_from_mcp_discovery() {
    let (app, _, media_calls) = media_test_app().await;
    let current = app
        .gateway
        .admin()
        .get_media_understanding_config()
        .await
        .expect("Media config");
    app.gateway
        .admin()
        .update_media_understanding_config(crate::admin::MediaUnderstandingConfigUpdate {
            enabled: false,
            model_id: current.model_id,
        })
        .await
        .expect("disable Media Understanding");

    let client = connect(&app).await;
    let listed = client.list_tools(None).await.expect("tools/list");
    assert!(
        listed
            .tools
            .iter()
            .all(|tool| tool.name != "understand_media")
    );
    assert_eq!(media_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn media_tool_requires_mcp_access_independently_from_transparent_injection() {
    let (app, _, media_calls) = media_test_app().await;
    let client = connect(&app).await;
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            crate::db::models::UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: None,
                is_enabled: None,
                mcp_access_enabled: Some(false),
                transparent_injection_enabled: None,
                inject_web_search: None,
                expires_at: None,
                model_ids: None,
                inject_media_understanding: None,
            },
        )
        .await
        .expect("disable MCP access");

    let listed = client.list_tools(None).await.expect("tools/list");
    assert!(
        listed
            .tools
            .iter()
            .all(|tool| tool.name != "understand_media")
    );
    assert_eq!(media_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn web_search_requires_mcp_access_independently_from_transparent_injection() {
    let app = test_app().await;
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            crate::db::models::UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: None,
                is_enabled: None,
                mcp_access_enabled: Some(true),
                transparent_injection_enabled: Some(false),
                inject_web_search: Some(false),
                expires_at: None,
                model_ids: None,
                inject_media_understanding: None,
            },
        )
        .await
        .expect("enable MCP access");
    crate::web_search::SettingsWebSearchConfigStore::new(app.gateway.storage.clone())
        .save(&crate::web_search::WebSearchConfig {
            revision: 1,
            enabled: true,
            backend: None,
            max_turns: 12,
            total_time_seconds: 600,
            updated_at: "2026-08-17T00:00:00Z".into(),
        })
        .await
        .expect("enable Web Search gate");

    let client = connect(&app).await;
    let listed = client.list_tools(None).await.expect("tools/list");
    assert!(listed.tools.iter().any(|tool| tool.name == "web_search"));

    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            crate::db::models::UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: None,
                is_enabled: None,
                mcp_access_enabled: Some(false),
                transparent_injection_enabled: None,
                inject_web_search: None,
                expires_at: None,
                model_ids: None,
                inject_media_understanding: None,
            },
        )
        .await
        .expect("disable MCP access");

    let listed = client.list_tools(None).await.expect("tools/list");
    assert!(listed.tools.iter().all(|tool| tool.name != "web_search"));
    let mut arguments = serde_json::Map::new();
    arguments.insert("query".into(), json!("Search the claim"));
    let unavailable = client
        .call_tool(CallToolRequestParams::new("web_search").with_arguments(arguments))
        .await
        .expect("structured unavailable result");
    assert_eq!(unavailable.is_error, Some(true));
    assert_eq!(
        unavailable
            .meta
            .as_ref()
            .and_then(|meta| meta.get("stravia/errorCode"))
            .and_then(Value::as_str),
        Some("tool_unavailable")
    );
}

#[tokio::test]
async fn mcp_transport_preserves_bearer_only_authentication_mappings() {
    let app = test_app().await;
    let http = reqwest::Client::new();

    assert_auth_error(
        http.post(&app.endpoint),
        reqwest::StatusCode::UNAUTHORIZED,
        "invalid_api_key",
        "Bearer API key is required",
    )
    .await;
    assert_auth_error(
        http.post(&app.endpoint).bearer_auth("unknown-key"),
        reqwest::StatusCode::UNAUTHORIZED,
        "invalid_api_key",
        "invalid API key",
    )
    .await;
    for header_name in ["x-api-key", "x-goog-api-key"] {
        assert_auth_error(
            http.post(&app.endpoint)
                .header(header_name, app.token.as_str()),
            reqwest::StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "Bearer API key is required",
        )
        .await;
    }

    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            crate::db::models::UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: None,
                is_enabled: Some(false),
                mcp_access_enabled: None,
                transparent_injection_enabled: None,
                inject_web_search: None,
                expires_at: None,
                model_ids: None,
                inject_media_understanding: None,
            },
        )
        .await
        .expect("disable API key");
    assert_auth_error(
        http.post(&app.endpoint).bearer_auth(&app.token),
        reqwest::StatusCode::FORBIDDEN,
        "api_key_disabled",
        "API key is disabled",
    )
    .await;
}

#[tokio::test]
async fn mcp_transport_maps_missing_auth_store_to_unavailable() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let storage = Arc::new(crate::storage::MemoryStorage::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ));
    let (gateway, _logs) = crate::Gateway::builder(config)
        .storage(storage)
        .mcp_tool(Arc::new(EchoTool))
        .build()
        .await
        .expect("gateway");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let app = crate::proxy::server::create_router(gateway);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("MCP test server");
    });

    assert_auth_error(
        reqwest::Client::new()
            .post(format!("http://{address}/mcp"))
            .bearer_auth("unavailable-key"),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "auth_unavailable",
        "API key authentication is unavailable",
    )
    .await;
    server.abort();
}

#[tokio::test]
async fn expired_mcp_credential_uses_canonical_unauthorized_status() {
    let app = test_app().await;
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            crate::db::models::UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: None,
                is_enabled: None,
                mcp_access_enabled: None,
                transparent_injection_enabled: None,
                inject_web_search: None,
                expires_at: Some("2000-01-01T00:00:00Z".into()),
                model_ids: None,
                inject_media_understanding: None,
            },
        )
        .await
        .expect("expire API key");

    let response = reqwest::Client::new()
        .post(&app.endpoint)
        .bearer_auth(&app.token)
        .json(&json!({}))
        .send()
        .await
        .expect("expired MCP request");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer")
    );
    let body: Value = response.json().await.expect("MCP auth error");
    assert_eq!(body["error"]["code"], "api_key_expired");
    assert_eq!(body["error"]["message"], "API key is expired");
}
