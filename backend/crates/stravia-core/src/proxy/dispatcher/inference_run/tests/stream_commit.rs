use super::*;

#[derive(Clone, Copy)]
enum ThinkingMarkerFailure {
    Persist,
    Publish,
}

struct FailingThinkingMarkerStore {
    inner: Arc<dyn crate::history_marker::HistoryMarkerStore>,
    failure: ThinkingMarkerFailure,
}

#[async_trait::async_trait]
impl crate::history_marker::HistoryMarkerStore for FailingThinkingMarkerStore {
    async fn create_platform(
        &self,
        principal: &crate::hook::Principal,
        input: crate::history_marker::PlatformMarkerInput,
    ) -> Result<crate::history_marker::HistoryMarker, crate::history_marker::HistoryMarkerError>
    {
        self.inner.create_platform(principal, input).await
    }

    async fn create_thinking(
        &self,
        principal: &crate::hook::Principal,
        input: crate::history_marker::ThinkingMarkerInput,
    ) -> Result<crate::history_marker::HistoryMarker, crate::history_marker::HistoryMarkerError>
    {
        if matches!(self.failure, ThinkingMarkerFailure::Persist) {
            return Err(crate::history_marker::HistoryMarkerError::Storage(
                "injected Thinking persistence failure".into(),
            ));
        }
        self.inner.create_thinking(principal, input).await
    }

    async fn create_reserved_thinking(
        &self,
        principal: &crate::hook::Principal,
        reserved: &crate::history_marker::HistoryMarker,
        input: crate::history_marker::ThinkingMarkerInput,
    ) -> Result<crate::history_marker::HistoryMarker, crate::history_marker::HistoryMarkerError>
    {
        if matches!(self.failure, ThinkingMarkerFailure::Persist) {
            return Err(crate::history_marker::HistoryMarkerError::Storage(
                "injected Thinking persistence failure".into(),
            ));
        }
        self.inner
            .create_reserved_thinking(principal, reserved, input)
            .await
    }

    async fn resolve(
        &self,
        principal: &crate::hook::Principal,
        reference: &str,
    ) -> Result<
        Option<crate::history_marker::ResolvedHistoryMarker>,
        crate::history_marker::HistoryMarkerError,
    > {
        self.inner.resolve(principal, reference).await
    }

    async fn claim_execution(
        &self,
        principal: &crate::hook::Principal,
        reference: &str,
        owner_id: &str,
        lease: std::time::Duration,
    ) -> Result<crate::history_marker::ClaimOutcome, crate::history_marker::HistoryMarkerError>
    {
        self.inner
            .claim_execution(principal, reference, owner_id, lease)
            .await
    }

    async fn finish_execution(
        &self,
        principal: &crate::hook::Principal,
        reference: &str,
        owner_id: &str,
        state: crate::history_marker::PlatformExecutionState,
        segment: crate::history_marker::HiddenHistorySegment,
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        self.inner
            .finish_execution(principal, reference, owner_id, state, segment)
            .await
    }

    async fn wait_terminal(
        &self,
        principal: &crate::hook::Principal,
        reference: &str,
    ) -> Result<
        Option<crate::history_marker::ResolvedHistoryMarker>,
        crate::history_marker::HistoryMarkerError,
    > {
        self.inner.wait_terminal(principal, reference).await
    }

    async fn publish(
        &self,
        principal: &crate::hook::Principal,
        references: &[String],
        retention: std::time::Duration,
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        if matches!(self.failure, ThinkingMarkerFailure::Publish) {
            return Err(crate::history_marker::HistoryMarkerError::Storage(
                "injected Thinking publish failure".into(),
            ));
        }
        self.inner.publish(principal, references, retention).await
    }

    async fn extend_retention(
        &self,
        principal: &crate::hook::Principal,
        references: &[String],
        retention: std::time::Duration,
    ) -> Result<(), crate::history_marker::HistoryMarkerError> {
        self.inner
            .extend_retention(principal, references, retention)
            .await
    }

    async fn cleanup_expired(&self) -> Result<u64, crate::history_marker::HistoryMarkerError> {
        self.inner.cleanup_expired().await
    }
}

