use super::*;

#[tokio::test]
async fn lifecycle_uses_in_memory_model_turn_executor_without_an_upstream() {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-in-memory-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (gateway, mut logs) = crate::Gateway::new(config).await.expect("gateway init");
    let headers = authorized_headers(&gateway).await;
    let mut scripted = AiResponse::new("response-in-memory", "in-memory-model");
    scripted.push_output_text("delivered from InMemory");
    let executor = crate::agent::InMemoryModelTurnExecutor::scripted([scripted]);

    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::new(executor.clone()),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({"model": "in-memory-model"})),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new("in-memory-model", Vec::new()),
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
        .expect("InMemory response body");
    assert!(String::from_utf8_lossy(&body).contains("delivered from InMemory"));
    assert_eq!(executor.requests().len(), 1);
    let log = logs
        .recv()
        .await
        .expect("successful Model Turn should emit a request log");
    assert_eq!(log.model_id.as_deref(), Some("in-memory-model"));
    assert_eq!(log.model_name.as_deref(), Some("in-memory-model"));
    assert_eq!(log.provider_id, "in-memory");
    assert_eq!(log.provider_name, "in-memory");
    assert_eq!(log.upstream_model, "in-memory-model");
}

#[tokio::test]
async fn edited_visible_reasoning_restores_the_authoritative_protected_block() {
    let data_dir = std::env::temp_dir().join(format!(
        "stravia-protected-history-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config = crate::config::GatewayConfig {
        data_dir,
        ..Default::default()
    };
    let (gateway, _logs) = crate::Gateway::new(config.clone())
        .await
        .expect("gateway init");
    let (incompatible_url, incompatible_calls) =
        serve_openai_response(200, openai_response("must not be called")).await;
    configure_route(&gateway, "opaque-incompatible", &[incompatible_url]).await;
    let headers = authorized_headers(&gateway).await;
    let mut protected = AiResponse::new("protected-response", "in-memory-model");
    protected.push_reasoning("provider reasoning", Some("opaque-signature".into()));
    protected.push_reasoning(
        "second provider reasoning",
        Some("second-opaque-signature".into()),
    );
    protected.push_output_text("first answer");
    let mut final_response = AiResponse::new("final-response", "in-memory-model");
    final_response.push_output_text("second answer");
    let executor = crate::agent::InMemoryModelTurnExecutor::scripted([protected, final_response]);

    let first = execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::new(executor.clone()),
        headers: headers.clone(),
        envelope: RawEnvelope::new(
            Some(serde_json::json!({"model": "in-memory-model"})),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new("in-memory-model", Vec::new()),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(first.into_body(), usize::MAX)
            .await
            .expect("first response body"),
    )
    .expect("first response JSON");
    let projected = first_body["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .expect("projected assistant reasoning");
    assert_eq!(
        first_body["choices"][0]["message"]["content"],
        "first answer"
    );
    assert!(projected.contains("stravia-history-marker:"), "{projected}");
    assert_eq!(projected.matches("stravia-history-marker:").count(), 2);
    let references =
        crate::history_marker::history_marker_references(&[crate::protocol::ir::AiItem::thinking(
            projected, None,
        )]);
    assert_eq!(references.len(), 2, "{projected}");
    assert_ne!(references[0], references[1], "{projected}");
    assert!(!projected.contains("opaque-signature"), "{projected}");
    let edited = projected.replace("provider reasoning", "client-edited reasoning");
    drop(gateway);
    let (gateway, _logs) = crate::Gateway::new(config)
        .await
        .expect("gateway reconstruction");
    let rejected = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers.clone(),
        AiRequest::new(
            "opaque-incompatible",
            vec![crate::protocol::ir::AiItem::thinking(edited.clone(), None)],
        ),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let rejected_body = to_bytes(rejected.into_body(), usize::MAX)
        .await
        .expect("incompatible route body");
    assert!(
        String::from_utf8_lossy(&rejected_body).contains("protected_context_unrepresentable"),
        "{}",
        String::from_utf8_lossy(&rejected_body)
    );
    assert_eq!(incompatible_calls.load(Ordering::SeqCst), 0);

    let second = execute(RunInput {
        gateway,
        executor: std::sync::Arc::new(executor.clone()),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({"model": "in-memory-model"})),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new(
            "in-memory-model",
            vec![crate::protocol::ir::AiItem::thinking(edited, None)],
        ),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;
    assert_eq!(second.status(), StatusCode::OK);

    let requests = executor.requests();
    let restored = requests[1]
        .items
        .iter()
        .flat_map(|item| match &item.content {
            crate::protocol::ir::MessageContent::Blocks(blocks) => blocks.as_slice(),
            crate::protocol::ir::MessageContent::Text(_) => &[],
        })
        .filter_map(|block| match block {
            crate::protocol::ir::ContentBlock::Thinking {
                thinking,
                signature,
            } => Some((thinking.as_str(), signature.as_deref())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        restored,
        vec![
            ("provider reasoning", Some("opaque-signature")),
            ("second provider reasoning", Some("second-opaque-signature")),
        ],
        "all protected reasoning blocks are restored in order after Gateway reconstruction"
    );
    assert!(
        !requests[1]
            .items
            .iter()
            .any(|item| item.thinking_ref() == Some(("client-edited reasoning", None))),
        "client edits must not replace the authoritative protected block"
    );
}

#[tokio::test]
async fn signed_reasoning_stream_replay_uses_one_preview_and_authoritative_marker() {
    let anthropic_stream = |response_id: &str, reasoning: &str, signature: &str, answer: &str| {
        format!(
            "event: message_start\ndata: {}\n\n\
             event: content_block_start\ndata: {}\n\n\
             event: content_block_delta\ndata: {}\n\n\
             event: content_block_delta\ndata: {}\n\n\
             event: content_block_stop\ndata: {}\n\n\
             event: content_block_start\ndata: {}\n\n\
             event: content_block_delta\ndata: {}\n\n\
             event: content_block_stop\ndata: {}\n\n\
             event: message_delta\ndata: {}\n\n\
             event: message_stop\ndata: {}\n\n",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": response_id,
                    "type": "message",
                    "role": "assistant",
                    "model": "provider-model",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 1, "output_tokens": 0}
                }
            }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": reasoning}
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": signature}
            }),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "text", "text": ""}
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "text_delta", "text": answer}
            }),
            serde_json::json!({"type": "content_block_stop", "index": 1}),
            serde_json::json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "end_turn",
                    "stop_sequence": null
                },
                "usage": {"output_tokens": 2}
            }),
            serde_json::json!({"type": "message_stop"})
        )
    };
    let (upstream_url, calls) = serve_sse_sequence(vec![
        anthropic_stream(
            "signed-first",
            "signed preview",
            "signed-opaque",
            "first answer",
        ),
        anthropic_stream(
            "signed-second",
            "continued reasoning",
            "signed-second-opaque",
            "second answer",
        ),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway init");
    configure_route_with_protocol(
        &gateway,
        "signed-chat-replay",
        &[upstream_url],
        "test-http",
        "anthropic-messages",
    )
    .await;
    let headers = authorized_headers(&gateway).await;
    let mut first_request = AiRequest::new(
        "signed-chat-replay",
        vec![crate::protocol::ir::AiItem::output_text("hello")],
    );
    first_request.stream.enabled = true;
    let first = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        first_request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = String::from_utf8(
        to_bytes(first.into_body(), usize::MAX)
            .await
            .expect("first response body")
            .to_vec(),
    )
    .expect("UTF-8 first response");
    let projected = first_body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["reasoning_content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<String>();
    assert_eq!(
        projected.matches("signed preview").count(),
        1,
        "{projected}"
    );
    assert_eq!(
        projected
            .matches(crate::history_marker::PROJECTION_DELIMITER_PREFIX)
            .count(),
        2,
        "signed preview must have one preview start/end pair: {projected}"
    );
    assert_eq!(
        projected
            .matches(crate::history_marker::HISTORY_MARKER_PREFIX)
            .count(),
        1,
        "{projected}"
    );
    let preview_end = projected
        .find(":preview:0:end -->")
        .expect("preview delimiter end");
    let marker_start = projected
        .find(crate::history_marker::HISTORY_MARKER_PREFIX)
        .expect("History Marker carrier");
    assert!(preview_end < marker_start, "{projected}");

    let mut second_request = AiRequest::new(
        "signed-chat-replay",
        vec![
            crate::protocol::ir::AiItem::output_text("hello"),
            crate::protocol::ir::AiItem::thinking(projected, None),
        ],
    );
    second_request.stream.enabled = true;
    let second = execute_request_with_headers(
        gateway,
        headers,
        second_request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = String::from_utf8(
        to_bytes(second.into_body(), usize::MAX)
            .await
            .expect("second response body")
            .to_vec(),
    )
    .expect("UTF-8 second response");
    assert!(second_body.contains("second answer"), "{second_body}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn protected_reasoning_replay_preserves_parallel_public_tool_calls() {
    let first_calls = [
        ("call_a", "glob"),
        ("call_b", "glob"),
        ("call_c", "glob"),
        ("call_d", "glob"),
        ("call_e", "glob"),
        ("call_f", "glob"),
    ]
    .into_iter()
    .map(|(id, name)| crate::protocol::ir::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: "{}".into(),
    })
    .collect::<Vec<_>>();
    let second_calls = [
        ("call_g", "glob"),
        ("call_h", "glob"),
        ("call_i", "glob"),
        ("call_j", "glob"),
        ("call_k", "glob"),
        ("call_l", "glob"),
        ("call_m", "glob"),
        ("call_n", "glob"),
    ]
    .into_iter()
    .map(|(id, name)| crate::protocol::ir::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: "{}".into(),
    })
    .collect::<Vec<_>>();
    let (base_url, _connections, provider_requests) = serve_responses_websocket_streams(vec![
        openai_responses_protected_parallel_tools_sse("resp-protected-first", &first_calls),
        openai_responses_protected_parallel_tools_sse("resp-protected-second", &second_calls),
        openai_responses_sse("finished"),
    ])
    .await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-protected-tool-history-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(expose_tool_hook))
        .platform_tool(Arc::new(OrderedTool {
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }))
        .build()
        .await
        .expect("gateway init");
    let model = "protected-tool-history";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "openai-compatible")
        .await;
    let headers = authorized_headers(&gateway).await;
    let tools = vec![crate::protocol::ir::ToolSpec {
        name: "glob".into(),
        description: Some("Find files".into()),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        strict: None,
        cache_control: None,
        meta: None,
    }];
    let system = crate::protocol::ir::AiItem {
        role: crate::protocol::ir::Role::System,
        content: crate::protocol::ir::MessageContent::Text("repository instructions".into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let user = crate::protocol::ir::AiItem {
        role: crate::protocol::ir::Role::User,
        content: crate::protocol::ir::MessageContent::Text("inspect repository".into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut first_request = AiRequest::new(model, vec![system.clone(), user.clone()]);
    first_request.stream.enabled = true;
    first_request.tools = Some(tools.clone());
    let marker_from_stream = |body: &str| {
        let marker_start = body
            .find("<!-- stravia-history-marker:")
            .unwrap_or_else(|| panic!("projected marker content: {body}"));
        let marker_end = body[marker_start..]
            .find(" -->")
            .map(|offset| marker_start + offset + " -->".len())
            .expect("marker suffix");
        body[marker_start..marker_end].to_owned()
    };

    let first = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        first_request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    let first_status = first.status();
    let first_body = String::from_utf8(
        to_bytes(first.into_body(), usize::MAX)
            .await
            .expect("first response body")
            .to_vec(),
    )
    .expect("UTF-8 response");
    assert_eq!(first_status, StatusCode::OK, "{first_body}");
    let first_marker = marker_from_stream(&first_body);

    let first_assistant = crate::protocol::ir::AiItem {
        role: crate::protocol::ir::Role::Assistant,
        content: crate::protocol::ir::MessageContent::Blocks(vec![
            crate::protocol::ir::ContentBlock::Thinking {
                thinking: "inspect repository".into(),
                signature: None,
            },
            crate::protocol::ir::ContentBlock::Text {
                text: first_marker,
                cache_control: None,
            },
        ]),
        tool_calls: Some(first_calls.clone()),
        tool_call_id: None,
        meta: None,
    };
    let mut first_history = vec![system.clone(), user.clone(), first_assistant];
    first_history.extend(first_calls.iter().map(|call| {
        crate::protocol::ir::AiItem::function_call_output(
            &call.id,
            serde_json::Value::String(format!("{}-result", call.id)),
        )
    }));
    let mut second_request = AiRequest::new(model, first_history.clone());
    second_request.stream.enabled = true;
    second_request.tools = Some(tools.clone());
    let second = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        second_request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    let second_status = second.status();
    let second_body = String::from_utf8(
        to_bytes(second.into_body(), usize::MAX)
            .await
            .expect("second response body")
            .to_vec(),
    )
    .expect("UTF-8 response");
    assert_eq!(second_status, StatusCode::OK, "{second_body}");
    let second_marker = marker_from_stream(&second_body);

    let second_assistant = crate::protocol::ir::AiItem {
        role: crate::protocol::ir::Role::Assistant,
        content: crate::protocol::ir::MessageContent::Blocks(vec![
            crate::protocol::ir::ContentBlock::Thinking {
                thinking: "inspect more files".into(),
                signature: None,
            },
            crate::protocol::ir::ContentBlock::Text {
                text: second_marker,
                cache_control: None,
            },
        ]),
        tool_calls: Some(second_calls.clone()),
        tool_call_id: None,
        meta: None,
    };
    let mut second_history = first_history;
    second_history.push(second_assistant);
    second_history.extend(second_calls.iter().map(|call| {
        crate::protocol::ir::AiItem::function_call_output(
            &call.id,
            serde_json::Value::String(format!("{}-result", call.id)),
        )
    }));
    let mut third_request = AiRequest::new(model, second_history);
    third_request.stream.enabled = true;
    third_request.tools = Some(tools);
    let third = execute_request_with_headers(
        gateway,
        headers,
        third_request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(third.status(), StatusCode::OK);
    let _ = to_bytes(third.into_body(), usize::MAX)
        .await
        .expect("third response body");

    let requests = provider_requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1].get("previous_response_id").is_none(),
        "{}",
        requests[1]
    );
    assert_eq!(
        requests[1]["input"]
            .as_array()
            .expect("second Open Responses input")
            .iter()
            .filter(|item| item["type"] == "function_call")
            .filter_map(|item| item["call_id"].as_str())
            .collect::<Vec<_>>(),
        vec!["call_a", "call_b", "call_c", "call_d", "call_e", "call_f"]
    );
    assert_eq!(
        requests[2]["input"]
            .as_array()
            .expect("third Open Responses input")
            .iter()
            .filter(|item| item["type"] == "function_call")
            .filter_map(|item| item["call_id"].as_str())
            .collect::<Vec<_>>(),
        vec![
            "call_a", "call_b", "call_c", "call_d", "call_e", "call_f", "call_g", "call_h",
            "call_i", "call_j", "call_k", "call_l", "call_m", "call_n"
        ]
    );
    assert_eq!(
        requests[2]["input"]
            .as_array()
            .expect("third Open Responses input")
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .filter_map(|item| item["call_id"].as_str())
            .collect::<Vec<_>>(),
        vec![
            "call_a", "call_b", "call_c", "call_d", "call_e", "call_f", "call_g", "call_h",
            "call_i", "call_j", "call_k", "call_l", "call_m", "call_n"
        ]
    );
}

#[tokio::test]
async fn unary_completion_fills_canonical_response_defaults() {
    let mut upstream = openai_response("canonical defaults");
    upstream["id"] = serde_json::Value::String(String::new());
    upstream["model"] = serde_json::Value::String(String::new());
    upstream["choices"][0]["finish_reason"] = serde_json::Value::Null;
    let (upstream_url, _) = serve_openai_response(200, upstream).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
    configure_route(&gateway, "canonical-defaults", &[upstream_url]).await;

    let response = execute_non_stream(gateway, "canonical-defaults").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("canonical response body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("canonical response JSON");
    assert!(
        body["id"]
            .as_str()
            .is_some_and(|response_id| !response_id.is_empty()),
        "{body}"
    );
    assert_eq!(body["model"], "provider-model", "{body}");
    assert_eq!(body["choices"][0]["finish_reason"], "stop", "{body}");
}

#[tokio::test]
async fn open_responses_owns_response_identity_and_logical_model() {
    let (upstream_url, _) = serve_openai_response(200, openai_response("gateway identity")).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
    configure_route(&gateway, "logical-model", &[upstream_url]).await;

    let response = execute_protocol_request(
        gateway,
        "logical-model",
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
        false,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Open Responses body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("Open Responses JSON");
    assert!(
        body["id"]
            .as_str()
            .is_some_and(|response_id| response_id.starts_with("resp_")),
        "{body}"
    );
    assert_eq!(body["model"], "logical-model", "{body}");
    assert!(body.get("output_text").is_none(), "{body}");
}

#[tokio::test]
async fn catalog_provider_without_dedicated_vendor_adapter_reaches_upstream() {
    let (upstream_base_url, provider_calls) =
        serve_openai_response(200, openai_response("catalog response")).await;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let (gateway, _logs) = Gateway::new(config).await.expect("gateway init");
    let catalog = gateway.provider_catalog.providers().await;
    let catalog_provider = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "zhipuai-coding-plan")
        .expect("bundled Catalog contains Zhipu AI Coding Plan");
    let channel = catalog_provider
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .expect("Zhipu AI Coding Plan default channel");
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: None,
            source: ProviderSourceInput::Catalog {
                provider_id: catalog_provider.id.clone(),
                channel_id: channel.id.clone(),
                fingerprint: channel.fingerprint.clone(),
                base_url_override: Some(upstream_base_url),
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-key".into(),
            },
            use_proxy: false,
        })
        .await
        .expect("create Catalog provider");
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "glm-5",
            crate::provider_models::CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "glm-5",
                    "name": "GLM-5",
                }),
            },
        )
        .await
        .expect("create test Provider Model");
    gateway
        .admin()
        .create_model(CreateRoute {
            name: "gpt-5.4".into(),
            balance: None,
            target_provider: provider.id,
            target_model: "glm-5".into(),
            targets: Vec::new(),
        })
        .await
        .expect("create model route");

    let response = execute_non_stream(gateway, "gpt-5.4").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hidden_rounds_are_iterative_and_platform_tools_keep_response_order() {
    let tool_round = serde_json::json!({
        "id": "chatcmpl-tools",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "stravia__ordered_tool",
                            "arguments": "{\"index\":1}"
                        }
                    },
                    {
                        "id": "call-2",
                        "type": "function",
                        "function": {
                            "name": "stravia__ordered_tool",
                            "arguments": "{\"index\":2}"
                        }
                    }
                ]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let (base_url, provider_calls, provider_requests) =
        serve_openai_sequence_with_requests(vec![tool_round, openai_response("final response")])
            .await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-hidden-round-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(expose_tool_hook))
        .platform_tool(Arc::new(OrderedTool {
            calls: tool_calls.clone(),
        }))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "hidden-round-route", &[base_url]).await;

    let response = execute_non_stream(gateway, "hidden-round-route").await;

    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("hidden-round response body");
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("final response"), "{body}");
    assert_eq!(
        body.matches("stravia-history-marker:").count(),
        2,
        "each Platform execution must retain an independent Marker"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *request_hook_rounds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![0, 1],
        "each provider round must run Request Hook exactly once"
    );
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1, 2]
    );
    let provider_requests = provider_requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(provider_requests.len(), 2);
    assert!(
        provider_requests[1].contains("\"tool_call_id\":\"call-1\""),
        "{}",
        provider_requests[1]
    );
    assert!(
        provider_requests[1].contains("\"tool_call_id\":\"call-2\""),
        "{}",
        provider_requests[1]
    );
}

