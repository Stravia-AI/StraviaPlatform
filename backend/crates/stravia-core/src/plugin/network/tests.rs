use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::Message as AxumMessage;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use stravia_runtime_contract::CancellationToken;
use stravia_vendor_runtime::{HostWebSocket, WebSocketMessage};
use stravia_vendor_sdk::{
    AiErrorKind, ErrorKind,
    net::{Coding, ConnectRequest, Fields, HttpVersion, Options, Request, WsCompression},
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::{
    MAX_WEBSOCKET_AGE, VendorNetwork, VendorWebSocketPool, bytes_value, http, ws_message_type,
    ws_payload,
};
use crate::plugin::lifecycle::VendorOperationTracker;

#[derive(Clone)]
struct UpstreamState {
    connections: Arc<AtomicUsize>,
    closed: mpsc::UnboundedSender<()>,
    response_waiting: Arc<tokio::sync::Notify>,
    response_release: Arc<tokio::sync::Notify>,
}

struct TestServer {
    url: String,
    connections: Arc<AtomicUsize>,
    closed: mpsc::UnboundedReceiver<()>,
    response_waiting: Arc<tokio::sync::Notify>,
    response_release: Arc<tokio::sync::Notify>,
    task: AbortHandle,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn websocket_handler(
    State(state): State<UpstreamState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .on_upgrade(move |mut socket| async move {
            state.connections.fetch_add(1, Ordering::SeqCst);
            while let Some(Ok(message)) = socket.recv().await {
                let AxumMessage::Text(text) = message else {
                    continue;
                };
                if text == "disconnect-without-close" {
                    break;
                }
                if text == "close-before-response" {
                    socket
                        .send(AxumMessage::Close(Some(axum::extract::ws::CloseFrame {
                            code: 1001,
                            reason: "synthetic upstream shutdown".into(),
                        })))
                        .await
                        .expect("send real upstream Close");
                    break;
                }
                if text == "delayed-response" {
                    state.response_waiting.notify_one();
                    state.response_release.notified().await;
                }
                if socket
                    .send(AxumMessage::Text(format!("ack:{text}").into()))
                    .await
                    .is_err()
                {
                    break;
                }
                if text == "close-after-ack" {
                    socket
                        .send(AxumMessage::Close(None))
                        .await
                        .expect("close acknowledged test socket");
                    break;
                }
            }
            let _ = state.closed.send(());
        })
        .into_response()
}

async fn test_server() -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local WebSocket upstream");
    let address = listener.local_addr().expect("local upstream address");
    let connections = Arc::new(AtomicUsize::new(0));
    let (closed_tx, closed) = mpsc::unbounded_channel();
    let response_waiting = Arc::new(tokio::sync::Notify::new());
    let response_release = Arc::new(tokio::sync::Notify::new());
    let state = UpstreamState {
        connections: Arc::clone(&connections),
        closed: closed_tx,
        response_waiting: Arc::clone(&response_waiting),
        response_release: Arc::clone(&response_release),
    };
    let app = Router::new()
        .route("/socket", get(websocket_handler))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve local WebSocket upstream");
    });
    TestServer {
        url: format!("ws://{address}/socket"),
        connections,
        closed,
        response_waiting,
        response_release,
        task: task.abort_handle(),
    }
}

fn network(url: &str, pool: Arc<VendorWebSocketPool>) -> VendorNetwork {
    let operation = VendorOperationTracker::default()
        .begin("websocket-lifetime-test")
        .expect("begin vendor operation");
    let origin = url::Url::parse(url)
        .expect("parse local WebSocket URL")
        .origin()
        .ascii_serialization();
    VendorNetwork::new(
        Arc::new(super::transport::VendorTransport::new()),
        super::VendorNetworkConfig {
            shapes: Arc::new(super::shape::ShapeRegistry::builtin().expect("load shapes")),
            resolver: Arc::new(super::transport::SystemResolver),
        },
        crate::outbound_proxy::OutboundProxySettings::default(),
        crate::plugin::permissions::NetworkPolicy::compile([
            crate::plugin::permissions::NetworkOrigin::Exact(origin),
        ])
        .unwrap(),
        operation,
        CancellationToken::new(),
        "open-responses".into(),
    )
    .with_websocket_pool(pool, "test-scope".into(), Some("response-id".into()))
}

