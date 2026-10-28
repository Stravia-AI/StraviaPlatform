use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use futures::StreamExt as _;
use rmcp::model::{CallToolRequestParams, InitializeRequestParams, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};
use serde_json::{Value, json};
use stravia_core::Gateway;
use stravia_core::config::GatewayConfig;
use stravia_core::data_paths::DataPaths;
use stravia_core::db::models::{
    CreateApiKey, CreateProvider, CreateProviderRecord, CreateRoute, CreateTarget,
    ProviderCredentialInput, ProviderSourceInput, UpsertOAuthCredential,
};
use stravia_core::media_generation::{ImageGenerationConfig, MediaGenerationConfig};
use stravia_core::plugin::{ConfirmPluginUpdate, PluginSource};
use stravia_core::provider_models::CreateManualProviderModel;

use stravia_runtime_contract::artifact::ArtifactId;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::{CancellationToken, Principal};
use stravia_vendor_sdk::Capability;
use stravia_web_search::{
    SearchTurnId, WebSearchBackendDraft, WebSearchConfig, WebSearchEvent, WebSearchInput,
    WebSearchRunPolicy,
};
use tokio::sync::{Mutex, oneshot};
use tower::ServiceExt;

mod vendor_plugin_artifacts;
use vendor_plugin_artifacts::install_distributed_vendor_plugin;

const SOURCE_URL: &str = "https://93.184.216.34/verified";
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

type McpClient = RunningService<RoleClient, InitializeRequestParams>;

struct TestHarness {
    _data_dir: tempfile::TempDir,
    gateway: Gateway,
    key_id: String,
    token: String,
    endpoint: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl TestHarness {
    async fn new() -> anyhow::Result<Self> {
        let data_dir = tempfile::tempdir()?;
        let gateway = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        })
        .await?;
        let key = gateway
            .admin()
            .create_api_key(CreateApiKey {
                key: None,
                name: "Capability contract owner".into(),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: true,
                transparent_injection_enabled: false,
                inject_web_search: false,
                inject_media_generation: false,
                model_ids: Vec::new(),
                inject_media_understanding: false,
                spend_limit: None,
            })
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = stravia_core::proxy::server::create_router(gateway.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve capability contract gateway");
        });
        let endpoint = format!("http://{address}/mcp");
        gateway
            .admin()
            .set_setting(
                "artifact_settings",
                &json!({"client_base_url": endpoint.trim_end_matches("/mcp")}).to_string(),
            )
            .await?;
        Ok(Self {
            _data_dir: data_dir,
            gateway,
            key_id: key.id,
            token: key.token,
            endpoint,
            server,
        })
    }

    async fn create_key(&self, name: &str) -> anyhow::Result<(String, String)> {
        let key = self
            .gateway
            .admin()
            .create_api_key(CreateApiKey {
                key: None,
                name: name.into(),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: true,
                transparent_injection_enabled: false,
                inject_web_search: false,
                inject_media_generation: false,
                model_ids: Vec::new(),
                inject_media_understanding: false,
                spend_limit: None,
            })
            .await?;
        Ok((key.id, key.token))
    }

    async fn connect(&self) -> McpClient {
        connect_mcp(&self.endpoint, &self.token).await
    }
}

/// Provider scopes arrive through the base plugin's `sync-catalog` export and
/// land in the on-disk cache; tests seed the bootstrap revision directly so
/// scope reads stay offline.
fn seed_provider_scope(
    data_dir: &std::path::Path,
    provider_id: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let path = stravia_core::data_paths::DataPaths::new(data_dir)
        .catalog_root()
        .join("catalog/scopes/bootstrap")
        .join(format!("{provider_id}.json"));
    std::fs::create_dir_all(path.parent().expect("scope path has a parent"))?;
    std::fs::write(path, body)?;
    Ok(())
}

struct LocalUpstream {
    url: String,
    state: Arc<UpstreamState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LocalUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    body: Value,
    headers: HeaderMap,
    state_before: Option<String>,
}

struct UpstreamState {
    replies: Mutex<HashMap<String, VecDeque<UpstreamReply>>>,
    requests: Mutex<Vec<CapturedRequest>>,
}

enum UpstreamReply {
    Json(StatusCode, Value),
    ResponsesStream(Value),
    Barrier {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
        responded: oneshot::Sender<()>,
        status: StatusCode,
        body: Value,
        retry_after_seconds: Option<u64>,
    },
}

impl LocalUpstream {
    async fn start() -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(UpstreamState {
            replies: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/{*path}", post(upstream_handler))
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve controlled capability upstream");
        });
        Ok(Self {
            url: format!("http://{address}"),
            state,
            task,
        })
    }

    async fn push_json(&self, path: &str, status: StatusCode, body: Value) {
        self.state
            .replies
            .lock()
            .await
            .entry(path.into())
            .or_default()
            .push_back(UpstreamReply::Json(status, body));
    }

    async fn push_responses_stream(&self, body: Value) {
        self.state
            .replies
            .lock()
            .await
            .entry("/responses".into())
            .or_default()
            .push_back(UpstreamReply::ResponsesStream(body));
    }

    async fn push_barrier(
        &self,
        path: &str,
        status: StatusCode,
        body: Value,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (responded_tx, responded_rx) = oneshot::channel();
        self.state
            .replies
            .lock()
            .await
            .entry(path.into())
            .or_default()
            .push_back(UpstreamReply::Barrier {
                entered: entered_tx,
                release: release_rx,
                responded: responded_tx,
                status,
                body,
                retry_after_seconds: None,
            });
        (entered_rx, release_tx, responded_rx)
    }

    async fn push_retry_barrier(
        &self,
        path: &str,
        body: Value,
        retry_after_seconds: u64,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (responded_tx, responded_rx) = oneshot::channel();
        self.state
            .replies
            .lock()
            .await
            .entry(path.into())
            .or_default()
            .push_back(UpstreamReply::Barrier {
                entered: entered_tx,
                release: release_rx,
                responded: responded_tx,
                status: StatusCode::TOO_MANY_REQUESTS,
                body,
                retry_after_seconds: Some(retry_after_seconds),
            });
        (entered_rx, release_tx, responded_rx)
    }

    async fn requests(&self) -> Vec<CapturedRequest> {
        self.state.requests.lock().await.clone()
    }

    async fn call_count(&self, path: &str) -> usize {
        self.state
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| request.path == path)
            .count()
    }
}

async fn upstream_handler(
    State(state): State<Arc<UpstreamState>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_owned();
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let streaming = body.get("stream").and_then(Value::as_bool) == Some(true);
    let state_before = headers
        .get("x-state-before")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.requests.lock().await.push(CapturedRequest {
        path: path.clone(),
        body,
        headers,
        state_before,
    });
    let reply = state
        .replies
        .lock()
        .await
        .get_mut(&path)
        .and_then(VecDeque::pop_front);
    match reply {
        Some(UpstreamReply::Json(status, body)) => (status, Json(body)).into_response(),
        Some(UpstreamReply::ResponsesStream(body)) => {
            if !streaming {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail":"stream must be set to true"})),
                )
                    .into_response();
            }
            let mut created = body.clone();
            created["status"] = "in_progress".into();
            created["output"] = json!([]);
            created["usage"] = Value::Null;
            let mut frames = vec![json!({
                "type":"response.created","sequence_number":0,"response":created
            })];
            let mut emit = |mut event: Value| {
                event["sequence_number"] = frames.len().into();
                frames.push(event);
            };
            for (output_index, item) in body["output"].as_array().unwrap().iter().enumerate() {
                let mut added = item.clone();
                added["status"] = "in_progress".into();
                if item["type"] == "message" {
                    added["content"] = json!([]);
                }
                emit(
                    json!({"type":"response.output_item.added","output_index":output_index,"item":added}),
                );
                if item["type"] == "message" {
                    for (content_index, part) in
                        item["content"].as_array().unwrap().iter().enumerate()
                    {
                        let mut added = part.clone();
                        added["text"] = "".into();
                        emit(
                            json!({"type":"response.content_part.added","output_index":output_index,
                            "content_index":content_index,"item_id":item["id"],"part":added}),
                        );
                        emit(
                            json!({"type":"response.output_text.delta","output_index":output_index,
                            "content_index":content_index,"item_id":item["id"],"delta":part["text"]}),
                        );
                        emit(
                            json!({"type":"response.output_text.done","output_index":output_index,
                            "content_index":content_index,"item_id":item["id"],"text":part["text"]}),
                        );
                        emit(
                            json!({"type":"response.content_part.done","output_index":output_index,
                            "content_index":content_index,"item_id":item["id"],"part":part}),
                        );
                    }
                }
                emit(
                    json!({"type":"response.output_item.done","output_index":output_index,"item":item}),
                );
            }
            emit(json!({"type":"response.completed","response":body}));
            let stream = frames
                .into_iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap()
                    )
                })
                .collect::<String>();
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                stream,
            )
                .into_response()
        }
        Some(UpstreamReply::Barrier {
            entered,
            release,
            responded,
            status,
            body,
            retry_after_seconds,
        }) => {
            let _ = entered.send(());
            let _ = release.await;
            let _ = responded.send(());
            let mut response = (status, Json(body)).into_response();
            if let Some(seconds) = retry_after_seconds {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    seconds.to_string().parse().expect("valid Retry-After"),
                );
            }
            response
        }
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":{"message":format!("no queued reply for {path}")}})),
        )
            .into_response(),
    }
}

async fn connect_mcp(endpoint: &str, token: &str) -> McpClient {
    let config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned())
        .auth_header(token.to_owned());
    let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
    InitializeRequestParams::default()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("connect official MCP client")
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-test-fixtures")
        .join(name)
}

async fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = fixture_path(name);
    tokio::fs::read(&path).await.unwrap_or_else(|error| {
        panic!(
            "missing real Wasm fixture {} ({error}); run task build:vendor-fixtures",
            path.display()
        )
    })
}

async fn install_fixture(
    gateway: &Gateway,
    name: &str,
    allow_data_discard: bool,
) -> anyhow::Result<stravia_core::plugin::PluginSummary> {
    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture_bytes(name).await)
        .await?;
    gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard,
        })
        .await
}

async fn preview_fixture(
    gateway: &Gateway,
    name: &str,
) -> anyhow::Result<stravia_core::plugin::PluginPreview> {
    gateway
        .admin()
        .preview_vendor_plugin(fixture_bytes(name).await)
        .await
}

async fn create_provider(
    gateway: &Gateway,
    name: &str,
    vendor: &str,
    base_url: &str,
    protocol: &str,
) -> anyhow::Result<stravia_core::db::models::Provider> {
    gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: name.into(),
            vendor: Some(vendor.into()),
            protocol: protocol.into(),
            base_url: base_url.into(),
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "fixture-secret-not-production".into(),
            adapter_credentials: r#"{"apiKey":"fixture-secret-not-production"}"#.into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await
}

async fn create_route(
    gateway: &Gateway,
    route_id: &str,
    targets: Vec<(&stravia_core::db::models::Provider, &str, i32)>,
    target_retry_budget: i32,
    output_modality: stravia_core::db::models::RouteOutputModality,
) -> anyhow::Result<stravia_core::db::models::RouteConfig> {
    for (provider, model, _) in &targets {
        if gateway
            .storage
            .provider_models()
            .find(&provider.id, model)
            .await?
            .is_none()
        {
            gateway
                .admin()
                .create_manual_provider_model(
                    &provider.id,
                    model,
                    CreateManualProviderModel {
                        metadata: json!({
                            "id": model,
                            "name": model,
                            "attachment": true,
                            "tool_call": true,
                            "capabilities": ["infer", "search", "media_image"],
                            "modalities": {"input":["text","image"],"output":["text","image"]}
                        }),
                        template_id: None,
                    },
                )
                .await?;
        }
    }
    gateway
        .admin()
        .create_model(CreateRoute {
            output_modality,
            model_id: route_id.into(),
            display_name: Some(route_id.into()),
            balance: Some("latency_preference".into()),
            targets: targets
                .into_iter()
                .map(|(provider, model, priority)| CreateTarget {
                    provider_id: provider.id.clone(),
                    model: model.to_owned(),
                    enabled: true,
                    priority: Some(priority),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(target_retry_budget),
                    target_cooldown_ms: Some(0),
                    thinking_level_map: Vec::new(),
                })
                .collect(),
            default_thinking_level: None,
            billing: None,
        })
        .await
}

async fn configure_external_search(gateway: &Gateway, route_id: &str) -> anyhow::Result<()> {
    let current = gateway.admin().get_web_search_config().await?;
    gateway
        .admin()
        .update_web_search_config(WebSearchConfig {
            revision: current.revision,
            enabled: true,
            backend: Some(WebSearchBackendDraft::External {
                route_id: Some(route_id.into()),
            }),
            max_turns: current.max_turns,
            total_time_seconds: current.total_time_seconds,
            updated_at: current.updated_at.clone(),
        })
        .await?;
    Ok(())
}

async fn configure_image_generation(gateway: &Gateway, route_id: &str) -> anyhow::Result<()> {
    let view = gateway
        .admin()
        .update_media_generation_config(MediaGenerationConfig {
            enabled: true,
            image: ImageGenerationConfig {
                route_id: Some(route_id.into()),
            },
        })
        .await?;
    anyhow::ensure!(view.validation.valid, "image Route must be valid");
    Ok(())
}