#[tokio::test]
async fn protected_reasoning_marker_failures_abort_after_live_summary() {
    let responses = (0..2)
        .map(|_| {
            let (summary, completion) = openai_responses_live_protected_summary_sse_parts();
            format!("{summary}{completion}")
        })
        .collect();
    let (upstream_url, provider_calls) = serve_sse_sequence(responses).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (mut gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway init");
    configure_route_with_protocol(
        &gateway,
        "failing-live-protected-reasoning",
        &[upstream_url],
        "test-http",
        "open-responses",
    )
    .await;
    let marker_store = Arc::clone(&gateway.history_markers);

    for failure in [
        ThinkingMarkerFailure::Persist,
        ThinkingMarkerFailure::Publish,
    ] {
        gateway.history_markers = Arc::new(FailingThinkingMarkerStore {
            inner: Arc::clone(&marker_store),
            failure,
        });
        let response = execute_stream(gateway.clone(), "failing-live-protected-reasoning").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("failed stream body");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("live protected "), "{body}");
        assert!(body.contains(r#""reasoning_content":"summary"#), "{body}");
        assert!(body.contains("stream_mid_error"), "{body}");
        assert!(!body.contains("opaque-reasoning"), "{body}");

        let generation_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM turn_chain_nodes WHERE kind = 'response'",
        )
        .fetch_one(gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool"))
        .await
        .expect("count Generation Chain nodes");
        assert_eq!(generation_count, 0);
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn disconnect_during_post_text_preview_persists_no_marker_or_generation_node() {
    let first_events = [
        serde_json::json!({
            "id": "upstream-cancel-preview",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": "C1"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-cancel-preview",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"reasoning_content": "R2"},
                "finish_reason": null
            }]
        }),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect();
    let remaining_events = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "upstream-cancel-preview",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        })
    );
    let (upstream_url, _calls, release_upstream) =
        serve_gated_sse(first_events, remaining_events).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway init");
    configure_route(&gateway, "cancel-post-text-preview", &[upstream_url]).await;

    let response = execute_stream(gateway.clone(), "cancel-post-text-preview").await;
    let mut chunks = response.into_body().into_data_stream();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut prefix = String::new();
        while !prefix.contains(crate::history_marker::PROJECTION_DELIMITER_PREFIX) {
            let chunk = chunks
                .next()
                .await
                .expect("stream ended before Preview")
                .expect("stream chunk");
            prefix.push_str(std::str::from_utf8(&chunk).expect("UTF-8 stream chunk"));
        }
    })
    .await
    .expect("Post-Text Preview should stream before terminal");
    drop(chunks);
    release_upstream
        .send(())
        .expect("release upstream completion after disconnect");
    tokio::task::yield_now().await;

    let pool = gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool");
    let marker_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM history_markers")
        .fetch_one(pool)
        .await
        .expect("count History Markers");
    let generation_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM turn_chain_nodes WHERE kind = 'response'",
    )
    .fetch_one(pool)
    .await
    .expect("count Generation Chain nodes");
    assert_eq!(marker_count, 0);
    assert_eq!(generation_count, 0);
}

