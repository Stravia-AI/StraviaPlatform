use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, Fields, GuestHost, Options, PluginError,
    ProviderSnapshot, Request, read_http_body,
};

use crate::transport::profile_origins;
use crate::{BUILDER_ID_CHANNEL, GITHUB_CHANNEL, GOOGLE_CHANNEL, IDENTITY_CENTER_CHANNEL};

mod oidc;
pub(crate) use oidc::OIDC_REGIONS;

const AUTH_ORIGIN: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";
const CLIENT_ID: &str = "Kiro-CLI";
const VERIFY_URI: &str = "https://app.kiro.dev/account/device";
const MAX_BODY: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct DeviceState {
    device_code: String,
    expires_at_ms: i64,
    next_poll_ms: i64,
    interval_ms: i64,
}

pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    let login_provider = match provider.channel.as_str() {
        GOOGLE_CHANNEL => "Google",
        GITHUB_CHANNEL => "Github",
        BUILDER_ID_CHANNEL => return oidc::execute(host, provider, request, false),
        IDENTITY_CENTER_CHANNEL => return oidc::execute(host, provider, request, true),
        _ => {
            return Err(plugin_error(
                ErrorKind::Unsupported,
                "unsupported Kiro auth channel",
            ));
        }
    };
    match request.step {
        AuthStep::Start { .. } => start(host, login_provider),
        AuthStep::Poll => poll(host),
        AuthStep::Refresh => refresh(host, provider),
        _ => Err(plugin_error(
            ErrorKind::Invalid,
            "unsupported Kiro auth step",
        )),
    }
}

fn start(host: &GuestHost, login_provider: &str) -> Result<AuthResponse, PluginError> {
    let value = post(
        host,
        "/oauth/device/authorization",
        json!({
            "clientId": CLIENT_ID, "loginProvider": login_provider
        }),
    )?;
    let device_code = required_string(&value, "deviceCode")?;
    let user_code = required_string(&value, "userCode")?;
    // The displayed URL is constructed at a fixed origin, never from upstream input.
    let interval_ms = positive_integer(&value, "intervalInMilliseconds")?;
    let lifetime_ms = positive_integer(&value, "expiresInMilliseconds")?;
    let interval_seconds = seconds_ceil(interval_ms)?;
    let now = now_ms()?;
    let state = DeviceState {
        device_code: device_code.to_owned(),
        expires_at_ms: add_ms(now, lifetime_ms)?,
        next_poll_ms: add_ms(now, interval_ms)?,
        interval_ms,
    };
    let encoded = serde_json::to_vec(&state)
        .map_err(|_| plugin_error(ErrorKind::Trapped, "failed to encode Kiro auth state"))?;
    host.write_private_state(&encoded)?;
    let provider_hint = if login_provider == "Github" {
        "&login_provider=github"
    } else {
        ""
    };
    Ok(AuthResponse::Authorization {
        url: format!(
            "{VERIFY_URI}?user_code={}{provider_hint}",
            encode_query(user_code)
        ),
        user_code: Some(user_code.to_owned()),
        verification_uri: Some(VERIFY_URI.into()),
        interval_seconds: Some(interval_seconds),
        state: None,
    })
}

fn poll(host: &GuestHost) -> Result<AuthResponse, PluginError> {
    let bytes = host
        .read_private_state()?
        .ok_or_else(|| plugin_error(ErrorKind::Invalid, "Kiro auth session is missing"))?;
    let mut state: DeviceState = serde_json::from_slice(&bytes)
        .map_err(|_| plugin_error(ErrorKind::Invalid, "invalid Kiro auth state"))?;
    if state.device_code.is_empty() || state.interval_ms <= 0 {
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
    // Persist the next permitted request before making the network call, including
    // when the request itself fails or returns an invalid response.
    state.next_poll_ms = add_ms(now, state.interval_ms)?;
    let encoded = serde_json::to_vec(&state)
        .map_err(|_| plugin_error(ErrorKind::Trapped, "failed to encode Kiro auth state"))?;
    host.write_private_state(&encoded)?;
    let value = post(
        host,
        "/oauth/device/poll",
        json!({
            "clientId": CLIENT_ID, "deviceCode": state.device_code
        }),
    )?;
    match value.get("status").and_then(Value::as_str) {
        Some("authorization_pending") => Ok(AuthResponse::Pending {
            retry_after_seconds: Some(seconds_ceil(state.interval_ms)?),
        }),
        Some("access_denied") | Some("authorization_declined") => Err(plugin_error(
            ErrorKind::Auth,
            "Kiro device authorization denied",
        )),
        Some("expired_token") | Some("authorization_expired") => Err(plugin_error(
            ErrorKind::Auth,
            "Kiro device authorization expired",
        )),
        Some("invalid_token") => Err(plugin_error(
            ErrorKind::Auth,
            "Kiro device authorization is invalid",
        )),
        Some("authorized") => credentials(&value, None),
        _ => Err(invalid_response()),
    }
}

fn refresh(host: &GuestHost, provider: &ProviderSnapshot) -> Result<AuthResponse, PluginError> {
    let old_refresh = provider
        .credentials
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| plugin_error(ErrorKind::Auth, "Kiro refresh token is missing"))?;
    let old_profile = provider
        .credentials
        .get("profileArn")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| plugin_error(ErrorKind::Auth, "Kiro OAuth profile is missing"))?;
    profile_origins(old_profile)?;
    let value = post(
        host,
        "/refreshToken",
        json!({ "refreshToken": old_refresh }),
    )?;
    credentials(&value, Some((old_refresh, old_profile)))
}

