use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request as AxumRequest, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::Response;
use stravia_core::Gateway;
use stravia_core::config::GatewayConfig;
use stravia_core::db::models::{
    CreateApiKey, CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput,
    ProviderSourceInput, UpdateRoute, UpsertTarget,
};
use stravia_core::plugin::{ConfirmPluginUpdate, PluginSource};
use stravia_core::provider_models::CreateManualProviderModel;
use stravia_core::proxy::server::create_router;
use stravia_runtime_contract::protocol::ir::AiResponse;
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

mod vendor_observation;
use vendor_observation::{finished_observation, observation_bundle_records};

const LIFECYCLE_VENDOR: &str = "fixture.lifecycle";
const OTHER_VENDOR: &str = "fixture.lifecycle.other";

struct PendingUpstream {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Vec<u8>,
    response: oneshot::Sender<UpstreamReply>,
}

impl PendingUpstream {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn reply(self, reply: UpstreamReply) {
        let _ = self.response.send(reply);
    }
}

struct UpstreamReply {
    status: StatusCode,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    truncate: bool,
}

impl UpstreamReply {
    fn model(text: &str) -> Self {
        let mut response = AiResponse::new("fixture-response", "fixture-model");
        response.push_output_text(text);
        Self {
            status: StatusCode::OK,
            headers: vec![("content-type", "application/json".into())],
            body: serde_json::to_vec(&response).expect("serialize fixture response"),
            truncate: false,
        }
    }

    fn redirect(location: String) -> Self {
        Self {
            status: StatusCode::FOUND,
            headers: vec![("location", location)],
            body: Vec::new(),
            truncate: false,
        }
    }

    fn ndjson(lines: &[serde_json::Value]) -> Self {
        let mut body = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        body.push(b'\n');
        Self {
            status: StatusCode::OK,
            headers: vec![("content-type", "application/x-ndjson".into())],
            body,
            truncate: false,
        }
    }

    fn truncated() -> Self {
        Self {
            status: StatusCode::OK,
            headers: vec![
                ("content-type", "application/json".into()),
                ("content-length", "4096".into()),
            ],
            body: br#"{"id":"cut-off"#.to_vec(),
            truncate: true,
        }
    }
}

struct TestUpstream {
    base_url: String,
    requests: mpsc::UnboundedReceiver<PendingUpstream>,
    task: tokio::task::JoinHandle<()>,
}

impl TestUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture upstream");
        let address = listener.local_addr().expect("fixture upstream address");
        let (sender, requests) = mpsc::unbounded_channel();
        let router = Router::new().fallback(capture_upstream).with_state(sender);
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve fixture upstream");
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            task,
        }
    }

    async fn next(&mut self) -> PendingUpstream {
        self.requests
            .recv()
            .await
            .expect("fixture upstream request")
    }

    async fn next_for(
        &mut self,
        call: &mut tokio::task::JoinHandle<(StatusCode, serde_json::Value)>,
    ) -> PendingUpstream {
        tokio::select! {
            request = self.next() => request,
            response = call => panic!("call ended before reaching the expected upstream: {response:?}"),
        }
    }

    fn assert_no_request(&mut self) {
        assert!(
            self.requests.try_recv().is_err(),
            "unexpected additional upstream request"
        );
    }
}

impl Drop for TestUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn capture_upstream(
    State(sender): State<mpsc::UnboundedSender<PendingUpstream>>,
    request: AxumRequest,
) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 4 * 1024 * 1024)
        .await
        .expect("read fixture request")
        .to_vec();
    let (response, receiver) = oneshot::channel();
    sender
        .send(PendingUpstream {
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body,
            response,
        })
        .expect("test still receives fixture request");
    let reply = receiver.await.unwrap_or(UpstreamReply {
        status: StatusCode::GONE,
        headers: Vec::new(),
        body: Vec::new(),
        truncate: false,
    });
    let mut builder = Response::builder().status(reply.status);
    for (name, value) in reply.headers {
        builder = builder.header(name, value);
    }
    let body = if reply.truncate {
        Body::from_stream(futures::stream::iter([
            Ok(axum::body::Bytes::from(reply.body)),
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "intentional fixture truncation",
            )),
        ]))
    } else {
        Body::from(reply.body)
    };
    builder.body(body).expect("fixture response")
}

#[derive(Clone)]
struct Connection {
    provider_id: String,
    route_id: String,
    target_id: String,
}

async fn new_gateway(data_dir: PathBuf) -> anyhow::Result<Gateway> {
    Gateway::new(GatewayConfig {
        data_dir,
        ..GatewayConfig::default()
    })
    .await
}

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-test-fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "missing lifecycle fixture {} ({error}); run `task build:vendor-fixtures`",
            path.display()
        )
    })
}

async fn install(
    gateway: &Gateway,
    artifact: &str,
    allow_data_discard: bool,
) -> anyhow::Result<stravia_core::plugin::PluginSummary> {
    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture(artifact))
        .await?;
    gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard,
        })
        .await
}

