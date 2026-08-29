//! IBM watsonx.ai adapter: IAM token exchange over its OpenAI-like chat wire.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{ProtocolId, WATSONX_TEXT_CHAT_V1};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::{openai_compat::openai_map_error, pipeline};
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, CredentialFieldDef, CredentialInputKind, Label,
    VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

const DEFAULT_BASE_URL: &str = "https://us-south.ml.cloud.ibm.com";
const DEFAULT_API_VERSION: &str = "2026-04-20";

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "apiKey",
        label: "IBM Cloud API key",
        secret: true,
        required: true,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "projectId",
        label: "Project ID",
        secret: false,
        required: true,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "baseUrl",
        label: "Service URL",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
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
    id: "watsonx",
    label: Label {
        zh: "watsonx.ai",
        en: "watsonx.ai",
    },
    icon: "ibm",
    default_protocol: "watsonx-text-chat",
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

pub struct WatsonxVendor;

#[derive(Clone)]
struct IamToken {
    value: String,
    expires_at: Instant,
}

#[async_trait]
impl Vendor for WatsonxVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "watsonx",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }

    fn build_url(&self, ctx: &VendorCtx<'_>, base_url: &str, _path: &str) -> String {
        let base = watsonx_base_url(ctx.provider, base_url);
        format!(
            "{base}/ml/v1/text/chat?version={}",
            watsonx_api_version(ctx.provider)
        )
    }

    async fn post_encode(
        &self,
        ctx: &VendorCtx<'_>,
        body: &mut Value,
        _headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        let object = body
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("watsonx request body is not an object"))?;
        object.insert(
            "project_id".into(),
            credential(ctx.provider, "projectId")
                .ok_or_else(|| anyhow::anyhow!("watsonx projectId is required"))?
                .into(),
        );
        object.remove("stream");
        if let Some(choice) = object.remove("tool_choice") {
            match choice.as_str() {
                Some("auto") | Some("none") | Some("required") => {
                    object.insert("tool_choice_option".into(), choice);
                }
                _ => {
                    object.insert("tool_choice".into(), choice);
                }
            }
        }
        Ok(())
    }

    fn vendor_id(&self) -> &'static str {
        "watsonx"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[WATSONX_TEXT_CHAT_V1]
    }

    async fn build_request(
        &self,
        request: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        let mut outbound = pipeline::build_request(self, request, ctx).await?;
        let api_key = credential(ctx.provider, "apiKey").ok_or_else(|| {
            GatewayError::provider_unavailable("watsonx", "IBM Cloud API key is required")
        })?;
        let token = iam_token(ctx, &api_key)
            .await
            .map_err(|error| GatewayError::provider_unavailable("watsonx", error.to_string()))?;
        let endpoint = if request.stream.enabled {
            "chat_stream"
        } else {
            "chat"
        };
        outbound.url = format!(
            "{}/ml/v1/text/{endpoint}?version={}",
            watsonx_base_url(ctx.provider, ctx.egress_base_url),
            watsonx_api_version(ctx.provider)
        );
        outbound.headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|error| GatewayError::internal(error.into()))?,
        );
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
        body.get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| errors.first())
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(|message| GatewayError::upstream_status("watsonx", status, Some(message.into())))
            .unwrap_or_else(|| openai_map_error("watsonx", status, body))
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(WatsonxVendor) } }

pub fn watsonx_base_url(
    provider: &crate::db::models::Provider,
    configured_base_url: &str,
) -> String {
    credential(provider, "baseUrl")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            (!configured_base_url.trim().is_empty()).then(|| configured_base_url.trim().to_string())
        })
        .map(|base| base.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_BASE_URL.into())
}

fn watsonx_api_version(provider: &crate::db::models::Provider) -> String {
    credential(provider, "apiVersion")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_VERSION.into())
}

async fn iam_token(ctx: &ProviderCtx<'_>, api_key: &str) -> anyhow::Result<String> {
    let cache = iam_cache();
    if let Some(entry) = cache.get(api_key)
        && entry.expires_at > Instant::now() + Duration::from_secs(300)
    {
        return Ok(entry.value.clone());
    }
    let client = ctx
        .gw
        .http_client_for_provider(ctx.provider.use_proxy)
        .await?;
    let response = client
        .post("https://iam.cloud.ibm.com/identity/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "urn:ibm:params:oauth:grant-type:apikey"),
            ("apikey", api_key),
        ])
        .send()
        .await?
        .error_for_status()?;
    let payload: Value = response.json().await?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("IBM IAM response is missing access_token"))?
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    cache.insert(
        api_key.into(),
        IamToken {
            value: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        },
    );
    Ok(token)
}

fn iam_cache() -> &'static DashMap<String, IamToken> {
    static CACHE: LazyLock<DashMap<String, IamToken>> = LazyLock::new(DashMap::new);
    &CACHE
}

fn credential(provider: &crate::db::models::Provider, key: &str) -> Option<String> {
    provider.adapter_credential(key)
}