async fn terminal_search(
    gateway: &Gateway,
    principal: Principal,
    query: &str,
    previous_turn_id: Option<SearchTurnId>,
    cancellation: CancellationToken,
) -> WebSearchEvent {
    // Keep absolute deadlines on the Tokio clock used by the cooldown test.
    // After virtual advancement, a wall-clock deadline is already behind Tokio's
    // timer clock and Deadline::wait can repeatedly wake without yielding.
    terminal_search_with_deadline(
        gateway,
        principal,
        query,
        previous_turn_id,
        cancellation,
        (tokio::time::Instant::now() + Duration::from_secs(30)).into_std(),
    )
    .await
}

async fn terminal_search_with_deadline(
    gateway: &Gateway,
    principal: Principal,
    query: &str,
    previous_turn_id: Option<SearchTurnId>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> WebSearchEvent {
    let runner = gateway
        .web_search_runner()
        .await
        .expect("Web Search Runner");
    let mut events = runner.run(WebSearchInput {
        principal,
        query: query.into(),
        previous_turn_id,
        policy: Some(WebSearchRunPolicy::default()),
        cancellation,
        deadline,
    });
    while let Some(event) = events.next().await {
        if event.is_terminal() {
            return event;
        }
    }
    panic!("Web Search stream ended without a terminal event")
}

fn completed_search(event: WebSearchEvent) -> stravia_web_search::WebSearchResult {
    match event {
        WebSearchEvent::Completed(result) => result,
        other => panic!("expected completed Search, got {other:?}"),
    }
}

fn failed_search(event: WebSearchEvent) -> stravia_web_search::WebSearchError {
    match event {
        WebSearchEvent::Failed(error) => error,
        other => panic!("expected failed Search, got {other:?}"),
    }
}

fn search_response(answer: &str) -> Value {
    json!({
        "answer": answer,
        "sources": [{
            "id": "fixture-source",
            "url": SOURCE_URL,
            "title": "Verified source",
            "snippet": "Controlled test evidence",
            "published_at": null
        }],
        "limitations": [],
        "usage": null
    })
}

fn image_response() -> Value {
    json!({
        "artifacts": [{
            "media_type": "image/png",
            "bytes": PNG_BASE64,
            "upstream_ref": "controlled-upstream-image",
            "metadata": {"fixture": true}
        }],
        "revised_prompt": null,
        "usage": null
    })
}

fn infer_response(id: &str, text: &str) -> Value {
    let mut response = AiResponse::new(id, "fixture-model");
    response.push_output_text(text);
    serde_json::to_value(response).expect("serialize controlled inference response")
}

async fn mark_base_as_previous_builtin(directory: &Path) -> anyhow::Result<()> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(DataPaths::new(directory).database()),
        )
        .await?;
    let updated =
        sqlx::query("UPDATE vendor_plugins SET source = 'builtin' WHERE vendor_id = 'base'")
            .execute(&pool)
            .await?;
    assert_eq!(updated.rows_affected(), 1);
    let digest: String =
        sqlx::query_scalar("SELECT digest FROM vendor_plugins WHERE vendor_id = 'base'")
            .fetch_one(&pool)
            .await?;
    assert!(
        DataPaths::new(directory)
            .plugins()
            .join("artifacts")
            .join(format!("{digest}.wasm"))
            .is_file(),
        "direct metadata fixture must retain its real content-addressed artifact"
    );
    pool.close().await;
    Ok(())
}

async fn call_generate(client: &McpClient, input: Value) -> rmcp::model::CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("generate")
                .with_arguments(input.as_object().expect("generation input object").clone()),
        )
        .await
        .expect("generate tools/call")
}

#[tokio::test]
async fn model_search_targets_enforce_complete_report_contracts() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-pure-search.wasm", false).await?;
    let upstream = LocalUpstream::start().await?;
    let provider = create_provider(
        &harness.gateway,
        "Model research",
        "fixture.capability-search",
        &upstream.url,
        "fixture-search",
    )
    .await?;

    let model_route = create_route(
        &harness.gateway,
        "model-search",
        vec![(&provider, "research-model", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&harness.gateway, &model_route.model_id).await?;
    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Model-qualified answer [sc:fixture-source]"),
        )
        .await;
    let model_result = completed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "model query",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(
        model_result
            .report
            .answer
            .contains("Model-qualified answer")
    );
    assert_eq!(model_result.report.sources.len(), 1);
    let source_path = format!("{}/sources/1", model_result.turn_id.reference());
    assert_eq!(model_result.report.sources[0].path, source_path);
    assert!(
        model_result
            .report
            .answer
            .contains(&format!("[{source_path}]"))
    );

    harness
        .gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "image-metadata-only",
            CreateManualProviderModel {
                metadata: json!({
                    "id":"image-metadata-only",
                    "name":"Not a Search Model",
                    "modalities":{"input":["text"],"output":["text"]},
                    "capabilities":["media_image"]
                }),
                template_id: None,
            },
        )
        .await?;
    let ineligible_model_route = create_route(
        &harness.gateway,
        "ineligible-model-search",
        vec![(&provider, "image-metadata-only", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let current = harness.gateway.admin().get_web_search_config().await?;
    let rejected_binding = harness
        .gateway
        .admin()
        .update_web_search_config(WebSearchConfig {
            revision: current.revision,
            enabled: true,
            backend: Some(WebSearchBackendDraft::External {
                route_id: Some(ineligible_model_route.model_id.clone().into()),
            }),
            max_turns: current.max_turns,
            total_time_seconds: current.total_time_seconds,
            updated_at: current.updated_at.clone(),
        })
        .await;
    assert!(rejected_binding.is_err());

    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Dangling reference [sc:not-a-source]"),
        )
        .await;
    let invalid = failed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "invalid citation query",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(invalid.code, "invalid_marker");

    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Independent report [sc:fixture-source]"),
        )
        .await;
    let root = completed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "independent root",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    let calls_before_continuation = upstream.call_count("/search").await;
    let continuation = failed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "attempt continuation",
            Some(root.turn_id),
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(continuation.code, "continuation_unsupported");
    assert_eq!(
        upstream.call_count("/search").await,
        calls_before_continuation,
        "external continuation must be rejected before the Vendor or upstream"
    );

    let eligible = harness
        .gateway
        .admin()
        .list_external_search_routes()
        .await?;
    assert!(
        eligible
            .iter()
            .any(|route| { route.model_id == model_route.model_id.as_str() && route.available })
    );
    assert!(eligible.iter().any(|route| {
        route.model_id == ineligible_model_route.model_id.as_str() && !route.available
    }));
    Ok(())
}

#[tokio::test]
async fn external_search_half_open_probe_sends_once_and_recovers_the_target() -> anyhow::Result<()>
{
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-pure-search.wasm", false).await?;
    let upstream = LocalUpstream::start().await?;
    let provider = create_provider(
        &harness.gateway,
        "Half-open search",
        "fixture.capability-search",
        &upstream.url,
        "fixture-search",
    )
    .await?;
    harness
        .gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "research-model",
            CreateManualProviderModel {
                metadata: json!({
                    "id": "research-model",
                    "name": "Research Model",
                    "capabilities": ["search"],
                    "modalities": {"input": ["text"], "output": ["text"]}
                }),
                template_id: None,
            },
        )
        .await?;
    let route = harness
        .gateway
        .admin()
        .create_model(CreateRoute {
            output_modality: stravia_core::db::models::RouteOutputModality::Text,
            model_id: "half-open-search".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "research-model".into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: Some(60_000),

                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    configure_external_search(&harness.gateway, &route.model_id).await?;
    upstream
        .push_json(
            "/search",
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"controlled outage"}}),
        )
        .await;
    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Probe recovered [sc:fixture-source]"),
        )
        .await;
    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Healthy again [sc:fixture-source]"),
        )
        .await;
    let principal = Principal::new(harness.key_id.clone());
    failed_search(
        terminal_search(
            &harness.gateway,
            principal.clone(),
            "outage",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    failed_search(
        terminal_search(
            &harness.gateway,
            principal.clone(),
            "still cooling",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(upstream.call_count("/search").await, 1);
    // Search revalidation and real HTTP need live timers. Pause only to move
    // the cooldown clock, not while awaiting those operations.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    let probe = completed_search(
        terminal_search(
            &harness.gateway,
            principal.clone(),
            "probe",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(probe.report.answer.contains("Probe recovered"));
    let healthy = completed_search(
        terminal_search(
            &harness.gateway,
            principal,
            "normal",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(healthy.report.answer.contains("Healthy again"));
    assert_eq!(upstream.call_count("/search").await, 3);
    Ok(())
}

#[tokio::test]
async fn external_search_retry_after_outside_window() -> anyhow::Result<()> {
    for backup in [true, false] {
        let harness = TestHarness::new().await?;
        install_fixture(&harness.gateway, "capability-pure-search.wasm", false).await?;
        let limited = LocalUpstream::start().await?;
        let healthy = LocalUpstream::start().await?;
        let primary = create_provider(
            &harness.gateway,
            "Limited",
            "fixture.capability-search",
            &limited.url,
            "fixture-search",
        )
        .await?;
        let fallback = create_provider(
            &harness.gateway,
            "Backup",
            "fixture.capability-search",
            &healthy.url,
            "fixture-search",
        )
        .await?;
        let mut targets = vec![(&primary, "research-model", 20)];
        if backup {
            targets.push((&fallback, "research-model", 10));
        }
        let route = create_route(
            &harness.gateway,
            "retry-window-search",
            targets,
            5,
            stravia_core::db::models::RouteOutputModality::Text,
        )
        .await?;
        configure_external_search(&harness.gateway, &route.model_id).await?;
        let (entered, release, responded) = limited
            .push_retry_barrier(
                "/search",
                json!({"error":{"message":"explicit upstream rate limit"}}),
                11450,
            )
            .await;
        healthy
            .push_json(
                "/search",
                StatusCode::OK,
                search_response("Window fallback [sc:fixture-source]"),
            )
            .await;
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        let task = tokio::spawn(async move {
            terminal_search_with_deadline(
                &gateway,
                principal,
                "window query",
                None,
                CancellationToken::new(),
                Instant::now() + Duration::from_secs(5),
            )
            .await
        });
        entered.await?;
        release.send(()).unwrap();
        responded.await?;
        let event = tokio::time::timeout(Duration::from_secs(2), task).await??;
        if backup {
            assert!(
                completed_search(event)
                    .report
                    .answer
                    .contains("Window fallback")
            );
            assert_eq!(healthy.call_count("/search").await, 1);
        } else {
            let error = failed_search(event);
            assert_eq!(error.code, "upstream_failed");
            assert_eq!(healthy.call_count("/search").await, 0);
        }
        assert_eq!(limited.call_count("/search").await, 1);
    }
    Ok(())
}

#[tokio::test]
async fn external_search_switches_only_retryable_started_failures() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-pure-search.wasm", false).await?;
    let primary_upstream = LocalUpstream::start().await?;
    let fallback_upstream = LocalUpstream::start().await?;
    let primary = create_provider(
        &harness.gateway,
        "Retryable primary",
        "fixture.capability-search",
        &primary_upstream.url,
        "fixture-search",
    )
    .await?;
    let fallback = create_provider(
        &harness.gateway,
        "Healthy fallback",
        "fixture.capability-search",
        &fallback_upstream.url,
        "fixture-search",
    )
    .await?;
    let route = create_route(
        &harness.gateway,
        "search-failover",
        vec![
            (&primary, "research-model", 20),
            (&fallback, "research-model", 10),
        ],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&harness.gateway, &route.model_id).await?;

    primary_upstream
        .push_json(
            "/search",
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"controlled retryable outage"}}),
        )
        .await;
    fallback_upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Fallback answer [sc:fixture-source]"),
        )
        .await;
    let result = completed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "retryable query",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(result.report.answer.contains("Fallback answer"));
    assert_eq!(primary_upstream.call_count("/search").await, 1);
    assert_eq!(fallback_upstream.call_count("/search").await, 1);

    let parameter_upstream = LocalUpstream::start().await?;
    let parameter_provider = create_provider(
        &harness.gateway,
        "Parameter rejection",
        "fixture.capability-search",
        &parameter_upstream.url,
        "fixture-search",
    )
    .await?;
    let parameter_route = create_route(
        &harness.gateway,
        "search-parameter-error",
        vec![(&parameter_provider, "research-model", 10)],
        3,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&harness.gateway, &parameter_route.model_id).await?;
    parameter_upstream
        .push_json(
            "/search",
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"controlled invalid query","type":"invalid_request"}}),
        )
        .await;
    let parameter_error = failed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "invalid upstream parameter",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(parameter_error.code, "upstream_failed");
    assert_eq!(
        parameter_upstream.call_count("/search").await,
        1,
        "parameter failures must not consume the retry budget"
    );

    let deadline_upstream = LocalUpstream::start().await?;
    let deadline_provider = create_provider(
        &harness.gateway,
        "Deadline target",
        "fixture.capability-search",
        &deadline_upstream.url,
        "fixture-search",
    )
    .await?;
    let deadline_route = create_route(
        &harness.gateway,
        "search-deadline",
        vec![(&deadline_provider, "research-model", 10)],
        3,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&harness.gateway, &deadline_route.model_id).await?;
    let (deadline_entered, deadline_release, deadline_responded) = deadline_upstream
        .push_barrier(
            "/search",
            StatusCode::OK,
            search_response("Expired Search must not publish [sc:fixture-source]"),
        )
        .await;
    let deadline_task = {
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        tokio::spawn(async move {
            terminal_search_with_deadline(
                &gateway,
                principal,
                "deadline query",
                None,
                CancellationToken::new(),
                Instant::now() + Duration::from_secs(2),
            )
            .await
        })
    };
    deadline_entered
        .await
        .expect("deadline Search reached controlled upstream");
    let deadline_error = failed_search(deadline_task.await?);
    assert_eq!(deadline_error.code, "deadline_exceeded");
    let _ = deadline_release.send(());
    let _ = deadline_responded.await;
    assert_eq!(
        deadline_upstream.call_count("/search").await,
        1,
        "deadline must not retry"
    );

    let cancellation_upstream = LocalUpstream::start().await?;
    let cancellation_provider = create_provider(
        &harness.gateway,
        "Cancellation target",
        "fixture.capability-search",
        &cancellation_upstream.url,
        "fixture-search",
    )
    .await?;
    let cancellation_route = create_route(
        &harness.gateway,
        "search-cancellation",
        vec![(&cancellation_provider, "research-model", 10)],
        3,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&harness.gateway, &cancellation_route.model_id).await?;
    let (entered, release, responded) = cancellation_upstream
        .push_barrier(
            "/search",
            StatusCode::OK,
            search_response("Too late [sc:fixture-source]"),
        )
        .await;
    let cancellation = CancellationToken::new();
    let task = {
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        let request_cancellation = cancellation.clone();
        tokio::spawn(async move {
            terminal_search(
                &gateway,
                principal,
                "cancelled query",
                None,
                request_cancellation,
            )
            .await
        })
    };
    entered.await.expect("search reached controlled upstream");
    cancellation.cancel();
    let cancelled = failed_search(task.await?);
    assert_eq!(cancelled.code, "cancelled");
    let _ = release.send(());
    // 取消可能已关闭 HTTP 连接，故上游响应既可能发送成功，也可能已被丢弃。
    let _ = responded.await;
    assert_eq!(
        cancellation_upstream.call_count("/search").await,
        1,
        "cancellation must not retry"
    );
    Ok(())
}

