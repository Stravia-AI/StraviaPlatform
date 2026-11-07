//! 显式 opt-in 的真实 Codex Component 与 Host WebSocket 合同回归。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, AiStreamDelta, CacheRetention, Continuation, MessageContent, Role,
    ServiceTier,
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

fn http_failure(error: reqwest::Error) -> HostFailure {
    HostFailure::new(ErrorKind::upstream_unknown(), error.to_string())
}

struct LoopbackResponse {
    client: reqwest::Client,
    request: Mutex<Option<reqwest::Request>>,
    response: tokio::sync::OnceCell<tokio::sync::Mutex<reqwest::Response>>,
}

impl LoopbackResponse {
    async fn response(&self) -> Result<&tokio::sync::Mutex<reqwest::Response>, HostFailure> {
        self.response
            .get_or_try_init(|| async {
                let request = self.request.lock().take().expect("one upstream request");
                Ok(tokio::sync::Mutex::new(
                    self.client.execute(request).await.map_err(http_failure)?,
                ))
            })
            .await
    }
}

#[async_trait]
impl HostHttpResponse for LoopbackResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.response().await?.lock().await.status().as_u16())
    }
    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure> {
        Ok(self
            .response()
            .await?
            .lock()
            .await
            .headers()
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
            .collect())
    }
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self
            .response()
            .await?
            .lock()
            .await
            .chunk()
            .await
            .map_err(http_failure)?
            .map(|chunk| chunk.to_vec()))
    }
}

struct LoopbackServices {
    client: reqwest::Client,
    address: String,
    deltas: Mutex<Vec<AiStreamDelta>>,
    incremental: tokio::sync::Notify,
}

