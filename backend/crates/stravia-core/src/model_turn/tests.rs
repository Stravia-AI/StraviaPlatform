use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue};

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::{
    CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput, ProviderSourceInput,
};
use crate::hook::Principal;
use crate::protocol::ir::{AiResponse, AiStreamDelta};
use crate::provider_models::CreateManualProviderModel;
use crate::proxy::context::CancellationToken;

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

async fn serve_openai_response(body: serde_json::Value) -> (String, Arc<AtomicUsize>) {
    serve_openai_status(200, body).await
}

async fn serve_incomplete_openai_stream() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind streaming provider");
    let address = listener.local_addr().expect("provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    tokio::spawn(async move {
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
    });
    (format!("http://{address}/v1"), calls)
}

async fn serve_complete_openai_stream(request_count: usize) -> (String, Arc<AtomicUsize>) {
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
            let frames = [
                serde_json::json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "created": 1,
                    "model": "upstream-model",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": "ok"},
                        "finish_reason": null
                    }]
                }),
                serde_json::json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "created": 1,
                    "model": "upstream-model",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "stop"
                    }]
                }),
                serde_json::json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "created": 1,
                    "model": "upstream-model",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 11,
                        "completion_tokens": 7,
                        "total_tokens": 18
                    }
                }),
            ]
            .into_iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>()
                + "data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{frames}",
                frames.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write streaming response");
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
            observed.lock().expect("captured ZDR requests").push(body);

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
                    crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
                        "resp-replayed",
                        "upstream-model",
                        "in_progress",
                        Vec::new(),
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                    );
                let completed =
                    crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
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

async fn serve_openai_capture() -> (String, Arc<Mutex<Vec<u8>>>) {
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
        *observed.lock().expect("captured request") = request;
        let body = serde_json::json!({
            "id": "chatcmpl-capture",
            "object": "chat.completion",
            "created": 1,
            "model": "upstream-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
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
    let (base_url, captured) = serve_openai_capture().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(GatewayConfig {
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
                vendor: Some("test-http".into()),
                protocol: "openai-compatible".into(),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            use_proxy: false,
        })
        .await
        .expect("Provider");
    add_test_provider_model(&gateway, &provider.id).await;
    let model = admin
        .create_model(CreateRoute {
            model_id: model_name.into(),
            display_name: None,
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "upstream-model".into(),
            targets: Vec::new(),
        })
        .await
        .expect("Model");
    let model_ids = if bind_key {
        vec![model.id]
    } else {
        let other_model = admin
            .create_model(CreateRoute {
                model_id: format!("{model_name}-other"),
                display_name: None,
                balance: None,
                target_provider: provider.id,
                target_model: "upstream-model".into(),
                targets: Vec::new(),
            })
            .await
            .expect("other Model");
        vec![other_model.id]
    };
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Capture key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
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
            .with_execution(cancellation, Instant::now() + Duration::from_secs(1)),
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
            .with_execution(CancellationToken::new(), Instant::now()),
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
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
                    vendor: Some("test-http".into()),
                    protocol: "openai-compatible".into(),
                    base_url,
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "test-provider-key".into(),
                },
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
            target_provider: String::new(),
            target_model: String::new(),
            targets: providers
                .iter()
                .enumerate()
                .map(|(index, provider)| CreateTarget {
                    enabled: true,
                    provider_id: provider.id.clone(),
                    model: "upstream-model".into(),
                    priority: Some((providers.len() - index) as i32),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(0),
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                })
                .collect(),
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Failover key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model.id],
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
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
                vendor: Some("xai".into()),
                protocol: "open-responses".into(),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
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
            target_provider: provider.id,
            target_model: "upstream-model".into(),
            targets: Vec::new(),
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "ZDR key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model.id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let executor = LiveModelTurnExecutor::new(
        gateway,
        super::continuation::ScriptedContinuation::hit("resp-zdr"),
    );
    let mut request = AiRequest::new(
        "zdr-model",
        vec![crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::User,
            content: crate::protocol::ir::MessageContent::Text("follow-up".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = true;
    request.meta.source_protocol = Some(crate::protocol::ids::OPEN_RESPONSES_2026_04_24);
    request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt::default(),
    ));

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

    let captured = captured.lock().expect("captured ZDR requests");
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["previous_response_id"], "resp-zdr");
    assert!(captured[1].get("previous_response_id").is_none());
}

