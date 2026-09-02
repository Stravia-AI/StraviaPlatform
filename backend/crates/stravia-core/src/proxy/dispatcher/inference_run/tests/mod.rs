use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{
    State, WebSocketUpgrade,
    ws::{Message as AxumWebSocketMessage, WebSocket},
};
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::routing::get;
use futures::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::db::models::{
    CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput, ProviderSourceInput,
};
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, BEDROCK_CONVERSE_V1, COHERE_CHAT_V2, GATEWAY_LANGUAGE_MODEL_V4,
    GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPEN_RESPONSES_2026_04_24,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1, WATSONX_TEXT_CHAT_V1,
};
use crate::protocol::ir::AiResponse;

struct NormalizingTestVendor;

struct FailingParentDiscoveryStore {
    inner: crate::turn_chain::SqlTurnChainStore,
    discovery_attempts: Arc<AtomicUsize>,
}

async fn failing_parent_discovery_store() -> FailingParentDiscoveryStore {
    FailingParentDiscoveryStore {
        inner: crate::turn_chain::test_store().await,
        discovery_attempts: Arc::new(AtomicUsize::new(0)),
    }
}

#[async_trait]
impl crate::turn_chain::TurnChainStore for FailingParentDiscoveryStore {
    async fn materialize(
        &self,
        principal: &crate::hook::Principal,
        kind: crate::turn_chain::TurnNodeKind,
        id: &crate::turn_chain::TurnNodeId,
    ) -> Result<Vec<crate::turn_chain::TurnNode>, crate::turn_chain::TurnUnavailable> {
        self.inner.materialize(principal, kind, id).await
    }

    async fn commit(
        &self,
        commit: crate::turn_chain::TurnCommit,
    ) -> Result<crate::turn_chain::TurnNodeId, crate::turn_chain::TurnCommitError> {
        self.inner.commit(commit).await
    }

    async fn find_reusable_prefixes(
        &self,
        _principal: &crate::hook::Principal,
        _kind: crate::turn_chain::TurnNodeKind,
        _query: &crate::turn_chain::ReusablePrefixQuery,
    ) -> Result<Vec<crate::turn_chain::ReusablePrefixCandidate>, crate::turn_chain::TurnUnavailable>
    {
        self.discovery_attempts.fetch_add(1, Ordering::SeqCst);
        Err(crate::turn_chain::TurnUnavailable::Storage(
            "injected parent-discovery failure".into(),
        ))
    }

    async fn sweep_expired(&self) -> Result<u64, crate::turn_chain::TurnUnavailable> {
        self.inner.sweep_expired().await
    }
}

#[async_trait]
impl crate::provider::vendor::Vendor for NormalizingTestVendor {
    fn scope(&self) -> crate::provider::registry::VendorScope {
        crate::provider::registry::VendorScope::Vendor {
            vendor_id: "normalizing-test",
        }
    }
    fn target_capabilities(
        &self,
        protocol: crate::protocol::ids::ProtocolId,
    ) -> crate::provider::vendor_ext::ResolvedTargetCapabilities {
        crate::provider::vendor_ext::ResolvedTargetCapabilities {
            stream_only: protocol == OPEN_RESPONSES_2026_04_24,
            ..Default::default()
        }
    }

    async fn post_parse(
        &self,
        _context: &crate::provider::vendor_ext::VendorCtx<'_>,
        response: &mut AiResponse,
    ) -> anyhow::Result<()> {
        response.replace_output_text(format!("normalized:{}", response.output_text()));
        Ok(())
    }

    async fn on_stream_delta(
        &self,
        _context: &crate::provider::vendor_ext::VendorCtx<'_>,
        delta: &mut crate::protocol::ir::AiStreamDelta,
    ) -> anyhow::Result<()> {
        if let crate::protocol::ir::AiStreamDelta::TextDelta(content) = delta {
            content.insert_str(0, "normalized:");
        }
        Ok(())
    }

    fn vendor_id(&self) -> &'static str {
        "normalizing-test"
    }

    fn supported_protocols(&self) -> &'static [crate::protocol::ids::ProtocolId] {
        &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
    }

    async fn build_request(
        &self,
        request: &mut AiRequest,
        context: &crate::provider::vendor::ProviderCtx<'_>,
    ) -> Result<crate::provider::outbound::OutboundRequest, crate::error::GatewayError> {
        crate::provider::common::pipeline::build_request(self, request, context).await
    }

    async fn parse_response(
        &self,
        response: crate::provider::inbound::InboundResponse,
        context: &crate::provider::vendor::ProviderCtx<'_>,
    ) -> Result<AiResponse, crate::error::GatewayError> {
        crate::provider::common::pipeline::parse_response(self, response, context).await
    }

    fn map_error(&self, status: u16, _body: serde_json::Value) -> crate::error::GatewayError {
        crate::error::GatewayError::upstream_status("normalizing-test", status, None)
    }
}

inventory::submit! {
    crate::provider::registry::VendorRegistration {
        make: || Box::new(NormalizingTestVendor),
    }
}

struct RuntimeShortCircuitHook;
struct RuntimeShortCircuitSession;

impl crate::hook::Hook for RuntimeShortCircuitHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("lifecycle-short-circuit")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(RuntimeShortCircuitSession)
    }
}

#[async_trait]
impl crate::hook::HookSession for RuntimeShortCircuitSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        let crate::hook::HookEvent::Request { current, .. } = event else {
            return Ok(crate::hook::ActionBatch::default());
        };
        if current.model == "__lifecycle_reject__" {
            return Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::Reject(crate::hook::HookRejection {
                    status: 451,
                    code: "request_rejected".into(),
                    message: "request rejected by lifecycle Hook".into(),
                }),
            ));
        }
        if current.model == "__lifecycle_short_circuit__" {
            let mut response = AiResponse::new("response-hook", &current.model);
            response.push_output_text("handled by lifecycle Hook");
            return Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::Respond(Box::new(response)),
            ));
        }
        Ok(crate::hook::ActionBatch::default())
    }
}

struct BlockingRequestHook {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct BlockingRequestSession {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl crate::hook::Hook for BlockingRequestHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("block-request-for-admission")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(BlockingRequestSession {
            entered: Arc::clone(&self.entered),
            release: Arc::clone(&self.release),
        })
    }
}

#[async_trait]
impl crate::hook::HookSession for BlockingRequestSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        let crate::hook::HookEvent::Request { current, .. } = event else {
            return Ok(crate::hook::ActionBatch::default());
        };
        self.entered.notify_one();
        self.release.notified().await;
        let mut response = AiResponse::new("admission-response", &current.model);
        response.push_output_text("request completed after admission");
        Ok(crate::hook::ActionBatch::one(
            crate::hook::HookAction::Respond(Box::new(response)),
        ))
    }
}

struct RewriteModelHook {
    model: String,
}

struct RewriteModelSession {
    model: String,
}

impl crate::hook::Hook for RewriteModelHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("rewrite-model-for-authorization")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(RewriteModelSession {
            model: self.model.clone(),
        })
    }
}

#[async_trait]
impl crate::hook::HookSession for RewriteModelSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if !matches!(event, crate::hook::HookEvent::Request { .. }) {
            return Ok(crate::hook::ActionBatch::default());
        }
        Ok(crate::hook::ActionBatch::one(
            crate::hook::HookAction::PatchRequest(Box::new(crate::hook::RequestPatch::SetModel(
                self.model.clone(),
            ))),
        ))
    }
}

struct PrependContextHook {
    observed: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

struct PrependContextSession {
    observed: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

impl crate::hook::Hook for PrependContextHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("prepend-context")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(PrependContextSession {
            observed: self.observed.clone(),
        })
    }
}

#[async_trait]
impl crate::hook::HookSession for PrependContextSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        let crate::hook::HookEvent::Request { current, .. } = event else {
            return Ok(crate::hook::ActionBatch::default());
        };
        self.observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(
                current
                    .items
                    .iter()
                    .map(|item| item.content.to_text())
                    .collect(),
            );
        let mut rewritten = current.clone();
        let mut marker = crate::protocol::ir::AiItem::output_text("hook context");
        marker.role = crate::protocol::ir::Role::User;
        rewritten.items.insert(0, marker);
        Ok(crate::hook::ActionBatch::one(
            crate::hook::HookAction::PatchRequest(Box::new(
                crate::hook::RequestPatch::ReplaceCanonical(Box::new(rewritten)),
            )),
        ))
    }
}

async fn gateway_rewriting_model(test_name: &str, final_model: &str) -> Gateway {
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir()
            .join(format!("stravia-{test_name}-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    };
    crate::Gateway::builder(config)
        .hook(Arc::new(RewriteModelHook {
            model: final_model.into(),
        }))
        .build()
        .await
        .expect("gateway init")
        .0
}

fn bearer_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("auth header"),
    );
    headers
}
async fn authorized_headers(gateway: &Gateway) -> HeaderMap {
    const KEY_NAME: &str = "inference-run-test-key";
    let admin = gateway.admin();
    let model_ids = admin
        .list_models()
        .await
        .expect("list test models")
        .into_iter()
        .map(|model| model.id)
        .collect::<Vec<_>>();
    let key = match admin
        .list_api_keys()
        .await
        .expect("list test API keys")
        .into_iter()
        .find(|key| key.name == KEY_NAME)
    {
        Some(key) => admin
            .update_api_key(
                &key.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: None,
                    is_enabled: None,
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    inject_media_understanding: None,
                    expires_at: None,
                    model_ids: Some(model_ids),
                },
            )
            .await
            .expect("bind test API key"),
        None => admin
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: KEY_NAME.into(),
                concurrency_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                inject_web_search: false,
                model_ids,
                inject_media_understanding: false,
            })
            .await
            .expect("create test API key"),
    };
    bearer_headers(&key.token)
}

