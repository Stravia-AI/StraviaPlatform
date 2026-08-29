use super::*;

#[tokio::test]
async fn principal_concurrency_limit_rejects_new_roots_until_delivery_completes() {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-principal-admission-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(BlockingRequestHook {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }))
        .build()
        .await
        .expect("gateway init");
    let headers = authorized_headers(&gateway).await;
    set_concurrency_limit(&gateway, 1).await;
    let run = |gateway: Gateway, headers: HeaderMap| RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({ "model": "__admission_blocked__" })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new("__admission_blocked__", Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    };

    let first = tokio::spawn(execute(run(gateway.clone(), headers.clone())));
    entered.notified().await;

    let rejected = execute(run(gateway.clone(), headers.clone())).await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(rejected.headers().get(header::RETRY_AFTER).is_none());
    let rejected: serde_json::Value = serde_json::from_slice(
        &to_bytes(rejected.into_body(), usize::MAX)
            .await
            .expect("concurrency rejection body"),
    )
    .expect("concurrency rejection JSON");
    assert_eq!(rejected["error"]["type"], "STRAVIA_CONCURRENCY_LIMIT");

    release.notify_one();
    let first = first.await.expect("first root request task");
    assert_eq!(first.status(), StatusCode::OK);
    let _ = to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first response body");

    let third = tokio::spawn(execute(run(gateway.clone(), headers)));
    entered.notified().await;
    release.notify_one();
    let third = third.await.expect("third root request task");
    assert_eq!(third.status(), StatusCode::OK);
    let _ = to_bytes(third.into_body(), usize::MAX)
        .await
        .expect("third response body");
}

#[tokio::test]
async fn principal_concurrency_limit_allows_multiple_slots_and_isolates_principals() {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-principal-admission-matrix-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(BlockingRequestHook {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }))
        .build()
        .await
        .expect("gateway init");
    let first_headers = authorized_headers(&gateway).await;
    set_concurrency_limit(&gateway, 2).await;
    let second_key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "isolated-principal".into(),
            concurrency_limit: Some(1),
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: Vec::new(),
            inject_media_understanding: false,
        })
        .await
        .expect("second API key");
    let second_headers = bearer_headers(&second_key.token);
    let run = |gateway: Gateway, headers: HeaderMap| RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({ "model": "__admission_matrix__" })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new("__admission_matrix__", Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    };

    let first = tokio::spawn(execute(run(gateway.clone(), first_headers.clone())));
    entered.notified().await;
    let second = tokio::spawn(execute(run(gateway.clone(), first_headers.clone())));
    entered.notified().await;
    let third = execute(run(gateway.clone(), first_headers)).await;
    assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);
    release.notify_one();
    release.notify_one();
    for request in [first, second] {
        let response = request.await.expect("accepted request");
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("accepted response body");
    }

    let first_principal = tokio::spawn(execute(run(gateway.clone(), second_headers)));
    entered.notified().await;
    let other_principal = tokio::spawn(execute(run(
        gateway.clone(),
        authorized_headers(&gateway).await,
    )));
    entered.notified().await;
    release.notify_one();
    release.notify_one();
    for request in [first_principal, other_principal] {
        let response = request.await.expect("isolated request");
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("isolated response body");
    }
}
