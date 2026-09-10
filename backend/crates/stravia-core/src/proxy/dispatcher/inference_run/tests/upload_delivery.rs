use super::*;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::atomic::AtomicI64;

const PLACEHOLDER: &str = "<stravia-upload-key>";

struct UploadArgumentTool {
    received: tokio::sync::mpsc::Sender<Value>,
}

#[async_trait]
impl stravia_runtime_contract::hook::PlatformTool for UploadArgumentTool {
    fn id(&self) -> stravia_runtime_contract::hook::ToolId {
        stravia_runtime_contract::hook::ToolId::new("ordered-tool")
    }

    fn external_name(&self) -> &str {
        "ordered_tool"
    }

    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]})
    }

    async fn execute(
        &self,
        arguments: Value,
        _context: stravia_runtime_contract::hook::ToolExecutionContext,
    ) -> Result<Value, stravia_runtime_contract::hook::PlatformToolError> {
        self.received.send(arguments).await.map_err(|_| {
            stravia_runtime_contract::hook::PlatformToolError::new("test receiver closed")
        })?;
        Ok(json!({"accepted":true}))
    }
}

#[tokio::test]
async fn upload_delivery_http_reuses_grant_only_in_allowed_locations_and_scrubs_replay() {
    upload_delivery_roundtrip(false).await;
}

#[tokio::test]
async fn upload_delivery_http_stream_matches_split_placeholders_without_retyping_thinking() {
    upload_delivery_roundtrip(true).await;
}

