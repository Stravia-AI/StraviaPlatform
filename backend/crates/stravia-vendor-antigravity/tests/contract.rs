//! 真实 Component + loopback HTTP；不登录 Google，也不访问生产服务。
//! `task build:vendors:all` 后运行本文件的 ignored 测试。
use async_trait::async_trait;
use axum::{
    Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use base64::Engine;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::protocol::{
    ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ir::{AiErrorKind, AiStreamDelta},
};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::host::{HostStateCell, StateError, StateScope, StateSnapshot};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, LoadedPlugin, LogLevel,
    OperationScope, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::{
    AllowanceBilling, AllowanceBillingKind, AllowanceBillingPeriod, AllowanceBillingPrice,
    AllowanceRequest, AuthRequest, AuthResponse, AuthStep, DiscoverRequest, ErrorKind,
    MediaImageAspectRatio, MediaImageRequest, MediaImageResolution, MediaReference, OperationInput,
    OperationOutput, ProviderSnapshot,
};

fn failure(error: reqwest::Error) -> HostFailure {
    HostFailure::new(ErrorKind::upstream_unknown(), error.to_string())
}
struct HttpResponse {
    client: reqwest::Client,
    request: Mutex<Option<reqwest::Request>>,
    response: tokio::sync::OnceCell<tokio::sync::Mutex<reqwest::Response>>,
}
impl HttpResponse {
    async fn response(&self) -> Result<&tokio::sync::Mutex<reqwest::Response>, HostFailure> {
        self.response
            .get_or_try_init(|| async {
                let request = self.request.lock().take().expect("one HTTP request");
                Ok(tokio::sync::Mutex::new(
                    self.client.execute(request).await.map_err(failure)?,
                ))
            })
            .await
    }
}
#[async_trait]
impl HostHttpResponse for HttpResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.response().await?.lock().await.status().as_u16())
    }
    async fn headers(&self) -> Result<Vec<stravia_vendor_sdk::net::Field>, HostFailure> {
        Ok(self
            .response()
            .await?
            .lock()
            .await
            .headers()
            .iter()
            .map(|(key, value)| stravia_vendor_sdk::net::Field {
                name: key.to_string(),
                bytes: value.as_bytes().to_vec(),
            })
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
            .map_err(failure)?
            .map(|bytes| bytes.to_vec()))
    }
}
#[derive(Default)]
struct Fixture {
    received: Mutex<Vec<(String, HeaderMap, Vec<u8>)>>,
    truncated: AtomicBool,
    error_stream: AtomicBool,
    require_call_signature: AtomicBool,
    image_refusal: Mutex<Option<Value>>,
    search_response: Mutex<Option<(StatusCode, Value)>>,
}
async fn upstream(
    State(fixture): State<Arc<Fixture>>,
    OriginalUri(uri): OriginalUri,
    method: axum::http::Method,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let cli_client = headers
        .get("user-agent")
        .is_some_and(|value| value.as_bytes().starts_with(b"antigravity/cli/"));
    fixture
        .received
        .lock()
        .push((uri.to_string(), headers, body.to_vec()));
    let payload = match uri.path() {
        "/token" => {
            let fields: BTreeMap<_, _> = url::form_urlencoded::parse(&body).into_owned().collect();
            if fields
                .get("grant_type")
                .is_some_and(|value| value == "refresh_token")
            {
                json!({"access_token":"refreshed-test-token","expires_in":3600})
            } else {
                if fields
                    .get("code")
                    .is_none_or(|value| value != "test-authorization-code")
                {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({"error":"invalid_grant"})),
                    )
                        .into_response();
                }
                json!({"access_token":"test-token","refresh_token":"test-refresh","expires_in":3600,"token_type":"Bearer"})
            }
        }
        "/v1internal:loadCodeAssist" => {
            // currentTier 只是产品名；订阅在 paidTier（与 live 响应一致）。
            json!({
                "allowedTiers":[{"id":"account-default","isDefault":true}],
                "paidTier":{"id":"g1-pro-tier","name":"Google AI Pro"}
            })
        }
        "/v1internal:onboardUser" => json!({"name":"operations/account-onboarding","done":false}),
        "/v1internal/operations/account-onboarding" => {
            json!({"name":"operations/account-onboarding","done":true,"response":{"cloudaicompanionProject":"test-project"}})
        }
        "/v1internal:fetchAvailableModels" => {
            let mut catalog = json!({
                "defaultAgentModelId": "gemini-3.8-flash-high",
                "agentModelSorts":[{"groups":[{"modelIds":[
                    "account-model", "gemini-pro-agent", "gemini-3.1-pro-low",
                    "gemini-3.8-flash-high", "gemini-3.8-flash-medium", "gemini-3.8-flash-low",
                    "gemini-4-hydrogen-high", "gemini-4-hydrogen-xhigh", "gemini-4-hydrogen-medium", "gemini-4-hydrogen-low",
                    "claude-sonnet-5-5-high", "claude-sonnet-5-5-medium"
                ]}]}],
                "deprecatedModelIds": {"gemini-3.1-pro-high": {"newModelId": "gemini-pro-agent"}},
                "imageGenerationModelIds": ["gemini-3.1-flash-image", "account-image"],
                "webSearchModelIds": ["search-first", "search-consumer", "search-vertex-number", "search-vertex-string"],
                "models":{
                "search-first":{"displayName":"First search"},
                "search-consumer":{"displayName":"Consumer search","apiProvider":1},
                "search-vertex-number":{"displayName":"Vertex search","apiProvider":3},
                "search-vertex-string":{"displayName":"Vertex string search","apiProvider":"API_PROVIDER_GOOGLE_VERTEX"},
                "account-model":{"displayName":"Account model","supportsImages":true,"maxTokens":131072},
                "hidden-model":{"isInternal":true},
                "gemini-pro-agent":{"displayName":"Gemini 3.1 Pro (High)","model":"MODEL_PRO_HIGH","supportsThinking":true,"thinkingBudget":-1},
                "gemini-3.1-pro-high":{"displayName":"Gemini 3.1 Pro (High)","supportsThinking":true},
                "gemini-3.1-pro-low":{"displayName":"Gemini 3.1 Pro (Low)","model":"MODEL_PRO_LOW","supportsThinking":true,"thinkingBudget":1024},
                "gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)","supportsThinking":true,"thinkingLevel":3,"minThinkingBudget":32},
                "gemini-3.8-flash-medium":{"displayName":"Gemini 3.8 Flash (Medium)","supportsThinking":true,"thinkingLevel":2,"minThinkingBudget":32},
                "gemini-3.8-flash-low":{"displayName":"Gemini 3.8 Flash (Low)","supportsThinking":true,"thinkingLevel":1,"minThinkingBudget":32},
                "gemini-4-hydrogen-low":{"displayName":"Gemini 4 Hydrogen (Low)","supportsThinking":true,"thinkingLevel":1,"maxTokens":1048576,"maxOutputTokens":262144},
                "gemini-4-hydrogen-medium":{"displayName":"Gemini 4 Hydrogen (Medium)","supportsThinking":true,"thinkingLevel":2,"maxTokens":1048576,"maxOutputTokens":262144},
                "gemini-4-hydrogen-high":{"displayName":"Gemini 4 Hydrogen (High)","supportsThinking":true,"thinkingLevel":3,"maxTokens":1048576,"maxOutputTokens":262144},
                "gemini-4-hydrogen-xhigh":{"displayName":"Gemini 4 Hydrogen (XHigh)","supportsThinking":true,"thinkingLevel":100,"maxTokens":1048576,"maxOutputTokens":262144},
                "claude-sonnet-5-5-high":{"displayName":"Claude Sonnet 5.5 (High)","supportsThinking":true,"thinkingLevel":3,"maxOutputTokens":128000},
                "claude-sonnet-5-5-medium":{"displayName":"Claude Sonnet 5.5 (Medium)","supportsThinking":true,"thinkingLevel":2,"maxOutputTokens":128000},
                "gemini-3.1-flash-image":{"displayName":"Gemini 3.1 Flash Image"},
                "account-image":{"displayName":"Account image","supportsImages":true},
                "gemini-2.5-pro":{"displayName":"Gemini 2.5 Pro"}
            }});
            // 真实上游按 CLI 客户端身份返回新 Flash 档位，完整目录夹具会漏掉这个边界。
            if !cli_client {
                catalog["defaultAgentModelId"] = json!("gemini-pro-agent");
                catalog["agentModelSorts"][0]["groups"][0]["modelIds"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|id| !id.as_str().unwrap().starts_with("gemini-3.8-flash-"));
                catalog["models"]
                    .as_object_mut()
                    .unwrap()
                    .retain(|id, _| !id.starts_with("gemini-3.8-flash-"));
            }
            catalog
        }
        "/v1internal:retrieveUserQuotaSummary" => {
            json!({"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0.7,"resetTime":"2026-01-01T00:00:00Z"}]}],"buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0.7}]})
        }
        "/v1internal:generateContent" => {
            let request: Value = serde_json::from_slice(&body).unwrap();
            if request["requestType"] == "web_search" {
                assert_eq!(method, axum::http::Method::POST);
                let (status, response) = fixture
                    .search_response
                    .lock()
                    .clone()
                    .expect("search fixture response");
                return (status, [("retry-after", "9")], axum::Json(response)).into_response();
            }
            if fixture.error_stream.load(Ordering::Relaxed) {
                return (
                    [("retry-after", "9")],
                    axum::Json(json!({"error":{"code":429,"message":"rate limit"}})),
                )
                    .into_response();
            }
            if let Some(refusal) = fixture.image_refusal.lock().as_ref() {
                return axum::Json(refusal.clone()).into_response();
            }
            let mut response = json!({"response":{
                "candidates":[{"content":{"role":"model","parts":[{
                    "inlineData":{"mimeType":"image/png","data":IMAGE_PNG}
                }]},"finishReason":"STOP"}],
                "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":13,"totalTokenCount":20}
            }});
            if fixture.truncated.load(Ordering::Relaxed) {
                response["response"]["candidates"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("finishReason");
            }
            response
        }
        "/v1internal:streamGenerateContent" => {
            if fixture.error_stream.load(Ordering::Relaxed) {
                return (
                    [("content-type", "text/event-stream"), ("retry-after", "9")],
                    "data: {\"error\":{\"code\":429,\"message\":\"rate limit\"}}\n\n",
                )
                    .into_response();
            }
            let request: Value = serde_json::from_slice(&body).unwrap();
            if fixture.require_call_signature.load(Ordering::Relaxed) {
                for content in request["request"]["contents"].as_array().unwrap() {
                    if content["role"] != "model" {
                        continue;
                    }
                    if let Some(call) = content["parts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|part| part.get("functionCall").is_some())
                        && call["thoughtSignature"].as_str().is_none_or(str::is_empty)
                    {
                        return (
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({"error":{"code":400,"message":"Function call is missing a thought_signature"}})),
                        ).into_response();
                    }
                }
            }
            let first = "data: {\"response\":{\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"真实 HTTP 冒烟\"}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":8,\"totalTokenCount\":13}}}\r\n\r\n";
            let last = "data: {\"response\":{\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"metadata\":{\"seed\":7}}},\"thoughtSignature\":\"genuine-signature\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":8,\"totalTokenCount\":13}}}\n\n";
            let body = if fixture.truncated.load(Ordering::Relaxed) {
                first.to_owned()
            } else {
                format!("{first}{last}")
            };
            return ([("content-type", "text/event-stream")], body).into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(payload).into_response()
}
// The wire-contract fixture supplies an isolated operation-bound cell. Durable
// checkpoint inheritance and fencing are exercised through Core, not this cell.
struct FixtureCell(Mutex<StateSnapshot>);

