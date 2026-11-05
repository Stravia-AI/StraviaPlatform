use super::*;

struct ChangingTargetExecutor(AtomicUsize);

#[async_trait]
impl crate::model_turn::ModelTurnExecutor for ChangingTargetExecutor {
    async fn execute(
        &self,
        input: crate::model_turn::TurnInput,
    ) -> Result<crate::model_turn::ModelTurn, stravia_runtime_contract::model_turn::ModelTurnError>
    {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        let target = if first {
            "first-target"
        } else {
            "final-target"
        };
        let mut response = AiResponse::new(target, "upstream-model");
        if first {
            response.extend_tool_calls(vec![stravia_runtime_contract::protocol::ir::ToolCall {
                id: "platform-call".into(),
                name: "stravia__ordered_tool".into(),
                arguments: r#"{"index":1}"#.into(),
            }]);
            response.stop_reason = Some("tool_calls".into());
        } else {
            response.push_output_text("final Target answer");
            response.stop_reason = Some("stop".into());
        }
        Ok(crate::model_turn::ModelTurn::in_memory(
            stravia_runtime_contract::hook::RouteContext {
                model_id: input.request.model.clone(),
                provider_id: target.into(),
                target_id: target.into(),
                egress: Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            },
            Arc::unwrap_or_clone(input.request),
            [Ok(
                stravia_runtime_contract::model_turn::CanonicalEvent::Completed(Box::new(response)),
            )],
        ))
    }
}

#[tokio::test]
async fn delivered_hidden_round_continuation_prefers_the_final_model_legs_target() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (hook, _) = ExposeOrderedToolHook::counting();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
    }))
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
    let mut request = AiRequest::new("changing-target-route", Vec::new());
    request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: Arc::new(ChangingTargetExecutor(AtomicUsize::new(0))),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({"model": "changing-target-route"})),
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
        .expect("full delivery");
    let response: serde_json::Value = serde_json::from_slice(&body).expect("response");
    assert!(String::from_utf8_lossy(&body).contains("final Target answer"));
    let mut continuation = AiRequest::new("changing-target-route", Vec::new());
    continuation.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    continuation.parameters.continuation = Some(
        stravia_runtime_contract::protocol::ir::Continuation::PreviousResponse(
            response["id"].as_str().expect("response identity").into(),
        ),
    );
    assert_eq!(
        gateway
            .generation_chains
            .continuation_lookup()
            .preferred_target(&principal, &continuation)
            .await
            .as_deref(),
        Some("final-target"),
    );
}

#[tokio::test]
async fn responses_terminal_body_drop_preserves_observed_generation_chain() {
    let (base_url, _, _) =
        serve_responses_websocket_sequence(vec!["first answer", "second answer", "third answer"])
            .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "terminal-body-drop";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "openai-compatible")
        .await;
    let headers = authorized_headers(&gateway).await;
    let authorization = headers.get(header::AUTHORIZATION).expect("authorization");
    let router = crate::proxy::server::create_router(gateway.clone());
    let mut events = gateway.observation.subscribe(0);
    let mut input = Vec::new();
    let mut previous_response = None;

    for prompt in ["first", "second", "third"] {
        input.push(serde_json::json!({"role": "user", "content": prompt}));
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .header(header::AUTHORIZATION, authorization.clone())
                    .body(Body::from(
                        serde_json::json!({
                            "model": model,
                            "stream": true,
                            "input": input,
                        })
                        .to_string(),
                    ))
                    .expect("Responses request"),
            )
            .await
            .expect("Responses response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut chunks = response.into_body().into_data_stream();
        let mut wire = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !wire.contains("data: [DONE]\n\n") {
                let chunk = chunks.next().await.expect("terminal frame").expect("chunk");
                wire.push_str(std::str::from_utf8(&chunk).expect("SSE UTF-8"));
            }
        })
        .await
        .expect("response terminal arrives");
        // 客户端收到协议终态即可结束读取，不保证再 poll 一次 HTTP EOF。
        drop(chunks);
        let completed = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .find(|event| event["type"] == "response.completed")
            .expect("completed response");
        let response_id = completed["response"]["id"].as_str().expect("response ID");
        // Query actual durable completion rather than assume a notification
        // itself proves it; retain metadata-only evidence if the bound expires.
        let mut observed_changes = Vec::new();
        let mut observed_interactions = std::collections::BTreeSet::new();
        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let event = events
                    .next()
                    .await
                    .expect("Observation stream remains open");
                let crate::interaction_observation::ObservationUpdate::Change(event) = event else {
                    continue;
                };
                observed_changes.push((event.kind.clone(), event.run_id.clone()));
                let Some(interaction_id) = event.interaction_id.as_deref() else {
                    continue;
                };
                observed_interactions.insert(interaction_id.to_owned());
                let detail = gateway
                    .observation
                    .get_interaction(interaction_id, Default::default())
                    .await
                    .expect("changed interaction query")
                    .expect("changed interaction");
                if detail.runs.iter().any(|run| {
                    run.generation_node_id.as_deref() == Some(response_id)
                        && run.status == "completed"
                        && run.finished_at.is_some()
                }) {
                    break;
                }
            }
        })
        .await;
        if observed.is_err() {
            let mut persisted_runs = Vec::new();
            let mut gap_categories = Vec::new();
            for interaction_id in observed_interactions {
                if let Ok(Some(detail)) = gateway
                    .observation
                    .get_interaction(&interaction_id, Default::default())
                    .await
                {
                    for run in detail.runs {
                        for event in &run.events {
                            if event.kind == "observation_gap" {
                                let category = match event
                                    .payload
                                    .get("reason")
                                    .and_then(serde_json::Value::as_str)
                                    .and_then(|reason| reason.split(':').next())
                                {
                                    Some("settlement_vendor_publication") => {
                                        "settlement_vendor_publication"
                                    }
                                    Some("settlement_generation_commit") => {
                                        "settlement_generation_commit"
                                    }
                                    Some("settlement_marker_publish") => {
                                        "settlement_marker_publish"
                                    }
                                    Some("settlement_executions_without_run") => {
                                        "settlement_executions_without_run"
                                    }
                                    Some("client_history_projection_unavailable") => {
                                        "client_history_projection_unavailable"
                                    }
                                    Some("generation_parent_observation_unavailable") => {
                                        "generation_parent_observation_unavailable"
                                    }
                                    _ => "other_observation_gap",
                                };
                                gap_categories.push((run.id.clone(), category));
                            }
                        }
                        persisted_runs.push((
                            run.id,
                            run.status,
                            run.generation_node_id,
                            run.finished_at,
                        ));
                    }
                }
            }
            panic!(
                "delivered response generation not observed within 1s: response_id={response_id}, changes={observed_changes:?}, persisted_runs={persisted_runs:?}, gap_categories={gap_categories:?}"
            );
        }
        let forest = gateway
            .observation
            .query_forest(Default::default())
            .await
            .expect("observation forest");
        let mut found = None;
        for interaction in forest.roots.iter().flat_map(|root| &root.interactions) {
            let detail = gateway
                .observation
                .get_interaction(&interaction.id, Default::default())
                .await
                .expect("interaction query")
                .expect("interaction");
            if detail.output_text.ends_with(&format!("{prompt} answer")) {
                found = Some(interaction);
                break;
            }
        }
        let interaction = found.expect("terminal response remains visible in its interaction");
        assert_eq!(interaction.status, "completed");
        let detail = gateway
            .observation
            .get_interaction(&interaction.id, Default::default())
            .await
            .expect("interaction query")
            .expect("interaction");
        let run = detail
            .runs
            .iter()
            .find(|run| run.generation_node_id.as_deref() == Some(response_id))
            .expect("delivered generation remains observed");
        assert_eq!(run.status, "completed");
        assert_eq!(run.terminal_reason, None);
        assert!(!run.user_interrupted);
        assert_eq!(run.generation_parent_id, previous_response);
        previous_response = Some(response_id.to_owned());
        input.extend(
            completed["response"]["output"]
                .as_array()
                .expect("response output")
                .iter()
                .cloned(),
        );
    }
}

