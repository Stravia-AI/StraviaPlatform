use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request as AxumRequest, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::Response;
use sqlx::Connection;
use stravia_core::Gateway;
use stravia_core::admin::ForestQuery;
use stravia_core::auth::{
    AuthCompletionInput, AuthCompletionValue, AuthSessionCandidate, AuthSessionStatusData,
    OAuthCallbackMode, OAuthSessionStartOptions,
};
use stravia_core::config::GatewayConfig;
use stravia_core::data_paths::DataPaths;
use stravia_core::db::models::{
    CreateApiKey, CreateProvider, CreateRoute, ProviderCredentialInput, ProviderSourceInput,
    UpdateProvider,
};
use stravia_core::plugin::ConfirmPluginUpdate;
use stravia_core::proxy::server::create_router;
use stravia_runtime_contract::protocol::ir::AiResponse;
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

const MANAGEMENT_VENDOR: &str = "fixture.management";
const OTHER_VENDOR: &str = "fixture.management.other";
const MODEL_ID: &str = "management-model";
const PAGINATED_MODEL_ID: &str = "management-model-paginated";

struct PendingUpstream {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Vec<u8>,
    response: oneshot::Sender<UpstreamReply>,
}

impl PendingUpstream {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn operation(&self) -> &str {
        self.header("x-management-operation")
            .expect("management operation header")
    }

    fn reply(self, reply: UpstreamReply) {
        let _ = self.response.send(reply);
    }
}

struct UpstreamReply {
    status: StatusCode,
    body: Vec<u8>,
}

impl UpstreamReply {
    fn json(value: serde_json::Value) -> Self {
        Self {
            status: StatusCode::OK,
            body: serde_json::to_vec(&value).expect("serialize upstream response"),
        }
    }

    fn token(access_token: &str, refresh_token: &str) -> Self {
        Self::json(serde_json::json!({
            "access_token": access_token,
            "refresh_token": refresh_token,
            "expires_at_unix_ms": 4_102_444_800_000_i64
        }))
    }

    fn models() -> Self {
        Self::model_page(MODEL_ID, "Management Model", None)
    }

    fn model_page(model_id: &str, display_name: &str, next_cursor: Option<&str>) -> Self {
        Self::json(serde_json::json!({
            "models": [{
                "id": model_id,
                "display_name": display_name,
                "family": "management",
                "selector": "management-selector"
            }],
            "next_cursor": next_cursor
        }))
    }

    fn allowance(remaining: &str) -> Self {
        Self::json(serde_json::json!({ "remaining": remaining }))
    }

    fn inference(text: &str) -> Self {
        let mut response = AiResponse::new("management-response", MODEL_ID);
        response.push_output_text(text);
        response.usage.prompt_tokens = 11;
        response.usage.completion_tokens = 7;
        response.usage.total_tokens = 18;
        response.usage.required_components_known = true;
        response.usage.cache_read_tokens = Some(2);
        Self::json(serde_json::to_value(response).expect("serialize canonical response"))
    }
}

struct TestUpstream {
    base_url: String,
    requests: mpsc::UnboundedReceiver<PendingUpstream>,
    task: tokio::task::JoinHandle<()>,
}

impl TestUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind management fixture upstream");
        let address = listener.local_addr().expect("management upstream address");
        let (sender, requests) = mpsc::unbounded_channel();
        let router = Router::new().fallback(capture_upstream).with_state(sender);
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve management fixture upstream");
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            task,
        }
    }

    async fn next(&mut self) -> PendingUpstream {
        self.requests
            .recv()
            .await
            .expect("management fixture upstream request")
    }

    async fn take_operations(&mut self, count: usize) -> HashMap<String, PendingUpstream> {
        let mut requests = HashMap::new();
        for _ in 0..count {
            let request = self.next().await;
            let operation = request.operation().to_owned();
            assert!(
                requests.insert(operation.clone(), request).is_none(),
                "duplicate active management operation: {operation}"
            );
        }
        requests
    }

    fn assert_no_request(&mut self) {
        assert!(
            self.requests.try_recv().is_err(),
            "an unavailable Target must not reach the upstream"
        );
    }
}

