//! Component consumer contracts. Build vendor components before running ignored tests.
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};
use stravia_runtime_contract::protocol::ir::request::{ToolChoice, ToolSpec};
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role, ToolCall};
use stravia_runtime_contract::thinking::ThinkingLevel;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::net::{self, ConnectRequest, Field, Options, Request as HttpRequest};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, DiscoverRequest, ErrorKind, OperationInput,
    OperationOutput, ProviderSnapshot,
};

struct Response {
    status: u16,
    body: Mutex<Option<Vec<u8>>>,
}
#[async_trait]
impl HostHttpResponse for Response {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.status)
    }
    async fn headers(&self) -> Result<Vec<Field>, HostFailure> {
        Ok(vec![
            Field { name: "content-type".into(), bytes: b"text/event-stream".to_vec() },
            Field { name: "retry-after".into(), bytes: b"7".to_vec() },
        ])
    }
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.body.lock().take())
    }
}
#[derive(Default)]
struct Services {
    replies: Mutex<VecDeque<(u16, String)>>,
    requests: Mutex<Vec<HttpRequest>>,
    state: Mutex<Option<Vec<u8>>>,
    events: Mutex<Vec<RuntimeEvent>>,
    logs: Mutex<Vec<String>>,
}
impl Services {
    fn reply(&self, status: u16, value: Value) {
        self.replies.lock().push_back((status, value.to_string()));
    }
    fn device(&self) {
        self.reply(200, json!({"device_code":"secret-device", "user_code":"ABCD", "verification_uri":"https://auth.meta.com/device", "interval":1,"expires_in":600}));
    }
    fn make_poll_due(&self) {
        let mut state = self.state.lock();
        let mut value: Value = serde_json::from_slice(state.as_ref().unwrap()).unwrap();
        value["next_poll_ms"] = json!(0);
        *state = Some(serde_json::to_vec(&value).unwrap());
    }
}
#[async_trait]
impl HostServices for Services {
    fn http_start(&self, request: HttpRequest, _options: Options) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        self.requests.lock().push(request);
        let (status, body) = self
            .replies
            .lock()
            .pop_front()
            .expect("unexpected upstream request");
        Ok(Arc::new(Response {
            status,
            body: Mutex::new(Some(body.into_bytes())),
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
        Err(HostFailure::new(ErrorKind::Trapped, "no websocket"))
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.state.lock().clone())
    }
    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        *self.state.lock() = Some(bytes);
        Ok(())
    }
    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure> {
        self.events.lock().push(event);
        Ok(())
    }
    fn log(&self, _level: LogLevel, message: &str) {
        self.logs.lock().push(message.into());
    }
    fn generation_is_current(&self, _generation: u64) -> bool {
        true
    }
}
async fn load() -> (VendorRuntime, LoadedPlugin) {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/vendor-plugins-all");
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(dir.join("manifest.json")).expect("run task build:vendors:all"),
    )
    .unwrap();
    let entry = manifest
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["vendor_id"] == "muse-code")
        .expect("Muse plugin manifest entry");
    let bytes = std::fs::read(dir.join(entry["file"].as_str().unwrap())).unwrap();
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&bytes).await.unwrap();
    (runtime, plugin)
}
fn provider(channel: &str, authenticated: bool) -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "muse-code".into(),
        channel: channel.into(),
        base_url: "https://api.meta.ai/v1".into(),
        protocol: "open-responses".into(),
        options: BTreeMap::new(),
        credentials: if authenticated {
            BTreeMap::from([("apiKey".into(), json!("inference-key"))])
        } else {
            BTreeMap::new()
        },
        model: Some("muse-spark-1.3".into()),
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    }
}
async fn run(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    services: &Arc<Services>,
    input: OperationInput,
) -> Result<OperationOutput, RuntimeError> {
    let channel = input.provider().channel.clone();
    let (operation, input) = input.encode_for_host().unwrap();
    let scope = OperationScope::new(
        services.clone(),
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(30)),
        0,
    );
    runtime
        .execute(plugin, &channel, operation, input, scope)
        .await
}
async fn auth(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    services: &Arc<Services>,
    step: AuthStep,
) -> Result<AuthResponse, RuntimeError> {
    let out = run(
        runtime,
        plugin,
        services,
        OperationInput::Auth {
            provider: provider("subscription", false),
            request: AuthRequest { step },
        },
    )
    .await?;
    let OperationOutput::Auth(out) = out else {
        panic!("auth output")
    };
    Ok(out)
}
fn start() -> AuthStep {
    AuthStep::Start {
        redirect_uri: String::new(),
        state: "host-state".into(),
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn device_flow_mints_once_and_keeps_account_token_out_of_inference() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    services.device();
    assert!(
        matches!(auth(&runtime, &plugin, &services, start()).await.unwrap(), AuthResponse::Authorization { user_code:Some(code), .. } if code == "ABCD")
    );
    assert!(matches!(
        auth(&runtime, &plugin, &services, AuthStep::Poll)
            .await
            .unwrap(),
        AuthResponse::Pending { .. }
    ));
    assert_eq!(services.requests.lock().len(), 1);
    services.make_poll_due();
    services.reply(200, json!({"access_token":"account-token"}));
    services.reply(200, json!({"api_key":"subscription-key","user_id":"user-1","user_email":"USER@EXAMPLE.COM","is_subs_active":true}));
    let AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    } = auth(&runtime, &plugin, &services, AuthStep::Poll)
        .await
        .unwrap()
    else {
        panic!("credentials")
    };
    assert_eq!(values["apiKey"], "subscription-key");
    assert_eq!(values["oauthAccessToken"], "account-token");
    assert_eq!(expires_at_unix_ms, None);
    assert!(!values.contains_key("refreshToken"));
    assert!(matches!(
        auth(&runtime, &plugin, &services, AuthStep::Poll)
            .await
            .unwrap(),
        AuthResponse::Credentials { .. }
    ));
    let requests = services.requests.lock();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].url,
        "https://auth.meta.com/oidc/device/authorization/"
    );
    assert_eq!(
        String::from_utf8(requests[0].body.clone()).unwrap(),
        "client_id=1031625952748946"
    );
    // Meta 把无 UA 的设备登录重定向到浏览器不支持页；每一步认证请求都必须带 UA。
    for request in requests.iter() {
        assert_eq!(request.headers.get("user-agent"), Some(b"Bun/1.4.3".as_slice()));
    }
    let form: BTreeMap<_, _> = url::form_urlencoded::parse(&requests[1].body)
        .into_owned()
        .collect();
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(form["device_code"], "secret-device");
    assert_eq!(requests[2].url, "https://api.meta.ai/muse-code/key");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[2].body).unwrap(),
        json!({"onboard":true})
    );
    drop(requests);
    assert!(services.logs.lock().is_empty());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn pending_slowdown_denial_expiry_and_entitlement_are_explicit_and_redacted() {
    let (runtime, plugin) = load().await;
    for error in [
        "authorization_pending",
        "slow_down",
        "access_denied",
        "expired_token",
    ] {
        let services = Arc::new(Services::default());
        services.device();
        auth(&runtime, &plugin, &services, start()).await.unwrap();
        services.make_poll_due();
        services.reply(
            400,
            json!({"error":error,"error_description":"secret-device account-token inference-key"}),
        );
        let result = auth(&runtime, &plugin, &services, AuthStep::Poll).await;
        if error == "authorization_pending" || error == "slow_down" {
            assert!(
                matches!(result.unwrap(), AuthResponse::Pending { retry_after_seconds:Some(n) } if n == if error == "slow_down" {6} else {1})
            );
        } else {
            let message = format!("{:?}", result.unwrap_err());
            assert!(!message.contains("secret-device"));
            assert!(!message.contains("account-token"));
        }
        assert_eq!(services.requests.lock().len(), 2);
    }
    for payload in [
        json!({"require_payment":true,"action_url":"https://example.com/account-token"}),
        json!({"api_key":"inference-key","is_subs_active":false}),
        json!({"api_key":"inference-key"}),
    ] {
        let services = Arc::new(Services::default());
        services.device();
        auth(&runtime, &plugin, &services, start()).await.unwrap();
        services.make_poll_due();
        services.reply(200, json!({"access_token":"account-token"}));
        services.reply(200, payload);
        let error = auth(&runtime, &plugin, &services, AuthStep::Poll)
            .await
            .unwrap_err();
        let message = format!("{error:?}");
        assert!(!message.contains("account-token"));
        assert!(!message.contains("inference-key"));
        assert!(
            auth(&runtime, &plugin, &services, AuthStep::Poll)
                .await
                .is_err()
        );
        assert_eq!(services.requests.lock().len(), 3);
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn manual_key_is_rejected_and_discovery_does_not_claim_unknown_model_capabilities() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    assert!(
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Auth {
                provider: provider("subscription", false),
                request: AuthRequest {
                    step: AuthStep::ManualInput {
                        value: "manual-key".into(),
                    },
                },
            },
        )
        .await
        .is_err()
    );
    assert!(services.requests.lock().is_empty());
    services.reply(
        200,
        json!({"data":[{"id":"muse-spark-1.3","created":42},{"id":"future-muse","created":43}]}),
    );
    let output = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Discover {
            provider: provider("subscription", true),
            request: DiscoverRequest::default(),
        },
    )
    .await
    .unwrap();
    let OperationOutput::Discover(result) = output else {
        panic!("discovery")
    };
    assert_eq!(result.models.len(), 2);
    assert_eq!(result.models[0].metadata["limit"]["context"], 1048576);
    assert!(
        result.models[0].metadata["reasoning_efforts"]
            .as_array()
            .unwrap()
            .contains(&json!("max"))
    );
    assert!(result.models[1].capabilities.is_empty());
    assert!(!result.models[1].metadata.contains_key("limit"));
    assert_eq!(result.models[1].metadata["created"], 43);
    assert_eq!(
        services.requests.lock()[0].url,
        "https://api.meta.ai/v1/models"
    );
}

