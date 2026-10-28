use super::*;
use crate::db::models::{CreateProviderRecord, CreateRoute, CreateTarget, UpsertOAuthCredential};
use crate::media_generation::{ImageGenerationConfig, MediaGenerationConfig};
use axum::response::IntoResponse;
use std::collections::VecDeque;
use std::time::Duration;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn response_resource(status: &str, output: Value) -> Value {
    json!({
        "id":"resp_generation","object":"response","created_at":1789862400,
        "completed_at":null,"status":status,"incomplete_details":null,"model":"gpt-5.4",
        "previous_response_id":null,"instructions":null,"output":output,"error":null,
        "tools":[],"tool_choice":"auto","truncation":"disabled","parallel_tool_calls":true,
        "text":{"format":{"type":"text"}},"top_p":1.0,"presence_penalty":0.0,"frequency_penalty":0.0,
        "top_logprobs":0,"temperature":1.0,"reasoning":null,
        "usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}},
        "max_output_tokens":null,"max_tool_calls":null,"store":false,"background":false,
        "service_tier":"default","metadata":{},"safety_identifier":null,"prompt_cache_key":null
    })
}

struct GenerationApp {
    app: TestApp,
    requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    upstream: tokio::task::JoinHandle<()>,
    route_id: String,
    replies: Arc<parking_lot::Mutex<VecDeque<GenerationReply>>>,
    entered: Arc<tokio::sync::Notify>,
}

