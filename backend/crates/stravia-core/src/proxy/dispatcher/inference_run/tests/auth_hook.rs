use super::*;

#[tokio::test]
async fn request_hook_response_bypasses_route_lookup_through_lifecycle_interface() {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-hook-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(RuntimeShortCircuitHook))
        .build()
        .await
        .expect("gateway init");
    let model = "__lifecycle_short_circuit__";
    let headers = authorized_headers(&gateway).await;
    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({ "model": model })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new(model, Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("hook response body");
    assert!(String::from_utf8_lossy(&body).contains("handled by lifecycle Hook"));
}

#[tokio::test]
async fn request_hook_rejection_bypasses_route_lookup_and_model_authorization() {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-hook-reject-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(RuntimeShortCircuitHook))
        .build()
        .await
        .expect("gateway init");
    let model = "__lifecycle_reject__";
    let headers = authorized_headers(&gateway).await;
    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({ "model": model })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new(model, Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;

    assert_eq!(response.status(), StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("hook rejection body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("request_rejected"), "{body}");
    assert!(
        body.contains("request rejected by lifecycle Hook"),
        "{body}"
    );
}

#[tokio::test]
async fn expired_key_is_rejected_before_request_hook_model_rewrite() {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-auth-ordering-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let initial_model = "initial-before-hook";
    let final_model = "final-after-hook";
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(RewriteModelHook {
            model: final_model.into(),
        }))
        .build()
        .await
        .expect("gateway init");
    let dummy_provider = "http://127.0.0.1:9/v1".to_string();
    configure_route(
        &gateway,
        initial_model,
        std::slice::from_ref(&dummy_provider),
    )
    .await;
    configure_route(&gateway, final_model, &[dummy_provider]).await;

    let final_route = {
        let cache = gateway.model_cache.read().await;
        cache
            .match_model(final_model)
            .cloned()
            .expect("final route")
    };
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Expired lifecycle key".into(),
            concurrency_limit: None,
            expires_at: Some("2000-01-01T00:00:00Z".into()),
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![final_route.id],
            inject_media_understanding: false,
        })
        .await
        .expect("expired API key");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.token)).expect("auth header"),
    );

    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({ "model": initial_model })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new(initial_model, Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("authorization response body");
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("authorization response JSON");
    assert_eq!(body["error"]["type"], "STRAVIA_AUTH_ERROR");
    assert_eq!(
        body["error"]["message"],
        "authentication failed: expired_api_key"
    );
    assert!(body["error"].get("request_id").is_none());
}

