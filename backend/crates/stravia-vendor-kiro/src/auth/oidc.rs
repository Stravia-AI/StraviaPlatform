use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, Fields, GuestHost, PluginError,
    ProviderSnapshot,
};

use super::{
    add_ms, invalid_response, now_ms, positive_integer, post_json, required_string, seconds_ceil,
};
use crate::transport::profile_origins;

// Commercial SSO OIDC endpoints: https://docs.aws.amazon.com/general/latest/gr/sso.html
pub(crate) const OIDC_REGIONS: &[&str] = &[
    "af-south-1",
    "ap-east-1",
    "ap-east-2",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "ap-south-1",
    "ap-south-2",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-southeast-3",
    "ap-southeast-4",
    "ap-southeast-5",
    "ap-southeast-6",
    "ap-southeast-7",
    "ca-central-1",
    "ca-west-1",
    "eu-central-1",
    "eu-central-2",
    "eu-north-1",
    "eu-south-1",
    "eu-south-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "il-central-1",
    "me-central-1",
    "me-south-1",
    "mx-central-1",
    "sa-east-1",
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
];

const BUILDER_PROFILE: &str = "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Serialize, Deserialize)]
struct Session {
    region: String,
    enterprise: bool,
    client_id: String,
    client_secret: String,
    device_code: String,
    expires_at_ms: i64,
    next_poll_ms: i64,
    interval_ms: i64,
}

pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
    enterprise: bool,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start { .. } => start(host, provider, enterprise),
        AuthStep::Poll => poll(host, enterprise),
        AuthStep::Refresh => refresh(host, provider, enterprise),
        _ => Err(plugin_error(
            ErrorKind::Invalid,
            "unsupported Kiro auth step",
        )),
    }
}

fn headers() -> Fields {
    let mut headers = Fields::new();
    headers.append("Content-Type", "application/x-amz-json-1.1");
    headers.append("User-Agent", "Kiro-CLI");
    headers
}

fn oidc(
    host: &GuestHost,
    region: &str,
    path: &str,
    body: Value,
) -> Result<(u16, Value), PluginError> {
    if !OIDC_REGIONS.contains(&region) {
        return Err(plugin_error(
            ErrorKind::Invalid,
            "unsupported IAM Identity Center region",
        ));
    }
    post_json(
        host,
        format!("https://oidc.{region}.amazonaws.com{path}"),
        body,
        headers(),
    )
}

fn accepted(status: u16) -> Result<(), PluginError> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(plugin_error(
            ErrorKind::Auth,
            "Kiro OIDC authorization request failed",
        ))
    }
}

fn save(host: &GuestHost, state: &Session) -> Result<(), PluginError> {
    let encoded = serde_json::to_vec(state)
        .map_err(|_| plugin_error(ErrorKind::Trapped, "failed to encode Kiro auth state"))?;
    host.write_private_state(&encoded)
}

fn start(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    enterprise: bool,
) -> Result<AuthResponse, PluginError> {
    let (region, start_url) = if enterprise {
        let region = provider
            .options
            .get("region")
            .and_then(Value::as_str)
            .filter(|s| OIDC_REGIONS.contains(s))
            .ok_or_else(|| {
                plugin_error(ErrorKind::Invalid, "unsupported IAM Identity Center region")
            })?;
        let start = provider
            .options
            .get("start_url")
            .and_then(Value::as_str)
            .filter(|s| {
                s.starts_with("https://")
                    && s.len() > 8
                    && !s
                        .bytes()
                        .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
            })
            .ok_or_else(|| {
                plugin_error(
                    ErrorKind::Invalid,
                    "IAM Identity Center start URL is missing or invalid",
                )
            })?;
        (region, start)
    } else {
        ("us-east-1", "https://view.awsapps.com/start")
    };
    let (status, registration) = oidc(
        host,
        region,
        "/client/register",
        json!({
            "clientName": "Kiro CLI", "clientType": "public",
            "scopes": ["codewhisperer:completions", "codewhisperer:analysis", "codewhisperer:conversations"]
        }),
    )?;
    accepted(status)?;
    let client_id = required_string(&registration, "clientId")?;
    let client_secret = required_string(&registration, "clientSecret")?;
    let (status, authorization) = oidc(
        host,
        region,
        "/device_authorization",
        json!({
            "clientId": client_id, "clientSecret": client_secret, "startUrl": start_url
        }),
    )?;
    accepted(status)?;
    let user_code = required_string(&authorization, "userCode")?;
    let interval_ms = positive_integer(&authorization, "interval")?
        .checked_mul(1000)
        .ok_or_else(invalid_response)?;
    let lifetime_ms = positive_integer(&authorization, "expiresIn")?
        .checked_mul(1000)
        .ok_or_else(invalid_response)?;
    let now = now_ms()?;
    let state = Session {
        region: region.into(),
        enterprise,
        client_id: client_id.into(),
        client_secret: client_secret.into(),
        device_code: required_string(&authorization, "deviceCode")?.into(),
        expires_at_ms: add_ms(now, lifetime_ms)?,
        next_poll_ms: add_ms(now, interval_ms)?,
        interval_ms,
    };
    let uri = required_string(&authorization, "verificationUri")?;
    let url = required_string(&authorization, "verificationUriComplete")?;
    let expected_device_host = format!("device.sso.{region}.amazonaws.com");
    let portal_host = verification_host(uri).ok_or_else(invalid_response)?;
    if !(portal_host.ends_with(".awsapps.com") || portal_host == expected_device_host)
        || verification_host(url) != Some(portal_host)
    {
        return Err(invalid_response());
    }
    save(host, &state)?;
    Ok(AuthResponse::Authorization {
        url: url.into(),
        user_code: Some(user_code.into()),
        verification_uri: Some(uri.into()),
        interval_seconds: Some(seconds_ceil(interval_ms)?),
        state: None,
    })
}

