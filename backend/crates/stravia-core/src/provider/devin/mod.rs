//! Devin vendor — Connect-RPC protobuf over `server.codeium.com`.
//!
//! Unlike the JSON vendors, `build_request` bypasses the codec's JSON
//! `encode_request` (which bails by design) and assembles the wire bytes
//! itself: `GetChatMessageRequest` protobuf → Connect envelope →
//! `OutboundRequest.body_bytes`. The session credential is sent twice per
//! the upstream contract — single inside `ClientMetadata` (codec) and
//! doubled `Basic <token>-<token>` in the HTTP `Authorization` header.

pub(crate) mod family;
pub(crate) mod selector;

use crate::error::GatewayError;
use crate::protocol::codec::devin_connect::{
    ASSIGN_MODEL_PATH, GET_CHAT_MESSAGE_PATH, ModelAssignment, decode_assign_model_response,
    encode_assign_model_request, encode_get_chat_message_request, session_shape, wrap_request,
};
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, OAuthConfig, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::{
    ConstructedRequest, RequestContext, RequestPurpose, ResolvedTargetCapabilities,
};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use stravia_runtime_contract::protocol::ids::DEVIN_CONNECT_GET_CHAT_MESSAGE_V1;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::thinking::TargetThinkingControl;

const DEFAULT_BASE_URL: &str = "https://server.codeium.com";
const DEVIN_WEBAPP_URL: &str = "https://app.devin.ai";
/// Public client id extracted from `devin.exe`; the same value is used by the
/// CLI's `windsurf/signin` implicit flow.
const DEVIN_OAUTH_CLIENT_ID: &str = "3GUryQ7ldAeKEuD2obYnppsnmj58eP5u";
/// The CLI's manual-flow redirect: the webapp renders the token for the user
/// to paste back instead of a loopback redirect.
const DEVIN_MANUAL_REDIRECT_URI: &str = "chisel-show-auth-token";

/// Selectors verified against the live model catalog by the reference
/// implementation (`dwgx/WindsurfAPI` committed snapshot). Entitlement is
/// plan-dependent — a free account resolves only `swe-1-6-slow`.
const STATIC_MODELS: &[&str] = &[
    "swe-1-6-slow",
    "swe-1-7",
    "swe-1-7-lightning",
    "swe-2-medium",
    "swe-2-high",
    "swe-2-max",
    "claude-sonnet-5-medium",
    "claude-opus-5-medium",
    "claude-5-fable-medium",
    "gpt-5-4-medium",
    "gpt-5-5-medium",
    "gpt-5-6-luna-medium",
    "gemini-3-5-flash-medium",
    "gemini-3-1-pro-low",
    "glm-5-2",
    "kimi-k2-7",
    "deepseek-v4",
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "devin",
    label: Label {
        zh: "Devin",
        en: "Devin",
    },
    icon: "devin",
    default_protocol: "devin-connect",
    credential_fields: crate::provider::metadata::API_KEY_CREDENTIAL_FIELDS,
    option_fields: crate::provider::metadata::NO_OPTION_FIELDS,
    channels: &[ChannelDef {
        // OAuth 渠道沿用「登录产品名」命名约定(openai/codex、xai/grok):
        // WebUI 用 channel id 推导 auth driver key,"devin" 正好命中 driver。
        id: "devin",
        label: Label {
            zh: "Devin",
            en: "Devin",
        },
        base_urls: &[ProtocolBaseUrl {
            protocol: "devin-connect",
            base_url: DEFAULT_BASE_URL,
        }],
        api_key: None,
        // Model discovery is a Connect-RPC (`GetCliModelConfigs`), not an HTTP
        // endpoint — ship the verified selector list instead.
        models_source: None,
        capabilities_source: CapabilitiesSource::Auto,
        static_models: STATIC_MODELS,
        auth_mode: AuthMode::OAuth,
        oauth: Some(OAuthConfig {
            auth_base_url: DEVIN_WEBAPP_URL,
            authorize_url: "https://app.devin.ai/auth/cli/continue",
            // Token exchange is a unary Connect-RPC on the api server, not an
            // HTTP endpoint on the webapp (`/auth/cli/token` is unreachable
            // behind CloudFront): {code, codeVerifier} -> {sessionToken}.
            token_url: "https://server.codeium.com/exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode",
            client_id: DEVIN_OAUTH_CLIENT_ID,
            redirect_uri: DEVIN_MANUAL_REDIRECT_URI,
            scope: "",
        }),
        runtime: None,
    }],
};

pub struct DevinVendor;

/// Connect-RPC client headers mirrored from current devin CLI captures
/// (3000.10.x): no User-Agent at all, a `sentry-trace` id per request, and
/// no accept-encoding (response frames self-describe gzip). `streaming`
/// switches Content-Type between the envelope codec and raw protobuf.
fn connect_headers(session_token: &str, streaming: bool) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(if streaming {
            "application/connect+proto"
        } else {
            "application/proto"
        }),
    );
    headers.insert(
        HeaderName::from_static("connect-protocol-version"),
        HeaderValue::from_static("1"),
    );
    headers.insert(
        HeaderName::from_static("sentry-trace"),
        HeaderValue::from_str(&format!(
            "{}-{}-1",
            uuid::Uuid::new_v4().simple(),
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        ))
        .map_err(|error| GatewayError::internal(error.into()))?,
    );
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    // The upstream contract doubles the session token in the Authorization
    // header while embedding it single in ClientMetadata.
    let authorization = HeaderValue::from_str(&format!("Basic {session_token}-{session_token}"))
        .map_err(|error| GatewayError::internal(error.into()))?;
    headers.insert(AUTHORIZATION, authorization);
    Ok(headers)
}

