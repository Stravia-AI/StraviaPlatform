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
use stravia_vendor_sdk::{ErrorKind, OperationInput, OperationOutput, ProviderSnapshot};

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
}

#[async_trait]
impl HostServices for FakeServices {
    fn http_start(
        &self,
        request: HttpRequest,
        _options: Options,
    ) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let quota = request.headers.get("x-amz-target")
            == Some(b"KiroControlPlaneBearerService.GetUsageLimits".as_slice());
        self.requests.lock().push(request);
        if quota {
            return Ok(Arc::new(FakeResponse {
                content_type: b"application/x-amz-json-1.0",
                body: Mutex::new(vec![br#"{"usageBreakdownList":[{"resourceType":"CREDIT","unit":"INVOCATIONS","usageLimit":10,"currentUsageWithPrecision":9.99}]}"#.to_vec()].into_iter()),
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
        channel: "default".into(),
        base_url: format!("https://runtime.{region}.kiro.dev"),
        protocol: "kiro-generate-assistant-response".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::from([("apiKey".into(), json!(key))]),
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
        .execute(&plugin, "default", op, input, scope)
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
        json!({"origin":"KIRO_CLI"})
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

    for (model, key, region) in [
        ("qwen3-coder-next", "synthetic-key-one", "eu-central-1"),
        ("claude-haiku-4.5", "synthetic-key-two", "us-east-1"),
    ] {
        let scope = OperationScope::new(
            services.clone(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(60)),
            0,
        );
        // A differing client model ensures the connection's selected model wins.
        let input = OperationInput::Infer {
            provider: provider(model, key, region),
            request: request("client-alias"),
        };
        let (operation, input) = input.encode_for_host().expect("encode inference");
        let output = runtime
            .execute(&plugin, "default", operation, input, scope)
            .await
            .expect("component inference succeeds");
        let OperationOutput::Infer(response) = output else {
            panic!("expected infer output")
        };
        assert!(response.error.is_none(), "{:?}", response.error);
    }

    // A non-secret option must never substitute for a connection-scoped key.
    let mut missing = provider("qwen3-coder-next", "unused", "eu-central-1");
    missing.credentials.clear();
    missing
        .options
        .insert("apiKey".into(), json!("synthetic-option-decoy"));
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
        .execute(&plugin, "default", operation, input, scope)
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
        assert_eq!(header("tokenType"), "API_KEY");
        assert_eq!(header("content-type"), "application/x-amz-json-1.0");
        assert_eq!(
            header("x-amz-target"),
            "KiroRuntimeService.GenerateAssistantResponse"
        );
        let body: Value = serde_json::from_slice(&request.body).expect("JSON upstream body");
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
