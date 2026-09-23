use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::AdminMode;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use stravia_core::admin::identity::AdminAuth;
use stravia_core::config::GatewayConfig;
use stravia_core::storage::MemoryStorage;
use tower::ServiceExt;

async fn memory_gateway(data_dir: &Path) -> anyhow::Result<Gateway> {
    Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.to_path_buf(),
            ..Default::default()
        },
        Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new())),
    )
    .await
}

#[tokio::test]
async fn status_reports_the_running_server_version() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let response = create_unprotected_router(gateway)
        .oneshot(Request::get("/api/v1/status").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["status"], "running");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    Ok(())
}

#[tokio::test]
async fn update_routes_expose_instance_state_and_exact_skip_version() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let storage = gateway.storage.clone();
    let app = create_unprotected_router(gateway);

    let initial = app
        .clone()
        .oneshot(Request::get("/api/v1/updates").body(Body::empty())?)
        .await?;
    assert_eq!(initial.status(), StatusCode::OK);
    let body = to_bytes(initial.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["data"]["check_status"], "idle");
    assert_eq!(json["data"]["download_supported"], false);

    for mode in ["automatic", "manual"] {
        let checked = app
            .clone()
            .oneshot(
                Request::post("/api/v1/updates/check")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"mode":"{mode}"}}"#)))?,
            )
            .await?;
        assert_eq!(checked.status(), StatusCode::OK);
        let body = to_bytes(checked.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(json["data"]["check_status"], "error");
        assert_eq!(
            json["data"]["last_failure"]["code"],
            "UPDATE_CHECK_DISABLED"
        );
    }

    let skipped = app
        .clone()
        .oneshot(
            Request::put("/api/v1/updates/skipped-version")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"version":"1.2.3"}"#))?,
        )
        .await?;
    assert_eq!(skipped.status(), StatusCode::OK);
    assert_eq!(
        storage
            .settings()
            .get("product_update_skipped_version")
            .await?,
        Some("1.2.3".to_string())
    );

    let other_data_dir = tempfile::tempdir()?;
    let other_gateway = memory_gateway(other_data_dir.path()).await?;
    assert_eq!(
        other_gateway
            .storage
            .settings()
            .get("product_update_skipped_version")
            .await?,
        None
    );

    let cleared = app
        .oneshot(
            Request::put("/api/v1/updates/skipped-version")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"version":null}"#))?,
        )
        .await?;
    assert_eq!(cleared.status(), StatusCode::OK);
    assert_eq!(
        storage
            .settings()
            .get("product_update_skipped_version")
            .await?,
        Some(String::new())
    );
    Ok(())
}

#[tokio::test]
async fn provider_allowance_routes_share_the_core_contract() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);

    let response = app
        .clone()
        .oneshot(Request::get("/api/v1/provider-allowances").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json, serde_json::json!({ "data": [] }));

    for request in [
        Request::get("/api/v1/provider-allowances/missing").body(Body::empty())?,
        Request::post("/api/v1/provider-allowances/missing/refresh").body(Body::empty())?,
    ] {
        let missing = app.clone().oneshot(request).await?;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(missing.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(json["error"], "provider allowance is unavailable");
        assert_eq!(json["code"], "PROVIDER_ALLOWANCE_UNAVAILABLE");
    }
    Ok(())
}

async fn automatic_callback_failure_body(locale: &str) -> anyhow::Result<String> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);
    let init_response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "vendor_id": "anthropic",
                    "channel": "claude-code",
                    "base_url": "https://api.anthropic.com",
                    "use_proxy": false,
                    "callback_mode": "auto",
                    "locale": locale,
                }))?))?,
        )
        .await?;
    let init_body = to_bytes(init_response.into_body(), usize::MAX).await?;
    let init: serde_json::Value = serde_json::from_slice(&init_body)?;
    let port = init["data"]["listener_port"].as_u64().unwrap() as u16;
    let state = reqwest::Url::parse(init["data"]["auth_url"].as_str().unwrap())?
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("authorization URL should contain state");

    let callback = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .get(format!(
            "http://127.0.0.1:{port}/callback?error=access_denied&state={state}"
        ))
        .send()
        .await?;
    assert_eq!(callback.status(), StatusCode::BAD_REQUEST);
    Ok(callback.text().await?)
}

#[tokio::test]
async fn automatic_callback_accepts_english_and_falls_back_for_an_invalid_locale()
-> anyhow::Result<()> {
    for locale in ["en-US", "zh-TW"] {
        let body = automatic_callback_failure_body(locale).await?;
        assert!(body.contains("<html lang=\"en-US\">"));
        assert!(body.contains("OAuth could not be completed"));
        assert!(
            body.contains(
                "Authorization failed. Return to Stravia for details and retry guidance."
            )
        );
    }
    Ok(())
}

#[tokio::test]
async fn manual_oauth_init_exposes_the_effective_callback_contract() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);
    let response = app
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"vendor_id":"openai-codex","channel":"codex","base_url":"https://chatgpt.com/backend-api/codex","use_proxy":false,"callback_mode":"manual"}"#,
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["data"]["callback_mode"], "manual");
    assert_eq!(json["data"]["listener_state"], "not_started");
    assert_eq!(
        json["data"]["redirect_uri"],
        "http://localhost:1457/auth/callback"
    );
    assert_eq!(json["data"]["listener_port"], serde_json::Value::Null);

    Ok(())
}

