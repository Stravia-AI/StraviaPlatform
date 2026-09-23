//! 显式 opt-in 的真实 Codex Component 与 Host WebSocket 合同回归。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, MessageContent, OpenResponsesExt, ProtocolExt, Role,
};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent, VendorRuntime, WebSocketMessage,
};
use stravia_vendor_sdk::{ErrorKind, OperationInput, OperationOutput, ProviderSnapshot};

/// 只从全量供应商 manifest 定位组件，避免误用遗留的单独构建产物。
fn codex_artifact() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-plugins-all");
    let manifest_path = dir.join("manifest.json");
    let bytes = std::fs::read(&manifest_path).unwrap_or_else(|error| {
        panic!(
            "cannot read {} ({error}); run task build:vendors:all",
            manifest_path.display()
        )
    });
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "invalid {} ({error}); run task build:vendors:all",
            manifest_path.display()
        )
    });
    let codex = manifest
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["vendor_id"] == "openai-codex")
        })
        .expect("openai-codex missing from vendor manifest; run task build:vendors:all");
    let file = codex["file"]
        .as_str()
        .expect("openai-codex artifact path missing; run task build:vendors:all");
    dir.join(file)
}

#[derive(Default)]
struct Recording {
    logs: Mutex<Vec<String>>,
    connections: Mutex<Vec<ConnectionRecord>>,
    sent: Mutex<Vec<serde_json::Value>>,
    closed: Mutex<Vec<Option<String>>>,
}

struct ConnectionRecord {
    headers: BTreeMap<String, String>,
    continuation_id: Option<String>,
}

struct MockServices {
    frames: Vec<String>,
    recorded: Arc<Recording>,
}

struct MockSocket {
    frames: Mutex<std::vec::IntoIter<WebSocketMessage>>,
    recorded: Arc<Recording>,
}

#[async_trait]
impl HostWebSocket for MockSocket {
    async fn send(&self, message: WebSocketMessage) -> Result<(), HostFailure> {
        if let WebSocketMessage::Text(text) = &message {
            self.recorded
                .sent
                .lock()
                .push(serde_json::from_str(text).expect("Codex sent valid JSON"));
        }
        Ok(())
    }

    async fn next(&self) -> Result<Option<WebSocketMessage>, HostFailure> {
        Ok(self.frames.lock().next())
    }

    async fn close(&self, continuation_id: Option<String>) -> Result<(), HostFailure> {
        self.recorded.closed.lock().push(continuation_id);
        Ok(())
    }
}

#[async_trait]
impl HostServices for MockServices {
    fn http_start(&self, _request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        Err(HostFailure::new(ErrorKind::Trapped, "unexpected HTTP path"))
    }

    async fn ws_connect(
        &self,
        _url: String,
        headers: Vec<(String, String)>,
        _protocols: Vec<String>,
        continuation_id: Option<String>,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        self.recorded.connections.lock().push(ConnectionRecord {
            headers: headers.into_iter().collect(),
            continuation_id,
        });
        Ok(Arc::new(MockSocket {
            frames: Mutex::new(
                self.frames
                    .iter()
                    .map(|frame| WebSocketMessage::Text(frame.clone()))
                    .collect::<Vec<_>>()
                    .into_iter(),
            ),
            recorded: Arc::clone(&self.recorded),
        }))
    }

    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(None)
    }

    async fn write_private_state(&self, _bytes: Vec<u8>) -> Result<(), HostFailure> {
        Ok(())
    }

    async fn emit_event(&self, _event: RuntimeEvent) -> Result<(), HostFailure> {
        Ok(())
    }

    fn log(&self, level: LogLevel, message: &str) {
        let level = match level {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        };
        self.recorded
            .logs
            .lock()
            .push(format!("{level}: {message}"));
    }

    fn generation_is_current(&self, _generation: u64) -> bool {
        true
    }
}

fn codex_provider() -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "openai-codex".into(),
        channel: "codex".into(),
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        protocol: "open-responses".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::from([
            (
                "access_token".into(),
                serde_json::Value::String("test-token".into()),
            ),
            (
                "account_id".into(),
                serde_json::Value::String("acct-test".into()),
            ),
        ]),
        model: Some("gpt-6-astra".into()),
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    }
}

