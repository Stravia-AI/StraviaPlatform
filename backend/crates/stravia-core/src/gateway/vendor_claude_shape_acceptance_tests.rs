use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

const MODEL: &str = "claude-sonnet-4-5";
const TOKEN: &str = "synthetic-claude-access";
const SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_local\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"local Claude answer\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

// The session is published through the same path as synthetic_oauth_provider;
// Claude is not one of that helper's declared vendors.
async fn claude_provider(gateway: &Gateway) -> anyhow::Result<crate::db::models::Provider> {
    let session = gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: "claude-code".into(),
                channel: "oauth".into(),
                provider_id: None,
                base_url: "https://api.anthropic.com".into(),
                protocol: Some("anthropic-messages".into()),
                options: Default::default(),
                credentials: Default::default(),
                use_proxy: false,
            },
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "http://localhost:1457/auth/callback".into(),
                listener_port: None,
                fallback_reason: None,
            },
        )
        .await?;
    let bundle = crate::auth::CredentialBundle {
        access_token: Some(TOKEN.into()),
        refresh_token: Some("synthetic-refresh".into()),
        expires_at: Some("2099-01-01T00:00:00Z".into()),
        resource_url: Some("https://api.anthropic.com".into()),
        raw: json!({"account_uuid":"00000000-0000-4000-8000-0000000000aa"}),
        ..Default::default()
    };
    let runtime = {
        let mut sessions = gateway.auth_sessions.write().await;
        let record = sessions
            .get_mut(&session.session_id)
            .context("synthetic Claude session")?;
        record.status = crate::auth::AuthSessionStatus::Ready.as_str().into();
        record.result_json = Some(serde_json::to_string(&bundle)?);
        record.expires_at = bundle.expires_at.clone();
        record
            .vendor_runtime
            .clone()
            .context("synthetic Claude runtime")?
    };
    let (_, operation, _) = gateway.vendor_plugins.acquire("claude-code")?;
    let publication = operation.publication_fence(
        runtime.cancellation.clone(),
        stravia_runtime_contract::Deadline::fixed(
            std::time::Instant::now() + Duration::from_secs(600),
        ),
    );
    drop(operation);
    *runtime.publication.lock().await = Some(publication);
    gateway
        .admin()
        .create_provider_with_oauth_session(
            &session.session_id,
            CreateProvider {
                name: Some("synthetic Claude".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "claude-code".into(),
                    channel: "oauth".into(),
                    protocol: Some("anthropic-messages".into()),
                    base_url: "https://api.anthropic.com".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: Default::default(),
                use_proxy: false,
            },
        )
        .await
}

// Capture before TLS consumes the socket; the same connection is then decrypted
// and serves the inference response, so the hello belongs to the actual call.
async fn capture_server() -> anyhow::Result<(
    LocalTlsServer,
    tokio::sync::oneshot::Receiver<(Vec<u8>, String, Vec<u8>)>,
)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["api.anthropic.com".to_string()])?;
    let certificate_der = cert.der().to_vec();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert.der().clone()], key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (capture_tx, capture_rx) = tokio::sync::oneshot::channel();
    let (_request_tx, requests) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let mut wire = vec![0; 65540];
        let size = loop {
            let n = socket.peek(&mut wire).await?;
            anyhow::ensure!(n != 0, "connection closed before ClientHello");
            if n >= 5 {
                let total = 5 + u16::from_be_bytes([wire[3], wire[4]]) as usize;
                if n >= total {
                    break total;
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        wire.truncate(size);
        let mut socket = acceptor.accept(socket).await?;
        let mut bytes = Vec::new();
        let mut scratch = [0; 4096];
        let (end, length) = loop {
            let n = socket.read(&mut scratch).await?;
            anyhow::ensure!(n != 0, "connection closed before request head");
            bytes.extend_from_slice(&scratch[..n]);
            if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = std::str::from_utf8(&bytes[..pos])?;
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .context("request content length")?
                    .1
                    .trim()
                    .parse::<usize>()?;
                break (pos + 4, length);
            }
            anyhow::ensure!(bytes.len() < 1024 * 1024, "request head too large");
        };
        while bytes.len() < end + length {
            let n = socket.read(&mut scratch).await?;
            anyhow::ensure!(n != 0, "connection closed before request body");
            bytes.extend_from_slice(&scratch[..n]);
        }
        let head = String::from_utf8(bytes[..end].to_vec())?;
        let body = bytes[end..end + length].to_vec();
        capture_tx
            .send((wire, head, body))
            .map_err(|_| anyhow::anyhow!("capture receiver dropped"))?;
        write_local_response(
            &mut socket,
            LocalResponse {
                status: "200 OK",
                content_type: "text/event-stream",
                body: SSE.as_bytes().to_vec(),
                release: None,
            },
        )
        .await?;
        Ok(Vec::new())
    });
    Ok((
        LocalTlsServer {
            address,
            certificate_der,
            requests,
            task,
        },
        capture_rx,
    ))
}