async fn set_concurrency_limit(gateway: &Gateway, limit: i32) {
    let key = gateway
        .admin()
        .list_api_keys()
        .await
        .expect("list API keys")
        .into_iter()
        .find(|key| key.name == "inference-run-test-key")
        .expect("test API key");
    gateway
        .admin()
        .update_api_key(
            &key.id,
            crate::db::models::UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: Some(Some(limit)),
                is_enabled: None,
                mcp_access_enabled: None,
                transparent_injection_enabled: None,
                inject_web_search: None,
                inject_media_understanding: None,
                expires_at: None,
                model_ids: None,
            },
        )
        .await
        .expect("set Principal Concurrency Limit");
}

#[derive(Clone, Default)]
struct AccessMutationFixture {
    gateway: Arc<std::sync::Mutex<Option<Gateway>>>,
    key_id: Arc<std::sync::Mutex<Option<String>>>,
    replacement_model_id: Arc<std::sync::Mutex<Option<String>>>,
}

impl AccessMutationFixture {
    fn set(&self, gateway: Gateway, key_id: String, replacement_model_id: String) {
        *self
            .gateway
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gateway);
        *self
            .key_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(key_id);
        *self
            .replacement_model_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(replacement_model_id);
    }

    fn gateway(&self) -> Option<Gateway> {
        self.gateway
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn key_id(&self) -> Option<String> {
        self.key_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn replacement_model_id(&self) -> Option<String> {
        self.replacement_model_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

async fn serve_openai_response(status: u16, body: serde_json::Value) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept provider request");
        let mut request = vec![0_u8; 16 * 1024];
        let _ = socket
            .read(&mut request)
            .await
            .expect("read provider request");
        observed.fetch_add(1, Ordering::SeqCst);
        let body = body.to_string();
        let reason = if status >= 500 {
            "Internal Server Error"
        } else {
            "OK"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write provider response");
    });
    (format!("http://{address}/v1"), calls)
}

async fn serve_openai_sequence(bodies: Vec<serde_json::Value>) -> (String, Arc<AtomicUsize>) {
    let (base_url, calls, _) = serve_openai_sequence_with_requests(bodies).await;
    (base_url, calls)
}

fn openai_chat_response_as_sse(body: serde_json::Value) -> String {
    let event = |choices: serde_json::Value| {
        serde_json::json!({
            "id": body["id"].clone(),
            "object": "chat.completion.chunk",
            "created": body["created"].clone(),
            "model": body["model"].clone(),
            "choices": choices,
        })
    };
    let mut events = Vec::new();
    for choice in body["choices"].as_array().into_iter().flatten() {
        let index = choice["index"].clone();
        let mut message = choice["message"].clone();
        let tool_calls = message
            .as_object_mut()
            .and_then(|message| message.remove("tool_calls"))
            .and_then(|calls| calls.as_array().cloned())
            .unwrap_or_default();
        if let Some(message) = message.as_object_mut() {
            message.retain(|_, value| !value.is_null());
        }
        if message
            .as_object()
            .is_some_and(|message| !message.is_empty())
        {
            events.push(event(serde_json::json!([{
                "index": index.clone(),
                "delta": message,
                "finish_reason": null,
            }])));
        }
        for (tool_index, mut tool_call) in tool_calls.into_iter().enumerate() {
            tool_call["index"] = serde_json::json!(tool_index);
            events.push(event(serde_json::json!([{
                "index": index.clone(),
                "delta": {"tool_calls": [tool_call]},
                "finish_reason": null,
            }])));
        }
        events.push(event(serde_json::json!([{
            "index": index,
            "delta": {},
            "finish_reason": choice["finish_reason"].clone(),
        }])));
    }
    let mut usage_event = event(serde_json::json!([]));
    usage_event["usage"] = body["usage"].clone();
    events.push(usage_event);
    let mut sse = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    sse.push_str("data: [DONE]\n\n");
    sse
}

async fn create_test_provider_with_model(
    gateway: &Gateway,
    name: &str,
    base_url: String,
    upstream_model: &str,
    metadata: serde_json::Value,
) -> crate::db::models::Provider {
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some(name.into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "test-key".into(),
            },
            use_proxy: false,
        })
        .await
        .expect("test Provider");
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            upstream_model,
            crate::provider_models::CreateManualProviderModel { metadata },
        )
        .await
        .expect("test Provider Model");
    provider
}

async fn serve_openai_sequence_with_requests(
    bodies: Vec<serde_json::Value>,
) -> (String, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider sequence");
    let address = listener.local_addr().expect("provider sequence address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_requests = requests.clone();
    tokio::spawn(async move {
        for body in bodies {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept provider sequence request");
            let mut request = vec![0_u8; 16 * 1024];
            let read = socket
                .read(&mut request)
                .await
                .expect("read provider sequence request");
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            observed_requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request.clone());
            observed.fetch_add(1, Ordering::SeqCst);
            let is_stream = request
                .split_once("\r\n\r\n")
                .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
                .and_then(|body| body["stream"].as_bool())
                .unwrap_or(false);
            let (content_type, body) = if is_stream {
                (
                    "text/event-stream",
                    openai_chat_response_as_sse(body.clone()),
                )
            } else {
                ("application/json", body.to_string())
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write provider sequence response");
        }
    });
    (format!("http://{address}/v1"), calls, requests)
}

async fn read_test_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let read = socket.read(&mut chunk).await.expect("read test request");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8(bytes).expect("UTF-8 test request")
}

async fn write_test_json_response(socket: &mut tokio::net::TcpStream, body: serde_json::Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("write test response");
}

async fn write_test_openai_response(
    socket: &mut tokio::net::TcpStream,
    request: &str,
    body: serde_json::Value,
) {
    let is_stream = request
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
        .and_then(|body| body["stream"].as_bool())
        .unwrap_or(false);
    if !is_stream {
        write_test_json_response(socket, body).await;
        return;
    }
    let body = openai_chat_response_as_sse(body);
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("write test OpenAI stream response");
}

fn marker_artifact_id(request: &str) -> Option<String> {
    let body = request.split_once("\r\n\r\n")?.1;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    fn visit(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(text) => {
                let marker = text.split_once("stravia_media artifact_id=\"")?.1;
                Some(marker.split_once('"')?.0.to_owned())
            }
            serde_json::Value::Array(values) => values.iter().find_map(visit),
            serde_json::Value::Object(values) => values.values().find_map(visit),
            _ => None,
        }
    }
    visit(&value)
}

fn media_turn_id(request: &str) -> Option<String> {
    let suffix = request
        .split_once("[stravia_media_turn turn_id=\\\"aturn_")
        .map(|(_, suffix)| suffix)
        .or_else(|| {
            request
                .split_once("\\\"turn_id\\\":\\\"aturn_")
                .map(|(_, suffix)| suffix)
        })?;
    Some(format!("aturn_{}", suffix.split_once("\\\"")?.0))
}

async fn serve_media_parent(
    source_id: Arc<std::sync::Mutex<Option<String>>>,
) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind parent provider");
    let address = listener.local_addr().expect("parent provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    tokio::spawn(async move {
        for ordinal in 0..4 {
            let (mut socket, _) = listener.accept().await.expect("accept parent request");
            let request = read_test_http_request(&mut socket).await;
            observed.fetch_add(1, Ordering::SeqCst);
            let body = match ordinal {
                0 => {
                    let id = marker_artifact_id(&request).expect("bridge Artifact marker");
                    *source_id
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id.clone());
                    serde_json::json!({
                        "id": "chatcmpl-media-tool",
                        "object": "chat.completion",
                        "created": 1,
                        "model": "parent",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "media-call",
                                    "type": "function",
                                    "function": {
                                        "name": "stravia__understand_media",
                                        "arguments": serde_json::json!({
                                            "prompt": "Describe the image",
                                            "artifacts": [{"artifact_id": id}]
                                        }).to_string()
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                }
                1 => {
                    assert!(
                        request.contains("The image is understood"),
                        "parent must receive the Media Report: {request}"
                    );
                    openai_response("parent used Media Report")
                }
                2 => {
                    let turn_id = media_turn_id(&request)
                        .unwrap_or_else(|| panic!("inherited Media Turn marker: {request}"));
                    assert!(
                        request.contains(r#""name":"stravia__understand_media""#),
                        "continued request must expose understand_media: {request}"
                    );
                    serde_json::json!({
                        "id": "chatcmpl-media-continuation",
                        "object": "chat.completion",
                        "created": 1,
                        "model": "parent",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "media-continuation-call",
                                    "type": "function",
                                    "function": {
                                        "name": "stravia__understand_media",
                                        "arguments": serde_json::json!({
                                            "prompt": "Identify the subject",
                                            "artifacts": [],
                                            "previous_turn_id": turn_id
                                        }).to_string()
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                }
                3 => {
                    assert!(
                        request.contains("The same image is understood"),
                        "parent must receive the continued Media Report: {request}"
                    );
                    openai_response("parent used continued Media Report")
                }
                _ => unreachable!("fixed request sequence"),
            };
            write_test_openai_response(&mut socket, &request, body).await;
        }
    });
    (format!("http://{address}/v1"), calls)
}

async fn serve_media_model(
    source_id: Arc<std::sync::Mutex<Option<String>>>,
) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Media provider");
    let address = listener.local_addr().expect("Media provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    tokio::spawn(async move {
        for ordinal in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept Media request");
            let request = read_test_http_request(&mut socket).await;
            observed.fetch_add(1, Ordering::SeqCst);
            let id = source_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .expect("source Artifact from parent request");
            assert!(
                request.contains("data:image/jpeg;base64"),
                "every Media Turn must hydrate the inherited image: {request}"
            );
            if ordinal == 1 {
                assert!(
                    request.contains("Identify the subject"),
                    "continued Media request must append the new task: {request}"
                );
            }
            let answer = if ordinal == 0 {
                format!("The image is understood [artifact:{id}]")
            } else {
                format!("The same image is understood [artifact:{id}]")
            };
            let report = serde_json::json!({
                "answer": answer,
                "artifacts": [{"artifact_id": id}],
                "limitations": []
            })
            .to_string();
            write_test_json_response(&mut socket, openai_response(&report)).await;
        }
    });
    (format!("http://{address}/v1"), calls)
}

async fn configure_route(gateway: &Gateway, model: &str, base_urls: &[String]) {
    // Generic fixtures speak Chat Completions over HTTP. Keep them off the
    // OpenAI-direct Responses WebSocket path; dedicated transport tests opt in.
    configure_route_with_vendor(gateway, model, base_urls, "test-http").await;
}

async fn configure_route_with_vendor(
    gateway: &Gateway,
    model: &str,
    base_urls: &[String],
    vendor: &str,
) {
    configure_route_with_protocol(gateway, model, base_urls, vendor, "openai-compatible").await;
}

async fn configure_route_with_protocol(
    gateway: &Gateway,
    model: &str,
    base_urls: &[String],
    vendor: &str,
    protocol: &str,
) -> String {
    let mut targets = Vec::with_capacity(base_urls.len());
    let reasoning_options = match protocol {
        "open-responses" => Some(serde_json::json!([{
            "type": "effort",
            "values": ["none", "low", "medium", "high", "xhigh"]
        }])),
        "anthropic-messages" => Some(serde_json::json!([{"type": "toggle"}])),
        _ => None,
    };
    for (priority, base_url) in base_urls.iter().enumerate() {
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some(format!("{model}-provider-{priority}")),
                source: ProviderSourceInput::Custom {
                    vendor: Some(vendor.into()),
                    protocol: protocol.into(),
                    base_url: base_url.clone(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "test-key".into(),
                },
                use_proxy: false,
            })
            .await
            .expect("create provider");
        gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                "provider-model",
                crate::provider_models::CreateManualProviderModel {
                    metadata: serde_json::json!({
                        "id": "provider-model",
                        "tool_call": true,
                        "reasoning_options": reasoning_options.clone()
                    }),
                },
            )
            .await
            .expect("create provider model");
        targets.push(CreateTarget {
            provider_id: provider.id,
            model: "provider-model".into(),
            weight: Some(100),
            priority: Some(priority as i32 + 1),
            thinking_level_map: Vec::new(),
        });
    }
    gateway
        .admin()
        .create_model(CreateRoute {
            model_id: model.into(),
            display_name: None,
            balance: Some("priority".into()),
            target_provider: String::new(),
            target_model: String::new(),
            targets,
        })
        .await
        .expect("create route")
        .id
}