#[tokio::test]
async fn general_provider_endpoint_rejects_oauth_channels_without_a_session() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let catalog = gateway.admin().catalog_choices().await;
    let fingerprint = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "openai-codex")
        .and_then(|provider| {
            provider
                .channels
                .iter()
                .find(|channel| channel.id == "codex")
        })
        .expect("OpenAI Codex Catalog channel")
        .fingerprint
        .clone();
    let request_body = serde_json::to_vec(&serde_json::json!({
        "name": "invalid",
        "source": {
            "type": "catalog",
            "provider_id": "openai-codex",
            "channel_id": "codex",
            "fingerprint": fingerprint
        },
        "credential": { "type": "none" },
        "use_proxy": false
    }))?;
    let response = create_unprotected_router(gateway)
        .oneshot(
            Request::post("/api/v1/providers")
                .header("content-type", "application/json")
                .body(Body::from(request_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    assert!(String::from_utf8_lossy(&body).contains("AUTH_SESSION_REQUIRED"));

    Ok(())
}

#[tokio::test]
async fn provider_endpoints_keep_unavailable_profiles_visible_without_echoing_secrets()
-> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let available = gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "available-provider".into(),
            vendor: Some("openai".into()),
            protocol: "openai-compatible".into(),
            base_url: "https://api.openai.com/v1".into(),
            preset_key: Some("openai".into()),
            channel: Some("default".into()),
            models_source: Some("catalog".into()),
            static_models: None,
            api_key: "available-secret".into(),
            adapter_credentials: r#"{"apiKey":"available-secret"}"#.into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await?;
    let unavailable = gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "unavailable-provider".into(),
            vendor: Some("removed-profile".into()),
            protocol: "removed-protocol".into(),
            base_url: "https://unavailable.example.test".into(),
            preset_key: Some("removed-profile".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: r#"{"privateToken":"unavailable-secret"}"#.into(),
            vendor_options: r#"{"workspace":"retained"}"#.into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await?;
    let storage = gateway.storage.clone();
    let app = create_unprotected_router(gateway);

    let response = app
        .clone()
        .oneshot(Request::get("/api/v1/providers").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let providers = json["data"].as_array().expect("provider list");
    let available_value = providers
        .iter()
        .find(|provider| provider["id"] == available.id)
        .expect("available provider remains visible");
    assert_eq!(
        available_value["configured_credential_fields"],
        serde_json::json!(["apiKey"])
    );
    let unavailable_value = providers
        .iter()
        .find(|provider| provider["id"] == unavailable.id)
        .expect("unavailable provider remains visible");
    assert_eq!(
        unavailable_value["configured_credential_fields"],
        serde_json::json!([])
    );
    assert_eq!(unavailable_value["vendor_options"]["workspace"], "retained");
    assert!(unavailable_value.get("adapter_credentials").is_none());
    assert!(unavailable_value.get("api_key").is_none());
    let body = String::from_utf8_lossy(&body);
    assert!(!body.contains("available-secret"));
    assert!(!body.contains("unavailable-secret"));

    let response = app
        .oneshot(Request::get(format!("/api/v1/providers/{}", unavailable.id)).body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let retained = storage
        .providers()
        .get(&unavailable.id)
        .await?
        .expect("unavailable provider remains stored");
    assert_eq!(
        retained.adapter_credentials,
        r#"{"privateToken":"unavailable-secret"}"#
    );
    assert_eq!(retained.vendor_options, r#"{"workspace":"retained"}"#);
    Ok(())
}

#[tokio::test]
async fn terminal_manual_completion_releases_the_auto_listener() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);
    let init_response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"vendor_id":"anthropic","channel":"claude-code","base_url":"https://api.anthropic.com","use_proxy":false,"callback_mode":"auto"}"#,
                ))?,
        )
        .await?;
    let init_body = to_bytes(init_response.into_body(), usize::MAX).await?;
    let init: serde_json::Value = serde_json::from_slice(&init_body)?;
    let session_id = init["data"]["session_id"].as_str().unwrap();
    let port = init["data"]["listener_port"].as_u64().unwrap() as u16;
    let state = reqwest::Url::parse(init["data"]["auth_url"].as_str().unwrap())?
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("authorization URL should contain state");
    let response = app
        .oneshot(
            Request::post(format!("/api/v1/oauth/sessions/{session_id}/complete"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "input": {
                        "type": "callback_url",
                        "value": format!(
                            "http://localhost:{port}/callback?error=access_denied&state={state}"
                        )
                    }
                }))?))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let listener = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let listener = loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => break listener,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        };
        Ok::<_, std::io::Error>(listener)
    })
    .await??;
    drop(listener);

    Ok(())
}