impl Drop for TestUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn capture_upstream(
    State(sender): State<mpsc::UnboundedSender<PendingUpstream>>,
    request: AxumRequest,
) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 4 * 1024 * 1024)
        .await
        .expect("read management fixture request")
        .to_vec();
    let (response, receiver) = oneshot::channel();
    sender
        .send(PendingUpstream {
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body,
            response,
        })
        .expect("test receives management fixture request");
    let reply = receiver.await.unwrap_or(UpstreamReply {
        status: StatusCode::GONE,
        body: Vec::new(),
    });
    Response::builder()
        .status(reply.status)
        .header("content-type", "application/json")
        .body(Body::from(reply.body))
        .expect("management upstream response")
}

async fn new_gateway(data_dir: PathBuf) -> anyhow::Result<Gateway> {
    Gateway::new(GatewayConfig {
        data_dir,
        ..GatewayConfig::default()
    })
    .await
}

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-test-fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "missing management fixture {} ({error}); run `task build:vendor-fixtures`",
            path.display()
        )
    })
}

async fn install(
    gateway: &Gateway,
    artifact: &str,
    allow_data_discard: bool,
) -> anyhow::Result<stravia_core::plugin::PluginSummary> {
    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture(artifact))
        .await?;
    gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard,
        })
        .await
}

fn options() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([("workspace".into(), serde_json::json!("contract-workspace"))])
}

fn provider_input(vendor: &str, name: &str, base_url: &str) -> CreateProvider {
    CreateProvider {
        name: Some(name.into()),
        source: ProviderSourceInput::Custom {
            vendor: vendor.into(),
            channel: "default".into(),
            protocol: Some("fixture-management".into()),
            base_url: base_url.into(),
            models_source: None,
            static_models: None,
        },
        credential: ProviderCredentialInput::None,
        vendor_options: options().into_iter().collect(),
        use_proxy: false,
    }
}

async fn begin_oauth(
    gateway: &Gateway,
    vendor: &str,
    provider_id: Option<String>,
    base_url: &str,
) -> anyhow::Result<stravia_core::auth::AuthSessionInitData> {
    gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: vendor.into(),
                channel: "default".into(),
                provider_id,
                base_url: base_url.into(),
                protocol: Some("fixture-management".into()),
                options: options(),
                credentials: BTreeMap::new(),
                use_proxy: false,
            },
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "http://127.0.0.1:18765/callback".into(),
                listener_port: None,
                fallback_reason: None,
            },
        )
        .await
}

fn completion(started: &stravia_core::auth::AuthSessionInitData) -> AuthCompletionInput {
    let state = url::Url::parse(started.auth_url.as_deref().expect("fixture auth URL"))
        .expect("parse fixture auth URL")
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("fixture OAuth state");
    AuthCompletionInput {
        input: AuthCompletionValue::CallbackUrl {
            value: format!("{}?code=fixture-code&state={state}", started.redirect_uri),
        },
    }
}

async fn create_oauth_provider(
    gateway: &Gateway,
    upstream: &mut TestUpstream,
    vendor: &str,
    name: &str,
    access_token: &str,
) -> anyhow::Result<stravia_core::db::models::Provider> {
    let started = begin_oauth(gateway, vendor, None, &upstream.base_url).await?;
    let gw = gateway.clone();
    let session_id = started.session_id.clone();
    let input = completion(&started);
    let exchange =
        tokio::spawn(async move { gw.admin().complete_oauth_session(&session_id, input).await });
    let request = upstream.next().await;
    assert_eq!(request.method, Method::POST);
    assert_eq!(request.uri.path(), "/oauth/exchange");
    assert_eq!(request.operation(), "oauth_exchange");
    assert_eq!(request.header("x-management-vendor"), Some(vendor));
    request.reply(UpstreamReply::token(access_token, "fixture-refresh-token"));
    assert!(matches!(
        exchange.await??,
        AuthSessionStatusData::Ready { .. }
    ));
    gateway
        .admin()
        .create_provider_with_oauth_session(
            &started.session_id,
            provider_input(vendor, name, &upstream.base_url),
        )
        .await
}

