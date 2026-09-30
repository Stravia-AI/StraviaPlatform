//! 显式 opt-in 的真实 Component 合同回归：在 HostServices stub 上验证
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
use stravia_runtime_contract::protocol::ir::{AiRequest, AiStreamDelta};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, OperationInput, OperationOutput,
    ProviderSnapshot,
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
}

#[async_trait]
impl HostServices for MockServices {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
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
        _url: String,
        _headers: Vec<(String, String)>,
        _protocols: Vec<String>,
        _continuation_id: Option<String>,
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
    runtime.execute(plugin, CHANNEL, input, scope).await
}

fn sent_cch(body: &[u8]) -> &str {
    let text = std::str::from_utf8(body).unwrap();
    let start = text.find("cch=").unwrap() + 4;
    &text[start..start + 5]
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
        "tools": [{"name": "read", "description": "Read a file", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "Read a."}]
    }));
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
    .expect("inference succeeds");

    let requests = services.requests.lock();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages?beta=true");
    let names: Vec<&str> = sent.headers.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "accept",
            "accept-encoding",
            "authorization",
            "connection",
            "content-type",
            "user-agent",
            "x-claude-code-session-id",
            "x-stainless-arch",
            "x-stainless-lang",
            "x-stainless-os",
            "x-stainless-package-version",
            "x-stainless-retry-count",
            "x-stainless-runtime",
            "x-stainless-runtime-version",
            "x-stainless-timeout",
            "anthropic-beta",
            "anthropic-dangerous-direct-browser-access",
            "anthropic-version",
            "x-app",
        ]
    );
    let header = |name: &str| {
        sent.headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(header("authorization"), Some("Bearer sk-ant-oat01-test"));
    assert_eq!(
        header("user-agent"),
        Some("claude-cli/2.1.280 (external, cli)")
    );
    assert!(
        header("anthropic-beta")
            .unwrap()
            .ends_with(",fallback-credit-2026-06-01")
    );

    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    let billing = body["system"][0]["text"].as_str().unwrap();
    assert!(billing.starts_with("x-anthropic-billing-header: cc_version=2.1.280."));
    // cch 的算法正确性由单元测试对照参考向量覆盖；这里确认占位符已被写回。
    assert_ne!(sent_cch(&sent.body), "00000");
    assert_eq!(
        body["system"][1]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(body["system"][2]["text"], "You are a helpful agent.");
    assert_eq!(body["tools"][0]["name"], "_read");
    assert_eq!(body["stream"], json!(true));
    let user_id: Value =
        serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(user_id["account_uuid"], ACCOUNT_UUID);
    assert_eq!(user_id["device_id"], DEVICE_ID);
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
