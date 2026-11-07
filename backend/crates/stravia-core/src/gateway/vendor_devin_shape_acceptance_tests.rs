use super::*;

const ASSIGN_PATH: &str = "/exa.api_server_pb.ApiServerService/AssignModel";
const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const ROUTER: &str = "swe-1-6-fast";
const ASSIGNED_MODEL: &str = "swe-1-6-fast";
const ASSIGNMENT_JWT: &str = "local-assignment-jwt";

// A real Connect stream: text and usage protobuf, followed by the success trailer.
fn chat_response() -> Vec<u8> {
    fn frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![flags];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
    let mut metadata = proto_u64(2, 1);
    metadata.extend(proto_u64(3, 1));
    metadata.extend(proto_string(9, ASSIGNED_MODEL));
    let mut message = proto_string(3, "wire accepted");
    message.extend(proto_u64(5, 2));
    message.extend(proto_len(7, &metadata));
    let mut result = frame(0, &message);
    result.extend(frame(2, b"{}"));
    result
}

// Capture every ClientHello before handing the same socket to rustls. The
// response closes each connection, so the chat capture cannot be confused
// with the preceding PKCE exchange or AssignModel request.
async fn devin_server() -> anyhow::Result<(
    LocalTlsServer,
    tokio::sync::oneshot::Receiver<(CapturedInference, Vec<u8>)>,
)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![DEVIN_HOST.to_owned()])?;
    let certificate_der = cert.der().to_vec();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert.der().clone()], key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // The independent cold captures explicitly disabled TLS tickets.
    config.send_tls13_tickets = 0;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (capture_tx, capture_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut observed = Vec::new();
        let mut assignment = None;
        let mut capture_tx = Some(capture_tx);
        loop {
            let (mut tcp, _) = listener.accept().await?;
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
                "expected complete ClientHello"
            );
            let (reader, writer) = tcp.into_split();
            let replay = std::io::Cursor::new(hello.clone()).chain(reader);
            let mut socket = acceptor.accept(tokio::io::join(replay, writer)).await?;
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let (end, body_len) = loop {
                let n = socket.read(&mut buffer).await?;
                anyhow::ensure!(n > 0 && bytes.len() < 4 * 1024 * 1024, "incomplete request");
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = std::str::from_utf8(&bytes[..end])?;
                    let length = head
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .context("Content-Length")?
                        .1
                        .trim()
                        .parse::<usize>()?;
                    anyhow::ensure!(length <= 4 * 1024 * 1024, "oversized request");
                    break (end + 4, length);
                }
            };
            while bytes.len() < end + body_len {
                let n = socket.read(&mut buffer).await?;
                anyhow::ensure!(n > 0, "truncated request");
                bytes.extend_from_slice(&buffer[..n]);
            }
            let head = String::from_utf8(bytes[..end].to_vec())?;
            let mut lines = head.lines();
            let mut request_line = lines.next().context("request line")?.split_whitespace();
            let method = request_line.next().context("method")?.to_owned();
            let path = request_line.next().context("path")?.to_owned();
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            let body = bytes[end..end + body_len].to_vec();
            let request = ObservedRequest {
                method,
                path: path.clone(),
                headers,
                body: body.clone(),
            };
            request_tx
                .send(request.clone())
                .map_err(|_| anyhow::anyhow!("request observer dropped"))?;
            let response = if let Some(reply) = devin_token_response(&request) {
                reply
            } else if path == ASSIGN_PATH {
                assignment = Some(body.clone());
                let mut value = proto_string(1, ASSIGNMENT_JWT);
                value.extend(proto_string(2, ASSIGNED_MODEL));
                LocalResponse::protobuf(proto_len(1, &value))
            } else if path == CHAT_PATH {
                LocalResponse {
                    status: "200 OK",
                    content_type: "application/connect+proto",
                    body: chat_response(),
                    release: None,
                }
            } else {
                anyhow::bail!("unexpected local Devin request {path}");
            };
            write_local_response(&mut socket, response).await?;
            observed.push(request);
            if path == CHAT_PATH {
                let capture = CapturedInference { hello, head, body };
                capture_tx
                    .take()
                    .unwrap()
                    .send((capture, assignment.context("missing AssignModel")?))
                    .map_err(|_| anyhow::anyhow!("capture receiver dropped"))?;
                break;
            }
        }
        Ok(observed)
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