async fn configure_route_with_id(gateway: &Gateway, model: &str, base_urls: &[String]) -> String {
    configure_route_with_protocol(gateway, model, base_urls, "custom", "openai-compatible").await
}

fn openai_response(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-provider",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    })
}

fn open_responses_response(content: &str) -> serde_json::Value {
    let mut response = AiResponse::new("resp_provider", "provider-model");
    response.push_output_text(content);
    response.stop_reason = Some("stop".into());
    crate::protocol::codec::open_responses::formatter::ResponsesResponseFormatter
        .format_response(&response)
}

async fn execute_request_with_headers(
    gateway: Gateway,
    headers: HeaderMap,
    request: AiRequest,
    ingress: ProtocolId,
    path: &str,
) -> Response {
    let envelope_body = serde_json::json!({
        "model": request.model.clone(),
        "stream": request.stream.enabled,
    });
    execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(Some(envelope_body), HashMap::new(), "POST", path),
        ingress,
        context: RequestContext::new(ingress, std::time::Duration::from_secs(30)),
        request,
    })
    .await
}

async fn execute_non_stream_request_with_headers(
    gateway: Gateway,
    headers: HeaderMap,
    request: AiRequest,
) -> Response {
    execute_request_with_headers(
        gateway,
        headers,
        request,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        "/v1/chat/completions",
    )
    .await
}

async fn execute_non_stream_request(gateway: Gateway, request: AiRequest) -> Response {
    let headers = authorized_headers(&gateway).await;
    execute_non_stream_request_with_headers(gateway, headers, request).await
}

async fn execute_non_stream(gateway: Gateway, model: &str) -> Response {
    execute_non_stream_request(gateway, AiRequest::new(model, Vec::new())).await
}

struct ExposeOrderedToolHook {
    request_rounds: Arc<std::sync::Mutex<Vec<u32>>>,
}

struct ExposeOrderedToolSession {
    request_rounds: Arc<std::sync::Mutex<Vec<u32>>>,
}

struct HiddenRoundRespondHook;

struct HiddenRoundRespondSession;

struct HiddenRoundRejectHook;

struct HiddenRoundRejectSession;

impl ExposeOrderedToolHook {
    fn counting() -> (Self, Arc<std::sync::Mutex<Vec<u32>>>) {
        let request_rounds = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                request_rounds: request_rounds.clone(),
            },
            request_rounds,
        )
    }
}

impl crate::hook::Hook for ExposeOrderedToolHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("expose-ordered-tool")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(ExposeOrderedToolSession {
            request_rounds: self.request_rounds.clone(),
        })
    }
}

impl crate::hook::Hook for HiddenRoundRespondHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("hidden-round-respond")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(HiddenRoundRespondSession)
    }
}

impl crate::hook::Hook for HiddenRoundRejectHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("hidden-round-reject")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(HiddenRoundRejectSession)
    }
}

#[async_trait]
impl crate::hook::HookSession for HiddenRoundRespondSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        let crate::hook::HookEvent::Request { round, .. } = event else {
            return Ok(crate::hook::ActionBatch::default());
        };
        if round == 0 {
            return Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::ExposeTool(crate::hook::ToolId::new("ordered-tool")),
            ));
        }
        let mut response = AiResponse::new("hook-followup", "hook-model");
        response.push_output_text("hook completed hidden round");
        Ok(crate::hook::ActionBatch::one(
            crate::hook::HookAction::Respond(Box::new(response)),
        ))
    }
}

#[async_trait]
impl crate::hook::HookSession for HiddenRoundRejectSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        let crate::hook::HookEvent::Request { round, .. } = event else {
            return Ok(crate::hook::ActionBatch::default());
        };
        let action = if round == 0 {
            crate::hook::HookAction::ExposeTool(crate::hook::ToolId::new("ordered-tool"))
        } else {
            crate::hook::HookAction::Reject(crate::hook::HookRejection {
                status: 403,
                code: "hidden_round_denied".into(),
                message: "hidden round rejected".into(),
            })
        };
        Ok(crate::hook::ActionBatch::one(action))
    }
}

#[async_trait]
impl crate::hook::HookSession for ExposeOrderedToolSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if let crate::hook::HookEvent::Request { round, .. } = event {
            self.request_rounds
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(round);
            Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::ExposeTool(crate::hook::ToolId::new("ordered-tool")),
            ))
        } else {
            Ok(crate::hook::ActionBatch::default())
        }
    }
}

struct OrderedTool {
    calls: Arc<std::sync::Mutex<Vec<u64>>>,
}

struct RetryingOrderedTool {
    calls: Arc<std::sync::Mutex<Vec<u64>>>,
}

#[async_trait]
impl crate::hook::PlatformTool for OrderedTool {
    fn id(&self) -> crate::hook::ToolId {
        crate::hook::ToolId::new("ordered-tool")
    }

    fn external_name(&self) -> &str {
        "ordered_tool"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "index": { "type": "integer" } },
            "required": ["index"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _context: crate::hook::ToolExecutionContext,
    ) -> Result<serde_json::Value, crate::hook::PlatformToolError> {
        let index = arguments["index"]
            .as_u64()
            .ok_or_else(|| crate::hook::PlatformToolError::new("missing index"))?;
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(index);
        Ok(serde_json::json!({ "index": index }))
    }
}

#[async_trait]
impl crate::hook::PlatformTool for RetryingOrderedTool {
    fn id(&self) -> crate::hook::ToolId {
        crate::hook::ToolId::new("ordered-tool")
    }

    fn external_name(&self) -> &str {
        "ordered_tool"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "index": { "type": "integer" } },
            "required": ["index"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _context: crate::hook::ToolExecutionContext,
    ) -> Result<serde_json::Value, crate::hook::PlatformToolError> {
        let index = arguments["index"]
            .as_u64()
            .ok_or_else(|| crate::hook::PlatformToolError::new("missing index"))?;
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(index);
        if index == 1 {
            return Err(crate::hook::PlatformToolError::new("first attempt failed"));
        }
        Ok(serde_json::json!({ "index": index }))
    }
}

#[derive(Clone, Copy)]
enum TestAccessMutation {
    DisableKey,
    RevokeBinding,
}

impl TestAccessMutation {
    fn tool_id(self) -> &'static str {
        match self {
            Self::DisableKey => "disable-key",
            Self::RevokeBinding => "revoke-binding",
        }
    }

    fn external_name(self) -> &'static str {
        match self {
            Self::DisableKey => "disable_key",
            Self::RevokeBinding => "revoke_binding",
        }
    }
}

struct ExposeAccessMutationHook(TestAccessMutation);
struct ExposeAccessMutationSession(TestAccessMutation);

impl crate::hook::Hook for ExposeAccessMutationHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::Request],
            ..crate::hook::HookDescriptor::all("expose-access-mutation-tool")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(ExposeAccessMutationSession(self.0))
    }
}

#[async_trait]
impl crate::hook::HookSession for ExposeAccessMutationSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if matches!(event, crate::hook::HookEvent::Request { .. }) {
            Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::ExposeTool(crate::hook::ToolId::new(self.0.tool_id())),
            ))
        } else {
            Ok(crate::hook::ActionBatch::default())
        }
    }
}

struct AccessMutationTool {
    access: AccessMutationFixture,
    mutation: TestAccessMutation,
}

#[async_trait]
impl crate::hook::PlatformTool for AccessMutationTool {
    fn id(&self) -> crate::hook::ToolId {
        crate::hook::ToolId::new(self.mutation.tool_id())
    }