async fn connection(
    gateway: &Gateway,
    vendor: &str,
    name: &str,
    route_id: &str,
    base_url: &str,
    secret: &str,
    options: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<Connection> {
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some(name.into()),
            source: ProviderSourceInput::Custom {
                vendor: vendor.into(),
                channel: "default".into(),
                protocol: Some("fixture-lifecycle".into()),
                base_url: base_url.into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::Fields {
                values: BTreeMap::from([(
                    "apiKey".into(),
                    serde_json::Value::String(secret.into()),
                )]),
            },
            vendor_options: options,
            use_proxy: false,
        })
        .await?;
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "fixture-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "fixture-model",
                    "name": "Fixture model",
                    "tool_call": true
                }),
            },
        )
        .await?;
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: route_id.into(),
            display_name: None,
            balance: None,
            target_provider: String::new(),
            target_model: None,
            targets: vec![CreateTarget {
                enabled: true,
                provider_id: provider.id.clone(),
                model: Some("fixture-model".into()),
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: Some(0),
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;
    Ok(Connection {
        provider_id: provider.id,
        target_id: route.targets[0].id.clone(),
        route_id: route.id,
    })
}

async fn api_key(gateway: &Gateway, routes: &[&str]) -> anyhow::Result<String> {
    Ok(gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "Lifecycle fixture key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: routes.iter().map(|route| (*route).into()).collect(),
        })
        .await?
        .token)
}

async fn invoke(router: Router, token: String, model: String) -> (StatusCode, serde_json::Value) {
    invoke_with_previous(router, token, model, None).await
}

async fn invoke_with_previous(
    router: Router,
    token: String,
    model: String,
    previous_response_id: Option<String>,
) -> (StatusCode, serde_json::Value) {
    let mut request = serde_json::json!({
        "model": model,
        "input": "exercise the lifecycle fixture",
        "store": true
    });
    if let Some(previous_response_id) = previous_response_id {
        request["previous_response_id"] = serde_json::Value::String(previous_response_id);
    }
    let response = router
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(request.to_string()))
                .expect("proxy request"),
        )
        .await
        .expect("proxy response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("proxy response body");
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::json!({ "unparsed": String::from_utf8_lossy(&bytes) }));
    (status, body)
}

async fn invoke_stream(
    router: Router,
    token: String,
    model: String,
) -> (StatusCode, String, String) {
    let response = router
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({
                        "model": model,
                        "input": "exercise the lifecycle fixture",
                        "store": true,
                        "stream": true
                    })
                    .to_string(),
                ))
                .expect("streaming proxy request"),
        )
        .await
        .expect("streaming proxy response");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("streaming proxy response body");
    let body = String::from_utf8(bytes.to_vec()).expect("UTF-8 streaming proxy response");
    (status, content_type, body)
}

fn assert_success_with(response: &(StatusCode, serde_json::Value), expected: &str) {
    assert_eq!(response.0, StatusCode::OK, "proxy response: {}", response.1);
    assert!(
        response.1.to_string().contains(expected),
        "response did not contain {expected:?}: {}",
        response.1
    );
}

fn options(entries: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).into(), serde_json::Value::String((*value).into())))
        .collect()
}

#[tokio::test]
async fn real_wasm_stream_preserves_typed_quota_over_http_429() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "typed quota account",
        "lifecycle-typed-quota",
        &upstream.base_url,
        "quota-secret",
        options(&[("mode", "typed-quota")]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let call = tokio::spawn(invoke_stream(
        create_router(gateway),
        token,
        target.route_id,
    ));

    let request = upstream.next().await;
    assert_eq!(request.method, Method::POST);
    assert_eq!(request.uri.path(), "/infer");
    request.reply(UpstreamReply {
        status: StatusCode::TOO_MANY_REQUESTS,
        headers: vec![("content-type", "application/json".into())],
        body: br#"{"error":{"type":"rate_limit_error"}}"#.to_vec(),
        truncate: false,
    });

    let (status, content_type, body) = call.await?;
    assert_eq!(status, StatusCode::OK, "streaming proxy response: {body}");
    assert!(
        content_type.starts_with("text/event-stream"),
        "streaming response content type: {content_type}"
    );
    let events = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str::<serde_json::Value>(data).expect("SSE JSON event"))
        .collect::<Vec<_>>();
    let partial_index = events
        .iter()
        .position(|event| {
            event["type"] == "response.output_text.delta"
                && event["delta"] == "visible partial before quota"
        })
        .expect("visible partial text delta");
    let error_index = events
        .iter()
        .position(|event| event["type"] == "error")
        .expect("public error event");
    let failed_index = events
        .iter()
        .position(|event| event["type"] == "response.failed")
        .expect("failed terminal event");
    assert!(partial_index < error_index && error_index < failed_index);
    let public_error = &events[error_index]["error"];
    assert_eq!(public_error["type"], "quota_exceeded");
    assert_eq!(public_error["code"], "quota_exceeded");
    assert_eq!(events[failed_index]["response"]["status"], "failed");
    assert_eq!(
        events[failed_index]["response"]["error"],
        events[error_index]["error"]
    );
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "response.completed"),
        "failed stream must not become a successful completion: {body}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"));
    upstream.assert_no_request();
    Ok(())
}

