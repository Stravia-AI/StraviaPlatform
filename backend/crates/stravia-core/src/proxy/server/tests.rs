use crate::config::GatewayConfig;
use crate::db::models::{
    CreateModel, CreateProvider, CreateWebProvider, ProviderCredentialInput, ProviderSourceInput,
    WebAccessSettings,
};
use crate::provider_models::CreateManualProviderModel;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

use super::*;

async fn proxy_router() -> Router {
    let config = GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-proxy-body-limit-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _log_rx) = Gateway::new(config).await.expect("gateway init");
    create_router(gateway)
}

async fn protected_responses_router() -> (Router, String) {
    protected_responses_router_with_key_state(true).await
}

async fn protected_responses_router_with_key_state(enabled: bool) -> (Router, String) {
    let data_dir = tempfile::tempdir().expect("temp data dir").keep();
    let config = GatewayConfig {
        data_dir,
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Open Responses auth target".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "open-responses".into(),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
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
                    "name": "Auth model",
                    "tool_call": true,
                    "reasoning_options": [{
                        "type": "effort",
                        "values": ["none", "low", "medium", "high", "xhigh"]
                    }]
                }),
            },
        )
        .await
        .expect("Provider Model");
    let model = admin
        .create_model(CreateModel {
            name: "auth-model".into(),
            balance: None,
            target_provider: provider.id,
            target_model: "auth-model".into(),
            targets: vec![],
        })
        .await
        .expect("Model Route");
    let api_key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Responses auth key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model.id],
            inject_media_understanding: false,
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
                    concurrency_limit: None,
                    is_enabled: Some(false),
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    inject_media_understanding: None,
                    expires_at: None,
                    model_ids: None,
                },
            )
            .await
            .expect("disable API key");
    }
    (create_router(gateway), api_key.token)
}

#[tokio::test]
async fn unknown_protocol_paths_and_methods_use_canonical_not_found() {
    for (method, uri) in [("GET", "/v1/unknown"), ("DELETE", "/v1/responses")] {
        let response = proxy_router()
            .await
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

    let response = proxy_router()
        .await
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
}

#[tokio::test]
async fn responses_accepts_json_media_type_parameters() {
    let response = proxy_router()
        .await
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(r#"{"model":"missing","input":"hello"}"#))
                .expect("Responses request"),
        )
        .await
        .expect("Responses response");

    assert_ne!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn responses_rejects_form_body_with_canonical_error() {
    let response = proxy_router()
        .await
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
}

#[tokio::test]
async fn responses_bearer_scheme_is_case_insensitive() {
    let (router, token) = protected_responses_router().await;
    let response = router
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("bEaReR {token}"))
                .body(Body::from(r#"{"model":"auth-model","input":"hello"}"#))
                .expect("Responses request"),
        )
        .await
        .expect("Responses response");

    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
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
        let response = proxy_router()
            .await
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
    }
}

#[tokio::test]
async fn responses_compact_requires_valid_authentication() {
    let (router, _) = protected_responses_router().await;
    let response = router
        .oneshot(
            Request::post("/v1/responses/compact")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"auth-model","input":"hello"}"#))
                .expect("compact request"),
        )
        .await
        .expect("compact response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn responses_compact_authenticates_before_parsing_the_body() {
    let (router, _) = protected_responses_router().await;
    for (content_type, body) in [
        ("application/json", "{"),
        ("text/plain", r#"{"model":"auth-model","input":"hello"}"#),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/responses/compact")
                    .header("content-type", content_type)
                    .body(Body::from(body))
                    .expect("compact request"),
            )
            .await
            .expect("compact response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn responses_compact_preserves_authorization_failures_before_parsing_the_body() {
    let (router, token) = protected_responses_router_with_key_state(false).await;
    let response = router
        .oneshot(
            Request::post("/v1/responses/compact")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from("{"))
                .expect("compact request"),
        )
        .await
        .expect("compact response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("compact response body"),
    )
    .expect("compact JSON");
    assert_eq!(body["error"]["code"], "permission_denied");
}

#[tokio::test]
async fn responses_compact_is_an_explicit_unsupported_feature() {
    let (router, token) = protected_responses_router().await;
    let response = router
        .oneshot(
            Request::post("/v1/responses/compact")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(r#"{"model":"auth-model","input":"hello"}"#))
                .expect("compact request"),
        )
        .await
        .expect("compact response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("compact response body"),
    )
    .expect("compact JSON");
    assert_eq!(
        body,
        serde_json::json!({
            "error": {
                "type": "invalid_request",
                "code": "unsupported_feature",
                "param": "compact",
                "message": "Response compaction is not supported."
            }
        })
    );
}

#[tokio::test]
async fn responses_compact_validates_its_dated_request_schema_before_rejecting_support() {
    let (router, token) = protected_responses_router().await;
    let response = router
        .oneshot(
            Request::post("/v1/responses/compact")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    r#"{"model":"auth-model","input":"hello","tools":[]}"#,
                ))
                .expect("malformed compact request"),
        )
        .await
        .expect("compact response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("compact response body"),
    )
    .expect("compact JSON");
    assert_eq!(body["error"]["code"], "invalid_request");
    assert_eq!(body["error"]["param"], "body");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown compact request field 'tools'"))
    );
}