    fn external_name(&self) -> &str {
        self.mutation.external_name()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _context: crate::hook::ToolExecutionContext,
    ) -> Result<serde_json::Value, crate::hook::PlatformToolError> {
        let gateway = self
            .access
            .gateway()
            .ok_or_else(|| crate::hook::PlatformToolError::new("missing test gateway"))?;
        let key_id = self
            .access
            .key_id()
            .ok_or_else(|| crate::hook::PlatformToolError::new("missing test key"))?;
        let (is_enabled, model_ids) = match self.mutation {
            TestAccessMutation::DisableKey => (Some(false), None),
            TestAccessMutation::RevokeBinding => (
                None,
                Some(vec![self.access.replacement_model_id().ok_or_else(
                    || crate::hook::PlatformToolError::new("missing replacement model"),
                )?]),
            ),
        };
        gateway
            .admin()
            .update_api_key(
                &key_id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: None,
                    is_enabled,
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    expires_at: None,
                    model_ids,
                    inject_media_understanding: None,
                },
            )
            .await
            .map_err(|error| crate::hook::PlatformToolError::new(error.to_string()))?;
        Ok(serde_json::json!({ "mutated": true }))
    }
}

struct CountingStreamToolHook {
    begins: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    expose_tool: bool,
}

struct CountingStreamToolSession {
    transformer: CountingStreamTransformer,
}

struct CountingStreamTransformer {
    begins: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    expose_tool: bool,
}

impl crate::hook::Hook for CountingStreamToolHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![
                crate::hook::EventKind::Request,
                crate::hook::EventKind::Stream,
            ],
            ..crate::hook::HookDescriptor::all("count-stream-tool-legs")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(CountingStreamToolSession {
            transformer: CountingStreamTransformer {
                begins: self.begins.clone(),
                closes: self.closes.clone(),
                expose_tool: self.expose_tool,
            },
        })
    }
}

#[async_trait]
impl crate::hook::HookSession for CountingStreamToolSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if self.transformer.expose_tool && matches!(event, crate::hook::HookEvent::Request { .. }) {
            Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::ExposeTool(crate::hook::ToolId::new("ordered-tool")),
            ))
        } else {
            Ok(crate::hook::ActionBatch::default())
        }
    }

    fn stream_transformer(&mut self) -> Option<&mut dyn crate::hook::StreamTransformer> {
        Some(&mut self.transformer)
    }
}

impl crate::hook::StreamTransformer for CountingStreamTransformer {
    fn begin(&mut self) -> Result<(), String> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn transform(
        &mut self,
        _delta: &crate::protocol::ir::AiStreamDelta,
    ) -> Result<crate::hook::StreamDirective, String> {
        Ok(crate::hook::StreamDirective::Pass)
    }

    fn close(&mut self) -> Result<Vec<crate::protocol::ir::AiStreamDelta>, String> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

async fn assert_hidden_round_rechecks_access(
    mutation: TestAccessMutation,
    expected_status: StatusCode,
    expected_type: &str,
    expected_message: &str,
) {
    let tool_round = serde_json::json!({
        "id": format!("chatcmpl-{}", mutation.tool_id()),
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call-{}", mutation.tool_id()),
                    "type": "function",
                    "function": {
                        "name": format!("stravia__{}", mutation.external_name()),
                        "arguments": "{}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let (base_url, provider_calls) =
        serve_openai_sequence(vec![tool_round, openai_response("must not be delivered")]).await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-hidden-round-{}-test-{}",
            mutation.tool_id(),
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let access = AccessMutationFixture::default();
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(ExposeAccessMutationHook(mutation)))
        .platform_tool(Arc::new(AccessMutationTool {
            access: access.clone(),
            mutation,
        }))
        .build()
        .await
        .expect("gateway init");
    let model = format!("hidden-round-protected-{}", mutation.tool_id());
    let replacement_model_id = configure_route_with_id(
        &gateway,
        &format!("hidden-round-replacement-{}", mutation.tool_id()),
        &[base_url.clone()],
    )
    .await;
    let model_id = configure_route_with_id(&gateway, &model, &[base_url]).await;
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: format!("Hidden-round {} key", mutation.tool_id()),
            concurrency_limit: Some(1),
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![model_id],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    access.set(gateway.clone(), key.id, replacement_model_id);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.token)).expect("auth header"),
    );

    let response = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers,
        AiRequest::new(model, Vec::new()),
    )
    .await;

    assert_eq!(response.status(), expected_status);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("authorization response");
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("authorization response JSON");
    assert_eq!(body["error"]["type"], expected_type);
    assert_eq!(body["error"]["message"], expected_message);
    assert!(body["error"].get("request_id").is_none());
}

