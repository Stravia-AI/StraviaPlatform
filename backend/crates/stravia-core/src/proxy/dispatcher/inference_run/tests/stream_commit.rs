use super::*;

#[derive(Clone, Copy)]
enum MidStreamFailure {
    Transport,
    Protocol,
    Quota,
}

struct MidStreamFailureExecutor {
    failure: MidStreamFailure,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::model_turn::ModelTurnExecutor for MidStreamFailureExecutor {
    async fn execute(
        &self,
        input: crate::model_turn::TurnInput,
    ) -> Result<crate::model_turn::ModelTurn, stravia_runtime_contract::model_turn::ModelTurnError>
    {
        use stravia_runtime_contract::model_turn::{CanonicalEvent, ModelTurnError};
        use stravia_runtime_contract::protocol::ir::AiStreamDelta;

        self.calls.fetch_add(1, Ordering::SeqCst);
        let failure = match self.failure {
            MidStreamFailure::Transport => ModelTurnError::new(
                "upstream_acceptance_unknown",
                "websocket reset at wss://internal.example with secret=token",
            ),
            MidStreamFailure::Protocol => ModelTurnError::new(
                "protocol_lossy_rejected",
                "private provider payload could not be normalized",
            ),
            MidStreamFailure::Quota => {
                let mut error = ModelTurnError::new(
                    "upstream_stream_error",
                    "private upstream quota diagnostic",
                );
                error.upstream_status = Some(529);
                error
            }
        };
        let request = input.request;
        let route = stravia_runtime_contract::hook::RouteContext {
            model_id: request.model.clone(),
            provider_id: "mid-stream-provider".into(),
            target_id: "mid-stream-target".into(),
            egress: Some(OPEN_RESPONSES_2026_04_24),
        };
        let mut turn = crate::model_turn::ModelTurn::in_memory(
            route,
            request,
            [
                Ok(CanonicalEvent::Delta(AiStreamDelta::ToolCallStart {
                    index: 0,
                    id: "call-partial".into(),
                    name: "write_file".into(),
                })),
                Ok(CanonicalEvent::Delta(AiStreamDelta::ToolCallDelta {
                    index: 0,
                    arguments: r#"{"path":"unfinished"#.into(),
                })),
                Err(failure),
            ],
        );
        turn.streamed = true;
        Ok(turn)
    }
}

struct CompletedThenErrorExecutor {
    streamed: bool,
}

#[async_trait::async_trait]
impl crate::model_turn::ModelTurnExecutor for CompletedThenErrorExecutor {
    async fn execute(
        &self,
        input: crate::model_turn::TurnInput,
    ) -> Result<crate::model_turn::ModelTurn, stravia_runtime_contract::model_turn::ModelTurnError>
    {
        use crate::model_turn::ModelTurn;
        use stravia_runtime_contract::model_turn::CanonicalEvent;
        use stravia_runtime_contract::model_turn::ModelTurnError;
        let mut response = AiResponse::new("completed-upstream", &input.request.model);
        response.push_output_text("completed answer");
        response.stop_reason = Some("stop".into());
        let mut turn = ModelTurn::in_memory(
            stravia_runtime_contract::hook::RouteContext {
                model_id: input.request.model.clone(),
                provider_id: "completed-provider".into(),
                target_id: "completed-target".into(),
                egress: Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            },
            input.request,
            [
                Ok(CanonicalEvent::Delta(
                    stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(
                        "completed answer".into(),
                    ),
                )),
                Ok(CanonicalEvent::Completed(Box::new(response))),
                Err(ModelTurnError::new(
                    "late_error",
                    "must not reinterpret completion",
                )),
            ],
        );
        turn.streamed = self.streamed;
        Ok(turn)
    }
}

#[tokio::test]
async fn mid_stream_failures_preserve_public_retry_classification() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let headers = authorized_headers(&gateway).await;

