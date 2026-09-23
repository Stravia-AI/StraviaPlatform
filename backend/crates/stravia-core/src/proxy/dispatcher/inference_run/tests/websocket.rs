use super::*;

#[tokio::test]
async fn precommit_connection_limit_falls_back_to_http_on_the_same_target() {
    for budget in [0, 1] {
        let (base_url, websocket_requests, http_requests) = serve_connection_limit_fallback().await;
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let gateway = Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        configure_route_with_protocol(
            &gateway,
            "connection-limit-fallback",
            &[base_url],
            "openai",
            "open-responses",
        )
        .await;
        set_target_retry_budget(&gateway, "connection-limit-fallback", budget).await;

        let response = execute_protocol_request(
            gateway.clone(),
            "connection-limit-fallback",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
            false,
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("fallback response body");
        if budget == 0 {
            assert_ne!(status, StatusCode::OK);
            assert_eq!(websocket_requests.load(Ordering::SeqCst), 1);
            assert_eq!(http_requests.load(Ordering::SeqCst), 0);
            let blocked = execute_protocol_request(
                gateway,
                "connection-limit-fallback",
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                "/v1/chat/completions",
                false,
            )
            .await;
            assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(websocket_requests.load(Ordering::SeqCst), 1);
            assert_eq!(http_requests.load(Ordering::SeqCst), 0);
            continue;
        }
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(
            String::from_utf8_lossy(&body).contains("http fallback"),
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(websocket_requests.load(Ordering::SeqCst), 1);
        assert_eq!(http_requests.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn stale_reused_websocket_falls_back_then_retries_websocket() {
    let (base_url, websocket_connections, http_requests) = serve_stale_websocket_fallback().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "stale-websocket-fallback";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "open-responses").await;
    set_target_retry_budget(&gateway, model, 1).await;

    let first = execute_protocol_request_with_session(
        gateway.clone(),
        model,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        true,
        "stale-websocket-session",
    )
    .await;
    let first_status = first.status();
    let first_body = to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first response body");
    assert_eq!(
        first_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&first_body)
    );
    assert!(String::from_utf8_lossy(&first_body).contains("websocket first"));

    let second = execute_protocol_request_with_session(
        gateway.clone(),
        model,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        true,
        "stale-websocket-session",
    )
    .await;
    let second_status = second.status();
    let second_body = to_bytes(second.into_body(), usize::MAX)
        .await
        .expect("fallback response body");

    assert_eq!(
        second_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&second_body)
    );
    assert!(
        String::from_utf8_lossy(&second_body).contains("http fallback"),
        "{}",
        String::from_utf8_lossy(&second_body)
    );
    assert_eq!(websocket_connections.load(Ordering::SeqCst), 1);
    assert_eq!(http_requests.load(Ordering::SeqCst), 1);

    let third = execute_protocol_request_with_session(
        gateway,
        model,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        true,
        "stale-websocket-session",
    )
    .await;
    let third_status = third.status();
    let third_body = to_bytes(third.into_body(), usize::MAX)
        .await
        .expect("WebSocket recovery response body");

    assert_eq!(
        third_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&third_body)
    );
    assert!(
        String::from_utf8_lossy(&third_body).contains("websocket first"),
        "{}",
        String::from_utf8_lossy(&third_body)
    );
    assert_eq!(websocket_connections.load(Ordering::SeqCst), 2);
    assert_eq!(http_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reusable_websocket_does_not_cross_effective_proxy_changes() {
    let (base_url, upstream_connections, upstream_requests) =
        serve_responses_websocket_sequence(vec!["first direct", "second direct", "stale direct"])
            .await;
    let (proxy_url, proxy_connections) = serve_rejecting_http_proxy().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    admin
        .set_setting("proxy_enabled", "false")
        .await
        .expect("disable Gateway proxy");
    admin
        .set_setting("proxy_url", &proxy_url)
        .await
        .expect("configure Gateway proxy");
    admin
        .set_setting("proxy_force_http1", "false")
        .await
        .expect("configure Gateway proxy HTTP version");
    let model = "websocket-proxy-scope";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "open-responses").await;
    let provider_id = gateway
        .storage
        .routes()
        .list()
        .await
        .expect("Routes")
        .into_iter()
        .find(|route| route.model_id == model)
        .expect("configured Route")
        .targets[0]
        .provider_id()
        .clone();
    gateway
        .admin()
        .update_provider(
            &provider_id,
            crate::db::models::UpdateProvider {
                use_proxy: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("enable provider proxy preference");

    for expected in ["first direct", "second direct"] {
        let response = execute_protocol_request_with_session(
            gateway.clone(),
            model,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
            true,
            "proxy-scope-session",
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("direct WebSocket response body");
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(
            String::from_utf8_lossy(&body).contains(expected),
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(upstream_connections.load(Ordering::SeqCst), 1);
    assert_eq!(upstream_requests.lock().len(), 2);
    assert_eq!(proxy_connections.load(Ordering::SeqCst), 0);

    gateway
        .admin()
        .set_setting("proxy_enabled", "true")
        .await
        .expect("enable Gateway proxy");
    let response = execute_protocol_request_with_session(
        gateway,
        model,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        true,
        "proxy-scope-session",
    )
    .await;

    assert_ne!(response.status(), StatusCode::OK);
    assert!(proxy_connections.load(Ordering::SeqCst) > 0);
    assert_eq!(upstream_connections.load(Ordering::SeqCst), 1);
    assert_eq!(upstream_requests.lock().len(), 2);
}

async fn serve_rejecting_http_proxy() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind rejecting proxy");
    let address = listener.local_addr().expect("proxy address");
    let connections = Arc::new(AtomicUsize::new(0));
    let observed = connections.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.expect("accept proxy request");
            observed.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    (format!("http://{address}"), connections)
}

#[tokio::test]
async fn unsupported_websocket_handshake_falls_back_before_sending_a_request() {
    let (base_url, calls) = serve_sse_sequence(vec![
        openai_responses_sse("unused handshake body"),
        openai_responses_sse("http fallback"),
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
        "unsupported-websocket-fallback",
        &[base_url],
        "openai",
        "open-responses",
    )
    .await;
    set_target_retry_budget(&gateway, "unsupported-websocket-fallback", 1).await;

    let response = execute_protocol_request(
        gateway,
        "unsupported-websocket-fallback",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("fallback response body");
    assert!(
        String::from_utf8_lossy(&body).contains("http fallback"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn generation_ingresses_preserve_unary_and_stream_contracts_over_upstream_websocket() {
    let responses = [
        "ws-openai-unary",
        "ws-openai-stream",
        "ws-responses-unary",
        "ws-responses-stream",
        "ws-anthropic-unary",
        "ws-anthropic-stream",
        "ws-gemini-unary",
        "ws-gemini-stream",
    ];
    let (base_url, connections, requests) =
        serve_responses_websocket_sequence(responses.to_vec()).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let model = "responses-websocket-protocol-matrix";
    configure_route_with_protocol(&gateway, model, &[base_url], "openai", "openai-compatible")
        .await;
    let protocols = [
        (
            "openai",
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
            "\"object\":\"chat.completion\"",
            "\"object\":\"chat.completion.chunk\"",
        ),
        (
            "responses",
            OPEN_RESPONSES_2026_04_24,
            "/v1/responses",
            "\"object\":\"response\"",
            "event: response.created",
        ),
        (
            "anthropic",
            ANTHROPIC_MESSAGES_2023_06_01,
            "/v1/messages",
            "\"type\":\"message\"",
            "event: message_start",
        ),
        (
            "gemini",
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            "/v1beta/models/provider-model:generateContent",
            "\"candidates\"",
            "\"candidates\"",
        ),
    ];

    for (index, (name, ingress, path, unary_marker, stream_marker)) in
        protocols.into_iter().enumerate()
    {
        let unary = execute_protocol_request(gateway.clone(), model, ingress, path, false).await;
        assert_eq!(unary.status(), StatusCode::OK, "{name} unary status");
        let unary_body = to_bytes(unary.into_body(), usize::MAX)
            .await
            .expect("unary WebSocket body");
        let unary_body = String::from_utf8_lossy(&unary_body);
        assert!(
            unary_body.contains(responses[index * 2]) && unary_body.contains(unary_marker),
            "{name} unary WebSocket contract: {unary_body}"
        );

        let stream = execute_protocol_request(gateway.clone(), model, ingress, path, true).await;
        assert_eq!(stream.status(), StatusCode::OK, "{name} stream status");
        let stream_body = to_bytes(stream.into_body(), usize::MAX)
            .await
            .expect("stream WebSocket body");
        let stream_body = String::from_utf8_lossy(&stream_body);
        assert!(
            stream_body.contains(responses[index * 2 + 1]) && stream_body.contains(stream_marker),
            "{name} stream WebSocket contract: {stream_body}"
        );
    }

    assert_eq!(connections.load(Ordering::SeqCst), responses.len());
    let requests = requests.lock();
    assert_eq!(requests.len(), responses.len());
    assert!(
        requests
            .iter()
            .all(|request| request["type"] == "response.create")
    );
}
