//! Anthropic Messages over Vertex AI's Anthropic publisher endpoint.

use anyhow::anyhow;
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{ANTHROPIC_MESSAGES_2023_06_01, ProtocolId};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::{openai_compat::openai_map_error, pipeline};
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, CredentialFieldDef, CredentialInputKind, Label,
    ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};

use super::{expand_vertex_base_url, vertex_access_token};

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "project",
        label: "Google Cloud project",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "location",
        label: "Google Cloud location",
        secret: false,
        required: false,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "credentials",
        label: "Service account JSON",
        secret: true,
        required: false,
        input: CredentialInputKind::Textarea,
    },
    CredentialFieldDef {
        key: "apiKey",
        label: "Vertex API key",
        secret: true,
        required: false,
        input: CredentialInputKind::Password,
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "google-vertex-anthropic",
    label: Label {
        zh: "Vertex AI Anthropic",
        en: "Vertex AI Anthropic",
    },
    icon: "googlecloud",
    default_protocol: "anthropic-messages",
    credential_fields: CREDENTIAL_FIELDS,
    channels: &[ChannelDef {
        id: "default",
        label: Label {
            zh: "默认",
            en: "Default",
        },
        base_urls: &[ProtocolBaseUrl {
            protocol: "anthropic-messages",
            base_url: "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global",
        }],
        api_key: None,
        models_source: None,
        capabilities_source: CapabilitiesSource::Catalog("anthropic"),
        static_models: &[],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
    }],
};

pub struct GoogleVertexAnthropicVendor;

#[async_trait]
impl Vendor for GoogleVertexAnthropicVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "google-vertex-anthropic",
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
        use crate::provider::vendor_ext::{ConstructedRequest, RequestPurpose};
        let url = match purpose {
            RequestPurpose::Compact { .. } => {
                anyhow::bail!("Vendor does not support standalone compaction")
            }
            RequestPurpose::Models { .. } => {
                return crate::provider::common::openai_compat::construct_openai_request(
                    ctx, purpose,
                );
            }
            RequestPurpose::Inference {
                base_url,
                path: _,
                protocol: _,
                actual_model,
            } => vertex_anthropic_url(base_url, actual_model, false),
        };
        let headers = reqwest::header::HeaderMap::new();
        ConstructedRequest::new(ctx, purpose, url, headers)
    }

    fn vendor_id(&self) -> &'static str {
        "google-vertex-anthropic"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[ANTHROPIC_MESSAGES_2023_06_01]
    }

    async fn build_request(
        &self,
        request: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        let mut outbound = pipeline::build_request(self, request, ctx).await?;
        outbound.url = vertex_anthropic_url(
            ctx.egress_base_url,
            ctx.actual_model,
            request.stream.enabled,
        );
        if !ctx.disable_default_auth {
            let access_token = vertex_access_token(ctx.api_key).await.map_err(|source| {
                GatewayError::provider_unavailable(
                    "google-vertex-anthropic",
                    format!("failed to fetch Vertex access token: {source}"),
                )
            })?;
            let authorization =
                HeaderValue::from_str(&format!("Bearer {access_token}")).map_err(|source| {
                    GatewayError::Internal {
                        source: anyhow!(source)
                            .context("build Vertex Anthropic authorization header"),
                    }
                })?;
            outbound.headers.insert(AUTHORIZATION, authorization);
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
        body.get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(|message| {
                GatewayError::upstream_status(
                    "google-vertex-anthropic",
                    status,
                    Some(message.into()),
                )
            })
            .unwrap_or_else(|| openai_map_error("google-vertex-anthropic", status, body))
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(GoogleVertexAnthropicVendor) } }

pub fn vertex_anthropic_url(base_url: &str, model: &str, stream: bool) -> String {
    let action = if stream {
        "streamRawPredict"
    } else {
        "rawPredict"
    };
    format!(
        "{}/publishers/anthropic/models/{}:{}",
        base_url.trim_end_matches('/'),
        model,
        action,
    )
}

pub fn expanded_vertex_anthropic_url(
    base_url: &str,
    secret: &str,
    model: &str,
    stream: bool,
) -> String {
    vertex_anthropic_url(&expand_vertex_base_url(base_url, secret), model, stream)
}