async fn buffered_platform_only_executes_hidden_round_impl() {
    let platform_round = serde_json::json!({
        "id": "chatcmpl-buffered-platform-only",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "platform-call",
                    "type": "function",
                    "function": {
                        "name": "stravia__ordered_tool",
                        "arguments": "{\"index\":1}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let (base_url, provider_calls) =
        serve_openai_sequence(vec![platform_round, openai_response("final answer")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "buffered-platform-only", &[base_url]).await;

    let response = execute_non_stream(gateway.clone(), "buffered-platform-only").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("buffered Platform-only response body");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{body}"
    );
    assert!(body.contains("final answer"), "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );
    let completed_marker_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM history_markers \
         WHERE published_at IS NOT NULL AND execution_state = 'completed'",
    )
    .fetch_one(gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool"))
    .await
    .expect("completed History Marker count");
    assert_eq!(completed_marker_count, 1);
}

async fn hidden_round_request_hook_response_is_delivered_impl() {
    let platform_round = serde_json::json!({
        "id": "chatcmpl-hook-followup",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "platform-call",
                    "type": "function",
                    "function": {
                        "name": "stravia__ordered_tool",
                        "arguments": "{\"index\":1}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let (base_url, provider_calls) = serve_openai_sequence(vec![platform_round]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (gateway, mut logs) = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(HiddenRoundRespondHook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "hook-followup", &[base_url]).await;

    let response = execute_stream(gateway.clone(), "hook-followup").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("hidden-round Hook response body");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{body}"
    );
    assert!(body.contains("hook completed hidden round"), "{body}");
    assert!(!body.contains("stream_mid_error"), "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );
    let generation_payload = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM turn_chain_nodes WHERE kind = 'response' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool"))
    .await
    .expect("hidden-round Hook Generation Chain payload");
    assert!(
        generation_payload.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{generation_payload}"
    );
    assert!(
        generation_payload.contains("hook completed hidden round"),
        "{generation_payload}"
    );
    let mut entries = Vec::new();
    for _ in 0..2 {
        entries.push(
            tokio::time::timeout(std::time::Duration::from_secs(1), logs.recv())
                .await
                .expect("hidden-round Hook log should be emitted")
                .expect("hidden-round Hook log channel should remain open"),
        );
    }
    assert!(
        entries.iter().any(|entry| {
            entry
                .client_response_body
                .as_deref()
                .is_some_and(|body| body.contains("hook completed hidden round"))
        }),
        "{entries:#?}"
    );
}

async fn hidden_round_request_hook_rejection_is_delivered_impl() {
    let platform_round = serde_json::json!({
        "id": "chatcmpl-hook-reject",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "platform-call",
                    "type": "function",
                    "function": {
                        "name": "stravia__ordered_tool",
                        "arguments": "{\"index\":1}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let (base_url, provider_calls) = serve_openai_sequence(vec![platform_round]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(HiddenRoundRejectHook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "hook-reject", &[base_url]).await;

    let response = execute_stream(gateway, "hook-reject").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("hidden-round Hook rejection body");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{body}"
    );
    assert!(body.contains("hidden round rejected"), "{body}");
    assert!(!body.contains("stream aborted"), "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );
}

async fn mixed_tool_continuation_replays_impl() {
    use crate::protocol::ir::{AiItem, MessageContent, Role, ToolCall, ToolSpec};

    let mixed_tool_round = serde_json::json!({
        "id": "chatcmpl-mixed-tools",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "platform-call",
                        "type": "function",
                        "function": {
                            "name": "stravia__ordered_tool",
                            "arguments": "{\"index\":1}"
                        }
                    },
                    {
                        "id": "client-call",
                        "type": "function",
                        "function": {
                            "name": "client_tool",
                            "arguments": "{\"query\":\"one\"}"
                        }
                    }
                ]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let (base_url, provider_calls) = serve_openai_sequence(vec![
        mixed_tool_round,
        openai_response("resumed response"),
        openai_response("fresh response"),
    ])
    .await;
    let config = crate::config::GatewayConfig {
        data_dir: std::env::temp_dir().join(format!(
            "stravia-lifecycle-mixed-continuation-test-{}",
            uuid::Uuid::new_v4()
        )),
        ..Default::default()
    };
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, _logs) = crate::Gateway::builder(config)
        .hook(Arc::new(expose_tool_hook))
        .platform_tool(Arc::new(OrderedTool {
            calls: tool_calls.clone(),
        }))
        .build()
        .await
        .expect("gateway init");
    configure_route(&gateway, "mixed-continuation-route", &[base_url]).await;
    let headers = authorized_headers(&gateway).await;
    let mut initial = AiRequest::new(
        "mixed-continuation-route",
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    initial.tools = Some(vec![ToolSpec {
        name: "client_tool".into(),
        description: None,
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } }
        }),
        strict: None,
        cache_control: None,
        meta: None,
    }]);

    let first =
        execute_non_stream_request_with_headers(gateway.clone(), headers.clone(), initial.clone())
            .await;
    assert_eq!(first.status(), StatusCode::OK);
    assert!(
        tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "Platform Tool execution must wait for response-body delivery"
    );
    let first_body = to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("mixed continuation first body");
    let first_body = String::from_utf8_lossy(&first_body);
    assert!(first_body.contains("client-call"), "{first_body}");
    assert!(!first_body.contains("platform-call"), "{first_body}");
    assert!(
        first_body.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{first_body}"
    );
    let reference_start = first_body
        .find(crate::history_marker::HISTORY_MARKER_PREFIX)
        .expect("History Marker")
        + crate::history_marker::HISTORY_MARKER_PREFIX.len();
    let reference_end = first_body[reference_start..]
        .find(" -->")
        .expect("History Marker suffix")
        + reference_start;
    let marker =
        crate::history_marker::render_history_marker(&crate::history_marker::HistoryMarker {
            reference: first_body[reference_start..reference_end].into(),
            kind: crate::history_marker::HistoryMarkerKind::Platform,
            activity: "Running a platform tool".into(),
        });

    let mut resumed = initial;
    resumed.items.push(AiItem {
        role: Role::Assistant,
        content: MessageContent::Text(marker),
        tool_calls: Some(vec![ToolCall {
            id: "client-call".into(),
            name: "client_tool".into(),
            arguments: r#"{"query":"one"}"#.into(),
        }]),
        tool_call_id: None,
        meta: None,
    });
    resumed.items.push(AiItem {
        role: Role::Tool,
        content: MessageContent::Text("client result".into()),
        tool_calls: None,
        tool_call_id: Some("client-call".into()),
        meta: None,
    });

    let resumed_response =
        execute_non_stream_request_with_headers(gateway.clone(), headers.clone(), resumed.clone())
            .await;
    assert_eq!(resumed_response.status(), StatusCode::OK);
    let resumed_body = to_bytes(resumed_response.into_body(), usize::MAX)
        .await
        .expect("resumed Marker body");
    assert!(
        String::from_utf8_lossy(&resumed_body).contains("resumed response"),
        "{}",
        String::from_utf8_lossy(&resumed_body)
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );

    let branch = execute_non_stream_request_with_headers(gateway, headers, resumed).await;
    assert_eq!(branch.status(), StatusCode::OK);
    let branch_body = to_bytes(branch.into_body(), usize::MAX)
        .await
        .expect("branch response body");
    assert!(
        String::from_utf8_lossy(&branch_body).contains("fresh response"),
        "{}",
        String::from_utf8_lossy(&branch_body)
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);
}

async fn platform_only_stream_continues_with_marker_impl() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse_platform_tool_call(),
        openai_sse_with_usage("final answer", 22, 2),
        openai_sse_with_usage("continued answer", 33, 3),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, mut logs) = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "platform-only-stream", &[base_url]).await;

    let response = execute_stream(gateway.clone(), "platform-only-stream").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Platform-only stream body");
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{body}"
    );
    assert!(
        !body.contains(r#""content":"\n\n<!-- stravia-history-marker:"#),
        "streamed history markers must match the canonical assistant output exactly: {body}"
    );
    assert_eq!(
        body.matches(crate::history_marker::HISTORY_MARKER_PREFIX)
            .count(),
        1,
        "{body}"
    );
    assert!(body.contains("final answer"), "{body}");
    assert!(!body.contains("platform-call"), "{body}");
    assert!(!body.contains("stravia__ordered_tool"), "{body}");
    assert!(!body.contains(r#"{\"index\":1}"#), "{body}");
    let assistant_reasoning = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|event| {
            event["choices"][0]["delta"]["reasoning_content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<String>();
    assert!(
        assistant_reasoning.starts_with(crate::history_marker::HISTORY_MARKER_PREFIX),
        "{assistant_reasoning}"
    );
    let assistant_text = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|event| {
            event["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<String>();
    assert_eq!(assistant_text, "final answer");

    let mut first_user = crate::protocol::ir::AiItem::output_text("test");
    first_user.role = crate::protocol::ir::Role::User;
    let mut second_user = crate::protocol::ir::AiItem::output_text("follow up");
    second_user.role = crate::protocol::ir::Role::User;
    let mut second_request = AiRequest::new(
        "platform-only-stream",
        vec![
            first_user,
            crate::protocol::ir::AiItem::thinking(assistant_reasoning, None),
            crate::protocol::ir::AiItem::output_text(assistant_text),
            second_user,
        ],
    );
    second_request.stream.enabled = true;
    let second_response = execute_non_stream_request(gateway.clone(), second_request).await;
    let second_status = second_response.status();
    let second_body = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("continued response body");
    assert_eq!(
        second_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&second_body)
    );
    assert!(
        String::from_utf8_lossy(&second_body).contains("continued answer"),
        "{}",
        String::from_utf8_lossy(&second_body)
    );

    let child_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM turn_chain_nodes WHERE kind = 'response' AND parent_id IS NOT NULL",
    )
    .fetch_one(gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool"))
    .await
    .expect("count Generation Chain children");
    assert_eq!(
        child_count, 1,
        "the streamed assistant output must discover the first response as its exact parent"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );
    let mut entries = Vec::new();
    for _ in 0..3 {
        entries.push(
            tokio::time::timeout(std::time::Duration::from_secs(1), logs.recv())
                .await
                .expect("one log per streamed Model Turn")
                .expect("request log channel remains open"),
        );
    }
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.usage.prompt_tokens)
            .collect::<Vec<_>>(),
        vec![11, 22, 33]
    );
    assert!(entries[0].path.is_none());
    assert!(entries[1].path.is_some());
    assert!(entries[2].path.is_some());
}

async fn platform_only_stream_preserves_client_tool_arguments_impl() {
    let (base_url, provider_calls) = serve_sse_sequence(vec![
        openai_sse_platform_tool_call(),
        openai_sse_client_tool_call(),
    ])
    .await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "platform-client-tool-stream", &[base_url]).await;

    let response = execute_stream(gateway, "platform-client-tool-stream").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Platform continuation client Tool body");
    let body = String::from_utf8_lossy(&body);
    let mut tool_name = String::new();
    let mut tool_arguments = String::new();
    for event in body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str::<serde_json::Value>(data).expect("client SSE event"))
    {
        for call in event["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(name) = call["function"]["name"].as_str() {
                tool_name.push_str(name);
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                tool_arguments.push_str(arguments);
            }
        }
    }

    assert_eq!(tool_name, "Read", "{body}");
    assert_eq!(tool_arguments, r#"{"value":"preserved"}"#, "{body}");
    assert!(!body.contains("platform-call"), "{body}");
    assert!(!body.contains("stravia__ordered_tool"), "{body}");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![1]
    );
}

async fn platform_markers_are_ingress_neutral_impl() {
    let platform_round = serde_json::json!({
        "id": "chatcmpl-platform-only",
        "object": "chat.completion",
        "created": 1,
        "model": "provider-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "platform-call",
                    "type": "function",
                    "function": {
                        "name": "stravia__ordered_tool",
                        "arguments": "{\"index\":1}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    });
    let protocols = [
        (
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "/v1/chat/completions",
        ),
        (OPEN_RESPONSES_2026_04_24, "/v1/responses"),
        (ANTHROPIC_MESSAGES_2023_06_01, "/v1/messages"),
        (
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            "/v1beta/models/ingress-neutral:generateContent",
        ),
        (BEDROCK_CONVERSE_V1, "/model/ingress-neutral/converse"),
        (COHERE_CHAT_V2, "/v2/chat"),
        (WATSONX_TEXT_CHAT_V1, "/ml/v1/text/chat"),
        (GATEWAY_LANGUAGE_MODEL_V4, "/language-model"),
    ];
    let mut provider_responses = Vec::new();
    for _ in &protocols {
        provider_responses.push(platform_round.clone());
        provider_responses.push(openai_response("final answer"));
    }
    let (base_url, provider_calls) = serve_openai_sequence(provider_responses).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let tool_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (expose_tool_hook, _request_hook_rounds) = ExposeOrderedToolHook::counting();
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(expose_tool_hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::clone(&tool_calls),
    }))
    .build()
    .await
    .expect("Gateway");
    configure_route(&gateway, "ingress-neutral", &[base_url]).await;

    for (ingress, path) in protocols {
        let response =
            execute_protocol_request(gateway.clone(), "ingress-neutral", ingress, path, false)
                .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("ingress-neutral Marker body");
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::OK, "{ingress}: {body}");
        assert!(
            body.contains(crate::history_marker::HISTORY_MARKER_PREFIX),
            "{ingress}: {body}"
        );
        assert!(body.contains("final answer"), "{ingress}: {body}");
        assert!(!body.contains("platform-call"), "{ingress}: {body}");
        assert!(!body.contains("stravia__ordered_tool"), "{ingress}: {body}");
        let body_json: serde_json::Value =
            serde_json::from_str(&body).expect("protocol response JSON");
        let marker_is_reasoning = if ingress == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
            || ingress == WATSONX_TEXT_CHAT_V1
        {
            body_json["choices"][0]["message"]["reasoning_content"]
                .as_str()
                .is_some_and(|text| text.contains(crate::history_marker::HISTORY_MARKER_PREFIX))
                && !body_json["choices"][0]["message"]["content"]
                    .as_str()
                    .is_some_and(|text| text.contains(crate::history_marker::HISTORY_MARKER_PREFIX))
        } else if ingress == OPEN_RESPONSES_2026_04_24 {
            body_json["output"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| {
                    item["type"] == "reasoning"
                        && item
                            .to_string()
                            .contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                })
        } else if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
            body_json["content"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|block| {
                    block["type"] == "thinking"
                        && block["thinking"].as_str().is_some_and(|text| {
                            text.contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                        })
                })
        } else if ingress == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
            body_json["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|part| {
                    part["thought"] == true
                        && part["text"].as_str().is_some_and(|text| {
                            text.contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                        })
                })
        } else if ingress == BEDROCK_CONVERSE_V1 {
            body_json["output"]["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|block| {
                    block
                        .pointer("/reasoningContent/reasoningText/text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| {
                            text.contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                        })
                })
        } else if ingress == COHERE_CHAT_V2 {
            body_json["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|block| {
                    block["type"] == "thinking"
                        && block["thinking"].as_str().is_some_and(|text| {
                            text.contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                        })
                })
        } else if ingress == GATEWAY_LANGUAGE_MODEL_V4 {
            body_json["content"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|block| {
                    block["type"] == "reasoning"
                        && block["text"].as_str().is_some_and(|text| {
                            text.contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                        })
                })
        } else {
            false
        };
        assert!(marker_is_reasoning, "{ingress}: {body}");
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 16);
    assert_eq!(
        tool_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len(),
        8
    );
}

