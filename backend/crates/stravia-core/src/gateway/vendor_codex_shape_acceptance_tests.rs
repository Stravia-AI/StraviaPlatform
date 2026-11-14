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

struct BoundaryExposure;
struct BoundaryExposureSession;
struct BoundaryTool;

impl stravia_runtime_contract::hook::Hook for BoundaryExposure {
    fn descriptor(&self) -> stravia_runtime_contract::hook::HookDescriptor {
        stravia_runtime_contract::hook::HookDescriptor {
            event_kinds: vec![stravia_runtime_contract::hook::EventKind::Request],
            ..stravia_runtime_contract::hook::HookDescriptor::all("boundary-exposure")
        }
    }
    fn create_session(
        &self,
        _: &stravia_runtime_contract::hook::SessionContext,
    ) -> Box<dyn stravia_runtime_contract::hook::HookSession> {
        Box::new(BoundaryExposureSession)
    }
}

#[async_trait::async_trait]
impl stravia_runtime_contract::hook::HookSession for BoundaryExposureSession {
    async fn handle(
        &mut self,
        event: stravia_runtime_contract::hook::HookEvent<'_>,
    ) -> Result<stravia_runtime_contract::hook::ActionBatch, String> {
        if matches!(
            event,
            stravia_runtime_contract::hook::HookEvent::Request { .. }
        ) {
            Ok(stravia_runtime_contract::hook::ActionBatch::one(
                stravia_runtime_contract::hook::HookAction::ExposeTool(
                    stravia_runtime_contract::hook::ToolId::new("boundary-tool"),
                ),
            ))
        } else {
            Ok(stravia_runtime_contract::hook::ActionBatch::default())
        }
    }
}