fn session_token(api_key: &str) -> Result<&str, GatewayError> {
    let token = api_key.trim();
    if token.is_empty() {
        return Err(GatewayError::bad_request(
            "devin_missing_session_token",
            "Devin session token is empty; connect the provider via OAuth or paste a session token",
        ));
    }
    Ok(token)
}

/// AssignModel 解析缓存：jwt 绑 cascade_id，同 (uid, cascade) 会话内复用。
/// 有界——键随会话数累积，触顶整体清空让会话重新解析。
static ASSIGNMENTS: LazyLock<Mutex<HashMap<String, ModelAssignment>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// AssignModel resolves a router uid into the real model uid plus a
/// cascade-bound assignment jwt carried at request field #26. Hitting a
/// router uid directly is answered with the canned `unavailable:
/// third-party model provider` trailer — a permanent failure disguised as
/// transient. `Err` carries the real upstream failure so a known router can
/// fail fast with the true cause instead of replaying the canned error.
async fn assign_model(
    ctx: &ProviderCtx<'_>,
    session_token: &str,
    router_uid: &str,
    cascade_id: &str,
) -> Result<Option<ModelAssignment>, String> {
    let key = format!("{router_uid}|{cascade_id}");
    if let Some(hit) = ASSIGNMENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return Ok(Some(hit.clone()));
    }
    let resolved = assign_model_uncached(ctx, session_token, router_uid, cascade_id).await;
    if let Ok(Some(assignment)) = &resolved {
        let mut cache = ASSIGNMENTS.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(key, assignment.clone());
    }
    resolved
}

#[cfg(test)]
static ASSIGN_MODEL_TEST: Mutex<Option<ModelAssignment>> = Mutex::new(None);
#[cfg(test)]
static ASSIGN_MODEL_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Reset the assignment cache + hook between tests — both are process-global
/// and would otherwise leak state across cases. The returned guard
/// serializes assign-sensitive tests: parallel cases would otherwise observe
/// each other's hook values and call counts.
#[cfg(test)]
async fn reset_assign_state() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = LOCK.lock().await;
    ASSIGNMENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    *ASSIGN_MODEL_TEST.lock().unwrap_or_else(|e| e.into_inner()) = None;
    ASSIGN_MODEL_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
    guard
}

async fn assign_model_uncached(
    ctx: &ProviderCtx<'_>,
    session_token: &str,
    router_uid: &str,
    cascade_id: &str,
) -> Result<Option<ModelAssignment>, String> {
    // Tests never reach upstream: the hook substitutes the whole call.
    #[cfg(test)]
    {
        let _ = (ctx, session_token, router_uid, cascade_id);
        ASSIGN_MODEL_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(ASSIGN_MODEL_TEST
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone());
    }
    #[cfg(not(test))]
    assign_model_http(ctx, session_token, router_uid, cascade_id).await
}

