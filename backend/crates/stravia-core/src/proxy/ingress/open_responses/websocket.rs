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
use crate::proxy::context::RequestContext;
use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;

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

fn ws_wire(
    direction: &str,
    message_type: &str,
    payload: Value,
) -> crate::interaction_observation::RunEvent {
    crate::interaction_observation::RunEvent::Wire {
        direction: direction.into(),
        transport: "websocket".into(),
        protocol: OPEN_RESPONSES_2026_04_24.to_string(),
        message_type: message_type.into(),
        model_turn_id: None,
        attempt_id: None,
        status_code: None,
        url: Some("/v1/responses".into()),
        headers: Value::Null,
        payload,
    }
}

fn handshake_ingress(
    gateway: &Gateway,
    headers: &HeaderMap,
) -> crate::interaction_observation::IngressObserver {
    let observer =
        gateway
            .observation
            .observe_ingress(crate::interaction_observation::IngressStart {
                id: format!("ws-handshake-{}", uuid::Uuid::new_v4()),
                method: "GET".into(),
                path: "/v1/responses".into(),
                protocol: OPEN_RESPONSES_2026_04_24.to_string(),
            });
    observer.record_debug(|| crate::interaction_observation::RunEvent::Wire {
        direction: "client_to_platform".into(),
        transport: "http".into(),
        protocol: OPEN_RESPONSES_2026_04_24.to_string(),
        message_type: "request_head".into(),
        model_turn_id: None,
        attempt_id: None,
        status_code: None,
        url: Some("/v1/responses".into()),
        headers: serde_json::Value::Object(
            headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_owned(), Value::String(value.to_owned())))
                })
                .collect(),
        ),
        payload: Value::Null,
    });
    observer
}

fn websocket_ingress(
    gateway: &Gateway,
    headers: &HeaderMap,
    handshake_response: &Value,
    suffix: &str,
) -> crate::interaction_observation::IngressObserver {
    let observer =
        gateway
            .observation
            .observe_ingress(crate::interaction_observation::IngressStart {
                id: format!("ws-{suffix}-{}", uuid::Uuid::new_v4()),
                method: "WEBSOCKET".into(),
                path: "/v1/responses".into(),
                protocol: OPEN_RESPONSES_2026_04_24.to_string(),
            });
    observer.record_debug(|| crate::interaction_observation::RunEvent::Wire {
        direction: "client_to_platform".into(),
        transport: "http".into(),
        protocol: OPEN_RESPONSES_2026_04_24.to_string(),
        message_type: "handshake_request".into(),
        model_turn_id: None,
        attempt_id: None,
        status_code: None,
        url: Some("/v1/responses".into()),
        headers: serde_json::Value::Object(
            headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_owned(), Value::String(value.to_owned())))
                })
                .collect(),
        ),
        payload: Value::Null,
    });
    observer.record_debug(|| crate::interaction_observation::RunEvent::Wire {
        direction: "platform_to_client".into(),
        transport: "http".into(),
        protocol: OPEN_RESPONSES_2026_04_24.to_string(),
        message_type: "handshake_response".into(),
        model_turn_id: None,
        attempt_id: None,
        status_code: handshake_response
            .get("status")
            .and_then(Value::as_u64)
            .map(|status| status as u16),
        url: Some("/v1/responses".into()),
        headers: handshake_response
            .get("headers")
            .cloned()
            .unwrap_or(Value::Null),
        payload: Value::Null,
    });
    observer
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
        let ingress = handshake_ingress(&gateway, &headers);
        let response = (StatusCode::FORBIDDEN, "WebSocket Origin is not allowed.").into_response();
        return crate::proxy::ingress::observation::reject(
            ingress,
            "protocol",
            "origin_forbidden",
            response,
        );
    }
    if let Err(response) = super::responses::authenticate(&gateway, &headers).await {
        let ingress = handshake_ingress(&gateway, &headers);
        return crate::proxy::ingress::observation::reject(
            ingress,
            "authentication",
            "authentication_error",
            response,
        );
    }

    let handshake_response = Arc::new(Mutex::new(Value::Null));
    let serve_handshake_response = handshake_response.clone();
    let response = ws
        .max_message_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve(socket, gateway, headers, serve_handshake_response));
    *handshake_response.lock().expect("handshake response lock") = serde_json::json!({
        "status": response.status().as_u16(),
        "headers": response.headers().iter().filter_map(|(name, value)| {
            value.to_str().ok().map(|value| {
                (name.as_str().to_owned(), Value::String(value.to_owned()))
            })
        }).collect::<serde_json::Map<String, Value>>(),
    });
    response
}

