use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use std::time::{Duration, Instant};
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;

use reqwest::header::{HeaderMap, HeaderValue};

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::{
    CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput, ProviderSourceInput,
};
use crate::provider_models::CreateManualProviderModel;
use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::thinking::ThinkingLevel;

async fn drain_test_http_request(socket: &mut tokio::net::TcpStream) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.expect("read test request");
        assert_ne!(read, 0, "peer closed before sending a complete request");
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            return;
        }
    }
}

async fn add_test_provider_model(gateway: &Gateway, provider_id: &str) {
    gateway
        .admin()
        .create_manual_provider_model(
            provider_id,
            "upstream-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "upstream-model",
                    "name": "upstream-model",
                }),
                template_id: None,
            },
        )
        .await
        .expect("Provider Model");
}

async fn serve_openai_status(status: u16, body: serde_json::Value) -> (String, Arc<AtomicUsize>) {
    serve_openai_status_repeated(status, body, 1).await
}

async fn serve_openai_status_repeated(
    status: u16,
    body: serde_json::Value,
    request_count: usize,
) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    tokio::spawn(async move {
        let body = body.to_string();
        for _ in 0..request_count {
            let (mut socket, _) = listener.accept().await.expect("accept provider request");
            let mut request = vec![0_u8; 16 * 1024];
            let bytes_read = socket.read(&mut request).await.expect("read request");
            request.truncate(bytes_read);
            observed.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
    });
    (format!("http://{address}/v1"), calls)
}

async fn serve_rate_limit_then_success(retry_after: u64) -> (String, Arc<Mutex<Vec<Instant>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            if socket.read(&mut request).await.unwrap() == 0 {
                break;
            }
            observed.lock().push(Instant::now());
            let (status, headers, body) = if attempt == 0 {
                (
                    429,
                    format!("retry-after: {retry_after}\r\n"),
                    serde_json::json!({"error":{"message":"temporarily rate limited","type":"rate_limit_error"}}),
                )
            } else {
                (
                    200,
                    String::new(),
                    serde_json::json!({"id":"retry-success","object":"chat.completion","created":1,"model":"upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"retry succeeded"},"finish_reason":"stop"}]}),
                )
            };
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}/v1"), calls)
}

async fn retry_window_fixture(
    model: &str,
    backup: bool,
    retry_after: u64,
) -> (
    tempfile::TempDir,
    Gateway,
    crate::db::models::ApiKeyWithBindings,
    Arc<Mutex<Vec<Instant>>>,
) {
    let (dir, gateway, _, key) =
        gateway_with_captured_thinking(model, true, "backup response", None).await;
    let success = gateway.admin().get_model(model).await.unwrap().targets[0]
        .provider_id()
        .to_string();
    let (url, calls) = serve_rate_limit_then_success(retry_after).await;
    let limited = add_captured_thinking_provider(&gateway, url).await;
    let mut target = thinking_target(&limited, &[], 20);
    target.target_retry_budget = Some(5);
    let mut targets = vec![target];
    if backup {
        targets.push(thinking_target(&success, &[], 10));
    }
    set_thinking_targets(&gateway, model, targets).await;
    (dir, gateway, key, calls)
}

#[tokio::test]
async fn retry_after_outside_fixed_window_fails_over() {
    let (_dir, gateway, key, calls) =
        retry_window_fixture("retry-window-backup", true, 11450).await;
    let turn = tokio::time::timeout(
        Duration::from_secs(2),
        gateway.model_turn.execute(
            TurnInput::new(
                Principal::new(key.id),
                AiRequest::new("retry-window-backup", Vec::new()),
            )
            .with_execution(
                CancellationToken::new(),
                Deadline::fixed(Instant::now() + Duration::from_secs(5)),
            ),
        ),
    )
    .await
    .expect("failover must not wait for the request deadline")
    .expect("backup must succeed before deadline");
    let output = turn.output.collect::<Vec<_>>().await;
    assert!(
        matches!(output.last(), Some(Ok(CanonicalEvent::Completed(response))) if response.output_text() == "backup response")
    );
    assert_eq!(calls.lock().len(), 1);
}

#[tokio::test]
async fn retry_after_outside_shared_window_preserves_upstream_error() {
    let (_dir, gateway, key, calls) =
        retry_window_fixture("retry-window-no-backup", false, 11450).await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        gateway.model_turn.execute(
            TurnInput::new(
                Principal::new(key.id),
                AiRequest::new("retry-window-no-backup", Vec::new()),
            )
            .with_execution(
                CancellationToken::new(),
                Deadline::from_now(Duration::from_secs(5)),
            ),
        ),
    )
    .await
    .expect("upstream error must return before idle expiry");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("rate limit must fail"),
    };
    assert_eq!(error.upstream_status, Some(429));
    assert_ne!(error.code, "deadline_exceeded");
    assert_eq!(calls.lock().len(), 1);
}

#[tokio::test]
async fn retry_after_within_window_respects_delay() {
    let (_dir, gateway, key, calls) = retry_window_fixture("retry-window-delay", false, 1).await;
    let turn = gateway
        .model_turn
        .execute(
            TurnInput::new(
                Principal::new(key.id),
                AiRequest::new("retry-window-delay", Vec::new()),
            )
            .with_execution(
                CancellationToken::new(),
                Deadline::fixed(Instant::now() + Duration::from_secs(5)),
            ),
        )
        .await
        .expect("same target retry");
    let output = turn.output.collect::<Vec<_>>().await;
    assert!(
        matches!(output.last(), Some(Ok(CanonicalEvent::Completed(response))) if response.output_text() == "retry succeeded")
    );
    let calls = calls.lock();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].duration_since(calls[0]) >= Duration::from_secs(1));
}

async fn serve_openai_response(body: serde_json::Value) -> (String, Arc<AtomicUsize>) {
    serve_openai_status(200, body).await
}

async fn serve_incomplete_openai_stream(request_count: usize) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind streaming provider");
    let address = listener.local_addr().expect("provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    tokio::spawn(async move {
        for _ in 0..request_count {
            let (mut socket, _) = listener.accept().await.expect("accept provider request");
            let mut request = vec![0_u8; 16 * 1024];
            let _ = socket.read(&mut request).await.expect("read request");
            observed.fetch_add(1, Ordering::SeqCst);
            let frame = format!(
                "data: {}\n\n",
                serde_json::json!({
                        "id": "chatcmpl-partial",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": "upstream-model",
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant", "content": "partial"},
                            "finish_reason": null
                        }]
                })
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{frame}",
                frame.len() + 1024
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write partial response");
        }
    });
    (format!("http://{address}/v1"), calls)
}

async fn serve_zdr_then_responses_stream() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ZDR provider");
    let address = listener.local_addr().expect("ZDR provider address");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&captured);
    tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept ZDR request");
            let mut request = vec![0_u8; 16 * 1024];
            let bytes_read = socket.read(&mut request).await.expect("read ZDR request");
            request.truncate(bytes_read);
            let (_, body) = captured_http(&request);
            observed.lock().push(body);

            let (status, content_type, body) = if attempt == 0 {
                (
                    "404 Not Found",
                    "application/json",
                    serde_json::json!({
                        "code": "not-found",
                        "error": "Previous response cannot be used for this organization due to Zero Data Retention"
        })
                    .to_string(),
                )
            } else {
                let created =
                    stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                        "resp-replayed",
                        "upstream-model",
                        "in_progress",
                        Vec::new(),
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                    );
                let completed =
                    stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                        "resp-replayed",
                        "upstream-model",
                        "completed",
                        Vec::new(),
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                    );
                (
                    "200 OK",
                    "text/event-stream",
                    format!(
                        "event: response.created\ndata: {}\n\n\
                         event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
                        serde_json::json!({
                            "type": "response.created",
                            "sequence_number": 0,
                            "response": created,
                        }),
                        serde_json::json!({
                            "type": "response.completed",
                            "sequence_number": 1,
                            "response": completed,
                        }),
                    ),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write ZDR response");
        }
    });
    (format!("http://{address}/v1"), captured)
}

async fn serve_openai_capture_text(text: &'static str) -> (String, Arc<Mutex<Vec<u8>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind capturing provider");
    let address = listener.local_addr().expect("capturing provider address");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&captured);
    tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .expect("accept capturing provider request");
        let mut request = vec![0_u8; 16 * 1024];
        let bytes_read = socket.read(&mut request).await.expect("read request");
        request.truncate(bytes_read);
        *observed.lock() = request;
        let body = serde_json::json!({
            "id": "chatcmpl-capture",
            "object": "chat.completion",
            "created": 1,
            "model": "upstream-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 0}
            }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write capturing response");
    });
    (format!("http://{address}/v1"), captured)
}

fn captured_http(raw: &[u8]) -> (String, serde_json::Value) {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let body = serde_json::from_str(body.trim_end_matches('\0')).unwrap_or(serde_json::Value::Null);
    (head.to_owned(), body)
}

async fn gateway_with_captured_model(
    model_name: &str,
    bind_key: bool,
) -> (
    tempfile::TempDir,
    crate::Gateway,
    Arc<Mutex<Vec<u8>>>,
    crate::db::models::ApiKeyWithBindings,
) {
    gateway_with_captured_thinking(model_name, bind_key, "ok", None).await
}

async fn gateway_with_captured_thinking(
    model_name: &str,
    bind_key: bool,
    text: &'static str,
    default_thinking_level: Option<ThinkingLevel>,
) -> (
    tempfile::TempDir,
    crate::Gateway,
    Arc<Mutex<Vec<u8>>>,
    crate::db::models::ApiKeyWithBindings,
) {
    let (base_url, captured) = serve_openai_capture_text(text).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Capture".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    admin
        .create_manual_provider_model(
            &provider.id,
            "upstream-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "upstream-model",
                    "reasoning_efforts": ["none", "minimal", "low", "medium", "high"],
                }),
                template_id: None,
            },
        )
        .await
        .expect("Provider Model with declared thinking efforts");
    let model = admin
        .create_model(CreateRoute {
            model_id: model_name.into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                rpm_pool_id: None,
                provider_id: provider.id.clone(),
                model: Some("upstream-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level,
        })
        .await
        .expect("Model");
    let model_ids = if bind_key {
        vec![model.id.into()]
    } else {
        let other_model = admin
            .create_model(CreateRoute {
                model_id: format!("{model_name}-other"),
                display_name: None,
                balance: None,
                targets: vec![CreateTarget {
                    rpm_pool_id: None,
                    provider_id: provider.id,
                    model: Some("upstream-model".into()),
                    enabled: true,
                    priority: None,
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                }],
                default_thinking_level: None,
            })
            .await
            .expect("other Model");
        vec![other_model.id.into()]
    };
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Capture key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids,
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    (data_dir, gateway, captured, key)
}

#[tokio::test]
async fn in_memory_adapter_uses_the_same_execute_interface() {
    let mut response = AiResponse::new("response-1", "model-1");
    response.push_output_text("scripted");
    let executor = InMemoryModelTurnExecutor::scripted([response]);
    let request = AiRequest::new("model-1", Vec::new());

    let turn = executor
        .execute(TurnInput::new(
            Principal::new("principal-1"),
            request.clone(),
        ))
        .await
        .expect("Model Turn");
    let events = turn.output.collect::<Vec<_>>().await;

    let requests = executor.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, request.model);
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(response)))
            if response.output_text() == "scripted"
    ));
}

#[tokio::test]
async fn execute_distinguishes_cancellation_from_deadline() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await
    .expect("Gateway");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = match gateway
        .model_turn
        .execute(
            TurnInput::new(
                Principal::new("principal"),
                AiRequest::new("model", Vec::new()),
            )
            .with_execution(
                cancellation,
                stravia_runtime_contract::Deadline::from_now(Duration::from_secs(1)),
            ),
        )
        .await
    {
        Ok(_) => panic!("cancelled turn must fail"),
        Err(error) => error,
    };
    let deadline = match gateway
        .model_turn
        .execute(
            TurnInput::new(
                Principal::new("principal"),
                AiRequest::new("model", Vec::new()),
            )
            .with_execution(
                CancellationToken::new(),
                stravia_runtime_contract::Deadline::fixed(Instant::now()),
            ),
        )
        .await
    {
        Ok(_) => panic!("expired turn must fail"),
        Err(error) => error,
    };

    assert_eq!(cancelled.code, "cancelled");
    assert_eq!(deadline.code, "deadline_exceeded");
}