/// The real unary call — kept outside the test hook so the ignored live
/// tests can exercise it against the actual endpoint. `Ok(None)` is an
/// upstream-side empty/invalid assignment; `Err` is a transport or HTTP
/// rejection carrying the Connect `code: message` when available.
async fn assign_model_http(
    ctx: &ProviderCtx<'_>,
    session_token: &str,
    router_uid: &str,
    cascade_id: &str,
) -> Result<Option<ModelAssignment>, String> {
    let client = ctx
        .gw
        .http_client_for_provider(ctx.provider.use_proxy)
        .await
        .map_err(|error| {
            tracing::debug!(%error, "devin AssignModel: no http client");
            format!("http client: {error}")
        })?;
    let url = format!(
        "{}{}",
        ctx.egress_base_url.trim_end_matches('/'),
        ASSIGN_MODEL_PATH
    );
    let response = client
        .post(&url)
        .headers(
            connect_headers(session_token, false).map_err(|error| format!("headers: {error}"))?,
        )
        .body(encode_assign_model_request(
            session_token,
            router_uid,
            cascade_id,
        ))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| {
            tracing::debug!(%error, "devin AssignModel request failed");
            error.to_string()
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response
            .text()
            .await
            .ok()
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            .map(|body| {
                let pick = |key: &str| {
                    body.pointer(&format!("/error/{key}"))
                        .or_else(|| body.get(key))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                match (pick("code"), pick("message")) {
                    (Some(code), Some(message)) => format!("{code}: {message}"),
                    (None, Some(message)) => message,
                    (Some(code), None) => code,
                    (None, None) => String::new(),
                }
            })
            .unwrap_or_default();
        let detail = if detail.is_empty() {
            status.to_string()
        } else {
            format!("{status} {detail}")
        };
        tracing::debug!(%detail, "devin AssignModel rejected");
        return Err(format!("AssignModel({router_uid}) rejected: {detail}"));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("AssignModel body: {error}"))?;
    Ok(decode_assign_model_response(&body))
}

#[async_trait]
impl Vendor for DevinVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor { vendor_id: "devin" }
    }

    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }

    fn assemble_base_url(
        &self,
        _credentials: &std::collections::BTreeMap<String, String>,
        configured_base_url: Option<&str>,
    ) -> anyhow::Result<String> {
        crate::provider::vendor::resolve_base_url(self.vendor_id(), configured_base_url, || {
            DEFAULT_BASE_URL.to_string()
        })
    }

    fn target_capabilities(&self, _protocol: ProtocolId) -> ResolvedTargetCapabilities {
        ResolvedTargetCapabilities {
            stream_only: true,
            responses_websocket: false,
        }
    }

    fn construct_request(
        &self,
        ctx: &RequestContext<'_>,
        purpose: RequestPurpose<'_>,
    ) -> anyhow::Result<ConstructedRequest> {
        let url = match purpose {
            RequestPurpose::Compact { .. } => {
                anyhow::bail!("Vendor does not support standalone compaction")
            }
            RequestPurpose::Inference { .. } | RequestPurpose::Models { .. } => purpose.endpoint(),
        };
        let headers = connect_headers(session_token(ctx.api_key)?, true)?;
        Ok(ConstructedRequest { url, headers })
    }

    fn vendor_id(&self) -> &'static str {
        "devin"
    }

    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[DEVIN_CONNECT_GET_CHAT_MESSAGE_V1]
    }

    async fn build_request(
        &self,
        req: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        req.model = selector::selector_from_alias(ctx.actual_model);
        // Reasoning effort lives in the selector suffix, not a request field.
        // Family records carry their full upstream selector set in
        // `metadata.extensions["devin"]` — resolve the thinking control
        // against it inside the vendor (1M preferred, ordinary lanes before
        // speed lanes, missing levels fall back to the family default).
        // Records without a table (manual adds) keep the legacy suffix
        // rewrite on the normalized id.
        let table = match ctx
            .gw
            .storage
            .provider_models()
            .get(&ctx.provider.id, ctx.actual_model)
            .await
        {
            Ok(record) => record
                .and_then(|record| selector::table_from_extensions(&record.metadata.extensions)),
            Err(error) => {
                tracing::debug!(
                    provider = %ctx.provider.id,
                    model = %ctx.actual_model,
                    %error,
                    "devin selector table lookup failed; using normalized id"
                );
                None
            }
        };
        if let Some(table) = &table {
            req.model = selector::resolve_selector(table, req.reasoning.target_control.as_ref());
        } else if let Some(TargetThinkingControl::Effort { value }) = &req.reasoning.target_control
        {
            req.model = selector::selector_with_level(&req.model, value);
        }
        crate::protocol::codec::tool_correlation::normalize_request_tool_results(req);

        let token = session_token(ctx.api_key)?;
        // Session shape is derived deterministically from the request so the
        // cascade id is computable before AssignModel binds a jwt to it.
        let shape = session_shape(req, token);
        // Catalog-flagged routers MUST resolve through AssignModel — the
        // upstream answers a bare router uid with the canned `unavailable:
        // third-party model provider` trailer, so a failed assignment is
        // propagated rather than replayed into a guaranteed failure.
        // Catalog-flagged non-routers skip the call; uids the catalog does
        // not know (manual adds, stale tables) still attempt it but tolerate
        // failure, falling back to the bare uid.
        let router_known = table
            .as_ref()
            .and_then(|t| t.routers.as_ref())
            .map(|routers| routers.iter().any(|r| r == &req.model));
        let assignment = if router_known == Some(false) {
            None
        } else {
            match assign_model(ctx, token, &req.model, &shape.cascade_id).await {
                Ok(assignment) => {
                    if router_known == Some(true) && assignment.is_none() {
                        return Err(GatewayError::provider_unavailable(
                            "devin",
                            format!("AssignModel({}) returned an empty assignment", req.model),
                        ));
                    }
                    assignment
                }
                Err(error) if router_known == Some(true) => {
                    return Err(GatewayError::provider_unavailable("devin", error));
                }
                Err(error) => {
                    tracing::debug!(%error, "devin AssignModel failed; falling back to bare uid");
                    None
                }
            }
        };
        if let Some(assignment) = &assignment {
            req.model = assignment.model_uid.clone();
        }
        let proto = encode_get_chat_message_request(
            req,
            token,
            &shape,
            assignment.as_ref().map(|a| a.jwt.as_str()),
        )
        .map_err(|error| GatewayError::bad_request("devin_unrepresentable", error.to_string()))?;
        let framed = wrap_request(&proto).map_err(GatewayError::internal)?;
        let url = format!(
            "{}{}",
            ctx.egress_base_url.trim_end_matches('/'),
            GET_CHAT_MESSAGE_PATH
        );
        Ok(OutboundRequest {
            url,
            headers: connect_headers(token, true)?,
            body: Value::Null,
            body_bytes: Some(framed),
        })
    }

    async fn parse_response(
        &self,
        _resp: InboundResponse,
        _ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        Err(GatewayError::bad_request(
            "devin_stream_only",
            "Devin Connect is stream-only; unary responses are not produced",
        ))
    }

    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        let mapped = match status {
            400 | 422 => 400,
            401 => 401,
            403 => 403,
            404 => 404,
            429 => 429,
            503 => 503,
            _ => 502,
        };
        // Non-2xx bodies are Connect/JSON error payloads, not frames: either
        // {"code":"...","message":"..."} or {"error":{"code","message"}}.
        let error = body.get("error").unwrap_or(&body);
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                if code.is_empty() {
                    None
                } else {
                    Some(code.to_string())
                }
            })
            .unwrap_or_else(|| format!("Devin Connect error ({status})"));
        GatewayError::upstream_status("devin", mapped, Some(message))
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(DevinVendor) } }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Provider;
    use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role};
    use uuid::Uuid;

    fn test_provider() -> Provider {
        Provider {
            id: "test".into(),
            name: "test".into(),
            vendor: Some("devin".into()),
            protocol: "devin-connect".into(),
            base_url: DEFAULT_BASE_URL.into(),
            preset_key: Some("devin".into()),
            channel: Some("devin".into()),
            models_source: None,
            static_models: None,
            api_key: "session-token".into(),
            adapter_credentials: r#"{"apiKey":"session-token"}"#.into(),
            vendor_options: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn connect_headers_double_the_session_token() {
        let headers = connect_headers("tok", true).unwrap();
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Basic tok-tok"
        );
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/connect+proto"
        );
        assert_eq!(
            headers
                .get("connect-protocol-version")
                .unwrap()
                .to_str()
                .unwrap(),
            "1"
        );
        // Current CLI captures carry a sentry-trace and no User-Agent or
        // accept-encoding.
        assert!(headers.get("sentry-trace").is_some());
        assert!(headers.get("user-agent").is_none());
        assert!(headers.get("connect-accept-encoding").is_none());
        // Unary calls ride raw protobuf.
        let unary = connect_headers("tok", false).unwrap();
        assert_eq!(
            unary.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/proto"
        );
    }

    #[test]
    fn build_request_uses_history_derived_session_shape() {
        let req = AiRequest::new(
            "swe-1-7",
            vec![
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("open".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Text("reply".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("again".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );
        let shape = session_shape(&req, "session-token");
        assert_eq!(
            shape.cascade_id,
            session_shape(&req, "session-token").cascade_id
        );
    }

    #[tokio::test]
    async fn build_request_emits_connect_envelope_bytes() {
        let _guard = reset_assign_state().await;
        let provider = test_provider();
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut req = AiRequest::new(
            "ignored",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: "swe-1-7",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        assert_eq!(
            outbound.url,
            format!("{DEFAULT_BASE_URL}{GET_CHAT_MESSAGE_PATH}")
        );
        let body = outbound.body_bytes.expect("binary body");
        assert_eq!(body[0], 0, "uncompressed request envelope flag");
        let len = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, body.len() - 5);
        // The protobuf payload contains the actual model selector (#21).
        let proto = String::from_utf8_lossy(&body[5..]);
        assert!(proto.contains("swe-1-7"));
    }

    #[tokio::test]
    async fn build_request_rewrites_selector_to_requested_effort() {
        let _guard = reset_assign_state().await;
        let provider = test_provider();
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut req = AiRequest::new(
            "ignored",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        req.reasoning.target_control = Some(TargetThinkingControl::Effort {
            value: "high".into(),
        });
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: "claude-sonnet-5-medium",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let body = outbound.body_bytes.expect("binary body");
        let proto = String::from_utf8_lossy(&body[5..]);
        assert!(proto.contains("claude-sonnet-5-high"));
        assert!(!proto.contains("claude-sonnet-5-medium"));
    }

    /// A stored family record's `extensions["devin"]` selector table drives
    /// resolution: the logical alias id never reaches the wire, a requested
    /// level maps to its concrete selector, and an absent level falls back
    /// to the family default.
    #[tokio::test]
    async fn build_request_resolves_family_selector_table() {
        let _guard = reset_assign_state().await;
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let provider = gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "devin".into(),
                vendor: Some("devin".into()),
                protocol: "devin-connect".into(),
                base_url: DEFAULT_BASE_URL.into(),
                preset_key: Some("devin".into()),
                channel: Some("devin".into()),
                models_source: None,
                static_models: None,
                api_key: "session-token".into(),
                adapter_credentials: r#"{"apiKey":"session-token"}"#.into(),
                vendor_options: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await
            .unwrap();
        let mut metadata = crate::provider_models::ProviderModelMetadata::bare("gpt-5.6-sol");
        metadata.extensions.insert(
            selector::SELECTOR_EXTENSION_KEY.to_string(),
            selector::table_extension_value(&selector::SelectorTable {
                default: "gpt-5-6-sol-medium".into(),
                selectors: [
                    "gpt-5-6-sol-medium",
                    "gpt-5-6-sol-high",
                    "gpt-5-6-sol-high-priority",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
                routers: Some(Vec::new()),
            }),
        );
        gw.storage
            .provider_models()
            .create(crate::provider_models::NewProviderModelRecord {
                provider_id: provider.id.clone(),
                model_id: "gpt-5.6-sol".into(),
                source_kind: crate::provider_models::ProviderModelSourceKind::Discovered,
                metadata_source_provider_id: None,
                presence: crate::provider_models::ProviderModelPresence::Present,
                selection_policy: crate::provider_models::ProviderModelSelectionPolicy::Auto,
                metadata,
            })
            .await
            .unwrap();

        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: "gpt-5.6-sol",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let make_req = || {
            AiRequest::new(
                "ignored",
                vec![AiItem {
                    role: Role::User,
                    content: MessageContent::Text("hi".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                }],
            )
        };
        // Explicit level → ordinary `-high` selector, never the priority lane
        // and never the logical alias id.
        let mut req = make_req();
        req.reasoning.target_control = Some(TargetThinkingControl::Effort {
            value: "high".into(),
        });
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let body = outbound.body_bytes.expect("binary body");
        let proto = String::from_utf8_lossy(&body[5..]);
        assert!(proto.contains("gpt-5-6-sol-high"));
        assert!(!proto.contains("gpt-5-6-sol-high-priority"));
        assert!(!proto.contains("gpt-5.6-sol"));

        // Missing level (`xhigh` is not in the table) → family default.
        let mut req = make_req();
        req.reasoning.target_control = Some(TargetThinkingControl::Effort {
            value: "xhigh".into(),
        });
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let body = outbound.body_bytes.expect("binary body");
        let proto = String::from_utf8_lossy(&body[5..]);
        assert!(proto.contains("gpt-5-6-sol-medium"));

        // No control → family default as well.
        let mut req = make_req();
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let body = outbound.body_bytes.expect("binary body");
        let proto = String::from_utf8_lossy(&body[5..]);
        assert!(proto.contains("gpt-5-6-sol-medium"));
    }

    /// Selector-table-driven router gating: catalog-flagged routers resolve
    /// through AssignModel and carry the jwt at #26; known non-routers skip
    /// the call; a failed router assignment fails fast instead of replaying
    /// into the guaranteed canned upstream error.
    #[tokio::test]
    async fn build_request_assigns_router_and_carries_jwt() {
        let _guard = reset_assign_state().await;
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let provider = gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "devin".into(),
                vendor: Some("devin".into()),
                protocol: "devin-connect".into(),
                base_url: DEFAULT_BASE_URL.into(),
                preset_key: Some("devin".into()),
                channel: Some("devin".into()),
                models_source: None,
                static_models: None,
                api_key: "session-token".into(),
                adapter_credentials: r#"{"apiKey":"session-token"}"#.into(),
                vendor_options: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await
            .unwrap();
        let mut metadata = crate::provider_models::ProviderModelMetadata::bare("swe-2");
        metadata.extensions.insert(
            selector::SELECTOR_EXTENSION_KEY.to_string(),
            selector::table_extension_value(&selector::SelectorTable {
                default: "swe-2-max".into(),
                selectors: vec!["swe-2-max".into()],
                routers: Some(vec!["swe-2-max".into()]),
            }),
        );
        gw.storage
            .provider_models()
            .create(crate::provider_models::NewProviderModelRecord {
                provider_id: provider.id.clone(),
                model_id: "swe-2".into(),
                source_kind: crate::provider_models::ProviderModelSourceKind::Discovered,
                metadata_source_provider_id: None,
                presence: crate::provider_models::ProviderModelPresence::Present,
                selection_policy: crate::provider_models::ProviderModelSelectionPolicy::Auto,
                metadata,
            })
            .await
            .unwrap();

        // Router + injected assignment → #21 resolved uid + #26 jwt.
        *ASSIGN_MODEL_TEST.lock().unwrap() = Some(ModelAssignment {
            jwt: "jwt-xyz".into(),
            model_uid: "swe-2-max-resolved".into(),
        });
        let mut req = AiRequest::new(
            "swe-2",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: "swe-2",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let body = outbound.body_bytes.expect("binary body");
        let proto = String::from_utf8_lossy(&body[5..]);
        assert!(proto.contains("swe-2-max-resolved"));
        assert!(proto.contains("jwt-xyz"));
        assert!(!proto.contains("swe-2-max\""));

        // A second request on the same (uid, cascade) reuses the cache —
        // the hook is not consulted again.
        let calls = ASSIGN_MODEL_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let mut req2 = AiRequest::new(
            "swe-2",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        DevinVendor.build_request(&mut req2, &ctx).await.unwrap();
        assert_eq!(
            ASSIGN_MODEL_CALLS.load(std::sync::atomic::Ordering::Relaxed),
            calls
        );
    }

    /// Known non-router: the catalog says this uid needs no assignment, so
    /// AssignModel is not called at all.
    #[tokio::test]
    async fn build_request_skips_assign_for_known_non_router() {
        let _guard = reset_assign_state().await;
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let provider = gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "devin".into(),
                vendor: Some("devin".into()),
                protocol: "devin-connect".into(),
                base_url: DEFAULT_BASE_URL.into(),
                preset_key: Some("devin".into()),
                channel: Some("devin".into()),
                models_source: None,
                static_models: None,
                api_key: "session-token".into(),
                adapter_credentials: r#"{"apiKey":"session-token"}"#.into(),
                vendor_options: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await
            .unwrap();
        let mut metadata = crate::provider_models::ProviderModelMetadata::bare("swe-1-7");
        metadata.extensions.insert(
            selector::SELECTOR_EXTENSION_KEY.to_string(),
            selector::table_extension_value(&selector::SelectorTable {
                default: "swe-1-7".into(),
                selectors: vec!["swe-1-7".into()],
                routers: Some(Vec::new()),
            }),
        );
        gw.storage
            .provider_models()
            .create(crate::provider_models::NewProviderModelRecord {
                provider_id: provider.id.clone(),
                model_id: "swe-1-7".into(),
                source_kind: crate::provider_models::ProviderModelSourceKind::Discovered,
                metadata_source_provider_id: None,
                presence: crate::provider_models::ProviderModelPresence::Present,
                selection_policy: crate::provider_models::ProviderModelSelectionPolicy::Auto,
                metadata,
            })
            .await
            .unwrap();
        let mut req = AiRequest::new(
            "swe-1-7",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: "swe-1-7",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        assert_eq!(
            ASSIGN_MODEL_CALLS.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(req.model, "swe-1-7");
    }

    /// Known router + failed/empty assignment → the build fails with the
    /// real cause rather than emitting a bare-uid request guaranteed to hit
    /// the canned `unavailable` trailer.
    #[tokio::test]
    async fn build_request_fails_fast_when_router_assignment_fails() {
        let _guard = reset_assign_state().await;
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let provider = gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "devin".into(),
                vendor: Some("devin".into()),
                protocol: "devin-connect".into(),
                base_url: DEFAULT_BASE_URL.into(),
                preset_key: Some("devin".into()),
                channel: Some("devin".into()),
                models_source: None,
                static_models: None,
                api_key: "session-token".into(),
                adapter_credentials: r#"{"apiKey":"session-token"}"#.into(),
                vendor_options: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await
            .unwrap();
        let mut metadata = crate::provider_models::ProviderModelMetadata::bare("swe-2");
        metadata.extensions.insert(
            selector::SELECTOR_EXTENSION_KEY.to_string(),
            selector::table_extension_value(&selector::SelectorTable {
                default: "swe-2-max".into(),
                selectors: vec!["swe-2-max".into()],
                routers: Some(vec!["swe-2-max".into()]),
            }),
        );
        gw.storage
            .provider_models()
            .create(crate::provider_models::NewProviderModelRecord {
                provider_id: provider.id.clone(),
                model_id: "swe-2".into(),
                source_kind: crate::provider_models::ProviderModelSourceKind::Discovered,
                metadata_source_provider_id: None,
                presence: crate::provider_models::ProviderModelPresence::Present,
                selection_policy: crate::provider_models::ProviderModelSelectionPolicy::Auto,
                metadata,
            })
            .await
            .unwrap();
        // Hook returns None → assignment resolution failed upstream.
        let mut req = AiRequest::new(
            "swe-2",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: "swe-2",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let error = DevinVendor.build_request(&mut req, &ctx).await.unwrap_err();
        assert!(error.message().contains("AssignModel"));
    }

    #[test]
    fn family_metadata_exports_levels_as_effort_options() {
        let entry = |selector: &str| crate::protocol::codec::devin_connect::DevinModelConfig {
            selector: selector.into(),
            label: Some("Claude Sonnet 5 Medium".into()),
            supports_images: Some(true),
            provider: Some(3),
            context_window: Some(400_000),
            alias: Some("claude-sonnet-5".into()),
            short_alias: None,
            is_router: false,
            cost: None,
        };
        let entries = vec![
            entry("claude-sonnet-5-low"),
            entry("claude-sonnet-5-medium"),
            entry("claude-sonnet-5-high"),
            entry("claude-sonnet-5-high-fast"),
        ];
        let selectors: Vec<String> = entries.iter().map(|e| e.selector.clone()).collect();
        let families = family::group_families(&selectors, &entries);
        assert_eq!(families.len(), 1);
        let metadata = family::family_metadata(&families[0], None);

        assert_eq!(metadata.id.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            metadata.modalities.as_ref().map(|m| m.input.as_slice()),
            Some(&["text".to_string(), "image".to_string()][..])
        );
        assert_eq!(
            metadata.limit.as_ref().and_then(|l| l.context),
            Some(400_000)
        );
        assert_eq!(
            metadata.extensions.get("devin_provider"),
            Some(&Value::String("anthropic".into()))
        );
        let Some([crate::provider_models::ReasoningOption::Effort { values }]) =
            metadata.reasoning_options.as_deref()
        else {
            panic!("expected effort reasoning options");
        };
        assert_eq!(
            values,
            &[
                Some("low".into()),
                Some("medium".into()),
                Some("high".into())
            ]
        );
        let map = crate::thinking::generate_thinking_level_map(&metadata);
        assert_eq!(
            crate::thinking::visible_levels(&map),
            vec![
                stravia_runtime_contract::thinking::ThinkingLevel::Low,
                stravia_runtime_contract::thinking::ThinkingLevel::Medium,
                stravia_runtime_contract::thinking::ThinkingLevel::High,
            ]
        );
        // The family's selector table resolves level requests inside the
        // vendor — medium is the canonical default.
        let table = selector::table_from_extensions(&metadata.extensions).expect("selector table");
        assert_eq!(table.default, "claude-sonnet-5-medium");
        assert_eq!(
            selector::resolve_selector(
                &table,
                Some(&TargetThinkingControl::Effort {
                    value: "high".into()
                })
            ),
            "claude-sonnet-5-high"
        );

        // A family with no level suffixes hides the picker instead of
        // advertising selectors upstream does not have.
        let families = family::group_families(&["swe-1-7".to_string()], &[]);
        let metadata = family::family_metadata(&families[0], None);
        assert!(
            crate::thinking::visible_levels(&crate::thinking::generate_thinking_level_map(
                &metadata
            ))
            .is_empty()
        );
    }

    #[test]
    fn map_error_reads_connect_error_payload() {
        let error = DevinVendor.map_error(
            429,
            serde_json::json!({"code": "resource_exhausted", "message": "quota"}),
        );
        assert_eq!(error.http_status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    }

    fn live_token() -> String {
        std::env::var("STRAVIA_DEVIN_LIVE_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                // <crate>/../../../.scratch/devin-token.txt
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../.scratch/devin-token.txt");
                std::fs::read_to_string(path)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
            .expect(
                "set STRAVIA_DEVIN_LIVE_TOKEN or write .scratch/devin-token.txt \
                 (the `devin-session-token` cookie value from app.devin.ai)",
            )
    }

    /// Live upstream verification — ignored by default. Provide the session
    /// token via `STRAVIA_DEVIN_LIVE_TOKEN` or `<repo>/.scratch/devin-token.txt`
    /// (gitignored); the model selector can be overridden with
    /// `STRAVIA_DEVIN_LIVE_MODEL` (defaults to `swe-1-7`, which does not
    /// consume the weekly quota bucket on Pro per the reference probes).
    ///
    /// Run: `cargo test -p stravia-core devin::tests::live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "calls the production Devin Connect endpoint"]
    async fn live_assign_model() {
        let token = live_token();
        let provider = Provider {
            api_key: token.clone(),
            auth_mode: "api_key".into(),
            ..test_provider()
        };
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: "swe-2-max",
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let assignment = assign_model_http(&ctx, &token, "swe-2-max", &Uuid::new_v4().to_string())
            .await
            .expect("AssignModel should succeed")
            .expect("AssignModel should resolve swe-2-max");
        assert!(!assignment.jwt.is_empty());
        assert!(!assignment.model_uid.is_empty());
        eprintln!(
            "AssignModel swe-2-max -> model_uid={} jwt_len={}",
            assignment.model_uid,
            assignment.jwt.len()
        );
    }

    /// Live upstream verification — ignored by default. Provide the session
    /// token via `STRAVIA_DEVIN_LIVE_TOKEN` or `<repo>/.scratch/devin-token.txt`
    /// (gitignored); the model selector can be overridden with
    /// `STRAVIA_DEVIN_LIVE_MODEL` (defaults to `swe-1-7`, which does not
    /// consume the weekly quota bucket on Pro per the reference probes).
    ///
    /// Run: `cargo test -p stravia-core devin::tests::live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "calls the production Devin Connect endpoint"]
    async fn live_get_chat_message_round_trip() {
        use futures::StreamExt;
        use stravia_runtime_contract::protocol::ir::AiStreamDelta;

        let token = live_token();
        let model = std::env::var("STRAVIA_DEVIN_LIVE_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "swe-1-7".to_string());

        let provider = Provider {
            api_key: token,
            auth_mode: "api_key".into(),
            ..test_provider()
        };
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut req = AiRequest::new(
            "ignored",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("Reply with exactly: connect-ok".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: &model,
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let body = outbound.body_bytes.expect("binary body");
        assert_eq!(body[0], 0, "uncompressed request envelope flag");

        let client = reqwest::Client::new();
        let response = client
            .post(&outbound.url)
            .headers(outbound.headers)
            .body(body)
            .send()
            .await
            .expect("connect to server.codeium.com");
        let status = response.status();
        assert!(
            status.is_success(),
            "upstream rejected the request: HTTP {status} {:?}",
            response.text().await.unwrap_or_default()
        );

        let mut parser = crate::protocol::codec::devin_connect::DevinConnectStreamParser::new();
        let mut deltas = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("stream chunk");
            deltas.extend(parser.parse_chunk(&chunk).expect("parse chunk"));
        }
        deltas.extend(parser.finish().expect("finish stream"));

        let mut text = String::new();
        let mut thinking = String::new();
        let mut saw_done = false;
        let mut usage = None;
        for delta in &deltas {
            match delta {
                AiStreamDelta::TextDelta(delta) => text.push_str(delta),
                AiStreamDelta::ThinkingDelta(delta) => thinking.push_str(delta),
                AiStreamDelta::Done { .. } => saw_done = true,
                AiStreamDelta::Usage(u) => usage = Some(u.clone()),
                _ => {}
            }
        }
        eprintln!(
            "live devin connect: model={model} deltas={} text_len={} thinking_len={} usage={usage:?}",
            deltas.len(),
            text.len(),
            thinking.len()
        );
        assert!(
            !text.is_empty() || !thinking.is_empty(),
            "no content deltas; first deltas: {:?}",
            &deltas[..deltas.len().min(5)]
        );
        assert!(saw_done, "stream ended without Done");
        assert!(usage.is_some(), "no usage metadata frame");
    }

    /// Live catalog probe — `GetCliModelConfigs` is the zero-billable unary
    /// RPC behind model sync. Prints the selectors the account can see.
    #[tokio::test]
    #[ignore = "calls the production Devin Connect endpoint"]
    async fn live_get_cli_model_configs() {
        let token = std::env::var("STRAVIA_DEVIN_LIVE_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../.scratch/devin-token.txt");
                std::fs::read_to_string(path)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
            .expect("session token (see live_get_chat_message_round_trip)");

        let client = reqwest::Client::new();
        let response = client
            .post(format!(
                "{DEFAULT_BASE_URL}/exa.api_server_pb.ApiServerService/GetCliModelConfigs"
            ))
            .header("accept", "*/*")
            .header("content-type", "application/proto")
            .header("connect-protocol-version", "1")
            .header("authorization", format!("Basic {token}-{token}"))
            .body(
                crate::protocol::codec::devin_connect::encode_client_metadata_request(&token, true),
            )
            .send()
            .await
            .expect("connect to server.codeium.com");
        assert!(response.status().is_success(), "HTTP {}", response.status());
        let body = response.bytes().await.expect("body");

        let entries = crate::protocol::codec::devin_connect::decode_cli_model_configs(&body);
        eprintln!("live devin catalog: {} entries", entries.len());
        for entry in &entries {
            eprintln!(
                "  {} | label={:?} images={:?} provider={:?} ctx={:?} alias={:?}",
                entry.selector,
                entry.label,
                entry.supports_images,
                entry.provider,
                entry.context_window,
                entry.alias
            );
        }
        assert!(!entries.is_empty(), "catalog returned no selectors");
    }

    /// Live tool-call round trip — the model must emit a native tool call
    /// whose argument fragments merge into valid JSON. Same token source as
    /// the round-trip test.
    #[tokio::test]
    #[ignore = "calls the production Devin Connect endpoint"]
    async fn live_get_chat_message_tool_call() {
        use futures::StreamExt;
        use stravia_runtime_contract::protocol::ir::AiStreamDelta;
        use stravia_runtime_contract::protocol::ir::ToolSpec;

        let token = std::env::var("STRAVIA_DEVIN_LIVE_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../.scratch/devin-token.txt");
                std::fs::read_to_string(path)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
            .expect("session token (see live_get_chat_message_round_trip)");
        let model = std::env::var("STRAVIA_DEVIN_LIVE_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "swe-1-7".to_string());

        let provider = Provider {
            api_key: token,
            auth_mode: "api_key".into(),
            ..test_provider()
        };
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut req = AiRequest::new(
            "ignored",
            vec![
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("Say hello.".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Text("Hello!".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text(
                        "What is the weather in Paris? You MUST call get_weather.".into(),
                    ),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );
        // The completed exchange above makes this a pinned-session turn-2
        // request: #15.1/#16 derive from the conversation root, #15.2 = 2.
        req.tools = Some(vec![ToolSpec {
            name: "get_weather".into(),
            description: Some("Get the current weather for a city".into()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name"}
                },
                "required": ["city"]
            }),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: &model,
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(&outbound.url)
            .headers(outbound.headers)
            .body(outbound.body_bytes.expect("binary body"))
            .send()
            .await
            .expect("connect to server.codeium.com");
        let status = response.status();
        assert!(
            status.is_success(),
            "upstream rejected the tool request: HTTP {status} {:?}",
            response.text().await.unwrap_or_default()
        );

        let mut parser = crate::protocol::codec::devin_connect::DevinConnectStreamParser::new();
        let mut deltas = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            deltas.extend(
                parser
                    .parse_chunk(&chunk.expect("stream chunk"))
                    .expect("parse chunk"),
            );
        }
        deltas.extend(parser.finish().expect("finish stream"));

        let starts: Vec<_> = deltas
            .iter()
            .filter_map(|d| match d {
                AiStreamDelta::ToolCallStart { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        let completes: Vec<_> = deltas
            .iter()
            .filter_map(|d| match d {
                AiStreamDelta::ToolCallComplete { tool_call, .. } => Some(tool_call.clone()),
                _ => None,
            })
            .collect();
        eprintln!(
            "live devin tools: model={model} deltas={} starts={starts:?} completes={completes:?}",
            deltas.len()
        );
        assert_eq!(starts.len(), 1, "expected exactly one tool call start");
        assert_eq!(starts[0].1, "get_weather");
        assert_eq!(completes.len(), 1);
        let args: Value = serde_json::from_str(&completes[0].arguments)
            .expect("merged tool arguments must be valid JSON");
        assert!(
            args.get("city")
                .and_then(Value::as_str)
                .is_some_and(|c| { c.to_ascii_lowercase().contains("paris") }),
            "unexpected tool arguments: {args}"
        );
    }

    /// Live image round trip — an inline base64 PNG rides ChatMessage #10
    /// ImageData{#1 b64, #2 mime}; the model must actually see the pixels.
    /// Defaults to `claude-sonnet-5-medium` (verified multimodal selector).
    #[tokio::test]
    #[ignore = "calls the production Devin Connect endpoint"]
    async fn live_get_chat_message_image() {
        use futures::StreamExt;
        use stravia_runtime_contract::protocol::ir::AiStreamDelta;
        use stravia_runtime_contract::protocol::ir::ContentBlock;
        use stravia_runtime_contract::protocol::ir::MediaSource;

        let token = std::env::var("STRAVIA_DEVIN_LIVE_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../.scratch/devin-token.txt");
                std::fs::read_to_string(path)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
            .expect("session token (see live_get_chat_message_round_trip)");
        let model = std::env::var("STRAVIA_DEVIN_LIVE_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "claude-sonnet-5-medium".to_string());

        // 8x8 solid-red PNG (75 bytes).
        const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEklEQVR4nGP4z8CAFWEXHbQSACj/P8Fu7N9hAAAAAElFTkSuQmCC";
        let provider = Provider {
            api_key: token,
            auth_mode: "api_key".into(),
            ..test_provider()
        };
        let gw = crate::Gateway::new(crate::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("stravia-devin-{}", Uuid::new_v4())),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut req = AiRequest::new(
            "ignored",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Image {
                        source: MediaSource::Base64 {
                            media_type: "image/png".into(),
                            data: RED_PNG_B64.into(),
                        },
                        detail: None,
                        cache_control: None,
                    },
                    ContentBlock::Text {
                        text: "What color is this image? Answer with one word.".into(),
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: DEVIN_CONNECT_GET_CHAT_MESSAGE_V1,
            egress_base_url: DEFAULT_BASE_URL,
            api_key: &provider.api_key,
            actual_model: &model,
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let outbound = DevinVendor.build_request(&mut req, &ctx).await.unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(&outbound.url)
            .headers(outbound.headers)
            .body(outbound.body_bytes.expect("binary body"))
            .send()
            .await
            .expect("connect to server.codeium.com");
        let status = response.status();
        assert!(
            status.is_success(),
            "upstream rejected the image request: HTTP {status} {:?}",
            response.text().await.unwrap_or_default()
        );

        let mut parser = crate::protocol::codec::devin_connect::DevinConnectStreamParser::new();
        let mut deltas = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            deltas.extend(
                parser
                    .parse_chunk(&chunk.expect("stream chunk"))
                    .expect("parse chunk"),
            );
        }
        deltas.extend(parser.finish().expect("finish stream"));

        let mut text = String::new();
        for delta in &deltas {
            if let AiStreamDelta::TextDelta(delta) = delta {
                text.push_str(delta);
            }
        }
        eprintln!("live devin image: model={model} answer={text:?}");
        assert!(
            text.to_ascii_lowercase().contains("red"),
            "model did not identify the red image; answer={text:?}"
        );
    }
}
