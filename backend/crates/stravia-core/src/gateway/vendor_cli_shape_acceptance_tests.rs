use super::*;
use crate::db::models::{CreateApiKey, CreateRoute, CreateTarget, RouteOutputModality};
use crate::provider_models::CreateManualProviderModel;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

#[path = "vendor_command_shape_acceptance_tests.rs"]
mod command_shape_acceptance_tests;
#[path = "vendor_devin_shape_acceptance_tests.rs"]
mod devin_shape_acceptance_tests;

struct CapturedInference {
    hello: Vec<u8>,
    head: String,
    body: Vec<u8>,
}

struct CapturedH2 {
    hello: Vec<u8>,
    fields: Vec<(String, String)>,
    settings: Vec<(u16, u32)>,
    windows: Vec<(u32, u32)>,
    body: Vec<u8>,
}

async fn write_h2_frame(
    socket: &mut (impl AsyncWrite + Unpin),
    kind: u8,
    flags: u8,
    stream: u32,
    payload: &[u8],
) -> anyhow::Result<()> {
    let len = payload.len();
    let id = stream.to_be_bytes();
    socket
        .write_all(&[
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
            kind,
            flags,
            id[0],
            id[1],
            id[2],
            id[3],
        ])
        .await?;
    socket.write_all(payload).await?;
    socket.flush().await?;
    Ok(())
}