#[async_trait]
impl HostStateCell for FixtureCell {
    async fn load(&self) -> Result<StateSnapshot, StateError> {
        Ok(self.0.lock().clone())
    }

    async fn compare_exchange(
        &self,
        revision: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, StateError> {
        let mut state = self.0.lock();
        if state.revision != revision {
            return Ok(false);
        }
        state.revision += 1;
        state.payload = payload;
        Ok(true)
    }
}

struct Services {
    root: String,
    client: reqwest::Client,
    state: Mutex<Option<Vec<u8>>>,
    conversation: Arc<FixtureCell>,
    fail_conversation: AtomicBool,
    deltas: Mutex<Vec<AiStreamDelta>>,
}
#[async_trait]
impl HostServices for Services {
    fn validate_net_option(
        &self,
        key: &str,
        value: &str,
    ) -> Result<(), stravia_vendor_sdk::net::OptionError> {
        stravia_vendor_sdk::net::validate_host_option(key, value)
    }
    fn http_start(
        &self,
        request: stravia_vendor_sdk::net::Request,
        _options: stravia_vendor_sdk::net::Options,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let url = url::Url::parse(&request.url).unwrap();
        assert!(matches!(
            url.host_str(),
            Some(
                "daily-cloudcode-pa.googleapis.com"
                    | "oauth2.googleapis.com"
                    | "unverified.example"
            )
        ));
        let mut local = format!("{}{}", self.root, url.path());
        if let Some(query) = url.query() {
            local.push('?');
            local.push_str(query);
        }
        let mut builder = self
            .client
            .request(
                reqwest::Method::from_bytes(request.method.as_bytes()).unwrap(),
                local,
            )
            .body(request.body);
        for field in request.headers.into_vec() {
            if field.bytes.is_empty()
                && (field.name.eq_ignore_ascii_case("host")
                    || field.name.eq_ignore_ascii_case("content-length"))
            {
                continue;
            }
            builder = builder.header(
                field.name,
                reqwest::header::HeaderValue::from_bytes(&field.bytes).unwrap(),
            );
        }
        Ok(Arc::new(HttpResponse {
            client: self.client.clone(),
            request: Mutex::new(Some(builder.build().map_err(failure)?)),
            response: tokio::sync::OnceCell::new(),
        }))
    }
    async fn ws_connect(
        &self,
        _: stravia_vendor_sdk::net::ConnectRequest,
        _: stravia_vendor_sdk::net::Options,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Unsupported,
            "HTTP-only fixture",
        ))
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.state.lock().clone())
    }
    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        *self.state.lock() = Some(bytes);
        Ok(())
    }
    async fn open_state(
        &self,
        scope: StateScope,
        _key: String,
    ) -> Result<Arc<dyn HostStateCell>, StateError> {
        if scope != StateScope::Conversation || self.fail_conversation.load(Ordering::Relaxed) {
            return Err(StateError::new(
                "unsupported",
                "conversation state unavailable",
            ));
        }
        Ok(self.conversation.clone())
    }
    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure> {
        if let RuntimeEvent::Delta(delta) = event {
            self.deltas.lock().push(delta);
        }
        Ok(())
    }
    fn log(&self, _: LogLevel, _: &str) {}
    fn generation_is_current(&self, _: u64) -> bool {
        true
    }
}
fn provider() -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "antigravity".into(),
        channel: "oauth".into(),
        base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
        protocol: "google-gemini".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: Some("account-model".into()),
        model_metadata: None,
        client_headers: vec![("x-downstream-secret".into(), "must-not-forward".into())],
        operation_metadata: BTreeMap::new(),
    }
}
async fn run(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    services: &Arc<Services>,
    mut input: OperationInput,
) -> Result<OperationOutput, stravia_vendor_runtime::RuntimeError> {
    // Each independent wire-shape request is a fresh admitted conversation.
    if let OperationInput::Infer { provider, .. } = &mut input
        && !provider.operation_metadata.contains_key("model_turn_id")
    {
        *services.conversation.0.lock() = StateSnapshot {
            revision: 0,
            payload: None,
        };
        for key in [
            "model_turn_id",
            "logical_turn_id",
            "client_conversation_id",
            "context_window_id",
        ] {
            provider
                .operation_metadata
                .insert(key.into(), json!(uuid::Uuid::new_v4().to_string()));
        }
        provider
            .operation_metadata
            .insert("context_window_index".into(), json!(0));
        provider
            .operation_metadata
            .insert("user_prompt_index".into(), json!(1));
        provider
            .operation_metadata
            .insert("model_turn_index".into(), json!(0));
        provider
            .operation_metadata
            .insert("model_turn_mixed_providers".into(), json!(false));
    }
    let (operation, encoded) = input
        .encode_for_host()
        .map_err(|_| stravia_vendor_runtime::RuntimeError::InvalidOutput)?;
    drop(input);
    runtime
        .execute(
            plugin,
            "oauth",
            operation,
            encoded,
            OperationScope::new(
                Arc::clone(services) as Arc<dyn HostServices>,
                CancellationToken::new(),
                Deadline::from_now(Duration::from_secs(60)),
                0,
            ),
        )
        .await
}
async fn auth(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    services: &Arc<Services>,
    provider: &ProviderSnapshot,
    step: AuthStep,
) -> AuthResponse {
    let OperationOutput::Auth(output) = run(
        runtime,
        plugin,
        services,
        OperationInput::Auth {
            provider: provider.clone(),
            request: AuthRequest { step },
        },
    )
    .await
    .unwrap() else {
        panic!("auth output")
    };
    output
}
fn artifact() -> std::path::PathBuf {
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
        .find(|entry| entry["vendor_id"] == "antigravity")
        .unwrap();
    dir.join(entry["file"].as_str().unwrap())
}