#[tokio::test]
async fn post_text_marker_failures_abort_stream_and_skip_generation_commit() {
    let (upstream_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse_text_thinking_text(),
        openai_sse_text_thinking_text(),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (mut gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway init");
    configure_route(&gateway, "failing-post-text-marker", &[upstream_url]).await;
    let marker_store = Arc::clone(&gateway.history_markers);

    for failure in [
        ThinkingMarkerFailure::Persist,
        ThinkingMarkerFailure::Publish,
    ] {
        gateway.history_markers = Arc::new(FailingThinkingMarkerStore {
            inner: Arc::clone(&marker_store),
            failure,
        });
        let response = execute_stream(gateway.clone(), "failing-post-text-marker").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("failed stream body");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("> R2"), "{body}");
        assert!(body.contains("stream_mid_error"), "{body}");
        assert!(!body.contains(r#""content":"C2""#), "{body}");

        let generation_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM turn_chain_nodes WHERE kind = 'response'",
        )
        .fetch_one(gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool"))
        .await
        .expect("count Generation Chain nodes");
        assert_eq!(generation_count, 0);
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn non_stream_post_text_marker_persistence_failure_is_typed_error() {
    let platform_round = serde_json::json!({
        "id": "buffered-marker-platform",
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "reasoning_content": "R1",
                "content": "C1",
                "tool_calls": [{
                    "id": "platform-marker-failure",
                    "type": "function",
                    "function": {
                        "name": "stravia__ordered_tool",
                        "arguments": "{\"index\":1}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let final_round = serde_json::json!({
        "id": "buffered-marker-final",
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "reasoning_content": "R2",
                "content": "C2"
            },
            "finish_reason": "stop"
        }]
    });
    let (upstream_url, provider_calls) = serve_openai_sequence(vec![
        platform_round.clone(),
        final_round.clone(),
        platform_round,
        final_round,
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (mut gateway, _logs) = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::new(std::sync::Mutex::new(Vec::new())),
    }))
    .build()
    .await
    .expect("gateway init");
    configure_route(
        &gateway,
        "failing-buffered-post-text-marker",
        &[upstream_url],
    )
    .await;
    let marker_store = Arc::clone(&gateway.history_markers);
    for failure in [
        ThinkingMarkerFailure::Persist,
        ThinkingMarkerFailure::Publish,
    ] {
        gateway.history_markers = Arc::new(FailingThinkingMarkerStore {
            inner: Arc::clone(&marker_store),
            failure,
        });
        let response =
            execute_non_stream(gateway.clone(), "failing-buffered-post-text-marker").await;
        assert!(response.status().is_server_error(), "{}", response.status());
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("typed marker failure body");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("hook_failed"), "{body}");
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn protocol_delivery_contract_matrix_covers_unary_and_sse_lifecycles() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
    let protocols = [
        (
            "openai",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
            "\"object\":\"chat.completion\"",
            "\"object\":\"chat.completion.chunk\"",
        ),
        (
            "anthropic",
            ANTHROPIC_MESSAGES_2023_06_01,
            "/v1/messages",
            "\"type\":\"message\"",
            "event: message_start",
        ),
        (
            "gemini",
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            "/v1beta/models/provider-model:generateContent",
            "\"candidates\"",
            "\"candidates\"",
        ),
    ];

    for (name, ingress, path, unary_marker, stream_marker) in protocols {
        let unary_content = format!("matrix-unary-{name}");
        let unary_model = format!("matrix-unary-model-{name}");
        let (unary_url, _) = serve_openai_response(200, openai_response(&unary_content)).await;
        configure_route(&gateway, &unary_model, &[unary_url]).await;

        let unary =
            execute_protocol_request(gateway.clone(), &unary_model, ingress, path, false).await;
        assert_eq!(unary.status(), StatusCode::OK, "{name} unary status");
        let unary_body = to_bytes(unary.into_body(), usize::MAX)
            .await
            .expect("unary contract body");
        let unary_body = String::from_utf8_lossy(&unary_body);
        assert!(
            unary_body.contains(&unary_content) && unary_body.contains(unary_marker),
            "{name} unary contract: {unary_body}"
        );

        let stream_content = format!("matrix-stream-{name}");
        let stream_model = format!("matrix-stream-model-{name}");
        let (stream_url, _) = serve_sse_sequence(vec![openai_sse(&stream_content)]).await;
        configure_route(&gateway, &stream_model, &[stream_url]).await;

        let stream =
            execute_protocol_request(gateway.clone(), &stream_model, ingress, path, true).await;
        assert_eq!(stream.status(), StatusCode::OK, "{name} stream status");
        let stream_body = to_bytes(stream.into_body(), usize::MAX)
            .await
            .expect("stream contract body");
        let stream_body = String::from_utf8_lossy(&stream_body);
        assert!(
            stream_body.contains(&stream_content) && stream_body.contains(stream_marker),
            "{name} stream contract: {stream_body}"
        );
    }
}

#[tokio::test]
async fn canonical_completion_contract_matrix_covers_four_delivery_paths() {
    let (unary_url, unary_calls) = serve_openai_response(200, openai_response("original")).await;
    let (forced_stream_url, forced_stream_calls) =
        serve_sse_sequence(vec![openai_responses_sse("original")]).await;
    let live_first_event = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "upstream-live",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": "original"},
                "finish_reason": null
            }]
        })
    );
    let live_remaining_events = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "upstream-live",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        })
    );
    let (live_stream_url, live_stream_calls, release_live_stream) =
        serve_gated_sse(live_first_event, live_remaining_events).await;
    let (buffered_stream_url, buffered_stream_calls) = serve_sse_sequence(vec![
        openai_sse_platform_tool_call(),
        openai_sse("original"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");

    let (gateway, _logs) = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().join("collected"),
        ..Default::default()
    })
    .hook(Arc::new(RewriteUpstreamHook))
    .build()
    .await
    .expect("collected gateway init");
    configure_route_with_vendor(
        &gateway,
        "normalizing-unary",
        &[unary_url],
        "normalizing-test",
    )
    .await;
    configure_route_with_protocol(
        &gateway,
        "normalizing-forced-stream",
        &[forced_stream_url],
        "normalizing-test",
        "open-responses",
    )
    .await;

    let observed_live_responses = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (live_gateway, _logs) = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().join("live"),
        ..Default::default()
    })
    .hook(Arc::new(ObserveUpstreamHook {
        responses: observed_live_responses.clone(),
    }))
    .build()
    .await
    .expect("live gateway init");
    configure_route_with_vendor(
        &live_gateway,
        "normalizing-live-stream",
        &[live_stream_url],
        "normalizing-test",
    )
    .await;

    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _) = ExposeOrderedToolHook::counting();
    let (buffered_gateway, _logs) = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().join("buffered"),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .hook(Arc::new(RewriteUpstreamHook))
    .platform_tool(Arc::new(OrderedTool {
        calls: tool_calls.clone(),
    }))
    .build()
    .await
    .expect("buffered gateway init");
    configure_route_with_vendor(
        &buffered_gateway,
        "normalizing-buffered-stream",
        &[buffered_stream_url],
        "normalizing-test",
    )
    .await;

    let unary = execute_non_stream(gateway.clone(), "normalizing-unary").await;
    let forced_stream = execute_protocol_request(
        gateway,
        "normalizing-forced-stream",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        false,
    )
    .await;
    let live_stream = execute_stream(live_gateway, "normalizing-live-stream").await;
    let buffered_stream = execute_stream(buffered_gateway, "normalizing-buffered-stream").await;

    let mut live_chunks = live_stream.into_body().into_data_stream();
    let live_prefix = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut prefix = String::new();
        loop {
            let chunk = live_chunks
                .next()
                .await
                .expect("live stream ended before content")
                .expect("live stream chunk");
            prefix.push_str(std::str::from_utf8(&chunk).expect("UTF-8 live stream prefix"));
            if prefix.contains("normalized:original") {
                break prefix;
            }
        }
    })
    .await
    .expect("normal live output before upstream completion");
    assert!(!live_prefix.contains("[DONE]"), "{live_prefix}");
    assert!(
        observed_live_responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "UpstreamResponse Hook ran before the upstream completed"
    );
    release_live_stream
        .send(())
        .expect("release normal live upstream");
    let live_suffix = to_bytes(axum::body::Body::from_stream(live_chunks), usize::MAX)
        .await
        .expect("remaining live-stream response body");
    let live_body = format!(
        "{live_prefix}{}",
        String::from_utf8(live_suffix.to_vec()).expect("UTF-8 live stream suffix")
    );

    let bodies = [
        (
            "unary",
            String::from_utf8(
                to_bytes(unary.into_body(), usize::MAX)
                    .await
                    .expect("unary response body")
                    .to_vec(),
            )
            .expect("UTF-8 unary response"),
            "rewritten",
            false,
        ),
        (
            "forced-stream",
            String::from_utf8(
                to_bytes(forced_stream.into_body(), usize::MAX)
                    .await
                    .expect("forced-stream response body")
                    .to_vec(),
            )
            .expect("UTF-8 forced-stream response"),
            "rewritten",
            false,
        ),
        ("live-stream", live_body, "normalized:original", true),
        (
            "buffered-platform-stream",
            String::from_utf8(
                to_bytes(buffered_stream.into_body(), usize::MAX)
                    .await
                    .expect("buffered-stream response body")
                    .to_vec(),
            )
            .expect("UTF-8 buffered-stream response"),
            "rewritten",
            true,
        ),
    ];

    for (path, body, expected_content, streams_to_client) in bodies {
        assert!(body.contains(expected_content), "{path}: {body}");
        assert!(body.contains("\"finish_reason\":\""), "{path}: {body}");
        assert!(body.contains("\"usage\""), "{path}: {body}");
        assert!(body.contains("\"id\":\""), "{path}: {body}");
        assert_eq!(
            body.matches("[DONE]").count(),
            usize::from(streams_to_client),
            "{path}: {body}"
        );
    }
    let observed_live_responses = observed_live_responses
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(observed_live_responses.len(), 1);
    assert_eq!(
        observed_live_responses[0].output_text(),
        "normalized:original"
    );
    assert_eq!(
        observed_live_responses[0].stop_reason.as_deref(),
        Some("stop")
    );
    assert!(!observed_live_responses[0].id.is_empty());
    assert_eq!(unary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(forced_stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(live_stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(buffered_stream_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );
}

#[tokio::test]
async fn reasoning_tags_are_canonicalized_across_delivery_modes() {
    let tagged = "<think>reason</think>answer";
    let (unary_url, _) = serve_openai_response(200, openai_response(tagged)).await;
    let (responses_unary_url, _) =
        serve_openai_response(200, open_responses_response(tagged)).await;
    let (live_stream_url, _) = serve_sse_sequence(vec![openai_sse(tagged)]).await;
    let unclosed = "<think>incomplete";
    let (unclosed_unary_url, _) = serve_openai_response(200, openai_response(unclosed)).await;
    let (unclosed_responses_url, _) =
        serve_openai_response(200, open_responses_response(unclosed)).await;
    let (unclosed_live_url, _) = serve_sse_sequence(vec![openai_sse(unclosed)]).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::builder(config)
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "reasoning-unary", &[unary_url]).await;
    configure_route_with_protocol(
        &gateway,
        "reasoning-responses-unary",
        &[responses_unary_url],
        "custom",
        "open-responses",
    )
    .await;
    configure_route(&gateway, "reasoning-live-stream", &[live_stream_url]).await;
    configure_route(&gateway, "unclosed-reasoning-unary", &[unclosed_unary_url]).await;
    configure_route_with_protocol(
        &gateway,
        "unclosed-reasoning-responses-unary",
        &[unclosed_responses_url],
        "custom",
        "open-responses",
    )
    .await;
    configure_route(
        &gateway,
        "unclosed-reasoning-live-stream",
        &[unclosed_live_url],
    )
    .await;

    let unary = execute_non_stream(gateway.clone(), "reasoning-unary").await;
    let responses_unary = execute_protocol_request(
        gateway.clone(),
        "reasoning-responses-unary",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        false,
    )
    .await;
    let live_stream = execute_stream(gateway.clone(), "reasoning-live-stream").await;
    for (mode, response) in [
        ("unary", unary),
        ("responses-unary", responses_unary),
        ("live-stream", live_stream),
    ] {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("reasoning response body");
        let body = String::from_utf8(body.to_vec()).expect("utf-8 reasoning response");
        assert!(body.contains("reason"), "{mode}: {body}");
        assert!(body.contains("answer"), "{mode}: {body}");
        assert!(!body.contains("<think>"), "{mode}: {body}");
    }

    let unclosed_unary = execute_non_stream(gateway.clone(), "unclosed-reasoning-unary").await;
    let unclosed_responses = execute_protocol_request(
        gateway.clone(),
        "unclosed-reasoning-responses-unary",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        false,
    )
    .await;
    let unclosed_live = execute_stream(gateway, "unclosed-reasoning-live-stream").await;
    for (mode, response) in [
        ("unclosed-unary", unclosed_unary),
        ("unclosed-responses-unary", unclosed_responses),
        ("unclosed-live-stream", unclosed_live),
    ] {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("unclosed reasoning response body");
        let body = String::from_utf8(body.to_vec()).expect("utf-8 unclosed reasoning response");
        assert!(body.contains("<think>incomplete"), "{mode}: {body}");
    }
}

#[tokio::test]
async fn stream_and_non_stream_share_terminal_hook_semantics() {
    let base_url = serve_sse_response().await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-stream-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::builder(config)
        .hook(Arc::new(RewriteUpstreamHook))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "stream-route", &[base_url]).await;
    let response = execute_stream(gateway, "stream-route").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream response body");
    let body = String::from_utf8(body.to_vec()).expect("utf-8 SSE");
    assert!(body.contains("rewritten"), "{body}");
    assert!(!body.contains("original"), "{body}");
    assert_eq!(body.matches("[DONE]").count(), 1, "{body}");
}