#[tokio::test]
async fn thinking_level_is_clamped_and_mapped_without_replaying_omitted_control() {
    let (base_url, calls, requests) = serve_openai_sequence_with_requests(vec![
        openai_response("clamped"),
        openai_response("omitted"),
        openai_response("off"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "thinking-level-model";
    configure_route_with_protocol(
        &gateway,
        model,
        &[base_url],
        "test-http",
        "openai-compatible",
    )
    .await;
    let headers = authorized_headers(&gateway).await;

    for level in [
        Some(crate::thinking::ThinkingLevel::Max),
        None,
        Some(crate::thinking::ThinkingLevel::Off),
    ] {
        let mut user = crate::protocol::ir::AiItem::output_text("hello");
        user.role = crate::protocol::ir::Role::User;
        let mut request = AiRequest::new(model, vec![user]);
        request.reasoning.level = level;
        let response = execute_request_with_headers(
            gateway.clone(),
            headers.clone(),
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("client response");
    }

    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(requests[0].contains("\"reasoning_effort\":\"high\""));
    assert!(!requests[1].contains("reasoning_effort"));
    assert!(requests[2].contains("\"reasoning_effort\":\"none\""));
}

#[tokio::test]
async fn unrepresentable_thinking_control_is_a_typed_422_before_upstream() {
    let (base_url, calls) = serve_openai_sequence(vec![openai_response("must not run")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "thinking-loss-model";
    let route_id = configure_route_with_protocol(
        &gateway,
        model,
        &[base_url],
        "test-http",
        "openai-compatible",
    )
    .await;
    let route = gateway
        .admin()
        .list_models()
        .await
        .expect("Routes")
        .into_iter()
        .find(|route| route.id == route_id)
        .expect("thinking Route");
    let mut map = route.targets[0].thinking_level_map.0.clone();
    map.iter_mut()
        .find(|row| row.level == crate::thinking::ThinkingLevel::Medium)
        .expect("medium row")
        .control = crate::thinking::TargetThinkingControl::Budget { value: 8192 };
    gateway
        .storage
        .routes()
        .put(crate::db::models::PutRoute {
            id: Some(route.id.clone()),
            route_id: route.name.clone(),
            selection_strategy: route.balance.clone(),
            is_enabled: route.is_enabled,
            targets: vec![CreateTarget {
                provider_id: route.targets[0].provider_id.clone(),
                model: route.targets[0].model.clone(),
                weight: Some(route.targets[0].weight),
                priority: Some(route.targets[0].priority),
                thinking_level_map: map,
            }],
        })
        .await
        .expect("inject legacy unrepresentable map");
    gateway
        .model_cache
        .write()
        .await
        .reload(gateway.storage.routes())
        .await
        .expect("reload injected Route");

    let mut user = crate::protocol::ir::AiItem::output_text("hello");
    user.role = crate::protocol::ir::Role::User;
    let mut request = AiRequest::new(model, vec![user]);
    request.reasoning.level = Some(crate::thinking::ThinkingLevel::Medium);
    let response = execute_request_with_headers(
        gateway.clone(),
        authorized_headers(&gateway).await,
        request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("loss response");
    assert!(String::from_utf8_lossy(&body).contains("STRAVIA_PROTOCOL_LOSSY_REJECTED"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_thinking_is_rejected_when_the_route_opens_no_levels() {
    let (base_url, calls) = serve_openai_sequence(vec![openai_response("must not run")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "no-thinking-level-model";
    let route_id = configure_route_with_protocol(
        &gateway,
        model,
        &[base_url],
        "test-http",
        "openai-compatible",
    )
    .await;
    let route = gateway
        .admin()
        .list_models()
        .await
        .expect("Routes")
        .into_iter()
        .find(|route| route.id == route_id)
        .expect("no-thinking-level Route");
    let targets = route
        .targets
        .iter()
        .map(|target| {
            let mut map = target.thinking_level_map.0.clone();
            for row in &mut map {
                row.control = crate::thinking::TargetThinkingControl::Hidden;
            }
            crate::db::models::UpsertTarget {
                id: Some(target.id.clone()),
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                weight: Some(target.weight),
                priority: Some(target.priority),
                thinking_level_map: map,
            }
        })
        .collect();
    gateway
        .admin()
        .update_model(
            &route.name,
            crate::db::models::UpdateRoute {
                targets: Some(targets),
                ..Default::default()
            },
        )
        .await
        .expect("close Thinking Levels");

    let mut user = crate::protocol::ir::AiItem::output_text("hello");
    user.role = crate::protocol::ir::Role::User;
    let mut request = AiRequest::new(model, vec![user]);
    request.reasoning.level = Some(crate::thinking::ThinkingLevel::Low);
    let response = execute_request_with_headers(
        gateway.clone(),
        authorized_headers(&gateway).await,
        request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failover_remaps_the_same_clamped_level_for_the_next_target() {
    let (failed_url, failed_calls) =
        serve_openai_response(500, serde_json::json!({"error": "retry"})).await;
    let (fallback_url, fallback_calls, fallback_requests) =
        serve_openai_sequence_with_requests(vec![openai_response("fallback")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "thinking-failover-model";
    let route_id = configure_route_with_protocol(
        &gateway,
        model,
        &[failed_url, fallback_url],
        "test-http",
        "openai-compatible",
    )
    .await;
    let route = gateway
        .admin()
        .list_models()
        .await
        .expect("Routes")
        .into_iter()
        .find(|route| route.id == route_id)
        .expect("thinking failover Route");
    let targets = route
        .targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let mut map = target.thinking_level_map.0.clone();
            map.iter_mut()
                .find(|row| row.level == crate::thinking::ThinkingLevel::Medium)
                .expect("medium row")
                .control = crate::thinking::TargetThinkingControl::Effort {
                value: if index == 0 { "low" } else { "high" }.into(),
            };
            crate::db::models::UpsertTarget {
                id: Some(target.id.clone()),
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                weight: Some(target.weight),
                priority: Some(target.priority),
                thinking_level_map: map,
            }
        })
        .collect();
    gateway
        .admin()
        .update_model(
            &route.name,
            crate::db::models::UpdateRoute {
                targets: Some(targets),
                ..Default::default()
            },
        )
        .await
        .expect("override per-Target thinking controls");

    let mut user = crate::protocol::ir::AiItem::output_text("hello");
    user.role = crate::protocol::ir::Role::User;
    let mut request = AiRequest::new(model, vec![user]);
    request.reasoning.level = Some(crate::thinking::ThinkingLevel::Medium);
    let response = execute_request_with_headers(
        gateway.clone(),
        authorized_headers(&gateway).await,
        request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("fallback response");
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    let fallback_requests = fallback_requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(fallback_requests[0].contains("\"reasoning_effort\":\"high\""));
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn wire_capture_records_both_sides_and_redacts_headers() {
    let (base_url, _connections, _requests) =
        serve_responses_websocket_sequence(vec!["first answer", "second answer"]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let capture_dir = data_dir.path().join("wire-captures");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        wire_capture_dir: Some(capture_dir.clone()),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "wire-capture-model";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "openai-compatible")
        .await;
    let headers = authorized_headers(&gateway).await;

    let mut first_user = crate::protocol::ir::AiItem::output_text("test");
    first_user.role = crate::protocol::ir::Role::User;
    let first_response = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        AiRequest::new(model, vec![first_user.clone()]),
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let _ = to_bytes(first_response.into_body(), usize::MAX)
        .await
        .expect("first captured client response");

    let mut second_user = crate::protocol::ir::AiItem::output_text("continue");
    second_user.role = crate::protocol::ir::Role::User;
    let second_response = execute_request_with_headers(
        gateway,
        headers,
        AiRequest::new(
            model,
            vec![
                first_user,
                crate::protocol::ir::AiItem::output_text("first answer"),
                second_user,
            ],
        ),
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    assert_eq!(second_response.status(), StatusCode::OK);
    let _ = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("second captured client response");

    let paths = std::fs::read_dir(&capture_dir)
        .expect("capture directory")
        .map(|entry| entry.expect("capture entry").path())
        .collect::<Vec<_>>();
    assert_eq!(paths.len(), 1);
    assert!(
        paths[0]
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("__chain-resp_"))
    );
    let capture = std::fs::read_to_string(&paths[0]).expect("wire capture");
    let events = capture
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("capture event"))
        .collect::<Vec<_>>();
    let capture_ids = events
        .iter()
        .filter_map(|event| event["capture_id"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(capture_ids.len(), 2);
    for (peer, phase) in [
        ("client", "request"),
        ("client", "response"),
        ("upstream", "request"),
        ("upstream", "response"),
    ] {
        assert!(
            events
                .iter()
                .any(|event| event["peer"] == peer && event["phase"] == phase),
            "missing {peer} {phase}: {capture}"
        );
    }
    assert!(!capture.contains("test-key"));
    assert!(capture.contains(r#"\"authorization\":\"***\""#));
    assert!(
        events
            .iter()
            .all(|event| event.get("body_base64").is_none())
    );
    assert!(events.iter().any(|event| {
        event["peer"] == "upstream"
            && event["phase"] == "request"
            && event["body"]
                .as_str()
                .is_some_and(|body| body.contains("\"test\""))
    }));
}
