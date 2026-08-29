//! SAP AI Core Foundation Models deployment adapter.
//!
//! The npm package delegates authentication to an SAP Cloud SDK destination.
//! A locally persisted provider cannot execute that runtime hook, so it stores
//! the corresponding service-key values explicitly and obtains an OAuth token
//! before sending the documented OpenAI-compatible deployment request.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::header::{AUTHORIZATION, HeaderValue};
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
use crate::provider::vendor_ext::VendorCtx;

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "deploymentUrl",
        label: "Deployment URL",
        secret: false,
        required: true,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "tokenUrl",
        label: "OAuth token URL",
        secret: false,
        required: true,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "clientId",
        label: "OAuth client ID",
        secret: true,
        required: true,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "clientSecret",
        label: "OAuth client secret",
        secret: true,
        required: true,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "resourceGroup",
        label: "Resource group",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "sap-ai-core",
    label: Label {
        zh: "SAP AI Core",
        en: "SAP AI Core",
    },
    icon: "sap",
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

#[derive(Clone)]
struct OAuthToken {
    value: String,
    expires_at: Instant,
}

static TOKENS: LazyLock<DashMap<String, OAuthToken>> = LazyLock::new(DashMap::new);

pub struct SapAiCoreVendor;

#[async_trait]
impl Vendor for SapAiCoreVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "sap-ai-core",
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
            credentials
                .get("deploymentUrl")
                .map(String::as_str)
                .unwrap_or("")
                .trim_end_matches('/')
                .to_string()
        })
    }
    fn build_url(&self, ctx: &VendorCtx<'_>, _base_url: &str, path: &str) -> String {
        let path = path.strip_prefix("/v1").unwrap_or(path);
        format!("{}{}", ctx.provider.base_url.trim_end_matches('/'), path)
    }
    fn vendor_id(&self) -> &'static str {
        "sap-ai-core"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
    }

    async fn build_request(
        &self,
        request: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        let mut outbound = pipeline::build_request(self, request, ctx).await?;
        let token = oauth_token(ctx).await.map_err(|error| {
            GatewayError::provider_unavailable("sap-ai-core", error.to_string())
        })?;
        outbound.headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|error| GatewayError::internal(error.into()))?,
        );
        if let Some(resource_group) = credential(ctx.provider, "resourceGroup") {
            outbound.headers.insert(
                "AI-Resource-Group",
                HeaderValue::from_str(&resource_group)
                    .map_err(|error| GatewayError::internal(error.into()))?,
            );
        }
        Ok(outbound)
    }

    async fn parse_response(
        &self,
        response: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, response, ctx).await
    }

    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        openai_map_error("sap-ai-core", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(SapAiCoreVendor) } }

fn credential(provider: &crate::db::models::Provider, key: &str) -> Option<String> {
    provider
        .adapter_credential(key)
        .filter(|value| !value.trim().is_empty())
}

async fn oauth_token(ctx: &ProviderCtx<'_>) -> anyhow::Result<String> {
    let token_url = credential(ctx.provider, "tokenUrl")
        .ok_or_else(|| anyhow::anyhow!("SAP OAuth token URL is required"))?;
    let client_id = credential(ctx.provider, "clientId")
        .ok_or_else(|| anyhow::anyhow!("SAP OAuth client ID is required"))?;
    let client_secret = credential(ctx.provider, "clientSecret")
        .ok_or_else(|| anyhow::anyhow!("SAP OAuth client secret is required"))?;
    let cache_key = format!("{token_url}\n{client_id}\n{client_secret}");
    if let Some(entry) = TOKENS.get(&cache_key)
        && entry.expires_at > Instant::now() + Duration::from_secs(300)
    {
        return Ok(entry.value.clone());
    }

    let client = ctx
        .gw
        .http_client_for_provider(ctx.provider.use_proxy)
        .await?;
    let payload: Value = client
        .post(token_url)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let value = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("SAP OAuth response is missing access_token"))?
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    TOKENS.insert(
        cache_key,
        OAuthToken {
            value: value.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        },
    );
    Ok(value)
}