#[async_trait]
impl HostServices for LoopbackServices {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.origin().ascii_serialization(), self.address);
        let mut builder = self
            .client
            .request(request.method.parse().unwrap(), request.url)
            .body(request.body);
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        Ok(Arc::new(LoopbackResponse {
            client: self.client.clone(),
            request: Mutex::new(Some(builder.build().map_err(http_failure)?)),
            response: tokio::sync::OnceCell::new(),
        }))
    }
    async fn ws_connect(
        &self,
        _: stravia_vendor_runtime::WsRequest,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        panic!("HTTP-only attempt opened WebSocket")
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(None)
    }
    async fn write_private_state(&self, _: Vec<u8>) -> Result<(), HostFailure> {
        Ok(())
    }
    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure> {
        if let RuntimeEvent::Delta(delta) = event {
            if matches!(&delta, AiStreamDelta::TextDelta(text) | AiStreamDelta::TextDeltaWithMetadata { text, .. } if text == "hello")
            {
                self.incremental.notify_one();
            }
            self.deltas.lock().push(delta);
        }
        Ok(())
    }
    fn log(&self, _: LogLevel, _: &str) {}
    fn generation_is_current(&self, _: u64) -> bool {
        true
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_sse_without_content_type_emits_before_eof() {
    http_case("missing").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_sse_with_content_type_emits_before_eof() {
    http_case("present").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_nonstream_client_uses_native_stream() {
    http_case("nonstream").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_reasoning_ignores_unsupported_logprobs() {
    http_case("logprobs").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_malformed_and_truncated_sse_fail() {
    http_case("malformed").await;
    http_case("truncated").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_non_success_json_keeps_status() {
    http_case("error").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_compaction_uses_the_trigger_stream_and_preserves_error() {
    http_case("compact").await;
    http_case("compact-explicit").await;
    http_case("compact-error").await;
    http_case("compact-duplicate").await;
    http_case("compact-truncated").await;
}

async fn http_case(case: &'static str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (runtime, plugin) = load_codex().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let services = Arc::new(LoopbackServices {
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        address: address.clone(),
        deltas: Mutex::new(Vec::new()),
        incremental: tokio::sync::Notify::new(),
    });
    let observed = Arc::clone(&services);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
            if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .expect("request content length");
        while request.len() < header_end + length {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        let body: serde_json::Value = serde_json::from_slice(&request[header_end..]).unwrap();
        // Routing fields lead the raw wire bytes in the official declaration
        // order; a sorted map would start with `client_metadata`.
        assert!(
            request[header_end..].starts_with(br#"{"model":"#),
            "{}",
            String::from_utf8_lossy(&request[header_end..])
        );
        let raw = String::from_utf8_lossy(&request[header_end..]);
        let at = |key: &str| raw.find(&format!(r#","{key}":"#)).unwrap();
        assert!(at("stream") < at("input") && at("input") < at("client_metadata"));
        let header = |expected: &str| {
            headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(expected).then(|| value.trim())
            })
        };
        let thread = header("thread-id").expect("thread header");
        assert_eq!(thread, "5d42eca3-00ce-7fd9-879a-2f189010d915");
        assert_eq!(header("session-id"), Some(thread));
        assert_eq!(
            header("x-codex-window-id"),
            Some(format!("{thread}:0").as_str())
        );
        if case.starts_with("compact") {
            // 独立压缩是普通流式 /responses，触发器是最后一个 input item。
            assert!(headers.starts_with("POST /responses "), "{headers}");
            assert_eq!(
                body["input"].as_array().unwrap().last().unwrap(),
                &serde_json::json!({"type": "compaction_trigger"})
            );
            assert!(body.get("context_management").is_none());
            assert_eq!(body["store"], false);
            assert_eq!(
                body["tools"],
                serde_json::json!([{"type": "function", "name": "lookup",
                    "parameters": {"type": "object"}}])
            );
            assert_eq!(body["reasoning"]["effort"], "high");
            // 实时 base instructions 与推理一样投影为 developer 前缀，不留在顶层。
            assert!(body.get("instructions").is_none());
            let base_instructions = &body["input"][0];
            assert_eq!(base_instructions["role"], "developer");
            assert_eq!(
                base_instructions["internal_chat_message_metadata_passthrough"]["content_item_kinds"],
                serde_json::json!(["model.base_instructions"])
            );
            if case == "compact-explicit" {
                assert_eq!(
                    base_instructions["content"][0]["text"],
                    "session instructions\n\nCompaction instructions:\nexplicit summary rules"
                );
            } else if case == "compact" {
                assert_eq!(
                    base_instructions["content"][0]["text"],
                    stravia_protocol_codec::codec::compaction::DEFAULT_COMPACTION_INSTRUCTIONS
                );
            }
        }
        assert_eq!(body["stream"], true);
        assert_eq!(header("x-client-request-id"), Some(thread));
        assert_eq!(body["client_metadata"]["thread_id"], thread);
        let turn: Value = serde_json::from_str(
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            uuid::Uuid::parse_str(turn["turn_id"].as_str().unwrap())
                .unwrap()
                .get_version_num(),
            7
        );
        assert_ne!(turn["turn_id"], "injected-turn");
        assert!(turn.get("parent_turn_id").is_none());
        // A root thread's turn is its own causal root; the injected client
        // label never reaches the native snapshot.
        assert_eq!(turn["root_turn_id"], turn["turn_id"]);
        assert_eq!(body["client_metadata"]["root_turn_id"], turn["turn_id"]);
        assert_eq!(
            header("x-codex-turn-metadata"),
            body["client_metadata"]["x-codex-turn-metadata"].as_str()
        );
        assert_eq!(header("version"), Some("0.167.0"));
        assert_eq!(header("originator"), Some("codex-tui"));
        assert_eq!(
            header("x-codex-beta-features"),
            Some("remote_compaction_v2")
        );
        assert_eq!(header("accept"), Some("text/event-stream"));
        assert!(header("x-codex-installation-id").is_none());
        assert!(body.get("instructions").is_none());
        if case == "logprobs"
            && body["include"].as_array().is_some_and(|include| {
                include
                    .iter()
                    .any(|value| value == "message.output_text.logprobs")
            })
        {
            let payload = serde_json::json!({"error":{
                "type":"invalid_request_error",
                "message":"logprobs are not supported with reasoning models."
            }})
            .to_string();
            socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).as_bytes()).await.unwrap();
            return;
        }
        if case == "logprobs" {
            assert_eq!(
                body["include"],
                serde_json::json!(["reasoning.encrypted_content"])
            );
            assert_eq!(body["reasoning"]["effort"], "high");
            assert!(body.get("top_logprobs").is_none());
        }
        if matches!(
            case,
            "error"
                | "malformed"
                | "truncated"
                | "compact"
                | "compact-explicit"
                | "compact-error"
                | "compact-duplicate"
                | "compact-truncated"
        ) {
            let payload = match case {
                "compact" | "compact-explicit" | "compact-duplicate" | "compact-truncated" => {
                    let state = serde_json::json!({"type":"compaction","id":"cmp_1","encrypted_content":"synthetic-state"});
                    let mut events = vec![
                        serde_json::json!({"type":"response.created","sequence_number":0,"response":response_resource("in_progress",serde_json::json!([]),"resp_cmp")}),
                        serde_json::json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"compaction","id":"cmp_1"}}),
                        serde_json::json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":state.clone()}),
                    ];
                    if case == "compact-duplicate" {
                        events.push(serde_json::json!({"type":"response.output_item.added","sequence_number":3,"output_index":1,"item":{"type":"compaction","id":"cmp_2"}}));
                        events.push(serde_json::json!({"type":"response.output_item.done","sequence_number":4,"output_index":1,"item":serde_json::json!({"type":"compaction","id":"cmp_2","encrypted_content":"second-state"})}));
                    }
                    let terminal_sequence = events.len();
                    if case != "compact-truncated" {
                        events.push(serde_json::json!({"type":"response.completed","sequence_number":terminal_sequence,"response":response_resource("completed",serde_json::json!([]),"resp_cmp")}));
                    }
                    events.iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())).collect::<String>()
                }
                "error" | "compact-error" => serde_json::json!({"error":{"type":"invalid_request_error","message":"synthetic upstream rejection"}}).to_string(),
                "malformed" => "event: response.created\ndata: {not-json}\n\n".to_string(),
                "truncated" => format!("event: response.created\ndata: {}\n\n", serde_json::json!({"type":"response.created","sequence_number":0,"response":response_resource("in_progress",serde_json::json!([]),"resp_http")})),
                _ => unreachable!(),
            };
            let status = if matches!(case, "error" | "compact-error") {
                "400 Bad Request"
            } else {
                "200 OK"
            };
            socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).as_bytes()).await.unwrap();
            return;
        }
        let content_type = if case == "present" {
            "Content-Type: text/event-stream\r\n"
        } else {
            ""
        };
        socket.write_all(format!("HTTP/1.1 200 OK\r\n{content_type}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let events = [
            serde_json::json!({"type":"response.created","sequence_number":0,"response":response_resource("in_progress",serde_json::json!([]),"resp_http")}),
            serde_json::json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}),
            serde_json::json!({"type":"response.content_part.added","sequence_number":2,"output_index":0,"content_index":0,"item_id":"msg_1","part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}),
            serde_json::json!({"type":"response.output_text.delta","sequence_number":3,"output_index":0,"content_index":0,"item_id":"msg_1","delta":"hello"}),
        ];
        let first = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        socket
            .write_all(format!("{:x}\r\n{first}\r\n", first.len()).as_bytes())
            .await
            .unwrap();
        // 服务端只有观察到真实 Component 增量后才发终态及 EOF。
        tokio::time::timeout(Duration::from_secs(10), observed.incremental.notified())
            .await
            .expect("Codex must emit text before upstream EOF");
        let message = serde_json::json!({"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[],"logprobs":[]}]});
        let tool = serde_json::json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"synthetic.txt\"}","status":"completed"});
        let events = [
            serde_json::json!({"type":"response.content_part.done","sequence_number":4,"output_index":0,"content_index":0,"item_id":"msg_1","part":{"type":"output_text","text":"hello","annotations":[],"logprobs":[]}}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":message}),
            serde_json::json!({"type":"response.output_item.added","sequence_number":6,"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"","status":"in_progress"}}),
            serde_json::json!({"type":"response.function_call_arguments.delta","sequence_number":7,"output_index":1,"item_id":"fc_1","delta":"{\"path\":\"synthetic.txt\"}"}),
            serde_json::json!({"type":"response.function_call_arguments.done","sequence_number":8,"output_index":1,"item_id":"fc_1","arguments":"{\"path\":\"synthetic.txt\"}"}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":9,"output_index":1,"item":tool}),
            serde_json::json!({"type":"response.completed","sequence_number":10,"response":response_resource("completed",serde_json::json!([message,tool]),"resp_http")}),
        ];
        let terminal = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        socket
            .write_all(format!("{:x}\r\n{terminal}\r\n0\r\n\r\n", terminal.len()).as_bytes())
            .await
            .unwrap();
    });
    let mut provider = codex_provider();
    provider.base_url = address;
    provider
        .operation_metadata
        .insert("transport_preference".into(), "http-only".into());
    provider
        .operation_metadata
        .insert("transport_affinity".into(), " 空白\0\r\n会话😀 ".into());
    let scope = OperationScope::new(
        services.clone(),
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(30)),
        0,
    );
    let mut inference = request();
    inference.stream.enabled = case != "nonstream";
    if case.starts_with("compact") {
        inference.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
            name: "lookup".into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            defer_loading: None,
        }]);
        inference.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::High);
    }
    if case == "compact-explicit" {
        inference.instructions = Some("session instructions".into());
        inference.parameters.compaction =
            Some(stravia_runtime_contract::protocol::ir::CompactionControl {
                threshold_tokens: Some(4096),
                instructions: Some("explicit summary rules".into()),
            });
    }
    if case == "logprobs" {
        inference = stravia_protocol_codec::codec::open_responses::decoder::ResponsesDecoder
            .decode_request(serde_json::json!({
                "model":"logical-model", "input":"hello", "stream":true,
                "reasoning":{"effort":"high"}, "top_logprobs":0
            }))
            .unwrap();
    }
    inference.parameters.session.id = Some("injected-session".into());
    inference.parameters.session.request_id = Some("injected-request\r\ninvalid".into());
    inference.parameters.session.turn_id = Some("injected-turn".into());
    inference.parameters.session.parent_turn_id = Some("injected-parent".into());
    inference.parameters.session.root_turn_id = Some("injected-root".into());
    let input = if case.starts_with("compact") {
        OperationInput::Compact {
            provider,
            request: inference,
        }
    } else {
        OperationInput::Infer {
            provider,
            request: inference,
        }
    };
    let (operation, encoded) = input.encode_for_host().expect("encode inference");
    drop(input);
    let result = runtime
        .execute(&plugin, "codex", operation, encoded, scope)
        .await;
    server.await.expect("loopback upstream");
    if matches!(case, "error" | "compact-error") {
        assert!(
            matches!(
                result,
                Err(RuntimeError::Plugin {
                    upstream_status: Some(400),
                    ..
                })
            ),
            "{result:?}"
        );
        if case == "compact-error" {
            assert!(services.deltas.lock().iter().any(|delta| matches!(delta, AiStreamDelta::StreamError { error } if error.raw.as_ref() == Some(&serde_json::json!({"error":{"type":"invalid_request_error","message":"synthetic upstream rejection"}})))));
        }
        return;
    }
    if matches!(case, "compact" | "compact-explicit") {
        let OperationOutput::Compact(response) = result.expect("trigger compaction succeeds")
        else {
            panic!("expected compaction")
        };
        assert_eq!(response.wire["object"], "response");
        assert_eq!(response.wire["id"], "resp_cmp");
        assert_eq!(response.wire["created_at"], 1790129800);
        assert_eq!(
            stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(
                &response.items[0]
            ),
            Some(
                serde_json::json!({"type":"compaction","id":"cmp_1","encrypted_content":"synthetic-state"})
            )
        );
        assert_eq!(response.items.len(), 1);
        assert!(response.items[0].is_compaction());
        assert_eq!(response.usage.unwrap().total_tokens, 30);
        assert!(!services.deltas.lock().iter().any(|delta| matches!(
            delta,
            AiStreamDelta::TextDelta(_) | AiStreamDelta::TextDeltaWithMetadata { .. }
        )));
        return;
    }
    if matches!(case, "compact-duplicate" | "compact-truncated") {
        assert!(
            matches!(result, Err(RuntimeError::Plugin { .. })),
            "{result:?}"
        );
        assert!(
            !services
                .deltas
                .lock()
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
        );
        return;
    }
    if matches!(case, "malformed" | "truncated") {
        assert!(
            matches!(result, Err(RuntimeError::Plugin { .. })),
            "{result:?}"
        );
        assert!(
            !services
                .deltas
                .lock()
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
        );
        return;
    }
    let OperationOutput::Infer(response) = result.expect("SSE inference succeeds") else {
        panic!("expected inference")
    };
    assert_eq!(response.output_text(), "hello");
    assert_eq!(response.usage.prompt_tokens, 10);
    assert_eq!(response.usage.completion_tokens, 20);
    assert_eq!(response.usage.total_tokens, 30);
    assert_eq!(response.usage.reasoning_tokens, Some(12));
    assert_eq!(response.usage.cache_read_tokens, Some(0));
    assert!(response.usage.required_components_known);
    let calls = response.tool_calls().collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments, "{\"path\":\"synthetic.txt\"}");
    assert!(services.deltas.lock().iter().any(|delta| {
        let matches_call = |call: &stravia_runtime_contract::protocol::ir::ToolCall| {
            call.id == "call_1"
                && call.name == "read_file"
                && call.arguments == "{\"path\":\"synthetic.txt\"}"
        };
        match delta {
            AiStreamDelta::ToolCallComplete { tool_call, .. } => matches_call(tool_call),
            AiStreamDelta::ItemDone { item, .. } => {
                item.tool_calls.iter().flatten().any(matches_call)
            }
            _ => false,
        }
    }));
    assert!(services.deltas.lock().iter().any(|delta| matches!(delta, AiStreamDelta::TextDelta(text) | AiStreamDelta::TextDeltaWithMetadata { text, .. } if text == "hello")));
    assert!(
        services
            .deltas
            .lock()
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
    );
}