#[tokio::test]
async fn vendor_network_diagnostics_preserve_every_selected_protocol_ndjson_record()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    gateway.admin().set_observation_debug(true);
    let mut observations = gateway.admin().observation_subscribe(0);
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "diagnostic NDJSON account",
        "lifecycle-diagnostic-ndjson",
        &upstream.base_url,
        "license-7B9Q-X5M2",
        options(&[("mode", "diagnostics")]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let router = create_router(gateway.clone());
    let call = tokio::spawn(invoke(router, token, target.route_id));
    let request = upstream.next().await;
    let lines = vec![
        serde_json::json!({"type": "text-delta", "text": "first-ndjson-business"}),
        serde_json::json!({"type": "finish", "text": "second-ndjson-business"}),
    ];
    request.reply(UpstreamReply::ndjson(&lines));
    assert_success_with(&call.await?, "first-ndjson-business");
    let (interaction_id, _) = finished_observation(&mut observations).await?;
    let records = observation_bundle_records(&gateway, &interaction_id).await?;
    let payloads = records
        .iter()
        .filter(|record| {
            record["direction"] == "upstream_response"
                && record["protocol"] == "command-code/generate/v1"
                && record["message_type"] == "body_chunk"
                && record["representation"] == "reassembled_application_message"
        })
        .map(|record| {
            record["payload"]
                .as_str()
                .expect("NDJSON diagnostic record retains its text")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        payloads,
        lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[tokio::test]
async fn vendor_network_diagnostics_redact_snapshot_and_late_private_secrets_from_trace()
-> anyhow::Result<()> {
    const LICENSE: &str = "license-7B9Q-X5M2";
    const PRIVATE_STATE: &str = "state-4N8P-C6R3";

    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    gateway.admin().set_observation_debug(true);
    let mut observations = gateway.admin().observation_subscribe(0);
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "diagnostic secret account",
        "lifecycle-diagnostic-secrets",
        &upstream.base_url,
        LICENSE,
        options(&[("mode", "diagnostics")]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let router = create_router(gateway.clone());
    let call = tokio::spawn(invoke(router, token, target.route_id));
    let request = upstream.next().await;
    request.reply(UpstreamReply::ndjson(&[
        serde_json::json!({
            "type": "text-delta",
            "text": format!("first-live-business {LICENSE}"),
        }),
        serde_json::json!({
            "type": "finish",
            "text": format!("second-live-business {PRIVATE_STATE}"),
        }),
    ]));
    assert_success_with(&call.await?, "first-live-business");

    let (interaction_id, live) = finished_observation(&mut observations).await?;
    assert!(
        live.contains("first-live-business"),
        "live diagnostic content: {live}"
    );
    assert!(
        live.contains("second-live-business"),
        "live diagnostic content: {live}"
    );
    assert!(live.contains("***"), "live diagnostic content: {live}");
    assert!(!live.contains(LICENSE), "live diagnostic content: {live}");
    assert!(
        !live.contains(PRIVATE_STATE),
        "live diagnostic content: {live}"
    );

    let records = observation_bundle_records(&gateway, &interaction_id).await?;
    let persisted = serde_json::to_string(&records)?;
    assert!(persisted.contains("retained-network-business-field"));
    assert!(persisted.contains("first-live-business"));
    assert!(persisted.contains("second-live-business"));
    assert!(persisted.contains("***"));
    assert!(
        !persisted.contains(LICENSE),
        "persisted diagnostics: {persisted}"
    );
    assert!(
        !persisted.contains(PRIVATE_STATE),
        "persisted diagnostics: {persisted}"
    );
    Ok(())
}

#[tokio::test]
async fn protocol_selection_errors_protect_connection_secrets_in_diagnostics() -> anyhow::Result<()>
{
    const LICENSE: &str = "license-7B9Q-X5M2";
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    gateway.admin().set_observation_debug(true);
    let mut observations = gateway.admin().observation_subscribe(0);
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "selection error account",
        "lifecycle-selection-error",
        &upstream.base_url,
        LICENSE,
        options(&[("mode", "selection-error")]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;

    let failed = invoke(create_router(gateway.clone()), token, target.route_id).await;
    assert!(failed.0.is_server_error());
    assert!(!failed.1.to_string().contains(LICENSE));
    upstream.assert_no_request();

    let (_, live) = finished_observation(&mut observations).await?;
    assert!(live.contains("selection-business-rejected"));
    assert!(!live.contains(LICENSE));
    let admin = gateway.admin();
    admin.observation_flush().await?;
    let failures = admin.failed_requests(Default::default()).await?;
    let persisted = serde_json::to_string(&failures)?;
    assert!(
        failures.items.iter().any(|failure| {
            failure
                .error
                .message
                .as_deref()
                .is_some_and(|message| message.contains("selection-business-rejected"))
        }),
        "persisted failure diagnostics: {persisted}"
    );
    assert!(!persisted.contains(LICENSE));
    Ok(())
}

async fn assert_wasm_fault_is_scoped(mode: &str) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let fault_route = format!("lifecycle-{mode}");
    let fault = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        &format!("{mode} account"),
        &fault_route,
        &upstream.base_url,
        "fault-secret",
        options(&[("mode", mode)]),
    )
    .await?;
    let healthy = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "healthy account",
        "lifecycle-after-fault",
        &upstream.base_url,
        "healthy-secret",
        options(&[]),
    )
    .await?;
    let token = api_key(&gateway, &[&fault.route_id, &healthy.route_id]).await?;
    let router = create_router(gateway.clone());

    let mut fault_call = tokio::spawn(invoke(router.clone(), token.clone(), fault.route_id));
    let failed = tokio::select! {
        response = &mut fault_call => response?,
        request = upstream.next() => {
            request.reply(UpstreamReply::model("resource limit was bypassed"));
            panic!("{mode} exceeded its budget but continued with network I/O");
        }
    };
    assert_eq!(
        failed.0,
        StatusCode::BAD_GATEWAY,
        "unexpected fault response: {}",
        failed.1
    );
    assert_eq!(
        failed.1.get("usage"),
        None,
        "failed operation must not report known usage: {}",
        failed.1
    );
    upstream.assert_no_request();

    let healthy_call = tokio::spawn(invoke(router, token, healthy.route_id));
    let request = upstream.next().await;
    assert_eq!(request.header("x-state-before"), Some("0"));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer healthy-secret")
    );
    request.reply(UpstreamReply::model("healthy-after-wasm-fault"));
    assert_success_with(&healthy_call.await?, "healthy-after-wasm-fault");

    let summary = gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == LIFECYCLE_VENDOR)
        .expect("lifecycle plugin remains installed");
    assert_eq!(summary.version, "1.0.0");
    assert_eq!(summary.status, "ready");
    Ok(())
}

