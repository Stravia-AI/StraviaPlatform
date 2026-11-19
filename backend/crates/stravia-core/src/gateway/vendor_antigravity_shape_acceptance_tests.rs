use super::*;
use crate::db::models::{CreateApiKey, CreateRoute, CreateTarget, RoutePurpose};
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
            purpose: RoutePurpose::Inference,
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
            transparent_injections: Default::default(),
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
        let expected_value = if name.eq_ignore_ascii_case("user-agent") {
            // 连接层夹具保留真实 1.5.8 抓包；请求身份已按 1.5.11 的构建值更新。
            "antigravity/cli/1.5.11 (aidev_client; os_type=linux; arch=amd64; cl=1024248524; auth_method=consumer)"
        } else {
            expected["value"].as_str().unwrap()
        };
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

// Capture actual request bodies over the same verified TLS transport used in
// production. State and checkpoint ownership remain entirely with Core.
async fn lifecycle_listener() -> anyhow::Result<(
    SocketAddr,
    Vec<u8>,
    tokio::sync::mpsc::UnboundedReceiver<Value>,
    tokio::task::JoinHandle<()>,
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
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let retries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mixed_fallbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            let failures = failures.clone();
            let retries = retries.clone();
            let mixed_fallbacks = mixed_fallbacks.clone();
            tokio::spawn(async move {
                let result: anyhow::Result<()> = async {
                    let mut stream = acceptor.accept(socket).await?;
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 4096];
                    let head_end = loop {
                        let n = stream.read(&mut buffer).await?;
                        anyhow::ensure!(n > 0, "incomplete request headers");
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
                    if !head.starts_with("POST /v1internal:streamGenerateContent") {
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                        return Ok(());
                    }
                    // The vendor transport streams HTTP/1 chunked bodies.
                    while !bytes[head_end..].windows(5).any(|w| w == b"0\r\n\r\n") {
                        let n = stream.read(&mut buffer).await?;
                        anyhow::ensure!(n > 0 && bytes.len() < 4 * 1024 * 1024, "incomplete request body");
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    let mut pos = head_end;
                    let mut body = Vec::new();
                    loop {
                        let end = pos + bytes[pos..].windows(2).position(|w| w == b"\r\n").context("chunk size")?;
                        let size = usize::from_str_radix(std::str::from_utf8(&bytes[pos..end])?.split(';').next().unwrap(), 16)?;
                        pos = end + 2;
                        if size == 0 { break; }
                        body.extend_from_slice(&bytes[pos..pos + size]);
                        pos += size + 2;
                    }
                    let request: Value = serde_json::from_slice(&body)?;
                    let text = request.to_string();
                    let tool_call = text.contains("tool-request") && !text.contains("functionResponse");
                    tx.send(request)?;
                    let fail = text.contains("failed-prefix") && failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    let retry = text.contains("retry-once") && retries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    let mixed = text.contains("mixed-fallback") && mixed_fallbacks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    if fail || retry || mixed {
                        let (status, code) = if fail { ("400 Bad Request", 400) } else { ("503 Service Unavailable", 503) };
                        let body = if fail {
                            "{\"error\":{\"code\":\"invalid_argument\",\"message\":\"invalid parameter in controlled local request\"}}".to_owned()
                        } else {
                            format!("{{\"error\":{{\"code\":{code},\"message\":\"controlled local failure\"}}}}")
                        };
                        stream.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
                        stream.shutdown().await?;
                        return Ok(());
                    }
                    let body = if tool_call {
                        "data: {\"response\":{\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"lookup\",\"args\":{}},\"thoughtSignature\":\"native-signature\"}]},\"finishReason\":\"STOP\"}]}}\n\n"
                    } else {
                        "data: {\"response\":{\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"local inference\"}]},\"finishReason\":\"STOP\"}]}}\n\n"
                    };
                    stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
                    stream.shutdown().await?;
                    Ok(())
                }.await;
                if let Err(error) = result {
                    panic!("lifecycle listener: {error:#}");
                }
            });
        }
    });
    Ok((address, certificate, rx, task))
}

async fn lifecycle_response(gateway: &Gateway, token: &str, input: Value) -> anyhow::Result<Value> {
    let response = with_real_time_limit(tokio::spawn(
        crate::proxy::server::create_router(gateway.clone()).oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))?,
        ),
    ))
    .await???;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    anyhow::ensure!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&body)
    );
    Ok(serde_json::from_slice(&body)?)
}

