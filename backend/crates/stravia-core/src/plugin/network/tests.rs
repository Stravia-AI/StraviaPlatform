use std::collections::BTreeSet;
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
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::{
    MAX_WEBSOCKET_AGE, VendorNetwork, VendorWebSocketPool, bytes_value, ws_message_type, ws_payload,
};
use crate::plugin::lifecycle::VendorOperationTracker;

#[derive(Clone)]
struct UpstreamState {
    connections: Arc<AtomicUsize>,
    closed: mpsc::UnboundedSender<()>,
}

struct TestServer {
    url: String,
    connections: Arc<AtomicUsize>,
    closed: mpsc::UnboundedReceiver<()>,
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
    let state = UpstreamState {
        connections: Arc::clone(&connections),
        closed: closed_tx,
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
        task: task.abort_handle(),
    }
}

fn network(url: &str, pool: Arc<VendorWebSocketPool>) -> VendorNetwork {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .build()
        .expect("build local WebSocket client");
    let operation = VendorOperationTracker::default()
        .begin("websocket-lifetime-test")
        .expect("begin vendor operation");
    let origin = reqwest::Url::parse(url)
        .expect("parse local WebSocket URL")
        .origin()
        .ascii_serialization();
    VendorNetwork::new(
        client.clone(),
        client,
        BTreeSet::from([origin]),
        operation,
        CancellationToken::new(),
        "test".into(),
    )
    .with_websocket_pool(pool, "test-scope".into(), Some("response-id".into()))
}

async fn checkout(network: &VendorNetwork, url: &str) -> Arc<dyn HostWebSocket> {
    network
        .ws_connect(url.to_owned(), Vec::new(), Vec::new(), None)
        .await
        .expect("connect local WebSocket upstream")
}

async fn round_trip_and_park(network: &VendorNetwork, url: &str, request: &str) {
    let socket = checkout(network, url).await;
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
    network
        .ws_connect(url.to_owned(), headers, Vec::new(), Some(tip.to_owned()))
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
        1,
        true,
        crate::generation_chain::test_chain().await,
        Some(Arc::new(tokio::sync::Mutex::new(()))),
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
                ),
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
        if entry.name().ends_with("/events.jsonl") {
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
    let headers = vec![("authorization".into(), "Bearer first".into())];
    let socket = network
        .ws_connect(server.url.clone(), headers.clone(), Vec::new(), None)
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
async fn plugin_negotiated_encoding_is_decoded_and_keep_alive_is_forwarded() {
    let seen = Arc::new(parking_lot::Mutex::new(Vec::<(
        Option<String>,
        Option<String>,
    )>::new()));
    let recorded = Arc::clone(&seen);
    let app = Router::new().route(
        "/gzip",
        get(move |headers: axum::http::HeaderMap| {
            let recorded = Arc::clone(&recorded);
            async move {
                let header = |name: &str| {
                    headers
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                };
                recorded
                    .lock()
                    .push((header("connection"), header("accept-encoding")));
                ([("content-encoding", "gzip")], GZIP_BODY)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local HTTP upstream");
    let url = format!("http://{}/gzip", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve local HTTP upstream");
    });
    let network = network(&url, Arc::new(VendorWebSocketPool::default()));
    let request = |headers: &[(&str, &str)]| stravia_vendor_runtime::HttpRequest {
        method: "GET".into(),
        url: url.clone(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).into(), (*value).into()))
            .collect(),
        body: Vec::new(),
    };

    let negotiated = network
        .http_start(request(&[
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("connection", "keep-alive"),
        ]))
        .expect("start negotiated request");
    assert_eq!(negotiated.status().await.expect("status"), 200);
    let headers = negotiated.headers().await.expect("headers");
    assert!(
        !headers.iter().any(|(name, _)| name == "content-encoding"),
        "decoded responses must not advertise the wire encoding: {headers:?}"
    );
    assert_eq!(
        read_all(negotiated.as_ref()).await,
        b"decoded upstream body"
    );

    // 未协商编码的旧插件读到原样字节，宿主不擅自解码。
    let plain = network
        .http_start(request(&[]))
        .expect("start plain request");
    assert!(
        plain
            .headers()
            .await
            .expect("headers")
            .iter()
            .any(|(name, value)| name == "content-encoding" && value == "gzip")
    );
    assert_eq!(read_all(plain.as_ref()).await, GZIP_BODY);

    assert_eq!(
        seen.lock().as_slice(),
        [
            (
                Some("keep-alive".to_owned()),
                Some("gzip, deflate, br, zstd".to_owned())
            ),
            (None, None),
        ]
    );
    for value in ["close", "upgrade", "keep-alive, x-secret"] {
        assert!(
            network
                .http_start(request(&[("connection", value)]))
                .is_err(),
            "connection: {value} must stay host-controlled"
        );
    }
    task.abort();
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
async fn parked_socket_is_released_at_max_age_without_another_request() {
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
