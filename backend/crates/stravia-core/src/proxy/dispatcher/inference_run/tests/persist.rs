use super::*;

#[tokio::test]
async fn delivered_root_hook_response_is_continuable_without_a_target() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(RuntimeShortCircuitHook))
    .build()
    .await
    .expect("Gateway");
    let headers = authorized_headers(&gateway).await;
    let principal = crate::proxy::security::Security::new(gateway.storage.auth())
        .required_principal(
            &crate::proxy::security::ClientCredential::from_inference_headers(&headers),
        )
        .await
        .expect("Principal");
    let mut request = AiRequest::new("__lifecycle_short_circuit__", Vec::new());
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(Default::default()),
    );
    let response = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        request,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("delivered root");
    let root: serde_json::Value = serde_json::from_slice(&body).expect("root response");
    let root_id = root["id"].as_str().expect("root identity").to_owned();
    let mut continuation = AiRequest::new("__lifecycle_short_circuit__", Vec::new());
    continuation.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                previous_response_id: Some(root_id.clone()),
                ..Default::default()
            },
        ),
    );
    assert_eq!(
        gateway
            .generation_chains
            .continuation_lookup()
            .preferred_target(&principal, &continuation)
            .await,
        None,
    );
    let response = execute_request_with_headers(
        gateway,
        headers,
        continuation,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("delivered continuation");
    let continued: serde_json::Value = serde_json::from_slice(&body).expect("continued response");
    assert_ne!(continued["id"].as_str(), Some(root_id.as_str()));
    assert!(String::from_utf8_lossy(&body).contains("handled by lifecycle Hook"));
}