#[tokio::test]
async fn regex_policy_reauthorizes_http_redirect_before_sending() {
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination_url = format!("http://{}/forbidden", destination.local_addr().unwrap());
    let reached = Arc::new(AtomicUsize::new(0));
    let reached_by_handler = reached.clone();
    let app = Router::new().route(
        "/",
        get(move || {
            reached_by_handler.fetch_add(1, Ordering::SeqCst);
            let destination_url = destination_url.clone();
            async move { axum::response::Redirect::temporary(&destination_url) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/");
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut network = network(&url, Arc::new(VendorWebSocketPool::default()));
    network.origins = crate::plugin::permissions::NetworkPolicy::compile([
        crate::plugin::permissions::NetworkOrigin::HostRegex {
            scheme: "http".into(),
            port: address.port(),
            regex: r"127\.0\.0\.1".into(),
        },
    ])
    .unwrap();
    let response = network
        .http_start(Request::new("GET", url), Options::new())
        .unwrap();
    let error = response
        .status()
        .await
        .expect_err("redirect port is outside frozen policy");
    assert!(matches!(error.kind, stravia_vendor_sdk::ErrorKind::Trapped));
    assert_eq!(reached.load(Ordering::SeqCst), 1);
    assert_eq!(
        destination.into_std().unwrap().accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
    );
    task.abort();
}

#[tokio::test]
async fn websocket_redirects_preserve_only_negotiation_across_origins() {
    use tokio::io::AsyncWriteExt;
    use tungstenite_deflate::handshake::derive_accept_key;

    for compression in [
        WsCompression::Disabled,
        WsCompression::Preferred,
        WsCompression::Required,
    ] {
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_url = format!("ws://{}/start", source.local_addr().unwrap());
        let destination_url = format!("ws://{}/final", destination.local_addr().unwrap());
        let source_task = tokio::spawn({
            let destination_url = destination_url.clone();
            async move {
                let mut requests = Vec::new();
                for location in ["/same-origin", destination_url.as_str()] {
                    let (mut socket, _) = source.accept().await.unwrap();
                    requests.push(capture_request(&mut socket).await.0);
                    socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
                }
                requests
            }
        });
        let destination_task = tokio::spawn(async move {
            let (mut socket, _) = destination.accept().await.unwrap();
            let (head, _) = capture_request(&mut socket).await;
            let key = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                .unwrap()
                .1
                .trim();
            let accept = derive_accept_key(key.as_bytes());
            let extension = if compression == WsCompression::Disabled {
                ""
            } else {
                "Sec-WebSocket-Extensions: permessage-deflate\r\n"
            };
            socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: chat\r\n{extension}\r\n").as_bytes()).await.unwrap();
            head
        });
        let mut network = network(&source_url, Arc::new(VendorWebSocketPool::default()));
        network.origins = crate::plugin::permissions::NetworkPolicy::compile([
            crate::plugin::permissions::NetworkOrigin::Exact(
                url::Url::parse(&source_url)
                    .unwrap()
                    .origin()
                    .ascii_serialization(),
            ),
            crate::plugin::permissions::NetworkOrigin::Exact(
                url::Url::parse(&destination_url)
                    .unwrap()
                    .origin()
                    .ascii_serialization(),
            ),
        ])
        .unwrap();
        let mut fields = Fields::new();
        fields.append("Authorization", b"Bearer test-secret".to_vec());
        fields.append("Cookie", b"session=test-secret".to_vec());
        fields.append("X-Custom", b"private".to_vec());
        fields.append("sEc-WebSocket-Protocol", b"chat, other".to_vec());
        if compression != WsCompression::Disabled {
            fields.append(
                "sEc-WebSocket-Extensions",
                b"permessage-deflate; client_max_window_bits".to_vec(),
            );
        }
        let mut options = http1();
        options.ws_compression(compression).unwrap();
        let _socket = network
            .ws_connect(ConnectRequest::new(&source_url).fields(fields), options)
            .await
            .expect("redirected handshake succeeds");
        let source_heads = source_task.await.unwrap();
        for head in source_heads {
            let head = head.to_ascii_lowercase();
            assert!(head.contains("authorization: bearer test-secret\r\n"));
            assert!(head.contains("cookie: session=test-secret\r\n"));
            assert!(head.contains("x-custom: private\r\n"));
        }
        let head = destination_task.await.unwrap().to_ascii_lowercase();
        assert!(!head.contains("authorization:"));
        assert!(!head.contains("cookie:"));
        assert!(!head.contains("x-custom:"));
        assert!(head.contains("sec-websocket-protocol: chat, other\r\n"));
        assert_eq!(
            head.contains(
                "sec-websocket-extensions: permessage-deflate; client_max_window_bits\r\n"
            ),
            compression != WsCompression::Disabled
        );
        assert!(head.contains(&format!(
            "host: {}\r\n",
            url::Url::parse(&destination_url).unwrap().authority()
        )));
    }
}

#[tokio::test]
async fn regex_policy_reauthorizes_websocket_redirect_before_handshake() {
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination_url = format!("ws://{}/forbidden", destination.local_addr().unwrap());
    let reached = Arc::new(AtomicUsize::new(0));
    let reached_by_handler = reached.clone();
    let app = Router::new().route(
        "/",
        get(move || {
            reached_by_handler.fetch_add(1, Ordering::SeqCst);
            let destination_url = destination_url.clone();
            async move { axum::response::Redirect::temporary(&destination_url) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("ws://{address}/");
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut network = network(&url, Arc::new(VendorWebSocketPool::default()));
    network.origins = crate::plugin::permissions::NetworkPolicy::compile([
        crate::plugin::permissions::NetworkOrigin::HostRegex {
            scheme: "ws".into(),
            port: address.port(),
            regex: r"127\.0\.0\.1".into(),
        },
    ])
    .unwrap();
    let error = network
        .ws_connect(ConnectRequest::new(url), Options::new())
        .await
        .err()
        .expect("redirect port is outside frozen policy");
    assert!(matches!(error.kind, stravia_vendor_sdk::ErrorKind::Trapped));
    assert_eq!(reached.load(Ordering::SeqCst), 1);
    assert_eq!(
        destination.into_std().unwrap().accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
    );
    task.abort();
}

#[tokio::test]
async fn regex_policy_rejects_previously_authorized_pooled_websocket() {
    let server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let original = network(&server.url, pool.clone());
    round_trip_and_park(&original, &server.url, "tip").await;
    assert_eq!(pool.idle.lock().len(), 1);
    let requested = url::Url::parse(&server.url).unwrap();
    let mut current = network(&server.url, pool.clone());
    current.origins = crate::plugin::permissions::NetworkPolicy::compile([
        crate::plugin::permissions::NetworkOrigin::HostRegex {
            scheme: "ws".into(),
            port: requested.port_or_known_default().unwrap(),
            regex: "localhost".into(),
        },
    ])
    .unwrap();
    let error = require_tip(&current, &server.url, "tip", Vec::new())
        .await
        .err()
        .expect("unauthorized pooled destination must not be reused");
    assert!(matches!(error.kind, stravia_vendor_sdk::ErrorKind::Trapped));
    assert_eq!(pool.idle.lock().len(), 1);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

async fn checkout(network: &VendorNetwork, url: &str) -> Arc<dyn HostWebSocket> {
    checkout_with_age(network, url, None).await
}

async fn checkout_with_age(
    network: &VendorNetwork,
    url: &str,
    reuse_max_age_millis: Option<u64>,
) -> Arc<dyn HostWebSocket> {
    let mut options = Options::new();
    if let Some(age) = reuse_max_age_millis {
        options.reuse_max_age(Duration::from_millis(age)).unwrap();
    }
    network
        .ws_connect(ConnectRequest::new(url), options)
        .await
        .expect("connect local WebSocket upstream")
}

async fn round_trip_and_park(network: &VendorNetwork, url: &str, request: &str) {
    round_trip_and_park_with_age(network, url, request, None).await;
}

async fn round_trip_and_park_with_age(
    network: &VendorNetwork,
    url: &str,
    request: &str,
    reuse_max_age_millis: Option<u64>,
) {
    let socket = checkout_with_age(network, url, reuse_max_age_millis).await;
    complete_and_park(&socket, request, Some(request)).await;
}

async fn complete_and_park(
    socket: &Arc<dyn HostWebSocket>,
    request: &str,
    continuation_id: Option<&str>,
) {
    socket
        .send(WebSocketMessage::Text(request.to_owned()))
        .await
        .expect("send local WebSocket request");
    let response = socket
        .next()
        .await
        .expect("receive local WebSocket response");
    assert!(matches!(
        response,
        Some(WebSocketMessage::Text(text)) if text == format!("ack:{request}")
    ));
    socket
        .close(continuation_id.map(str::to_owned))
        .await
        .expect("return socket to pool");
}

async fn require_tip(
    network: &VendorNetwork,
    url: &str,
    tip: &str,
    headers: Vec<(String, String)>,
) -> Result<Arc<dyn HostWebSocket>, stravia_vendor_runtime::HostFailure> {
    require_tip_with_age(network, url, tip, headers, None).await
}

async fn require_tip_with_age(
    network: &VendorNetwork,
    url: &str,
    tip: &str,
    headers: Vec<(String, String)>,
    reuse_max_age_millis: Option<u64>,
) -> Result<Arc<dyn HostWebSocket>, stravia_vendor_runtime::HostFailure> {
    let mut options = Options::new();
    options.continuation(tip).unwrap();
    if let Some(age) = reuse_max_age_millis {
        options.reuse_max_age(Duration::from_millis(age)).unwrap();
    }
    let mut fields = Fields::new();
    for (name, value) in headers {
        fields.append(name, value.into_bytes());
    }
    network
        .ws_connect(ConnectRequest::new(url).fields(fields), options)
        .await
}

fn assert_local_miss(error: stravia_vendor_runtime::HostFailure) {
    assert!(matches!(
        error.kind,
        stravia_vendor_sdk::ErrorKind::ContinuationUnavailable
    ));
    assert_eq!(error.upstream_status, None);
}

async fn capture_observer(
    directory: &std::path::Path,
    debug: bool,
) -> (
    crate::interaction_observation::InteractionObservation,
    crate::interaction_observation::RunObserver,
) {
    use crate::interaction_observation::{AdmissionFacts, IngressStart, RunStart};
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let observation = crate::interaction_observation::InteractionObservation::new(
        Some(pool),
        None,
        directory.to_path_buf(),
        crate::history_retention::HistoryRetention::new(1),
        true,
        crate::generation_chain::test_chain().await,
    )
    .await;
    observation.set_debug_enabled(debug);
    let observer = observation
        .observe_ingress(IngressStart {
            id: "ws-capture-ingress".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        })
        .admit(
            RunStart {
                id: "ws-capture-run".into(),
                principal: "synthetic-owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "synthetic-route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "synthetic-model",
                    Vec::new(),
                )
                .into(),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    (observation, observer)
}

async fn captured_records(
    observation: &crate::interaction_observation::InteractionObservation,
) -> Vec<serde_json::Value> {
    use crate::interaction_observation::{BundleRequest, BundleResourceKind, ForestQuery};
    use futures::StreamExt as _;
    use std::io::Read as _;
    observation.flush().await.unwrap();
    let forest = observation
        .query_forest(ForestQuery::default())
        .await
        .unwrap();
    let interaction = &forest.roots.first().unwrap().interactions[0];
    let ticket = observation
        .issue_bundle_ticket(BundleRequest {
            kind: BundleResourceKind::Interaction,
            resource_id: interaction.id.clone(),
            through_sequence: Some(forest.snapshot_sequence),
        })
        .await
        .unwrap();
    let token = ticket.download_url.rsplit('/').next().unwrap();
    let mut stream = observation.consume_bundle_ticket(token).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut records: Vec<serde_json::Value> = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        if entry
            .name()
            .expect("valid bundle entry name")
            .ends_with("/events.jsonl")
        {
            let mut text = String::new();
            entry.read_to_string(&mut text).unwrap();
            records.extend(text.lines().map(|line| serde_json::from_str(line).unwrap()));
        }
    }
    records.sort_by_key(|record| record["recorded_at"].as_i64());
    records
}

#[tokio::test]
async fn reused_websocket_pre_application_close_is_captured_before_failure() {
    let server = test_server().await;
    let directory = tempfile::tempdir().unwrap();
    let (observation, observer) = capture_observer(directory.path(), true).await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool)
        .with_observer(Some(observer))
        .with_observation_scope(Some("ws-model-turn".into()), Some("ws-attempt".into()));
    round_trip_and_park(&network, &server.url, "first").await;
    let socket = require_tip(&network, &server.url, "first", Vec::new())
        .await
        .unwrap();
    socket
        .send(WebSocketMessage::Text("close-before-response".into()))
        .await
        .unwrap();
    let failure = socket
        .next()
        .await
        .expect_err("reused Close must still fail");
    assert_eq!(
        failure.kind.transport_failure(),
        Some(stravia_vendor_sdk::TransportFailure::Websocket)
    );
    observation.flush().await.unwrap();
    let records = captured_records(&observation).await;
    let closes: Vec<_> = records
        .iter()
        .filter(|record| {
            record["direction"] == "upstream_response" && record["message_type"] == "close"
        })
        .collect();
    assert_eq!(closes.len(), 1, "real Close must be captured exactly once");
    assert_eq!(closes[0]["run_id"], "ws-capture-run");
    assert_eq!(closes[0]["model_turn_id"], "ws-model-turn");
    assert_eq!(closes[0]["attempt_id"], "ws-attempt");
    assert_eq!(
        closes[0]["payload"],
        serde_json::json!({"code": 1001, "reason": "synthetic upstream shutdown"})
    );
    // 对端已关闭，关闭握手允许失败；这里验证失败连接不会被重新归池。
    let _ = socket.close(Some("invalid-tip".into())).await;
    assert_local_miss(
        require_tip(&network, &server.url, "invalid-tip", Vec::new())
            .await
            .err()
            .unwrap(),
    );
    observation.shutdown().await;
}

#[tokio::test]
async fn abnormal_websocket_termination_has_no_phantom_received_close() {
    let server = test_server().await;
    let directory = tempfile::tempdir().unwrap();
    let (observation, observer) = capture_observer(directory.path(), true).await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()))
        .with_observer(Some(observer))
        .with_observation_scope(Some("ws-model-turn".into()), Some("ws-attempt".into()));
    round_trip_and_park(&network, &server.url, "first").await;
    let socket = require_tip(&network, &server.url, "first", Vec::new())
        .await
        .unwrap();
    socket
        .send(WebSocketMessage::Text("disconnect-without-close".into()))
        .await
        .unwrap();
    let failure = socket.next().await.unwrap_err();
    assert!(failure.message.contains("without closing handshake"));
    assert_eq!(
        failure.kind.transport_failure(),
        Some(stravia_vendor_sdk::TransportFailure::Websocket)
    );
    let send_failure = socket
        .send(WebSocketMessage::Text("synthetic-after-disconnect".into()))
        .await
        .expect_err("failed transport must reject subsequent sends");
    assert!(send_failure.message.contains("send failed"));
    assert_eq!(
        send_failure.kind.transport_failure(),
        Some(stravia_vendor_sdk::TransportFailure::Websocket)
    );
    // 传输已失败，关闭握手不再保证成功；失败连接仍不得被重新归池。
    let _ = socket.close(Some("abnormal-tip".into())).await;
    assert_local_miss(
        require_tip(&network, &server.url, "abnormal-tip", Vec::new())
            .await
            .err()
            .unwrap(),
    );
    observation.flush().await.unwrap();
    let records = captured_records(&observation).await;
    assert!(!records.iter().any(|record| {
        record["direction"] == "upstream_response" && record["message_type"] == "close"
    }));
    assert!(
        records
            .iter()
            .filter(|record| record["run_id"] == "ws-capture-run")
            .all(|record| record["model_turn_id"] == "ws-model-turn"
                && record["attempt_id"] == "ws-attempt")
    );
    observation.shutdown().await;
}

#[tokio::test]
async fn websocket_eof_after_real_close_does_not_duplicate_the_received_frame() {
    let server = test_server().await;
    let directory = tempfile::tempdir().unwrap();
    let (observation, observer) = capture_observer(directory.path(), true).await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()))
        .with_observer(Some(observer));
    let socket = checkout(&network, &server.url).await;
    socket
        .send(WebSocketMessage::Text("close-before-response".into()))
        .await
        .unwrap();
    assert!(matches!(
        socket.next().await.unwrap(),
        Some(WebSocketMessage::Close(Some((1001, _))))
    ));
    assert!(socket.next().await.is_err());
    observation.flush().await.unwrap();
    assert_eq!(
        captured_records(&observation)
            .await
            .iter()
            .filter(|record| {
                record["direction"] == "upstream_response" && record["message_type"] == "close"
            })
            .count(),
        1
    );
    observation.shutdown().await;
}

#[tokio::test]
async fn local_websocket_expiry_and_cancellation_do_not_capture_received_close() {
    let server = test_server().await;
    let directory = tempfile::tempdir().unwrap();
    let (observation, observer) = capture_observer(directory.path(), true).await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()))
        .with_observer(Some(observer));
    let socket = checkout(&network, &server.url).await;
    // Admission persists asynchronously through SQLx. Settle it before advancing
    // Tokio's clock so the transport-age probe cannot expire its pool-acquire timeout.
    observation.flush().await.unwrap();
    assert_eq!(
        observation
            .query_forest(crate::interaction_observation::ForestQuery::default())
            .await
            .unwrap()
            .roots
            .len(),
        1,
        "WebSocket capture run must be admitted before the expiry probe"
    );
    tokio::time::pause();
    tokio::time::advance(MAX_WEBSOCKET_AGE).await;
    let expired = socket.next().await.unwrap_err();
    tokio::time::resume();
    assert!(expired.message.contains("maximum age"));
    assert!(
        socket
            .next()
            .await
            .unwrap_err()
            .message
            .contains("maximum age")
    );
    let socket = checkout(&network, &server.url).await;
    network.cancellation.cancel();
    assert!(matches!(
        socket.next().await.unwrap_err().kind,
        stravia_vendor_sdk::ErrorKind::Cancelled
    ));
    drop(socket);
    observation.flush().await.unwrap();
    assert!(!captured_records(&observation).await.iter().any(|record| {
        record["direction"] == "upstream_response" && record["message_type"] == "close"
    }));
    observation.shutdown().await;
}

#[tokio::test]
async fn debug_disabled_websocket_close_retains_failure_without_wire_capture() {
    let server = test_server().await;
    let directory = tempfile::tempdir().unwrap();
    let (observation, observer) = capture_observer(directory.path(), false).await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()))
        .with_observer(Some(observer));
    round_trip_and_park(&network, &server.url, "first").await;
    let socket = require_tip(&network, &server.url, "first", Vec::new())
        .await
        .unwrap();
    socket
        .send(WebSocketMessage::Text("close-before-response".into()))
        .await
        .unwrap();
    assert_eq!(
        socket.next().await.unwrap_err().kind.transport_failure(),
        Some(stravia_vendor_sdk::TransportFailure::Websocket)
    );
    drop(socket);
    observation.flush().await.unwrap();
    assert!(captured_records(&observation).await.is_empty());
    observation.shutdown().await;
}

