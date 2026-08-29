use super::*;

const RESPONSES_WEBSOCKET_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const RESPONSES_WEBSOCKET_TRANSIENT_COOLDOWN: std::time::Duration =
    std::time::Duration::from_secs(15);

#[derive(Clone, Default)]
pub(crate) struct ResponsesWebSocketRegistry {
    state: std::sync::Arc<std::sync::Mutex<ResponsesWebSocketRegistryState>>,
}

#[derive(Clone, Copy)]
pub(crate) struct ResponsesWebSocketTrace<'a> {
    pub provider_id: &'a str,
    pub target_id: &'a str,
    pub transport_attempt: &'a str,
}

pub(crate) struct ResponsesWebSocketRequest<'a> {
    pub url: &'a str,
    pub headers: HeaderMap,
}

#[derive(Default)]
struct ResponsesWebSocketRegistryState {
    connections: std::collections::HashMap<String, ResponsesWebSocketConnectionRecord>,
    affinity: std::collections::HashMap<ResponsesWebSocketAffinity, String>,
    capabilities: std::collections::HashMap<String, ResponsesWebSocketCapability>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ResponsesWebSocketAffinity {
    Response {
        namespace: String,
        response_id: String,
    },
    Session {
        namespace: String,
        session_id: String,
    },
}

impl ResponsesWebSocketAffinity {
    fn response(namespace: &str, response_id: &str) -> Self {
        Self::Response {
            namespace: namespace.to_owned(),
            response_id: response_id.to_owned(),
        }
    }

    fn session(namespace: &str, session_id: &str) -> Self {
        Self::Session {
            namespace: namespace.to_owned(),
            session_id: session_id.to_owned(),
        }
    }
}

struct ResponsesWebSocketConnectionRecord {
    connection: std::sync::Arc<tokio::sync::Mutex<ResponsesWebSocketConnection>>,
    namespace: String,
    created_at: tokio::time::Instant,
    provider_id: String,
    target_id: String,
    transport_attempt: String,
}
struct ResponsesWebSocketConnection {
    socket: reqwest_websocket::WebSocket,
    tip: Option<String>,
    session_id: String,
    thread_id: String,
    window_id: String,
}

enum ResponsesWebSocketCapability {
    Unsupported,
    CooldownUntil(tokio::time::Instant),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResponsesWebSocketAcquireError {
    #[error("Responses WebSocket is unsupported for this Target")]
    Unsupported,
    #[error("Responses WebSocket is cooling down for this Target")]
    Cooldown,
    #[error("Responses WebSocket request was rejected: {0}")]
    Rejected(u16),
    #[error("Responses WebSocket connection failed: {0}")]
    Transport(String),
}

pub(crate) struct ResponsesWebSocketLease {
    registry: ResponsesWebSocketRegistry,
    namespace: String,
    connection_id: String,
    provider_id: String,
    target_id: String,
    transport_attempt: String,
    connection: tokio::sync::OwnedMutexGuard<ResponsesWebSocketConnection>,
    previous_response_id: Option<String>,
    session_affinity: Option<String>,
    reused_connection: bool,
    terminal: bool,
}

impl ResponsesWebSocketRegistry {
    #[cfg(test)]
    pub(crate) fn continuation_available(
        &self,
        namespace: &str,
        response_id: &str,
        cross_socket: bool,
    ) -> bool {
        if cross_socket {
            return true;
        }
        let mut state = self
            .state
            .lock()
            .expect("Responses WebSocket registry poisoned");
        prune_expired_connections(&mut state);
        state
            .affinity
            .contains_key(&ResponsesWebSocketAffinity::response(
                namespace,
                response_id,
            ))
    }

