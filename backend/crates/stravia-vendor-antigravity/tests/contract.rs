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
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LoadedPlugin,
    LogLevel, OperationScope, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::{
    AllowanceRequest, AuthRequest, AuthResponse, AuthStep, DiscoverRequest, ErrorKind,
    OperationInput, OperationOutput, ProviderSnapshot,
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
    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure> {
        Ok(self
            .response()
            .await?
            .lock()
            .await
            .headers()
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_str().unwrap().into()))
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
}
async fn upstream(
    State(fixture): State<Arc<Fixture>>,
    OriginalUri(uri): OriginalUri,
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
            json!({"allowedTiers":[{"id":"account-default","isDefault":true}]})
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
                    "gemini-3.8-flash-high", "gemini-3.8-flash-medium", "gemini-3.8-flash-low"
                ]}]}],
                "deprecatedModelIds": {"gemini-3.1-pro-high": {"newModelId": "gemini-pro-agent"}},
                "imageGenerationModelIds": ["gemini-3.1-flash-image"],
                "models":{
                "account-model":{"displayName":"Account model","supportsImages":true,"maxTokens":131072},
                "hidden-model":{"isInternal":true},
                "gemini-pro-agent":{"displayName":"Gemini 3.1 Pro (High)","supportsThinking":true},
                "gemini-3.1-pro-high":{"displayName":"Gemini 3.1 Pro (High)","supportsThinking":true},
                "gemini-3.1-pro-low":{"displayName":"Gemini 3.1 Pro (Low)","supportsThinking":true},
                "gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)","supportsThinking":true},
                "gemini-3.8-flash-medium":{"displayName":"Gemini 3.8 Flash (Medium)","supportsThinking":true},
                "gemini-3.8-flash-low":{"displayName":"Gemini 3.8 Flash (Low)","supportsThinking":true},
                "gemini-3.1-flash-image":{"displayName":"Gemini 3.1 Flash Image"},
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
        "/v1internal:streamGenerateContent" => {
            if fixture.error_stream.load(Ordering::Relaxed) {
                return (
                    [("content-type", "text/event-stream")],
                    "data: {\"error\":{\"code\":429,\"message\":\"rate limit\"}}\n\n",
                )
                    .into_response();
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
struct Services {
    root: String,
    client: reqwest::Client,
    state: Mutex<Option<Vec<u8>>>,
    deltas: Mutex<Vec<AiStreamDelta>>,
}
#[async_trait]
impl HostServices for Services {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let url = url::Url::parse(&request.url).unwrap();
        assert!(matches!(
            url.host_str(),
            Some("daily-cloudcode-pa.googleapis.com" | "oauth2.googleapis.com")
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
        for (key, value) in request.headers {
            builder = builder.header(key, value);
        }
        Ok(Arc::new(HttpResponse {
            client: self.client.clone(),
            request: Mutex::new(Some(builder.build().map_err(failure)?)),
            response: tokio::sync::OnceCell::new(),
        }))
    }
    async fn ws_connect(
        &self,
        _: String,
        _: Vec<(String, String)>,
        _: Vec<String>,
        _: Option<String>,
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
    input: OperationInput,
) -> Result<OperationOutput, stravia_vendor_runtime::RuntimeError> {
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
        "properties": {
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
    let parameters = &envelope["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
    let validator = jsonschema::validator_for(parameters).unwrap();
    for (arguments, valid) in [
        (json!({"values": []}), true),
        (json!({"values": [], "allowed": {"flag": false}}), true),
        (json!({"values": [], "choice": false}), true),
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
    assert_eq!(
        catalog
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["account-model", "gemini-3.1-pro", "gemini-3.8-flash"]
    );
    let OperationOutput::Allowance(quota) = run(
        &runtime,
        &plugin,
        &services,
        OperationInput::Allowance {
            provider: provider.clone(),
            request: AllowanceRequest { model: None },
        },
    )
    .await
    .unwrap() else {
        panic!("quota")
    };
    assert_eq!(quota.allowances.len(), 1);
    assert_eq!(quota.allowances[0].used_percent.as_deref(), Some("30"));
    let AuthResponse::Credentials { values, .. } =
        auth(&runtime, &plugin, &services, &provider, AuthStep::Refresh).await
    else {
        panic!("refresh")
    };
    assert_eq!(values["refresh_token"], "test-refresh");
    assert_eq!(values["project_id"], "test-project");
    provider.credentials = values;
    let endpoint = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let request = ProtocolTransform::global().bind(endpoint,endpoint).unwrap().decode_request(json!({
        "contents":[{"role":"user","parts":[{"text":"Run read","future":"strip"}],"foreign":"strip"}],
        "tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"OBJECT","properties":{"metadata":{"type":"OBJECT","properties":{"seed":{"type":"INTEGER"}}}},"foreign":"strip"},"strict":true}]}],
        "generationConfig":{"temperature":0.5,"seed":42,"foreign":"strip","thinkingConfig":{"thinkingBudget":2048,"foreign":"strip"}},
        "cachedContent":"strip", "foreign":"strip"
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
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["project"], "test-project");
        assert_eq!(body["model"], "account-model");
        assert!(body["request"].get("cachedContent").is_none());
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
            }}}],
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
        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], 128);
        assert_eq!(body["request"]["generationConfig"]["temperature"], 0.5);
        assert_eq!(
            body["request"]["toolConfig"]["functionCallingConfig"],
            json!({"mode":"ANY","allowedFunctionNames":["read"]})
        );
        let parameters = &body["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(
            parameters["properties"]["metadata"]["properties"]["seed"]["ref"],
            "#/defs/Seed"
        );
        assert_eq!(parameters["defs"]["Seed"]["default"], 7);
        fn accepts_kind(schema: &Value, kind: &str) -> bool {
            if kind == "NULL" && schema["nullable"] == true {
                return true;
            }
            schema.get("type").is_none_or(|value| value == kind)
                && schema.get("allOf").is_none_or(|clauses| {
                    clauses.as_array().is_some_and(|clauses| {
                        clauses.iter().all(|clause| accepts_kind(clause, kind))
                    })
                })
                && schema.get("anyOf").is_none_or(|clauses| {
                    clauses.as_array().is_some_and(|clauses| {
                        clauses.iter().any(|clause| accepts_kind(clause, kind))
                    })
                })
        }
        let seed = &parameters["defs"]["Seed"];
        assert!(accepts_kind(seed, "INTEGER"));
        assert!(accepts_kind(seed, "NULL"));
        assert!(!accepts_kind(seed, "STRING"));
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
        if !summary {
            selected_request.reasoning.display = Some("omitted".into());
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
        assert_eq!(
            envelope.pointer("/request/generationConfig/thinkingConfig/includeThoughts"),
            Some(&serde_json::json!(summary))
        );
        if effort.is_some() {
            assert!(
                envelope
                    .pointer("/request/generationConfig/thinkingConfig/thinkingLevel")
                    .is_none()
            );
            assert!(
                envelope
                    .pointer("/request/generationConfig/thinkingConfig/thinkingBudget")
                    .is_none()
            );
        }
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
    println!(
        "Antigravity Wasm + loopback HTTP: OAuth PKCE/state/manual exchange/refresh; async project; Agent-only families; default/low/medium/high request IDs; unsupported effort rejected before HTTP; shared quota 30%; strict wire filter; SSE text/tool signature; truncated/429 errors: OK"
    );
    server.abort();
}
