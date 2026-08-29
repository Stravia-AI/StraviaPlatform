//! GitLab Duo adapter using the package's direct-access token exchange.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId};
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
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

const DEFAULT_INSTANCE_URL: &str = "https://gitlab.com";
const DEFAULT_GATEWAY_URL: &str = "https://cloud.gitlab.com";

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "apiKey",
        label: "GitLab access token",
        secret: true,
        required: true,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "instanceUrl",
        label: "GitLab instance URL",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "aiGatewayUrl",
        label: "GitLab AI Gateway URL",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "gitlab",
    label: Label {
        zh: "GitLab Duo",
        en: "GitLab Duo",
    },
    icon: "gitlab",
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
struct DirectAccessToken {
    token: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    expires_at: Instant,
}

static DIRECT_ACCESS_TOKENS: LazyLock<DashMap<String, DirectAccessToken>> =
    LazyLock::new(DashMap::new);

pub struct GitLabVendor;

#[async_trait]
impl Vendor for GitLabVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "gitlab",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn build_url(&self, ctx: &VendorCtx<'_>, _base_url: &str, path: &str) -> String {
        openai_build_url(&gitlab_openai_proxy_url(ctx.provider), path)
    }
    fn vendor_id(&self) -> &'static str {
        "gitlab"
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
        let access = direct_access_token(ctx)
            .await
            .map_err(|error| GatewayError::provider_unavailable("gitlab", error.to_string()))?;
        apply_direct_access_headers(&mut outbound.headers, &access)?;
        Ok(outbound)
    }

    async fn refresh_auth_on_unauthorized(
        &self,
        ctx: &ProviderCtx<'_>,
        outbound: &mut OutboundRequest,
    ) -> Result<bool, GatewayError> {
        let (previous, fresh) = refresh_direct_access_token(ctx)
            .await
            .map_err(|error| GatewayError::provider_unavailable("gitlab", error.to_string()))?;
        replace_direct_access_headers(&mut outbound.headers, previous.as_ref(), &fresh)?;
        Ok(true)
    }

    async fn parse_response(
        &self,
        response: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, response, ctx).await
    }
    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        openai_map_error("gitlab", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(GitLabVendor) } }

fn credential(provider: &crate::db::models::Provider, key: &str) -> Option<String> {
    provider
        .adapter_credential(key)
        .filter(|value| !value.trim().is_empty())
}

fn gitlab_openai_proxy_url(provider: &crate::db::models::Provider) -> String {
    credential(provider, "aiGatewayUrl")
        .unwrap_or_else(|| DEFAULT_GATEWAY_URL.into())
        .trim_end_matches('/')
        .to_string()
        + "/ai/v1/proxy/openai/v1"
}

async fn direct_access_token(ctx: &ProviderCtx<'_>) -> anyhow::Result<DirectAccessToken> {
    let (api_key, instance_url, cache_key) = direct_access_config(ctx);
    if let Some(entry) = DIRECT_ACCESS_TOKENS.get(&cache_key)
        && entry.expires_at > Instant::now() + Duration::from_secs(300)
    {
        return Ok(entry.clone());
    }
    fetch_direct_access_token(ctx, &api_key, &instance_url, &cache_key).await
}

async fn refresh_direct_access_token(
    ctx: &ProviderCtx<'_>,
) -> anyhow::Result<(Option<DirectAccessToken>, DirectAccessToken)> {
    let (api_key, instance_url, cache_key) = direct_access_config(ctx);
    let previous = DIRECT_ACCESS_TOKENS
        .remove(&cache_key)
        .map(|(_, entry)| entry);
    let fresh = fetch_direct_access_token(ctx, &api_key, &instance_url, &cache_key).await?;
    Ok((previous, fresh))
}

fn direct_access_config(ctx: &ProviderCtx<'_>) -> (String, String, String) {
    let api_key = credential(ctx.provider, "apiKey").unwrap_or_else(|| ctx.api_key.to_string());
    let instance_url =
        credential(ctx.provider, "instanceUrl").unwrap_or_else(|| DEFAULT_INSTANCE_URL.into());
    let cache_key = format!("{}\n{}", instance_url.trim_end_matches('/'), api_key);
    (api_key, instance_url, cache_key)
}

async fn fetch_direct_access_token(
    ctx: &ProviderCtx<'_>,
    api_key: &str,
    instance_url: &str,
    cache_key: &str,
) -> anyhow::Result<DirectAccessToken> {
    let client = ctx
        .gw
        .http_client_for_provider(ctx.provider.use_proxy)
        .await?;
    let payload: Value = client
        .post(format!(
            "{}/api/v4/ai/third_party_agents/direct_access",
            instance_url.trim_end_matches('/')
        ))
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = payload
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("GitLab direct-access response is missing token"))?
        .to_string();
    let headers = payload
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
            let value = HeaderValue::from_str(value.as_str()?).ok()?;
            (!name.as_str().eq_ignore_ascii_case("x-api-key")).then_some((name, value))
        })
        .collect();
    let entry = DirectAccessToken {
        token,
        headers,
        expires_at: Instant::now() + Duration::from_secs(25 * 60),
    };
    DIRECT_ACCESS_TOKENS.insert(cache_key.into(), entry.clone());
    Ok(entry)
}

fn apply_direct_access_headers(
    headers: &mut reqwest::header::HeaderMap,
    access: &DirectAccessToken,
) -> Result<(), GatewayError> {
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", access.token))
            .map_err(|error| GatewayError::internal(error.into()))?,
    );
    for (name, value) in &access.headers {
        headers.insert(name.clone(), value.clone());
    }
    Ok(())
}

fn replace_direct_access_headers(
    headers: &mut reqwest::header::HeaderMap,
    previous: Option<&DirectAccessToken>,
    fresh: &DirectAccessToken,
) -> Result<(), GatewayError> {
    headers.remove(AUTHORIZATION);
    if let Some(previous) = previous {
        for (name, _) in &previous.headers {
            headers.remove(name);
        }
    }
    apply_direct_access_headers(headers, fresh)
}