#[tokio::test]
async fn untrusted_credential_is_rejected_before_hook_model_rewrite() {
    let initial_model = "invalid-before-hook";
    let final_model = "invalid-after-hook";
    let gateway = gateway_rewriting_model("invalid-rewrite-test", final_model).await;
    let dummy_provider = "http://127.0.0.1:9/v1".to_string();
    configure_route(
        &gateway,
        initial_model,
        std::slice::from_ref(&dummy_provider),
    )
    .await;
    configure_route_with_id(&gateway, final_model, &[dummy_provider]).await;

    for headers in [HeaderMap::new(), bearer_headers("unknown-key")] {
        let response = execute_non_stream_request_with_headers(
            gateway.clone(),
            headers,
            AiRequest::new(initial_model, Vec::new()),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("authorization response body");

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("authorization response JSON");
        assert_eq!(body["error"]["type"], "STRAVIA_AUTH_ERROR");
        assert_eq!(
            body["error"]["message"],
            "authentication failed: invalid_api_key"
        );
        assert!(body["error"].get("request_id").is_none());
    }
}

#[tokio::test]
async fn hook_rewrite_checks_the_final_model_binding() {
    let initial_model = "bound-before-hook";
    let final_model = "unbound-after-hook";
    let gateway = gateway_rewriting_model("binding-rewrite-test", final_model).await;
    let dummy_provider = "http://127.0.0.1:9/v1".to_string();
    let initial_model_id = configure_route_with_id(
        &gateway,
        initial_model,
        std::slice::from_ref(&dummy_provider),
    )
    .await;
    configure_route_with_id(&gateway, final_model, &[dummy_provider]).await;
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Final binding key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![initial_model_id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");

    let response = execute_non_stream_request_with_headers(
        gateway,
        bearer_headers(&key.token),
        AiRequest::new(initial_model, Vec::new()),
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("authorization response body");

    assert_eq!(status, StatusCode::FORBIDDEN);
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("authorization response JSON");
    assert_eq!(body["error"]["type"], "STRAVIA_FORBIDDEN");
    assert_eq!(
        body["error"]["message"],
        "api key not allowed for this model"
    );
    assert!(body["error"].get("request_id").is_none());
}

#[tokio::test]
async fn missing_route_logs_redacted_client_headers_through_lifecycle_interface() {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-header-redaction-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, mut logs) = Gateway::new(config).await.expect("gateway init");
    let mut envelope_headers = HashMap::new();
    envelope_headers.insert("authorization".into(), "Bearer client-secret".into());
    envelope_headers.insert("x-api-key".into(), "client-key".into());
    envelope_headers.insert("content-type".into(), "application/json".into());
    let model = "missing-model";
    let headers = authorized_headers(&gateway).await;

    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({"model": model})),
            envelope_headers,
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new(model, Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let entry = tokio::time::timeout(std::time::Duration::from_secs(1), logs.recv())
        .await
        .expect("log entry should be emitted")
        .expect("log channel should remain open");
    let headers = entry
        .client_request_headers
        .as_deref()
        .expect("client headers should be logged");
    let parsed: serde_json::Value = serde_json::from_str(headers).expect("headers should be JSON");
    assert_eq!(parsed["authorization"], "***");
    assert_eq!(parsed["x-api-key"], "***");
    assert_eq!(parsed["content-type"], "application/json");
    assert!(!headers.contains("client-secret"));
    assert!(!headers.contains("client-key"));
}

#[tokio::test]
async fn hidden_round_rechecks_key_binding_after_platform_tool_execution() {
    for (mutation, status, error_type, message) in [
        (
            TestAccessMutation::DisableKey,
            StatusCode::FORBIDDEN,
            "STRAVIA_FORBIDDEN",
            "api key disabled",
        ),
        (
            TestAccessMutation::RevokeBinding,
            StatusCode::FORBIDDEN,
            "STRAVIA_FORBIDDEN",
            "api key not allowed for this model",
        ),
    ] {
        assert_hidden_round_rechecks_access(mutation, status, error_type, message).await;
    }
}

#[tokio::test]
async fn automatic_parent_materializes_rewritten_history_before_the_current_hook() {
    let (base_url, _connections, requests) =
        serve_responses_websocket_sequence(vec!["first answer", "second answer", "third answer"])
            .await;
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(PrependContextHook {
        observed: observed.clone(),
    }))
    .build()
    .await
    .expect("Gateway");
    let model = "hook-prefix-response-chain";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "openai-compatible")
        .await;
    let headers = authorized_headers(&gateway).await;

    let mut first_user = crate::protocol::ir::AiItem::output_text("first");
    first_user.role = crate::protocol::ir::Role::User;
    let mut first_request = AiRequest::new(model, vec![first_user.clone()]);
    first_request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            store: Some(true),
            ..Default::default()
        },
    ));
    let first_response = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        first_request,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let _ = to_bytes(first_response.into_body(), usize::MAX)
        .await
        .expect("first response body");

    let mut second_user = crate::protocol::ir::AiItem::output_text("second");
    second_user.role = crate::protocol::ir::Role::User;
    let mut second_request = AiRequest::new(
        model,
        vec![
            first_user,
            crate::protocol::ir::AiItem::output_text("first answer"),
            second_user,
        ],
    );
    second_request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            store: Some(true),
            ..Default::default()
        },
    ));
    let second_response = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        second_request,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("second response body");
    let second_body: serde_json::Value =
        serde_json::from_slice(&second_body).expect("second response JSON");
    let second_response_id = second_body["id"]
        .as_str()
        .expect("Gateway response ID")
        .to_owned();

    let mut third_user = crate::protocol::ir::AiItem::output_text("third");
    third_user.role = crate::protocol::ir::Role::User;
    let mut third_request = AiRequest::new(model, vec![third_user]);
    third_request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            previous_response_id: Some(second_response_id),
            store: Some(true),
            ..Default::default()
        },
    ));
    let third_response = execute_request_with_headers(
        gateway,
        headers,
        third_request,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(third_response.status(), StatusCode::OK);

    let observed = observed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(observed.len(), 3);
    assert!(
        observed[2].iter().any(|item| item == "second"),
        "explicit continuation lost the client item after automatic prefix reuse: {:?}",
        observed[2]
    );
    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(requests[1].get("previous_response_id").is_none());
    assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(5));
}
