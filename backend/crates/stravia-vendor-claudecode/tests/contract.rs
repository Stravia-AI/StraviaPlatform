//! 非忽略的 canonical/codec 合同与显式 opt-in 的真实 Component 回归：
//! 在 HostServices stub 上验证
//! Claude Code 线上形态（计费关联、工具名前缀往返、多轮线程与请求 ID 生命周期）与
//! OAuth 授权码交换 / 身份补全。
//!
//! 运行：`task build:vendors:all` 后执行
//! `cargo test -p stravia-vendor-claudecode --test contract -- --ignored`。

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::protocol::ir::{
    AiRequest, AiStreamDelta, NativeTool, ReasoningSummary,
};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::net::{ConnectRequest, Field, Options, Request as HttpRequest};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, DiscoverRequest, ErrorKind, OperationInput,
    OperationOutput, ProviderSnapshot,
};

const UPSTREAM: &str = "https://api.anthropic.com";
const CHANNEL: &str = "oauth";
const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-0000000000aa";
const DEVICE_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn artifact() -> std::path::PathBuf {
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
    let manifest: Value = serde_json::from_slice(&bytes).unwrap();
    let entry = manifest
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["vendor_id"] == "claude-code")
        })
        .expect("claude-code missing from vendor manifest; run task build:vendors:all");
    dir.join(entry["file"].as_str().unwrap())
}

struct MockResponse {
    status: u16,
    content_type: &'static str,
    request_id: Option<String>,
    body: Mutex<Option<Vec<u8>>>,
}

#[async_trait]
impl HostHttpResponse for MockResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.status)
    }

    async fn headers(&self) -> Result<Vec<Field>, HostFailure> {
        let mut headers = vec![Field { name: "content-type".into(), bytes: self.content_type.as_bytes().to_vec() }];
        if let Some(id) = &self.request_id {
            headers.push(Field { name: "request-id".into(), bytes: id.as_bytes().to_vec() });
        }
        Ok(headers)
    }

    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.body.lock().take())
    }
}

type ScriptedReply = (u16, &'static str, String, Option<String>);

/// 按 URL 前缀路由的上游桩；未匹配的请求以 404 返回，便于断言调用序列。
#[derive(Default)]
struct MockServices {
    routes: Vec<(&'static str, u16, &'static str, String)>,
    script: Mutex<VecDeque<ScriptedReply>>,
    requests: Mutex<Vec<HttpRequest>>,
    private_state: Mutex<Option<Vec<u8>>>,
    deltas: Mutex<Vec<AiStreamDelta>>,
    reject_assistant_prefill: bool,
    /// 宿主可见的有序事件：`start`、`request`、`failed`。
    wire_log: Mutex<Vec<&'static str>>,
}

#[async_trait]
impl HostServices for MockServices {
    fn validate_net_option(&self, key: &str, value: &str) -> Result<(), stravia_vendor_sdk::net::OptionError> {
        stravia_vendor_sdk::net::validate_host_option(key, value)
    }

    fn http_start(&self, request: HttpRequest, _options: Options) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        self.wire_log.lock().push("request");
        if self.reject_assistant_prefill {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .is_some_and(|message| message["role"] == "assistant")
            {
                self.requests.lock().push(request);
                return Ok(Arc::new(MockResponse {
                    status: 400,
                    content_type: "application/json",
                    request_id: None,
                    body: Mutex::new(Some(
                        json!({
                            "type": "error",
                            "error": {
                                "type": "invalid_request_error",
                                "message": "This model does not support assistant message prefill. The conversation must end with a user message."
                            }
                        })
                        .to_string()
                        .into_bytes(),
                    )),
                }));
            }
        }
        if let Some((status, content_type, body, request_id)) = self.script.lock().pop_front() {
            self.requests.lock().push(request);
            return Ok(Arc::new(MockResponse {
                status,
                content_type,
                request_id,
                body: Mutex::new(Some(body.into_bytes())),
            }));
        }
        let route = self
            .routes
            .iter()
            .find(|(prefix, ..)| request.url.starts_with(prefix));
        self.requests.lock().push(request);
        let (status, content_type, body) = route
            .map(|(_, status, content_type, body)| (*status, *content_type, body.clone()))
            .unwrap_or((404, "application/json", "{}".into()));
        Ok(Arc::new(MockResponse {
            status,
            content_type,
            request_id: None,
            body: Mutex::new(Some(body.into_bytes())),
        }))
    }

    async fn ws_connect(
        &self,
        _request: ConnectRequest,
        _options: Options,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "no websocket upstream",
        ))
    }

    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.private_state.lock().clone())
    }

    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        *self.private_state.lock() = Some(bytes);
        Ok(())
    }

    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure> {
        match event {
            RuntimeEvent::UpstreamStarted => self.wire_log.lock().push("start"),
            RuntimeEvent::Failed { .. } => self.wire_log.lock().push("failed"),
            RuntimeEvent::Delta(delta) => self.deltas.lock().push(delta),
            RuntimeEvent::Completed | RuntimeEvent::Compacted => {}
        }
        Ok(())
    }

    fn log(&self, _level: LogLevel, _message: &str) {}

    fn generation_is_current(&self, _generation: u64) -> bool {
        true
    }
}

const SSE_TOOL_USE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"_read\",\"input\":{}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\\\"}\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn signed_compaction_block() -> Value {
    json!({
        "type": "compaction",
        "content": "Signed summary of the work",
        "signature": "sig-stream",
        "tool_changes": [{"name": "read", "opaque": ["unchanged"]}],
        "opaque_future": {"keep": true}
    })
}

fn native_compaction_stream(stop_reason: &str, usage: Value) -> String {
    [
        sse(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_compact",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-sonnet-4-5",
                    "content": [],
                    "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        ),
        sse(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": signed_compaction_block()
            }),
        ),
        sse(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        sse(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage
            }),
        ),
        sse("message_stop", json!({"type": "message_stop"})),
    ]
    .concat()
}

fn native_compaction_usage() -> Value {
    json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "iterations": [
            {
                "type": "compaction",
                "input_tokens": 64,
                "output_tokens": 2002,
                "cache_creation_input_tokens": 80082
            },
            {"type": "message", "input_tokens": 5, "output_tokens": 1}
        ]
    })
}

/// 以 Anthropic 客户端入口构造独立原生压缩请求，摘要控制与触发器进入 IR。
fn native_compaction_request() -> AiRequest {
    anthropic_client_request(json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 4096,
        "system": [{"type": "text", "text": "stable system"}],
        "tools": [{"name": "read", "input_schema": {"type": "object"}}],
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [{"role": "user", "content": "history"}],
        "compaction": {"type": "summarize", "instructions": "Only file names"}
    }))
}

