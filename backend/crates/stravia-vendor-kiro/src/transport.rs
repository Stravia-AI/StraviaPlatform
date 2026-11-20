use serde_json::Value;
use stravia_vendor_common::common::{plugin_error, upstream_error};
use stravia_vendor_sdk::{
    ErrorKind, Fields, GuestHost, HttpResponse, Options, PluginError, ProviderSnapshot, Request,
    read_http_body,
};
use uuid::Uuid;

use crate::{BUILDER_ID_CHANNEL, DEFAULT_BASE_URL, IDENTITY_CENTER_CHANNEL};

pub(crate) const CLI_EGRESS_SHAPE: &str = "plugin:kiro-cli-3-4-0";
pub(crate) const SDK_X_AMZ_USER_AGENT: &str =
    "aws-sdk-js/1.0.0 KiroCLI/3.4.0 KAS/0.66.111 os/linux app/AmazonQ-For-CLI acp-client/kiro-cli";
pub(crate) const SDK_USER_AGENT: &str = "aws-sdk-js/1.0.0 lang/js m/N KiroCLI/3.4.0 KAS/0.66.111 os/linux app/AmazonQ-For-CLI acp-client/kiro-cli";

#[derive(Clone, Copy)]
pub(super) enum Action {
    Generate,
    GetUsageLimits,
    ListModels,
}

pub(super) fn rpc(
    provider: &ProviderSnapshot,
    action: Action,
    input: &impl serde::Serialize,
) -> Result<Request, PluginError> {
    let secret = |name: &str| {
        provider
            .credentials
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let key = secret("access_token")
        .ok_or_else(|| plugin_error(ErrorKind::Auth, "Kiro OAuth access token is missing"))?;
    let profile = secret("profileArn")
        .ok_or_else(|| plugin_error(ErrorKind::Auth, "Kiro OAuth profile ARN is missing"))?;
    let (runtime, management) = profile_origins(profile)?;
    let (origin, target) = match action {
        Action::Generate => (runtime, "KiroRuntimeService.GenerateAssistantResponse"),
        Action::GetUsageLimits => (management, "KiroControlPlaneBearerService.GetUsageLimits"),
        Action::ListModels => (
            management,
            "KiroControlPlaneBearerService.ListAvailableModels",
        ),
    };
    let mut headers = Fields::new();
    headers.append("content-type", "application/x-amz-json-1.0");
    headers.append("x-amz-target", target);
    // Node 的 HTTP/1 sender 在固定位置填写长度和 authority。值由宿主根据最终请求填写。
    headers.append("content-length", "");
    let inference = matches!(action, Action::Generate);
    if matches!(
        provider.channel.as_str(),
        BUILDER_ID_CHANNEL | IDENTITY_CENTER_CHANNEL
    ) {
        headers.append("TokenType", "SSO_OIDC");
    }
    if inference {
        headers.append("x-amzn-kiro-client-attribution", "kiro-cli");
        headers.append("x-amzn-kiro-mode-identifier", "vibe");
        headers.append("x-amzn-kiro-fallback", "0;max=0");
    }
    headers.append("x-amz-user-agent", SDK_X_AMZ_USER_AGENT);
    headers.append("user-agent", SDK_USER_AGENT);
    headers.append("host", "");
    headers.append("amz-sdk-invocation-id", Uuid::new_v4().to_string());
    headers.append("amz-sdk-request", "attempt=1; max=3");
    if inference {
        headers.append("x-amzn-kiro-attempt", "1;max=3");
    }
    headers.append("Authorization", format!("Bearer {key}"));
    headers.append("Connection", "keep-alive");
    let mut body =
        serde_json::to_vec(input).map_err(|e| plugin_error(ErrorKind::Invalid, e.to_string()))?;
    // Append to the serialized object instead of walking the inference history twice.
    if body.first() != Some(&b'{') || body.pop() != Some(b'}') {
        return Err(plugin_error(
            ErrorKind::Invalid,
            "Kiro RPC body must be an object",
        ));
    }
    if body.len() > 1 {
        body.push(b',');
    }
    body.extend_from_slice(b"\"profileArn\":");
    serde_json::to_writer(&mut body, profile)
        .map_err(|e| plugin_error(ErrorKind::Invalid, e.to_string()))?;
    body.push(b'}');
    Ok(Request {
        method: "POST".into(),
        url: format!("{origin}/"),
        headers,
        body,
    })
}

pub(super) fn profile_origins(profile: &str) -> Result<(&'static str, &'static str), PluginError> {
    let mut parts = profile.splitn(6, ':');
    let (prefix, partition, service, region, account, resource) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    );
    if prefix != Some("arn")
        || partition != Some("aws")
        || service.is_none_or(str::is_empty)
        || account.is_none_or(str::is_empty)
        || resource.is_none_or(str::is_empty)
    {
        return Err(plugin_error(
            ErrorKind::Auth,
            "invalid Kiro OAuth profile ARN",
        ));
    }
    match region {
        Some("us-east-1") => Ok((DEFAULT_BASE_URL, "https://management.us-east-1.kiro.dev")),
        Some("eu-central-1") => Ok((
            "https://runtime.eu-central-1.kiro.dev",
            "https://management.eu-central-1.kiro.dev",
        )),
        _ => Err(plugin_error(
            ErrorKind::Auth,
            "unsupported Kiro OAuth profile region",
        )),
    }
}