enum GenerationReply {
    Images(Vec<Value>),
    Status(axum::http::StatusCode, &'static str),
    Disconnect,
    Hold,
    CompleteAfter(tokio::sync::oneshot::Receiver<()>),
    WithoutUsage,
}

fn generated_item(encoded: &str) -> Value {
    json!({"type":"image_generation_call","id":"ig_1","status":"completed","result":encoded,"output_format":"png"})
}

fn generation_response(images: Vec<Value>, without_usage: bool) -> axum::response::Response {
    let mut created = response_resource("in_progress", json!([]));
    let mut completed = response_resource("completed", json!(images));
    if without_usage {
        created["usage"] = Value::Null;
        completed["usage"] = Value::Null;
    }
    let mut events =
        vec![json!({"type":"response.created","sequence_number":0,"response":created})];
    for (index, image) in images.into_iter().enumerate() {
        events.push(json!({"type":"response.output_item.added","sequence_number":events.len(),"output_index":index,"item":{"type":"image_generation_call","id":image["id"],"status":"in_progress","result":null}}));
        for phase in ["in_progress", "generating", "completed"] {
            events.push(json!({"type":format!("response.image_generation_call.{phase}"),"sequence_number":events.len(),"output_index":index,"item_id":image["id"]}));
        }
        events.push(json!({"type":"response.output_item.done","sequence_number":events.len(),"output_index":index,"item":image}));
    }
    events.push(
        json!({"type":"response.completed","sequence_number":events.len(),"response":completed}),
    );
    let body: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

impl Drop for GenerationApp {
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

async fn generation_app() -> GenerationApp {
    let app = test_app().await;
    crate::plugin::test_support::install_distributed_vendor(&app.gateway, "openai-codex")
        .await
        .expect("Codex vendor plugin");
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let observed = requests.clone();
    let replies = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
    let queued = replies.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let notified = entered.clone();
    let upstream = axum::Router::new().route(
        "/responses",
        axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
            let observed = observed.clone();
            let queued = queued.clone();
            let notified = notified.clone();
            async move {
                observed.lock().push(request);
                notified.notify_one();
                let reply = queued.lock().pop_front();
                match reply {
                    Some(GenerationReply::Images(images)) => generation_response(images, false),
                    Some(GenerationReply::Status(status, message)) => (
                        status,
                        axum::Json(json!({"error":{"message":message,"type":"server_error"}})),
                    )
                        .into_response(),
                    Some(GenerationReply::Disconnect) => axum::response::Response::builder()
                        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                        .body(axum::body::Body::from_stream(futures::stream::once(
                            async {
                                Err::<Bytes, _>(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionReset,
                                    "accepted request lost before output",
                                ))
                            },
                        )))
                        .unwrap(),
                    Some(GenerationReply::Hold) => futures::future::pending().await,
                    Some(GenerationReply::CompleteAfter(release)) => {
                        release.await.expect("release controlled generation");
                        generation_response(vec![generated_item(PNG)], false)
                    }
                    Some(GenerationReply::WithoutUsage) => {
                        generation_response(vec![generated_item(PNG)], true)
                    }
                    None => generation_response(vec![generated_item(PNG)], false),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let provider = app
        .gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "Local Codex fixture".into(),
            vendor: Some("openai-codex".into()),
            protocol: "open-responses".into(),
            base_url: base_url.clone(),
            preset_key: Some("openai".into()),
            channel: Some("codex".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
        })
        .await
        .unwrap();
    app.gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "local-test-not-a-production-credential".into(),
                resource_url: Some(base_url),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    app.gateway.admin().create_manual_provider_model(&provider.id, "gpt-5.4", crate::provider_models::CreateManualProviderModel {
        metadata: json!({"id":"gpt-5.4","name":"Local GPT","attachment":true,"tool_call":true,"capabilities":["media_image"],"modalities":{"input":["text","image"],"output":["text","image"]}}),
        template_id: None,
    }).await.unwrap();
    let route = app
        .gateway
        .admin()
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Image,
            model_id: "image-generation".into(),
            display_name: Some("Image generation".into()),
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id,
                model: "gpt-5.4".into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await
        .unwrap();
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            serde_json::from_value(json!({
                "mcp_access_enabled":true,"transparent_injection_enabled":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    app.gateway
        .admin()
        .set_setting(
            "artifact_settings",
            &json!({
                "client_base_url":app.endpoint.trim_end_matches("/mcp")
            })
            .to_string(),
        )
        .await
        .unwrap();
    app.gateway
        .admin()
        .update_media_generation_config(MediaGenerationConfig {
            enabled: true,
            image: ImageGenerationConfig {
                route_id: Some(route.model_id.clone().into()),
            },
        })
        .await
        .unwrap();
    GenerationApp {
        app,
        requests,
        upstream,
        route_id: route.model_id.into(),
        replies,
        entered,
    }
}

async fn call_generate(client: &SdkClient, input: Value) -> rmcp::model::CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("generate")
                .with_arguments(input.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
}

struct HandoffBarrierStore {
    inner: Arc<dyn crate::history_marker::HistoryMarkerStore>,
    barrier: parking_lot::Mutex<Option<HandoffBarrier>>,
}

struct HandoffBarrier {
    after_commit: bool,
    entered: tokio::sync::oneshot::Sender<String>,
    release: tokio::sync::oneshot::Receiver<()>,
    finished: tokio::sync::oneshot::Sender<bool>,
}

fn install_handoff_barrier(
    fixture: &mut GenerationApp,
    after_commit: bool,
) -> (
    tokio::sync::oneshot::Receiver<String>,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<bool>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    fixture.app.gateway.history_markers = Arc::new(HandoffBarrierStore {
        inner: fixture.app.gateway.history_markers.clone(),
        barrier: parking_lot::Mutex::new(Some(HandoffBarrier {
            after_commit,
            entered: entered_tx,
            release: release_rx,
            finished: finished_tx,
        })),
    });
    (entered_rx, release_tx, finished_rx)
}

#[async_trait::async_trait]
impl crate::history_marker::HistoryMarkerStore for HandoffBarrierStore {
    async fn create_public_execution(
        &self,
        principal: &stravia_runtime_contract::Principal,
        marker_reference: &str,
        input: crate::history_marker::PublicExecutionInput,
    ) -> Result<String, crate::history_marker::HistoryMarkerError> {
        self.inner
            .create_public_execution(principal, marker_reference, input)
            .await
    }

    async fn resolve_public_execution(
        &self,
        principal: &stravia_runtime_contract::Principal,
        id: &str,
    ) -> Result<
        Option<crate::history_marker::PublicExecution>,
        crate::history_marker::HistoryMarkerError,
    > {
        self.inner.resolve_public_execution(principal, id).await
    }

    async fn handoff_public_execution(
        &self,
        principal: &stravia_runtime_contract::Principal,
        id: &str,
        before_commit: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        let barrier = self.barrier.lock().take();
        let Some(barrier) = barrier else {
            return self
                .inner
                .handoff_public_execution(principal, id, before_commit)
                .await;
        };
        let HandoffBarrier {
            after_commit,
            entered,
            release,
            finished,
        } = barrier;
        if after_commit {
            let result = self
                .inner
                .handoff_public_execution(principal, id, before_commit)
                .await;
            assert!(
                result.is_ok(),
                "real handoff must commit before the ACK barrier: {result:?}"
            );
            entered
                .send(id.to_owned())
                .expect("observe committed handoff");
            release.await.expect("release committed handoff ACK");
            let _ = finished.send(result.is_ok());
            result
        } else {
            entered
                .send(id.to_owned())
                .expect("observe precommit handoff");
            release.await.expect("release precommit handoff");
            let result = self
                .inner
                .handoff_public_execution(principal, id, before_commit)
                .await;
            let _ = finished.send(result.is_ok());
            result
        }
    }

    async fn recover_public_executions(
        &self,
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        self.inner.recover_public_executions().await
    }

    async fn create_platform(
        &self,
        principal: &stravia_runtime_contract::Principal,
        input: crate::history_marker::PlatformMarkerInput,
    ) -> Result<crate::history_marker::HistoryMarker, crate::history_marker::HistoryMarkerError>
    {
        self.inner.create_platform(principal, input).await
    }

    async fn create_thinking(
        &self,
        principal: &stravia_runtime_contract::Principal,
        input: crate::history_marker::ThinkingMarkerInput,
    ) -> Result<crate::history_marker::HistoryMarker, crate::history_marker::HistoryMarkerError>
    {
        self.inner.create_thinking(principal, input).await
    }

    async fn create_reserved_thinking(
        &self,
        principal: &stravia_runtime_contract::Principal,
        reserved: &crate::history_marker::HistoryMarker,
        input: crate::history_marker::ThinkingMarkerInput,
    ) -> Result<crate::history_marker::HistoryMarker, crate::history_marker::HistoryMarkerError>
    {
        self.inner
            .create_reserved_thinking(principal, reserved, input)
            .await
    }

    async fn resolve(
        &self,
        principal: &stravia_runtime_contract::Principal,
        reference: &str,
    ) -> Result<
        Option<crate::history_marker::ResolvedHistoryMarker>,
        crate::history_marker::HistoryMarkerError,
    > {
        self.inner.resolve(principal, reference).await
    }

    async fn claim_execution(
        &self,
        principal: &stravia_runtime_contract::Principal,
        reference: &str,
        owner_id: &str,
        lease: Duration,
    ) -> Result<crate::history_marker::ClaimOutcome, crate::history_marker::HistoryMarkerError>
    {
        self.inner
            .claim_execution(principal, reference, owner_id, lease)
            .await
    }

    async fn finish_execution(
        &self,
        principal: &stravia_runtime_contract::Principal,
        reference: &str,
        owner_id: &str,
        state: crate::history_marker::PlatformExecutionState,
        segment: crate::history_marker::HiddenHistorySegment,
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        self.inner
            .finish_execution(principal, reference, owner_id, state, segment)
            .await
    }

    async fn wait_terminal(
        &self,
        principal: &stravia_runtime_contract::Principal,
        reference: &str,
    ) -> Result<
        Option<crate::history_marker::ResolvedHistoryMarker>,
        crate::history_marker::HistoryMarkerError,
    > {
        self.inner.wait_terminal(principal, reference).await
    }

    async fn publish(
        &self,
        principal: &stravia_runtime_contract::Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        self.inner.publish(principal, references, retention).await
    }

    async fn extend_retention(
        &self,
        principal: &stravia_runtime_contract::Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        self.inner
            .extend_retention(principal, references, retention)
            .await
    }

    async fn cleanup_expired(&self) -> Result<u64, crate::history_marker::HistoryMarkerError> {
        self.inner.cleanup_expired().await
    }
}

#[tokio::test]
async fn cancellation_before_handoff_commit_releases_generation_gate_without_upstream_release() {
    let mut fixture = generation_app().await;
    allow_slow_generation(&fixture).await;
    let (handoff_entered, release_handoff, handoff_finished) =
        install_handoff_barrier(&mut fixture, false);
    let (_release_upstream, upstream_release) = tokio::sync::oneshot::channel();
    fixture
        .replies
        .lock()
        .push_back(GenerationReply::CompleteAfter(upstream_release));
    let tool = crate::media_generation::platform::GenerateTool::new(&fixture.app.gateway);
    let caller = stravia_runtime_contract::CancellationToken::new();
    let context = McpContext::new(fixture.app.key_id.clone()).for_call(
        caller.clone(),
        std::time::Instant::now() + Duration::from_secs(900),
    );
    let call = tokio::spawn(async move {
        McpTool::call(
            &tool,
            json!({"type":"image","input":{"prompt":"Cancelled before handoff"}}),
            &context,
        )
        .await
    });
    fixture.entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    let _public_id = handoff_entered.await.unwrap();
    caller.cancel();
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    release_handoff.send(()).unwrap();
    assert!(
        !handoff_finished.await.unwrap(),
        "cancelled local work must not commit a durable handoff"
    );

    // Keep the first HTTP response blocked: only local cancellation can release its gate.
    let client = connect(&fixture.app).await;
    let next = tokio::time::timeout(
        Duration::from_secs(10),
        call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"Independent next square"}}),
        ),
    )
    .await
    .expect("next generation must pass the cancelled generation's gate");
    assert_ne!(next.is_error, Some(true), "{next:?}");
    assert_eq!(
        next.structured_content.unwrap()["media"],
        json!({"width":1,"height":1})
    );
    assert_eq!(fixture.requests.lock().len(), 2);
}

#[tokio::test]
async fn cancellation_after_handoff_commit_before_ack_preserves_original_generation() {
    let mut fixture = generation_app().await;
    allow_slow_generation(&fixture).await;
    let (handoff_entered, release_ack, handoff_finished) =
        install_handoff_barrier(&mut fixture, true);
    let (release_upstream, upstream_release) = tokio::sync::oneshot::channel();
    fixture
        .replies
        .lock()
        .push_back(GenerationReply::CompleteAfter(upstream_release));
    let tool = crate::media_generation::platform::GenerateTool::new(&fixture.app.gateway);
    let caller = stravia_runtime_contract::CancellationToken::new();
    let context = McpContext::new(fixture.app.key_id.clone()).for_call(
        caller.clone(),
        std::time::Instant::now() + Duration::from_secs(900),
    );
    let call = tokio::spawn(async move {
        McpTool::call(
            &tool,
            json!({"type":"image","input":{"prompt":"Committed original square"}}),
            &context,
        )
        .await
    });
    fixture.entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    let public_id = handoff_entered.await.unwrap();
    caller.cancel();
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    release_ack.send(()).unwrap();
    assert!(handoff_finished.await.unwrap());
    release_upstream.send(()).unwrap();

    let client = connect(&fixture.app).await;
    let execution_path = format!("stravia://tool-executions/{public_id}");
    let completed = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead").with_arguments(
                json!({"path":format!("{execution_path}?wait=120")})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(completed.is_error, Some(true), "{completed:?}");
    let completed = completed.structured_content.unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["path"], execution_path);
    assert_eq!(completed["result"]["media"], json!({"width":1,"height":1}));
    let artifact = completed["result"]["path"].as_str().unwrap();
    let download = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead").with_arguments(
                json!({"path":format!("{artifact}?download=1")})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(download.is_error, Some(true), "{download:?}");
    let download = download.structured_content.unwrap();
    let bytes = reqwest::get(download["download_url"].as_str().unwrap())
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        bytes.as_ref(),
        base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap()
    );
    assert_eq!(
        fixture.requests.lock().len(),
        1,
        "committed generation must never restart"
    );
}

#[tokio::test]
async fn bounded_generation_queries_share_a_durable_result_and_preserve_artifact_bytes() {
    let fixture = generation_app().await;
    allow_slow_generation(&fixture).await;
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    fixture
        .replies
        .lock()
        .push_back(GenerationReply::CompleteAfter(release_rx));
    let tool = crate::media_generation::platform::GenerateTool::new(&fixture.app.gateway);
    let caller = stravia_runtime_contract::CancellationToken::new();
    let context = McpContext::new(fixture.app.key_id.clone()).for_call(
        caller.clone(),
        std::time::Instant::now() + Duration::from_secs(900),
    );
    let call = tokio::spawn(async move {
        McpTool::call(
            &tool,
            json!({"type":"image","input":{"prompt":"A controlled square"}}),
            &context,
        )
        .await
        .unwrap()
    });
    fixture.entered.notified().await;
    // 只移动受理后的等待时钟；不依赖调度 sleep，也不修改产品默认窗口。
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    let handed_off = call.await.unwrap();
    assert!(!handed_off.is_error, "{handed_off:?}");
    let execution_path = handed_off.structured_content["path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(execution_path.starts_with("stravia://tool-executions/"));
    assert_eq!(handed_off.structured_content["status"], "running");
    caller.cancel();

    let client = connect(&fixture.app).await;
    let running = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead").with_arguments(
                json!({"path":format!("{execution_path}?wait=0")})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(running.is_error, Some(true), "{running:?}");
    assert_eq!(
        running.structured_content.unwrap(),
        json!({"path":execution_path,"status":"running"})
    );
    release_tx.send(()).unwrap();
    let completed = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead").with_arguments(
                json!({"path":format!("{execution_path}?wait=120")})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(completed.is_error, Some(true), "{completed:?}");
    let completed = completed.structured_content.unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["path"], execution_path);
    assert_eq!(completed["result"]["media"], json!({"width":1,"height":1}));
    let artifact = completed["result"]["path"].as_str().unwrap();
    let download = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead").with_arguments(
                json!({"path":format!("{artifact}?download=1")})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let bytes = reqwest::get(download["download_url"].as_str().unwrap())
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        bytes.as_ref(),
        base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap()
    );
    let repeated = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead")
                .with_arguments(json!({"path":execution_path}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(repeated.structured_content.unwrap(), completed);
    assert_eq!(fixture.requests.lock().len(), 1);
}

async fn allow_slow_generation(fixture: &GenerationApp) {
    let route = fixture
        .app
        .gateway
        .admin()
        .get_model(&fixture.route_id)
        .await
        .unwrap();
    fixture
        .app
        .gateway
        .admin()
        .update_model(
            &fixture.route_id,
            crate::db::models::UpdateRoute {
                targets: Some(vec![CreateTarget {
                    provider_id: route.targets[0].provider_id().as_str().into(),
                    model: "gpt-5.4".into(),
                    enabled: true,
                    priority: Some(0),
                    first_token_timeout_ms: Some(120_000),
                    target_retry_budget: Some(0),
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn execution_storage_failure_never_claims_handoff_or_success_or_retries_generation() {
    for fail_handoff in [true, false] {
        let mut fixture = generation_app().await;
        allow_slow_generation(&fixture).await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::migrations::migrate_sqlite(&pool, None)
            .await
            .unwrap();
        sqlx::query(if fail_handoff {
            "CREATE TRIGGER fail_execution_write BEFORE UPDATE OF handed_off_at ON public_tool_executions BEGIN SELECT RAISE(ABORT, 'controlled handoff storage failure'); END"
        } else {
            "CREATE TRIGGER fail_execution_write BEFORE UPDATE OF execution_state ON history_markers WHEN NEW.execution_state = 'completed' BEGIN SELECT RAISE(ABORT, 'controlled result storage failure'); END"
        }).execute(&pool).await.unwrap();
        fixture.app.gateway.history_markers =
            Arc::new(crate::history_marker::SqlHistoryMarkerStore::sqlite(pool));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        fixture
            .replies
            .lock()
            .push_back(GenerationReply::CompleteAfter(release_rx));
        let tool = crate::media_generation::platform::GenerateTool::new(&fixture.app.gateway);
        let context = McpContext::new(fixture.app.key_id.clone());
        let call = tokio::spawn(async move {
            McpTool::call(
                &tool,
                json!({"type":"image","input":{"prompt":"A storage-bound square"}}),
                &context,
            )
            .await
            .unwrap()
        });
        fixture.entered.notified().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::resume();
        let initial = call.await.unwrap();
        release_tx.send(()).unwrap();
        if fail_handoff {
            assert!(initial.is_error, "{initial:?}");
            assert!(initial.structured_content.get("path").is_none());
            let tool = crate::media_generation::platform::GenerateTool::new(&fixture.app.gateway);
            let next = tokio::time::timeout(
                Duration::from_secs(10),
                McpTool::call(
                    &tool,
                    json!({"type":"image","input":{"prompt":"A new independent square"}}),
                    &McpContext::new(fixture.app.key_id.clone()),
                ),
            )
            .await
            .expect("failed handoff releases the original local execution")
            .unwrap();
            assert!(!next.is_error, "{next:?}");
            assert_eq!(
                next.structured_content["media"],
                json!({"width":1,"height":1})
            );
            assert_eq!(fixture.requests.lock().len(), 2);
        } else {
            assert!(!initial.is_error, "{initial:?}");
            let path = initial.structured_content["path"].as_str().unwrap();
            let reader =
                crate::mcp::read::ReadTool::new(&fixture.app.gateway, &mut Vec::new()).unwrap();
            let result = McpTool::call(
                &reader,
                json!({"path":format!("{path}?wait=120")}),
                &McpContext::new(fixture.app.key_id.clone()),
            )
            .await
            .unwrap();
            assert!(result.is_error, "{result:?}");
            assert!(result.structured_content.get("result").is_none());
            assert_eq!(fixture.requests.lock().len(), 1);
        }
    }
}

#[tokio::test]
async fn registered_image_output_changes_gate_real_requests_and_recover_the_same_binding() {
    let fixture = generation_app().await;
    let admin = fixture.app.gateway.admin();
    let route = admin.get_model(&fixture.route_id).await.unwrap();
    let provider_id = route.targets[0].provider_id().as_str();
    let original_model = admin
        .get_provider_model(provider_id, "gpt-5.4")
        .await
        .unwrap();
    let mut fallback_metadata = serde_json::to_value(original_model.metadata).unwrap();
    fallback_metadata["id"] = json!("gpt-5.3-codex");
    admin
        .create_manual_provider_model(
            provider_id,
            "gpt-5.3-codex",
            crate::provider_models::CreateManualProviderModel {
                metadata: fallback_metadata,
                template_id: None,
            },
        )
        .await
        .unwrap();
    let make_target = |model: &str, priority| CreateTarget {
        provider_id: provider_id.into(),
        model: model.into(),
        enabled: true,
        priority: Some(priority),
        first_token_timeout_ms: None,
        target_retry_budget: Some(0),
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    };
    admin
        .update_model(
            &fixture.route_id,
            crate::db::models::UpdateRoute {
                targets: Some(vec![
                    make_target("gpt-5.4", 100),
                    make_target("gpt-5.3-codex", 0),
                ]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let bound = admin.get_model(&fixture.route_id).await.unwrap();
    let original_target_id = bound
        .targets
        .iter()
        .find(|target| target.model().as_str() == "gpt-5.4")
        .unwrap()
        .id
        .clone();
    let client = connect(&fixture.app).await;

    let first = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"Before change"}}),
    )
    .await;
    assert_ne!(first.is_error, Some(true), "{first:?}");
    assert_eq!(fixture.requests.lock().len(), 1);
    assert_eq!(fixture.requests.lock()[0]["model"], "gpt-5.4");

    let model = admin
        .get_provider_model(provider_id, "gpt-5.4")
        .await
        .unwrap();
    admin
        .update_provider_model(
            provider_id,
            "gpt-5.4",
            crate::provider_models::UpdateProviderModel {
                changes: std::collections::BTreeMap::from([(
                    "modalities.output".to_string(),
                    json!(["text"]),
                )]),
                revision: model.revision,
            },
        )
        .await
        .unwrap();
    let fallback = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"Use conforming fallback"}}),
    )
    .await;
    assert_ne!(fallback.is_error, Some(true), "{fallback:?}");
    assert_eq!(fixture.requests.lock().len(), 2);
    assert_eq!(
        fixture.requests.lock()[1]["model"],
        "gpt-5.3-codex",
        "only the still-conforming Target may receive a request"
    );
    let model = admin
        .get_provider_model(provider_id, "gpt-5.3-codex")
        .await
        .unwrap();
    admin
        .update_provider_model(
            provider_id,
            "gpt-5.3-codex",
            crate::provider_models::UpdateProviderModel {
                changes: std::collections::BTreeMap::from([(
                    "modalities.output".to_string(),
                    json!(["text"]),
                )]),
                revision: model.revision,
            },
        )
        .await
        .unwrap();
    let rejected = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"Must not send"}}),
    )
    .await;
    assert_eq!(rejected.is_error, Some(true), "{rejected:?}");
    assert_eq!(
        fixture.requests.lock().len(),
        2,
        "mismatched Targets must receive zero additional requests"
    );
    let retained = admin.get_model(&fixture.route_id).await.unwrap();
    let original = retained
        .targets
        .iter()
        .find(|target| target.id == original_target_id)
        .unwrap();
    assert!(original.output_modality_mismatch);

    let model = admin
        .get_provider_model(provider_id, "gpt-5.4")
        .await
        .unwrap();
    admin
        .update_provider_model(
            provider_id,
            "gpt-5.4",
            crate::provider_models::UpdateProviderModel {
                changes: std::collections::BTreeMap::from([(
                    "modalities.output".to_string(),
                    json!(["text", "image"]),
                )]),
                revision: model.revision,
            },
        )
        .await
        .unwrap();
    let recovered = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"After repair"}}),
    )
    .await;
    assert_ne!(recovered.is_error, Some(true), "{recovered:?}");
    assert_eq!(fixture.requests.lock().len(), 3);
    assert_eq!(fixture.requests.lock()[2]["model"], "gpt-5.4");
    let repaired = admin.get_model(&fixture.route_id).await.unwrap();
    let original = repaired
        .targets
        .iter()
        .find(|target| target.id == original_target_id)
        .unwrap();
    assert!(!original.output_modality_mismatch);
}