#[tokio::test]
async fn first_token_timeout_records_one_precise_attempt_terminal_without_usage() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        while !request.ends_with(b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let _ = tokio::io::copy(&mut socket, &mut tokio::io::sink()).await;
    });
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Timeout fixture".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .unwrap();
    add_test_provider_model(&gateway, &provider.id).await;
    let model = admin
        .create_model(CreateRoute {
            model_id: "timeout-model".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                rpm_pool_id: None,
                enabled: true,
                provider_id: provider.id,
                model: Some("upstream-model".into()),
                priority: None,
                first_token_timeout_ms: Some(1000),
                target_retry_budget: Some(0),
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .unwrap();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Timeout fixture".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .unwrap();
    let principal = Principal::new(key.id);
    let observer = credential_observer(&gateway, &principal);
    let mut request = AiRequest::new("timeout-model", Vec::new());
    request.stream.enabled = true;
    let result = gateway
        .model_turn
        .execute(TurnInput::new(principal, request).with_observer(observer))
        .await;
    assert!(
        matches!(result, Err(ModelTurnError { ref code, .. }) if code == "first_token_timeout")
    );
    gateway.observation.flush().await.unwrap();
    let events: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT payload FROM observation_events WHERE kind='target_attempt_finished'",
    )
    .fetch_all(gateway._sqlite_pool.as_ref().unwrap())
    .await
    .unwrap();
    assert_eq!(events.len(), 1);
    let terminal: serde_json::Value =
        serde_json::from_slice(&crate::storage_codec::decode(&events[0]).unwrap()).unwrap();
    assert_eq!(terminal["error_code"], "first_token_timeout");
    assert_eq!(terminal["status"], "failed");
    assert!(terminal["first_token_ms"].is_null());
    assert!(terminal.get("usage").is_none_or(serde_json::Value::is_null));
    upstream.abort();
    let _ = upstream.await;
}

#[tokio::test]
async fn execute_fails_over_before_canonical_output_and_returns_the_locked_target() {
    let (failed_url, failed_calls) =
        serve_openai_status(500, serde_json::json!({"error": {"message": "retry"}})).await;
    let (fallback_url, fallback_calls) = serve_openai_response(serde_json::json!({
        "id": "chatcmpl-fallback",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "fallback"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
    }))
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let mut providers = Vec::new();
    for (name, base_url) in [("primary", failed_url), ("fallback", fallback_url)] {
        let provider = admin
            .create_provider(CreateProvider {
                name: Some(name.into()),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some("openai-compatible".into()),
                    base_url,
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "test-provider-key".into(),
                },
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await
            .expect("Provider");
        providers.push(provider);
    }
    for provider in &providers {
        add_test_provider_model(&gateway, &provider.id).await;
    }
    let model = admin
        .create_model(CreateRoute {
            model_id: "failover-model".into(),
            display_name: None,
            balance: Some("traffic_equalization".into()),
            targets: providers
                .iter()
                .enumerate()
                .map(|(index, provider)| CreateTarget {
                    enabled: true,
                    provider_id: provider.id.clone(),
                    model: Some("upstream-model".into()),
                    priority: Some((providers.len() - index) as i32),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(0),
                    target_cooldown_ms: None,
                    rpm_pool_id: None,
                    thinking_level_map: Vec::new(),
                })
                .collect(),
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Failover key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");

    let turn = gateway
        .model_turn
        .execute(TurnInput::new(
            Principal::new(key.id),
            AiRequest::new("failover-model", Vec::new()),
        ))
        .await
        .expect("fallback Model Turn");
    let locked_provider = turn.route.provider_id.clone();
    let events = turn.output.collect::<Vec<_>>().await;

    assert_eq!(locked_provider, providers[1].id);
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, Ok(CanonicalEvent::Delta(_))) })
    );
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(response)))
            if response.output_text() == "fallback"
    ));
}

#[tokio::test]
async fn http_continuation_not_retained_by_zdr_replays_full_request_once() {
    let (base_url, captured) = serve_zdr_then_responses_stream().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("ZDR".into()),
            source: ProviderSourceInput::Custom {
                vendor: "xai".into(),
                channel: "default".into(),
                protocol: Some("open-responses".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    add_test_provider_model(&gateway, &provider.id).await;
    let model = admin
        .create_model(CreateRoute {
            model_id: "zdr-model".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                rpm_pool_id: None,
                provider_id: provider.id,
                model: Some("upstream-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "ZDR key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let executor = LiveModelTurnExecutor::new(
        gateway,
        crate::router::continuation::ScriptedContinuation::hit("resp-zdr"),
    );
    let mut request = AiRequest::new(
        "zdr-model",
        vec![stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                "follow-up".into(),
            ),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = true;
    request.meta.source_protocol =
        Some(stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24);
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt::default(),
        ),
    );

    let turn = executor
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("ZDR continuation falls back to full replay");
    let events = turn.output.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(response)))
            if response.id == "resp-replayed"
    ));

    let captured = captured.lock();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["previous_response_id"], "resp-zdr");
    assert!(captured[1].get("previous_response_id").is_none());
}

