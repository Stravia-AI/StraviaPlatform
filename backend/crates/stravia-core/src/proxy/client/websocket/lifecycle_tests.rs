//! Regression tests for the Responses WebSocket connection lifecycle.
//!
//! An idle retained socket must keep answering upstream Ping, must notice a
//! peer Close, and must not leak late frames into the next turn. The Axum
//! fixture hands each accepted connection a command channel plus a `closed`
//! oneshot that resolves when the server sees the peer's terminal reaction
//! (Close reply or EOF) — that, not sleeps, is the ordering barrier.

use super::*;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{
    State, WebSocketUpgrade,
    ws::{Message as AxumMessage, WebSocket},
};
use axum::routing::get;
use tokio::sync::{mpsc, oneshot};

fn test_trace() -> ResponsesWebSocketTrace<'static> {
    ResponsesWebSocketTrace {
        provider_id: "provider",
        target_id: "target",
        transport_attempt: "attempt",
    }
}

enum ConnCommand {
    /// `answered` resolves `true` when the pong arrives, `false` if the
    /// socket dies first. A pong also proves the client drained everything
    /// sent before the ping (network ordering).
    Ping { answered: oneshot::Sender<bool> },
    /// A stray post-completion text frame, e.g. a late delta.
    SendText(&'static str),
    /// Send Close, then keep reading until the peer's Close reply or EOF
    /// lands; `confirmed` resolves then. Proves the client actually read the
    /// close instead of leaving the socket parked.
    Close { confirmed: oneshot::Sender<()> },
}

struct ConnHandle {
    ordinal: usize,
    commands: mpsc::UnboundedSender<ConnCommand>,
    /// Resolves when the connection task exits.
    closed: oneshot::Receiver<()>,
}

impl ConnHandle {
    async fn assert_ping_answered(&self) {
        let (answered, wait) = oneshot::channel();
        self.commands
            .send(ConnCommand::Ping { answered })
            .expect("ping command");
        assert!(
            tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .expect("ping waiter timed out")
                .expect("ping waiter dropped"),
            "retained idle connection did not answer upstream ping"
        );
    }
}

#[derive(Clone, Default)]
struct ConnCounts {
    accepted: std::sync::Arc<AtomicUsize>,
}

#[derive(Clone)]
struct FixtureState {
    accepted: mpsc::UnboundedSender<ConnHandle>,
    counts: ConnCounts,
}

struct WsFixture {
    url: String,
    inbound: mpsc::UnboundedReceiver<ConnHandle>,
    counts: ConnCounts,
}

impl WsFixture {
    async fn next_conn(&mut self) -> ConnHandle {
        tokio::time::timeout(Duration::from_secs(5), self.inbound.recv())
            .await
            .expect("timed out waiting for an upstream connection")
            .expect("upstream connection channel closed")
    }

