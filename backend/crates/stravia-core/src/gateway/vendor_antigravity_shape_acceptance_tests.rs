use super::*;
use crate::db::models::{CreateApiKey, CreateRoute, CreateTarget, RouteOutputModality};
use crate::provider_models::CreateManualProviderModel;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

// This listener records the ClientHello before rustls consumes it and the actual
// HTTP/1 head after verified TLS, rather than observing plugin-side arguments.
async fn inference_listener() -> anyhow::Result<(
    SocketAddr,
    Vec<u8>,
    tokio::task::JoinHandle<anyhow::Result<(Vec<u8>, String)>>,
)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["daily-cloudcode-pa.googleapis.com".into()])?;
    let certificate = cert.der().to_vec();
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
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let mut peek = vec![0; 65540];
        let hello = loop {
            socket.readable().await?;
            let count = socket.peek(&mut peek).await?;
            anyhow::ensure!(count != 0, "TLS connection closed before ClientHello");
            if count >= 5 {
                let length = 5 + u16::from_be_bytes([peek[3], peek[4]]) as usize;
                if count >= length {
                    break peek[..length].to_vec();
                }
            }
            tokio::task::yield_now().await;
        };
        let mut stream = acceptor.accept(socket).await?;
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        let end = loop {
            let count = stream.read(&mut buffer).await?;
            anyhow::ensure!(
                count > 0 && request.len() < 1024 * 1024,
                "incomplete inference request"
            );
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let head = String::from_utf8(request[..end].to_vec())?;
        // Drain the real chunked request before closing the TLS connection.
        while !request[end..]
            .windows(5)
            .any(|window| window == b"0\r\n\r\n")
        {
            let count = stream.read(&mut buffer).await?;
            anyhow::ensure!(
                count > 0 && request.len() < 4 * 1024 * 1024,
                "incomplete chunked inference body"
            );
            request.extend_from_slice(&buffer[..count]);
        }
        let body = "data: {\"response\":{\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"local inference\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":8,\"totalTokenCount\":13}}}\n\n";
        stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
        stream.shutdown().await?;
        Ok((hello, head))
    });
    Ok((address, certificate, task))
}

#[tokio::test]
async fn antigravity_real_wasm_inference_matches_inference_capture() -> anyhow::Result<()> {
    let (address, certificate, listener) = inference_listener().await?;
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let gateway = Gateway::from_storage_with_vendor_network(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::SqliteStorage::from_pool(pool)),
        local_vendor_network(address, &certificate)?,
    )
    .await?;
    crate::plugin::test_support::install_distributed_vendor(&gateway, "antigravity").await?;
    let provider = synthetic_oauth_provider(&gateway, "antigravity", false).await?;
    let model = "gemini-2.5-pro";
    gateway.admin().create_manual_provider_model(&provider.id, model, CreateManualProviderModel {
        metadata: json!({"id": model, "name": model, "modalities": {"input": ["text"], "output": ["text"]}}), template_id: None,
    }).await?;
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "antigravity-shape-test".into(),
            display_name: None,
            output_modality: RouteOutputModality::Text,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: model.into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: Some(0),
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    let key = gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "shape acceptance".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![route.id.into()],
            spend_limit: None,
        })
        .await?;
    let response = with_real_time_limit(
        crate::proxy::server::create_router(gateway).oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {}", key.token))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": "antigravity-shape-test", "input": "hello"}).to_string(),
                ))?,
        ),
    )
    .await??;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    assert!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(String::from_utf8_lossy(&body).contains("local inference"));
    let (hello, head) = with_real_time_limit(listener).await???;
    let fixture = include_str!("../plugin/network/shape/fixtures/antigravity-inference.json");
    crate::plugin::network::shape::tests::assert_wire_matches_fixture(
        "antigravity",
        &hello,
        fixture,
        false,
    );
    let sample: Value = serde_json::from_str(fixture)?;
    assert_eq!(sample["provenance"]["mode"], "interactive");
    assert_eq!(sample["provenance"]["model"], "gemini-3.5-flash-low");
    assert_eq!(sample["body_pattern"]["synthetic_prompt_present"], true);
    let mut lines = head.split("\r\n");
    assert_eq!(
        lines.next(),
        Some("POST /v1internal:streamGenerateContent?alt=sse HTTP/1.1")
    );
    let fields: Vec<_> = lines
        .filter(|line| !line.is_empty())
        .map(|line| line.split_once(": ").expect("HTTP field"))
        .collect();
    let expected = sample["h1"]["headers"]
        .as_array()
        .context("fixture fields")?;
    assert_eq!(fields.len(), expected.len());
    for ((name, value), expected) in fields.iter().zip(expected) {
        assert_eq!(*name, expected["name"].as_str().unwrap());
        let expected_value = expected["value"].as_str().unwrap();
        if expected_value != "<redacted>" {
            assert_eq!(*value, expected_value);
        }
    }
    assert!(
        !fields
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("accept"))
    );
    assert_eq!(
        fields
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .unwrap()
            .1,
        "Bearer synthetic-old-access"
    );
    Ok(())
}