#[tokio::test]
async fn request_scoped_http_errors_count_without_same_target_retries() {
    let (base_url, calls) = serve_openai_status_repeated(
        404,
        serde_json::json!({"error": {"message": "request-specific resource is missing"}}),
        6,
    )
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Request scoped failure".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    add_test_provider_model(&gateway, &provider.id).await;
    let model = admin
        .create_model(CreateRoute {
            model_id: "request-error-model".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                rpm_pool_id: None,
                provider_id: provider.id,
                model: Some("upstream-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Request error key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");

    for failure_count in 1..=6 {
        let result = gateway
            .model_turn
            .execute(TurnInput::new(
                Principal::new(key.id.clone()),
                AiRequest::new("request-error-model", Vec::new()),
            ))
            .await;
        let Err(error) = result else {
            panic!("request-scoped 404 must fail the request");
        };
        assert_eq!(error.code, "upstream_error");
        assert_eq!(calls.load(Ordering::SeqCst), failure_count);
    }
    let result = gateway
        .model_turn
        .execute(TurnInput::new(
            Principal::new(key.id),
            AiRequest::new("request-error-model", Vec::new()),
        ))
        .await;
    assert!(matches!(result, Err(ModelTurnError { code, .. }) if code == "model_unavailable"));
    assert_eq!(calls.load(Ordering::SeqCst), 6);
}

mod cooldown_isolation {
    use super::*;

    async fn execute_root(
        gateway: &Gateway,
        key: &str,
        root: &crate::rpm::RootRequest,
    ) -> Result<Vec<Result<CanonicalEvent, ModelTurnError>>, ModelTurnError> {
        let mut input = TurnInput::new(
            Principal::new(key.to_owned()),
            AiRequest::new("root-cooldown", Vec::new()),
        );
        input.root_request = root.clone();
        let turn = gateway.model_turn.execute(input).await?;
        Ok(turn.output.collect().await)
    }

    #[tokio::test]
    async fn independent_roots_each_receive_one_exception_and_full_success_restores() {
        let (_directory, gateway, _capture, key) =
            gateway_with_captured_model("root-cooldown", true).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tokio::spawn(async move {
            for index in 0..6 {
                let (mut socket, _) = listener.accept().await.unwrap();
                drain_test_http_request(&mut socket).await;
                observed.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if index < 2 {
                    (
                        400,
                        serde_json::json!({"error":{"code":"unsupported_parameter",
                        "message":"Unsupported parameter"}}),
                    )
                } else if index < 4 {
                    (
                        404,
                        serde_json::json!({"error":{"message":"resource missing"}}),
                    )
                } else {
                    (
                        200,
                        serde_json::json!({"id":"restored","object":"chat.completion",
                        "model":"upstream-model","choices":[{"index":0,
                        "message":{"role":"assistant","content":"restored"},
                        "finish_reason":"stop"}],"usage":{"prompt_tokens":1,
                        "completion_tokens":1,"total_tokens":2}}),
                    )
                };
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let provider =
            add_captured_thinking_provider(&gateway, format!("http://{address}/v1")).await;
        let mut target = thinking_target(&provider, &[], 0);
        target.target_cooldown_ms = Some(60_000);
        set_thinking_targets(&gateway, "root-cooldown", vec![target]).await;
        let first = crate::rpm::RootRequest::default();
        let second = crate::rpm::RootRequest::default();
        assert!(execute_root(&gateway, &key.id, &first).await.is_err());
        assert!(execute_root(&gateway, &key.id, &second).await.is_err());
        assert!(
            execute_root(&gateway, &key.id, &crate::rpm::RootRequest::default())
                .await
                .is_err()
        );
        let unrelated = execute_root(&gateway, &key.id, &crate::rpm::RootRequest::default()).await;
        assert!(
            matches!(unrelated, Err(ModelTurnError { code, .. }) if code == "model_unavailable")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(execute_root(&gateway, &key.id, &first).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let hidden = execute_root(&gateway, &key.id, &first).await;
        assert!(matches!(hidden, Err(ModelTurnError { code, .. }) if code == "model_unavailable"));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let events = execute_root(&gateway, &key.id, &second).await.unwrap();
        assert!(events.iter().any(
            |event| matches!(event, Ok(CanonicalEvent::Completed(response))
                    if response.error.is_none() && response.output_texts().eq(["restored"]))
        ));
        let events = execute_root(&gateway, &key.id, &crate::rpm::RootRequest::default())
            .await
            .unwrap();
        assert!(events.iter().any(
            |event| matches!(event, Ok(CanonicalEvent::Completed(response))
                    if response.error.is_none() && response.output_texts().eq(["restored"]))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn rejected_transport_resend_does_not_refund_a_consumed_root_exception() {
        let (_directory, gateway, _capture, key) =
            gateway_with_captured_model("root-cooldown", true).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tokio::spawn(async move {
            for index in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                drain_test_http_request(&mut socket).await;
                observed.fetch_add(1, Ordering::SeqCst);
                let response = match index {
                    0 => {
                        let body = r#"{"error":{"code":"unsupported_parameter","message":"Unsupported parameter"}}"#;
                        format!("HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
                    }
                    1 => "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".into(),
                    _ => "HTTP/1.1 307 Temporary Redirect\r\nlocation: /v1/chat/completions\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let provider =
            add_captured_thinking_provider(&gateway, format!("http://{address}/v1")).await;
        let mut target = thinking_target(&provider, &[], 0);
        target.target_cooldown_ms = Some(60_000);
        set_thinking_targets(&gateway, "root-cooldown", vec![target]).await;
        let root = crate::rpm::RootRequest::default();
        assert!(execute_root(&gateway, &key.id, &root).await.is_err());
        assert!(
            execute_root(&gateway, &key.id, &Default::default())
                .await
                .is_err()
        );
        let resend = execute_root(&gateway, &key.id, &root).await;
        assert!(matches!(resend, Err(ModelTurnError { code, .. }) if code == "target_ineligible"));
        let hidden = execute_root(&gateway, &key.id, &root).await;
        assert!(matches!(hidden, Err(ModelTurnError { code, .. }) if code == "model_unavailable"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn parameter_rejections_preserve_existing_failure_budget() {
        let (_directory, gateway, _capture, key) =
            gateway_with_captured_model("isolated-errors", true).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tokio::spawn(async move {
            for index in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                drain_test_http_request(&mut socket).await;
                observed.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if index == 0 {
                    (
                        400,
                        serde_json::json!({"error":{"type":"invalid_request_error",
                        "message":"Request rejected"}}),
                    )
                } else if index == 4 {
                    (
                        400,
                        serde_json::json!({"error":{"code":"insufficient_quota",
                        "message":"Account quota exhausted"}}),
                    )
                } else {
                    let code = [
                        "unsupported_parameter",
                        "context_length_exceeded",
                        "content_policy_violation",
                    ][index - 1];
                    (
                        400,
                        serde_json::json!({"error":{"code":code,
                        "message":"This request cannot be accepted"}}),
                    )
                };
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let provider =
            add_captured_thinking_provider(&gateway, format!("http://{address}/v1")).await;
        let mut target = thinking_target(&provider, &[], 0);
        target.target_retry_budget = Some(1);
        target.target_cooldown_ms = Some(60_000);
        set_thinking_targets(&gateway, "isolated-errors", vec![target]).await;
        for expected in 1..=5 {
            let result = gateway
                .model_turn
                .execute(TurnInput::new(
                    Principal::new(key.id.clone()),
                    AiRequest::new("isolated-errors", Vec::new()),
                ))
                .await;
            assert!(matches!(result, Err(ModelTurnError { code, .. }) if code == "upstream_error"));
            assert_eq!(calls.load(Ordering::SeqCst), expected);
        }
        let result = gateway
            .model_turn
            .execute(TurnInput::new(
                Principal::new(key.id),
                AiRequest::new("isolated-errors", Vec::new()),
            ))
            .await;
        assert!(matches!(result, Err(ModelTurnError { code, .. }) if code == "model_unavailable"));
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn newer_cooldown_does_not_cancel_active_stream_or_accept_its_stale_success() {
        let (_directory, gateway, _capture, key) =
            gateway_with_captured_model("root-cooldown", true).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let upstream_release = release.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            drain_test_http_request(&mut stream).await;
            let prefix = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id":"active","object":"chat.completion.chunk","model":"upstream-model",
                    "choices":[{"index":0,"delta":{"role":"assistant","content":"first"},
                        "finish_reason":null}]
                })
            );
            let suffix = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "id":"active","object":"chat.completion.chunk","model":"upstream-model",
                    "choices":[{"index":0,"delta":{"content":"last"},"finish_reason":"stop"}]
                })
            );
            stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{prefix}", prefix.len() + suffix.len()).as_bytes()).await.unwrap();
            tokio::spawn(async move {
                upstream_release.notified().await;
                stream.write_all(suffix.as_bytes()).await.unwrap();
            });
            let (mut failure, _) = listener.accept().await.unwrap();
            drain_test_http_request(&mut failure).await;
            let body = serde_json::json!({"error":{"message":"resource missing"}}).to_string();
            failure.write_all(format!("HTTP/1.1 404 Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let provider =
            add_captured_thinking_provider(&gateway, format!("http://{address}/v1")).await;
        let mut target = thinking_target(&provider, &[], 0);
        target.target_cooldown_ms = Some(60_000);
        set_thinking_targets(&gateway, "root-cooldown", vec![target]).await;
        let mut request = AiRequest::new("root-cooldown", Vec::new());
        request.stream.enabled = true;
        let turn = gateway
            .model_turn
            .execute(TurnInput::new(Principal::new(key.id.clone()), request))
            .await
            .unwrap();
        assert!(
            execute_root(&gateway, &key.id, &crate::rpm::RootRequest::default())
                .await
                .is_err()
        );
        release.notify_one();
        let events = turn.output.collect::<Vec<_>>().await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(CanonicalEvent::Delta(AiStreamDelta::TextDelta(text))) if text == "last"))
        );
        assert!(matches!(
            events.last(),
            Some(Ok(CanonicalEvent::Completed(_)))
        ));
        let unrelated = execute_root(&gateway, &key.id, &crate::rpm::RootRequest::default()).await;
        assert!(
            matches!(unrelated, Err(ModelTurnError { code, .. }) if code == "model_unavailable")
        );
    }
}

mod rpm_admission {
    use stravia_vendor_sdk::AiErrorKind;

    use super::*;
    use crate::rpm::{DestinationRpmLimit, RpmConfig, RpmPool};

    struct Fixture {
        _directory: tempfile::TempDir,
        gateway: Arc<Gateway>,
        key: String,
        providers: [String; 2],
        calls: [Arc<AtomicUsize>; 2],
    }

    struct Clock {
        keeper: tokio::task::JoinHandle<()>,
    }

    impl Clock {
        fn pause() -> Self {
            tokio::time::pause();
            // 外部 HTTP/SQLite I/O 仍须调度，不能让虚拟钟自动跳过窗口边界。
            let keeper = tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            });
            Self { keeper }
        }
    }

    impl Drop for Clock {
        fn drop(&mut self) {
            self.keeper.abort();
            tokio::time::resume();
        }
    }

    async fn upstream() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let observed = observed.clone();
                tokio::spawn(async move {
                    drain_test_http_request(&mut socket).await;
                    observed.fetch_add(1, Ordering::SeqCst);
                    let body = serde_json::json!({"id":"rpm-response",
                        "object":"chat.completion","model":"upstream-model",
                        "choices":[{"index":0,"message":{"role":"assistant",
                        "content":"admitted"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
                    .to_string();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                });
            }
        });
        (format!("http://{address}/v1"), calls)
    }

    fn target(provider: &str, pool: Option<&str>) -> CreateTarget {
        CreateTarget {
            provider_id: provider.into(),
            model: Some("upstream-model".into()),
            enabled: true,
            priority: Some(0),
            first_token_timeout_ms: Some(60_000),
            target_retry_budget: Some(0),
            target_cooldown_ms: Some(0),
            rpm_pool_id: pool.map(str::to_owned),
            thinking_level_map: Vec::new(),
        }
    }

    async fn fixture(shared_destination: bool) -> Fixture {
        let (url_a, calls_a) = upstream().await;
        let (url_b, calls_b) = upstream().await;
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(
            Gateway::new(GatewayConfig {
                data_dir: directory.path().to_path_buf(),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let provider_a = add_captured_thinking_provider(&gateway, url_a).await;
        let provider_b = add_captured_thinking_provider(&gateway, url_b).await;
        let mut route_ids = Vec::new();
        for (name, provider) in [
            ("rpm-a", &provider_a),
            (
                "rpm-b",
                if shared_destination {
                    &provider_a
                } else {
                    &provider_b
                },
            ),
        ] {
            route_ids.push(
                gateway
                    .admin()
                    .create_model(CreateRoute {
                        model_id: name.into(),
                        display_name: None,
                        balance: None,
                        targets: vec![target(provider, None)],
                        default_thinking_level: None,
                    })
                    .await
                    .unwrap()
                    .id
                    .to_string(),
            );
        }
        let key = gateway
            .admin()
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: "Target RPM behavior".into(),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                inject_web_search: false,
                inject_media_generation: false,
                inject_media_understanding: false,
                model_ids: route_ids,
            })
            .await
            .unwrap()
            .id;
        Fixture {
            _directory: directory,
            gateway,
            key,
            providers: [provider_a, provider_b],
            calls: [calls_a, calls_b],
        }
    }

    async fn configure(fixture: &Fixture, config: RpmConfig) {
        fixture
            .gateway
            .admin()
            .set_setting(
                crate::rpm::SETTINGS_KEY,
                &serde_json::to_string(&config).unwrap(),
            )
            .await
            .unwrap();
    }

    fn destination(provider: &str, limit: i32) -> DestinationRpmLimit {
        DestinationRpmLimit {
            provider_id: provider.into(),
            model: Some("upstream-model".into()),
            rpm_limit: Some(limit),
        }
    }

    async fn execute(
        gateway: &Gateway,
        key: &str,
        model: &str,
    ) -> Result<Vec<Result<CanonicalEvent, ModelTurnError>>, ModelTurnError> {
        let input = TurnInput::new(
            Principal::new(key.to_owned()),
            AiRequest::new(model, Vec::new()),
        )
        .with_execution(
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        let turn = gateway.model_turn.execute(input).await?;
        Ok(turn.output.collect().await)
    }

    fn exhausted(result: Result<Vec<Result<CanonicalEvent, ModelTurnError>>, ModelTurnError>) {
        assert!(
            matches!(result, Err(ModelTurnError { code, .. }) if code == "target_rpm_exceeded")
        );
    }

    fn spawn_root(
        fixture: &Fixture,
        root: crate::rpm::RootRequest,
        cancellation: CancellationToken,
        deadline: stravia_runtime_contract::Deadline,
    ) -> tokio::task::JoinHandle<Result<Vec<Result<CanonicalEvent, ModelTurnError>>, ModelTurnError>>
    {
        let gateway = fixture.gateway.clone();
        let key = fixture.key.clone();
        tokio::spawn(async move {
            let mut input =
                TurnInput::new(Principal::new(key), AiRequest::new("rpm-a", Vec::new()))
                    .with_execution(cancellation, deadline);
            input.root_request = root;
            let turn = gateway.model_turn.execute(input).await?;
            Ok(turn.output.collect().await)
        })
    }

    #[tokio::test]
    async fn published_rebinding_or_disable_invalidates_an_inflight_send_snapshot() {
        for disable in [false, true] {
            let fixture = fixture(false).await;
            configure(
                &fixture,
                RpmConfig {
                    total_wait_ms: 0,
                    preferred_wait_ms: 0,
                    pools: vec![RpmPool {
                        id: "full".into(),
                        name: "Full".into(),
                        rpm_limit: Some(1),
                    }],
                    ..Default::default()
                },
            )
            .await;
            set_thinking_targets(
                &fixture.gateway,
                "rpm-b",
                vec![target(&fixture.providers[1], Some("full"))],
            )
            .await;
            let events = execute(&fixture.gateway, &fixture.key, "rpm-b")
                .await
                .unwrap();
            assert!(events.iter().any(
                |event| matches!(event, Ok(CanonicalEvent::Completed(response))
                    if response.error.is_none() && response.output_texts().eq(["admitted"]))
            ));
            assert_eq!(fixture.calls[1].load(Ordering::SeqCst), 1);
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            *fixture.gateway.rpm_admission.validation_gate.lock() =
                Some((entered.clone(), release.clone()));
            let pending = spawn_root(
                &fixture,
                Default::default(),
                CancellationToken::new(),
                stravia_runtime_contract::Deadline::never(),
            );
            entered.notified().await;
            let mut updated = target(&fixture.providers[0], Some("full"));
            updated.enabled = !disable;
            set_thinking_targets(
                &fixture.gateway,
                "rpm-a",
                vec![updated, target(&fixture.providers[1], Some("full"))],
            )
            .await;
            release.notify_one();
            let result = pending.await.unwrap();
            assert!(
                result.is_err()
                    || result.as_ref().is_ok_and(|events| {
                        events.iter().any(|event| {
                            event.is_err()
                                || matches!(event, Ok(CanonicalEvent::Completed(response))
                        if response.error.is_some())
                        })
                    }),
                "invalidated send unexpectedly completed successfully"
            );
            assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_single_attempt_sends_only_one_actual_http_request() {
        let fixture = fixture(false).await;
        configure(
            &fixture,
            RpmConfig {
                destinations: vec![destination(&fixture.providers[0], 2)],
                ..Default::default()
            },
        )
        .await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *fixture.gateway.rpm_admission.validation_gate.lock() = Some((entered.clone(), release));
        let (capture, captured) = tokio::sync::oneshot::channel();
        *fixture.gateway.rpm_admission.capture_send.lock() = Some(capture);
        let pending = spawn_root(
            &fixture,
            Default::default(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        let mut admission = captured.await.unwrap();
        entered.notified().await;
        admission.eligibility.as_mut().unwrap().single_attempt = true;
        let (url, calls) = upstream().await;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut competitors = Vec::new();
        for _ in 0..2 {
            let admission = admission.clone();
            let url = url.clone();
            let barrier = barrier.clone();
            competitors.push(tokio::spawn(async move {
                barrier.wait().await;
                if admission.acquire().await.is_ok() {
                    reqwest::Client::new().get(url).send().await.unwrap();
                    true
                } else {
                    false
                }
            }));
        }
        barrier.wait().await;
        let first = competitors.remove(0).await.unwrap();
        let second = competitors.remove(0).await.unwrap();
        assert_ne!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Two legal pool slots do not grant a second single-attempt opportunity.
        assert!(admission.acquire().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn a_window_release_at_the_wait_deadline_cannot_start_a_late_send() {
        let fixture = fixture(true).await;
        configure(
            &fixture,
            RpmConfig {
                destinations: vec![destination(&fixture.providers[0], 1)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        let queued = spawn_root(
            &fixture,
            Default::default(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        tokio::time::advance(Duration::from_millis(30_001)).await;
        exhausted(queued.await.unwrap());
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rebinding_a_queued_target_uses_its_current_pool() {
        let mut fixture = fixture(false).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sent = Arc::new(tokio::sync::Notify::new());
        let observed = sent.clone();
        tokio::spawn(async move {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                drain_test_http_request(&mut socket).await;
                if index == 1 {
                    observed.notify_one();
                }
                let body = serde_json::json!({"id":"rebound","model":"upstream-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"rebound"},
                    "finish_reason":"stop"}]})
                .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        fixture.providers[0] =
            add_captured_thinking_provider(&fixture.gateway, format!("http://{address}/v1")).await;
        set_thinking_targets(
            &fixture.gateway,
            "rpm-a",
            vec![target(&fixture.providers[0], None)],
        )
        .await;
        let config = RpmConfig {
            destinations: vec![destination(&fixture.providers[0], 1)],
            pools: vec![RpmPool {
                id: "new-pool".into(),
                name: "New capacity".into(),
                rpm_limit: None,
            }],
            ..Default::default()
        };
        configure(&fixture, config).await;
        let _clock = Clock::pause();
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        let queued = spawn_root(
            &fixture,
            Default::default(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        set_thinking_targets(
            &fixture.gateway,
            "rpm-a",
            vec![target(&fixture.providers[0], Some("new-pool"))],
        )
        .await;
        tokio::select! {
            biased;
            () = sent.notified() => {}
            () = fixture.gateway.rpm_admission.wait_started.notified() => {
                queued.abort();
                panic!("queued request retained its old exhausted pool after rebinding");
            }
        }
        let events = queued.await.unwrap().unwrap();
        assert!(events.iter().any(
            |event| matches!(event, Ok(CanonicalEvent::Completed(response))
            if response.error.is_none() && response.output_texts().eq(["rebound"]))
        ));
    }

    #[tokio::test]
    async fn quota_exhaustion_is_not_rewritten_as_backup_rpm_recovery() {
        let mut fixture = fixture(false).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tokio::spawn(async move {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                drain_test_http_request(&mut socket).await;
                observed.fetch_add(1, Ordering::SeqCst);
                let (status, code) = if index == 0 {
                    (400, "insufficient_quota")
                } else {
                    (503, "overloaded_error")
                };
                let body = serde_json::json!({"error":{"code":code,"message":code}}).to_string();
                socket.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        fixture.providers[0] =
            add_captured_thinking_provider(&fixture.gateway, format!("http://{address}/v1")).await;
        let mut primary = target(&fixture.providers[0], None);
        primary.priority = Some(10);
        set_thinking_targets(
            &fixture.gateway,
            "rpm-a",
            vec![primary, target(&fixture.providers[1], None)],
        )
        .await;
        configure(
            &fixture,
            RpmConfig {
                preferred_wait_ms: 0,
                total_wait_ms: 0,
                destinations: vec![destination(&fixture.providers[1], 1)],
                ..Default::default()
            },
        )
        .await;
        execute(&fixture.gateway, &fixture.key, "rpm-b")
            .await
            .unwrap();
        let quota = execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap_err();
        assert_eq!(quota.upstream_error_kind, Some(AiErrorKind::QuotaExceeded));
        assert!(quota.retry_after_secs.is_none());
        let transient = execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap_err();
        assert_eq!(transient.code, "target_rpm_exceeded");
        assert!(
            transient
                .retry_after_secs
                .is_some_and(|seconds| seconds > 0 && seconds <= 60)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.calls[1].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transport_redirects_debit_each_real_send_in_the_destination_pool() {
        let mut fixture = fixture(false).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tokio::spawn(async move {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                drain_test_http_request(&mut socket).await;
                observed.fetch_add(1, Ordering::SeqCst);
                let response = if index == 0 {
                    "HTTP/1.1 307 Temporary Redirect\r\nlocation: /v1/redirected\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into()
                } else {
                    let body = serde_json::json!({"id":"redirected",
                        "object":"chat.completion","model":"upstream-model",
                        "choices":[{"index":0,"message":{"role":"assistant",
                        "content":"redirected"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        fixture.providers[0] =
            add_captured_thinking_provider(&fixture.gateway, format!("http://{address}/v1")).await;
        set_thinking_targets(
            &fixture.gateway,
            "rpm-a",
            vec![target(&fixture.providers[0], None)],
        )
        .await;
        configure(
            &fixture,
            RpmConfig {
                preferred_wait_ms: 0,
                total_wait_ms: 0,
                destinations: vec![destination(&fixture.providers[0], 2)],
                ..Default::default()
            },
        )
        .await;
        let events = execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        assert!(
            events.iter().any(
                |event| matches!(event, Ok(CanonicalEvent::Completed(response))
            if response.error.is_none() && response.output_texts().eq(["redirected"]))
            ),
            "{events:?}"
        );
        exhausted(execute(&fixture.gateway, &fixture.key, "rpm-a").await);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn original_target_gets_five_seconds_before_an_available_backup() {
        let fixture = fixture(false).await;
        let mut preferred = target(&fixture.providers[0], None);
        preferred.priority = Some(10);
        set_thinking_targets(
            &fixture.gateway,
            "rpm-a",
            vec![preferred, target(&fixture.providers[1], None)],
        )
        .await;
        configure(
            &fixture,
            RpmConfig {
                destinations: vec![destination(&fixture.providers[0], 1)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        let root = crate::rpm::RootRequest::default();
        spawn_root(
            &fixture,
            root.clone(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        )
        .await
        .unwrap()
        .unwrap();
        let waiting = spawn_root(
            &fixture,
            root.clone(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        tokio::time::advance(Duration::from_millis(4_999)).await;
        assert!(!waiting.is_finished());
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
        assert_eq!(fixture.calls[1].load(Ordering::SeqCst), 0);
        // Tokio 的 timer wheel 向上取整到毫秒；精确离窗边界另以直接请求验证。
        tokio::time::advance(Duration::from_millis(2)).await;
        waiting.await.unwrap().unwrap();
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
        assert_eq!(fixture.calls[1].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn preferred_and_general_waits_share_thirty_seconds_across_hidden_turns() {
        let fixture = fixture(true).await;
        configure(
            &fixture,
            RpmConfig {
                destinations: vec![destination(&fixture.providers[0], 1)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        let root = crate::rpm::RootRequest::default();
        spawn_root(
            &fixture,
            root.clone(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        )
        .await
        .unwrap()
        .unwrap();
        let waiting = spawn_root(
            &fixture,
            root.clone(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        tokio::time::advance(Duration::from_millis(5_001)).await;
        fixture.gateway.rpm_admission.wait_started.notified().await;
        tokio::time::advance(Duration::from_millis(24_998)).await;
        assert!(!waiting.is_finished());
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(2)).await;
        exhausted(waiting.await.unwrap());
        let before = tokio::time::Instant::now();
        exhausted(
            spawn_root(
                &fixture,
                root,
                CancellationToken::new(),
                stravia_runtime_contract::Deadline::never(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(tokio::time::Instant::now(), before);
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn parallel_hidden_waits_share_wall_time_and_one_queue_position() {
        let fixture = fixture(true).await;
        configure(
            &fixture,
            RpmConfig {
                preferred_wait_ms: 0,
                queue_capacity: 1,
                destinations: vec![destination(&fixture.providers[0], 1)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        let root = crate::rpm::RootRequest::default();
        let cancellation = CancellationToken::new();
        let first = spawn_root(
            &fixture,
            root.clone(),
            cancellation.clone(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        let second = spawn_root(
            &fixture,
            root.clone(),
            cancellation.clone(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        cancellation.cancel();
        for result in [first.await.unwrap(), second.await.unwrap()] {
            assert!(matches!(result, Err(ModelTurnError { code, .. }) if code == "cancelled"));
        }
        let remaining = spawn_root(
            &fixture,
            root,
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        tokio::time::advance(Duration::from_millis(19_999)).await;
        assert!(!remaining.is_finished());
        tokio::time::advance(Duration::from_millis(2)).await;
        exhausted(remaining.await.unwrap());
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn default_queue_allows_128_waiters_in_addition_to_an_active_stream() {
        let mut fixture = fixture(false).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        let finish = release.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            drain_test_http_request(&mut socket).await;
            let first = serde_json::json!({"id":"queue-stream","model":"upstream-model",
                "choices":[{"index":0,"delta":{"role":"assistant","content":"active"},
                "finish_reason":null}]})
            .to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {first}\n\n").as_bytes()).await.unwrap();
            finish.notified().await;
            socket.write_all(b"data: [DONE]\n\n").await.unwrap();
        });
        fixture.providers[0] =
            add_captured_thinking_provider(&fixture.gateway, format!("http://{address}/v1")).await;
        set_thinking_targets(
            &fixture.gateway,
            "rpm-a",
            vec![target(&fixture.providers[0], None)],
        )
        .await;
        configure(
            &fixture,
            RpmConfig {
                destinations: vec![destination(&fixture.providers[0], 1)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        let mut request = AiRequest::new("rpm-a", Vec::new());
        request.stream.enabled = true;
        let mut active = fixture
            .gateway
            .model_turn
            .execute(
                TurnInput::new(Principal::new(fixture.key.clone()), request).with_execution(
                    CancellationToken::new(),
                    stravia_runtime_contract::Deadline::never(),
                ),
            )
            .await
            .unwrap();
        loop {
            if matches!(active.output.next().await.unwrap().unwrap(),
                CanonicalEvent::Delta(AiStreamDelta::TextDelta(text)) if text == "active")
            {
                break;
            }
        }
        let completion = tokio::spawn(async move { active.output.collect::<Vec<_>>().await });
        let cancellation = CancellationToken::new();
        let mut waiters = Vec::with_capacity(128);
        for _ in 0..128 {
            waiters.push(spawn_root(
                &fixture,
                Default::default(),
                cancellation.clone(),
                stravia_runtime_contract::Deadline::never(),
            ));
            fixture.gateway.rpm_admission.wait_started.notified().await;
        }
        assert!(!completion.is_finished());
        let overflow = spawn_root(
            &fixture,
            Default::default(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        )
        .await
        .unwrap();
        assert!(
            matches!(overflow, Err(ModelTurnError { code, .. }) if code == "target_rpm_queue_full")
        );
        cancellation.cancel();
        for waiter in waiters {
            assert!(
                matches!(waiter.await.unwrap(), Err(ModelTurnError { code, .. }) if code == "cancelled")
            );
        }
        release.notify_one();
        assert!(
            completion
                .await
                .unwrap()
                .into_iter()
                .all(|event| event.is_ok())
        );
        let cancellation = CancellationToken::new();
        let reused = spawn_root(
            &fixture,
            Default::default(),
            cancellation.clone(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        cancellation.cancel();
        assert!(
            matches!(reused.await.unwrap(), Err(ModelTurnError { code, .. }) if code == "cancelled")
        );
    }

    #[tokio::test]
    async fn queue_capacity_cancel_and_deadline_release_waiters_without_late_sends() {
        let fixture = fixture(true).await;
        configure(
            &fixture,
            RpmConfig {
                queue_capacity: 1,
                destinations: vec![destination(&fixture.providers[0], 1)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let first = spawn_root(
            &fixture,
            Default::default(),
            cancellation.clone(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        let rejected = execute(&fixture.gateway, &fixture.key, "rpm-b").await;
        assert!(
            matches!(rejected, Err(ModelTurnError { code, .. }) if code == "target_rpm_queue_full")
        );
        cancellation.cancel();
        let cancelled = first.await.unwrap();
        assert!(matches!(cancelled, Err(ModelTurnError { code, .. }) if code == "cancelled"));
        let deadline = stravia_runtime_contract::Deadline::never();
        let second = spawn_root(
            &fixture,
            Default::default(),
            CancellationToken::new(),
            deadline.clone(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        deadline.reset(std::time::Instant::now() - Duration::from_secs(1));
        let expired = second.await.unwrap();
        assert!(matches!(expired, Err(ModelTurnError { code, .. }) if code == "deadline_exceeded"));
        // 两种终态都回收队列名额；预算耗尽后不迟到发送，离窗后新请求才能获准。
        let third = spawn_root(
            &fixture,
            Default::default(),
            CancellationToken::new(),
            stravia_runtime_contract::Deadline::never(),
        );
        fixture.gateway.rpm_admission.wait_started.notified().await;
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(30_001)).await;
        exhausted(third.await.unwrap());
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(29_999)).await;
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn destination_window_is_shared_across_routes_and_rolls_at_exact_sixty_seconds() {
        let fixture = fixture(true).await;
        configure(
            &fixture,
            RpmConfig {
                preferred_wait_ms: 0,
                total_wait_ms: 0,
                destinations: vec![destination(&fixture.providers[0], 2)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        execute(&fixture.gateway, &fixture.key, "rpm-b")
            .await
            .unwrap();
        tokio::time::advance(Duration::from_millis(39_999)).await;
        exhausted(execute(&fixture.gateway, &fixture.key, "rpm-a").await);
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_millis(1)).await;
        execute(&fixture.gateway, &fixture.key, "rpm-b")
            .await
            .unwrap();
        tokio::time::advance(Duration::from_millis(19_999)).await;
        exhausted(execute(&fixture.gateway, &fixture.key, "rpm-a").await);
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 3);
        tokio::time::advance(Duration::from_millis(1)).await;
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 4);
        assert_eq!(fixture.calls[1].load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn simultaneous_sends_share_one_atomic_destination_allowance() {
        let fixture = fixture(true).await;
        configure(
            &fixture,
            RpmConfig {
                preferred_wait_ms: 0,
                total_wait_ms: 0,
                destinations: vec![destination(&fixture.providers[0], 3)],
                ..Default::default()
            },
        )
        .await;
        let _clock = Clock::pause();
        let requests = (0..12).map(|index| {
            execute(
                &fixture.gateway,
                &fixture.key,
                if index % 2 == 0 { "rpm-a" } else { "rpm-b" },
            )
        });
        let results = futures::future::join_all(requests).await;
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 3);
        for result in results.into_iter().filter(Result::is_err) {
            exhausted(result);
        }
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn explicit_pool_shares_cross_provider_capacity_without_debiting_default_pools() {
        let fixture = fixture(false).await;
        configure(
            &fixture,
            RpmConfig {
                preferred_wait_ms: 0,
                total_wait_ms: 0,
                destinations: fixture
                    .providers
                    .iter()
                    .map(|id| destination(id, 1))
                    .collect(),
                pools: vec![RpmPool {
                    id: "shared".into(),
                    name: "Shared account".into(),
                    rpm_limit: Some(2),
                }],
                ..Default::default()
            },
        )
        .await;
        for (model, provider) in ["rpm-a", "rpm-b"].into_iter().zip(&fixture.providers) {
            set_thinking_targets(
                &fixture.gateway,
                model,
                vec![target(provider, Some("shared"))],
            )
            .await;
        }
        let _clock = Clock::pause();
        execute(&fixture.gateway, &fixture.key, "rpm-a")
            .await
            .unwrap();
        execute(&fixture.gateway, &fixture.key, "rpm-b")
            .await
            .unwrap();
        exhausted(execute(&fixture.gateway, &fixture.key, "rpm-a").await);
        for (model, provider) in ["rpm-a", "rpm-b"].into_iter().zip(&fixture.providers) {
            set_thinking_targets(&fixture.gateway, model, vec![target(provider, None)]).await;
            execute(&fixture.gateway, &fixture.key, model)
                .await
                .unwrap();
            exhausted(execute(&fixture.gateway, &fixture.key, model).await);
        }
        assert_eq!(fixture.calls[0].load(Ordering::SeqCst), 2);
        assert_eq!(fixture.calls[1].load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn execute_tools_without_model_capability_declarations() {
    let (base_url, calls) = serve_openai_response(serde_json::json!({
        "id": "chatcmpl-tool",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call-lookup",
                "type": "function",
                "function": {"name": "lookup", "arguments": "{\"query\":\"local\"}"}
            }]},
            "finish_reason": "tool_calls"
        }]
    }))
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("No tools".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    add_test_provider_model(&gateway, &provider.id).await;
    let model = admin
        .create_model(CreateRoute {
            model_id: "no-tools-model".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                rpm_pool_id: None,
                provider_id: provider.id,
                model: Some("upstream-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "No tools key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let mut request = AiRequest::new("no-tools-model", Vec::new());
    request.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "lookup".into(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);

    let turn = gateway
        .model_turn
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("protocol-supported tools must reach the provider");
    let events = turn.output.collect::<Vec<_>>().await;
    let Some(Ok(CanonicalEvent::Completed(response))) = events.last() else {
        panic!("expected completed tool response: {events:?}");
    };
    let tool = response
        .tool_calls()
        .next()
        .expect("returned function call");
    assert_eq!(tool.id, "call-lookup");
    assert_eq!(tool.name, "lookup");
    assert_eq!(tool.arguments, "{\"query\":\"local\"}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn execute_does_not_fail_over_after_the_first_canonical_delta() {
    let (partial_url, partial_calls) = serve_incomplete_openai_stream(6).await;
    let (fallback_url, fallback_calls) = serve_openai_response(serde_json::json!({
        "id": "chatcmpl-fallback",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "must not run"},
            "finish_reason": "stop"
        }]
    }))
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let mut providers = Vec::new();
    for (name, base_url) in [("partial", partial_url), ("fallback", fallback_url)] {
        providers.push(
            admin
                .create_provider(CreateProvider {
                    name: Some(name.into()),
                    source: ProviderSourceInput::Custom {
                        vendor: "custom".into(),
                        channel: "default".into(),
                        protocol: Some("openai-compatible".into()),
                        base_url,
                        models_source: None,
                        static_models: None,
                    },
                    credential: ProviderCredentialInput::ApiKey {
                        value: "test-provider-key".into(),
                    },
                    vendor_options: Default::default(),
                    use_proxy: false,
                })
                .await
                .expect("Provider"),
        );
    }
    for provider in &providers {
        add_test_provider_model(&gateway, &provider.id).await;
    }
    let model = admin
        .create_model(CreateRoute {
            model_id: "stream-lock-model".into(),
            display_name: None,
            balance: Some("traffic_equalization".into()),
            targets: providers
                .iter()
                .enumerate()
                .map(|(index, provider)| CreateTarget {
                    enabled: true,
                    provider_id: provider.id.clone(),
                    model: Some("upstream-model".into()),
                    priority: Some((providers.len() - index) as i32),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(5),
                    target_cooldown_ms: None,
                    rpm_pool_id: None,
                    thinking_level_map: Vec::new(),
                })
                .collect(),
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Stream lock key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let mut request = AiRequest::new("stream-lock-model", Vec::new());
    request.stream.enabled = true;

    for failure_count in 1..=6 {
        let turn = gateway
            .model_turn
            .execute(TurnInput::new(
                Principal::new(key.id.clone()),
                request.clone(),
            ))
            .await
            .expect("streaming Model Turn locks the first Target");
        assert_eq!(turn.route.provider_id, providers[0].id);
        let mut output = turn.output;
        let events = output.by_ref().collect::<Vec<_>>().await;

        assert!(events.iter().any(
            |event| matches!(event, Ok(CanonicalEvent::Delta(AiStreamDelta::TextDelta(text))) if text == "partial")
        ));
        assert!(matches!(
            events.last(),
            Some(Err(ModelTurnError { code, .. })) if code == "upstream_stream_error"
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(CanonicalEvent::Completed(_))))
        );
        for _ in 0..3 {
            assert!(output.next().await.is_none());
        }
        assert_eq!(partial_calls.load(Ordering::SeqCst), failure_count);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }
    let recovered = gateway
        .model_turn
        .execute(TurnInput::new(
            Principal::new(key.id),
            AiRequest::new("stream-lock-model", Vec::new()),
        ))
        .await
        .expect("sixth failure makes the next request choose fallback");
    assert_eq!(recovered.route.provider_id, providers[1].id);
    let events = recovered.output.collect::<Vec<_>>().await;
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(_)))
    ));
    assert_eq!(partial_calls.load(Ordering::SeqCst), 6);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn execute_omits_previous_response_id_when_lookup_misses() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_model("lookup-miss-model", true).await;
    let executor = LiveModelTurnExecutor::new(
        gateway.clone(),
        crate::router::continuation::ScriptedContinuation::miss(),
    );
    let mut request = AiRequest::new("lookup-miss-model", Vec::new());
    crate::router::stamp_previous_response_id(&mut request, "stravia-parent");

    let turn = executor
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("missed continuation still executes");
    let _ = turn.output.collect::<Vec<_>>().await;
    let (_, body) = captured_http(&captured.lock());

    assert!(body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn execute_sends_previous_response_id_when_lookup_hits() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_model("lookup-hit-model", true).await;
    let executor = LiveModelTurnExecutor::new(
        gateway.clone(),
        crate::router::continuation::ScriptedContinuation::hit("upstream-resp-1"),
    );
    let mut request = AiRequest::new("lookup-hit-model", Vec::new());
    crate::router::stamp_previous_response_id(&mut request, "stravia-parent");

    let turn = executor
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("hit continuation executes");
    let _ = turn.output.collect::<Vec<_>>().await;
    let (_, body) = captured_http(&captured.lock());

    assert_eq!(
        body.get("previous_response_id")
            .and_then(|value| value.as_str()),
        Some("upstream-resp-1")
    );
}

#[tokio::test]
async fn execute_forwards_extra_headers_without_overriding_authorization() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_model("header-model", true).await;
    let mut extra_headers = HeaderMap::new();
    extra_headers.insert("openai-beta", HeaderValue::from_static("responses=v1"));
    extra_headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer attacker-key"),
    );

    let turn = gateway
        .model_turn
        .execute(
            TurnInput::new(
                Principal::new(key.id),
                AiRequest::new("header-model", Vec::new()),
            )
            .with_extra_headers(extra_headers),
        )
        .await
        .expect("extra-header Model Turn");
    let _ = turn.output.collect::<Vec<_>>().await;
    let (head, _) = captured_http(&captured.lock());
    let head = head.to_ascii_lowercase();

    assert!(head.contains("openai-beta: responses=v1"));
    assert!(head.contains("authorization: bearer test-provider-key"));
    assert!(!head.contains("attacker-key"));
}

fn restricted_thinking_map(levels: &[ThinkingLevel]) -> Vec<crate::thinking::ThinkingLevelMapping> {
    ThinkingLevel::ALL
        .into_iter()
        .map(|level| crate::thinking::ThinkingLevelMapping {
            level,
            control: if levels.contains(&level) {
                stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                    value: level.as_str().into(),
                }
            } else {
                stravia_runtime_contract::thinking::TargetThinkingControl::Hidden
            },
            source: crate::thinking::ThinkingMappingSource::Overridden,
        })
        .collect()
}

fn thinking_target(provider_id: &str, levels: &[ThinkingLevel], priority: i32) -> CreateTarget {
    CreateTarget {
        provider_id: provider_id.into(),
        model: Some("upstream-model".into()),
        enabled: true,
        priority: Some(priority),
        first_token_timeout_ms: None,
        target_retry_budget: Some(0),
        target_cooldown_ms: None,
        rpm_pool_id: None,
        thinking_level_map: restricted_thinking_map(levels),
    }
}

async fn set_thinking_targets(gateway: &Gateway, model: &str, targets: Vec<CreateTarget>) {
    gateway
        .admin()
        .update_model(
            model,
            crate::db::models::UpdateRoute {
                targets: Some(targets),
                ..Default::default()
            },
        )
        .await
        .expect("update thinking Targets");
}

async fn add_captured_thinking_provider(gateway: &Gateway, base_url: String) -> String {
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some(format!("Second capture {base_url}")),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .expect("second Provider");
    add_test_provider_model(gateway, &provider.id).await;
    provider.id
}

async fn captured_reasoning_effort(
    gateway: &crate::Gateway,
    captured: &Arc<Mutex<Vec<u8>>>,
    key_id: &str,
    model: &str,
    request: AiRequest,
) -> serde_json::Value {
    let turn = gateway
        .model_turn
        .execute(TurnInput::new(Principal::new(key_id.to_owned()), request))
        .await
        .unwrap_or_else(|error| panic!("Model Turn for {model}: {error}"));
    let _ = turn.output.collect::<Vec<_>>().await;
    let (_, body) = captured_http(&captured.lock());
    body
}

#[tokio::test]
async fn route_default_thinking_level_applies_when_reasoning_is_unspecified() {
    let (_data_dir, gateway, captured, key) = gateway_with_captured_thinking(
        "default-thinking-model",
        true,
        "ok",
        Some(ThinkingLevel::High),
    )
    .await;

    let body = captured_reasoning_effort(
        &gateway,
        &captured,
        &key.id,
        "default-thinking-model",
        AiRequest::new("default-thinking-model", Vec::new()),
    )
    .await;

    assert_eq!(
        body.get("reasoning_effort")
            .and_then(|value| value.as_str()),
        Some("high")
    );
}

#[tokio::test]
async fn route_default_thinking_level_yields_to_explicit_client_level() {
    let (_data_dir, gateway, captured, key) = gateway_with_captured_thinking(
        "explicit-thinking-model",
        true,
        "ok",
        Some(ThinkingLevel::High),
    )
    .await;
    let mut request = AiRequest::new("explicit-thinking-model", Vec::new());
    request.reasoning.level = Some(ThinkingLevel::Low);

    let body = captured_reasoning_effort(
        &gateway,
        &captured,
        &key.id,
        "explicit-thinking-model",
        request,
    )
    .await;

    assert_eq!(
        body.get("reasoning_effort")
            .and_then(|value| value.as_str()),
        Some("low")
    );
}

#[tokio::test]
async fn route_default_thinking_level_yields_to_other_reasoning_directives() {
    let (_data_dir, gateway, captured, key) = gateway_with_captured_thinking(
        "directive-thinking-model",
        true,
        "ok",
        Some(ThinkingLevel::High),
    )
    .await;
    let mut request = AiRequest::new("directive-thinking-model", Vec::new());
    request.reasoning.enabled = true;
    request.reasoning.effort =
        Some(stravia_runtime_contract::protocol::ir::request::ReasoningEffort::High);

    let body = captured_reasoning_effort(
        &gateway,
        &captured,
        &key.id,
        "directive-thinking-model",
        request,
    )
    .await;

    assert!(
        body.get("reasoning_effort").is_none(),
        "a client-side reasoning directive without level must not trigger the Route default"
    );
}

#[tokio::test]
async fn route_default_thinking_level_clamps_to_the_nearest_supported_level() {
    let (_data_dir, gateway, captured, key) = gateway_with_captured_thinking(
        "clamped-thinking-model",
        true,
        "ok",
        Some(ThinkingLevel::Xhigh),
    )
    .await;

    let body = captured_reasoning_effort(
        &gateway,
        &captured,
        &key.id,
        "clamped-thinking-model",
        AiRequest::new("clamped-thinking-model", Vec::new()),
    )
    .await;

    assert_eq!(
        body.get("reasoning_effort")
            .and_then(|value| value.as_str()),
        Some("high")
    );
}

#[tokio::test]
async fn target_thinking_uses_selected_mapping_instead_of_route_intersection() {
    // Distinct cases: no shared levels, a shared level below the request, and a
    // Hidden first Target that must reroute with the original requested level.
    for (first_levels, second_levels, requested, first_selected, expected) in [
        (
            vec![ThinkingLevel::Low],
            vec![ThinkingLevel::High],
            ThinkingLevel::Low,
            true,
            "low",
        ),
        (
            vec![ThinkingLevel::Low, ThinkingLevel::High],
            vec![ThinkingLevel::Low],
            ThinkingLevel::High,
            true,
            "high",
        ),
        (
            Vec::new(),
            vec![ThinkingLevel::Low, ThinkingLevel::High],
            ThinkingLevel::Medium,
            false,
            "high",
        ),
    ] {
        let (_data_dir, gateway, first_capture, key) =
            gateway_with_captured_thinking("target-thinking-model", true, "ok", None).await;
        let first_provider = gateway
            .admin()
            .get_model("target-thinking-model")
            .await
            .expect("Route")
            .targets[0]
            .provider_id()
            .to_string();
        let (second_url, second_capture) = serve_openai_capture_text("ok").await;
        let second_provider = add_captured_thinking_provider(&gateway, second_url).await;
        set_thinking_targets(
            &gateway,
            "target-thinking-model",
            vec![
                thinking_target(&first_provider, &first_levels, 20),
                thinking_target(&second_provider, &second_levels, 10),
            ],
        )
        .await;
        let mut request = AiRequest::new("target-thinking-model", Vec::new());
        request.reasoning.level = Some(requested);
        let selected_capture = if first_selected {
            &first_capture
        } else {
            &second_capture
        };
        let body = captured_reasoning_effort(
            &gateway,
            selected_capture,
            &key.id,
            "target-thinking-model",
            request,
        )
        .await;
        assert_eq!(body["reasoning_effort"], expected, "request {requested:?}");
        let unselected_capture = if first_selected {
            &second_capture
        } else {
            &first_capture
        };
        assert!(
            unselected_capture.lock().is_empty(),
            "unselected Target must not receive an upstream request"
        );
    }
}

#[tokio::test]
async fn target_thinking_prefers_upward_then_falls_back_downward() {
    for (requested, expected) in [
        (ThinkingLevel::Medium, "high"),
        (ThinkingLevel::Xhigh, "high"),
    ] {
        let (_data_dir, gateway, captured, key) =
            gateway_with_captured_thinking("nearest-thinking-model", true, "ok", None).await;
        let provider = gateway
            .admin()
            .get_model("nearest-thinking-model")
            .await
            .expect("Route")
            .targets[0]
            .provider_id()
            .to_string();
        set_thinking_targets(
            &gateway,
            "nearest-thinking-model",
            vec![thinking_target(
                &provider,
                &[ThinkingLevel::Low, ThinkingLevel::High],
                10,
            )],
        )
        .await;
        let mut request = AiRequest::new("nearest-thinking-model", Vec::new());
        request.reasoning.level = Some(requested);
        let body = captured_reasoning_effort(
            &gateway,
            &captured,
            &key.id,
            "nearest-thinking-model",
            request,
        )
        .await;
        assert_eq!(body["reasoning_effort"], expected, "request {requested:?}");
    }
}

#[tokio::test]
async fn target_thinking_failover_restarts_from_original_level() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_thinking("failover-thinking-model", true, "ok", None).await;
    let success_provider = gateway
        .admin()
        .get_model("failover-thinking-model")
        .await
        .expect("Route")
        .targets[0]
        .provider_id()
        .to_string();
    let (failure_url, failure_calls) = serve_openai_status(
        429,
        serde_json::json!({
            "error": {"message": "quota exhausted", "type": "insufficient_quota"}
        }),
    )
    .await;
    let failure_provider = add_captured_thinking_provider(&gateway, failure_url).await;
    set_thinking_targets(
        &gateway,
        "failover-thinking-model",
        vec![
            thinking_target(&failure_provider, &[ThinkingLevel::Low], 20),
            thinking_target(
                &success_provider,
                &[ThinkingLevel::Low, ThinkingLevel::High],
                10,
            ),
        ],
    )
    .await;
    let mut request = AiRequest::new("failover-thinking-model", Vec::new());
    request.reasoning.level = Some(ThinkingLevel::High);
    let body = captured_reasoning_effort(
        &gateway,
        &captured,
        &key.id,
        "failover-thinking-model",
        request,
    )
    .await;
    assert_eq!(failure_calls.load(Ordering::SeqCst), 1);
    assert_eq!(body["reasoning_effort"], "high");
}

#[tokio::test]
async fn target_thinking_explicit_level_rejects_all_hidden_targets_without_upstream_call() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_thinking("hidden-thinking-model", true, "ok", None).await;
    let provider = gateway
        .admin()
        .get_model("hidden-thinking-model")
        .await
        .expect("Route")
        .targets[0]
        .provider_id()
        .to_string();
    let (second_url, second_capture) = serve_openai_capture_text("ok").await;
    let second_provider = add_captured_thinking_provider(&gateway, second_url).await;
    set_thinking_targets(
        &gateway,
        "hidden-thinking-model",
        vec![
            thinking_target(&provider, &[], 20),
            thinking_target(&second_provider, &[], 10),
        ],
    )
    .await;
    let mut request = AiRequest::new("hidden-thinking-model", Vec::new());
    request.reasoning.level = Some(ThinkingLevel::High);
    let result = gateway
        .model_turn
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await;
    let error = result
        .err()
        .expect("an explicit level must not be silently dropped");
    assert_eq!(error.code, "thinking_level_unsupported");
    assert!(captured.lock().is_empty(), "no upstream call is permitted");
    assert!(
        second_capture.lock().is_empty(),
        "no upstream call is permitted"
    );
}

#[tokio::test]
async fn route_default_thinking_level_is_dropped_when_no_level_is_supported() {
    let (_data_dir, gateway, captured, key) = gateway_with_captured_thinking(
        "empty-support-model",
        true,
        "ok",
        Some(ThinkingLevel::High),
    )
    .await;
    let route = gateway
        .admin()
        .get_model("empty-support-model")
        .await
        .expect("Route");
    let target = route.targets.first().expect("Route Target").clone();
    let hidden_map = ThinkingLevel::ALL
        .into_iter()
        .map(|level| crate::thinking::ThinkingLevelMapping {
            level,
            control: stravia_runtime_contract::thinking::TargetThinkingControl::Hidden,
            source: crate::thinking::ThinkingMappingSource::Overridden,
        })
        .collect();
    gateway
        .admin()
        .update_model(
            "empty-support-model",
            crate::db::models::UpdateRoute {
                targets: Some(vec![crate::db::models::CreateTarget {
                    rpm_pool_id: None,
                    provider_id: target.provider_id().clone().into(),
                    model: target.model().cloned().map(Into::into),
                    enabled: true,
                    priority: None,
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: hidden_map,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("hide every Thinking Level");

    let body = captured_reasoning_effort(
        &gateway,
        &captured,
        &key.id,
        "empty-support-model",
        AiRequest::new("empty-support-model", Vec::new()),
    )
    .await;

    assert!(
        body.get("reasoning_effort").is_none(),
        "an empty Supported Thinking Level set must degrade the Route default to unspecified"
    );
}

#[tokio::test]
async fn execute_capability_grant_does_not_require_route_binding() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_model("grant-model", false).await;
    let bound = gateway
        .model_turn
        .execute(TurnInput::new(
            Principal::new(key.id.clone()),
            AiRequest::new("grant-model", Vec::new()),
        ))
        .await;
    assert!(
        bound.is_err(),
        "RouteBinding must fail without a bound Model"
    );

    let turn = gateway
        .model_turn
        .execute(
            TurnInput::new(
                Principal::new(key.id),
                AiRequest::new("grant-model", Vec::new()),
            )
            .with_authorization(ModelTurnAuthorization::CapabilityGrant),
        )
        .await
        .expect("CapabilityGrant Model Turn");
    let _ = turn.output.collect::<Vec<_>>().await;

    assert!(!captured.lock().is_empty());
}

// The real MappingStore seam can hold intern or publication acknowledgements
// after the database commits. Cancellation is not evidence that commit failed.
struct HeldPublicationStore {
    inner: Arc<dyn stravia_credential_protection::store::MappingStore>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail: bool,
    expire_intern: bool,
    held_intern: Option<Arc<HeldInternAcknowledgement>>,
    starts: AtomicUsize,
    cancel_on_release: Mutex<Option<CancellationToken>>,
}

#[derive(Default)]
struct HeldInternAcknowledgement {
    committed: tokio::sync::Notify,
    release: tokio::sync::Notify,
    acknowledged: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl stravia_credential_protection::store::MappingStore for HeldPublicationStore {
    async fn active(
        &self,
        principal: &Principal,
    ) -> Result<
        Vec<stravia_credential_protection::store::Mapping>,
        stravia_runtime_contract::redaction::RedactionError,
    > {
        self.inner.active(principal).await
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[String],
    ) -> Result<
        stravia_credential_protection::store::InternedMappings,
        stravia_runtime_contract::redaction::RedactionError,
    > {
        let mut result = self.inner.intern(principal, secrets).await?;
        if let Some(held) = &self.held_intern {
            held.committed.notify_one();
            held.release.notified().await;
            held.acknowledged.notify_one();
        }
        if self.expire_intern {
            for mapping in &mut result.mappings {
                mapping.expires_at = 0;
            }
        }
        Ok(result)
    }

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), stravia_runtime_contract::redaction::RedactionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if !self.fail {
            self.inner.publish(principal, references, retention).await?;
        }
        self.entered.notify_one();
        self.release.notified().await;
        if let Some(cancellation) = self.cancel_on_release.lock().take() {
            cancellation.cancel();
        }
        if self.fail {
            Err(stravia_runtime_contract::redaction::RedactionError::Storage)
        } else {
            Ok(())
        }
    }

    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), stravia_runtime_contract::redaction::RedactionError> {
        self.inner
            .extend_retention(principal, references, retention)
            .await
    }

    async fn cleanup_expired(
        &self,
    ) -> Result<u64, stravia_runtime_contract::redaction::RedactionError> {
        self.inner.cleanup_expired().await
    }
}

fn credential_observer(
    gateway: &Gateway,
    principal: &Principal,
) -> crate::interaction_observation::RunObserver {
    let id = stravia_runtime_contract::identifier::new_id();
    gateway
        .observation
        .observe_ingress(crate::interaction_observation::IngressStart {
            id: id.clone(),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            protocol: "openai-compatible".into(),
        })
        .admit(
            crate::interaction_observation::RunStart {
                id,
                principal: principal.continuation_key(),
                api_key_id: None,
                api_key_name: Some("Discovery test".into()),
                route_id: "discovery-model".into(),
                model_display_name: None,
                ingress_protocol: "openai-compatible".into(),
            },
            crate::interaction_observation::AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "model",
                    Vec::new(),
                ),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        )
}

#[tokio::test]
async fn committed_discovery_survives_dropped_protection_before_intern_acknowledgement() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await
    .unwrap();
    gateway
        .storage
        .settings()
        .set("reversible_redaction_enabled", "true")
        .await
        .unwrap();
    let principal = Principal::new("cancelled-discovery-owner");
    let held = Arc::new(HeldInternAcknowledgement::default());
    let store = Arc::new(HeldPublicationStore {
        inner: gateway.redaction.mappings.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        fail: false,
        expire_intern: false,
        held_intern: Some(held.clone()),
        starts: AtomicUsize::new(0),
        cancel_on_release: Mutex::new(None),
    });
    gateway.redaction = crate::reversible_redaction::ReversibleRedaction::new(
        gateway.storage.clone(),
        store.clone(),
        gateway.redaction.custom_rules.clone(),
    );
    let observer = credential_observer(&gateway, &principal);
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    let protection = {
        let redaction = gateway.redaction.clone();
        let principal = principal.clone();
        let observer = observer.clone();
        let mut request = request.clone();
        tokio::spawn(async move {
            redaction
                .protect(&principal, &mut request, Some(&observer))
                .await
        })
    };
    held.committed.notified().await;
    assert_eq!(store.inner.active(&principal).await.unwrap().len(), 1);
    protection.abort();
    assert!(matches!(protection.await, Err(error) if error.is_cancelled()));
    observer.finish(crate::interaction_observation::RunOutcome {
        delivery: None,
        client_output_committed: false,
        delivery_completed_at: None,
        status: "cancelled".into(),
        terminal_reason: Some("cancelled".into()),
        generation_node_id: None,
        generation_root_id: None,
    });
    // Terminal delivery must not wait for the held mapping acknowledgement.
    let before_ack = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert!(before_ack.items.is_empty());
    let forest = gateway
        .observation
        .query_forest(Default::default())
        .await
        .unwrap();
    let interaction = &forest.roots[0].interactions[0];
    let detail = gateway
        .observation
        .get_interaction(&interaction.id, Default::default())
        .await
        .unwrap()
        .unwrap();
    let terminal = detail.runs[0]
        .events
        .iter()
        .find(|event| event.kind == "run_finished")
        .expect("cancellation is observable before the mapping acknowledgement");
    assert_eq!(terminal.payload["status"], "cancelled");
    held.release.notify_one();
    // This current-thread test cannot resume between acknowledgement and the
    // synchronous event emission. On the old implementation the cancelled
    // intern never acknowledges; the bounded wait then exposes the missing row.
    let _ = tokio::time::timeout(Duration::from_secs(1), held.acknowledged.notified()).await;
    let page = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].new_credential_count, 1);
    assert_eq!(page.items[0].status, "interrupted");
    assert_eq!(page.items[0].source_types, ["system_or_history"]);
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("Q8n4Vk7sT2p9X5a3Lc6D0h1R"));
    assert!(!encoded.contains(stravia_credential_protection::marker::PREFIX));
    let reused_observer = credential_observer(&gateway, &principal);
    held.release.notify_one();
    gateway
        .redaction
        .protect(&principal, &mut request, Some(&reused_observer))
        .await
        .unwrap();
    let reused = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(reused.items.len(), 1);
    assert_eq!(reused.items[0].new_credential_count, 1);
    assert_eq!(reused.items[0].status, "interrupted");
    assert_eq!(store.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn committed_discovery_survives_replacement_failure_but_failed_intern_creates_none() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    gateway
        .storage
        .settings()
        .set("reversible_redaction_enabled", "true")
        .await
        .unwrap();
    let principal = Principal::new("discovery-owner");
    let store = Arc::new(HeldPublicationStore {
        inner: gateway.redaction.mappings.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        fail: false,
        expire_intern: true,
        held_intern: None,
        starts: AtomicUsize::new(0),
        cancel_on_release: Mutex::new(None),
    });
    gateway.redaction = crate::reversible_redaction::ReversibleRedaction::new(
        gateway.storage.clone(),
        store.clone(),
        gateway.redaction.custom_rules.clone(),
    );
    let observer = credential_observer(&gateway, &principal);
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    assert!(matches!(
        gateway
            .redaction
            .protect(&principal, &mut request, Some(&observer))
            .await,
        Err(stravia_runtime_contract::redaction::RedactionError::InvalidText)
    ));
    observer.finish(crate::interaction_observation::RunOutcome {
        delivery: None,
        client_output_committed: false,
        delivery_completed_at: None,
        status: "failed".into(),
        terminal_reason: Some("reversible_redaction_failed".into()),
        generation_node_id: None,
        generation_root_id: None,
    });
    drop(observer);
    let page = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].new_credential_count, 1);
    assert_eq!(page.items[0].status, "interrupted");
    assert_eq!(store.inner.active(&principal).await.unwrap().len(), 1);
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("Q8n4Vk7sT2p9X5a3Lc6D0h1R"));
    assert!(!encoded.contains(stravia_credential_protection::marker::PREFIX));

    let pool = gateway._sqlite_pool.as_ref().unwrap();
    sqlx::query("CREATE TRIGGER reject_discovery_mapping BEFORE INSERT ON reversible_redaction_mappings BEGIN SELECT RAISE(FAIL, 'injected mapping failure'); END").execute(pool).await.unwrap();
    let other = Principal::new("discovery-other");
    let observer = credential_observer(&gateway, &other);
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    assert!(matches!(
        gateway
            .redaction
            .protect(&other, &mut request, Some(&observer))
            .await,
        Err(stravia_runtime_contract::redaction::RedactionError::Storage)
    ));
    drop(observer);
    assert!(store.inner.active(&other).await.unwrap().is_empty());
    assert_eq!(
        gateway
            .observation
            .credential_discoveries(Default::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn discovery_event_write_failure_does_not_change_protection_and_reports_gap() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    gateway
        .storage
        .settings()
        .set("reversible_redaction_enabled", "true")
        .await
        .unwrap();
    let principal = Principal::new("discovery-gap-owner");
    let observer = credential_observer(&gateway, &principal);
    gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_discovery_event BEFORE INSERT ON observation_events WHEN NEW.kind = 'credential_mappings_created' BEGIN SELECT RAISE(FAIL, 'injected observation failure'); END").execute(gateway._sqlite_pool.as_ref().unwrap()).await.unwrap();
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    let mappings = gateway
        .redaction
        .protect(&principal, &mut request, Some(&observer))
        .await
        .unwrap();
    assert_eq!(mappings.len(), 1);
    assert!(
        !request
            .instructions
            .as_deref()
            .unwrap()
            .contains("Q8n4Vk7sT2p9X5a3Lc6D0h1R")
    );
    observer.finish(crate::interaction_observation::RunOutcome {
        delivery: None,
        client_output_committed: false,
        delivery_completed_at: None,
        status: "completed".into(),
        terminal_reason: None,
        generation_node_id: None,
        generation_root_id: None,
    });
    drop(observer);
    let page = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert!(page.items.is_empty());
    assert!(page.observation_gap);
}

async fn held_publication_turn(
    fail: bool,
    late_reference: bool,
) -> (
    tempfile::TempDir,
    Gateway,
    ModelTurn,
    Arc<HeldPublicationStore>,
    Principal,
    (CancellationToken, Deadline),
    i64,
) {
    let (directory, mut gateway, _, key) =
        gateway_with_captured_thinking("publication-model", true, "answer <!--sr:", None).await;
    let principal = Principal::new(key.id);
    let store = Arc::new(HeldPublicationStore {
        inner: gateway.redaction.mappings.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        fail,
        expire_intern: false,
        held_intern: None,
        starts: AtomicUsize::new(0),
        cancel_on_release: Mutex::new(None),
    });
    gateway.redaction = crate::reversible_redaction::ReversibleRedaction::new(
        gateway.storage.clone(),
        store.clone(),
        gateway.redaction.custom_rules.clone(),
    );
    let mut request = AiRequest::new("publication-model", Vec::new());
    let mut pending_expiry = 0;
    if !late_reference {
        let mapping = store
            .inner
            .intern(&principal, &["synthetic-secret".into()])
            .await
            .unwrap()
            .mappings
            .remove(0);
        gateway
            .storage
            .settings()
            .set("reversible_redaction_enabled", "true")
            .await
            .unwrap();
        request.instructions = Some(format!(
            "{}\napi_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"",
            mapping.reference
        ));
        pending_expiry = mapping.expires_at;
    }
    let mut related = request.clone();
    let cancellation = CancellationToken::new();
    let deadline = Deadline::from_now(Duration::from_secs(300));
    let executor = LiveModelTurnExecutor::new(
        gateway.clone(),
        crate::router::continuation::ScriptedContinuation::miss(),
    );
    let run_id = stravia_runtime_contract::identifier::new_id();
    let observer = gateway
        .observation
        .observe_ingress(crate::interaction_observation::IngressStart {
            id: run_id.clone(),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            protocol: "openai-compatible".into(),
        })
        .admit(
            crate::interaction_observation::RunStart {
                id: run_id,
                principal: principal.continuation_key(),
                api_key_id: None,
                api_key_name: None,
                route_id: "publication-model".into(),
                model_display_name: None,
                ingress_protocol: "openai-compatible".into(),
            },
            crate::interaction_observation::AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "model",
                    Vec::new(),
                ),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    let turn = executor
        .execute(
            TurnInput::new(principal.clone(), request)
                .with_observer(observer)
                .with_execution(cancellation.clone(), deadline.clone()),
        )
        .await
        .expect("upstream completed before local publication");
    if late_reference {
        // The returned turn captured no local mappings. A related turn now adds a
        // valid reference to its shared trace through normal request protection.
        let mapping = store
            .inner
            .intern(&principal, &["related-secret".into()])
            .await
            .unwrap()
            .mappings
            .remove(0);
        pending_expiry = mapping.expires_at;
        related.instructions = Some(mapping.reference);
        gateway
            .redaction
            .protect(&principal, &mut related, None)
            .await
            .unwrap();
    }
    (
        directory,
        gateway,
        turn,
        store,
        principal,
        (cancellation, deadline),
        pending_expiry,
    )
}

async fn consume_until_publication(turn: &mut ModelTurn, store: &HeldPublicationStore) -> String {
    let mut text = String::new();
    loop {
        tokio::select! {
            biased;
            _ = store.entered.notified() => return text,
            event = turn.output.next() => match event.expect("publication remains pending").expect("delta") {
                CanonicalEvent::Delta(AiStreamDelta::TextDelta(delta))
                | CanonicalEvent::Delta(AiStreamDelta::TextDeltaWithMetadata { text: delta, .. }) => text.push_str(&delta),
                CanonicalEvent::Delta(_) => {},
                CanonicalEvent::Completed(_) | CanonicalEvent::Compacted(_) => panic!("success escaped pending publication"),
            }
        }
    }
}

#[tokio::test]
async fn canonical_completion_publishes_after_trailing_output_and_is_permanently_terminal() {
    let (_directory, gateway, mut turn, store, principal, (cancellation, _), pending_expiry) =
        held_publication_turn(false, false).await;
    assert_eq!(
        consume_until_publication(&mut turn, &store).await,
        "answer <!--sr:"
    );
    // The select above dropped a pending next() future. Publication must survive
    // that pause and resume rather than issuing a second write.
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
    store.release.notify_one();
    match turn.output.next().await.unwrap().unwrap() {
        CanonicalEvent::Completed(response) => {
            assert_eq!(response.output_text(), "answer <!--sr:")
        }
        CanonicalEvent::Delta(_) | CanonicalEvent::Compacted(_) => {
            panic!("expected generation completion")
        }
    }
    assert_eq!(store.starts.load(Ordering::SeqCst), 1);
    cancellation.cancel();
    for _ in 0..3 {
        assert!(turn.output.next().await.is_none());
    }
    drop(turn);
    assert_publication_observation(&gateway, "completed").await;
}

#[tokio::test]
async fn canonical_completion_publishes_current_shared_trace_with_empty_local_mappings() {
    let (_directory, _gateway, mut turn, store, principal, _, pending_expiry) =
        held_publication_turn(false, true).await;
    assert_eq!(
        consume_until_publication(&mut turn, &store).await,
        "answer <!--sr:"
    );
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
    store.release.notify_one();
    assert!(matches!(
        turn.output.next().await,
        Some(Ok(CanonicalEvent::Completed(_)))
    ));
    assert!(turn.output.next().await.is_none());
}

#[tokio::test]
async fn canonical_completion_reports_publication_failure_without_success_or_upstream_replay() {
    let (_directory, gateway, mut turn, store, _, _, _) = held_publication_turn(true, false).await;
    assert_eq!(
        consume_until_publication(&mut turn, &store).await,
        "answer <!--sr:"
    );
    store.release.notify_one();
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "reversible_redaction_failed"
    );
    for _ in 0..3 {
        assert!(turn.output.next().await.is_none());
    }
    drop(turn);
    assert_publication_observation(&gateway, "reversible_redaction_failed").await;
}

#[tokio::test]
async fn canonical_completion_cancellation_interrupts_publication_without_revoking_committed_mappings()
 {
    let (_directory, gateway, mut turn, store, principal, (cancellation, _), pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    cancellation.cancel();
    // Do not release publication: cancellation must independently wake the gate.
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "cancelled"
    );
    assert!(turn.output.next().await.is_none());
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
    drop(turn);
    assert_publication_observation(&gateway, "cancelled").await;
    let discoveries = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(discoveries.items.len(), 1);
    assert_eq!(discoveries.items[0].new_credential_count, 1);
    assert_eq!(discoveries.items[0].source_types, ["system_or_history"]);
    assert_ne!(discoveries.items[0].status, "completed");
}

#[tokio::test]
async fn canonical_completion_deadline_interrupts_publication() {
    let (_directory, gateway, mut turn, store, _, (_, deadline), _) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    // Deadline 使用 std::time::Instant；虚拟推进 Tokio 会空转至真实五分钟。
    // 发布已经进入阻塞点后，移动共享截止时间，验证独立唤醒和终态收口。
    deadline.reset(Instant::now());
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "deadline_exceeded"
    );
    assert!(turn.output.next().await.is_none());
    drop(turn);
    assert_publication_observation(&gateway, "deadline_exceeded").await;
}

async fn assert_publication_observation(gateway: &Gateway, status: &str) {
    use crate::interaction_observation::ForestQuery;

    // 查询路径不再隐式等待异步观测 writer，需要读己之写的断言必须显式请求屏障。
    gateway.observation.flush().await.unwrap();
    let forest = gateway
        .observation
        .query_forest(ForestQuery::default())
        .await
        .unwrap();
    let interaction = &forest.roots[0].interactions[0];
    let detail = gateway
        .observation
        .get_interaction(&interaction.id, ForestQuery::default())
        .await
        .unwrap()
        .unwrap();
    let run = &detail.runs[0];
    let terminals = run
        .events
        .iter()
        .filter(|event| event.kind == "model_turn_finished")
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "one owner records the Model Turn result"
    );
    assert_eq!(terminals[0].payload["status"], status);
    let attempts = run
        .events
        .iter()
        .filter(|event| event.kind == "target_attempt_finished")
        .collect::<Vec<_>>();
    assert_eq!(
        attempts.len(),
        1,
        "local publication never retries an upstream attempt"
    );
    assert_eq!(attempts[0].payload["status"], "completed");
    assert_eq!(run.usage.input_tokens, Some(1));
    assert_eq!(run.usage.output_tokens, Some(1));
}

#[tokio::test]
async fn dropping_pending_canonical_publication_records_cancelled_once() {
    let (_directory, gateway, mut turn, store, principal, _, pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    drop(turn);
    assert_publication_observation(&gateway, "cancelled").await;
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
}

#[tokio::test]
async fn cancellation_in_publications_final_poll_preempts_completed() {
    let (_directory, gateway, mut turn, store, principal, (cancellation, _), pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    *store.cancel_on_release.lock() = Some(cancellation);
    store.release.notify_one();
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "cancelled"
    );
    assert!(turn.output.next().await.is_none());
    drop(turn);
    assert_publication_observation(&gateway, "cancelled").await;
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
}

#[tokio::test]
async fn codex_native_compaction_preserves_errors_and_account_identity() {
    use axum::{
        Json, Router,
        extract::{
            State,
            ws::{Message, WebSocketUpgrade},
        },
        response::IntoResponse,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    fn compaction_error(code: &str) -> Value {
        json!({
            "code": code,
            "type": "invalid_request_error",
            "message": "native-compaction-rejected access-token-before-refresh",
            "details": {"retained": true},
        })
    }
    async fn compact(
        State(calls): State<Arc<AtomicUsize>>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        calls.fetch_add(1, Ordering::SeqCst);
        if body["instructions"] == "reject-native-compaction" {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": compaction_error("invalid_encrypted_content")})),
            )
                .into_response();
        }
        if body.get("stream").is_some() || body.get("input").and_then(Value::as_array).is_none() {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        Json(json!({"id":"compact-unary","object":"response.compaction","created_at":17,
            "output":[{"type":"message","role":"user","content":[{"type":"input_text","text":"retained"}]},
                {"type":"compaction","id":"compact-state-unary","encrypted_content":"unary-state","rolling_identity":{"version":2}}],
            "usage":{"input_tokens":9,"output_tokens":2,"total_tokens":11,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}})).into_response()
    }
    async fn responses(
        State(calls): State<Arc<AtomicUsize>>,
        upgrade: WebSocketUpgrade,
    ) -> axum::response::Response {
        upgrade.on_upgrade(|mut socket| async move {
            let state = json!({"type":"compaction","id":"inline-state","encrypted_content":"inline-cipher","rolling_identity":{"version":3}});
            while let Some(Ok(Message::Text(text))) = socket.recv().await {
                calls.fetch_add(1, Ordering::SeqCst);
                let request: Value = serde_json::from_str(&text).expect("native request");
                if request["instructions"] == "reject-native-compaction" {
                    socket.send(Message::Text(json!({
                        "type": "error",
                        "status": 503,
                        "error": compaction_error("previous_response_not_found"),
                    }).to_string().into())).await.unwrap();
                    continue;
                }
                let input = request["input"].as_array().expect("native input");
                let triggered = input.iter().any(|item| item["type"] == "compaction_trigger");
                let replayed = input.iter().any(|item| item == &state);
                if !triggered && !replayed {
                    socket.send(Message::Text(json!({"type":"error","error":{"type":"invalid_request_error","code":"invalid_state","message":"native state was changed"}}).to_string().into())).await.unwrap();
                    continue;
                }
                let output = if triggered { vec![state.clone()] } else { vec![json!({"type":"message","id":"reply","role":"assistant","status":"completed","content":[{"type":"output_text","text":"native replay accepted","annotations":[]}]})] };
                let response_id = if triggered { "trigger-response" } else { "replay-response" };
                let created = stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                    response_id, "upstream-model", "in_progress", Vec::new(),
                    Value::Null, Value::Null, Value::Null,
                );
                socket.send(Message::Text(json!({"type":"response.created","response":created}).to_string().into())).await.unwrap();
                for (index, item) in output.iter().enumerate() {
                    socket.send(Message::Text(json!({"type":"response.output_item.added","output_index":index,"item":item}).to_string().into())).await.unwrap();
                    socket.send(Message::Text(json!({"type":"response.output_item.done","output_index":index,"item":item}).to_string().into())).await.unwrap();
                }
                let completed = stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                    response_id, "upstream-model", "completed", if triggered { Vec::new() } else { output },
                    Value::Null, Value::Null,
                    json!({"input_tokens":3,"output_tokens":1,"total_tokens":4,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}),
                );
                socket.send(Message::Text(json!({"type":"response.completed","response":completed}).to_string().into())).await.unwrap();
            }
        }).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/responses", get(responses))
        .route("/responses/compact", post(compact))
        .with_state(Arc::clone(&upstream_calls));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    crate::plugin::test_support::install_distributed_vendor(&gateway, "openai-codex")
        .await
        .expect("Codex vendor plugin");
    let provider = gateway
        .storage
        .providers()
        .create(crate::db::models::CreateProviderRecord {
            name: "local Codex".into(),
            vendor: Some("openai-codex".into()),
            channel: Some("codex".into()),
            protocol: "open-responses".into(),
            base_url: format!("http://{address}"),
            preset_key: Some("openai".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: json!({
                "websocket_url": format!("ws://{address}/responses")
            })
            .to_string(),
            auth_mode: "oauth".into(),
            use_proxy: false,
        })
        .await
        .unwrap();
    gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            crate::db::models::UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "access-token-before-refresh".into(),
                refresh_token: Some("refresh-token-before-refresh".into()),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                resource_url: Some(format!("http://{address}")),
                subject_id: Some("account-one".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    add_test_provider_model(&gateway, &provider.id).await;
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "compact-codex".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                rpm_pool_id: None,
                provider_id: provider.id.clone(),
                model: Some("upstream-model".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .unwrap();
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "native compact".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![route.id.into()],
        })
        .await
        .unwrap();
    let principal = Principal::new(key.id);
    let pair = stravia_protocol_codec::transform::ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let request = pair
        .decode_request(
            json!({"model":"compact-codex","input":[{"role":"user","content":"compact this"}]}),
        )
        .unwrap();
    let mut input = TurnInput::new(principal.clone(), request);
    input.purpose = ModelTurnPurpose::Compact;
    let mut turn = gateway
        .model_turn
        .execute(input)
        .await
        .expect("standalone uses HTTP unary despite Codex stream-only generation");
    let CanonicalEvent::Compacted(result) = turn.output.next().await.unwrap().unwrap() else {
        panic!("native compact terminal required")
    };
    assert_eq!(result.wire["id"], "compact-unary");
    assert_eq!(result.wire["output"][0]["content"][0]["text"], "retained");
    assert_eq!(
        result.wire["output"][1]["rolling_identity"],
        json!({"version":2})
    );
    for (standalone, code) in [
        (true, "invalid_encrypted_content"),
        (false, "previous_response_not_found"),
    ] {
        let calls_before_rejection = upstream_calls.load(Ordering::SeqCst);
        let mut wire = json!({
            "model": "compact-codex",
            "input": [{"role": "user", "content": "do not replay rejected compaction"}],
            "instructions": "reject-native-compaction",
        });
        if !standalone {
            wire["context_management"] = json!([{"type": "compaction", "compact_threshold": 2000}]);
        }
        let mut input = TurnInput::new(principal.clone(), pair.decode_request(wire).unwrap());
        if standalone {
            input.purpose = ModelTurnPurpose::Compact;
        }
        let rejected = gateway
            .model_turn
            .execute(input)
            .await
            .err()
            .expect("native compaction must preserve the upstream rejection");
        assert_eq!(rejected.upstream_status, Some(503));
        let body = rejected
            .upstream_body
            .expect("original upstream error body");
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["details"], json!({"retained": true}));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("native-compaction-rejected")
        );
        assert!(!body.to_string().contains("access-token-before-refresh"));
        assert_eq!(
            upstream_calls.load(Ordering::SeqCst),
            calls_before_rejection + 1,
            "native compaction rejection must not trigger recovery or replay"
        );
    }
    let trigger = pair.decode_request(json!({"model":"compact-codex","input":[{"role":"user","content":"remote v2"},{"type":"compaction_trigger"}]})).unwrap();
    let mut triggered = gateway
        .model_turn
        .execute(TurnInput::new(principal.clone(), trigger))
        .await
        .unwrap();
    let mut native = None;
    while let Some(event) = triggered.output.next().await {
        match event.unwrap() {
            CanonicalEvent::Delta(AiStreamDelta::ItemDone { item, .. }) if item.is_compaction() => {
                assert!(
                    gateway
                        .compaction
                        .resolve(&principal, std::slice::from_ref(&item))
                        .await
                        .unwrap()
                        .is_some(),
                    "complete state is durable before delivery"
                );
                native = stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(
                    &item,
                );
            }
            CanonicalEvent::Completed(_) => break,
            _ => {}
        }
    }
    let native = native.expect("native triggered state");
    let oauth = gateway.storage.oauth_credentials();
    let before_refresh = oauth
        .get(&provider.id)
        .await
        .unwrap()
        .expect("OAuth connection");
    let locked = oauth
        .try_begin_refresh(&provider.id, before_refresh.status_version)
        .await
        .unwrap()
        .expect("OAuth refresh lease");
    let refreshed = oauth
        .complete_refresh(
            &provider.id,
            locked.status_version,
            crate::db::models::UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "access-token-after-refresh".into(),
                refresh_token: Some("refresh-token-after-refresh".into()),
                expires_at: Some("2099-06-01T00:00:00Z".into()),
                resource_url: Some(format!("http://{address}")),
                subject_id: Some("account-one".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(refreshed.connection_id, before_refresh.connection_id);

    let replay = pair.decode_request(json!({"model":"compact-codex","input":[native.clone(),{"role":"user","content":"continue after refresh"}]})).unwrap();
    let mut continued = gateway
        .model_turn
        .execute(TurnInput::new(principal.clone(), replay))
        .await
        .unwrap();
    let mut accepted = false;
    while let Some(event) = continued.output.next().await {
        if let CanonicalEvent::Completed(response) = event.unwrap() {
            accepted = response.output_text() == "native replay accepted";
            break;
        }
    }
    assert!(
        accepted,
        "rotating every OAuth token field must preserve native state replay"
    );

    let reconnected = oauth
        .upsert(
            &provider.id,
            crate::db::models::UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "different-account-access-token".into(),
                refresh_token: Some("different-account-refresh-token".into()),
                expires_at: Some("2099-12-01T00:00:00Z".into()),
                resource_url: Some(format!("http://{address}")),
                subject_id: Some("account-two".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(reconnected.connection_id, refreshed.connection_id);
    let calls_before_rejected_replay = upstream_calls.load(Ordering::SeqCst);
    let stale_replay = pair.decode_request(json!({"model":"compact-codex","input":[native,{"role":"user","content":"must not cross accounts"}]})).unwrap();
    let rejected = gateway
        .model_turn
        .execute(TurnInput::new(principal, stale_replay))
        .await
        .err()
        .expect("old account state must be rejected");
    assert_eq!(rejected.code, "compaction_target_mismatch");
    assert_eq!(
        upstream_calls.load(Ordering::SeqCst),
        calls_before_rejected_replay,
        "old native state must be rejected before reaching the reconnected account"
    );
    server.abort();
}

#[tokio::test]
async fn observation_writer_failure_does_not_change_canonical_publication_success() {
    let (_directory, gateway, mut turn, store, principal, _, pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    gateway.observation.shutdown().await;
    store.release.notify_one();
    assert!(matches!(
        turn.output.next().await,
        Some(Ok(CanonicalEvent::Completed(_)))
    ));
    assert!(turn.output.next().await.is_none());
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
}
