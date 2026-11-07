//! 显式 opt-in 的真实 Component 合同回归：在 HostServices stub 上验证
//! 免费层契约——指纹头、Bearer public 兜底、占位工具注入与禁令、stream
//! 强制，以及 discovery 只暴露免费模型。
//!
//! 先运行 `task build:vendors:all`。离线合同使用 `--ignored --skip live_`；
//! 匿名真实调用须显式选择 `live_free_model_infer_against_real_zen -- --ignored --exact`。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::request::{ToolChoice, ToolSpec};
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, LoadedPlugin, LogLevel,
    OperationScope, RuntimeError, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::net::{self, ConnectRequest, Field, Options, Request as HttpRequest};
use stravia_vendor_sdk::{ErrorKind, OperationInput, OperationOutput, ProviderSnapshot};

const UPSTREAM: &str = "https://opencode.ai/inference/v1";

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
                .find(|entry| entry["vendor_id"] == "opencode-free")
        })
        .expect("opencode-free missing from vendor manifest; run task build:vendors:all");
    dir.join(entry["file"].as_str().unwrap())
}

struct MockResponse {
    status: u16,
    body: Mutex<std::vec::IntoIter<Vec<u8>>>,
}

#[async_trait]
impl HostHttpResponse for MockResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.status)
    }

    async fn headers(&self) -> Result<Vec<Field>, HostFailure> {
        Ok(vec![Field {
            name: "content-type".into(),
            bytes: b"text/event-stream".to_vec(),
        }])
    }

    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.body.lock().next())
    }
}

#[derive(Default)]
struct Recorded {
    requests: Mutex<Vec<HttpRequest>>,
    private_state: Mutex<Option<Vec<u8>>>,
}

struct MockServices {
    upstream_body: &'static str,
    upstream_status: u16,
    recorded: Arc<Recorded>,
}

#[async_trait]
impl HostServices for MockServices {
    fn http_start(
        &self,
        request: HttpRequest,
        _options: Options,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let rejects_tool_choice = request.method == "POST"
            && serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
                let model = body["model"].as_str().unwrap_or_default();
                (model == "big-pickle" || model.ends_with("-free")) && body["tool_choice"] != "auto"
            });
        self.recorded.requests.lock().push(request);
        if rejects_tool_choice {
            return Ok(Arc::new(MockResponse {
                status: 400,
                body: Mutex::new(
                    vec![br#"{"error":{"message":"Only auto tool choice is supported","type":"invalid_request_error"}}"#.to_vec()].into_iter(),
                ),
            }));
        }
        Ok(Arc::new(MockResponse {
            status: self.upstream_status,
            body: Mutex::new(vec![self.upstream_body.as_bytes().to_vec()].into_iter()),
        }))
    }

    fn validate_net_option(&self, key: &str, value: &str) -> Result<(), net::OptionError> {
        net::validate_host_option(key, value)
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
        Ok(self.recorded.private_state.lock().clone())
    }

    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        *self.recorded.private_state.lock() = Some(bytes);
        Ok(())
    }

    async fn emit_event(&self, _event: RuntimeEvent) -> Result<(), HostFailure> {
        Ok(())
    }

    fn log(&self, _level: LogLevel, _message: &str) {}

    fn generation_is_current(&self, _generation: u64) -> bool {
        true
    }
}

const SSE_OK: &str = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"big-pickle\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\ndata: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"big-pickle\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n";

const MODELS_JSON: &str = r#"{"object":"list","data":[{"id":"big-pickle"},{"id":"gpt-5.5"},{"id":"mimo-v2.5-free"},{"id":"claude-opus-5"}]}"#;

fn provider(model: &str) -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "opencode-free".into(),
        channel: "default".into(),
        base_url: UPSTREAM.into(),
        protocol: "openai-compatible".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: Some(model.into()),
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::from([
            (
                "client_conversation_id".into(),
                json!("00000000-0000-4000-8000-000000000001"),
            ),
            (
                "client_conversation_created_at_ms".into(),
                json!(1700000000000_u64),
            ),
            (
                "user_prompt_id".into(),
                json!("00000000-0000-4000-8000-000000000001"),
            ),
            ("user_prompt_created_at_ms".into(), json!(1700000000000_u64)),
        ]),
    }
}

fn request(model: &str) -> AiRequest {
    AiRequest::new(
        model,
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("say ok".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
}

fn client_tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: Some("client tool".into()),
        parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        strict: None,
        defer_loading: None,
    }
}

async fn load_plugin() -> (VendorRuntime, LoadedPlugin) {
    let bytes = std::fs::read(artifact()).unwrap();
    let runtime = VendorRuntime::new().expect("runtime");
    let plugin = runtime.load(&bytes).await.expect("component loads");
    (runtime, plugin)
}