#[tokio::test]
async fn request_scoped_http_errors_do_not_quarantine_the_target() {
    let (base_url, calls) = serve_openai_status_repeated(
        404,
        serde_json::json!({"error": {"message": "request-specific resource is missing"}}),
        4,
    )
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(GatewayConfig {
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
                vendor: Some("test-http".into()),
                protocol: "openai-compatible".into(),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
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
            target_provider: provider.id,
            target_model: "upstream-model".into(),
            targets: Vec::new(),
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Request error key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model.id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");

    for _ in 0..4 {
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
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn execute_rejects_tools_when_no_target_declares_function_tool_support() {
    let (base_url, calls) = serve_openai_response(serde_json::json!({
        "id": "chatcmpl-unexpected",
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
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
                vendor: Some("test-http".into()),
                protocol: "openai-compatible".into(),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
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
            target_provider: provider.id,
            target_model: "upstream-model".into(),
            targets: Vec::new(),
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "No tools key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model.id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let mut request = AiRequest::new("no-tools-model", Vec::new());
    request.tools = Some(vec![crate::protocol::ir::ToolSpec {
        name: "lookup".into(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);

    let error = match gateway
        .model_turn
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
    {
        Ok(_) => panic!("unknown function-tool capability must fail closed"),
        Err(error) => error,
    };

    assert_eq!(error.code, "tools_unsupported");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn execute_does_not_fail_over_after_the_first_canonical_delta() {
    let (partial_url, partial_calls) = serve_incomplete_openai_stream().await;
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
    let (gateway, _logs) = Gateway::new(GatewayConfig {
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
                        vendor: Some("test-http".into()),
                        protocol: "openai-compatible".into(),
                        base_url,
                        models_source: None,
                        static_models: None,
                    },
                    credential: ProviderCredentialInput::ApiKey {
                        value: "test-provider-key".into(),
                    },
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
            target_provider: String::new(),
            target_model: String::new(),
            targets: providers
                .iter()
                .enumerate()
                .map(|(index, provider)| CreateTarget {
                    enabled: true,
                    provider_id: provider.id.clone(),
                    model: "upstream-model".into(),
                    priority: Some((providers.len() - index) as i32),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(0),
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                })
                .collect(),
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Stream lock key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model.id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let mut request = AiRequest::new("stream-lock-model", Vec::new());
    request.stream.enabled = true;

    let turn = gateway
        .model_turn
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("streaming Model Turn locks the first Target");
    assert_eq!(turn.route.provider_id, providers[0].id);
    let events = turn.output.collect::<Vec<_>>().await;

    assert!(events.iter().any(
            |event| matches!(event, Ok(CanonicalEvent::Delta(AiStreamDelta::TextDelta(text))) if text == "partial")
        ));
    assert!(matches!(
        events.last(),
        Some(Err(ModelTurnError { code, .. })) if code == "upstream_stream_error"
    ));
    assert_eq!(partial_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn execute_omits_previous_response_id_when_lookup_misses() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_model("lookup-miss-model", true).await;
    let executor =
        LiveModelTurnExecutor::new(gateway.clone(), continuation::ScriptedContinuation::miss());
    let mut request = AiRequest::new("lookup-miss-model", Vec::new());
    crate::model_turn::stamp_previous_response_id(&mut request, "stravia-parent");

    let turn = executor
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("missed continuation still executes");
    let _ = turn.output.collect::<Vec<_>>().await;
    let (_, body) = captured_http(&captured.lock().expect("captured miss"));

    assert!(body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn execute_sends_previous_response_id_when_lookup_hits() {
    let (_data_dir, gateway, captured, key) =
        gateway_with_captured_model("lookup-hit-model", true).await;
    let executor = LiveModelTurnExecutor::new(
        gateway.clone(),
        continuation::ScriptedContinuation::hit("upstream-resp-1"),
    );
    let mut request = AiRequest::new("lookup-hit-model", Vec::new());
    crate::model_turn::stamp_previous_response_id(&mut request, "stravia-parent");

    let turn = executor
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("hit continuation executes");
    let _ = turn.output.collect::<Vec<_>>().await;
    let (_, body) = captured_http(&captured.lock().expect("captured hit"));

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
    let (head, _) = captured_http(&captured.lock().expect("captured headers"));
    let head = head.to_ascii_lowercase();

    assert!(head.contains("openai-beta: responses=v1"));
    assert!(head.contains("authorization: bearer test-provider-key"));
    assert!(!head.contains("attacker-key"));
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

    assert!(!captured.lock().expect("captured grant").is_empty());
}

#[tokio::test]
async fn internal_stream_log_uses_terminal_usage_and_transport_metrics() {
    let (base_url, calls) = serve_complete_openai_stream(2).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, mut logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("Streaming".into()),
            source: ProviderSourceInput::Custom {
                vendor: Some("test-http".into()),
                protocol: "openai-compatible".into(),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-provider-key".into(),
            },
            use_proxy: false,
        })
        .await
        .expect("Provider");
    add_test_provider_model(&gateway, &provider.id).await;
    let _model = admin
        .create_model(CreateRoute {
            model_id: "stream-grant-model".into(),
            display_name: Some("Streaming grant".into()),
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "upstream-model".into(),
            targets: Vec::new(),
        })
        .await
        .expect("Model");
    let other_model = admin
        .create_model(CreateRoute {
            model_id: "bound-model".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "upstream-model".into(),
            targets: Vec::new(),
        })
        .await
        .expect("other Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Streaming key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![other_model.id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let mut request = AiRequest::new("stream-grant-model", Vec::new());
    request.stream.enabled = true;

    let turn = gateway
        .model_turn
        .execute(
            TurnInput::new(Principal::new(key.id.clone()), request)
                .with_authorization(ModelTurnAuthorization::CapabilityGrant),
        )
        .await
        .expect("streaming CapabilityGrant Model Turn");
    let _ = turn.output.collect::<Vec<_>>().await;
    let entry = tokio::time::timeout(Duration::from_secs(1), logs.recv())
        .await
        .expect("internal Model Turn log")
        .expect("log channel remains open");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(entry.usage.prompt_tokens, 11);
    assert_eq!(entry.usage.completion_tokens, 7);
    assert_eq!(entry.usage.total_tokens, 18);
    assert_eq!(entry.model_name.as_deref(), Some("Streaming grant"));
    assert_eq!(entry.client_model, "stream-grant-model");
    assert!(entry.stream_chunks_count > 0);
    assert!(entry.stream_first_chunk_ms.is_some());

    let mut fallback_request = AiRequest::new("bound-model", Vec::new());
    fallback_request.stream.enabled = true;
    let fallback_turn = gateway
        .model_turn
        .execute(
            TurnInput::new(Principal::new(key.id), fallback_request)
                .with_authorization(ModelTurnAuthorization::CapabilityGrant),
        )
        .await
        .expect("bound streaming Model Turn");
    let _ = fallback_turn.output.collect::<Vec<_>>().await;
    let fallback_entry = tokio::time::timeout(Duration::from_secs(1), logs.recv())
        .await
        .expect("fallback Model Turn log")
        .expect("log channel remains open");
    assert_eq!(fallback_entry.model_name.as_deref(), Some("bound-model"));
    assert_eq!(fallback_entry.client_model, "bound-model");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