#[tokio::test]
async fn automatic_callback_listener_is_loopback_only_and_returns_safe_html() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);
    let init_response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"vendor_id":"anthropic","channel":"claude-code","base_url":"https://api.anthropic.com","use_proxy":false,"callback_mode":"auto"}"#,
                ))?,
        )
        .await?;
    assert_eq!(init_response.status(), StatusCode::OK);
    let init_body = to_bytes(init_response.into_body(), usize::MAX).await?;
    let init: serde_json::Value = serde_json::from_slice(&init_body)?;
    let session_id = init["data"]["session_id"].as_str().unwrap();
    let port = init["data"]["listener_port"].as_u64().unwrap() as u16;
    let state = reqwest::Url::parse(init["data"]["auth_url"].as_str().unwrap())?
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("authorization URL should contain state");
    assert_eq!(init["data"]["callback_mode"], "auto");
    assert_eq!(init["data"]["listener_state"], "listening");
    assert_eq!(
        init["data"]["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );

    let callback = reqwest::Client::builder()
            .no_proxy()
            .build()?
            .get(format!(
                "http://127.0.0.1:{port}/callback?error=access_denied&error_description=secret-value&state={state}"
            ))
            .send()
            .await?;
    assert_eq!(callback.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        callback
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store, max-age=0")
    );
    assert!(callback.headers().contains_key("content-security-policy"));
    let callback_body = callback.text().await?;
    assert!(!callback_body.contains("secret-value"));
    assert!(!callback_body.contains(session_id));
    assert!(callback_body.contains("<html lang=\"en-US\">"));
    assert!(callback_body.contains("OAuth could not be completed"));
    assert!(
        callback_body
            .contains("Authorization failed. Return to Stravia for details and retry guidance.")
    );

    let status_response = app
        .oneshot(
            Request::get(format!("/api/v1/oauth/sessions/{session_id}/status"))
                .body(Body::empty())?,
        )
        .await?;
    let status_body = to_bytes(status_response.into_body(), usize::MAX).await?;
    let status: serde_json::Value = serde_json::from_slice(&status_body)?;
    assert_eq!(status["data"]["status"], "error");
    assert_eq!(status["data"]["code"], "AUTH_ACCESS_DENIED");

    Ok(())
}

#[tokio::test]
async fn automatic_callback_uses_the_requested_simplified_chinese_locale() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);
    let init_response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/oauth/sessions/init")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"vendor_id":"anthropic","channel":"claude-code","base_url":"https://api.anthropic.com","use_proxy":false,"callback_mode":"auto","locale":"zh-CN"}"#,
                    ))?,
            )
            .await?;
    assert_eq!(init_response.status(), StatusCode::OK);
    let init_body = to_bytes(init_response.into_body(), usize::MAX).await?;
    let init: serde_json::Value = serde_json::from_slice(&init_body)?;
    let port = init["data"]["listener_port"].as_u64().unwrap() as u16;
    let state = reqwest::Url::parse(init["data"]["auth_url"].as_str().unwrap())?
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("authorization URL should contain state");

    let callback = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .get(format!(
            "http://127.0.0.1:{port}/callback?error=access_denied&state={state}"
        ))
        .send()
        .await?;
    assert_eq!(callback.status(), StatusCode::BAD_REQUEST);
    let callback_body = callback.text().await?;
    assert!(callback_body.contains("<html lang=\"zh-CN\">"));
    assert!(callback_body.contains("OAuth 无法完成"));
    assert!(callback_body.contains("授权失败。请返回 Stravia 查看详情和重试指引。"));

    Ok(())
}
#[tokio::test]
async fn catalog_routes_replace_the_legacy_provider_presets_route() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);

    let response = app
        .clone()
        .oneshot(Request::get("/api/v1/catalog/providers").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("catalog response must include an ETag")
        .to_string();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(
        json["providers"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert_eq!(json["revision"].as_str(), Some(etag.trim_matches('"')));

    let not_modified = app
        .clone()
        .oneshot(
            Request::get("/api/v1/catalog/providers")
                .header("if-none-match", etag)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);

    let canonical = app
        .clone()
        .oneshot(Request::get("/api/v1/catalog/models").body(Body::empty())?)
        .await?;
    assert_eq!(canonical.status(), StatusCode::OK);
    let canonical_etag = canonical
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("canonical catalog response must include an ETag")
        .to_string();
    let canonical_body = to_bytes(canonical.into_body(), usize::MAX).await?;
    let canonical_json: serde_json::Value = serde_json::from_slice(&canonical_body)?;
    assert!(
        canonical_json["models"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert_eq!(
        canonical_json["revision"].as_str(),
        Some(canonical_etag.trim_matches('"'))
    );

    let canonical_not_modified = app
        .clone()
        .oneshot(
            Request::get("/api/v1/catalog/models")
                .header("if-none-match", canonical_etag)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(canonical_not_modified.status(), StatusCode::NOT_MODIFIED);

    let capability_drifts = app
        .clone()
        .oneshot(Request::get("/api/v1/providers/image-capability-drifts").body(Body::empty())?)
        .await?;
    assert_eq!(capability_drifts.status(), StatusCode::OK);
    let capability_drifts: serde_json::Value =
        serde_json::from_slice(&to_bytes(capability_drifts.into_body(), usize::MAX).await?)?;
    assert_eq!(capability_drifts, serde_json::json!({ "data": [] }));

    let legacy = app
        .oneshot(Request::get("/api/v1/providers/presets").body(Body::empty())?)
        .await?;
    assert_eq!(legacy.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn prepare_provider_model_uses_the_post_template_contract() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Template Provider".to_string()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".to_string(),
                channel: "default".to_string(),
                protocol: Some("openai-compatible".into()),
                base_url: "https://example.test/v1".to_string(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    let app = create_unprotected_router(gateway);

    let prepared = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/providers/{}/model/prepare", provider.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model_id":"provider-gpt-3.5","template_id":"openai/gpt-3.5-turbo"}"#,
                ))?,
        )
        .await?;
    assert_eq!(prepared.status(), StatusCode::OK);
    let prepared_body = to_bytes(prepared.into_body(), usize::MAX).await?;
    let prepared: serde_json::Value = serde_json::from_slice(&prepared_body)?;
    assert_eq!(prepared["data"]["id"], "provider-gpt-3.5");
    assert_eq!(prepared["data"]["metadata"]["id"], "provider-gpt-3.5");
    assert_eq!(prepared["data"]["metadata"]["family"], "gpt");
    assert!(
        prepared["data"]["extensions"]["benchmarks"]
            .as_array()
            .is_some_and(|benchmarks| !benchmarks.is_empty())
    );

    let bare = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/providers/{}/model/prepare", provider.id))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model_id":"private/model"}"#))?,
        )
        .await?;
    assert_eq!(bare.status(), StatusCode::OK);
    let bare_body = to_bytes(bare.into_body(), usize::MAX).await?;
    let bare: serde_json::Value = serde_json::from_slice(&bare_body)?;
    assert_eq!(bare["data"]["metadata"]["id"], "private/model");
    assert!(bare["data"]["metadata"]["description"].is_null());

    let missing = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/providers/{}/model/prepare", provider.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model_id":"private/missing","template_id":"openai/not-in-catalog"}"#,
                ))?,
        )
        .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let missing_body = to_bytes(missing.into_body(), usize::MAX).await?;
    let missing: serde_json::Value = serde_json::from_slice(&missing_body)?;
    assert_eq!(missing["code"], "CATALOG_MODEL_NOT_FOUND");

    let legacy = app
        .oneshot(
            Request::get(format!("/api/v1/providers/{}/model/prepare", provider.id))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(legacy.status(), StatusCode::METHOD_NOT_ALLOWED);
    Ok(())
}

