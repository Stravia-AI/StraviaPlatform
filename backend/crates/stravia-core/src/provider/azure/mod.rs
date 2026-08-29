//! Azure OpenAI adapter: resource-scoped URL plus `api-key` authentication.

use std::collections::BTreeMap;

use async_trait::async_trait;
use reqwest::{
    Url,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1, ProtocolId,
};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::{
    openai_compat::{openai_build_url, openai_map_error},
    pipeline,
};
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, CredentialFieldDef, CredentialInputKind, Label,
    VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor, resolve_base_url};
use crate::provider::vendor_ext::VendorCtx;

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "resourceName",
        label: "Azure resource name",
        secret: false,
        required: true,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "apiKey",
        label: "API key",
        secret: true,
        required: true,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "apiVersion",
        label: "API version",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "azure",
    label: Label {
        zh: "Azure OpenAI",
        en: "Azure OpenAI",
    },
    icon: "azure",
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

pub struct AzureVendor;

#[async_trait]
impl Vendor for AzureVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor { vendor_id: "azure" }
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
            azure_base_url(
                credentials
                    .get("resourceName")
                    .map(String::as_str)
                    .unwrap_or(""),
            )
        })
    }

    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(ctx.api_key.trim()) {
            headers.insert("api-key", value);
        }
        headers
    }

    fn build_url(&self, ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        azure_request_url(base_url, path, &azure_api_version(ctx.provider))
    }

    fn vendor_id(&self) -> &'static str {
        "azure"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
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
        response: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, response, ctx).await
    }

    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        openai_map_error("azure", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(AzureVendor) } }

pub fn azure_base_url(resource_name: &str) -> String {
    let resource_name = resource_name.trim();
    if resource_name.is_empty() {
        String::new()
    } else {
        format!("https://{resource_name}.openai.azure.com/openai/v1")
    }
}

fn azure_api_version(provider: &crate::db::models::Provider) -> String {
    provider
        .adapter_credential("apiVersion")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "v1".into())
}

fn azure_request_url(base_url: &str, path: &str, api_version: &str) -> String {
    let url = openai_build_url(base_url, path);
    let Ok(mut url) = Url::parse(&url) else {
        return url;
    };
    url.query_pairs_mut()
        .append_pair("api-version", api_version);
    url.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_resource_scoped_url() {
        assert_eq!(
            azure_base_url("my-resource"),
            "https://my-resource.openai.azure.com/openai/v1"
        );
    }

    #[test]
    fn appends_the_configured_api_version_to_chat_requests() {
        assert_eq!(
            azure_request_url(
                "https://my-resource.openai.azure.com/openai/v1",
                "/v1/chat/completions",
                "2025-04-01-preview",
            ),
            "https://my-resource.openai.azure.com/openai/v1/chat/completions?api-version=2025-04-01-preview"
        );
    }

    #[test]
    fn defaults_api_version_to_v1() {
        assert_eq!(
            azure_request_url(
                "https://my-resource.openai.azure.com/openai/v1",
                "/v1/chat/completions",
                "v1",
            ),
            "https://my-resource.openai.azure.com/openai/v1/chat/completions?api-version=v1"
        );
    }
}
