//! xAI vendor — direct API plus the Grok Build OAuth channel.

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
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

const METADATA: VendorMetadata = VendorMetadata {
    id: "xai",
    label: Label {
        zh: "xAI",
        en: "xAI",
    },
    icon: "xai",
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
                base_url: "https://api.x.ai/v1",
            }],
            api_key: None,
            models_source: Some("https://api.x.ai/v1/models"),
            capabilities_source: CapabilitiesSource::Catalog("xai"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
        },
        ChannelDef {
            id: "grok",
            label: Label {
                zh: "Grok OAuth",
                en: "Grok OAuth",
            },
            base_urls: &[ProtocolBaseUrl {
                protocol: "open-responses",
                base_url: "https://cli-chat-proxy.grok.com/v1",
            }],
            api_key: None,
            models_source: None,
            capabilities_source: CapabilitiesSource::Catalog("xai"),
            static_models: &[],
            auth_mode: AuthMode::OAuth,
            oauth: Some(OAuthConfig {
                auth_base_url: "https://auth.x.ai",
                authorize_url: "https://auth.x.ai/oauth2/authorize",
                token_url: "https://auth.x.ai/oauth2/token",
                client_id: "b1a00492-073a-47ea-816f-4c329264a828",
                redirect_uri: "",
                scope: "openid profile email offline_access grok-cli:access api:access",
            }),
            runtime: Some(RuntimeConfig {
                api_base_url: "https://cli-chat-proxy.grok.com/v1",
                models_url: "https://api.x.ai/v1/models",
                models_client_version: "0.2.120",
            }),
        },
    ],
};

pub struct XaiVendor;

#[async_trait]
impl Vendor for XaiVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor { vendor_id: "xai" }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        openai_build_url(base_url, path)
    }
    fn vendor_id(&self) -> &'static str {
        "xai"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        };
        &[
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
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
        openai_map_error("xai", status, body)
    }

    fn is_continuation_not_found(&self, status: u16, body: &Value) -> bool {
        if matches!(status, 400 | 404)
            && body
                .pointer("/error/code")
                .or_else(|| body.get("code"))
                .and_then(Value::as_str)
                == Some("previous_response_not_found")
        {
            return true;
        }
        status == 404
            && body.get("code").and_then(Value::as_str) == Some("not-found")
            && body
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|message| {
                    message.starts_with("Previous response cannot be used")
                        && message.contains("due to Zero Data Retention")
                })
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(XaiVendor) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_channel_uses_oauth_responses_upstream() {
        let channel = METADATA
            .channels
            .iter()
            .find(|channel| channel.id == "grok")
            .expect("Grok OAuth channel");
        assert_eq!(channel.auth_mode, AuthMode::OAuth);
        assert_eq!(
            channel
                .base_urls
                .iter()
                .find(|base| base.protocol == "open-responses")
                .map(|base| base.base_url),
            Some("https://cli-chat-proxy.grok.com/v1")
        );
        let provider = crate::db::models::Provider {
            id: String::new(),
            name: String::new(),
            vendor: Some("xai".to_string()),
            protocol: "open-responses".to_string(),
            base_url: "https://cli-chat-proxy.grok.com/v1".to_string(),
            preset_key: Some("xai".to_string()),
            channel: Some("grok".to_string()),
            models_source: Some("catalog".to_string()),
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".to_string(),
            auth_mode: "oauth".to_string(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let ctx = VendorCtx {
            provider: &provider,
            protocol_id: crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            api_key: "",
            actual_model: "",
            credential: None,
        };
        assert_eq!(
            XaiVendor.build_url(&ctx, "https://cli-chat-proxy.grok.com/v1", "/v1/responses",),
            "https://cli-chat-proxy.grok.com/v1/responses"
        );
    }
}
