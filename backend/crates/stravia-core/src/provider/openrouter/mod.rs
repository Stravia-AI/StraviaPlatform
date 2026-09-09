//! OpenRouter vendor (OpenAI-compatible aggregator).

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use serde_json::Value;

use crate::error::GatewayError;
use crate::provider::common::openai_compat::openai_map_error;
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, CredentialFieldDef, CredentialInputKind, Label,
    ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "apiKey",
        label: "API key",
        secret: true,
        required: true,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "httpReferer",
        label: "App referer URL",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "xTitle",
        label: "App title",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "openrouter",
    label: Label {
        zh: "OpenRouter",
        en: "OpenRouter",
    },
    icon: "openrouter",
    default_protocol: "openai-compatible",
    credential_fields: CREDENTIAL_FIELDS,
    channels: &[ChannelDef {
        id: "default",
        label: Label {
            zh: "默认",
            en: "Default",
        },
        base_urls: &[
            ProtocolBaseUrl {
                protocol: "openai-compatible",
                base_url: "https://openrouter.ai/api/v1",
            },
            ProtocolBaseUrl {
                protocol: "anthropic-messages",
                base_url: "https://openrouter.ai/api",
            },
        ],
        api_key: None,
        models_source: Some("https://openrouter.ai/api/v1/models"),
        capabilities_source: CapabilitiesSource::Http("https://openrouter.ai/api/v1/models"),
        static_models: &[],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
    }],
};

pub struct OpenrouterVendor;

#[async_trait]
impl Vendor for OpenrouterVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "openrouter",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn construct_request(
        &self,
        ctx: &crate::provider::vendor_ext::RequestContext<'_>,
        purpose: crate::provider::vendor_ext::RequestPurpose<'_>,
    ) -> anyhow::Result<crate::provider::vendor_ext::ConstructedRequest> {
        let mut request =
            crate::provider::common::openai_compat::construct_openai_request(ctx, purpose)?;
        let headers = &mut request.headers;
        if let Some(value) = ctx.provider.adapter_credential("httpReferer")
            && let Ok(value) = HeaderValue::from_str(&value)
        {
            headers.insert("HTTP-Referer", value);
        }
        if let Some(value) = ctx.provider.adapter_credential("xTitle")
            && let Ok(value) = HeaderValue::from_str(&value)
        {
            headers.insert("X-Title", value);
        }
        Ok(request)
    }
    fn vendor_id(&self) -> &'static str {
        "openrouter"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
        &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
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
        openai_map_error("openrouter", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(OpenrouterVendor) } }
