use super::*;

// Startup and inference are accepted on separate real TLS connections.  The
// inference ClientHello is replayed into rustls so the HTTP request and TLS
// fingerprint being checked necessarily belong to the same socket.
async fn command_capture_server() -> anyhow::Result<(
    LocalTlsServer,
    tokio::sync::oneshot::Receiver<Vec<CapturedInference>>,
)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["api.commandcode.ai".to_owned()])?;
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
        let mut captures = Vec::new();
        for _ in 0..3 {
            let (mut tcp, _) = listener.accept().await?;
            let mut record = [0; 5];
            tcp.read_exact(&mut record).await?;
            anyhow::ensure!(record[0] == 22, "expected ClientHello record");
            let len = usize::from(u16::from_be_bytes([record[3], record[4]]));
            anyhow::ensure!((4..=16384).contains(&len), "invalid ClientHello length");
            let mut hello = record.to_vec();
            hello.resize(5 + len, 0);
            tcp.read_exact(&mut hello[5..]).await?;
            anyhow::ensure!(
                hello[5] == 1
                    && u32::from_be_bytes([0, hello[6], hello[7], hello[8]]) as usize + 4 == len,
                "expected complete ClientHello"
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
                    "incomplete request"
                );
                received.extend_from_slice(&buffer[..count]);
                if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let head = std::str::from_utf8(&received[..end])?;
                    let length = head
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .context("request Content-Length")?
                        .1
                        .trim()
                        .parse::<usize>()?;
                    anyhow::ensure!(length <= 4 * 1024 * 1024, "oversized request");
                    break (end + 4, length);
                }
            };
            while received.len() < head_end + body_len {
                let count = socket.read(&mut buffer).await?;
                anyhow::ensure!(count > 0, "truncated request body");
                received.extend_from_slice(&buffer[..count]);
            }
            let head = String::from_utf8(received[..head_end].to_vec())?;
            let inference = head.starts_with("POST /alpha/generate HTTP/1.1\r\n");
            let response = if inference {
                LocalResponse {
                    status: "200 OK", content_type: "application/x-ndjson",
                    body: concat!(
                        "{\"type\":\"text-delta\",\"text\":\"wire accepted\"}\n",
                        "{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":1,\"outputTokens\":2}}\n"
                    ).as_bytes().to_vec(), release: None,
                }
            } else {
                LocalResponse::json(json!({}))
            };
            captures.push(CapturedInference {
                hello,
                head,
                body: received[head_end..head_end + body_len].to_vec(),
            });
            write_local_response(&mut socket, response).await?;
        }
        capture_tx
            .send(captures)
            .map_err(|_| anyhow::anyhow!("capture receiver dropped"))?;
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
async fn command_code_real_wasm_inference_matches_interactive_tls_and_http() -> anyhow::Result<()> {
    let (server, capture) = command_capture_server().await?;
    let (_directory, gateway) = inference_gateway(&server, "command-code").await?;
    let provider = create_vendor_provider(
        &gateway,
        "interactive Command Code",
        "command-code",
        "default",
        "command-code",
        "https://api.commandcode.ai",
        ProviderCredentialInput::Fields {
            values: BTreeMap::from([("apiKey".into(), json!("local-command-key"))]),
        },
    )
    .await?;
    let model = "deepseek/deepseek-v4-flash";
    let key = route_key(&gateway, &provider.id, model, "command-wire").await?;
    let response = with_real_time_limit(
        crate::proxy::server::create_router(gateway).oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"command-wire", "input":"Reply wire accepted"}).to_string(),
                ))?,
        ),
    )
    .await??;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    anyhow::ensure!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&body)
    );
    let result: Value = serde_json::from_slice(&body)?;
    assert!(result["output"].as_array().unwrap().iter().any(|item| {
        item["content"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part["type"] == "output_text" && part["text"] == "wire accepted")
        })
    }));

    let captures = with_real_time_limit(capture).await??;
    let paths: std::collections::BTreeSet<_> = captures
        .iter()
        .map(|capture| capture.head.lines().next().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "POST /alpha/fingerprint/record HTTP/1.1",
            "POST /alpha/lifecycle-events HTTP/1.1",
            "POST /alpha/generate HTTP/1.1"
        ]
        .into_iter()
        .collect()
    );
    let lifecycle = captures
        .iter()
        .find(|capture| capture.head.starts_with("POST /alpha/lifecycle-events "))
        .context("lifecycle request")?;
    let lifecycle: Value = serde_json::from_slice(&lifecycle.body)?;
    assert_eq!(lifecycle["eventType"], "cli_session_exists");
    let captured = captures
        .iter()
        .find(|capture| capture.head.starts_with("POST /alpha/generate "))
        .context("inference request")?;
    let fixture = include_str!("../plugin/network/shape/fixtures/command-code-interactive.json");
    let repeat =
        include_str!("../plugin/network/shape/fixtures/command-code-interactive.repeat.json");
    for fixture in [fixture, repeat] {
        crate::plugin::network::shape::tests::assert_wire_matches_fixture(
            "command-code",
            &captured.hello,
            fixture,
            false,
        );
        let expected: Value = serde_json::from_str(fixture)?;
        let actual: Vec<_> = captured
            .head
            .lines()
            .skip(1)
            .filter(|line| !line.is_empty())
            .map(|line| line.split_once(": ").expect("HTTP field"))
            .collect();
        let fields = expected["h1"]["headers"].as_array().unwrap();
        assert_eq!(actual.len(), fields.len(), "{}", captured.head);
        let request: Value = serde_json::from_slice(&captured.body)?;
        for ((name, value), field) in actual.iter().zip(fields) {
            assert_eq!(*name, field["name"].as_str().unwrap(), "{}", captured.head);
            match *name {
                "host" => assert_eq!(*value, "api.commandcode.ai"),
                "x-project-slug" => {
                    let directory = request["config"]["workingDir"]
                        .as_str()
                        .context("project directory")?;
                    let slug = directory
                        .split(|character: char| !character.is_ascii_alphanumeric())
                        .filter(|part| !part.is_empty())
                        .map(str::to_ascii_lowercase)
                        .collect::<Vec<_>>()
                        .join("-");
                    assert_eq!(*value, slug);
                }
                "x-session-id" => {
                    uuid::Uuid::parse_str(value)?;
                    assert_eq!(request["threadId"], *value);
                }
                "Authorization" => assert_eq!(*value, "Bearer local-command-key"),
                "traceparent" => {
                    let parts: Vec<_> = value.split('-').collect();
                    assert_eq!(parts.len(), 4);
                    assert_eq!((parts[0], parts[3]), ("00", "01"));
                    assert_eq!((parts[1].len(), parts[2].len()), (32, 16));
                    assert!(
                        parts[1..3]
                            .iter()
                            .all(|part| part.bytes().all(|byte| byte.is_ascii_hexdigit())
                                && part.bytes().any(|byte| byte != b'0'))
                    );
                }
                "content-length" => assert_eq!(value.parse::<usize>()?, captured.body.len()),
                _ => assert_eq!(*value, field["value"].as_str().unwrap(), "field {name}"),
            }
        }
        assert_eq!(request["params"]["model"], model);
        assert_eq!(request["params"]["stream"], true);
        assert_eq!(request["permissionMode"], "standard");
        assert!(request["mode"].is_null());
        assert!(
            request["params"]["messages"]
                .to_string()
                .contains("Reply wire accepted")
        );
    }
    with_real_time_limit(server.task).await???;
    Ok(())
}