fn provider(credentials: BTreeMap<String, Value>) -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "claude-code".into(),
        channel: CHANNEL.into(),
        base_url: UPSTREAM.into(),
        protocol: "anthropic-messages".into(),
        options: BTreeMap::new(),
        credentials,
        model: Some("claude-sonnet-4-5".into()),
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    }
}

fn signed_in() -> ProviderSnapshot {
    provider(BTreeMap::from([
        ("access_token".into(), json!("sk-ant-oat01-test")),
        ("account_uuid".into(), json!(ACCOUNT_UUID)),
        ("device_id".into(), json!(DEVICE_ID)),
    ]))
}

/// 以 Anthropic 客户端入口解码请求，与服务端收到 `/v1/messages` 时的路径一致。
fn anthropic_client_request(body: Value) -> AiRequest {
    let endpoint =
        stravia_vendor_common::common::endpoint("anthropic-messages/messages/2023-06-01")
            .expect("anthropic endpoint is registered");
    ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .expect("anthropic transform binds")
        .decode_request(body)
        .expect("client request decodes")
}

#[test]
fn unified_tools_and_summary_cross_the_anthropic_codec_boundary() {
    let request = anthropic_client_request(json!({
        "model":"claude-sonnet-4-5",
        "max_tokens":4096,
        "thinking":{"type":"enabled","budget_tokens":1024,"display":"omitted"},
        "tools":[
            {"name":"read","input_schema":{"type":"object","properties":{
                "cache_control":{"type":"string"}}},
             "strict":true,"defer_loading":false,"eager_input_streaming":true,
             "cache_control":{"type":"ephemeral","ttl":"5m"},
             "allowed_callers":["direct"],"input_examples":[{}],"citations":{"enabled":true}},
            {"type":"web_search_20250305","name":"web_search","max_uses":8,
             "allowed_domains":["example.com"],"user_location":{"type":"approximate","city":"Paris"},
             "search_profile":"client-profile"}
        ],
        "metadata":{"user_id":"client-identity"},
        "messages":[{"role":"user","content":"Read a."}]
    }));
    assert_eq!(request.reasoning.summary, Some(ReasoningSummary::None));
    let tools = request.tools.as_ref().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].strict, Some(true));
    assert_eq!(tools[0].defer_loading, Some(false));
    assert_eq!(
        tools[0].parameters["properties"]["cache_control"]["type"],
        "string"
    );
    let [NativeTool::WebSearch(search)] = request.parameters.native_tools.as_slice() else {
        panic!("expected unified native web search");
    };
    assert_eq!(search.max_uses, Some(8));
    assert_eq!(search.allowed_domains, ["example.com"]);
    assert_eq!(
        search.user_location.as_ref().unwrap().city.as_deref(),
        Some("Paris")
    );
    let encoded = stravia_vendor_common::common::encode_inference_request(
        "anthropic-messages/messages/2023-06-01",
        request,
    )
    .unwrap();
    assert_eq!(encoded.body["tools"][0]["name"], "read");
    assert_eq!(encoded.body["tools"][1]["type"], "web_search_20250305");
    assert_eq!(encoded.body["tools"][1]["name"], "web_search");
    for tool in encoded.body["tools"].as_array().unwrap() {
        for field in [
            "eager_input_streaming",
            "cache_control",
            "allowed_callers",
            "input_examples",
            "citations",
            "search_profile",
        ] {
            assert!(tool.get(field).is_none(), "{field}");
        }
    }
}

async fn load_plugin() -> (VendorRuntime, LoadedPlugin) {
    let bytes = std::fs::read(artifact()).unwrap();
    let runtime = VendorRuntime::new().expect("runtime");
    let plugin = runtime.load(&bytes).await.expect("component loads");
    (runtime, plugin)
}

