use crate::config::GatewayConfig;
use crate::db::models::{
    CreateProvider, CreateRoute, ProviderCredentialInput, ProviderSourceInput,
};
use crate::provider_models::CreateManualProviderModel;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

use super::*;

struct ProxyRouterFixture {
    router: Router,
    gateway: Gateway,
    data_dir: tempfile::TempDir,
}

impl ProxyRouterFixture {
    async fn close(self) {
        let Self {
            router,
            gateway,
            data_dir,
        } = self;
        drop(router);
        gateway.shutdown().await;
        if let Some(pool) = &gateway._sqlite_pool {
            pool.close().await;
        }
        if let Some(pool) = &gateway._postgres_pool {
            pool.close().await;
        }
        drop(gateway);
        crate::proxy::test_cleanup::close_temp_dir(data_dir);
    }
}

struct ResponsesUpstream {
    base_url: String,
    requests: std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ResponsesUpstream {
    async fn close(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            match task.await {
                Err(error) if error.is_cancelled() => {}
                Err(error) => panic!("local Responses upstream failed: {error}"),
                Ok(()) => {}
            }
        }
    }
}

impl Drop for ResponsesUpstream {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_responses_upstream() -> ResponsesUpstream {
    let requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let observed = std::sync::Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
            let observed = std::sync::Arc::clone(&observed);
            async move {
                observed.lock().push(request);
                axum::Json(
                    stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                        "resp-auth",
                        "auth-model",
                        "completed",
                        vec![serde_json::json!({
                            "type": "message",
                            "id": "msg-auth",
                            "role": "assistant",
                            "status": "completed",
                            "content": [{
                                "type": "output_text",
                                "text": "authenticated",
                                "annotations": []
                            }]
                        })],
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                    ),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local Responses upstream");
    let address = listener.local_addr().expect("local Responses address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve local Responses upstream");
    });
    ResponsesUpstream {
        base_url: format!("http://{address}/v1"),
        requests,
        task: Some(task),
    }
}

async fn proxy_router() -> ProxyRouterFixture {
    let data_dir = tempfile::tempdir().expect("temporary gateway directory");
    let config = GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
    let router = create_router(gateway.clone());
    ProxyRouterFixture {
        router,
        gateway,
        data_dir,
    }
}

async fn protected_responses_router() -> (ProxyRouterFixture, String) {
    protected_responses_router_with_key_state(true).await
}

async fn protected_responses_router_with_key_state(enabled: bool) -> (ProxyRouterFixture, String) {
    protected_responses_router_with_hook(enabled, None).await
}

async fn protected_responses_router_with_hook(
    enabled: bool,
    hook: Option<std::sync::Arc<dyn stravia_runtime_contract::hook::Hook>>,
) -> (ProxyRouterFixture, String) {
    protected_responses_router_with_base_url(enabled, hook, "http://127.0.0.1:9").await
}

async fn protected_responses_router_with_base_url(
    enabled: bool,
    hook: Option<std::sync::Arc<dyn stravia_runtime_contract::hook::Hook>>,
    base_url: &str,
) -> (ProxyRouterFixture, String) {
    let data_dir = tempfile::tempdir().expect("temporary gateway directory");
    let config = GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let builder = Gateway::builder(config);
    let gateway = match hook {
        Some(hook) => builder.hook(hook).build().await,
        None => builder.build().await,
    }
    .expect("gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Open Responses auth target".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("open-responses".into()),
                base_url: base_url.into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    admin
        .create_manual_provider_model(
            &provider.id,
            "auth-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "auth-model",
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "name": "Auth model",
                    "reasoning_efforts": ["none", "low", "medium", "high", "xhigh"]
                }),
                template_id: None,
            },
        )
        .await
        .expect("Provider Model");
    let model = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "auth-model".into(),
            display_name: None,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: provider.id,
                model: "auth-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await
        .expect("Model Route");
    let api_key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Responses auth key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            transparent_injections: Default::default(),
            model_ids: vec![model.id.into()],
            spend_limit: None,
        })
        .await
        .expect("API key");
    if !enabled {
        admin
            .update_api_key(
                &api_key.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    rpm_limit: None,
                    is_enabled: Some(false),
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    transparent_injections: None,
                    expires_at: None,
                    model_ids: None,
                    spend_limit: None,
                },
            )
            .await
            .expect("disable API key");
    }
    let router = create_router(gateway.clone());
    (
        ProxyRouterFixture {
            router,
            gateway,
            data_dir,
        },
        api_key.token,
    )
}