#[tokio::test]
async fn stream_logs_capture_request_and_response_payloads() {
    let base_url = serve_sse_response().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, mut logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route(&gateway, "logged-stream", &[base_url]).await;

    let response = execute_stream(gateway, "logged-stream").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream response body");
    assert!(String::from_utf8_lossy(&body).contains("original"));
    let entry = tokio::time::timeout(std::time::Duration::from_secs(1), logs.recv())
        .await
        .expect("stream completion log")
        .expect("log channel remains open");
    assert!(
        entry
            .client_request_body
            .as_deref()
            .is_some_and(|body| body.contains(r#""stream":true"#))
    );
    assert!(entry.client_response_body.is_some());
    assert!(entry.upstream_request_body.is_some());
    assert!(entry.upstream_response_body.is_some());
    assert!(entry.stream_chunks_count > 0);
    assert!(entry.stream_first_chunk_ms.is_some());
}

#[tokio::test]
async fn hidden_stream_rounds_close_each_provider_leg_once() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse_platform_tool_call(),
        openai_sse("final stream response"),
    ])
    .await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-hidden-stream-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let begins = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (gateway, _logs) = Gateway::builder(config)
        .hook(Arc::new(CountingStreamToolHook {
            begins: begins.clone(),
            closes: closes.clone(),
            expose_tool: true,
        }))
        .platform_tool(Arc::new(OrderedTool {
            calls: tool_calls.clone(),
        }))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "hidden-stream-route", &[base_url]).await;

    let response = execute_stream(gateway, "hidden-stream-route").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("hidden stream response body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("final stream response"), "{body}");
    assert!(!body.contains("platform-call"), "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(begins.load(Ordering::SeqCst), 2);
    assert_eq!(closes.load(Ordering::SeqCst), 2);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );
}