    pub(crate) async fn acquire(
        &self,
        client: &reqwest::Client,
        namespace: &str,
        trace: ResponsesWebSocketTrace<'_>,
        request: ResponsesWebSocketRequest<'_>,
        previous_response_id: Option<&str>,
        session_affinity: Option<&str>,
        require_affinity: bool,
    ) -> Result<ResponsesWebSocketLease, ResponsesWebSocketAcquireError> {
        let ResponsesWebSocketRequest { url, headers } = request;
        let response_affinity =
            previous_response_id.map(|id| ResponsesWebSocketAffinity::response(namespace, id));
        let session_affinity_key =
            session_affinity.map(|id| ResponsesWebSocketAffinity::session(namespace, id));
        let affinity = {
            let mut state = self
                .state
                .lock()
                .expect("Responses WebSocket registry poisoned");
            prune_expired_connections(&mut state);
            match state.capabilities.get(namespace) {
                Some(ResponsesWebSocketCapability::Unsupported) => {
                    return Err(ResponsesWebSocketAcquireError::Unsupported);
                }
                Some(ResponsesWebSocketCapability::CooldownUntil(until))
                    if *until > tokio::time::Instant::now() =>
                {
                    return Err(ResponsesWebSocketAcquireError::Cooldown);
                }
                _ => {
                    state.capabilities.remove(namespace);
                }
            }
            response_affinity
                .as_ref()
                .and_then(|key| {
                    state
                        .affinity
                        .get(key)
                        .map(|connection_id| (connection_id, true))
                })
                .or_else(|| {
                    session_affinity_key.as_ref().and_then(|key| {
                        state
                            .affinity
                            .get(key)
                            .map(|connection_id| (connection_id, false))
                    })
                })
                .and_then(|(connection_id, matched_response)| {
                    let record = state.connections.get(connection_id)?;
                    Some((
                        connection_id.clone(),
                        record.connection.clone(),
                        matched_response,
                    ))
                })
        };

        if let Some((connection_id, connection, matched_response)) = affinity {
            let connection = connection.lock_owned().await;
            let still_current = {
                let state = self
                    .state
                    .lock()
                    .expect("Responses WebSocket registry poisoned");
                state.connections.contains_key(&connection_id)
                    && if matched_response {
                        response_affinity
                            .as_ref()
                            .is_some_and(|key| state.affinity.get(key) == Some(&connection_id))
                    } else {
                        session_affinity_key
                            .as_ref()
                            .is_some_and(|key| state.affinity.get(key) == Some(&connection_id))
                    }
            };
            if still_current {
                let reusable_previous = previous_response_id
                    .filter(|previous| connection.tip.as_deref() == Some(*previous));
                tracing::debug!(
                    transport = "responses_websocket",
                    target_namespace = namespace,
                    provider_id = trace.provider_id,
                    target_id = trace.target_id,
                    transport_attempt = trace.transport_attempt,
                    continuation = true,
                    "reusing upstream connection"
                );
                return Ok(ResponsesWebSocketLease {
                    registry: self.clone(),
                    namespace: namespace.to_owned(),
                    connection_id,
                    provider_id: trace.provider_id.to_owned(),
                    target_id: trace.target_id.to_owned(),
                    transport_attempt: trace.transport_attempt.to_owned(),
                    connection,
                    previous_response_id: reusable_previous.map(str::to_owned),
                    session_affinity: session_affinity.map(str::to_owned),
                    reused_connection: true,
                    terminal: false,
                });
            }
            tracing::debug!(
                target_namespace = namespace,
                provider_id = trace.provider_id,
                target_id = trace.target_id,
                transport_attempt = trace.transport_attempt,
                "Responses WebSocket affinity tip changed while queued; opening a new connection"
            );
        }

        let connection_value = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        };
        let session_id = connection_value("session-id");
        let thread_id = connection_value("thread-id");
        let window_id = connection_value("x-codex-window-id");

