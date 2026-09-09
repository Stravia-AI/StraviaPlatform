//! Custom vendor preset — user-defined "Bring Your Own Endpoint".

use async_trait::async_trait;

use serde_json::Value;

use crate::error::GatewayError;
use crate::provider::common::openai_compat::{GenericOpenAICompatibleAdapter, openai_map_error};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{AuthMode, CapabilitiesSource, ChannelDef, Label, VendorMetadata};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;

const METADATA: VendorMetadata = VendorMetadata {
    id: "custom",
    label: Label {
        zh: "自定义",
        en: "Custom",
    },
    icon: "custom",
    default_protocol: "openai-compatible",
    credential_fields: crate::provider::metadata::API_KEY_CREDENTIAL_FIELDS,
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

pub struct CustomVendor;

#[async_trait]
impl Vendor for CustomVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "custom",
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
        GenericOpenAICompatibleAdapter.construct_request(ctx, purpose)
    }
    fn vendor_id(&self) -> &'static str {
        "custom"
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
        openai_map_error("custom", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(CustomVendor) } }