#[tokio::test]
async fn invalid_component_or_contract_never_replaces_the_working_generation() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    let installed = install(&gateway, "lifecycle-v1.wasm", false).await?;
    assert_eq!(installed.version, "1.0.0");
    assert_eq!(installed.source, PluginSource::Local);

    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "working lifecycle provider",
        "lifecycle-working",
        &upstream.base_url,
        "working-secret",
        options(&[]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let router = create_router(gateway.clone());

    let first = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        target.route_id.clone(),
    ));
    let request = upstream.next().await;
    assert_eq!(request.header("x-fixture-version"), Some("1.0.0"));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer working-secret")
    );
    request.reply(UpstreamReply::model("working-before-rejection"));
    assert_success_with(&first.await?, "working-before-rejection");

    let malformed = gateway
        .admin()
        .preview_vendor_plugin(b"not a WebAssembly component".to_vec())
        .await;
    assert!(malformed.is_err());
    let unsupported = gateway
        .admin()
        .preview_vendor_plugin(fixture("lifecycle-host-incompatible.wasm"))
        .await;
    assert!(unsupported.is_err());

    let second = tokio::spawn(invoke(router, token, target.route_id));
    let request = upstream.next().await;
    assert_eq!(request.header("x-fixture-version"), Some("1.0.0"));
    assert_eq!(request.header("x-state-before"), Some("1"));
    request.reply(UpstreamReply::model("working-after-rejection"));
    assert_success_with(&second.await?, "working-after-rejection");
    Ok(())
}

