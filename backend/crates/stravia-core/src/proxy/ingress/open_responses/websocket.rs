//! Open Responses 2026-04-24 WebSocket transport.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::Gateway;
use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
use crate::proxy::context::{CancellationToken, RequestContext};

const MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024;
const OUTGOING_QUEUE_CAPACITY: usize = 64;
const CONNECTION_TTL: Duration = Duration::from_secs(60 * 60);
const WRITER_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const RUN_DEADLINE: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct AllowedWebSocketOrigins {
    allow_any: bool,
    origins: Arc<HashSet<String>>,
}

impl AllowedWebSocketOrigins {
    pub fn new(origins: impl IntoIterator<Item = String>) -> Self {
        let origins: HashSet<String> = origins
            .into_iter()
            .map(|origin| origin.trim().to_owned())
            .filter(|origin| !origin.is_empty())
            .collect();
        Self {
            allow_any: origins.contains("*"),
            origins: Arc::new(origins),
        }
    }

    fn allows(&self, origin: &str) -> bool {
        self.allow_any || self.origins.contains(origin)
    }
}

pub async fn handler(
    ws: WebSocketUpgrade,
    State(gateway): State<Gateway>,
    headers: HeaderMap,
    origins: Option<Extension<AllowedWebSocketOrigins>>,
) -> Response {
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        && !origins
            .as_ref()
            .is_some_and(|origins| origins.allows(origin))
    {
        return (StatusCode::FORBIDDEN, "WebSocket Origin is not allowed.").into_response();
    }
    if let Err(response) = super::responses::authenticate(&gateway, &headers).await {
        return response;
    }

    ws.max_message_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve(socket, gateway, headers))
}

struct OutgoingMessage {
    message: Message,
    delivered: Option<tokio::sync::oneshot::Sender<()>>,
}

impl OutgoingMessage {
    fn queued(message: Message) -> Self {
        Self {
            message,
            delivered: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct StreamForwardProgress {
    response: Option<Value>,
    next_sequence_number: u64,
}

impl StreamForwardProgress {
    fn observe_delivered(&mut self, event: &str) {
        let Ok(body) = serde_json::from_str::<Value>(event) else {
            return;
        };
        if let Some(sequence_number) = body.get("sequence_number").and_then(Value::as_u64) {
            self.next_sequence_number = self.next_sequence_number.max(sequence_number + 1);
        }
        if body.get("type").and_then(Value::as_str) == Some("response.created") {
            self.response = body.get("response").cloned();
        }
    }
}

async fn serve(socket: WebSocket, gateway: Gateway, headers: HeaderMap) {
    let (mut sink, mut source) = socket.split();
    let (outgoing, mut outgoing_rx) = mpsc::channel::<OutgoingMessage>(OUTGOING_QUEUE_CAPACITY);
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if sink.send(message.message).await.is_err() {
                break;
            }
            if let Some(delivered) = message.delivered {
                let _ = delivered.send(());
            }
        }
    });
    let in_flight = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::new(Mutex::new(None::<CancellationToken>));