async fn sync_models(
    gateway: &Gateway,
    upstream: &mut TestUpstream,
    provider_id: &str,
) -> anyhow::Result<()> {
    let gw = gateway.clone();
    let provider_id = provider_id.to_owned();
    let sync = tokio::spawn(async move { gw.admin().sync_provider_models(&provider_id).await });
    let request = upstream.next().await;
    assert_eq!(request.uri.path(), "/models");
    assert_eq!(request.operation(), "model_discovery");
    request.reply(UpstreamReply::models());
    let summary = sync.await??;
    assert_eq!(summary.added + summary.restored, 1);
    Ok(())
}

async fn create_route(
    gateway: &Gateway,
    provider_id: &str,
    route_id: &str,
) -> anyhow::Result<stravia_core::db::models::RouteConfig> {
    gateway
        .admin()
        .create_model(CreateRoute {
            model_id: route_id.into(),
            display_name: Some("Management Lifecycle Route".into()),
            balance: None,
            targets: vec![stravia_core::db::models::CreateTarget {
                provider_id: provider_id.into(),
                model: Some(MODEL_ID.into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
}

async fn api_key(gateway: &Gateway, route_id: &str) -> anyhow::Result<String> {
    Ok(gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "Management lifecycle key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![route_id.into()],
        })
        .await?
        .token)
}

async fn invoke(router: Router, token: String, model: String) -> (StatusCode, serde_json::Value) {
    let response = router
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({
                        "model": model,
                        "input": "exercise management lifecycle"
                    })
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
        .unwrap_or_else(|_| serde_json::json!({ "unparsed": String::from_utf8_lossy(&bytes) }));
    (status, body)
}

async fn allowance_sample_count(data_dir: &Path, provider_id: &str) -> anyhow::Result<i64> {
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(DataPaths::new(data_dir).database()),
    )
    .await?;
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM provider_allowance_samples WHERE provider_id = ?")
            .bind(provider_id)
            .fetch_one(&mut connection)
            .await?,
    )
}

#[tokio::test]
async fn compatible_update_keeps_old_management_work_and_routes_new_work_to_v2()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "management-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let provider = create_oauth_provider(
        &gateway,
        &mut upstream,
        MANAGEMENT_VENDOR,
        "Compatible management provider",
        "compatible-access-token",
    )
    .await?;

    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let old_discovery =
        tokio::spawn(async move { gw.admin().sync_provider_models(&provider_id).await });
    let old_request = upstream.next().await;
    assert_eq!(old_request.operation(), "model_discovery");
    assert!(!old_request.body.is_empty());
    assert_eq!(old_request.header("x-management-version"), Some("1.0.0"));

    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("management-v2.wasm"))
        .await?;
    assert!(!preview.cancels_active_operations);
    assert!(preview.active_operations >= 1);
    let updated = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    assert_eq!(updated.version, "2.0.0");

    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let new_allowance =
        tokio::spawn(async move { gw.admin().refresh_provider_allowance(&provider_id).await });
    let new_request = upstream.next().await;
    assert_eq!(new_request.operation(), "allowance");
    assert_eq!(new_request.header("x-management-version"), Some("2.0.0"));
    new_request.reply(UpstreamReply::allowance("82"));
    let allowance = new_allowance.await??.expect("allowance snapshot");
    assert_eq!(allowance.plan_label.as_deref(), Some("fixture-2.0.0"));

    old_request.reply(UpstreamReply::models());
    let summary = old_discovery.await??;
    assert_eq!(summary.added, 1);
    let model = gateway
        .admin()
        .get_provider_model(&provider.id, MODEL_ID)
        .await?;
    assert_eq!(model.metadata.extensions["fixture_version"], "1.0.0");
    Ok(())
}

