use super::*;
use axum::http::HeaderMap;

fn id(response: &Value) -> &str {
    response["id"].as_str().expect("response id")
}
fn user(text: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
}

#[derive(Clone, Default)]
struct OpenCodeCapture {
    ids: Arc<Mutex<Vec<(String, String)>>>,
}

async fn opencode_upstream(
    State(capture): State<OpenCodeCapture>,
    headers: HeaderMap,
    request: AxumRequest,
) -> Response {
    let session = headers
        .get("x-opencode-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("missing")
        .to_owned();
    let message = headers
        .get("x-opencode-request")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("missing")
        .to_owned();
    let _ = to_bytes(request.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let number = {
        let mut ids = capture.ids.lock();
        ids.push((session, message));
        ids.len()
    };
    let delta = if number == 1 {
        json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"lookup","arguments":"{}"}}]})
    } else {
        json!({"role":"assistant","content":"READY"})
    };
    let finish = if number == 1 { "tool_calls" } else { "stop" };
    let first = json!({"id":format!("chatcmpl-{number}"),"object":"chat.completion.chunk","created":1,
        "model":"big-pickle","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let last = json!({"id":format!("chatcmpl-{number}"),"object":"chat.completion.chunk","created":1,
        "model":"big-pickle","choices":[{"index":0,"delta":{},"finish_reason":finish}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from(format!(
            "data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"
        )))
        .unwrap()
}

async fn configure_opencode(gateway: &Gateway, url: String) -> anyhow::Result<(String, String)> {
    install_distributed_vendor_plugin(gateway, "opencode-free").await?;
    let provider = gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "OpenCode identity fixture".into(),
            vendor: Some("opencode-free".into()),
            protocol: "openai-compatible/chat-completions/v1".into(),
            base_url: url,
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await?;
    gateway.admin().create_manual_provider_model(&provider.id, "big-pickle", CreateManualProviderModel {
        metadata:json!({"id":"big-pickle","name":"big-pickle","modalities":{"input":["text"],"output":["text"]}}),
        template_id:None,
    }).await?;
    let route = gateway.admin().create_model(serde_json::from_value(json!({
        "model_id":"opencode-identity-fixture","output_modality":"text",
        "targets":[{"provider_id":provider.id,"model":"big-pickle","enabled":true,"target_retry_budget":0}]
    }))?).await?;
    let key = gateway.admin().create_api_key(serde_json::from_value(json!({
        "name":"OpenCode identity owner","model_ids":[route.id],"transparent_injection_enabled":false
    }))?).await?;
    let other = gateway.admin().create_api_key(serde_json::from_value(json!({
        "name":"OpenCode identity other principal","model_ids":[route.id],"transparent_injection_enabled":false
    }))?).await?;
    Ok((key.token, other.token))
}

async fn send_opencode(
    gateway: &Gateway,
    token: &str,
    input: Value,
    parent: Option<&str>,
) -> anyhow::Result<Value> {
    let mut body = json!({"model":"opencode-identity-fixture","store":false,"stream":false,"input":input,
        "tools":[{"type":"function","name":"lookup","description":"fixture lookup","parameters":{"type":"object","properties":{}}}]});
    if let Some(parent) = parent {
        body["previous_response_id"] = json!(parent);
    }
    let response = create_router(gateway.clone())
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    let body: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(body)
}

// OpenCode's first 48 bits encode the low 36 timestamp bits and a 12-bit
// counter, with all bits inverted for descending session IDs.
fn native_timestamp(id: &str, prefix: &str, descending: bool) -> u64 {
    assert_eq!(id.len(), 30, "{id}");
    assert!(id.starts_with(prefix), "{id}");
    let mut encoded = u64::from_str_radix(&id[4..16], 16).expect("native timestamp");
    if descending {
        encoded ^= (1u64 << 48) - 1;
    }
    encoded >> 12
}

#[tokio::test]
async fn opencode_native_identity_follows_prompt_and_conversation_across_gateway_restart()
-> anyhow::Result<()> {
    let capture = OpenCodeCapture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            axum::routing::post(opencode_upstream),
        )
        .route(
            "/models",
            axum::routing::get(|| async { axum::Json(json!({"data":[{"id":"big-pickle"}]})) }),
        )
        .with_state(capture.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve OpenCode fixture");
    });
    let directory = tempfile::tempdir()?;
    sqlite_fixture::seed_database(directory.path()).await?;
    let config = GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    };
    let gateway = Gateway::new(config.clone()).await?;
    let (token, other) = configure_opencode(&gateway, url).await?;
    let before = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64)
        & ((1 << 36) - 1);
    let first = send_opencode(&gateway, &token, json!([user("first prompt")]), None).await?;
    let tool = send_opencode(
        &gateway,
        &token,
        json!([{"type":"function_call_output","call_id":"call_fixture","output":"found"}]),
        Some(id(&first)),
    )
    .await?;
    let second = send_opencode(
        &gateway,
        &token,
        json!([user("second prompt")]),
        Some(id(&tool)),
    )
    .await?;
    gateway.shutdown().await;
    drop(gateway);
    let gateway = Gateway::new(config).await?;
    let after_restart = send_opencode(
        &gateway,
        &token,
        json!([user("after restart")]),
        Some(id(&second)),
    )
    .await?;
    let (left, right, isolated) = tokio::join!(
        send_opencode(
            &gateway,
            &token,
            json!([user("same branch")]),
            Some(id(&after_restart))
        ),
        send_opencode(
            &gateway,
            &token,
            json!([user("same branch")]),
            Some(id(&after_restart))
        ),
        send_opencode(&gateway, &other, json!([user("first prompt")]), None),
    );
    left?;
    right?;
    isolated?;
    let after = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64)
        & ((1 << 36) - 1);
    {
        let ids = capture.ids.lock();
        assert_eq!(ids.len(), 7);
        let root_session = &ids[0].0;
        let root_message = &ids[0].1;
        assert_eq!(&ids[1].0, root_session);
        assert_eq!(
            &ids[1].1, root_message,
            "tool continuation retains the user prompt identity"
        );
        assert_eq!(&ids[2].0, root_session);
        assert_ne!(
            &ids[2].1, root_message,
            "a new user prompt gets a new message identity"
        );
        assert_eq!(&ids[3].0, root_session, "conversation survives restart");
        assert_ne!(&ids[3].1, &ids[2].1);
        let root_time = native_timestamp(root_session, "ses_", true);
        let first_time = native_timestamp(root_message, "msg_", false);
        assert_eq!(
            root_time, first_time,
            "root session and first prompt share their admission time"
        );
        for (session, message) in ids.iter() {
            let session_time = native_timestamp(session, "ses_", true);
            let message_time = native_timestamp(message, "msg_", false);
            assert!(
                (before..=after).contains(&session_time),
                "session time {session_time} outside {before}..={after}"
            );
            assert!(
                (before..=after).contains(&message_time),
                "prompt time {message_time} outside {before}..={after}"
            );
        }
        let branches: Vec<_> = ids[4..]
            .iter()
            .filter(|(session, _)| session == root_session)
            .collect();
        let isolated: Vec<_> = ids[4..]
            .iter()
            .filter(|(session, _)| session != root_session)
            .collect();
        assert_eq!(branches.len(), 2);
        assert_eq!(isolated.len(), 1);
        assert_ne!(
            branches[0].1, branches[1].1,
            "identical branch admissions get distinct prompt IDs"
        );
        assert_ne!(isolated[0].1, *root_message);
    }
    gateway.shutdown().await;
    server.abort();
    Ok(())
}