#[tokio::test]
async fn image_route_filters_reference_inputs_before_priority_and_failover() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-pure-image.wasm", false).await?;
    let text_upstream = LocalUpstream::start().await?;
    let image_upstream = LocalUpstream::start().await?;
    let text_provider = create_provider(
        &harness.gateway,
        "Text-to-image Provider",
        "fixture.capability-image",
        &text_upstream.url,
        "fixture-image",
    )
    .await?;
    let image_provider = create_provider(
        &harness.gateway,
        "Image-to-image Provider",
        "fixture.capability-image",
        &image_upstream.url,
        "fixture-image",
    )
    .await?;
    for (provider, model, inputs) in [
        (&text_provider, "text-to-image", json!(["text"])),
        (&image_provider, "image-to-image", json!(["text", "image"])),
    ] {
        harness
            .gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                model,
                CreateManualProviderModel {
                    metadata: json!({
                        "id": model,
                        "capabilities": ["media_image"],
                        "modalities": {"input": inputs, "output": ["image"]}
                    }),
                    template_id: None,
                },
            )
            .await?;
    }
    let route = create_route(
        &harness.gateway,
        "mixed-image-inputs",
        vec![
            (&text_provider, "text-to-image", 20),
            (&image_provider, "image-to-image", 10),
        ],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    configure_image_generation(&harness.gateway, &route.model_id).await?;
    let client = harness.connect().await;

    text_upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;
    let generated = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"generate without a reference","reference_images":[]}}),
    )
    .await;
    assert_ne!(generated.is_error, Some(true), "{generated:?}");
    assert_eq!(text_upstream.call_count("/image").await, 1);
    assert_eq!(image_upstream.call_count("/image").await, 0);
    let reference = generated.structured_content.expect("generated Artifact")["path"]
        .as_str()
        .expect("Artifact Reference")
        .to_owned();

    // 无参考图时两类均可执行，优先级和失败切换不因输入能力另设顺序。
    text_upstream
        .push_json(
            "/image",
            StatusCode::BAD_GATEWAY,
            json!({"error":"overloaded"}),
        )
        .await;
    image_upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;
    let fallback = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"generate through the fallback"}}),
    )
    .await;
    assert_ne!(fallback.is_error, Some(true), "{fallback:?}");
    assert_eq!(text_upstream.call_count("/image").await, 2);
    assert_eq!(image_upstream.call_count("/image").await, 1);

    text_upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;
    image_upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;
    let edited = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"edit the reference","reference_images":[reference]}}),
    )
    .await;
    assert_ne!(edited.is_error, Some(true), "{edited:?}");
    assert_eq!(
        text_upstream.call_count("/image").await,
        2,
        "reference images must exclude text-only input before selecting the higher-priority Target"
    );
    assert_eq!(image_upstream.call_count("/image").await, 2);
    let requests = image_upstream.requests().await;
    assert_eq!(requests[1].body["references"][0]["media_type"], "image/png");

    let text_only_route = create_route(
        &harness.gateway,
        "text-to-image-only",
        vec![(&text_provider, "text-to-image", 0)],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    configure_image_generation(&harness.gateway, &text_only_route.model_id).await?;
    let rejected = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"cannot edit","reference_images":[reference]}}),
    )
    .await;
    assert_eq!(rejected.is_error, Some(true), "{rejected:?}");
    assert_eq!(text_upstream.call_count("/image").await, 2);

    // 保存后的规格漂移立即生效，旧附件标志不能代替实际图片输入登记。
    let detail = harness
        .gateway
        .admin()
        .get_provider_model(&image_provider.id, "image-to-image")
        .await?;
    harness
        .gateway
        .admin()
        .update_provider_model(
            &image_provider.id,
            "image-to-image",
            stravia_core::provider_models::UpdateProviderModel {
                revision: detail.revision,
                changes: std::collections::BTreeMap::from([
                    ("modalities.input".to_string(), json!(["text"])),
                    ("modalities.output".to_string(), json!(["image"])),
                ]),
            },
        )
        .await?;
    configure_image_generation(&harness.gateway, &route.model_id).await?;
    let drifted = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"cannot edit after input drift","reference_images":[reference]}}),
    )
    .await;
    assert_eq!(drifted.is_error, Some(true), "{drifted:?}");
    assert_eq!(text_upstream.call_count("/image").await, 2);
    assert_eq!(image_upstream.call_count("/image").await, 2);
    Ok(())
}

#[tokio::test]
async fn pure_image_vendor_stores_real_owned_png_and_cannot_chat() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-pure-image.wasm", false).await?;
    install_fixture(&harness.gateway, "capability-pure-search.wasm", false).await?;
    let upstream = LocalUpstream::start().await?;
    let provider = create_provider(
        &harness.gateway,
        "Pure image Provider",
        "fixture.capability-image",
        &upstream.url,
        "fixture-image",
    )
    .await?;
    let route = create_route(
        &harness.gateway,
        "pure-image-route",
        vec![(&provider, "image-only-model", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    let client = harness.connect().await;
    assert!(
        client
            .list_tools(None)
            .await?
            .tools
            .iter()
            .all(|tool| tool.name != "generate"),
        "disabled platform capability must hide the media tool"
    );

    let search_only_provider = create_provider(
        &harness.gateway,
        "Search-only mixed Target",
        "fixture.capability-search",
        &upstream.url,
        "fixture-search",
    )
    .await?;
    let mixed_route = create_route(
        &harness.gateway,
        "mixed-image-route",
        vec![
            (&provider, "image-only-model", 20),
            (&search_only_provider, "search-only-model", 10),
        ],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    assert!(
        harness
            .gateway
            .admin()
            .update_media_generation_config(MediaGenerationConfig {
                enabled: true,
                image: ImageGenerationConfig {
                    route_id: Some(mixed_route.model_id.into()),
                },
            })
            .await
            .is_err(),
        "one incompatible enabled Target must reject the whole media Route"
    );

    configure_image_generation(&harness.gateway, &route.model_id).await?;
    assert!(
        client
            .list_tools(None)
            .await?
            .tools
            .iter()
            .any(|tool| tool.name == "generate"),
        "MCP access may use an enabled media tool without transparent injection"
    );
    upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;

    let generated = call_generate(
        &client,
        json!({
            "type":"image",
            "input":{"prompt":"a controlled one pixel image","aspect_ratio":"1:1","resolution":"1K"}
        }),
    )
    .await;
    assert_ne!(generated.is_error, Some(true), "{generated:?}");
    let output = generated
        .structured_content
        .expect("structured generation result");
    assert_eq!(output["mime_type"], "image/png");
    assert_eq!(output["media"], json!({"width":1,"height":1}));
    let expected_bytes = base64::engine::general_purpose::STANDARD.decode(PNG_BASE64)?;
    assert_eq!(output["size"], expected_bytes.len());
    let reference = output["path"].as_str().expect("Artifact Reference");
    let artifact_id = ArtifactId::from_reference(reference)
        .expect("generated output must be a plain Artifact Reference");
    let store = harness
        .gateway
        .artifact_store()
        .expect("SQLite Gateway Artifact store");
    let owner_reader = store
        .open(&Principal::new(harness.key_id.clone()), &artifact_id)
        .await?;
    assert_eq!(owner_reader.artifact.mime_type, "image/png");
    assert_eq!(owner_reader.artifact.size, expected_bytes.len() as u64);
    assert!(
        store
            .open(&Principal::new("different-principal"), &artifact_id)
            .await
            .is_err(),
        "Artifact identity must not grant cross-Principal access"
    );

    let (_other_id, other_token) = harness.create_key("Different capability owner").await?;
    let other_client = connect_mcp(&harness.endpoint, &other_token).await;
    let calls_before_foreign_reference = upstream.call_count("/image").await;
    let foreign = call_generate(
        &other_client,
        json!({
            "type":"image",
            "input":{"prompt":"attempt a foreign edit","reference_images":[reference]}
        }),
    )
    .await;
    assert_eq!(foreign.is_error, Some(true));
    assert_eq!(
        upstream.call_count("/image").await,
        calls_before_foreign_reference,
        "foreign Artifact references must be rejected before Vendor execution"
    );

    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[route.id.clone()]}))?,
        )
        .await?;
    let calls_before_chat = upstream.call_count("/image").await;
    let response = reqwest::Client::new()
        .post(harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses")
        .bearer_auth(&harness.token)
        .json(&json!({"model":route.model_id,"input":"this must not become image generation"}))
        .send()
        .await?;
    assert!(
        !response.status().is_success(),
        "a pure image model must not be admitted as chat inference"
    );
    assert_eq!(upstream.call_count("/image").await, calls_before_chat);
    assert_eq!(upstream.call_count("/infer").await, 0);
    Ok(())
}

#[tokio::test]
async fn model_turn_unsupported_target_fails_over_without_retry_or_health_penalty()
-> anyhow::Result<()> {
    model_turn_guest_input_error("unsupported", true, false).await
}

#[tokio::test]
async fn model_turn_invalid_target_input_remains_terminal() -> anyhow::Result<()> {
    model_turn_guest_input_error("invalid", false, false).await
}

#[tokio::test]
async fn model_turn_unsupported_opaque_conversation_remains_terminal() -> anyhow::Result<()> {
    model_turn_guest_input_error("unsupported", false, true).await
}