#[tokio::test]
async fn antigravity_lifecycle_survives_gateway_restart_and_isolates_branches() -> anyhow::Result<()>
{
    let (address, certificate, mut captured, server) = lifecycle_listener().await?;
    let assert_dispatch_id = |wire: &Value, steps: &str| {
        let labels = &wire["request"]["labels"];
        let id = wire["requestId"].as_str().expect("inference requestId");
        let parts: Vec<_> = id.split('/').collect();
        assert_eq!(parts.len(), 5, "unexpected inference requestId: {id}");
        assert_eq!(parts[0], "agent");
        assert_eq!(parts[1], labels["cascade_id"].as_str().unwrap());
        assert_eq!(parts[3], labels["trajectory_id"].as_str().unwrap());
        assert_eq!(parts[4], steps);
    };
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
        metadata: json!({"id":model,"name":model,"modalities":{"input":["text"],"output":["text"]},
            "antigravity":{"default":model,"variants":[{"id":model,"metadata":{"apiProvider":1,"modelProvider":4}}]}}),
        template_id: None,
    }).await?;
    let route = gateway.admin().create_model(serde_json::from_value(json!({"model_id":"agy-lifecycle", "purpose":"inference", "targets":[{"provider_id":provider.id,"model":model,"enabled":true,"target_retry_budget":0}]}))?).await?;
    let mut route_ids = vec![route.id];
    gateway
        .admin()
        .update_provider(
            &provider.id,
            crate::db::models::UpdateProvider {
                name: Some("synthetic antigravity primary".into()),
                ..Default::default()
            },
        )
        .await?;
    let fallback_provider = synthetic_oauth_provider(&gateway, "antigravity", false).await?;
    assert_ne!(provider.id, fallback_provider.id);
    let fallback_model = "gemini-fallback-model";
    gateway.admin().create_manual_provider_model(&fallback_provider.id, fallback_model, CreateManualProviderModel {
        metadata: json!({"id":fallback_model,"name":fallback_model,"modalities":{"input":["text"],"output":["text"]},
            "antigravity":{"default":fallback_model,"variants":[{"id":fallback_model,"metadata":{"apiProvider":1,"modelProvider":4}}]}}),
        template_id: None,
    }).await?;
    let fallback_route = gateway.admin().create_model(serde_json::from_value(json!({"model_id":"agy-mixed-fallback","purpose":"inference","targets":[
        {"provider_id":provider.id,"model":model,"enabled":true,"priority":100,"target_retry_budget":0,"target_cooldown_ms":0},
        {"provider_id":fallback_provider.id,"model":fallback_model,"enabled":true,"priority":0,"target_retry_budget":0,"target_cooldown_ms":0}
    ]}))?).await?;
    route_ids.push(fallback_route.id);
    for (model, route, api_provider) in [
        ("claude-lifecycle", "agy-claude", 26),
        ("gpt-lifecycle", "agy-gpt", 1),
    ] {
        gateway.admin().create_manual_provider_model(&provider.id, model, CreateManualProviderModel {
            metadata: json!({"id":model,"name":model,"modalities":{"input":["text"],"output":["text"]},
                "antigravity":{"default":model,"variants":[{"id":model,"metadata":{"apiProvider":api_provider,"modelProvider":1}}]}}),
            template_id: None,
        }).await?;
        let route = gateway.admin().create_model(serde_json::from_value(json!({"model_id":route,"purpose":"inference","targets":[{"provider_id":provider.id,"model":model,"enabled":true,"target_retry_budget":0}]}))?).await?;
        route_ids.push(route.id);
    }
    let retry_route = gateway.admin().create_model(serde_json::from_value(json!({"model_id":"agy-retry","purpose":"inference","targets":[{"provider_id":provider.id,"model":model,"enabled":true,"target_retry_budget":1,"target_cooldown_ms":0}]}))?).await?;
    route_ids.push(retry_route.id);
    let key = gateway.admin().create_api_key(serde_json::from_value(json!({"name":"lifecycle", "model_ids":route_ids, "transparent_injection_enabled":false}))?).await?;
    let root = lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-lifecycle","input":"first"}),
    )
    .await?;
    let root_wire = with_real_time_limit(captured.recv())
        .await?
        .context("root inference")?;
    assert_dispatch_id(&root_wire, "1");
    let root_labels = &root_wire["request"]["labels"];
    let trajectory = root_labels["trajectory_id"]
        .as_str()
        .context("trajectory")?
        .to_owned();
    assert_eq!(root_labels["request_id"], format!("{trajectory}-0"));
    assert_eq!(root_labels["last_step_index"], "0");
    assert_eq!(root_labels["used_non_gemini_model"], "false");
    let root_id = root["id"].as_str().context("root response id")?.to_owned();
    let first = lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-lifecycle","previous_response_id":root_id,"input":"second"}),
    )
    .await?;
    let first_wire = with_real_time_limit(captured.recv())
        .await?
        .context("first continuation")?;
    assert_dispatch_id(&first_wire, "3");
    assert_eq!(
        first_wire["request"]["labels"]["request_id"],
        format!("{trajectory}-1")
    );
    assert_eq!(first_wire["request"]["labels"]["last_step_index"], "2");
    assert_eq!(
        first_wire["request"]["labels"]["used_claude_conservative"],
        "false"
    );
    let first_id = first["id"]
        .as_str()
        .context("first response id")?
        .to_owned();
    drop(gateway);
    let pool = crate::db::init_pool(directory.path()).await?;
    let gateway = Gateway::from_storage_with_vendor_network(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::SqliteStorage::from_pool(pool)),
        local_vendor_network(address, &certificate)?,
    )
    .await?;
    lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-lifecycle","previous_response_id":first_id,"input":"third"}),
    )
    .await?;
    let resumed = with_real_time_limit(captured.recv())
        .await?
        .context("resumed inference")?;
    assert_eq!(
        resumed["request"]["labels"]["request_id"],
        format!("{trajectory}-2")
    );
    assert_eq!(resumed["request"]["labels"]["last_step_index"], "4");
    lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-lifecycle","previous_response_id":root_id,"input":"sibling"}),
    )
    .await?;
    let sibling = with_real_time_limit(captured.recv())
        .await?
        .context("sibling inference")?;
    assert_eq!(
        sibling["request"]["labels"]["request_id"],
        format!("{trajectory}-1")
    );
    assert_eq!(sibling["request"]["labels"]["last_step_index"], "2");
    assert_eq!(
        sibling["request"]["labels"]["used_non_gemini_model"],
        "false"
    );
    for (route, claude) in [("agy-claude", true), ("agy-gpt", false)] {
        let prior = lifecycle_response(
            &gateway,
            &key.token,
            json!({"model":route,"input":"other family"}),
        )
        .await?;
        let _ = with_real_time_limit(captured.recv())
            .await?
            .context("other family inference")?;
        let prior_id = prior["id"].as_str().context("other family response id")?;
        lifecycle_response(&gateway, &key.token, json!({"model":"agy-lifecycle","previous_response_id":prior_id,"input":"back to google"})).await?;
        let google = with_real_time_limit(captured.recv())
            .await?
            .context("google continuation")?;
        let labels = &google["request"]["labels"];
        assert_eq!(labels["used_claude"], claude.to_string());
        assert_eq!(labels["used_claude_conservative"], claude.to_string());
        assert_eq!(labels["used_non_gemini_model"], "true");
    }
    let tools =
        json!([{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]);
    let tool_parent = lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-lifecycle","input":"tool-request","tools":tools}),
    )
    .await?;
    let tool_wire = with_real_time_limit(captured.recv())
        .await?
        .context("tool generation")?;
    assert_dispatch_id(&tool_wire, "1");
    let tool_trajectory = tool_wire["request"]["labels"]["trajectory_id"]
        .as_str()
        .context("tool trajectory")?
        .to_owned();
    assert_eq!(
        tool_wire["request"]["labels"]["request_id"],
        format!("{tool_trajectory}-0")
    );
    let tool_id = tool_parent["output"]
        .as_array()
        .context("tool output")?
        .iter()
        .find(|item| item["type"] == "function_call")
        .and_then(|item| item["call_id"].as_str())
        .context("tool call id")?;
    let parent_id = tool_parent["id"].as_str().context("tool response id")?;
    lifecycle_response(&gateway, &key.token, json!({"model":"agy-lifecycle","previous_response_id":parent_id,"input":[{"type":"function_call_output","call_id":tool_id,"output":"found"}],"tools":tools})).await?;
    let tool_result = with_real_time_limit(captured.recv())
        .await?
        .context("tool continuation")?;
    assert_dispatch_id(&tool_result, "3");
    assert_eq!(
        tool_result["request"]["labels"]["request_id"],
        format!("{tool_trajectory}-1")
    );
    assert_eq!(tool_result["request"]["labels"]["last_step_index"], "2");
    assert!(
        tool_result["request"]["labels"]
            .get("last_execution_id")
            .is_none()
    );

    // A failed request has no response ID. Continue via the actual strict-prefix
    // admission path, without fabricating a parent response or assistant item.
    let failed = with_real_time_limit(tokio::spawn(
        crate::proxy::server::create_router(gateway.clone()).oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {}", key.token))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"agy-lifecycle","input":"failed-prefix"}).to_string(),
                ))?,
        ),
    ))
    .await???;
    assert!(!failed.status().is_success());
    let failed_wire = with_real_time_limit(captured.recv())
        .await?
        .context("failed inference")?;
    let failed_labels = &failed_wire["request"]["labels"];
    let failed_trajectory = failed_labels["trajectory_id"]
        .as_str()
        .context("failed trajectory")?;
    assert_eq!(
        failed_labels["request_id"],
        format!("{failed_trajectory}-0")
    );
    lifecycle_response(&gateway, &key.token, json!({"model":"agy-lifecycle","input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"failed-prefix"}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"after failure"}]}
    ]})).await.context("failed-prefix recovery request")?;
    let recovered = with_real_time_limit(captured.recv())
        .await?
        .context("failed-prefix continuation")?;
    assert_eq!(
        recovered["request"]["labels"]["request_id"],
        format!("{failed_trajectory}-1")
    );
    assert_eq!(recovered["request"]["labels"]["last_step_index"], "2");

    // The first upstream attempt is 503 and the target's own retry budget
    // permits one retry of the same host Model Turn.
    let retried = lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-retry","input":"retry-once"}),
    )
    .await?;
    let retry_first = with_real_time_limit(captured.recv())
        .await?
        .context("first retry attempt")?;
    let retry_second = with_real_time_limit(captured.recv())
        .await?
        .context("second retry attempt")?;
    assert_dispatch_id(&retry_first, "1");
    assert_dispatch_id(&retry_second, "1");
    let first_labels = &retry_first["request"]["labels"];
    let second_labels = &retry_second["request"]["labels"];
    assert_eq!(first_labels["request_id"], second_labels["request_id"]);
    assert_eq!(
        first_labels["trajectory_id"],
        second_labels["trajectory_id"]
    );
    assert_eq!(first_labels["last_step_index"], "0");
    assert_eq!(second_labels["last_step_index"], "0");
    let retry_trajectory = first_labels["trajectory_id"]
        .as_str()
        .context("retry trajectory")?;
    let retry_id = retried["id"].as_str().context("retried response id")?;
    lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-retry","previous_response_id":retry_id,"input":"after retry"}),
    )
    .await?;
    let after_retry = with_real_time_limit(captured.recv())
        .await?
        .context("after retry inference")?;
    assert_eq!(
        after_retry["request"]["labels"]["request_id"],
        format!("{retry_trajectory}-1")
    );
    assert_eq!(after_retry["request"]["labels"]["last_step_index"], "2");

    // The first provider fails MT0; a distinct provider succeeds that same
    // Model Turn. A plain 0→1 cursor would hide this fallback from provider A.
    let mixed = lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-mixed-fallback","input":"mixed-fallback"}),
    )
    .await
    .context("mixed-provider fallback request")?;
    let mixed_first = with_real_time_limit(captured.recv())
        .await?
        .context("mixed first attempt")?;
    let mixed_second = with_real_time_limit(captured.recv())
        .await?
        .context("mixed fallback attempt")?;
    assert_eq!(mixed_first["model"], model);
    assert_eq!(mixed_second["model"], fallback_model);
    let first_labels = &mixed_first["request"]["labels"];
    let trajectory = first_labels["trajectory_id"]
        .as_str()
        .context("mixed initial trajectory")?;
    assert_eq!(first_labels["request_id"], format!("{trajectory}-0"));
    assert_eq!(first_labels["last_step_index"], "0");
    assert_eq!(first_labels["used_claude_conservative"], "false");
    assert_eq!(first_labels["used_non_gemini_model"], "false");
    let assert_mixed = |request: &Value| {
        let request_id = request["requestId"].as_str().unwrap();
        let fallback = uuid::Uuid::parse_str(request_id.strip_prefix("agent/").unwrap()).unwrap();
        assert_eq!(fallback.get_version_num(), 4);
        let labels = &request["request"]["labels"];
        assert!(
            labels.get("request_id").is_none(),
            "mixed provider generation count must be unknown: {labels}"
        );
        assert!(
            labels.get("last_step_index").is_none(),
            "mixed provider step count must be unknown: {labels}"
        );
        assert_eq!(labels["used_claude"], "false");
        assert_eq!(labels["used_claude_conservative"], "true");
        assert_eq!(labels["used_non_gemini_model"], "true");
    };
    assert_mixed(&mixed_second);
    let mixed_id = mixed["id"].as_str().context("mixed response id")?;
    lifecycle_response(
        &gateway,
        &key.token,
        json!({"model":"agy-lifecycle","previous_response_id":mixed_id,"input":"back to original provider"}),
    ).await.context("mixed-provider return request")?;
    let mixed_return = with_real_time_limit(captured.recv())
        .await?
        .context("return to first provider")?;
    assert_eq!(mixed_return["model"], model);
    assert_mixed(&mixed_return);
    server.abort();
    Ok(())
}