struct OutgoingMessage {
    message: Message,
    delivered: Option<tokio::sync::oneshot::Sender<()>>,
}

type SharedRunDelivery =
    Arc<Mutex<Option<Arc<Mutex<crate::proxy::dispatcher::WebSocketRunDelivery>>>>>;

#[derive(Clone, Debug, Default)]
struct StreamForwardProgress {
    response: Option<Value>,
    next_sequence_number: u64,
    terminal_failure: Option<String>,
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
        if body.get("type").and_then(Value::as_str) == Some("response.failed") {
            self.terminal_failure = Some(
                body.pointer("/response/error/code")
                    .and_then(Value::as_str)
                    .unwrap_or("response_failed")
                    .to_owned(),
            );
        }
    }
}

async fn serve(
    socket: WebSocket,
    gateway: Gateway,
    headers: HeaderMap,
    handshake_response: Arc<Mutex<Value>>,
) {
    let handshake_response = handshake_response
        .lock()
        .expect("handshake response lock")
        .clone();
    let (mut sink, mut source) = socket.split();
    let (outgoing, mut outgoing_rx) = mpsc::channel::<OutgoingMessage>(OUTGOING_QUEUE_CAPACITY);
    let terminal_started = Arc::new(AtomicBool::new(false));
    let writer_terminal_started = terminal_started.clone();
    let run_finished = Arc::new(tokio::sync::Notify::new());
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if let Message::Text(text) = &message.message
                && serde_json::from_str::<Value>(text)
                    .ok()
                    .is_some_and(|event| {
                        matches!(
                            event.get("type").and_then(Value::as_str),
                            Some("response.completed" | "response.failed" | "response.incomplete")
                        )
                    })
            {
                writer_terminal_started.store(true, Ordering::Release);
            }
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
    let active_observer = Arc::new(Mutex::new(
        None::<crate::interaction_observation::RunObserver>,
    ));
    let active_delivery: SharedRunDelivery = Arc::new(Mutex::new(None));

    let read_loop = async {
        while let Some(message) = source.next().await {
            let Ok(message) = message else {
                break;
            };
            match message {
                Message::Text(text) => {
                    let request_context =
                        RequestContext::new(OPEN_RESPONSES_2026_04_24, RUN_DEADLINE);
                    let ingress =
                        websocket_ingress(&gateway, &headers, &handshake_response, "message");
                    ingress.record_debug(|| {
                        ws_wire(
                            "client_to_platform",
                            "text",
                            Value::String(text.to_string()),
                        )
                    });
                    let Ok(mut event) = serde_json::from_str::<Value>(&text) else {
                        if send_error(
                            &outgoing,
                            400,
                            "invalid_request",
                            "WebSocket message must be valid JSON.",
                        )
                        .await
                        {
                            ingress.record_debug(|| {
                                ws_wire(
                                    "platform_to_client",
                                    "text",
                                    Value::String(
                                        error_body(
                                            400,
                                            "invalid_request",
                                            "WebSocket message must be valid JSON.",
                                        )
                                        .to_string(),
                                    ),
                                )
                            });
                        }
                        ingress.reject(crate::interaction_observation::RejectedOutcome {
                            stage: "decode".into(),
                            code: "invalid_request".into(),
                            status_code: 400,
                        });
                        continue;
                    };
                    if event.get("type").and_then(Value::as_str) != Some("response.create") {
                        if send_error(
                            &outgoing,
                            400,
                            "invalid_request",
                            "WebSocket message type must be response.create.",
                        )
                        .await
                        {
                            ingress.record_debug(|| {
                                ws_wire(
                                    "platform_to_client",
                                    "text",
                                    Value::String(
                                        error_body(
                                            400,
                                            "invalid_request",
                                            "WebSocket message type must be response.create.",
                                        )
                                        .to_string(),
                                    ),
                                )
                            });
                        }
                        ingress.reject(crate::interaction_observation::RejectedOutcome {
                            stage: "protocol".into(),
                            code: "invalid_request".into(),
                            status_code: 400,
                        });
                        continue;
                    }
                    // A terminal frame may reach the client before delivery bookkeeping
                    // finishes. Serialize that handoff, but still reject overlapping turns.
                    loop {
                        let finished = run_finished.notified();
                        if !in_flight.load(Ordering::Acquire)
                            || !terminal_started.load(Ordering::Acquire)
                        {
                            break;
                        }
                        finished.await;
                    }
                    if in_flight.swap(true, Ordering::AcqRel) {
                        if send_error(
                            &outgoing,
                            409,
                            "response_in_progress",
                            "A response is already in progress on this connection.",
                        )
                        .await
                        {
                            ingress.record_debug(|| {
                                ws_wire(
                                    "platform_to_client",
                                    "text",
                                    Value::String(
                                        error_body(
                                            409,
                                            "response_in_progress",
                                            "A response is already in progress on this connection.",
                                        )
                                        .to_string(),
                                    ),
                                )
                            });
                        }
                        ingress.reject(crate::interaction_observation::RejectedOutcome {
                            stage: "admission".into(),
                            code: "response_in_progress".into(),
                            status_code: 409,
                        });
                        continue;
                    }
                    terminal_started.store(false, Ordering::Release);
                    let Some(object) = event.as_object_mut() else {
                        in_flight.store(false, Ordering::Release);
                        if send_error(
                            &outgoing,
                            400,
                            "invalid_request",
                            "response.create must be an object.",
                        )
                        .await
                        {
                            ingress.record_debug(|| {
                                ws_wire(
                                    "platform_to_client",
                                    "text",
                                    Value::String(
                                        error_body(
                                            400,
                                            "invalid_request",
                                            "response.create must be an object.",
                                        )
                                        .to_string(),
                                    ),
                                )
                            });
                        }
                        ingress.reject(crate::interaction_observation::RejectedOutcome {
                            stage: "decode".into(),
                            code: "invalid_request".into(),
                            status_code: 400,
                        });
                        continue;
                    };
                    object.remove("type");
                    object.insert("stream".into(), Value::Bool(true));

                    let gateway = gateway.clone();
                    let headers = headers.clone();
                    let outgoing = outgoing.clone();
                    let in_flight = in_flight.clone();
                    let run_finished = run_finished.clone();
                    let cancellation_slot = cancellation.clone();
                    let active_observer = active_observer.clone();
                    request_context.extensions.insert(ingress);
                    crate::proxy::dispatcher::defer_websocket_delivery(&request_context);
                    let request_cancellation = request_context.cancellation.clone();
                    let timeout_context = request_context.clone();
                    *cancellation_slot.lock().expect("cancellation lock") =
                        Some(request_cancellation.clone());
                    let progress = Arc::new(Mutex::new(StreamForwardProgress::default()));
                    let delivery_slot = active_delivery.clone();
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
                                &active_observer,
                                &delivery_slot,
                            ),
                        )
                        .await;
                        if result.is_err() {
                            request_cancellation.cancel();
                            if active_observer
                                .lock()
                                .expect("active observer lock")
                                .is_none()
                                && let Some(observer) = timeout_context
                                    .extensions
                                    .get::<crate::interaction_observation::RunObserver>(
                                )
                            {
                                *active_observer.lock().expect("active observer lock") =
                                    Some(observer);
                            }
                            let _ = tokio::time::timeout(
                                WRITER_SHUTDOWN_GRACE,
                                send_run_timeout(
                                    &outgoing,
                                    &progress,
                                    &delivery_slot,
                                    &active_observer,
                                ),
                            )
                            .await;
                        }
                        *cancellation_slot.lock().expect("cancellation lock") = None;
                        *active_observer.lock().expect("active observer lock") = None;
                        *delivery_slot.lock().expect("delivery slot lock") = None;
                        in_flight.store(false, Ordering::Release);
                        run_finished.notify_waiters();
                    });
                }
                Message::Ping(payload) => {
                    let observer = active_observer
                        .lock()
                        .expect("active observer lock")
                        .clone();
                    let observed_payload = observer.as_ref().map(|_| payload.clone());
                    if let (Some(observer), Some(observed_payload)) =
                        (&observer, observed_payload.as_ref())
                    {
                        observer.record_debug(|| ws_wire(
                            "client_to_platform",
                            "ping",
                            serde_json::json!({"encoding":"base64","data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD, observed_payload)}),
                        ));
                    }
                    let (delivered, delivered_rx) = tokio::sync::oneshot::channel();
                    if outgoing
                        .send(OutgoingMessage {
                            message: Message::Pong(payload),
                            delivered: Some(delivered),
                        })
                        .await
                        .is_err()
                        || delivered_rx.await.is_err()
                    {
                        break;
                    }
                    if let (Some(observer), Some(observed_payload)) = (observer, observed_payload) {
                        observer.record_debug(|| ws_wire(
                            "platform_to_client",
                            "pong",
                            serde_json::json!({"encoding":"base64","data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD, observed_payload)}),
                        ));
                    }
                }
                Message::Pong(payload) => {
                    if let Some(observer) = active_observer
                        .lock()
                        .expect("active observer lock")
                        .clone()
                    {
                        observer.record_debug(|| ws_wire(
                            "client_to_platform",
                            "pong",
                            serde_json::json!({"encoding":"base64","data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &payload)}),
                        ));
                    }
                }
                Message::Close(frame) => {
                    if let Some(observer) = active_observer
                        .lock()
                        .expect("active observer lock")
                        .clone()
                    {
                        observer.record_debug(|| {
                            ws_wire(
                                "client_to_platform",
                                "close",
                                frame.map_or(Value::Null, |frame| {
                                    serde_json::json!({
                                        "code": u16::from(frame.code),
                                        "reason": frame.reason.as_str(),
                                    })
                                }),
                            )
                        });
                    }
                    break;
                }
                Message::Binary(payload) => {
                    let ingress =
                        websocket_ingress(&gateway, &headers, &handshake_response, "binary");
                    ingress.record_debug(|| ws_wire(
                        "client_to_platform",
                        "binary",
                        serde_json::json!({"encoding":"base64","data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &payload)}),
                    ));
                    if send_error(
                        &outgoing,
                        400,
                        "invalid_request",
                        "WebSocket request messages must be text.",
                    )
                    .await
                    {
                        ingress.record_debug(|| {
                            ws_wire(
                                "platform_to_client",
                                "text",
                                Value::String(
                                    error_body(
                                        400,
                                        "invalid_request",
                                        "WebSocket request messages must be text.",
                                    )
                                    .to_string(),
                                ),
                            )
                        });
                    }
                    ingress.reject(crate::interaction_observation::RejectedOutcome {
                        stage: "decode".into(),
                        code: "invalid_request".into(),
                        status_code: 400,
                    });
                }
            }
        }
    };

    let expired = tokio::time::timeout(CONNECTION_TTL, read_loop)
        .await
        .is_err();
    if expired {
        terminate_expired_connection(
            &outgoing,
            &cancellation,
            &active_observer,
            &active_delivery,
            &writer,
            WRITER_SHUTDOWN_GRACE,
        )
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
    active_observer: &Arc<Mutex<Option<crate::interaction_observation::RunObserver>>>,
    delivery_slot: &SharedRunDelivery,
) {
    let mut response = super::responses::handler(
        State(gateway),
        Extension(context.clone()),
        headers,
        Ok(Json(body)),
    )
    .await;
    let rejection_observer =
        crate::proxy::ingress::observation::take_rejection_observer(&mut response);
    let delivery = crate::proxy::dispatcher::take_websocket_delivery(&context)
        .map(|delivery| Arc::new(Mutex::new(delivery)));
    *delivery_slot.lock().expect("delivery slot lock") = delivery.clone();
    if let Some(delivery) = delivery.as_ref() {
        *active_observer.lock().expect("active observer lock") =
            Some(delivery.lock().expect("delivery lock").observer());
    }
    let status = response.status();
    let upstream_error = response
        .extensions()
        .get::<crate::model_turn::UpstreamErrorResponse>()
        .is_some();
    let mut stream = response.into_body().into_data_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            if send_error(outgoing, 500, "server_error", "Response stream failed.").await
                && let Some(delivery) = delivery.as_ref()
            {
                delivery.lock().expect("delivery lock").sent_error_text(
                    &error_body(500, "server_error", "Response stream failed.").to_string(),
                );
            }
            if let Some(delivery) = delivery.as_ref() {
                delivery
                    .lock()
                    .expect("delivery lock")
                    .finish("delivery_failed", Some("response_stream_failed".into()));
            }
            return;
        };
        let Ok(text) = std::str::from_utf8(&chunk) else {
            if send_error(
                outgoing,
                500,
                "server_error",
                "Response stream was not UTF-8.",
            )
            .await
                && let Some(delivery) = delivery.as_ref()
            {
                delivery.lock().expect("delivery lock").sent_error_text(
                    &error_body(500, "server_error", "Response stream was not UTF-8.").to_string(),
                );
            }
            if let Some(delivery) = delivery.as_ref() {
                delivery
                    .lock()
                    .expect("delivery lock")
                    .finish("delivery_failed", Some("response_stream_not_utf8".into()));
            }
            return;
        };
        buffer.push_str(text);
        if status.is_success()
            && !forward_sse_frames(&mut buffer, outgoing, progress, delivery.as_ref()).await
        {
            return;
        }
    }
    if status.is_success() {
        let terminal_failure = progress
            .lock()
            .expect("stream progress lock")
            .terminal_failure
            .clone();
        if let Some(delivery) = delivery.as_ref() {
            let mut delivery = delivery.lock().expect("delivery lock");
            if let Some(reason) = terminal_failure {
                delivery.finish("delivery_failed", Some(reason));
            } else {
                delivery.finish("delivered", None);
            }
        }
    } else {
        let decoded = serde_json::from_str::<Value>(&buffer).ok();
        let message = decoded
            .as_ref()
            .and_then(|body| {
                body.pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("Request failed with HTTP status {status}."));
        let code = decoded
            .as_ref()
            .and_then(|body| {
                body.pointer("/error/code")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "invalid_request".into());
        let event = if upstream_error
            && let Some(error) = decoded.as_ref().and_then(|body| body.get("error"))
        {
            serde_json::json!({"type": "error", "status": status.as_u16(), "error": error})
        } else {
            error_body(status.as_u16(), &code, &message)
        };
        let error_text = event.to_string();
        if send_error_text(outgoing, error_text.clone()).await {
            if let Some(observer) = rejection_observer.as_ref() {
                observer.record_debug(|| {
                    ws_wire(
                        "platform_to_client",
                        "text",
                        Value::String(error_text.clone()),
                    )
                });
            }
            if let Some(delivery) = delivery.as_ref() {
                delivery
                    .lock()
                    .expect("delivery lock")
                    .sent_error_text(&error_text);
            }
        }
        if let Some(delivery) = delivery.as_ref() {
            delivery
                .lock()
                .expect("delivery lock")
                .finish("delivery_failed", Some(code));
        }
    }
}

async fn forward_sse_frames(
    buffer: &mut String,
    outgoing: &mpsc::Sender<OutgoingMessage>,
    progress: &Arc<Mutex<StreamForwardProgress>>,
    delivery: Option<&Arc<Mutex<crate::proxy::dispatcher::WebSocketRunDelivery>>>,
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
                if let Some(delivery) = delivery {
                    delivery
                        .lock()
                        .expect("delivery lock")
                        .finish("delivery_failed", Some("websocket_write_failed".into()));
                }
                return false;
            }
            if let Some(delivery) = delivery {
                delivery.lock().expect("delivery lock").sent_text(data);
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
    delivery_slot: &SharedRunDelivery,
    active_observer: &Arc<Mutex<Option<crate::interaction_observation::RunObserver>>>,
) {
    let delivery = delivery_slot.lock().expect("delivery slot lock").clone();
    let progress = progress.lock().expect("stream progress lock").clone();
    let Some(mut response) = progress.response else {
        if send_error(
            outgoing,
            408,
            "request_timeout",
            "The response exceeded the 300 second deadline.",
        )
        .await
        {
            let error_text = error_body(
                408,
                "request_timeout",
                "The response exceeded the 300 second deadline.",
            )
            .to_string();
            if let Some(delivery) = delivery.as_ref() {
                delivery
                    .lock()
                    .expect("delivery lock")
                    .sent_error_text(&error_text);
            } else if let Some(observer) = active_observer
                .lock()
                .expect("active observer lock")
                .clone()
            {
                observer.record_debug(|| {
                    ws_wire("platform_to_client", "text", Value::String(error_text))
                });
            }
        }
        if let Some(delivery) = delivery {
            delivery
                .lock()
                .expect("delivery lock")
                .finish("delivery_failed", Some("request_timeout".into()));
        }
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
        let text = body.to_string();
        if outgoing
            .send(OutgoingMessage {
                message: Message::Text(text.clone().into()),
                delivered: Some(delivered),
            })
            .await
            .is_err()
            || delivered_rx.await.is_err()
        {
            if let Some(delivery) = delivery.as_ref() {
                delivery
                    .lock()
                    .expect("delivery lock")
                    .finish("delivery_failed", Some("websocket_write_failed".into()));
            }
            return;
        }
        if let Some(delivery) = delivery.as_ref() {
            delivery
                .lock()
                .expect("delivery lock")
                .sent_error_text(&text);
        } else if let Some(observer) = active_observer
            .lock()
            .expect("active observer lock")
            .clone()
        {
            observer.record_debug(|| ws_wire("platform_to_client", "text", Value::String(text)));
        }
    }
    if let Some(delivery) = delivery {
        delivery
            .lock()
            .expect("delivery lock")
            .finish("delivery_failed", Some("request_timeout".into()));
    }
}

async fn terminate_expired_connection(
    outgoing: &mpsc::Sender<OutgoingMessage>,
    cancellation: &Arc<Mutex<Option<CancellationToken>>>,
    active_observer: &Arc<Mutex<Option<crate::interaction_observation::RunObserver>>>,
    active_delivery: &SharedRunDelivery,
    writer: &tokio::task::JoinHandle<()>,
    grace: Duration,
) {
    if matches!(
        tokio::time::timeout(grace, send_connection_limit_error(outgoing)).await,
        Ok(true)
    ) {
        let error_text = error_body(
            429,
            "websocket_connection_limit_reached",
            "The WebSocket connection exceeded the 60 minute limit.",
        )
        .to_string();
        if let Some(delivery) = active_delivery.lock().expect("delivery slot lock").clone() {
            let mut delivery = delivery.lock().expect("delivery lock");
            delivery.sent_error_text(&error_text);
            delivery.finish(
                "cancelled",
                Some("websocket_connection_limit_reached".into()),
            );
        } else if let Some(observer) = active_observer
            .lock()
            .expect("active observer lock")
            .clone()
        {
            observer
                .record_debug(|| ws_wire("platform_to_client", "text", Value::String(error_text)));
        }
    }
    if let Some(token) = cancellation.lock().expect("cancellation lock").take() {
        token.cancel();
    }
    writer.abort();
}

async fn send_connection_limit_error(outgoing: &mpsc::Sender<OutgoingMessage>) -> bool {
    send_error(
        outgoing,
        429,
        "websocket_connection_limit_reached",
        "The WebSocket connection exceeded the 60 minute limit.",
    )
    .await
}

fn error_body(status: u16, code: &str, message: &str) -> Value {
    serde_json::json!({
        "type": "error",
        "status": status,
        "error": {
            "type": "invalid_request",
            "code": code,
            "message": message,
            "param": null
        }
    })
}

async fn send_error(
    outgoing: &mpsc::Sender<OutgoingMessage>,
    status: u16,
    code: &str,
    message: &str,
) -> bool {
    let body = error_body(status, code, message);
    send_error_text(outgoing, body.to_string()).await
}

async fn send_error_text(outgoing: &mpsc::Sender<OutgoingMessage>, text: String) -> bool {
    let (delivered, delivered_rx) = tokio::sync::oneshot::channel();
    outgoing
        .send(OutgoingMessage {
            message: Message::Text(text.into()),
            delivered: Some(delivered),
        })
        .await
        .is_ok()
        && delivered_rx.await.is_ok()
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
        let forwarding = async { forward_sse_frames(&mut buffer, &tx, &progress, None).await };
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
        let (delivered, event) = tokio::join!(sending, receiving);
        assert!(delivered);
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
        let (delivered, event) = tokio::join!(sending, receiving);
        assert!(delivered);
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
            terminal_failure: None,
        }));
        let delivery = Arc::new(Mutex::new(None));
        let active_observer = Arc::new(Mutex::new(None));
        let sending = send_run_timeout(&tx, &progress, &delivery, &active_observer);
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
        tx.send(OutgoingMessage {
            message: Message::Text("queued".into()),
            delivered: None,
        })
        .await
        .expect("fill outgoing queue");
        let token = CancellationToken::new();
        let cancellation = Arc::new(Mutex::new(Some(token.clone())));
        let writer = tokio::spawn(std::future::pending::<()>());

        terminate_expired_connection(
            &tx,
            &cancellation,
            &Arc::new(Mutex::new(None)),
            &Arc::new(Mutex::new(None)),
            &writer,
            Duration::from_millis(10),
        )
        .await;

        assert!(token.is_cancelled());
        assert!(
            writer
                .await
                .expect_err("writer must be aborted")
                .is_cancelled()
        );
    }
}