#[derive(Clone, Default)]
struct GrokCapture {
    ordinals: Arc<Mutex<Vec<String>>>,
}

async fn grok_upstream(
    State(capture): State<GrokCapture>,
    headers: HeaderMap,
    request: AxumRequest,
) -> Response {
    let ordinal = headers
        .get("x-grok-turn-idx")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("missing")
        .to_owned();
    let _ = to_bytes(request.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let number = {
        let mut values = capture.ordinals.lock();
        values.push(ordinal);
        values.len()
    };
    if number == 4 {
        return Response::builder()
            .status(400)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"error":{"type":"invalid_request_error","message":"scripted failure"}}"#,
            ))
            .unwrap();
    }
    let item = if number == 1 {
        json!({"id":"fc_fixture","type":"function_call","status":"completed","call_id":"call_fixture","name":"lookup","arguments":"{}"})
    } else {
        json!({"id":format!("msg_{number}"),"type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"READY","annotations":[]}]})
    };
    let completed = json!({"id":format!("resp_{number}"),"object":"response","created_at":1,"completed_at":2,
        "status":"completed","model":"grok-4.6","output":[item],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}},
        "incomplete_details":null,"previous_response_id":null,"instructions":null,"error":null,"tools":[],"tool_choice":"auto",
        "truncation":"disabled","parallel_tool_calls":true,"text":{"format":{"type":"text"}},"top_p":null,"presence_penalty":null,
        "frequency_penalty":null,"top_logprobs":null,"temperature":null,"reasoning":null,"max_output_tokens":null,"max_tool_calls":null,
        "store":false,"background":false,"service_tier":"default","metadata":{},"safety_identifier":null,"prompt_cache_key":null});
    let mut created = completed.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["usage"] = Value::Null;
    let mut added = item.clone();
    added["status"] = json!("in_progress");
    if number == 1 {
        added["arguments"] = json!("");
    } else {
        added["content"] = json!([]);
    }
    let mut events = vec![
        json!({"type":"response.created","response":created}),
        json!({"type":"response.output_item.added","output_index":0,"item":added}),
    ];
    if number == 1 {
        events.extend([
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":item["id"],"delta":"{}"}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":item["id"],"arguments":"{}"}),
        ]);
    } else {
        let part = item["content"][0].clone();
        let mut empty = part.clone();
        empty["text"] = json!("");
        events.extend([
            json!({"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":item["id"],"part":empty}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":item["id"],"delta":"READY"}),
            json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":item["id"],"text":"READY"}),
            json!({"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":item["id"],"part":part}),
        ]);
    }
    events.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
    events.push(json!({"type":"response.completed","response":completed}));
    let stream = events
        .into_iter()
        .enumerate()
        .map(|(i, mut event)| {
            event["sequence_number"] = json!(i);
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from(stream))
        .unwrap()
}

async fn configure_grok(gateway: &Gateway, url: String) -> anyhow::Result<(String, String)> {
    install_distributed_vendor_plugin(gateway, "xai-grok").await?;
    let provider = gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "Grok ordinal fixture".into(),
            vendor: Some("xai-grok".into()),
            protocol: "open-responses".into(),
            base_url: url,
            preset_key: None,
            channel: Some("grok".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
        })
        .await?;
    gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "xai-grok".into(),
                scheme: "Bearer".into(),
                access_token: "local-fixture".into(),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                meta: Some(json!({"user_id":"fixture-user"}).to_string()),
                ..Default::default()
            },
        )
        .await?;
    gateway.admin().create_manual_provider_model(&provider.id, "grok-4.6", CreateManualProviderModel {
        metadata:json!({"id":"grok-4.6","name":"grok-4.6","modalities":{"input":["text"],"output":["text"]}}), template_id:None,
    }).await?;
    let route = gateway.admin().create_model(serde_json::from_value(json!({"model_id":"grok-ordinal-fixture","output_modality":"text",
        "targets":[{"provider_id":provider.id,"model":"grok-4.6","enabled":true,"target_retry_budget":0,"target_cooldown_ms":1}]}))?).await?;
    let key = gateway.admin().create_api_key(serde_json::from_value(json!({"name":"Grok owner","model_ids":[route.id],"transparent_injection_enabled":false}))?).await?;
    let other = gateway.admin().create_api_key(serde_json::from_value(json!({"name":"Grok other principal","model_ids":[route.id],"transparent_injection_enabled":false}))?).await?;
    Ok((key.token, other.token))
}