#[async_trait::async_trait]
impl stravia_runtime_contract::hook::PlatformTool for BoundaryTool {
    fn id(&self) -> stravia_runtime_contract::hook::ToolId {
        stravia_runtime_contract::hook::ToolId::new("boundary-tool")
    }
    fn external_name(&self) -> &str {
        "boundary_tool"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn execute(
        &self,
        arguments: Value,
        _: stravia_runtime_contract::hook::ToolExecutionContext,
    ) -> Result<Value, stravia_runtime_contract::hook::PlatformToolError> {
        Ok(arguments)
    }
}

async fn legacy_boundary_continuation(
    expose_tool: bool,
    mixed_reasoning: bool,
    remove_item: bool,
) -> anyhow::Result<()> {
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
    let (_directory, mut gateway) =
        gateway_for_storage(&dummy, &["openai-codex"], storage.clone()).await?;
    if expose_tool {
        crate::gateway::extensions::configure_gateway_extensions(
            &mut gateway,
            vec![Arc::new(BoundaryExposure)],
            vec![Arc::new(BoundaryTool)],
            vec![],
            vec![],
        )
        .await?;
    }
    let provider = synthetic_oauth_provider(&gateway, "openai-codex", false).await?;
    gateway.admin().create_manual_provider_model(&provider.id, MODEL, crate::provider_models::CreateManualProviderModel {
        metadata: json!({"id":MODEL,"name":MODEL,"use_responses_lite":true,"reasoning_efforts":["xhigh"],"modalities":{"input":["text"],"output":["text"]}}), template_id: None,
    }).await?;
    let route = gateway.admin().create_model(serde_json::from_value(json!({"model_id":ROUTE,"output_modality":"text",
        "targets":[{"provider_id":provider.id,"model":MODEL,"enabled":true,"target_retry_budget":0}]}))?).await?;
    let key = gateway.admin().create_api_key(serde_json::from_value(json!({"name":"Codex continuation", "model_ids":[route.id],"transparent_injection_enabled":false}))?).await?;
    let (captured, mut requests) = mpsc::unbounded_channel::<Value>();
    let server = tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let captured = captured.clone();
            tokio::spawn(async move {
                let mut socket = acceptor.accept(tcp).await?;
                let (_, fields) = head(&mut socket).await?;
                if !fields.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("upgrade") && value.eq_ignore_ascii_case("websocket")
                }) {
                    // Gateway 的后台额度探测不属于此推理上游，返回真实 HTTP 错误而非 panic。
                    socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                    socket.shutdown().await?;
                    return anyhow::Ok(());
                }
                let accept = derive_accept_key(field(&fields, "sec-websocket-key").as_bytes());
                socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await?;
                let mut ws = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
                    socket,
                    Role::Server,
                    None,
                )
                .await;
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let body: Value = serde_json::from_str(&text)?;
                    let _ = captured.send(body);
                    for event in events() {
                        ws.send(Message::Text(event.to_string().into())).await?;
                    }
                }
                anyhow::Ok(())
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });
    let mut input = json!([
        {"type":"message","role":"user","content":[{"type":"input_text","text":"previous"}]},
        {"type":"reasoning","id":"reason_1","summary":[{"type":"summary_text","text":"prior reasoning"}],"encrypted_content":"foreign-encrypted-reasoning"},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"lookup"}]}
    ]);
    if remove_item {
        input[1]["summary"] = json!([]);
    }
    if mixed_reasoning {
        input.as_array_mut().unwrap().insert(2, json!({"type":"reasoning","id":"reason_2",
            "summary":[{"type":"summary_text","text":"retained reasoning"}],"encrypted_content":"foreign-preserved-reasoning"}));
    }
    let body = json!({"model":ROUTE,"store":false,"stream":false,"reasoning":{"effort":"xhigh","summary":"auto"},"prompt_cache_key":"original-session","input":input.clone(),
        "tools":[{"type":"function","name":"lookup","description":"lookup","parameters":{"type":"object","properties":{}}}]});
    // 冷恢复 fixture 自身较大；像真实 HTTP 服务一样独立调度请求，避免叠加其 poll 栈。
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::spawn(
            crate::proxy::server::create_router(gateway.clone()).oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", key.token))
                    .body(Body::from(body.to_string()))?,
            ),
        ),
    )
    .await???;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "root: {}",
        String::from_utf8_lossy(&bytes)
    );
    let root_request = requests.try_recv()?;
    assert!(
        root_request
            .to_string()
            .contains("foreign-encrypted-reasoning")
    );
    assert!(root_request.to_string().contains("lookup"));
    assert_eq!(
        root_request.to_string().contains("boundary_tool"),
        expose_tool
    );
    let first: Value = serde_json::from_slice(&bytes)?;
    let root_id = first["id"].as_str().context("root response id")?;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", key.token).parse()?,
    );
    let credential = crate::proxy::security::ClientCredential::from_inference_headers(&headers);
    let principal = crate::proxy::security::Security::new(gateway.storage.auth())
        .authenticated_principal(&credential)
        .await?
        .principal;
    let mut request = stravia_protocol_codec::transform::ProtocolTransform::global()
        .bind(stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24,
              stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24).context("Responses codec")?
        .decode_request(json!({"model":ROUTE,"store":false,"stream":false,"reasoning":{"effort":"xhigh","summary":"auto"},
            "prompt_cache_key":"original-session",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"lookup now"}]}],
            "tools":[{"type":"function","name":"lookup","description":"lookup","parameters":{"type":"object","properties":{}}}]}))?;
    crate::router::stamp_previous_response_id(&mut request, root_id);
    let mut parent = gateway
        .generation_chains
        .begin(principal.clone(), request)
        .await?;
    parent.admit_user_prompt().await?;
    let candidate = parent
        .history_boundary_candidate()
        .context("admitted boundary")?;
    let source = parent.request_snapshot();
    assert!(
        source
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|tool| tool.name == "lookup"))
    );
    assert!(
        !source
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|tool| tool.name == "boundary_tool"))
    );
    assert!(candidate.source_item_gap > 1);
    let atomic = parent
        .atomic_conversation_context()
        .context("atomic admission")?;
    let store = &gateway.vendor_plugins.store;
    let (_plugin, _operation, data_epoch) = gateway.vendor_plugins.acquire("openai-codex")?;
    let fence = store
        .open_atomic_provider_fence("openai-codex", &provider.id, data_epoch)
        .await
        .context("opening provider fence")?;
    let scope = crate::plugin::JournalScope {
        vendor_id: "openai-codex".into(),
        provider_id: provider.id.clone(),
        data_epoch,
        provider_fence: fence,
        principal: atomic.principal.clone(),
        admission: atomic.admission_id.to_string(),
        lease: atomic.atomic_lease_id.to_string(),
        key: "legacy-proof-fixture".into(),
        candidate: Some(candidate.clone()),
    };
    let root_boundaries = store
        .history_boundary_candidates(
            &scope.vendor_id,
            &scope.provider_id,
            &scope.principal,
            &scope.admission,
            &scope.lease,
        )
        .await
        .context("reading published root boundary")?;
    let head = store
        .journal_append(&scope, "legacy proof".into(), None)
        .await
        .context("appending legacy journal")?;
    assert!(
        store
            .journal_compare_exchange(&scope, 0, None, Some(&head))
            .await
            .context("publishing legacy journal")?
    );
    let mut effective = source.as_ref().clone();
    let mut removed = Vec::new();
    assert!(
        stravia_protocol_codec::transform::prepare_thinking_replay_with_removals(
            &mut effective,
            |_, block| matches!(block,
            stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning {
                encrypted_content: Some(value), ..
            } if value == "foreign-preserved-reasoning"),
            |index| removed.push(index)
        )
    );
    if remove_item {
        assert_eq!(removed, vec![1]);
        assert_eq!(effective.items.len() + 1, source.items.len());
    } else {
        assert!(
            removed.is_empty(),
            "signature-only replay must preserve item coordinates"
        );
        assert_eq!(effective.items.len(), source.items.len());
    }
    assert!(
        crate::generation_chain::project_history_boundaries(&source, &candidate, &[]).is_some()
    );
    assert!(
        crate::generation_chain::project_history_boundaries(&effective, &candidate, &[]).is_none()
    );
    parent.observe_effective(effective);
    let mut response = stravia_runtime_contract::protocol::ir::AiResponse::new("legacy", MODEL);
    response.items.push(
        stravia_runtime_contract::protocol::ir::AiItem::function_call(
            stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            },
        ),
    );
    let mut provider_request = parent.request_snapshot().as_ref().clone();
    provider_request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "xhigh".into(),
        },
    );
    parent.observe_provider_response(&provider_request, &response);
    assert!(parent.stage(
        &mut response,
        &crate::generation_chain::GenerationSource::Hook {
            protocol: stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24,
        },
        None
    ));
    assert_ne!(
        stravia_runtime_contract::protocol::ir::canonical::history_request_controls_hash(
            parent.request()
        ),
        candidate.source_controls_hash,
        "legacy stage must differ from the controls proved before Target resolution"
    );
    assert_eq!(
        root_boundaries
            .first()
            .context("published root boundary")?
            .source_controls_hash,
        stravia_runtime_contract::protocol::ir::canonical::history_request_controls_hash(
            parent.request()
        ),
        "fresh boundary must prove the final Target controls"
    );
    let parent_id = parent.id().to_owned();
    parent.persist().await?;
    let mut probe_request = stravia_runtime_contract::protocol::ir::AiRequest::new(
        ROUTE,
        vec![
            stravia_runtime_contract::protocol::ir::AiItem::function_call_output(
                "call_1",
                json!("found"),
            ),
        ],
    );
    probe_request.reasoning = source.reasoning.clone();
    crate::router::stamp_previous_response_id(&mut probe_request, &parent_id);
    let mut probe = gateway
        .generation_chains
        .begin(principal.clone(), probe_request)
        .await?;
    probe.admit_user_prompt().await?;
    let current = probe
        .history_boundary_candidate()
        .context("probe boundary")?;
    let mut forged = candidate.clone();
    forged.source_prefix_hash[0] ^= 1;
    let recovered = gateway
        .generation_chains
        .recover_legacy_history_boundaries(
            &principal,
            &parent_id,
            probe.request(),
            std::slice::from_ref(&forged),
        )
        .await
        .context("ancestor recovery")?;
    assert!(
        crate::generation_chain::project_history_boundaries(probe.request(), &current, &recovered)
            .is_none()
    );
    let recovered = gateway
        .generation_chains
        .recover_legacy_history_boundaries(
            &principal,
            root_id,
            probe.request(),
            std::slice::from_ref(&candidate),
        )
        .await
        .context("other branch")?;
    assert!(
        crate::generation_chain::project_history_boundaries(probe.request(), &current, &recovered)
            .is_none()
    );
    drop(probe);
    drop(parent);
    drop(_operation);
    gateway.shutdown().await;
    let mut gateway = Gateway::from_storage_with_vendor_network(
        GatewayConfig {
            data_dir: _directory.path().to_path_buf(),
            ..Default::default()
        },
        storage,
        local_vendor_network(dummy.address, &dummy.certificate_der)?,
    )
    .await?;
    if expose_tool {
        crate::gateway::extensions::configure_gateway_extensions(
            &mut gateway,
            vec![Arc::new(BoundaryExposure)],
            vec![Arc::new(BoundaryTool)],
            vec![],
            vec![],
        )
        .await?;
    }
    let result = json!([{"type":"function_call_output","call_id":"call_1","output":"found"}]);
    let body = json!({"model":ROUTE,"store":false,"stream":false,"reasoning":{"effort":"xhigh","summary":"auto"},"prompt_cache_key":"original-session",
        "previous_response_id":parent_id,"input":result,
        "tools":[{"type":"function","name":"lookup","description":"lookup","parameters":{"type":"object","properties":{}}}]});
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::spawn(
            crate::proxy::server::create_router(gateway.clone()).oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", key.token))
                    .body(Body::from(body.to_string()))?,
            ),
        ),
    )
    .await???;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "continuation: {}",
        String::from_utf8_lossy(&bytes)
    );
    let continued_request = requests.try_recv()?;
    assert!(continued_request.to_string().contains("call_1"));
    assert!(continued_request.to_string().contains("lookup"));
    assert_eq!(
        continued_request.to_string().contains("boundary_tool"),
        expose_tool
    );
    let mut full = input.as_array().unwrap().clone();
    full.extend(first["output"].as_array().unwrap().iter().cloned());
    full.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":"lookup now"}]}));
    full.push(json!({"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}));
    full.push(json!({"type":"function_call_output","call_id":"call_1","output":"found"}));
    let body = json!({"model":ROUTE,"store":false,"stream":false,"reasoning":{"effort":"xhigh","summary":"auto"},"prompt_cache_key":"original-session",
        "input":full,"tools":[{"type":"function","name":"lookup","description":"lookup","parameters":{"type":"object","properties":{}}}]});
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::spawn(
            crate::proxy::server::create_router(gateway.clone()).oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", key.token))
                    .body(Body::from(body.to_string()))?,
            ),
        ),
    )
    .await???;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "full history: {}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(requests.try_recv()?.to_string().contains("call_1"));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn lite_tool_continuation_recovers_persisted_pre_replay_boundary() -> anyhow::Result<()> {
    legacy_boundary_continuation(false, false, false).await
}

#[tokio::test]
async fn lite_tool_continuation_recovers_removed_reasoning_item() -> anyhow::Result<()> {
    legacy_boundary_continuation(false, false, true).await
}

#[tokio::test]
async fn lite_tool_continuation_recovers_selectively_replayed_reasoning() -> anyhow::Result<()> {
    legacy_boundary_continuation(false, true, false).await
}

#[tokio::test]
async fn lite_tool_continuation_preserves_client_tools_with_exposed_platform_tool()
-> anyhow::Result<()> {
    legacy_boundary_continuation(true, false, false).await
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