#[tokio::test]
async fn compatible_update_keeps_oauth_401_refresh_and_replay_on_the_original_component()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "management-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let provider = create_oauth_provider(
        &gateway,
        &mut upstream,
        MANAGEMENT_VENDOR,
        "Pinned OAuth recovery provider",
        "expired-access-token",
    )
    .await?;
    sync_models(&gateway, &mut upstream, &provider.id).await?;
    let route = create_route(&gateway, &provider.id, "management-pinned-oauth-recovery").await?;
    let token = api_key(&gateway, &route.id).await?;
    let router = create_router(gateway.clone());

    let mut call = tokio::spawn(invoke(router, token, route.model_id.into()));
    let rejected = upstream.next().await;
    assert_eq!(rejected.operation(), "infer");
    assert_eq!(rejected.header("x-management-version"), Some("1.0.0"));
    assert_eq!(
        rejected.header("authorization"),
        Some("Bearer expired-access-token")
    );

    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("management-v2.wasm"))
        .await?;
    assert!(!preview.cancels_active_operations);
    assert!(preview.active_operations >= 1);
    let updated = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    assert_eq!(updated.version, "2.0.0");

    rejected.reply(UpstreamReply {
        status: StatusCode::UNAUTHORIZED,
        body: br#"{"error":{"type":"authentication_error","message":"expired token"}}"#.to_vec(),
    });
    let refresh = tokio::select! {
        request = upstream.next() => request,
        result = &mut call => panic!("inference ended before OAuth refresh: {result:?}"),
    };
    assert_eq!(refresh.operation(), "oauth_refresh");
    assert_eq!(refresh.header("x-management-version"), Some("1.0.0"));
    assert_eq!(
        refresh.header("authorization"),
        Some("Bearer expired-access-token")
    );
    refresh.reply(UpstreamReply::token(
        "refreshed-access-token",
        "rotated-refresh-token",
    ));

    let replay = tokio::select! {
        request = upstream.next() => request,
        result = &mut call => panic!("inference ended before OAuth replay: {result:?}"),
    };
    assert_eq!(replay.operation(), "infer");
    assert_eq!(
        replay.header("x-management-version"),
        Some("1.0.0"),
        "OAuth recovery must stay on the original compatible component"
    );
    assert_eq!(
        replay.header("authorization"),
        Some("Bearer refreshed-access-token")
    );
    replay.reply(UpstreamReply::inference("OAuth recovery stayed pinned"));

    let response = call.await?;
    assert_eq!(response.0, StatusCode::OK, "{}", response.1);
    assert!(
        response
            .1
            .to_string()
            .contains("OAuth recovery stayed pinned")
    );
    upstream.assert_no_request();
    Ok(())
}

#[tokio::test]
async fn compatible_update_pins_paginated_model_sync_to_one_plugin_version() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "management-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let provider = create_oauth_provider(
        &gateway,
        &mut upstream,
        MANAGEMENT_VENDOR,
        "Paginated management provider",
        "paginated-access-token",
    )
    .await?;

    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let old_sync = tokio::spawn(async move { gw.admin().sync_provider_models(&provider_id).await });
    let first_old_page = upstream.next().await;
    assert_eq!(first_old_page.operation(), "model_discovery");
    assert_eq!(first_old_page.header("x-management-version"), Some("1.0.0"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first_old_page.body)?["cursor"],
        serde_json::Value::Null
    );

    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("management-v2.wasm"))
        .await?;
    assert!(!preview.cancels_active_operations);
    assert!(preview.active_operations >= 1);
    let updated = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    assert_eq!(updated.version, "2.0.0");

    first_old_page.reply(UpstreamReply::model_page(
        MODEL_ID,
        "Management Model Page One",
        Some("v1-page-2"),
    ));
    let second_old_page = upstream.next().await;
    assert_eq!(second_old_page.operation(), "model_discovery");
    assert_eq!(
        second_old_page.header("x-management-version"),
        Some("1.0.0")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&second_old_page.body)?["cursor"],
        "v1-page-2"
    );
    second_old_page.reply(UpstreamReply::model_page(
        PAGINATED_MODEL_ID,
        "Management Model Page Two",
        None,
    ));
    let old_summary = old_sync.await??;
    assert_eq!(old_summary.added, 2);

    let mut old_model_ids = gateway
        .admin()
        .list_provider_models(&provider.id)
        .await?
        .models
        .into_iter()
        .map(|model| model.id)
        .collect::<Vec<_>>();
    old_model_ids.sort();
    assert_eq!(
        old_model_ids,
        [MODEL_ID.to_owned(), PAGINATED_MODEL_ID.to_owned()]
    );
    for (model_id, display_name) in [
        (MODEL_ID, "Management Model Page One"),
        (PAGINATED_MODEL_ID, "Management Model Page Two"),
    ] {
        let model = gateway
            .admin()
            .get_provider_model(&provider.id, model_id)
            .await?;
        assert!(model.available);
        assert_eq!(model.metadata.name.as_deref(), Some(display_name));
        assert_eq!(model.metadata.family.as_deref(), Some("management"));
        assert_eq!(model.metadata.extensions["selector"], "management-selector");
        assert_eq!(model.metadata.extensions["fixture_version"], "1.0.0");
    }

    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let new_sync = tokio::spawn(async move { gw.admin().sync_provider_models(&provider_id).await });
    let first_new_page = upstream.next().await;
    assert_eq!(first_new_page.operation(), "model_discovery");
    assert_eq!(first_new_page.header("x-management-version"), Some("2.0.0"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first_new_page.body)?["cursor"],
        serde_json::Value::Null
    );
    first_new_page.reply(UpstreamReply::model_page(
        MODEL_ID,
        "Management Model Page One",
        Some("v2-page-2"),
    ));
    let second_new_page = upstream.next().await;
    assert_eq!(second_new_page.operation(), "model_discovery");
    assert_eq!(
        second_new_page.header("x-management-version"),
        Some("2.0.0")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&second_new_page.body)?["cursor"],
        "v2-page-2"
    );
    second_new_page.reply(UpstreamReply::model_page(
        PAGINATED_MODEL_ID,
        "Management Model Page Two",
        None,
    ));
    new_sync.await??;

    let mut new_model_ids = gateway
        .admin()
        .list_provider_models(&provider.id)
        .await?
        .models
        .into_iter()
        .map(|model| model.id)
        .collect::<Vec<_>>();
    new_model_ids.sort();
    assert_eq!(
        new_model_ids,
        [MODEL_ID.to_owned(), PAGINATED_MODEL_ID.to_owned()]
    );
    for model_id in [MODEL_ID, PAGINATED_MODEL_ID] {
        let model = gateway
            .admin()
            .get_provider_model(&provider.id, model_id)
            .await?;
        assert!(model.available);
        assert_eq!(model.metadata.extensions["fixture_version"], "2.0.0");
    }
    Ok(())
}