fn native_event(mut event: Value) -> String {
    if let Some(partial) = event.get_mut("response") {
        let mut resource =
            stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                partial["id"].as_str().unwrap(),
                partial["model"].as_str().unwrap_or("muse-spark-1.3"),
                partial["status"].as_str().unwrap(),
                Vec::new(),
                Value::Null,
                Value::Null,
                Value::Null,
            );
        resource["created_at"] = json!(1);
        resource
            .as_object_mut()
            .unwrap()
            .extend(std::mem::take(partial.as_object_mut().unwrap()));
        *partial = resource;
    }
    format!(
        "event: {}\ndata: {event}\n\n",
        event["type"].as_str().unwrap()
    )
}
fn text_stream() -> String {
    [
        json!({"type":"response.created","response":{"id":"resp-1","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg-1","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"msg-1","part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg-1","delta":"ok"}),
        json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"msg-1","text":"ok"}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":"msg-1","part":{"type":"output_text","text":"ok","annotations":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}}),
        json!({"type":"response.completed","response":{"id":"resp-1","status":"completed","output":[{"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}]}}),
    ].into_iter().map(native_event).collect()
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn responses_stream_preserves_tools_results_and_real_encrypted_reasoning() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    services.replies.lock().push_back((200, text_stream()));
    let mut p = provider("subscription", true);
    p.credentials
        .insert("oauthAccessToken".into(), json!("account-token"));
    p.client_headers
        .push(("authorization".into(), "Bearer injected".into()));
    let mut req = AiRequest::new(
        "ignored",
        vec![
            AiItem::reasoning(
                vec!["summary".into()],
                Vec::new(),
                Some("real-encrypted-content".into()),
            ),
            AiItem::function_call(ToolCall {
                id: "call-1".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            }),
            AiItem {
                role: Role::Tool,
                content: MessageContent::Text("result".to_owned().into()),
                tool_calls: None,
                tool_call_id: Some("call-1".into()),
                meta: None,
            },
        ],
    );
    req.tools = Some(vec![ToolSpec {
        name: "lookup".into(),
        description: None,
        parameters: json!({"type":"object","properties":{"query":{"type":["string","null"]}},"required":["query"]}),
        strict: None,
        defer_loading: None,
    }]);
    req.tool_choice = Some(ToolChoice::Required);
    req.reasoning.level = Some(ThinkingLevel::Max);
    let output = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: p,
            request: req,
        },
    )
    .await
    .unwrap();
    let OperationOutput::Infer(response) = output else {
        panic!("inference response")
    };
    assert_eq!(response.output_text(), "ok");
    let requests = services.requests.lock();
    let sent = &requests[0];
    assert_eq!(sent.url, "https://api.meta.ai/v1/responses");
    assert_eq!(sent.headers.get("authorization"), Some(b"Bearer inference-key".as_slice()));
    assert!(sent.headers.iter().all(|field| !field.bytes.windows(b"account-token".len()).any(|w| w == b"account-token") && !field.bytes.windows(b"injected".len()).any(|w| w == b"injected")));
    assert_eq!(sent.headers.get("x-api-version"), Some(b"1.0.0".as_slice()));
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["stream"], true);
    assert!(body.get("tool_choice").is_none());
    assert_eq!(body["reasoning"]["effort"], "max");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "lookup");
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["query"]["type"],
        json!(["string", "null"])
    );
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["encrypted_content"] == "real-encrypted-content")
    );
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "function_call_output" && i["call_id"] == "call-1")
    );
    assert!(
        services
            .events
            .lock()
            .iter()
            .any(|event| matches!(event,RuntimeEvent::Delta(
        stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(text)
        | stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDeltaWithMetadata {text,..}
    ) if text=="ok"))
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn invalid_origins_credentials_refresh_and_upstream_errors_never_fallback() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    let mut p = provider("subscription", true);
    p.base_url = "https://untrusted.example/v1".into();
    assert!(
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Discover {
                provider: p,
                request: DiscoverRequest::default()
            }
        )
        .await
        .is_err()
    );
    let mut p = provider("subscription", false);
    p.options.insert("apiKey".into(), json!("option-key"));
    assert!(
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Discover {
                provider: p,
                request: DiscoverRequest::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        auth(&runtime, &plugin, &services, AuthStep::Refresh)
            .await
            .is_err()
    );
    assert!(services.requests.lock().is_empty());
    for status in [401, 403, 429] {
        services.reply(
            status,
            json!({"error":{"message":"upstream rejected","type":"invalid_request_error"}}),
        );
        assert!(
            run(
                &runtime,
                &plugin,
                &services,
                OperationInput::Discover {
                    provider: provider("subscription", true),
                    request: DiscoverRequest::default()
                }
            )
            .await
            .is_err()
        );
    }
    assert_eq!(services.requests.lock().len(), 3);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn failed_response_stream_remains_a_terminal_failure() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    let sse = native_event(
        json!({"type":"response.created","response":{"id":"resp-fail","status":"in_progress","output":[]}}),
    ) + &native_event(
        json!({"type":"response.failed","response":{"id":"resp-fail","status":"failed","output":[],"error":{"code":"server_error","message":"Meta could not complete this response"}}}),
    );
    services.replies.lock().push_back((200, sse));
    let req = AiRequest::new(
        "muse-spark-1.3",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    let result = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider("subscription", true),
            request: req,
        },
    )
    .await;
    assert_eq!(
        result.unwrap_err().model_error_kind(),
        Some(stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError)
    );
    assert_eq!(services.requests.lock().len(), 1);
    assert!(
        !services.events.lock().is_empty(),
        "stream failure is emitted to the consumer"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn locally_expired_device_code_never_polls_or_onboards() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    services.reply(200,json!({"device_code":"secret-device","user_code":"ABCD","verification_uri":"https://auth.meta.com/device","expires_in":0}));
    auth(&runtime, &plugin, &services, start()).await.unwrap();
    let error = auth(&runtime, &plugin, &services, AuthStep::Poll)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::Plugin {
            kind: ErrorKind::Auth,
            ..
        }
    ));
    assert_eq!(services.requests.lock().len(), 1);
}