struct BlockingRequestHook {
    entered: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
}

struct BlockingRequestSession {
    entered: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
}

impl stravia_runtime_contract::hook::Hook for BlockingRequestHook {
    fn descriptor(&self) -> stravia_runtime_contract::hook::HookDescriptor {
        stravia_runtime_contract::hook::HookDescriptor {
            event_kinds: vec![stravia_runtime_contract::hook::EventKind::Request],
            ..stravia_runtime_contract::hook::HookDescriptor::all("block-websocket-request")
        }
    }

    fn create_session(
        &self,
        _context: &stravia_runtime_contract::hook::SessionContext,
    ) -> Box<dyn stravia_runtime_contract::hook::HookSession> {
        Box::new(BlockingRequestSession {
            entered: std::sync::Arc::clone(&self.entered),
            release: std::sync::Arc::clone(&self.release),
        })
    }
}

#[async_trait::async_trait]
impl stravia_runtime_contract::hook::HookSession for BlockingRequestSession {
    async fn handle(
        &mut self,
        event: stravia_runtime_contract::hook::HookEvent<'_>,
    ) -> Result<stravia_runtime_contract::hook::ActionBatch, String> {
        if matches!(
            event,
            stravia_runtime_contract::hook::HookEvent::Request { .. }
        ) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(stravia_runtime_contract::hook::ActionBatch::default())
    }
}

#[tokio::test]
async fn unknown_protocol_paths_and_methods_use_canonical_not_found() {
    for (method, uri) in [("GET", "/v1/unknown"), ("DELETE", "/v1/responses")] {
        let fixture = proxy_router().await;
        let response = fixture
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("error body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON error");
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["type"], "not_found");
        fixture.close().await;
    }
}

#[tokio::test]
async fn proxy_accepts_json_bodies_larger_than_axum_default_limit() {
    let large_content = "x".repeat(2 * 1024 * 1024);
    let body = serde_json::json!({
        "contents": [
            {
                "role": "user",
                "parts": [
                    {
                        "text": large_content,
                    }
                ],
            }
        ],
    });

    let fixture = proxy_router().await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/unconfigured-gemini:generateContent")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("valid test request"),
        )
        .await
        .expect("proxy response");

    assert_ne!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "proxy must not reject large Gemini JSON bodies with axum's default 2 MiB limit"
    );
    drop(response);
    fixture.close().await;
}

#[tokio::test]
async fn responses_accepts_json_media_type_parameters() {
    let fixture = proxy_router().await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(r#"{"model":"missing","input":"hello"}"#))
                .expect("Responses request"),
        )
        .await
        .expect("Responses response");

    assert_ne!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    drop(response);
    fixture.close().await;
}

#[tokio::test]
async fn responses_rejects_form_body_with_canonical_error() {
    let fixture = proxy_router().await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("model=missing&input=hello"))
                .expect("Responses request"),
        )
        .await
        .expect("Responses response");

    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body"),
    )
    .expect("canonical JSON error");
    assert_eq!(
        body,
        serde_json::json!({
            "error": {
                "type": "unsupported_media_type",
                "code": "unsupported_media_type",
                "param": "content_type",
                "message": "Content-Type must be application/json."
            }
        })
    );
    fixture.close().await;
}

#[tokio::test]
async fn responses_bearer_scheme_is_case_insensitive() {
    let upstream = serve_responses_upstream().await;
    let (fixture, token) =
        protected_responses_router_with_base_url(true, None, &upstream.base_url).await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("bEaReR {token}"))
                .body(Body::from(r#"{"model":"auth-model","input":"hello"}"#))
                .expect("Responses request"),
        )
        .await
        .expect("Responses response");

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("Responses body"),
    )
    .expect("Responses JSON");
    assert_eq!(body["output"][0]["content"][0]["text"], "authenticated");
    {
        let requests = upstream.requests.lock();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["model"], "auth-model");
        assert_eq!(requests[0]["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(requests[0]["input"][0]["role"], "user");
        assert_eq!(requests[0]["input"][0]["content"][0]["text"], "hello");
    }
    fixture.close().await;
    upstream.close().await;
}