async fn model_turn_guest_input_error(
    mode: &str,
    should_fail_over: bool,
    opaque_conversation: bool,
) -> anyhow::Result<()> {
    use stravia_core::thinking::{ThinkingLevelMapping, ThinkingMappingSource};
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-multi.wasm", false).await?;
    let primary_upstream = LocalUpstream::start().await?;
    let fallback_upstream = LocalUpstream::start().await?;
    let primary = create_provider(
        &harness.gateway,
        "Locally incompatible inference",
        "fixture.capability-contract",
        &primary_upstream.url,
        "fixture-multi",
    )
    .await?;
    let fallback = create_provider(
        &harness.gateway,
        "Compatible inference",
        "custom",
        &fallback_upstream.url,
        "openai-compatible",
    )
    .await?;
    let route = create_route(
        &harness.gateway,
        "guest-input-error",
        vec![(&primary, "primary", 20), (&fallback, "fallback", 10)],
        5,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    for (provider, model, efforts) in [
        (&primary, "primary", vec!["low"]),
        (&fallback, "fallback", vec!["low", "high"]),
    ] {
        let detail = harness
            .gateway
            .admin()
            .get_provider_model(&provider.id, model)
            .await?;
        harness
            .gateway
            .admin()
            .update_provider_model(
                &provider.id,
                model,
                stravia_core::provider_models::UpdateProviderModel {
                    revision: detail.revision,
                    changes: std::collections::BTreeMap::from([
                        ("modalities.input".to_string(), json!(["text"])),
                        ("modalities.output".to_string(), json!(["text"])),
                        ("reasoning_efforts".to_string(), json!(efforts)),
                    ]),
                },
            )
            .await?;
    }
    let targets = [(&primary, "primary", 20), (&fallback, "fallback", 10)]
        .into_iter()
        .map(|(provider, model, priority)| CreateTarget {
            provider_id: provider.id.clone(),
            model: model.into(),
            enabled: true,
            priority: Some(priority),
            first_token_timeout_ms: None,
            target_retry_budget: Some(5),
            target_cooldown_ms: Some(60_000),
            thinking_level_map: ThinkingLevel::ALL
                .into_iter()
                .map(|level| ThinkingLevelMapping {
                    level,
                    control: if level == ThinkingLevel::Low
                        || (provider.id == fallback.id && level == ThinkingLevel::High)
                    {
                        TargetThinkingControl::Effort {
                            value: level.as_str().into(),
                        }
                    } else {
                        TargetThinkingControl::Hidden
                    },
                    source: ThinkingMappingSource::Overridden,
                })
                .collect(),
        })
        .collect();
    harness
        .gateway
        .admin()
        .update_model(
            &route.model_id,
            stravia_core::db::models::UpdateRoute {
                targets: Some(targets),
                ..Default::default()
            },
        )
        .await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[route.id]}))?,
        )
        .await?;

    // 重复独立请求可暴露错误累计的健康失败；每次仍应恰好尝试一次不适配的首选 Target。
    let request_count = if should_fail_over { 7 } else { 1 };
    let client = reqwest::Client::new();
    for attempt in 1..=request_count {
        primary_upstream
            .push_json(
                "/infer",
                StatusCode::OK,
                json!({"fixture_infer_error": mode}),
            )
            .await;
        fallback_upstream
            .push_json(
                "/v1/chat/completions",
                StatusCode::OK,
                json!({
                    "id": "chatcmpl-compatible",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "fallback",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "compatible Target completed"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }),
            )
            .await;
        let (path, request) = if opaque_conversation {
            (
                "/v1/responses",
                json!({
                    "model": route.model_id,
                    "input": "Continue the opaque upstream conversation",
                    "conversation": "conversation-not-replayable-on-another-target",
                    "reasoning": {"effort": "high"},
                    "stream": false
                }),
            )
        } else {
            (
                "/v1/chat/completions",
                json!({
                    "model": route.model_id,
                    "messages": [{"role":"user","content":"Use the original high effort on fallback"}],
                    "reasoning_effort": "high",
                    "stream": false
                }),
            )
        };
        let response = client
            .post(harness.endpoint.trim_end_matches("/mcp").to_owned() + path)
            .bearer_auth(&harness.token)
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        assert_eq!(
            primary_upstream.call_count("/infer").await,
            attempt,
            "local incompatibility must neither retry the same Target nor make it unhealthy"
        );
        if should_fail_over {
            let requests = fallback_upstream.requests().await;
            assert!(
                status.is_success(),
                "{status}: {body}; fallback paths: {:?}",
                requests
                    .iter()
                    .map(|request| request.path.as_str())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                body["choices"][0]["message"]["content"],
                "compatible Target completed"
            );
            assert_eq!(requests.len(), attempt);
            assert_eq!(
                requests.last().unwrap().body["reasoning_effort"],
                "high",
                "fallback must restart from the client's level, not the first Target's low clamp"
            );
        } else {
            assert!(!status.is_success(), "{status}: {body}");
            assert!(
                fallback_upstream.requests().await.is_empty(),
                "terminal input errors must not dispatch any fallback request"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn mixed_capability_channel_keeps_image_only_models_out_of_chat() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-multi.wasm", false).await?;
    let upstream = LocalUpstream::start().await?;
    let provider = create_provider(
        &harness.gateway,
        "Mixed inference and image Provider",
        "fixture.capability-contract",
        &upstream.url,
        "fixture-multi",
    )
    .await?;

    for (model, capabilities, modalities) in [
        (
            "media-image-only",
            json!(["media_image"]),
            json!({"input":["text","image"],"output":["image"]}),
        ),
        (
            "image-output-only",
            json!(["image_output"]),
            json!({"input":["text","image"],"output":["text","image"]}),
        ),
    ] {
        harness
            .gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                model,
                CreateManualProviderModel {
                    metadata: json!({
                        "id": model,
                        "name": model,
                        "capabilities": capabilities,
                        "modalities": modalities,
                    }),
                    template_id: None,
                },
            )
            .await?;
    }
    harness
        .gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "legacy-text-model",
            CreateManualProviderModel {
                metadata: json!({
                    "id":"legacy-text-model",
                    "name":"Legacy text model without an explicit infer label",
                    "modalities":{"input":["text"],"output":["text"]},
                }),
                template_id: None,
            },
        )
        .await?;

    let image_route = create_route(
        &harness.gateway,
        "mixed-channel-image-only",
        vec![(&provider, "media-image-only", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    let failover_route = create_route(
        &harness.gateway,
        "mixed-channel-chat-failover",
        vec![
            (&provider, "image-output-only", 20),
            (&provider, "legacy-text-model", 10),
        ],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    // The binding was valid when saved; a later specification change makes
    // this target image-only while retaining its original binding.
    let detail = harness
        .gateway
        .admin()
        .get_provider_model(&provider.id, "image-output-only")
        .await?;
    harness
        .gateway
        .admin()
        .update_provider_model(
            &provider.id,
            "image-output-only",
            stravia_core::provider_models::UpdateProviderModel {
                changes: std::collections::BTreeMap::from([(
                    "modalities.output".to_string(),
                    json!(["image"]),
                )]),
                revision: detail.revision,
            },
        )
        .await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({
                "model_ids":[image_route.id.clone(), failover_route.id.clone()]
            }))?,
        )
        .await?;

    configure_image_generation(&harness.gateway, &image_route.model_id).await?;
    harness
        .gateway
        .admin()
        .set_setting(
            "artifact_settings",
            &json!({
                "client_base_url": harness.endpoint.trim_end_matches("/mcp"),
                "external_signed_downloads": true,
                "file_public_base_url": "https://files.invalid/not-the-client"
            })
            .to_string(),
        )
        .await?;
    upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;
    let client = harness.connect().await;
    let generated = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"mixed-channel image remains available"}}),
    )
    .await;
    assert_ne!(generated.is_error, Some(true), "{generated:?}");
    assert_eq!(upstream.call_count("/image").await, 1);
    let image = generated
        .structured_content
        .expect("generated image result");
    let download_url = image["download_url"].as_str().expect("image download URL");
    assert!(download_url.starts_with(harness.endpoint.trim_end_matches("/mcp")));
    assert!(image.get("expires_at").is_none());
    let downloaded = reqwest::get(download_url).await?;
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(downloaded.headers()["content-type"], "image/png");
    assert_eq!(
        downloaded.bytes().await?.as_ref(),
        base64::engine::general_purpose::STANDARD
            .decode(PNG_BASE64)?
            .as_slice()
    );

    let endpoint = harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses";
    let rejected = reqwest::Client::new()
        .post(&endpoint)
        .bearer_auth(&harness.token)
        .json(&json!({"model":image_route.model_id,"input":"ordinary chat must be ineligible"}))
        .send()
        .await?;
    let rejected_status = rejected.status();
    let rejected_body = rejected.text().await?;
    assert_eq!(
        rejected_status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{rejected_body}"
    );
    assert_eq!(
        upstream.call_count("/infer").await,
        0,
        "image-only Provider Model must be rejected before network execution"
    );

    upstream
        .push_json(
            "/infer",
            StatusCode::OK,
            infer_response("legacy-chat", "Legacy text inference remains available"),
        )
        .await;
    let chat = reqwest::Client::new()
        .post(endpoint)
        .bearer_auth(&harness.token)
        .json(&json!({"model":failover_route.model_id,"input":"use a genuinely eligible Target"}))
        .send()
        .await?;
    let chat_status = chat.status();
    let chat_body = chat.text().await?;
    assert_eq!(chat_status, StatusCode::OK, "{chat_body}");
    assert!(chat_body.contains("Legacy text inference remains available"));
    assert_eq!(upstream.call_count("/infer").await, 1);
    for base in ["", "file:///invalid-client-base"] {
        // 模拟缺失或损坏的持久配置；管理面正常保存会先拒绝这些值。
        harness
            .gateway
            .storage
            .settings()
            .set(
                "artifact_settings",
                &json!({"client_base_url":base}).to_string(),
            )
            .await?;
        let rejected = call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"must not incur a paid call"}}),
        )
        .await;
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(upstream.call_count("/image").await, 1);
    }

    harness
        .gateway
        .admin()
        .set_setting(
            "artifact_settings",
            &json!({"client_base_url":harness.endpoint.trim_end_matches("/mcp")}).to_string(),
        )
        .await?;
    let (entered, release, responded) = upstream
        .push_barrier("/image", StatusCode::OK, image_response())
        .await;
    let pending = {
        let client = harness.connect().await;
        tokio::spawn(async move {
            call_generate(
                &client,
                json!({"type":"image","input":{"prompt":"retain this paid image"}}),
            )
            .await
        })
    };
    entered.await?;
    // Configuration changes after admission must not discard an already paid image.
    harness
        .gateway
        .storage
        .settings()
        .set(
            "artifact_settings",
            &json!({"client_base_url":""}).to_string(),
        )
        .await?;
    release.send(()).expect("release image response");
    responded.await?;
    let failed = pending.await?;
    assert_eq!(failed.is_error, Some(true));
    let failed = failed
        .structured_content
        .expect("structured download failure");
    assert_eq!(failed["error"]["code"], "artifact_download_failed");
    let retained =
        ArtifactId::from_reference(failed["artifact"]["path"].as_str().expect("retained image"))?;
    let reader = harness
        .gateway
        .artifact_store()
        .unwrap()
        .open(&Principal::new(harness.key_id.clone()), &retained)
        .await?;
    assert_eq!(reader.artifact.mime_type, "image/png");
    assert_eq!(
        upstream.call_count("/image").await,
        2,
        "issuance failure must not regenerate"
    );
    Ok(())
}

#[tokio::test]
async fn compatible_update_keeps_search_and_image_retries_on_the_pinned_component()
-> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-multi.wasm", false).await?;
    let upstream = LocalUpstream::start().await?;
    let provider = create_provider(
        &harness.gateway,
        "Pinned retry Provider",
        "fixture.capability-contract",
        &upstream.url,
        "fixture-multi",
    )
    .await?;
    let search_route = create_route(
        &harness.gateway,
        "pinned-retry-search",
        vec![(&provider, "research-model", 10)],
        1,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let image_route = create_route(
        &harness.gateway,
        "pinned-retry-image",
        vec![(&provider, "multi-image", 10)],
        1,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    configure_external_search(&harness.gateway, &search_route.model_id).await?;
    configure_image_generation(&harness.gateway, &image_route.model_id).await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[image_route.id.clone()]}))?,
        )
        .await?;

    let (search_entered, search_release, search_responded) = upstream
        .push_retry_barrier(
            "/search",
            json!({"error":{"message":"retry pinned Search"}}),
            1,
        )
        .await;
    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Pinned Search result [sc:fixture-source]"),
        )
        .await;
    let (image_entered, image_release, image_responded) = upstream
        .push_retry_barrier(
            "/image",
            json!({"error":{"message":"retry pinned image"}}),
            1,
        )
        .await;
    upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;

    let search_task = {
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        tokio::spawn(async move {
            terminal_search(
                &gateway,
                principal,
                "compatible update during Search retry",
                None,
                CancellationToken::new(),
            )
            .await
        })
    };
    let image_task = {
        let client = harness.connect().await;
        tokio::spawn(async move {
            call_generate(
                &client,
                json!({"type":"image","input":{"prompt":"compatible retry image"}}),
            )
            .await
        })
    };
    search_entered.await.expect("Search reached old component");
    image_entered.await.expect("image reached old component");

    let preview = preview_fixture(&harness.gateway, "capability-removed.wasm").await?;
    assert!(!preview.cancels_active_operations);
    assert_eq!(preview.active_operations, 2);
    harness
        .gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    search_release
        .send(())
        .expect("release retryable Search response");
    image_release
        .send(())
        .expect("release retryable image response");
    search_responded.await.expect("Search retry response sent");
    image_responded.await.expect("image retry response sent");

    let search = completed_search(search_task.await?);
    assert!(search.report.answer.contains("Pinned Search result"));
    let image = image_task.await?;
    assert_ne!(image.is_error, Some(true), "{image:?}");
    assert!(
        image
            .structured_content
            .as_ref()
            .is_some_and(|value| value.get("path").is_some())
    );
    assert_eq!(upstream.call_count("/search").await, 2);
    assert_eq!(upstream.call_count("/image").await, 2);
    Ok(())
}