#[tokio::test]
async fn web_search_routes_replace_the_legacy_web_research_routes() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);

    let current = app
        .clone()
        .oneshot(Request::get("/api/v1/web-search/config").body(Body::empty())?)
        .await?;
    assert_eq!(current.status(), StatusCode::OK);

    let legacy = app
        .oneshot(Request::get("/api/v1/web-research/config").body(Body::empty())?)
        .await?;
    assert_eq!(legacy.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn provider_model_routes_support_slash_ids_and_exact_decimal_costs() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("HTTP Provider Model".to_string()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".to_string(),
                channel: "default".to_string(),
                protocol: Some("openai-compatible".into()),
                base_url: "https://example.test/v1".to_string(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    let app = create_unprotected_router(gateway);

    let created = app
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/providers/{}/models", provider.id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model_id":"private/model","metadata":{"id":"private/model","name":"Private Model","cost":{"input":0.123456789012345678}}}"#,
                    ))?,
            )
            .await?;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = to_bytes(created.into_body(), usize::MAX).await?;
    assert!(
        String::from_utf8_lossy(&created_body).contains("0.123456789012345678"),
        "Admin JSON must preserve arbitrary-precision decimal text"
    );
    let created_json: serde_json::Value = serde_json::from_slice(&created_body)?;
    let revision = created_json["data"]["revision"]
        .as_i64()
        .expect("Provider Model revision");

    let loaded = app
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/v1/providers/{}/model?model=private%2Fmodel",
                provider.id
            ))
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(loaded.status(), StatusCode::OK);

    let updated = app
            .oneshot(
                Request::put(format!("/api/v1/providers/{}/model", provider.id))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model_id":"private/model","revision":{revision},"metadata":{{"id":"private/model","name":"Private Model","cost":{{"input":0.987654321098765432}}}}}}"#
                    )))?,
            )
            .await?;
    assert_eq!(updated.status(), StatusCode::OK);
    let updated_body = to_bytes(updated.into_body(), usize::MAX).await?;
    assert!(String::from_utf8_lossy(&updated_body).contains("0.987654321098765432"));
    Ok(())
}

#[tokio::test]
async fn model_target_statuses_stay_behind_admin_auth() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Target Status Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: "https://example.test/v1".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "upstream-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({"id": "upstream-model", "name": "Upstream Model"}),
            },
        )
        .await?;
    gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "statused-model".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: Some("upstream-model".into()),
            targets: Vec::new(),
            default_thinking_level: None,
        })
        .await?;
    let auth = AdminAuth::new(gateway.storage.clone());
    auth.ensure_native_admin().await?;
    let app = create_router(
        gateway,
        AdminHttpState {
            auth: auth.clone(),
            mode: AdminMode::Desktop,
        },
    );

    let denied = app
        .clone()
        .oneshot(Request::get("/api/v1/models/statused-model/target-statuses").body(Body::empty())?)
        .await?;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let tokens = auth.login_native().await?;
    let allowed = app
        .clone()
        .oneshot(
            Request::get("/api/v1/models/statused-model/target-statuses")
                .header("authorization", format!("Bearer {}", tokens.access_token))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(allowed.status(), StatusCode::OK);

    let missing = app
        .oneshot(
            Request::get("/api/v1/models/no-such-model/target-statuses")
                .header("authorization", format!("Bearer {}", tokens.access_token))
                .body(Body::empty())?,
        )
        .await?;
    let body = to_bytes(missing.into_body(), usize::MAX).await?;
    let payload: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(payload["error"].is_string());
    assert!(payload.get("data").is_none());
    Ok(())
}

