use super::*;
use serde_json::{Value, json};

const MODEL: &str = "responses-websocket-continuation-fixture";

#[derive(Clone, Debug)]
struct ObservedFrame {
    socket: usize,
    frame: Value,
}

#[derive(Clone, Debug)]
struct ObservedHandshake {
    authorization: Option<String>,
}

#[derive(Clone, Copy)]
enum UpstreamError {
    MissingPrevious,
    OrdinaryBadRequest,
    VisibleMissingPrevious,
}

struct FrameGate {
    prompt: &'static str,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

impl FrameGate {
    fn new(prompt: &'static str) -> Self {
        Self {
            prompt,
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    async fn wait_for_frame(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.entered.acquire())
            .await
            .expect("branch reaches the local upstream")
            .expect("frame gate open")
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

#[derive(Clone)]
struct UpstreamState {
    frames: Arc<parking_lot::Mutex<Vec<ObservedFrame>>>,
    handshakes: Arc<parking_lot::Mutex<Vec<ObservedHandshake>>>,
    next_socket: Arc<AtomicUsize>,
    next_response: Arc<AtomicUsize>,
    close_after_first: Arc<std::sync::atomic::AtomicBool>,
    next_error: Arc<parking_lot::Mutex<Option<UpstreamError>>>,
    reject_full_replay: Arc<std::sync::atomic::AtomicBool>,
    gate: Arc<parking_lot::Mutex<Option<Arc<FrameGate>>>>,
}

struct ResponsesContinuationFixture {
    gateway: Gateway,
    _directory: tempfile::TempDir,
    upstream: tokio::task::JoinHandle<()>,
    state: UpstreamState,
    authorization: HeaderValue,
    provider_id: String,
}

impl Drop for ResponsesContinuationFixture {
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

fn header_text(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .map(|value| value.to_str().expect("fixture handshake header").to_owned())
}

async fn responses_websocket(
    State(state): State<UpstreamState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    let socket = state.next_socket.fetch_add(1, Ordering::SeqCst);
    state.handshakes.lock().push(ObservedHandshake {
        authorization: header_text(&headers, "authorization"),
    });
    upgrade.on_upgrade(move |mut websocket| async move {
        while let Some(Ok(AxumWebSocketMessage::Text(text))) = websocket.recv().await {
            let frame: Value =
                serde_json::from_str(&text).expect("Responses WebSocket request JSON");
            state.frames.lock().push(ObservedFrame {
                socket,
                frame: frame.clone(),
            });
            let gate = { state.gate.lock().clone() };
            if let Some(gate) = gate
                && frame["input"].to_string().contains(gate.prompt)
            {
                gate.entered.add_permits(1);
                gate.release
                    .acquire()
                    .await
                    .expect("release gated frame")
                    .forget();
            }
            let next_error = if frame.get("previous_response_id").is_some() {
                state.next_error.lock().take()
            } else if state.next_response.load(Ordering::SeqCst) > 0
                && state.reject_full_replay.swap(false, Ordering::SeqCst)
            {
                Some(UpstreamError::MissingPrevious)
            } else {
                None
            };
            if let Some(error) = next_error {
                if matches!(error, UpstreamError::VisibleMissingPrevious) {
                    let created = openai_responses_sse("not delivered")
                        .split("\n\n")
                        .find_map(|event| {
                            event.lines().find_map(|line| {
                                line.strip_prefix("data: ")
                                    .filter(|data| data.contains("\"response.created\""))
                                    .map(str::to_owned)
                            })
                        })
                        .expect("response.created fixture event");
                    websocket
                        .send(AxumWebSocketMessage::Text(created.into()))
                        .await
                        .expect("send visible response event");
                }
                let (code, message) = if matches!(error, UpstreamError::OrdinaryBadRequest) {
                    ("invalid_request_error", "Unsupported parameter")
                } else {
                    (
                        "previous_response_not_found",
                        "Connection-local response expired",
                    )
                };
                websocket
                    .send(AxumWebSocketMessage::Text(
                        json!({
                            "type": "error",
                            "status": 400,
                            "error": {"code": code, "message": message}
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .expect("send upstream error");
                continue;
            }
            let ordinal = state.next_response.fetch_add(1, Ordering::SeqCst);
            let answer = format!("answer-{ordinal}");
            let id = format!("upstream-{ordinal}");
            for event in openai_responses_sse(&answer)
                .replace("resp-provider", &id)
                .replace("msg-1", &format!("message-{ordinal}"))
                .split("\n\n")
                .filter_map(|event| {
                    event.lines().find_map(|line| {
                        line.strip_prefix("data: ")
                            .filter(|data| *data != "[DONE]")
                            .map(str::to_owned)
                    })
                })
            {
                websocket
                    .send(AxumWebSocketMessage::Text(event.into()))
                    .await
                    .expect("send Responses WebSocket response event");
            }
            if ordinal == 0 && state.close_after_first.load(Ordering::SeqCst) {
                websocket
                    .send(AxumWebSocketMessage::Close(None))
                    .await
                    .expect("close first upstream socket");
                break;
            }
        }
    })
}

async fn responses_websocket_fixture() -> ResponsesContinuationFixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local Responses upstream");
    let address = listener.local_addr().expect("local Responses address");
    let base_url = format!("http://{address}/v1");
    let state = UpstreamState {
        frames: Arc::new(parking_lot::Mutex::new(Vec::new())),
        handshakes: Arc::new(parking_lot::Mutex::new(Vec::new())),
        next_socket: Arc::new(AtomicUsize::new(0)),
        next_response: Arc::new(AtomicUsize::new(0)),
        close_after_first: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        next_error: Arc::new(parking_lot::Mutex::new(None)),
        reject_full_replay: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        gate: Arc::new(parking_lot::Mutex::new(None)),
    };
    let app = Router::new()
        .route("/v1/responses", get(responses_websocket))
        .with_state(state.clone());
    let upstream = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("local Responses WebSocket server");
    });

    let directory = tempfile::tempdir().expect("gateway directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Responses Gateway");
    configure_route_with_protocol(&gateway, MODEL, &[base_url], "openai", "open-responses").await;
    let provider_id = gateway
        .storage
        .routes()
        .list()
        .await
        .expect("fixture routes")
        .into_iter()
        .find(|route| route.model_id == MODEL)
        .expect("fixture route")
        .targets[0]
        .provider_id()
        .clone()
        .into();
    let headers = authorized_headers(&gateway).await;
    ResponsesContinuationFixture {
        gateway,
        _directory: directory,
        upstream,
        state,
        authorization: headers[header::AUTHORIZATION].clone(),
        provider_id,
    }
}

async fn post_responses(
    fixture: &ResponsesContinuationFixture,
    input: Vec<Value>,
) -> (StatusCode, Value) {
    post_responses_in_session(fixture, input, "stable-responses-prompt-cache-key").await
}

async fn post_responses_in_session(
    fixture: &ResponsesContinuationFixture,
    input: Vec<Value>,
    cache_key: &str,
) -> (StatusCode, Value) {
    let response = crate::proxy::server::create_router(fixture.gateway.clone())
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, fixture.authorization.clone())
                .body(Body::from(
                    json!({
                        "model": MODEL,
                        "store": false,
                        "prompt_cache_key": cache_key,
                        "input": input,
                    })
                    .to_string(),
                ))
                .expect("public Responses WebSocket request"),
        )
        .await
        .expect("public Responses WebSocket response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Responses WebSocket response body");
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "Responses WebSocket response JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });
    (status, body)
}

fn user_input(prompt: &str) -> Value {
    json!({"role": "user", "content": prompt})
}

#[tokio::test]
async fn responses_store_false_continuation_reuses_socket_with_exact_tip_and_delta() {
    let fixture = responses_websocket_fixture().await;
    let first = user_input("first");
    let (status, first_response) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{first_response}");
    let second = user_input("second");
    let (status, second_response) = post_responses(
        &fixture,
        vec![first, first_response["output"][0].clone(), second],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second_response}");
    assert_eq!(
        second_response["output"][0]["content"][0]["text"],
        "answer-1"
    );

    let frames = fixture.state.frames.lock();
    let handshakes = fixture.state.handshakes.lock();
    assert_eq!(
        frames.len(),
        2,
        "two completed upstream requests: {frames:?}"
    );
    assert_eq!(handshakes.len(), 1, "one Responses socket: {handshakes:?}");
    assert_eq!(frames[0].socket, frames[1].socket);
    assert_eq!(frames[1].frame["previous_response_id"], "upstream-0");
    assert_eq!(frames[1].frame["input"].as_array().map(Vec::len), Some(1));
    assert!(frames[1].frame["input"][0].to_string().contains("second"));
    assert_eq!(
        handshakes[0].authorization.as_deref(),
        Some("Bearer test-key")
    );
}

fn continued_input(first: &Value, response: &Value, prompt: &str) -> Vec<Value> {
    vec![
        first.clone(),
        response["output"][0].clone(),
        user_input(prompt),
    ]
}

#[tokio::test]
async fn responses_closed_socket_replays_full_history_without_consuming_retry_budget() {
    let fixture = responses_websocket_fixture().await;
    fixture
        .state
        .close_after_first
        .store(true, Ordering::SeqCst);
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    let (status, child) = post_responses(&fixture, continued_input(&first, &parent, "child")).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let frames = fixture.state.frames.lock();
    assert_eq!(
        frames.len(),
        2,
        "local socket miss must not emit a failed frame: {frames:?}"
    );
    assert_ne!(frames[0].socket, frames[1].socket);
    assert!(frames[1].frame.get("previous_response_id").is_none());
    assert_eq!(frames[1].frame["input"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn responses_evicted_socket_sends_full_history_without_a_speculative_continuation() {
    let fixture = responses_websocket_fixture().await;
    let first = user_input("evicted parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");

    // 通过独立会话填满文档规定的 64 条空闲池，不直接操纵宿主内部状态。
    for index in 0..64 {
        let (status, response) = post_responses_in_session(
            &fixture,
            vec![user_input(&format!("independent root {index}"))],
            &format!("eviction-session-{index}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
    }

    let (status, child) = post_responses(&fixture, continued_input(&first, &parent, "child")).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let frames = fixture.state.frames.lock();
    assert_eq!(
        frames.len(),
        66,
        "eviction must not send a failed incremental request"
    );
    let child_frame = frames.last().expect("child request");
    assert_ne!(frames[0].socket, child_frame.socket);
    assert!(child_frame.frame.get("previous_response_id").is_none());
    assert_eq!(child_frame.frame["input"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn responses_concurrent_children_never_send_parent_tip_to_another_socket() {
    let fixture = Arc::new(responses_websocket_fixture().await);
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    let gate = Arc::new(FrameGate::new("branch-a"));
    *fixture.state.gate.lock() = Some(gate.clone());
    let branch_a_fixture = fixture.clone();
    let branch_a_input = continued_input(&first, &parent, "branch-a");
    let branch_a =
        tokio::spawn(async move { post_responses(&branch_a_fixture, branch_a_input).await });
    gate.wait_for_frame().await;
    let (status, sibling) =
        post_responses(&fixture, continued_input(&first, &parent, "branch-b")).await;
    assert_eq!(status, StatusCode::OK, "{sibling}");
    gate.release();
    let (status, branch_a_response) = branch_a.await.expect("branch-a task");
    assert_eq!(status, StatusCode::OK, "{branch_a_response}");

    let frames = fixture.state.frames.lock();
    let parent_frame = frames
        .iter()
        .find(|record| {
            record.frame["input"].to_string().contains("parent")
                && !record.frame["input"].to_string().contains("branch-")
        })
        .expect("parent frame");
    let a = frames
        .iter()
        .find(|record| record.frame["input"].to_string().contains("branch-a"))
        .expect("branch-a frame");
    let b = frames
        .iter()
        .find(|record| record.frame["input"].to_string().contains("branch-b"))
        .expect("branch-b frame");
    assert_eq!(a.socket, parent_frame.socket);
    assert_eq!(a.frame["previous_response_id"], "upstream-0");
    assert_ne!(b.socket, parent_frame.socket);
    assert!(b.frame.get("previous_response_id").is_none());
    assert_eq!(b.frame["input"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn responses_advanced_tip_forces_sibling_full_history() {
    let fixture = responses_websocket_fixture().await;
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    let (status, child) =
        post_responses(&fixture, continued_input(&first, &parent, "child-a")).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let (status, sibling) =
        post_responses(&fixture, continued_input(&first, &parent, "child-b")).await;
    assert_eq!(status, StatusCode::OK, "{sibling}");
    let frames = fixture.state.frames.lock();
    assert_eq!(
        frames.len(),
        3,
        "no speculative failed continuation: {frames:?}"
    );
    assert_eq!(frames[0].socket, frames[1].socket);
    assert_eq!(frames[1].frame["previous_response_id"], "upstream-0");
    assert!(frames[2].frame.get("previous_response_id").is_none());
    assert_eq!(frames[2].frame["input"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn responses_coded_missing_tip_replays_once_with_existing_retry_budget() {
    let fixture = responses_websocket_fixture().await;
    set_target_retry_budget(&fixture.gateway, MODEL, 1).await;
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    *fixture.state.next_error.lock() = Some(UpstreamError::MissingPrevious);
    let (status, child) = post_responses(&fixture, continued_input(&first, &parent, "child")).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let frames = fixture.state.frames.lock();
    assert_eq!(
        frames.len(),
        3,
        "one continuation and one full replay: {frames:?}"
    );
    assert_eq!(frames[1].frame["previous_response_id"], "upstream-0");
    assert!(frames[2].frame.get("previous_response_id").is_none());
    assert_eq!(frames[2].frame["input"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn responses_remote_missing_tip_requires_existing_retry_budget() {
    let fixture = responses_websocket_fixture().await;
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    *fixture.state.next_error.lock() = Some(UpstreamError::MissingPrevious);
    let (status, _) = post_responses(&fixture, continued_input(&first, &parent, "child")).await;
    assert_ne!(status, StatusCode::OK);
    let frames = fixture.state.frames.lock();
    assert_eq!(
        frames.len(),
        2,
        "remote miss with retry budget zero must not replay: {frames:?}"
    );
    assert_eq!(frames[1].frame["previous_response_id"], "upstream-0");
}

#[tokio::test]
async fn responses_failed_full_replay_is_not_retried_again() {
    let fixture = responses_websocket_fixture().await;
    set_target_retry_budget(&fixture.gateway, MODEL, 1).await;
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    *fixture.state.next_error.lock() = Some(UpstreamError::MissingPrevious);
    fixture
        .state
        .reject_full_replay
        .store(true, Ordering::SeqCst);
    let (status, _) = post_responses(&fixture, continued_input(&first, &parent, "child")).await;
    assert_ne!(status, StatusCode::OK);
    let frames = fixture.state.frames.lock();
    assert_eq!(
        frames.len(),
        3,
        "only one full-history replay permitted: {frames:?}"
    );
    assert_eq!(frames[1].frame["previous_response_id"], "upstream-0");
    assert!(frames[2].frame.get("previous_response_id").is_none());
    assert_eq!(frames[2].frame["input"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn responses_ordinary_bad_request_does_not_replay() {
    let fixture = responses_websocket_fixture().await;
    set_target_retry_budget(&fixture.gateway, MODEL, 1).await;
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    *fixture.state.next_error.lock() = Some(UpstreamError::OrdinaryBadRequest);
    let (status, _) =
        post_responses(&fixture, continued_input(&first, &parent, "bad-request")).await;
    assert_ne!(status, StatusCode::OK);
    let frames = fixture.state.frames.lock();
    assert_eq!(frames.len(), 2, "ordinary 400 must not replay: {frames:?}");
    assert_eq!(frames[1].frame["previous_response_id"], "upstream-0");
}

#[tokio::test]
async fn responses_visible_event_before_missing_tip_is_not_replayed() {
    let fixture = responses_websocket_fixture().await;
    set_target_retry_budget(&fixture.gateway, MODEL, 1).await;
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    *fixture.state.next_error.lock() = Some(UpstreamError::VisibleMissingPrevious);
    let (status, _) = post_responses(&fixture, continued_input(&first, &parent, "started")).await;
    assert_ne!(status, StatusCode::OK);
    let frames = fixture.state.frames.lock();
    assert_eq!(frames.len(), 2, "response event forbids replay: {frames:?}");
}

#[tokio::test]
async fn responses_api_key_change_never_reuses_socket() {
    let fixture = responses_websocket_fixture().await;
    let first = user_input("parent");
    let (status, parent) = post_responses(&fixture, vec![first.clone()]).await;
    assert_eq!(status, StatusCode::OK, "{parent}");
    fixture
        .gateway
        .admin()
        .update_provider(
            &fixture.provider_id,
            crate::db::models::UpdateProvider {
                api_key: Some("rotated-test-key".into()),
                ..Default::default()
            },
        )
        .await
        .expect("rotate provider API key");
    let (status, child) = post_responses(&fixture, continued_input(&first, &parent, "child")).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let frames = fixture.state.frames.lock();
    let handshakes = fixture.state.handshakes.lock();
    assert_eq!(frames.len(), 2, "no mismatched socket attempt: {frames:?}");
    assert_eq!(handshakes.len(), 2);
    assert_ne!(frames[0].socket, frames[1].socket);
    assert!(frames[1].frame.get("previous_response_id").is_none());
    assert_eq!(frames[1].frame["input"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        handshakes[0].authorization.as_deref(),
        Some("Bearer test-key")
    );
    assert_eq!(
        handshakes[1].authorization.as_deref(),
        Some("Bearer rotated-test-key")
    );
}
