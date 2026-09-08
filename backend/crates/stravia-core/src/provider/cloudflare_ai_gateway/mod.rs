//! Cloudflare AI Gateway's unified OpenAI-compatible `/compat` surface.

use std::collections::BTreeMap;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::{openai_compat::openai_map_error, pipeline};
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, CredentialFieldDef, CredentialInputKind, Label,
    VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor, resolve_base_url};

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "apiToken",
        label: "AI Gateway API token",
        secret: true,
        required: true,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "accountId",
        label: "Cloudflare account ID",
        secret: false,
        required: true,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "gatewayId",
        label: "Gateway ID",
        secret: false,
        required: true,
        input: CredentialInputKind::Text,
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "cloudflare-ai-gateway",
    label: Label {
        zh: "Cloudflare AI Gateway",
        en: "Cloudflare AI Gateway",
    },
    icon: "cloudflare",
    default_protocol: "openai-compatible",
    credential_fields: CREDENTIAL_FIELDS,
    channels: &[ChannelDef {
        id: "default",
        label: Label {
            zh: "默认",
            en: "Default",
        },
        base_urls: &[],
        api_key: None,
        models_source: None,
        capabilities_source: CapabilitiesSource::Auto,
        static_models: &[],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
    }],
};

pub struct CloudflareAiGatewayVendor;

#[async_trait]
impl Vendor for CloudflareAiGatewayVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "cloudflare-ai-gateway",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }

    fn assemble_base_url(
        &self,
        credentials: &BTreeMap<String, String>,
        configured_base_url: Option<&str>,
    ) -> anyhow::Result<String> {
        resolve_base_url(self.vendor_id(), configured_base_url, || {
            cloudflare_gateway_base_url(
                credentials
                    .get("accountId")
                    .map(String::as_str)
                    .unwrap_or(""),
                credentials
                    .get("gatewayId")
                    .map(String::as_str)
                    .unwrap_or(""),
            )
        })
    }

    fn construct_request(
        &self,
        ctx: &crate::provider::vendor_ext::RequestContext<'_>,
        purpose: crate::provider::vendor_ext::RequestPurpose<'_>,
    ) -> anyhow::Result<crate::provider::vendor_ext::ConstructedRequest> {
        use crate::provider::vendor_ext::{ConstructedRequest, RequestPurpose};
        let url = match purpose {
            RequestPurpose::Models { endpoint } => endpoint.to_string(),
            RequestPurpose::Inference {
                base_url,
                path: _,
                protocol: _,
                actual_model: _,
            } => base_url.trim_end_matches('/').into(),
        };
        let headers = if ctx.disable_default_auth {
            reqwest::header::HeaderMap::new()
        } else {
            let mut headers = HeaderMap::new();
            if let Some(token) = ctx.provider.adapter_credential("apiToken")
                && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
            {
                headers.insert("cf-aig-authorization", value);
            }
            headers
        };
        ConstructedRequest::new(ctx, purpose, url, headers)
    }

    fn vendor_id(&self) -> &'static str {
        "cloudflare-ai-gateway"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
    }

    async fn build_request(
        &self,
        request: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        pipeline::build_request(self, request, ctx).await
    }

    async fn parse_response(
        &self,
        response: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, response, ctx).await
    }

    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        openai_map_error("cloudflare-ai-gateway", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(CloudflareAiGatewayVendor) } }

pub fn cloudflare_gateway_base_url(account_id: &str, gateway_id: &str) -> String {
    let account_id = account_id.trim();
    let gateway_id = gateway_id.trim();
    if account_id.is_empty() || gateway_id.is_empty() {
        String::new()
    } else {
        format!("https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn vendor_assembles_unified_compat_endpoint() {
        let credentials = BTreeMap::from([
            ("apiToken".to_string(), "token".to_string()),
            ("accountId".to_string(), "account_1".to_string()),
            ("gatewayId".to_string(), "gateway_1".to_string()),
        ]);
        assert_eq!(
            CloudflareAiGatewayVendor
                .assemble_base_url(&credentials, None)
                .unwrap(),
            "https://gateway.ai.cloudflare.com/v1/account_1/gateway_1/compat"
        );
    }
}