async fn run(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    services: &Arc<MockServices>,
    input: OperationInput,
) -> Result<OperationOutput, RuntimeError> {
    let scope = OperationScope::new(
        Arc::clone(services) as Arc<dyn HostServices>,
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(60)),
        0,
    );
    let (operation, encoded) = input
        .encode_for_host()
        .map_err(|_| RuntimeError::InvalidOutput)?;
    drop(input);
    runtime
        .execute(plugin, CHANNEL, operation, encoded, scope)
        .await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn native_compaction_stream_keeps_signed_state_and_iteration_usage() {
    let (runtime, plugin) = load_plugin().await;
    let services = Arc::new(MockServices {
        routes: vec![(
            UPSTREAM,
            200,
            "text/event-stream",
            native_compaction_stream("compaction", native_compaction_usage()),
        )],
        ..MockServices::default()
    });
    let output = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Compact {
            provider: signed_in(),
            request: native_compaction_request(),
        },
    )
    .await
    .expect("native compaction stream is accepted");
    {
        let requests = services.requests.lock();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["stream"],
            json!(true),
            "native compaction must request an SSE envelope"
        );
        assert_eq!(
            body["compaction"],
            json!({"type": "summarize", "instructions": "Only file names"})
        );
        assert!(body.get("thread").is_none() && body.get("diagnostics").is_none());
        assert_eq!(body["system"].as_array().unwrap().len(), 3);
        assert_eq!(body["tools"][0]["name"], "_read");
        assert_eq!(body["thinking"]["type"], "enabled");
        let betas = wire_header(&requests[0], "anthropic-beta");
        let betas: Vec<&str> = betas.split(',').map(str::trim).collect();
        assert!(
            betas.contains(&"compact-2026-09-04"),
            "{}",
            wire_header(&requests[0], "anthropic-beta")
        );
        assert!(!betas.contains(&"message-threads-2026-08-12"));
    }
    let OperationOutput::Compact(response) = output else {
        panic!("compact operation must not become a generation response");
    };
    assert_eq!(response.wire["stop_reason"], "compaction");
    assert_eq!(
        response.wire["content"],
        json!([signed_compaction_block()]),
        "signature and unknown block fields must survive the SSE fold"
    );
    assert_eq!(
        response.wire["usage"]["iterations"],
        native_compaction_usage()["iterations"]
    );
    let usage = response.usage.expect("compaction usage");
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (64, 2002));
    assert!(response.items[0].is_compaction());

    let mut request = native_compaction_request();
    request.instructions = None;
    request.parameters.compaction.as_mut().unwrap().instructions = None;
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Compact {
            provider: signed_in(),
            request,
        },
    )
    .await
    .expect("omitted instructions use the local summary prompt");
    let requests = services.requests.lock();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let default = stravia_protocol_codec::codec::compaction::DEFAULT_COMPACTION_INSTRUCTIONS;
    assert_eq!(body["compaction"]["instructions"], default);
    assert!(
        body["system"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block.get("text").and_then(Value::as_str) == Some(default))
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn native_compaction_stream_truncation_and_errors_are_not_summaries() {
    let (runtime, plugin) = load_plugin().await;
    let complete = native_compaction_stream("compaction", native_compaction_usage());
    let truncated = complete
        .split("event: message_stop")
        .next()
        .expect("fixture has a message_stop")
        .to_owned();
    for (body, expected) in [
        (truncated.clone(), "message_stop"),
        (
            format!(
                "{truncated}{}",
                sse(
                    "error",
                    json!({
                        "type": "error",
                        "error": {"type": "overloaded_error", "message": "Overloaded"}
                    }),
                ),
            ),
            "Overloaded",
        ),
        (
            native_compaction_stream("end_turn", native_compaction_usage()),
            "end_turn",
        ),
        (
            native_compaction_stream("refusal", native_compaction_usage()),
            "refusal",
        ),
    ] {
        let services = Arc::new(MockServices {
            routes: vec![(UPSTREAM, 200, "text/event-stream", body)],
            ..MockServices::default()
        });
        let error = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Compact {
                provider: signed_in(),
                request: native_compaction_request(),
            },
        )
        .await
        .expect_err("an incomplete compaction stream cannot succeed");
        let diagnostic = match error {
            RuntimeError::Plugin {
                diagnostic_message, ..
            } => diagnostic_message,
            other => panic!("expected a plugin upstream error, got {other:?}"),
        };
        assert!(
            diagnostic.contains(expected),
            "expected {expected:?} in {diagnostic:?}"
        );
        if expected == "Overloaded" {
            let deltas = services.deltas.lock();
            let error = deltas
                .iter()
                .find_map(|delta| match delta {
                    AiStreamDelta::StreamError { error } => Some(error),
                    _ => None,
                })
                .expect("the upstream SSE error must remain available for client relay");
            assert_eq!(
                error.raw,
                Some(json!({
                    "type": "error",
                    "error": {"type": "overloaded_error", "message": "Overloaded"}
                }))
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn native_compaction_replay_stops_using_an_existing_thread() {
    let (runtime, plugin) = load_plugin().await;
    let services = Arc::new(MockServices {
        script: Mutex::new(VecDeque::from([
            (
                200,
                "application/json",
                message_reply(
                    "msg_011111111111111111111110",
                    json!([{"type":"text","text":"Ready"}]),
                    "end_turn",
                ),
                Some("req_before_compaction".into()),
            ),
            (
                200,
                "application/json",
                message_reply(
                    "msg_011111111111111111111111",
                    json!([{"type":"text","text":"Still ready"}]),
                    "end_turn",
                ),
                Some("req_thread_continuation".into()),
            ),
            (
                200,
                "application/json",
                message_reply(
                    "msg_011111111111111111111112",
                    json!([{"type":"text","text":"Continued"}]),
                    "end_turn",
                ),
                Some("req_after_compaction".into()),
            ),
        ])),
        ..MockServices::default()
    });
    let mut provider = signed_in();
    provider.operation_metadata.insert(
        "transport_affinity".into(),
        json!("native-compaction-replay"),
    );
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: threaded_request(&[json!({"role":"user","content":"Remember the task."})]),
        },
    )
    .await
    .expect("the original turn establishes a message thread");
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: threaded_request(&[
                json!({"role":"user","content":"Remember the task."}),
                json!({"role":"assistant","content":[{"type":"text","text":"Ready"}]}),
                json!({"role":"user","content":"Keep going."}),
            ]),
        },
    )
    .await
    .expect("ordinary history continues the established thread");

    let block = signed_compaction_block();
    let request = anthropic_client_request(json!({
        "model":"claude-sonnet-4-5",
        "max_tokens":4096,
        "thinking":{"type":"enabled","budget_tokens":1024},
        "tools":[{"name":"read","input_schema":{"type":"object"}}],
        "messages":[
            {"role":"assistant","content":[block.clone()]},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"Read the retained file.","signature":"sig-retained-thinking"},
                {"type":"tool_use","id":"toolu_retained","name":"read","input":{"path":"a"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_retained","content":"a contents"}
            ]}
        ]
    }));
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer { provider, request },
    )
    .await
    .expect("the compacted tool continuation succeeds without thread recovery");

    let requests = services.requests.lock();
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(first["thread"]["type"], "create");
    let ordinary: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(ordinary["thread"]["type"], "continue");
    let replay: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert!(replay.get("thread").is_none() && replay.get("diagnostics").is_none());
    assert!(!wire_header(&requests[2], "anthropic-beta").contains("message-threads"));
    assert_eq!(replay["messages"].as_array().unwrap().len(), 2);
    let content = replay["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0], block);
    assert_eq!(content[1]["signature"], "sig-retained-thinking");
    assert_eq!(content[2]["id"], "toolu_retained");
    assert_eq!(
        replay["messages"][1]["content"][0]["tool_use_id"],
        "toolu_retained"
    );
    assert_eq!(
        *services.wire_log.lock(),
        ["start", "request", "start", "request", "start", "request"]
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn responses_tail_prefill_is_dropped_without_losing_completed_tool_history() {
    let (runtime, plugin) = load_plugin().await;
    let services = Arc::new(MockServices {
        routes: vec![(UPSTREAM, 200, "text/event-stream", SSE_TOOL_USE.into())],
        reject_assistant_prefill: true,
        ..MockServices::default()
    });
    let endpoint = stravia_vendor_common::common::endpoint("open-responses/responses/2026-04-24")
        .expect("Responses endpoint is registered");
    let request = ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .decode_request(json!({
            "model": "claude-sonnet-5-5",
            "input": [
                {"role": "user", "content": "read the file"},
                {"type": "function_call", "call_id": "call_read", "name": "read", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call_output", "call_id": "call_read", "output": "retained tool result"},
                {"role": "assistant", "content": "unsupported response prefix"}
            ]
        }))
        .unwrap();
    let mut provider = signed_in();
    provider.model = Some("claude-sonnet-5-5".into());
    let output = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer { provider, request },
    )
    .await
    .expect("a model without prefill accepts the converted tool continuation");
    let requests = services.requests.lock();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"][0]["text"], "read the file");
    assert_eq!(messages[1]["content"][0]["id"], "call_read");
    assert_eq!(messages[1]["content"][0]["name"], "_read");
    assert_eq!(messages[1]["content"][0]["input"], json!({"path": "a"}));
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_read");
    assert_eq!(messages[2]["content"][0]["content"], "retained tool result");
    let OperationOutput::Infer(response) = output else {
        panic!("expected parsed Infer output");
    };
    assert_eq!(response.id, "msg_1");
    assert_eq!(response.tool_calls().next().unwrap().name, "read");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn responses_auto_truncation_sends_complete_tool_continuation_history() {
    let (runtime, plugin) = load_plugin().await;
    let services = Arc::new(MockServices {
        routes: vec![(UPSTREAM, 200, "text/event-stream", SSE_TOOL_USE.into())],
        ..MockServices::default()
    });
    let endpoint = stravia_vendor_common::common::endpoint("open-responses/responses/2026-04-24")
        .expect("Responses endpoint is registered");
    let request = ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .expect("Responses transform binds")
        .decode_request(json!({
            "model": "claude-sonnet-4-5",
            "truncation": "auto",
            "max_output_tokens": 128,
            "tools": [{"type": "function", "name": "read", "description": "Read a file", "parameters": {"type": "object"}}],
            "input": [
                {"role": "user", "content": "retain the first turn"},
                {"type": "function_call", "call_id": "call_read", "name": "read", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call_output", "call_id": "call_read", "output": "retained tool result"},
                {"role": "user", "content": "continue with all history"}
            ]
        }))
        .expect("Responses client request decodes");
    let output = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: signed_in(),
            request,
        },
    )
    .await
    .expect("unsupported truncation is omitted and complete tool history reaches Anthropic");

    let requests = services.requests.lock();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://api.anthropic.com/v1/messages?beta=true"
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let messages = body["messages"]
        .as_array()
        .expect("complete Anthropic history");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"].as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["content"][0]["type"], "text");
    assert_eq!(messages[0]["content"][0]["text"], "retain the first turn");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"].as_array().unwrap().len(), 1);
    let call = &messages[1]["content"][0];
    assert_eq!(call["type"], "tool_use");
    assert_eq!(call["id"], "call_read");
    assert_eq!(call["name"], "_read");
    assert_eq!(call["input"], json!({"path": "a"}));
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
    let result = &messages[2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "call_read");
    assert_eq!(result["content"], "retained tool result");
    assert_eq!(messages[2]["content"][1]["type"], "text");
    assert_eq!(
        messages[2]["content"][1]["text"],
        "continue with all history"
    );

    let OperationOutput::Infer(response) = output else {
        panic!("expected parsed Infer output");
    };
    assert_eq!(response.id, "msg_1");
    let calls = response.tool_calls().collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].name, "read");
    assert_eq!(calls[0].arguments, "{\"path\":\"a\"}");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn always_on_default_summary_preserves_explicit_omission_and_effort() {
    let (runtime, plugin) = load_plugin().await;
    for (summary, expected) in [
        (None, "updates"),
        (Some(ReasoningSummary::None), "omitted"),
        (Some(ReasoningSummary::Auto), "summarized"),
        (Some(ReasoningSummary::Concise), "summarized"),
        (Some(ReasoningSummary::Detailed), "summarized"),
    ] {
        let services = Arc::new(MockServices {
            routes: vec![(UPSTREAM, 200, "text/event-stream", SSE_TOOL_USE.into())],
            ..MockServices::default()
        });
        let mut request = anthropic_client_request(json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Read a."}]
        }));
        request.reasoning.summary = summary;
        let mut provider = signed_in();
        provider.model = Some("claude-opus-5-5".into());
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer { provider, request },
        )
        .await
        .expect("implicit always-on inference succeeds");
        let requests = services.requests.lock();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], expected);
        assert!(body.get("output_config").is_none());
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn required_output_limit_prefers_explicit_then_discovered_then_fallback() {
    let (runtime, plugin) = load_plugin().await;
    // 发现数据里的原生 `max_tokens` 是该型号的输出上限，进入模型元数据扩展；
    // 已退休的 `limit.output` 不恢复。
    let discovery = Arc::new(MockServices {
        routes: vec![(
            "https://api.anthropic.com/v1/models",
            200,
            "application/json",
            json!({
                "data": [{
                    "id": "claude-sonnet-4-5",
                    "display_name": "Claude Sonnet 4.5",
                    "max_input_tokens": 200_000,
                    "max_tokens": 32_000,
                    "capabilities": {
                        "thinking": {"supported": true, "types": {
                            "adaptive": {"supported": false},
                            "enabled": {"supported": true}}},
                        "effort": {"supported": false}
                    }
                }]
            })
            .to_string(),
        )],
        ..MockServices::default()
    });
    let OperationOutput::Discover(catalog) = run(
        &runtime,
        &plugin,
        &discovery,
        OperationInput::Discover {
            provider: signed_in(),
            request: DiscoverRequest { cursor: None },
        },
    )
    .await
    .expect("model discovery succeeds") else {
        panic!("expected Discover output");
    };
    let discovered = catalog
        .models
        .iter()
        .find(|model| model.id == "claude-sonnet-4-5")
        .expect("discovered model");
    assert_eq!(discovered.metadata["max_tokens"], 32_000);
    assert!(discovered.metadata["limit"].get("output").is_none());
    let discovered_extensions = discovered.metadata.clone();
    let invalid_extensions = BTreeMap::from([("max_tokens".into(), json!(0))]);

    for (explicit, requested_budget, extensions, expected_limit, expected_budget) in [
        // 显式上限优先于发现数据；已足够时原样发送。
        (
            Some(24_000),
            16_000,
            Some(&discovered_extensions),
            24_000,
            16_000,
        ),
        // 无显式上限：使用发现到的型号上限。
        (None, 16_000, Some(&discovered_extensions), 32_000, 16_000),
        // 自动补出的型号上限不被思考预算抬高，预算收缩到上限内。
        (
            None,
            63_000,
            Some(&discovered_extensions),
            32_000,
            32_000 - 4_000,
        ),
        // 发现数据缺失或不可信：编码器必填回退 64,000。
        (None, 16_000, None, 64_000, 16_000),
        // 编码器的必填回退同样不被预算抬高。
        (None, 63_000, None, 64_000, 64_000 - 4_000),
        (None, 16_000, Some(&invalid_extensions), 64_000, 16_000),
    ] {
        let services = Arc::new(MockServices {
            routes: vec![(UPSTREAM, 200, "text/event-stream", SSE_TOOL_USE.into())],
            ..MockServices::default()
        });
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "thinking": {"type": "enabled", "budget_tokens": requested_budget},
            "messages": [{"role": "user", "content": "Read a."}]
        });
        if let Some(explicit) = explicit {
            body["max_tokens"] = json!(explicit);
        }
        let request = anthropic_client_request(body);
        let mut provider = signed_in();
        provider.model_metadata = extensions.map(|extensions| stravia_vendor_sdk::ModelMetadata {
            extensions: extensions.clone(),
            ..Default::default()
        });
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer { provider, request },
        )
        .await
        .expect("inference with the required output limit succeeds");

        let requests = services.requests.lock();
        assert_eq!(requests.len(), 1);
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            sent["thinking"],
            json!({"type": "enabled", "budget_tokens": expected_budget})
        );
        assert_eq!(
            sent.get("max_tokens").and_then(Value::as_u64),
            Some(expected_limit)
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn declared_future_native_effort_survives_claude_code_normalization() {
    let (runtime, plugin) = load_plugin().await;
    for (declared, requested, expected, raw_capabilities) in [
        (
            Some(json!(["future-native"])),
            "future-native",
            Some("future-native"),
            true,
        ),
        (Some(json!([])), "high", None, true),
        (None, "high", Some("high"), true),
        (None, "future-native", None, true),
        (
            Some(json!(["future-native"])),
            "future-native",
            Some("future-native"),
            false,
        ),
    ] {
        let services = Arc::new(MockServices {
            routes: vec![(UPSTREAM, 200, "text/event-stream", SSE_TOOL_USE.into())],
            ..MockServices::default()
        });
        let mut request = anthropic_client_request(json!({
            "model": "synthetic-adaptive-effort-model",
            "max_tokens": 1024,
            "thinking": {"type": "adaptive"},
            "messages": [{"role": "user", "content": "Read a."}]
        }));
        request.reasoning.summary = Some(ReasoningSummary::Detailed);
        request.reasoning.target_control = Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: requested.into(),
            },
        );
        let mut provider = signed_in();
        provider.model = Some("synthetic-adaptive-effort-model".into());
        // 标准声明供 Target 解析使用；原生能力快照供 Claude Code 的模式约束使用。
        provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([(
                "anthropic_capabilities".into(),
                json!({
                    "thinking": {
                        "supported": true,
                        "types": {
                            "adaptive": {"supported": true},
                            "enabled": {"supported": false}
                        }
                    },
                    "effort": {
                        "supported": true,
                        "high": {"supported": true}
                    }
                }),
            )]),
            ..Default::default()
        });
        if !raw_capabilities {
            provider
                .model_metadata
                .as_mut()
                .unwrap()
                .extensions
                .remove("anthropic_capabilities");
        }
        if let Some(declared) = declared {
            provider
                .model_metadata
                .as_mut()
                .unwrap()
                .extensions
                .insert("reasoning_efforts".into(), declared);
        }
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer { provider, request },
        )
        .await
        .expect("native effort inference succeeds");

        let requests = services.requests.lock();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(
            body["output_config"]["effort"].as_str(),
            expected,
            "{requested}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn scoped_ids_survive_reload_without_reusing_http_request_ids() {
    let services = Arc::new(MockServices {
        routes: vec![(UPSTREAM, 200, "text/event-stream", SSE_TOOL_USE.into())],
        private_state: Mutex::new(Some(
            br#"{"oauth_flow":{"code_verifier":"retained"}}"#.to_vec(),
        )),
        ..MockServices::default()
    });
    let source = " 空白\0\r\n会话😀 ";
    for _ in 0..2 {
        let (runtime, plugin) = load_plugin().await;
        let mut provider = signed_in();
        provider
            .operation_metadata
            .insert("transport_affinity".into(), source.into());
        provider
            .operation_metadata
            .insert("parent_transport_affinity".into(), source.into());
        provider.client_headers = vec![
            ("x-client-request-id".into(), "injected-request".into()),
            ("x-claude-code-session-id".into(), "injected-session".into()),
        ];
        let mut request = anthropic_client_request(json!({
            "model": "claude-sonnet-4-5", "max_tokens": 1024,
            "metadata": {"user_id": "{\"session_id\":\"injected-session\"}"},
            "messages": [{"role": "user", "content": "Read a."}]
        }));
        request.parameters.session.id = Some("injected-session".into());
        request.parameters.session.request_id = Some("injected-request".into());
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer { provider, request },
        )
        .await
        .expect("inference succeeds after reload");
    }
    let requests = services.requests.lock();
    assert_eq!(requests.len(), 2);
    let mut http_ids = Vec::new();
    for sent in requests.iter() {
        let header = |name: &str| wire_header(sent, name);
        assert_eq!(
            header("x-claude-code-session-id"),
            "0a00c8c6-848d-463f-94f9-b8c3e6ce1a14"
        );
        let http_id = header("x-client-request-id");
        assert_eq!(uuid::Uuid::parse_str(http_id).unwrap().get_version_num(), 4);
        assert_ne!(http_id, header("x-claude-code-session-id"));
        http_ids.push(http_id);
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        let identity: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(identity["session_id"], header("x-claude-code-session-id"));
        assert_eq!(identity["parent_session_id"], identity["session_id"]);
        assert_eq!(identity["device_id"], DEVICE_ID);
        assert_eq!(identity["account_uuid"], ACCOUNT_UUID);
    }
    assert_ne!(http_ids[0], http_ids[1]);
    let state: Value =
        serde_json::from_slice(services.private_state.lock().as_ref().unwrap()).unwrap();
    assert_eq!(state["oauth_flow"]["code_verifier"], "retained");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn inference_is_sent_in_claude_code_shape_and_tool_names_round_trip() {
    let (runtime, plugin) = load_plugin().await;
    let services = Arc::new(MockServices {
        routes: vec![(
            UPSTREAM,
            200,
            "text/event-stream",
            SSE_TOOL_USE.replace("\"name\":\"_read\"", "\"name\":\"__read\""),
        )],
        ..MockServices::default()
    });
    let request = anthropic_client_request(json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "system": "You are a helpful agent.",
        "tools": [
            {"name": "read", "description": "Read a file", "input_schema": {"type": "object"},
             "eager_input_streaming":true,"cache_control":{"type":"ephemeral","ttl":"5m"}},
            {"type":"web_search_20250305","name":"web_search","max_uses":8},
            {"name":"_read","input_schema":{"type":"object"}}
        ],
        "tool_choice":{"type":"tool","name":"_read"},
        "metadata":{"user_id":"{\"session_id\":\"client-session\",\"parent_session_id\":\"client-parent\"}"},
        "messages": [{"role": "user", "content": "Read a."}]
    }));
    let mut provider = signed_in();
    provider
        .operation_metadata
        .insert("transport_affinity".into(), json!("host-session"));
    provider
        .operation_metadata
        .insert("parent_transport_affinity".into(), json!("host-parent"));
    provider
        .client_headers
        .push(("x-claude-code-session-id".into(), "client-session".into()));
    let output = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer { provider, request },
    )
    .await
    .expect("inference succeeds");

    let requests = services.requests.lock();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages?beta=true");
    assert_eq!(wire_header(sent, "accept-encoding"), "gzip, deflate, br, zstd");
    let header = |name: &str| sent.headers.get(name).and_then(|bytes| std::str::from_utf8(bytes).ok());
    assert_eq!(header("authorization"), Some("Bearer sk-ant-oat01-test"));
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["system"][2]["text"], "You are a helpful agent.");
    let tools = body["tools"].as_array().unwrap();
    let names: std::collections::BTreeSet<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        std::collections::BTreeSet::from(["_read", "__read", "web_search"])
    );
    let read = tools.iter().find(|tool| tool["name"] == "_read").unwrap();
    assert_eq!(read["eager_input_streaming"], true);
    assert!(read.get("cache_control").is_none());
    let web = tools
        .iter()
        .find(|tool| tool["name"] == "web_search")
        .unwrap();
    assert_eq!(web["type"], "web_search_20250305");
    assert!(web.get("cache_control").is_none());
    assert_eq!(body["tool_choice"]["name"], "__read");
    assert_eq!(body["thread"], json!({"type":"create"}));
    assert_eq!(body["diagnostics"], json!({"previous_message_id":null}));
    assert_eq!(body["stream"], json!(true));
    let user_id: Value =
        serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(user_id["account_uuid"], ACCOUNT_UUID);
    assert_eq!(user_id["device_id"], DEVICE_ID);
    assert_ne!(user_id["session_id"], "client-session");
    assert_ne!(user_id["parent_session_id"], "client-parent");
    assert!(uuid::Uuid::parse_str(user_id["parent_session_id"].as_str().unwrap()).is_ok());
    assert_eq!(
        user_id["session_id"].as_str(),
        header("x-claude-code-session-id")
    );

    let OperationOutput::Infer(response) = output else {
        panic!("expected Infer output");
    };
    let calls: Vec<&str> = response
        .items
        .iter()
        .flat_map(|item| item.tool_calls.iter().flatten())
        .map(|call| call.name.as_str())
        .collect();
    assert_eq!(calls, ["_read"]);
    let deltas = services.deltas.lock();
    let starts: Vec<_> = deltas
        .iter()
        .filter_map(|delta| match delta {
            AiStreamDelta::ToolCallStart { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [("toolu_1", "_read")]);
}

fn message_reply(id: &str, content: Value, stop_reason: &str) -> String {
    json!({
        "id":id, "type":"message", "role":"assistant",
        "model":"claude-sonnet-4-5", "content":content,
        "stop_reason":stop_reason, "stop_sequence":null,
        "usage":{"input_tokens":10,"output_tokens":5}
    })
    .to_string()
}

fn wire_header<'a>(request: &'a HttpRequest, name: &str) -> &'a str {
    std::str::from_utf8(request.headers.get(name).unwrap_or_else(|| panic!("missing header {name}")))
        .expect("header is UTF-8")
}