#[tokio::test]
async fn uninstall_cancels_auth_exchange_without_deleting_saved_connection_credentials()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "management-v2.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let provider = create_oauth_provider(
        &gateway,
        &mut upstream,
        MANAGEMENT_VENDOR,
        "Retained uninstall provider",
        "retained-access-token",
    )
    .await?;
    let credential = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .expect("saved OAuth credential");

    let unsaved = begin_oauth(&gateway, MANAGEMENT_VENDOR, None, &upstream.base_url).await?;
    let gw = gateway.clone();
    let session_id = unsaved.session_id.clone();
    let input = completion(&unsaved);
    let exchange =
        tokio::spawn(async move { gw.admin().complete_oauth_session(&session_id, input).await });
    let late_request = upstream.next().await;
    assert_eq!(late_request.operation(), "oauth_exchange");

    gateway
        .admin()
        .uninstall_vendor_plugin(MANAGEMENT_VENDOR)
        .await?;
    assert!(exchange.await?.is_err());
    late_request.reply(UpstreamReply::token("late-token", "late-refresh"));
    assert!(
        gateway
            .admin()
            .get_oauth_session_status(&unsaved.session_id)
            .await
            .is_err(),
        "uninstall must remove the cancelled authentication session"
    );
    assert_eq!(
        gateway
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?,
        Some(credential),
        "uninstall must retain the saved connection credential"
    );
    let retained = gateway.admin().get_provider(&provider.id).await?;
    assert_eq!(retained.id, provider.id);
    assert_eq!(retained.vendor_options, provider.vendor_options);
    Ok(())
}

