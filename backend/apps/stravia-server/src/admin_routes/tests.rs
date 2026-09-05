use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use stravia_core::config::GatewayConfig;
use tower::ServiceExt;

#[tokio::test]
async fn status_reports_the_running_server_version() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let response = create_router(gateway, None)
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let storage = gateway.storage.clone();
    let app = create_router(gateway, None);

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
    let (other_gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: other_data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);

    for request in [
        Request::get("/api/v1/provider-allowances").body(Body::empty())?,
        Request::post("/api/v1/provider-allowances/refresh").body(Body::empty())?,
    ] {
        let response = app.clone().oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(json, serde_json::json!({ "data": [] }));
    }

    let missing = app
        .oneshot(Request::post("/api/v1/provider-allowances/missing/refresh").body(Body::empty())?)
        .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(missing.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["error"], "provider allowance is unavailable");
    assert_eq!(json["code"], "PROVIDER_ALLOWANCE_UNAVAILABLE");
    Ok(())
}

async fn automatic_callback_failure_body(locale: &str) -> anyhow::Result<String> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);
    let init_response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "vendor": "claude-code",
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);
    let response = app
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"vendor":"codex","use_proxy":false,"callback_mode":"manual"}"#,
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let catalog = gateway.provider_catalog.providers().await;
    let fingerprint = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "openai")
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
            "provider_id": "openai",
            "channel_id": "codex",
            "fingerprint": fingerprint
        },
        "credential": { "type": "none" },
        "use_proxy": false
    }))?;
    let response = create_router(gateway, None)
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
async fn terminal_manual_completion_releases_the_auto_listener() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);
    let init_response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"vendor":"claude-code","use_proxy":false,"callback_mode":"auto"}"#,
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
                    "callback_url": format!(
                        "http://localhost:{port}/callback?error=access_denied&state={state}"
                    )
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);
    let init_response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/oauth/sessions/init")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"vendor":"claude-code","use_proxy":false,"callback_mode":"auto"}"#,
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);
    let init_response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/oauth/sessions/init")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"vendor":"claude-code","use_proxy":false,"callback_mode":"auto","locale":"zh-CN"}"#,
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);

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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Template Provider".to_string()),
            source: ProviderSourceInput::Custom {
                vendor: Some("openai".to_string()),
                protocol: "openai-compatible".to_string(),
                base_url: "https://example.test/v1".to_string(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await?;
    let app = create_router(gateway, None);

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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);

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
async fn catalog_logo_proxy_does_not_require_the_admin_bearer_token() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, Some("admin-secret".to_string()));

    let catalog = app
        .clone()
        .oneshot(Request::get("/api/v1/catalog/providers").body(Body::empty())?)
        .await?;
    assert_eq!(catalog.status(), StatusCode::UNAUTHORIZED);

    let logo = app
        .oneshot(Request::get("/api/v1/catalog/providers/not-a-provider/logo").body(Body::empty())?)
        .await?;
    assert_ne!(logo.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}
#[tokio::test]
async fn provider_model_routes_support_slash_ids_and_exact_decimal_costs() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("HTTP Provider Model".to_string()),
            source: ProviderSourceInput::Custom {
                vendor: Some("openai".to_string()),
                protocol: "openai-compatible".to_string(),
                base_url: "https://example.test/v1".to_string(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await?;
    let app = create_router(gateway, None);

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
async fn route_bind_endpoint_owns_one_click_target_creation() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Route Bind Provider".to_string()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".to_string(),
                base_url: "https://example.test/v1".to_string(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
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
    let app = create_router(gateway, None);
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
async fn web_access_admin_routes_persist_masked_providers_and_atomic_priority() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let app = create_router(gateway, None);

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
                    "enabled": true,
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
    assert_eq!(settings_json["data"]["enabled"], true);
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
