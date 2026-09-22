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

use super::{MAX_WEBSOCKET_AGE, VendorNetwork, VendorWebSocketPool};
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
        .ws_connect(url.to_owned(), Vec::new(), Vec::new())
        .await
        .expect("connect local WebSocket upstream")
}

async fn round_trip_and_park(network: &VendorNetwork, url: &str, request: &str) {
    let socket = checkout(network, url).await;
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
    socket.close(true).await.expect("return socket to pool");
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