fn prompt() -> AiRequest {
    AiRequest::new(
        "ignored",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn meta_projection_omits_foreign_controls_and_keeps_supported_native_values() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    services.replies.lock().push_back((200, text_stream()));
    let mut req = prompt();
    req.generation.max_tokens = Some(200000);
    req.generation.temperature = Some(0.7);
    req.generation.top_p = Some(0.9);
    req.generation.frequency_penalty = Some(0.2);
    req.generation.presence_penalty = Some(0.3);
    req.generation.seed = Some(42);
    req.generation.stop = Some(vec!["END".into()]);
    req.parallel_tool_calls = Some(false);
    req.reasoning.level = Some(ThinkingLevel::Off);
    req.parameters.background = Some(true);
    req.parameters.truncation = Some(stravia_runtime_contract::protocol::ir::Truncation::Auto);
    req.parameters.logprobs = Some(5);
    req.parameters.verbosity = Some(stravia_runtime_contract::protocol::ir::Verbosity::High);
    req.parameters.store = Some(true);
    req.parameters.cache.key = Some("explicit-cache-bucket".into());
    req.parameters.cache.retention =
        Some(stravia_runtime_contract::protocol::ir::CacheRetention::Extended);
    req.parameters.metadata = Some(std::collections::BTreeMap::from([(
        "purpose".into(),
        "contract".into(),
    )]));
    req.parameters.session.id = Some("raw-session".into());
    req.parameters.session.request_id = Some("correlation-only".into());
    let mut parameters = serde_json::to_value(&req.parameters).unwrap();
    parameters["extensions"] = json!({"open-responses":{"arbitrary_control":true}});
    parameters["arbitrary_control"] = json!(true);
    req.parameters = serde_json::from_value(parameters).unwrap();
    req.parameters
        .native_tools
        .push(stravia_runtime_contract::protocol::ir::NativeTool::Custom(
            stravia_runtime_contract::protocol::ir::CustomTool {
                name: "patch".into(),
                description: None,
                defer_loading: None,
                grammar: None,
            },
        ));
    let mut p = provider("subscription", true);
    p.operation_metadata
        .insert("transport_affinity".into(), json!("scoped-session"));
    p.operation_metadata
        .insert("session_affinity".into(), json!("scoped-root"));
    p.client_headers
        .push(("session_id".into(), "injected-session".into()));
    p.client_headers
        .push(("x-request-id".into(), "injected-request".into()));
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: p,
            request: req,
        },
    )
    .await
    .unwrap();
    let requests = services.requests.lock();
    let sent = &requests[0];
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["max_output_tokens"], 131072);
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["frequency_penalty"], 0.2);
    assert_eq!(body["presence_penalty"], 0.3);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["store"], true);
    assert_eq!(body["prompt_cache_key"], "explicit-cache-bucket");
    assert_eq!(body["prompt_cache_retention"], "24h");
    assert_eq!(body["metadata"], json!({"purpose":"contract"}));
    assert_eq!(body["truncation"], "disabled");
    assert_eq!(body["stream"], true);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body["reasoning"].get("effort").is_none());
    assert!(
        body["tools"]
            .as_array()
            .is_none_or(|tools| tools.is_empty())
    );
    for name in [
        "background",
        "seed",
        "stop",
        "top_logprobs",
        "verbosity",
        "client_metadata",
        "conversation",
        "session_id",
        "request_id",
        "arbitrary_control",
    ] {
        assert!(body.get(name).is_none(), "unexpected upstream field {name}");
    }
    assert!(
        body.get("text")
            .is_none_or(|text| text.get("verbosity").is_none())
    );
    assert_eq!(sent.headers.iter().map(|field| field.name.as_str()).collect::<Vec<_>>(),
        vec!["authorization", "x-api-version", "user-agent", "accept-encoding", "content-type", "accept"]);
    let serialized = serde_json::to_string(&body).unwrap();
    for private in [
        "raw-session",
        "correlation-only",
        "private-thread",
        "scoped-session",
        "injected-session",
    ] {
        assert!(!serialized.contains(private));
        assert!(
            !sent
                .headers
                .iter()
                .any(|field| field.bytes.windows(private.len()).any(|bytes| bytes == private.as_bytes()))
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn model_gates_and_continuation_do_not_invent_capabilities_or_mix_replay_modes() {
    let (runtime, plugin) = load().await;
    for (model, max_effort) in [
        ("muse-spark-1.3", true),
        ("muse-spark-1.2", false),
        ("muse-spark-1.3-contributor", false),
        ("future-muse", false),
    ] {
        let services = Arc::new(Services::default());
        services.replies.lock().push_back((200, text_stream()));
        let mut p = provider("subscription", true);
        p.model = Some(model.into());
        let mut req = prompt();
        req.reasoning.level = Some(ThinkingLevel::Max);
        req.generation.temperature = Some(0.8);
        req.generation.max_tokens = Some(1);
        req.parameters.continuation = Some(
            stravia_runtime_contract::protocol::ir::Continuation::PreviousResponse(
                "resp-previous".into(),
            ),
        );
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: p,
                request: req,
            },
        )
        .await
        .unwrap();
        let requests = services.requests.lock();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["reasoning"]["effort"].as_str(),
            max_effort.then_some("max")
        );
        assert_eq!(body.get("temperature").is_some(), model != "future-muse");
        assert_eq!(body["max_output_tokens"], 16);
        assert_eq!(body["previous_response_id"], "resp-previous");
        assert!(body.get("include").is_none());
        assert_eq!(body["store"], false);
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn streaming_function_arguments_and_terminal_usage_reach_the_consumer() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    let events = [
        json!({"type":"response.created","response":{"id":"resp-tools","model":"muse-spark-1.3","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"","status":"in_progress"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc-1","output_index":0,"delta":"{\"query\":"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc-1","output_index":0,"delta":"null}"}),
        json!({"type":"response.function_call_arguments.done","item_id":"fc-1","output_index":0,"arguments":"{\"query\":null}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{\"query\":null}","status":"completed"}}),
        json!({"type":"response.completed","response":{"id":"resp-tools","model":"muse-spark-1.3","status":"completed","output":[{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{\"query\":null}","status":"completed"}],"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":12}}}}),
    ];
    let sse: String = events.into_iter().map(native_event).collect();
    services.replies.lock().push_back((200, sse));
    let output = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider("subscription", true),
            request: prompt(),
        },
    )
    .await
    .unwrap();
    let OperationOutput::Infer(response) = output else {
        panic!("inference output")
    };
    let call = response.tool_calls().next().expect("completed tool call");
    assert_eq!(call.id, "call-1");
    assert_eq!(call.name, "lookup");
    assert_eq!(
        serde_json::from_str::<Value>(&call.arguments).unwrap(),
        json!({"query":null})
    );
    assert_eq!(response.usage.prompt_tokens, 100);
    assert_eq!(response.usage.completion_tokens, 20);
    assert_eq!(response.usage.total_tokens, 120);
    assert_eq!(response.usage.cache_read_tokens, Some(40));
    assert_eq!(response.usage.reasoning_tokens, Some(12));
    let emitted = services.events.lock();
    assert!(
        emitted
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Delta(_)))
    );
    assert!(
        emitted
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Completed))
    );
}