#[tokio::test]
async fn compatible_update_keeps_search_and_image_failover_on_the_supplier_component()
-> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-multi.wasm", false).await?;
    let primary_upstream = LocalUpstream::start().await?;
    let fallback_upstream = LocalUpstream::start().await?;
    let primary = create_provider(
        &harness.gateway,
        "Pinned supplier primary",
        "fixture.capability-contract",
        &primary_upstream.url,
        "fixture-multi",
    )
    .await?;
    let fallback = create_provider(
        &harness.gateway,
        "Pinned supplier fallback",
        "fixture.capability-contract",
        &fallback_upstream.url,
        "fixture-multi",
    )
    .await?;
    let search_route = create_route(
        &harness.gateway,
        "pinned-supplier-search-failover",
        vec![
            (&primary, "research-model", 20),
            (&fallback, "research-model", 10),
        ],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let image_route = create_route(
        &harness.gateway,
        "pinned-supplier-image-failover",
        vec![
            (&primary, "multi-image", 20),
            (&fallback, "multi-image", 10),
        ],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    configure_external_search(&harness.gateway, &search_route.model_id).await?;
    configure_image_generation(&harness.gateway, &image_route.model_id).await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[image_route.id.clone()]}))?,
        )
        .await?;

    let (search_entered, search_release, search_responded) = primary_upstream
        .push_barrier(
            "/search",
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"fail over pinned Search"}}),
        )
        .await;
    fallback_upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Pinned supplier Search fallback [sc:fixture-source]"),
        )
        .await;
    let (image_entered, image_release, image_responded) = primary_upstream
        .push_barrier(
            "/image",
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"message":"fail over pinned image"}}),
        )
        .await;
    fallback_upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;

    let search_task = {
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        tokio::spawn(async move {
            terminal_search(
                &gateway,
                principal,
                "compatible update during Search failover",
                None,
                CancellationToken::new(),
            )
            .await
        })
    };
    let image_task = {
        let client = harness.connect().await;
        tokio::spawn(async move {
            call_generate(
                &client,
                json!({"type":"image","input":{"prompt":"compatible failover image"}}),
            )
            .await
        })
    };
    search_entered.await.expect("Search reached primary Target");
    image_entered.await.expect("image reached primary Target");

    let preview = preview_fixture(&harness.gateway, "capability-removed.wasm").await?;
    assert!(!preview.cancels_active_operations);
    assert_eq!(preview.active_operations, 2);
    harness
        .gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    search_release
        .send(())
        .expect("release failed Search primary");
    image_release
        .send(())
        .expect("release failed image primary");
    search_responded.await.expect("Search failure sent");
    image_responded.await.expect("image failure sent");

    let search = completed_search(search_task.await?);
    assert!(
        search
            .report
            .answer
            .contains("Pinned supplier Search fallback")
    );
    let image = image_task.await?;
    assert_ne!(image.is_error, Some(true), "{image:?}");
    assert!(
        image
            .structured_content
            .as_ref()
            .is_some_and(|value| value.get("path").is_some())
    );
    assert_eq!(primary_upstream.call_count("/search").await, 1);
    assert_eq!(fallback_upstream.call_count("/search").await, 1);
    assert_eq!(primary_upstream.call_count("/image").await, 1);
    assert_eq!(fallback_upstream.call_count("/image").await, 1);
    Ok(())
}

#[tokio::test]
async fn incompatible_update_cancels_search_and_image_retry_backoff_without_replay()
-> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-multi.wasm", false).await?;
    let upstream = LocalUpstream::start().await?;
    let provider = create_provider(
        &harness.gateway,
        "Cancelled retry Provider",
        "fixture.capability-contract",
        &upstream.url,
        "fixture-multi",
    )
    .await?;
    let search_route = create_route(
        &harness.gateway,
        "cancelled-retry-search",
        vec![(&provider, "research-model", 10)],
        1,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let image_route = create_route(
        &harness.gateway,
        "cancelled-retry-image",
        vec![(&provider, "multi-image", 10)],
        1,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    configure_external_search(&harness.gateway, &search_route.model_id).await?;
    configure_image_generation(&harness.gateway, &image_route.model_id).await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[image_route.id.clone()]}))?,
        )
        .await?;

    let (search_entered, search_release, search_responded) = upstream
        .push_retry_barrier(
            "/search",
            json!({"error":{"message":"cancel Search retry backoff"}}),
            2,
        )
        .await;
    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Replayed Search must not publish [sc:fixture-source]"),
        )
        .await;
    let (image_entered, image_release, image_responded) = upstream
        .push_retry_barrier(
            "/image",
            json!({"error":{"message":"cancel image retry backoff"}}),
            2,
        )
        .await;
    upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;

    let search_task = {
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        tokio::spawn(async move {
            terminal_search(
                &gateway,
                principal,
                "incompatible update during Search retry backoff",
                None,
                CancellationToken::new(),
            )
            .await
        })
    };
    let image_task = {
        let client = harness.connect().await;
        tokio::spawn(async move {
            call_generate(
                &client,
                json!({"type":"image","input":{"prompt":"cancelled retry image"}}),
            )
            .await
        })
    };
    search_entered.await.expect("Search reached first attempt");
    image_entered.await.expect("image reached first attempt");
    // Wasm compilation belongs outside the retry window; both operations are
    // already active while their first upstream responses remain blocked.
    let preview = preview_fixture(&harness.gateway, "capability-incompatible.wasm").await?;
    assert!(preview.cancels_active_operations);
    assert_eq!(
        preview.active_operations, 2,
        "both pending capabilities must remain owned by the old Vendor"
    );
    search_release
        .send(())
        .expect("release retryable Search response");
    image_release
        .send(())
        .expect("release retryable image response");
    search_responded.await.expect("Search retry response sent");
    image_responded.await.expect("image retry response sent");
    tokio::task::yield_now().await;

    harness
        .gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: true,
        })
        .await?;

    let search = failed_search(search_task.await?);
    assert_eq!(search.code, "cancelled");
    let image = image_task.await?;
    assert_eq!(image.is_error, Some(true));
    assert_eq!(
        image
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/code"))
            .and_then(Value::as_str),
        Some("cancelled")
    );
    assert!(
        image
            .structured_content
            .as_ref()
            .is_none_or(|value| value.get("path").is_none())
    );
    assert_eq!(
        upstream.call_count("/search").await,
        1,
        "cancelled Search retry must not replay through the new component"
    );
    assert_eq!(
        upstream.call_count("/image").await,
        1,
        "cancelled image retry must not replay through the new component"
    );
    Ok(())
}

#[tokio::test]
async fn compatible_capability_removal_preserves_inflight_result_and_binding() -> anyhow::Result<()>
{
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-multi.wasm", false).await?;
    let upstream = LocalUpstream::start().await?;
    let provider = create_provider(
        &harness.gateway,
        "Capability removal Provider",
        "fixture.capability-contract",
        &upstream.url,
        "fixture-multi",
    )
    .await?;
    let route = create_route(
        &harness.gateway,
        "capability-removal-search",
        vec![(&provider, "research-model", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&harness.gateway, &route.model_id).await?;
    let (entered, release, responded) = upstream
        .push_barrier(
            "/search",
            StatusCode::OK,
            search_response("Old version result [sc:fixture-source]"),
        )
        .await;
    let inflight = {
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        tokio::spawn(async move {
            terminal_search(
                &gateway,
                principal,
                "started before compatible update",
                None,
                CancellationToken::new(),
            )
            .await
        })
    };
    entered
        .await
        .expect("old Search entered controlled upstream");

    let preview = preview_fixture(&harness.gateway, "capability-removed.wasm").await?;
    assert!(!preview.cancels_active_operations);
    assert_eq!(preview.active_operations, 1);
    assert!(preview.affected_bindings.iter().any(|impact| {
        impact.route_id == route.model_id.as_str() && impact.capability == "search"
    }));
    harness
        .gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    release.send(()).expect("release old Search response");
    responded.await.expect("old Search response sent");
    let old_result = completed_search(inflight.await?);
    assert!(old_result.report.answer.contains("Old version result"));

    let config = harness.gateway.admin().get_web_search_config().await?;
    assert!(matches!(
        config.backend.as_ref(),
        Some(WebSearchBackendDraft::External { route_id: Some(id) }) if id.as_str() == route.model_id.as_str()
    ));
    let persisted_route = harness.gateway.admin().get_model(&route.model_id).await?;
    assert_eq!(persisted_route.targets.len(), 1);
    let calls_before_new = upstream.call_count("/search").await;
    failed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "started after capability removal",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(upstream.call_count("/search").await, calls_before_new);
    let summary = harness
        .gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "fixture.capability-contract")
        .expect("updated fixture summary");
    assert_eq!(summary.version, "2.0.0");
    assert!(summary.affected_bindings.iter().any(|impact| {
        impact.route_id == route.model_id.as_str() && impact.capability == "search"
    }));
    Ok(())
}

#[tokio::test]
async fn incompatible_update_cancels_late_inference_search_and_image_without_harming_other_vendor()
-> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_fixture(&harness.gateway, "capability-multi.wasm", false).await?;
    install_fixture(&harness.gateway, "capability-pure-search.wasm", false).await?;
    let updating_upstream = LocalUpstream::start().await?;
    let other_upstream = LocalUpstream::start().await?;
    let updating_provider = create_provider(
        &harness.gateway,
        "Updating multi-capability Provider",
        "fixture.capability-contract",
        &updating_upstream.url,
        "fixture-multi",
    )
    .await?;
    let other_provider = create_provider(
        &harness.gateway,
        "Independent search Provider",
        "fixture.capability-search",
        &other_upstream.url,
        "fixture-search",
    )
    .await?;
    let search_route = create_route(
        &harness.gateway,
        "updating-search",
        vec![(&updating_provider, "research-model", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let image_route = create_route(
        &harness.gateway,
        "updating-image",
        vec![(&updating_provider, "multi-image", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    let other_route = create_route(
        &harness.gateway,
        "independent-search",
        vec![(&other_provider, "research-model", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&harness.gateway, &search_route.model_id).await?;
    configure_image_generation(&harness.gateway, &image_route.model_id).await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[image_route.id.clone()]}))?,
        )
        .await?;

    let (infer_entered, infer_release, infer_responded) = updating_upstream
        .push_barrier(
            "/infer",
            StatusCode::OK,
            infer_response("late-inference", "Late inference must not be published"),
        )
        .await;
    let (search_entered, search_release, search_responded) = updating_upstream
        .push_barrier(
            "/search",
            StatusCode::OK,
            search_response("Late Search report [sc:fixture-source]"),
        )
        .await;
    let (image_entered, image_release, image_responded) = updating_upstream
        .push_barrier("/image", StatusCode::OK, image_response())
        .await;
    let infer_task = {
        let endpoint = harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses";
        let token = harness.token.clone();
        let route_id = image_route.model_id.clone();
        tokio::spawn(async move {
            let response = reqwest::Client::new()
                .post(endpoint)
                .bearer_auth(token)
                .json(&json!({"model":route_id,"input":"inference interrupted by update"}))
                .send()
                .await?;
            let status = response.status();
            let body = response.text().await?;
            Ok::<_, reqwest::Error>((status, body))
        })
    };
    let search_task = {
        let gateway = harness.gateway.clone();
        let principal = Principal::new(harness.key_id.clone());
        tokio::spawn(async move {
            terminal_search(
                &gateway,
                principal,
                "Search interrupted by incompatible update",
                None,
                CancellationToken::new(),
            )
            .await
        })
    };
    let image_task = {
        let client = harness.connect().await;
        tokio::spawn(async move {
            call_generate(
                &client,
                json!({"type":"image","input":{"prompt":"late incompatible image"}}),
            )
            .await
        })
    };
    infer_entered
        .await
        .expect("inference reached controlled upstream");
    search_entered
        .await
        .expect("Search reached controlled upstream");
    image_entered
        .await
        .expect("image reached controlled upstream");

    let preview = preview_fixture(&harness.gateway, "capability-incompatible.wasm").await?;
    assert!(preview.cancels_active_operations);
    assert!(preview.active_operations >= 3);
    harness
        .gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: true,
        })
        .await?;

    let infer_terminal = infer_task.await??;
    let search_terminal = search_task.await?;
    let image_terminal = image_task.await?;
    let _ = infer_release.send(());
    let _ = search_release.send(());
    let _ = image_release.send(());
    // 连接关闭和仍在上游执行后返回都合法；下面验证两种情形均不能提交旧结果。
    let _ = infer_responded.await;
    let _ = search_responded.await;
    let _ = image_responded.await;
    assert!(!infer_terminal.0.is_success());
    assert!(
        !infer_terminal
            .1
            .contains("Late inference must not be published")
    );
    assert_eq!(failed_search(search_terminal).code, "cancelled");
    assert_eq!(image_terminal.is_error, Some(true));
    assert_eq!(
        image_terminal
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer("/error/code"))
            .and_then(Value::as_str),
        Some("cancelled")
    );
    assert!(
        image_terminal
            .structured_content
            .as_ref()
            .is_none_or(|value| value.get("path").is_none()),
        "cancelled old image bytes must not be published as an Artifact"
    );

    let updated = harness
        .gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "fixture.capability-contract")
        .expect("updated multi-capability fixture");
    assert_eq!(updated.version, "3.0.0");

    updating_upstream
        .push_json(
            "/infer",
            StatusCode::OK,
            infer_response("after-reset", "New generation after reset"),
        )
        .await;
    let after_reset = reqwest::Client::new()
        .post(harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses")
        .bearer_auth(&harness.token)
        .json(&json!({"model":image_route.model_id,"input":"verify reset state"}))
        .send()
        .await?;
    let after_reset_status = after_reset.status();
    let after_reset_body = after_reset.text().await?;
    assert!(after_reset_status.is_success(), "{after_reset_body}");
    assert!(after_reset_body.contains("New generation after reset"));
    let reset_request = updating_upstream
        .requests()
        .await
        .into_iter()
        .rev()
        .find(|request| request.path == "/infer")
        .expect("new inference reached updated fixture");
    assert_eq!(reset_request.state_before.as_deref(), Some("0"));

    let independent = harness
        .gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "fixture.capability-search")
        .expect("independent Vendor remains installed");
    assert_eq!(independent.status, "ready");

    configure_external_search(&harness.gateway, &other_route.model_id).await?;
    other_upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Independent Vendor survived [sc:fixture-source]"),
        )
        .await;
    let independent_result = completed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "independent Vendor query",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(
        independent_result
            .report
            .answer
            .contains("Independent Vendor survived")
    );
    assert_eq!(other_upstream.call_count("/search").await, 1);
    Ok(())
}

