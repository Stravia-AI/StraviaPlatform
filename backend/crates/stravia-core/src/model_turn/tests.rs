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
        *observed.lock().expect("captured request") = request;
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
    gateway_with_captured_text(model_name, bind_key, "ok").await
}

async fn gateway_with_captured_text(
    model_name: &str,
    bind_key: bool,
    text: &'static str,
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
    let gateway = Gateway::new(GatewayConfig {
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

// The real MappingStore seam can hold intern or publication acknowledgements
// after the database commits. Cancellation is not evidence that commit failed.
struct HeldPublicationStore {
    inner: Arc<dyn crate::reversible_redaction::store::MappingStore>,
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
impl crate::reversible_redaction::store::MappingStore for HeldPublicationStore {
    async fn active(
        &self,
        principal: &Principal,
    ) -> Result<
        Vec<crate::reversible_redaction::store::Mapping>,
        crate::reversible_redaction::RedactionError,
    > {
        self.inner.active(principal).await
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[String],
    ) -> Result<
        crate::reversible_redaction::store::InternedMappings,
        crate::reversible_redaction::RedactionError,
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
    ) -> Result<(), crate::reversible_redaction::RedactionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if !self.fail {
            self.inner.publish(principal, references, retention).await?;
        }
        self.entered.notify_one();
        self.release.notified().await;
        if let Some(cancellation) = self.cancel_on_release.lock().unwrap().take() {
            cancellation.cancel();
        }
        if self.fail {
            Err(crate::reversible_redaction::RedactionError::Storage)
        } else {
            Ok(())
        }
    }

    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), crate::reversible_redaction::RedactionError> {
        self.inner
            .extend_retention(principal, references, retention)
            .await
    }

    async fn cleanup_expired(&self) -> Result<u64, crate::reversible_redaction::RedactionError> {
        self.inner.cleanup_expired().await
    }
}

fn credential_observer(
    gateway: &Gateway,
    principal: &Principal,
) -> crate::interaction_observation::RunObserver {
    let id = uuid::Uuid::new_v4().to_string();
    gateway
        .observation
        .observe_ingress(crate::interaction_observation::IngressStart {
            id: id.clone(),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            protocol: "openai-compatible".into(),
        })
        .admit(crate::interaction_observation::RunStart {
            id,
            principal: principal.continuation_key(),
            api_key_id: None,
            api_key_name: Some("Discovery test".into()),
            generation_root_id: None,
            generation_parent_id: None,
            has_new_user: true,
            canonical_fingerprint: uuid::Uuid::new_v4().to_string(),
            route_id: "discovery-model".into(),
            model_display_name: None,
            ingress_protocol: "openai-compatible".into(),
        })
}

#[tokio::test]
async fn committed_discovery_survives_dropped_protection_before_intern_acknowledgement() {
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
    assert!(!encoded.contains("~stravia-secret:"));
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
    );
    let observer = credential_observer(&gateway, &principal);
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    assert!(matches!(
        gateway
            .redaction
            .protect(&principal, &mut request, Some(&observer))
            .await,
        Err(crate::reversible_redaction::RedactionError::InvalidText)
    ));
    observer.finish(crate::interaction_observation::RunOutcome {
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
    assert!(!encoded.contains("~stravia-secret:"));

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
        Err(crate::reversible_redaction::RedactionError::Storage)
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
    CancellationToken,
    i64,
) {
    let (directory, mut gateway, _, key) =
        gateway_with_captured_text("publication-model", true, "answer ~stravia-secret:").await;
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
    let executor =
        LiveModelTurnExecutor::new(gateway.clone(), continuation::ScriptedContinuation::miss());
    let run_id = uuid::Uuid::new_v4().to_string();
    let observer = gateway
        .observation
        .observe_ingress(crate::interaction_observation::IngressStart {
            id: run_id.clone(),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            protocol: "openai-compatible".into(),
        })
        .admit(crate::interaction_observation::RunStart {
            id: run_id,
            principal: principal.continuation_key(),
            api_key_id: None,
            api_key_name: None,
            generation_root_id: None,
            generation_parent_id: None,
            has_new_user: true,
            canonical_fingerprint: "publication-fixture".into(),
            route_id: "publication-model".into(),
            model_display_name: None,
            ingress_protocol: "openai-compatible".into(),
        });
    let turn = executor
        .execute(
            TurnInput::new(principal.clone(), request)
                .with_observer(observer)
                .with_execution(
                    cancellation.clone(),
                    Instant::now() + Duration::from_secs(300),
                ),
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
        cancellation,
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
                CanonicalEvent::Completed(_) => panic!("success escaped pending publication"),
            }
        }
    }
}

#[tokio::test]
async fn canonical_completion_publishes_after_trailing_output_and_is_permanently_terminal() {
    let (_directory, gateway, mut turn, store, principal, cancellation, pending_expiry) =
        held_publication_turn(false, false).await;
    assert_eq!(
        consume_until_publication(&mut turn, &store).await,
        "answer ~stravia-secret:"
    );
    // The select above dropped a pending next() future. Publication must survive
    // that pause and resume rather than issuing a second write.
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
    store.release.notify_one();
    match turn.output.next().await.unwrap().unwrap() {
        CanonicalEvent::Completed(response) => {
            assert_eq!(response.output_text(), "answer ~stravia-secret:")
        }
        CanonicalEvent::Delta(_) => panic!("all deltas must precede publication"),
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
        "answer ~stravia-secret:"
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
        "answer ~stravia-secret:"
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
    let (_directory, gateway, mut turn, store, principal, cancellation, pending_expiry) =
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
    let (_directory, _gateway, mut turn, store, _, _, _) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(301)).await;
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "deadline_exceeded"
    );
    assert!(turn.output.next().await.is_none());
    tokio::time::resume();
}

async fn assert_publication_observation(gateway: &Gateway, status: &str) {
    use crate::interaction_observation::ForestQuery;

    // Detail queries synchronize the existing asynchronous observation writer.
    gateway
        .observation
        .get_interaction("absent", ForestQuery::default())
        .await
        .unwrap();
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
    let (_directory, gateway, mut turn, store, principal, cancellation, pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    *store.cancel_on_release.lock().unwrap() = Some(cancellation);
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