#[tokio::test]
async fn incompatible_update_cancels_management_work_and_requires_selective_recovery()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "management-v2.wasm", false).await?;
    install(&gateway, "management-other.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let mut other_upstream = TestUpstream::start().await;

    let provider = create_oauth_provider(
        &gateway,
        &mut upstream,
        MANAGEMENT_VENDOR,
        "Preserved management provider",
        "management-access-token",
    )
    .await?;
    sync_models(&gateway, &mut upstream, &provider.id).await?;
    let route = create_route(&gateway, &provider.id, "management-lifecycle-route").await?;
    let original_target = route.targets[0].clone();
    let token = api_key(&gateway, &route.id).await?;
    let router = create_router(gateway.clone());

    let other = create_oauth_provider(
        &gateway,
        &mut other_upstream,
        OTHER_VENDOR,
        "Unaffected management provider",
        "other-access-token",
    )
    .await?;

    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let allowance =
        tokio::spawn(async move { gw.admin().refresh_provider_allowance(&provider_id).await });
    let request = upstream.next().await;
    assert_eq!(request.operation(), "allowance");
    request.reply(UpstreamReply::allowance("75"));
    let seeded_allowance = allowance.await??.expect("seeded allowance");
    assert_eq!(
        seeded_allowance.allowances[0]
            .remaining
            .as_ref()
            .map(|v| v.value),
        Some(75.0)
    );
    let samples_before = allowance_sample_count(directory.path(), &provider.id).await?;
    assert!(
        samples_before >= 1,
        "fresh allowance must create a durable sample"
    );

    let mut seed = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        route.model_id.clone().into(),
    ));
    let request = tokio::select! {
        request = upstream.next() => request,
        result = &mut seed => panic!("seed inference ended before reaching the upstream: {result:?}"),
    };
    assert_eq!(request.operation(), "infer");
    assert_eq!(request.header("x-management-version"), Some("2.0.0"));
    request.reply(UpstreamReply::inference("history-before-update"));
    let seeded = seed.await?;
    assert_eq!(seeded.0, StatusCode::OK, "seed response: {}", seeded.1);
    gateway.admin().observation_flush().await?;
    let forest = gateway
        .admin()
        .observation_forest(ForestQuery::default())
        .await?;
    let history = forest
        .roots
        .iter()
        .flat_map(|root| &root.interactions)
        .find(|interaction| interaction.first_route_id == route.id.as_str())
        .expect("seeded interaction history");
    assert_eq!(history.usage.output_tokens, Some(7));
    let history_id = history.id.clone();
    let history_usage = history.usage.clone();

    let unsaved = begin_oauth(&gateway, MANAGEMENT_VENDOR, None, &upstream.base_url).await?;
    let gw = gateway.clone();
    let session_id = unsaved.session_id.clone();
    let input = completion(&unsaved);
    let mut exchange_task =
        tokio::spawn(async move { gw.admin().complete_oauth_session(&session_id, input).await });

    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let mut discovery_task =
        tokio::spawn(async move { gw.admin().sync_provider_models(&provider_id).await });
    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let mut allowance_task =
        tokio::spawn(async move { gw.admin().refresh_provider_allowance(&provider_id).await });
    let gw = gateway.clone();
    let other_id = other.id.clone();
    let other_allowance_task =
        tokio::spawn(async move { gw.admin().refresh_provider_allowance(&other_id).await });

    let mut late_requests = tokio::select! {
        requests = upstream.take_operations(3) => requests,
        result = &mut exchange_task => panic!("OAuth exchange ended before the update boundary: {result:?}"),
        result = &mut discovery_task => panic!("model discovery ended before the update boundary: {result:?}"),
        result = &mut allowance_task => panic!("allowance refresh ended before the update boundary: {result:?}"),
    };
    // 刷新租约会改变凭据版本；先让其他操作固定快照，再验证四类活跃操作一起取消。
    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let mut refresh_task =
        tokio::spawn(async move { gw.admin().reconnect_provider_oauth(&provider_id).await });
    let refresh_request = tokio::select! {
        request = upstream.next() => request,
        result = &mut refresh_task => panic!("OAuth refresh ended before the update boundary: {result:?}"),
    };
    assert_eq!(refresh_request.operation(), "oauth_refresh");
    assert!(
        late_requests
            .insert("oauth_refresh".into(), refresh_request)
            .is_none()
    );
    for (operation, path) in [
        ("oauth_exchange", "/oauth/exchange"),
        ("oauth_refresh", "/oauth/refresh"),
        ("model_discovery", "/models"),
        ("allowance", "/allowance"),
    ] {
        let request = late_requests
            .get(operation)
            .unwrap_or_else(|| panic!("missing active {operation}"));
        assert_eq!(request.uri.path(), path);
        assert_eq!(request.header("x-management-version"), Some("2.0.0"));
    }
    let unrelated_request = other_upstream.next().await;
    assert_eq!(unrelated_request.operation(), "allowance");
    assert_eq!(
        unrelated_request.header("x-management-vendor"),
        Some(OTHER_VENDOR)
    );

    let credential_before_confirmation = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .expect("active OAuth credential");
    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("management-v3.wasm"))
        .await?;
    assert!(preview.cancels_active_operations);
    assert!(preview.active_operations >= 4);
    assert_eq!(preview.affected_auth_sessions, 1);
    let discarded = preview
        .discarded_data
        .iter()
        .find(|discard| discard.provider.id == provider.id)
        .expect("affected management provider");
    assert_eq!(
        discarded.kinds,
        ["options", "credentials", "models", "private_state"]
    );
    assert_eq!(
        discarded.recovery_actions,
        ["options", "credentials", "models"]
    );

    let before_confirmation = gateway.admin().get_provider(&provider.id).await?;
    assert_eq!(before_confirmation.name, provider.name);
    assert_eq!(before_confirmation.vendor_options, provider.vendor_options);
    assert!(
        gateway
            .admin()
            .get_provider_model(&provider.id, MODEL_ID)
            .await?
            .available
    );
    let rejected = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id.clone(),
            allow_data_discard: false,
        })
        .await;
    assert!(rejected.is_err());
    assert_eq!(
        gateway
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?,
        Some(credential_before_confirmation),
        "refusing data discard must preserve the active OAuth credential"
    );
    assert!(!exchange_task.is_finished());
    assert!(!refresh_task.is_finished());
    assert!(!discovery_task.is_finished());
    assert!(!allowance_task.is_finished());
    assert!(!other_allowance_task.is_finished());

    let updated = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: true,
        })
        .await?;
    assert_eq!(updated.version, "3.0.0");
    assert!(exchange_task.await?.is_err());
    assert!(refresh_task.await?.is_err());
    assert!(discovery_task.await?.is_err());
    assert!(allowance_task.await?.is_err());

    unrelated_request.reply(UpstreamReply::allowance("64"));
    let other_snapshot = other_allowance_task
        .await??
        .expect("other allowance snapshot");
    assert_eq!(other_snapshot.provider_id, other.id);
    assert_eq!(
        other_snapshot.status,
        stravia_core::admin::provider_allowance::ProviderAllowanceStatus::Fresh
    );

    late_requests
        .remove("oauth_exchange")
        .expect("late exchange")
        .reply(UpstreamReply::token("late-exchange", "late-refresh"));
    late_requests
        .remove("oauth_refresh")
        .expect("late refresh")
        .reply(UpstreamReply::token("late-refreshed", "late-refresh"));
    late_requests
        .remove("model_discovery")
        .expect("late discovery")
        .reply(UpstreamReply::models());
    late_requests
        .remove("allowance")
        .expect("late allowance")
        .reply(UpstreamReply::allowance("99"));

    assert!(
        gateway
            .admin()
            .get_oauth_session_status(&unsaved.session_id)
            .await
            .is_err(),
        "cancelled unsaved auth session must not be revived"
    );
    let reset_provider = gateway.admin().get_provider(&provider.id).await?;
    assert_eq!(reset_provider.id, provider.id);
    assert_eq!(reset_provider.name, provider.name);
    assert_eq!(reset_provider.vendor_options, "{}");
    assert_eq!(
        gateway
            .admin()
            .get_provider_oauth_status(&provider.id)
            .await?
            .status,
        "disconnected"
    );
    let reset_model = gateway
        .admin()
        .get_provider_model(&provider.id, MODEL_ID)
        .await?;
    assert!(!reset_model.available);
    assert_eq!(
        reset_model.metadata.name.as_deref(),
        Some("Management Model")
    );
    assert!(reset_model.metadata.extensions.is_empty());
    assert_eq!(reset_model.metadata.provider, None);
    assert_eq!(reset_model.metadata.status, None);
    let retained_route = gateway.admin().get_model(&route.model_id).await?;
    assert_eq!(retained_route.id, route.id);
    assert_eq!(retained_route.display_name, route.display_name);
    assert_eq!(retained_route.targets.len(), 1);
    assert_eq!(retained_route.targets[0].id, original_target.id);
    assert_eq!(
        retained_route.targets[0].provider_id().as_str(),
        provider.id.as_str()
    );
    assert_eq!(
        retained_route.targets[0]
            .model()
            .map(|model| model.as_str()),
        Some(MODEL_ID)
    );
    assert_eq!(
        allowance_sample_count(directory.path(), &provider.id).await?,
        samples_before,
        "allowance history samples are platform facts"
    );
    gateway.admin().observation_flush().await?;
    let retained_history = gateway
        .admin()
        .observation_interaction_summary(&history_id, ForestQuery::default())
        .await?
        .expect("interaction history survives plugin reset");
    assert_eq!(retained_history.interaction.usage, history_usage);

    // Keep HTTP dispatch on its own task, as in the server and the other requests here.
    // Nesting it inside this long-lived scenario exhausts the Windows test thread stack.
    let unavailable = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        route.model_id.clone().into(),
    ))
    .await?;
    assert_ne!(unavailable.0, StatusCode::OK);
    upstream.assert_no_request();

    gateway
        .admin()
        .update_provider(
            &provider.id,
            UpdateProvider {
                vendor_options: Some(options().into_iter().collect()),
                ..Default::default()
            },
        )
        .await?;
    let reauthorization = begin_oauth(
        &gateway,
        MANAGEMENT_VENDOR,
        Some(provider.id.clone()),
        &upstream.base_url,
    )
    .await?;
    let gw = gateway.clone();
    let session_id = reauthorization.session_id.clone();
    let input = completion(&reauthorization);
    let mut exchange =
        tokio::spawn(async move { gw.admin().complete_oauth_session(&session_id, input).await });
    let request = tokio::select! {
        request = upstream.next() => request,
        result = &mut exchange => panic!("recovery exchange ended before reaching the upstream: {result:?}"),
    };
    assert_eq!(request.operation(), "oauth_exchange");
    assert_eq!(request.header("x-management-version"), Some("3.0.0"));
    request.reply(UpstreamReply::token(
        "recovered-access-token",
        "recovered-refresh-token",
    ));
    assert!(matches!(
        exchange.await??,
        AuthSessionStatusData::Ready { .. }
    ));
    gateway
        .admin()
        .bind_provider_with_oauth_session(&provider.id, &reauthorization.session_id)
        .await?;
    let gw = gateway.clone();
    let provider_id = provider.id.clone();
    let mut discovery =
        tokio::spawn(async move { gw.admin().sync_provider_models(&provider_id).await });
    let request = tokio::select! {
        request = upstream.next() => request,
        result = &mut discovery => panic!("recovery discovery ended before reaching the upstream: {result:?}"),
    };
    assert_eq!(request.operation(), "model_discovery");
    assert_eq!(request.header("x-management-version"), Some("3.0.0"));
    assert_eq!(request.header("x-state-before"), Some("empty"));
    request.reply(UpstreamReply::models());
    assert_eq!(discovery.await??.restored, 1);
    let recovered_model = gateway
        .admin()
        .get_provider_model(&provider.id, MODEL_ID)
        .await?;
    assert!(recovered_model.available);
    assert_eq!(
        recovered_model.metadata.extensions["fixture_version"],
        "3.0.0"
    );

    let mut recovered = tokio::spawn(invoke(router, token, route.model_id.into()));
    let request = tokio::select! {
        request = upstream.next() => request,
        result = &mut recovered => panic!("recovered inference ended before reaching the upstream: {result:?}"),
    };
    assert_eq!(request.operation(), "infer");
    assert_eq!(request.header("x-management-version"), Some("3.0.0"));
    assert_eq!(request.header("x-state-before"), Some("1"));
    assert_eq!(request.header("x-workspace"), Some("contract-workspace"));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer recovered-access-token")
    );
    request.reply(UpstreamReply::inference("recovered-after-reconfiguration"));
    let recovered = recovered.await?;
    assert_eq!(
        recovered.0,
        StatusCode::OK,
        "recovered response: {}",
        recovered.1
    );
    assert!(
        recovered
            .1
            .to_string()
            .contains("recovered-after-reconfiguration")
    );
    Ok(())
}