fn openai_sse_platform_tool_call() -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "upstream-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "platform-call",
                        "type": "function",
                        "function": {
                            "name": "stravia__ordered_tool",
                            "arguments": "{\"index\":1}"
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        }),
        serde_json::json!({
            "id": "upstream-tool",
            "model": "provider-model",
            "choices": [],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 1,
                "total_tokens": 12
            }
        })
    )
}

fn openai_sse_projected_platform_leg() -> String {
    [
        serde_json::json!({
            "id": "upstream-projected-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "reasoning_content": "R1"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"content": "C"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"content": "1"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "platform-call",
                        "type": "function",
                        "function": {
                            "name": "stravia__ordered_tool",
                            "arguments": "{\"index\":1}"
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-tool",
            "model": "provider-model",
            "choices": [],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 1,
                "total_tokens": 12
            }
        }),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".into()))
    .collect()
}

fn openai_sse_reasoning_and_text(
    reasoning: &str,
    text: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
) -> String {
    [
        serde_json::json!({
            "id": "upstream-projected-final",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "reasoning_content": reasoning},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-final",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"content": text},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-final",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        }),
        serde_json::json!({
            "id": "upstream-projected-final",
            "model": "provider-model",
            "choices": [],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            }
        }),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".into()))
    .collect()
}

fn openai_sse_text_thinking_text() -> String {
    [
        serde_json::json!({
            "id": "upstream-post-text-thinking",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": "C1"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-post-text-thinking",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"reasoning_content": "R2"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-post-text-thinking",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {"content": "C2"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-post-text-thinking",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        }),
        serde_json::json!({
            "id": "upstream-post-text-thinking",
            "model": "provider-model",
            "choices": [],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 3,
                "total_tokens": 4
            }
        }),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".into()))
    .collect()
}

fn openai_sse_client_tool_call() -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "upstream-client-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "client-call",
                        "type": "function",
                        "function": {
                            "name": "Read",
                            "arguments": "{\"value\":"
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-client-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "\"preserved\"}"
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-client-tool",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        }),
        serde_json::json!({
            "id": "upstream-client-tool",
            "model": "provider-model",
            "choices": [],
            "usage": {
                "prompt_tokens": 22,
                "completion_tokens": 2,
                "total_tokens": 24
            }
        })
    )
}

fn openai_sse(content: &str) -> String {
    openai_sse_with_usage(content, 1, 1)
}

fn openai_sse_with_usage(content: &str, prompt_tokens: u32, completion_tokens: u32) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "upstream-1",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": content },
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "upstream-1",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        }),
        serde_json::json!({
            "id": "upstream-1",
            "model": "provider-model",
            "choices": [],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens.saturating_add(completion_tokens)
            }
        })
    )
}

fn openai_responses_sse(content: &str) -> String {
    let message_in_progress = serde_json::json!({
        "id": "msg-1",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": []
    });
    let message_completed = serde_json::json!({
        "id": "msg-1",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": content,
            "annotations": []
        }]
    });
    let in_progress_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            "resp-provider",
            "provider-model",
            "in_progress",
            Vec::new(),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        );
    let completed_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            "resp-provider",
            "provider-model",
            "completed",
            vec![message_completed.clone()],
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!({
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }),
        );
    format!(
        "event: response.created\ndata: {}\n\n\
             event: response.output_item.added\ndata: {}\n\n\
             event: response.content_part.added\ndata: {}\n\n\
             event: response.output_text.delta\ndata: {}\n\n\
             event: response.content_part.done\ndata: {}\n\n\
             event: response.output_item.done\ndata: {}\n\n\
             event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": in_progress_response
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": message_in_progress
        }),
        serde_json::json!({
            "type": "response.content_part.added",
            "sequence_number": 2,
            "item_id": "msg-1",
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": "",
                "annotations": [],
                "logprobs": []
            }
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "sequence_number": 3,
            "item_id": "msg-1",
            "output_index": 0,
            "content_index": 0,
            "delta": content
        }),
        serde_json::json!({
            "type": "response.content_part.done",
            "sequence_number": 4,
            "item_id": "msg-1",
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": content,
                "annotations": [],
                "logprobs": []
            }
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 5,
            "output_index": 0,
            "item": message_completed
        }),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 6,
            "response": completed_response
        })
    )
}

fn openai_responses_protected_reasoning_sse_parts(content: &str) -> (String, String) {
    let reasoning_in_progress = serde_json::json!({
        "id": "rs-protected",
        "type": "reasoning",
        "status": "in_progress",
        "summary": [],
        "content": []
    });
    let reasoning_completed = serde_json::json!({
        "id": "rs-protected",
        "type": "reasoning",
        "status": "completed",
        "summary": [],
        "content": [],
        "encrypted_content": "opaque-reasoning"
    });
    let message_in_progress = serde_json::json!({
        "id": "msg-after-reasoning",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": []
    });
    let message_completed = serde_json::json!({
        "id": "msg-after-reasoning",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": content,
            "annotations": []
        }]
    });
    let in_progress_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            "resp-protected",
            "provider-model",
            "in_progress",
            Vec::new(),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        );
    let completed_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            "resp-protected",
            "provider-model",
            "completed",
            vec![reasoning_completed.clone(), message_completed.clone()],
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!({
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 1}
            }),
        );
    let before_answer = format!(
        "event: response.created\ndata: {}\n\n\
         event: response.output_item.added\ndata: {}\n\n\
         event: response.output_item.done\ndata: {}\n\n",
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": in_progress_response
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": reasoning_in_progress
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 2,
            "output_index": 0,
            "item": reasoning_completed
        }),
    );
    let answer = format!(
        "event: response.output_item.added\ndata: {}\n\n\
         event: response.content_part.added\ndata: {}\n\n\
         event: response.output_text.delta\ndata: {}\n\n\
         event: response.content_part.done\ndata: {}\n\n\
         event: response.output_item.done\ndata: {}\n\n\
         event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 3,
            "output_index": 1,
            "item": message_in_progress
        }),
        serde_json::json!({
            "type": "response.content_part.added",
            "sequence_number": 4,
            "item_id": "msg-after-reasoning",
            "output_index": 1,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": "",
                "annotations": [],
                "logprobs": []
            }
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "sequence_number": 5,
            "item_id": "msg-after-reasoning",
            "output_index": 1,
            "content_index": 0,
            "delta": content
        }),
        serde_json::json!({
            "type": "response.content_part.done",
            "sequence_number": 6,
            "item_id": "msg-after-reasoning",
            "output_index": 1,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": content,
                "annotations": [],
                "logprobs": []
            }
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 7,
            "output_index": 1,
            "item": message_completed
        }),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 8,
            "response": completed_response
        })
    );
    (before_answer, answer)
}

fn openai_responses_tool_sse(content: &str, call_id: &str) -> String {
    let message_in_progress = serde_json::json!({
        "id": "msg-tool",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": []
    });
    let message_completed = serde_json::json!({
        "id": "msg-tool",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": content,
            "annotations": []
        }]
    });
    let function_call_in_progress = serde_json::json!({
        "id": "fc-tool",
        "type": "function_call",
        "status": "in_progress",
        "arguments": "",
        "call_id": call_id,
        "name": "client_tool"
    });
    let function_call_completed = serde_json::json!({
        "id": "fc-tool",
        "type": "function_call",
        "status": "completed",
        "arguments": "{\"value\":1}",
        "call_id": call_id,
        "name": "client_tool"
    });
    let in_progress_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            "resp-provider",
            "provider-model",
            "in_progress",
            Vec::new(),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        );
    let completed_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            "resp-provider",
            "provider-model",
            "completed",
            vec![message_completed.clone(), function_call_completed.clone()],
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!({
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }),
        );
    format!(
        "event: response.created\ndata: {}\n\n\
             event: response.output_item.added\ndata: {}\n\n\
             event: response.output_item.done\ndata: {}\n\n\
             event: response.output_item.added\ndata: {}\n\n\
             event: response.output_item.done\ndata: {}\n\n\
             event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": in_progress_response
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": message_in_progress
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 2,
            "output_index": 0,
            "item": message_completed
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 3,
            "output_index": 1,
            "item": function_call_in_progress
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 4,
            "output_index": 1,
            "item": function_call_completed
        }),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 5,
            "response": completed_response
        }),
    )
}

