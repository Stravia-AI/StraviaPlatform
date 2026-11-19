//! Offline component contract; build with `task build:vendors:all` first, or set
//! STRAVIA_KIRO_COMPONENT_PATH to a Kiro component. Run with `--ignored`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, AiStreamDelta, MessageContent, Role,
};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, LogLevel, OperationScope,
    RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::net::{self, ConnectRequest, Field, Options, Request as HttpRequest};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, OperationInput, OperationOutput,
    ProviderSnapshot,
};

fn artifact() -> PathBuf {
    if let Some(path) = std::env::var_os("STRAVIA_KIRO_COMPONENT_PATH") {
        return path.into();
    }
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
    let manifest: Value = serde_json::from_slice(&bytes).expect("valid vendor manifest");
    let entry = manifest
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["vendor_id"] == "kiro"))
        .expect("kiro missing from vendor manifest; run task build:vendors:all");
    dir.join(entry["file"].as_str().expect("component file"))
}

// Independent IEEE CRC-32 implementation for constructing real AWS EventStream
// frames without depending on the guest decoder's implementation.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn frame(event: &str, payload: &str) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", "event"), (":event-type", event)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7); // EventStream string header
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let total = 16 + headers.len() + payload.len();
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&(total as u32).to_be_bytes());
    bytes.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_be_bytes());
    bytes.extend_from_slice(&headers);
    bytes.extend_from_slice(payload.as_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_be_bytes());
    bytes
}

struct FakeResponse {
    content_type: &'static [u8],
    body: Mutex<std::vec::IntoIter<Vec<u8>>>,
}

#[async_trait]
impl HostHttpResponse for FakeResponse {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(200)
    }
    async fn headers(&self) -> Result<Vec<Field>, HostFailure> {
        Ok(vec![Field {
            name: "content-type".into(),
            bytes: self.content_type.to_vec(),
        }])
    }
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.body.lock().next())
    }
}

#[derive(Default)]
struct FakeServices {
    requests: Mutex<Vec<HttpRequest>>,
    events: Mutex<Vec<RuntimeEvent>>,
    private_state: Mutex<Option<Vec<u8>>>,
    enterprise_opt_out: bool,
}