#[tokio::test]
async fn provider_state_secrets_and_network_grants_remain_connection_scoped() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut base = TestUpstream::start().await;
    let mut auxiliary = TestUpstream::start().await;
    let mut forbidden = TestUpstream::start().await;

    let first = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "first account",
        "lifecycle-first",
        &base.base_url,
        "first-secret",
        options(&[]),
    )
    .await?;
    let second = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "second account",
        "lifecycle-second",
        &base.base_url,
        "second-secret",
        options(&[]),
    )
    .await?;
    let auxiliary_target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "auxiliary origin",
        "lifecycle-aux",
        &base.base_url,
        "aux-secret",
        options(&[("mode", "aux"), ("auxUrl", &auxiliary.base_url)]),
    )
    .await?;
    let forbidden_target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "undeclared origin",
        "lifecycle-forbidden",
        &base.base_url,
        "forbidden-secret",
        options(&[("mode", "target"), ("targetUrl", &forbidden.base_url)]),
    )
    .await?;
    let redirect_target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "redirect origin",
        "lifecycle-redirect",
        &base.base_url,
        "redirect-secret",
        options(&[("mode", "redirect"), ("auxUrl", &auxiliary.base_url)]),
    )
    .await?;
    let token = api_key(
        &gateway,
        &[
            &first.route_id,
            &second.route_id,
            &auxiliary_target.route_id,
            &forbidden_target.route_id,
            &redirect_target.route_id,
        ],
    )
    .await?;
    let router = create_router(gateway.clone());

    for (target, model, secret, before, answer) in [
        (&first, "lifecycle-first", "first-secret", "0", "first-0"),
        (
            &second,
            "lifecycle-second",
            "second-secret",
            "0",
            "second-0",
        ),
        (&first, "lifecycle-first", "first-secret", "1", "first-1"),
    ] {
        let call = tokio::spawn(invoke(
            router.clone(),
            token.clone(),
            target.route_id.clone(),
        ));
        let request = base.next().await;
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.uri.path(), "/infer");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body)?["model"],
            model,
        );
        let expected_authorization = format!("Bearer {secret}");
        assert_eq!(
            request.header("authorization"),
            Some(expected_authorization.as_str())
        );
        assert_eq!(request.header("x-state-before"), Some(before));
        request.reply(UpstreamReply::model(answer));
        assert_success_with(&call.await?, answer);
    }

    let call = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        auxiliary_target.route_id,
    ));
    let request = auxiliary.next().await;
    assert_eq!(request.header("authorization"), Some("Bearer aux-secret"));
    request.reply(UpstreamReply::model("declared-auxiliary-origin"));
    assert_success_with(&call.await?, "declared-auxiliary-origin");

    let blocked = invoke(router.clone(), token.clone(), forbidden_target.route_id).await;
    assert_ne!(blocked.0, StatusCode::OK);
    forbidden.assert_no_request();

    let redirected = tokio::spawn(invoke(router, token, redirect_target.route_id));
    let initial = base.next().await;
    assert_eq!(initial.method, Method::GET);
    assert_eq!(initial.uri.path(), "/redirect");
    assert_eq!(
        initial.header("authorization"),
        Some("Bearer redirect-secret")
    );
    initial.reply(UpstreamReply::redirect(format!(
        "{}/redirected",
        auxiliary.base_url
    )));
    let landing = auxiliary.next().await;
    assert_eq!(landing.uri.path(), "/redirected");
    assert_eq!(landing.header("authorization"), None);
    assert_eq!(landing.header("x-fixture-version"), None);
    landing.reply(UpstreamReply::model("redirect-without-secret"));
    assert_success_with(&redirected.await?, "redirect-without-secret");
    Ok(())
}

#[tokio::test]
async fn compatible_update_keeps_continuation_recovery_on_the_original_component()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "continuation recovery",
        "lifecycle-continuation-recovery",
        &upstream.base_url,
        "continuation-secret",
        options(&[("mode", "continuation")]),
    )
    .await?;
    gateway
        .admin()
        .update_model(
            "lifecycle-continuation-recovery",
            UpdateRoute {
                targets: Some(vec![UpsertTarget {
                    id: Some(target.target_id.clone()),
                    provider_id: target.provider_id.clone(),
                    model: Some("fixture-model".into()),
                    enabled: true,
                    priority: Some(0),
                    first_token_timeout_ms: None,
                    target_retry_budget: Some(1),
                    target_cooldown_ms: Some(0),
                    thinking_level_map: Vec::new(),
                }]),
                ..Default::default()
            },
        )
        .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let router = create_router(gateway.clone());

    let seeded = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        target.route_id.clone(),
    ));
    let seed_request = upstream.next().await;
    assert_eq!(seed_request.header("x-fixture-version"), Some("1.0.0"));
    seed_request.reply(UpstreamReply::model("continuation seed"));
    let seeded = seeded.await?;
    assert_success_with(&seeded, "continuation seed");
    let response_id = seeded.1["id"]
        .as_str()
        .expect("Responses result has a continuation identity")
        .to_owned();

    let mut continued = tokio::spawn(invoke_with_previous(
        router,
        token,
        target.route_id,
        Some(response_id),
    ));
    let old_request = upstream.next_for(&mut continued).await;
    assert_eq!(old_request.header("x-fixture-version"), Some("1.0.0"));
    assert!(
        String::from_utf8_lossy(&old_request.body).contains("fixture-response"),
        "first attempt must carry the upstream continuation identity"
    );

    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("lifecycle-v2.wasm"))
        .await?;
    assert!(!preview.cancels_active_operations);
    assert!(preview.active_operations >= 1);
    let updated = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    assert_eq!(updated.version, "2.0.0");

    old_request.reply(UpstreamReply {
        status: StatusCode::NOT_FOUND,
        headers: vec![("content-type", "application/json".into())],
        body: br#"{"error":"continuation expired"}"#.to_vec(),
        truncate: false,
    });
    let replay = upstream.next_for(&mut continued).await;
    assert_eq!(
        replay.header("x-fixture-version"),
        Some("1.0.0"),
        "continuation recovery must remain on the originally admitted component"
    );
    assert!(
        !String::from_utf8_lossy(&replay.body).contains("previous_response_id"),
        "recovery must replay full history without the rejected continuation"
    );
    replay.reply(UpstreamReply::model("recovered on pinned component"));
    assert_success_with(&continued.await?, "recovered on pinned component");
    Ok(())
}