#[tokio::test]
async fn route_bind_endpoint_owns_one_click_target_creation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Route Bind Provider".to_string()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".to_string(),
                channel: "default".to_string(),
                protocol: Some("openai-compatible".into()),
                base_url: "https://example.test/v1".to_string(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "route-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "route-model",
                    "name": "Route Model"
                }),
            },
        )
        .await?;
    let app = create_unprotected_router(gateway);
    let body = serde_json::to_vec(&serde_json::json!({
        "provider_id": provider.id,
        "provider_model_id": "route-model"
    }))?;

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/models/bind")
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = app
        .oneshot(Request::get("/api/v1/models").body(Body::empty())?)
        .await?;
    let response_body = to_bytes(response.into_body(), usize::MAX).await?;
    let routes: serde_json::Value = serde_json::from_slice(&response_body)?;
    assert_eq!(routes["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(routes["data"][0]["model_id"], "route-model");
    assert_eq!(
        routes["data"][0]["targets"].as_array().map(Vec::len),
        Some(1)
    );
    Ok(())
}

#[tokio::test]
async fn web_access_browser_http_controls_local_selection() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await?;
    let local = gateway
        .admin()
        .list_web_providers()
        .await?
        .into_iter()
        .find(|provider| provider.kind == "local")
        .unwrap();
    let app = create_unprotected_router(gateway.clone());
    let settings = stravia_core::db::models::WebAccessSettings {
        search_provider_ids: vec![local.id.clone()],
        fetch_provider_ids: vec![local.id],
    };
    gateway
        .admin()
        .update_web_access_settings(stravia_core::db::models::WebAccessSettings::default())
        .await?;
    let missing = directory.path().join("missing-chrome.exe");
    gateway.set_browser_path(Some(missing));
    let rejected = app
        .clone()
        .oneshot(
            Request::put("/api/v1/web-access/settings")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&settings)?))?,
        )
        .await?;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let rejected_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(rejected.into_body(), usize::MAX).await?)?;
    assert_eq!(rejected_body["code"], "WEB_ACCESS_BROWSER_REQUIRED");
    let executable = std::env::current_exe()?.to_string_lossy().into_owned();
    let configured = app
        .clone()
        .oneshot(
            Request::put("/api/v1/web-access/browser")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(
                    &serde_json::json!({ "path": executable }),
                )?))?,
        )
        .await?;
    assert_eq!(configured.status(), StatusCode::OK);
    let payload: serde_json::Value =
        serde_json::from_slice(&to_bytes(configured.into_body(), usize::MAX).await?)?;
    assert_eq!(payload["data"]["configuredPath"], executable);
    assert_eq!(payload["data"]["source"], "manual");
    assert_eq!(payload["data"]["available"], true);
    let response = app
        .clone()
        .oneshot(
            Request::put("/api/v1/web-access/settings")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&settings)?))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(Request::get("/api/v1/web-access/settings").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
    assert_eq!(body["data"], serde_json::to_value(&settings)?);
    assert_eq!(gateway.admin().get_web_access_settings().await?, settings);
    let state = app
        .clone()
        .oneshot(Request::get("/api/v1/web-access/browser").body(Body::empty())?)
        .await?;
    assert_eq!(state.status(), StatusCode::OK);
    let state: serde_json::Value =
        serde_json::from_slice(&to_bytes(state.into_body(), usize::MAX).await?)?;
    assert_eq!(state["data"]["resolvedPath"], executable);
    let restarted = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await?;
    assert_eq!(
        restarted
            .admin()
            .get_web_access_browser()
            .await
            .configured_path
            .as_deref(),
        Some(executable.as_str())
    );
    let invalid = app
        .clone()
        .oneshot(
            Request::put("/api/v1/web-access/browser")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"missing-chrome.exe"}"#))?,
        )
        .await?;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        gateway
            .admin()
            .get_web_access_browser()
            .await
            .configured_path
            .as_deref(),
        Some(executable.as_str())
    );
    let reset = app
        .oneshot(
            Request::put("/api/v1/web-access/browser")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":null}"#))?,
        )
        .await?;
    assert_eq!(reset.status(), StatusCode::OK);
    let reset: serde_json::Value =
        serde_json::from_slice(&to_bytes(reset.into_body(), usize::MAX).await?)?;
    assert!(reset["data"]["configuredPath"].is_null());
    Ok(())
}

#[tokio::test]
async fn browser_settings_routes_require_admin_auth() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await?;
    let auth = AdminAuth::new(gateway.storage.clone());
    auth.ensure_native_admin().await?;
    let app = create_router(
        gateway,
        AdminHttpState {
            auth: auth.clone(),
            mode: AdminMode::Desktop,
        },
    );
    for method in ["GET", "PUT"] {
        let request = Request::builder()
            .method(method)
            .uri("/api/v1/web-access/browser")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"path":null}"#))?;
        let denied = app.clone().oneshot(request).await?;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }
    let tokens = auth.login_native().await?;
    let allowed = app
        .oneshot(
            Request::get("/api/v1/web-access/browser")
                .header("authorization", format!("Bearer {}", tokens.access_token))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(allowed.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn web_access_admin_routes_persist_masked_providers_and_atomic_priority() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_unprotected_router(gateway);

    let created = app
            .clone()
            .oneshot(
                Request::post("/api/v1/web-providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"Exa primary","kind":"exa","api_key":"secret-exa","provider_id":null}"#,
                    ))?,
            )
            .await?;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = to_bytes(created.into_body(), usize::MAX).await?;
    let created_json: serde_json::Value = serde_json::from_slice(&created_body)?;
    assert!(created_json["data"].get("api_key").is_none());
    assert_eq!(
        created_json["data"]["capabilities"],
        serde_json::json!({ "search": true, "fetch": true })
    );
    let id = created_json["data"]["id"]
        .as_str()
        .expect("Web Provider ID");

    let updated = app
        .clone()
        .oneshot(
            Request::put("/api/v1/web-access/settings")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "search_provider_ids": [id],
                    "fetch_provider_ids": [id]
                }))?))?,
        )
        .await?;
    assert_eq!(updated.status(), StatusCode::OK);

    let deleted = app
        .clone()
        .oneshot(Request::delete(format!("/api/v1/web-providers/{id}")).body(Body::empty())?)
        .await?;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let settings = app
        .oneshot(Request::get("/api/v1/web-access/settings").body(Body::empty())?)
        .await?;
    assert_eq!(settings.status(), StatusCode::OK);
    let settings_body = to_bytes(settings.into_body(), usize::MAX).await?;
    let settings_json: serde_json::Value = serde_json::from_slice(&settings_body)?;
    assert_eq!(
        settings_json["data"]["search_provider_ids"],
        serde_json::json!([])
    );
    assert_eq!(
        settings_json["data"]["fetch_provider_ids"],
        serde_json::json!([])
    );
    Ok(())
}

