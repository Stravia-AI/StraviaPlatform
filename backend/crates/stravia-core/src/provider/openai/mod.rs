//! OpenAI vendor — direct API plus the Codex channel (OAuth via ChatGPT).

pub mod codex;

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::openai_compat::{
    openai_bearer_auth_headers, openai_build_url, openai_map_error,
};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, OAuthConfig, ProtocolBaseUrl, RuntimeConfig,
    VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{ExtensionRegistration, VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::{
    ResolvedTargetCapabilities, ResponsesWebSocketConnectionMetadata, VendorCtx, VendorExtension,
};

const METADATA: VendorMetadata = VendorMetadata {
    id: "openai",
    label: Label {
        zh: "OpenAI",
        en: "OpenAI",
    },
    icon: "openai",
    default_protocol: "openai-compatible",
    credential_fields: crate::provider::metadata::API_KEY_CREDENTIAL_FIELDS,
    channels: &[
        ChannelDef {
            id: "default",
            label: Label {
                zh: "默认",
                en: "Default",
            },
            base_urls: &[ProtocolBaseUrl {
                protocol: "openai-compatible",
                base_url: "https://api.openai.com/v1",
            }],
            api_key: None,
            models_source: Some("https://api.openai.com/v1/models"),
            capabilities_source: CapabilitiesSource::Catalog("openai"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
        },
        ChannelDef {
            id: "codex",
            label: Label {
                zh: "Codex",
                en: "Codex",
            },
            base_urls: &[ProtocolBaseUrl {
                protocol: "open-responses",
                base_url: "https://chatgpt.com/backend-api/codex",
            }],
            api_key: None,
            models_source: Some("https://chatgpt.com/backend-api/codex/models"),
            capabilities_source: CapabilitiesSource::Catalog("openai"),
            static_models: &[],
            auth_mode: AuthMode::OAuth,
            oauth: Some(OAuthConfig {
                auth_base_url: "https://auth.openai.com",
                authorize_url: "https://auth.openai.com/oauth/authorize",
                token_url: "https://auth.openai.com/oauth/token",
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
                redirect_uri: "http://localhost:1455/auth/callback",
                scope: "openid profile email offline_access",
            }),
            runtime: Some(RuntimeConfig {
                api_base_url: "https://chatgpt.com/backend-api/codex",
                models_url: "https://chatgpt.com/backend-api/codex/models",
                models_client_version: "0.153.0",
            }),
        },
    ],
};
pub(super) fn openai_responses_websocket_request(body: &Value) -> anyhow::Result<Value> {
    let mut request = body.clone();
    let object = request
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenAI Responses request body must be a JSON object"))?;
    object.remove("stream");
    object.remove("background");
    object.insert("type".into(), Value::String("response.create".into()));
    Ok(request)
}
pub(super) fn normalize_openai_responses_websocket_event(event: &mut Value) -> anyhow::Result<()> {
    let object = event
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenAI Responses WebSocket event must be a JSON object"))?;
    if object.get("type").and_then(Value::as_str) != Some("response.done") {
        return Ok(());
    }
    let terminal_type = match object
        .get("response")
        .and_then(|response| response.get("status"))
        .and_then(Value::as_str)
    {
        Some("completed") => "response.completed",
        Some("incomplete") => "response.incomplete",
        Some("failed") => "response.failed",
        status => anyhow::bail!(
            "OpenAI Responses WebSocket response.done has invalid terminal status: {status:?}"
        ),
    };
    object.insert("type".into(), Value::String(terminal_type.into()));
    Ok(())
}

pub(super) fn retain_openai_responses_websocket_event(event: &Value) -> bool {
    !event
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|event_type| {
            event_type.starts_with("codex.") || event_type.starts_with("responsesapi.")
        })
}

pub struct OpenAiVendor;

#[async_trait]
impl Vendor for OpenAiVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "openai",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn target_capabilities(&self, protocol: ProtocolId) -> ResolvedTargetCapabilities {
        ResolvedTargetCapabilities {
            responses_websocket: protocol == crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            ..Default::default()
        }
    }
    fn responses_websocket_request(
        &self,
        _ctx: &VendorCtx<'_>,
        body: &Value,
        _connection: ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<Value> {
        openai_responses_websocket_request(body)
    }
    fn normalize_responses_websocket_event(
        &self,
        _ctx: &VendorCtx<'_>,
        event: &mut Value,
    ) -> anyhow::Result<()> {
        normalize_openai_responses_websocket_event(event)
    }
    fn retain_responses_websocket_event(&self, _ctx: &VendorCtx<'_>, event: &Value) -> bool {
        retain_openai_responses_websocket_event(event)
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        openai_build_url(base_url, path)
    }
    fn vendor_id(&self) -> &'static str {
        "openai"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_COMPATIBLE_EMBEDDINGS_V1,
        };
        &[
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
            OPENAI_COMPATIBLE_EMBEDDINGS_V1,
        ]
    }

    async fn build_request(
        &self,
        req: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        pipeline::build_request(self, req, ctx).await
    }
    async fn parse_response(
        &self,
        resp: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, resp, ctx).await
    }
    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        openai_map_error("openai", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(OpenAiVendor) } }

/// Family-level fallback for any provider whose `vendor` field is blank or unknown
/// but whose egress protocol belongs to the OpenAI family.
pub struct OpenAIFamilyExt;

impl VendorExtension for OpenAIFamilyExt {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "openai",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        None
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        openai_build_url(base_url, path)
    }
}

inventory::submit! { ExtensionRegistration { make: || Box::new(OpenAIFamilyExt) } }

#[cfg(test)]
mod tests {
    use super::{
        normalize_openai_responses_websocket_event, retain_openai_responses_websocket_event,
    };

    #[test]
    fn websocket_done_event_preserves_terminal_status() {
        for (status, event_type) in [
            ("completed", "response.completed"),
            ("incomplete", "response.incomplete"),
            ("failed", "response.failed"),
        ] {
            let mut event = serde_json::json!({
                "type": "response.done",
                "response": {"status": status}
            });

            normalize_openai_responses_websocket_event(&mut event).expect("known terminal status");

            assert_eq!(event["type"], event_type);
        }
    }

    #[test]
    fn websocket_done_event_rejects_non_terminal_status() {
        let mut event = serde_json::json!({
            "type": "response.done",
            "response": {"status": "in_progress"}
        });

        let error = normalize_openai_responses_websocket_event(&mut event)
            .expect_err("non-terminal response.done must fail");

        assert!(error.to_string().contains("terminal status"));
    }

    #[test]
    fn websocket_ignores_gateway_metadata_events() {
        for event_type in [
            "codex.rate_limits",
            "codex.response.metadata",
            "responsesapi.websocket_timing",
        ] {
            assert!(!retain_openai_responses_websocket_event(
                &serde_json::json!({"type": event_type})
            ));
        }
        assert!(retain_openai_responses_websocket_event(
            &serde_json::json!({"type": "response.completed"})
        ));
    }
}
