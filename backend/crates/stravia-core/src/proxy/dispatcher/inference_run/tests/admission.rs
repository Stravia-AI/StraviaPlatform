use super::*;

// 保持运行时可运行，防止暂停时钟在数据库 I/O 等待时自动跳至后台定时器。
struct ManualClock(tokio::task::JoinHandle<()>);
impl ManualClock {
    fn pause() -> Self {
        tokio::time::pause();
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }
}
impl Drop for ManualClock {
    fn drop(&mut self) {
        self.0.abort();
        tokio::time::resume();
    }
}

fn admission_run(gateway: Gateway, headers: HeaderMap, model: &str) -> RunInput {
    RunInput {
        executor: Arc::clone(&gateway.model_turn),
        gateway,
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
            std::time::Duration::from_secs(300),
        ),
    }
}

async fn assert_rpm_rejection(response: axum::response::Response, retry_after: u64) {
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers()[header::RETRY_AFTER],
        retry_after.to_string()
    );
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["type"], "STRAVIA_RPM_LIMIT");
}

#[tokio::test]
async fn principal_rpm_rolling_window_expires_at_exact_sixty_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .hook(Arc::new(RuntimeShortCircuitHook))
    .build()
    .await
    .unwrap();
    let headers = authorized_headers(&gateway).await;
    set_rpm_limit(&gateway, 2).await;
    let clock = ManualClock::pause();
    let run = || {
        admission_run(
            gateway.clone(),
            headers.clone(),
            "__lifecycle_short_circuit__",
        )
    };
    assert_eq!(execute(run()).await.status(), StatusCode::OK);
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    assert_eq!(execute(run()).await.status(), StatusCode::OK);
    assert_rpm_rejection(execute(run()).await, 30).await;
    tokio::time::advance(std::time::Duration::from_millis(29_999)).await;
    assert_rpm_rejection(execute(run()).await, 1).await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    assert_eq!(execute(run()).await.status(), StatusCode::OK);
    assert_rpm_rejection(execute(run()).await, 30).await;
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    assert_eq!(execute(run()).await.status(), StatusCode::OK);
    assert_rpm_rejection(execute(run()).await, 30).await;
    drop(clock);
    close_test_gateway(gateway, dir).await;
}

#[tokio::test]
async fn principal_rpm_upstream_failover_reuses_one_root_admission() {
    let (failed_url, failed_calls) = serve_openai_response(
        500,
        serde_json::json!({"error": {"message": "upstream failed"}}),
    )
    .await;
    let (fallback_url, fallback_calls) =
        serve_openai_response(200, openai_response("fallback answer")).await;
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .await
    .unwrap();
    configure_route(&gateway, "rpm-failover", &[failed_url, fallback_url]).await;
    set_target_retry_budget(&gateway, "rpm-failover", 0).await;
    let headers = authorized_headers(&gateway).await;
    set_rpm_limit(&gateway, 1).await;
    let clock = ManualClock::pause();
    let response = execute(admission_run(
        gateway.clone(),
        headers.clone(),
        "rpm-failover",
    ))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("fallback answer"));
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    assert_rpm_rejection(
        execute(admission_run(gateway.clone(), headers, "rpm-failover")).await,
        60,
    )
    .await;
    drop(clock);
    close_test_gateway(gateway, dir).await;
}

#[tokio::test]
async fn principal_rpm_atomic_burst_isolates_keys_and_retains_cancelled_admissions() {
    let dir = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .hook(Arc::new(BlockingRequestHook {
        entered: entered.clone(),
        release: release.clone(),
    }))
    .build()
    .await
    .unwrap();
    let headers = authorized_headers(&gateway).await;
    set_rpm_limit(&gateway, 2).await;
    let other_key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "isolated-principal".into(),
            rpm_limit: Some(1),
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_media_understanding: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: Vec::new(),
        })
        .await
        .unwrap();
    let clock = ManualClock::pause();
    let mut attempts = Vec::new();
    let first_entered = entered.notified();
    let second_entered = entered.notified();
    tokio::pin!(first_entered, second_entered);
    first_entered.as_mut().enable();
    second_entered.as_mut().enable();
    for _ in 0..8 {
        attempts.push(tokio::spawn(execute(admission_run(
            gateway.clone(),
            headers.clone(),
            "blocked",
        ))));
    }
    // 只有两次成功入口触发 Hook；其余请求立即结束，不增加窗口。
    first_entered.await;
    second_entered.await;
    release.notify_waiters();
    let mut accepted = 0;
    for attempt in attempts {
        let response = attempt.await.unwrap();
        if response.status() == StatusCode::OK {
            accepted += 1;
        } else {
            assert_rpm_rejection(response, 60).await;
        }
    }
    assert_eq!(accepted, 2);
    assert_rpm_rejection(
        execute(admission_run(gateway.clone(), headers.clone(), "blocked")).await,
        60,
    )
    .await;
    let other = tokio::spawn(execute(admission_run(
        gateway.clone(),
        bearer_headers(&other_key.token),
        "blocked",
    )));
    entered.notified().await;
    other.abort();
    assert!(other.await.unwrap_err().is_cancelled());
    assert_rpm_rejection(
        execute(admission_run(
            gateway.clone(),
            bearer_headers(&other_key.token),
            "blocked",
        ))
        .await,
        60,
    )
    .await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    let renewed = tokio::spawn(execute(admission_run(gateway.clone(), headers, "blocked")));
    entered.notified().await;
    release.notify_one();
    assert_eq!(renewed.await.unwrap().status(), StatusCode::OK);
    drop(clock);
    close_test_gateway(gateway, dir).await;
}

