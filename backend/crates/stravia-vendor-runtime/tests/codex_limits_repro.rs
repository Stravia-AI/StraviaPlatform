//! Reproduction harness: drive the real Codex vendor component with a
//! synthetic Responses-over-WebSocket stream. Not a product regression test;
//! used for local diagnosis.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LogLevel,
    OperationScope, RuntimeEvent, VendorRuntime, WebSocketMessage,
};
use stravia_vendor_sdk::{ErrorKind, OperationInput, ProviderSnapshot};

/// Locate a locally built Codex component (`task` plugin build output lives
/// under `target/vendor-plugins/`). The test skips when no build exists.
fn codex_artifact() -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-plugins");
    let mut candidates: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.to_owned();
            (name.starts_with("openai-codex-") && name.ends_with(".wasm")).then_some(path)
        })
        .collect();
    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    candidates.pop()
}

#[derive(Default)]
struct Recording {
    events: Mutex<Vec<(usize, String)>>,
    logs: Mutex<Vec<String>>,
    sent: Mutex<Vec<usize>>,
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
            self.recorded.sent.lock().unwrap().push(text.len());
        }
        Ok(())
    }

    async fn next(&self) -> Result<Option<WebSocketMessage>, HostFailure> {
        Ok(self.frames.lock().unwrap().next())
    }

    async fn close(&self, _response_continuation: bool) -> Result<(), HostFailure> {
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
        _headers: Vec<(String, String)>,
        _protocols: Vec<String>,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
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

    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure> {
        let (size, kind) = match &event {
            RuntimeEvent::UpstreamStarted => (0, "upstream_started".to_string()),
            RuntimeEvent::Delta(delta) => (
                serde_json::to_string(delta).map_or(0, |v| v.len()),
                "delta".to_string(),
            ),
            RuntimeEvent::Completed => (0, "completed".to_string()),
            RuntimeEvent::Compacted => (0, "compacted".to_string()),
            RuntimeEvent::Failed { message, .. } => (message.len(), format!("failed:{message}")),
        };
        self.recorded.events.lock().unwrap().push((size, kind));
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
            .unwrap()
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

fn response_resource(status: &str, output: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "resp_1", "object": "response", "created_at": 1790129800, "completed_at": 1790129810,
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

fn codex_frames(encrypted_len: usize) -> Vec<String> {
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
            "response": response_resource("in_progress", serde_json::json!([]))
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
            "response": response_resource("completed", serde_json::json!([reasoning, message]))
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

async fn run_once(encrypted_len: usize) -> Option<(String, Arc<Recording>)> {
    let Some(path) = codex_artifact() else {
        eprintln!("no built openai-codex component under target/vendor-plugins; skipping");
        return None;
    };
    let runtime = VendorRuntime::new().expect("runtime");
    let bytes = std::fs::read(path).expect("codex plugin artifact");
    let plugin = runtime.load(&bytes).await.expect("plugin loads");
    let recorded = Arc::new(Recording::default());
    let services = Arc::new(MockServices {
        frames: codex_frames(encrypted_len),
        recorded: Arc::clone(&recorded),
    });
    let scope = OperationScope::new(
        services,
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(300)),
        0,
    );
    let result = runtime
        .execute(
            &plugin,
            "codex",
            OperationInput::Infer {
                provider: codex_provider(),
                request: request(),
            },
            scope,
        )
        .await;
    let outcome = match result {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("{error}"),
    };
    Some((outcome, recorded))
}

/// Regression: reasoning signatures of realistic size must complete now that
/// the guest resource budgets (fuel and per-event bytes) are gone — previously
/// `fuel_per_operation = 50_000_000` trapped `OutOfFuel` while decoding a
/// ~256 KiB `encrypted_content` blob, and `max_event_bytes` rejected single
/// canonical events over 2 MiB.
#[tokio::test(flavor = "current_thread")]
async fn large_reasoning_signature_completes() {
    for len in [64 * 1024usize, 256 * 1024, 1024 * 1024, 3 * 1024 * 1024] {
        let Some((outcome, recorded)) = run_once(len).await else {
            return;
        };
        let logs = recorded.logs.lock().unwrap();
        assert_eq!(outcome, "ok", "encrypted_len={len}, logs={:?}", *logs);
    }
}