async fn h2_capture_server(
    hostname: &str,
    response: Vec<u8>,
) -> anyhow::Result<(LocalTlsServer, tokio::sync::oneshot::Receiver<CapturedH2>)> {
    // Exercise the native Accept-Encoding contract, not only identity responses.
    let mut gzip = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    gzip.write_all(&response).await?;
    gzip.shutdown().await?;
    let response = gzip.into_inner();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![hostname.to_owned()])?;
    let certificate_der = cert.der().to_vec();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert.der().clone()], key)?;
    config.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (capture_tx, capture_rx) = tokio::sync::oneshot::channel();
    let (_request_tx, requests) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await?;
        let mut record = [0; 5];
        tcp.read_exact(&mut record).await?;
        anyhow::ensure!(record[0] == 22, "expected ClientHello record");
        let len = usize::from(u16::from_be_bytes([record[3], record[4]]));
        anyhow::ensure!((4..=16384).contains(&len), "invalid ClientHello record");
        let mut hello = record.to_vec();
        hello.resize(5 + len, 0);
        tcp.read_exact(&mut hello[5..]).await?;
        let (reader, writer) = tcp.into_split();
        let replay = std::io::Cursor::new(hello.clone()).chain(reader);
        let mut socket = acceptor.accept(tokio::io::join(replay, writer)).await?;
        let mut preface = [0; 24];
        socket.read_exact(&mut preface).await?;
        anyhow::ensure!(
            &preface == b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
            "invalid HTTP/2 preface"
        );
        write_h2_frame(&mut socket, 4, 0, 0, &[]).await?;
        let mut settings = Vec::new();
        let mut windows = Vec::new();
        let mut block = bytes::BytesMut::new();
        let mut body = Vec::new();
        let mut request_stream = 0;
        let mut continuing = false;
        let mut first_frame = true;
        loop {
            let mut header = [0; 9];
            socket.read_exact(&mut header).await?;
            let length = (usize::from(header[0]) << 16)
                | (usize::from(header[1]) << 8)
                | usize::from(header[2]);
            anyhow::ensure!(length <= 16384, "oversized HTTP/2 frame");
            let (kind, flags) = (header[3], header[4]);
            let stream = u32::from_be_bytes(header[5..9].try_into()?) & 0x7fff_ffff;
            if first_frame {
                anyhow::ensure!(
                    (kind, flags, stream) == (4, 0, 0),
                    "expected initial SETTINGS"
                );
                first_frame = false;
            }
            if continuing {
                anyhow::ensure!(
                    kind == 9 && stream == request_stream,
                    "interleaved CONTINUATION"
                );
            }
            let mut payload = vec![0; length];
            socket.read_exact(&mut payload).await?;
            match kind {
                4 if flags & 1 == 0 => {
                    anyhow::ensure!(stream == 0 && length % 6 == 0, "invalid SETTINGS");
                    for setting in payload.as_chunks::<6>().0 {
                        settings.push((
                            u16::from_be_bytes(setting[..2].try_into()?),
                            u32::from_be_bytes(setting[2..].try_into()?),
                        ));
                    }
                    write_h2_frame(&mut socket, 4, 1, 0, &[]).await?;
                }
                4 => anyhow::ensure!(stream == 0 && length == 0, "invalid SETTINGS ACK"),
                8 => {
                    anyhow::ensure!(length == 4, "invalid WINDOW_UPDATE");
                    windows.push((
                        stream,
                        u32::from_be_bytes(payload.as_slice().try_into()?) & 0x7fff_ffff,
                    ));
                }
                1 | 9 => {
                    anyhow::ensure!(flags & 0x28 == 0, "unexpected HEADERS padding or priority");
                    if kind == 1 {
                        anyhow::ensure!(
                            request_stream == 0 && stream != 0,
                            "unexpected extra request"
                        );
                        request_stream = stream;
                    }
                    anyhow::ensure!(
                        block.len() + payload.len() <= 65536,
                        "oversized HPACK block"
                    );
                    block.extend_from_slice(&payload);
                    continuing = flags & 4 == 0;
                    if flags & 1 != 0 {
                        anyhow::ensure!(!continuing, "incomplete header-only request");
                        break;
                    }
                }
                0 => {
                    anyhow::ensure!(
                        stream == request_stream && flags & 8 == 0,
                        "invalid request DATA"
                    );
                    anyhow::ensure!(
                        body.len() + payload.len() <= 4 * 1024 * 1024,
                        "oversized inference body"
                    );
                    body.extend_from_slice(&payload);
                    if flags & 1 != 0 {
                        break;
                    }
                }
                _ => anyhow::bail!("unexpected client frame {kind}"),
            }
        }
        let mut fields = Vec::new();
        rama_http_types::proto::h2::hpack::Decoder::new(4096)
            .decode(&mut std::io::Cursor::new(&mut block), |field| {
                fields.push((
                    field.name().as_slice().to_vec(),
                    field.value_slice().to_vec(),
                ));
                std::ops::ControlFlow::Continue(())
            })
            .map_err(|err| anyhow::anyhow!("decode received HPACK: {err:?}"))?;
        let fields = fields
            .into_iter()
            .map(|(name, value)| Ok((String::from_utf8(name)?, String::from_utf8(value)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        // HPACK indexed :status=200; literal content-type (31) and content-encoding (26).
        let mut response_head = vec![0x88, 0x0f, 0x10, 17];
        response_head.extend_from_slice(b"text/event-stream");
        response_head.extend_from_slice(&[0x0f, 0x0b, 4]);
        response_head.extend_from_slice(b"gzip");
        write_h2_frame(&mut socket, 1, 4, request_stream, &response_head).await?;
        for (index, chunk) in response.chunks(16384).enumerate() {
            let last = (index + 1) * 16384 >= response.len();
            write_h2_frame(&mut socket, 0, u8::from(last), request_stream, chunk).await?;
        }
        capture_tx
            .send(CapturedH2 {
                hello,
                fields,
                settings,
                windows,
                body,
            })
            .map_err(|_| anyhow::anyhow!("H2 capture receiver dropped"))?;
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

fn responses_success(model: &str) -> Vec<u8> {
    let item = json!({"type":"message","id":"msg_wire","status":"completed","role":"assistant",
        "content":[{"type":"output_text","text":"wire accepted","annotations":[]}]});
    let completed = json!({"id":"resp_wire","object":"response","created_at":1,"completed_at":2,
        "status":"completed","model":model,"output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},
        "error":null,"incomplete_details":null,"previous_response_id":null,"instructions":null,
        "tools":[],"tool_choice":"auto","truncation":"disabled","parallel_tool_calls":true,
        "text":{"format":{"type":"text"}},"top_p":1.0,"presence_penalty":0.0,
        "frequency_penalty":0.0,"top_logprobs":0,"temperature":1.0,"reasoning":null,
        "max_output_tokens":null,"max_tool_calls":null,"store":false,"background":false,
        "service_tier":"default","metadata":{},"safety_identifier":null,"prompt_cache_key":null});
    let mut created = completed.clone();
    created["status"] = json!("in_progress");
    created["completed_at"] = Value::Null;
    created["output"] = json!([]);
    created["usage"] = Value::Null;
    let events = [
        json!({"type":"response.created","response":created}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_wire","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"msg_wire","part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_wire","delta":"wire accepted"}),
        json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"msg_wire","text":"wire accepted"}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":"msg_wire","part":item["content"][0]}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":completed}),
    ];
    events
        .into_iter()
        .enumerate()
        .map(|(index, mut event)| {
            event["sequence_number"] = json!(index);
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>()
        .into_bytes()
}

#[tokio::test]
async fn grok_real_wasm_inference_matches_interactive_tls_and_http2() -> anyhow::Result<()> {
    let (mut server, mut capture) =
        h2_capture_server("api.x.ai", responses_success("grok-4.6")).await?;
    let (_directory, gateway) = inference_gateway(&server, "xai-grok").await?;
    let provider = gateway
        .storage
        .providers()
        .create(crate::db::models::CreateProviderRecord {
            name: "Grok wire fixture".into(),
            vendor: Some("xai-grok".into()),
            protocol: "open-responses".into(),
            base_url: "https://api.x.ai/v1".into(),
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
            crate::db::models::UpsertOAuthCredential {
                driver_key: "xai-grok".into(),
                scheme: "Bearer".into(),
                access_token: "local-fixture".into(),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                meta: Some(json!({"user_id":"local-test"}).to_string()),
                ..Default::default()
            },
        )
        .await?;
    let key = route_key(&gateway, &provider.id, "grok-4.6", "grok-wire").await?;
    let response = with_real_time_limit(
        crate::proxy::server::create_router(gateway).oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"grok-wire","input":"reply wire accepted","stream":true})
                        .to_string(),
                ))?,
        ),
    )
    .await??;
    let status = response.status();
    let response_body = to_bytes(response.into_body(), 1024 * 1024).await?;
    if !status.is_success() {
        if let Err(tokio::sync::oneshot::error::TryRecvError::Closed) = capture.try_recv() {
            match with_real_time_limit(&mut server.task).await? {
                Ok(Err(error)) => anyhow::bail!(
                    "{status}: {}; H2 fixture failed: {error:#}",
                    String::from_utf8_lossy(&response_body)
                ),
                Err(error) => anyhow::bail!(
                    "{status}: {}; H2 fixture task failed: {error}",
                    String::from_utf8_lossy(&response_body)
                ),
                Ok(Ok(_)) => {}
            }
        }
        anyhow::bail!("{status}: {}", String::from_utf8_lossy(&response_body));
    }
    assert!(String::from_utf8_lossy(&response_body).contains("wire accepted"));
    let captured = with_real_time_limit(capture).await??;
    let fixture = include_str!("../plugin/network/shape/fixtures/grok-interactive.json");
    crate::plugin::network::shape::tests::assert_wire_matches_fixture(
        "grok",
        &captured.hello,
        fixture,
        true,
    );
    let expected: Value = serde_json::from_str(fixture)?;
    let settings = expected["h2"]["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["type"] == 4 && frame["flags"] == 0)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&captured.settings)?,
        settings["settings"]
    );
    assert_eq!(captured.windows, vec![(0, 5177345)]);
    let fields = expected["h2"]["headers"].as_array().unwrap();
    assert_eq!(captured.fields.len(), fields.len(), "{:?}", captured.fields);
    for ((name, value), field) in captured.fields.iter().zip(fields) {
        assert_eq!(name, field["name"].as_str().unwrap());
        match name.as_str() {
            ":authority" => assert_eq!(value, "api.x.ai"),
            "authorization" => assert_eq!(value, "Bearer local-fixture"),
            "content-length" => assert_eq!(value.parse::<usize>()?, captured.body.len()),
            "traceparent" => {
                let parts: Vec<_> = value.split('-').collect();
                assert_eq!(parts.len(), 4);
                assert_eq!(parts[0], "00");
                assert_eq!(parts[3], "01");
                assert_eq!(parts[1].len(), 32);
                assert_eq!(parts[2].len(), 16);
                assert!(
                    parts[1..3]
                        .iter()
                        .all(|part| part.bytes().all(|byte| byte.is_ascii_hexdigit()))
                );
            }
            _ if field["value"] == "<dynamic>" => {
                uuid::Uuid::parse_str(value)?;
            }
            _ => assert_eq!(value, field["value"].as_str().unwrap(), "{name}"),
        }
    }
    let request: Value = serde_json::from_slice(&captured.body)?;
    assert_eq!(request["model"], "grok-4.6");
    assert_eq!(request["stream"], true);
    assert_eq!(request["store"], false);
    assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
    with_real_time_limit(server.task).await???;
    Ok(())
}