#[tokio::test]
async fn newer_builtin_removes_bound_search_and_media_without_confirmation_or_rebinding()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let first = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    })
    .await?;
    let bundled = first
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "base")
        .expect("bundled base plugin");
    let previous = install_fixture(&first, "capability-base-older.wasm", false).await?;
    assert_eq!(previous.source, PluginSource::Local);
    assert_eq!(previous.version, "0.0.0");

    let upstream = LocalUpstream::start().await?;
    let provider = first
        .admin()
        .create_provider(CreateProvider {
            name: Some("Previous builtin multi-capability connection".into()),
            source: ProviderSourceInput::Custom {
                vendor: "openai".into(),
                channel: "default".into(),
                protocol: None,
                base_url: upstream.url.clone(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::Fields {
                values: BTreeMap::from([(
                    "api_key".into(),
                    json!("previous-builtin-secret-not-production"),
                )]),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    let search_route = create_route(
        &first,
        "previous-builtin-search",
        vec![(&provider, "research-model", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let image_route = create_route(
        &first,
        "previous-builtin-image",
        vec![(&provider, "fixture-image", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    configure_external_search(&first, &search_route.model_id).await?;
    configure_image_generation(&first, &image_route.model_id).await?;
    let key = first
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "Previous builtin capability owner".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![image_route.id.clone().into()],
            inject_media_understanding: false,
            spend_limit: None,
        })
        .await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let first_endpoint = format!("http://{}/mcp", listener.local_addr()?);
    first
        .admin()
        .set_setting(
            "artifact_settings",
            &json!({"client_base_url":first_endpoint.trim_end_matches("/mcp")}).to_string(),
        )
        .await?;
    let first_router = stravia_core::proxy::server::create_router(first.clone());
    let first_server = tokio::spawn(async move {
        axum::serve(listener, first_router)
            .await
            .expect("serve previous builtin Gateway");
    });

    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Previous builtin Search works [sc:fixture-source]"),
        )
        .await;
    let prior_search = completed_search(
        terminal_search(
            &first,
            Principal::new(key.id.clone()),
            "bound before bundled update",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(
        prior_search
            .report
            .answer
            .contains("Previous builtin Search works")
    );
    upstream
        .push_json("/image", StatusCode::OK, image_response())
        .await;
    let first_client = connect_mcp(&first_endpoint, &key.token).await;
    let prior_image = call_generate(
        &first_client,
        json!({"type":"image","input":{"prompt":"bound before bundled update"}}),
    )
    .await;
    assert_ne!(prior_image.is_error, Some(true), "{prior_image:?}");
    drop(first_client);
    first_server.abort();
    let _ = first_server.await;
    drop(first);
    mark_base_as_previous_builtin(directory.path()).await?;

    let restarted = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    })
    .await?;
    let updated = restarted
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "base")
        .expect("automatically updated base plugin");
    assert_eq!(updated.source, PluginSource::Builtin);
    assert_eq!(updated.version, bundled.version);
    assert_eq!(updated.status, "ready");
    assert!(updated.pending_update.is_none());
    assert!(updated.affected_bindings.iter().any(|impact| {
        impact.route_id == search_route.model_id.as_str() && impact.capability == "search"
    }));
    assert!(updated.affected_bindings.iter().any(|impact| {
        impact.route_id == image_route.model_id.as_str() && impact.capability == "media_image"
    }));

    let retained_provider = restarted.admin().get_provider(&provider.id).await?;
    assert_eq!(retained_provider.id, provider.id);
    for route in [&search_route, &image_route] {
        let retained = restarted.admin().get_model(&route.model_id).await?;
        assert_eq!(retained.targets.len(), 1);
        assert_eq!(
            retained.targets[0].provider_id().as_str(),
            provider.id.as_str()
        );
    }
    let search_config = restarted.admin().get_web_search_config().await?;
    assert!(matches!(
        &search_config.backend,
        Some(WebSearchBackendDraft::External { route_id: Some(id) })
            if id.as_str() == search_route.model_id.as_str()
    ));
    let media_config = restarted.admin().get_media_generation_config().await?;
    assert_eq!(
        media_config.config.image.route_id.as_deref(),
        Some(image_route.model_id.as_str())
    );
    assert!(!media_config.validation.valid);
    assert_eq!(
        media_config.validation.code,
        Some("media_generation_target_incompatible")
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let restarted_endpoint = format!("http://{}/mcp", listener.local_addr()?);
    restarted
        .admin()
        .set_setting(
            "artifact_settings",
            &json!({"client_base_url":restarted_endpoint.trim_end_matches("/mcp")}).to_string(),
        )
        .await?;
    let restarted_router = stravia_core::proxy::server::create_router(restarted.clone());
    let restarted_server = tokio::spawn(async move {
        axum::serve(listener, restarted_router)
            .await
            .expect("serve updated builtin Gateway");
    });

    let search_calls = upstream.call_count("/search").await;
    failed_search(
        terminal_search(
            &restarted,
            Principal::new(key.id.clone()),
            "removed builtin Search capability",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(upstream.call_count("/search").await, search_calls);
    let image_calls = upstream.call_count("/image").await;
    let restarted_client = connect_mcp(&restarted_endpoint, &key.token).await;
    let rejected_image = call_generate(
        &restarted_client,
        json!({"type":"image","input":{"prompt":"removed builtin image capability"}}),
    )
    .await;
    assert_eq!(rejected_image.is_error, Some(true));
    assert!(
        rejected_image
            .structured_content
            .as_ref()
            .is_none_or(|value| value.get("path").is_none())
    );
    assert_eq!(upstream.call_count("/image").await, image_calls);
    drop(restarted_client);
    restarted_server.abort();
    let _ = restarted_server.await;
    Ok(())
}

#[tokio::test]
async fn dedicated_profile_wholly_replaces_base_and_never_falls_back_when_its_artifact_is_missing()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    })
    .await?;
    let base_deepseek = gateway.admin().vendor_metadata("deepseek")?;
    assert!(base_deepseek.capabilities.contains(&Capability::Infer));
    assert!(
        base_deepseek
            .channels
            .iter()
            .any(|channel| channel.id == "default")
    );

    let upstream = LocalUpstream::start().await?;
    let preexisting = create_provider(
        &gateway,
        "DeepSeek connection created from base",
        "deepseek",
        &upstream.url,
        "openai-compatible",
    )
    .await?;
    let inference_route = create_route(
        &gateway,
        "deepseek-base-inference",
        vec![(&preexisting, "deepseek-v4", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let key = gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "Dedicated takeover owner".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![inference_route.id.clone().into()],
            spend_limit: None,
        })
        .await?;

    let dedicated = install_fixture(&gateway, "capability-dedicated-deepseek.wasm", false).await?;
    assert_eq!(dedicated.vendor_id, "deepseek");
    let effective = gateway.admin().vendor_metadata("deepseek")?;
    assert_eq!(effective.catalog_id.as_deref(), Some("deepseek"));
    assert_eq!(
        effective.capabilities,
        std::collections::BTreeSet::from([Capability::Search])
    );
    assert_eq!(
        effective
            .channels
            .iter()
            .map(|channel| channel.id.as_str())
            .collect::<Vec<_>>(),
        ["dedicated"]
    );

    let dedicated_provider = gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "Dedicated DeepSeek search".into(),
            vendor: Some("deepseek".into()),
            protocol: "fixture-deepseek".into(),
            base_url: upstream.url.clone(),
            preset_key: Some("deepseek".into()),
            channel: Some("dedicated".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await?;
    let search_route = create_route(
        &gateway,
        "deepseek-dedicated-search",
        vec![(&dedicated_provider, "research-model", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    configure_external_search(&gateway, &search_route.model_id).await?;
    upstream
        .push_json(
            "/search",
            StatusCode::OK,
            search_response("Dedicated profile answered [sc:fixture-source]"),
        )
        .await;
    let searched = completed_search(
        terminal_search(
            &gateway,
            Principal::new(key.id.clone()),
            "prove dedicated auxiliary capability",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(
        searched
            .report
            .answer
            .contains("Dedicated profile answered")
    );

    let response = stravia_core::proxy::server::create_router(gateway.clone())
        .oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {}", key.token))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":inference_route.model_id,"input":"must not fall back to base"})
                        .to_string(),
                ))?,
        )
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        upstream.requests().await.len(),
        1,
        "missing dedicated inference/channel capability must fail before any base wire request"
    );

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(DataPaths::new(directory.path()).database()),
        )
        .await?;
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('vendor_plugins')")
            .fetch_all(&pool)
            .await?;
    assert!(!columns.iter().any(|column| column == "component"));
    let digest: String =
        sqlx::query_scalar("SELECT digest FROM vendor_plugins WHERE vendor_id = 'deepseek'")
            .fetch_one(&pool)
            .await?;
    pool.close().await;
    let artifact = DataPaths::new(directory.path())
        .plugins()
        .join("artifacts")
        .join(format!("{digest}.wasm"));
    assert!(
        artifact.is_file(),
        "installed SQL metadata must reference a real artifact file"
    );
    drop(gateway);
    std::fs::remove_file(&artifact)?;

    let restarted = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    })
    .await?;
    let unavailable = restarted
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "deepseek")
        .expect("dedicated record remains authoritative");
    assert_eq!(unavailable.status, "unavailable");
    assert!(restarted.admin().vendor_metadata("deepseek").is_err());
    let unaffected = restarted.admin().vendor_metadata("openai")?;
    assert!(unaffected.capabilities.contains(&Capability::Infer));
    assert!(
        unaffected
            .channels
            .iter()
            .any(|channel| channel.id == "default")
    );
    let openai = restarted
        .admin()
        .create_provider(CreateProvider {
            name: Some("Unaffected base OpenAI".into()),
            source: ProviderSourceInput::Custom {
                vendor: "openai".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: upstream.url.clone(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "unaffected-base-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    let openai_route = create_route(
        &restarted,
        "unaffected-base-inference",
        vec![(&openai, "gpt-test", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let openai_key = restarted
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "Unaffected base inference owner".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![openai_route.id.clone().into()],
            spend_limit: None,
        })
        .await?;
    upstream
        .push_json(
            "/v1/responses",
            StatusCode::OK,
            stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                "unaffected-base-response",
                "gpt-test",
                "completed",
                vec![json!({
                    "id":"unaffected-base-message",
                    "type":"message",
                    "role":"assistant",
                    "status":"completed",
                    "content":[{"type":"output_text","text":"base remains usable","annotations":[]}]
                })],
                Value::Null,
                Value::Null,
                json!({
                    "input_tokens":1,"output_tokens":1,"total_tokens":2,
                    "input_tokens_details":{"cached_tokens":0},
                    "output_tokens_details":{"reasoning_tokens":0}
                }),
            ),
        )
        .await;
    let response = stravia_core::proxy::server::create_router(restarted)
        .oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {}", openai_key.token))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":openai_route.model_id,"input":"prove base is unaffected"})
                        .to_string(),
                ))?,
        )
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("base remains usable"));
    Ok(())
}

async fn only_deepseek_card(
    gateway: &Gateway,
) -> anyhow::Result<stravia_core::plugin::ProviderMetadata> {
    let mut cards: Vec<_> = gateway
        .admin()
        .list_selectable_vendor_metadata()
        .await?
        .into_iter()
        .filter(|card| card.descriptor.provider_id == "deepseek")
        .collect();
    anyhow::ensure!(
        cards.len() == 1,
        "expected exactly one deepseek card, got {}",
        cards.len()
    );
    Ok(cards.remove(0))
}