// Minimal checked protobuf wire reader for the independent request assertions.
fn wire_varint(bytes: &[u8], at: &mut usize) -> anyhow::Result<u64> {
    let mut result = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*at).context("truncated protobuf varint")?;
        *at += 1;
        anyhow::ensure!(shift < 63 || byte <= 1, "protobuf varint overflow");
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    anyhow::bail!("protobuf varint overflow")
}

fn wire_fields(bytes: &[u8]) -> anyhow::Result<Vec<(u32, u64, &[u8])>> {
    let mut fields = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let tag = wire_varint(bytes, &mut at)?;
        let number = (tag >> 3) as u32;
        anyhow::ensure!(number != 0, "invalid protobuf field");
        let (scalar, data) = match tag & 7 {
            0 => (wire_varint(bytes, &mut at)?, &bytes[0..0]),
            1 | 2 | 5 => {
                let len = match tag & 7 {
                    1 => 8,
                    5 => 4,
                    _ => usize::try_from(wire_varint(bytes, &mut at)?)?,
                };
                let end = at.checked_add(len).context("protobuf length overflow")?;
                let data = bytes.get(at..end).context("truncated protobuf field")?;
                at = end;
                (0, data)
            }
            _ => anyhow::bail!("unsupported protobuf wire type"),
        };
        fields.push((number, scalar, data));
    }
    Ok(fields)
}

fn field<'a>(fields: &[(u32, u64, &'a [u8])], number: u32) -> anyhow::Result<&'a [u8]> {
    let mut values = fields.iter().filter(|entry| entry.0 == number);
    let value = values
        .next()
        .context(format!("missing protobuf field {number}"))?
        .2;
    anyhow::ensure!(values.next().is_none(), "duplicate protobuf field {number}");
    Ok(value)
}