const IMAGE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
const IMAGE_POLICY_MESSAGE: &str = "Unable to show the generated image. The image was filtered out because it violated Google's [Generative AI Prohibited Use policy](https://policies.google.com/terms/generative-ai/use-policy). You will not be charged for blocked images. Try rephrasing the prompt. If you think this was an error, [send feedback](https://ai.google.dev/gemini-api/docs/troubleshooting).";

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn trusted_model_turn_gap_marks_lifecycle_counts_unknown() {
    let fixture = Arc::new(Fixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let services = Arc::new(Services {
        root: format!("http://{}", listener.local_addr().unwrap()),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        state: Mutex::new(None),
        conversation: Arc::new(FixtureCell(Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }))),
        fail_conversation: AtomicBool::new(false),
        deltas: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .fallback(upstream)
        .with_state(Arc::clone(&fixture));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let mut provider = provider();
    provider.credentials = BTreeMap::from([
        ("access_token".into(), json!("test-token")),
        ("project_id".into(), json!("test-project")),
    ]);
    provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
        extensions: BTreeMap::from([(
            "antigravity".into(),
            json!({"default":"account-model","variants":[{"id":"account-model","metadata":{"modelProvider":4}}]}),
        )]),
        ..Default::default()
    });
    for key in [
        "model_turn_id",
        "logical_turn_id",
        "client_conversation_id",
        "context_window_id",
    ] {
        provider
            .operation_metadata
            .insert(key.into(), json!(uuid::Uuid::new_v4().to_string()));
    }
    provider
        .operation_metadata
        .insert("context_window_index".into(), json!(0));
    provider
        .operation_metadata
        .insert("user_prompt_index".into(), json!(1));
    provider
        .operation_metadata
        .insert("model_turn_index".into(), json!(0));
    provider
        .operation_metadata
        .insert("model_turn_mixed_providers".into(), json!(false));
    let endpoint = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let request = ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .decode_request(json!({"contents":[{"role":"user","parts":[{"text":"first"}]}]}))
        .unwrap();
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request,
        },
    )
    .await
    .unwrap();
    provider.operation_metadata.insert(
        "model_turn_id".into(),
        json!(uuid::Uuid::new_v4().to_string()),
    );
    provider.operation_metadata.insert(
        "logical_turn_id".into(),
        json!(uuid::Uuid::new_v4().to_string()),
    );
    provider
        .operation_metadata
        .insert("user_prompt_index".into(), json!(2));
    // One intervening generation was routed to another provider, outside this cell.
    provider
        .operation_metadata
        .insert("model_turn_index".into(), json!(2));
    provider
        .operation_metadata
        .insert("model_turn_mixed_providers".into(), json!(true));
    let request = ProtocolTransform::global().bind(endpoint, endpoint).unwrap()
        .decode_request(json!({"contents":[{"role":"user","parts":[{"text":"first"}]},{"role":"user","parts":[{"text":"after other provider"}]}]})).unwrap();
    run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer { provider, request },
    )
    .await
    .unwrap();
    let received = fixture.received.lock();
    let first: Value = serde_json::from_slice(&received[0].2).unwrap();
    let last: Value = serde_json::from_slice(&received[1].2).unwrap();
    assert!(
        first["request"]["labels"]["request_id"]
            .as_str()
            .unwrap()
            .ends_with("-0")
    );
    let labels = &last["request"]["labels"];
    assert!(labels.get("request_id").is_none());
    assert!(labels.get("last_step_index").is_none());
    assert_eq!(labels["used_claude_conservative"], "true");
    assert_eq!(labels["used_non_gemini_model"], "true");
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn search_discovery_grounding_and_errors_at_real_http_boundary() {
    let fixture = Arc::new(Fixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = Router::new()
        .fallback(upstream)
        .with_state(Arc::clone(&fixture));
    let services = Arc::new(Services {
        root: format!("http://{}", listener.local_addr().unwrap()),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        state: Mutex::new(None),
        conversation: Arc::new(FixtureCell(Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }))),
        fail_conversation: AtomicBool::new(false),
        deltas: Mutex::new(Vec::new()),
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let mut provider = provider();
    provider.credentials = BTreeMap::from([
        ("access_token".into(), json!("test-token")),
        ("project_id".into(), json!("test-project")),
    ]);
    let OperationOutput::Discover(catalog) = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Discover {
            provider: provider.clone(),
            request: DiscoverRequest::default(),
        },
    )
    .await
    .unwrap() else {
        panic!("catalog")
    };
    let request = stravia_vendor_sdk::SearchRequest {
        query: "中文查询".into(),
        continuation: None,
        allowed_domains: vec!["example.com".into(), "example.org".into()],
        language: Some("zh-CN".into()),
        max_sources: Some(3),
    };
    let response = json!({"response":{"candidates":[{
        "finishReason":"STOP",
        "content":{"role":"model","parts":[
            {"thought":true,"text":"hidden thought"}, {"text":"中文答案"}, {"text":"第二段"}
        ]},
        "groundingMetadata":{
            "groundingChunks":[
                {"web":{"uri":"https://example.com/a","title":"来源一"}},
                {"web":{"uri":"https://example.com/a","title":"重复"}},
                {"web":{"uri":"https://example.org/b","title":"来源二"}}
            ],
            "groundingSupports":[
                {"segment":{"partIndex":1,"startIndex":0,"endIndex":6,"text":"中文"},"groundingChunkIndices":[0,1]},
                {"segment":{"partIndex":2,"startIndex":0,"endIndex":9,"text":"第二段"},"groundingChunkIndices":[2]}
            ]
        }
    }]}});
    *fixture.search_response.lock() = Some((StatusCode::OK, response.clone()));
    for (id, api_provider, tool) in [
        (
            "search-first",
            None,
            json!({"googleSearch":{"enhancedContent":{"imageSearch":{"maxResultCount":5}}}}),
        ),
        (
            "search-consumer",
            Some(json!(1)),
            json!({"googleSearch":{"enhancedContent":{"imageSearch":{"maxResultCount":5}}}}),
        ),
        (
            "search-vertex-number",
            Some(json!(3)),
            json!({"enterpriseWebSearch":{}}),
        ),
        (
            "search-vertex-string",
            Some(json!("API_PROVIDER_GOOGLE_VERTEX")),
            json!({"enterpriseWebSearch":{}}),
        ),
    ] {
        let model = catalog.models.iter().find(|model| model.id == id).unwrap();
        assert!(
            model
                .capabilities
                .iter()
                .any(|capability| capability == "search")
        );
        assert_eq!(model.metadata["antigravity_search"]["model_id"], id);
        assert_eq!(
            model.metadata["antigravity_search"].get("api_provider"),
            api_provider.as_ref()
        );
        // The inference selector and provider.model must not override the discovered search binding.
        provider.model = Some("account-model".into());
        provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            selector: Some("wrong-infer-selector".into()),
            extensions: BTreeMap::from([(
                "antigravity_search".into(),
                model.metadata["antigravity_search"].clone(),
            )]),
            ..Default::default()
        });
        let OperationOutput::Search(output) = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Search {
                provider: provider.clone(),
                request: request.clone(),
            },
        )
        .await
        .unwrap() else {
            panic!("search output")
        };
        assert_eq!(output.answer, "中文[sc:g1]答案第二段[sc:g2]");
        assert_eq!(output.sources.len(), 2);
        assert_eq!(output.sources[0].id, "g1");
        assert_eq!(output.sources[0].url, "https://example.com/a");
        assert_eq!(output.sources[0].title.as_deref(), Some("来源一"));
        assert_eq!(output.sources[1].id, "g2");
        assert_eq!(output.sources[1].url, "https://example.org/b");
        for source in &output.sources {
            assert!(output.answer.contains(&format!("[sc:{}]", source.id)));
        }
        assert!(output.usage.is_none());
        let received = fixture.received.lock();
        let (path, headers, body) = received.last().unwrap();
        assert_eq!(path, "/v1internal:generateContent");
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert!(!headers.contains_key("x-downstream-secret"));
        let envelope: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(envelope["project"], "test-project");
        assert_eq!(envelope["model"], id);
        assert_eq!(envelope["requestType"], "web_search");
        assert_eq!(envelope["request"]["contents"][0]["role"], "user");
        assert_eq!(
            envelope["request"]["contents"][0]["parts"][0]["text"],
            "中文查询 (site:example.com OR site:example.org)"
        );
        assert_eq!(envelope["request"]["systemInstruction"]["role"], "user");
        assert_eq!(
            envelope["request"]["generationConfig"],
            json!({"candidateCount":1})
        );
        assert_eq!(envelope["request"]["tools"], json!([tool]));
    }
    let before = fixture.received.lock().len();
    let mut missing = provider.clone();
    missing.model_metadata = None;
    let error = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Search {
            provider: missing,
            request: request.clone(),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        stravia_vendor_runtime::RuntimeError::Plugin {
            kind: ErrorKind::Invalid,
            ..
        }
    ));
    assert_eq!(fixture.received.lock().len(), before);
    let mut no_grounding = response.clone();
    no_grounding["response"]["candidates"][0]
        .as_object_mut()
        .unwrap()
        .remove("groundingMetadata");
    let mut invalid_utf8 = response.clone();
    invalid_utf8["response"]["candidates"][0]["groundingMetadata"]["groundingSupports"][0]["segment"]
        ["startIndex"] = json!(1);
    let mut out_of_bounds = response.clone();
    out_of_bounds["response"]["candidates"][0]["groundingMetadata"]["groundingSupports"][0]["segment"]
        ["endIndex"] = json!(99);
    let mut out_of_range = response.clone();
    out_of_range["response"]["candidates"][0]["groundingMetadata"]["groundingSupports"][0]["groundingChunkIndices"] =
        json!([99]);
    let mut max_tokens = response.clone();
    max_tokens["response"]["candidates"][0]["finishReason"] = json!("MAX_TOKENS");
    for bad in [
        no_grounding,
        invalid_utf8,
        out_of_bounds,
        out_of_range,
        max_tokens,
    ] {
        *fixture.search_response.lock() = Some((StatusCode::OK, bad));
        let error = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Search {
                provider: provider.clone(),
                request: request.clone(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, stravia_vendor_runtime::RuntimeError::Plugin { kind, .. } if kind.model_error_kind() == Some(AiErrorKind::ServerError))
        );
    }
    for (status, expected) in [
        (StatusCode::UNAUTHORIZED, AiErrorKind::AuthenticationError),
        (StatusCode::TOO_MANY_REQUESTS, AiErrorKind::RateLimitError),
    ] {
        *fixture.search_response.lock() = Some((
            status,
            json!({"error":{"message":"upstream rejected request"}}),
        ));
        let error = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Search {
                provider: provider.clone(),
                request: request.clone(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, stravia_vendor_runtime::RuntimeError::Plugin { kind, .. } if kind.model_error_kind() == Some(expected))
        );
    }
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn native_images_preserve_selected_model_and_references_at_real_http_boundary() {
    let fixture = Arc::new(Fixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = Router::new()
        .fallback(upstream)
        .with_state(Arc::clone(&fixture));
    let services = Arc::new(Services {
        root: format!("http://{}", listener.local_addr().unwrap()),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        state: Mutex::new(None),
        conversation: Arc::new(FixtureCell(Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }))),
        fail_conversation: AtomicBool::new(false),
        deltas: Mutex::new(Vec::new()),
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let mut provider = provider();
    provider.credentials = BTreeMap::from([
        ("access_token".into(), json!("test-token")),
        ("project_id".into(), json!("test-project")),
    ]);
    let OperationOutput::Discover(catalog) = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Discover {
            provider: provider.clone(),
            request: DiscoverRequest { cursor: None },
        },
    )
    .await
    .unwrap() else {
        panic!("catalog")
    };
    let model = catalog
        .models
        .iter()
        .find(|model| model.id == "account-image")
        .unwrap();
    assert_eq!(model.capabilities, ["media_image"]);
    assert_eq!(
        model.metadata["modalities"],
        json!({"input":["text","image"],"output":["image"]})
    );
    provider.model = Some(model.id.clone());
    provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
        selector: model.selector.clone(),
        ..Default::default()
    });
    let png = base64::engine::general_purpose::STANDARD
        .decode(IMAGE_PNG)
        .unwrap();
    for references in [
        Vec::new(),
        vec![
            MediaReference {
                media_type: "image/png".into(),
                bytes: png.clone(),
            },
            MediaReference {
                media_type: "image/webp".into(),
                bytes: b"second reference bytes".to_vec(),
            },
        ],
    ] {
        let request = MediaImageRequest {
            prompt: "Draw a lighthouse using the references in order".into(),
            references: references.clone(),
            aspect_ratio: Some(MediaImageAspectRatio::Wide),
            resolution: Some(MediaImageResolution::TwoK),
        };
        let OperationOutput::MediaImage(output) = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::MediaImage {
                provider: provider.clone(),
                request,
            },
        )
        .await
        .unwrap() else {
            panic!("image output")
        };
        assert_eq!(output.artifacts.len(), 1);
        assert_eq!(output.artifacts[0].media_type, "image/png");
        assert_eq!(output.artifacts[0].bytes, png);
        let usage = output.usage.as_ref().unwrap();
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 13);
        let received = fixture.received.lock();
        let (url, headers, bytes) = received.last().unwrap();
        assert_eq!(url, "/v1internal:generateContent");
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert!(!headers.contains_key("transfer-encoding"));
        assert!(!headers.contains_key("x-downstream-secret"));
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(envelope["model"], "account-image");
        assert_eq!(envelope["project"], "test-project");
        assert_eq!(envelope["requestType"], "image_gen");
        assert!(
            envelope["requestId"]
                .as_str()
                .unwrap()
                .starts_with("image_gen/")
        );
        assert!(envelope["request"].get("systemInstruction").is_none());
        assert_eq!(
            envelope["request"]["generationConfig"],
            json!({
                "candidateCount":1,
                "imageConfig":{"aspectRatio":"16:9","imageSize":"2K"}
            })
        );
        let parts = envelope["request"]["contents"][0]["parts"]
            .as_array()
            .unwrap();
        assert!(parts[0].get("text").is_some());
        let returned_references: Vec<_> = parts
            .iter()
            .filter_map(|part| part.get("inlineData"))
            .collect();
        assert_eq!(returned_references.len(), references.len());
        for (part, reference) in returned_references.iter().zip(&references) {
            assert_eq!(part["mimeType"], reference.media_type);
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(part["data"].as_str().unwrap())
                    .unwrap(),
                reference.bytes,
            );
        }
    }
    let request = MediaImageRequest {
        prompt: "Draw a lighthouse".into(),
        references: Vec::new(),
        aspect_ratio: None,
        resolution: None,
    };
    for refusal in [
        json!({"response":{"candidates":[{"content":{},"finishMessage":IMAGE_POLICY_MESSAGE}],"usageMetadata":{"promptTokenCount":398,"totalTokenCount":398},"modelVersion":"gemini-3.1-flash-image"}}),
        json!({"response":{"candidates":[{"finishReason":"IMAGE_SAFETY","finishMessage":IMAGE_POLICY_MESSAGE}]}}),
        json!({"response":{"promptFeedback":{"blockReason":"PROHIBITED_CONTENT","blockReasonMessage":IMAGE_POLICY_MESSAGE}}}),
    ] {
        *fixture.image_refusal.lock() = Some(refusal);
        let error = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::MediaImage {
                provider: provider.clone(),
                request: request.clone(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.model_error_kind(), Some(AiErrorKind::ContentFiltered));
        assert_eq!(error.diagnostic_message(), Some(IMAGE_POLICY_MESSAGE));
    }
    let failure_message = "The upstream renderer could not complete this image request";
    *fixture.image_refusal.lock() = Some(json!({"response":{"candidates":[{
        "finishReason":"OTHER","finishMessage":failure_message
    }]}}));
    let error = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::MediaImage {
            provider: provider.clone(),
            request: request.clone(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.diagnostic_message(), Some(failure_message));
    assert_ne!(error.model_error_kind(), Some(AiErrorKind::ContentFiltered));
    *fixture.image_refusal.lock() =
        Some(json!({"response":{"candidates":[{"content":{},"finishMessage":failure_message}]}}));
    let error = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::MediaImage {
            provider: provider.clone(),
            request: request.clone(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.model_error_kind(), Some(AiErrorKind::ServerError));
    assert_eq!(error.diagnostic_message(), Some(failure_message));
    *fixture.image_refusal.lock() = None;
    fixture.truncated.store(true, Ordering::Relaxed);
    let error = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::MediaImage {
            provider: provider.clone(),
            request: request.clone(),
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, stravia_vendor_runtime::RuntimeError::Plugin {kind, ..}
        if kind.model_error_kind() == Some(AiErrorKind::UnexpectedEof))
    );
    fixture.error_stream.store(true, Ordering::Relaxed);
    let error = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::MediaImage { provider, request },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, stravia_vendor_runtime::RuntimeError::Plugin {kind, ..}
        if kind.model_error_kind() == Some(AiErrorKind::RateLimitError))
    );
    assert_eq!(error.retry_after(), Some(Duration::from_secs(9)));
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn imported_tool_history_continues_without_replacing_native_signatures() {
    let fixture = Arc::new(Fixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = Router::new()
        .fallback(upstream)
        .with_state(Arc::clone(&fixture));
    let services = Arc::new(Services {
        root: format!("http://{}", listener.local_addr().unwrap()),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        state: Mutex::new(None),
        conversation: Arc::new(FixtureCell(Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }))),
        fail_conversation: AtomicBool::new(false),
        deltas: Mutex::new(Vec::new()),
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    for (signature, unverified) in [
        (None, false),
        (Some("opaque-native+/=="), false),
        (None, true),
    ] {
        fixture
            .require_call_signature
            .store(!unverified, Ordering::Relaxed);
        let mut provider = provider();
        provider.model = Some("gemini-3.8-flash".into());
        if unverified {
            provider.base_url = "https://unverified.example".into();
        }
        provider.credentials = BTreeMap::from([
            ("access_token".into(), json!("test-token")),
            ("project_id".into(), json!("test-project")),
        ]);
        let mut body = json!({
            "model": "gemini-3.8-flash",
            "contents": [
                {"role":"user","parts":[{"text":"Add both lookup results."}]},
                {"role":"model","parts":[
                    {"functionCall":{"id":"a","name":"lookup","args":{"key":"A"}}},
                    {"functionCall":{"id":"b","name":"lookup","args":{"key":"B"}}}
                ]},
                {"role":"user","parts":[
                    {"functionResponse":{"id":"a","name":"lookup","response":{"value":1}}},
                    {"functionResponse":{"id":"b","name":"lookup","response":{"value":2}}}
                ]}
            ],
            "tools":[{"functionDeclarations":[{"name":"lookup","parameters":{"type":"OBJECT"}}]}]
        });
        if let Some(signature) = signature {
            body["contents"][1]["parts"][0]["thoughtSignature"] = json!(signature);
        }
        let mut expected = body["contents"].clone();
        if !unverified {
            expected[1]["parts"][0]["thoughtSignature"] =
                json!(signature.unwrap_or("skip_thought_signature_validator"));
        }
        let request = ProtocolTransform::global()
            .bind(
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            )
            .unwrap()
            .decode_request(body)
            .unwrap();
        let result = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer { provider, request },
        )
        .await;
        let OperationOutput::Infer(output) = result.unwrap() else {
            panic!("inference output")
        };
        assert_eq!(output.output_text(), "真实 HTTP 冒烟");
        let received = fixture.received.lock();
        let envelope: Value = serde_json::from_slice(&received.last().unwrap().2).unwrap();
        assert_eq!(envelope["request"]["contents"], expected);
        let labels = &envelope["request"]["labels"];
        assert!(labels.get("request_id").is_none());
        assert!(labels.get("last_step_index").is_none());
        assert_eq!(labels["used_claude"], "false");
        assert_eq!(labels["used_claude_conservative"], "true");
        assert_eq!(labels["used_non_gemini_model"], "true");
    }
    fixture
        .require_call_signature
        .store(false, Ordering::Relaxed);
    services.fail_conversation.store(true, Ordering::Relaxed);
    let mut provider = provider();
    provider.credentials = BTreeMap::from([
        ("access_token".into(), json!("test-token")),
        ("project_id".into(), json!("test-project")),
    ]);
    let request = ProtocolTransform::global()
        .bind(
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        )
        .unwrap()
        .decode_request(json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}))
        .unwrap();
    let before = fixture.received.lock().len();
    assert!(
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer { provider, request }
        )
        .await
        .is_err()
    );
    assert_eq!(
        fixture.received.lock().len(),
        before,
        "state failure must precede inference HTTP"
    );
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn boolean_tool_schemas_preserve_constraints_at_real_http_boundary() {
    let fixture = Arc::new(Fixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = Router::new()
        .fallback(upstream)
        .with_state(Arc::clone(&fixture));
    let services = Arc::new(Services {
        root: format!("http://{}", listener.local_addr().unwrap()),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        state: Mutex::new(None),
        conversation: Arc::new(FixtureCell(Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }))),
        fail_conversation: AtomicBool::new(false),
        deltas: Mutex::new(Vec::new()),
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let mut provider = provider();
    provider.credentials = BTreeMap::from([
        ("access_token".into(), json!("test-token")),
        ("project_id".into(), json!("test-project")),
    ]);
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "$defs": {"Port": {"type": ["integer", "null"], "minimum": 1}},
        "properties": {
            "ready": {"anyOf": [
                {"type": "object", "properties": {"port": {"$ref": "#/$defs/Port"}},
                 "required": ["port"], "additionalProperties": false},
                {"type": "null"}
            ]},
            "toggle": {"anyOf": [{"type": "boolean"}, {"type": "null"}]},
            "task": {"anyOf": [
                {"type": "string"}, {"type": "array", "items": {"type": "string"}},
                {"type": "null"}
            ]},
            "model": {"not": true},
            "allowed": true,
            "forbidden": false,
            "values": {"items": false},
            "choice": {"anyOf": [false, {"not": false}]}
        },
        "required": ["values"],
        "additionalProperties": false,
        "default": {"not": true, "flag": false}
    });
    let endpoint = stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
    let request = ProtocolTransform::global()
        .bind(endpoint, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        .unwrap()
        .decode_request(json!({
            "model": "account-model",
            "input": "Use task without a model override",
            "tools": [{"type": "function", "name": "task", "parameters": schema}]
        }))
        .unwrap();
    let result = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer { provider, request },
    )
    .await;
    server.abort();
    let OperationOutput::Infer(output) = result.unwrap() else {
        panic!("inference output")
    };
    assert_eq!(output.output_text(), "真实 HTTP 冒烟");
    let received = fixture.received.lock();
    let (_, _, bytes) = received
        .iter()
        .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
        .unwrap();
    let envelope: Value = serde_json::from_slice(bytes).unwrap();
    let declaration = &envelope["request"]["tools"][0]["functionDeclarations"][0];
    assert!(declaration.get("parameters").is_none());
    let parameters = &declaration["parametersJsonSchema"];
    assert_eq!(parameters, &schema);
    let validator = jsonschema::validator_for(parameters).unwrap();
    for (arguments, valid) in [
        (json!({"values": []}), true),
        (json!({"values": [], "allowed": {"flag": false}}), true),
        (json!({"values": [], "choice": false}), true),
        (
            json!({"values": [], "ready": null, "toggle": null, "task": null}),
            true,
        ),
        (
            json!({"values": [], "ready": {"port": null}, "toggle": false, "task": "read"}),
            true,
        ),
        (
            json!({"values": [], "ready": {"port": 8080}, "task": ["read", "edit"]}),
            true,
        ),
        (json!({"values": [], "ready": {"port": 0}}), false),
        (json!({"values": [], "ready": {"port": "8080"}}), false),
        (
            json!({"values": [], "ready": {"port": 8080, "extra": true}}),
            false,
        ),
        (json!({"values": [], "ready": {}}), false),
        (json!({"values": [], "toggle": "false"}), false),
        (json!({"values": [], "task": [false]}), false),
        (json!({"values": [], "task": 1}), false),
        (json!({"values": [], "model": "override"}), false),
        (json!({"values": [], "model": null}), false),
        (json!({"values": [], "forbidden": false}), false),
        (json!({"values": [null]}), false),
        (json!({"values": [], "unknown": true}), false),
        (json!({}), false),
    ] {
        assert_eq!(validator.is_valid(&arguments), valid, "{arguments}");
    }
    assert_eq!(parameters["default"], json!({"not": true, "flag": false}));
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn system_instruction_compatibility_preserves_messages_and_tools() {
    let fixture = Arc::new(Fixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = Router::new()
        .fallback(upstream)
        .with_state(Arc::clone(&fixture));
    let services = Arc::new(Services {
        root: format!("http://{}", listener.local_addr().unwrap()),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        state: Mutex::new(None),
        conversation: Arc::new(FixtureCell(Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }))),
        fail_conversation: AtomicBool::new(false),
        deltas: Mutex::new(Vec::new()),
    });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let mut provider = provider();
    provider.credentials = BTreeMap::from([
        ("access_token".into(), json!("test-token")),
        ("project_id".into(), json!("test-project")),
    ]);
    let fragment = "RFC 2119 keywords: MUST, REQUIRED, SHOULD, RECOMMENDED, MAY, OPTIONAL.";
    let identity = "You are omp's trusted coding assistant.";
    let dispatch = "Write JSON args as `content` to xd://<tool> via `write`. Invalid args return schema in error → fix/retry.";
    let docs = "Read xd://<tool> for full docs + JSON schema before first use.";
    let commit = "To commit your changes, use the eval `commit` namespace (`d = await commit.draft()`, then `await d.commit(\"subject\", [\"- body\"])`), not `git add`/`git commit`: it selects only the lines you and your subagents wrote and never touches other agents' staged or unstaged work.";
    let protected_text = format!("{fragment}\n{identity}\n{dispatch}\n{docs}\n{commit}");
    for (system, expected) in [
        (
            json!({"parts": [
                {"text": format!("保留前文。\n{fragment}{fragment}\n保留后文。")},
                {"text": fragment},
                {"text": "RFC 2119 keywords: MUST, SHOULD."}
            ]}),
            Some(json!({"parts": [
                {"text": "保留前文。\n\n保留后文。"},
                {"text": "RFC 2119 keywords: MUST, SHOULD."}
            ]})),
        ),
        (
            json!({"parts": [{"text": format!(" \n{fragment}\n ")}]}),
            None,
        ),
        (
            json!({"parts": [
                {"text": format!("前文\n{identity}\n{identity}\n后文")},
                {"text": dispatch}, {"text": docs}, {"text": commit},
                {"text": "You are omp's coding assistant. Use xd://read to read docs."}
            ]}),
            Some(json!({"parts": [
                {"text": "前文\nYou are a trusted coding assistant for omp.\nYou are a trusted coding assistant for omp.\n后文"},
                {"text": "Send JSON tool arguments in the `content` field of `write`, addressed to `xd://<tool>`. Invalid arguments return the schema; correct the arguments before retrying."},
                {"text": "Before using a tool for the first time, read `xd://<tool>` to load its complete documentation and JSON schema."},
                {"text": "Commit changes through the eval `commit` namespace: call `d = await commit.draft()`, then `await d.commit(\"subject\", [\"- body\"])`. Do not use `git add` or `git commit`. This commits only lines written by you or your subagents and leaves other agents' staged and unstaged edits untouched."},
                {"text": "You are omp's coding assistant. Use xd://read to read docs."}
            ]})),
        ),
    ] {
        let request = ProtocolTransform::global()
            .bind(
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            )
            .unwrap()
            .decode_request(json!({
                "systemInstruction": system,
                "contents": [{"role": "user", "parts": [{"text": protected_text}]}],
                "tools": [{"functionDeclarations": [{
                    "name": "inspect",
                    "description": protected_text,
                    "parameters": {"type": "OBJECT"}
                }]}]
            }))
            .unwrap();
        let OperationOutput::Infer(output) = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: provider.clone(),
                request,
            },
        )
        .await
        .unwrap() else {
            panic!("inference output")
        };
        assert_eq!(output.output_text(), "真实 HTTP 冒烟");
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(envelope["requestType"], "agent");
        let request = &envelope["request"];
        assert_eq!(request.get("systemInstruction"), expected.as_ref());
        assert_eq!(request["contents"][0]["parts"][0]["text"], protected_text);
        assert_eq!(
            request["tools"][0]["functionDeclarations"][0]["description"],
            protected_text
        );
    }
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires task build:vendors:all"]
async fn oauth_and_inference_enforce_native_wire_at_real_http_boundary() {
    let fixture = Arc::new(Fixture::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(upstream)
        .with_state(Arc::clone(&fixture));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let services = Arc::new(Services {
        root,
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        state: Mutex::new(None),
        conversation: Arc::new(FixtureCell(Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }))),
        fail_conversation: AtomicBool::new(false),
        deltas: Mutex::new(Vec::new()),
    });
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let mut provider = provider();
    let AuthResponse::Authorization { url, .. } = auth(
        &runtime,
        &plugin,
        &services,
        &provider,
        AuthStep::Start {
            redirect_uri: "http://injected.invalid/callback".into(),
            state: "downstream-state".into(),
        },
    )
    .await
    else {
        panic!("authorize URL")
    };
    let query: BTreeMap<_, _> = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(
        query["redirect_uri"],
        "https://antigravity.google/oauth-callback"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    assert_ne!(query["state"], "downstream-state");
    let rejected = run(&runtime,&plugin,&services,OperationInput::Auth{provider:provider.clone(),request:AuthRequest{step:AuthStep::Exchange{callback_url:"https://antigravity.google/oauth-callback?state=wrong&code=test-authorization-code".into()}}}).await;
    assert!(rejected.is_err());
    assert!(fixture.received.lock().is_empty());
    let AuthResponse::Credentials { values, .. } = auth(
        &runtime,
        &plugin,
        &services,
        &provider,
        AuthStep::ManualInput {
            value: "test-authorization-code".into(),
        },
    )
    .await
    else {
        panic!("credentials")
    };
    provider.credentials = values;
    assert!(!provider.credentials.contains_key("project_id"));
    let OperationOutput::Discover(catalog) = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Discover {
            provider: provider.clone(),
            request: DiscoverRequest { cursor: None },
        },
    )
    .await
    .unwrap() else {
        panic!("catalog")
    };
    let chat = catalog
        .models
        .iter()
        .find(|model| model.id == "account-model")
        .unwrap();
    assert_eq!(
        chat.metadata["modalities"],
        json!({"input":["text","image"],"output":["text"]})
    );
    let image = catalog
        .models
        .iter()
        .find(|model| model.id == "gemini-3.1-flash-image")
        .unwrap();
    assert_eq!(image.capabilities, ["media_image"]);
    assert_eq!(image.metadata["modalities"]["output"], json!(["image"]));
    let OperationOutput::Allowance(quota) = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Allowance {
            provider: provider.clone(),
            request: AllowanceRequest {
                model: None,
                reset: None,
            },
        },
    )
    .await
    .unwrap() else {
        panic!("quota")
    };
    assert_eq!(quota.allowances.len(), 1);
    assert_eq!(quota.allowances[0].used_percent.as_deref(), Some("30"));
    assert_eq!(quota.plan_label.as_deref(), Some("Google AI Pro"));
    assert_eq!(
        quota.billing,
        Some(AllowanceBilling {
            kind: AllowanceBillingKind::Subscription,
            period: Some(AllowanceBillingPeriod::Month),
            price: Some(AllowanceBillingPrice {
                amount: "19.99".into(),
                currency: "USD".into(),
            }),
        })
    );
    let AuthResponse::Credentials { values, .. } =
        auth(&runtime, &plugin, &services, &provider, AuthStep::Refresh).await
    else {
        panic!("refresh")
    };
    assert_eq!(values["refresh_token"], "test-refresh");
    assert_eq!(values["project_id"], "test-project");
    provider.credentials = values;
    provider
        .operation_metadata
        .insert("session_affinity".into(), json!(" principal/目标/会话 "));
    let endpoint = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let request = ProtocolTransform::global().bind(endpoint,endpoint).unwrap().decode_request(json!({
        "contents":[{"role":"user","parts":[{"text":"Run read","future":"strip"}],"foreign":"strip"}],
        "tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"OBJECT","properties":{"metadata":{"type":"OBJECT","properties":{"seed":{"type":"INTEGER"}}}},"foreign":"strip"},"strict":true},{"name":"list","description":"List entries","parameters":{"type":"OBJECT"}}]},{"googleSearch":{}}],
        "generationConfig":{"temperature":0.5,"seed":42,"foreign":"strip","thinkingConfig":{"thinkingBudget":2048,"foreign":"strip"}},
        "cachedContent":"strip", "foreign":"strip",
        "labels":{"request_id":"client-supplied","model_enum":"client-model","trajectory_id":"client-trajectory"}
    })).unwrap();
    let OperationOutput::Infer(output) = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: request.clone(),
        },
    )
    .await
    .unwrap() else {
        panic!("inference")
    };
    assert_eq!(output.output_text(), "真实 HTTP 冒烟");
    assert_eq!(output.usage.cache_read_tokens, Some(0));
    assert_eq!(output.usage.prompt_tokens, 5);
    assert_eq!(output.usage.completion_tokens, 8);
    {
        let deltas = services.deltas.lock();
        let mut usages = deltas.iter().filter_map(|delta| match delta {
            AiStreamDelta::Usage(usage) => Some(usage),
            _ => None,
        });
        assert_eq!(
            usages.next().expect("partial usage").cache_read_tokens,
            None
        );
        assert_eq!(
            usages.next().expect("final usage").cache_read_tokens,
            Some(0)
        );
    }
    let assert_read_list_groups = |request: &Value| {
        let groups: Vec<Vec<&str>> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("functionDeclarations"))
            .map(|declarations| {
                declarations
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|decl| decl["name"].as_str().unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(groups, [["read"], ["list"]]);
    };
    let call = output
        .items
        .iter()
        .flat_map(|item| item.tool_calls.iter().flatten())
        .find(|call| call.name == "read")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&call.arguments).unwrap(),
        json!({"metadata":{"seed":7}})
    );
    assert!(
        serde_json::to_string(&output)
            .unwrap()
            .contains("genuine-signature")
    );
    {
        let received = fixture.received.lock();
        let (_, operation_headers, _) = received
            .iter()
            .find(|(url, _, _)| url == "/v1internal/operations/account-onboarding")
            .unwrap();
        assert!(!operation_headers.contains_key("content-type"));
        let (_, headers, body) = received
            .iter()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        assert!(!headers.contains_key("x-downstream-secret"));
        assert!(!headers.contains_key("x-goog-api-client"));
        assert_eq!(headers["transfer-encoding"], "chunked");
        assert_eq!(headers["accept-encoding"], "gzip");
        assert_eq!(
            headers["user-agent"],
            "antigravity/cli/1.5.11 (aidev_client; os_type=linux; arch=amd64; cl=1024248524; auth_method=consumer)"
        );
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["project"], "test-project");
        assert_eq!(body["model"], "account-model");
        assert_eq!(
            body["request"]["generationConfig"]["maxOutputTokens"],
            65536
        );
        assert_eq!(body["request"]["sessionId"], "-2713670015314466186");
        let request_id = body["requestId"].as_str().unwrap();
        let native_id = uuid::Uuid::parse_str(request_id.strip_prefix("agent/").unwrap()).unwrap();
        assert_eq!(native_id.get_version_num(), 4);
        let labels = &body["request"]["labels"];
        let cascade = labels["cascade_id"].as_str().unwrap();
        uuid::Uuid::parse_str(cascade).unwrap();
        let trajectory = labels["trajectory_id"].as_str().unwrap();
        uuid::Uuid::parse_str(trajectory).unwrap();
        assert_ne!(trajectory, cascade);
        assert_eq!(
            labels,
            &json!({
                "request_id": format!("{trajectory}-0"),
                "cascade_id": cascade, "root_cascade_id": cascade, "trajectory_id": trajectory,
                "last_step_index": "0", "used_claude": "false",
                "used_claude_conservative": "true", "used_non_gemini_model": "true"
            })
        );
        assert!(body["request"].get("cachedContent").is_none());
        assert_read_list_groups(&body["request"]);
        let tools = body["request"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert!(
            tools
                .iter()
                .any(|tool| tool.get("googleSearch") == Some(&json!({})))
        );
        assert!(body["request"]["generationConfig"].get("seed").is_none());
        for pointer in [
            "/request/foreign",
            "/request/contents/0/foreign",
            "/request/contents/0/parts/0/future",
            "/request/tools/0/functionDeclarations/0/strict",
            "/request/tools/0/functionDeclarations/0/parameters/foreign",
            "/request/generationConfig/foreign",
            "/request/generationConfig/thinkingConfig/foreign",
        ] {
            assert!(
                body.pointer(pointer).is_none(),
                "unexpected protocol field: {pointer}"
            );
        }
        let (_, _, token_body) = received.iter().find(|(url, _, _)| url == "/token").unwrap();
        let token_fields: BTreeMap<_, _> = url::form_urlencoded::parse(token_body)
            .into_owned()
            .collect();
        let verifier = &token_fields["code_verifier"];
        use base64::Engine;
        use sha2::Digest;
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(verifier.as_bytes())),
            query["code_challenge"]
        );
    }
    {
        let mut zero_limit = request.clone();
        zero_limit.generation.max_tokens = Some(0);
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: provider.clone(),
                request: zero_limit,
            },
        )
        .await
        .unwrap();
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let body: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], 0);
    }
    let openai = stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    let openai_request = ProtocolTransform::global()
        .bind(openai, openai)
        .unwrap()
        .decode_request(json!({
            "model":"client-model", "messages":[{"role":"user","content":"Cross-protocol"}],
            "temperature":0.5, "max_tokens":128, "stop":["END"],
            "seed":42, "presence_penalty":0.2, "frequency_penalty":0.3,
            "parallel_tool_calls":true, "logit_bias":{"1":100},
            "service_tier":"priority", "metadata":{"foreign":"strip"},
            "tools":[{"type":"function","function":{"name":"read","strict":true,"parameters":{
                "type":"object","additionalProperties":false,
                "$defs":{"Seed":{"type":["integer","null"],"default":7}},
                "properties":{"metadata":{"type":"object","properties":{"seed":{"$ref":"#/$defs/Seed"}}}}
            }}},{"type":"function","function":{"name":"list","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"read"}},
            "response_format":{"type":"json_schema","json_schema":{"name":"reply","strict":true,
                "schema":{"type":"object","additionalProperties":false,
                    "properties":{"result":{"type":"string","default":"business data"}}}
            }}
        }))
        .unwrap();
    let OperationOutput::Infer(openai_output) = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: openai_request,
        },
    )
    .await
    .unwrap() else {
        panic!("cross-protocol inference")
    };
    assert_eq!(openai_output.output_text(), "真实 HTTP 冒烟");
    {
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let body: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(
            body["request"]["generationConfig"]["stopSequences"],
            json!(["END"])
        );
        assert_read_list_groups(&body["request"]);
        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], 128);
        assert_eq!(body["request"]["generationConfig"]["temperature"], 0.5);
        assert_eq!(
            body["request"]["toolConfig"]["functionCallingConfig"],
            json!({"mode":"ANY","allowedFunctionNames":["read"]})
        );
        let declaration = &body["request"]["tools"][0]["functionDeclarations"][0];
        assert!(declaration.get("parameters").is_none());
        let parameters = &declaration["parametersJsonSchema"];
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(
            parameters["properties"]["metadata"]["properties"]["seed"]["$ref"],
            "#/$defs/Seed"
        );
        assert_eq!(parameters["$defs"]["Seed"]["default"], 7);
        let validator = jsonschema::validator_for(parameters).unwrap();
        for (arguments, valid) in [
            (json!({"metadata": {"seed": 7}}), true),
            (json!({"metadata": {"seed": null}}), true),
            (json!({"metadata": {"seed": "7"}}), false),
            (json!({"extra": true}), false),
        ] {
            assert_eq!(validator.is_valid(&arguments), valid, "{arguments}");
        }
        let output_schema = &body["request"]["generationConfig"]["responseSchema"];
        assert_eq!(output_schema["additionalProperties"], false);
        assert_eq!(
            output_schema["properties"]["result"]["default"],
            "business data"
        );
        assert_eq!(
            body["request"]["generationConfig"]["responseMimeType"],
            "application/json"
        );
        for pointer in [
            "/request/metadata",
            "/request/service_tier",
            "/request/parallel_tool_calls",
            "/request/logit_bias",
            "/request/generationConfig/seed",
            "/request/generationConfig/presencePenalty",
            "/request/generationConfig/frequencyPenalty",
        ] {
            assert!(
                body.pointer(pointer).is_none(),
                "unexpected protocol field: {pointer}"
            );
        }
    }
    for (family, effort, expected, summary) in [
        ("gemini-3.1-pro", None, "gemini-pro-agent", true),
        ("gemini-3.1-pro", Some("low"), "gemini-3.1-pro-low", true),
        ("gemini-3.1-pro", Some("high"), "gemini-pro-agent", true),
        (
            "gemini-3.8-flash",
            Some("low"),
            "gemini-3.8-flash-low",
            true,
        ),
        (
            "gemini-3.8-flash",
            Some("medium"),
            "gemini-3.8-flash-medium",
            true,
        ),
        (
            "gemini-3.8-flash",
            Some("high"),
            "gemini-3.8-flash-high",
            true,
        ),
        (
            "gemini-3.8-flash",
            Some("high"),
            "gemini-3.8-flash-high",
            false,
        ),
        ("gemini-3.8-flash", None, "gemini-3.8-flash-high", true),
        (
            "gemini-4-hydrogen",
            Some("low"),
            "gemini-4-hydrogen-low",
            true,
        ),
        (
            "gemini-4-hydrogen",
            Some("medium"),
            "gemini-4-hydrogen-medium",
            true,
        ),
        (
            "gemini-4-hydrogen",
            Some("high"),
            "gemini-4-hydrogen-high",
            true,
        ),
        (
            "gemini-4-hydrogen",
            Some("xhigh"),
            "gemini-4-hydrogen-xhigh",
            true,
        ),
        ("gemini-4-hydrogen", None, "gemini-4-hydrogen-medium", true),
        ("claude-sonnet-5-5", None, "claude-sonnet-5-5-medium", true),
        (
            "claude-sonnet-5-5",
            Some("medium"),
            "claude-sonnet-5-5-medium",
            true,
        ),
    ] {
        let model = catalog
            .models
            .iter()
            .find(|model| model.id == family)
            .unwrap();
        let mut selected_provider = provider.clone();
        selected_provider.model = Some(model.id.clone());
        selected_provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            id: Some(model.id.clone()),
            family: model.family.clone(),
            selector: model.selector.clone(),
            extensions: BTreeMap::from([(
                "antigravity".into(),
                model.metadata["antigravity"].clone(),
            )]),
            ..Default::default()
        });
        let mut selected_request = request.clone();
        selected_request.reasoning.mode = None;
        selected_request.reasoning.level = None;
        selected_request.reasoning.budget_tokens = None;
        if !summary {
            selected_request.reasoning.summary =
                Some(stravia_runtime_contract::protocol::ir::ReasoningSummary::None);
        }
        selected_request.reasoning.target_control =
            effort.map(
                |value| stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                    value: value.into(),
                },
            );
        let OperationOutput::Infer(output) = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: selected_provider,
                request: selected_request,
            },
        )
        .await
        .unwrap() else {
            panic!("family inference");
        };
        assert_eq!(output.output_text(), "真实 HTTP 冒烟");
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(envelope["model"], expected);
        let expected_enum = match expected {
            "gemini-pro-agent" => Some("MODEL_PRO_HIGH"),
            "gemini-3.1-pro-low" => Some("MODEL_PRO_LOW"),
            _ => None,
        };
        assert_eq!(
            envelope
                .pointer("/request/labels/model_enum")
                .and_then(Value::as_str),
            expected_enum
        );
        assert_eq!(
            envelope.pointer("/request/generationConfig/thinkingConfig/includeThoughts"),
            Some(&serde_json::json!(summary))
        );
        if family == "claude-sonnet-5-5" {
            let generation = &envelope["request"]["generationConfig"];
            assert_eq!(generation["maxOutputTokens"], 128000);
            assert_eq!(
                generation["thinkingConfig"],
                json!({"thinkingLevel":"MEDIUM", "includeThoughts":true})
            );
        } else if family == "gemini-4-hydrogen" {
            assert_eq!(
                model.metadata["reasoning_efforts"],
                json!(["low", "medium", "high", "xhigh"])
            );
            assert_eq!(
                envelope["request"]["generationConfig"]["maxOutputTokens"],
                262144
            );
            assert_eq!(
                envelope.pointer("/request/generationConfig/thinkingConfig/thinkingLevel"),
                Some(&match effort {
                    Some("low") => json!("LOW"),
                    Some("high") => json!("HIGH"),
                    Some("xhigh") => json!(100),
                    Some("medium") | None => json!("MEDIUM"),
                    _ => unreachable!(),
                })
            );
            assert!(
                envelope
                    .pointer("/request/generationConfig/thinkingConfig/thinkingBudget")
                    .is_none()
            );
        } else if family == "gemini-3.8-flash" {
            assert!(
                envelope
                    .pointer("/request/generationConfig/thinkingConfig/thinkingBudget")
                    .is_none()
            );
            assert_eq!(
                envelope.pointer("/request/generationConfig/thinkingConfig/thinkingLevel"),
                Some(&json!(match effort {
                    Some("low") => "LOW",
                    Some("medium") => "MEDIUM",
                    Some("high") | None => "HIGH",
                    _ => unreachable!(),
                }))
            );
        } else {
            assert!(
                envelope
                    .pointer("/request/generationConfig/thinkingConfig/thinkingLevel")
                    .is_none()
            );
            assert_eq!(
                envelope.pointer("/request/generationConfig/thinkingConfig/thinkingBudget"),
                Some(&json!(match effort {
                    Some("low") => 1024,
                    Some("high") | None => -1,
                    _ => unreachable!(),
                }))
            );
        }
    }
    {
        let model = catalog
            .models
            .iter()
            .find(|model| model.id == "gemini-4-hydrogen")
            .unwrap();
        let mut table = model.metadata["antigravity"].clone();
        let xhigh = table["variants"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|variant| variant["effort"] == "xhigh")
            .unwrap();
        xhigh["metadata"]["maxOutputTokens"] = json!(12345);
        let mut selected_provider = provider.clone();
        selected_provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([("antigravity".into(), table)]),
            ..Default::default()
        });
        let mut selected_request = request.clone();
        selected_request.reasoning.target_control = Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: "xhigh".into(),
            },
        );
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: selected_provider,
                request: selected_request,
            },
        )
        .await
        .unwrap();
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(
            envelope["request"]["generationConfig"]["maxOutputTokens"],
            12345
        );
    }
    for cap in [0, 128] {
        let model = catalog
            .models
            .iter()
            .find(|model| model.id == "gemini-4-hydrogen")
            .unwrap();
        let mut selected_provider = provider.clone();
        selected_provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([(
                "antigravity".into(),
                model.metadata["antigravity"].clone(),
            )]),
            ..Default::default()
        });
        let mut selected_request = request.clone();
        selected_request.generation.max_tokens = Some(cap);
        selected_request.reasoning.target_control =
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Budget { value: 2048 });
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: selected_provider,
                request: selected_request,
            },
        )
        .await
        .unwrap();
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(
            envelope["request"]["generationConfig"]["maxOutputTokens"],
            cap
        );
        assert_eq!(
            envelope["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            2048
        );
        assert!(
            envelope
                .pointer("/request/generationConfig/thinkingConfig/thinkingLevel")
                .is_none()
        );
    }
    for (level, budget, control, expected_level, expected_budget, summary) in [
        (
            Some("MEDIUM"),
            None,
            None,
            Some(json!("MEDIUM")),
            None,
            true,
        ),
        (Some("HIGH"), None, None, Some(json!("HIGH")), None, false),
        (
            Some("XHIGH"),
            None,
            None,
            Some(json!("EXTRA_HIGH")),
            None,
            true,
        ),
        (Some("MAX"), None, None, Some(json!("MAX")), None, true),
        (None, Some(2048), None, None, Some(json!(2048)), true),
        (
            Some("HIGH"),
            None,
            Some(
                stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                    value: "medium".into(),
                },
            ),
            Some(json!("MEDIUM")),
            None,
            true,
        ),
        (
            Some("HIGH"),
            None,
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Budget { value: 7777 }),
            None,
            Some(json!(7777)),
            true,
        ),
        (
            Some("HIGH"),
            None,
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled),
            None,
            Some(json!(0)),
            false,
        ),
    ] {
        let mut selected_request = request.clone();
        let mut selected_provider = provider.clone();
        let model = catalog
            .models
            .iter()
            .find(|model| model.id == "gemini-3.8-flash")
            .unwrap();
        let mut table = model.metadata["antigravity"].clone();
        for variant in table["variants"].as_array_mut().unwrap() {
            variant["metadata"]["thinkingBudget"] = json!(4000);
        }
        selected_provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([("antigravity".into(), table)]),
            ..Default::default()
        });
        selected_request.reasoning.mode = None;
        selected_request.reasoning.budget_tokens = budget;
        selected_request.reasoning.level = level.map(|level| {
            stravia_runtime_contract::thinking::ThinkingLevel::from_wire(level).unwrap()
        });
        selected_request.reasoning.target_control = control;
        selected_request.reasoning.summary = if matches!(
            selected_request.reasoning.target_control,
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Disabled)
        ) {
            // 客户端即使索取摘要，也不能反转 Target 的显式禁用。
            Some(stravia_runtime_contract::protocol::ir::ReasoningSummary::Auto)
        } else {
            (!summary).then_some(stravia_runtime_contract::protocol::ir::ReasoningSummary::None)
        };
        let OperationOutput::Infer(_) = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: selected_provider,
                request: selected_request,
            },
        )
        .await
        .unwrap() else {
            panic!("raw thinking inference");
        };
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(
            envelope.pointer("/request/generationConfig/thinkingConfig/thinkingLevel"),
            expected_level.as_ref()
        );
        assert_eq!(
            envelope.pointer("/request/generationConfig/thinkingConfig/thinkingBudget"),
            expected_budget.as_ref()
        );
        assert_eq!(
            envelope.pointer("/request/generationConfig/thinkingConfig/includeThoughts"),
            Some(&json!(summary))
        );
    }
    for (number, name) in [
        (0, json!("THINKING_LEVEL_UNSPECIFIED")),
        (1, json!("LOW")),
        (2, json!("MEDIUM")),
        (3, json!("HIGH")),
        (4, json!("MINIMAL")),
        (5, json!("EXTRA_HIGH")),
        (6, json!("MAX")),
        (7, json!("XHIGH")),
        (100, json!(100)),
        (-1, json!(-1)),
    ] {
        let mut selected_provider = provider.clone();
        selected_provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([(
                "antigravity".into(),
                json!({"default":"flash", "variants":[{
                    "id":"flash", "effort":"medium", "metadata":{"thinkingLevel":number}
                }]}),
            )]),
            ..Default::default()
        });
        let mut selected_request = request.clone();
        selected_request.reasoning.target_control = Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: "medium".into(),
            },
        );
        run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: selected_provider,
                request: selected_request,
            },
        )
        .await
        .unwrap();
        let received = fixture.received.lock();
        let (_, _, bytes) = received
            .iter()
            .rev()
            .find(|(url, _, _)| url.starts_with("/v1internal:streamGenerateContent"))
            .unwrap();
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(envelope["model"], "flash");
        assert_eq!(
            envelope["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            name
        );
        assert!(
            envelope
                .pointer("/request/generationConfig/thinkingConfig/thinkingBudget")
                .is_none()
        );
    }
    for metadata in [
        json!({}),
        json!({"minThinkingBudget":32}),
        json!({"thinkingBudget":"4000"}),
        json!({"thinkingBudget":-2}),
        json!({"thinkingLevel":null}),
        json!({"thinkingLevel":2147483648_i64}),
        json!({"thinkingLevel":"MEDIUM"}),
        json!({"thinkingLevel":2147483648_i64,"thinkingBudget":4000}),
    ] {
        let mut selected_provider = provider.clone();
        selected_provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([(
                "antigravity".into(),
                json!({
                    "default":"gemini-3.8-flash-medium",
                    "variants":[{"id":"gemini-3.8-flash-medium","effort":"medium","metadata":metadata}]
                }),
            )]),
            ..Default::default()
        });
        let mut selected_request = request.clone();
        selected_request.reasoning.target_control = Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: "medium".into(),
            },
        );
        let before = fixture.received.lock().len();
        let error = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: selected_provider,
                request: selected_request,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            &error,
            stravia_vendor_runtime::RuntimeError::Plugin {
                kind: ErrorKind::Invalid,
                ..
            }
        ));
        assert_eq!(fixture.received.lock().len(), before);
    }
    {
        let model = catalog
            .models
            .iter()
            .find(|model| model.id == "gemini-3.1-pro")
            .unwrap();
        let mut selected_provider = provider.clone();
        selected_provider.model = Some(model.id.clone());
        selected_provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([(
                "antigravity".into(),
                model.metadata["antigravity"].clone(),
            )]),
            ..Default::default()
        });
        let mut selected_request = request.clone();
        selected_request.reasoning.target_control = Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
                value: "medium".into(),
            },
        );
        let before = fixture.received.lock().len();
        let error = run(
            &runtime,
            &plugin,
            &services,
            OperationInput::Infer {
                provider: selected_provider,
                request: selected_request,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            &error,
            stravia_vendor_runtime::RuntimeError::Plugin {
                kind: ErrorKind::Invalid,
                ..
            }
        ));
        assert_eq!(fixture.received.lock().len(), before);
    }
    fixture.truncated.store(true, Ordering::Relaxed);
    let error = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer {
            provider: provider.clone(),
            request: request.clone(),
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, stravia_vendor_runtime::RuntimeError::Plugin { kind, .. } if kind.model_error_kind()==Some(AiErrorKind::UnexpectedEof))
    );
    fixture.error_stream.store(true, Ordering::Relaxed);
    let error = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Infer { provider, request },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, stravia_vendor_runtime::RuntimeError::Plugin { kind, .. } if kind.model_error_kind()==Some(AiErrorKind::RateLimitError))
    );
    let received = fixture.received.lock();
    let failed_request: Value = serde_json::from_slice(&received.last().unwrap().2).unwrap();
    let correlation_id = failed_request["request"]["labels"]["request_id"]
        .as_str()
        .unwrap();
    assert!(error.diagnostic_message().unwrap().contains(correlation_id));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(9)));
    println!(
        "Antigravity Wasm + loopback HTTP: OAuth PKCE/state/manual exchange/refresh; async project; Agent-only families; default/low/medium/high request IDs; unsupported effort rejected before HTTP; shared quota 30%; strict wire filter; SSE text/tool signature; truncated/429 errors: OK"
    );
    server.abort();
}