#[tokio::test]
async fn rewritten_tool_history_keeps_observation_on_the_current_run() {
    let mut responses = Vec::new();
    for step in 0..3 {
        let mut response = openai_response("");
        response["choices"][0]["message"]["tool_calls"] = serde_json::json!([{
            "id": format!("call-{step}"),
            "type": "function",
            "function": {"name": "inspect", "arguments": "{}"}
        }]);
        response["choices"][0]["finish_reason"] = serde_json::json!("tool_calls");
        responses.push(response);
    }
    responses.push(openai_response("task completed"));
    let (base_url, _, upstream_requests) = serve_openai_sequence_with_requests(responses).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "rewritten-tool-history";
    configure_route(&gateway, model, &[base_url]).await;
    let headers = authorized_headers(&gateway).await;
    let authorization = headers.get(header::AUTHORIZATION).expect("authorization");
    let router = crate::proxy::server::create_router(gateway.clone());
    let mut events = gateway.observation.subscribe(0);
    let mut input = vec![serde_json::json!({"role": "user", "content": "inspect the task"})];
    let mut first_result: Option<usize> = None;
    let mut first_response = None;
    let mut interaction_id = None;
    let mut previous_run = None;

    for step in 0..4 {
        if step >= 2 {
            // 改写旧工具结果，但保留最新调用与回传；严格前缀退回首个响应。
            input[first_result.expect("first tool result")]["output"] =
                serde_json::json!(format!("trimmed-history-{step}"));
        }
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .header(header::AUTHORIZATION, authorization.clone())
                    .body(Body::from(
                        serde_json::json!({
                            "model": model,
                            "input": input,
                            "tools": [{
                                "type": "function",
                                "name": "inspect",
                                "parameters": {"type": "object", "properties": {}}
                            }]
                        })
                        .to_string(),
                    ))
                    .expect("Responses request"),
            )
            .await
            .expect("Responses response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let response: serde_json::Value = serde_json::from_slice(&body).expect("response JSON");
        let response_id = response["id"].as_str().expect("response ID");
        wait_for_observed_run_finish(&mut events).await;
        let forest = gateway
            .observation
            .query_forest(Default::default())
            .await
            .expect("observation forest");
        let interactions: Vec<_> = forest
            .roots
            .iter()
            .flat_map(|root| &root.interactions)
            .collect();
        assert_eq!(
            interactions.len(),
            1,
            "tool continuation must not split the task"
        );
        let interaction = interactions[0];
        if let Some(expected) = &interaction_id {
            assert_eq!(&interaction.id, expected);
        } else {
            interaction_id = Some(interaction.id.clone());
        }
        let detail = gateway
            .observation
            .get_interaction(&interaction.id, Default::default())
            .await
            .expect("interaction query")
            .expect("interaction");
        let run = detail
            .runs
            .iter()
            .find(|run| run.generation_node_id.as_deref() == Some(response_id))
            .expect("delivered run");
        assert_eq!(run.parent_run_id, previous_run);
        assert_eq!(run.generation_parent_id, first_response);
        assert!(detail.runs.iter().all(|run| !run.user_interrupted));
        previous_run = Some(run.id.clone());
        if first_response.is_none() {
            first_response = Some(response_id.to_owned());
        }
        if step == 3 {
            assert_eq!(interaction.status, "completed");
            assert_eq!(detail.runs.len(), 4);
        } else {
            let output = response["output"].as_array().expect("response output");
            let call_id = output
                .iter()
                .find(|item| item["type"] == "function_call")
                .and_then(|item| item["call_id"].as_str())
                .expect("client tool call ID");
            input.extend(output.iter().cloned());
            if first_result.is_none() {
                first_result = Some(input.len());
            }
            input.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": format!("initial-tool-result-{step}")
            }));
            if step == 0 {
                input.push(serde_json::json!({"role": "user", "content": "keep inspecting"}));
            }
        }
    }

    {
        let requests = upstream_requests.lock();
        let last = requests.last().expect("final upstream request");
        assert!(last.contains("trimmed-history-3"));
        assert!(!last.contains("initial-tool-result-0"));
    }
    drop(router);
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn anthropic_cache_breakpoint_on_reusable_history_keeps_target_continuation() {
    let (base_url, connections, requests) =
        serve_responses_websocket_sequence(vec!["first answer", "second answer"]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route_with_protocol(
        &gateway,
        "cache-breakpoint-continuation",
        &[base_url],
        "openai",
        "openai-compatible",
    )
    .await;
    let headers = authorized_headers(&gateway).await;
    let authorization = headers
        .get(header::AUTHORIZATION)
        .expect("authorization header")
        .clone();
    let mut events = gateway.observation.subscribe(0);
    let router = crate::proxy::server::create_router(gateway);

    let first_response = router
        .clone()
        .oneshot(
            Request::post("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .header(header::AUTHORIZATION, authorization.clone())
                .body(Body::from(
                    serde_json::json!({
                        "model": "cache-breakpoint-continuation",
                        "max_tokens": 128,
                        "messages": [{
                            "role": "user",
                            "content": [{"type": "text", "text": "first"}]
                        }]
                    })
                    .to_string(),
                ))
                .expect("first Anthropic request"),
        )
        .await
        .expect("first Anthropic response");
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = to_bytes(first_response.into_body(), usize::MAX)
        .await
        .expect("first Anthropic response body");
    assert!(
        String::from_utf8_lossy(&first_body).contains("first answer"),
        "{}",
        String::from_utf8_lossy(&first_body)
    );
    wait_for_observed_run_finish(&mut events).await;

    let second_response = router
        .oneshot(
            Request::post("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .header(header::AUTHORIZATION, authorization)
                .body(Body::from(
                    serde_json::json!({
                        "model": "cache-breakpoint-continuation",
                        "max_tokens": 128,
                        "messages": [
                            {
                                "role": "user",
                                "content": [{
                                    "type": "text",
                                    "text": "first",
                                    "cache_control": {"type": "ephemeral"}
                                }]
                            },
                            {
                                "role": "assistant",
                                "content": [{"type": "text", "text": "first answer"}]
                            },
                            {
                                "role": "user",
                                "content": [{"type": "text", "text": "second"}]
                            }
                        ]
                    })
                    .to_string(),
                ))
                .expect("second Anthropic request"),
        )
        .await
        .expect("second Anthropic response");
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("second Anthropic response body");
    assert!(
        String::from_utf8_lossy(&second_body).contains("second answer"),
        "{}",
        String::from_utf8_lossy(&second_body)
    );

    let requests = requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]["previous_response_id"],
        serde_json::json!("resp-provider")
    );
    assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(1));
    assert!(requests[1]["input"][0].to_string().contains("second"));
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn anthropic_combined_assistant_turn_reuses_generation_chain() {
    let (base_url, _connections, requests) = serve_responses_websocket_streams(vec![
        openai_responses_tool_sse("planning", "call-1"),
        openai_responses_sse("done"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route_with_protocol(
        &gateway,
        "combined-assistant-continuation",
        &[base_url],
        "openai",
        "openai-compatible",
    )
    .await;
    let headers = authorized_headers(&gateway).await;
    let authorization = headers
        .get(header::AUTHORIZATION)
        .expect("authorization header")
        .clone();
    let mut events = gateway.observation.subscribe(0);
    let router = crate::proxy::server::create_router(gateway);
    let tools = serde_json::json!([{
        "name": "client_tool",
        "description": "Client-owned tool",
        "input_schema": {
            "type": "object",
            "properties": {"value": {"type": "integer"}},
            "required": ["value"]
        }
    }]);

    let first_response = router
        .clone()
        .oneshot(
            Request::post("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .header(header::AUTHORIZATION, authorization.clone())
                .body(Body::from(
                    serde_json::json!({
                        "model": "combined-assistant-continuation",
                        "max_tokens": 128,
                        "tools": tools.clone(),
                        "messages": [{"role": "user", "content": "first"}]
                    })
                    .to_string(),
                ))
                .expect("first Anthropic request"),
        )
        .await
        .expect("first Anthropic response");
    let first_status = first_response.status();
    let first_body = to_bytes(first_response.into_body(), usize::MAX)
        .await
        .expect("first Anthropic response body");
    assert_eq!(
        first_status,
        StatusCode::OK,
        "{}; upstream requests: {}",
        String::from_utf8_lossy(&first_body),
        requests.lock().len()
    );
    assert!(
        String::from_utf8_lossy(&first_body).contains("call-1"),
        "{}",
        String::from_utf8_lossy(&first_body)
    );
    wait_for_observed_run_finish(&mut events).await;

    let second_response = router
        .oneshot(
            Request::post("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .header(header::AUTHORIZATION, authorization)
                .body(Body::from(
                    serde_json::json!({
                        "model": "combined-assistant-continuation",
                        "max_tokens": 128,
                        "tools": tools,
                        "messages": [
                            {"role": "user", "content": "first"},
                            {
                                "role": "assistant",
                                "content": [
                                    {"type": "text", "text": "planning"},
                                    {
                                        "type": "tool_use",
                                        "id": "call-1",
                                        "name": "client_tool",
                                        "input": {"value": 1}
                                    }
                                ]
                            },
                            {
                                "role": "user",
                                "content": [{
                                    "type": "tool_result",
                                    "tool_use_id": "call-1",
                                    "content": "tool result"
                                }]
                            }
                        ]
                    })
                    .to_string(),
                ))
                .expect("second Anthropic request"),
        )
        .await
        .expect("second Anthropic response");
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("second Anthropic response body");
    assert!(
        String::from_utf8_lossy(&second_body).contains("done"),
        "{}",
        String::from_utf8_lossy(&second_body)
    );

    let requests = requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]["previous_response_id"],
        serde_json::json!("resp-provider")
    );
    assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        requests[1]["input"][0]["call_id"],
        serde_json::json!("call-1")
    );
}

#[test]
fn buffered_platform_only_executes_hidden_round_before_returning() {
    std::thread::Builder::new()
        .name("buffered-platform-only-rejection".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(buffered_platform_only_executes_hidden_round_impl())
        })
        .expect("test thread")
        .join()
        .expect("test thread panicked");
}

#[test]
fn hidden_round_request_hook_response_continues_the_committed_stream() {
    std::thread::Builder::new()
        .name("hidden-round-hook-response".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(hidden_round_request_hook_response_is_delivered_impl())
        })
        .expect("test thread")
        .join()
        .expect("test thread panicked");
}

#[test]
fn hidden_round_request_hook_rejection_terminates_the_committed_stream() {
    std::thread::Builder::new()
        .name("hidden-round-hook-rejection".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(hidden_round_request_hook_rejection_is_delivered_impl())
        })
        .expect("test thread")
        .join()
        .expect("test thread panicked");
}

