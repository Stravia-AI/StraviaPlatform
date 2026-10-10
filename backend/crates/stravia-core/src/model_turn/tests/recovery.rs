use super::*;

mod metadata;

#[tokio::test]
async fn execute_fails_over_before_canonical_output_and_returns_the_locked_target() {
    let (failed_url, failed_calls) =
        serve_openai_status(500, serde_json::json!({"error": {"message": "retry"}})).await;
    let (fallback_url, fallback_calls) = serve_openai_response(serde_json::json!({
        "id": "chatcmpl-fallback",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "fallback"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
    }))
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let mut providers = Vec::new();
    for (name, base_url) in [("primary", failed_url), ("fallback", fallback_url)] {
        let provider = admin
            .create_provider(CreateProvider {
                name: Some(name.into()),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some("openai-compatible".into()),
                    base_url,
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "test-provider-key".into(),
                },
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await
            .expect("Provider");
        providers.push(provider);
    }
    for provider in &providers {
        add_test_provider_model(&gateway, &provider.id).await;
    }
    let model = admin
        .create_model(CreateRoute {
            model_id: "failover-model".into(),
            display_name: None,
            balance: Some("traffic_equalization".into()),
            targets: providers
                .iter()
                .enumerate()
                .map(|(index, provider)| CreateTarget {
                    enabled: true,
                    provider_id: provider.id.clone(),
                    model: "upstream-model".into(),
                    priority: Some((providers.len() - index) as i32),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(0),
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                })
                .collect(),
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Failover key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");

    let turn = gateway
        .model_turn
        .execute(TurnInput::new(
            Principal::new(key.id),
            AiRequest::new("failover-model", Vec::new()),
        ))
        .await
        .expect("fallback Model Turn");
    let locked_provider = turn.route.provider_id.clone();
    let events = turn.output.collect::<Vec<_>>().await;

    assert_eq!(locked_provider, providers[1].id);
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, Ok(CanonicalEvent::Delta(_))) })
    );
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(response)))
            if response.output_text() == "fallback"
    ));
}

#[tokio::test]
async fn http_continuation_not_retained_by_zdr_replays_full_request_once() {
    let (base_url, captured) = serve_zdr_then_responses_stream().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("ZDR".into()),
            source: ProviderSourceInput::Custom {
                vendor: "xai".into(),
                channel: "default".into(),
                protocol: Some("open-responses".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    add_test_provider_model(&gateway, &provider.id).await;
    let model = admin
        .create_model(CreateRoute {
            model_id: "zdr-model".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id,
                model: "upstream-model".into(),
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
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "ZDR key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let executor = LiveModelTurnExecutor::new(
        gateway,
        crate::router::continuation::ScriptedContinuation::hit("resp-zdr"),
    );
    let mut request = AiRequest::new(
        "zdr-model",
        vec![stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                "follow-up".to_owned().into(),
            ),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = true;
    request.meta.source_protocol =
        Some(stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24);
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt::default(),
        ),
    );

    let turn = executor
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("ZDR continuation falls back to full replay");
    let events = turn.output.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(response)))
            if response.id == "resp-replayed"
    ));

    let captured = captured.lock();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["previous_response_id"], "resp-zdr");
    assert!(captured[1].get("previous_response_id").is_none());
}

#[tokio::test]
async fn execute_does_not_fail_over_after_the_first_canonical_delta() {
    let (partial_url, partial_calls) = serve_incomplete_openai_stream(6).await;
    let (fallback_url, fallback_calls) = serve_openai_response(serde_json::json!({
        "id": "chatcmpl-fallback",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "must not run"},
            "finish_reason": "stop"
        }]
    }))
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let mut providers = Vec::new();
    for (name, base_url) in [("partial", partial_url), ("fallback", fallback_url)] {
        providers.push(
            admin
                .create_provider(CreateProvider {
                    name: Some(name.into()),
                    source: ProviderSourceInput::Custom {
                        vendor: "custom".into(),
                        channel: "default".into(),
                        protocol: Some("openai-compatible".into()),
                        base_url,
                        models_source: None,
                        static_models: None,
                    },
                    credential: ProviderCredentialInput::ApiKey {
                        value: "test-provider-key".into(),
                    },
                    vendor_options: Default::default(),
                    use_proxy: false,
                })
                .await
                .expect("Provider"),
        );
    }
    for provider in &providers {
        add_test_provider_model(&gateway, &provider.id).await;
    }
    let model = admin
        .create_model(CreateRoute {
            model_id: "stream-lock-model".into(),
            display_name: None,
            balance: Some("traffic_equalization".into()),
            targets: providers
                .iter()
                .enumerate()
                .map(|(index, provider)| CreateTarget {
                    enabled: true,
                    provider_id: provider.id.clone(),
                    model: "upstream-model".into(),
                    priority: Some((providers.len() - index) as i32),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(5),
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                })
                .collect(),
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Stream lock key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let mut request = AiRequest::new("stream-lock-model", Vec::new());
    request.stream.enabled = true;

    for failure_count in 1..=6 {
        let turn = gateway
            .model_turn
            .execute(TurnInput::new(
                Principal::new(key.id.clone()),
                request.clone(),
            ))
            .await
            .expect("streaming Model Turn locks the first Target");
        assert_eq!(turn.route.provider_id, providers[0].id);
        let mut output = turn.output;
        let events = output.by_ref().collect::<Vec<_>>().await;

        assert!(events.iter().any(
            |event| matches!(event, Ok(CanonicalEvent::Delta(AiStreamDelta::TextDelta(text))) if text == "partial")
        ));
        assert!(matches!(
            events.last(),
            Some(Err(ModelTurnError { code, .. })) if code == "upstream_stream_error"
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(CanonicalEvent::Completed(_))))
        );
        for _ in 0..3 {
            assert!(output.next().await.is_none());
        }
        assert_eq!(partial_calls.load(Ordering::SeqCst), failure_count);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }
    let recovered = gateway
        .model_turn
        .execute(TurnInput::new(
            Principal::new(key.id),
            AiRequest::new("stream-lock-model", Vec::new()),
        ))
        .await
        .expect("sixth failure makes the next request choose fallback");
    assert_eq!(recovered.route.provider_id, providers[1].id);
    let events = recovered.output.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(_)))
    ));
    assert_eq!(partial_calls.load(Ordering::SeqCst), 6);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
}