fn verification_host(url: &str) -> Option<&str> {
    if !url.bytes().all(|c| c.is_ascii_graphic() && c != b'\\') {
        return None;
    }
    let (host, _) = url.strip_prefix("https://")?.split_once('/')?;
    host.split('.')
        .all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
        .then_some(host)
}

fn poll(host: &GuestHost, enterprise: bool) -> Result<AuthResponse, PluginError> {
    let bytes = host
        .read_private_state()?
        .ok_or_else(|| plugin_error(ErrorKind::Invalid, "Kiro auth session is missing"))?;
    let mut state: Session = serde_json::from_slice(&bytes)
        .map_err(|_| plugin_error(ErrorKind::Invalid, "invalid Kiro auth state"))?;
    if state.enterprise != enterprise
        || !OIDC_REGIONS.contains(&state.region.as_str())
        || state.client_id.is_empty()
        || state.client_secret.is_empty()
        || state.device_code.is_empty()
        || state.interval_ms <= 0
    {
        return Err(plugin_error(ErrorKind::Invalid, "invalid Kiro auth state"));
    }
    let now = now_ms()?;
    if now >= state.expires_at_ms {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Kiro device authorization expired",
        ));
    }
    if now < state.next_poll_ms {
        return Ok(AuthResponse::Pending {
            retry_after_seconds: Some(seconds_ceil(state.next_poll_ms - now)?),
        });
    }
    state.next_poll_ms = add_ms(now, state.interval_ms)?;
    save(host, &state)?;
    let (status, value) = oidc(
        host,
        &state.region,
        "/token",
        json!({
            "clientId": state.client_id, "clientSecret": state.client_secret,
            "grantType": DEVICE_GRANT, "deviceCode": state.device_code,
        }),
    )?;
    if !(200..300).contains(&status) {
        match value.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => {
                return Ok(AuthResponse::Pending {
                    retry_after_seconds: Some(seconds_ceil(state.interval_ms)?),
                });
            }
            Some("slow_down") => {
                state.interval_ms = add_ms(state.interval_ms, 5000)?;
                state.next_poll_ms = add_ms(now, state.interval_ms)?;
                save(host, &state)?;
                return Ok(AuthResponse::Pending {
                    retry_after_seconds: Some(seconds_ceil(state.interval_ms)?),
                });
            }
            Some("access_denied") => {
                return Err(plugin_error(
                    ErrorKind::Auth,
                    "Kiro device authorization denied",
                ));
            }
            Some("expired_token") => {
                return Err(plugin_error(
                    ErrorKind::Auth,
                    "Kiro device authorization expired",
                ));
            }
            _ => {
                return Err(plugin_error(
                    ErrorKind::Auth,
                    "Kiro device authorization failed",
                ));
            }
        }
    }
    let access = required_string(&value, "accessToken")?;
    let profile = if enterprise {
        find_profile(host, &state.region, access)?
    } else {
        BUILDER_PROFILE.to_owned()
    };
    finish(
        value,
        &profile,
        &state.region,
        &state.client_id,
        &state.client_secret,
        enterprise,
        None,
    )
}

fn refresh(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    enterprise: bool,
) -> Result<AuthResponse, PluginError> {
    let get = |key: &str| {
        provider
            .credentials
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| plugin_error(ErrorKind::Auth, "Kiro OIDC credentials are incomplete"))
    };
    let region = get("oidc_region")?;
    let client_id = get("oidc_client_id")?;
    let client_secret = get("oidc_client_secret")?;
    let refresh_token = get("refresh_token")?;
    let profile = get("profileArn")?;
    if provider
        .credentials
        .get("oidc_enterprise")
        .and_then(Value::as_bool)
        != Some(enterprise)
        || (!enterprise && (region != "us-east-1" || profile != BUILDER_PROFILE))
    {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Kiro OIDC identity does not match",
        ));
    }
    profile_origins(profile)?;
    let (status, value) = oidc(
        host,
        region,
        "/token",
        json!({
            "clientId": client_id, "clientSecret": client_secret,
            "grantType": "refresh_token", "refreshToken": refresh_token,
        }),
    )?;
    accepted(status)?;
    finish(
        value,
        profile,
        region,
        client_id,
        client_secret,
        enterprise,
        Some(refresh_token),
    )
}

