use super::*;
use axum::extract::ws::{Message, WebSocketUpgrade};
use base64::Engine;

const CODEX_MODEL: &str = "gpt-6-astra";
const CODEX_ROUTE: &str = "codex-media-replay";

struct SocketUpstream {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    reject_upgrade: Arc<std::sync::atomic::AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

impl SocketUpstream {
    async fn start() -> anyhow::Result<Self> {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = bodies.clone();
        let reject_upgrade = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rejection = reject_upgrade.clone();
        let router = Router::new().route(
            "/responses",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let captured = captured.clone();
                let rejection = rejection.clone();
                async move {
                    use axum::response::IntoResponse;
                    if rejection.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    upgrade
                        .on_upgrade(move |mut socket| async move {
                            while let Some(Ok(Message::Text(text))) = socket.recv().await {
                                let body = serde_json::from_str(&text).expect("Codex request JSON");
                                let turn = {
                                    let mut bodies = captured.lock();
                                    bodies.push(body);
                                    bodies.len()
                                };
                                for event in response_frames(turn) {
                                    if socket
                                        .send(Message::Text(event.to_string().into()))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        })
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve Codex fixture");
        });
        Ok(Self {
            url,
            bodies,
            reject_upgrade,
            server,
        })
    }
}

impl Drop for SocketUpstream {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn response_frames(turn: usize) -> Vec<Value> {
    let item_id = format!("msg_{turn}");
    let item = json!({
        "type": "message", "id": item_id, "status": "completed", "role": "assistant",
        "content": [{"type": "output_text", "text": "READY", "annotations": []}]
    });
    let response = json!({
        "id": format!("resp_{turn}"), "object": "response", "created_at": 1, "completed_at": 2,
        "status": "completed", "model": CODEX_MODEL, "output": [item],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
            "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}},
        "incomplete_details": null, "previous_response_id": null, "instructions": null, "error": null,
        "tools": [], "tool_choice": "auto", "truncation": "disabled", "parallel_tool_calls": true,
        "text": {"format": {"type": "text"}}, "top_p": null, "presence_penalty": null,
        "frequency_penalty": null, "top_logprobs": null, "temperature": null, "reasoning": null,
        "max_output_tokens": null, "max_tool_calls": null, "store": false, "background": false,
        "service_tier": "default", "metadata": {}, "safety_identifier": null, "prompt_cache_key": null
    });
    let mut created = response.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["usage"] = Value::Null;
    let part = item["content"][0].clone();
    let mut empty_part = part.clone();
    empty_part["text"] = json!("");
    let mut events = vec![
        json!({"type": "response.created", "response": created}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
        json!({"type": "response.content_part.added", "output_index": 0, "content_index": 0, "item_id": item_id, "part": empty_part}),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": item_id, "delta": "READY"}),
        json!({"type": "response.output_text.done", "output_index": 0, "content_index": 0, "item_id": item_id, "text": "READY"}),
        json!({"type": "response.content_part.done", "output_index": 0, "content_index": 0, "item_id": item_id, "part": part}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": response}),
    ];
    for (sequence, event) in events.iter_mut().enumerate() {
        event["sequence_number"] = json!(sequence);
    }
    events
}

async fn configure(
    gateway: &Gateway,
    upstream: &SocketUpstream,
    responses_lite: bool,
) -> anyhow::Result<String> {
    install_distributed_vendor_plugin(gateway, "openai-codex").await?;
    let provider = gateway.storage.providers().create(CreateProviderRecord {
        name: "Codex media replay fixture".into(),
        vendor: Some("openai-codex".into()),
        protocol: "open-responses".into(),
        base_url: upstream.url.clone(),
        preset_key: None,
        channel: Some("codex".into()),
        models_source: None,
        static_models: None,
        api_key: String::new(),
        adapter_credentials: "{}".into(),
        vendor_options: json!({"websocket_url": format!("{}/responses", upstream.url.replacen("http://", "ws://", 1))}).to_string(),
        auth_mode: "oauth".into(),
        use_proxy: false,
    }).await?;
    gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "Bearer".into(),
                access_token: "synthetic-local-fixture".into(),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                meta: Some(json!({"account_id": "local-fixture-account"}).to_string()),
                ..Default::default()
            },
        )
        .await?;
    gateway.admin().create_manual_provider_model(&provider.id, CODEX_MODEL, CreateManualProviderModel {
        metadata: json!({"id": CODEX_MODEL, "name": CODEX_MODEL, "use_responses_lite": responses_lite, "modalities": {"input": ["text", "image"], "output": ["text"]}}),
        template_id: None,
    }).await?;
    let route = gateway.admin().create_model(serde_json::from_value(json!({
        "model_id": CODEX_ROUTE, "output_modality": "text",
        "targets": [{"provider_id": provider.id, "model": CODEX_MODEL, "enabled": true, "target_retry_budget": 0, "target_cooldown_ms": COOLDOWN_MS}]
    }))?).await?;
    let key = gateway.admin().create_api_key(serde_json::from_value(json!({
        "name": "Codex media replay key", "model_ids": [route.id], "transparent_injection_enabled": false
    }))?).await?;
    gateway
        .admin()
        .set_setting(
            "artifact_settings",
            r#"{"external_signed_downloads":false,"client_base_url":"https://downloads.example"}"#,
        )
        .await?;
    Ok(key.token)
}

async fn send(
    gateway: &Gateway,
    token: &str,
    input: Value,
    parent: Option<&str>,
) -> anyhow::Result<Value> {
    let (status, body) = send_status(gateway, token, input, parent).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(body)
}

async fn send_status(
    gateway: &Gateway,
    token: &str,
    input: Value,
    parent: Option<&str>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut body = json!({
        "model": CODEX_ROUTE, "store": false, "stream": false,
        "prompt_cache_key": "codex-media-replay-session", "input": input
    });
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
    Ok((status, body))
}

fn image_urls(request: &Value) -> Vec<&str> {
    request["input"]
        .as_array()
        .expect("Responses input")
        .iter()
        .flat_map(|item| {
            item.get("output")
                .or_else(|| item.get("content"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|part| part["type"] == "input_image")
        .map(|part| part["image_url"].as_str().expect("image URL"))
        .collect()
}

fn static_image_url() -> String {
    format!(
        "data:image/webp;base64,{}",
        base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../fixtures/media/static.webp"))
    )
}

async fn lite_image_continuations(tool_result: bool) -> anyhow::Result<()> {
    let upstream = SocketUpstream::start().await?;
    let directory = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    };
    let gateway = Gateway::new(config.clone()).await?;
    let token = configure(&gateway, &upstream, true).await?;
    let image = static_image_url();
    let image_content = json!([
        {"type": "input_text", "text": "Describe this image."},
        {"type": "input_image", "image_url": image, "detail": "auto"}
    ]);
    let mut history = if tool_result {
        vec![
            json!({"type": "function_call", "call_id": "read-image", "name": "read_image", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "read-image", "output": image_content}),
        ]
    } else {
        vec![json!({"role": "user", "content": image_content})]
    };
    // Native image routing must not invalidate the canonical boundaries required
    // by the real Codex Lite encoder, even on the first request.
    let first = send(&gateway, &token, json!(history), None).await?;
    assert_eq!(first["output"][0]["content"][0]["text"], "READY");
    {
        let sent = upstream.bodies.lock();
        assert_eq!(sent.len(), 1);
        assert_eq!(image_urls(&sent[0]), [image.as_str()]);
    }

    history.extend(
        first["output"]
            .as_array()
            .expect("first response output")
            .iter()
            .cloned(),
    );
    history.push(json!({"role": "user", "content": [
        {"type": "input_text", "text": "Describe it again from the complete history."}
    ]}));
    let second = send(&gateway, &token, json!(history), None).await?;
    assert_eq!(second["output"][0]["content"][0]["text"], "READY");
    {
        let sent = upstream.bodies.lock();
        assert_eq!(sent.len(), 2);
        let full_history = &sent[1];
        if full_history.get("previous_response_id").is_none() {
            assert_eq!(image_urls(full_history), [image.as_str()]);
        } else {
            assert_eq!(full_history["previous_response_id"], "resp_1");
            assert!(
                image_urls(full_history).is_empty(),
                "a Lite continuation must not resend the already accepted image"
            );
        }
    }

    let parent = second["id"]
        .as_str()
        .expect("second response ID")
        .to_owned();
    // Lose the socket and in-memory history: previous_response_id must restore
    // the persisted image and send a valid complete Lite request to a new socket.
    gateway.shutdown().await;
    drop(gateway);
    let gateway = Gateway::new(config).await?;
    let third = send(
        &gateway,
        &token,
        json!([{"role": "user", "content": [
            {"type": "input_text", "text": "Describe the saved image once more."}
        ]}]),
        Some(&parent),
    )
    .await?;
    assert_eq!(third["output"][0]["content"][0]["text"], "READY");
    {
        let sent = upstream.bodies.lock();
        assert_eq!(
            sent.len(),
            3,
            "each successful turn must reach the local WS"
        );
        assert!(sent[2].get("previous_response_id").is_none());
        assert_eq!(image_urls(&sent[2]), [image.as_str()]);
        for request in sent.iter() {
            assert_eq!(
                request["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
                "true",
                "the fixture must exercise the real Lite encoder"
            );
        }
    }
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lite_native_image_preserves_full_history_and_cold_continuation() -> anyhow::Result<()> {
    lite_image_continuations(false).await
}

#[tokio::test]
async fn lite_tool_result_image_preserves_full_history_and_cold_continuation() -> anyhow::Result<()>
{
    lite_image_continuations(true).await
}

#[tokio::test]
async fn socket_miss_replays_history_and_new_images_as_inline_bytes() -> anyhow::Result<()> {
    let upstream = SocketUpstream::start().await?;
    let directory = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    };
    let gateway = Gateway::new(config.clone()).await?;
    let token = configure(&gateway, &upstream, false).await?;
    let history_image = static_image_url();
    let new_image = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../fixtures/media/transparent.png"))
    );
    let first = send(&gateway, &token, json!([
        {"type": "function_call", "call_id": "read-image", "name": "read_image", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "read-image", "output": [
            {"type": "input_text", "text": "Screenshot"},
            {"type": "input_image", "image_url": history_image, "detail": "auto"}
        ]}
    ]), None).await?;
    assert_eq!(
        image_urls(&upstream.bodies.lock()[0]),
        [history_image.as_str()]
    );
    let parent = first["id"].as_str().expect("parent response").to_owned();
    // 持久化历史继续有效，但新 Gateway 不持有 store=false 响应所在的 WebSocket。
    gateway.shutdown().await;
    drop(gateway);
    let gateway = Gateway::new(config).await?;
    // A genuine failed upgrade cools the target on the restarted gateway,
    // independently of the persisted parent whose socket was lost.
    upstream
        .reject_upgrade
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = send_status(&gateway, &token, json!("independent failure"), None).await?;
    assert!(!status.is_success(), "{body}");
    assert!(
        !upstream
            .reject_upgrade
            .load(std::sync::atomic::Ordering::SeqCst),
        "expected the upstream upgrade rejection"
    );
    let statuses = gateway
        .admin()
        .get_model_target_statuses(CODEX_ROUTE)
        .await?;
    let state = serde_json::to_value(&statuses[0])?["state"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        matches!(state.as_str(), "cooling_down" | "half_open"),
        "{state}"
    );
    tokio::time::sleep(Duration::from_millis(COOLDOWN_MS as u64 + 150)).await;
    let statuses = gateway
        .admin()
        .get_model_target_statuses(CODEX_ROUTE)
        .await?;
    assert_eq!(serde_json::to_value(&statuses[0])?["state"], "half_open");
    let response = send(
        &gateway,
        &token,
        json!([{"role": "user", "content": [
            {"type": "input_text", "text": "Compare with this image."},
            {"type": "input_image", "image_url": new_image, "detail": "auto"}
        ]}]),
        Some(&parent),
    )
    .await?;
    assert_eq!(response["output"][0]["content"][0]["text"], "READY");
    {
        let sent = upstream.bodies.lock();
        assert_eq!(
            sent.len(),
            2,
            "socket miss must not send a continuation frame"
        );
        let replay = sent.last().expect("replayed request");
        assert!(
            replay.get("previous_response_id").is_none(),
            "a new socket must receive complete history"
        );
        assert_eq!(
            image_urls(replay),
            [history_image.as_str(), new_image.as_str()],
            "history and current images must retain their original inline bytes"
        );
    }
    let attempts = attempt_statuses(&gateway).await?;
    assert_eq!(attempts.len(), 3, "{attempts:?}");
    assert_eq!(attempts[0], ("completed".to_owned(), None));
    assert_eq!(attempts[1].0, "failed");
    assert_eq!(attempts[2], ("completed".to_owned(), None));
    let statuses = gateway
        .admin()
        .get_model_target_statuses(CODEX_ROUTE)
        .await?;
    assert_eq!(serde_json::to_value(&statuses[0])?["state"], "available");
    gateway.shutdown().await;
    Ok(())
}
