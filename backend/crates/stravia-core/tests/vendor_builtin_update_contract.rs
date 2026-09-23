use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request as AxumRequest, State};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use stravia_core::Gateway;
use stravia_core::config::GatewayConfig;
use stravia_core::data_paths::DataPaths;
use stravia_core::db::models::{
    CreateApiKey, CreateProvider, CreateRoute, ProviderCredentialInput, ProviderSourceInput,
};
use stravia_core::plugin::{ConfirmPluginUpdate, PluginSource, PluginSummary};
use stravia_core::provider_models::CreateManualProviderModel;
use stravia_core::proxy::server::create_router;
use stravia_runtime_contract::protocol::ir::AiResponse;
use tokio::sync::mpsc;
use tower::ServiceExt;

async fn gateway(directory: &Path) -> anyhow::Result<Gateway> {
    Gateway::new(GatewayConfig {
        data_dir: directory.to_owned(),
        ..GatewayConfig::default()
    })
    .await
}

async fn installed(gateway: &Gateway) -> anyhow::Result<PluginSummary> {
    gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "base")
        .ok_or_else(|| anyhow::anyhow!("base plugin is missing"))
}

async fn install(gateway: &Gateway, artifact: &str) -> anyhow::Result<PluginSummary> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-test-fixtures")
        .join(artifact);
    let preview = gateway
        .admin()
        .preview_vendor_plugin(std::fs::read(path)?)
        .await?;
    assert!(preview.discarded_data.is_empty());
    assert_eq!(preview.target_source, PluginSource::Local);
    gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await
}