#[tokio::test]
async fn compatible_update_pins_old_calls_and_routes_new_calls_to_the_new_component()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "compatible update",
        "lifecycle-compatible",
        &upstream.base_url,
        "compatible-secret",
        options(&[]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let router = create_router(gateway.clone());

    let old_call = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        target.route_id.clone(),
    ));
    let old_request = upstream.next().await;
    assert_eq!(old_request.header("x-fixture-version"), Some("1.0.0"));

    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("lifecycle-v2.wasm"))
        .await?;
    assert!(!preview.cancels_active_operations);
    assert!(preview.active_operations >= 1);
    let updated = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    assert_eq!(updated.version, "2.0.0");

    let new_call = tokio::spawn(invoke(router, token, target.route_id));
    let new_request = upstream.next().await;
    assert_eq!(new_request.header("x-fixture-version"), Some("2.0.0"));
    new_request.reply(UpstreamReply::model("new-generation"));
    assert_success_with(&new_call.await?, "new-generation");

    old_request.reply(UpstreamReply::model("old-generation-finished"));
    assert_success_with(&old_call.await?, "old-generation-finished");
    Ok(())
}

#[tokio::test]
async fn incompatible_update_cancels_only_its_vendor_rejects_late_results_and_resets_only_state()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v2.wasm", false).await?;
    install(&gateway, "lifecycle-other-v1.wasm", false).await?;
    let mut lifecycle_upstream = TestUpstream::start().await;
    let mut other_upstream = TestUpstream::start().await;
    let lifecycle = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "incompatible account",
        "lifecycle-incompatible",
        &lifecycle_upstream.base_url,
        "retained-secret",
        options(&[]),
    )
    .await?;
    let other = connection(
        &gateway,
        OTHER_VENDOR,
        "unrelated account",
        "lifecycle-unrelated",
        &other_upstream.base_url,
        "other-secret",
        options(&[]),
    )
    .await?;
    let token = api_key(&gateway, &[&lifecycle.route_id, &other.route_id]).await?;
    let router = create_router(gateway.clone());

    let seed = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        lifecycle.route_id.clone(),
    ));
    let seed_request = lifecycle_upstream.next().await;
    assert_eq!(seed_request.header("x-state-before"), Some("0"));
    seed_request.reply(UpstreamReply::model("seeded-state"));
    assert_success_with(&seed.await?, "seeded-state");

    let cancelled_call = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        lifecycle.route_id.clone(),
    ));
    let late_request = lifecycle_upstream.next().await;
    assert_eq!(late_request.header("x-fixture-version"), Some("2.0.0"));
    assert_eq!(late_request.header("x-state-before"), Some("1"));

    let unrelated_call = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        other.route_id.clone(),
    ));
    let unrelated_request = other_upstream.next().await;
    assert_eq!(
        unrelated_request.header("x-fixture-vendor"),
        Some(OTHER_VENDOR)
    );

    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("lifecycle-v3.wasm"))
        .await?;
    assert!(preview.cancels_active_operations);
    assert!(preview.active_operations >= 1);
    let discarded = preview
        .discarded_data
        .iter()
        .find(|discard| discard.provider.id == lifecycle.provider_id)
        .expect("private state discard is disclosed for the affected provider");
    assert_eq!(discarded.kinds, vec!["private_state"]);
    assert!(discarded.recovery_actions.is_empty());
    let rejected = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id.clone(),
            allow_data_discard: false,
        })
        .await;
    assert!(rejected.is_err());
    assert!(
        !cancelled_call.is_finished(),
        "declining data discard must not cancel the active generation"
    );
    let updated = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: true,
        })
        .await?;
    assert_eq!(updated.version, "3.0.0");

    let cancelled = cancelled_call.await?;
    assert_ne!(cancelled.0, StatusCode::OK);
    late_request.reply(UpstreamReply::model("must-not-be-published"));

    unrelated_request.reply(UpstreamReply::model("other-vendor-completed"));
    assert_success_with(&unrelated_call.await?, "other-vendor-completed");

    let after_reset = tokio::spawn(invoke(router, token, lifecycle.route_id.clone()));
    let reset_request = lifecycle_upstream.next().await;
    assert_eq!(reset_request.header("x-fixture-version"), Some("3.0.0"));
    assert_eq!(reset_request.header("x-state-before"), Some("0"));
    assert_eq!(
        reset_request.header("authorization"),
        Some("Bearer retained-secret")
    );
    reset_request.reply(UpstreamReply::model("new-generation-after-reset"));
    assert_success_with(&after_reset.await?, "new-generation-after-reset");

    let retained = gateway.admin().get_provider(&lifecycle.provider_id).await?;
    assert_eq!(retained.id, lifecycle.provider_id);
    let other_summary = gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == OTHER_VENDOR)
        .expect("other vendor remains installed");
    assert_eq!(other_summary.version, "1.0.0");
    Ok(())
}

