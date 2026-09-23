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