#[tokio::test]
async fn run_deadline_remains_authoritative_after_stream_preflight() {
    let (base_url, provider_calls) = serve_stalling_sse().await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-stream-deadline-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
    configure_route(&gateway, "stream-deadline-route", &[base_url]).await;
    let response = execute_stream_with_timeout(
        gateway,
        "stream-deadline-route",
        std::time::Duration::from_millis(500),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("run deadline must close the delivered stream")
    .expect("deadline stream body");
    assert!(
        String::from_utf8_lossy(&body).contains("before deadline"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_unpolled_live_body_closes_provider_leg() {
    let (base_url, provider_calls) = serve_stalling_sse().await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-unpolled-body-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let begins = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let (gateway, _logs) = Gateway::builder(config)
        .hook(Arc::new(CountingStreamToolHook {
            begins: begins.clone(),
            closes: closes.clone(),
            expose_tool: false,
        }))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "unpolled-body-route", &[base_url]).await;

    let response = execute_stream(gateway, "unpolled-body-route").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(begins.load(Ordering::SeqCst), 1);
    drop(response);

    tokio::time::timeout(std::time::Duration::from_millis(1500), async {
        while closes.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping an unpolled body must stop the producer");
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_deadline_cancels_forced_stream_collection() {
    let first_event = format!(
        "event: response.output_text.delta\ndata: {}\n\n",
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg-1",
            "output_index": 0,
            "content_index": 0,
            "delta": "before deadline"
        })
    );
    let (base_url, provider_calls) = serve_stalling_sse_with_event(first_event).await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-forced-stream-deadline-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
    configure_route_with_protocol(
        &gateway,
        "forced-stream-deadline-route",
        &[base_url],
        "custom",
        "open-responses",
    )
    .await;

    let response = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        execute_protocol_request_with_timeout(
            gateway,
            "forced-stream-deadline-route",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
            false,
            std::time::Duration::from_millis(500),
        ),
    )
    .await
    .expect("run deadline must interrupt forced-stream collection");

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn post_commit_hook_failures_end_the_stream_without_retry_or_response_chain() {
    for failure in [
        PostCommitHookFailure::Reject,
        PostCommitHookFailure::Respond,
        PostCommitHookFailure::Patch,
        PostCommitHookFailure::Error,
    ] {
        let (first_url, first_calls) =
            serve_sse_sequence(vec![openai_sse("committed output")]).await;
        let (fallback_url, fallback_calls) =
            serve_sse_sequence(vec![openai_sse("must not retry")]).await;
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let config = crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        };
        let (gateway, _logs) = Gateway::builder(config)
            .hook(Arc::new(PostCommitFailureHook(failure)))
            .build()
            .await
            .expect("gateway init");
        configure_route(&gateway, failure.id(), &[first_url.clone(), fallback_url]).await;

        let response = execute_protocol_request(
            gateway.clone(),
            failure.id(),
            OPEN_RESPONSES_2026_04_24,
            "/v1/responses",
            true,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK, "{}", failure.id());
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("post-commit stream body");
        let body = String::from_utf8(body.to_vec()).expect("UTF-8 stream body");
        assert!(
            body.contains("committed output"),
            "{}: {body}",
            failure.id()
        );
        assert!(
            !body.contains("response.completed"),
            "{}: {body}",
            failure.id()
        );
        assert!(body.contains("event: error"), "{}: {body}", failure.id());
        assert!(
            body.contains("response_stream_failed")
                && body.contains("The response stream failed.")
                && !body.contains("stream aborted"),
            "{}: {body}",
            failure.id()
        );
        assert!(
            !body.contains("must not replace committed output"),
            "{}: {body}",
            failure.id()
        );
        assert_eq!(first_calls.load(Ordering::SeqCst), 1, "{}", failure.id());
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0, "{}", failure.id());

        let response_id_start = body.find("resp_").expect("gateway response ID");
        let response_id: String = body[response_id_start..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        let mut continuation = AiRequest::new(failure.id(), Vec::new());
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
                    "model": failure.id(),
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
        assert_eq!(
            continuation_response.status(),
            StatusCode::BAD_REQUEST,
            "{}",
            failure.id()
        );
        let continuation_body = to_bytes(continuation_response.into_body(), usize::MAX)
            .await
            .expect("continuation error body");
        assert!(
            String::from_utf8_lossy(&continuation_body).contains("previous_response_not_found"),
            "{}",
            failure.id()
        );
    }
}

#[tokio::test]
async fn terminal_stream_hook_rejection_is_returned_before_http_commit() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![openai_sse("must not escape")]).await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-stream-reject-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::builder(config)
        .hook(Arc::new(RejectStreamHook))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "stream-reject-route", &[base_url]).await;

    let response = execute_stream(gateway, "stream-reject-route").await;

    assert_eq!(response.status(), StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream rejection body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("stream_blocked"), "{body}");
    assert!(!body.contains("must not escape"), "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn terminal_stream_hook_response_replaces_output_before_http_commit() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![openai_sse("must not escape")]).await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-stream-respond-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::builder(config)
        .hook(Arc::new(RespondStreamHook))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "stream-respond-route", &[base_url]).await;

    let response = execute_stream(gateway, "stream-respond-route").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream replacement body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("hook replacement"), "{body}");
    assert!(!body.contains("must not escape"), "{body}");
    assert_eq!(body.matches("[DONE]").count(), 1, "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}