async fn h1_capture_server(
    hostname: &str,
    response: LocalResponse,
) -> anyhow::Result<(
    LocalTlsServer,
    tokio::sync::oneshot::Receiver<CapturedInference>,
)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![hostname.to_owned()])?;
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
        let (mut tcp, _) = listener.accept().await?;
        // Read a complete record rather than polling a partially arrived TCP packet.
        // The tested host ClientHello fits one TLS record, including hybrid shares.
        let mut record = [0; 5];
        tcp.read_exact(&mut record).await?;
        anyhow::ensure!(record[0] == 22, "expected ClientHello record");
        let len = usize::from(u16::from_be_bytes([record[3], record[4]]));
        anyhow::ensure!(
            (4..=16384).contains(&len),
            "invalid ClientHello record length"
        );
        let mut hello = record.to_vec();
        hello.resize(5 + len, 0);
        tcp.read_exact(&mut hello[5..]).await?;
        anyhow::ensure!(
            hello[5] == 1
                && u32::from_be_bytes([0, hello[6], hello[7], hello[8]]) as usize + 4 == len,
            "expected a complete ClientHello handshake"
        );
        let (reader, writer) = tcp.into_split();
        let replay = std::io::Cursor::new(hello.clone()).chain(reader);
        let mut socket = acceptor.accept(tokio::io::join(replay, writer)).await?;
        let mut received = Vec::new();
        let mut buffer = [0; 4096];
        let (head_end, body_len) = loop {
            let count = socket.read(&mut buffer).await?;
            anyhow::ensure!(
                count > 0 && received.len() < 4 * 1024 * 1024,
                "incomplete inference request"
            );
            received.extend_from_slice(&buffer[..count]);
            if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let head = std::str::from_utf8(&received[..end])?;
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .context("inference Content-Length")?
                    .1
                    .trim()
                    .parse::<usize>()?;
                anyhow::ensure!(length <= 4 * 1024 * 1024, "oversized inference body");
                break (end + 4, length);
            }
        };
        while received.len() < head_end + body_len {
            let count = socket.read(&mut buffer).await?;
            anyhow::ensure!(count > 0, "truncated inference body");
            received.extend_from_slice(&buffer[..count]);
        }
        let capture = CapturedInference {
            hello,
            head: String::from_utf8(received[..head_end].to_vec())?,
            body: received[head_end..head_end + body_len].to_vec(),
        };
        write_local_response(&mut socket, response).await?;
        capture_tx
            .send(capture)
            .map_err(|_| anyhow::anyhow!("inference capture receiver dropped"))?;
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