#[tokio::test]
async fn local_websocket_close_is_only_an_outgoing_wire_frame() {
    let server = test_server().await;
    let directory = tempfile::tempdir().unwrap();
    let (observation, observer) = capture_observer(directory.path(), true).await;
    // 无池的显式释放会真正发送 Close；归池不应伪造该帧。
    let mut network = network(&server.url, Arc::new(VendorWebSocketPool::default()))
        .with_observer(Some(observer));
    network.websocket_pool = None;
    let socket = checkout(&network, &server.url).await;
    socket.close(None).await.unwrap();
    socket.close(None).await.unwrap();
    observation.flush().await.unwrap();
    let records = captured_records(&observation).await;
    let closes: Vec<_> = records
        .iter()
        .filter(|record| record["message_type"] == "close")
        .collect();
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0]["direction"], "upstream_request");
    assert_eq!(
        closes[0]["payload"],
        serde_json::json!({"code": 1000, "reason": ""})
    );
    observation.shutdown().await;
}

#[test]
fn websocket_transport_diagnostics_redact_source_chain_secrets() {
    #[derive(Debug)]
    struct SyntheticFailure(std::io::Error);
    impl std::fmt::Display for SyntheticFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("synthetic receive error")
        }
    }
    impl std::error::Error for SyntheticFailure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    let error = SyntheticFailure(std::io::Error::other(
        "https://synthetic.invalid/socket?api_key=synthetic-secret",
    ));
    let cause = super::transport_cause(&error);
    assert!(cause.contains("synthetic receive error"));
    assert!(!cause.contains("synthetic-secret"));
}