        let response = {
            use reqwest_websocket::Upgrade as _;
            client
                .get(url)
                .version(reqwest::Version::HTTP_11)
                .headers(headers)
                .upgrade()
                .send()
                .await
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.mark_transient_failure(namespace, trace);
                return Err(ResponsesWebSocketAcquireError::Transport(error.to_string()));
            }
        };
        let socket = match response.into_websocket().await {
            Ok(socket) => socket,
            Err(reqwest_websocket::Error::Handshake(
                reqwest_websocket::HandshakeError::UnexpectedStatusCode(status),
            )) if status.is_success() || matches!(status.as_u16(), 400 | 404 | 405 | 426 | 501) => {
                self.state
                    .lock()
                    .expect("Responses WebSocket registry poisoned")
                    .capabilities
                    .insert(
                        namespace.to_owned(),
                        ResponsesWebSocketCapability::Unsupported,
                    );
                tracing::debug!(
                    transport = "responses_websocket",
                    target_namespace = namespace,
                    provider_id = trace.provider_id,
                    target_id = trace.target_id,
                    transport_attempt = trace.transport_attempt,
                    rejection_status = status.as_u16(),
                    capability = "unsupported",
                    "cached upstream WebSocket capability"
                );
                return Err(ResponsesWebSocketAcquireError::Unsupported);
            }
            Err(reqwest_websocket::Error::Handshake(
                reqwest_websocket::HandshakeError::UnexpectedStatusCode(status),
            )) if matches!(status.as_u16(), 401 | 403 | 429) => {
                return Err(ResponsesWebSocketAcquireError::Rejected(status.as_u16()));
            }
            Err(error) => {
                self.mark_transient_failure(namespace, trace);
                return Err(ResponsesWebSocketAcquireError::Transport(error.to_string()));
            }
        };
        let connection_id = uuid::Uuid::new_v4().to_string();
        let connection =
            std::sync::Arc::new(tokio::sync::Mutex::new(ResponsesWebSocketConnection {
                socket,
                tip: None,
                session_id,
                thread_id,
                window_id,
            }));
        {
            let mut state = self
                .state
                .lock()
                .expect("Responses WebSocket registry poisoned");
            state.connections.insert(
                connection_id.clone(),
                ResponsesWebSocketConnectionRecord {
                    connection: connection.clone(),
                    namespace: namespace.to_owned(),
                    provider_id: trace.provider_id.to_owned(),
                    target_id: trace.target_id.to_owned(),
                    transport_attempt: trace.transport_attempt.to_owned(),
                    created_at: tokio::time::Instant::now(),
                },
            );
            if let Some(key) = session_affinity_key {
                state.affinity.insert(key, connection_id.clone());
            }
        }
        let expiry_registry = self.clone();
        let expiry_connection_id = connection_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RESPONSES_WEBSOCKET_MAX_AGE).await;
            let removed = {
                let mut state = expiry_registry
                    .state
                    .lock()
                    .expect("Responses WebSocket registry poisoned");
                let removed = state.connections.remove(&expiry_connection_id);
                state
                    .affinity
                    .retain(|_, candidate| candidate != &expiry_connection_id);
                removed
            };
            if let Some(record) = removed {
                let mut connection = record.connection.lock().await;
                connection.tip = None;
                let _ = futures::SinkExt::close(&mut connection.socket).await;
                tracing::debug!(
                    transport = "responses_websocket",
                    target_namespace = record.namespace,
                    provider_id = record.provider_id,
                    target_id = record.target_id,
                    transport_attempt = record.transport_attempt,
                    connection_age_ms = RESPONSES_WEBSOCKET_MAX_AGE.as_millis(),
                    close_reason = "max_age",
                    "closed upstream connection"
                );
            }
        });
        tracing::debug!(
            transport = "responses_websocket",
            target_namespace = namespace,
            provider_id = trace.provider_id,
            target_id = trace.target_id,
            transport_attempt = trace.transport_attempt,
            "connected upstream"
        );
        Ok(ResponsesWebSocketLease {
            registry: self.clone(),
            namespace: namespace.to_owned(),
            connection_id,
            provider_id: trace.provider_id.to_owned(),
            target_id: trace.target_id.to_owned(),
            transport_attempt: trace.transport_attempt.to_owned(),
            connection: connection.lock_owned().await,
            previous_response_id: (!require_affinity)
                .then(|| previous_response_id.map(str::to_owned))
                .flatten(),
            session_affinity: session_affinity.map(str::to_owned),
            reused_connection: false,
            terminal: false,
        })
    }

    fn mark_transient_failure(&self, namespace: &str, trace: ResponsesWebSocketTrace<'_>) {
        self.state
            .lock()
            .expect("Responses WebSocket registry poisoned")
            .capabilities
            .insert(
                namespace.to_owned(),
                ResponsesWebSocketCapability::CooldownUntil(
                    tokio::time::Instant::now() + RESPONSES_WEBSOCKET_TRANSIENT_COOLDOWN,
                ),
            );
        tracing::debug!(
            transport = "responses_websocket",
            target_namespace = namespace,
            provider_id = trace.provider_id,
            target_id = trace.target_id,
            transport_attempt = trace.transport_attempt,
            cooldown_seconds = RESPONSES_WEBSOCKET_TRANSIENT_COOLDOWN.as_secs(),
            "entered upstream WebSocket capability cooldown"
        );
    }

    fn invalidate(&self, namespace: &str, response_id: &str, trace: ResponsesWebSocketTrace<'_>) {
        self.state
            .lock()
            .expect("Responses WebSocket registry poisoned")
            .affinity
            .remove(&ResponsesWebSocketAffinity::response(
                namespace,
                response_id,
            ));
        tracing::debug!(
            transport = "responses_websocket",
            target_namespace = namespace,
            provider_id = trace.provider_id,
            target_id = trace.target_id,
            transport_attempt = trace.transport_attempt,
            continuation = true,
            "invalidated upstream continuation affinity"
        );
    }
}