// 内置-only vendor(command-code)必须能通过标准 catalog 流程创建;
// 目录解析失败要返回结构化错误码,而不是 200 + 裸错误串。
#[tokio::test]
async fn provider_create_accepts_builtin_vendors_and_codes_catalog_mismatches() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let gateway = memory_gateway(data_dir.path()).await?;
    let app = create_unprotected_router(gateway);

    let catalog_response = app
        .clone()
        .oneshot(Request::get("/api/v1/catalog/providers").body(Body::empty())?)
        .await?;
    assert_eq!(catalog_response.status(), StatusCode::OK);
    let catalog_body = to_bytes(catalog_response.into_body(), usize::MAX).await?;
    let catalog: serde_json::Value = serde_json::from_slice(&catalog_body)?;
    let channel = catalog["providers"]
        .as_array()
        .expect("provider list")
        .iter()
        .find(|provider| provider["id"] == "command-code")
        .and_then(|provider| provider["channels"].as_array())
        .and_then(|channels| {
            channels
                .iter()
                .find(|channel| channel["id"] == "default")
                .cloned()
        })
        .expect("command-code must be exposed as a catalog provider");
    assert_eq!(channel["protocol"], "command-code");
    assert_eq!(channel["base_url"], "https://api.commandcode.ai");
    let fingerprint = channel["fingerprint"].as_str().expect("fingerprint");

    // 模型清单来自 `/provider/v1/models`(HTTP 发现),用本地 mock 提供
    // 真实 API 的 OpenAI 兼容格式,避免 E2E 触达生产服务。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mock_models = axum::routing::get(|| async {
        axum::Json(serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": "deepseek/deepseek-v4-pro",
                    "object": "model",
                    "owned_by": "command-code",
                    "name": "DeepSeek V4 Pro (latest)",
                    "context_length": 1000000
                },
                {
                    "id": "claude-fable-5",
                    "object": "model",
                    "owned_by": "command-code",
                    "name": "Claude Fable 5",
                    "context_length": 1000000
                },
                {
                    "id": "commandcode/ccc-private-1",
                    "object": "model",
                    "owned_by": "command-code",
                    "name": "CCC Private 1",
                    "context_length": 64000
                }
            ]
        }))
    });
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/provider/v1/models", mock_models),
        )
        .await
    });

    let create = |source: serde_json::Value| {
        app.clone().oneshot(
            Request::post("/api/v1/providers")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "Command Code",
                        "source": source,
                        "credential": { "type": "api_key", "value": "sk-commandcode" }
                    }))
                    .expect("serialize create provider request"),
                ))
                .expect("build create provider request"),
        )
    };

    let created = create(serde_json::json!({
        "type": "catalog",
        "provider_id": "command-code",
        "channel_id": "default",
        "fingerprint": fingerprint,
        "base_url_override": format!("http://{address}"),
    }))
    .await?;
    assert_eq!(created.status(), StatusCode::OK);
    let created_body = to_bytes(created.into_body(), usize::MAX).await?;
    let created_json: serde_json::Value = serde_json::from_slice(&created_body)?;
    assert_eq!(created_json["data"]["vendor"], "command-code");
    assert_eq!(created_json["data"]["protocol"], "command-code");
    assert_eq!(created_json["data"]["preset_key"], "command-code");
    assert_eq!(
        created_json["data"]["base_url"],
        format!("http://{address}")
    );
    let provider_id = created_json["data"]["id"].as_str().expect("provider id");

    // 模型清单经 HTTP 发现自 mock 端点;同步后持久化,id 在 canonical 目录
    // 命中时补齐缺少的元数据,未命中时保留上游规格,不依赖远端目录 scope。
    let synced = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/providers/{provider_id}/models/sync"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(synced.status(), StatusCode::OK);
    let synced_body = to_bytes(synced.into_body(), usize::MAX).await?;
    let synced_json: serde_json::Value = serde_json::from_slice(&synced_body)?;
    assert_eq!(synced_json["data"]["added"], 3);

    let models = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/providers/{provider_id}/models")).body(Body::empty())?,
        )
        .await?;
    assert_eq!(models.status(), StatusCode::OK);
    let models_body = to_bytes(models.into_body(), usize::MAX).await?;
    let models_json: serde_json::Value = serde_json::from_slice(&models_body)?;
    let mut entries: Vec<serde_json::Value> = models_json["data"]["models"]
        .as_array()
        .expect("persisted models")
        .clone();
    // 发现结果没有顺序契约，按 ID 检查对应的元数据。
    entries.sort_unstable_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    let model_ids: Vec<&str> = entries
        .iter()
        .map(|model| model["id"].as_str().expect("model id"))
        .collect();
    assert_eq!(
        model_ids,
        vec![
            "claude-fable-5",
            "commandcode/ccc-private-1",
            "deepseek/deepseek-v4-pro",
        ]
    );

    // canonical 目录补齐上游未提供的通用规格与能力。
    let deepseek = &entries[2];
    assert_eq!(deepseek["specification"]["limit"]["context"], 1000000);
    assert_eq!(deepseek["specification"]["tool_call"], true);

    server.abort();

    let stale = create(serde_json::json!({
        "type": "catalog",
        "provider_id": "command-code",
        "channel_id": "default",
        "fingerprint": "stale-fingerprint",
    }))
    .await?;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let stale_body = to_bytes(stale.into_body(), usize::MAX).await?;
    let stale_json: serde_json::Value = serde_json::from_slice(&stale_body)?;
    assert_eq!(stale_json["code"], "CATALOG_FINGERPRINT_STALE");
    assert_eq!(stale_json["params"]["provider_id"], "command-code");

    let missing = create(serde_json::json!({
        "type": "catalog",
        "provider_id": "gone-vendor",
        "channel_id": "default",
        "fingerprint": "fingerprint",
    }))
    .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let missing_body = to_bytes(missing.into_body(), usize::MAX).await?;
    let missing_json: serde_json::Value = serde_json::from_slice(&missing_body)?;
    assert_eq!(missing_json["code"], "CATALOG_PROVIDER_NOT_FOUND");
    assert_eq!(missing_json["params"]["provider_id"], "gone-vendor");

    Ok(())
}

