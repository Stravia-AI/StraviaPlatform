use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, OperationInput, OperationOutput,
    ProviderSnapshot,
};

struct Response {
    status: u16,
    mime: String,
    chunks: Mutex<VecDeque<Vec<u8>>>,
}
#[async_trait]
impl HostHttpResponse for Response {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.status)
    }
    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure> {
        Ok(vec![("content-type".into(), self.mime.clone())])
    }
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.chunks.lock().pop_front())
    }
}
#[derive(Default)]
struct Services {
    responses: Mutex<VecDeque<Response>>,
    requests: Mutex<Vec<HttpRequest>>,
    state: Mutex<Option<Vec<u8>>>,
    events: Mutex<Vec<RuntimeEvent>>,
}
impl Services {
    fn add(&self, status: u16, mime: &str, body: &str) {
        self.responses.lock().push_back(Response {
            status,
            mime: mime.into(),
            chunks: Mutex::new(body.as_bytes().chunks(7).map(<[u8]>::to_vec).collect()),
        });
    }
}
#[async_trait]
impl HostServices for Services {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        self.requests.lock().push(request);
        self.responses
            .lock()
            .pop_front()
            .map(|r| Arc::new(r) as Arc<dyn HostHttpResponse>)
            .ok_or_else(|| HostFailure::new(ErrorKind::Trapped, "Unexpected HTTP request"))
    }
    async fn ws_connect(
        &self,
        _request: stravia_vendor_runtime::WsRequest,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        Err(HostFailure::new(ErrorKind::Trapped, "Unexpected websocket"))
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
    fn log(&self, _level: LogLevel, _message: &str) {}
    fn generation_is_current(&self, _generation: u64) -> bool {
        true
    }
}
fn provider() -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "kilo".into(),
        channel: "default".into(),
        base_url: "https://api.kilo.ai/api/gateway".into(),
        protocol: "openai-compatible".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: None,
        model_metadata: None,
        client_headers: vec![],
        operation_metadata: BTreeMap::new(),
    }
}
async fn load() -> (VendorRuntime, LoadedPlugin) {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/vendor-plugins-all");
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(dir.join("manifest.json")).expect("build vendor components first"),
    )
    .unwrap();
    let entry = manifest
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["vendor_id"] == "kilo")
        .expect("Kilo component registered");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(dir.join(entry["file"].as_str().unwrap())).unwrap())
        .await
        .unwrap();
    (runtime, plugin)
}
async fn run(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    input: OperationInput,
    services: Arc<Services>,
) -> Result<OperationOutput, RuntimeError> {
    let channel = input.provider().channel.clone();
    let (operation, input) = input.encode_for_host().unwrap();
    runtime
        .execute(
            plugin,
            &channel,
            operation,
            input,
            OperationScope::new(
                services,
                CancellationToken::new(),
                Deadline::from_now(Duration::from_secs(60)),
                0,
            ),
        )
        .await
}
fn auth(provider: ProviderSnapshot, step: AuthStep) -> OperationInput {
    OperationInput::Auth {
        provider,
        request: AuthRequest { step },
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires built Kilo Wasm component"]
async fn device_pending_approval_profile_and_expiry() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    services.add(
        200,
        "application/json",
        r#"{"code":"AB/C","verificationUrl":"https://kilo.ai/device","expiresIn":300}"#,
    );
    let out = run(
        &runtime,
        &plugin,
        auth(
            provider(),
            AuthStep::Start {
                redirect_uri: String::new(),
                state: "host".into(),
            },
        ),
        services.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(
        out,
        OperationOutput::Auth(AuthResponse::Authorization {
            interval_seconds: Some(3),
            ..
        })
    ));
    services.add(202, "application/json", "");
    assert!(matches!(
        run(
            &runtime,
            &plugin,
            auth(provider(), AuthStep::Poll),
            services.clone()
        )
        .await
        .unwrap(),
        OperationOutput::Auth(AuthResponse::Pending { .. })
    ));
    services.add(
        200,
        "application/json",
        r#"{"status":"approved","token":"secret","userEmail":"me@example.com"}"#,
    );
    services.add(
        200,
        "application/json",
        r#"{"user":{"email":"me@example.com"},"organizations":[{"id":"org","name":"Org"}]}"#,
    );
    let mut p = provider();
    p.options.insert("organizationId".into(), json!("org"));
    let out = run(&runtime, &plugin, auth(p, AuthStep::Poll), services.clone())
        .await
        .unwrap();
    let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) = out else {
        panic!("credentials expected")
    };
    assert_eq!(values["apiKey"], "secret");
    assert_eq!(values["organizationId"], "org");
    assert!(!values.contains_key("refreshToken"));
    assert_eq!(
        services.requests.lock()[1].url,
        "https://api.kilo.ai/api/device-auth/codes/AB%2FC"
    );
    assert!(
        services.requests.lock()[3]
            .headers
            .iter()
            .all(|(n, _)| !n.eq_ignore_ascii_case("X-KiloCode-OrganizationId"))
    );
    *services.state.lock() = Some(br#"{"code":"expired","deadline_ms":0}"#.to_vec());
    let count = services.requests.lock().len();
    assert!(
        run(
            &runtime,
            &plugin,
            auth(provider(), AuthStep::Poll),
            services.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(services.requests.lock().len(), count);
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires built Kilo Wasm component"]
async fn device_denied_expired_and_unverified_org_membership() {
    let (runtime, plugin) = load().await;
    for status in [403, 410] {
        let s = Arc::new(Services::default());
        *s.state.lock() = Some(
            format!(
                "{{\"code\":\"code\",\"deadline_ms\":{}}}",
                chrono::Utc::now().timestamp_millis() + 60000
            )
            .into_bytes(),
        );
        s.add(status, "application/json", "");
        assert!(
            run(&runtime, &plugin, auth(provider(), AuthStep::Poll), s)
                .await
                .is_err()
        );
    }
    let s = Arc::new(Services::default());
    *s.state.lock() = Some(
        format!(
            "{{\"code\":\"code\",\"deadline_ms\":{}}}",
            chrono::Utc::now().timestamp_millis() + 60000
        )
        .into_bytes(),
    );
    s.add(
        200,
        "application/json",
        r#"{"status":"approved","token":"secret"}"#,
    );
    s.add(
        200,
        "application/json",
        r#"{"email":"user@example.com","organizations":[]}"#,
    );
    let mut p = provider();
    p.options
        .insert("organizationId".into(), json!("unauthorized"));
    assert!(
        run(&runtime, &plugin, auth(p, AuthStep::Poll), s.clone())
            .await
            .is_err()
    );
    let count = s.requests.lock().len();
    assert!(
        run(
            &runtime,
            &plugin,
            auth(
                provider(),
                AuthStep::ManualInput {
                    value: "key".into()
                }
            ),
            s.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(s.requests.lock().len(), count);
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires built Kilo Wasm component"]
async fn signed_in_catalog_and_organization_balance() {
    let (runtime, plugin) = load().await;
    let s = Arc::new(Services::default());
    assert!(
        run(
            &runtime,
            &plugin,
            OperationInput::Discover {
                provider: provider(),
                request: Default::default(),
            },
            s.clone(),
        )
        .await
        .is_err()
    );
    assert!(s.requests.lock().is_empty());
    s.add(200,"application/json",r#"{"data":[{"id":"vendor/paid","pricing":{"prompt":"1","completion":"1"}},{"id":"vendor/free:free","pricing":{"prompt":"0","completion":"0"}}]}"#);
    let mut p = provider();
    p.credentials.insert("apiKey".into(), json!("key"));
    let out = run(
        &runtime,
        &plugin,
        OperationInput::Discover {
            provider: p.clone(),
            request: Default::default(),
        },
        s.clone(),
    )
    .await
    .unwrap();
    let OperationOutput::Discover(out) = out else {
        panic!("discover")
    };
    assert_eq!(out.models.len(), 2);
    assert!(
        s.requests.lock()[0]
            .headers
            .contains(&("authorization".into(), "Bearer key".into()))
    );
    p.credentials
        .insert("organizations".into(), json!([{"id":"org/id"}]));
    p.options.insert("organizationId".into(), json!("org/id"));
    s.add(200, "application/json", r#"{"balance":7.5}"#);
    let out = run(
        &runtime,
        &plugin,
        OperationInput::Allowance {
            provider: p,
            request: Default::default(),
        },
        s.clone(),
    )
    .await
    .unwrap();
    let OperationOutput::Allowance(out) = out else {
        panic!("allowance")
    };
    assert_eq!(out.allowances[0].kind, "balance");
    assert_eq!(out.allowances[0].remaining.as_ref().unwrap().value, "7.5");
    assert!(out.billing.is_none());
    let request = s.requests.lock()[1].clone();
    assert!(request.url.ends_with("/api/profile/balance"));
    assert!(
        request
            .headers
            .contains(&("X-KiloCode-OrganizationId".into(), "org/id".into()))
    );
    let count = s.requests.lock().len();
    assert!(
        run(
            &runtime,
            &plugin,
            OperationInput::Allowance {
                provider: provider(),
                request: Default::default()
            },
            s.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(s.requests.lock().len(), count);
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires built Kilo Wasm component"]
async fn route_alias_uses_the_target_model_and_requires_sign_in() {
    let (runtime, plugin) = load().await;
    let services = Arc::new(Services::default());
    services.add(
        200,
        "text/event-stream",
        "data: {\"id\":\"native\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"target accepted\"}}]}\n\ndata: {\"id\":\"native\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n",
    );
    let mut target = provider();
    target.model = Some("vendor/free:free".into());
    let mut request = AiRequest::new("user-facing-route", vec![]);
    request.stream.enabled = true;
    assert!(
        run(
            &runtime,
            &plugin,
            OperationInput::Infer {
                provider: target.clone(),
                request: request.clone(),
            },
            services.clone(),
        )
        .await
        .is_err()
    );
    assert!(services.requests.lock().is_empty());
    target.credentials.insert("apiKey".into(), json!("key"));
    let output = run(
        &runtime,
        &plugin,
        OperationInput::Infer {
            provider: target,
            request,
        },
        services.clone(),
    )
    .await
    .unwrap();
    let OperationOutput::Infer(response) = output else {
        panic!("expected inference");
    };
    assert_eq!(response.output_text(), "target accepted");
    let requests = services.requests.lock();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "vendor/free:free");
    assert!(
        requests[0]
            .headers
            .contains(&("authorization".into(), "Bearer key".into()))
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires built Kilo Wasm component"]
async fn billing_and_midstream_failure_never_complete_successfully() {
    let (runtime, plugin) = load().await;
    for (status, mime, body) in [
        (
            402,
            "application/json",
            r#"{"error":{"message":"Insufficient balance"}}"#,
        ),
        (
            200,
            "text/event-stream",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"error\"}],\"error\":{\"message\":\"billing failure\"}}\n\ndata: [DONE]\n\n",
        ),
    ] {
        let s = Arc::new(Services::default());
        s.add(status, mime, body);
        let mut p = provider();
        p.credentials.insert("apiKey".into(), json!("key"));
        p.model = Some("vendor/model".into());
        p.client_headers
            .push(("session_id".into(), "unsafe".into()));
        let mut request = AiRequest::new("vendor/model", vec![]);
        request.stream.enabled = true;
        assert!(
            run(
                &runtime,
                &plugin,
                OperationInput::Infer {
                    provider: p,
                    request
                },
                s.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(
            s.requests.lock()[0].url,
            "https://api.kilo.ai/api/gateway/chat/completions"
        );
        assert!(
            s.requests.lock()[0]
                .headers
                .iter()
                .all(|(n, _)| n != "session_id")
        );
    }
}