#[tokio::test]
async fn devin_real_wasm_inference_matches_interactive_tls_and_http() -> anyhow::Result<()> {
    let (mut server, capture) = devin_server().await?;
    let (_directory, gateway) = inference_gateway(&server, "devin").await?;
    let authorization = sign_in_devin_manually(&gateway, &mut server).await?;
    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &authorization.session_id,
            devin_oauth_provider("Devin wire fixture"),
        )
        .await?;
    let key = route_key(&gateway, &provider.id, ROUTER, "devin-wire").await?;
    let response = with_real_time_limit(crate::proxy::server::create_router(gateway).oneshot(
        Request::post("/v1/responses").header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"model":"devin-wire", "input":"reply wire accepted", "max_output_tokens":1234}).to_string()))?,
    )).await??;
    let status = response.status();
    let response_body = to_bytes(response.into_body(), 1024 * 1024).await?;
    anyhow::ensure!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&response_body)
    );
    let output: Value = serde_json::from_slice(&response_body)?;
    assert_eq!(output["status"], "completed");
    assert!(
        output["output"]
            .as_array()
            .context("response output")?
            .iter()
            .any(|item| item["role"] == "assistant"
                && item["content"].as_array().is_some_and(|content| content
                    .iter()
                    .any(|part| part["type"] == "output_text" && part["text"] == "wire accepted")))
    );

    let (captured, assignment) = with_real_time_limit(capture).await??;
    let fixture = include_str!("../plugin/network/shape/fixtures/devin-interactive.json");
    let repeat = include_str!("../plugin/network/shape/fixtures/devin-interactive.repeat.json");
    for sample in [fixture, repeat] {
        crate::plugin::network::shape::tests::assert_wire_matches_fixture(
            "devin",
            &captured.hello,
            sample,
            true,
        );
        let expected: Value = serde_json::from_str(sample)?;
        assert_eq!(
            captured.head.lines().next(),
            Some("POST /exa.api_server_pb.ApiServerService/GetChatMessage HTTP/1.1")
        );
        assert_ordered_head_matches_fixture(&captured.head, &expected);
    }
    let headers: Vec<_> = captured
        .head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(": "))
        .collect();
    assert_eq!(
        headers[0].1,
        format!("Basic {DEVIN_CONTRACT_TOKEN}-{DEVIN_CONTRACT_TOKEN}")
    );
    let trace: Vec<_> = headers[1].1.split('-').collect();
    assert_eq!(trace.len(), 3);
    assert_eq!((trace[0].len(), trace[1].len(), trace[2]), (32, 16, "1"));
    assert!(
        trace[..2]
            .iter()
            .all(|part| part.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    assert_eq!(headers[5].1, DEVIN_HOST);
    assert_eq!(headers[6].1.parse::<usize>()?, captured.body.len());

    assert!(captured.body.len() >= 5);
    assert_eq!(captured.body[0], 0);
    assert_eq!(
        u32::from_be_bytes(captured.body[1..5].try_into()?) as usize,
        captured.body.len() - 5
    );
    let fields = wire_fields(&captured.body[5..])?;
    let metadata = wire_fields(field(&fields, 1)?)?;
    let expected: Value = serde_json::from_str(fixture)?;
    for number in [1, 2, 4, 5, 7, 12, 28] {
        assert_eq!(
            std::str::from_utf8(field(&metadata, number)?)?,
            expected["body_pattern"]["client_metadata"][format!("field_{number}")]
                .as_str()
                .unwrap()
        );
    }
    assert_eq!(field(&metadata, 3)?, DEVIN_CONTRACT_TOKEN.as_bytes());
    let fingerprint = field(&metadata, 31)?;
    // The fixture omits the identity itself; ciphertext length also depends on
    // the (different) session token and the native timestamp in its plaintext.
    assert!(fingerprint.len() >= 56 && fingerprint.len() % 2 == 0);
    assert!(fingerprint.iter().all(u8::is_ascii_hexdigit));
    let assigned = wire_fields(&assignment)?;
    assert_eq!(field(&assigned, 2)?, ROUTER.as_bytes());
    assert_eq!(field(&fields, 16)?, field(&assigned, 3)?);
    assert_eq!(field(&fields, 21)?, ASSIGNED_MODEL.as_bytes());
    assert_eq!(field(&fields, 26)?, ASSIGNMENT_JWT.as_bytes());
    let messages: Vec<_> = fields.iter().filter(|entry| entry.0 == 3).collect();
    let messages = messages
        .iter()
        .map(|entry| wire_fields(entry.2))
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert!(
        messages
            .iter()
            .any(|message| field(message, 3).is_ok_and(|prompt| prompt == b"reply wire accepted"))
    );
    assert!(fields.iter().any(|entry| entry.0 == 7 && entry.1 == 5));
    let completion = wire_fields(field(&fields, 8)?)?;
    assert!(
        completion
            .iter()
            .any(|entry| entry.0 == 2 && entry.1 == 1234)
    );
    let trajectory = wire_fields(field(&fields, 15)?)?;
    let trajectory_id = uuid::Uuid::parse_str(std::str::from_utf8(field(&trajectory, 1)?)?)?;
    let cascade_id = uuid::Uuid::parse_str(std::str::from_utf8(field(&fields, 16)?)?)?;
    assert_ne!(trajectory_id, cascade_id);
    assert!(trajectory.iter().any(|entry| entry.0 == 3 && entry.1 == 4));
    assert!(trajectory.iter().any(|entry| entry.0 == 4 && entry.1 == 14));
    assert!(fields.iter().any(|entry| entry.0 == 20 && entry.1 == 1));
    with_real_time_limit(server.task).await???;
    Ok(())
}