impl ResponsesWebSocketLease {
    pub(crate) fn connection_metadata(
        &self,
    ) -> crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata<'_> {
        crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata {
            session_id: &self.connection.session_id,
            thread_id: &self.connection.thread_id,
            window_id: &self.connection.window_id,
        }
    }

    pub(crate) fn trace(&self) -> ResponsesWebSocketTrace<'_> {
        ResponsesWebSocketTrace {
            provider_id: &self.provider_id,
            target_id: &self.target_id,
            transport_attempt: &self.transport_attempt,
        }
    }

    pub(crate) async fn send(&mut self, request: &serde_json::Value) -> anyhow::Result<()> {
        use futures::SinkExt as _;
        self.connection
            .socket
            .send(reqwest_websocket::Message::Text(serde_json::to_string(
                request,
            )?))
            .await?;
        Ok(())
    }

    pub(crate) async fn next(
        &mut self,
    ) -> Option<Result<reqwest_websocket::Message, reqwest_websocket::Error>> {
        use futures::StreamExt as _;
        self.connection.socket.next().await
    }

    pub(crate) fn completed(&mut self, response_id: String) {
        let mut state = self
            .registry
            .state
            .lock()
            .expect("Responses WebSocket registry poisoned");
        state.affinity.retain(|key, connection_id| {
            connection_id != &self.connection_id
                || matches!(key, ResponsesWebSocketAffinity::Session { .. })
        });
        state.affinity.insert(
            ResponsesWebSocketAffinity::response(&self.namespace, &response_id),
            self.connection_id.clone(),
        );
        if let Some(session_affinity) = &self.session_affinity {
            state.affinity.insert(
                ResponsesWebSocketAffinity::session(&self.namespace, session_affinity),
                self.connection_id.clone(),
            );
        }
        self.connection.tip = Some(response_id.clone());
        self.terminal = true;
        tracing::debug!(
            transport = "responses_websocket",
            target_namespace = self.namespace,
            provider_id = self.provider_id,
            target_id = self.target_id,
            transport_attempt = self.transport_attempt,
            continuation = true,
            "retained completed upstream connection affinity"
        );
    }

    pub(crate) fn terminal(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .expect("Responses WebSocket registry poisoned");
        let age_ms = state
            .connections
            .remove(&self.connection_id)
            .map(|record| record.created_at.elapsed().as_millis())
            .unwrap_or_default();
        state
            .affinity
            .retain(|_, candidate| candidate != &self.connection_id);
        self.connection.tip = None;
        self.terminal = true;
        tracing::debug!(
            transport = "responses_websocket",
            target_namespace = self.namespace,
            provider_id = self.provider_id,
            target_id = self.target_id,
            transport_attempt = self.transport_attempt,
            connection_age_ms = age_ms,
            close_reason = "terminal_without_reusable_response",
            "closed upstream connection"
        );
    }

    pub(crate) fn previous_response_id(&self) -> Option<&str> {
        self.previous_response_id.as_deref()
    }

    pub(crate) fn reused_connection(&self) -> bool {
        self.reused_connection
    }

    pub(crate) fn invalidate_previous(&mut self) {
        let Some(previous_response_id) = self.previous_response_id.clone() else {
            return;
        };
        self.registry.invalidate(
            &self.namespace,
            &previous_response_id,
            ResponsesWebSocketTrace {
                provider_id: &self.provider_id,
                target_id: &self.target_id,
                transport_attempt: &self.transport_attempt,
            },
        );
    }
    pub(crate) fn connection_limit(&mut self) {
        self.registry.mark_transient_failure(
            &self.namespace,
            ResponsesWebSocketTrace {
                provider_id: &self.provider_id,
                target_id: &self.target_id,
                transport_attempt: &self.transport_attempt,
            },
        );
        self.terminal();
    }
}

impl Drop for ResponsesWebSocketLease {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        let mut state = self
            .registry
            .state
            .lock()
            .expect("Responses WebSocket registry poisoned");
        let age_ms = state
            .connections
            .remove(&self.connection_id)
            .map(|record| record.created_at.elapsed().as_millis())
            .unwrap_or_default();
        state
            .affinity
            .retain(|_, candidate| candidate != &self.connection_id);
        self.connection.tip = None;
        tracing::debug!(
            transport = "responses_websocket",
            target_namespace = self.namespace,
            provider_id = self.provider_id,
            target_id = self.target_id,
            transport_attempt = self.transport_attempt,
            connection_age_ms = age_ms,
            close_reason = "lease_dropped_before_terminal",
            "closed upstream connection"
        );
    }
}

