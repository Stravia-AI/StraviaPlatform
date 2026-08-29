use super::*;

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
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
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
    let (mut gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
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
    let mut first = crate::protocol::ir::AiItem::output_text("first");
    first.role = crate::protocol::ir::Role::User;
    let mut second = crate::protocol::ir::AiItem::output_text("second");
    second.role = crate::protocol::ir::Role::User;

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
    let (mut gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
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
            crate::protocol::ir::AiItem::output_text("history 1"),
            crate::protocol::ir::AiItem::output_text("history 2"),
        ],
    );
    request.embedding = Some(crate::protocol::ir::EmbeddingRequest {
        input: crate::protocol::ir::EmbeddingInput::Text("embed me".into()),
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
async fn dropped_stream_before_terminal_does_not_publish_response_chain() {
    let (provider_url, _) = serve_sse_sequence(vec![openai_sse("partial output")]).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, mut logs) = Gateway::new(config).await.expect("gateway init");
    configure_route(&gateway, "dropped-terminal", &[provider_url]).await;

    let response = execute_protocol_request(
        gateway.clone(),
        "dropped-terminal",
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let first = body
        .next()
        .await
        .expect("response.created frame")
        .expect("stream frame");
    let first = String::from_utf8(first.to_vec()).expect("UTF-8 stream frame");
    let response_id_start = first
        .find("resp_")
        .unwrap_or_else(|| panic!("gateway response ID in first frame: {first}"));
    let response_id: String = first[response_id_start..]
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    drop(body);

    tokio::time::timeout(std::time::Duration::from_secs(1), logs.recv())
        .await
        .expect("stream completion log")
        .expect("log channel remains open");

    let mut continuation = AiRequest::new("dropped-terminal", Vec::new());
    continuation.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            previous_response_id: Some(response_id.clone()),
            ..Default::default()
        },
    ));
    let headers = authorized_headers(&gateway).await;
    let continuation_response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({
                "model": "dropped-terminal",
                "previous_response_id": response_id
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
    assert_eq!(continuation_response.status(), StatusCode::BAD_REQUEST);
    let continuation_body = to_bytes(continuation_response.into_body(), usize::MAX)
        .await
        .expect("continuation error body");
    assert!(String::from_utf8_lossy(&continuation_body).contains("previous_response_not_found"));
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
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
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
    let response_id_start = body.find("resp_").expect("gateway response ID");
    let response_id: String = body[response_id_start..]
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();

    let mut continuation = AiRequest::new("delivered-terminal", Vec::new());
    continuation.stream.enabled = true;
    continuation.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            previous_response_id: Some(response_id.clone()),
            ..Default::default()
        },
    ));
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
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
    let model = "store-false-generation-chain";
    configure_route(&gateway, model, &[provider_url]).await;
    let headers = authorized_headers(&gateway).await;

    let mut first = AiRequest::new(
        model,
        vec![crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::User,
            content: crate::protocol::ir::MessageContent::Text("first".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    first.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            store: Some(false),
            ..Default::default()
        },
    ));
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
        vec![crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::User,
            content: crate::protocol::ir::MessageContent::Text("second".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    continuation.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            previous_response_id: Some(response_id),
            store: Some(false),
            ..Default::default()
        },
    ));
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
