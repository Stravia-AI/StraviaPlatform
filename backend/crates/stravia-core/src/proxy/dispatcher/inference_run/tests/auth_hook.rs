use super::*;

#[tokio::test]
async fn request_hook_response_bypasses_route_lookup_through_lifecycle_interface() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = crate::Gateway::builder(config)
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
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn request_hook_rejection_bypasses_route_lookup_and_model_authorization() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let gateway = crate::Gateway::builder(config)
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
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn expired_key_is_rejected_before_request_hook_model_rewrite() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let initial_model = "initial-before-hook";
    let final_model = "final-after-hook";
    let gateway = crate::Gateway::builder(config)
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
            rpm_limit: None,
            expires_at: Some("2000-01-01T00:00:00Z".into()),
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![final_route.id.into()],
            inject_media_understanding: false,
            spend_limit: None,
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
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn untrusted_credential_is_rejected_before_hook_model_rewrite() {
    let initial_model = "invalid-before-hook";
    let final_model = "invalid-after-hook";
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = gateway_rewriting_model(&data_dir, final_model).await;
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
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn hook_rewrite_checks_the_final_model_binding() {
    let initial_model = "bound-before-hook";
    let final_model = "unbound-after-hook";
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = gateway_rewriting_model(&data_dir, final_model).await;
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
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![initial_model_id],
            inject_media_understanding: false,
            spend_limit: None,
        })
        .await
        .expect("API key");

    let response = execute_non_stream_request_with_headers(
        gateway.clone(),
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
    close_test_gateway(gateway, data_dir).await;
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
async fn content_hook_processes_each_input_once_across_history_reuse_restart_and_edit() {
    use stravia_runtime_contract::protocol::ir::{AiItem, Continuation, Role, ToolCall};

    let reply = |text: &str| {
        serde_json::json!({
            "id": format!("chatcmpl-{text}"),
            "object": "chat.completion",
            "created": 1,
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
    };
    let mut first_reply = reply("first answer");
    first_reply["choices"][0]["message"]["tool_calls"] = serde_json::json!([{
        "id": "lookup-once",
        "type": "function",
        "function": { "name": "lookup", "arguments": "{}" }
    }]);
    first_reply["choices"][0]["finish_reason"] = serde_json::json!("tool_calls");
    let (base_url, _, requests) = serve_openai_sequence_with_requests(vec![
        first_reply,
        reply("second answer"),
        reply("third answer"),
        reply("branch answer"),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = gateway_with_once_content_hook(&data_dir).await;
    let model = "incremental-content-hook";
    configure_route(&gateway, model, &[base_url]).await;
    let headers = authorized_headers(&gateway).await;
    let user = |text: &str| {
        let mut item = AiItem::output_text(text);
        item.role = Role::User;
        item
    };
    let first_user = user("first");
    send_content_hook_request(
        &gateway,
        &headers,
        AiRequest::new(model, vec![first_user.clone()]),
    )
    .await;
    assert_content_hook_upstream_text(&requests, 0, &["first [processed]"]);

    // The assistant text and tool result have not previously been input.
    // Replay the client's original, unmodified history, not Hook output.
    let mut first_assistant = AiItem::output_text("first answer");
    first_assistant.tool_calls = Some(vec![ToolCall {
        id: "lookup-once".into(),
        name: "lookup".into(),
        arguments: "{}".into(),
    }]);
    let tool_result = AiItem::function_call_output("lookup-once", serde_json::json!("tool result"));
    let second_user = user("second");
    let second_id = send_content_hook_request(
        &gateway,
        &headers,
        AiRequest::new(
            model,
            vec![
                first_user.clone(),
                first_assistant.clone(),
                tool_result.clone(),
                second_user,
            ],
        ),
    )
    .await;
    assert_content_hook_upstream_text(
        &requests,
        1,
        &[
            "first [processed]",
            "first answer [processed]",
            "tool result [processed]",
            "second [processed]",
        ],
    );

    // Rebuild with the same isolated database and a fresh Hook/session.
    // Explicit continuation must restore processed history without rerunning it.
    shutdown_test_gateway(gateway).await;
    let gateway = gateway_with_once_content_hook(&data_dir).await;
    let third_user = user("third");
    let mut third_request = AiRequest::new(model, vec![third_user.clone()]);
    third_request.parameters.continuation = Some(Continuation::PreviousResponse(second_id));
    send_content_hook_request(&gateway, &headers, third_request).await;
    assert_content_hook_upstream_text(
        &requests,
        2,
        &[
            "first [processed]",
            "first answer [processed]",
            "tool result [processed]",
            "second [processed]",
            "second answer [processed]",
            "third [processed]",
        ],
    );

    // A client edit forks the history: no stale text may leak from the
    // previously processed second input, and every branch input is marked once.
    send_content_hook_request(
        &gateway,
        &headers,
        AiRequest::new(
            model,
            vec![
                first_user,
                first_assistant,
                tool_result,
                user("edited second"),
                AiItem::output_text("second answer"),
                third_user,
                AiItem::output_text("third answer"),
                user("branch"),
            ],
        ),
    )
    .await;
    assert_content_hook_upstream_text(
        &requests,
        3,
        &[
            "first [processed]",
            "first answer [processed]",
            "tool result [processed]",
            "edited second [processed]",
            "second answer [processed]",
            "third [processed]",
            "third answer [processed]",
            "branch [processed]",
        ],
    );
    close_test_gateway(gateway, data_dir).await;
}

struct OnceContentHook;

struct OnceContentSession;

impl stravia_runtime_contract::hook::Hook for OnceContentHook {
    fn descriptor(&self) -> stravia_runtime_contract::hook::HookDescriptor {
        stravia_runtime_contract::hook::HookDescriptor {
            event_kinds: vec![stravia_runtime_contract::hook::EventKind::Request],
            ..stravia_runtime_contract::hook::HookDescriptor::all("once-content")
        }
    }

    fn create_session(
        &self,
        _context: &stravia_runtime_contract::hook::SessionContext,
    ) -> Box<dyn stravia_runtime_contract::hook::HookSession> {
        Box::new(OnceContentSession)
    }
}

#[async_trait]
impl stravia_runtime_contract::hook::HookSession for OnceContentSession {
    async fn handle(
        &mut self,
        event: stravia_runtime_contract::hook::HookEvent<'_>,
    ) -> Result<stravia_runtime_contract::hook::ActionBatch, String> {
        let stravia_runtime_contract::hook::HookEvent::Request {
            current,
            input_start,
            ..
        } = event
        else {
            return Ok(stravia_runtime_contract::hook::ActionBatch::default());
        };
        let mut items = current.items[input_start..].to_vec();
        for item in &mut items {
            let text = item.content.to_text();
            if !text.is_empty() {
                item.content = stravia_runtime_contract::protocol::ir::MessageContent::Text(
                    format!("{text} [processed]").into(),
                );
            }
        }
        Ok(stravia_runtime_contract::hook::ActionBatch::one(
            stravia_runtime_contract::hook::HookAction::PatchRequest(Box::new(
                stravia_runtime_contract::hook::RequestPatch::SpliceInput {
                    range: 0..items.len(),
                    items,
                },
            )),
        ))
    }
}

async fn gateway_with_once_content_hook(data_dir: &tempfile::TempDir) -> Gateway {
    Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(OnceContentHook))
    .build()
    .await
    .expect("Gateway with content Hook")
}

async fn send_content_hook_request(
    gateway: &Gateway,
    headers: &HeaderMap,
    mut request: AiRequest,
) -> String {
    request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    request.parameters.store = Some(true);
    let response = execute_request_with_headers(
        gateway.clone(),
        headers.clone(),
        request,
        OPEN_RESPONSES_2026_04_24,
        "/v1/responses",
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("content Hook response body");
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("content Hook response JSON");
    assert_eq!(status, StatusCode::OK, "{body}");
    body["id"].as_str().expect("Gateway response ID").to_owned()
}

fn assert_content_hook_upstream_text(
    requests: &Arc<parking_lot::Mutex<Vec<String>>>,
    ordinal: usize,
    expected: &[&str],
) {
    let requests = requests.lock();
    let request = requests.get(ordinal).expect("captured upstream request");
    let (_, body) = request.split_once("\r\n\r\n").expect("HTTP request body");
    let body: serde_json::Value = serde_json::from_str(body).expect("upstream JSON");
    let actual: Vec<String> = body["messages"]
        .as_array()
        .expect("upstream messages")
        .iter()
        .filter_map(|message| match &message["content"] {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Array(blocks) => Some(
                blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect();
    assert_eq!(actual, expected, "upstream request {ordinal}: {body}");
}