fn credentials(value: &Value, previous: Option<(&str, &str)>) -> Result<AuthResponse, PluginError> {
    let access = required_string(value, "accessToken")?;
    let refresh = optional_string(value, "refreshToken")?
        .or_else(|| previous.map(|p| p.0))
        .ok_or_else(invalid_response)?;
    let profile = optional_string(value, "profileArn")?
        .or_else(|| previous.map(|p| p.1))
        .ok_or_else(invalid_response)?;
    if previous.is_some_and(|p| p.1 != profile) {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Kiro refresh changed OAuth profile",
        ));
    }
    profile_origins(profile)?;
    let seconds = match value.get("expiresIn") {
        // Kiro CLI 3.4.0 defaults a successful device poll (not refresh) to one hour.
        None | Some(Value::Null) if previous.is_none() => 3600,
        _ => positive_integer(value, "expiresIn")?,
    };
    let expiry = add_ms(
        now_ms()?,
        seconds.checked_mul(1000).ok_or_else(invalid_response)?,
    )?;
    let mut values = BTreeMap::new();
    values.insert("access_token".into(), Value::String(access.to_owned()));
    values.insert("refresh_token".into(), Value::String(refresh.to_owned()));
    values.insert("profileArn".into(), Value::String(profile.to_owned()));
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms: Some(expiry),
    })
}

fn post(host: &GuestHost, path: &str, body: Value) -> Result<Value, PluginError> {
    let mut headers = Fields::new();
    headers.append("Content-Type", "application/json");
    headers.append("User-Agent", CLIENT_ID);
    let (status, value) = post_json(host, format!("{AUTH_ORIGIN}{path}"), body, headers)?;
    if !(200..300).contains(&status) {
        return Err(
            match value
                .get("status")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
            {
                Some("access_denied" | "authorization_declined") => {
                    plugin_error(ErrorKind::Auth, "Kiro device authorization denied")
                }
                Some("expired_token" | "authorization_expired") => {
                    plugin_error(ErrorKind::Auth, "Kiro device authorization expired")
                }
                _ => plugin_error(
                    ErrorKind::upstream_unknown(),
                    format!("Kiro auth request failed (HTTP {status})"),
                ),
            },
        );
    }
    Ok(value)
}

fn post_json(
    host: &GuestHost,
    url: String,
    body: Value,
    headers: Fields,
) -> Result<(u16, Value), PluginError> {
    let request = Request {
        method: "POST".into(),
        url,
        headers,
        body: serde_json::to_vec(&body).map_err(|_| invalid_response())?,
    };
    let mut options = Options::new();
    options
        .shape(crate::transport::CLI_EGRESS_SHAPE)
        .expect("valid Kiro CLI egress shape selector");
    let response = host.http_start(request, &options)?;
    let status = response.status()?;
    let bytes = read_http_body(&response, MAX_BODY)?;
    let value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => Value::Null,
        Err(_) => return Err(invalid_response()),
    };
    Ok((status, value))
}

fn invalid_response() -> PluginError {
    plugin_error(ErrorKind::upstream_unknown(), "invalid Kiro auth response")
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, PluginError> {
    optional_string(value, key)?.ok_or_else(invalid_response)
}

fn optional_string<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, PluginError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s)),
        _ => Err(invalid_response()),
    }
}

fn positive_integer(value: &Value, key: &str) -> Result<i64, PluginError> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(invalid_response)
}

fn now_ms() -> Result<i64, PluginError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .ok_or_else(|| plugin_error(ErrorKind::Trapped, "Kiro auth clock unavailable"))
}

fn add_ms(now: i64, duration: i64) -> Result<i64, PluginError> {
    now.checked_add(duration).ok_or_else(invalid_response)
}

fn seconds_ceil(ms: i64) -> Result<u32, PluginError> {
    let seconds = ms / 1000 + i64::from(ms % 1000 != 0);
    u32::try_from(seconds).map_err(|_| invalid_response())
}

fn encode_query(value: &str) -> String {
    let mut encoded = String::new();
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 15) as usize]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = "arn:aws:codewhisperer:us-east-1:123456789012:profile/test";

    #[test]
    fn query_code_is_escaped() {
        assert_eq!(encode_query("a+b &/中"), "a%2Bb%20%26%2F%E4%B8%AD");
    }

    #[test]
    fn complete_bundle_requires_positive_expiry_and_supported_profile() {
        let mut response = json!({
            "accessToken": "access", "refreshToken": "refresh",
            "profileArn": PROFILE, "expiresIn": 3600
        });
        assert!(credentials(&response, None).is_ok());
        response["expiresIn"] = json!(0);
        assert!(credentials(&response, None).is_err());
        response["expiresIn"] = json!(3600);
        response["profileArn"] = json!("arn:aws:codewhisperer:eu-west-1:123:profile/test");
        assert!(credentials(&response, None).is_err());
        response["profileArn"] = json!(PROFILE);
        response["refreshToken"] = Value::Null;
        assert!(credentials(&response, None).is_err());
    }

    #[test]
    fn refresh_retains_missing_values_but_rejects_profile_change() {
        let response = json!({"accessToken": "new", "expiresIn": 30});
        let AuthResponse::Credentials {
            values,
            expires_at_unix_ms,
        } = credentials(&response, Some(("old", PROFILE))).unwrap()
        else {
            panic!("expected credentials")
        };
        assert_eq!(values["refresh_token"], "old");
        assert_eq!(values["profileArn"], PROFILE);
        assert!(expires_at_unix_ms.is_some());
        let response = json!({"accessToken": "new", "expiresIn": 30,
            "profileArn": "arn:aws:codewhisperer:us-east-1:123456789012:profile/other"});
        assert!(credentials(&response, Some(("old", PROFILE))).is_err());
    }
}