async fn seed_previous_builtin(directory: &Path) -> anyhow::Result<()> {
    // 模拟上一发行版已持久化的内置包；管理接口仍必须将导入的测试包标为本地来源。
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

#[tokio::test]
async fn a_newer_bundle_automatically_updates_a_trusted_installation_and_its_origins()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let first = gateway(directory.path()).await?;
    let bundled = installed(&first).await?;
    let previous = install(&first, "lifecycle-base-older.wasm").await?;
    assert!(semver::Version::parse(&bundled.version)? > semver::Version::parse(&previous.version)?);
    let _connection = connection(&first, "http://127.0.0.1:19327/v1").await?;
    let preview = first.admin().preview_builtin_vendor_plugin("base").await?;
    let added_origins: Vec<_> = preview
        .network_permissions
        .iter()
        .filter(|permission| permission.added)
        .map(|permission| permission.origin.clone())
        .collect();
    assert!(
        !added_origins.is_empty(),
        "the bundled implementation adds actual fixed origins"
    );
    drop(first);
    seed_previous_builtin(directory.path()).await?;

    let restarted = gateway(directory.path()).await?;
    let updated = installed(&restarted).await?;
    assert_eq!(updated.source, PluginSource::Builtin);
    assert_eq!(updated.version, bundled.version);
    assert_eq!(updated.name, bundled.name);
    assert_eq!(updated.status, "ready");
    assert!(updated.pending_update.is_none());
    let current = restarted
        .admin()
        .preview_builtin_vendor_plugin("base")
        .await?;
    for origin in added_origins {
        assert!(
            current
                .network_permissions
                .iter()
                .any(|permission| permission.origin == origin && !permission.added)
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_bundle_does_not_replace_equal_version_bytes_or_downgrade_a_trusted_installation()
-> anyhow::Result<()> {
    for artifact in ["lifecycle-base-same.wasm", "lifecycle-base-local.wasm"] {
        let directory = tempfile::tempdir()?;
        let first = gateway(directory.path()).await?;
        let bundled = installed(&first).await?;
        let previous = install(&first, artifact).await?;
        assert_ne!(previous.name, bundled.name);
        assert!(
            semver::Version::parse(&previous.version)? >= semver::Version::parse(&bundled.version)?
        );
        drop(first);
        seed_previous_builtin(directory.path()).await?;

        let restarted = gateway(directory.path()).await?;
        let retained = installed(&restarted).await?;
        assert_eq!(retained.source, PluginSource::Builtin);
        assert_eq!(retained.version, previous.version);
        assert_eq!(retained.name, previous.name);
        assert_eq!(retained.status, "ready");
        assert!(retained.pending_update.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn even_a_newer_bundle_cannot_replace_a_local_installation() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let first = gateway(directory.path()).await?;
    let bundled = installed(&first).await?;
    let previous = install(&first, "lifecycle-base-older.wasm").await?;
    assert!(semver::Version::parse(&bundled.version)? > semver::Version::parse(&previous.version)?);
    drop(first);

    let restarted = gateway(directory.path()).await?;
    let retained = installed(&restarted).await?;
    assert_eq!(retained.source, PluginSource::Local);
    assert_eq!(retained.version, previous.version);
    assert_eq!(retained.name, previous.name);
    assert!(retained.pending_update.is_none());
    Ok(())
}

struct Connection {
    provider_id: String,
    route_id: String,
    token: String,
}

async fn connection(gateway: &Gateway, base_url: &str) -> anyhow::Result<Connection> {
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Retained builtin connection".into()),
            source: ProviderSourceInput::Custom {
                vendor: "openai".into(),
                channel: "default".into(),
                protocol: None,
                base_url: base_url.into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::Fields {
                values: BTreeMap::from([(
                    "apiKey".into(),
                    serde_json::json!("retained-upstream-secret"),
                )]),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "fixture-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({"id":"fixture-model", "name":"Fixture model"}),
                template_id: None,
            },
        )
        .await?;
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "builtin-update-route".into(),
            display_name: None,
            balance: None,
            targets: vec![stravia_core::db::models::CreateTarget {
                provider_id: provider.id.clone(),
                model: Some("fixture-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;
    let key = gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "Builtin update regression".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![route.id.clone().into()],
        })
        .await?;
    Ok(Connection {
        provider_id: provider.id,
        route_id: route.id.into(),
        token: key.token,
    })
}

struct Upstream {
    base_url: String,
    requests: mpsc::UnboundedReceiver<axum::http::request::Parts>,
    task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    async fn start() -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        let (sender, requests) = mpsc::unbounded_channel();
        let router = Router::new().fallback(upstream_reply).with_state(sender);
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve owned upstream");
        });
        Ok(Self {
            base_url,
            requests,
            task,
        })
    }

    async fn assert_request(&mut self, state_before: Option<&str>) {
        let request = self.requests.recv().await.expect("upstream request");
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Bearer retained-upstream-secret"
        );
        assert_eq!(
            request
                .headers
                .get("x-state-before")
                .map(|value| value.to_str().unwrap()),
            state_before
        );
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn upstream_reply(
    State(sender): State<mpsc::UnboundedSender<axum::http::request::Parts>>,
    request: AxumRequest,
) -> Response {
    let (parts, body) = request.into_parts();
    to_bytes(body, 1024 * 1024)
        .await
        .expect("read upstream request");
    let reply = if parts.uri.path().ends_with("/infer") {
        let mut reply = AiResponse::new("builtin-fixture", "fixture-model");
        reply.push_output_text("retained connection works");
        serde_json::to_vec(&reply).unwrap()
    } else {
        assert!(
            parts.uri.path().ends_with("/responses"),
            "unexpected upstream URI: {}",
            parts.uri
        );
        serde_json::to_vec(
            &stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                "builtin-fixture",
                "fixture-model",
                "completed",
                vec![serde_json::json!({
                "id":"builtin-message",
                "type":"message",
                "role":"assistant",
                "status":"completed",
                "content":[{"type":"output_text","text":"retained connection works","annotations":[]}]
                })],
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::json!({
                    "input_tokens":1,"output_tokens":1,"total_tokens":2,
                    "input_tokens_details":{"cached_tokens":0},
                    "output_tokens_details":{"reasoning_tokens":0}
                }),
            ),
        )
        .unwrap()
    };
    sender.send(parts).expect("capture upstream request");
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(reply))
        .unwrap()
}

async fn invoke(gateway: &Gateway, connection: &Connection) -> anyhow::Result<()> {
    let response = create_router(gateway.clone()).oneshot(
        Request::post("/v1/responses")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", connection.token))
            .body(Body::from(serde_json::json!({"model":connection.route_id,"input":"verify retained connection"}).to_string()))?
    ).await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let response: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(response["output"].as_array().unwrap().iter().any(|item| {
        item["content"].as_array().is_some_and(|content| {
            content
                .iter()
                .any(|part| part["text"] == "retained connection works")
        })
    }));
    Ok(())
}

#[tokio::test]
async fn an_incompatible_builtin_waits_for_consent_and_discards_only_the_incompatible_state()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let first = gateway(directory.path()).await?;
    let bundled = installed(&first).await?;
    install(&first, "lifecycle-base-incompatible.wasm").await?;
    let mut upstream = Upstream::start().await?;
    let connection = connection(&first, &upstream.base_url).await?;
    invoke(&first, &connection).await?;
    upstream.assert_request(Some("0")).await;
    drop(first);
    seed_previous_builtin(directory.path()).await?;

    let restarted = gateway(directory.path()).await?;
    let waiting = installed(&restarted).await?;
    assert_eq!(waiting.version, "0.0.0");
    assert_eq!(waiting.status, "pending_update");
    let preview = waiting.pending_update.expect("data-discard confirmation");
    assert_eq!(preview.new_version, bundled.version);
    assert_eq!(preview.discarded_data.len(), 1);
    assert_eq!(
        preview.discarded_data[0].provider.id,
        connection.provider_id
    );
    assert_eq!(preview.discarded_data[0].kinds, ["private_state"]);
    assert!(preview.discarded_data[0].recovery_actions.is_empty());
    let rejected = restarted
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id.clone(),
            allow_data_discard: false,
        })
        .await;
    assert!(rejected.is_err());
    invoke(&restarted, &connection).await?;
    upstream.assert_request(Some("1")).await;

    // 活跃旧版本可继续改变待丢弃状态；重新预览再确认，避免使用已过期的数据快照。
    let refreshed = restarted
        .admin()
        .preview_builtin_vendor_plugin("base")
        .await?;
    restarted
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: refreshed.id,
            allow_data_discard: true,
        })
        .await?;
    let updated = installed(&restarted).await?;
    assert_eq!(updated.source, PluginSource::Builtin);
    assert_eq!(updated.version, bundled.version);
    assert_eq!(updated.status, "ready");
    assert_eq!(
        restarted
            .admin()
            .get_provider(&connection.provider_id)
            .await?
            .id,
        connection.provider_id
    );
    invoke(&restarted, &connection).await?;
    upstream.assert_request(None).await;

    // 相同新格式的探针不触发第二次清理；它应读到已确认更新留下的空状态。
    install(&restarted, "lifecycle-base-same.wasm").await?;
    invoke(&restarted, &connection).await?;
    upstream.assert_request(Some("0")).await;
    Ok(())
}