async fn infer_once(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    provider: ProviderSnapshot,
    request: AiRequest,
    recorded: &Arc<Recorded>,
) -> Result<OperationOutput, RuntimeError> {
    let services = Arc::new(MockServices {
        upstream_body: SSE_OK,
        upstream_status: 200,
        recorded: Arc::clone(recorded),
    });
    let scope = OperationScope::new(
        services,
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(60)),
        0,
    );
    let (operation, input) = OperationInput::Infer { provider, request }
        .encode_for_host()
        .map_err(|_| RuntimeError::InvalidOutput)?;
    runtime
        .execute(plugin, "default", operation, input, scope)
        .await
}

fn sent_body(recorded: &Recorded) -> Value {
    let requests = recorded.requests.lock();
    assert_eq!(requests.len(), 1, "exactly one upstream request");
    serde_json::from_slice(&requests[0].body).expect("request body is JSON")
}

fn sent_header(recorded: &Recorded, name: &str) -> Option<String> {
    recorded.requests.lock()[0]
        .headers
        .get(name)
        .map(|bytes| String::from_utf8(bytes.to_vec()).expect("UTF-8 header"))
}

fn sent_tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    t["function"]["name"]
                        .as_str()
                        .or_else(|| t["name"].as_str())
                        .unwrap_or("")
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn assistant_text(items: &[AiItem]) -> String {
    items
        .iter()
        .filter_map(|item| match &item.content {
            MessageContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn required_choice_with_complete_tools_does_not_trigger_native_400() {
    let (runtime, plugin) = load_plugin().await;
    let recorded = Arc::new(Recorded::default());
    let mut request = request("big-pickle");
    request.tools = Some(vec![client_tool("bash"), client_tool("read")]);
    request.tool_choice = Some(ToolChoice::Required);
    let output = infer_once(
        &runtime,
        &plugin,
        provider("big-pickle"),
        request,
        &recorded,
    )
    .await
    .expect("inference succeeds");

    let OperationOutput::Infer(response) = output else {
        panic!("expected Infer output");
    };
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(assistant_text(&response.items), "ok");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn prompt_and_session_identity_survive_component_stores_without_private_state() {
    let (runtime, plugin) = load_plugin().await;
    let recorded = Arc::new(Recorded::default());
    let mut provider = provider("big-pickle");
    provider.operation_metadata.insert(
        "session_affinity".into(),
        Value::String("主体/Target/链路\0\r\n🙂".into()),
    );
    provider.client_headers = vec![
        ("x-opencode-session".into(), "injected-session".into()),
        ("x-opencode-request".into(), "injected-request".into()),
    ];
    *recorded.private_state.lock() = Some(b"obsolete state must not be read".to_vec());
    for index in 0..4 {
        if index == 2 {
            provider.operation_metadata.insert(
                "user_prompt_id".into(),
                json!("00000000-0000-4000-8000-000000000002"),
            );
            provider
                .operation_metadata
                .insert("user_prompt_created_at_ms".into(), json!(1700000001000_u64));
        }
        if index == 3 {
            provider.operation_metadata.insert(
                "client_conversation_id".into(),
                json!("00000000-0000-4000-8000-000000000003"),
            );
            provider.operation_metadata.insert(
                "client_conversation_created_at_ms".into(),
                json!(1700000002000_u64),
            );
            provider.operation_metadata.insert(
                "user_prompt_id".into(),
                json!("00000000-0000-4000-8000-000000000003"),
            );
            provider
                .operation_metadata
                .insert("user_prompt_created_at_ms".into(), json!(1700000002000_u64));
        }
        let mut input = request("big-pickle");
        input.parameters.session.id = Some("client-session".into());
        input.parameters.session.request_id = Some("client-request".into());
        infer_once(&runtime, &plugin, provider.clone(), input, &recorded)
            .await
            .expect("inference succeeds");
    }
    let requests = recorded.requests.lock();
    assert_eq!(requests.len(), 4);
    let header = |index: usize, name: &str| {
        requests[index]
            .headers
            .get(name)
            .map(|bytes| std::str::from_utf8(bytes).expect("UTF-8 header"))
            .expect("native identity header")
    };
    assert_eq!(
        header(0, "x-opencode-session"),
        header(1, "x-opencode-session")
    );
    assert_eq!(
        header(0, "x-opencode-session"),
        header(2, "x-opencode-session")
    );
    assert_eq!(
        header(0, "x-opencode-request"),
        header(1, "x-opencode-request")
    );
    assert_ne!(
        header(0, "x-opencode-session"),
        header(3, "x-opencode-session")
    );
    assert_ne!(
        header(0, "x-opencode-request"),
        header(2, "x-opencode-request")
    );
    assert_ne!(
        header(2, "x-opencode-request"),
        header(3, "x-opencode-request")
    );
    for index in 0..4 {
        assert!(header(index, "x-opencode-request").starts_with("msg_"));
        let body: Value = serde_json::from_slice(&requests[index].body).unwrap();
        for name in [
            "session_id",
            "request_id",
            "conversation",
            "previous_response_id",
        ] {
            assert!(body.get(name).is_none(), "{name}");
        }
    }
    assert_eq!(
        recorded.private_state.lock().as_deref(),
        Some(b"obsolete state must not be read".as_slice())
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn client_named_tool_overrides_placeholder() {
    let (runtime, plugin) = load_plugin().await;
    let mut request = request("mimo-v2.5-free");
    request.tools = Some(vec![client_tool("bash")]);
    let recorded = Arc::new(Recorded::default());
    infer_once(
        &runtime,
        &plugin,
        provider("mimo-v2.5-free"),
        request,
        &recorded,
    )
    .await
    .expect("inference succeeds");

    let body = sent_body(&recorded);
    let names = sent_tool_names(&body);
    assert_eq!(names, vec!["bash", "read"]);
    // 客户端 bash 的 schema 原样保留。
    let bash = &body["tools"][0];
    assert_eq!(
        bash["function"]["parameters"]["properties"]["q"]["type"],
        json!("string")
    );
    let system = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "system")
        .expect("system message present");
    let text = system["content"].as_str().unwrap_or_default();
    assert!(text.contains("`read`"));
    assert!(!text.contains("`bash`"));
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn paid_model_passes_through_without_injection() {
    let (runtime, plugin) = load_plugin().await;
    let recorded = Arc::new(Recorded::default());
    infer_once(
        &runtime,
        &plugin,
        provider("gpt-5.5"),
        request("gpt-5.5"),
        &recorded,
    )
    .await
    .expect("inference succeeds");

    let body = sent_body(&recorded);
    assert!(body["tools"].is_null());
    // 客户端非流式请求不被改写（付费模型不受免费门控）。
    assert_ne!(body["stream"], json!(true));
    // 指纹头仍发送——与官方客户端一致。
    assert!(sent_header(&recorded, "x-opencode-session").is_some());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn discovery_lists_only_free_models() {
    let (runtime, plugin) = load_plugin().await;
    let recorded = Arc::new(Recorded::default());
    let services = Arc::new(MockServices {
        upstream_body: MODELS_JSON,
        upstream_status: 200,
        recorded: Arc::clone(&recorded),
    });
    let scope = OperationScope::new(
        services,
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(60)),
        0,
    );
    let (operation, input) = OperationInput::Discover {
        provider: provider(""),
        request: stravia_vendor_sdk::DiscoverRequest { cursor: None },
    }
    .encode_for_host()
    .expect("encode discovery");
    let output = runtime
        .execute(&plugin, "default", operation, input, scope)
        .await
        .expect("discovery succeeds");
    let OperationOutput::Discover(response) = output else {
        panic!("expected Discover output");
    };
    let ids: Vec<&str> = response.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["big-pickle", "mimo-v2.5-free"]);
    let recorded_request = &recorded.requests.lock()[0];
    assert_eq!(recorded_request.method, "GET");
    assert_eq!(recorded_request.url, format!("{UPSTREAM}/models"));
}

// ── 真实上游 ────────────────────────────────────────────────────────────────
// 用 reqwest 实现 HostServices，让真实组件调用 opencode.ai；匿名配额和门控
// 由上游决定，仅显式 opt-in 才运行，不使用用户凭据、不自动重试。

#[derive(Default)]
struct RealServices {
    private_state: Mutex<Option<Vec<u8>>>,
}

type RealResponseData = (u16, Vec<Field>, Vec<u8>);

struct RealResponse {
    builder: Mutex<Option<reqwest::RequestBuilder>>,
    cell: tokio::sync::OnceCell<RealResponseData>,
    body_taken: Mutex<bool>,
}

impl RealResponse {
    /// 惰性发起请求并共享结果给 status/headers/read_body 三个读取入口。
    async fn fetch(&self) -> Result<&RealResponseData, HostFailure> {
        self.cell
            .get_or_try_init(|| async {
                let builder = self
                    .builder
                    .lock()
                    .take()
                    .ok_or_else(|| "request already consumed".to_owned())?;
                let response = builder.send().await.map_err(|e| e.to_string())?;
                let status = response.status().as_u16();
                eprintln!("anonymous Zen live upstream_status={status}");
                let headers = response
                    .headers()
                    .iter()
                    .map(|(n, v)| Field {
                        name: n.as_str().to_owned(),
                        bytes: v.as_bytes().to_vec(),
                    })
                    .collect();
                let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
                Ok((status, headers, body))
            })
            .await
            .map_err(|e: String| HostFailure::new(ErrorKind::Trapped, e))
    }
}

#[async_trait]
impl HostHttpResponse for RealResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.fetch().await?.0)
    }

    async fn headers(&self) -> Result<Vec<Field>, HostFailure> {
        Ok(self.fetch().await?.1.clone())
    }

    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        let bytes = self.fetch().await?.2.clone();
        let mut taken = self.body_taken.lock();
        if *taken {
            return Ok(None);
        }
        *taken = true;
        Ok(Some(bytes))
    }
}

#[async_trait]
impl HostServices for RealServices {
    fn http_start(
        &self,
        request: HttpRequest,
        _options: Options,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let client = reqwest::Client::new();
        let method: reqwest::Method = request
            .method
            .parse()
            .map_err(|_| HostFailure::new(ErrorKind::Invalid, "bad method"))?;
        let mut builder = client.request(method, request.url).body(request.body);
        for field in request.headers.into_vec() {
            let value = reqwest::header::HeaderValue::from_bytes(&field.bytes)
                .map_err(|_| HostFailure::new(ErrorKind::Invalid, "bad header value"))?;
            builder = builder.header(field.name, value);
        }
        Ok(Arc::new(RealResponse {
            builder: Mutex::new(Some(builder)),
            cell: tokio::sync::OnceCell::new(),
            body_taken: Mutex::new(false),
        }))
    }

    fn validate_net_option(&self, key: &str, value: &str) -> Result<(), net::OptionError> {
        net::validate_host_option(key, value)
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

    async fn emit_event(&self, _event: RuntimeEvent) -> Result<(), HostFailure> {
        Ok(())
    }

    fn log(&self, _level: LogLevel, _message: &str) {}

    fn generation_is_current(&self, _generation: u64) -> bool {
        true
    }
}

/// 真实端到端：组件 → VendorRuntime → reqwest → opencode.ai。
/// 验证免费层契约在真实上游成立（不只本地 stub）。
#[tokio::test(flavor = "current_thread")]
#[ignore = "live upstream call; requires network and task build:vendors:all"]
async fn live_free_model_infer_against_real_zen() {
    let (runtime, plugin) = load_plugin().await;
    let services: Arc<dyn HostServices> = Arc::new(RealServices::default());
    let base_url = plugin
        .descriptor()
        .provider("opencode-free")
        .expect("free provider")
        .channels
        .iter()
        .find(|channel| channel.id == "default")
        .and_then(|channel| channel.default_base_url.as_deref())
        .expect("native default endpoint")
        .to_owned();
    let scope = OperationScope::new(
        services,
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(120)),
        0,
    );
    let mut anonymous_provider = provider("big-pickle");
    anonymous_provider.base_url = base_url.clone();
    anonymous_provider.operation_metadata.insert(
        "session_affinity".into(),
        Value::String("abcdef0123456789".repeat(4)),
    );
    // 仅合成内容与公开的 Bearer public；错误诊断不得扩展到用户连接。
    assert!(anonymous_provider.credentials.is_empty());
    assert!(anonymous_provider.client_headers.is_empty());
    let mut probe = request("big-pickle");
    probe.items[0].content = MessageContent::Text(
        "Reply with exactly STRAVIA_ZEN_PROBE_OK. Do not use tools."
            .to_owned()
            .into(),
    );
    probe.generation.max_tokens = Some(1024);
    let (operation, input) = OperationInput::Infer {
        provider: anonymous_provider,
        request: probe,
    }
    .encode_for_host()
    .expect("encode inference");
    let output = runtime
        .execute(&plugin, "default", operation, input, scope)
        .await
        .unwrap_or_else(|error| {
            let diagnostic: String = error
                .diagnostic_message()
                .unwrap_or("no upstream diagnostic")
                .chars()
                .take(2048)
                .collect();
            panic!(
                "anonymous Zen live infer failed at {base_url}: {error}; upstream_status={:?}; diagnostic={diagnostic}",
                error.upstream_status()
            );
        });
    let OperationOutput::Infer(response) = output else {
        panic!("expected Infer output");
    };
    assert!(
        response.error.is_none(),
        "upstream error: {:?}",
        response.error
    );
    let text = assistant_text(&response.items);
    assert_eq!(text.trim(), "STRAVIA_ZEN_PROBE_OK");
    eprintln!("anonymous Zen {base_url}: {text}");
}