fn openai_responses_protected_parallel_tools_sse(
    response_id: &str,
    calls: &[crate::protocol::ir::ToolCall],
) -> String {
    let reasoning_in_progress = serde_json::json!({
        "id": "rs-protected-tools",
        "type": "reasoning",
        "status": "in_progress",
        "summary": [],
        "content": []
    });
    let reasoning_completed = serde_json::json!({
        "id": "rs-protected-tools",
        "type": "reasoning",
        "status": "completed",
        "summary": [{
            "type": "summary_text",
            "text": "inspect repository"
        }],
        "content": [],
        "encrypted_content": "opaque-reasoning"
    });
    let in_progress_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            response_id,
            "provider-model",
            "in_progress",
            Vec::new(),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        );
    let mut output = vec![reasoning_completed.clone()];
    let mut events = vec![serde_json::json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": in_progress_response
    })];
    events.push(serde_json::json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": reasoning_in_progress
    }));
    events.push(serde_json::json!({
        "type": "response.output_item.done",
        "sequence_number": 2,
        "output_index": 0,
        "item": reasoning_completed
    }));
    for (index, call) in calls.iter().enumerate() {
        let item_id = format!("fc-protected-{index}");
        let in_progress = serde_json::json!({
            "id": item_id,
            "type": "function_call",
            "status": "in_progress",
            "arguments": "",
            "call_id": call.id,
            "name": call.name
        });
        let completed = serde_json::json!({
            "id": item_id,
            "type": "function_call",
            "status": "completed",
            "arguments": call.arguments,
            "call_id": call.id,
            "name": call.name
        });
        let sequence_number = 3 + index * 2;
        events.push(serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": sequence_number,
            "output_index": index + 1,
            "item": in_progress
        }));
        events.push(serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": sequence_number + 1,
            "output_index": index + 1,
            "item": completed
        }));
        output.push(completed);
    }
    let completed_response =
        crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
            response_id,
            "provider-model",
            "completed",
            output,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!({
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 1}
            }),
        );
    events.push(serde_json::json!({
        "type": "response.completed",
        "sequence_number": 3 + calls.len() * 2,
        "response": completed_response
    }));

    let mut stream = events
        .into_iter()
        .map(|event| {
            let event_type = event["type"].as_str().expect("event type");
            format!("event: {event_type}\ndata: {event}\n\n")
        })
        .collect::<String>();
    stream.push_str("data: [DONE]\n\n");
    stream
}

#[derive(Clone)]
struct ResponsesWebSocketFixture {
    response_streams: Arc<std::sync::Mutex<VecDeque<String>>>,
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    connections: Arc<AtomicUsize>,
}

async fn responses_websocket_handler(
    State(fixture): State<ResponsesWebSocketFixture>,
    upgrade: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    upgrade.on_upgrade(move |socket| serve_responses_websocket(socket, fixture))
}

async fn serve_responses_websocket(mut socket: WebSocket, fixture: ResponsesWebSocketFixture) {
    fixture.connections.fetch_add(1, Ordering::SeqCst);
    while let Some(Ok(AxumWebSocketMessage::Text(text))) = socket.recv().await {
        fixture
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(serde_json::from_str(&text).expect("Responses WebSocket request JSON"));
        let response_stream = fixture
            .response_streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .expect("configured Responses WebSocket response");
        for event in response_stream.split("\n\n").filter_map(|event| {
            event.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("data: ")
                    .filter(|data| *data != "[DONE]")
                    .map(str::to_owned)
            })
        }) {
            socket
                .send(AxumWebSocketMessage::Text(event.into()))
                .await
                .expect("send Responses WebSocket event");
        }
    }
}

async fn serve_responses_websocket_sequence(
    responses: Vec<&str>,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    serve_responses_websocket_streams(responses.into_iter().map(openai_responses_sse).collect())
        .await
}

async fn serve_responses_websocket_streams(
    response_streams: Vec<String>,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Responses WebSocket provider");
    let address = listener
        .local_addr()
        .expect("Responses WebSocket provider address");
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fixture = ResponsesWebSocketFixture {
        response_streams: Arc::new(std::sync::Mutex::new(response_streams.into())),
        requests: requests.clone(),
        connections: connections.clone(),
    };
    let app = Router::new()
        .route("/v1/responses", get(responses_websocket_handler))
        .with_state(fixture);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve Responses WebSocket provider");
    });
    (format!("http://{address}/v1"), connections, requests)
}

#[derive(Clone)]
struct MissingPreviousFixture {
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    connections: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    visible_event_before_error: bool,
}

async fn missing_previous_handler(
    State(fixture): State<MissingPreviousFixture>,
    upgrade: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        fixture.connections.fetch_add(1, Ordering::SeqCst);
        while let Some(Ok(AxumWebSocketMessage::Text(text))) = socket.recv().await {
            let request = serde_json::from_str(&text).expect("Responses WebSocket request JSON");
            fixture
                .requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            let call = fixture.calls.fetch_add(1, Ordering::SeqCst);
            if call == 1 {
                if fixture.visible_event_before_error {
                    let created = openai_responses_sse("unused")
                        .split("\n\n")
                        .find_map(|event| {
                            event.lines().find_map(|line| {
                                line.trim()
                                    .strip_prefix("data: ")
                                    .filter(|data| data.contains("\"response.created\""))
                                    .map(str::to_owned)
                            })
                        })
                        .expect("response.created fixture");
                    socket
                        .send(AxumWebSocketMessage::Text(created.into()))
                        .await
                        .expect("send visible response event");
                }
                socket
                    .send(AxumWebSocketMessage::Text(
                        serde_json::json!({
                            "type": "error",
                            "error": {
                                "code": "previous_response_not_found",
                                "message": "connection-local response expired"
                            }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .expect("send missing previous error");
                continue;
            }
            let content = if call == 0 {
                "first answer"
            } else {
                "second answer"
            };
            for event in openai_responses_sse(content)
                .split("\n\n")
                .filter_map(|event| {
                    event.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix("data: ")
                            .filter(|data| *data != "[DONE]")
                            .map(str::to_owned)
                    })
                })
            {
                socket
                    .send(AxumWebSocketMessage::Text(event.into()))
                    .await
                    .expect("send Responses WebSocket event");
            }
        }
    })
}

async fn serve_missing_previous_websocket(
    visible_event_before_error: bool,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind missing-previous provider");
    let address = listener.local_addr().expect("provider address");
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fixture = MissingPreviousFixture {
        requests: requests.clone(),
        connections: connections.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
        visible_event_before_error,
    };
    let app = Router::new()
        .route("/v1/responses", get(missing_previous_handler))
        .with_state(fixture);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve missing-previous provider");
    });
    (format!("http://{address}/v1"), connections, requests)
}

#[derive(Clone)]
struct ConnectionLimitFixture {
    websocket_requests: Arc<AtomicUsize>,
    http_requests: Arc<AtomicUsize>,
}

async fn connection_limit_websocket_handler(
    State(fixture): State<ConnectionLimitFixture>,
    upgrade: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        if let Some(Ok(AxumWebSocketMessage::Text(_))) = socket.recv().await {
            fixture.websocket_requests.fetch_add(1, Ordering::SeqCst);
            socket
                .send(AxumWebSocketMessage::Text(
                    serde_json::json!({
                        "type": "error",
                        "error": {
                            "code": "websocket_connection_limit_reached",
                            "message": "connection limit"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send connection-limit error");
        }
    })
}

async fn connection_limit_http_handler(
    State(fixture): State<ConnectionLimitFixture>,
) -> impl axum::response::IntoResponse {
    fixture.http_requests.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        openai_responses_sse("http fallback"),
    )
}

async fn serve_connection_limit_fallback() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind connection-limit provider");
    let address = listener.local_addr().expect("provider address");
    let websocket_requests = Arc::new(AtomicUsize::new(0));
    let http_requests = Arc::new(AtomicUsize::new(0));
    let fixture = ConnectionLimitFixture {
        websocket_requests: websocket_requests.clone(),
        http_requests: http_requests.clone(),
    };
    let app = Router::new()
        .route(
            "/v1/responses",
            get(connection_limit_websocket_handler).post(connection_limit_http_handler),
        )
        .with_state(fixture);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve connection-limit provider");
    });
    (
        format!("http://{address}/v1"),
        websocket_requests,
        http_requests,
    )
}

#[derive(Clone)]
struct StaleWebSocketFixture {
    websocket_connections: Arc<AtomicUsize>,
    http_requests: Arc<AtomicUsize>,
    websocket_closed: Arc<tokio::sync::Notify>,
}

async fn stale_websocket_handler(
    State(fixture): State<StaleWebSocketFixture>,
    upgrade: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    fixture.websocket_connections.fetch_add(1, Ordering::SeqCst);
    upgrade.on_upgrade(move |mut socket| async move {
        if let Some(Ok(AxumWebSocketMessage::Text(_))) = socket.recv().await {
            for event in openai_responses_sse("websocket first")
                .split("\n\n")
                .filter_map(|event| {
                    event.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix("data: ")
                            .filter(|data| *data != "[DONE]")
                            .map(str::to_owned)
                    })
                })
            {
                socket
                    .send(AxumWebSocketMessage::Text(event.into()))
                    .await
                    .expect("send Responses WebSocket event");
            }
        }
        drop(socket);
        fixture.websocket_closed.notify_one();
    })
}

async fn stale_websocket_http_handler(
    State(fixture): State<StaleWebSocketFixture>,
) -> impl axum::response::IntoResponse {
    fixture.http_requests.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        openai_responses_sse("http fallback"),
    )
}

async fn serve_stale_websocket_fallback() -> (
    String,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<tokio::sync::Notify>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stale WebSocket provider");
    let address = listener.local_addr().expect("provider address");
    let websocket_connections = Arc::new(AtomicUsize::new(0));
    let http_requests = Arc::new(AtomicUsize::new(0));
    let websocket_closed = Arc::new(tokio::sync::Notify::new());
    let fixture = StaleWebSocketFixture {
        websocket_connections: websocket_connections.clone(),
        http_requests: http_requests.clone(),
        websocket_closed: websocket_closed.clone(),
    };
    let app = Router::new()
        .route(
            "/v1/responses",
            get(stale_websocket_handler).post(stale_websocket_http_handler),
        )
        .with_state(fixture);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve stale WebSocket provider");
    });
    (
        format!("http://{address}/v1"),
        websocket_connections,
        http_requests,
        websocket_closed,
    )
}

async fn serve_sse_sequence(bodies: Vec<String>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind streaming provider");
    let address = listener.local_addr().expect("streaming provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    tokio::spawn(async move {
        for body in bodies {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept streaming provider request");
            let mut request = vec![0_u8; 16 * 1024];
            let _ = socket
                .read(&mut request)
                .await
                .expect("read streaming provider request");
            observed.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write streaming provider response");
        }
    });
    (format!("http://{address}/v1"), calls)
}

