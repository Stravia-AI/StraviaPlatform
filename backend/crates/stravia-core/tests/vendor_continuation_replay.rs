//! 真实 Vendor Wasm + Core Gateway/HTTP router + 隔离 SQLite + 回环上游：
//! 线程续接被拒绝或本地连接失效时，宿主在同一次逻辑 attempt 内完整重放一次。
//!
//! 前置：`task build:vendors:all`（读取 target/vendor-plugins-all）。上游、凭据均为假 fixture。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request as AxumRequest, State};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tower::ServiceExt;

use stravia_core::Gateway;
use stravia_core::config::GatewayConfig;
use stravia_core::data_paths::DataPaths;
use stravia_core::db::models::{
    CreateApiKey, CreateProviderRecord, CreateRoute, CreateTarget, RouteOutputModality,
    UpsertOAuthCredential,
};
use stravia_core::provider_models::CreateManualProviderModel;
use stravia_core::proxy::server::create_router;

#[path = "support/sqlite_fixture.rs"]
mod sqlite_fixture;
mod vendor_plugin_artifacts;
use vendor_plugin_artifacts::install_distributed_vendor_plugin;

#[path = "vendor_continuation_replay/codex.rs"]
mod codex;

const MODEL: &str = "claude-sonnet-4-5";
const ROUTE: &str = "claude-replay-route";
const COOLDOWN_MS: i64 = 400;

/// 按脚本依次应答的回环上游，并记录每个收到的请求体。
#[derive(Clone, Default)]
struct Upstream {
    script: Arc<Mutex<VecDeque<(u16, Value)>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

async fn serve_script(State(upstream): State<Upstream>, request: AxumRequest) -> Response {
    let bytes = to_bytes(request.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read upstream request");
    upstream
        .bodies
        .lock()
        .push(serde_json::from_slice(&bytes).expect("upstream request is JSON"));
    let (status, body) = upstream
        .script
        .lock()
        .pop_front()
        .expect("upstream script exhausted: unexpected extra request");
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header(
            "request-id",
            format!("req_{}", upstream.bodies.lock().len()),
        )
        .body(Body::from(body.to_string()))
        .expect("upstream response")
}

fn message(id: &str) -> (u16, Value) {
    (
        200,
        json!({
            "id": id, "type": "message", "role": "assistant", "model": MODEL,
            "content": [{"type": "text", "text": "READY"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }),
    )
}

fn error(status: u16, code: Option<&str>) -> (u16, Value) {
    let details = code.map_or(Value::Null, |code| json!({"error_code": code}));
    (
        status,
        json!({"type": "error", "error": {
            "type": "invalid_request_error", "message": "scripted failure", "details": details}}),
    )
}

struct Harness {
    gateway: Gateway,
    upstream: Upstream,
    token: String,
    _directory: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start() -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        sqlite_fixture::seed_database(directory.path()).await?;
        let gateway = Gateway::new(GatewayConfig {
            data_dir: directory.path().to_owned(),
            ..GatewayConfig::default()
        })
        .await?;
        install_distributed_vendor_plugin(&gateway, "claude-code").await?;

        let upstream = Upstream::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let router = Router::new()
            .fallback(serve_script)
            .with_state(upstream.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve upstream");
        });

        let provider = gateway
            .storage
            .providers()
            .create(CreateProviderRecord {
                name: "Claude replay fixture".into(),
                vendor: Some("claude-code".into()),
                protocol: "anthropic-messages".into(),
                base_url,
                preset_key: None,
                channel: Some("oauth".into()),
                models_source: None,
                static_models: None,
                api_key: String::new(),
                adapter_credentials: "{}".into(),
                vendor_options: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await?;
        gateway
            .storage
            .oauth_credentials()
            .upsert(
                &provider.id,
                UpsertOAuthCredential {
                    driver_key: "claude-code".into(),
                    scheme: "oauth_auth_code_pkce".into(),
                    access_token: "sk-ant-oat01-fixture".into(),
                    meta: Some(
                        json!({
                            "account_uuid": "00000000-0000-4000-8000-0000000000aa",
                            "device_id": "0123456789abcdef".repeat(4),
                        })
                        .to_string(),
                    ),
                    ..UpsertOAuthCredential::default()
                },
            )
            .await?;
        gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                MODEL,
                CreateManualProviderModel {
                    metadata: json!({
                        "id": MODEL, "name": MODEL,
                        "modalities": {"input": ["text", "image"], "output": ["text"]},
                        "limit": {"context": 200000, "output": 8192}
                    }),
                    template_id: None,
                },
            )
            .await?;
        // 重试预算为 0：任何一次被记录的上游失败都会立即让 Target 进入冷却。
        let route = gateway
            .admin()
            .create_model(CreateRoute {
                output_modality: RouteOutputModality::Text,
                model_id: ROUTE.into(),
                display_name: None,
                balance: None,
                targets: vec![CreateTarget {
                    enabled: true,
                    provider_id: provider.id.clone(),
                    model: MODEL.into(),
                    priority: Some(0),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(0),
                    target_cooldown_ms: Some(COOLDOWN_MS),
                    thinking_level_map: Vec::new(),
                }],
                default_thinking_level: None,
                billing: None,
            })
            .await?;
        let token = gateway
            .admin()
            .create_api_key(CreateApiKey {
                key: None,
                name: "Claude replay key".into(),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                inject_web_search: false,
                inject_media_generation: false,
                inject_media_understanding: false,
                model_ids: vec![route.id.to_string()],
                spend_limit: None,
            })
            .await?
            .token;
        Ok(Self {
            gateway,
            upstream,
            token,
            _directory: directory,
            _server: server,
        })
    }

    fn script(&self, replies: impl IntoIterator<Item = (u16, Value)>) {
        self.upstream.script.lock().extend(replies);
    }

    /// 同一会话的第 `turn` 轮：历史由客户端完整重放，线程续接由插件决定。
    async fn turn(&self, turn: usize) -> (StatusCode, Value) {
        let mut messages = vec![json!({"role": "user", "content": "Remember the marker."})];
        for index in 1..=turn {
            messages
                .push(json!({"role": "assistant", "content": [{"type": "text", "text": "READY"}]}));
            messages.push(json!({"role": "user", "content": format!("Continue {index}.")}));
        }
        self.messages(messages).await
    }

    async fn messages(&self, messages: Vec<Value>) -> (StatusCode, Value) {
        let response = create_router(self.gateway.clone())
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", self.token))
                    .header("x-api-key", &self.token)
                    .header("anthropic-version", "2023-06-01")
                    .header("x-session-id", "claude-replay-session")
                    .body(Body::from(
                        json!({"model": ROUTE, "max_tokens": 256, "messages": messages})
                            .to_string(),
                    ))
                    .expect("proxy request"),
            )
            .await
            .expect("proxy response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("proxy response body");
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"unparsed": String::from_utf8_lossy(&bytes)}));
        (status, body)
    }

