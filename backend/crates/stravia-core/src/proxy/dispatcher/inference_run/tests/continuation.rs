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
                egress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            },
            input.request,
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
        calls: Arc::new(std::sync::Mutex::new(Vec::new())),
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
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(Default::default()),
    );
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
    continuation.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                previous_response_id: Some(
                    response["id"].as_str().expect("response identity").into(),
                ),
                ..Default::default()
            },
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
        wait_for_observed_run_finish(&mut events).await;
        let forest = gateway
            .observation
            .query_forest(Default::default())
            .await
            .expect("observation forest");
        let interaction = forest
            .roots
            .iter()
            .flat_map(|root| &root.interactions)
            .find(|interaction| {
                interaction
                    .visible_tail
                    .ends_with(&format!("{prompt} answer"))
            })
            .expect("terminal response remains visible in its interaction");
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

    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
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

    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    third_request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::Anthropic(
            stravia_runtime_contract::protocol::ir::AnthropicExt::default(),
        ),
    );
    let third_response = execute_request_with_headers(
        gateway,
        headers,
        third_request,
        ANTHROPIC_MESSAGES_2023_06_01,
        "/v1/messages",
    )
    .await;
    assert_eq!(third_response.status(), StatusCode::OK);

    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    first_request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                store: Some(false),
                ..Default::default()
            },
        ),
    );
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
    second_request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                store: Some(false),
                ..Default::default()
            },
        ),
    );
    let second_response =
        execute_non_stream_request_with_headers(gateway, headers, second_request).await;
    assert_eq!(second_response.status(), StatusCode::OK);
    to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("second response body");

    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len(),
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
        "test-http",
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
    let first_target = crate::router::selected_target_key(&crate::router::SelectedTarget {
        provider_id: backends[0].provider_id.clone(),
        model: backends[0].model.clone(),
        priority: backends[0].priority,
        first_token_timeout_ms: backends[0].first_token_timeout_ms,
        target_retry_budget: backends[0].target_retry_budget,
        target_cooldown_ms: backends[0].target_cooldown_ms,
        thinking_level_map: backends[0].thinking_level_map.0.clone(),
    });
    for _ in 0..3 {
        gateway.health_registry.record_failure(&first_target);
    }
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
    assert_eq!(first_turn.route.provider_id, backends[1].provider_id);
    let _ = first_turn.output.collect::<Vec<_>>().await;

    gateway.health_registry.record_success(&first_target);
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
    assert_eq!(second_turn.route.provider_id, backends[1].provider_id);
    let events = second_turn.output.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Ok(stravia_runtime_contract::model_turn::CanonicalEvent::Completed(response)))
            if response.output_text() == "continued output"
    ));

    assert_eq!(first_calls.load(Ordering::SeqCst), 0);
    assert_eq!(second_calls.load(Ordering::SeqCst), 2);
}