async fn serve_gated_sse(
    first_event: String,
    remaining_events: String,
) -> (String, Arc<AtomicUsize>, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gated streaming provider");
    let address = listener
        .local_addr()
        .expect("gated streaming provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .expect("accept gated streaming provider request");
        let mut request = vec![0_u8; 16 * 1024];
        let _ = socket
            .read(&mut request)
            .await
            .expect("read gated streaming provider request");
        observed.fetch_add(1, Ordering::SeqCst);
        let response_head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        socket
            .write_all(response_head.as_bytes())
            .await
            .expect("write gated streaming response headers");
        socket
            .write_all(first_event.as_bytes())
            .await
            .expect("write gated first event");
        socket.flush().await.expect("flush gated first event");
        let _ = release_rx.await;
        socket
            .write_all(remaining_events.as_bytes())
            .await
            .expect("write gated remaining events");
    });
    (format!("http://{address}/v1"), calls, release_tx)
}

async fn serve_stalling_sse() -> (String, Arc<AtomicUsize>) {
    let first_event = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "upstream-stall",
            "model": "provider-model",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": "before deadline"
                },
                "finish_reason": null
            }]
        })
    );
    serve_stalling_sse_with_event(first_event).await
}

async fn serve_stalling_sse_with_event(first_event: String) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalling streaming provider");
    let address = listener
        .local_addr()
        .expect("stalling streaming provider address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .expect("accept stalling streaming provider request");
        let mut request = vec![0_u8; 16 * 1024];
        let _ = socket
            .read(&mut request)
            .await
            .expect("read stalling streaming provider request");
        observed.fetch_add(1, Ordering::SeqCst);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{first_event}"
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write first stalling stream event");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });
    (format!("http://{address}/v1"), calls)
}

async fn serve_sse_response() -> String {
    serve_sse_sequence(vec![openai_sse("original")]).await.0
}

async fn execute_stream_with_timeout(
    gateway: Gateway,
    model: &str,
    timeout: std::time::Duration,
) -> Response {
    let mut request = AiRequest::new(
        model,
        vec![crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::User,
            content: crate::protocol::ir::MessageContent::Text("test".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = true;
    let headers = authorized_headers(&gateway).await;
    execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "test"}],
                "stream": true
            })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request,
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, timeout),
    })
    .await
}

async fn execute_stream(gateway: Gateway, model: &str) -> Response {
    execute_stream_with_timeout(gateway, model, std::time::Duration::from_secs(30)).await
}

async fn execute_protocol_request(
    gateway: Gateway,
    model: &str,
    ingress: ProtocolId,
    path: &str,
    stream: bool,
) -> Response {
    execute_protocol_request_with_timeout(
        gateway,
        model,
        ingress,
        path,
        stream,
        std::time::Duration::from_secs(30),
    )
    .await
}

async fn execute_protocol_request_with_session(
    gateway: Gateway,
    model: &str,
    ingress: ProtocolId,
    path: &str,
    stream: bool,
    session_id: &str,
) -> Response {
    let mut request = AiRequest::new(
        model,
        vec![crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::User,
            content: crate::protocol::ir::MessageContent::Text("test".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = stream;
    let mut headers = authorized_headers(&gateway).await;
    headers.insert(
        header::HeaderName::from_static("x-session-id"),
        header::HeaderValue::from_str(session_id).expect("session header"),
    );
    execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "test"}],
                "stream": stream
            })),
            HashMap::new(),
            "POST",
            path,
        ),
        request,
        ingress,
        context: RequestContext::new(ingress, std::time::Duration::from_secs(30)),
    })
    .await
}

async fn execute_protocol_request_with_timeout(
    gateway: Gateway,
    model: &str,
    ingress: ProtocolId,
    path: &str,
    stream: bool,
    timeout: std::time::Duration,
) -> Response {
    let mut request = AiRequest::new(
        model,
        vec![crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::User,
            content: crate::protocol::ir::MessageContent::Text("test".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = stream;
    let headers = authorized_headers(&gateway).await;
    execute(RunInput {
        gateway: gateway.clone(),
        executor: std::sync::Arc::clone(&gateway.model_turn),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "test"}],
                "stream": stream
            })),
            HashMap::new(),
            "POST",
            path,
        ),
        request,
        ingress,
        context: RequestContext::new(ingress, timeout),
    })
    .await
}

struct RewriteUpstreamHook;
struct RewriteUpstreamSession;

impl crate::hook::Hook for RewriteUpstreamHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::UpstreamResponse],
            ..crate::hook::HookDescriptor::all("rewrite-stream-response")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(RewriteUpstreamSession)
    }
}

#[async_trait]
impl crate::hook::HookSession for RewriteUpstreamSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if matches!(event, crate::hook::HookEvent::UpstreamResponse { .. }) {
            Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::PatchResponse(crate::hook::ResponsePatch::SetContent(
                    "rewritten".into(),
                )),
            ))
        } else {
            Ok(crate::hook::ActionBatch::default())
        }
    }
}

struct ObserveUpstreamHook {
    responses: Arc<std::sync::Mutex<Vec<AiResponse>>>,
}

struct ObserveUpstreamSession {
    responses: Arc<std::sync::Mutex<Vec<AiResponse>>>,
}

impl crate::hook::Hook for ObserveUpstreamHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::UpstreamResponse],
            ..crate::hook::HookDescriptor::all("observe-stream-response")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(ObserveUpstreamSession {
            responses: self.responses.clone(),
        })
    }
}

#[async_trait]
impl crate::hook::HookSession for ObserveUpstreamSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if let crate::hook::HookEvent::UpstreamResponse { response, .. } = event {
            self.responses
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(response.clone());
        }
        Ok(crate::hook::ActionBatch::default())
    }

    fn requires_terminal_buffering(&self) -> bool {
        false
    }
}

struct RejectStreamHook;
struct RejectStreamSession;

impl crate::hook::Hook for RejectStreamHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::UpstreamResponse],
            ..crate::hook::HookDescriptor::all("reject-stream-response")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(RejectStreamSession)
    }
}

#[async_trait]
impl crate::hook::HookSession for RejectStreamSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if matches!(event, crate::hook::HookEvent::UpstreamResponse { .. }) {
            Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::Reject(crate::hook::HookRejection {
                    status: 451,
                    code: "stream_blocked".into(),
                    message: "stream rejected by hook".into(),
                }),
            ))
        } else {
            Ok(crate::hook::ActionBatch::default())
        }
    }
}

struct RespondStreamHook;
struct RespondStreamSession;

impl crate::hook::Hook for RespondStreamHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::UpstreamResponse],
            ..crate::hook::HookDescriptor::all("respond-stream-response")
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(RespondStreamSession)
    }
}

#[async_trait]
impl crate::hook::HookSession for RespondStreamSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if !matches!(event, crate::hook::HookEvent::UpstreamResponse { .. }) {
            return Ok(crate::hook::ActionBatch::default());
        }
        let mut response = AiResponse::new("hook-replacement", "provider-model");
        response.push_output_text("hook replacement");
        response.stop_reason = Some("stop".into());
        Ok(crate::hook::ActionBatch::one(
            crate::hook::HookAction::Respond(Box::new(response)),
        ))
    }
}

#[derive(Clone, Copy)]
enum PostCommitHookFailure {
    Reject,
    Respond,
    Patch,
    Error,
}

impl PostCommitHookFailure {
    fn id(self) -> &'static str {
        match self {
            Self::Reject => "post-commit-reject",
            Self::Respond => "post-commit-respond",
            Self::Patch => "post-commit-patch",
            Self::Error => "post-commit-error",
        }
    }
}

struct PostCommitFailureHook(PostCommitHookFailure);
struct PostCommitFailureSession(PostCommitHookFailure);

impl crate::hook::Hook for PostCommitFailureHook {
    fn descriptor(&self) -> crate::hook::HookDescriptor {
        crate::hook::HookDescriptor {
            event_kinds: vec![crate::hook::EventKind::UpstreamResponse],
            ..crate::hook::HookDescriptor::all(self.0.id())
        }
    }

    fn create_session(
        &self,
        _context: &crate::hook::SessionContext,
    ) -> Box<dyn crate::hook::HookSession> {
        Box::new(PostCommitFailureSession(self.0))
    }
}

#[async_trait]
impl crate::hook::HookSession for PostCommitFailureSession {
    async fn handle(
        &mut self,
        event: crate::hook::HookEvent<'_>,
    ) -> Result<crate::hook::ActionBatch, String> {
        if !matches!(event, crate::hook::HookEvent::UpstreamResponse { .. }) {
            return Ok(crate::hook::ActionBatch::default());
        }
        match self.0 {
            PostCommitHookFailure::Reject => Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::Reject(crate::hook::HookRejection {
                    status: 451,
                    code: "late_reject".into(),
                    message: "late rejection".into(),
                }),
            )),
            PostCommitHookFailure::Patch => Ok(crate::hook::ActionBatch::one(
                crate::hook::HookAction::PatchResponse(crate::hook::ResponsePatch::SetContent(
                    "must not replace committed output".into(),
                )),
            )),
            PostCommitHookFailure::Respond => {
                let mut response = AiResponse::new("late-response", "provider-model");
                response.push_output_text("must not replace committed output");
                Ok(crate::hook::ActionBatch::one(
                    crate::hook::HookAction::Respond(Box::new(response)),
                ))
            }
            PostCommitHookFailure::Error => Err("late Hook error".into()),
        }
    }

    fn requires_terminal_buffering(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod admission;
#[cfg(test)]
mod auth_hook;
#[cfg(test)]
mod continuation;
#[cfg(test)]
mod lifecycle;
#[cfg(test)]
mod media;
#[cfg(test)]
mod persist;
#[cfg(test)]
mod projection;
#[cfg(test)]
mod stream_commit;
#[cfg(test)]
mod websocket;