fn threaded_request(messages: &[Value]) -> AiRequest {
    anthropic_client_request(json!({
        "model":"claude-sonnet-4-5", "max_tokens":1024,
        "system":"Read only the requested file.",
        "tools":[{"name":"Read","description":"Read a local file",
            "input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}],
        "messages":messages
    }))
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn thread_delta_and_native_ids_survive_tool_loop_and_guest_reload() {
    let (runtime, plugin) = load_plugin().await;
    let replies = [
        json!([{"type":"text","text":"READY-WIRE317"}]),
        json!([{"type":"tool_use","id":"toolu_011111111111111111111111","name":"_Read","input":{"path":"numbers.txt"}}]),
        json!([{"type":"text","text":"SUM=42"}]),
        json!([{"type":"text","text":"MARKER=WIRE317;SUM=42"}]),
    ];
    let services = Arc::new(MockServices {
        script: Mutex::new(
            replies
                .iter()
                .enumerate()
                .map(|(index, content)| {
                    (
                        200,
                        "application/json",
                        message_reply(
                            &format!("msg_01111111111111111111111{index}"),
                            content.clone(),
                            if index == 1 { "tool_use" } else { "end_turn" },
                        ),
                        Some(format!("req_01111111111111111111111{index}")),
                    )
                })
                .collect(),
        ),
        ..Default::default()
    });
    let mut provider = signed_in();
    provider
        .operation_metadata
        .insert("transport_affinity".into(), json!("threaded-user"));
    let prompts = [
        json!({"role":"user","content":"Remember WIRE317."}),
        json!({"role":"user","content":"Read numbers.txt and add its numbers."}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_011111111111111111111111","content":"19\n23\n"}]}),
        json!({"role":"user","content":"Recall the marker and sum."}),
    ];
    let mut history = Vec::new();
    for (index, prompt) in prompts.into_iter().enumerate() {
        history.push(prompt);
        let output = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: provider.clone(),
                request: threaded_request(&history),
            },
        )
        .await
        .expect("the complete multi-turn transcript is accepted");
        let OperationOutput::Infer(response) = output else {
            panic!("expected inference")
        };
        assert_eq!(response.id, format!("msg_01111111111111111111111{index}"));
        if index == 1 {
            let call = response.tool_calls().next().unwrap();
            assert_eq!(call.name, "Read");
            assert_eq!(call.id, "toolu_011111111111111111111111");
        }
        let mut replay = stravia_vendor_common::common::encode_inference_request(
            "anthropic-messages/messages/2023-06-01",
            AiRequest::new(&response.model, response.items),
        )
        .unwrap();
        history.append(replay.body["messages"].as_array_mut().unwrap());
    }
    // 新 Runtime 不共享 guest 内存，仍从宿主私有状态继续。
    let (reloaded_runtime, reloaded_plugin) = load_plugin().await;
    services.script.lock().push_back((
        200,
        "application/json",
        message_reply(
            "msg_011111111111111111111114",
            json!([{"type":"text","text":"42"}]),
            "end_turn",
        ),
        Some("req_011111111111111111111114".into()),
    ));
    history.push(json!({"role":"user","content":"Just the sum."}));
    run(
        &reloaded_runtime,
        &reloaded_plugin,
        &services,
        OperationInput::Infer {
            provider,
            request: threaded_request(&history),
        },
    )
    .await
    .expect("reload continues the same transcript");

    let sent = services.requests.lock();
    assert_eq!(sent.len(), 5);
    let mut request_ids = std::collections::BTreeSet::new();
    let first_session = wire_header(&sent[0], "x-claude-code-session-id");
    for (index, request) in sent.iter().enumerate() {
        assert_eq!(
            wire_header(request, "x-claude-code-session-id"),
            first_session
        );
        for field in [
            "x-claude-code-session-id",
            "x-claude-code-prompt-id",
            "x-client-request-id",
        ] {
            assert_eq!(
                uuid::Uuid::parse_str(wire_header(request, field))
                    .unwrap()
                    .get_version_num(),
                4
            );
        }
        assert!(request_ids.insert(wire_header(request, "x-client-request-id")));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let billing = body["system"][0]["text"].as_str().unwrap();
        assert!(billing.contains(&format!(
            "cc_prompt_id={};",
            wire_header(request, "x-claude-code-prompt-id")
        )));
        assert!(
            billing.contains("cc_entrypoint=cli;") && billing.contains("cc_turn_origin=human;")
        );
        assert!(billing.contains(&format!("cc_prompt_index={};", [1, 2, 2, 3, 4][index])));
        assert!(billing.contains(&format!("cc_turn_index={};", [1, 2, 2, 3, 4][index])));
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        if index == 0 {
            assert_eq!(body["thread"], json!({"type":"create"}));
            assert_eq!(body["tools"][0]["name"], "_Read");
        } else {
            let previous = format!("msg_01111111111111111111111{}", index - 1);
            assert_eq!(
                body["thread"],
                json!({"type":"continue","previous_message_id":previous})
            );
            assert_eq!(body["diagnostics"]["previous_message_id"], previous);
            assert_eq!(body["system"].as_array().unwrap().len(), 1);
            assert!(body.get("tools").is_none());
            assert!(billing.contains(&format!(
                "cc_prev_req=req_01111111111111111111111{};",
                index - 1
            )));
        }
        if index == 2 {
            let result = &body["messages"][0]["content"][0];
            assert_eq!(result["tool_use_id"], "toolu_011111111111111111111111");
            assert_eq!(result["content"], "19\n23\n");
            assert!(result.get("is_error").is_none());
        }
    }
    assert_eq!(
        wire_header(&sent[1], "x-claude-code-prompt-id"),
        wire_header(&sent[2], "x-claude-code-prompt-id")
    );
    for (left, right) in [(0, 1), (2, 3), (3, 4)] {
        assert_ne!(
            wire_header(&sent[left], "x-claude-code-prompt-id"),
            wire_header(&sent[right], "x-claude-code-prompt-id")
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn thread_rejections_are_reported_with_the_original_upstream_status() {
    let (runtime, plugin) = load_plugin().await;
    for (code, expected_status) in [
        ("thread_not_found", 404),
        ("thread_already_continued", 400),
        ("thread_continue_in_flight", 400),
        ("thread_fingerprint_mismatch", 400),
        ("thread_unsupported_request", 400),
    ] {
        let reply = json!([{"type":"text","text":"READY"}]);
        let services = Arc::new(MockServices {
            script: Mutex::new(VecDeque::from([
                (200, "application/json", message_reply("msg_1", reply.clone(), "end_turn"), Some("req_1".into())),
                (expected_status, "application/json",
                 json!({"type":"error","error":{"type":"invalid_request_error","message":"thread rejected",
                     "details":{"error_code":code}}}).to_string(), Some("req_rejected".into())),
            ])),
            ..Default::default()
        });
        let mut provider = signed_in();
        provider
            .operation_metadata
            .insert("transport_affinity".into(), json!("rejected-session"));
        let mut history = vec![json!({"role":"user","content":"Remember the marker."})];
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: provider.clone(),
                request: threaded_request(&history),
            },
        )
        .await
        .expect("first turn establishes the native thread");
        let state_after_first = services.private_state.lock().clone();
        history.push(json!({"role":"assistant","content":reply}));
        history.push(json!({"role":"user","content":"Continue."}));
        let error = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider,
                request: threaded_request(&history),
            },
        )
        .await
        .expect_err("a rejected native thread must fail this operation");
        match error {
            RuntimeError::Plugin {
                kind: ErrorKind::ContinuationNotFound,
                upstream_status,
                ..
            } => assert_eq!(upstream_status, Some(expected_status), "{code}"),
            other => panic!("{code}: expected continuation-not-found, got {other:?}"),
        }
        // 插件不自行重发，也不把已确认未执行的拒绝记成失败事件；失败的轮次
        // 不改写线程状态，宿主可据此授权一次完整历史重放。
        assert_eq!(
            *services.wire_log.lock(),
            ["start", "request", "start", "request"],
            "{code}"
        );
        let sent = services.requests.lock();
        assert_eq!(sent.len(), 2, "{code}");
        let rejected: Value = serde_json::from_slice(&sent[1].body).unwrap();
        assert_eq!(rejected["thread"]["type"], "continue", "{code}");
        assert_eq!(rejected["messages"].as_array().unwrap().len(), 1, "{code}");
        assert_eq!(*services.private_state.lock(), state_after_first, "{code}");
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn host_authorized_continuation_replay_sends_full_history_without_a_thread() {
    let (runtime, plugin) = load_plugin().await;
    let reply = json!([{"type":"text","text":"READY"}]);
    let services = Arc::new(MockServices {
        script: Mutex::new(VecDeque::from([
            (200, "application/json", message_reply("msg_1", reply.clone(), "end_turn"), Some("req_1".into())),
            (404, "application/json",
             json!({"type":"error","error":{"type":"invalid_request_error","message":"thread rejected",
                 "details":{"error_code":"thread_not_found"}}}).to_string(), Some("req_rejected".into())),
            (200, "application/json", message_reply("msg_2", reply.clone(), "end_turn"), Some("req_2".into())),
            (200, "application/json", message_reply("msg_3", reply.clone(), "end_turn"), Some("req_3".into())),
        ])),
        ..Default::default()
    });
    let mut provider = signed_in();
    provider
        .operation_metadata
        .insert("transport_affinity".into(), json!("host-replay-session"));
    let mut history = vec![json!({"role":"user","content":"Remember the marker."})];
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: threaded_request(&history),
        },
    )
    .await
    .expect("first turn establishes the native thread");
    history.push(json!({"role":"assistant","content":reply}));
    history.push(json!({"role":"user","content":"Continue."}));
    let rejected = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: threaded_request(&history),
        },
    )
    .await;
    assert!(matches!(
        rejected,
        Err(RuntimeError::Plugin {
            kind: ErrorKind::ContinuationNotFound,
            ..
        })
    ));

    // 宿主授权一次完整历史重放：本轮必须发送 canonical 全量历史，且不再携带
    // 被拒绝的 native thread。普通会话（没有该标记）仍按 session 计划续接。
    provider
        .operation_metadata
        .insert("continuation_replay".into(), json!(true));
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: threaded_request(&history),
        },
    )
    .await
    .expect("the host-authorized full replay succeeds");

    {
        let sent = services.requests.lock();
        assert_eq!(sent.len(), 3);
        let replay: Value = serde_json::from_slice(&sent[2].body).unwrap();
        assert!(replay.get("thread").is_none() && replay.get("diagnostics").is_none());
        assert!(!wire_header(&sent[2], "anthropic-beta").contains("message-threads"));
        assert_eq!(replay["messages"].as_array().unwrap().len(), 3);
        assert_eq!(replay["messages"][1]["content"], reply);
        assert!(replay.get("tools").is_some());
        // 重放是独立的新插件操作：重试的 HTTP 请求使用新的 request id，但仍不复用
        // 被拒绝的线程状态。
        assert_ne!(
            wire_header(&sent[1], "x-client-request-id"),
            wire_header(&sent[2], "x-client-request-id")
        );
        assert_eq!(
            *services.wire_log.lock(),
            ["start", "request", "start", "request", "start", "request"]
        );
    }

    provider.operation_metadata.remove("continuation_replay");
    history.push(json!({"role":"assistant","content":reply}));
    history.push(json!({"role":"user","content":"Continue with a new thread."}));
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider,
            request: threaded_request(&history),
        },
    )
    .await
    .expect("normal requests can create a fresh native thread after recovery");
    let sent = services.requests.lock();
    let next: Value = serde_json::from_slice(&sent[3].body).unwrap();
    assert_eq!(next["thread"], json!({"type":"create"}));
    assert_eq!(next["messages"].as_array().unwrap().len(), 5);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn unrelated_upstream_failures_are_not_reclassified_as_continuation_loss() {
    let (runtime, plugin) = load_plugin().await;
    for status in [400, 401, 429, 503] {
        let services = Arc::new(MockServices {
            routes: vec![(UPSTREAM, status, "application/json",
                json!({"type":"error","error":{"type":"invalid_request_error","message":"unrelated failure"}}).to_string())],
            ..Default::default()
        });
        let result = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: signed_in(),
                request: threaded_request(&[json!({"role":"user","content":"Hi"})]),
            },
        )
        .await;
        match result {
            Err(RuntimeError::Plugin { kind, .. }) => assert!(
                !matches!(
                    kind,
                    ErrorKind::ContinuationNotFound | ErrorKind::ContinuationUnavailable
                ),
                "{status}: unrelated failure was reclassified as continuation loss"
            ),
            other => panic!("{status}: expected a plugin failure, got {other:?}"),
        }
        assert_eq!(services.requests.lock().len(), 1);
        assert!(services.private_state.lock().is_none());
        // 普通失败只声明一次开始、发送一次，不产生第二次请求。
        assert_eq!(*services.wire_log.lock(), ["start", "request"]);
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn authorization_code_exchange_fills_identity_from_bootstrap() {
    let (runtime, plugin) = load_plugin().await;
    let services = Arc::new(MockServices {
        routes: vec![
            (
                "https://api.anthropic.com/v1/oauth/token",
                200,
                "application/json",
                json!({
                    "access_token": "sk-ant-oat01-new",
                    "refresh_token": "sk-ant-ort01-new",
                    "expires_in": 28800,
                    "scope": "user:inference user:profile"
                })
                .to_string(),
            ),
            (
                "https://api.anthropic.com/api/claude_cli/bootstrap",
                200,
                "application/json",
                json!({"oauth_account": {
                    "account_uuid": ACCOUNT_UUID,
                    "account_email": "user@example.com",
                    "organization_uuid": "org-1",
                    "organization_name": "Org"
                }})
                .to_string(),
            ),
        ],
        ..MockServices::default()
    });
    let auth = |step| OperationInput::Auth {
        provider: provider(BTreeMap::new()),
        request: AuthRequest { step },
    };
    let started = run(
        &runtime,
        &plugin,
        &services,
        auth(AuthStep::Start {
            redirect_uri: "http://localhost:54545/callback".into(),
            state: "hostproposedstate".into(),
        }),
    )
    .await
    .expect("auth starts");
    let OperationOutput::Auth(AuthResponse::Authorization {
        url,
        state: Some(state),
        ..
    }) = started
    else {
        panic!("expected authorization URL with a plugin-owned state");
    };
    assert!(url.starts_with("https://claude.ai/oauth/authorize?client_id="));
    // claude.ai 拒绝宿主默认 state 形态；插件必须改用 omp 同款 32 位小写 hex。
    assert_eq!(state.len(), 32);
    assert!(
        state
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert!(url.contains(&format!("&state={state}&")));

    let rejected = run(
        &runtime,
        &plugin,
        &services,
        auth(AuthStep::Exchange {
            callback_url: "http://localhost:54545/callback?code=c&state=other".into(),
        }),
    )
    .await;
    assert!(
        rejected.is_err(),
        "state mismatch must not exchange the code"
    );
    assert!(services.requests.lock().is_empty());

    let exchanged = run(
        &runtime,
        &plugin,
        &services,
        auth(AuthStep::Exchange {
            callback_url: format!("http://localhost:54545/callback?code=c&state={state}"),
        }),
    )
    .await
    .expect("code exchanges");
    let OperationOutput::Auth(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    }) = exchanged
    else {
        panic!("expected credentials");
    };
    assert!(expires_at_unix_ms.is_some());
    assert_eq!(values["access_token"], "sk-ant-oat01-new");
    assert_eq!(values["refresh_token"], "sk-ant-ort01-new");
    assert_eq!(values["account_uuid"], ACCOUNT_UUID);
    assert_eq!(values["email"], "user@example.com");
    assert_eq!(values["organization_uuid"], "org-1");
    assert_eq!(values["device_id"].as_str().unwrap().len(), 64);

    let requests = services.requests.lock();
    let token: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(token["grant_type"], "authorization_code");
    assert_eq!(token["code"], "c");
    assert_eq!(token["state"], state.as_str());
    assert_eq!(token["redirect_uri"], "http://localhost:54545/callback");
    assert!(
        requests[1]
            .url
            .starts_with("https://api.anthropic.com/api/claude_cli/bootstrap")
    );
}
