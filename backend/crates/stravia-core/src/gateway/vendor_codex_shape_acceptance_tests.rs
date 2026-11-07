use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures::{SinkExt, StreamExt};
use tower::ServiceExt;
use tungstenite_deflate::{
    handshake::derive_accept_key,
    protocol::{Message, Role},
};

const MODEL: &str = "gpt-6-astra";
const ROUTE: &str = "codex-shape-acceptance";
const UA: &str = "codex-tui/0.167.0 (Linux Unknown; x86_64) unknown (codex-tui; 0.167.0)";

fn events() -> Vec<Value> {
    let item = json!({"type":"message","id":"msg_1","status":"completed","role":"assistant",
        "content":[{"type":"output_text","text":"READY","annotations":[]}]});
    let response = json!({"id":"resp_1","object":"response","created_at":1,"completed_at":2,
        "status":"completed","model":MODEL,"output":[item],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2,
            "input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}},
        "incomplete_details":null,"previous_response_id":null,"instructions":null,"error":null,
        "tools":[],"tool_choice":"auto","truncation":"disabled","parallel_tool_calls":true,
        "text":{"format":{"type":"text"}},"top_p":null,"presence_penalty":null,
        "frequency_penalty":null,"top_logprobs":null,"temperature":null,"reasoning":null,
        "max_output_tokens":null,"max_tool_calls":null,"store":false,"background":false,
        "service_tier":"default","metadata":{},"safety_identifier":null,"prompt_cache_key":null});
    let mut created = response.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["usage"] = Value::Null;
    let part = item["content"][0].clone();
    let mut empty = part.clone();
    empty["text"] = json!("");
    let mut events = vec![
        json!({"type":"response.created","response":created}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"msg_1","part":empty}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_1","delta":"READY"}),
        json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"msg_1","text":"READY"}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":"msg_1","part":part}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":response}),
    ];
    for (index, event) in events.iter_mut().enumerate() {
        event["sequence_number"] = json!(index);
    }
    events
}

async fn hello(socket: &tokio::net::TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut bytes = vec![0; 65540];
    loop {
        let n = socket.peek(&mut bytes).await?;
        anyhow::ensure!(n != 0, "connection closed before ClientHello");
        if n >= 5 {
            let length = 5 + u16::from_be_bytes([bytes[3], bytes[4]]) as usize;
            if n >= length {
                bytes.truncate(length);
                return Ok(bytes);
            }
        }
        tokio::task::yield_now().await;
    }
}

async fn head(
    socket: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await?);
        anyhow::ensure!(bytes.len() < 65536, "request head too large");
    }
    let text = String::from_utf8(bytes)?;
    let mut lines = text.split("\r\n");
    let request = lines.next().context("request line")?.to_owned();
    let fields = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once(':').expect("header field");
            (name.to_owned(), value.trim().to_owned())
        })
        .collect();
    Ok((request, fields))
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> &'a str {
    let values: Vec<_> = fields
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .collect();
    assert_eq!(values.len(), 1, "expected one {name}: {fields:?}");
    &values[0].1
}

fn common_fields(fields: &[(String, String)]) {
    assert_eq!(field(fields, "host"), "chatgpt.com");
    assert_eq!(field(fields, "version"), "0.167.0");
    assert_eq!(
        field(fields, "authorization"),
        "Bearer synthetic-old-access"
    );
    assert_eq!(field(fields, "originator"), "codex-tui");
    assert_eq!(field(fields, "user-agent"), UA);
    assert_eq!(
        field(fields, "x-codex-beta-features"),
        "remote_compaction_v2"
    );
    assert_eq!(
        field(fields, "x-codex-routing-hint"),
        format!("model={MODEL}")
    );
    let thread = field(fields, "thread-id");
    assert!(!thread.is_empty());
    assert_eq!(field(fields, "session-id"), thread);
    assert_eq!(field(fields, "x-client-request-id"), thread);
    assert_eq!(field(fields, "x-codex-window-id"), format!("{thread}:0"));
}

fn socket_fields(fields: &[(String, String)]) {
    common_fields(fields);
    let names: Vec<_> = fields.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Host",
            "Connection",
            "Upgrade",
            "Sec-WebSocket-Version",
            "Sec-WebSocket-Key",
            "authorization",
            "user-agent",
            "originator",
            "openai-beta",
            "version",
            "x-codex-beta-features",
            "x-client-request-id",
            "session-id",
            "thread-id",
            "x-codex-window-id",
            "x-codex-turn-metadata",
            "x-codex-routing-hint",
            "sec-websocket-extensions"
        ]
    );
    assert_eq!(field(fields, "connection"), "Upgrade");
    assert_eq!(field(fields, "upgrade"), "websocket");
    assert_eq!(field(fields, "sec-websocket-version"), "13");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(field(fields, "sec-websocket-key"))
            .unwrap()
            .len(),
        16
    );
    assert_eq!(
        field(fields, "openai-beta"),
        "responses_websockets=2026-02-06"
    );
    assert_eq!(
        field(fields, "sec-websocket-extensions"),
        "permessage-deflate; client_max_window_bits"
    );
}

fn assert_interactive_fixture(fixture: &str, operation: &str) {
    let capture: Value = serde_json::from_str(fixture).unwrap();
    assert_eq!(capture["provenance"]["mode"], "interactive");
    assert_eq!(capture["provenance"]["operation"], operation);
    assert_eq!(capture["provenance"]["prompt_present"], true);
    assert_eq!(capture["provenance"]["authorization_scheme"], "Bearer");
    assert_eq!(capture["provenance"]["account_header_present"], true);
    assert!(
        capture["h1"]["headers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|header| header["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("originator"))
                && header["value"] == "codex-tui")
    );
}