    let read_loop = async {
        while let Some(message) = source.next().await {
            let Ok(message) = message else {
                break;
            };
            match message {
                Message::Text(text) => {
                    let Ok(mut event) = serde_json::from_str::<Value>(&text) else {
                        send_error(
                            &outgoing,
                            400,
                            "invalid_request",
                            "WebSocket message must be valid JSON.",
                        )
                        .await;
                        continue;
                    };
                    if event.get("type").and_then(Value::as_str) != Some("response.create") {
                        send_error(
                            &outgoing,
                            400,
                            "invalid_request",
                            "WebSocket message type must be response.create.",
                        )
                        .await;
                        continue;
                    }
                    if in_flight.swap(true, Ordering::AcqRel) {
                        send_error(
                            &outgoing,
                            409,
                            "response_in_progress",
                            "A response is already in progress on this connection.",
                        )
                        .await;
                        continue;
                    }
                    let Some(object) = event.as_object_mut() else {
                        in_flight.store(false, Ordering::Release);
                        send_error(
                            &outgoing,
                            400,
                            "invalid_request",
                            "response.create must be an object.",
                        )
                        .await;
                        continue;
                    };
                    object.remove("type");
                    object.insert("stream".into(), Value::Bool(true));

                    let gateway = gateway.clone();
                    let headers = headers.clone();
                    let outgoing = outgoing.clone();
                    let in_flight = in_flight.clone();
                    let cancellation_slot = cancellation.clone();
                    let request_context =
                        RequestContext::new(OPEN_RESPONSES_2026_04_24, RUN_DEADLINE);
                    let request_cancellation = request_context.cancellation.clone();
                    *cancellation_slot.lock().expect("cancellation lock") =
                        Some(request_cancellation.clone());
                    let progress = Arc::new(Mutex::new(StreamForwardProgress::default()));
                    tokio::spawn(async move {
                        let result = tokio::time::timeout(
                            RUN_DEADLINE,
                            forward_response(
                                gateway,
                                headers,
                                request_context,
                                event,
                                &outgoing,
                                &progress,
                            ),
                        )
                        .await;
                        if result.is_err() {
                            request_cancellation.cancel();
                            let _ = tokio::time::timeout(
                                WRITER_SHUTDOWN_GRACE,
                                send_run_timeout(&outgoing, &progress),
                            )
                            .await;
                        }
                        *cancellation_slot.lock().expect("cancellation lock") = None;
                        in_flight.store(false, Ordering::Release);
                    });
                }
                Message::Ping(payload) => {
                    if outgoing
                        .send(OutgoingMessage::queued(Message::Pong(payload)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Binary(_) | Message::Pong(_) => {}
            }
        }
    };

    let expired = tokio::time::timeout(CONNECTION_TTL, read_loop)
        .await
        .is_err();
    if expired {
        terminate_expired_connection(&outgoing, &cancellation, &writer, WRITER_SHUTDOWN_GRACE)
            .await;
    } else if let Some(token) = cancellation.lock().expect("cancellation lock").take() {
        token.cancel();
    }
    drop(outgoing);
    let _ = writer.await;
}

async fn forward_response(
    gateway: Gateway,
    headers: HeaderMap,
    context: RequestContext,
    body: Value,
    outgoing: &mpsc::Sender<OutgoingMessage>,
    progress: &Arc<Mutex<StreamForwardProgress>>,
) {
    let response =
        super::responses::handler(State(gateway), Extension(context), headers, Ok(Json(body)))
            .await;
    let status = response.status();
    let mut stream = response.into_body().into_data_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            send_error(outgoing, 500, "server_error", "Response stream failed.").await;
            return;
        };
        let Ok(text) = std::str::from_utf8(&chunk) else {
            send_error(
                outgoing,
                500,
                "server_error",
                "Response stream was not UTF-8.",
            )
            .await;
            return;
        };
        buffer.push_str(text);
        if status.is_success() && !forward_sse_frames(&mut buffer, outgoing, progress).await {
            return;
        }
    }
    if !status.is_success() {
        let message = serde_json::from_str::<Value>(&buffer)
            .ok()
            .and_then(|body| {
                body.pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("Request failed with HTTP status {status}."));
        let code = serde_json::from_str::<Value>(&buffer)
            .ok()
            .and_then(|body| {
                body.pointer("/error/code")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "invalid_request".into());
        send_error(outgoing, status.as_u16(), &code, &message).await;
    }
}

async fn forward_sse_frames(
    buffer: &mut String,
    outgoing: &mpsc::Sender<OutgoingMessage>,
    progress: &Arc<Mutex<StreamForwardProgress>>,
) -> bool {
    while let Some(end) = buffer.find("\n\n") {
        let frame = buffer[..end].to_owned();
        buffer.drain(..end + 2);
        for line in frame.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            let (delivered, delivered_rx) = tokio::sync::oneshot::channel();
            if outgoing
                .send(OutgoingMessage {
                    message: Message::Text(data.to_owned().into()),
                    delivered: Some(delivered),
                })
                .await
                .is_err()
                || delivered_rx.await.is_err()
            {
                return false;
            }
            progress
                .lock()
                .expect("stream progress lock")
                .observe_delivered(data);
        }
    }
    true
}

async fn send_run_timeout(
    outgoing: &mpsc::Sender<OutgoingMessage>,
    progress: &Arc<Mutex<StreamForwardProgress>>,
) {
    let progress = progress.lock().expect("stream progress lock").clone();
    let Some(mut response) = progress.response else {
        send_error(
            outgoing,
            408,
            "request_timeout",
            "The response exceeded the 300 second deadline.",
        )
        .await;
        return;
    };
    let public_error = serde_json::json!({
        "type": "server_error",
        "code": "request_timeout",
        "message": "The response exceeded the 300 second deadline.",
        "param": null
    });
    response["status"] = Value::String("failed".into());
    response["error"] = public_error.clone();
    response["completed_at"] = Value::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    for body in [
        serde_json::json!({
            "type": "error",
            "sequence_number": progress.next_sequence_number,
            "error": public_error,
        }),
        serde_json::json!({
            "type": "response.failed",
            "sequence_number": progress.next_sequence_number + 1,
            "response": response,
        }),
    ] {
        let (delivered, delivered_rx) = tokio::sync::oneshot::channel();
        if outgoing
            .send(OutgoingMessage {
                message: Message::Text(body.to_string().into()),
                delivered: Some(delivered),
            })
            .await
            .is_err()
            || delivered_rx.await.is_err()
        {
            return;
        }
    }
}

async fn terminate_expired_connection(
    outgoing: &mpsc::Sender<OutgoingMessage>,
    cancellation: &Arc<Mutex<Option<CancellationToken>>>,
    writer: &tokio::task::JoinHandle<()>,
    grace: Duration,
) {
    if let Some(token) = cancellation.lock().expect("cancellation lock").take() {
        token.cancel();
    }
    let _ = tokio::time::timeout(grace, send_connection_limit_error(outgoing)).await;
    writer.abort();
}

async fn send_connection_limit_error(outgoing: &mpsc::Sender<OutgoingMessage>) {
    send_error(
        outgoing,
        429,
        "websocket_connection_limit_reached",
        "The WebSocket connection exceeded the 60 minute limit.",
    )
    .await;
}

async fn send_error(
    outgoing: &mpsc::Sender<OutgoingMessage>,
    status: u16,
    code: &str,
    message: &str,
) {
    let body = serde_json::json!({
        "type": "error",
        "status": status,
        "error": {
            "type": "invalid_request",
            "code": code,
            "message": message,
            "param": null
        }
    });
    let (delivered, delivered_rx) = tokio::sync::oneshot::channel();
    if outgoing
        .send(OutgoingMessage {
            message: Message::Text(body.to_string().into()),
            delivered: Some(delivered),
        })
        .await
        .is_ok()
    {
        let _ = delivered_rx.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_origin_allowlist_is_exact() {
        let origins = AllowedWebSocketOrigins::new([
            "https://console.example".to_string(),
            "tauri://localhost".to_string(),
        ]);
        assert!(origins.allows("https://console.example"));
        assert!(origins.allows("tauri://localhost"));
        assert!(!origins.allows("https://console.example.attacker.test"));
    }

    #[tokio::test]
    async fn sse_bridge_forwards_json_events_without_done_sentinel() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut buffer = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
            "data: [DONE]\n\n"
        )
        .to_string();
        let progress = Arc::new(Mutex::new(StreamForwardProgress::default()));
        let forwarding = async { forward_sse_frames(&mut buffer, &tx, &progress).await };
        let receiving = async {
            let Some(OutgoingMessage {
                message: Message::Text(event),
                delivered: Some(delivered),
            }) = rx.recv().await
            else {
                panic!("forwarded event");
            };
            delivered.send(()).expect("delivery ack receiver");
            event
        };
        let (forwarded, event) = tokio::join!(forwarding, receiving);
        assert!(forwarded);
        assert_eq!(event, r#"{"type":"response.created","sequence_number":0}"#);
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn websocket_errors_include_the_http_style_status() {
        let (tx, mut rx) = mpsc::channel(1);
        let sending = send_error(&tx, 409, "response_in_progress", "busy");
        let receiving = async {
            let OutgoingMessage {
                message: Message::Text(event),
                delivered: Some(delivered),
            } = rx.recv().await.expect("error event")
            else {
                panic!("text error event");
            };
            delivered.send(()).expect("delivery ack receiver");
            event
        };
        let ((), event) = tokio::join!(sending, receiving);
        let body: Value = serde_json::from_str(&event).expect("error JSON");
        assert_eq!(body["type"], "error");
        assert_eq!(body["status"], 409);
        assert_eq!(body["error"]["code"], "response_in_progress");
        assert!(body.get("sequence_number").is_none());
    }

    #[tokio::test]
    async fn connection_ttl_uses_the_protocol_limit_error() {
        let (tx, mut rx) = mpsc::channel(1);
        let sending = send_connection_limit_error(&tx);
        let receiving = async {
            let OutgoingMessage {
                message: Message::Text(event),
                delivered: Some(delivered),
            } = rx.recv().await.expect("connection limit event")
            else {
                panic!("text connection limit event");
            };
            delivered.send(()).expect("delivery ack receiver");
            event
        };
        let ((), event) = tokio::join!(sending, receiving);
        let body: Value = serde_json::from_str(&event).expect("error JSON");
        assert_eq!(body["status"], 429);
        assert_eq!(body["error"]["code"], "websocket_connection_limit_reached");
    }

    #[tokio::test]
    async fn post_commit_run_timeout_emits_error_then_failed() {
        let (tx, mut rx) = mpsc::channel(2);
        let progress = Arc::new(Mutex::new(StreamForwardProgress {
            response: Some(
                crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
                    "resp-timeout",
                    "logical-model",
                    "in_progress",
                    Vec::new(),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ),
            ),
            next_sequence_number: 7,
        }));
        let sending = send_run_timeout(&tx, &progress);
        let receiving = async {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let OutgoingMessage {
                    message: Message::Text(event),
                    delivered: Some(delivered),
                } = rx.recv().await.expect("timeout event")
                else {
                    panic!("text timeout event");
                };
                delivered.send(()).expect("delivery ack receiver");
                bodies.push(serde_json::from_str::<Value>(&event).expect("timeout event JSON"));
            }
            bodies
        };
        let ((), bodies) = tokio::join!(sending, receiving);

        assert_eq!(bodies[0]["type"], "error");
        assert_eq!(bodies[0]["sequence_number"], 7);
        assert_eq!(bodies[0]["error"]["code"], "request_timeout");
        assert_eq!(bodies[1]["type"], "response.failed");
        assert_eq!(bodies[1]["sequence_number"], 8);
        assert_eq!(bodies[1]["response"]["id"], "resp-timeout");
        assert_eq!(bodies[1]["response"]["status"], "failed");
    }
    #[tokio::test]
    async fn ttl_shutdown_cancels_the_run_and_aborts_a_blocked_writer() {
        let (tx, _rx) = mpsc::channel(1);
        tx.send(OutgoingMessage::queued(Message::Text("queued".into())))
            .await
            .expect("fill outgoing queue");
        let token = CancellationToken::new();
        let cancellation = Arc::new(Mutex::new(Some(token.clone())));
        let writer = tokio::spawn(std::future::pending::<()>());

        terminate_expired_connection(&tx, &cancellation, &writer, Duration::from_millis(10)).await;

        assert!(token.is_cancelled());
        assert!(
            writer
                .await
                .expect_err("writer must be aborted")
                .is_cancelled()
        );
    }
}