fn response_resource(status: &str, output: serde_json::Value, id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "object": "response", "created_at": 1790129800, "completed_at": 1790129810,
        "status": status, "incomplete_details": null, "model": "gpt-6-astra",
        "previous_response_id": null, "instructions": null, "output": output, "error": null,
        "tools": [], "tool_choice": "auto", "truncation": "disabled", "parallel_tool_calls": true,
        "text": {"format": {"type": "text"}}, "top_p": null, "presence_penalty": null,
        "frequency_penalty": null, "top_logprobs": null, "temperature": null,
        "reasoning": {"effort": "high", "summary": "auto"},
        "usage": {
            "input_tokens": 10, "output_tokens": 20, "total_tokens": 30,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 12}
        },
        "max_output_tokens": null, "max_tool_calls": null, "store": false, "background": false,
        "service_tier": "default", "metadata": {}, "safety_identifier": null, "prompt_cache_key": null
    })
}

fn codex_frames(encrypted_len: usize, response_id: &str) -> Vec<String> {
    let encrypted = "gAAAAA".to_string() + &"x".repeat(encrypted_len);
    let reasoning = serde_json::json!({
        "type": "reasoning",
        "id": "rs_1",
        "summary": [{"type": "summary_text", "text": "thinking"}],
        "content": [],
        "encrypted_content": encrypted,
    });
    let message = serde_json::json!({
        "type": "message",
        "id": "msg_1",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "你好，我是 ChatGPT。"}],
    });
    vec![
        serde_json::json!({
            "type": "response.created", "sequence_number": 0,
            "response": response_resource("in_progress", serde_json::json!([]), response_id)
        })
        .to_string(),
        serde_json::json!({
            "type": "response.output_item.added", "sequence_number": 1, "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": [], "content": []}
        })
        .to_string(),
        serde_json::json!({
            "type": "response.output_item.done", "sequence_number": 2, "output_index": 0,
            "item": reasoning
        })
        .to_string(),
        serde_json::json!({
            "type": "response.output_item.added", "sequence_number": 3, "output_index": 1,
            "item": {"type": "message", "id": "msg_1", "status": "in_progress", "role": "assistant", "content": []}
        })
        .to_string(),
        serde_json::json!({
            "type": "response.content_part.added", "sequence_number": 4,
            "output_index": 1, "content_index": 0, "item_id": "msg_1",
            "part": {"type": "output_text", "text": ""}
        })
        .to_string(),
        serde_json::json!({
            "type": "response.output_text.delta", "sequence_number": 5,
            "output_index": 1, "content_index": 0, "item_id": "msg_1",
            "delta": "你好，我是 ChatGPT。"
        })
        .to_string(),
        serde_json::json!({
            "type": "response.output_text.done", "sequence_number": 6,
            "output_index": 1, "content_index": 0, "item_id": "msg_1",
            "text": "你好，我是 ChatGPT。"
        })
        .to_string(),
        serde_json::json!({
            "type": "response.content_part.done", "sequence_number": 7,
            "output_index": 1, "content_index": 0, "item_id": "msg_1",
            "part": {"type": "output_text", "text": "你好，我是 ChatGPT。"}
        })
        .to_string(),
        serde_json::json!({
            "type": "response.output_item.done", "sequence_number": 8, "output_index": 1,
            "item": message
        })
        .to_string(),
        serde_json::json!({
            "type": "response.completed", "sequence_number": 9,
            "response": response_resource("completed", serde_json::json!([reasoning, message]), response_id)
        })
        .to_string(),
    ]
}

fn request() -> AiRequest {
    AiRequest::new(
        "gpt-6-astra",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("你好".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
}

async fn load_codex() -> (VendorRuntime, LoadedPlugin) {
    let path = codex_artifact();
    let bytes = std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {} ({error}); run task build:vendors:all",
            path.display()
        )
    });
    let runtime = VendorRuntime::new().expect("runtime");
    let plugin = runtime.load(&bytes).await.expect("Codex component loads");
    (runtime, plugin)
}