#[tokio::test]
async fn cancelled_run_stops_before_provider_io() {
    let (base_url, provider_calls) =
        serve_openai_response(200, openai_response("must not be called")).await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-cancellation-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
    configure_route(&gateway, "cancelled-route", &[base_url]).await;
    let context = RequestContext::new(
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        std::time::Duration::from_secs(30),
    );
    context.cancellation.cancel();
    let headers = authorized_headers(&gateway).await;
    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({
                "model": "cancelled-route",
                "messages": []
            })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new("cancelled-route", Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context,
    })
    .await;

    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("cancellation response body");
    assert_eq!(status, 499, "{}", String::from_utf8_lossy(&body));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn automatic_parent_discovery_failure_falls_back_to_a_chat_root() {
    let (base_url, calls) = serve_openai_sequence(vec![openai_response("answer")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let mut gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    gateway.generation_chains = crate::generation_chain::GenerationChain::from_turn_chain(
        Arc::new(failing_parent_discovery_store().await),
        std::time::Duration::from_secs(60),
        None,
    );
    let model = "parent-discovery-fallback";
    configure_route(&gateway, model, &[base_url]).await;
    let headers = authorized_headers(&gateway).await;
    let mut first = stravia_runtime_contract::protocol::ir::AiItem::output_text("first");
    first.role = stravia_runtime_contract::protocol::ir::Role::User;
    let mut second = stravia_runtime_contract::protocol::ir::AiItem::output_text("second");
    second.role = stravia_runtime_contract::protocol::ir::Role::User;

    let response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        AiRequest::new(model, vec![first, second]),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn embeddings_skip_generation_chain_begin() {
    let embedding_response = serde_json::json!({
        "object": "list",
        "data": [{
            "object": "embedding",
            "index": 0,
            "embedding": [0.25, 0.75]
        }],
        "model": "provider-model",
        "usage": {
            "prompt_tokens": 1,
            "total_tokens": 1
        }
    });
    let (base_url, calls) = serve_openai_sequence(vec![embedding_response]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let mut gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let history = Arc::new(failing_parent_discovery_store().await);
    let discovery_attempts = Arc::clone(&history.discovery_attempts);
    gateway.generation_chains = crate::generation_chain::GenerationChain::from_turn_chain(
        history,
        std::time::Duration::from_secs(60),
        None,
    );
    let model = "embedding-without-generation-chain";
    configure_route(&gateway, model, &[base_url]).await;
    let headers = authorized_headers(&gateway).await;
    let mut request = AiRequest::new(
        model,
        vec![
            stravia_runtime_contract::protocol::ir::AiItem::output_text("history 1"),
            stravia_runtime_contract::protocol::ir::AiItem::output_text("history 2"),
        ],
    );
    request.embedding = Some(stravia_runtime_contract::protocol::ir::EmbeddingRequest {
        input: stravia_runtime_contract::protocol::ir::EmbeddingInput::Text("embed me".into()),
        dimensions: None,
        encoding_format: None,
        user: None,
    });

    let response = execute_request_with_headers(
        gateway,
        headers,
        request,
        OPENAI_COMPATIBLE_EMBEDDINGS_V1,
        "/v1/embeddings",
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(discovery_attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn delivered_terminal_publishes_response_chain() {
    let (provider_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse("first output"),
        openai_sse("continued output"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
    configure_route(&gateway, "delivered-terminal", &[provider_url]).await;

    let response = execute_protocol_request(
        gateway.clone(),
        "delivered-terminal",
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
        true,
    )
    .await;
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("complete stream body");
    let body = String::from_utf8(body.to_vec()).expect("UTF-8 stream body");
    assert!(body.contains("response.completed"));
    let response_id = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .find_map(|event| {
            event
                .pointer("/response/id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .expect("gateway response ID");
    assert!(stravia_runtime_contract::identifier::valid_id(&response_id));

    let mut continuation = AiRequest::new("delivered-terminal", Vec::new());
    continuation.stream.enabled = true;
    continuation.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                previous_response_id: Some(response_id.clone()),
                ..Default::default()
            },
        ),
    );
    let headers = authorized_headers(&gateway).await;
    let continuation_response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({
                "model": "delivered-terminal",
                "previous_response_id": response_id,
                "stream": true
            })),
            HashMap::new(),
            "POST",
            "/v1/responses",
        ),
        request: continuation,
        ingress: OPEN_RESPONSES_2026_04_24,
        context: RequestContext::new(
            OPEN_RESPONSES_2026_04_24,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;
    assert_eq!(continuation_response.status(), StatusCode::OK);
    let continuation_body = to_bytes(continuation_response.into_body(), usize::MAX)
        .await
        .expect("continuation stream body");
    assert!(String::from_utf8_lossy(&continuation_body).contains("continued output"));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn terminal_commit_window_keeps_generation_and_interaction_parentage_aligned() {
    let (provider_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse("first output"),
        openai_sse("second output"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let mut gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let barrier = Arc::new(CommitBarrierStore::new(Arc::clone(&gateway.turn_chains)));
    gateway.generation_chains = crate::generation_chain::GenerationChain::from_turn_chain(
        barrier.clone(),
        std::time::Duration::from_secs(60),
        None,
    );
    configure_route(&gateway, "commit-window-parent", &[provider_url]).await;
    let headers = authorized_headers(&gateway).await;
    let mut events = gateway.observation.subscribe(0);
    let question = stravia_runtime_contract::protocol::ir::AiItem {
        role: stravia_runtime_contract::protocol::ir::Role::User,
        content: stravia_runtime_contract::protocol::ir::MessageContent::Text("first".into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut first = AiRequest::new("commit-window-parent", vec![question.clone()]);
    first.stream.enabled = true;
    first.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(Default::default()),
    );

    barrier.block_next_commit();
    let first_response = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        first,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let mut chunks = first_response.into_body().into_data_stream();
    let mut wire = String::new();
    while !wire.contains("data: [DONE]\n\n") {
        let chunk = chunks
            .next()
            .await
            .expect("terminal frame")
            .expect("terminal bytes");
        wire.push_str(std::str::from_utf8(&chunk).expect("SSE UTF-8"));
    }
    let completed = wire
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("completed response");
    let first_id = completed["response"]["id"]
        .as_str()
        .expect("response ID")
        .to_owned();
    drop(chunks);
    barrier.wait_until_blocked().await;

    let mut second = AiRequest::new(
        "commit-window-parent",
        vec![
            question,
            stravia_runtime_contract::protocol::ir::AiItem::output_text("first output"),
            stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                    "second".into(),
                ),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );
    second.stream.enabled = true;
    second.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(Default::default()),
    );
    let continuation = tokio::spawn({
        let gateway = gateway.clone();
        let headers = headers.clone();
        async move {
            execute_request_with_headers(
                gateway,
                headers,
                second,
                OPEN_RESPONSES_2026_04_24,
                "/v1/responses",
            )
            .await
        }
    });
    tokio::task::yield_now().await;
    barrier.release_commit();

    let continuation = continuation.await.expect("continuation task");
    assert_eq!(continuation.status(), StatusCode::OK);
    let continuation_body = to_bytes(continuation.into_body(), usize::MAX)
        .await
        .expect("continuation response body");
    assert!(String::from_utf8_lossy(&continuation_body).contains("second output"));
    wait_for_observed_run_finish(&mut events).await;
    wait_for_observed_run_finish(&mut events).await;
    gateway
        .observation
        .flush()
        .await
        .expect("observation flush");

    let forest = gateway
        .observation
        .query_forest(Default::default())
        .await
        .expect("observation forest");
    let interactions: Vec<_> = forest
        .roots
        .iter()
        .flat_map(|root| root.interactions.iter())
        .collect();
    assert_eq!(
        interactions.len(),
        1,
        "commit window must not split interaction"
    );
    let detail = gateway
        .observation
        .get_interaction(&interactions[0].id, Default::default())
        .await
        .expect("interaction query")
        .expect("interaction");
    assert_eq!(detail.runs.len(), 2);
    let parent_run = detail
        .runs
        .iter()
        .find(|run| run.generation_node_id.as_deref() == Some(first_id.as_str()))
        .expect("delivered parent run");
    let continued = detail
        .runs
        .iter()
        .find(|run| run.generation_parent_id.as_deref() == Some(first_id.as_str()))
        .expect("continuation keeps the delivered Generation parent");
    assert_eq!(
        continued.parent_run_id.as_deref(),
        Some(parent_run.id.as_str())
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unavailable_observation_writer_never_holds_generation_progress() {
    let (provider_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse("first output"),
        openai_sse("second output"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route(&gateway, "observation-unavailable", &[provider_url]).await;
    let headers = authorized_headers(&gateway).await;
    gateway.observation.shutdown().await;

    let question = stravia_runtime_contract::protocol::ir::AiItem {
        role: stravia_runtime_contract::protocol::ir::Role::User,
        content: stravia_runtime_contract::protocol::ir::MessageContent::Text("first".into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut first = AiRequest::new("observation-unavailable", vec![question.clone()]);
    first.stream.enabled = true;
    first.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(Default::default()),
    );
    let first = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        first,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    let first_body = to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first response body");
    assert!(String::from_utf8_lossy(&first_body).contains("first output"));

    let mut second = AiRequest::new(
        "observation-unavailable",
        vec![
            question,
            stravia_runtime_contract::protocol::ir::AiItem::output_text("first output"),
            stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                    "second".into(),
                ),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
    );
    second.stream.enabled = true;
    second.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(Default::default()),
    );
    let second = execute_request_with_headers(
        gateway,
        headers,
        second,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    let second_body = to_bytes(second.into_body(), usize::MAX)
        .await
        .expect("second response body");
    assert!(String::from_utf8_lossy(&second_body).contains("second output"));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn store_false_keeps_the_gateway_generation_chain_available() {
    let (provider_url, provider_calls) = serve_openai_sequence(vec![
        openai_response("first output"),
        openai_response("continued output"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
    let model = "store-false-generation-chain";
    configure_route(&gateway, model, &[provider_url]).await;
    let headers = authorized_headers(&gateway).await;

    let mut first = AiRequest::new(
        model,
        vec![stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Text("first".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    first.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                store: Some(false),
                ..Default::default()
            },
        ),
    );
    let first_response = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        first,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    let first_body = to_bytes(first_response.into_body(), usize::MAX)
        .await
        .expect("first response body");
    let first_body: serde_json::Value =
        serde_json::from_slice(&first_body).expect("Open Responses response");
    let response_id = first_body["id"]
        .as_str()
        .expect("gateway response ID")
        .to_owned();

    let mut continuation = AiRequest::new(
        model,
        vec![stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Text("second".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    continuation.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                previous_response_id: Some(response_id),
                store: Some(false),
                ..Default::default()
            },
        ),
    );
    let continuation_response = execute_request_with_headers(
        gateway,
        headers,
        continuation,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(continuation_response.status(), StatusCode::OK);
    let continuation_body = to_bytes(continuation_response.into_body(), usize::MAX)
        .await
        .expect("continuation response body");
    assert!(String::from_utf8_lossy(&continuation_body).contains("continued output"));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}