#[test]
fn mixed_tool_continuation_replays_after_drop_and_is_consumed_after_delivery() {
    std::thread::Builder::new()
        .name("mixed-tool-continuation".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(mixed_tool_continuation_replays_impl())
        })
        .expect("test thread")
        .join()
        .expect("test thread panicked");
}

#[test]
fn platform_only_stream_emits_marker_and_continues_on_the_same_response() {
    std::thread::Builder::new()
        .name("platform-only-marker-stream".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(platform_only_stream_continues_with_marker_impl())
        })
        .expect("test thread")
        .join()
        .expect("test thread result");
}

#[test]
fn platform_only_stream_preserves_followup_client_tool_arguments() {
    std::thread::Builder::new()
        .name("platform-client-tool-stream".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(platform_only_stream_preserves_client_tool_arguments_impl())
        })
        .expect("test thread")
        .join()
        .expect("test thread result");
}

#[test]
fn platform_markers_are_projected_for_all_generation_ingresses() {
    std::thread::Builder::new()
        .name("platform-marker-ingresses".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(platform_markers_are_ingress_neutral_impl())
        })
        .expect("test thread")
        .join()
        .expect("test thread result");
}

#[tokio::test]
async fn chat_full_history_uses_upstream_websocket_and_longest_reusable_prefix() {
    let (base_url, connections, requests) =
        serve_responses_websocket_sequence(vec!["first answer", "second answer", "third answer"])
            .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route_with_protocol(
        &gateway,
        "responses-websocket-route",
        &[base_url],
        "openai",
        "openai-compatible",
    )
    .await;
    let headers = authorized_headers(&gateway).await;

    let mut first_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("first");
    first_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let first_response = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers.clone(),
        AiRequest::new("responses-websocket-route", vec![first_user.clone()]),
    )
    .await;
    let first_status = first_response.status();
    let first_body = to_bytes(first_response.into_body(), usize::MAX)
        .await
        .expect("first response body");
    let first_body: serde_json::Value =
        serde_json::from_slice(&first_body).expect("first response JSON");
    assert_eq!(
        first_status,
        StatusCode::OK,
        "{first_body}; connections={}; requests={:?}",
        connections.load(Ordering::SeqCst),
        requests.lock()
    );
    assert_eq!(
        first_body["choices"][0]["message"]["content"],
        serde_json::json!("first answer")
    );

    let mut second_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("second");
    second_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let second_request = AiRequest::new(
        "responses-websocket-route",
        vec![
            first_user.clone(),
            stravia_runtime_contract::protocol::ir::AiItem::output_text("first answer"),
            second_user.clone(),
        ],
    );
    let second_response =
        execute_non_stream_request_with_headers(gateway.clone(), headers.clone(), second_request)
            .await;
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("second response body");
    let second_body: serde_json::Value =
        serde_json::from_slice(&second_body).expect("second response JSON");
    assert_eq!(
        second_body["choices"][0]["message"]["content"],
        serde_json::json!("second answer")
    );

    let mut third_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("third");
    third_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let mut third_request = AiRequest::new(
        "responses-websocket-route",
        vec![
            first_user,
            stravia_runtime_contract::protocol::ir::AiItem::output_text("first answer"),
            second_user,
            stravia_runtime_contract::protocol::ir::AiItem::output_text("second answer"),
            third_user,
        ],
    );
    third_request.meta.source_protocol = Some(ANTHROPIC_MESSAGES_2023_06_01);
    let third_response = execute_request_with_headers(
        gateway,
        headers,
        third_request,
        ANTHROPIC_MESSAGES_2023_06_01,
        "/v1/messages",
    )
    .await;
    assert_eq!(third_response.status(), StatusCode::OK);

    let requests = requests.lock();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].get("previous_response_id").is_none());
    assert_eq!(requests[0]["input"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        requests[1]["previous_response_id"],
        serde_json::json!("resp-provider")
    );
    assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(1));
    assert!(requests[1]["input"][0].to_string().contains("second"));
    assert_eq!(
        requests[2]["previous_response_id"],
        serde_json::json!("resp-provider")
    );
    assert_eq!(requests[2]["input"].as_array().map(Vec::len), Some(1));
    assert!(requests[2]["input"][0].to_string().contains("third"));
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn edited_history_reaches_websocket_without_reusing_stale_upstream_state() {
    use stravia_runtime_contract::protocol::ir::{AiItem, Role, ToolCall};

    let (base_url, _, requests) = serve_responses_websocket_sequence(vec![
        "original answer",
        "tool edit accepted",
        "user edit accepted",
        "answer edit accepted",
    ])
    .await;
    let data_dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    configure_route_with_protocol(
        &gateway,
        "history-edit-route",
        &[base_url],
        "openai",
        "openai-compatible",
    )
    .await;
    let headers = authorized_headers(&gateway).await;
    let user = |text: &str| {
        let mut item = AiItem::output_text(text);
        item.role = Role::User;
        item
    };
    let history = |question: &str, result: &str| {
        vec![
            user(question),
            AiItem::function_call(ToolCall {
                id: "lookup-history".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            }),
            AiItem::function_call_output("lookup-history", serde_json::json!(result)),
        ]
    };
    let response = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers.clone(),
        AiRequest::new(
            "history-edit-route",
            history("original question", "original result"),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();

    for (question, result, answer, changed, obsolete) in [
        (
            "original question",
            "edited result",
            "original answer",
            "edited result",
            "original result",
        ),
        (
            "edited question",
            "original result",
            "original answer",
            "edited question",
            "original question",
        ),
        (
            "original question",
            "original result",
            "edited answer",
            "edited answer",
            "original answer",
        ),
    ] {
        let mut items = history(question, result);
        items.extend([AiItem::output_text(answer), user("new task")]);
        let response = execute_non_stream_request_with_headers(
            gateway.clone(),
            headers.clone(),
            AiRequest::new("history-edit-route", items),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let requests = requests.lock();
        let upstream = requests.last().unwrap();
        assert!(upstream.get("previous_response_id").is_none(), "{upstream}");
        let sent = upstream["input"].to_string();
        assert!(sent.contains(changed), "{sent}");
        assert!(!sent.contains(obsolete), "{sent}");
        assert!(sent.contains("new task"), "{sent}");
    }
}

#[tokio::test]
async fn store_false_chat_chain_generates_a_stable_prompt_cache_key() {
    let (base_url, connections, requests) =
        serve_responses_websocket_sequence(vec!["first answer", "second answer"]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route_with_protocol(
        &gateway,
        "store-false-cache-key",
        &[base_url],
        "openai",
        "openai-compatible",
    )
    .await;
    let headers = authorized_headers(&gateway).await;

    let mut first_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("first");
    first_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let mut first_request = AiRequest::new("store-false-cache-key", vec![first_user.clone()]);
    first_request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    first_request.parameters.store = Some(false);
    let first_response =
        execute_non_stream_request_with_headers(gateway.clone(), headers.clone(), first_request)
            .await;
    assert_eq!(first_response.status(), StatusCode::OK);
    to_bytes(first_response.into_body(), usize::MAX)
        .await
        .expect("first response body");

    let mut second_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("second");
    second_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let mut second_request = AiRequest::new(
        "store-false-cache-key",
        vec![
            first_user,
            stravia_runtime_contract::protocol::ir::AiItem::output_text("first answer"),
            second_user,
        ],
    );
    second_request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    second_request.parameters.store = Some(false);
    let second_response =
        execute_non_stream_request_with_headers(gateway, headers, second_request).await;
    assert_eq!(second_response.status(), StatusCode::OK);
    to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("second response body");

    let requests = requests.lock();
    assert_eq!(requests.len(), 2);
    let prompt_cache_key = requests[0]["prompt_cache_key"]
        .as_str()
        .expect("first request prompt cache key");
    assert_eq!(
        requests[1]["prompt_cache_key"].as_str(),
        Some(prompt_cache_key)
    );
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn openrouter_session_id_follows_the_local_generation_chain() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let captured = requests.clone();
    tokio::spawn(async move {
        for answer in ["first answer", "second answer", "other answer"] {
            let (mut socket, _) = listener.accept().await.expect("accept provider request");
            let request = read_test_http_request(&mut socket).await;
            captured.lock().push(request.clone());
            write_test_openai_response(&mut socket, &request, openai_response(answer)).await;
        }
    });
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route_with_protocol(
        &gateway,
        "session-affinity",
        &[format!("http://{address}/v1")],
        "openrouter",
        "openai-compatible",
    )
    .await;
    // 客户端对三次请求自报同一会话；上游键仍须按本地链路区分。
    let mut headers = authorized_headers(&gateway).await;
    headers.insert(
        header::HeaderName::from_static("session-id"),
        header::HeaderValue::from_static("client-conversation"),
    );
    let user = |text: &str| {
        let mut item = stravia_runtime_contract::protocol::ir::AiItem::output_text(text);
        item.role = stravia_runtime_contract::protocol::ir::Role::User;
        item
    };
    for items in [
        vec![user("first")],
        vec![
            user("first"),
            stravia_runtime_contract::protocol::ir::AiItem::output_text("first answer"),
            user("second"),
        ],
        vec![user("unrelated")],
    ] {
        let response = execute_non_stream_request_with_headers(
            gateway.clone(),
            headers.clone(),
            AiRequest::new("session-affinity", items),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
    }

    let session_ids = requests
        .lock()
        .iter()
        .map(|request| {
            request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("x-session-id")
                        .then(|| value.trim().to_owned())
                })
                .unwrap_or_else(|| panic!("upstream session header missing: {request}"))
        })
        .collect::<Vec<_>>();
    assert_eq!(session_ids.len(), 3);
    assert_eq!(
        session_ids[0], session_ids[1],
        "continued chain keeps its key"
    );
    assert_ne!(session_ids[0], session_ids[2], "new chain gets a new key");
    assert!(
        session_ids.iter().all(|id| id != "client-conversation"),
        "{session_ids:?}"
    );
}

#[tokio::test]
async fn missing_upstream_prefix_replays_full_history_once_on_the_same_socket() {
    let (base_url, connections, requests) = serve_missing_previous_websocket(false).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "responses-websocket-missing-prefix";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "openai-compatible")
        .await;
    set_target_retry_budget(&gateway, model, 1).await;
    let headers = authorized_headers(&gateway).await;

    let mut first_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("first");
    first_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let first = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers.clone(),
        AiRequest::new(model, vec![first_user.clone()]),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first response body");
    let first_body: serde_json::Value =
        serde_json::from_slice(&first_body).expect("first response JSON");
    let first_answer = first_body["choices"][0]["message"]["content"]
        .as_str()
        .expect("first answer")
        .to_owned();

    let mut second_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("second");
    second_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let second = execute_non_stream_request_with_headers(
        gateway,
        headers,
        AiRequest::new(
            model,
            vec![
                first_user,
                stravia_runtime_contract::protocol::ir::AiItem::output_text(first_answer),
                second_user,
            ],
        ),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = to_bytes(second.into_body(), usize::MAX)
        .await
        .expect("second response body");
    assert!(
        String::from_utf8_lossy(&second_body).contains("second answer"),
        "{}",
        String::from_utf8_lossy(&second_body)
    );

    let requests = requests.lock();
    assert_eq!(requests.len(), 3);
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(
        requests[1]["previous_response_id"],
        serde_json::json!("resp-provider")
    );
    assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(1));
    assert!(requests[2].get("previous_response_id").is_none());
    assert_eq!(requests[2]["input"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn missing_upstream_prefix_is_not_replayed_after_upstream_event() {
    let (base_url, connections, requests) = serve_missing_previous_websocket(true).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "responses-websocket-visible-prefix-error";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "openai-compatible")
        .await;
    let headers = authorized_headers(&gateway).await;

    let mut first_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("first");
    first_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let first = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers.clone(),
        AiRequest::new(model, vec![first_user.clone()]),
    )
    .await;
    let first_body = to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first response body");
    let first_body: serde_json::Value =
        serde_json::from_slice(&first_body).expect("first response JSON");
    let first_answer = first_body["choices"][0]["message"]["content"]
        .as_str()
        .expect("first answer")
        .to_owned();

    let mut second_user = stravia_runtime_contract::protocol::ir::AiItem::output_text("second");
    second_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let mut second_request = AiRequest::new(
        model,
        vec![
            first_user,
            stravia_runtime_contract::protocol::ir::AiItem::output_text(first_answer),
            second_user,
        ],
    );
    second_request.stream.enabled = true;
    let second = execute_non_stream_request_with_headers(gateway, headers, second_request).await;
    assert_eq!(second.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(second.into_body(), usize::MAX)
        .await
        .expect("visible error body");
    assert!(
        String::from_utf8_lossy(&body).contains("upstream stream error"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(
        requests.lock().len(),
        2,
        "visible output forbids a full-history replay"
    );
}

#[tokio::test]
async fn cache_affinity_prefers_the_target_that_processed_a_long_exact_prefix() {
    let (first_url, first_calls) = serve_openai_sequence(vec![openai_response("unexpected")]).await;
    let mut first_cached = openai_response("first output");
    first_cached["usage"]["prompt_tokens"] = serde_json::json!(20_000);
    first_cached["usage"]["total_tokens"] = serde_json::json!(20_001);
    let (second_url, second_calls) =
        serve_openai_sequence(vec![first_cached, openai_response("continued output")]).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = Gateway::new(config).await.expect("gateway init");
    let model = "cache-affinity-route";
    let route_id = configure_route_with_protocol(
        &gateway,
        model,
        &[first_url, second_url],
        "custom",
        "openai-compatible",
    )
    .await;
    let backends = gateway
        .storage
        .routes()
        .list()
        .await
        .expect("Routes")
        .into_iter()
        .find(|route| route.id == route_id)
        .expect("cache-affinity Route")
        .targets;
    let configure_primary = |enabled| crate::db::models::UpdateRoute {
        targets: Some(
            backends
                .iter()
                .enumerate()
                .map(|(index, target)| crate::db::models::CreateTarget {
                    provider_id: target.provider_id().clone().into(),
                    model: target.model().clone().into(),
                    enabled: index != 0 || enabled,
                    priority: Some(target.priority),
                    first_token_timeout_ms: Some(target.first_token_timeout_ms),
                    target_retry_budget: Some(target.target_retry_budget),
                    target_cooldown_ms: Some(target.target_cooldown_ms),
                    thinking_level_map: target.thinking_level_map.clone(),
                })
                .collect(),
        ),
        ..Default::default()
    };
    gateway
        .admin()
        .update_model(model, configure_primary(false))
        .await
        .expect("disable primary");
    let headers = authorized_headers(&gateway).await;
    let principal = crate::proxy::security::Security::new(gateway.storage.auth())
        .required_principal(
            &crate::proxy::security::ClientCredential::from_inference_headers(&headers),
        )
        .await
        .expect("authorized principal");
    let mut prefix =
        stravia_runtime_contract::protocol::ir::AiItem::output_text("long cacheable prefix");
    prefix.role = stravia_runtime_contract::protocol::ir::Role::User;
    let first_turn = gateway
        .model_turn
        .execute(crate::agent::TurnInput::new(
            principal.clone(),
            AiRequest::new(model, vec![prefix.clone()]),
        ))
        .await
        .expect("first Model Turn");
    assert_eq!(
        first_turn.route.provider_id,
        backends[1].provider_id().as_str()
    );
    let first_events = first_turn.output.collect::<Vec<_>>().await;
    assert!(matches!(
        first_events.last(),
        Some(Ok(stravia_runtime_contract::model_turn::CanonicalEvent::Completed(response)))
            if response.output_text() == "first output" && response.usage.prompt_tokens == 20_000
    ));

    // 合格缓存前缀优先于更高的 Priority 和更低的一小时流量。
    gateway
        .admin()
        .update_model(model, configure_primary(true))
        .await
        .expect("re-enable primary");
    let mut follow_up = stravia_runtime_contract::protocol::ir::AiItem::output_text("follow up");
    follow_up.role = stravia_runtime_contract::protocol::ir::Role::User;
    let second_turn = gateway
        .model_turn
        .execute(crate::agent::TurnInput::new(
            principal,
            AiRequest::new(
                model,
                vec![
                    prefix,
                    stravia_runtime_contract::protocol::ir::AiItem::output_text("first output"),
                    follow_up,
                ],
            ),
        ))
        .await
        .expect("affine Model Turn");
    assert_eq!(
        second_turn.route.provider_id,
        backends[1].provider_id().as_str()
    );
    let events = second_turn.output.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Ok(stravia_runtime_contract::model_turn::CanonicalEvent::Completed(response)))
            if response.output_text() == "continued output"
    ));

    assert_eq!(first_calls.load(Ordering::SeqCst), 0);
    assert_eq!(second_calls.load(Ordering::SeqCst), 2);
}

/// 自定义 Responses Provider 的 `use_websocket` / `use_previous_response_id`
/// 行为回归：真实 Gateway 经本地 HTTP + WebSocket 上游，观察实际传输与请求体。
///
/// 上游按 `store=false` 的真实语义工作：WebSocket 的 `previous_response_id`
/// 只在该连接的当前 tip 上有效，HTTP 只认 `store=true` 保存过的响应；
/// 任何越界使用都会被拒绝并记录为 violation，测试同时断言 violation 为空。
mod responses_option_continuation {
    use super::*;
    use axum::{Json, response::IntoResponse};
    use serde_json::{Map, Value, json};
    use std::collections::HashSet;

    const MODEL: &str = "custom-responses-option-continuation";
    const CACHE_KEY: &str = "option-continuation-cache-key";
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Transport {
        Http,
        WebSocket,
    }

    #[derive(Clone, Debug)]
    struct Observed {
        transport: Transport,
        socket: Option<usize>,
        body: Value,
    }

    #[derive(Clone, Default)]
    struct UpstreamState {
        observed: Arc<parking_lot::Mutex<Vec<Observed>>>,
        authorizations: Arc<parking_lot::Mutex<Vec<Option<String>>>>,
        violations: Arc<parking_lot::Mutex<Vec<String>>>,
        stored: Arc<parking_lot::Mutex<HashSet<String>>>,
        next_socket: Arc<AtomicUsize>,
        next_response: Arc<AtomicUsize>,
        close_after_next_response: Arc<std::sync::atomic::AtomicBool>,
    }

    fn snapshot(id: &str, status: &str, output: Vec<Value>, store: bool, request: &Value) -> Value {
        let usage = if status == "completed" {
            json!({
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 1}
            })
        } else {
            Value::Null
        };
        let mut response =
            stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                id,
                "provider-model",
                status,
                output,
                Value::Null,
                Value::Null,
                usage,
            );
        // Responses 报告本轮实际采用的参数；默认快照会把 high 错报成 null，
        // 使后续请求因有效推理配置不一致而正确地退回完整历史。
        for (key, value) in response.as_object_mut().expect("response resource") {
            if let Some(requested) = request.get(key) {
                *value = requested.clone();
            }
        }
        response["store"] = store.into();
        response
    }

    /// 每个上游响应固定为「受保护推理 + 助手文本」，密文与答案都带序号。
    fn response_output(ordinal: usize) -> Vec<Value> {
        vec![
            json!({
                "id": format!("rs-{ordinal}"),
                "type": "reasoning",
                "status": "completed",
                "summary": [{"type": "summary_text", "text": format!("thinking-{ordinal}")}],
                "content": [],
                "encrypted_content": format!("cipher-{ordinal}")
            }),
            json!({
                "id": format!("msg-{ordinal}"),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": format!("answer-{ordinal}"),
                    "annotations": []
                }]
            }),
        ]
    }

    fn response_events(id: &str, ordinal: usize, store: bool, request: &Value) -> Vec<Value> {
        let output = response_output(ordinal);
        let message_id = format!("msg-{ordinal}");
        let text = format!("answer-{ordinal}");
        let mut events = vec![
            json!({"type": "response.created", "response": snapshot(id, "in_progress", Vec::new(), store, request)}),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": format!("rs-{ordinal}"),
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": [],
                    "content": []
                }
            }),
            json!({"type": "response.output_item.done", "output_index": 0, "item": output[0]}),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {
                    "id": message_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            }),
            json!({
                "type": "response.content_part.added",
                "item_id": message_id,
                "output_index": 1,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []}
            }),
            json!({
                "type": "response.output_text.delta",
                "item_id": message_id,
                "output_index": 1,
                "content_index": 0,
                "delta": text
            }),
            json!({
                "type": "response.content_part.done",
                "item_id": message_id,
                "output_index": 1,
                "content_index": 0,
                "part": {"type": "output_text", "text": text, "annotations": [], "logprobs": []}
            }),
            json!({"type": "response.output_item.done", "output_index": 1, "item": output[1]}),
            json!({"type": "response.completed", "response": snapshot(id, "completed", output, store, request)}),
        ];
        for (sequence, event) in events.iter_mut().enumerate() {
            event["sequence_number"] = sequence.into();
        }
        events
    }

    fn missing_previous(previous: &str) -> Value {
        json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "code": "previous_response_not_found",
                "message": format!("response {previous} is not available here")
            }
        })
    }

    async fn upstream_websocket(
        State(state): State<UpstreamState>,
        headers: HeaderMap,
        upgrade: WebSocketUpgrade,
    ) -> impl IntoResponse {
        let socket = state.next_socket.fetch_add(1, Ordering::SeqCst);
        state.authorizations.lock().push(
            headers
                .get(header::AUTHORIZATION)
                .map(|v| v.to_str().expect("authorization header").to_owned()),
        );
        upgrade.on_upgrade(move |mut websocket| async move {
            let mut tip: Option<String> = None;
            while let Some(Ok(AxumWebSocketMessage::Text(text))) = websocket.recv().await {
                let body: Value = serde_json::from_str(&text).expect("Responses WebSocket frame");
                state.observed.lock().push(Observed {
                    transport: Transport::WebSocket,
                    socket: Some(socket),
                    body: body.clone(),
                });
                if let Some(previous) = body.get("previous_response_id").and_then(Value::as_str)
                    && tip.as_deref() != Some(previous)
                {
                    state.violations.lock().push(format!(
                        "socket {socket} received {previous} while its tip was {tip:?}"
                    ));
                    websocket
                        .send(AxumWebSocketMessage::Text(
                            missing_previous(previous).to_string().into(),
                        ))
                        .await
                        .expect("send missing previous response error");
                    continue;
                }
                let ordinal = state.next_response.fetch_add(1, Ordering::SeqCst);
                let id = format!("upstream-{ordinal}");
                for event in response_events(&id, ordinal, false, &body) {
                    websocket
                        .send(AxumWebSocketMessage::Text(event.to_string().into()))
                        .await
                        .expect("send Responses WebSocket event");
                }
                tip = Some(id);
                if state
                    .close_after_next_response
                    .swap(false, Ordering::SeqCst)
                {
                    websocket
                        .send(AxumWebSocketMessage::Close(None))
                        .await
                        .expect("close upstream socket");
                    break;
                }
            }
        })
    }

    async fn upstream_http(
        State(state): State<UpstreamState>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        state.observed.lock().push(Observed {
            transport: Transport::Http,
            socket: None,
            body: body.clone(),
        });
        if let Some(previous) = body.get("previous_response_id").and_then(Value::as_str)
            && !state.stored.lock().contains(previous)
        {
            state
                .violations
                .lock()
                .push(format!("HTTP received unstored response {previous}"));
            return (
                StatusCode::BAD_REQUEST,
                Json(missing_previous(previous)["error"].clone()),
            )
                .into_response();
        }
        let ordinal = state.next_response.fetch_add(1, Ordering::SeqCst);
        let id = format!("upstream-{ordinal}");
        let store = body.get("store").and_then(Value::as_bool) != Some(false);
        if store {
            state.stored.lock().insert(id.clone());
        }
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            let mut stream = response_events(&id, ordinal, store, &body)
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().expect("event type")
                    )
                })
                .collect::<String>();
            stream.push_str("data: [DONE]\n\n");
            ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
        } else {
            Json(snapshot(
                &id,
                "completed",
                response_output(ordinal),
                store,
                &body,
            ))
            .into_response()
        }
    }

    struct Fixture {
        gateway: Gateway,
        _directory: tempfile::TempDir,
        upstream: tokio::task::JoinHandle<()>,
        state: UpstreamState,
        authorization: HeaderValue,
        provider_id: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.upstream.abort();
        }
    }

    impl Fixture {
        fn observed(&self) -> Vec<Observed> {
            self.state.observed.lock().clone()
        }

        fn authorizations(&self) -> Vec<Option<String>> {
            self.state.authorizations.lock().clone()
        }

        fn assert_no_violations(&self, label: &str) {
            let violations = self.state.violations.lock();
            assert!(
                violations.is_empty(),
                "{label}: upstream rejected a temporary response ID: {violations:?}"
            );
        }
    }

    fn options(websocket: bool, previous: bool) -> Map<String, Value> {
        let mut options = Map::new();
        options.insert("use_websocket".into(), websocket.into());
        options.insert("use_previous_response_id".into(), previous.into());
        options
    }

    async fn start_fixture(vendor_options: Map<String, Value>) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local Responses upstream");
        let address = listener.local_addr().expect("local Responses address");
        let base_url = format!("http://{address}/v1");
        let state = UpstreamState::default();
        let app = Router::new()
            .route("/v1/responses", get(upstream_websocket).post(upstream_http))
            .with_state(state.clone());
        let upstream = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("local Responses upstream");
        });

        let directory = tempfile::tempdir().expect("gateway directory");
        let gateway = Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some(format!("{MODEL}-provider")),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some("open-responses".into()),
                    base_url,
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "test-key".into(),
                },
                vendor_options,
                use_proxy: false,
            })
            .await
            .expect("create custom Responses provider");
        gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                "provider-model",
                crate::provider_models::CreateManualProviderModel {
                    metadata: json!({
                        "id": "provider-model",
                        "modalities": {"input": ["text", "image"], "output": ["text"]},
                        "reasoning_efforts": ["none", "low", "medium", "high", "xhigh"]
                    }),
                    template_id: None,
                },
            )
            .await
            .expect("create provider model");
        let provider_id = provider.id.clone();
        gateway
            .admin()
            .create_model(CreateRoute {
                output_modality: crate::db::models::RouteOutputModality::Text,
                model_id: MODEL.into(),
                display_name: None,
                balance: Some("traffic_equalization".into()),
                targets: vec![CreateTarget {
                    enabled: true,
                    provider_id: provider.id,
                    model: "provider-model".into(),
                    priority: None,
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(0),
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                }],
                default_thinking_level: None,
                billing: None,
            })
            .await
            .expect("create route");
        let headers = authorized_headers(&gateway).await;
        Fixture {
            gateway,
            _directory: directory,
            upstream,
            state,
            authorization: headers[header::AUTHORIZATION].clone(),
            provider_id,
        }
    }

    async fn post(fixture: &Fixture, path: &str, body: Value) -> (StatusCode, Value) {
        let response = crate::proxy::server::create_router(fixture.gateway.clone())
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, fixture.authorization.clone())
                    .body(Body::from(body.to_string()))
                    .expect("public inference request"),
            )
            .await
            .expect("public inference response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("public inference body");
        let body = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            panic!(
                "public inference JSON ({error}): {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, body)
    }

    #[derive(Clone, Copy, Debug)]
    enum Ingress {
        Responses,
        Chat,
    }

    /// 模拟标准客户端：Responses 只携带 Stravia 的 `previous_response_id` 与新输入；
    /// Chat 无状态地重放完整历史（含上一轮返回的助手消息）。`Clone` 即从当前节点分叉。
    #[derive(Clone)]
    struct Client {
        ingress: Ingress,
        store: bool,
        effort: &'static str,
        history: Vec<Value>,
        previous: Option<String>,
    }

    impl Client {
        fn new(ingress: Ingress, store: bool) -> Self {
            Self {
                ingress,
                store,
                effort: "high",
                history: Vec::new(),
                previous: None,
            }
        }

        fn user_message(&self, prompt: &str, with_image: bool) -> Value {
            let data_url = format!("data:image/png;base64,{PNG}");
            let (text_type, image) = match self.ingress {
                Ingress::Responses => (
                    "input_text",
                    json!({"type": "input_image", "image_url": data_url}),
                ),
                Ingress::Chat => (
                    "text",
                    json!({"type": "image_url", "image_url": {"url": data_url}}),
                ),
            };
            let mut content = vec![json!({"type": text_type, "text": prompt})];
            if with_image {
                content.push(image);
            }
            json!({"role": "user", "content": content})
        }

        async fn send(&mut self, fixture: &Fixture, prompt: &str, with_image: bool) {
            let user = self.user_message(prompt, with_image);
            match self.ingress {
                Ingress::Responses => {
                    let mut body = json!({
                        "model": MODEL,
                        "store": self.store,
                        "prompt_cache_key": CACHE_KEY,
                        "reasoning": {"effort": self.effort},
                        "input": [user],
                    });
                    if let Some(previous) = &self.previous {
                        body["previous_response_id"] = json!(previous);
                    }
                    let (status, response) = post(fixture, "/v1/responses", body).await;
                    assert_eq!(status, StatusCode::OK, "{response}");
                    self.previous = Some(
                        response["id"]
                            .as_str()
                            .expect("Stravia response ID")
                            .to_owned(),
                    );
                }
                Ingress::Chat => {
                    self.history.push(user);
                    let body = json!({
                        "model": MODEL,
                        "store": self.store,
                        "prompt_cache_key": CACHE_KEY,
                        "reasoning_effort": self.effort,
                        "messages": self.history,
                    });
                    let (status, response) = post(fixture, "/v1/chat/completions", body).await;
                    assert_eq!(status, StatusCode::OK, "{response}");
                    self.history.push(response["choices"][0]["message"].clone());
                }
            }
        }
    }

    fn input_wire(request: &Value) -> String {
        request["input"].to_string()
    }

    fn input_len(request: &Value) -> usize {
        request["input"].as_array().map_or(0, Vec::len)
    }

    fn assert_first_request(label: &str, request: &Value) {
        assert!(
            request.get("previous_response_id").is_none(),
            "{label}: {request}"
        );
        assert_eq!(input_len(request), 1, "{label}: {request}");
        let wire = input_wire(request);
        assert!(
            wire.contains("first") && wire.contains(PNG),
            "{label}: {request}"
        );
    }

    /// 完整历史：无任何上游 ID；每个已完成回合贡献用户、推理、助手三项，
    /// 图片与受保护推理密文都由本地 Generation Chain 恢复。
    fn assert_full_history(label: &str, request: &Value, prior_turns: usize, latest: &str) {
        assert!(
            request.get("previous_response_id").is_none(),
            "{label}: {request}"
        );
        assert_eq!(
            input_len(request),
            prior_turns * 3 + 1,
            "{label}: {request}"
        );
        let wire = input_wire(request);
        assert!(
            wire.contains("first") && wire.contains(PNG),
            "{label}: {request}"
        );
        assert!(wire.contains(latest), "{label}: {request}");
        for ordinal in 0..prior_turns {
            assert!(
                wire.contains(&format!("cipher-{ordinal}")),
                "{label}: {request}"
            );
            assert!(
                wire.contains(&format!("answer-{ordinal}")),
                "{label}: {request}"
            );
        }
    }

    /// 增量续接：精确的上游 tip 加仅含新输入的一项。
    fn assert_continues(label: &str, request: &Value, previous: &str, latest: &str) {
        assert_eq!(
            request["previous_response_id"],
            json!(previous),
            "{label}: {request}"
        );
        assert_eq!(input_len(request), 1, "{label}: {request}");
        let wire = input_wire(request);
        assert!(wire.contains(latest), "{label}: {request}");
        assert!(
            !wire.contains(PNG) && !wire.contains("cipher-") && !wire.contains("answer-"),
            "{label}: {request}"
        );
    }

    async fn assert_option_matrix(ingress: Ingress) {
        for (websocket, previous) in [(false, false), (false, true), (true, false), (true, true)] {
            let label = format!(
                "{ingress:?} use_websocket={websocket} use_previous_response_id={previous}"
            );
            let fixture = start_fixture(options(websocket, previous)).await;
            let mut client = Client::new(ingress, false);
            client.send(&fixture, "first", true).await;
            client.send(&fixture, "second", false).await;

            fixture.assert_no_violations(&label);
            let observed = fixture.observed();
            assert_eq!(observed.len(), 2, "{label}: {observed:?}");
            for request in &observed {
                assert_eq!(request.body["prompt_cache_key"], CACHE_KEY, "{label}");
                assert_eq!(request.body["reasoning"]["effort"], "high", "{label}");
            }
            let transport = if websocket {
                Transport::WebSocket
            } else {
                Transport::Http
            };
            assert!(
                observed
                    .iter()
                    .all(|request| request.transport == transport),
                "{label}: {observed:?}"
            );
            assert_eq!(
                fixture.authorizations().len(),
                usize::from(websocket),
                "{label}: one WebSocket handshake only when enabled"
            );
            assert!(
                observed
                    .iter()
                    .all(|request| request.body["store"] == json!(false)),
                "{label}: {observed:?}"
            );
            assert_first_request(&label, &observed[0].body);
            if websocket && previous {
                assert_eq!(observed[0].socket, observed[1].socket, "{label}");
                assert_continues(&label, &observed[1].body, "upstream-0", "second");
            } else {
                assert_full_history(&label, &observed[1].body, 1, "second");
            }
        }
    }

    #[tokio::test]
    async fn responses_ingress_selects_transport_and_history_for_each_option_combination() {
        assert_option_matrix(Ingress::Responses).await;
    }

    #[tokio::test]
    async fn chat_ingress_selects_transport_and_history_for_each_option_combination() {
        assert_option_matrix(Ingress::Chat).await;
    }

    #[tokio::test]
    async fn toggling_previous_response_id_keeps_images_and_protected_reasoning() {
        for ingress in [Ingress::Responses, Ingress::Chat] {
            let fixture = start_fixture(options(true, true)).await;
            let mut client = Client::new(ingress, false);
            client.send(&fixture, "first", true).await;
            fixture
                .gateway
                .admin()
                .update_provider(
                    &fixture.provider_id,
                    crate::db::models::UpdateProvider {
                        vendor_options: Some(options(true, false)),
                        ..Default::default()
                    },
                )
                .await
                .expect("disable previous response optimization");
            client.send(&fixture, "second", false).await;
            fixture
                .gateway
                .admin()
                .update_provider(
                    &fixture.provider_id,
                    crate::db::models::UpdateProvider {
                        vendor_options: Some(options(true, true)),
                        ..Default::default()
                    },
                )
                .await
                .expect("enable previous response optimization");
            client.send(&fixture, "third", false).await;
            client.send(&fixture, "fourth", false).await;

            fixture.assert_no_violations("toggled previous response optimization");
            let observed = fixture.observed();
            assert_eq!(observed.len(), 4, "{ingress:?}: {observed:?}");
            assert!(
                observed
                    .iter()
                    .all(|request| request.transport == Transport::WebSocket)
            );
            assert_full_history("disabled previous response", &observed[1].body, 1, "second");
            assert_full_history("changed provider options", &observed[2].body, 2, "third");
            assert_continues(
                "restored optimization",
                &observed[3].body,
                "upstream-2",
                "fourth",
            );
        }
    }

    #[tokio::test]
    async fn changed_thinking_effort_replays_history_before_resuming_websocket() {
        for ingress in [Ingress::Responses, Ingress::Chat] {
            let fixture = start_fixture(options(true, true)).await;
            let mut client = Client::new(ingress, false);
            client.send(&fixture, "first", true).await;
            client.effort = "low";
            client.send(&fixture, "second", false).await;
            client.send(&fixture, "third", false).await;

            fixture.assert_no_violations("changed thinking effort");
            let observed = fixture.observed();
            assert_eq!(observed.len(), 3, "{ingress:?}: {observed:?}");
            assert_eq!(observed[0].body["reasoning"]["effort"], "high");
            assert_eq!(observed[1].body["reasoning"]["effort"], "low");
            assert_eq!(observed[2].body["reasoning"]["effort"], "low");
            assert_full_history("changed effort", &observed[1].body, 1, "second");
            assert_continues("same new effort", &observed[2].body, "upstream-1", "third");
        }
    }

    #[tokio::test]
    async fn http_continuation_follows_previous_response_id_option_when_upstream_stores_state() {
        for (label, vendor_options, continues) in [
            ("defaults", Map::new(), true),
            ("previous enabled", options(false, true), true),
            ("previous disabled", options(false, false), false),
        ] {
            let fixture = start_fixture(vendor_options).await;
            let mut client = Client::new(Ingress::Responses, true);
            client.send(&fixture, "first", true).await;
            client.send(&fixture, "second", false).await;

            fixture.assert_no_violations(label);
            let observed = fixture.observed();
            assert_eq!(observed.len(), 2, "{label}: {observed:?}");
            assert!(fixture.authorizations().is_empty(), "{label}: HTTP only");
            assert!(
                observed
                    .iter()
                    .all(|request| request.transport == Transport::Http
                        && request.body["store"] == json!(true)),
                "{label}: {observed:?}"
            );
            assert_first_request(label, &observed[0].body);
            if continues {
                assert_continues(label, &observed[1].body, "upstream-0", "second");
            } else {
                assert_full_history(label, &observed[1].body, 1, "second");
            }
        }
    }

    #[tokio::test]
    async fn lost_or_replaced_websocket_never_receives_a_temporary_response_id() {
        let fixture = start_fixture(options(true, true)).await;
        fixture
            .state
            .close_after_next_response
            .store(true, Ordering::SeqCst);
        let mut client = Client::new(Ingress::Responses, false);
        client.send(&fixture, "first", true).await;
        // 上游在第一轮后关闭连接：第二轮必须完整历史，且不先发失败的增量。
        client.send(&fixture, "second", false).await;
        // 新连接上的下一轮可以安全续接。
        client.send(&fixture, "third", false).await;
        {
            let observed = fixture.observed();
            assert_eq!(observed.len(), 3, "{observed:?}");
            assert_ne!(observed[0].socket, observed[1].socket);
            assert_full_history("closed socket", &observed[1].body, 1, "second");
            assert_eq!(observed[1].socket, observed[2].socket);
            assert_continues("new socket tip", &observed[2].body, "upstream-1", "third");
        }

        fixture
            .gateway
            .admin()
            .update_provider(
                &fixture.provider_id,
                crate::db::models::UpdateProvider {
                    api_key: Some("rotated-test-key".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("rotate provider API key");
        client.send(&fixture, "fourth", false).await;

        fixture.assert_no_violations("lost or replaced socket");
        let observed = fixture.observed();
        assert_eq!(observed.len(), 4, "{observed:?}");
        assert_ne!(observed[2].socket, observed[3].socket);
        assert_full_history("replaced credential", &observed[3].body, 3, "fourth");
        assert_eq!(
            fixture.authorizations(),
            vec![
                Some("Bearer test-key".to_owned()),
                Some("Bearer test-key".to_owned()),
                Some("Bearer rotated-test-key".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn advanced_websocket_tip_sends_full_history_to_a_chat_sibling() {
        let fixture = start_fixture(options(true, true)).await;
        let mut parent = Client::new(Ingress::Chat, false);
        parent.send(&fixture, "first", true).await;
        let mut child_a = parent.clone();
        let mut child_b = parent.clone();
        child_a.send(&fixture, "child-a", false).await;
        child_b.send(&fixture, "child-b", false).await;

        fixture.assert_no_violations("sibling branch");
        let observed = fixture.observed();
        assert_eq!(observed.len(), 3, "{observed:?}");
        assert_eq!(observed[0].socket, observed[1].socket);
        assert_continues("first child", &observed[1].body, "upstream-0", "child-a");
        assert_full_history("advanced sibling", &observed[2].body, 1, "child-b");
        assert!(
            !input_wire(&observed[2].body).contains("child-a"),
            "{:?}",
            observed[2].body
        );
    }

    /// 请求 `store: true`，但上游实际不保留状态（只认存活连接的 tip，与
    /// 代理型上游一致）：请求里的 store 不能让 Gateway 把临时 ID 带到别的连接。
    #[tokio::test]
    async fn store_true_request_never_sends_a_temporary_id_beyond_the_live_websocket_tip() {
        let fixture = start_fixture(options(true, true)).await;
        fixture
            .state
            .close_after_next_response
            .store(true, Ordering::SeqCst);
        let mut client = Client::new(Ingress::Responses, true);
        client.send(&fixture, "first", true).await;
        client.send(&fixture, "second", false).await;
        let mut sibling = client.clone();
        client.send(&fixture, "third", false).await;
        sibling.send(&fixture, "sibling", false).await;

        fixture.assert_no_violations("store=true against a non-retaining upstream");
        let observed = fixture.observed();
        assert_eq!(observed.len(), 4, "{observed:?}");
        assert!(
            observed
                .iter()
                .all(|request| request.transport == Transport::WebSocket
                    && request.body["store"] == json!(true)),
            "{observed:?}"
        );
        assert_first_request("store=true first", &observed[0].body);
        // 第一条连接已被上游关闭：换连接后只能发完整历史。
        assert_ne!(observed[0].socket, observed[1].socket);
        assert_full_history("store=true closed socket", &observed[1].body, 1, "second");
        // 新连接的 tip 可续接。
        assert_eq!(observed[1].socket, observed[2].socket);
        assert_continues(
            "store=true live tip",
            &observed[2].body,
            "upstream-1",
            "third",
        );
        // tip 已前进，从 second 分叉出的 sibling 必须完整历史。
        assert_full_history("store=true stale sibling", &observed[3].body, 2, "sibling");
        assert!(
            !input_wire(&observed[3].body).contains("third"),
            "{:?}",
            observed[3].body
        );
    }
}