// ── Provider Catalog parity over management HTTP ─────────────────────────
//
// The real base Wasm component fetches the catalog from an in-process
// fixture through the production `sync-catalog` boundary; management HTTP
// drives refresh, dynamic registration, provider creation, scoped models,
// the logo endpoint, and catalog removal semantics. No request reaches the
// production network — the catalog origin is injected at construction.

const CATALOG_TEST_SVG: &[u8] =
    b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 4 4\"><rect width=\"4\" height=\"4\"/></svg>";

#[derive(Clone)]
struct FakeCatalog {
    state: Arc<tokio::sync::RwLock<FakeCatalogState>>,
    base_url: String,
}

struct FakeCatalogState {
    revision: String,
    index: serde_json::Value,
    scopes: std::collections::HashMap<String, serde_json::Value>,
}

impl FakeCatalog {
    async fn start() -> anyhow::Result<Self> {
        let state = Arc::new(tokio::sync::RwLock::new(FakeCatalogState {
            revision: "rev-1".to_owned(),
            index: serde_json::json!({
                "fake-zen": {
                    "id": "fake-zen",
                    "name": "Fake Zen",
                    "npm": "@ai-sdk/openai-compatible",
                    "api": "http://upstream.invalid/v1"
                },
                "impossible": {
                    "id": "impossible",
                    "name": "Impossible",
                    "npm": "@totally/unknown-sdk"
                }
            }),
            scopes: std::collections::HashMap::from([(
                "fake-zen".to_owned(),
                serde_json::json!({
                    "zen-1": {
                        "id": "zen-1",
                        "name": "Zen One",
                        "tool_call": true,
                        "modalities": { "input": ["text"], "output": ["text"] },
                        "limit": { "context": 200000, "output": 8192 },
                        "cost": { "input": 1.0, "output": 2.0 }
                    }
                }),
            )]),
        }));
        let app = axum::Router::new()
            .route("/version.json", axum::routing::get(catalog_version))
            .route("/providers.json", axum::routing::get(catalog_index))
            .route("/models.json", axum::routing::get(canonical_models))
            .route(
                "/providers/{id}/models.json",
                axum::routing::get(catalog_scope),
            )
            .route("/logos/{file}", axum::routing::get(catalog_logo))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { state, base_url })
    }

    async fn set_index(&self, revision: &str, index: serde_json::Value) {
        let mut state = self.state.write().await;
        state.revision = revision.to_owned();
        state.index = index;
    }
}

async fn catalog_version(
    axum::extract::State(state): axum::extract::State<Arc<tokio::sync::RwLock<FakeCatalogState>>>,
) -> impl axum::response::IntoResponse {
    let state = state.read().await;
    axum::Json(serde_json::json!({
        "revision": state.revision,
        "generated_at": "2025-01-01T00:00:00Z",
    }))
}

async fn catalog_index(
    axum::extract::State(state): axum::extract::State<Arc<tokio::sync::RwLock<FakeCatalogState>>>,
) -> impl axum::response::IntoResponse {
    let state = state.read().await;
    axum::Json(state.index.clone())
}

async fn canonical_models() -> impl axum::response::IntoResponse {
    axum::Json(serde_json::json!({
        "acme/m-1": { "id": "acme/m-1", "name": "Acme Model One" }
    }))
}

async fn catalog_scope(
    axum::extract::State(state): axum::extract::State<Arc<tokio::sync::RwLock<FakeCatalogState>>>,
    axum::extract::Path(provider_id): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    let state = state.read().await;
    match state.scopes.get(&provider_id) {
        Some(body) => axum::Json(body.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn catalog_logo() -> impl axum::response::IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], CATALOG_TEST_SVG)
}