#[tokio::test]
async fn local_builtin_replacement_survives_restart_until_explicit_restore() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    };
    let gateway = Gateway::new(config.clone()).await?;
    let preview = gateway
        .admin()
        .preview_vendor_plugin(fixture("lifecycle-base-local.wasm"))
        .await?;
    assert_eq!(preview.vendor_id, "base");
    assert_eq!(preview.author.as_deref(), Some("Stravia Builtin Team"));
    assert_eq!(preview.target_source, PluginSource::Local);
    let local = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    assert_eq!(local.source, PluginSource::Local);
    assert_eq!(local.version, "999.0.0");
    drop(gateway);

    let gateway = Gateway::new(config).await?;
    let still_local = gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == "base")
        .expect("base plugin");
    assert_eq!(still_local.source, PluginSource::Local);
    assert_eq!(still_local.version, "999.0.0");

    let restore = gateway
        .admin()
        .preview_builtin_vendor_plugin("base")
        .await?;
    assert_eq!(restore.target_source, PluginSource::Builtin);
    assert!(restore.is_downgrade);
    let restored = gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: restore.id,
            allow_data_discard: false,
        })
        .await?;
    assert_eq!(restored.source, PluginSource::Builtin);
    assert_ne!(restored.version, "999.0.0");
    Ok(())
}

#[tokio::test]
async fn installed_generation_and_private_state_survive_gateway_restart() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    };
    let gateway = Gateway::new(config.clone()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "persistent account",
        "lifecycle-persistent",
        &upstream.base_url,
        "persistent-secret",
        options(&[]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let router = create_router(gateway.clone());
    let first = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        target.route_id.clone(),
    ));
    let request = upstream.next().await;
    assert_eq!(request.header("x-state-before"), Some("0"));
    request.reply(UpstreamReply::model("before-restart"));
    assert_success_with(&first.await?, "before-restart");
    drop(router);
    drop(gateway);

    let gateway = Gateway::new(config).await?;
    let persisted = gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == LIFECYCLE_VENDOR)
        .expect("persisted lifecycle plugin");
    assert_eq!(persisted.version, "1.0.0");
    assert_eq!(persisted.source, PluginSource::Local);
    let second = tokio::spawn(invoke(create_router(gateway), token, target.route_id));
    let request = upstream.next().await;
    assert_eq!(request.header("x-state-before"), Some("1"));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer persistent-secret")
    );
    request.reply(UpstreamReply::model("after-restart"));
    assert_success_with(&second.await?, "after-restart");
    Ok(())
}

#[tokio::test]
async fn one_operation_can_overlap_http_calls_without_implicitly_forwarding_credentials()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut primary = TestUpstream::start().await;
    let mut auxiliary = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "Parallel auxiliary connection",
        "parallel-auxiliary-route",
        &primary.base_url,
        "primary-only-secret",
        options(&[("mode", "parallel"), ("auxUrl", &auxiliary.base_url)]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let mut call = tokio::spawn(invoke(create_router(gateway), token, target.route_id));
    let primary_request = primary.next_for(&mut call).await;
    let auxiliary_request = auxiliary.next_for(&mut call).await;
    assert_eq!(
        primary_request.header("authorization"),
        Some("Bearer primary-only-secret")
    );
    assert_eq!(auxiliary_request.header("authorization"), None);
    // 两个上游都已收到请求后才释放任一响应；串行的 HTTP start 实现无法越过此屏障。
    auxiliary_request.reply(UpstreamReply::model("auxiliary-complete"));
    primary_request.reply(UpstreamReply::model("parallel-complete"));
    assert_success_with(&call.await?, "parallel-complete");
    Ok(())
}

#[tokio::test]
async fn protocol_selection_cannot_perform_network_state_or_event_side_effects()
-> anyhow::Result<()> {
    for mode in ["selection-http", "selection-state", "selection-event"] {
        assert_wasm_fault_is_scoped(mode).await?;
    }
    Ok(())
}

#[tokio::test]
async fn an_http_origin_does_not_authorize_websocket_on_the_same_host_and_port()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let base_url = format!("http://{address}");
    let websocket_url = format!("ws://{address}/ws");
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "HTTP-only connection",
        "http-does-not-grant-websocket",
        &base_url,
        "isolated-http-secret",
        options(&[("mode", "websocket"), ("targetUrl", &websocket_url)]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id]).await?;
    let call = invoke(create_router(gateway), token, target.route_id);
    let response = tokio::select! {
        connection = listener.accept() => panic!("WebSocket reached an HTTP-only origin: {connection:?}"),
        response = call => response,
    };
    assert_eq!(response.0, StatusCode::BAD_GATEWAY, "{}", response.1);
    assert_eq!(response.1.get("usage"), None);
    Ok(())
}

