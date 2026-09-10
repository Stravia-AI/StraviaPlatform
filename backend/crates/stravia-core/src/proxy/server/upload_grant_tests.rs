use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::create_router;
use crate::Gateway;
use crate::agent::upload_grant::UploadGrantIssuer;
use crate::config::GatewayConfig;

async fn create_key(gateway: &Gateway) -> crate::db::models::ApiKeyWithBindings {
    gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Upload grant owner".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_understanding: false,
            model_ids: vec![],
        })
        .await
        .expect("API key")
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn upload(router: &axum::Router, credential: &str, content: &'static str) -> Value {
    let auth = format!("Bearer {credential}");
    let created = router
        .clone()
        .oneshot(
            Request::post("/v1/artifacts/uploads")
                .header("authorization", &auth)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"mime_type":"text/plain","size":content.len()}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json_body(created).await;
    let id = created["upload_id"].as_str().unwrap();
    let token = created["upload_token"].as_str().unwrap();
    let part = router
        .clone()
        .oneshot(
            Request::put(format!("/v1/artifacts/uploads/{id}/parts/1"))
                .header("authorization", &auth)
                .header("x-upload-token", token)
                .body(Body::from(content))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(part.status(), StatusCode::OK);
    let part = json_body(part).await;
    let completed = router
        .clone()
        .oneshot(
            Request::post(format!("/v1/artifacts/uploads/{id}/complete"))
                .header("authorization", &auth)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"upload_token":token,"parts":[part]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(completed.status(), StatusCode::OK);
    json_body(completed).await
}

#[tokio::test]
async fn upload_grant_is_multifile_fixed_lifetime_and_upload_only() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let key = create_key(&gateway).await;
    let now = Arc::new(AtomicI64::new(1_800_000_000_000));
    let clock = Arc::clone(&now);
    gateway.upload_grants = Arc::new(UploadGrantIssuer::with_clock(
        &[42; 32],
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    let principal = stravia_runtime_contract::Principal::new(&key.id);
    let grant = gateway.upload_grants.issue(&principal).unwrap();
    let router = create_router(gateway.clone());

    let first = upload(&router, &grant.key, "first file").await;
    now.fetch_add(14 * 60 * 1000, Ordering::SeqCst);
    let second = upload(&router, &grant.key, "second file").await;
    assert_ne!(first["reference"], second["reference"]);
    let store = gateway.artifact_store().unwrap();
    let id = stravia_runtime_contract::artifact::ArtifactId::from_reference(
        first["reference"].as_str().unwrap(),
    )
    .unwrap();
    let (_, bytes) = store
        .read_bytes(&principal, &id, std::time::Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), b"first file");

    let models = router
        .clone()
        .oneshot(
            Request::get("/v1/models")
                .header("authorization", format!("Bearer {}", grant.key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::UNAUTHORIZED);

    now.fetch_add(60 * 1000, Ordering::SeqCst);
    let expired = router
        .oneshot(
            Request::post("/v1/artifacts/uploads")
                .header("authorization", format!("Bearer {}", grant.key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mime_type":"text/plain","size":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
    gateway.shutdown().await;
}

#[tokio::test]
async fn issued_upload_grant_survives_restart_but_not_owner_revocation() {
    let directory = tempfile::tempdir().unwrap();
    let config = GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config.clone()).await.unwrap();
    let key = create_key(&gateway).await;
    let principal = stravia_runtime_contract::Principal::new(&key.id);
    let grant = gateway.upload_grants.issue(&principal).unwrap();
    gateway.shutdown().await;
    drop(gateway);

    let gateway = Gateway::new(config).await.unwrap();
    let router = create_router(gateway.clone());
    // 默认关闭注入，不撤销已经交付的授权；上传认证仍检查所属 API Key。
    upload(&router, &grant.key, "after restart").await;
    gateway.admin().delete_api_key(&key.id).await.unwrap();
    let denied = router
        .oneshot(
            Request::post("/v1/artifacts/uploads")
                .header("authorization", format!("Bearer {}", grant.key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mime_type":"text/plain","size":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    gateway.shutdown().await;
}

#[tokio::test]
async fn signed_download_reads_the_completed_file_without_an_api_key() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let key = create_key(&gateway).await;
    let router = create_router(gateway.clone());
    let completed = upload(&router, &key.token, "complete file bytes").await;
    let reference = completed["reference"].as_str().unwrap();
    let id = stravia_runtime_contract::artifact::ArtifactId::from_reference(reference).unwrap();
    let store = gateway.artifact_store().unwrap();
    let principal = stravia_runtime_contract::Principal::new(&key.id);
    let settings = stravia_runtime_contract::artifact::ArtifactSettings {
        client_base_url: "https://client.example:8443/deployment".into(),
        ..Default::default()
    };
    let grant = store
        .download(
            &principal,
            &id,
            std::time::Duration::from_secs(60),
            &settings,
        )
        .await
        .unwrap();
    assert!(
        grant
            .url
            .starts_with("https://client.example:8443/deployment/v1/artifacts/downloads/")
    );
    let path = grant
        .url
        .strip_prefix("https://client.example:8443/deployment")
        .unwrap();
    let response = router
        .clone()
        .oneshot(
            Request::get(path)
                .header("host", "attacker.invalid")
                .header("x-forwarded-host", "attacker.invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .as_ref(),
        b"complete file bytes"
    );

    let denied = store
        .download(
            &stravia_runtime_contract::Principal::new("another-principal"),
            &id,
            std::time::Duration::from_secs(60),
            &settings,
        )
        .await;
    assert!(matches!(
        denied,
        Err(stravia_runtime_contract::artifact::ArtifactError::Forbidden
            | stravia_runtime_contract::artifact::ArtifactError::NotFound)
    ));
    let tampered = router
        .oneshot(
            Request::get(format!("{path}tampered"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!tampered.status().is_success());
    gateway.shutdown().await;
}