#[tokio::test]
async fn websocket_receive_failure_preserves_the_underlying_close_reason() {
    let server = test_server().await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()));
    let socket = checkout(&network, &server.url).await;
    socket
        .send(WebSocketMessage::Text("disconnect-without-close".into()))
        .await
        .expect("ask local upstream to disconnect without a closing handshake");

    let failure = socket
        .next()
        .await
        .expect_err("an abnormal upstream close must fail the response stream");
    assert!(
        failure.message.contains("without closing handshake"),
        "the failure must distinguish an abnormal close from other WebSocket errors: {}",
        failure.message
    );
}

#[tokio::test]
async fn continuation_requires_exact_idle_tip_and_moves_forward_on_completion() {
    let server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool);

    let missing = require_tip(&network, &server.url, "first", Vec::new())
        .await
        .err()
        .expect("no socket for requested tip");
    assert_local_miss(missing);
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);

    round_trip_and_park(&network, &server.url, "first").await;
    let wrong = require_tip(&network, &server.url, "other", Vec::new())
        .await
        .err()
        .expect("another response ID cannot borrow this socket");
    assert_local_miss(wrong);
    let socket = require_tip(&network, &server.url, "first", Vec::new())
        .await
        .expect("exact socket tip is available");
    let occupied = require_tip(&network, &server.url, "first", Vec::new())
        .await
        .err()
        .expect("checkout is exclusive");
    assert_local_miss(occupied);
    complete_and_park(&socket, "second", Some("second")).await;
    let stale = require_tip(&network, &server.url, "first", Vec::new())
        .await
        .err()
        .expect("old response ID is no longer a tip");
    assert_local_miss(stale);
    let socket = require_tip(&network, &server.url, "second", Vec::new())
        .await
        .expect("latest response ID is available");
    complete_and_park(&socket, "third", None).await;
    let cleared = require_tip(&network, &server.url, "second", Vec::new())
        .await
        .err()
        .expect("close without a completed response clears previous tip");
    assert_local_miss(cleared);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn continuation_does_not_cross_scope_affinity_or_credentials() {
    let server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, Arc::clone(&pool));
    let headers: Vec<(String, String)> = vec![("authorization".into(), "Bearer first".into())];
    let mut fields = Fields::new();
    for (name, value) in &headers {
        fields.append(name.clone(), value.as_bytes().to_vec());
    }
    let socket = network
        .ws_connect(
            ConnectRequest::new(&server.url).fields(fields),
            Options::new(),
        )
        .await
        .expect("open credential-bound socket");
    complete_and_park(&socket, "response", Some("response")).await;

    for foreign in [
        network.clone().with_websocket_pool(
            Arc::clone(&pool),
            "another-account".into(),
            Some("response-id".into()),
        ),
        network.clone().with_websocket_pool(
            Arc::clone(&pool),
            "test-scope".into(),
            Some("another-affinity".into()),
        ),
    ] {
        let error = require_tip(&foreign, &server.url, "response", headers.clone())
            .await
            .err()
            .expect("another account or response affinity cannot resume");
        assert_local_miss(error);
    }
    let credential_error = require_tip(
        &network,
        &server.url,
        "response",
        vec![("authorization".into(), "Bearer second".into())],
    )
    .await
    .err()
    .expect("different credentials cannot borrow this socket");
    assert_local_miss(credential_error);
    let same = require_tip(&network, &server.url, "response", headers)
        .await
        .expect("original credential-bound socket remains available");
    complete_and_park(&same, "next", None).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_or_server_closed_tip_never_opens_a_replacement_connection() {
    let mut server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool);
    round_trip_and_park(&network, &server.url, "old").await;
    tokio::time::pause();
    tokio::time::advance(MAX_WEBSOCKET_AGE).await;
    tokio::time::resume();
    let error = require_tip(&network, &server.url, "old", Vec::new())
        .await
        .err()
        .expect("expired tip cannot be resumed");
    assert_local_miss(error);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("expired socket was released")
        .expect("upstream observed expired socket");

    let socket = checkout(&network, &server.url).await;
    complete_and_park(&socket, "close-after-ack", Some("closed")).await;
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("server closed one socket")
        .expect("upstream close notification");
    let error = require_tip(&network, &server.url, "closed", Vec::new())
        .await
        .err()
        .expect("closed tip cannot be resumed");
    assert_local_miss(error);
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn evicted_tip_cannot_be_resumed_even_while_newer_tip_remains() {
    let server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool);
    let mut sockets = Vec::new();
    for _ in 0..=super::MAX_IDLE_WEBSOCKETS {
        sockets.push(checkout(&network, &server.url).await);
    }
    let oldest = sockets.remove(0);
    complete_and_park(&oldest, "oldest", Some("oldest")).await;
    for (index, socket) in sockets.iter().enumerate() {
        let id = format!("new-{index}");
        complete_and_park(socket, &id, Some(&id)).await;
    }
    let error = require_tip(&network, &server.url, "oldest", Vec::new())
        .await
        .err()
        .expect("evicted socket tip is gone");
    assert_local_miss(error);
    let newest = require_tip(
        &network,
        &server.url,
        &format!("new-{}", super::MAX_IDLE_WEBSOCKETS - 1),
        Vec::new(),
    )
    .await
    .expect("recent socket tip remains available");
    newest.close(None).await.expect("release recent socket");
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        super::MAX_IDLE_WEBSOCKETS + 1
    );
}

const GZIP_BODY: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 0, 10, 75, 73, 77, 206, 79, 73, 77, 81, 40, 45, 40, 46, 41, 74, 77,
    204, 85, 72, 202, 79, 169, 4, 0, 62, 59, 204, 84, 21, 0, 0, 0,
];

async fn read_all(response: &dyn stravia_vendor_runtime::HostHttpResponse) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(chunk) = response.read_body().await.expect("read upstream body") {
        body.extend(chunk);
    }
    body
}

#[tokio::test]
async fn compression_policy_isolates_pool_checkout_and_continuation() {
    let server = test_server().await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()));
    for (compression, tip) in [
        (WsCompression::Disabled, "disabled"),
        (WsCompression::Preferred, "preferred"),
    ] {
        let mut fields = Fields::new();
        if compression == WsCompression::Preferred {
            fields.append(
                "Sec-WebSocket-Extensions",
                b"permessage-deflate; client_max_window_bits".to_vec(),
            );
        }
        for _ in 0..2 {
            let mut options = Options::new();
            options.ws_compression(compression).unwrap();
            let socket = network
                .ws_connect(
                    ConnectRequest::new(&server.url).fields(fields.clone()),
                    options,
                )
                .await
                .unwrap();
            complete_and_park(&socket, tip, Some(tip)).await;
        }
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    let mut options = Options::new();
    options.ws_compression(WsCompression::Required).unwrap();
    options.continuation("preferred").unwrap();
    let mut fields = Fields::new();
    fields.append(
        "Sec-WebSocket-Extensions",
        b"permessage-deflate; client_max_window_bits".to_vec(),
    );
    let error = network
        .ws_connect(ConnectRequest::new(&server.url).fields(fields), options)
        .await
        .err()
        .expect("compression policy cannot cross a socket tip");
    assert_local_miss(error);
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn websocket_extension_policy_is_validated_before_pool_reuse() {
    let server = test_server().await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()));
    let mut fields = Fields::new();
    fields.append(
        "Sec-WebSocket-Extensions",
        b"permessage-deflate; client_max_window_bits".to_vec(),
    );
    let mut preferred = Options::new();
    preferred.ws_compression(WsCompression::Preferred).unwrap();
    let socket = network
        .ws_connect(
            ConnectRequest::new(&server.url).fields(fields.clone()),
            preferred.clone(),
        )
        .await
        .unwrap();
    complete_and_park(&socket, "offered", Some("offered")).await;
    let mut disabled = Options::new();
    disabled.ws_compression(WsCompression::Disabled).unwrap();
    assert!(
        network
            .ws_connect(
                ConnectRequest::new(&server.url).fields(fields.clone()),
                disabled
            )
            .await
            .is_err()
    );
    let socket = network
        .ws_connect(ConnectRequest::new(&server.url).fields(fields), preferred)
        .await
        .unwrap();
    complete_and_park(&socket, "reused", None).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