#[tokio::test]
async fn exhausted_wasm_fuel_ends_only_that_call_and_the_plugin_recovers() -> anyhow::Result<()> {
    assert_wasm_fault_is_scoped("fuel").await
}

#[tokio::test]
async fn exceeding_the_wasm_memory_budget_ends_only_that_call_and_the_plugin_recovers()
-> anyhow::Result<()> {
    assert_wasm_fault_is_scoped("memory").await
}

#[tokio::test]
async fn an_oversized_vendor_event_ends_only_that_call_and_the_plugin_recovers()
-> anyhow::Result<()> {
    assert_wasm_fault_is_scoped("oversized-event").await
}

#[tokio::test]
async fn cumulative_vendor_output_ends_only_that_call_and_the_plugin_recovers() -> anyhow::Result<()>
{
    assert_wasm_fault_is_scoped("oversized-output").await
}

#[tokio::test]
async fn a_truncated_local_upstream_response_fails_without_usage_and_recovers() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let target = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "truncated upstream account",
        "lifecycle-truncated-upstream",
        &upstream.base_url,
        "truncated-secret",
        options(&[]),
    )
    .await?;
    let healthy = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "healthy truncation peer",
        "lifecycle-truncation-peer",
        &upstream.base_url,
        "peer-secret",
        options(&[]),
    )
    .await?;
    let token = api_key(&gateway, &[&target.route_id, &healthy.route_id]).await?;
    let router = create_router(gateway.clone());

    let mut truncated_call = tokio::spawn(invoke(
        router.clone(),
        token.clone(),
        target.route_id.clone(),
    ));
    let failed = loop {
        tokio::select! {
            response = &mut truncated_call => break response?,
            request = upstream.next() => {
                assert_eq!(request.header("x-state-before"), Some("0"));
                assert_eq!(request.header("authorization"), Some("Bearer truncated-secret"));
                request.reply(UpstreamReply::truncated());
            }
        }
    };
    assert_eq!(
        failed.0,
        StatusCode::BAD_GATEWAY,
        "unexpected truncated response result: {}",
        failed.1
    );
    assert_eq!(
        failed.1.get("usage"),
        None,
        "truncated response must not report known usage: {}",
        failed.1
    );
    upstream.assert_no_request();

    let mut peer_call = tokio::spawn(invoke(router.clone(), token.clone(), healthy.route_id));
    let peer_request = upstream.next_for(&mut peer_call).await;
    assert_eq!(peer_request.header("x-state-before"), Some("0"));
    assert_eq!(
        peer_request.header("authorization"),
        Some("Bearer peer-secret")
    );
    peer_request.reply(UpstreamReply::model("peer-after-truncation"));
    assert_success_with(&peer_call.await?, "peer-after-truncation");

    let mut recovered_call = tokio::spawn(invoke(router, token, target.route_id));
    let recovered_request = upstream.next_for(&mut recovered_call).await;
    assert_eq!(recovered_request.header("x-state-before"), Some("0"));
    assert_eq!(
        recovered_request.header("authorization"),
        Some("Bearer truncated-secret")
    );
    recovered_request.reply(UpstreamReply::model("same-connection-after-truncation"));
    assert_success_with(&recovered_call.await?, "same-connection-after-truncation");

    let summary = gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == LIFECYCLE_VENDOR)
        .expect("lifecycle plugin remains installed");
    assert_eq!(summary.version, "1.0.0");
    assert_eq!(summary.status, "ready");
    Ok(())
}

#[tokio::test]
async fn a_guest_trap_fails_the_request_without_poisoning_the_installed_plugin()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "lifecycle-v1.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let trapped = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "trapping account",
        "lifecycle-trap",
        &upstream.base_url,
        "trap-secret",
        options(&[("mode", "trap")]),
    )
    .await?;
    let healthy = connection(
        &gateway,
        LIFECYCLE_VENDOR,
        "healthy account",
        "lifecycle-after-trap",
        &upstream.base_url,
        "healthy-secret",
        options(&[]),
    )
    .await?;
    let token = api_key(&gateway, &[&trapped.route_id, &healthy.route_id]).await?;
    let router = create_router(gateway.clone());

    let failed = invoke(router.clone(), token.clone(), trapped.route_id).await;
    assert_ne!(failed.0, StatusCode::OK);
    upstream.assert_no_request();

    let healthy_call = tokio::spawn(invoke(router, token, healthy.route_id));
    let request = upstream.next().await;
    assert_eq!(
        request.header("authorization"),
        Some("Bearer healthy-secret")
    );
    request.reply(UpstreamReply::model("healthy-after-trap"));
    assert_success_with(&healthy_call.await?, "healthy-after-trap");
    let summary = gateway
        .admin()
        .list_vendor_plugins()
        .await?
        .into_iter()
        .find(|plugin| plugin.vendor_id == LIFECYCLE_VENDOR)
        .expect("lifecycle plugin remains installed");
    assert_eq!(summary.status, "ready");
    Ok(())
}