#[tokio::test]
async fn responses_rejects_ambiguous_credentials() {
    let cases = [
        vec![
            ("authorization", "Bearer first"),
            ("authorization", "Bearer second"),
        ],
        vec![("authorization", "Bearer   ")],
        vec![("x-api-key", "alternate")],
        vec![("x-goog-api-key", "alternate")],
    ];

    for headers in cases {
        let mut request = Request::post("/v1/responses").header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let fixture = proxy_router().await;
        let response = fixture
            .router
            .clone()
            .oneshot(
                request
                    .body(Body::from(r#"{"model":"missing","input":"hello"}"#))
                    .expect("Responses request"),
            )
            .await
            .expect("Responses response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("response body"),
        )
        .expect("canonical JSON error");
        assert_eq!(body["error"]["code"], "invalid_authentication");
        assert_eq!(body["error"]["param"], "authorization");
        fixture.close().await;
    }
}

#[tokio::test]
async fn legacy_compact_route_is_not_found() {
    let (fixture, token) = protected_responses_router().await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/responses/compact")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(r#"{"model":"auth-model","input":"hello"}"#))
                .expect("legacy compact request"),
        )
        .await
        .expect("legacy compact response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    fixture.close().await;
}

#[tokio::test]
async fn v2_compaction_requires_valid_authentication() {
    let (fixture, _) = protected_responses_router().await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"auth-model","stream":true,"input":[{"role":"user","content":"hello"},{"type":"compaction_trigger"}]}"#,
                ))
                .expect("V2 compaction request"),
        )
        .await
        .expect("V2 compaction response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    drop(response);
    fixture.close().await;
}

#[tokio::test]
async fn v2_compaction_requires_stream_and_one_terminal_trigger() {
    let (fixture, token) = protected_responses_router().await;
    for (body, param) in [
        (
            r#"{"model":"auth-model","input":[{"role":"user","content":"hello"},{"type":"compaction_trigger"}]}"#,
            "stream",
        ),
        (
            r#"{"model":"auth-model","stream":true,"input":[{"type":"compaction_trigger"},{"role":"user","content":"hello"}]}"#,
            "input",
        ),
        (
            r#"{"model":"auth-model","stream":true,"input":[{"type":"compaction_trigger"},{"type":"compaction_trigger"}]}"#,
            "input",
        ),
    ] {
        let response = fixture
            .router
            .clone()
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(body))
                    .expect("V2 compaction request"),
            )
            .await
            .expect("V2 compaction response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("V2 compaction response body"),
        )
        .expect("V2 compaction JSON");
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["param"], param);
    }
    fixture.close().await;
}

#[tokio::test]
async fn responses_rejects_background_with_canonical_error() {
    let (fixture, token) = protected_responses_router().await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    r#"{"model":"auth-model","input":"hello","background":true}"#,
                ))
                .expect("background request"),
        )
        .await
        .expect("background response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("background response body"),
    )
    .expect("background error JSON");
    assert_eq!(body["error"]["code"], "unsupported_feature");
    assert_eq!(body["error"]["param"], "background");
    fixture.close().await;
}
#[tokio::test]
async fn responses_websocket_rejects_unknown_event_types() {
    use futures::{SinkExt, StreamExt};
    use reqwest_websocket::Upgrade as _;

    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let (fixture, token) = protected_responses_router_with_hook(
        true,
        Some(std::sync::Arc::new(BlockingRequestHook {
            entered: std::sync::Arc::clone(&entered),
            release: std::sync::Arc::clone(&release),
        })),
    )
    .await;
    let router = fixture.router.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind WebSocket test listener");
    let address = listener.local_addr().expect("WebSocket test address");
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve WebSocket test router");
    });
    let response = reqwest::Client::new()
        .get(format!("http://{address}/v1/responses"))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .upgrade()
        .send()
        .await
        .expect("WebSocket handshake");
    let mut socket = response.into_websocket().await.expect("WebSocket upgrade");
    socket
        .send(reqwest_websocket::Message::Text(
            r#"{"type":"unknown.event"}"#.into(),
        ))
        .await
        .expect("send unknown event");
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .expect("WebSocket response timeout")
        .expect("WebSocket response")
        .expect("WebSocket message");
    let reqwest_websocket::Message::Text(message) = message else {
        panic!("JSON text error event");
    };
    let body: serde_json::Value = serde_json::from_str(&message).expect("WebSocket error JSON");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["code"], "invalid_request");
    socket
        .send(reqwest_websocket::Message::Text(
            r#"{"type":"response.create","model":"auth-model","input":"first"}"#.into(),
        ))
        .await
        .expect("send first response.create");
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("request Hook timeout");
    socket
        .send(reqwest_websocket::Message::Text(
            r#"{"type":"response.create","model":"auth-model","input":"second"}"#.into(),
        ))
        .await
        .expect("send concurrent response.create");
    let response_in_progress = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(message) = socket.next().await {
            let Ok(reqwest_websocket::Message::Text(message)) = message else {
                continue;
            };
            let Ok(body) = serde_json::from_str::<serde_json::Value>(&message) else {
                continue;
            };
            if body
                .pointer("/error/code")
                .and_then(serde_json::Value::as_str)
                == Some("response_in_progress")
            {
                return true;
            }
        }
        false
    })
    .await
    .expect("response_in_progress timeout");
    assert!(response_in_progress);

    release.notify_one();
    socket
        .close(reqwest_websocket::CloseCode::Normal, None)
        .await
        .expect("close WebSocket");
    server.abort();
    match server.await {
        Err(error) if error.is_cancelled() => {}
        Err(error) => panic!("WebSocket test server failed: {error}"),
        Ok(()) => {}
    }
    fixture.close().await;
}