async fn inference_gateway(
    server: &LocalTlsServer,
    vendor: &str,
) -> anyhow::Result<(tempfile::TempDir, Gateway)> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let gateway = Gateway::from_storage_with_vendor_network(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::SqliteStorage::from_pool(pool)),
        local_vendor_network(server.address, &server.certificate_der)?,
    )
    .await?;
    crate::plugin::test_support::install_distributed_vendor(&gateway, vendor).await?;
    Ok((directory, gateway))
}

async fn route_key(
    gateway: &Gateway,
    provider_id: &str,
    model: &str,
    route_name: &str,
) -> anyhow::Result<String> {
    gateway.admin().create_manual_provider_model(provider_id, model, CreateManualProviderModel {
        metadata: json!({"id":model, "name":model, "modalities":{"input":["text"],"output":["text"]}}),
        template_id: None,
    }).await?;
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: route_name.into(),
            display_name: None,
            output_modality: RouteOutputModality::Text,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider_id.into(),
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
    Ok(gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: route_name.into(),
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
        .await?
        .token)
}

fn assert_ordered_head_matches_fixture(head: &str, fixture: &Value) {
    let actual: Vec<_> = head
        .lines()
        .skip(1)
        .filter(|line| !line.is_empty())
        .map(|line| line.split_once(": ").expect("HTTP field"))
        .collect();
    let expected = fixture["h1"]["headers"]
        .as_array()
        .expect("ordered captured HTTP fields");
    assert_eq!(actual.len(), expected.len(), "{head}");
    for ((name, value), field) in actual.iter().zip(expected) {
        assert_eq!(*name, field["name"].as_str().unwrap(), "{head}");
        let expected_value = field["value"].as_str().unwrap();
        if expected_value != "<redacted>" {
            assert_eq!(*value, expected_value, "field {name}");
        }
    }
}