// Capture bytes from an actual HTTP/1 peer rather than a normalized HeaderMap.
async fn capture_request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 65536, "request head exceeds test bound");
    }
    let head = String::from_utf8(bytes).unwrap();
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    (head, body)
}

fn http1() -> Options {
    let mut options = Options::new();
    options.http_version(HttpVersion::Http1).unwrap();
    options
}

#[tokio::test]
async fn http_wire_preserves_accept_absence_order_duplicates_and_framing_slots() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = capture_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
            .await
            .unwrap();
        request
    });
    let url = format!("http://{address}/ordered");
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let mut fields = Fields::new();
    fields.append("X-First", b"one".to_vec());
    fields.slot("Host");
    fields.append("X-Repeat", b"first".to_vec());
    fields.slot("Content-Length");
    fields.append("x-repeat", b"second".to_vec());
    fields.append("X-Last", b"end".to_vec());
    let response = network
        .http_start(
            Request::new("POST", url)
                .fields(fields)
                .body(b"abc".to_vec()),
            http1(),
        )
        .unwrap();
    assert_eq!(response.status().await.unwrap(), 200);
    assert_eq!(read_all(response.as_ref()).await, b"OK");
    let (head, body) = server.await.unwrap();
    assert_eq!(
        head,
        format!(
            "POST /ordered HTTP/1.1\r\nX-First: one\r\nHost: {address}\r\nX-Repeat: first\r\nContent-Length: 3\r\nx-repeat: second\r\nX-Last: end\r\n\r\n"
        )
    );
    assert_eq!(body, b"abc");
}

#[tokio::test]
async fn explicit_accept_and_compression_are_sent_and_decoded() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = capture_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                    GZIP_BODY.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(GZIP_BODY).await.unwrap();
        request
    });
    let url = format!("http://{address}/gzip");
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let mut fields = Fields::new();
    fields.append("Accept", b"application/json".to_vec());
    fields.append("Accept-Encoding", b"gzip".to_vec());
    let mut options = http1();
    options.response_decoding(&[Coding::Gzip]).unwrap();
    let response = network
        .http_start(Request::new("GET", url).fields(fields), options)
        .unwrap();
    assert_eq!(response.status().await.unwrap(), 200);
    assert_eq!(read_all(response.as_ref()).await, b"decoded upstream body");
    let headers = response.headers().await.unwrap();
    assert!(
        !headers
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("content-encoding"))
    );
    let (head, _) = server.await.unwrap();
    assert!(
        head.contains("Accept: application/json\r\nAccept-Encoding: gzip\r\n"),
        "{head:?}"
    );
}