struct ArtifactUploadFixture {
    router: Router,
    auth: String,
    gateway: Gateway,
    data_dir: tempfile::TempDir,
}

async fn artifact_upload_fixture() -> ArtifactUploadFixture {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let api_key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Artifact key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            transparent_injections: Default::default(),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    ArtifactUploadFixture {
        router: create_router(gateway.clone()),
        auth: format!("Bearer {}", api_key.token),
        gateway,
        data_dir,
    }
}

async fn post_artifact(
    router: &Router,
    authorization: Option<&str>,
    content_type: Option<&str>,
    body: Body,
) -> axum::response::Response {
    let mut request = Request::post("/v1/artifacts");
    if let Some(value) = authorization {
        request = request.header("authorization", value);
    }
    if let Some(value) = content_type {
        request = request.header("content-type", value);
    }
    router
        .clone()
        .oneshot(request.body(body).expect("upload request"))
        .await
        .expect("upload response")
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body"),
    )
    .expect("response JSON")
}

async fn upload_ok(
    router: &Router,
    auth: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> serde_json::Value {
    let response = post_artifact(router, Some(auth), Some(content_type), Body::from(bytes)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    response_json(response).await
}

#[tokio::test]
async fn artifact_upload_streams_raw_bytes_and_returns_the_stable_path() {
    let fixture = artifact_upload_fixture().await;

    let body = upload_ok(&fixture.router, &fixture.auth, "image/png", b"png".to_vec()).await;

    let fields = body.as_object().expect("upload JSON object");
    let mut keys: Vec<_> = fields.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["mime_type", "path", "size"]);
    assert_eq!(body["mime_type"], "image/png");
    assert_eq!(body["size"], 3);
    stravia_runtime_contract::artifact::ArtifactId::from_reference(
        body["path"].as_str().expect("Artifact path"),
    )
    .expect("path resolves to an Artifact identity");
}

#[tokio::test]
async fn artifact_upload_keeps_the_file_for_the_configured_retention() {
    let fixture = artifact_upload_fixture().await;
    fixture
        .gateway
        .storage
        .settings()
        .set("log_retention_days", "1")
        .await
        .expect("retention setting");

    let before = chrono::Utc::now().timestamp_millis();
    let body = upload_ok(&fixture.router, &fixture.auth, "image/png", b"png".to_vec()).await;
    let after = chrono::Utc::now().timestamp_millis();

    let id = stravia_runtime_contract::artifact::ArtifactId::from_reference(
        body["path"].as_str().expect("Artifact path"),
    )
    .expect("Artifact identity");
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(fixture.data_dir.path().join("db").join("gateway.db")),
    )
    .await
    .expect("gateway database");
    let expires_at: i64 = sqlx::query_scalar("SELECT expires_at FROM artifacts WHERE id = ?")
        .bind(id.as_str())
        .fetch_one(&pool)
        .await
        .expect("Artifact expiry");
    pool.close().await;
    let day = 86_400_000;
    assert!(
        (before + day..=after + day).contains(&expires_at),
        "upload must follow log_retention_days, got expiry {expires_at} for upload in [{before}, {after}]"
    );
}

#[tokio::test]
async fn artifact_upload_requires_the_file_mime_as_content_type() {
    let fixture = artifact_upload_fixture().await;

    for content_type in [
        None,
        Some(""),
        Some("application/x-www-form-urlencoded"),
        Some("Application/X-WWW-Form-Urlencoded; charset=utf-8"),
        Some("multipart/form-data; boundary=x"),
        Some("MULTIPART/mixed"),
    ] {
        let response = post_artifact(
            &fixture.router,
            Some(&fixture.auth),
            content_type,
            Body::from("png"),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type {content_type:?}"
        );
        let body = response_json(response).await;
        assert!(
            body["error"]["message"].is_string(),
            "415 must carry a JSON error: {body}"
        );
    }
}