    /// 持久化的 Target attempt 终态，按开始顺序：`(status, error_code)`。
    async fn attempts(&self) -> anyhow::Result<Vec<(String, Option<String>)>> {
        attempt_statuses(&self.gateway).await
    }

    async fn target_state(&self) -> anyhow::Result<String> {
        let statuses = self
            .gateway
            .admin()
            .get_model_target_statuses(ROUTE)
            .await?;
        Ok(serde_json::to_value(&statuses[0])?["state"]
            .as_str()
            .expect("target state")
            .to_owned())
    }
}

async fn attempt_statuses(gateway: &Gateway) -> anyhow::Result<Vec<(String, Option<String>)>> {
    gateway.admin().observation_flush().await?;
    let paths = DataPaths::new(&gateway.config.data_dir);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(paths.database())
                .read_only(true),
        )
        .await?;
    let rows = sqlx::query_as(
        "SELECT status, error_code FROM target_attempt_observations ORDER BY started_at, id",
    )
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    Ok(rows)
}

fn completed(count: usize) -> Vec<(String, Option<String>)> {
    vec![("completed".to_owned(), None); count]
}

/// 线程失效 → 宿主完整重放含图工具结果：只记录成功 attempt，没有失败事件。
#[tokio::test]
async fn rejected_thread_is_replayed_inside_one_completed_attempt() -> anyhow::Result<()> {
    use base64::Engine as _;

    let harness = Harness::start().await?;
    harness.script([
        message("msg_1"),
        error(404, Some("thread_not_found")),
        message("msg_2"),
    ]);

    assert_eq!(harness.turn(0).await.0, StatusCode::OK);
    let source = json!({
        "type": "base64", "media_type": "image/png",
        "data": base64::engine::general_purpose::STANDARD.encode(include_bytes!("fixtures/media/transparent.png"))
    });
    let (status, body) = harness.messages(vec![
        json!({"role": "user", "content": "Remember the marker."}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "READY"}]}),
        json!({"role": "user", "content": "Inspect the screenshot."}),
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "read_image", "name": "read_image", "input": {}}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "read_image", "content": [
            {"type": "text", "text": "Screenshot"}, {"type": "image", "source": source}
        ]}]}),
    ]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content"][0]["text"], "READY");

    {
        let sent = harness.upstream.bodies.lock();
        assert_eq!(sent.len(), 3, "one rejected send and exactly one replay");
        assert_eq!(sent[1]["thread"]["type"], "continue");
        assert_eq!(sent[1]["messages"].as_array().unwrap().len(), 3);
        assert!(sent[2].get("thread").is_none());
        assert_eq!(sent[2]["messages"].as_array().unwrap().len(), 5);
        assert_eq!(
            sent[1]["messages"][2]["content"][0]["content"][1]["source"],
            source
        );
        assert_eq!(
            sent[2]["messages"][4]["content"][0]["content"][1]["source"],
            source
        );
    }

    // 被消化的线程拒绝不是 attempt：两轮各一条成功，没有任何 failed 记录。
    assert_eq!(harness.attempts().await?, completed(2));
    assert_eq!(harness.target_state().await?, "available");
    Ok(())
}