    for (failure, expected_code, private_diagnostic) in [
        (
            MidStreamFailure::Transport,
            "server_error",
            "websocket reset at wss://internal.example with secret=token",
        ),
        (
            MidStreamFailure::Protocol,
            "invalid_request",
            "private provider payload could not be normalized",
        ),
        (
            MidStreamFailure::Quota,
            "quota_exceeded",
            "private upstream quota diagnostic",
        ),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut request = AiRequest::new("mid-stream-failure", Vec::new());
        request.stream.enabled = true;
        let response = execute(RunInput {
            gateway: gateway.clone(),
            executor: Arc::new(MidStreamFailureExecutor {
                failure,
                calls: calls.clone(),
            }),
            headers: headers.clone(),
            envelope: RawEnvelope::new(
                Some(serde_json::json!({
                    "model": "mid-stream-failure",
                    "stream": true
                })),
                HashMap::new(),
                "POST",
                "/v1/responses",
            ),
            request,
            ingress: OPEN_RESPONSES_2026_04_24,
            context: RequestContext::new(
                OPEN_RESPONSES_2026_04_24,
                std::time::Duration::from_secs(30),
            ),
        })
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("mid-stream failure body");
        let body = String::from_utf8(body.to_vec()).expect("UTF-8 stream body");
        let events = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str::<serde_json::Value>(data).expect("SSE JSON"))
            .collect::<Vec<_>>();
        let arguments_index = events
            .iter()
            .position(|event| event["type"] == "response.function_call_arguments.delta")
            .expect("partial tool arguments are delivered before reset");
        let error_index = events
            .iter()
            .position(|event| event["type"] == "error")
            .expect("stream error event");
        let failed_index = events
            .iter()
            .position(|event| event["type"] == "response.failed")
            .expect("failed terminal event");
        assert!(arguments_index < error_index && error_index < failed_index);
        let error = &events[error_index];
        assert_eq!(error["error"]["type"], expected_code);
        assert_eq!(error["error"]["code"], expected_code);
        assert!(
            error.get("code").is_none(),
            "error payload must remain nested"
        );
        assert_eq!(events[failed_index]["response"]["status"], "failed");
        assert_eq!(events[failed_index]["response"]["error"], error["error"]);
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "response.completed")
        );
        assert!(!body.contains(private_diagnostic));
        assert!(body.trim_end().ends_with("data: [DONE]"));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "must not replay upstream");
    }
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn delivery_stops_reading_at_completed_for_buffered_and_live_turns() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let headers = authorized_headers(&gateway).await;
    for (streamed, live) in [(false, false), (true, false), (true, true)] {
        let mut request = AiRequest::new("completed-route", Vec::new());
        request.stream.enabled = live;
        let response = execute(RunInput {
            gateway: gateway.clone(),
            executor: Arc::new(CompletedThenErrorExecutor { streamed }),
            headers: headers.clone(),
            envelope: RawEnvelope::new(
                Some(serde_json::json!({"model": "completed-route", "stream": live})),
                HashMap::new(),
                "POST",
                "/v1/chat/completions",
            ),
            request,
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
            .expect("complete body");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("completed answer"), "{body}");
        assert!(
            !body.contains("late_error") && !body.contains("stream_mid_error"),
            "{body}"
        );
        if live {
            assert!(body.contains("[DONE]"), "{body}");
        }
    }
    close_test_gateway(gateway, data_dir).await;
}

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
        principal: &stravia_runtime_contract::Principal,
        input: crate::history_marker::PlatformMarkerInput,
    ) -> Result<crate::history_marker::HistoryMarker, crate::history_marker::HistoryMarkerError>
    {
        self.inner.create_platform(principal, input).await
    }

    async fn create_thinking(
        &self,
        principal: &stravia_runtime_contract::Principal,
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
        principal: &stravia_runtime_contract::Principal,
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
        principal: &stravia_runtime_contract::Principal,
        reference: &str,
    ) -> Result<
        Option<crate::history_marker::ResolvedHistoryMarker>,
        crate::history_marker::HistoryMarkerError,
    > {
        self.inner.resolve(principal, reference).await
    }

    async fn claim_execution(
        &self,
        principal: &stravia_runtime_contract::Principal,
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
        principal: &stravia_runtime_contract::Principal,
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
        principal: &stravia_runtime_contract::Principal,
        reference: &str,
    ) -> Result<
        Option<crate::history_marker::ResolvedHistoryMarker>,
        crate::history_marker::HistoryMarkerError,
    > {
        self.inner.wait_terminal(principal, reference).await
    }

    async fn publish(
        &self,
        principal: &stravia_runtime_contract::Principal,
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
        principal: &stravia_runtime_contract::Principal,
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
    let mut gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway init");
    configure_route_with_protocol(
        &gateway,
        "failing-live-protected-reasoning",
        &[upstream_url],
        "custom",
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
        assert!(body.contains(r#""finish_reason":"failed""#), "{body}");
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
    drop(marker_store);
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn open_responses_public_summaries_stream_before_late_encrypted_content() {
    let (summary, completion) = openai_responses_late_encrypted_summary_sse_parts();
    let (upstream_url, _calls, release_upstream) = serve_gated_sse(summary, completion).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway init");
    configure_route_with_protocol(
        &gateway,
        "late-encrypted-summary",
        &[upstream_url],
        "custom",
        "open-responses",
    )
    .await;

    let request = stravia_protocol_codec::transform::ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("Open Responses pair")
        .decode_request(serde_json::json!({
            "model": "late-encrypted-summary",
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "reasoning": {"effort": "high", "summary": "auto"},
            "input": "test"
        }))
        .expect("Open Responses request");
    let headers = authorized_headers(&gateway).await;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        execute_request_with_headers(
            gateway.clone(),
            headers,
            request,
            OPEN_RESPONSES_2026_04_24,
            "/v1/responses",
        ),
    )
    .await
    .expect("HTTP headers start when public summary deltas arrive");
    assert_eq!(response.status(), StatusCode::OK);

    let mut chunks = response.into_body().into_data_stream();
    let prefix = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut prefix = String::new();
        while !prefix.contains("response.reasoning_summary_text.delta")
            || !prefix.contains("live protected")
        {
            let chunk = chunks
                .next()
                .await
                .expect("stream ended before public summary")
                .expect("stream chunk");
            prefix.push_str(std::str::from_utf8(&chunk).expect("UTF-8 stream chunk"));
        }
        prefix
    })
    .await
    .expect("public summary should stream before encrypted item.done");
    assert!(
        prefix.contains(r#""delta":"live protected "#) || prefix.contains("live protected"),
        "{prefix}"
    );
    assert!(
        !prefix.contains("opaque-reasoning"),
        "encrypted payload must not leak into live summary deltas: {prefix}"
    );
    assert!(
        !prefix.contains("response.output_item.done"),
        "summary must not wait for thinking item completion: {prefix}"
    );

    release_upstream
        .send(())
        .expect("release upstream completion after live summary");
    let mut rest = prefix;
    while let Some(chunk) = chunks.next().await {
        rest.push_str(std::str::from_utf8(&chunk.expect("stream chunk")).expect("UTF-8"));
    }
    assert!(
        rest.contains("opaque-reasoning"),
        "encrypted content still arrives on item.done: {rest}"
    );
    drop(chunks);
    close_test_gateway(gateway, data_dir).await;
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
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway init");
    configure_route(&gateway, "cancel-post-text-preview", &[upstream_url]).await;

    let mut events = gateway.observation.subscribe(0);
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
    // 断流时 Run 与上游 Model Turn 独立收尾；两者结束后 Interaction 才进入终态。
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let mut run_finished = false;
        let mut model_turn_finished = false;
        while !run_finished || !model_turn_finished {
            if let crate::interaction_observation::ObservationUpdate::Event(event) = events
                .next()
                .await
                .expect("Observation stream remains open")
            {
                match event.kind.as_str() {
                    "run_finished" => run_finished = true,
                    "model_turn_finished" => model_turn_finished = true,
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("disconnected Run and Model Turn should both finish");
    let forest = gateway
        .observation
        .query_forest(Default::default())
        .await
        .expect("interrupted observation forest");
    let interaction = &forest.roots[0].interactions[0];
    assert_eq!(interaction.status, "interrupted");
    let detail = gateway
        .observation
        .get_interaction(&interaction.id, Default::default())
        .await
        .expect("interrupted interaction query")
        .expect("interrupted interaction");
    assert_eq!(detail.runs[0].status, "cancelled");
    assert_eq!(detail.runs[0].generation_node_id, None);

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
    drop(events);
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn post_text_marker_failures_abort_stream_and_skip_generation_commit() {
    let (upstream_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse_text_thinking_text(),
        openai_sse_text_thinking_text(),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let mut gateway = Gateway::new(crate::config::GatewayConfig {
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
        assert!(body.contains(r#""finish_reason":"failed""#), "{body}");
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
    assert_marker_failure_diagnostics(&gateway).await;
    drop(marker_store);
    close_test_gateway(gateway, data_dir).await;
}

async fn assert_marker_failure_diagnostics(gateway: &Gateway) {
    gateway.observation.flush().await.expect("flush failures");
    let failures = gateway
        .observation
        .failed_requests(Default::default())
        .await
        .expect("failed requests");
    assert_eq!(failures.items.len(), 2);
    for cause in [
        "injected Thinking persistence failure",
        "injected Thinking publish failure",
    ] {
        assert!(
            failures.items.iter().any(|failure| {
                failure.error.source.as_deref() == Some("platform")
                    && failure
                        .error
                        .message
                        .as_deref()
                        .is_some_and(|message| message.contains(cause))
            }),
            "missing hook cause {cause}: {:?}",
            failures.items
        );
    }
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
    let mut gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
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
    assert_marker_failure_diagnostics(&gateway).await;
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);
    drop(marker_store);
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn protocol_delivery_contract_matrix_covers_unary_and_sse_lifecycles() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
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
    close_test_gateway(gateway, data_dir).await;
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

    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().join("collected"),
        ..Default::default()
    })
    .hook(Arc::new(RewriteUpstreamHook))
    .build()
    .await
    .expect("collected gateway init");
    configure_route(&gateway, "normalizing-unary", &[unary_url]).await;
    configure_route_with_protocol(
        &gateway,
        "normalizing-forced-stream",
        &[forced_stream_url],
        "custom",
        "open-responses",
    )
    .await;

    let observed_live_responses = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let live_gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().join("live"),
        ..Default::default()
    })
    .hook(Arc::new(ObserveUpstreamHook {
        responses: observed_live_responses.clone(),
    }))
    .build()
    .await
    .expect("live gateway init");
    configure_route(&live_gateway, "normalizing-live-stream", &[live_stream_url]).await;

    let tool_calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (expose_tool_hook, _) = ExposeOrderedToolHook::counting();
    let buffered_gateway = Gateway::builder(crate::config::GatewayConfig {
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
    configure_route(
        &buffered_gateway,
        "normalizing-buffered-stream",
        &[buffered_stream_url],
    )
    .await;

    let unary = execute_non_stream(gateway.clone(), "normalizing-unary").await;
    let forced_stream = execute_protocol_request(
        gateway.clone(),
        "normalizing-forced-stream",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        false,
    )
    .await;
    let live_stream = execute_stream(live_gateway.clone(), "normalizing-live-stream").await;

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
            if prefix.contains("original") {
                break prefix;
            }
        }
    })
    .await
    .expect("normal live output before upstream completion");
    assert!(!live_prefix.contains("[DONE]"), "{live_prefix}");
    assert!(
        observed_live_responses.lock().is_empty(),
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
    let buffered_stream =
        execute_stream(buffered_gateway.clone(), "normalizing-buffered-stream").await;

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
        ("live-stream", live_body, "original", true),
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
    let observed_live_responses = observed_live_responses.lock();
    assert_eq!(observed_live_responses.len(), 1);
    assert_eq!(observed_live_responses[0].output_text(), "original");
    assert_eq!(
        observed_live_responses[0].stop_reason.as_deref(),
        Some("stop")
    );
    assert!(!observed_live_responses[0].id.is_empty());
    assert_eq!(unary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(forced_stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(live_stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(buffered_stream_calls.load(Ordering::SeqCst), 2);
    assert_eq!(*tool_calls.lock(), vec![1]);
    drop(observed_live_responses);
    shutdown_test_gateway(gateway).await;
    shutdown_test_gateway(live_gateway).await;
    shutdown_test_gateway(buffered_gateway).await;
    data_dir
        .close()
        .expect("remove temporary gateway directory");
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
    let gateway = Gateway::builder(config)
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
    let unclosed_live = execute_stream(gateway.clone(), "unclosed-reasoning-live-stream").await;
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
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn stream_and_non_stream_share_terminal_hook_semantics() {
    let base_url = serve_sse_response().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::builder(config)
        .hook(Arc::new(RewriteUpstreamHook))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "stream-route", &[base_url]).await;
    let response = execute_stream(gateway.clone(), "stream-route").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream response body");
    let body = String::from_utf8(body.to_vec()).expect("utf-8 SSE");
    assert!(body.contains("rewritten"), "{body}");
    assert!(!body.contains("original"), "{body}");
    assert_eq!(body.matches("[DONE]").count(), 1, "{body}");
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn hidden_stream_rounds_close_each_provider_leg_once() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse_platform_tool_call(),
        openai_sse("final stream response"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let begins = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let gateway = Gateway::builder(config)
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

    let response = execute_stream(gateway.clone(), "hidden-stream-route").await;

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
    assert_eq!(*tool_calls.lock(), vec![1]);
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn run_deadline_remains_authoritative_after_stream_preflight() {
    let (base_url, provider_calls) = serve_stalling_sse().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
    configure_route(&gateway, "stream-deadline-route", &[base_url]).await;
    let response = execute_stream_with_timeout(
        gateway.clone(),
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
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn dropping_unpolled_live_body_closes_provider_leg() {
    let (base_url, provider_calls) = serve_stalling_sse().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let begins = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let gateway = Gateway::builder(config)
        .hook(Arc::new(CountingStreamToolHook {
            begins: begins.clone(),
            closes: closes.clone(),
            expose_tool: false,
        }))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "unpolled-body-route", &[base_url]).await;

    let response = execute_stream(gateway.clone(), "unpolled-body-route").await;
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
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn run_deadline_cancels_forced_stream_collection() {
    let complete_stream = openai_responses_sse("before deadline");
    let terminal_offset = complete_stream
        .find("event: response.content_part.done")
        .expect("Open Responses fixture has post-delta events");
    let first_event = complete_stream[..terminal_offset].to_owned();
    let (base_url, provider_calls) = serve_stalling_sse_with_event(first_event).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
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
            gateway.clone(),
            "forced-stream-deadline-route",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
            false,
            std::time::Duration::from_millis(500),
        ),
    )
    .await
    .expect("run deadline must interrupt forced-stream collection");

    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("deadline response body");
    assert_eq!(
        status,
        StatusCode::GATEWAY_TIMEOUT,
        "provider calls: {}; response: {}",
        provider_calls.load(Ordering::SeqCst),
        String::from_utf8_lossy(&body)
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    close_test_gateway(gateway, data_dir).await;
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
        let gateway = Gateway::builder(config)
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
        let stream_error = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .find(|event| event["type"] == "error")
            .expect("post-commit stream error");
        assert_eq!(stream_error["error"]["code"], "unknown");
        assert!(stream_error.get("code").is_none());
        assert!(
            !body.contains("must not replace committed output")
                && !body.contains("late Hook error"),
            "{}: {body}",
            failure.id()
        );
        assert_eq!(first_calls.load(Ordering::SeqCst), 1, "{}", failure.id());
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0, "{}", failure.id());

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
        let mut continuation = AiRequest::new(failure.id(), Vec::new());
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
        close_test_gateway(gateway, data_dir).await;
    }
}

#[tokio::test]
async fn terminal_stream_hook_rejection_is_returned_before_http_commit() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![openai_sse("must not escape")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::builder(config)
        .hook(Arc::new(RejectStreamHook))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "stream-reject-route", &[base_url]).await;

    let response = execute_stream(gateway.clone(), "stream-reject-route").await;

    assert_eq!(response.status(), StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream rejection body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("stream_blocked"), "{body}");
    assert!(!body.contains("must not escape"), "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn terminal_stream_hook_response_replaces_output_before_http_commit() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![openai_sse("must not escape")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::builder(config)
        .hook(Arc::new(RespondStreamHook))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "stream-respond-route", &[base_url]).await;

    let response = execute_stream(gateway.clone(), "stream-respond-route").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream replacement body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("hook replacement"), "{body}");
    assert!(!body.contains("must not escape"), "{body}");
    assert_eq!(body.matches("[DONE]").count(), 1, "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    close_test_gateway(gateway, data_dir).await;
}