#[tokio::test]
async fn management_metadata_names_the_loaded_plugin_and_returns_to_base_without_touching_connections()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    })
    .await?;
    let saved = create_provider(
        &gateway,
        "DeepSeek connection created from base",
        "deepseek",
        "http://127.0.0.1:1",
        "openai-compatible",
    )
    .await?;
    let base_card = only_deepseek_card(&gateway).await?;
    assert_eq!(base_card.plugin.id, "base");
    assert_eq!(base_card.plugin.name, "Stravia Base Providers");
    assert_eq!(
        base_card.descriptor,
        gateway.admin().vendor_metadata("deepseek")?
    );

    install_fixture(&gateway, "capability-dedicated-deepseek.wasm", false).await?;
    let dedicated_card = only_deepseek_card(&gateway).await?;
    assert_eq!(dedicated_card.plugin.id, "deepseek");
    assert_eq!(
        dedicated_card.plugin.name,
        "Capability Contract (dedicated-deepseek)"
    );
    assert_eq!(dedicated_card.descriptor.provider_id, "deepseek");
    assert_eq!(
        dedicated_card.descriptor.catalog_id.as_deref(),
        Some("deepseek")
    );
    assert_eq!(
        dedicated_card
            .descriptor
            .channels
            .iter()
            .map(|channel| channel.id.as_str())
            .collect::<Vec<_>>(),
        ["dedicated"],
        "a channel that no base channel matches keeps its own card and name"
    );
    assert_eq!(
        dedicated_card.descriptor,
        gateway.admin().vendor_metadata("deepseek")?,
        "runtime resolution reports the raw dedicated profile"
    );

    gateway.admin().uninstall_vendor_plugin("deepseek").await?;
    let restored_card = only_deepseek_card(&gateway).await?;
    assert_eq!(restored_card, base_card);
    assert_eq!(
        restored_card.descriptor,
        gateway.admin().vendor_metadata("deepseek")?
    );

    let retained = gateway.admin().get_provider(&saved.id).await?;
    assert_eq!(retained.vendor.as_deref(), Some("deepseek"));
    assert_eq!(retained.channel.as_deref(), Some("default"));
    assert_eq!(retained.protocol, "openai-compatible");
    Ok(())
}

#[tokio::test]
async fn manually_installed_codex_component_reuses_one_connection_for_search_and_image()
-> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_distributed_vendor_plugin(&harness.gateway, "openai-codex").await?;
    let descriptor = harness
        .gateway
        .admin()
        .list_vendor_metadata()
        .await?
        .into_iter()
        .map(|card| card.descriptor)
        .find(|descriptor| descriptor.provider_id == "openai-codex")
        .expect("manually installed OpenAI Codex provider profile");
    let codex = descriptor
        .channels
        .iter()
        .find(|channel| channel.id == "codex")
        .expect("Codex channel");
    assert!(codex.capabilities.contains(&Capability::Infer));
    assert!(codex.capabilities.contains(&Capability::Search));
    assert!(codex.capabilities.contains(&Capability::MediaImage));

    let upstream = LocalUpstream::start().await?;
    let provider = harness
        .gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "Local Codex component".into(),
            vendor: Some("openai-codex".into()),
            protocol: "open-responses".into(),
            base_url: upstream.url.clone(),
            preset_key: Some("openai".into()),
            channel: Some("codex".into()),
            models_source: None,
            static_models: Some("gpt-5.4".into()),
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
        })
        .await?;
    harness
        .gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "controlled-codex-token-not-production".into(),
                resource_url: Some(upstream.url.clone()),
                ..UpsertOAuthCredential::default()
            },
        )
        .await?;
    // Static IDs do not prove hosted image support; declare the fixture's
    // capabilities explicitly instead of relying on model-name inference.
    harness
        .gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "gpt-5.4",
            CreateManualProviderModel {
                metadata: json!({
                    "id": "gpt-5.4",
                    "name": "GPT-5.4",
                    "capabilities": ["infer", "search", "media_image"],
                    "modalities": {"input": ["text", "image"], "output": ["text", "image"]},
                    "limit": {"context": 272000, "output": 128000},
                    "cost": {"input": 2.5, "output": 15.0}
                }),
                template_id: None,
            },
        )
        .await?;
    let route = create_route(
        &harness.gateway,
        "codex-multi-capability",
        vec![(&provider, "gpt-5.4", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    let image_route = create_route(
        &harness.gateway,
        "codex-image-capability",
        vec![(&provider, "gpt-5.4", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Image,
    )
    .await?;
    configure_external_search(&harness.gateway, &route.model_id).await?;
    configure_image_generation(&harness.gateway, &image_route.model_id).await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[route.id.clone()]}))?,
        )
        .await?;

    upstream
        .push_responses_stream(
            stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                "resp_infer_fixture", "gpt-5.4", "completed",
                vec![json!({
                    "id":"msg_infer_fixture","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"Codex inference completed","annotations":[]}]
                })],
                Value::Null,
                Value::Null,
                json!({
                    "input_tokens":2,"input_tokens_details":{"cached_tokens":0},
                    "output_tokens":3,"output_tokens_details":{"reasoning_tokens":0},
                    "total_tokens":5
                }),
            ),
        )
        .await;
    let turn_context = json!({
        "installation_id":"spoofed-installation",
        "session_id":"spoofed-session",
        "thread_id":"spoofed-thread",
        "window_id":"spoofed-window",
        "turn_id":"spoofed-turn",
        "custom_hint":"native-consumer-context"
    })
    .to_string();
    let declared_headers = [
        ("X-CODEX-BETA-FEATURES", "native-beta"),
        ("X-CODEX-INSTALLATION-ID", "client-installation"),
        ("X-CODEX-TURN-METADATA", turn_context.as_str()),
        ("X-CODEX-TURN-STATE", "native-turn-state"),
        ("X-CODEX-WINDOW-ID", "raw-window"),
    ];
    let mut infer_request = reqwest::Client::new()
        .post(harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses")
        .bearer_auth(&harness.token)
        .header("cookie", "client-session=never-upstream")
        .header("idempotency-key", "public-client-request")
        .header("x-session-id", "shared-client-session")
        .header("session_id", "lower-priority-client-session")
        .header("x-client-request-id", "independent-request-correlation")
        .header("x-undeclared-plugin-context", "never-upstream");
    for (name, value) in &declared_headers {
        infer_request = infer_request.header(*name, *value);
    }
    let inferred = infer_request
        .json(&json!({
            "model":route.model_id,
            "input":"Codex inference through Wasm",
            "temperature":0.2,
            "top_p":0.9,
            "max_output_tokens":256,
            "parallel_tool_calls":false,
            "metadata":{"verification":"canonical-final"},
            "prompt_cache_key":"canonical-final-smoke",
            "prompt_cache_retention":"24h",
            "store":false
        }))
        .send()
        .await?;
    let infer_status = inferred.status();
    let infer_body: Value = inferred.json().await?;
    assert!(infer_status.is_success(), "{infer_body}");
    assert!(
        infer_body["output"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["content"].as_array().is_some_and(|content| {
                    content
                        .iter()
                        .any(|part| part["text"] == "Codex inference completed")
                })
            }))
    );

    upstream
        .push_responses_stream(
            stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                "resp_child_fixture", "gpt-5.4", "completed",
                vec![json!({
                    "id":"msg_child_fixture","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"Follow-up retained full history","annotations":[]}]
                })],
                Value::Null,
                Value::Null,
                json!({
                    "input_tokens":8,"input_tokens_details":{"cached_tokens":0},
                    "output_tokens":4,"output_tokens_details":{"reasoning_tokens":0},
                    "total_tokens":12
                }),
            ),
        )
        .await;
    let child = reqwest::Client::new()
        .post(harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses")
        .bearer_auth(&harness.token)
        .header("x-session-id", "shared-client-session")
        .header("x-client-request-id", "child-request-correlation")
        .json(&json!({
            "model":route.model_id,
            "previous_response_id":infer_body["id"],
            "input":"Continue the prior Codex exchange",
            "store":false
        }))
        .send()
        .await?;
    let child_status = child.status();
    let child_body: Value = child.json().await?;
    assert!(child_status.is_success(), "{child_body}");
    assert!(child_body["output"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|part| part["text"] == "Follow-up retained full history")
            })
        })
    }));

    upstream
        .push_responses_stream(json!({
            "id":"resp_search_fixture",
            "status":"completed",
            "output":[{
                "type":"message",
                "role":"assistant",
                "content":[{
                    "type":"output_text",
                    "text":"Verified",
                    "annotations":[{
                        "type":"url_citation",
                        "start_index":0,
                        "end_index":8,
                        "url":SOURCE_URL,
                        "title":"Verified source"
                    }]
                }]
            }, {"type":"web_search_call","id":"search_fixture","status":"completed"}],
            "usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8}
        }))
        .await;
    let searched = completed_search(
        terminal_search(
            &harness.gateway,
            Principal::new(harness.key_id.clone()),
            "Codex hosted search through Wasm",
            None,
            CancellationToken::new(),
        )
        .await,
    );
    assert!(searched.report.answer.contains("Verified"));
    assert_eq!(searched.report.sources[0].url, SOURCE_URL);

    upstream
        .push_responses_stream(json!({
            "id":"resp_image_fixture",
            "status":"completed",
            "output":[{
                "type":"image_generation_call",
                "id":"image_fixture",
                "status":"completed",
                "result":PNG_BASE64,
                "output_format":"png"
            }],
            "usage":{"input_tokens":4,"output_tokens":6,"total_tokens":10}
        }))
        .await;
    let client = harness.connect().await;
    let generated = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"Codex hosted image"}}),
    )
    .await;
    assert_ne!(generated.is_error, Some(true), "{generated:?}");
    let image = generated.structured_content.expect("Codex image result");
    assert_eq!(image["mime_type"], "image/png");
    assert_eq!(image["media"], json!({"width":1,"height":1}));
    let reference = image["path"]
        .as_str()
        .expect("Codex output Artifact Reference")
        .to_owned();

    upstream
        .push_responses_stream(json!({
            "id":"resp_edit_fixture",
            "status":"completed",
            "output":[{
                "type":"image_generation_call",
                "id":"image_edit_fixture",
                "status":"completed",
                "result":PNG_BASE64,
                "output_format":"png"
            }],
            "usage":{"input_tokens":7,"output_tokens":6,"total_tokens":13}
        }))
        .await;
    let edited = call_generate(
        &client,
        json!({
            "type":"image",
            "input":{"prompt":"Edit the owned Codex image","reference_images":[reference]}
        }),
    )
    .await;
    assert_ne!(edited.is_error, Some(true), "{edited:?}");
    assert_eq!(
        edited.structured_content.expect("Codex edited image")["mime_type"],
        "image/png"
    );

    let (_, other_token) = harness.create_key("Isolated Codex consumer").await?;
    upstream
        .push_responses_stream(
            stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                "resp_isolated_fixture", "gpt-5.4", "completed",
                vec![json!({
                    "id":"msg_isolated_fixture","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"Isolated consumer completed","annotations":[]}]
                })],
                Value::Null,
                Value::Null,
                json!({
                    "input_tokens":2,"input_tokens_details":{"cached_tokens":0},
                    "output_tokens":3,"output_tokens_details":{"reasoning_tokens":0},
                    "total_tokens":5
                }),
            ),
        )
        .await;
    let isolated = reqwest::Client::new()
        .post(harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses")
        .bearer_auth(&other_token)
        .header("x-session-id", "shared-client-session")
        .header("session_id", "lower-priority-client-session")
        .header("x-client-request-id", "other-request-correlation")
        .json(&json!({"model":route.model_id,"input":"Isolated Codex consumer"}))
        .send()
        .await?;
    let isolated_status = isolated.status();
    let isolated_body: Value = isolated.json().await?;
    assert!(isolated_status.is_success(), "{isolated_body}");
    assert!(isolated_body["output"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|part| part["text"] == "Isolated consumer completed")
            })
        })
    }));

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().all(|request| request.path == "/responses"));
    for request in &requests {
        for (header, key) in [
            ("session-id", "session_id"),
            ("thread-id", "thread_id"),
            ("x-codex-window-id", "x-codex-window-id"),
            ("x-codex-turn-metadata", "x-codex-turn-metadata"),
        ] {
            let identity = request.headers[header].to_str()?;
            assert_eq!(request.body["client_metadata"][key], identity);
            assert!(
                ![
                    "shared-client-session",
                    "lower-priority-client-session",
                    "spoofed-session",
                    "spoofed-thread",
                    "raw-window",
                    "spoofed-window"
                ]
                .contains(&identity)
            );
        }
        let turn: Value = serde_json::from_str(request.headers["x-codex-turn-metadata"].to_str()?)?;
        assert_eq!(
            turn["session_id"],
            request.body["client_metadata"]["session_id"]
        );
        assert_eq!(
            turn["thread_id"],
            request.body["client_metadata"]["thread_id"]
        );
        assert_eq!(
            turn["window_id"],
            request.body["client_metadata"]["x-codex-window-id"]
        );
        assert_eq!(turn["turn_id"], request.body["client_metadata"]["turn_id"]);
        assert_ne!(turn["turn_id"], "spoofed-turn");
        assert!(!request.headers.contains_key("session_id"));
        assert!(!request.headers.contains_key("x-session-id"));
        assert!(!request.headers.contains_key("idempotency-key"));
    }
    let infer_turn: Value =
        serde_json::from_str(requests[0].headers["x-codex-turn-metadata"].to_str()?)?;
    let installation_id = requests[0].headers["x-codex-installation-id"].to_str()?;
    uuid::Uuid::parse_str(installation_id)?;
    assert_ne!(installation_id, "client-installation");
    assert_eq!(infer_turn["installation_id"], installation_id);
    assert_eq!(
        requests[0].body["client_metadata"]["x-codex-installation-id"],
        requests[0].headers["x-codex-installation-id"].to_str()?
    );
    assert!(infer_turn.get("custom_hint").is_none());
    for request in &requests {
        assert_eq!(
            request.headers["x-codex-installation-id"].to_str()?,
            installation_id
        );
        assert!(!request.headers.contains_key("x-codex-beta-features"));
        assert_ne!(
            request
                .headers
                .get("x-codex-turn-state")
                .and_then(|value| value.to_str().ok()),
            Some("native-turn-state")
        );
    }
    for header in ["session-id", "thread-id", "x-codex-window-id"] {
        assert_eq!(
            requests[0].headers[header], requests[1].headers[header],
            "HTTP continuation retains the same principal-scoped session"
        );
        assert_ne!(requests[0].headers[header], requests[5].headers[header]);
    }
    for (index, request) in requests.iter().enumerate() {
        for earlier in &requests[..index] {
            assert_ne!(
                request.body["client_metadata"]["turn_id"],
                earlier.body["client_metadata"]["turn_id"]
            );
        }
    }
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer controlled-codex-token-not-production"),
    );
    assert!(!requests[0].headers.contains_key("cookie"));
    assert!(
        !requests[0]
            .headers
            .contains_key("x-undeclared-plugin-context")
    );
    assert_eq!(requests[0].body["model"], "gpt-5.4");
    for unsupported in [
        "metadata",
        "prompt_cache_retention",
        "temperature",
        "top_p",
        "max_output_tokens",
    ] {
        assert!(
            requests[0].body.get(unsupported).is_none(),
            "unsupported native control: {unsupported}"
        );
    }
    assert_eq!(requests[0].body["parallel_tool_calls"], false);
    assert_eq!(
        requests[0].body["prompt_cache_key"],
        "canonical-final-smoke"
    );
    assert!(requests[1].body.get("previous_response_id").is_none());
    let child_history = requests[1].body["input"].as_array().expect("full history");
    for text in [
        "Codex inference through Wasm",
        "Codex inference completed",
        "Continue the prior Codex exchange",
    ] {
        assert!(
            child_history.iter().any(|item| {
                item["content"]
                    .as_array()
                    .is_some_and(|content| content.iter().any(|part| part["text"] == text))
            }),
            "missing HTTP continuation history: {text}"
        );
    }
    assert_eq!(requests[2].body["model"], "gpt-5.4");
    assert!(
        requests[2].body["tools"]
            .as_array()
            .is_some_and(|tools| { tools.iter().any(|tool| tool["type"] == "web_search") })
    );
    assert_eq!(requests[3].body["model"], "gpt-5.4");
    assert!(requests[3].body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["type"] == "image_generation" && tool["action"] == "generate")
    }));
    assert_eq!(requests[4].body["model"], "gpt-5.4");
    assert!(requests[4].body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["type"] == "image_generation" && tool["action"] == "edit")
    }));
    let edit_images: Vec<_> = requests[4].body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|content| content["image_url"].as_str())
        .collect();
    assert_eq!(edit_images, [format!("data:image/png;base64,{PNG_BASE64}")]);
    Ok(())
}