/// 重放后的第二次真实错误仍是最终失败：观测为失败，且不会再重放第三次。
#[tokio::test]
async fn second_real_error_after_replay_is_an_observed_failure() -> anyhow::Result<()> {
    let harness = Harness::start().await?;
    harness.script([
        message("msg_1"),
        error(404, Some("thread_not_found")),
        error(400, None),
    ]);

    assert_eq!(harness.turn(0).await.0, StatusCode::OK);
    let (status, body) = harness.turn(1).await;
    assert!(!status.is_success(), "{body}");
    assert_eq!(harness.upstream.bodies.lock().len(), 3);

    let attempts = harness.attempts().await?;
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert_eq!(attempts[0].0, "completed");
    assert_eq!(attempts[1].0, "failed", "{attempts:?}");
    Ok(())
}

/// 半开探测同样可以在单个探测 attempt 内完成恢复：成功后 Target 回到 Available。
#[tokio::test]
async fn half_open_probe_recovers_thread_rejection_in_its_single_attempt() -> anyhow::Result<()> {
    let harness = Harness::start().await?;
    harness.script([
        message("msg_1"),
        error(503, None),
        error(404, Some("thread_not_found")),
        message("msg_2"),
    ]);

    assert_eq!(harness.turn(0).await.0, StatusCode::OK);
    // 重试预算 0：一次上游失败即冷却；插件未提交线程状态，下次仍是 continue。
    assert!(!harness.turn(1).await.0.is_success());
    // 慢速 runner 读取状态时可能已经跨过冷却截止时间。
    let state = harness.target_state().await?;
    assert!(
        matches!(state.as_str(), "cooling_down" | "half_open"),
        "{state}"
    );
    tokio::time::sleep(Duration::from_millis(COOLDOWN_MS as u64 + 150)).await;
    assert_eq!(harness.target_state().await?, "half_open");

    let (status, body) = harness.turn(1).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    {
        let sent = harness.upstream.bodies.lock();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[2]["thread"]["type"], "continue");
        assert!(sent[3].get("thread").is_none());
        assert_eq!(sent[3]["messages"].as_array().unwrap().len(), 3);
    }

    // 仅冷却前的 503 是失败；探测 attempt 本身是一条成功，并关闭了熔断。
    let attempts = harness.attempts().await?;
    assert_eq!(attempts.len(), 3, "{attempts:?}");
    assert_eq!(attempts[0].0, "completed");
    assert_eq!(attempts[1].0, "failed");
    assert_eq!(attempts[2], ("completed".to_owned(), None));
    assert_eq!(harness.target_state().await?, "available");
    Ok(())
}