#[tokio::test]
async fn claude_inference_uses_genuine_tls_shape_and_ordered_http1() -> anyhow::Result<()> {
    let (server, capture) = capture_server().await?;
    let storage_directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(storage_directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage = Arc::new(crate::storage::SqliteStorage::from_pool(pool));
    let (_directory, gateway) = gateway_for_storage(&server, &["claude-code"], storage).await?;
    let provider = claude_provider(&gateway).await?;
    gateway.admin().create_manual_provider_model(
        &provider.id, MODEL, crate::provider_models::CreateManualProviderModel {
            metadata: json!({"id": MODEL, "name": MODEL, "modalities": {"input": ["text"], "output": ["text"]}}),
            template_id: None,
        },
    ).await?;
    let route = gateway
        .admin()
        .create_model(crate::db::models::CreateRoute {
            model_id: "local-claude".into(),
            display_name: None,
            output_modality: crate::db::models::RouteOutputModality::Text,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: provider.id,
                model: MODEL.into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Claude acceptance".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![route.id.into()],
            spend_limit: None,
        })
        .await?;
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        crate::proxy::server::create_router(gateway.clone()).oneshot(
            Request::post("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .header("authorization", format!("Bearer {}", key.token))
                .body(Body::from(
                    json!({"model":"local-claude","max_tokens":64,
                    "messages":[{"role":"user","content":"Say hello"}]})
                    .to_string(),
                ))?,
        ),
    )
    .await??;
    let status = response.status();
    let body = tokio::time::timeout(
        Duration::from_secs(30),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await??;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let result: Value = serde_json::from_slice(&body)?;
    assert_eq!(result["content"][0]["text"], "local Claude answer");
    assert_eq!(result["stop_reason"], "end_turn");
    let (wire, head, upstream_body) =
        tokio::time::timeout(Duration::from_secs(10), capture).await??;
    crate::plugin::network::shape::tests::assert_wire_matches_fixture(
        "claude-code",
        &wire,
        // Both the gateway request and this default-interactive fixture use a cold handshake.
        include_str!("../plugin/network/shape/fixtures/claude-code.json"),
        false,
    );
    let mut lines = head.lines();
    assert_eq!(lines.next(), Some("POST /v1/messages?beta=true HTTP/1.1"));
    let fields: Vec<(&str, &str)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name, value.trim()))
        .collect();
    let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
    let expected = vec![
        "Accept",
        "Authorization",
        "Content-Type",
        "User-Agent",
        "X-Claude-Code-Session-Id",
        "X-Stainless-Arch",
        "X-Stainless-Lang",
        "X-Stainless-OS",
        "X-Stainless-Package-Version",
        "X-Stainless-Retry-Count",
        "X-Stainless-Runtime",
        "X-Stainless-Runtime-Version",
        "X-Stainless-Timeout",
        "anthropic-beta",
        "anthropic-dangerous-direct-browser-access",
        "anthropic-version",
        "x-app",
        "x-claude-code-prompt-id",
        "x-claude-code-request-class",
        "x-claude-code-sandy-plover",
        "x-client-request-id",
        "Connection",
        "Host",
        "Accept-Encoding",
        "Content-Length",
    ];
    assert_eq!(names, expected, "actual head: {head}");
    let fixture: Value = serde_json::from_str(include_str!(
        "../plugin/network/shape/fixtures/claude-code.json"
    ))?;
    let captured: Vec<&str> = fixture["h1"]["headers"]
        .as_array()
        .context("fixture HTTP headers")?
        .iter()
        .map(|header| header["name"].as_str().expect("header name"))
        .collect();
    assert_eq!(names, captured, "interactive OAuth header order: {head}");
    let accepts: Vec<_> = fields
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("accept"))
        .collect();
    assert_eq!(
        accepts.len(),
        1,
        "host must not inject an Accept field: {head}"
    );
    assert_eq!(accepts[0].1, "application/json");
    assert!(
        fields
            .iter()
            .any(|(name, value)| *name == "Authorization" && *value == format!("Bearer {TOKEN}"))
    );
    assert!(fields.iter().any(|(name, value)| {
        *name == "User-Agent" && *value == "claude-cli/2.1.319 (external, cli)"
    }));
    assert!(fields.iter().any(|(name, value)| {
        *name == "Accept-Encoding" && *value == "gzip, deflate, br, zstd"
    }));
    let sent: Value = serde_json::from_slice(&upstream_body)?;
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["messages"][0]["content"][0]["text"], "Say hello");
    tokio::time::timeout(Duration::from_secs(10), server.task).await???;
    Ok(())
}