struct CodexContinuationUpstream {
    url: String,
    state: Arc<CodexContinuationState>,
    task: tokio::task::JoinHandle<()>,
}

struct CodexContinuationState {
    handshakes: Mutex<Vec<HeaderMap>>,
    frames: Mutex<Vec<(usize, Value)>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Drop for CodexContinuationUpstream {
    fn drop(&mut self) {
        self.state.shutdown.send_replace(true);
        self.task.abort();
    }
}

impl CodexContinuationUpstream {
    async fn start() -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let state = Arc::new(CodexContinuationState {
            handshakes: Mutex::new(Vec::new()),
            frames: Mutex::new(Vec::new()),
            shutdown,
        });
        let app = Router::new()
            .route(
                "/responses",
                axum::routing::get(codex_continuation_websocket),
            )
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve local Codex continuation upstream");
        });
        Ok(Self {
            url: format!("http://{address}"),
            state,
            task,
        })
    }
}

async fn codex_continuation_websocket(
    State(state): State<Arc<CodexContinuationState>>,
    headers: HeaderMap,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> impl IntoResponse {
    let socket = {
        let mut handshakes = state.handshakes.lock().await;
        let socket = handshakes.len();
        handshakes.push(headers);
        socket
    };
    let mut shutdown = state.shutdown.subscribe();
    upgrade.on_upgrade(move |mut websocket| async move {
        loop {
            let message = tokio::select! {
                _ = shutdown.changed() => break,
                message = websocket.recv() => message,
            };
            let Some(Ok(axum::extract::ws::Message::Text(text))) = message else {
                break;
            };
            let frame: Value = serde_json::from_str(&text).expect("Codex response.create JSON");
            let ordinal = {
                let mut frames = state.frames.lock().await;
                let ordinal = frames.len();
                frames.push((socket, frame));
                ordinal
            };
            // Accept full replays too: the regression must detect lost reuse even when
            // the host's fallback still produces a successful public response.
            let output = if ordinal < 2 {
                json!({
                    "id":format!("fc_codex_{ordinal}"), "type":"function_call",
                    "status":"completed", "call_id":format!("call_codex_{ordinal}"),
                    "name":"lookup", "arguments":format!("{{\"step\":{ordinal}}}")
                })
            } else {
                json!({
                    "id":"msg_codex_final", "type":"message", "role":"assistant",
                    "status":"completed", "content":[{
                        "type":"output_text", "text":"Both tool results received", "annotations":[]
                    }]
                })
            };
            let completed =
                stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                    &format!("resp_codex_{ordinal}"), "gpt-5.4", "completed",
                    vec![output.clone()], Value::Null, Value::Null,
                    json!({"input_tokens":2,"output_tokens":3,"total_tokens":5,
                        "input_tokens_details":{"cached_tokens":0},
                        "output_tokens_details":{"reasoning_tokens":0}}),
                );
            let mut created = completed.clone();
            created["status"] = json!("in_progress");
            created["output"] = json!([]);
            created["usage"] = Value::Null;
            let mut added = output.clone();
            added["status"] = json!("in_progress");
            let mut events = vec![json!({"type":"response.created","response":created})];
            if output["type"] == "function_call" {
                added["arguments"] = json!("");
                events.push(json!({"type":"response.output_item.added","output_index":0,"item":added}));
                events.push(json!({"type":"response.function_call_arguments.delta",
                    "output_index":0,"item_id":output["id"],"delta":output["arguments"]}));
                events.push(json!({"type":"response.function_call_arguments.done",
                    "output_index":0,"item_id":output["id"],"arguments":output["arguments"]}));
            } else {
                added["content"] = json!([]);
                events.push(json!({"type":"response.output_item.added","output_index":0,"item":added}));
                let part = &output["content"][0];
                let mut empty_part = part.clone();
                empty_part["text"] = json!("");
                events.push(json!({"type":"response.content_part.added","output_index":0,
                    "content_index":0,"item_id":output["id"],"part":empty_part}));
                events.push(json!({"type":"response.output_text.delta","output_index":0,
                    "content_index":0,"item_id":output["id"],"delta":part["text"]}));
                events.push(json!({"type":"response.output_text.done","output_index":0,
                    "content_index":0,"item_id":output["id"],"text":part["text"]}));
                events.push(json!({"type":"response.content_part.done","output_index":0,
                    "content_index":0,"item_id":output["id"],"part":part}));
            }
            events.push(json!({"type":"response.output_item.done","output_index":0,"item":output}));
            events.push(json!({"type":"response.completed","response":completed}));
            for (sequence, mut event) in events.into_iter().enumerate() {
                event["sequence_number"] = sequence.into();
                if websocket.send(axum::extract::ws::Message::Text(event.to_string().into())).await.is_err() {
                    return;
                }
            }
        }
    })
}

#[tokio::test]
async fn manually_installed_codex_component_continues_client_tool_results_on_one_websocket()
-> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    install_distributed_vendor_plugin(&harness.gateway, "openai-codex").await?;
    let upstream = CodexContinuationUpstream::start().await?;
    let provider = harness.gateway.storage.providers().create(CreateProviderRecord {
        name: "Local Codex tool continuation".into(),
        vendor: Some("openai-codex".into()),
        protocol: "open-responses".into(),
        base_url: upstream.url.clone(),
        preset_key: Some("openai".into()),
        channel: Some("codex".into()),
        models_source: None,
        static_models: Some("gpt-5.4".into()),
        api_key: String::new(),
        adapter_credentials: "{}".into(),
        vendor_options: json!({
            "websocket_url":format!("{}/responses", upstream.url.replacen("http://", "ws://", 1))
        }).to_string(),
        auth_mode: "oauth".into(),
        use_proxy: false,
    }).await?;
    harness
        .gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "controlled-codex-continuation-token".into(),
                resource_url: Some(upstream.url.clone()),
                ..UpsertOAuthCredential::default()
            },
        )
        .await?;
    seed_provider_scope(
        &harness.gateway.config.data_dir,
        "openai",
        br#"{
        "gpt-5.4": {
            "id":"gpt-5.4", "name":"GPT-5.4", "tool_call":true,
            "modalities":{"input":["text"],"output":["text"]},
            "limit":{"context":272000,"output":128000},
            "cost":{"input":2.5,"output":15.0}
        }
    }"#,
    )?;
    harness
        .gateway
        .admin()
        .sync_provider_models(&provider.id)
        .await?;
    let route = create_route(
        &harness.gateway,
        "codex-client-tool-continuation",
        vec![(&provider, "gpt-5.4", 10)],
        0,
        stravia_core::db::models::RouteOutputModality::Text,
    )
    .await?;
    harness
        .gateway
        .admin()
        .update_api_key(
            &harness.key_id,
            serde_json::from_value(json!({"model_ids":[route.id.clone()]}))?,
        )
        .await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut history = vec![json!({"role":"user","content":"Look up both steps, then summarize."})];
    let mut tool_results = Vec::new();
    for turn in 0..3 {
        let response = client
            .post(harness.endpoint.trim_end_matches("/mcp").to_owned() + "/v1/responses")
            .bearer_auth(&harness.token)
            .header("x-session-id", "codex-client-tool-session")
            .header("x-client-request-id", format!("codex-tool-request-{turn}"))
            .header(
                "x-codex-turn-metadata",
                json!({"turn_id":format!("client-turn-{turn}")}).to_string(),
            )
            .json(&json!({
                "model":route.model_id, "store":false,
                "prompt_cache_key":"codex-client-tool-session", "input":history,
                "tools":[{"type":"function","name":"lookup","description":"Look up one step",
                    "parameters":{"type":"object","properties":{"step":{"type":"integer"}},
                        "required":["step"],"additionalProperties":false},"strict":true}]
            }))
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        assert_eq!(status, StatusCode::OK, "turn {turn}: {body}");
        let output = body["output"].as_array().expect("Responses output");
        if turn < 2 {
            let call = output
                .iter()
                .find(|item| item["type"] == "function_call")
                .expect("client tool call");
            assert_eq!(call["name"], "lookup");
            assert_eq!(call["call_id"], format!("call_codex_{turn}"));
            assert_eq!(
                serde_json::from_str::<Value>(call["arguments"].as_str().expect("tool arguments"))?,
                json!({"step":turn})
            );
            history.extend(output.iter().cloned());
            let result = json!({"type":"function_call_output","call_id":call["call_id"],
                "output":format!("step {turn} result")});
            history.push(result.clone());
            tool_results.push(result);
        } else {
            assert_eq!(
                output[0]["content"][0]["text"],
                "Both tool results received"
            );
        }
    }

    let frames = upstream.state.frames.lock().await;
    let handshakes = upstream.state.handshakes.lock().await;
    assert_eq!(
        frames.len(),
        3,
        "one response.create per public turn: {frames:?}"
    );
    assert_eq!(
        handshakes.len(),
        1,
        "all client tool turns reuse one socket: {handshakes:?}"
    );
    assert_eq!(
        handshakes[0]["authorization"],
        "Bearer controlled-codex-continuation-token"
    );
    assert!(!handshakes[0].contains_key("x-codex-turn-metadata"));
    assert!(!handshakes[0].contains_key("x-codex-turn-state"));
    for (turn, (socket, frame)) in frames.iter().enumerate() {
        assert_eq!(*socket, 0);
        assert_eq!(frame["type"], "response.create");
        let metadata = &frame["client_metadata"];
        for (header, key) in [("session-id", "session_id"), ("thread-id", "thread_id")] {
            assert_eq!(metadata[key], handshakes[0][header].to_str()?);
        }
        let context: Value = serde_json::from_str(
            metadata["x-codex-turn-metadata"]
                .as_str()
                .expect("per-frame Codex turn context"),
        )?;
        assert!(
            metadata["turn_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(context["turn_id"], metadata["turn_id"]);
        assert_eq!(
            metadata["session_id"],
            frames[0].1["client_metadata"]["session_id"]
        );
        for (_, earlier) in &frames[..turn] {
            assert_ne!(
                metadata["turn_id"], earlier["client_metadata"]["turn_id"],
                "turn metadata must not freeze at the socket handshake"
            );
        }
        if turn == 0 {
            assert!(frame.get("previous_response_id").is_none_or(Value::is_null));
        } else {
            assert_eq!(
                frame["previous_response_id"],
                format!("resp_codex_{}", turn - 1)
            );
            assert_eq!(
                frame["input"],
                json!([tool_results[turn - 1]]),
                "only the new client tool result is sent, never full history"
            );
        }
    }
    Ok(())
}