#[tokio::test]
async fn opencode_real_wasm_inference_matches_interactive_tls_and_http() -> anyhow::Result<()> {
    let sse = concat!(
        "data: {\"id\":\"local-cli\",\"object\":\"chat.completion.chunk\",\"model\":\"big-pickle\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"local CLI answer\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"local-cli\",\"object\":\"chat.completion.chunk\",\"model\":\"big-pickle\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let (server, capture) = h1_capture_server(
        "opencode.ai",
        LocalResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: sse.as_bytes().to_vec(),
            release: None,
        },
    )
    .await?;
    let (_directory, gateway) = inference_gateway(&server, "opencode-free").await?;
    let provider = create_vendor_provider(
        &gateway,
        "interactive OpenCode",
        "opencode-free",
        "default",
        "openai-compatible/chat-completions/v1",
        "https://opencode.ai/inference/v1",
        ProviderCredentialInput::None,
    )
    .await?;
    let key = route_key(&gateway, &provider.id, "big-pickle", "opencode-interactive").await?;
    let response = with_real_time_limit(crate::proxy::server::create_router(gateway).oneshot(
        Request::post("/v1/responses").header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json").body(Body::from(json!({
                "model":"opencode-interactive", "input":"Reply with local CLI answer", "max_output_tokens":16000, "stream":true
            }).to_string()))?,
    )).await??;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    anyhow::ensure!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(String::from_utf8_lossy(&body).contains("local CLI answer"));
    let captured = with_real_time_limit(capture).await??;
    let fixture = include_str!("../plugin/network/shape/fixtures/opencode-interactive.json");
    crate::plugin::network::shape::tests::assert_wire_server_name(&captured.hello, "opencode.ai");
    crate::plugin::network::shape::tests::assert_wire_matches_fixture(
        "bun-1-3-14",
        &captured.hello,
        fixture,
        false,
    );
    let expected: Value = serde_json::from_str(fixture)?;
    assert_ordered_head_matches_fixture(&captured.head, &expected);
    assert!(
        captured
            .head
            .starts_with("POST /inference/v1/chat/completions HTTP/1.1\r\n")
    );
    let request: Value = serde_json::from_slice(&captured.body)?;
    for field in [
        "model",
        "stream",
        "tool_choice",
        "stream_options",
        "max_tokens",
    ] {
        assert_eq!(request[field], expected["request"][field], "{field}");
    }
    with_real_time_limit(server.task).await???;
    Ok(())
}