#[tokio::test]
async fn explicit_accept_encoding_order_and_duplicates_are_not_rewritten() {
    use tokio::io::AsyncWriteExt;
    for (value, codings) in [
        (
            "br, gzip, deflate",
            vec![Coding::Br, Coding::Gzip, Coding::Deflate],
        ),
        (
            "zstd, br, zstd, gzip",
            vec![Coding::Zstd, Coding::Br, Coding::Gzip],
        ),
        ("identity", Vec::new()),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = capture_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .await
                .unwrap();
            request
        });
        let network = network(&url, Arc::new(VendorWebSocketPool::default()));
        let mut fields = Fields::new();
        fields.append("Accept-Encoding", value.as_bytes().to_vec());
        let mut options = http1();
        options.response_decoding(&codings).unwrap();
        let response = network
            .http_start(Request::new("GET", url).fields(fields), options)
            .unwrap();
        assert_eq!(response.status().await.unwrap(), 204);
        let (head, _) = server.await.unwrap();
        assert!(
            head.contains(&format!("Accept-Encoding: {value}\r\n")),
            "{head:?}"
        );
        assert_eq!(
            head.to_ascii_lowercase()
                .matches("accept-encoding:")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn compression_declarations_are_required_and_wire_response_can_remain_encoded() {
    let app = Router::new()
        .route(
            "/gzip",
            get(|| async { ([("content-encoding", "gzip")], GZIP_BODY) }),
        )
        .route(
            "/unknown",
            get(|| async { ([("content-encoding", "custom")], GZIP_BODY) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/gzip", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let plain = network
        .http_start(Request::new("GET", &url), http1())
        .unwrap();
    assert_eq!(read_all(plain.as_ref()).await, GZIP_BODY);
    let unknown_url = url.replace("/gzip", "/unknown");
    let unknown = network
        .http_start(Request::new("GET", unknown_url), http1())
        .unwrap();
    let headers = unknown.headers().await.unwrap();
    assert!(
        headers
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("content-encoding") && f.bytes == b"custom")
    );
    assert_eq!(read_all(unknown.as_ref()).await, GZIP_BODY);
    let mut decoding = http1();
    decoding.response_decoding(&[Coding::Gzip]).unwrap();
    assert!(
        network
            .http_start(Request::new("GET", &url), decoding)
            .is_err()
    );
    let mut encoding = http1();
    encoding.request_encoding(Coding::Gzip).unwrap();
    assert!(
        network
            .http_start(
                Request::new("POST", &url).body(b"body".to_vec()),
                encoding.clone()
            )
            .is_err()
    );
    let mut fields = Fields::new();
    fields.append("Content-Encoding", b"br".to_vec());
    assert!(
        network
            .http_start(
                Request::new("POST", &url)
                    .fields(fields)
                    .body(b"body".to_vec()),
                encoding
            )
            .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn fixed_length_redirects_preserve_body_and_rewrite_removes_body_headers() {
    let (tx, mut rx) = mpsc::channel(2);
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let tx = tx.clone();
        async move {
            let (parts, body) = request.into_parts();
            let path = parts.uri.path().to_owned();
            let body = axum::body::to_bytes(body, 1024).await.unwrap();
            if path == "/temporary" {
                return axum::response::Redirect::temporary("/received").into_response();
            }
            if path == "/see-other" {
                return axum::response::Redirect::to("/received").into_response();
            }
            tx.send((parts, body)).await.unwrap();
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let network = network(&base, Arc::new(VendorWebSocketPool::default()));
    for path in ["temporary", "see-other"] {
        let mut fields = Fields::new();
        fields.append("Content-Type", b"application/json".to_vec());
        let response = network
            .http_start(
                Request::new("POST", format!("{base}/{path}"))
                    .fields(fields)
                    .body(b"payload".to_vec()),
                http1(),
            )
            .unwrap();
        assert_eq!(response.status().await.unwrap(), 204);
        let (parts, body) = rx.recv().await.unwrap();
        if path == "temporary" {
            assert_eq!(parts.method, axum::http::Method::POST);
            assert_eq!(body.as_ref(), b"payload");
            assert_eq!(parts.headers["content-type"], "application/json");
        } else {
            assert_eq!(parts.method, axum::http::Method::GET);
            assert!(body.is_empty());
            assert!(!parts.headers.contains_key("content-type"));
            assert!(!parts.headers.contains_key("content-length"));
        }
    }
    server.abort();
}

#[tokio::test]
async fn cross_origin_redirect_with_body_is_rejected_before_destination_io() {
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination_url = format!("http://{}/received", destination.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        axum::routing::post(move || {
            let destination_url = destination_url.clone();
            async move { axum::response::Redirect::temporary(&destination_url) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let destination_origin = format!("http://{}", destination.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let source_origin = url::Url::parse(&url)
        .unwrap()
        .origin()
        .ascii_serialization();
    network.origins = crate::plugin::permissions::NetworkPolicy::compile([
        crate::plugin::permissions::NetworkOrigin::Exact(source_origin),
        crate::plugin::permissions::NetworkOrigin::Exact(destination_origin),
    ])
    .unwrap();
    let response = network
        .http_start(Request::new("POST", url).body(b"private".to_vec()), http1())
        .unwrap();
    assert!(response.status().await.is_err());
    assert_eq!(
        destination.into_std().unwrap().accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    server.abort();
}

#[tokio::test]
async fn invalid_request_fields_fail_before_connecting() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    for (name, value) in [
        ("bad name", &b"x"[..]),
        ("x-header", &b"bad\r\nvalue"[..]),
        ("Proxy-Authorization", &b"secret"[..]),
        ("Host", &b"other.example"[..]),
        ("Content-Length", &b"99"[..]),
    ] {
        let mut fields = Fields::new();
        fields.append(name, value.to_vec());
        let failure = match network.http_start(
            Request::new("POST", &url)
                .fields(fields)
                .body(b"body".to_vec()),
            http1(),
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("invalid {name} accepted"),
        };
        assert!(matches!(failure.kind, ErrorKind::Invalid), "{name}");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_websocket_fields_fail_before_connecting() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    for (name, value) in [
        ("Host", "other.example"),
        ("Proxy-Connection", "close"),
        ("Sec-WebSocket-Key", "not-a-slot"),
    ] {
        let mut fields = Fields::new();
        fields.append(name, value.as_bytes().to_vec());
        let failure = match network
            .ws_connect(ConnectRequest::new(&url).fields(fields), Options::new())
            .await
        {
            Err(failure) => failure,
            Ok(_) => panic!("invalid {name} accepted"),
        };
        assert!(matches!(failure.kind, ErrorKind::Invalid), "{name}");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn compressed_content_length_is_checked_against_wire_body() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let body =
        br#"{"messages":[{"role":"user","content":"synthetic payload"}],"stream":true}"#.to_vec();
    let mut options = http1();
    options.request_encoding(Coding::Gzip).unwrap();
    let encoded = http::encode(body.clone(), &options).await.unwrap();
    assert_ne!(encoded.len(), body.len());

    let mut malformed = Fields::new();
    malformed.append("Content-Encoding", b"gzip".to_vec());
    malformed.append("Content-Length", b"not-a-length".to_vec());
    let failure = match network.http_start(
        Request::new("POST", &url)
            .fields(malformed)
            .body(body.clone()),
        options.clone(),
    ) {
        Err(failure) => failure,
        Ok(_) => panic!("malformed length admitted while awaiting content encoding"),
    };
    assert!(matches!(failure.kind, ErrorKind::Invalid));

    let mut fields = Fields::new();
    fields.append("Content-Encoding", b"gzip".to_vec());
    fields.append("Content-Length", body.len().to_string().into_bytes());
    let response = network
        .http_start(
            Request::new("POST", &url).fields(fields).body(body.clone()),
            options.clone(),
        )
        .unwrap();
    let failure = match response.status().await {
        Err(failure) => failure,
        Ok(_) => panic!("bad encoded length accepted"),
    };
    assert!(matches!(failure.kind, ErrorKind::Invalid));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = capture_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .await
            .unwrap();
        request
    });
    let mut fields = Fields::new();
    fields.append("Accept", b"text/event-stream".to_vec());
    fields.append("Content-Encoding", b"gzip".to_vec());
    fields.slot("Content-Length");
    fields.append("Accept-Encoding", b"gzip, compress, deflate, br".to_vec());
    fields.slot("Host");
    options
        .response_decoding(&[Coding::Gzip, Coding::Deflate, Coding::Br])
        .unwrap();
    let response = network
        .http_start(
            Request::new("POST", &url).fields(fields).body(body.clone()),
            options,
        )
        .unwrap();
    assert_eq!(response.status().await.unwrap(), 204);
    let (head, wire_body) = server.await.unwrap();
    assert_eq!(
        head,
        format!(
            "POST / HTTP/1.1\r\nAccept: text/event-stream\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nAccept-Encoding: gzip, compress, deflate, br\r\nHost: {}\r\n\r\n",
            wire_body.len(),
            url::Url::parse(&url).unwrap().authority(),
        )
    );
    assert_eq!(wire_body, encoded.as_ref());
    use tokio::io::AsyncReadExt;
    let mut decoder =
        async_compression::tokio::bufread::GzipDecoder::new(std::io::Cursor::new(&wire_body));
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).await.unwrap();
    assert_eq!(decoded, body);
    assert_eq!(decoder.into_inner().position() as usize, wire_body.len());
}

#[tokio::test]
async fn chunked_wire_framing_and_invalid_header_combinations() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for (payload, encoding) in [
            (Vec::new(), ""),
            (Vec::new(), "chunked"),
            (
                (0..=255).cycle().take(128 * 1024 + 19).collect::<Vec<u8>>(),
                "",
            ),
            (
                (0..=255).cycle().take(128 * 1024 + 19).collect::<Vec<u8>>(),
                "chunked",
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/chunked", listener.local_addr().unwrap());
            let network = network(&url, Arc::new(VendorWebSocketPool::default()));
            for invalid in [
                vec![("Transfer-Encoding", "gzip")],
                vec![("Transfer-Encoding", " ")],
                vec![("Transfer-Encoding", "chunked"), ("Content-Length", "7")],
                vec![("Transfer-Encoding", ""), ("Content-Length", "7")],
                vec![
                    ("Transfer-Encoding", "chunked"),
                    ("transfer-encoding", "chunked"),
                ],
            ] {
                let mut fields = Fields::new();
                for (name, value) in invalid {
                    fields.append(name, value.as_bytes().to_vec());
                }
                let failure = match network.http_start(
                    Request::new("POST", &url)
                        .fields(fields)
                        .body(b"payload".to_vec()),
                    http1(),
                ) {
                    Err(failure) => failure,
                    Ok(_) => panic!("invalid fields accepted"),
                };
                assert!(matches!(failure.kind, ErrorKind::Invalid));
                assert!(
                    tokio::time::timeout(Duration::from_millis(10), listener.accept())
                        .await
                        .is_err()
                );
            }
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                let mut head = String::new();
                loop {
                    line.clear();
                    assert_ne!(
                        socket.read_line(&mut line).await.unwrap(),
                        0,
                        "EOF before end of request head"
                    );
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = Vec::new();
                loop {
                    line.clear();
                    assert_ne!(
                        socket.read_line(&mut line).await.unwrap(),
                        0,
                        "EOF before final chunk"
                    );
                    let size = usize::from_str_radix(line.trim(), 16).unwrap();
                    let start = body.len();
                    body.resize(start + size, 0);
                    socket.read_exact(&mut body[start..]).await.unwrap();
                    let mut crlf = [0; 2];
                    socket.read_exact(&mut crlf).await.unwrap();
                    assert_eq!(&crlf, b"\r\n");
                    if size == 0 {
                        break;
                    }
                }
                socket
                    .get_mut()
                    .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                    .await
                    .unwrap();
                (head, body)
            });
            let mut fields = Fields::new();
            fields.append("X-Before", b"one".to_vec());
            fields.append("tRaNsFeR-EnCoDiNg", encoding.as_bytes().to_vec());
            fields.append("x-After", b"two".to_vec());
            let response = network
                .http_start(
                    Request::new("POST", url)
                        .fields(fields)
                        .body(payload.clone()),
                    http1(),
                )
                .unwrap();
            assert_eq!(response.status().await.unwrap(), 204);
            let (head, body) = server.await.unwrap();
            assert!(
                head.contains("X-Before: one\r\ntRaNsFeR-EnCoDiNg: chunked\r\nx-After: two\r\n"),
                "{head:?}"
            );
            assert!(!head.to_ascii_lowercase().contains("content-length:"));
            assert_eq!(body, payload);
        }
    })
    .await
    .expect("chunked request did not complete");
}

#[tokio::test]
async fn chunked_redirect_preserves_encoded_body_or_removes_all_body_headers() {
    use tokio::io::AsyncReadExt;
    let (tx, mut rx) = mpsc::channel(2);
    let app = Router::new()
        .route(
            "/temporary",
            axum::routing::post(|| async { axum::response::Redirect::temporary("/received") }),
        )
        .route(
            "/see-other",
            axum::routing::post(|| async { axum::response::Redirect::to("/received") }),
        )
        .route(
            "/received",
            axum::routing::any(
                move |method: axum::http::Method,
                      headers: axum::http::HeaderMap,
                      body: bytes::Bytes| {
                    let tx = tx.clone();
                    async move {
                        tx.send((method, headers, body)).await.unwrap();
                        axum::http::StatusCode::NO_CONTENT
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let network = network(&base, Arc::new(VendorWebSocketPool::default()));
    let original = b"preserve an encoded body across redirects";
    for path in ["temporary", "see-other"] {
        let mut fields = Fields::new();
        fields.append("Transfer-Encoding", b"chunked".to_vec());
        fields.append("Content-Type", b"application/json".to_vec());
        fields.append("Content-Encoding", b"gzip".to_vec());
        let mut options = http1();
        options.request_encoding(Coding::Gzip).unwrap();
        let response = network
            .http_start(
                Request::new("POST", format!("{base}/{path}"))
                    .fields(fields)
                    .body(original.to_vec()),
                options,
            )
            .unwrap();
        assert_eq!(response.status().await.unwrap(), 204);
        let (method, headers, body) = rx.recv().await.unwrap();
        if path == "temporary" {
            assert_eq!(method, axum::http::Method::POST);
            assert_eq!(headers["transfer-encoding"], "chunked");
            assert!(!headers.contains_key("content-length"));
            assert_eq!(headers["content-encoding"], "gzip");
            let mut decoded = Vec::new();
            async_compression::tokio::bufread::GzipDecoder::new(body.as_ref())
                .read_to_end(&mut decoded)
                .await
                .unwrap();
            assert_eq!(decoded, original);
        } else {
            assert_eq!(method, axum::http::Method::GET);
            assert!(body.is_empty());
            assert!(!headers.contains_key("transfer-encoding"));
            assert!(!headers.contains_key("content-encoding"));
            assert!(!headers.contains_key("content-type"));
        }
    }
    server.abort();
}

#[tokio::test]
async fn gzip_request_encoding_survives_a_307_redirect() {
    use tokio::io::AsyncReadExt;
    let (tx, mut rx) = mpsc::channel(1);
    let app = Router::new()
        .route(
            "/redirect",
            axum::routing::post(|| async { axum::response::Redirect::temporary("/received") }),
        )
        .route(
            "/received",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: bytes::Bytes| {
                let tx = tx.clone();
                async move {
                    tx.send((headers, body)).await.unwrap();
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let network = network(&base, Arc::new(VendorWebSocketPool::default()));
    let mut fields = Fields::new();
    fields.append("Content-Encoding", b"gzip".to_vec());
    let mut options = http1();
    options.request_encoding(Coding::Gzip).unwrap();
    let response = network
        .http_start(
            Request::new("POST", format!("{base}/redirect"))
                .fields(fields)
                .body(b"replay this body".to_vec()),
            options,
        )
        .unwrap();
    assert_eq!(response.status().await.unwrap(), 204);
    let (headers, body) = rx.recv().await.unwrap();
    assert_eq!(headers["content-encoding"], "gzip");
    let mut decoded = Vec::new();
    async_compression::tokio::bufread::GzipDecoder::new(body.as_ref())
        .read_to_end(&mut decoded)
        .await
        .unwrap();
    assert_eq!(decoded, b"replay this body");
    server.abort();
}

#[tokio::test]
async fn decoded_response_rejects_unknown_and_truncated_codings() {
    let app = Router::new()
        .route(
            "/unknown",
            get(|| async { ([("content-encoding", "custom")], GZIP_BODY) }),
        )
        .route(
            "/truncated",
            get(|| async {
                (
                    [("content-encoding", "gzip")],
                    &GZIP_BODY[..GZIP_BODY.len() - 5],
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let network = network(&base, Arc::new(VendorWebSocketPool::default()));
    let mut fields = Fields::new();
    fields.append("Accept-Encoding", b"gzip".to_vec());
    let mut options = http1();
    options.response_decoding(&[Coding::Gzip]).unwrap();
    let unknown = network
        .http_start(
            Request::new("GET", format!("{base}/unknown")).fields(fields.clone()),
            options.clone(),
        )
        .unwrap();
    assert!(unknown.status().await.is_err());
    let truncated = network
        .http_start(
            Request::new("GET", format!("{base}/truncated")).fields(fields),
            options,
        )
        .unwrap();
    assert_eq!(truncated.status().await.unwrap(), 200);
    loop {
        match truncated.read_body().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("truncated gzip must not be reported as EOF"),
            Err(_) => break,
        }
    }
    server.abort();
}

#[tokio::test]
async fn gzip_response_is_decoded_in_bounded_chunks() {
    use tokio::io::AsyncWriteExt;
    let expected = vec![b'x'; 128 * 1024 + 17];
    let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    encoder.write_all(&expected).await.unwrap();
    encoder.shutdown().await.unwrap();
    let compressed = encoder.into_inner();
    let app = Router::new().route(
        "/",
        get(move || {
            let compressed = compressed.clone();
            async move { ([("content-encoding", "gzip")], compressed) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let mut fields = Fields::new();
    fields.append("Accept-Encoding", b"gzip".to_vec());
    let mut options = http1();
    options.response_decoding(&[Coding::Gzip]).unwrap();
    let response = network
        .http_start(Request::new("GET", url).fields(fields), options)
        .unwrap();
    let mut decoded = Vec::new();
    let mut chunks = 0;
    while let Some(chunk) = response.read_body().await.unwrap() {
        assert!(!chunk.is_empty() && chunk.len() <= 16 * 1024);
        chunks += 1;
        decoded.extend(chunk);
    }
    assert!(chunks > 1);
    assert_eq!(decoded, expected);
    server.abort();
}

#[tokio::test]
async fn host_validates_options_before_http_or_websocket_io() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/");
    let host_network = network(&url, Arc::new(VendorWebSocketPool::default()));
    assert!(host_network.validate_net_option("shape", "default").is_ok());
    for (key, value) in [
        ("shape", "does-not-exist"),
        ("http.version", "http3"),
        ("ws.reuse-max-age-millis", "18446744073709551616"),
        ("future.option", "x"),
    ] {
        assert!(
            host_network.validate_net_option(key, value).is_err(),
            "{key}={value}"
        );
    }
    let mut options = Options::new();
    options.set("future.option", "x").unwrap();
    assert!(
        host_network
            .http_start(Request::new("GET", &url), options.clone())
            .is_err()
    );
    let ws_url = format!("ws://{address}/");
    let ws_network = network(&ws_url, Arc::new(VendorWebSocketPool::default()));
    assert!(
        ws_network
            .ws_connect(ConnectRequest::new(ws_url), options)
            .await
            .is_err()
    );
    assert_eq!(
        listener.into_std().unwrap().accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn cancellation_interrupts_pending_http_response() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        capture_request(&mut socket).await;
        let _ = seen_tx.send(());
        std::future::pending::<()>().await;
    });
    let response = network
        .http_start(Request::new("GET", url), http1())
        .unwrap();
    let waiting = tokio::spawn(async move { response.status().await });
    tokio::time::timeout(Duration::from_secs(2), seen_rx)
        .await
        .unwrap()
        .unwrap();
    network.cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        error.kind,
        stravia_vendor_sdk::ErrorKind::Cancelled
    ));
    server.abort();
}

#[test]
fn wire_payloads_preserve_text_media_and_non_utf8_websocket_messages() {
    let media = r#"{"image":"data:image/png;base64,AAECA/8="}"#;
    assert_eq!(
        bytes_value(media.as_bytes()),
        serde_json::Value::String(media.into())
    );
    assert_eq!(
        bytes_value(&[0xff, 0x00, 0x80]),
        serde_json::json!({"encoding": "base64", "data": "/wCA"})
    );

    let text = WebSocketMessage::Text("ws-雪".into());
    assert_eq!(ws_message_type(&text), "text");
    assert_eq!(ws_payload(&text), "ws-雪");
    let binary = WebSocketMessage::Binary(vec![0xff, 0x00, 0x80]);
    assert_eq!(ws_message_type(&binary), "binary");
    assert_eq!(
        ws_payload(&binary),
        serde_json::json!({"encoding": "base64", "data": "/wCA"})
    );
}

#[tokio::test]
async fn requested_age_retires_original_connection_and_requires_full_request() {
    let mut server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool);

    round_trip_and_park_with_age(&network, &server.url, "first", Some(55 * 60 * 1000)).await;
    for (elapsed, previous, next) in [
        (20 * 60, "first", "second"),
        (20 * 60, "second", "third"),
        (14 * 60, "third", "latest"),
    ] {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(elapsed)).await;
        tokio::time::resume();
        let socket = require_tip_with_age(
            &network,
            &server.url,
            previous,
            Vec::new(),
            Some(60 * 60 * 1000),
        )
        .await
        .expect("continuation available before requested retirement");
        complete_and_park(&socket, next, Some(next)).await;
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    assert!(server.closed.try_recv().is_err());

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("idle connection released at its original requested 55-minute deadline")
        .expect("upstream observed retirement without a new request");
    let error = require_tip(&network, &server.url, "latest", Vec::new())
        .await
        .err()
        .expect("retired connection-local tip must be unavailable");
    assert_local_miss(error);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);

    round_trip_and_park(&network, &server.url, "full-request").await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn requested_age_does_not_expire_default_upstream_connections() {
    let server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let generic = network(&server.url, Arc::clone(&pool));
    let requested = generic.clone().with_websocket_pool(
        pool,
        "test-scope".into(),
        Some("another-affinity".into()),
    );
    round_trip_and_park(&generic, &server.url, "generic-tip").await;
    round_trip_and_park_with_age(
        &requested,
        &server.url,
        "requested-tip",
        Some(55 * 60 * 1000),
    )
    .await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(55 * 60)).await;
    tokio::time::resume();

    let socket = require_tip(&generic, &server.url, "generic-tip", Vec::new())
        .await
        .expect("generic upstream retains its 60-minute lifetime");
    complete_and_park(&socket, "generic-still-live", Some("generic-still-live")).await;
    let error = require_tip(&requested, &server.url, "requested-tip", Vec::new())
        .await
        .err()
        .expect("requested-age tip retired independently in the shared pool");
    assert_local_miss(error);
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn requested_age_inflight_response_completes_without_reparking_retired_tip() {
    let mut server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool);
    round_trip_and_park_with_age(&network, &server.url, "original-tip", Some(55 * 60 * 1000)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(54 * 60)).await;
    tokio::time::resume();
    let socket = require_tip(&network, &server.url, "original-tip", Vec::new())
        .await
        .expect("checkout before retirement");
    socket
        .send(WebSocketMessage::Text("delayed-response".into()))
        .await
        .expect("start response before retirement");
    server.response_waiting.notified().await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2 * 60)).await;
    tokio::time::resume();
    assert!(
        server.closed.try_recv().is_err(),
        "retirement must not close an in-flight response"
    );
    let error = require_tip(&network, &server.url, "original-tip", Vec::new())
        .await
        .err()
        .expect("exclusive checkout leaves no available original tip");
    assert_local_miss(error);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    round_trip_and_park(&network, &server.url, "new-tip").await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);

    server.response_release.notify_one();
    assert!(matches!(
        socket.next().await.expect("finish response after 55 minutes"),
        Some(WebSocketMessage::Text(text)) if text == "ack:delayed-response"
    ));
    let (released, resumed) = tokio::join!(
        socket.close(Some("expired-tip".into())),
        require_tip(&network, &server.url, "expired-tip", Vec::new()),
    );
    released.expect("release expired response");
    assert!(
        !network
            .response_continuation_available
            .load(Ordering::Acquire)
    );
    let error = resumed
        .err()
        .expect("expired park must not restore a continuation tip");
    assert_local_miss(error);
    let fresh = require_tip(&network, &server.url, "new-tip", Vec::new())
        .await
        .expect("concurrent fresh connection remains reusable");
    complete_and_park(&fresh, "fresh-continuation", Some("fresh-continuation")).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn requested_age_active_connection_keeps_sixty_minute_limit() {
    let server = test_server().await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()));
    let socket = checkout_with_age(&network, &server.url, Some(55 * 60 * 1000)).await;
    tokio::time::pause();
    tokio::time::advance(MAX_WEBSOCKET_AGE).await;
    let error = socket
        .send(WebSocketMessage::Text("past-active-limit".into()))
        .await
        .expect_err("requested reuse age retains the 60-minute active hard limit");
    tokio::time::resume();
    assert_eq!(
        error.kind.model_error_kind(),
        Some(AiErrorKind::ServiceUnavailable)
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn requested_age_zero_disables_reuse_and_continuation() {
    let mut server = test_server().await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()));
    round_trip_and_park(&network, &server.url, "existing-tip").await;

    // A full request with zero age must not borrow even an otherwise reusable socket.
    round_trip_and_park_with_age(&network, &server.url, "zero-tip", Some(0)).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("zero-age socket released immediately")
        .expect("upstream observed zero-age release");
    assert_local_miss(
        require_tip(&network, &server.url, "zero-tip", Vec::new())
            .await
            .err()
            .expect("zero-age completed response cannot be reused"),
    );
    assert_local_miss(
        require_tip_with_age(&network, &server.url, "existing-tip", Vec::new(), Some(0))
            .await
            .err()
            .expect("zero-age continuation cannot borrow an existing socket"),
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    round_trip_and_park(&network, &server.url, "full-request").await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn requested_age_above_sixty_minutes_is_capped() {
    let mut server = test_server().await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()));
    round_trip_and_park_with_age(&network, &server.url, "first", Some(u64::MAX)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(59 * 60)).await;
    tokio::time::resume();
    let socket = require_tip_with_age(&network, &server.url, "first", Vec::new(), Some(u64::MAX))
        .await
        .expect("capped socket remains reusable before sixty minutes");
    complete_and_park(&socket, "latest", Some("latest")).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("capped socket released at sixty minutes")
        .expect("upstream observed capped release");
    assert_local_miss(
        require_tip(&network, &server.url, "latest", Vec::new())
            .await
            .err()
            .expect("requested age cannot exceed the sixty-minute cap"),
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn requested_age_shorter_checkout_tightens_original_deadline() {
    let mut server = test_server().await;
    let network = network(&server.url, Arc::new(VendorWebSocketPool::default()));
    round_trip_and_park(&network, &server.url, "first").await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(20 * 60)).await;
    tokio::time::resume();
    let socket = require_tip_with_age(
        &network,
        &server.url,
        "first",
        Vec::new(),
        Some(55 * 60 * 1000),
    )
    .await
    .expect("shorter checkout remains before original handshake deadline");
    complete_and_park(&socket, "shortened", Some("shortened")).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(34 * 60)).await;
    tokio::time::resume();
    let socket = require_tip(&network, &server.url, "shortened", Vec::new())
        .await
        .expect("default checkout cannot extend the shortened deadline");
    complete_and_park(&socket, "latest", Some("latest")).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("shortened socket retires at handshake plus fifty-five minutes")
        .expect("upstream observed shortened release");
    assert_local_miss(
        require_tip(&network, &server.url, "latest", Vec::new())
            .await
            .err()
            .expect("shortened tip unavailable"),
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn requested_age_shorter_checkout_retires_only_matching_affinity() {
    let mut server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let matching = network(&server.url, Arc::clone(&pool));
    let other = matching.clone().with_websocket_pool(
        pool,
        "test-scope".into(),
        Some("another-affinity".into()),
    );
    // Identical tips ensure that affinity, not the response ID, isolates retirement.
    round_trip_and_park(&matching, &server.url, "shared-tip").await;
    round_trip_and_park(&other, &server.url, "shared-tip").await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    tokio::time::resume();
    assert_local_miss(
        require_tip_with_age(
            &matching,
            &server.url,
            "shared-tip",
            Vec::new(),
            Some(20 * 60 * 1000),
        )
        .await
        .err()
        .expect("shorter requested age retires matching socket immediately"),
    );
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("matching socket released on shortened checkout")
        .expect("upstream observed matching release");
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    let socket = require_tip(&other, &server.url, "shared-tip", Vec::new())
        .await
        .expect("different affinity retains its original default lifetime");
    complete_and_park(&socket, "other-latest", Some("other-latest")).await;
    assert!(server.closed.try_recv().is_err());
    round_trip_and_park(&matching, &server.url, "full-request").await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn repeated_reuse_does_not_extend_the_connection_lifetime() {
    let server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool);

    round_trip_and_park(&network, &server.url, "first").await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(20 * 60)).await;
    tokio::time::resume();
    round_trip_and_park(&network, &server.url, "second").await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(20 * 60)).await;
    tokio::time::resume();
    round_trip_and_park(&network, &server.url, "third").await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(15 * 60)).await;
    tokio::time::resume();
    let socket = checkout(&network, &server.url).await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let send = socket
        .send(WebSocketMessage::Text("past-max-age".into()))
        .await;
    tokio::time::resume();

    assert!(
        send.is_err(),
        "an actively checked-out socket must stop at its original max-age"
    );
}

#[tokio::test]
async fn requested_age_none_releases_parked_socket_at_sixty_minutes() {
    let mut server = test_server().await;
    let pool = Arc::new(VendorWebSocketPool::default());
    let network = network(&server.url, pool);

    round_trip_and_park(&network, &server.url, "only-request").await;
    assert!(server.closed.try_recv().is_err());

    let boundary_margin = Duration::from_secs(5 * 60);
    tokio::time::pause();
    tokio::time::advance(MAX_WEBSOCKET_AGE - boundary_margin).await;
    tokio::task::yield_now().await;
    assert!(server.closed.try_recv().is_err());

    tokio::time::advance(boundary_margin).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(1), server.closed.recv())
        .await
        .expect("idle socket close notification before timeout")
        .expect("local upstream observed socket release");
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}