#[derive(Default)]
struct Recording {
    logs: Mutex<Vec<String>>,
    connections: Mutex<Vec<ConnectionRecord>>,
    sent: Mutex<Vec<serde_json::Value>>,
    closed: Mutex<Vec<Option<String>>>,
    /// Ordered "started" / "failed" / "completed" lifecycle events.
    events: Mutex<Vec<&'static str>>,
}

struct ConnectionRecord {
    headers: BTreeMap<String, String>,
    continuation_id: Option<String>,
    reuse_max_age_millis: Option<u64>,
    compression: Option<stravia_vendor_sdk::WsCompression>,
}

/// What one `ws_connect` does. The queue is consumed in connection order.
enum Connect {
    /// Local continuation socket miss: nothing is sent upstream.
    Miss,
    Frames(Vec<String>),
}

struct MockServices {
    connects: Mutex<std::collections::VecDeque<Connect>>,
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
            // Field order is wire behavior; only parse after checking the raw text.
            assert!(
                text.starts_with(r#"{"type":"response.create","model":"#),
                "{text}"
            );
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
        request: stravia_vendor_runtime::WsRequest,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        let continuation_requested = request.continuation_id.is_some();
        self.recorded.connections.lock().push(ConnectionRecord {
            headers: request.headers.into_iter().collect(),
            continuation_id: request.continuation_id,
            reuse_max_age_millis: request.reuse_max_age_millis,
            compression: request.compression,
        });
        let connect = self
            .connects
            .lock()
            .pop_front()
            .expect("unexpected extra WebSocket connection");
        let frames = match connect {
            Connect::Miss => {
                assert!(continuation_requested, "only a continuation can miss");
                return Err(HostFailure::new(
                    ErrorKind::ContinuationUnavailable,
                    "requested WebSocket continuation is not available locally",
                ));
            }
            Connect::Frames(frames) => frames,
        };
        Ok(Arc::new(MockSocket {
            frames: Mutex::new(
                frames
                    .into_iter()
                    .map(WebSocketMessage::Text)
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
        let label = match event {
            RuntimeEvent::UpstreamStarted => Some("started"),
            RuntimeEvent::Failed { .. } => Some("failed"),
            RuntimeEvent::Completed => Some("completed"),
            _ => None,
        };
        if let Some(label) = label {
            self.recorded.events.lock().push(label);
        }
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
            content: MessageContent::Text("你好".to_owned().into()),
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
    execute_scripted(
        runtime,
        plugin,
        provider,
        request,
        [Connect::Frames(frames)],
        recorded,
    )
    .await
}

async fn execute_scripted(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    provider: ProviderSnapshot,
    request: AiRequest,
    connects: impl IntoIterator<Item = Connect>,
    recorded: &Arc<Recording>,
) -> Result<OperationOutput, RuntimeError> {
    let services = Arc::new(MockServices {
        connects: Mutex::new(connects.into_iter().collect()),
        recorded: Arc::clone(recorded),
    });
    let scope = OperationScope::new(
        services,
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(300)),
        0,
    );
    let (operation, input) = OperationInput::Infer { provider, request }
        .encode_for_host()
        .map_err(|_| RuntimeError::InvalidOutput)?;
    runtime
        .execute(plugin, "codex", operation, input, scope)
        .await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn websocket_ignores_chat_logprobs_without_disabling_reasoning() {
    use stravia_protocol_codec::transform::ProtocolAdapter;
    let (runtime, plugin) = load_codex().await;
    let recorded = Arc::new(Recording::default());
    let inference = stravia_protocol_codec::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1
        .decode_request(serde_json::json!({
            "model":"logical-model", "messages":[{"role":"user","content":"hello"}],
            "reasoning_effort":"high", "logprobs":true, "top_logprobs":2
        }))
        .unwrap();
    let result = execute_once(
        &runtime,
        &plugin,
        codex_provider(),
        inference,
        codex_frames(0, "resp_without_logprobs"),
        &recorded,
    )
    .await
    .expect("Codex WebSocket inference");
    let OperationOutput::Infer(response) = result else {
        panic!("expected inference");
    };
    assert_eq!(response.output_text(), "你好，我是 ChatGPT。");
    assert_eq!(response.usage.reasoning_tokens, Some(12));
    let sent = recorded.sent.lock();
    assert_eq!(
        sent[0]["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    assert_eq!(sent[0]["reasoning"]["effort"], "high");
    assert!(sent[0].get("top_logprobs").is_none());
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
        turn_request.parameters.metadata = Some(BTreeMap::from([(
            "verification".into(),
            "canonical-final".into(),
        )]));
        turn_request.parameters.cache.key = Some("canonical-final-smoke".into());
        turn_request.parameters.cache.retention = Some(CacheRetention::Extended);
        turn_request.parallel_tool_calls = Some(false);
        turn_request.parameters.service_tier = tier.map(|tier| match tier {
            "priority" => ServiceTier::Priority,
            "flex" => ServiceTier::Flex,
            "auto" => ServiceTier::Auto,
            _ => ServiceTier::Default,
        });
        turn_request.parameters.continuation =
            previous_id.map(|id| Continuation::PreviousResponse(id.into()));
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
        assert_eq!(response.output_text(), "你好，我是 ChatGPT。");
    }

    let connections = recorded.connections.lock();
    let sent = recorded.sent.lock();
    let closed = recorded.closed.lock();
    assert_eq!(connections.len(), cases.len());
    assert_eq!(sent.len(), cases.len());
    assert_eq!(closed.len(), cases.len());
    for (index, (_, previous_id, _, response_id)) in cases.iter().enumerate() {
        assert!(matches!(
            &connections[index].compression,
            Some(stravia_vendor_sdk::WsCompression::PermessageDeflate(
                stravia_vendor_sdk::CompressionNegotiation::Preferred
            ))
        ));
        assert_eq!(connections[index].continuation_id.as_deref(), *previous_id);
        assert_eq!(connections[index].reuse_max_age_millis, Some(3_300_000));
        assert_eq!(sent[index]["previous_response_id"].as_str(), *previous_id);
        assert_eq!(sent[index]["type"], "response.create");
        assert!(sent[index].get("metadata").is_none());
        assert!(sent[index].get("prompt_cache_retention").is_none());
        assert_eq!(sent[index]["prompt_cache_key"], "canonical-final-smoke");
        assert_eq!(sent[index]["parallel_tool_calls"], false);
        assert_eq!(closed[index].as_deref(), Some(*response_id));
        let thread = &connections[index].headers["thread-id"];
        assert_eq!(uuid::Uuid::parse_str(thread).unwrap().get_version_num(), 7);
        assert_eq!(connections[index].headers["session-id"], *thread);
        assert_eq!(connections[index].headers["x-client-request-id"], *thread);
        assert_eq!(
            connections[index].headers["x-codex-window-id"],
            format!("{thread}:0")
        );
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
    for (frame, (tier, _, _, _)) in sent.iter().zip(cases.iter()) {
        assert_eq!(frame.get("service_tier").and_then(Value::as_str), *tier);
    }
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

/// Official 0.167.0 WS capture: Lite frames carry the HTTP-only Lite header
/// and the dispatch time as reserved client_metadata keys; a continuation
/// frame sends only the appended items, never the prompt prefix again.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn lite_websocket_frames_follow_the_official_capture() {
    let (runtime, plugin) = load_codex().await;
    let recorded = Arc::new(Recording::default());
    let mut provider = codex_provider();
    provider
        .operation_metadata
        .insert("transport_affinity".into(), "lite-window".into());
    provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
        extensions: BTreeMap::from([
            ("use_responses_lite".into(), serde_json::json!(true)),
            ("support_verbosity".into(), serde_json::json!(true)),
            ("default_verbosity".into(), serde_json::json!("low")),
        ]),
        ..Default::default()
    });
    for previous_id in [None, Some("resp_lite_first")] {
        let mut turn_request = request();
        turn_request.instructions = Some("base instructions".into());
        turn_request.parameters.continuation =
            previous_id.map(|id| Continuation::PreviousResponse(id.into()));
        execute_once(
            &runtime,
            &plugin,
            provider.clone(),
            turn_request,
            codex_frames(0, "resp_lite"),
            &recorded,
        )
        .await
        .expect("Codex Lite WebSocket inference");
    }
    let connections = recorded.connections.lock();
    let sent = recorded.sent.lock();
    for (connection, frame) in connections.iter().zip(sent.iter()) {
        for absent in [
            "x-codex-turn-metadata",
            "x-openai-internal-codex-responses-lite",
            "x-codex-installation-id",
        ] {
            assert!(!connection.headers.contains_key(absent), "{absent}");
        }
        assert_eq!(
            connection.headers["x-codex-beta-features"],
            "remote_compaction_v2"
        );
        assert_eq!(connection.headers["originator"], "codex-tui");
        let metadata = &frame["client_metadata"];
        assert_eq!(
            metadata["ws_request_header_x_openai_internal_codex_responses_lite"],
            "true"
        );
        assert!(
            metadata["x-codex-ws-stream-request-start-ms"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .is_ok()
        );
        assert!(frame.get("instructions").is_none());
        assert!(frame.get("tools").is_none());
        assert_eq!(frame["parallel_tool_calls"], false);
        assert_eq!(frame["reasoning"]["context"], "all_turns");
        assert_eq!(frame["text"]["verbosity"], "low");
        assert_eq!(
            frame["access_programs"],
            serde_json::json!({"cyber":"standard"})
        );
    }
    let roles = |frame: &serde_json::Value| {
        frame["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["role"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(roles(&sent[0]), ["developer", "user"]);
    assert_eq!(
        sent[0]["input"][0]["content"][0]["text"],
        "base instructions"
    );
    assert_eq!(sent[1]["previous_response_id"], "resp_lite_first");
    assert_eq!(roles(&sent[1]), ["user"]);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn continuation_rejection_is_classified_only_for_a_requested_unstarted_attempt() {
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
    for (previous_id, saw_created, continuation_lost) in [
        (None, false, false),
        (Some("resp_missing"), true, false),
        (Some("resp_missing"), false, true),
    ] {
        let mut turn_request = request();
        turn_request.parameters.continuation =
            previous_id.map(|id| Continuation::PreviousResponse(id.into()));
        let mut frames = Vec::new();
        if saw_created {
            frames.push(created.clone());
        }
        frames.push(rejection.clone());
        let recorded = Arc::new(Recording::default());
        let result = execute_scripted(
            &runtime,
            &plugin,
            codex_provider(),
            turn_request,
            [Connect::Frames(frames)],
            &recorded,
        )
        .await;
        // 插件只发送一次并把精确事实交给宿主，不自行重连或重发。
        assert_eq!(recorded.connections.lock().len(), 1);
        assert_eq!(
            recorded.connections.lock()[0].continuation_id.as_deref(),
            previous_id
        );
        assert_eq!(
            recorded.sent.lock()[0]["previous_response_id"].as_str(),
            previous_id
        );
        if continuation_lost {
            assert!(
                matches!(
                    result,
                    Err(RuntimeError::Plugin {
                        kind: ErrorKind::ContinuationNotFound,
                        upstream_status: Some(400),
                        ..
                    })
                ),
                "unexpected classification for previous_id={previous_id:?}, saw_created={saw_created}: {result:?}"
            );
        } else {
            assert!(
                matches!(
                    result,
                    Err(RuntimeError::Plugin {
                        kind: ErrorKind::Upstream(_),
                        ..
                    })
                ),
                "unexpected classification for previous_id={previous_id:?}, saw_created={saw_created}: {result:?}"
            );
        }
    }
}

fn user_item(text: &str) -> AiItem {
    AiItem {
        role: Role::User,
        content: MessageContent::Text(text.to_owned().into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

/// Request settings that must reach the single model dispatch unchanged.
fn with_continuation_settings(mut request: AiRequest) -> AiRequest {
    request.parallel_tool_calls = Some(false);
    request.parameters.cache.key = Some("replay-cache-key".into());
    request.parameters.service_tier = Some(ServiceTier::Priority);
    request.parameters.continuation = Some(Continuation::PreviousResponse("resp_missing".into()));
    request
}

fn continuation_rejection() -> String {
    serde_json::json!({
        "type": "error", "status": 400,
        "error": {"type": "invalid_request_error", "message": "Invalid `previous_response_id`."}
    })
    .to_string()
}

fn input_texts(frame: &serde_json::Value) -> Vec<String> {
    frame["input"]
        .as_array()
        .expect("full-history input array")
        .iter()
        .map(|item| item.to_string())
        .collect()
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn local_socket_miss_reports_continuation_unavailable_before_starting() {
    let (runtime, plugin) = load_codex().await;
    let recorded = Arc::new(Recording::default());
    let mut provider = codex_provider();
    provider
        .operation_metadata
        .insert("transport_affinity".into(), "replay-window".into());
    let delta = with_continuation_settings(AiRequest::new("gpt-6-astra", vec![user_item("delta")]));
    let result = execute_scripted(
        &runtime,
        &plugin,
        provider,
        delta,
        [Connect::Miss],
        &recorded,
    )
    .await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Plugin {
                kind: ErrorKind::ContinuationUnavailable,
                upstream_status: None,
                ..
            })
        ),
        "{result:?}"
    );
    // The local miss sent no frame and never declared the model boundary; Core
    // may now authorize one full-history dispatch on a fresh plugin operation.
    assert!(recorded.events.lock().is_empty());
    assert!(recorded.sent.lock().is_empty());
    let connections = recorded.connections.lock();
    assert_eq!(connections.len(), 1);
    assert_eq!(
        connections[0].continuation_id.as_deref(),
        Some("resp_missing")
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn exact_remote_rejection_propagates_continuation_not_found_without_failed_event() {
    let (runtime, plugin) = load_codex().await;
    let recorded = Arc::new(Recording::default());
    let full = with_continuation_settings(AiRequest::new(
        "gpt-6-astra",
        vec![user_item("history"), user_item("delta")],
    ));
    let result = execute_scripted(
        &runtime,
        &plugin,
        codex_provider(),
        full,
        [Connect::Frames(vec![continuation_rejection()])],
        &recorded,
    )
    .await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Plugin {
                kind: ErrorKind::ContinuationNotFound,
                upstream_status: Some(400),
                ..
            })
        ),
        "{result:?}"
    );
    // The exact unstarted rejection is a pre-execution fact, not a failed model
    // event; the plugin makes no second connection or frame.
    assert_eq!(*recorded.events.lock(), vec!["started"]);
    assert_eq!(recorded.connections.lock().len(), 1);
    let sent = recorded.sent.lock();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["previous_response_id"], "resp_missing");
    assert_eq!(input_texts(&sent[0]).len(), 2);
    assert_eq!(sent[0]["parallel_tool_calls"], false);
    assert_eq!(sent[0]["prompt_cache_key"], "replay-cache-key");
    assert_eq!(sent[0]["service_tier"], "priority");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn inline_compaction_keeps_exact_stale_id_error_and_other_errors() {
    let (runtime, plugin) = load_codex().await;
    // An inline trigger makes the plugin preserve upstream errors.
    let compacting = || {
        let mut request = with_continuation_settings(AiRequest::new(
            "gpt-6-astra",
            vec![user_item("history"), user_item("delta")],
        ));
        request.items.push(AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                stravia_runtime_contract::protocol::ir::ContentBlock::CompactionTrigger {},
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        });
        request
    };

    let recorded = Arc::new(Recording::default());
    let result = execute_scripted(
        &runtime,
        &plugin,
        codex_provider(),
        compacting(),
        [Connect::Frames(vec![continuation_rejection()])],
        &recorded,
    )
    .await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Plugin {
                kind: ErrorKind::ContinuationNotFound,
                upstream_status: Some(400),
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(recorded.connections.lock().len(), 1);
    {
        let sent = recorded.sent.lock();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].get("previous_response_id").is_some());
        assert!(
            input_texts(&sent[0])
                .iter()
                .any(|item| item.contains("compaction_trigger"))
        );
    }

    // Any other upstream rejection is still delivered, never reclassified.
    let other = serde_json::json!({
        "type": "error", "status": 400,
        "error": {"type": "invalid_request_error", "message": "Invalid `input`: unsupported content"}
    })
    .to_string();
    let recorded = Arc::new(Recording::default());
    let result = execute_scripted(
        &runtime,
        &plugin,
        codex_provider(),
        compacting(),
        [Connect::Frames(vec![other])],
        &recorded,
    )
    .await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Plugin {
                kind: ErrorKind::Upstream(_),
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(recorded.connections.lock().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn http_only_continuation_delta_is_rejected_before_started_or_send() {
    let (runtime, plugin) = load_codex().await;
    let recorded = Arc::new(Recording::default());
    let mut provider = codex_provider();
    provider
        .operation_metadata
        .insert("transport_preference".into(), "http-only".into());
    provider
        .operation_metadata
        .insert("transport_affinity".into(), "http-window".into());
    // The host may have retained full history; the plugin must not silently
    // drop the reference and send the remaining history as a truncated delta.
    let delta = with_continuation_settings(AiRequest::new(
        "gpt-6-astra",
        vec![user_item("history"), user_item("delta")],
    ));
    let result = execute_scripted(
        &runtime,
        &plugin,
        provider,
        delta,
        Vec::<Connect>::new(),
        &recorded,
    )
    .await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Plugin {
                kind: ErrorKind::ContinuationUnavailable,
                upstream_status: None,
                ..
            })
        ),
        "{result:?}"
    );
    assert!(recorded.events.lock().is_empty());
    assert!(recorded.connections.lock().is_empty());
    assert!(recorded.sent.lock().is_empty());
}