#[tokio::test]
async fn catalog_parity_drives_dynamic_profiles_through_management_http() -> anyhow::Result<()> {
    let catalog = FakeCatalog::start().await?;
    let data_dir = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            catalog_base_url: Some(catalog.base_url.clone()),
            ..Default::default()
        },
        Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new())),
    )
    .await?;
    let app = create_unprotected_router(gateway);

    // Remote refresh over management HTTP installs the confirmed snapshot.
    let refreshed = app
        .clone()
        .oneshot(Request::post("/api/v1/catalog/refresh").body(Body::empty())?)
        .await?;
    assert_eq!(refreshed.status(), StatusCode::OK);
    let refreshed_body = to_bytes(refreshed.into_body(), usize::MAX).await?;
    let refreshed_json: serde_json::Value = serde_json::from_slice(&refreshed_body)?;
    assert_eq!(refreshed_json["revision"], "rev-1");

    // Dynamic registration is visible through the catalog list endpoint; an
    // entry with an unmapped npm never registers.
    let providers = app
        .clone()
        .oneshot(Request::get("/api/v1/catalog/providers").body(Body::empty())?)
        .await?;
    assert_eq!(providers.status(), StatusCode::OK);
    let providers_body = to_bytes(providers.into_body(), usize::MAX).await?;
    let providers_json: serde_json::Value = serde_json::from_slice(&providers_body)?;
    let zen = providers_json["providers"]
        .as_array()
        .expect("provider list")
        .iter()
        .find(|provider| provider["id"] == "fake-zen")
        .cloned()
        .expect("compatible catalog entry registers over HTTP");
    assert!(
        providers_json["providers"]
            .as_array()
            .expect("provider list")
            .iter()
            .all(|provider| provider["id"] != "impossible")
    );
    let fingerprint = zen["channels"]
        .as_array()
        .and_then(|channels| {
            channels
                .iter()
                .find(|channel| channel["id"] == "default")
                .and_then(|channel| channel["fingerprint"].as_str())
        })
        .expect("default channel fingerprint")
        .to_owned();

    // Provider creation goes through management HTTP, not an internal call.
    let created = app
        .clone()
        .oneshot(
            Request::post("/api/v1/providers")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "Zen",
                        "source": {
                            "type": "catalog",
                            "provider_id": "fake-zen",
                            "channel_id": "default",
                            "fingerprint": fingerprint,
                        },
                        "credential": { "type": "none" }
                    }))
                    .expect("serialize create provider request"),
                ))
                .expect("build create provider request"),
        )
        .await?;
    assert_eq!(created.status(), StatusCode::OK);
    let created_body = to_bytes(created.into_body(), usize::MAX).await?;
    let created_json: serde_json::Value = serde_json::from_slice(&created_body)?;
    assert_eq!(created_json["data"]["vendor"], "fake-zen");

    // Scoped models and the logo endpoint are management-HTTP visible.
    let models = app
        .clone()
        .oneshot(
            Request::get("/api/v1/catalog/providers/fake-zen/channels/default/models")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(models.status(), StatusCode::OK);
    let models_body = to_bytes(models.into_body(), usize::MAX).await?;
    let models_json: serde_json::Value = serde_json::from_slice(&models_body)?;
    assert!(
        models_json["models"]
            .as_array()
            .is_some_and(|items| items.iter().any(|model| model["id"] == "zen-1"))
    );

    let logo = app
        .clone()
        .oneshot(Request::get("/api/v1/catalog/providers/fake-zen/logo").body(Body::empty())?)
        .await?;
    assert_eq!(logo.status(), StatusCode::OK);
    assert_eq!(
        logo.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("image/svg+xml")
    );
    let logo_body = to_bytes(logo.into_body(), usize::MAX).await?;
    assert_eq!(logo_body, CATALOG_TEST_SVG);

    // A confirmed refresh removing the entry blocks new creations with the
    // typed catalog miss while the saved provider stays listed.
    catalog
        .set_index(
            "rev-2",
            serde_json::json!({
                "other-zen": {
                    "id": "other-zen",
                    "name": "Other Zen",
                    "npm": "@ai-sdk/openai-compatible"
                }
            }),
        )
        .await;
    let refreshed = app
        .clone()
        .oneshot(Request::post("/api/v1/catalog/refresh").body(Body::empty())?)
        .await?;
    assert_eq!(refreshed.status(), StatusCode::OK);

    let rejected = app
        .clone()
        .oneshot(
            Request::post("/api/v1/providers")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "Zen Two",
                        "source": {
                            "type": "catalog",
                            "provider_id": "fake-zen",
                            "channel_id": "default",
                            "fingerprint": fingerprint,
                        },
                        "credential": { "type": "none" }
                    }))
                    .expect("serialize create provider request"),
                ))
                .expect("build create provider request"),
        )
        .await?;
    assert_eq!(rejected.status(), StatusCode::NOT_FOUND);
    let rejected_body = to_bytes(rejected.into_body(), usize::MAX).await?;
    let rejected_json: serde_json::Value = serde_json::from_slice(&rejected_body)?;
    assert_eq!(rejected_json["code"], "CATALOG_PROVIDER_NOT_FOUND");

    let listed = app
        .oneshot(Request::get("/api/v1/providers").body(Body::empty())?)
        .await?;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = to_bytes(listed.into_body(), usize::MAX).await?;
    let listed_json: serde_json::Value = serde_json::from_slice(&listed_body)?;
    assert!(
        listed_json["data"].as_array().is_some_and(|items| items
            .iter()
            .any(|provider| provider["vendor"] == "fake-zen")),
        "the saved provider survives removal from the advertised catalog"
    );

    Ok(())
}
