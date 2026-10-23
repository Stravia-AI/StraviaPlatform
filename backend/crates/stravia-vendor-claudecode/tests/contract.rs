//! 非忽略的 canonical/codec 合同与显式 opt-in 的真实 Component 回归：
//! 在 HostServices stub 上验证
//! Claude Code 线上形态（请求头顺序、计费头与 cch、工具名前缀往返）与
//! OAuth 授权码交换 / 身份补全。
//!
//! 运行：`task build:vendors:all` 后执行
//! `cargo test -p stravia-vendor-claudecode --test contract -- --ignored`。

use std::collections::BTreeMap;
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
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent, VendorRuntime,
};
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
    body: Mutex<Option<Vec<u8>>>,
}

#[async_trait]
impl HostHttpResponse for MockResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.status)
    }

    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure> {
        Ok(vec![("content-type".into(), self.content_type.into())])
    }

    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.body.lock().take())
    }
}

/// 按 URL 前缀路由的上游桩；未匹配的请求以 404 返回，便于断言调用序列。
#[derive(Default)]
struct MockServices {
    routes: Vec<(&'static str, u16, &'static str, String)>,
    requests: Mutex<Vec<HttpRequest>>,
    private_state: Mutex<Option<Vec<u8>>>,
    deltas: Mutex<Vec<AiStreamDelta>>,
    reject_assistant_prefill: bool,
}

#[async_trait]
impl HostServices for MockServices {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
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
            body: Mutex::new(Some(body.into_bytes())),
        }))
    }

    async fn ws_connect(
        &self,
        _request: stravia_vendor_runtime::WsRequest,
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
        if let RuntimeEvent::Delta(delta) = event {
            self.deltas.lock().push(delta);
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

fn sent_cch(body: &[u8]) -> &str {
    let text = std::str::from_utf8(body).unwrap();
    let start = text.find("cch=").unwrap() + 4;
    &text[start..start + 5]
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
        (None, "summarized"),
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
        let header = |name: &str| {
            sent.headers
                .iter()
                .find(|(candidate, _)| candidate == name)
                .unwrap()
                .1
                .as_str()
        };
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
    assert!(
        services.private_state.lock().is_none(),
        "identity projection does not rewrite authentication state"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn inference_is_sent_in_claude_code_shape_and_tool_names_round_trip() {
    let (runtime, plugin) = load_plugin().await;
    let services = Arc::new(MockServices {
        routes: vec![(UPSTREAM, 200, "text/event-stream", SSE_TOOL_USE.into())],
        ..MockServices::default()
    });
    let request = anthropic_client_request(json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "system": "You are a helpful agent.",
        "tools": [
            {"name": "read", "description": "Read a file", "input_schema": {"type": "object"},
             "eager_input_streaming":true,"cache_control":{"type":"ephemeral","ttl":"5m"}},
            {"type":"web_search_20250305","name":"web_search","max_uses":8}
        ],
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
    let compression = sent
        .compression
        .as_ref()
        .expect("explicit HTTP compression");
    assert!(compression.request.is_none());
    assert!(matches!(
        compression.response.as_slice(),
        [
            stravia_vendor_sdk::HttpContentCoding::Gzip,
            stravia_vendor_sdk::HttpContentCoding::Deflate,
            stravia_vendor_sdk::HttpContentCoding::Br,
            stravia_vendor_sdk::HttpContentCoding::Zstd,
        ]
    ));
    assert!(
        !sent
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("accept-encoding"))
    );
    let header = |name: &str| {
        sent.headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(header("authorization"), Some("Bearer sk-ant-oat01-test"));
    let client_version = header("user-agent")
        .and_then(|value| value.strip_prefix("claude-cli/"))
        .and_then(|value| value.strip_suffix(" (external, sdk-cli)"))
        .expect("Claude CLI identity");
    assert!(
        header("anthropic-beta")
            .unwrap()
            .ends_with(",fallback-credit-2026-06-01")
    );

    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    let billing = body["system"][0]["text"].as_str().unwrap();
    assert!(billing.starts_with(&format!(
        "x-anthropic-billing-header: cc_version={client_version}."
    )));
    // cch 的算法正确性由单元测试对照参考向量覆盖；这里确认占位符已被写回。
    assert_ne!(sent_cch(&sent.body), "00000");
    assert_eq!(
        body["system"][1]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(body["system"][2]["text"], "You are a helpful agent.");
    assert_eq!(body["tools"][0]["name"], "_read");
    assert!(body["tools"][0].get("eager_input_streaming").is_none());
    assert!(body["tools"][0].get("cache_control").is_none());
    assert_eq!(body["tools"][1]["type"], "web_search_20250305");
    assert_eq!(body["tools"][1]["name"], "web_search");
    assert_eq!(
        body["tools"][1]["cache_control"],
        json!({"type":"ephemeral","ttl":"1h"})
    );
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
    assert_eq!(calls, ["read"], "response tool names drop the wire prefix");
    let streamed: Vec<String> = services
        .deltas
        .lock()
        .iter()
        .filter_map(|delta| match delta {
            AiStreamDelta::ToolCallStart { name, .. } => Some(name.clone()),
            AiStreamDelta::ToolCallComplete { tool_call, .. } => Some(tool_call.name.clone()),
            _ => None,
        })
        .collect();
    assert!(!streamed.is_empty());
    assert!(streamed.iter().all(|name| name == "read"), "{streamed:?}");
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