async fn upload_delivery_roundtrip(stream: bool) {
    let captured = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let requests = Arc::clone(&captured);
    let provider = Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
            let requests = Arc::clone(&requests);
            async move {
                requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(request.clone());
                let messages = request["messages"].as_array().unwrap();
                let replay = messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"].as_str().is_some_and(|text| text.starts_with("Replay "))
                });
                if replay {
                    return axum::Json(openai_response("replay accepted")).into_response();
                }
                let platform_round = !messages.iter().any(|message| message["role"] == "tool");
                let (name, call_id, arguments) = if platform_round {
                    ("stravia__ordered_tool", "platform-upload", json!({"value":PLACEHOLDER}).to_string())
                } else {
                    ("client_upload", "client-upload", json!({"command":format!("upload {PLACEHOLDER} now")}).to_string())
                };
                let content = (!platform_round).then(|| format!("upload {PLACEHOLDER} now"));
                let thinking = (!platform_round).then(|| format!("private {PLACEHOLDER}"));
                if request["stream"] != true {
                    return axum::Json(json!({
                        "id":"upload-response","object":"chat.completion","model":"provider-model",
                        "choices":[{"index":0,"message":{
                            "role":"assistant","content":content,"reasoning_content":thinking,
                            "tool_calls":[{"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}]
                        },"finish_reason":"tool_calls"}]
                    })).into_response();
                }
                let mut deltas = vec![json!({"role":"assistant"})];
                if let Some(content) = content {
                    // One-character provider deltas split every possible placeholder boundary.
                    // An unrelated event in the middle must not reorder or drop held text.
                    for (index, character) in content.chars().enumerate() {
                        deltas.push(json!({"content":character.to_string()}));
                        if index == 12 {
                            deltas.push(json!({}));
                        }
                    }
                }
                if let Some(thinking) = thinking {
                    // Post-text Thinking is projected into a quoted preview for this ingress.
                    for character in thinking.chars() {
                        deltas.push(json!({"reasoning_content":character.to_string()}));
                    }
                }
                deltas.push(json!({"tool_calls":[{"index":0,"id":call_id,"type":"function","function":{"name":name,"arguments":""}}]}));
                for character in arguments.chars() {
                    deltas.push(json!({"tool_calls":[{"index":0,"function":{"arguments":character.to_string()}}]}));
                }
                let mut wire = String::new();
                for delta in deltas {
                    let event = json!({
                        "id":"upload-response","object":"chat.completion.chunk","model":"provider-model",
                        "choices":[{"index":0,"delta":delta,"finish_reason":null}]
                    });
                    wire.push_str(&format!("data: {event}\n\n"));
                }
                let terminal = json!({
                    "id":"upload-response","object":"chat.completion.chunk","model":"provider-model",
                    "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]
                });
                wire.push_str(&format!("data: {terminal}\n\ndata: [DONE]\n\n"));
                ([(header::CONTENT_TYPE,"text/event-stream")],wire).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_url = format!("http://{}", listener.local_addr().unwrap());
    let provider_task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let (received, mut tool_arguments) = tokio::sync::mpsc::channel(1);
    let (hook, _) = ExposeOrderedToolHook::counting();
    let mut gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(hook))
    .platform_tool(Arc::new(UploadArgumentTool { received }))
    .build()
    .await
    .unwrap();
    let now = Arc::new(AtomicI64::new(1_800_000_000_000));
    let clock = Arc::clone(&now);
    gateway.upload_grants = Arc::new(crate::agent::upload_grant::UploadGrantIssuer::with_clock(
        &[52; 32],
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    configure_route(&gateway, "upload-delivery", &[provider_url]).await;
    let mut settings = stravia_runtime_contract::artifact::ArtifactSettings {
        client_base_url: "http://client.example/deployment".into(),
        upload_prompt_injection: true,
        ..Default::default()
    };
    gateway
        .admin()
        .set_setting(
            "artifact_settings",
            &serde_json::to_string(&settings).unwrap(),
        )
        .await
        .unwrap();
    // Credential protection is mandatory, independently of optional reversible redaction.
    gateway
        .admin()
        .set_setting(stravia_credential_protection::SETTING_KEY, "false")
        .await
        .unwrap();
    gateway.observation.set_debug_enabled(true);
    let mut observations = gateway.observation.subscribe(0);
    let headers = authorized_headers(&gateway).await;
    let router = crate::proxy::server::create_router(gateway.clone());
    let mut request = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({
            "model":"upload-delivery","stream":stream,
            "messages":[{"role":"user","content":"Prepare my upload"}],
            "tools":[{"type":"function","function":{"name":"client_upload","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}]
        }).to_string())).unwrap();
    request.headers_mut().extend(headers.clone());
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let wire = String::from_utf8(body.to_vec()).unwrap();
    let (content, thinking, arguments) = if stream {
        let mut content = String::new();
        let mut thinking = String::new();
        let mut arguments = String::new();
        for line in wire.lines().filter_map(|line| line.strip_prefix("data: ")) {
            if line == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(line).unwrap();
            assert!(event.get("error").is_none(), "{event}");
            let delta = &event["choices"][0]["delta"];
            content.push_str(delta["content"].as_str().unwrap_or_default());
            thinking.push_str(delta["reasoning_content"].as_str().unwrap_or_default());
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    arguments.push_str(call["function"]["arguments"].as_str().unwrap_or_default());
                }
            }
        }
        (content, thinking, arguments)
    } else {
        let response: Value = serde_json::from_str(&wire).unwrap();
        let message = &response["choices"][0]["message"];
        (
            message["content"].as_str().unwrap().to_owned(),
            message["reasoning_content"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            message["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    };
    let arguments: Value = serde_json::from_str(&arguments).unwrap();
    let command = arguments["command"].as_str().unwrap();
    let key = command
        .strip_prefix("upload ")
        .unwrap()
        .strip_suffix(" now")
        .unwrap();
    assert_ne!(key, PLACEHOLDER);
    assert!(content.contains(&format!("upload {key} now")), "{content}");
    assert!(format!("{content}{thinking}").contains(&format!("private {PLACEHOLDER}")));
    assert!(!thinking.contains(key));
    assert_eq!(
        tool_arguments.recv().await.unwrap(),
        json!({"value":PLACEHOLDER})
    );
    let grants =
        regex::Regex::new(r"stravia_upload_[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
            .unwrap();
    assert_eq!(
        grants
            .find_iter(&wire)
            .map(|value| value.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([key])
    );

    // The substituted client argument is an actual upload credential, not a display token.
    let upload = router
        .clone()
        .oneshot(
            Request::post("/v1/artifacts/uploads")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"mime_type":"text/plain","size":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::CREATED);
    let first_interaction = finished_upload_interaction(&mut observations).await;

    // Replaying an expired delivered key while injection is disabled must still
    // restore placeholders before Provider input and every new platform record.
    now.fetch_add(16 * 60 * 1000, Ordering::SeqCst);
    settings.upload_prompt_injection = false;
    gateway
        .admin()
        .set_setting(
            "artifact_settings",
            &serde_json::to_string(&settings).unwrap(),
        )
        .await
        .unwrap();
    let mut replay = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({
            "model":"upload-delivery",
            "messages":[
                {"role":"user","content":"Prepare my upload"},
                {"role":"assistant","content":content,"tool_calls":[{"id":"client-upload","type":"function","function":{"name":"client_upload","arguments":arguments.to_string()}}]},
                {"role":"tool","tool_call_id":"client-upload","content":"uploaded"},
                {"role":"user","content":format!("Replay {key}")}
            ]
        }).to_string())).unwrap();
    replay.headers_mut().extend(headers);
    let replay = router.oneshot(replay).await.unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let replay: Value =
        serde_json::from_slice(&to_bytes(replay.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        replay["choices"][0]["message"]["content"],
        "replay accepted"
    );
    let replay_interaction = finished_upload_interaction(&mut observations).await;
    let captured = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for request in &captured {
        assert!(!request.to_string().contains("stravia_upload_"));
    }
    let replay_messages = captured.last().unwrap()["messages"].to_string();
    assert!(replay_messages.contains(&format!("Replay {PLACEHOLDER}")));
    assert!(replay_messages.contains(&format!("upload {PLACEHOLDER} now")));
    for id in [first_interaction, replay_interaction] {
        let detail = gateway
            .observation
            .get_interaction(&id, Default::default())
            .await
            .unwrap()
            .unwrap();
        let record = serde_json::to_string(&detail).unwrap();
        assert!(!record.contains("stravia_upload_"));
        assert!(record.contains(PLACEHOLDER));
    }
    gateway.shutdown().await;
    provider_task.abort();
}

async fn finished_upload_interaction(
    events: &mut crate::interaction_observation::ObservationStream,
) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(update) = events.next().await {
            if let crate::interaction_observation::ObservationUpdate::Event(event) = update {
                if event.kind == "run_finished" {
                    return event.interaction_id.unwrap();
                }
            }
        }
        panic!("observation stream ended before delivery completed");
    })
    .await
    .unwrap()
}