async fn execute_once(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    provider: ProviderSnapshot,
    request: AiRequest,
    frames: Vec<String>,
    recorded: &Arc<Recording>,
) -> Result<OperationOutput, RuntimeError> {
    let services = Arc::new(MockServices {
        frames,
        recorded: Arc::clone(recorded),
    });
    let scope = OperationScope::new(
        services,
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(300)),
        0,
    );
    runtime
        .execute(
            plugin,
            "codex",
            OperationInput::Infer { provider, request },
            scope,
        )
        .await
}

/// Regression: reasoning signatures of realistic size must complete now that
/// the guest resource budgets (fuel and per-event bytes) are gone — previously
/// `fuel_per_operation = 50_000_000` trapped `OutOfFuel` while decoding a
/// ~256 KiB `encrypted_content` blob, and `max_event_bytes` rejected single
/// canonical events over 2 MiB.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn large_reasoning_signature_completes() {
    let (runtime, plugin) = load_codex().await;
    for len in [64 * 1024usize, 256 * 1024, 1024 * 1024, 3 * 1024 * 1024] {
        let recorded = Arc::new(Recording::default());
        let result = execute_once(
            &runtime,
            &plugin,
            codex_provider(),
            request(),
            codex_frames(len, "resp_1"),
            &recorded,
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "encrypted_len={len}: {error}; logs={:?}",
                *recorded.logs.lock()
            )
        });
        let OperationOutput::Infer(response) = result else {
            panic!("Codex inference returned a different operation");
        };
        assert_eq!(response.output_text(), "你好，我是 ChatGPT。");
        assert_eq!(
            response
                .protected_reasoning_signatures()
                .flatten()
                .map(str::len)
                .collect::<Vec<_>>(),
            vec![len + "gAAAAA".len()],
            "encrypted_len={len}"
        );
        assert_eq!(*recorded.closed.lock(), vec![Some("resp_1".into())]);
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn affinity_and_account_headers_follow_actual_codex_frames() {
    let (runtime, plugin) = load_codex().await;
    let recorded = Arc::new(Recording::default());
    let mut provider = codex_provider();
    provider
        .operation_metadata
        .insert("transport_affinity".into(), "window-key".into());
    let first_id = "resp_first_complete_id_without_truncation_0123456789";
    let cases = [
        (None, None, None, first_id),
        (
            Some("default"),
            Some(first_id),
            None,
            "resp_second_complete_id_abcdef",
        ),
        (Some("priority"), None, None, "resp_priority"),
        (
            None,
            None,
            Some(("acct-other", "test-token")),
            "resp_account",
        ),
        (None, None, Some(("acct-test", "other-token")), "resp_token"),
    ];
    for (tier, previous_id, credentials, response_id) in cases {
        let mut turn_provider = provider.clone();
        if let Some((account_id, access_token)) = credentials {
            turn_provider
                .credentials
                .insert("account_id".into(), account_id.into());
            turn_provider
                .credentials
                .insert("access_token".into(), access_token.into());
        }
        let mut turn_request = request();
        turn_request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            service_tier: tier.map(str::to_owned),
            previous_response_id: previous_id.map(str::to_owned),
            ..Default::default()
        }));
        let result = execute_once(
            &runtime,
            &plugin,
            turn_provider,
            turn_request,
            codex_frames(0, response_id),
            &recorded,
        )
        .await
        .expect("Codex WebSocket inference");
        let OperationOutput::Infer(response) = result else {
            panic!("Codex inference returned a different operation");
        };
        assert_eq!(response.id, response_id);
    }

    let connections = recorded.connections.lock();
    let sent = recorded.sent.lock();
    let closed = recorded.closed.lock();
    assert_eq!(connections.len(), cases.len());
    assert_eq!(sent.len(), cases.len());
    assert_eq!(closed.len(), cases.len());
    for (index, (_, previous_id, _, response_id)) in cases.iter().enumerate() {
        assert_eq!(connections[index].continuation_id.as_deref(), *previous_id);
        assert_eq!(sent[index]["previous_response_id"].as_str(), *previous_id);
        assert_eq!(sent[index]["type"], "response.create");
        assert_eq!(closed[index].as_deref(), Some(*response_id));
        for (header, metadata) in [
            ("session-id", "session_id"),
            ("thread-id", "thread_id"),
            ("x-codex-window-id", "x-codex-window-id"),
        ] {
            let value = connections[index]
                .headers
                .get(header)
                .expect("identity header");
            assert_eq!(
                sent[index]["client_metadata"][metadata].as_str(),
                Some(value.as_str())
            );
            assert_eq!(connections[0].headers.get(header), Some(value));
        }
        assert_eq!(
            connections[index].headers.get("x-client-request-id"),
            connections[index].headers.get("thread-id")
        );
    }
    assert!(
        sent[0]["client_metadata"]["turn_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_ne!(
        sent[0]["client_metadata"]["turn_id"],
        sent[1]["client_metadata"]["turn_id"]
    );
    assert!(sent.iter().all(|frame| frame.get("service_tier").is_none()));
    assert_eq!(
        connections[0].headers.get("x-codex-routing-hint"),
        connections[1].headers.get("x-codex-routing-hint")
    );
    assert_eq!(
        connections[0]
            .headers
            .get("x-codex-routing-hint")
            .map(String::as_str),
        Some("model=gpt-6-astra")
    );
    assert_eq!(
        connections[2]
            .headers
            .get("x-codex-routing-hint")
            .map(String::as_str),
        Some("model=gpt-6-astra;tier=priority")
    );
    assert_eq!(
        connections[0]
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        Some("acct-test")
    );
    assert_eq!(
        connections[3]
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        Some("acct-other")
    );
    assert_eq!(
        connections[0].headers.get("authorization"),
        connections[3].headers.get("authorization")
    );
    assert_eq!(
        connections[0].headers.get("chatgpt-account-id"),
        connections[4].headers.get("chatgpt-account-id")
    );
    assert_eq!(
        connections[0]
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer test-token")
    );
    assert_eq!(
        connections[4]
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer other-token")
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn no_code_continuation_error_requires_requested_id_before_response_created() {
    let (runtime, plugin) = load_codex().await;
    let rejection = serde_json::json!({
        "type": "error", "status": 400,
        "error": {"type": "invalid_request_error", "message": "Invalid `previous_response_id`."}
    })
    .to_string();
    let created = serde_json::json!({
        "type": "response.created", "sequence_number": 0,
        "response": response_resource("in_progress", serde_json::json!([]), "resp_started")
    })
    .to_string();
    for (previous_id, saw_created, expected_not_found) in [
        (Some("resp_missing"), false, true),
        (None, false, false),
        (Some("resp_missing"), true, false),
    ] {
        let mut turn_request = request();
        turn_request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            previous_response_id: previous_id.map(str::to_owned),
            ..Default::default()
        }));
        let mut frames = Vec::new();
        if saw_created {
            frames.push(created.clone());
        }
        frames.push(rejection.clone());
        let recorded = Arc::new(Recording::default());
        let result = execute_once(
            &runtime,
            &plugin,
            codex_provider(),
            turn_request,
            frames,
            &recorded,
        )
        .await;
        assert_eq!(
            recorded.connections.lock()[0].continuation_id.as_deref(),
            previous_id
        );
        assert_eq!(
            recorded.sent.lock()[0]["previous_response_id"].as_str(),
            previous_id
        );
        match (expected_not_found, result) {
            (
                true,
                Err(RuntimeError::Plugin {
                    kind: ErrorKind::ContinuationNotFound,
                    upstream_status: Some(400),
                    ..
                }),
            ) => {}
            (
                false,
                Err(RuntimeError::Plugin {
                    kind: ErrorKind::Upstream(_),
                    ..
                }),
            ) => {}
            (_, other) => panic!(
                "unexpected classification for previous_id={previous_id:?}, saw_created={saw_created}: {other:?}"
            ),
        }
    }
}
