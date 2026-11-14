use super::*;
use axum::http::{HeaderMap, StatusCode};
use futures::SinkExt;
use reqwest_websocket::Upgrade as _;
use serde_json::{Value, json};

#[derive(Clone)]
struct CountWarmupHooks(Arc<AtomicUsize>);

impl stravia_runtime_contract::hook::Hook for CountWarmupHooks {
    fn descriptor(&self) -> stravia_runtime_contract::hook::HookDescriptor {
        stravia_runtime_contract::hook::HookDescriptor {
            id: stravia_runtime_contract::hook::HookId::new("warmup-hook-counter"),
            event_kinds: vec![stravia_runtime_contract::hook::EventKind::Request],
            requires_full_context: false,
            max_buffered_bytes: 0,
            max_delayed_events: 0,
        }
    }

    fn create_session(
        &self,
        _context: &stravia_runtime_contract::hook::SessionContext,
    ) -> Box<dyn stravia_runtime_contract::hook::HookSession> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl stravia_runtime_contract::hook::HookSession for CountWarmupHooks {
    async fn handle(
        &mut self,
        event: stravia_runtime_contract::hook::HookEvent<'_>,
    ) -> Result<stravia_runtime_contract::hook::ActionBatch, String> {
        use stravia_runtime_contract::hook::{ActionBatch, HookAction, HookEvent, RequestPatch};
        use stravia_runtime_contract::protocol::ir::MessageContent;

        self.0.fetch_add(1, Ordering::SeqCst);
        if let HookEvent::Request {
            current,
            input_start,
            ..
        } = event
        {
            for (index, item) in current.items[input_start..].iter().enumerate() {
                if matches!(&item.content, MessageContent::Text(text) if text.as_str() == "retained warmup context")
                {
                    let mut processed = item.clone();
                    processed.content =
                        MessageContent::Text("processed warmup context".to_owned().into());
                    return Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                        RequestPatch::SpliceInput {
                            range: index..index + 1,
                            items: vec![processed],
                        },
                    ))));
                }
            }
        }
        Ok(Default::default())
    }
}

async fn receive_warmup_completion(socket: &mut reqwest_websocket::WebSocket) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let reqwest_websocket::Message::Text(text) = socket
                .next()
                .await
                .expect("open client socket")
                .expect("client frame")
            {
                let event: Value = serde_json::from_str(&text).expect("response event");
                assert_ne!(event["type"], "error", "{event}");
                if event["type"] == "response.completed" {
                    return event["response"].clone();
                }
            }
        }
    })
    .await
    .expect("response completed before test deadline")
}

#[tokio::test]
async fn responses_warmup_never_generates_and_continues_with_retained_input() {
    for empty in [false, true] {
        let (upstream, calls, requests) =
            serve_openai_sequence_with_requests(vec![openai_response("real answer")]).await;
        let dir = tempfile::tempdir().unwrap();
        let request_hooks = Arc::new(AtomicUsize::new(0));
        let hook = CountWarmupHooks(Arc::clone(&request_hooks));
        let gateway = Gateway::builder(crate::config::GatewayConfig {
            data_dir: dir.path().into(),
            ..Default::default()
        })
        .hook(Arc::new(hook))
        .build()
        .await
        .unwrap();
        configure_route(&gateway, "warmup-model", &[upstream]).await;
        let headers = authorized_headers(&gateway).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = crate::proxy::server::create_router(gateway.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut socket = reqwest::Client::new()
            .get(format!("http://{address}/v1/responses"))
            .headers(headers)
            .upgrade()
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        let input = if empty {
            json!([])
        } else {
            json!([{"type":"message", "role":"developer", "content":"retained warmup context"}])
        };
        socket
            .send(reqwest_websocket::Message::Text(
                json!({
                    "type":"response.create", "model":"warmup-model", "input":input,
                    "instructions":"retained instructions", "generate":false, "store":false,
                "tools":[{"type":"web_search"}]
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let warmup = receive_warmup_completion(&mut socket).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "warmup must not invoke the upstream"
        );
        assert_eq!(
            request_hooks.load(Ordering::SeqCst),
            0,
            "warmup must not run request hooks"
        );
        assert_eq!(warmup["output"], json!([]));
        assert!(warmup["usage"].is_null());
        let parent = warmup["id"].as_str().expect("continuable warmup id");
        socket
            .send(reqwest_websocket::Message::Text(
                json!({
                    "type":"response.create", "previous_response_id":parent,
                    "input":[{"role":"user", "content":"actual user request"}], "tools":[]
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let completed = receive_warmup_completion(&mut socket).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(request_hooks.load(Ordering::SeqCst), 1);
        assert!(completed.to_string().contains("real answer"), "{completed}");
        let captured = requests.lock();
        let body: Value =
            serde_json::from_str(captured[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        let messages = body["messages"].to_string();
        assert!(messages.contains("actual user request"), "{messages}");
        assert!(messages.contains("retained instructions"), "{messages}");
        assert_eq!(
            messages.contains("processed warmup context"),
            !empty,
            "{messages}"
        );
        assert!(!messages.contains("retained warmup context"), "{messages}");
        assert!(body.get("generate").is_none());
        drop(captured);
        drop(socket);
        server.abort();
        close_test_gateway(gateway, dir).await;
    }
}

async fn post_warmup(
    gateway: &Gateway,
    headers: &HeaderMap,
    body: Value,
) -> axum::response::Response {
    use tower::ServiceExt;

    let mut request = axum::http::Request::post("/v1/responses")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    request.headers_mut().extend(headers.clone());
    crate::proxy::server::create_router(gateway.clone())
        .oneshot(request)
        .await
        .unwrap()
}

#[tokio::test]
async fn responses_warmup_preserves_authentication_model_grants_and_parent_validation() {
    let (upstream, calls, _) =
        serve_openai_sequence_with_requests(vec![openai_response("unused")]).await;
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .build()
    .await
    .unwrap();
    configure_route(&gateway, "warmup-allowed", &[upstream.clone()]).await;
    let headers = authorized_headers(&gateway).await;
    configure_route(&gateway, "warmup-forbidden", &[upstream]).await;
    let body = json!({"model":"warmup-allowed", "generate":false});
    assert_eq!(
        post_warmup(&gateway, &HeaderMap::new(), body.clone())
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
    );
    for (model, status) in [
        ("warmup-forbidden", StatusCode::FORBIDDEN),
        ("warmup-missing", StatusCode::NOT_FOUND),
    ] {
        let mut denied = body.clone();
        denied["model"] = json!(model);
        assert_eq!(
            post_warmup(&gateway, &headers, denied).await.status(),
            status
        );
    }
    let mut missing_parent = body.clone();
    missing_parent["previous_response_id"] = json!("missing-response");
    assert_eq!(
        post_warmup(&gateway, &headers, missing_parent)
            .await
            .status(),
        StatusCode::BAD_REQUEST,
    );
    let accepted = post_warmup(&gateway, &headers, body).await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(accepted.into_body(), usize::MAX)
        .await
        .unwrap();
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["output"], json!([]));
    assert!(response["usage"].is_null());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    close_test_gateway(gateway, dir).await;
}