#[async_trait]
impl HostServices for FakeServices {
    fn http_start(
        &self,
        request: HttpRequest,
        _options: Options,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let mut requests = self.requests.lock();
        let body = match request.url.as_str() {
            "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/device/authorization" => Some(
                br#"{"deviceCode":"synthetic-device","userCode":"ABCD-1234","verificationUri":"https://app.kiro.dev/account/device","verificationUriComplete":"https://app.kiro.dev/account/device?user_code=ABCD-1234&login_provider=google","expiresInMilliseconds":300000,"intervalInMilliseconds":1}"#.as_slice(),
            ),
            "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/device/poll" if !requests.iter().any(|prior| prior.url == request.url) => Some(
                br#"{"status":"authorization_pending","accessToken":null,"refreshToken":null,"profileArn":null,"identityProvider":null}"#.as_slice(),
            ),
            "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/device/poll" => Some(
                br#"{"status":"authorized","accessToken":"synthetic-access","refreshToken":"synthetic-refresh","profileArn":"arn:aws:codewhisperer:eu-central-1:123456789012:profile/synthetic","identityProvider":"Google"}"#.as_slice(),
            ),
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken" => Some(
                br#"{"accessToken":"synthetic-rotated-access","expiresIn":3600}"#.as_slice(),
            ),
            _ if request.url.starts_with("https://oidc.") && request.url.ends_with("/client/register") => Some(
                br#"{"clientId":"synthetic-client","clientSecret":"synthetic-client-secret"}"#.as_slice(),
            ),
            _ if request.url.starts_with("https://oidc.") && request.url.ends_with("/device_authorization") => Some(
                br#"{"deviceCode":"synthetic-oidc-device","userCode":"OIDC-1234","verificationUri":"https://example.awsapps.com/start","verificationUriComplete":"https://example.awsapps.com/start?user_code=OIDC-1234","expiresIn":300,"interval":1}"#.as_slice(),
            ),
            _ if request.url.starts_with("https://oidc.") && request.url.ends_with("/token") => Some(
                br#"{"accessToken":"synthetic-oidc-access","refreshToken":"synthetic-oidc-refresh","expiresIn":3600}"#.as_slice(),
            ),
            _ if request.headers.get("x-amz-target") == Some(b"AmazonCodeWhispererService.ListAvailableProfiles".as_slice()) && self.enterprise_opt_out => Some(
                br#"{"profiles":[{"arn":"arn:aws:codewhisperer:eu-central-1:123456789012:profile/enterprise","optInFeatures":{"deviceCodeSignIn":{"toggle":"DISABLED"}}}]}"#.as_slice(),
            ),
            _ if request.headers.get("x-amz-target") == Some(b"AmazonCodeWhispererService.ListAvailableProfiles".as_slice()) => Some(
                br#"{"profiles":[{"arn":"arn:aws:codewhisperer:eu-central-1:123456789012:profile/enterprise"}]}"#.as_slice(),
            ),
            _ if request.headers.get("x-amz-target") == Some(b"KiroControlPlaneBearerService.ListAvailableModels".as_slice()) => Some(
                br#"{"models":[
                    {"modelId":"qwen3-coder-next","modelName":"Qwen3 Coder Next"},
                    {"modelId":"claude-sonnet-4.6","supportedInputTypes":["TEXT","IMAGE"],"additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{"effort":{"enum":["low","medium","high","max"],"default":"high"}}}}}}
                ]}"#.as_slice(),
            ),
            _ if request.headers.get("x-amz-target") == Some(b"KiroControlPlaneBearerService.GetUsageLimits".as_slice()) => Some(
                br#"{"usageBreakdownList":[{"resourceType":"CREDIT","unit":"INVOCATIONS","usageLimit":10,"currentUsageWithPrecision":9.99}]}"#.as_slice(),
            ),
            _ => None,
        };
        requests.push(request);
        drop(requests);
        if let Some(body) = body {
            return Ok(Arc::new(FakeResponse {
                content_type: b"application/json",
                body: Mutex::new(vec![body.to_vec()].into_iter()),
            }));
        }
        let mut bytes = frame("assistantResponseEvent", r#"{"content":"hello "}"#);
        bytes.extend(frame("assistantResponseEvent", r#"{"content":"world"}"#));
        bytes.extend(frame("metadataEvent", r#"{"stopReason":"END_TURN"}"#));
        // Exercise fragmented preludes, headers, payloads, and checksums.
        let chunks = bytes.chunks(7).map(<[u8]>::to_vec).collect::<Vec<_>>();
        Ok(Arc::new(FakeResponse {
            content_type: b"application/vnd.amazon.eventstream",
            body: Mutex::new(chunks.into_iter()),
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
        Err(HostFailure::new(ErrorKind::Trapped, "unexpected websocket"))
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.private_state.lock().clone())
    }
    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        *self.private_state.lock() = Some(bytes);
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

fn tui_header<'a>(fixture: &'a Value, name: &str) -> &'a str {
    fixture["headers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pair| pair[0] == name)
        .unwrap()[1]
        .as_str()
        .unwrap()
}

fn request(model: &str) -> AiRequest {
    AiRequest::new(
        model,
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("say hello".to_owned().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
}

fn provider(model: &str, key: &str, region: &str) -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "kiro".into(),
        channel: "oauth".into(),
        base_url: format!("https://runtime.{region}.kiro.dev"),
        protocol: "kiro-generate-assistant-response-oauth".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::from([
            ("access_token".into(), json!(key)),
            (
                "profileArn".into(),
                json!(format!(
                    "arn:aws:codewhisperer:{region}:123456789012:profile/synthetic"
                )),
            ),
        ]),
        model: Some(model.into()),
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::from([(
            "client_conversation_id".into(),
            json!("00000000-0000-4000-8000-000000000001"),
        )]),
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a built Kiro component; run task build:vendors:all"]
async fn device_sign_in_omits_the_hint_and_authenticates_the_profile() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let services = Arc::new(FakeServices::default());
    let execute = async |input: OperationInput| {
        let (operation, input) = input.encode_for_host().unwrap();
        let scope = OperationScope::new(
            services.clone(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(10)),
            0,
        );
        runtime
            .execute(&plugin, "oauth", operation, input, scope)
            .await
            .unwrap()
    };
    let auth = |provider, step| OperationInput::Auth {
        provider,
        request: AuthRequest { step },
    };
    let mut profile = provider("qwen3-coder-next", "unused", "us-east-1");
    profile.channel = "oauth".into();
    profile.protocol = "kiro-generate-assistant-response-oauth".into();
    profile.credentials.clear();
    let OperationOutput::Auth(AuthResponse::Authorization {
        url,
        user_code,
        interval_seconds,
        ..
    }) = execute(auth(
        profile.clone(),
        AuthStep::Start {
            redirect_uri: String::new(),
            state: "host-state".into(),
        },
    ))
    .await
    else {
        panic!("expected device verification link");
    };
    assert_eq!(user_code.as_deref(), Some("ABCD-1234"));
    assert_eq!(
        url,
        "https://app.kiro.dev/account/device?user_code=ABCD-1234"
    );
    let interval = Duration::from_secs(u64::from(interval_seconds.unwrap()));
    tokio::time::sleep(interval).await;
    assert!(matches!(
        execute(auth(profile.clone(), AuthStep::Poll)).await,
        OperationOutput::Auth(AuthResponse::Pending { .. })
    ));
    tokio::time::sleep(interval).await;
    let OperationOutput::Auth(AuthResponse::Credentials {
        values,
        expires_at_unix_ms: Some(_),
    }) = execute(auth(profile.clone(), AuthStep::Poll)).await
    else {
        panic!("expected completed device authorization");
    };
    profile.credentials = values;
    let OperationOutput::Auth(AuthResponse::Credentials {
        values,
        expires_at_unix_ms: Some(_),
    }) = execute(auth(profile.clone(), AuthStep::Refresh)).await
    else {
        panic!("expected refreshed authorization and expiry");
    };
    assert_eq!(values["refresh_token"], "synthetic-refresh");
    profile.credentials = values;
    assert!(matches!(
        execute(OperationInput::Infer {
            provider: profile.clone(),
            request: request("qwen3-coder-next")
        })
        .await,
        OperationOutput::Infer(_)
    ));
    let OperationOutput::Discover(models) = execute(OperationInput::Discover {
        provider: profile.clone(),
        request: Default::default(),
    })
    .await
    else {
        panic!("expected discovered models");
    };
    let [without_effort, with_effort] = models.models.as_slice() else {
        panic!("expected both catalog models")
    };
    assert_eq!(without_effort.id, "qwen3-coder-next");
    assert!(!without_effort.metadata.contains_key("reasoning_efforts"));
    assert_eq!(with_effort.id, "claude-sonnet-4.6");
    assert_eq!(
        with_effort.metadata["reasoning_efforts"],
        json!(["low", "medium", "high", "max"])
    );
    assert_eq!(with_effort.metadata["kiro_effort_group"], "output_config");
    let OperationOutput::Allowance(_) = execute(OperationInput::Allowance {
        provider: profile,
        request: Default::default(),
    })
    .await
    else {
        panic!("expected Kiro credits");
    };

    let requests = services.requests.lock();
    let [start, pending, approved, refresh, infer, discover, quota] = requests.as_slice() else {
        panic!("device flow must issue start, two polls, refresh, inference, catalog and credits");
    };
    let body = |r: &HttpRequest| serde_json::from_slice::<Value>(&r.body).unwrap();
    assert_eq!(
        body(start),
        json!({"clientId":"Kiro-CLI","loginProvider":"Google"})
    );
    let poll_body = json!({"clientId":"Kiro-CLI","deviceCode":"synthetic-device"});
    assert_eq!(body(pending), poll_body);
    assert_eq!(body(approved), poll_body);
    assert_eq!(body(refresh), json!({"refreshToken":"synthetic-refresh"}));
    for auth in [start, pending, approved, refresh] {
        assert_eq!(auth.headers.get("User-Agent"), Some(b"Kiro-CLI".as_slice()));
    }
    let tui: Value = serde_json::from_str(include_str!("../kiro-cli-3.4.0-tui-http.json")).unwrap();
    for (request, url) in [
        (infer, "https://runtime.eu-central-1.kiro.dev/"),
        (discover, "https://management.eu-central-1.kiro.dev/"),
        (quota, "https://management.eu-central-1.kiro.dev/"),
    ] {
        assert_eq!(request.url, url);
        assert_eq!(
            request.headers.get("authorization"),
            Some(b"Bearer synthetic-rotated-access".as_slice())
        );
        assert!(request.headers.get("TokenType").is_none());
        assert!(request.headers.get("x-amzn-codewhisperer-optout").is_none());
        assert_eq!(
            body(request)["profileArn"],
            "arn:aws:codewhisperer:eu-central-1:123456789012:profile/synthetic"
        );
        for name in ["x-amz-user-agent", "user-agent", "amz-sdk-request"] {
            assert_eq!(
                request.headers.get(name),
                Some(tui_header(&tui, name).as_bytes())
            );
        }
    }
    for name in [
        "x-amzn-kiro-client-attribution",
        "x-amzn-kiro-mode-identifier",
        "x-amzn-kiro-fallback",
        "x-amzn-kiro-attempt",
    ] {
        assert_eq!(
            infer.headers.get(name),
            Some(tui_header(&tui, name).as_bytes())
        );
        assert!(discover.headers.get(name).is_none());
        assert!(quota.headers.get(name).is_none());
    }
    assert_eq!(body(discover)["origin"], "KIRO_CLI");
    assert_eq!(body(quota)["origin"], "KIRO_CLI");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a built Kiro component; run task build:vendors:all"]
async fn allowance_reads_the_regional_management_endpoint_without_generating() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let services = Arc::new(FakeServices::default());
    let scope = OperationScope::new(
        services.clone(),
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(60)),
        0,
    );
    let (op, input) = OperationInput::Allowance {
        provider: provider("qwen3-coder-next", "synthetic-key", "eu-central-1"),
        request: Default::default(),
    }
    .encode_for_host()
    .unwrap();
    let OperationOutput::Allowance(result) = runtime
        .execute(&plugin, "oauth", op, input, scope)
        .await
        .unwrap()
    else {
        panic!("expected allowance output")
    };
    let remaining = result.allowances[0].remaining.as_ref().unwrap();
    assert_eq!(remaining.value, "0.01");
    assert_eq!(remaining.unit, "credits");
    let requests = services.requests.lock();
    let [request] = requests.as_slice() else {
        panic!("quota read must not generate")
    };
    assert_eq!(request.url, "https://management.eu-central-1.kiro.dev/");
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).unwrap(),
        json!({"origin":"KIRO_CLI", "profileArn":"arn:aws:codewhisperer:eu-central-1:123456789012:profile/synthetic"})
    );
    assert!(request.headers.get("x-amzn-kiro-mode-identifier").is_none());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a built Kiro component; run task build:vendors:all"]
async fn offline_component_inference_contract() {
    let bytes = std::fs::read(artifact()).expect("read Kiro component");
    let runtime = VendorRuntime::new().expect("runtime");
    let plugin = runtime.load(&bytes).await.expect("load Kiro component");
    let services = Arc::new(FakeServices::default());

    const TURN_ID: &str = "00000000-0000-4000-8000-000000000002";
    for (model, key, region, turn_id) in [
        (
            "qwen3-coder-next",
            "synthetic-key-one",
            "eu-central-1",
            Some(TURN_ID),
        ),
        ("claude-haiku-4.5", "synthetic-key-two", "us-east-1", None),
    ] {
        let scope = OperationScope::new(
            services.clone(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(60)),
            0,
        );
        // A differing client model ensures the connection's selected model wins.
        let mut provider = provider(model, key, region);
        if let Some(turn_id) = turn_id {
            provider
                .operation_metadata
                .insert("user_prompt_id".into(), json!(turn_id));
        }
        let input = OperationInput::Infer {
            provider,
            request: request("client-alias"),
        };
        let (operation, input) = input.encode_for_host().expect("encode inference");
        let output = runtime
            .execute(&plugin, "oauth", operation, input, scope)
            .await
            .expect("component inference succeeds");
        let OperationOutput::Infer(response) = output else {
            panic!("expected infer output")
        };
        assert!(response.error.is_none(), "{:?}", response.error);
    }

    // A non-secret option must never substitute for a connection-scoped token.
    let mut missing = provider("qwen3-coder-next", "unused", "eu-central-1");
    missing.credentials.clear();
    missing
        .options
        .insert("access_token".into(), json!("synthetic-option-decoy"));
    let scope = OperationScope::new(
        services.clone(),
        CancellationToken::new(),
        Deadline::from_now(Duration::from_secs(60)),
        0,
    );
    let (operation, input) = OperationInput::Infer {
        provider: missing,
        request: request("client-alias"),
    }
    .encode_for_host()
    .expect("encode inference without key");
    let rejected = runtime
        .execute(&plugin, "oauth", operation, input, scope)
        .await;
    match rejected {
        Err(_) => {}
        Ok(OperationOutput::Infer(response)) => {
            assert!(response.error.is_some(), "missing key must fail")
        }
        Ok(_) => panic!("missing key must not succeed"),
    }

    let requests = services.requests.lock();
    assert_eq!(requests.len(), 2, "missing scoped key must not start HTTP");
    for (request, (model, key, region)) in requests.iter().zip([
        ("qwen3-coder-next", "synthetic-key-one", "eu-central-1"),
        ("claude-haiku-4.5", "synthetic-key-two", "us-east-1"),
    ]) {
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, format!("https://runtime.{region}.kiro.dev/"));
        let header = |name: &str| {
            std::str::from_utf8(request.headers.get(name).expect("header present"))
                .expect("UTF-8 header")
        };
        assert_eq!(header("authorization"), format!("Bearer {key}"));
        assert!(request.headers.get("tokenType").is_none());
        assert!(request.headers.get("TokenType").is_none());
        assert_eq!(header("content-type"), "application/x-amz-json-1.0");
        assert_eq!(
            header("x-amz-target"),
            "KiroRuntimeService.GenerateAssistantResponse"
        );
        let body: Value = serde_json::from_slice(&request.body).expect("JSON upstream body");
        assert_eq!(
            body["profileArn"],
            format!("arn:aws:codewhisperer:{region}:123456789012:profile/synthetic")
        );
        let state = &body["conversationState"];
        assert_eq!(state["chatTriggerType"], "MANUAL");
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["content"],
            "say hello"
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["modelId"],
            model
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["origin"],
            "KIRO_CLI"
        );
        assert_eq!(state["history"], json!([]));
        assert_eq!(
            state["conversationId"],
            "sess_00000000-0000-4000-8000-000000000001"
        );
        assert_eq!(state["rootConversationId"], state["conversationId"]);
        let continuation = state["agentContinuationId"]
            .as_str()
            .expect("continuation ID");
        if model == "qwen3-coder-next" {
            assert_eq!(continuation, TURN_ID);
        } else {
            assert_eq!(
                uuid::Uuid::parse_str(continuation)
                    .unwrap()
                    .get_version_num(),
                4
            );
        }
        assert!(
            !String::from_utf8_lossy(&request.body).contains(key),
            "credential leaked into body"
        );
    }
    drop(requests);

    let events = services.events.lock();
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Delta(AiStreamDelta::TextDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello worldhello world");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                RuntimeEvent::Delta(AiStreamDelta::Done { stop_reason }) if stop_reason == "stop"
            ))
            .count(),
        2
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a built Kiro component; run task build:vendors:all"]
async fn descriptor_exposes_four_device_channels_and_enterprise_fields() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let descriptor = plugin.descriptor();
    let provider = &descriptor.providers[0];
    assert_eq!(
        provider
            .channels
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["oauth", "github", "builder-id", "identity-center"]
    );
    for channel in &provider.channels {
        assert!(channel.auth.is_some());
        assert_eq!(
            channel.protocol.as_deref(),
            Some(if channel.id == "identity-center" {
                "kiro-generate-assistant-response-sso-oidc"
            } else {
                "kiro-generate-assistant-response-oauth"
            })
        );
    }
    for key in ["start_url", "region"] {
        let field = provider
            .config_fields
            .iter()
            .find(|f| f.key == key)
            .unwrap();
        assert!(field.required);
        assert!(!field.secret);
        assert_eq!(
            field.visible_when.as_ref().unwrap().equals,
            json!("kiro-generate-assistant-response-sso-oidc")
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a built Kiro component; run task build:vendors:all"]
async fn github_device_authorization_uses_provider_hint() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let services = Arc::new(FakeServices::default());
    let mut profile = provider("qwen3-coder-next", "unused", "us-east-1");
    profile.channel = "github".into();
    profile.credentials.clear();
    let execute = async |step| {
        let (op, input) = OperationInput::Auth {
            provider: profile.clone(),
            request: AuthRequest { step },
        }
        .encode_for_host()
        .unwrap();
        let scope = OperationScope::new(
            services.clone(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(10)),
            0,
        );
        runtime
            .execute(&plugin, "github", op, input, scope)
            .await
            .unwrap()
    };
    let OperationOutput::Auth(AuthResponse::Authorization {
        url,
        interval_seconds: Some(interval),
        ..
    }) = execute(AuthStep::Start {
        redirect_uri: String::new(),
        state: String::new(),
    })
    .await
    else {
        panic!("expected GitHub authorization")
    };
    assert_eq!(
        url,
        "https://app.kiro.dev/account/device?user_code=ABCD-1234&login_provider=github"
    );
    tokio::time::sleep(Duration::from_secs(u64::from(interval))).await;
    assert!(matches!(
        execute(AuthStep::Poll).await,
        OperationOutput::Auth(AuthResponse::Pending { .. })
    ));
    tokio::time::sleep(Duration::from_secs(u64::from(interval))).await;
    let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) =
        execute(AuthStep::Poll).await
    else {
        panic!("expected GitHub completion")
    };
    assert_eq!(values["access_token"], "synthetic-access");
    assert_eq!(
        values["profileArn"],
        "arn:aws:codewhisperer:eu-central-1:123456789012:profile/synthetic"
    );
    let requests = services.requests.lock();
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"clientId":"Kiro-CLI", "loginProvider":"Github"})
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a built Kiro component; run task build:vendors:all"]
async fn oidc_device_registration_refresh_and_profile_routing() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    for (channel, oidc_region, start_url, arn) in [
        (
            "builder-id",
            "us-east-1",
            "https://view.awsapps.com/start",
            "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX",
        ),
        (
            "identity-center",
            "eu-west-1",
            "https://example.awsapps.com/start",
            "arn:aws:codewhisperer:eu-central-1:123456789012:profile/enterprise",
        ),
    ] {
        let services = Arc::new(FakeServices::default());
        let mut profile = provider("qwen3-coder-next", "unused", "us-east-1");
        profile.channel = channel.into();
        profile.credentials.clear();
        if channel == "identity-center" {
            profile.protocol = "kiro-generate-assistant-response-sso-oidc".into();
            profile.options.insert("region".into(), json!(oidc_region));
            profile.options.insert("start_url".into(), json!(start_url));
        }
        let execute = async |input: OperationInput| {
            let (op, input) = input.encode_for_host().unwrap();
            let scope = OperationScope::new(
                services.clone(),
                CancellationToken::new(),
                Deadline::from_now(Duration::from_secs(10)),
                0,
            );
            runtime
                .execute(&plugin, channel, op, input, scope)
                .await
                .unwrap()
        };
        let auth = |provider, step| OperationInput::Auth {
            provider,
            request: AuthRequest { step },
        };
        let OperationOutput::Auth(AuthResponse::Authorization {
            url,
            interval_seconds: Some(interval),
            ..
        }) = execute(auth(
            profile.clone(),
            AuthStep::Start {
                redirect_uri: String::new(),
                state: String::new(),
            },
        ))
        .await
        else {
            panic!("expected OIDC authorization")
        };
        assert_eq!(url, "https://example.awsapps.com/start?user_code=OIDC-1234");
        let private = services.private_state.lock().clone().unwrap();
        assert!(
            String::from_utf8(private)
                .unwrap()
                .contains("synthetic-client-secret")
        );
        tokio::time::sleep(Duration::from_secs(u64::from(interval))).await;
        let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) =
            execute(auth(profile.clone(), AuthStep::Poll)).await
        else {
            panic!("expected OIDC credentials")
        };
        assert_eq!(values["profileArn"], arn);
        assert_eq!(values["oidc_region"], oidc_region);
        assert_eq!(values["oidc_enterprise"], channel == "identity-center");
        profile.credentials = values;
        let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) =
            execute(auth(profile.clone(), AuthStep::Refresh)).await
        else {
            panic!("expected OIDC refresh")
        };
        assert_eq!(values["profileArn"], arn);
        profile.credentials = values;
        assert!(matches!(
            execute(OperationInput::Infer {
                provider: profile.clone(),
                request: request("qwen3-coder-next")
            })
            .await,
            OperationOutput::Infer(_)
        ));
        assert!(matches!(
            execute(OperationInput::Allowance {
                provider: profile,
                request: Default::default()
            })
            .await,
            OperationOutput::Allowance(_)
        ));
        let requests = services.requests.lock();
        let body = |r: &HttpRequest| serde_json::from_slice::<Value>(&r.body).unwrap();
        assert_eq!(
            requests[0].url,
            format!("https://oidc.{oidc_region}.amazonaws.com/client/register")
        );
        assert_eq!(
            body(&requests[0]),
            json!({"clientName":"Kiro CLI", "clientType":"public", "scopes":["codewhisperer:completions", "codewhisperer:analysis", "codewhisperer:conversations"]})
        );
        assert_eq!(
            requests[1].url,
            format!("https://oidc.{oidc_region}.amazonaws.com/device_authorization")
        );
        assert_eq!(
            body(&requests[1]),
            json!({"clientId":"synthetic-client", "clientSecret":"synthetic-client-secret", "startUrl":start_url})
        );
        assert_eq!(
            body(&requests[2]),
            json!({"clientId":"synthetic-client", "clientSecret":"synthetic-client-secret", "grantType":"urn:ietf:params:oauth:grant-type:device_code", "deviceCode":"synthetic-oidc-device"})
        );
        let refresh = requests
            .iter()
            .find(|r| r.url.ends_with("/token") && body(r)["grantType"] == "refresh_token")
            .unwrap();
        assert_eq!(
            refresh.url,
            format!("https://oidc.{oidc_region}.amazonaws.com/token")
        );
        assert_eq!(
            body(refresh),
            json!({"clientId":"synthetic-client", "clientSecret":"synthetic-client-secret", "grantType":"refresh_token", "refreshToken":"synthetic-oidc-refresh"})
        );
        let destination_region = if channel == "identity-center" {
            "eu-central-1"
        } else {
            "us-east-1"
        };
        for request in requests.iter().filter(|r| {
            r.url.starts_with("https://runtime.")
                || r.headers.get("x-amz-target")
                    == Some(b"KiroControlPlaneBearerService.GetUsageLimits".as_slice())
        }) {
            assert!(request.url.contains(destination_region));
            assert_eq!(body(request)["profileArn"], arn);
            if channel == "identity-center" {
                assert_eq!(
                    request.headers.get("TokenType"),
                    Some(b"SSO_OIDC".as_slice())
                );
            }
            assert!(!String::from_utf8_lossy(&request.body).contains("synthetic-client-secret"));
        }
        if channel == "identity-center" {
            assert!(
                requests
                    .iter()
                    .any(|r| r.url == "https://management.eu-central-1.kiro.dev/"
                        && r.headers.get("x-amz-target")
                            == Some(
                                b"AmazonCodeWhispererService.ListAvailableProfiles".as_slice()
                            ))
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a built Kiro component; run task build:vendors:all"]
async fn enterprise_device_sign_in_rejects_organization_opt_out() {
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact()).unwrap())
        .await
        .unwrap();
    let services = Arc::new(FakeServices {
        enterprise_opt_out: true,
        ..Default::default()
    });
    let mut profile = provider("qwen3-coder-next", "unused", "us-east-1");
    profile.channel = "identity-center".into();
    profile.protocol = "kiro-generate-assistant-response-sso-oidc".into();
    profile.credentials.clear();
    profile.options.insert("region".into(), json!("eu-west-1"));
    profile.options.insert(
        "start_url".into(),
        json!("https://example.awsapps.com/start"),
    );
    let execute = async |step| {
        let (op, input) = OperationInput::Auth {
            provider: profile.clone(),
            request: AuthRequest { step },
        }
        .encode_for_host()
        .unwrap();
        let scope = OperationScope::new(
            services.clone(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(10)),
            0,
        );
        runtime
            .execute(&plugin, "identity-center", op, input, scope)
            .await
    };
    let OperationOutput::Auth(AuthResponse::Authorization {
        interval_seconds: Some(interval),
        ..
    }) = execute(AuthStep::Start {
        redirect_uri: String::new(),
        state: String::new(),
    })
    .await
    .unwrap()
    else {
        panic!("expected authorization")
    };
    tokio::time::sleep(Duration::from_secs(u64::from(interval))).await;
    assert!(execute(AuthStep::Poll).await.is_err());
    let requests = services.requests.lock();
    let lookups: Vec<_> = requests
        .iter()
        .filter(|r| {
            r.headers.get("x-amz-target")
                == Some(b"AmazonCodeWhispererService.ListAvailableProfiles".as_slice())
        })
        .collect();
    assert_eq!(lookups.len(), 2);
    assert_eq!(lookups[0].url, "https://management.eu-central-1.kiro.dev/");
    assert_eq!(lookups[1].url, "https://management.us-east-1.kiro.dev/");
}