fn prune_expired_connections(state: &mut ResponsesWebSocketRegistryState) {
    let now = tokio::time::Instant::now();
    let expired = state
        .connections
        .iter()
        .filter(|(_, record)| now.duration_since(record.created_at) >= RESPONSES_WEBSOCKET_MAX_AGE)
        .map(|(id, record)| {
            tracing::debug!(
                transport = "responses_websocket",
                target_namespace = record.namespace,
                provider_id = record.provider_id,
                target_id = record.target_id,
                transport_attempt = record.transport_attempt,
                connection_age_ms = now.duration_since(record.created_at).as_millis(),
                close_reason = "max_age",
                "closed upstream connection"
            );
            id.clone()
        })
        .collect::<std::collections::HashSet<_>>();
    if expired.is_empty() {
        return;
    }
    state
        .connections
        .retain(|connection_id, _| !expired.contains(connection_id));
    state
        .affinity
        .retain(|_, connection_id| !expired.contains(connection_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use axum::Router;
    use axum::extract::{
        State, WebSocketUpgrade,
        ws::{Message as AxumMessage, WebSocket},
    };
    use axum::routing::get;

    fn test_trace() -> ResponsesWebSocketTrace<'static> {
        ResponsesWebSocketTrace {
            provider_id: "provider",
            target_id: "target",
            transport_attempt: "attempt",
        }
    }

    #[derive(Default)]
    struct WebSocketConnectionCounts {
        accepted: AtomicUsize,
        active: AtomicUsize,
    }

    #[derive(Clone)]
    struct WebSocketTestState {
        requests: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
        connections: std::sync::Arc<WebSocketConnectionCounts>,
    }

    async fn websocket_handler(
        State(state): State<WebSocketTestState>,
        upgrade: WebSocketUpgrade,
    ) -> impl axum::response::IntoResponse {
        upgrade.on_upgrade(move |socket| serve_websocket(socket, state))
    }

    async fn serve_websocket(mut socket: WebSocket, state: WebSocketTestState) {
        state.connections.accepted.fetch_add(1, Ordering::SeqCst);
        state.connections.active.fetch_add(1, Ordering::SeqCst);
        let active_connections = state.connections.clone();
        let mut ordinal = 0_u32;
        while let Some(Ok(AxumMessage::Text(text))) = socket.recv().await {
            ordinal += 1;
            state
                .requests
                .send(serde_json::from_str(&text).expect("request JSON"))
                .expect("request observer");
            socket
                .send(AxumMessage::Text(
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": format!("upstream-{ordinal}"),
                            "status": "completed",
                            "output": [],
                            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send terminal event");
        }
        active_connections.active.fetch_sub(1, Ordering::SeqCst);
    }

    async fn websocket_server() -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        std::sync::Arc<WebSocketConnectionCounts>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind WebSocket test server");
        let address = listener.local_addr().expect("WebSocket server address");
        let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
        let connections = std::sync::Arc::new(WebSocketConnectionCounts::default());
        let app = Router::new()
            .route("/v1/responses", get(websocket_handler))
            .with_state(WebSocketTestState {
                requests,
                connections: connections.clone(),
            });
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve WebSocket test endpoint");
        });
        (
            format!("ws://{address}/v1/responses"),
            receiver,
            connections,
        )
    }

    async fn handshake_status_server(
        status: axum::http::StatusCode,
    ) -> (String, std::sync::Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind handshake test server");
        let address = listener.local_addr().expect("handshake server address");
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = Router::new().route(
            "/v1/responses",
            get(move || {
                let observed = observed.clone();
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    status
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve handshake test endpoint");
        });
        (format!("ws://{address}/v1/responses"), calls)
    }

    async fn serve_once(response: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await.expect("read request");
            socket.write_all(response).await.expect("write response");
        });
        format!("http://{addr}/v1beta/models/gemini:generateContent?key=secret")
    }

    #[tokio::test]
    async fn non_stream_json_decode_error_retains_upstream_metadata() {
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-request-id: upstream-123\r\ncontent-length: 16\r\n\r\nnot valid json!!",
        )
        .await;
        let client = ProxyClient::new(reqwest::Client::new());

        let err = client
            .call_non_stream(
                &url,
                HeaderMap::new(),
                serde_json::json!({"model": "gemini"}),
            )
            .await
            .expect_err("invalid upstream JSON must fail");

        let decode = err
            .downcast_ref::<UpstreamResponseDecodeError>()
            .expect("decode failure should expose upstream status, headers, and raw body");
        assert_eq!(decode.status, 200);
        assert_eq!(
            decode
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some("upstream-123")
        );
        assert_eq!(decode.body_text(), "not valid json!!");
    }

    #[tokio::test]
    async fn responses_websocket_reuses_affinity_for_finalized_requests() {
        let (url, mut requests, connections) = websocket_server().await;
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();

        let mut first = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                None,
                None,
                true,
            )
            .await
            .expect("connect first request");
        let first_connection = {
            let metadata = first.connection_metadata();
            (
                metadata.session_id.to_owned(),
                metadata.thread_id.to_owned(),
            )
        };
        first
            .send(&serde_json::json!({
                "type": "response.create",
                "model": "gpt-test",
                "input": [{"role": "user", "content": "first"}]
            }))
            .await
            .expect("send first request");
        let first_request = requests.recv().await.expect("observe first request");
        assert_eq!(first_request["type"], "response.create");
        let _ = first.next().await.expect("first terminal event");
        first.completed("upstream-1".into());
        drop(first);

        assert!(registry.continuation_available("target", "upstream-1", false));
        let mut second = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                Some("upstream-1"),
                None,
                true,
            )
            .await
            .expect("reuse affinity");
        {
            let metadata = second.connection_metadata();
            assert_eq!(metadata.session_id, first_connection.0);
            assert_eq!(metadata.thread_id, first_connection.1);
        }
        second
            .send(&serde_json::json!({
                "type": "response.create",
                "model": "gpt-test",
                "previous_response_id": "upstream-1",
                "input": [{"role": "user", "content": "second"}]
            }))
            .await
            .expect("send continuation");
        let second_request = requests.recv().await.expect("observe continuation");
        assert_eq!(
            second_request["previous_response_id"],
            serde_json::json!("upstream-1")
        );
        assert_eq!(connections.accepted.load(Ordering::SeqCst), 1);
        let _ = second.next().await.expect("second terminal event");
        second.completed("upstream-2".into());
    }

    #[tokio::test]
    async fn responses_websocket_reuses_session_affinity_for_full_history_requests() {
        let (url, mut requests, connections) = websocket_server().await;
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();

        let mut first = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                None,
                Some("session-1"),
                true,
            )
            .await
            .expect("connect first request");
        let first_connection = {
            let metadata = first.connection_metadata();
            (
                metadata.session_id.to_owned(),
                metadata.thread_id.to_owned(),
                metadata.window_id.to_owned(),
            )
        };
        first
            .send(&serde_json::json!({
                "type": "response.create",
                "input": ["first"]
            }))
            .await
            .expect("send first request");
        let _ = requests.recv().await.expect("observe first request");
        let _ = first.next().await.expect("first terminal event");
        first.completed("upstream-1".into());
        drop(first);

        let mut second = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                None,
                Some("session-1"),
                true,
            )
            .await
            .expect("reuse session affinity");
        assert_eq!(second.previous_response_id(), None);
        {
            let metadata = second.connection_metadata();
            assert_eq!(metadata.session_id, first_connection.0);
            assert_eq!(metadata.thread_id, first_connection.1);
            assert_eq!(metadata.window_id, first_connection.2);
        }
        second
            .send(&serde_json::json!({
                "type": "response.create",
                "input": ["first", "second"]
            }))
            .await
            .expect("send full second request");
        let second_request = requests.recv().await.expect("observe second request");
        assert!(second_request.get("previous_response_id").is_none());
        assert_eq!(connections.accepted.load(Ordering::SeqCst), 1);
        let _ = second.next().await.expect("second terminal event");
        second.completed("upstream-2".into());
    }

    #[tokio::test]
    async fn stale_store_false_sibling_opens_new_socket_without_parent_affinity() {
        let (url, mut requests, connections) = websocket_server().await;
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();

        let mut parent = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                None,
                None,
                true,
            )
            .await
            .expect("connect parent");
        parent
            .send(&serde_json::json!({
                "type": "response.create",
                "model": "gpt-test",
                "input": ["parent"]
            }))
            .await
            .expect("send parent");
        let _ = requests.recv().await.expect("observe parent");
        let _ = parent.next().await.expect("parent terminal");
        parent.completed("upstream-1".into());
        drop(parent);

        let mut first_branch = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                Some("upstream-1"),
                None,
                true,
            )
            .await
            .expect("acquire first branch");
        let second_registry = registry.clone();
        let second_client = client.clone();
        let second_url = url.clone();
        let second_branch = tokio::spawn(async move {
            second_registry
                .acquire(
                    &second_client,
                    "target",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: &second_url,
                        headers: HeaderMap::new(),
                    },
                    Some("upstream-1"),
                    None,
                    true,
                )
                .await
        });

        first_branch
            .send(&serde_json::json!({
                "type": "response.create",
                "model": "gpt-test",
                "previous_response_id": "upstream-1",
                "input": ["first branch"]
            }))
            .await
            .expect("send first branch");
        let _ = requests.recv().await.expect("observe first branch");
        let _ = first_branch.next().await.expect("first branch terminal");
        first_branch.completed("upstream-2".into());
        drop(first_branch);

        let mut second_branch = second_branch
            .await
            .expect("second branch task")
            .expect("open second branch socket");
        assert_eq!(second_branch.previous_response_id(), None);
        second_branch
            .send(&serde_json::json!({
                "type": "response.create",
                "model": "gpt-test",
                "input": ["parent", "second branch"]
            }))
            .await
            .expect("send full second branch");
        let second_request = requests.recv().await.expect("observe second branch");
        assert!(second_request.get("previous_response_id").is_none());
        assert_eq!(
            second_request["input"],
            serde_json::json!(["parent", "second branch"])
        );
        assert_eq!(connections.accepted.load(Ordering::SeqCst), 2);
        let _ = second_branch.next().await.expect("second branch terminal");
        second_branch.completed("upstream-3".into());
    }

    #[tokio::test]
    async fn unsupported_capability_is_cached_per_target_namespace() {
        let (url, calls) = handshake_status_server(axum::http::StatusCode::NOT_FOUND).await;
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();

        assert!(matches!(
            registry
                .acquire(
                    &client,
                    "target-a",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: &url,
                        headers: HeaderMap::new(),
                    },
                    None,
                    None,
                    false,
                )
                .await,
            Err(ResponsesWebSocketAcquireError::Unsupported)
        ));
        assert!(matches!(
            registry
                .acquire(
                    &client,
                    "target-a",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: &url,
                        headers: HeaderMap::new(),
                    },
                    None,
                    None,
                    false,
                )
                .await,
            Err(ResponsesWebSocketAcquireError::Unsupported)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(matches!(
            registry
                .acquire(
                    &client,
                    "target-b",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: &url,
                        headers: HeaderMap::new(),
                    },
                    None,
                    None,
                    false,
                )
                .await,
            Err(ResponsesWebSocketAcquireError::Unsupported)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn transient_connect_failure_enters_a_short_target_cooldown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve unused address");
        let address = listener.local_addr().expect("unused address");
        drop(listener);
        let url = format!("ws://{address}/v1/responses");
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();

        assert!(matches!(
            registry
                .acquire(
                    &client,
                    "target",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: &url,
                        headers: HeaderMap::new(),
                    },
                    None,
                    None,
                    false,
                )
                .await,
            Err(ResponsesWebSocketAcquireError::Transport(_))
        ));
        assert!(matches!(
            registry
                .acquire(
                    &client,
                    "target",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: &url,
                        headers: HeaderMap::new(),
                    },
                    None,
                    None,
                    false,
                )
                .await,
            Err(ResponsesWebSocketAcquireError::Cooldown)
        ));
    }

    #[tokio::test]
    async fn auth_and_rate_limit_handshake_statuses_remain_typed_rejections() {
        for status in [
            axum::http::StatusCode::UNAUTHORIZED,
            axum::http::StatusCode::TOO_MANY_REQUESTS,
        ] {
            let (url, _) = handshake_status_server(status).await;
            let result = ResponsesWebSocketRegistry::default()
                .acquire(
                    &reqwest::Client::new(),
                    "target",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: &url,
                        headers: HeaderMap::new(),
                    },
                    None,
                    None,
                    false,
                )
                .await;
            let Err(ResponsesWebSocketAcquireError::Rejected(code)) = result else {
                panic!("{status} must remain a typed handshake rejection");
            };
            assert_eq!(code, status.as_u16());
        }
    }

    #[tokio::test]
    async fn websocket_upgrade_uses_reqwest_connect_proxy_and_proxy_authentication() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy");
        let address = listener.local_addr().expect("proxy address");
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept proxy client");
            let mut buffer = vec![0_u8; 4096];
            let length = socket.read(&mut buffer).await.expect("read CONNECT");
            let request = String::from_utf8_lossy(&buffer[..length]).to_string();
            let _ = observed_tx.send(request);
            socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("reject CONNECT");
        });
        let proxy = reqwest::Proxy::all(format!("http://{address}"))
            .expect("proxy URL")
            .basic_auth("proxy-user", "proxy-pass");
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .build()
            .expect("proxied client");

        assert!(matches!(
            ResponsesWebSocketRegistry::default()
                .acquire(
                    &client,
                    "target",
                    test_trace(),
                    ResponsesWebSocketRequest {
                        url: "wss://provider.invalid/v1/responses",
                        headers: HeaderMap::new(),
                    },
                    None,
                    None,
                    false,
                )
                .await,
            Err(ResponsesWebSocketAcquireError::Transport(_))
        ));
        let observed = observed_rx.await.expect("observed CONNECT request");
        assert!(
            observed.starts_with("CONNECT provider.invalid:443 HTTP/1.1\r\n"),
            "{observed}"
        );
        assert!(
            observed
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic chjvehktdxnlcjpwcm94es1wyxnz"),
            "{observed}"
        );
    }

    #[tokio::test]
    async fn expired_affinity_opens_a_new_connection_for_store_false_history() {
        let (url, mut requests, connections) = websocket_server().await;
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();
        let mut parent = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                None,
                None,
                true,
            )
            .await
            .expect("connect parent");
        parent
            .send(&serde_json::json!({"type": "response.create", "input": ["parent"]}))
            .await
            .expect("send parent");
        let _ = requests.recv().await.expect("observe parent");
        let _ = parent.next().await.expect("parent terminal");
        parent.completed("upstream-1".into());
        drop(parent);

        {
            let mut state = registry
                .state
                .lock()
                .expect("Responses WebSocket registry poisoned");
            let connection_id = state
                .affinity
                .get(&ResponsesWebSocketAffinity::response(
                    "target",
                    "upstream-1",
                ))
                .expect("parent affinity")
                .clone();
            state
                .connections
                .get_mut(&connection_id)
                .expect("parent connection")
                .created_at -= RESPONSES_WEBSOCKET_MAX_AGE;
        }
        assert!(!registry.continuation_available("target", "upstream-1", false));

        let continuation = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                Some("upstream-1"),
                None,
                true,
            )
            .await
            .expect("open replacement connection");
        assert_eq!(continuation.previous_response_id(), None);
        assert_eq!(connections.accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn completed_affinity_is_closed_at_the_hard_max_age_without_new_traffic() {
        let (url, mut requests, _connections) = websocket_server().await;
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();
        let mut lease = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                None,
                None,
                true,
            )
            .await
            .expect("connect request");
        lease
            .send(&serde_json::json!({
                "type": "response.create",
                "input": ["parent"]
            }))
            .await
            .expect("send request");
        let _ = requests.recv().await.expect("observe request");
        let _ = lease.next().await.expect("terminal event");
        lease.completed("upstream-1".into());
        drop(lease);
        assert!(registry.continuation_available("target", "upstream-1", false));

        tokio::time::advance(RESPONSES_WEBSOCKET_MAX_AGE).await;
        tokio::task::yield_now().await;

        let state = registry
            .state
            .lock()
            .expect("Responses WebSocket registry poisoned");
        assert!(state.connections.is_empty());
        assert!(state.affinity.is_empty());
    }

    #[tokio::test]
    async fn dropping_an_inflight_lease_removes_and_closes_its_socket() {
        let (url, _requests, connections) = websocket_server().await;
        let registry = ResponsesWebSocketRegistry::default();
        let client = reqwest::Client::new();
        let lease = registry
            .acquire(
                &client,
                "target",
                test_trace(),
                ResponsesWebSocketRequest {
                    url: &url,
                    headers: HeaderMap::new(),
                },
                None,
                None,
                false,
            )
            .await
            .expect("connect request");
        assert_eq!(
            registry
                .state
                .lock()
                .expect("Responses WebSocket registry poisoned")
                .connections
                .len(),
            1
        );
        assert_eq!(connections.active.load(Ordering::SeqCst), 1);

        drop(lease);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while connections.active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping an in-flight lease must close the upstream socket");

        let state = registry
            .state
            .lock()
            .expect("Responses WebSocket registry poisoned");
        assert!(state.connections.is_empty());
        assert!(state.affinity.is_empty());
    }
}