async fn serve(listener: TcpListener, acceptor: TlsAcceptor, http: bool) -> anyhow::Result<()> {
    assert_interactive_fixture(
        include_str!("../plugin/network/shape/fixtures/codex-ws.json"),
        "response.create",
    );
    if http {
        assert_interactive_fixture(
            include_str!("../plugin/network/shape/fixtures/codex-http.json"),
            "responses",
        );
    }
    let (tcp, _) = listener.accept().await?;
    let wire = hello(&tcp).await?;
    crate::plugin::network::shape::tests::assert_wire_matches_fixture(
        "codex-ws",
        &wire,
        include_str!("../plugin/network/shape/fixtures/codex-ws.json"),
        true,
    );
    let mut socket = acceptor.accept(tcp).await?;
    let (line, fields) = head(&mut socket).await?;
    assert_eq!(line, "GET /backend-api/codex/responses HTTP/1.1");
    socket_fields(&fields);
    if !http {
        let accept = derive_accept_key(field(&fields, "sec-websocket-key").as_bytes());
        socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await?;
        let mut ws =
            tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(socket, Role::Server, None)
                .await;
        let message = ws.next().await.context("response.create frame")??;
        let Message::Text(text) = message else {
            anyhow::bail!("expected text response.create frame")
        };
        let body: Value = serde_json::from_str(&text)?;
        assert_eq!(body["type"], "response.create");
        assert_eq!(body["model"], MODEL);
        for event in events() {
            ws.send(Message::Text(event.to_string().into())).await?;
        }
        return Ok(());
    }
    socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await?;
    drop(socket);
    let (tcp, _) = listener.accept().await?;
    let wire = hello(&tcp).await?;
    crate::plugin::network::shape::tests::assert_wire_matches_fixture(
        "codex-http",
        &wire,
        include_str!("../plugin/network/shape/fixtures/codex-http.json"),
        false,
    );
    let mut socket = acceptor.accept(tcp).await?;
    let (line, fields) = head(&mut socket).await?;
    assert_eq!(line, "POST /backend-api/codex/responses HTTP/1.1");
    common_fields(&fields);
    let names: Vec<_> = fields.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "version",
            "x-codex-beta-features",
            "x-codex-window-id",
            "x-codex-turn-metadata",
            "x-codex-routing-hint",
            "x-client-request-id",
            "session-id",
            "thread-id",
            "accept",
            "content-encoding",
            "content-type",
            "authorization",
            "originator",
            "user-agent",
            "host",
            "content-length"
        ]
    );
    assert_eq!(field(&fields, "accept"), "text/event-stream");
    assert_eq!(field(&fields, "content-type"), "application/json");
    assert_eq!(field(&fields, "content-encoding"), "zstd");
    let len: usize = field(&fields, "content-length").parse()?;
    let mut encoded = vec![0; len];
    socket.read_exact(&mut encoded).await?;
    let mut decoder = async_compression::tokio::bufread::ZstdDecoder::new(encoded.as_slice());
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).await?;
    let request: Value = serde_json::from_slice(&decoded)?;
    assert_eq!(request["model"], MODEL);
    assert_eq!(request["stream"], true);
    let body = events()
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
    Ok(())
}

async fn acceptance(http: bool) -> anyhow::Result<()> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["chatgpt.com".into()])?;
    let certificate_der = cert.der().to_vec();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert.der().clone()], key)?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (_tx, requests) = mpsc::unbounded_channel();
    let dummy = LocalTlsServer {
        address,
        certificate_der,
        requests,
        task: tokio::spawn(async { Ok(Vec::new()) }),
    };
    let storage_directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(storage_directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage = Arc::new(crate::storage::SqliteStorage::from_pool(pool));
    let (_directory, gateway) = gateway_for_storage(&dummy, &["openai-codex"], storage).await?;
    let provider = synthetic_oauth_provider(&gateway, "openai-codex", false).await?;
    gateway.admin().create_manual_provider_model(&provider.id, MODEL, crate::provider_models::CreateManualProviderModel {
        metadata: json!({"id":MODEL,"name":MODEL,"modalities":{"input":["text"],"output":["text"]}}), template_id: None,
    }).await?;
    let route = gateway.admin().create_model(serde_json::from_value(json!({"model_id":ROUTE,"output_modality":"text",
        "targets":[{"provider_id":provider.id,"model":MODEL,"enabled":true,"target_retry_budget":1}]}))?).await?;
    let key = gateway.admin().create_api_key(serde_json::from_value(json!({"name":"Codex acceptance", "model_ids":[route.id],"transparent_injection_enabled":false}))?).await?;
    let server = tokio::spawn(serve(listener, acceptor, http));
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        crate::proxy::server::create_router(gateway).oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", key.token))
                .body(Body::from(
                    json!({"model":ROUTE,"store":false,"stream":false,
                "prompt_cache_key":"acceptance-session","input":"Say READY"})
                    .to_string(),
                ))?,
        ),
    )
    .await??;
    let status = response.status();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let result: Value = serde_json::from_slice(&body)?;
    assert_eq!(
        result["output"][0]["content"][0]["text"], "READY",
        "{result}"
    );
    tokio::time::timeout(Duration::from_secs(10), server).await???;
    Ok(())
}

#[tokio::test]
async fn codex_websocket_inference_has_genuine_tls_shape_and_ordered_upgrade() -> anyhow::Result<()>
{
    acceptance(false).await
}

#[tokio::test]
async fn codex_http_inference_has_genuine_tls_shape_and_ordered_compressed_request()
-> anyhow::Result<()> {
    acceptance(true).await
}
