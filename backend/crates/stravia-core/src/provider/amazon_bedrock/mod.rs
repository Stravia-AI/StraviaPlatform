//! Amazon Bedrock Converse adapter with bearer and AWS SigV4 authentication.

use std::collections::BTreeMap;
use std::time::SystemTime;

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{BEDROCK_CONVERSE_V1, ProtocolId};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, CredentialFieldDef, CredentialInputKind, Label,
    VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{
    ProviderCtx, Vendor, resolve_base_url, validate_declared_credentials,
};
use crate::provider::vendor_ext::VendorCtx;

const CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[
    CredentialFieldDef {
        key: "region",
        label: "AWS region",
        secret: false,
        required: true,
        input: CredentialInputKind::Text,
    },
    CredentialFieldDef {
        key: "apiKey",
        label: "Bedrock API key",
        secret: true,
        required: false,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "accessKeyId",
        label: "Access key ID",
        secret: true,
        required: false,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "secretAccessKey",
        label: "Secret access key",
        secret: true,
        required: false,
        input: CredentialInputKind::Password,
    },
    CredentialFieldDef {
        key: "sessionToken",
        label: "Session token",
        secret: true,
        required: false,
        input: CredentialInputKind::Password,
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: "amazon-bedrock",
    label: Label {
        zh: "Amazon Bedrock",
        en: "Amazon Bedrock",
    },
    icon: "aws",
    default_protocol: "bedrock-converse",
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

pub struct AmazonBedrockVendor;

#[async_trait]
impl Vendor for AmazonBedrockVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "amazon-bedrock",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }

    fn validate_credentials(
        &self,
        values: BTreeMap<String, String>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let values = validate_declared_credentials(self.vendor_id(), self.metadata(), values)?;
        if !has_bedrock_auth(
            values.contains_key("apiKey"),
            values.contains_key("accessKeyId"),
            values.contains_key("secretAccessKey"),
        ) {
            anyhow::bail!("configure an API key or both AWS access keys");
        }
        Ok(values)
    }

    fn assemble_base_url(
        &self,
        credentials: &BTreeMap<String, String>,
        configured_base_url: Option<&str>,
    ) -> anyhow::Result<String> {
        resolve_base_url(self.vendor_id(), configured_base_url, || {
            bedrock_base_url(credentials.get("region").map(String::as_str).unwrap_or(""))
        })
    }

    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(api_key) = credential(ctx.provider, "apiKey")
            && let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}"))
        {
            headers.insert(AUTHORIZATION, value);
        }
        headers
    }

    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        format!("{}{}", base_url.trim_end_matches('/'), path)
    }

    fn vendor_id(&self) -> &'static str {
        "amazon-bedrock"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[BEDROCK_CONVERSE_V1]
    }

    async fn build_request(
        &self,
        req: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        let mut outbound = pipeline::build_request(self, req, ctx).await?;
        if credential(ctx.provider, "apiKey").is_none() {
            sign_bedrock_request(&mut outbound, ctx.provider).map_err(|error| {
                GatewayError::provider_unavailable("amazon-bedrock", error.to_string())
            })?;
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
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        GatewayError::upstream_status("amazon-bedrock", status, message)
    }

    fn validate_environment(
        &self,
        provider: &crate::db::models::Provider,
    ) -> Result<(), GatewayError> {
        if credential(provider, "region").is_none() {
            return Err(GatewayError::provider_unavailable(
                "amazon-bedrock",
                "AWS region is required",
            ));
        }
        if !has_bedrock_auth(
            credential(provider, "apiKey").is_some(),
            credential(provider, "accessKeyId").is_some(),
            credential(provider, "secretAccessKey").is_some(),
        ) {
            return Err(GatewayError::provider_unavailable(
                "amazon-bedrock",
                "configure an API key or both AWS access keys",
            ));
        }
        Ok(())
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(AmazonBedrockVendor) } }

pub fn bedrock_base_url(region: &str) -> String {
    let region = region.trim();
    if region.is_empty() {
        String::new()
    } else {
        format!("https://bedrock-runtime.{region}.amazonaws.com")
    }
}

fn sign_bedrock_request(
    outbound: &mut OutboundRequest,
    provider: &crate::db::models::Provider,
) -> anyhow::Result<()> {
    let region = credential(provider, "region").expect("validated Bedrock region");
    let access_key_id = credential(provider, "accessKeyId").expect("validated Bedrock access key");
    let secret_access_key =
        credential(provider, "secretAccessKey").expect("validated Bedrock secret key");
    let session_token = credential(provider, "sessionToken");
    let body = serde_json::to_vec(&outbound.body)?;
    let headers = outbound
        .headers
        .iter()
        .map(|(name, value)| value.to_str().map(|value| (name.as_str(), value)))
        .collect::<Result<Vec<_>, _>>()?;
    let signable = SignableRequest::new(
        "POST",
        &outbound.url,
        headers.into_iter(),
        SignableBody::Bytes(&body),
    )?;
    let credentials = Credentials::new(
        access_key_id,
        secret_access_key,
        session_token,
        None,
        "stravia-bedrock",
    );
    let identity = credentials.into();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(&region)
        .name("bedrock")
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()?
        .into();
    let (instructions, _) = sign(signable, &signing_params)?.into_parts();
    for (name, value) in instructions.headers() {
        outbound.headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    Ok(())
}

fn credential(provider: &crate::db::models::Provider, key: &str) -> Option<String> {
    provider.adapter_credential(key)
}

fn has_bedrock_auth(api_key: bool, access_key_id: bool, secret_access_key: bool) -> bool {
    api_key || access_key_id && secret_access_key
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn vendor_requires_one_bedrock_auth_mode_and_assembles_region() {
        let missing_auth = BTreeMap::from([("region".to_string(), "us-east-1".to_string())]);
        assert!(
            AmazonBedrockVendor
                .validate_credentials(missing_auth)
                .is_err()
        );

        let credentials = BTreeMap::from([
            ("region".to_string(), "us-east-1".to_string()),
            ("apiKey".to_string(), "key".to_string()),
        ]);
        let credentials = AmazonBedrockVendor
            .validate_credentials(credentials)
            .unwrap();
        assert_eq!(
            AmazonBedrockVendor
                .assemble_base_url(&credentials, None)
                .unwrap(),
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
    }
}