#[tokio::test]
async fn responses_compact_accepts_dated_compaction_input_before_rejecting_support() {
    let (router, token) = protected_responses_router().await;
    let response = router
            .oneshot(
                Request::post("/v1/responses/compact")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(
                        r#"{"model":"auth-model","input":[{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}]}"#,
                    ))
                    .expect("compact request"),
            )
            .await
            .expect("compact response");
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("compact response body"),
    )
    .expect("compact JSON");

    assert_eq!(body["error"]["type"], "invalid_request");
    assert_eq!(body["error"]["code"], "unsupported_feature");
    assert_eq!(body["error"]["param"], "compact");
}

#[tokio::test]
async fn responses_rejects_background_with_canonical_error() {
    let (router, token) = protected_responses_router().await;
    let response = router
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
}
#[tokio::test]
async fn responses_websocket_rejects_unknown_event_types() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (router, token) = protected_responses_router().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind WebSocket test listener");
    let address = listener.local_addr().expect("WebSocket test address");
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve WebSocket test router");
    });
    let mut request = format!("ws://{address}/v1/responses")
        .into_client_request()
        .expect("WebSocket request");
    request.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {token}")
            .parse()
            .expect("Authorization header"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("WebSocket handshake");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"unknown.event"}"#.into(),
        ))
        .await
        .expect("send unknown event");
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .expect("WebSocket response timeout")
        .expect("WebSocket response")
        .expect("WebSocket message");
    let tokio_tungstenite::tungstenite::Message::Text(message) = message else {
        panic!("JSON text error event");
    };
    let body: serde_json::Value = serde_json::from_str(&message).expect("WebSocket error JSON");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["code"], "invalid_request");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"response.create","model":"auth-model","input":"first"}"#.into(),
        ))
        .await
        .expect("send first response.create");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"response.create","model":"auth-model","input":"second"}"#.into(),
        ))
        .await
        .expect("send concurrent response.create");
    let response_in_progress = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(message) = socket.next().await {
            let Ok(tokio_tungstenite::tungstenite::Message::Text(message)) = message else {
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

    socket.close(None).await.expect("close WebSocket");
    server.abort();
}
#[tokio::test]
async fn responses_native_web_search_is_concealed_when_search_is_unavailable() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("No tools".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: "https://example.invalid".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await
        .expect("Provider");
    admin
        .create_manual_provider_model(
            &provider.id,
            "no-tools",
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "no-tools",
                    "name": "No tools",
                    "tool_call": false
                }),
            },
        )
        .await
        .expect("Provider Model");
    let model = admin
        .create_model(CreateModel {
            name: "web-search-test".into(),
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "no-tools".into(),
            targets: vec![],
        })
        .await
        .expect("Model Route");
    let api_key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Web key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model.id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let web_provider = admin
        .create_web_provider(CreateWebProvider {
            name: "Brave".into(),
            kind: "brave".into(),
            api_key: Some("secret".into()),
        })
        .await
        .expect("Web Provider");
    admin
        .update_web_access_settings(WebAccessSettings {
            enabled: true,
            search_provider_ids: vec![web_provider.id],
            fetch_provider_ids: vec![],
        })
        .await
        .expect("Web Access settings");

    let response = create_router(gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", api_key.token))
                .body(Body::from(
                    serde_json::json!({
                        "model": "web-search-test",
                        "input": "Search the web",
                        "tools": [{ "type": "stravia:web_search" }]
                    })
                    .to_string(),
                ))
                .expect("Responses request"),
        )
        .await
        .expect("Responses response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body"),
    )
    .expect("response JSON");
    assert_eq!(body["error"]["code"], "web_search_unavailable", "{body}");
}
#[tokio::test]
async fn artifact_upload_is_api_key_scoped_and_completes() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (gateway, _logs) = Gateway::new(GatewayConfig {
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
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let router = create_router(gateway);
    let auth = format!("Bearer {}", api_key.token);

    let response = router
        .clone()
        .oneshot(
            Request::post("/v1/artifacts/uploads")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(r#"{"mime_type":"image/png","size":3}"#))
                .expect("upload request"),
        )
        .await
        .expect("upload response");
    assert_eq!(response.status(), StatusCode::CREATED);
    let upload: crate::agent::ArtifactUpload = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("upload body"),
    )
    .expect("Artifact upload");

    let response = router
        .clone()
        .oneshot(
            Request::put(format!(
                "/v1/artifacts/uploads/{}/parts/1",
                upload.upload_id
            ))
            .header("authorization", &auth)
            .header("x-upload-token", &upload.upload_token)
            .body(Body::from("png"))
            .expect("part request"),
        )
        .await
        .expect("part response");
    assert_eq!(response.status(), StatusCode::OK);
    let part: crate::agent::UploadedArtifactPart = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("part body"),
    )
    .expect("Artifact part");

    let response = router
        .oneshot(
            Request::post(format!(
                "/v1/artifacts/uploads/{}/complete",
                upload.upload_id
            ))
            .header("content-type", "application/json")
            .header("authorization", auth)
            .body(Body::from(
                serde_json::json!({
                    "upload_token": upload.upload_token,
                    "parts": [part],
                })
                .to_string(),
            ))
            .expect("complete request"),
        )
        .await
        .expect("complete response");
    assert_eq!(response.status(), StatusCode::OK);
}