    fn accepted_count(&self) -> usize {
        self.counts.accepted.load(Ordering::SeqCst)
    }
}

async fn ws_handler(
    State(state): State<FixtureState>,
    upgrade: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    upgrade.on_upgrade(move |socket| serve_conn(socket, state))
}

/// `response.create` gets a canned `response.completed`; commands drive Ping /
/// late Text / Close. Ping pongs are tungstenite-automatic; the `Pong` arm
/// resolves the oldest waiter so the test can block on the client actually
/// servicing the wire.
async fn serve_conn(mut socket: WebSocket, state: FixtureState) {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ConnCommand>();
    let (closed_tx, closed_rx) = oneshot::channel();
    let ordinal = state.counts.accepted.fetch_add(1, Ordering::SeqCst);
    let _ = state.accepted.send(ConnHandle {
        ordinal,
        commands: cmd_tx,
        closed: closed_rx,
    });

    let mut pending_pings: VecDeque<oneshot::Sender<bool>> = VecDeque::new();
    // Set once a Close command asks us to drain for the peer's terminal frame.
    let mut close_confirm: Option<oneshot::Sender<()>> = None;
    let mut response_ordinal = 0_u32;
    // After sending Close, stop accepting commands and just read.
    let mut draining = false;

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv(), if !draining => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    ConnCommand::Ping { answered } => {
                        if socket
                            .send(AxumMessage::Ping(bytes::Bytes::from_static(b"hb")))
                            .await
                            .is_err()
                        {
                            let _ = answered.send(false);
                            break;
                        }
                        pending_pings.push_back(answered);
                    }
                    ConnCommand::SendText(text) => {
                        if socket
                            .send(AxumMessage::Text(text.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    ConnCommand::Close { confirmed } => {
                        let _ = socket.send(AxumMessage::Close(None)).await;
                        close_confirm = Some(confirmed);
                        draining = true;
                    }
                }
            }
            frame = socket.recv() => {
                match frame {
                    Some(Ok(AxumMessage::Text(text))) => {
                        let parsed: serde_json::Value =
                            serde_json::from_str(&text).expect("request JSON");
                        if parsed["type"] == "response.create" {
                            response_ordinal += 1;
                            if socket
                                .send(AxumMessage::Text(
                                    serde_json::json!({
                                        "type": "response.completed",
                                        "response": {
                                            "id": format!("resp-{ordinal}-{response_ordinal}"),
                                            "status": "completed",
                                            "output": [],
                                            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                                        }
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Some(Ok(AxumMessage::Pong(_))) => {
                        if let Some(waiter) = pending_pings.pop_front() {
                            let _ = waiter.send(true);
                        }
                    }
                    // Peer's terminal reaction to our Close (or its own close).
                    Some(Ok(AxumMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    while let Some(waiter) = pending_pings.pop_front() {
        let _ = waiter.send(false);
    }
    if let Some(confirm) = close_confirm {
        let _ = confirm.send(());
    }
    let _ = closed_tx.send(());
}

async fn spawn_fixture() -> WsFixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws fixture");
    let address = listener.local_addr().expect("fixture addr");
    let (accepted, inbound) = mpsc::unbounded_channel();
    let counts = ConnCounts::default();
    let app = Router::new()
        .route("/v1/responses", get(ws_handler))
        .with_state(FixtureState {
            accepted,
            counts: counts.clone(),
        });
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve ws fixture");
    });
    WsFixture {
        url: format!("ws://{address}/v1/responses"),
        inbound,
        counts,
    }
}

fn acquire_request(url: &str) -> ResponsesWebSocketRequest<'_> {
    ResponsesWebSocketRequest {
        url,
        headers: HeaderMap::new(),
        on_connect_start: None,
    }
}

fn affinity(previous_response_id: Option<&str>) -> ResponsesWebSocketAffinityHint<'_> {
    ResponsesWebSocketAffinityHint {
        previous_response_id,
        session_affinity: None,
        require_affinity: true,
    }
}

/// One full turn; returns the completed lease and the response id registered
/// for continuation.
async fn run_turn(
    registry: &ResponsesWebSocketRegistry,
    client: &reqwest::Client,
    url: &str,
    previous_response_id: Option<&str>,
) -> (ResponsesWebSocketLease, String) {
    let mut lease = registry
        .acquire(
            client,
            "target",
            test_trace(),
            acquire_request(url),
            affinity(previous_response_id),
        )
        .await
        .expect("acquire lease");
    lease
        .send_text(serde_json::json!({"type": "response.create", "input": ["turn"]}).to_string())
        .await
        .expect("send request");
    let response_id = loop {
        match lease.next().await.expect("stream ended before completed") {
            Ok(reqwest_websocket::Message::Text(text)) => {
                let event: serde_json::Value = serde_json::from_str(&text).expect("event JSON");
                if event["type"] == "response.completed" {
                    break event["response"]["id"]
                        .as_str()
                        .expect("response id")
                        .to_owned();
                }
            }
            Ok(_) => {}
            Err(error) => panic!("upstream frame error: {error}"),
        }
    };
    lease.completed(response_id.clone());
    (lease, response_id)
}

/// A socket retained after `completed` must still service the wire — if
/// nothing reads it, an upstream Ping goes unanswered and a NAT/LB reaps the
/// connection before the next acquire inherits the corpse.
#[tokio::test]
async fn idle_retained_connection_answers_upstream_ping() {
    let mut fixture = spawn_fixture().await;
    let registry = ResponsesWebSocketRegistry::default();
    let client = reqwest::Client::new();

    let (first, response_id) = run_turn(&registry, &client, &fixture.url, None).await;
    let first_conn = fixture.next_conn().await;
    assert_eq!(first_conn.ordinal, 0);
    // Cancel a checkout while the completed lease still owns the socket.
    // Its cancellation must not strand the retained connection without a reader.
    assert!(
        futures::FutureExt::now_or_never(registry.acquire(
            &client,
            "target",
            test_trace(),
            acquire_request(&fixture.url),
            affinity(Some(response_id.as_str())),
        ))
        .is_none()
    );
    drop(first);

    first_conn.assert_ping_answered().await;
    // On this single-thread runtime, the idle reader still owns the socket
    // while checkout requests its cancellation. Dropping the pending checkout
    // must restore that reader rather than leave the connection unserviced.
    assert!(
        futures::FutureExt::now_or_never(registry.acquire(
            &client,
            "target",
            test_trace(),
            acquire_request(&fixture.url),
            affinity(Some(response_id.as_str())),
        ))
        .is_none()
    );
    first_conn.assert_ping_answered().await;
    let (second, _) = run_turn(&registry, &client, &fixture.url, Some(response_id.as_str())).await;
    assert!(
        second.reused_connection(),
        "healthy idle socket was replaced"
    );
    assert_eq!(fixture.accepted_count(), 1);
}

#[tokio::test]
async fn completing_expired_lease_does_not_republish_affinity() {
    let mut fixture = spawn_fixture().await;
    let registry = ResponsesWebSocketRegistry::default();
    let client = reqwest::Client::new();
    let mut lease = registry
        .acquire(
            &client,
            "target",
            test_trace(),
            acquire_request(&fixture.url),
            affinity(None),
        )
        .await
        .expect("acquire lease");
    lease
        .send_text(serde_json::json!({"type": "response.create", "input": ["turn"]}).to_string())
        .await
        .expect("send request");
    let reqwest_websocket::Message::Text(text) = lease.next().await.unwrap().unwrap() else {
        panic!("expected completed event");
    };
    let event: serde_json::Value = serde_json::from_str(&text).unwrap();
    let response_id = event["response"]["id"].as_str().unwrap();
    let connection = fixture.next_conn().await;

    tokio::time::pause();
    tokio::time::advance(RESPONSES_WEBSOCKET_MAX_AGE).await;
    assert!(!registry.continuation_available("target", response_id, false));
    lease.completed(response_id.to_owned());
    assert!(
        !registry.continuation_available("target", response_id, false),
        "completing an expired lease republished a dead affinity"
    );
    drop(lease);
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), connection.closed)
        .await
        .expect("expired connection stayed open after lease release")
        .expect("connection close observer disappeared");
}

#[tokio::test]
async fn dropping_registry_closes_idle_connection() {
    let mut fixture = spawn_fixture().await;
    let registry = ResponsesWebSocketRegistry::default();
    let client = reqwest::Client::new();
    let (lease, _) = run_turn(&registry, &client, &fixture.url, None).await;
    let connection = fixture.next_conn().await;
    drop(lease);
    connection.assert_ping_answered().await;
    drop(registry);
    tokio::time::timeout(Duration::from_secs(5), connection.closed)
        .await
        .expect("dropping the registry left an idle connection alive")
        .expect("connection close observer disappeared");
}

/// When the peer closes an idle retained socket, the response affinity must be
/// dropped so the next acquire opens a fresh connection instead of chaining
/// onto a dead wire.
#[tokio::test]
async fn peer_close_on_idle_socket_drops_affinity_and_forces_new_connection() {
    let mut fixture = spawn_fixture().await;
    let registry = ResponsesWebSocketRegistry::default();
    let client = reqwest::Client::new();

    let (first, first_response_id) = run_turn(&registry, &client, &fixture.url, None).await;
    let first_conn = fixture.next_conn().await;
    assert_eq!(first_conn.ordinal, 0);
    drop(first);
    assert!(registry.continuation_available("target", &first_response_id, false));

    let (confirmed, wait) = oneshot::channel();
    first_conn
        .commands
        .send(ConnCommand::Close { confirmed })
        .expect("close command");
    // Network barrier: resolves only once the client emitted its terminal
    // reaction (Close reply or EOF) — proof it read the close.
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("client never reacted to peer close")
        .expect("close waiter dropped");

    assert!(
        !registry.continuation_available("target", &first_response_id, false),
        "peer close left the response affinity registered"
    );

    let second = registry
        .acquire(
            &client,
            "target",
            test_trace(),
            acquire_request(&fixture.url),
            affinity(Some(first_response_id.as_str())),
        )
        .await
        .expect("acquire after peer close");
    assert!(
        !second.reused_connection(),
        "dead socket was handed out as a continuation"
    );
    assert_eq!(
        second.previous_response_id(),
        None,
        "continuation onto a peer-closed socket kept its affinity tip"
    );
    let second_conn = fixture.next_conn().await;
    assert_eq!(
        fixture.accepted_count(),
        2,
        "expected a brand-new upstream connection"
    );
    assert_eq!(second_conn.ordinal, 1);
}

/// A frame that lands after `completed` — a late delta from the previous turn
/// — must retire the socket instead of being delivered to the next consumer.
#[tokio::test]
async fn late_idle_frame_is_not_leaked_into_the_next_turn() {
    let mut fixture = spawn_fixture().await;
    let registry = ResponsesWebSocketRegistry::default();
    let client = reqwest::Client::new();

    let (first, first_response_id) = run_turn(&registry, &client, &fixture.url, None).await;
    let first_conn = fixture.next_conn().await;
    assert_eq!(first_conn.ordinal, 0);
    drop(first);

    first_conn
        .commands
        .send(ConnCommand::SendText(
            r#"{"type":"response.output_text.delta","delta":"late"}"#,
        ))
        .expect("late frame command");

    // A healthy pump reads the stray frame, retires the socket, and closes it;
    // the server observing that close is the barrier — no sleeps. A missing
    // pump leaves the socket parked and the affinity intact.
    tokio::time::timeout(Duration::from_secs(5), first_conn.closed)
        .await
        .expect("dirty idle socket was never closed")
        .expect("connection task vanished");
    assert!(
        !registry.continuation_available("target", &first_response_id, false),
        "late idle frame left the response affinity registered"
    );

    let mut second = registry
        .acquire(
            &client,
            "target",
            test_trace(),
            acquire_request(&fixture.url),
            affinity(Some(first_response_id.as_str())),
        )
        .await
        .expect("acquire after late frame");
    assert!(
        !second.reused_connection(),
        "dirty socket was reused after a late idle frame"
    );

    second
        .send_text(serde_json::json!({"type": "response.create", "input": ["second"]}).to_string())
        .await
        .expect("send second request");
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match second.next().await {
                Some(Ok(reqwest_websocket::Message::Text(text))) => {
                    break serde_json::from_str::<serde_json::Value>(&text).expect("event JSON");
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("upstream error: {error}"),
                None => panic!("stream ended before second response"),
            }
        }
    })
    .await
    .expect("second turn never produced an event");
    assert_eq!(
        event["type"], "response.completed",
        "stale idle frame leaked into the next turn: {event}"
    );
}