#[tokio::test]
async fn artifact_upload_rejects_anonymous_requests_before_reading_the_body() {
    let fixture = artifact_upload_fixture().await;
    let polled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = polled.clone();
    let body = Body::from_stream(futures::stream::poll_fn(move |_| {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        std::task::Poll::Ready(None::<Result<bytes::Bytes, std::io::Error>>)
    }));

    let response = post_artifact(&fixture.router, None, Some("image/png"), body).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    );
    assert!(response_json(response).await["error"].is_object());
    assert!(
        !polled.load(std::sync::atomic::Ordering::SeqCst),
        "the request body must not be read before authentication"
    );

    let wrong_key = post_artifact(
        &fixture.router,
        Some("Bearer sk-not-a-real-key"),
        Some("image/png"),
        Body::from("png"),
    )
    .await;
    assert_eq!(wrong_key.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn artifact_repeated_http_uploads_keep_one_identity_and_download() {
    let bytes: &'static [u8] = include_bytes!("../../../tests/fixtures/media/transparent.png");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let api_key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Artifact identity key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            transparent_injections: Default::default(),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    let auth = format!("Bearer {}", api_key.token);
    let router = create_router(gateway.clone());

    let first = upload_ok(&router, &auth, "image/png", bytes.to_vec()).await;
    let second = upload_ok(&router, &auth, "image/png", bytes.to_vec()).await;
    assert_eq!(
        first["path"], second["path"],
        "identical uploads must resolve to one final Artifact identity: first={first} second={second}"
    );
    assert!(first.get("id").is_none());
    assert!(first.get("reference").is_none());
    assert!(
        first["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("stravia://artifacts/"))
    );
    assert_eq!(first["mime_type"], "image/png");
    assert_eq!(first["size"], serde_json::json!(bytes.len()));

    // The completed identity must still serve the stored bytes through the
    // real download route; no production endpoint is contacted.
    use stravia_runtime_contract::artifact::{ArtifactId, ArtifactSettings};
    let principal = stravia_runtime_contract::Principal::new(api_key.id.clone());
    let store = gateway.artifact_store.as_ref().expect("Artifact store");
    let settings = ArtifactSettings {
        client_base_url: "http://127.0.0.1:9".into(),
        ..Default::default()
    };
    let id = ArtifactId::from_reference(first["path"].as_str().expect("Artifact path"))
        .expect("Artifact path identity");
    let download = store
        .download(
            &principal,
            &id,
            std::time::Duration::from_secs(3600),
            &settings,
        )
        .await
        .expect("issue download");
    assert_eq!(download.artifact.id, id);
    let token = download
        .url
        .rsplit('/')
        .next()
        .expect("download token")
        .to_owned();
    let response = router
        .oneshot(
            Request::get(format!("/v1/artifacts/downloads/{token}"))
                .body(Body::empty())
                .expect("download request"),
        )
        .await
        .expect("download response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"].to_str().unwrap(),
        "image/png"
    );
    let downloaded = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("downloaded bytes");
    assert_eq!(downloaded.as_ref(), bytes);
}

async fn compress_body(encoding: &str, bytes: &[u8]) -> Vec<u8> {
    use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
    use tokio::io::AsyncWriteExt;
    macro_rules! encode {
        ($encoder:ty) => {{
            let mut encoder = <$encoder>::new(Vec::new());
            encoder.write_all(bytes).await.expect("encode request body");
            encoder.shutdown().await.expect("finish request body");
            encoder.into_inner()
        }};
    }
    match encoding {
        "gzip" => encode!(GzipEncoder<Vec<u8>>),
        // HTTP 的 deflate 是 zlib 封装，而不是裸 DEFLATE 流。
        "deflate" => encode!(ZlibEncoder<Vec<u8>>),
        "br" => encode!(BrotliEncoder<Vec<u8>>),
        "zstd" => encode!(ZstdEncoder<Vec<u8>>),
        other => panic!("unsupported test encoding {other}"),
    }
}

/// 压缩后只有几 KiB、解压后刚好超过代理上限的 zstd 请求体。
async fn zstd_body_just_over_the_limit() -> Vec<u8> {
    use async_compression::tokio::write::ZstdEncoder;
    use tokio::io::AsyncWriteExt;
    let chunk = vec![0_u8; 1024 * 1024];
    let mut encoder = ZstdEncoder::new(Vec::new());
    for _ in 0..=PROXY_JSON_BODY_LIMIT_BYTES / chunk.len() {
        encoder
            .write_all(&chunk)
            .await
            .expect("encode oversized body");
    }
    encoder.shutdown().await.expect("finish oversized body");
    let compressed = encoder.into_inner();
    assert!(compressed.len() < PROXY_JSON_BODY_LIMIT_BYTES / 100);
    compressed
}

async fn post_encoded(
    router: &Router,
    uri: &str,
    content_type: &str,
    encoding: &str,
    authorization: Option<&str>,
    body: Vec<u8>,
) -> axum::response::Response {
    let mut request = Request::post(uri)
        .header("content-type", content_type)
        .header("content-encoding", encoding);
    if let Some(authorization) = authorization {
        request = request.header("authorization", authorization);
    }
    router
        .clone()
        .oneshot(request.body(Body::from(body)).expect("encoded request"))
        .await
        .expect("encoded response")
}

#[tokio::test]
async fn compressed_artifact_uploads_store_the_decoded_file() {
    let bytes: &'static [u8] = include_bytes!("../../../tests/fixtures/media/transparent.png");
    let fixture = artifact_upload_fixture().await;
    let plain = upload_ok(&fixture.router, &fixture.auth, "image/png", bytes.to_vec()).await;

    for encoding in ["gzip", "deflate", "br", "zstd"] {
        let response = post_encoded(
            &fixture.router,
            "/v1/artifacts",
            "image/png",
            encoding,
            Some(&fixture.auth),
            compress_body(encoding, bytes).await,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "{encoding}");
        let body = response_json(response).await;
        // 内容身份按解码后的完整字节计算，与未压缩上传得到同一个 Artifact。
        assert_eq!(body["path"], plain["path"], "{encoding}");
        assert_eq!(body["size"], serde_json::json!(bytes.len()), "{encoding}");
    }
}

#[tokio::test]
async fn compressed_json_requests_behave_like_identity_requests() {
    let fixture = proxy_router().await;
    let json = br#"{"model":"missing","input":"hello"}"#;
    let plain = post_encoded(
        &fixture.router,
        "/v1/responses",
        "application/json",
        "identity",
        None,
        json.to_vec(),
    )
    .await;
    let expected_status = plain.status();
    let expected_body = response_json(plain).await;
    assert_ne!(
        expected_body["error"]["code"], "invalid_json",
        "control request must reach the handler: {expected_body}"
    );

    for encoding in ["gzip", "deflate", "br", "zstd"] {
        let response = post_encoded(
            &fixture.router,
            "/v1/responses",
            "application/json",
            encoding,
            None,
            compress_body(encoding, json).await,
        )
        .await;
        assert_eq!(response.status(), expected_status, "{encoding}");
        assert_eq!(response_json(response).await, expected_body, "{encoding}");
    }
    fixture.close().await;
}

#[tokio::test]
async fn compressed_request_size_limit_applies_after_decompression() {
    let fixture = proxy_router().await;
    let response = post_encoded(
        &fixture.router,
        "/v1/responses",
        "application/json",
        "zstd",
        None,
        zstd_body_just_over_the_limit().await,
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response_json(response).await["error"]["code"],
        "request_too_large"
    );
    fixture.close().await;
}

#[tokio::test]
async fn unsupported_or_corrupt_request_encodings_are_client_errors() {
    let fixture = artifact_upload_fixture().await;

    let unsupported = post_encoded(
        &fixture.router,
        "/v1/artifacts",
        "image/png",
        "snappy",
        Some(&fixture.auth),
        b"png".to_vec(),
    )
    .await;
    assert_eq!(unsupported.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    for encoding in ["gzip", "br", "zstd"] {
        let corrupt = post_encoded(
            &fixture.router,
            "/v1/artifacts",
            "image/png",
            encoding,
            Some(&fixture.auth),
            b"this is not a compressed stream".to_vec(),
        )
        .await;
        assert_eq!(corrupt.status(), StatusCode::BAD_REQUEST, "{encoding}");
        assert!(
            response_json(corrupt).await["error"]["message"].is_string(),
            "{encoding}"
        );
    }
}

async fn decompress_body(encoding: &str, bytes: &[u8]) -> Vec<u8> {
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
    use tokio::io::AsyncReadExt;
    macro_rules! decode {
        ($decoder:ty) => {{
            let mut decoded = Vec::new();
            <$decoder>::new(bytes)
                .read_to_end(&mut decoded)
                .await
                .expect("decode response body");
            decoded
        }};
    }
    match encoding {
        "gzip" => decode!(GzipDecoder<&[u8]>),
        "deflate" => decode!(ZlibDecoder<&[u8]>),
        "br" => decode!(BrotliDecoder<&[u8]>),
        "zstd" => decode!(ZstdDecoder<&[u8]>),
        other => panic!("unsupported test encoding {other}"),
    }
}

#[tokio::test]
async fn responses_are_compressed_with_the_encoding_the_client_accepts() {
    let fixture = proxy_router().await;
    let get = |accept_encoding: Option<&'static str>| {
        let mut request = Request::get("/v1/not-a-protocol-route");
        if let Some(accept_encoding) = accept_encoding {
            request = request.header("accept-encoding", accept_encoding);
        }
        fixture
            .router
            .clone()
            .oneshot(request.body(Body::empty()).expect("request"))
    };

    let plain = get(None).await.expect("identity response");
    assert!(plain.headers().get("content-encoding").is_none());
    let plain_body = to_bytes(plain.into_body(), usize::MAX)
        .await
        .expect("identity body");

    for encoding in ["gzip", "deflate", "br", "zstd"] {
        let response = get(Some(encoding)).await.expect("compressed response");
        assert_eq!(
            response.headers()["content-encoding"],
            encoding,
            "{encoding}"
        );
        assert_eq!(response.headers()["vary"], "accept-encoding", "{encoding}");
        assert!(response.headers().get("content-length").is_none());
        let compressed = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("compressed body");
        assert_eq!(
            decompress_body(encoding, &compressed).await,
            plain_body.as_ref(),
            "{encoding}"
        );
    }

    // Claude Code / Bun 同时声明四种编码，等权重时取 zstd。
    let negotiated = get(Some("gzip, deflate, br, zstd"))
        .await
        .expect("negotiated response");
    assert_eq!(negotiated.headers()["content-encoding"], "zstd");
    fixture.close().await;
}

#[tokio::test]
async fn streaming_and_already_compressed_responses_stay_identity_encoded() {
    use axum::http::header::CONTENT_TYPE;
    let large = "x".repeat(4096);
    let route = |content_type: &'static str| {
        let body = large.clone();
        get(move || async move { ([(CONTENT_TYPE, content_type)], body) })
    };
    let router = Router::new()
        .route("/json", route("application/json"))
        .route("/sse", route("text/event-stream"))
        .route("/png", route("image/png"))
        .route("/mp3", route("audio/mpeg"))
        .route("/mp4", route("video/mp4"))
        .layer(response_compression());

    for (path, compressed) in [
        ("/json", true),
        ("/sse", false),
        ("/png", false),
        ("/mp3", false),
        ("/mp4", false),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::get(path)
                    .header("accept-encoding", "gzip, br, zstd")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            response.headers().contains_key("content-encoding"),
            compressed,
            "{path}"
        );
    }
}

