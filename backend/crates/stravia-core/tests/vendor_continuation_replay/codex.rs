use super::*;
use axum::extract::ws::{Message, WebSocketUpgrade};
use base64::Engine;

const CODEX_MODEL: &str = "gpt-6-astra";
const CODEX_ROUTE: &str = "codex-media-replay";

struct SocketUpstream {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}

impl SocketUpstream {
    async fn start() -> anyhow::Result<Self> {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = bodies.clone();
        let router = Router::new().route(
            "/responses",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let captured = captured.clone();
                async move {
                    upgrade.on_upgrade(move |mut socket| async move {
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

async fn configure(gateway: &Gateway, upstream: &SocketUpstream) -> anyhow::Result<String> {
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
        metadata: json!({"id": CODEX_MODEL, "name": CODEX_MODEL, "modalities": {"input": ["text", "image"], "output": ["text"]}}),
        template_id: None,
    }).await?;
    let route = gateway.admin().create_model(serde_json::from_value(json!({
        "model_id": CODEX_ROUTE, "output_modality": "text",
        "targets": [{"provider_id": provider.id, "model": CODEX_MODEL, "enabled": true, "target_retry_budget": 0}]
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
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(body)
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

#[tokio::test]
async fn socket_miss_replays_history_and_new_images_as_inline_bytes() -> anyhow::Result<()> {
    let upstream = SocketUpstream::start().await?;
    let directory = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    };
    let gateway = Gateway::new(config.clone()).await?;
    let token = configure(&gateway, &upstream).await?;
    let history_image = format!(
        "data:image/webp;base64,{}",
        base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../fixtures/media/static.webp"))
    );
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
    assert_eq!(attempt_statuses(&gateway).await?, completed(2));
    gateway.shutdown().await;
    Ok(())
}