#[tokio::test]
async fn principal_rpm_failed_roots_and_limit_updates_preserve_window() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .hook(Arc::new(RuntimeShortCircuitHook))
    .build()
    .await
    .unwrap();
    let headers = authorized_headers(&gateway).await;
    let clock = ManualClock::pause();
    let run = |model| admission_run(gateway.clone(), headers.clone(), model);
    assert_eq!(
        execute(run("__lifecycle_short_circuit__")).await.status(),
        StatusCode::OK
    );
    // 从不限启用时不追溯没有记录的请求。
    set_rpm_limit(&gateway, 2).await;
    assert_eq!(
        execute(run("__lifecycle_reject__")).await.status().as_u16(),
        451
    );
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    assert_eq!(
        execute(run("__lifecycle_short_circuit__")).await.status(),
        StatusCode::OK
    );
    set_rpm_limit(&gateway, 1).await;
    assert_rpm_rejection(execute(run("__lifecycle_short_circuit__")).await, 60).await;
    tokio::time::advance(std::time::Duration::from_secs(50)).await;
    assert_rpm_rejection(execute(run("__lifecycle_short_circuit__")).await, 10).await;
    set_rpm_limit(&gateway, 2).await;
    assert_eq!(
        execute(run("__lifecycle_short_circuit__")).await.status(),
        StatusCode::OK
    );
    drop(clock);
    close_test_gateway(gateway, dir).await;
}

#[tokio::test]
async fn principal_rpm_expiry_does_not_wait_for_active_stream_delivery() {
    let first_event = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "active-rpm-stream", "model": "provider-model", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "first"}, "finish_reason": null}]
        })
    );
    let (url, calls, release) = serve_gated_sse(first_event, "data: [DONE]\n\n".into()).await;
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .hook(Arc::new(RuntimeShortCircuitHook))
    .build()
    .await
    .unwrap();
    configure_route(&gateway, "rpm-stream", &[url]).await;
    let headers = authorized_headers(&gateway).await;
    set_rpm_limit(&gateway, 1).await;
    let clock = ManualClock::pause();
    let mut input = admission_run(gateway.clone(), headers.clone(), "rpm-stream");
    input.request.stream.enabled = true;
    let response = execute(input).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut first = Vec::new();
    while !String::from_utf8_lossy(&first).contains("first") {
        first.extend_from_slice(&stream.next().await.unwrap().unwrap());
    }
    let run = || {
        admission_run(
            gateway.clone(),
            headers.clone(),
            "__lifecycle_short_circuit__",
        )
    };
    assert_rpm_rejection(execute(run()).await, 60).await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    assert_eq!(execute(run()).await.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    while let Some(chunk) = stream.next().await {
        chunk.unwrap();
    }
    assert_rpm_rejection(execute(run()).await, 60).await;
    drop(clock);
    close_test_gateway(gateway, dir).await;
}

#[tokio::test]
async fn principal_rpm_counts_each_generation_on_reused_inbound_websocket() {
    use futures::SinkExt;
    use reqwest_websocket::Upgrade as _;
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .hook(Arc::new(RuntimeShortCircuitHook))
    .build()
    .await
    .unwrap();
    let headers = authorized_headers(&gateway).await;
    set_rpm_limit(&gateway, 1).await;
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
    let request = serde_json::json!({"type": "response.create", "model": "__lifecycle_short_circuit__", "input": "hello"}).to_string();
    socket
        .send(reqwest_websocket::Message::Text(request.clone()))
        .await
        .unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        if let reqwest_websocket::Message::Text(text) = message {
            let event: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_ne!(event["type"], "error", "{event}");
            if event["type"] == "response.completed" {
                break;
            }
        }
    }
    socket
        .send(reqwest_websocket::Message::Text(request))
        .await
        .unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        if let reqwest_websocket::Message::Text(text) = message {
            let event: serde_json::Value = serde_json::from_str(&text).unwrap();
            if event["type"] == "error" {
                assert_eq!(event["status"], 429);
                break;
            }
            assert_ne!(event["type"], "response.completed");
        }
    }
    drop(socket);
    server.abort();
    let _ = server.await;
    close_test_gateway(gateway, dir).await;
}