#[tokio::test]
async fn websocket_upgrade_is_not_encoded_when_the_client_accepts_compression() {
    use reqwest_websocket::Upgrade as _;
    let fixture = artifact_upload_fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let router = fixture.router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let upgrade = reqwest::Client::new()
        .get(format!("http://{address}/v1/responses"))
        .header("authorization", &fixture.auth)
        .header("accept-encoding", "gzip, br, zstd")
        .upgrade()
        .send()
        .await
        .expect("upgrade response");
    assert_eq!(upgrade.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert!(upgrade.headers().get("content-encoding").is_none());
    upgrade.into_websocket().await.expect("WebSocket");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn compressed_request_and_response_round_trip_through_the_inference_path() {
    let upstream = serve_responses_upstream().await;
    let (fixture, token) =
        protected_responses_router_with_base_url(true, None, &upstream.base_url).await;
    let request = br#"{"model":"auth-model","input":"hello"}"#;

    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("content-encoding", "zstd")
                .header("accept-encoding", "zstd")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(compress_body("zstd", request).await))
                .expect("compressed Responses request"),
        )
        .await
        .expect("Responses response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-encoding"], "zstd");
    let compressed = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("compressed Responses body");
    let body: serde_json::Value =
        serde_json::from_slice(&decompress_body("zstd", &compressed).await)
            .expect("Responses JSON");
    assert_eq!(body["output"][0]["content"][0]["text"], "authenticated");
    assert_eq!(
        upstream.requests.lock()[0]["input"][0]["content"][0]["text"],
        "hello"
    );
    fixture.close().await;
    upstream.close().await;
}