async fn send_grok(
    gateway: &Gateway,
    token: &str,
    input: Value,
    parent: Option<&str>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut body = json!({"model":"grok-ordinal-fixture","store":false,"stream":false,"input":input,
        "tools":[{"type":"function","name":"lookup","description":"fixture lookup","parameters":{"type":"object","properties":{}}}]});
    if let Some(parent) = parent {
        body["previous_response_id"] = json!(parent);
    }
    let response = create_router(gateway.clone())
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .header("x-grok-turn-idx", "999")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

#[tokio::test]
async fn grok_ordinal_tracks_user_prompts_not_tool_legs_and_survives_restart() -> anyhow::Result<()>
{
    let capture = GrokCapture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let router = Router::new()
        .fallback(grok_upstream)
        .with_state(capture.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve Grok fixture");
    });
    let directory = tempfile::tempdir()?;
    sqlite_fixture::seed_database(directory.path()).await?;
    let config = GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    };
    let gateway = Gateway::new(config.clone()).await?;
    let (token, other) = configure_grok(&gateway, url).await?;
    let first_input = user("first prompt");
    let (status, first) = send_grok(&gateway, &token, json!([first_input]), None).await?;
    assert_eq!(status, StatusCode::OK, "{first}");
    let tool_result =
        json!({"type":"function_call_output","call_id":"call_fixture","output":"found"});
    let (status, tool) =
        send_grok(&gateway, &token, json!([tool_result]), Some(id(&first))).await?;
    assert_eq!(status, StatusCode::OK, "{tool}");
    let second_input = user("second prompt");
    let (status, second) =
        send_grok(&gateway, &token, json!([second_input]), Some(id(&tool))).await?;
    assert_eq!(status, StatusCode::OK, "{second}");
    let (status, failed) = send_grok(
        &gateway,
        &token,
        json!([user("failed prompt")]),
        Some(id(&second)),
    )
    .await?;
    assert!(!status.is_success(), "{failed}");
    tokio::time::sleep(Duration::from_millis(25)).await;
    let mut history = vec![first_input];
    history.extend(first["output"].as_array().unwrap().iter().cloned());
    history.push(tool_result);
    history.extend(tool["output"].as_array().unwrap().iter().cloned());
    history.push(second_input);
    history.extend(second["output"].as_array().unwrap().iter().cloned());
    history.push(user("failed prompt"));
    history.push(user("new prompt after failure"));
    let (status, third) = send_grok(&gateway, &token, json!(history), None).await?;
    assert_eq!(status, StatusCode::OK, "{third}");
    gateway.shutdown().await;
    drop(gateway);
    let gateway = Gateway::new(config).await?;
    let (status, fourth) = send_grok(
        &gateway,
        &token,
        json!([user("after restart")]),
        Some(id(&third)),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{fourth}");
    let (left, right, isolated) = tokio::join!(
        send_grok(
            &gateway,
            &token,
            json!([user("concurrent branch")]),
            Some(id(&fourth))
        ),
        send_grok(
            &gateway,
            &token,
            json!([user("concurrent branch")]),
            Some(id(&fourth))
        ),
        send_grok(&gateway, &other, json!([user("first prompt")]), None),
    );
    for result in [left?, right?, isolated?] {
        assert_eq!(result.0, StatusCode::OK, "{}", result.1);
    }
    {
        let ordinals = capture.ordinals.lock();
        assert_eq!(&ordinals[..6], &["1", "1", "2", "3", "4", "5"]);
        let mut concurrent = ordinals[6..].to_vec();
        concurrent.sort_unstable();
        assert_eq!(concurrent, ["1", "6", "6"]);
    }
    gateway.shutdown().await;
    server.abort();
    Ok(())
}