fn finish(
    mut value: Value,
    profile: &str,
    region: &str,
    client_id: &str,
    client_secret: &str,
    enterprise: bool,
    previous: Option<&str>,
) -> Result<AuthResponse, PluginError> {
    let object = value.as_object_mut().ok_or_else(invalid_response)?;
    // The profile is selected locally, not accepted from an OIDC response.
    object.insert("profileArn".into(), Value::String(profile.into()));
    let mut response = super::credentials(&value, previous.map(|r| (r, profile)))?;
    if let AuthResponse::Credentials { values, .. } = &mut response {
        values.insert("oidc_region".into(), Value::String(region.into()));
        values.insert("oidc_client_id".into(), Value::String(client_id.into()));
        values.insert(
            "oidc_client_secret".into(),
            Value::String(client_secret.into()),
        );
        values.insert("oidc_enterprise".into(), Value::Bool(enterprise));
    }
    Ok(response)
}

fn find_profile(host: &GuestHost, region: &str, access: &str) -> Result<String, PluginError> {
    let first = if matches!(region, "eu-central-1" | "eu-west-1") {
        "eu-central-1"
    } else {
        "us-east-1"
    };
    let second = if first == "us-east-1" {
        "eu-central-1"
    } else {
        "us-east-1"
    };
    let mut unavailable = false;
    let mut disabled = false;
    for location in [first, second] {
        let mut next = Option::<String>::None;
        let mut seen = HashSet::new();
        let mut complete = false;
        for _ in 0..20 {
            let mut headers = Fields::new();
            headers.append("Content-Type", "application/x-amz-json-1.0");
            headers.append(
                "X-Amz-Target",
                "AmazonCodeWhispererService.ListAvailableProfiles",
            );
            headers.append("Authorization", format!("Bearer {access}"));
            headers.append("User-Agent", "Kiro-CLI");
            let mut body = json!({"maxResults": 10});
            if let Some(token) = &next {
                body["nextToken"] = Value::String(token.clone());
            }
            let (status, value) = post_json(
                host,
                format!("https://management.{location}.kiro.dev/"),
                body,
                headers,
            )?;
            if !(200..300).contains(&status) {
                unavailable = true;
                complete = true;
                break;
            }
            let profiles = value
                .get("profiles")
                .and_then(Value::as_array)
                .ok_or_else(invalid_response)?;
            for entry in profiles {
                if let Some(arn) = entry.get("arn").and_then(Value::as_str) {
                    if valid_profile(arn) {
                        match entry.pointer("/optInFeatures/deviceCodeSignIn/toggle") {
                            None | Some(Value::Null) | Some(Value::Bool(true)) => {}
                            Some(Value::String(v)) if v.eq_ignore_ascii_case("enabled") => {}
                            Some(Value::Bool(false)) => {
                                disabled = true;
                                continue;
                            }
                            Some(Value::String(v)) if v.eq_ignore_ascii_case("disabled") => {
                                disabled = true;
                                continue;
                            }
                            _ => return Err(invalid_response()),
                        }
                        return Ok(arn.to_owned());
                    }
                }
            }
            match value.get("nextToken") {
                None | Some(Value::Null) => {
                    complete = true;
                    break;
                }
                Some(Value::String(token)) if token.is_empty() => {
                    complete = true;
                    break;
                }
                Some(Value::String(token)) if seen.insert(token.clone()) => {
                    next = Some(token.clone())
                }
                _ => return Err(invalid_response()),
            }
        }
        if !complete {
            return Err(invalid_response());
        }
    }
    if unavailable {
        return Err(plugin_error(
            ErrorKind::upstream_unknown(),
            "Kiro enterprise profile lookup failed",
        ));
    }
    if disabled {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Kiro enterprise device sign-in is disabled",
        ));
    }
    Err(plugin_error(
        ErrorKind::Auth,
        "no eligible Kiro enterprise profile",
    ))
}

fn valid_profile(arn: &str) -> bool {
    let mut parts = arn.split(':');
    let (
        Some("arn"),
        Some("aws"),
        Some("codewhisperer"),
        Some(_),
        Some(account),
        Some(resource),
        None,
    ) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    )
    else {
        return false;
    };
    let Some(id) = resource.strip_prefix("profile/") else {
        return false;
    };
    account.len() == 12
        && account.bytes().all(|c| c.is_ascii_digit())
        && !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|c| c.is_ascii_alphanumeric())
        && profile_origins(arn).is_ok()
}