pub(super) fn send(host: &GuestHost, request: Request) -> Result<HttpResponse, PluginError> {
    let mut options = Options::new();
    options
        .shape(CLI_EGRESS_SHAPE)
        .expect("valid Kiro CLI egress shape selector");
    let response = host.http_start(request, &options)?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(upstream_error(
            status,
            &response.headers()?,
            &read_http_body(&response, 1024 * 1024)?,
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn provider(channel: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "kiro".into(),
            channel: channel.into(),
            base_url: DEFAULT_BASE_URL.into(),
            protocol: String::new(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: None,
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn oauth_uses_profile_region_and_body_for_every_action() {
        let mut provider = provider(crate::GOOGLE_CHANNEL);
        provider
            .credentials
            .insert("access_token".into(), json!("oauth-token"));
        provider
            .credentials
            .insert("apiKey".into(), json!("wrong-key"));
        provider.credentials.insert(
            "profileArn".into(),
            json!("arn:aws:codewhisperer:eu-central-1:123456789012:profile/test"),
        );
        for (action, origin) in [
            (Action::Generate, "https://runtime.eu-central-1.kiro.dev/"),
            (
                Action::ListModels,
                "https://management.eu-central-1.kiro.dev/",
            ),
            (
                Action::GetUsageLimits,
                "https://management.eu-central-1.kiro.dev/",
            ),
        ] {
            let request = rpc(&provider, action, &json!({"origin": "KIRO_CLI"})).unwrap();
            assert_eq!(request.url, origin);
            assert_eq!(
                request.headers.get("Authorization"),
                Some(&b"Bearer oauth-token"[..])
            );
            assert_eq!(request.headers.get("TokenType"), None);
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["origin"], "KIRO_CLI");
            assert_eq!(
                body["profileArn"],
                "arn:aws:codewhisperer:eu-central-1:123456789012:profile/test"
            );
        }
    }

    #[test]
    fn identity_center_and_builder_id_use_sso_token_type_and_never_accept_api_keys() {
        for channel in [BUILDER_ID_CHANNEL, IDENTITY_CENTER_CHANNEL] {
            let mut provider = provider(channel);
            provider
                .credentials
                .insert("apiKey".into(), json!("api-key"));
            assert!(rpc(&provider, Action::Generate, &json!({})).is_err());
            provider
                .credentials
                .insert("access_token".into(), json!("oauth-token"));
            provider.credentials.insert(
                "profileArn".into(),
                json!("arn:aws:codewhisperer:us-east-1:123456789012:profile/test"),
            );
            let request = rpc(&provider, Action::Generate, &json!({})).unwrap();
            assert_eq!(
                request.headers.get("TokenType"),
                Some(b"SSO_OIDC".as_slice())
            );
            assert_eq!(
                request.headers.get("Authorization"),
                Some(b"Bearer oauth-token".as_slice())
            );
        }
    }

    #[test]
    fn profile_region_must_be_allowed_and_arn_complete() {
        assert!(
            profile_origins("arn:aws:codewhisperer:us-east-1:123456789012:profile/test").is_ok()
        );
        assert!(
            profile_origins("arn:aws:codewhisperer:eu-west-1:123456789012:profile/test").is_err()
        );
        assert!(profile_origins("arn:aws:codewhisperer:us-east-1::").is_err());
        assert!(profile_origins("https://runtime.us-east-1.kiro.dev").is_err());
    }
}