async fn serve_on_loopback(router: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve proxy router");
    });
    (address, server)
}

/// 手写 Upgrade 请求并发送一条未压缩的非法事件，返回 101 响应头与服务端回复帧的首字节。
/// RSV1 位（0x40）只在该帧经 permessage-deflate 压缩时置位。
async fn raw_websocket_exchange(
    address: std::net::SocketAddr,
    authorization: &str,
    extensions: Option<&str>,
) -> (String, u8) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to proxy");
    let mut request = format!(
        "GET /v1/responses HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: {authorization}\r\n"
    );
    if let Some(extensions) = extensions {
        request.push_str(&format!("Sec-WebSocket-Extensions: {extensions}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send upgrade request");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await.expect("handshake response"));
    }

    let payload = br#"{"type":"unknown.event"}"#;
    let mask = [1_u8, 2, 3, 4];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .zip(mask.iter().cycle())
            .map(|(byte, mask)| byte ^ mask),
    );
    stream.write_all(&frame).await.expect("send text frame");
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_u8())
        .await
        .expect("server frame timeout")
        .expect("server frame");
    (
        String::from_utf8(head)
            .expect("handshake response is ASCII")
            .to_ascii_lowercase(),
        first,
    )
}

#[tokio::test]
async fn websocket_frames_are_compressed_only_when_the_client_offers_permessage_deflate() {
    let fixture = artifact_upload_fixture().await;
    let (address, server) = serve_on_loopback(fixture.router.clone()).await;

    let (head, first) = raw_websocket_exchange(address, &fixture.auth, None).await;
    assert!(!head.contains("sec-websocket-extensions"), "{head}");
    assert_eq!(first, 0x81, "uncompressed text frame");

    let (head, first) = raw_websocket_exchange(
        address,
        &fixture.auth,
        Some("permessage-deflate; client_max_window_bits"),
    )
    .await;
    assert!(
        head.contains("sec-websocket-extensions: permessage-deflate\r\n"),
        "{head}"
    );
    assert_eq!(first, 0xC1, "text frame with RSV1 set");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn permessage_deflate_client_keeps_context_across_messages_in_both_directions() {
    use futures::{SinkExt as _, StreamExt as _};
    use tungstenite_deflate::{
        ClientRequestBuilder, Message, extensions::compression::deflate::DeflateConfig,
        protocol::WebSocketConfig,
    };

    let fixture = artifact_upload_fixture().await;
    let (address, server) = serve_on_loopback(fixture.router.clone()).await;
    let mut config = WebSocketConfig::default();
    config.extensions.permessage_deflate = Some(DeflateConfig::new());
    let request = ClientRequestBuilder::new(
        format!("ws://{address}/v1/responses")
            .parse()
            .expect("WebSocket URI"),
    )
    .with_header("authorization", fixture.auth.clone());
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to proxy");
    let (mut socket, response) =
        tokio_tungstenite_deflate::client_async_with_config(request, stream, Some(config))
            .await
            .expect("WebSocket handshake");
    assert_eq!(
        response.headers()["sec-websocket-extensions"],
        "permessage-deflate"
    );

    // 超过 deflate 窗口的重复载荷：解压必须沿用同一连接的上下文，而不是逐条独立。
    let padding = "x".repeat(1024 * 1024);
    for round in 0..3 {
        let event =
            serde_json::json!({"type": "unknown.event", "round": round, "padding": padding});
        socket
            .send(Message::text(event.to_string()))
            .await
            .expect("send compressed event");
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("reply timeout")
            .expect("reply")
            .expect("reply frame");
        let Message::Text(reply) = reply else {
            panic!("JSON text error event");
        };
        let body: serde_json::Value = serde_json::from_str(&reply).expect("error JSON");
        assert_eq!(body["type"], "error");
        assert_eq!(body["status"], 400);
        assert_eq!(
            body["error"]["message"],
            "WebSocket message type must be response.create."
        );
    }

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn plain_requests_to_the_websocket_endpoint_are_bad_requests() {
    let fixture = artifact_upload_fixture().await;
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::get("/v1/responses")
                .header("authorization", &fixture.auth)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