async fn upload_image(app: &TestApp, bytes: Vec<u8>, mime: &str) -> String {
    let base = app.endpoint.strip_suffix("/mcp").unwrap();
    let http = reqwest::Client::new();
    let upload: Value = http
        .post(format!("{base}/v1/artifacts/uploads"))
        .bearer_auth(&app.token)
        .json(&json!({"mime_type":mime,"size":bytes.len()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let upload_id = upload["upload_id"].as_str().unwrap();
    let token = upload["upload_token"].as_str().unwrap();
    let part: Value = http
        .put(format!("{base}/v1/artifacts/uploads/{upload_id}/parts/1"))
        .bearer_auth(&app.token)
        .header("x-upload-token", token)
        .body(bytes)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let completed: Value = http
        .post(format!("{base}/v1/artifacts/uploads/{upload_id}/complete"))
        .bearer_auth(&app.token)
        .json(&json!({"upload_token":token,"parts":[part]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(completed.get("id").is_none());
    assert!(completed.get("reference").is_none());
    let path = completed["path"].as_str().expect("Artifact path");
    assert!(path.starts_with("stravia://artifacts/"));
    path.to_owned()
}

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(width, height)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[tokio::test]
async fn uploaded_references_keep_order_and_do_not_forward_source_identities() {
    let fixture = generation_app().await;
    let first = png_bytes(2, 3);
    let second = png_bytes(4, 5);
    let first_ref = upload_image(&fixture.app, first.clone(), "image/png").await;
    let second_ref = upload_image(&fixture.app, second.clone(), "image/png").await;
    let client = connect(&fixture.app).await;
    let result = call_generate(&client, json!({"type":"image","input":{"prompt":"Use image 1 layout and image 2 colors","reference_images":[first_ref,second_ref]}})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let requests = fixture.requests.lock();
    let images: Vec<&str> = requests[0]["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|content| content["image_url"].as_str())
        .collect();
    let expected: Vec<String> = [first, second]
        .iter()
        .map(|bytes| {
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
        })
        .collect();
    assert_eq!(images, expected);
    assert!(
        requests[0]["tools"][0].get("size").is_none(),
        "omitted preferences must stay omitted"
    );
}

#[tokio::test]
async fn invalid_arguments_and_unsafe_references_never_start_generation() {
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let reference = upload_image(&fixture.app, png_bytes(2, 2), "image/png").await;
    for arguments in [
        json!({"type":"video","input":{"prompt":"hello"}}),
        json!({"type":"image","input":{"prompt":"   "}}),
        json!({"type":"image","route":"override","input":{"prompt":"hello"}}),
        json!({"type":"image","input":{"prompt":"hello","quality":"high"}}),
        json!({"type":"image","input":{"prompt":"hello","aspect_ratio":"2:1"}}),
        json!({"type":"image","input":{"prompt":"hello","resolution":null}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":vec![reference.clone();6]}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":[format!("{reference}?download=1")]}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":[format!("data:image/png;base64,{PNG}")]}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":["http://127.0.0.1/private.png"]}}),
    ] {
        let result = call_generate(&client, arguments.clone()).await;
        assert_eq!(result.is_error, Some(true), "{arguments}: {result:?}");
        assert!(result.structured_content.unwrap().get("path").is_none());
    }
    assert!(fixture.requests.lock().is_empty());
}

#[tokio::test]
async fn foreign_expired_missing_and_corrupt_artifacts_fail_before_generation() {
    use stravia_runtime_contract::artifact::{ArtifactId, ArtifactSource};
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let store = fixture.app.gateway.artifact_store().unwrap();
    let foreign = store
        .ingest(
            &stravia_runtime_contract::Principal::new("another-principal"),
            "image/png",
            None,
            Box::pin(futures::stream::once(async {
                Ok(Bytes::from(png_bytes(2, 4)))
            })),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    let invalid = upload_image(
        &fixture.app,
        b"not a decodable picture".to_vec(),
        "image/png",
    )
    .await;
    let expired = upload_image(&fixture.app, png_bytes(3, 4), "image/png").await;
    sqlx::query("UPDATE artifacts SET expires_at=0 WHERE id=?")
        .bind(ArtifactId::from_reference(&expired).unwrap().as_str())
        .execute(fixture.app.gateway._sqlite_pool.as_ref().unwrap())
        .await
        .unwrap();
    let missing = upload_image(&fixture.app, png_bytes(4, 4), "image/png").await;
    let reader = store
        .open(
            &stravia_runtime_contract::Principal::new(fixture.app.key_id.clone()),
            &ArtifactId::from_reference(&missing).unwrap(),
        )
        .await
        .unwrap();
    let ArtifactSource::LocalPath(path) = &reader.source else {
        panic!("local artifact");
    };
    tokio::fs::remove_file(path).await.unwrap();
    drop(reader);
    for reference in [foreign.reference(), invalid, expired, missing] {
        let result = call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"Edit this","reference_images":[reference]}}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
    }
    assert!(fixture.requests.lock().is_empty());
}

#[tokio::test]
async fn unusable_completed_outputs_are_errors_not_regeneration_requests() {
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let mut extra = generated_item(PNG);
    extra["id"] = json!("ig_2");
    for images in [
        vec![],
        vec![generated_item("bm90LWFuLWltYWdl")],
        vec![generated_item(PNG), extra],
    ] {
        fixture
            .replies
            .lock()
            .push_back(GenerationReply::Images(images));
        let result = call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"A square"}}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert!(result.structured_content.unwrap().get("path").is_none());
    }
    assert_eq!(fixture.requests.lock().len(), 3);
}

#[tokio::test]
async fn artifact_save_failure_does_not_repeat_successful_upstream_generation() {
    let fixture = generation_app().await;
    let root = fixture.app._data_dir.path().join("artifacts");
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    std::fs::write(&root, b"artifact storage is unavailable").unwrap();
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    assert_eq!(
        result.structured_content.unwrap()["error"]["code"],
        "artifact_storage_failed"
    );
    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn route_failover_handles_typed_status_and_transport_failures() {
    for failure in [
        GenerationReply::Status(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Local transient failure",
        ),
        GenerationReply::Disconnect,
    ] {
        let fixture = generation_app().await;
        let route = fixture
            .app
            .gateway
            .admin()
            .get_model(&fixture.route_id)
            .await
            .unwrap();
        let provider = route.targets[0].provider_id().as_str();
        fixture
            .app
            .gateway
            .admin()
            .create_manual_provider_model(
                provider,
                "gpt-5.2",
                crate::provider_models::CreateManualProviderModel {
                    metadata: json!({"id":"gpt-5.2","name":"Fallback GPT","tool_call":true,"capabilities":["media_image"],"modalities":{"input":["text","image"],"output":["text","image"]}}),
                    template_id: None,
                },
            )
            .await
            .unwrap();
        fixture.app.gateway.admin().update_model(&fixture.route_id, serde_json::from_value(json!({
            "targets":[
                {"provider_id":provider,"model":"gpt-5.4","enabled":true,"priority":100,"target_retry_budget":0},
                {"provider_id":provider,"model":"gpt-5.2","enabled":true,"priority":0,"target_retry_budget":0}
            ]
        })).unwrap()).await.unwrap();
        fixture.replies.lock().push_back(failure);
        let client = connect(&fixture.app).await;
        let result = call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"A square"}}),
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let requests = fixture.requests.lock();
        assert_eq!(
            requests
                .iter()
                .map(|request| request["model"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["gpt-5.4", "gpt-5.2"]
        );
    }
}

#[tokio::test]
async fn cancellation_stops_an_accepted_generation_without_retry() {
    let fixture = generation_app().await;
    fixture.replies.lock().push_back(GenerationReply::Hold);
    let tool = crate::media_generation::platform::GenerateTool::new(&fixture.app.gateway);
    let token = stravia_runtime_contract::CancellationToken::new();
    let context = McpContext::new(fixture.app.key_id.clone()).for_call(
        token.clone(),
        std::time::Instant::now() + Duration::from_secs(60),
    );
    let call = tokio::spawn(async move {
        McpTool::call(
            &tool,
            json!({"type":"image","input":{"prompt":"A square"}}),
            &context,
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), fixture.entered.notified())
        .await
        .unwrap();
    token.cancel();
    let output = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .unwrap()
        .unwrap();
    assert!(output.is_error);
    assert_eq!(output.structured_content["error"]["code"], "cancelled");

    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn upstream_business_refusal_reaches_platform_tool_without_retry() {
    let fixture = generation_app().await;
    let reason = "The image was filtered because it violated the upstream content policy";
    fixture.replies.lock().push_back(GenerationReply::Status(
        axum::http::StatusCode::BAD_REQUEST,
        reason,
    ));
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let output = result.structured_content.unwrap();
    assert_eq!(output["error"]["code"], "upstream_generation_failed");
    assert!(
        output["error"]["message"]
            .as_str()
            .unwrap()
            .contains(reason)
    );
    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn upstream_business_refusal_redacts_credentials_in_public_message() {
    let fixture = generation_app().await;
    fixture.replies.lock().push_back(GenerationReply::Status(
        axum::http::StatusCode::BAD_REQUEST,
        "Image rejected; local-test-not-a-production-credential",
    ));
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let output = result.structured_content.unwrap();
    let message = output["error"]["message"].as_str().unwrap();
    assert!(message.contains("Image rejected"));
    assert!(!message.contains("local-test-not-a-production-credential"));
    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn upstream_credential_errors_do_not_echo_credentials_or_retry() {
    let fixture = generation_app().await;
    fixture.replies.lock().push_back(GenerationReply::Status(
        axum::http::StatusCode::UNAUTHORIZED,
        "Rejected local-test-not-a-production-credential",
    ));
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let output = result.structured_content.unwrap();
    assert_eq!(output["error"]["code"], "upstream_generation_failed");
    assert!(
        !output
            .to_string()
            .contains("local-test-not-a-production-credential")
    );
    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn missing_upstream_usage_remains_unknown() {
    let fixture = generation_app().await;
    fixture
        .replies
        .lock()
        .push_back(GenerationReply::WithoutUsage);
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    fixture
        .app
        .gateway
        .admin()
        .observation_flush()
        .await
        .unwrap();
    let stats = fixture
        .app
        .gateway
        .admin()
        .get_stats_overview(crate::db::models::StatsWindow::default())
        .await
        .unwrap();
    assert_eq!(stats.total_requests, 1);
    assert_eq!(stats.total_input_tokens, None);
    assert_eq!(stats.total_output_tokens, None);
}

#[tokio::test]
async fn generated_image_downloads_real_bytes_and_can_be_edited() {
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let result = call_generate(&client, json!({"type":"image","input":{"prompt":"A blue square","aspect_ratio":"16:9","resolution":"4K"}})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let image = result.structured_content.unwrap();
    assert_eq!(image["mime_type"], "image/png");
    assert_eq!(image["media"], json!({"width":1,"height":1}));
    let reference = image["path"].as_str().unwrap();
    assert!(reference.starts_with("stravia://artifacts/"));

    let grant = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead").with_arguments(
                json!({"path":format!("{reference}?download=1")})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(grant.is_error, Some(true), "{grant:?}");
    let grant = grant.structured_content.unwrap();
    let response = reqwest::get(grant["download_url"].as_str().unwrap())
        .await
        .unwrap();
    assert!(response.status().is_success());
    let bytes = response.bytes().await.unwrap();
    assert_eq!(
        bytes.as_ref(),
        base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap()
    );
    assert_eq!(image["size"], bytes.len());
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1, 1));
    let edited = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"Edit image 1","reference_images":[reference]}}),
    )
    .await;
    assert_ne!(edited.is_error, Some(true), "{edited:?}");
    let requests = fixture.requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]["tool_choice"],
        json!({"type":"image_generation"})
    );
    assert_eq!(requests[0]["tools"][0]["action"], "generate");
    assert_eq!(requests[0]["tools"][0]["size"], "1536x1024");
    assert_eq!(requests[0]["tools"][0]["quality"], "high");
    assert!(
        requests[0]["instructions"]
            .as_str()
            .is_some_and(|instructions| {
                instructions.contains("16:9") && instructions.contains("4K")
            })
    );
    assert_eq!(requests[1]["tools"][0]["action"], "edit");
    assert!(
        requests[1]
            .to_string()
            .contains(&format!("data:image/png;base64,{PNG}"))
    );
    assert!(!requests[1].to_string().contains("sa:"));
    assert!(!requests[1].to_string().contains("stravia://artifacts/"));
}

#[tokio::test]
async fn generation_gate_controls_discovery_without_injection_permission() {
    let app = test_app().await;
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            serde_json::from_value(json!({
                "mcp_access_enabled": true,
                "transparent_injection_enabled": false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let client = connect(&app).await;
    assert!(
        client
            .list_tools(None)
            .await
            .unwrap()
            .tools
            .iter()
            .all(|tool| tool.name != "generate")
    );
    app.gateway
        .storage
        .settings()
        .set(
            "media_generation_config",
            r#"{"enabled":true,"image":{"route_id":null}}"#,
        )
        .await
        .unwrap();
    assert!(
        client
            .list_tools(None)
            .await
            .unwrap()
            .tools
            .iter()
            .any(|tool| tool.name == "generate")
    );
    let result = client
        .call_tool(
            CallToolRequestParams::new("generate").with_arguments(
                json!({"type":"image","input":{"prompt":"A blue square"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        result.is_error,
        Some(true),
        "missing binding cannot yield an image"
    );
    app.gateway
        .storage
        .settings()
        .set(
            "media_generation_config",
            r#"{"enabled":false,"image":{"route_id":null}}"#,
        )
        .await
        .unwrap();
    assert!(
        client
            .list_tools(None)
            .await
            .unwrap()
            .tools
            .iter()
            .all(|tool| tool.name != "generate")
    );
}

#[tokio::test]
async fn generation_reports_usage_and_preserves_raw_media_in_debug_exports() {
    use futures::StreamExt;
    use std::io::Read;
    let fixture = generation_app().await;
    let admin = fixture.app.gateway.admin();
    admin.set_observation_debug(true);
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    admin.observation_flush().await.unwrap();
    let stats = admin
        .get_stats_overview(crate::db::models::StatsWindow::default())
        .await
        .unwrap();
    assert_eq!(
        (
            stats.total_requests,
            stats.total_input_tokens,
            stats.total_output_tokens
        ),
        (1, Some(10), Some(20))
    );
    let forest = admin.observation_forest(Default::default()).await.unwrap();
    let id = &forest.roots[0].interactions[0].id;
    let ticket = admin
        .issue_observation_bundle_ticket(crate::admin::BundleRequest {
            kind: crate::admin::BundleResourceKind::Interaction,
            resource_id: id.clone(),
            through_sequence: None,
        })
        .await
        .unwrap();
    let mut stream = admin
        .consume_observation_bundle_ticket(ticket.download_url.rsplit('/').next().unwrap())
        .await
        .unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut exported = String::new();
    for index in 0..archive.len() {
        archive
            .by_index(index)
            .unwrap()
            .read_to_string(&mut exported)
            .unwrap();
    }
    assert!(
        exported.contains(PNG),
        "current wire capture keeps the generated media payload in the bundle"
    );
    assert!(!exported.contains("local-test-not-a-production-credential"));
    assert!(!exported.contains(&fixture.app.token));
    assert!(!exported.contains(&fixture.app._data_dir.path().to_string_lossy().to_string()));
}
