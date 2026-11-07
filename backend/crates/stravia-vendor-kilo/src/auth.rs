use super::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize)]
struct DeviceState {
    code: String,
    deadline_ms: i64,
}

fn approved(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    token: &str,
    expires: Option<i64>,
) -> Result<AuthResponse, PluginError> {
    if token.trim().is_empty() {
        return Err(fail(
            ErrorKind::Auth,
            "Kilo authorization returned no token",
        ));
    }
    let mut authenticated = provider.clone();
    authenticated.credentials = BTreeMap::from([("apiKey".into(), json!(token))]);
    // Profile requests deliberately have no organization header: verify membership first.
    authenticated.options.remove("organizationId");
    let profile = http_json(host, &authenticated, "GET", "/api/profile")?;
    let email = profile
        .pointer("/user/email")
        .or_else(|| profile.get("email"))
        .and_then(Value::as_str);
    let name = profile
        .pointer("/user/name")
        .or_else(|| profile.get("name"))
        .and_then(Value::as_str);
    if email.is_none() && name.is_none() {
        return Err(fail(ErrorKind::Auth, "Kilo profile has no identity"));
    }
    let mut values = authenticated.credentials;
    if let Some(email) = email {
        values.insert("email".into(), json!(email));
    }
    if let Some(name) = name {
        values.insert("name".into(), json!(name));
    }
    if let Some(org) = organization(provider) {
        if !profile
            .get("organizations")
            .and_then(Value::as_array)
            .is_some_and(|orgs| {
                orgs.iter()
                    .any(|v| v.get("id").and_then(Value::as_str) == Some(org))
            })
        {
            return Err(fail(
                ErrorKind::Auth,
                "Selected Kilo organization is not in the authenticated profile",
            ));
        }
        values.insert("organizationId".into(), json!(org));
    }
    if let Some(orgs) = profile.get("organizations") {
        values.insert("organizations".into(), orgs.clone());
    }
    if let Some(expires) = expires {
        values.insert("expiresAtUnixMs".into(), json!(expires));
    }
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms: expires,
    })
}
pub(super) fn poll_status(status: u16) -> Result<bool, PluginError> {
    match status {
        202 => Ok(false),
        403 => Err(fail(ErrorKind::Auth, "Kilo authorization denied")),
        410 => Err(fail(ErrorKind::Auth, "Kilo authorization expired")),
        _ => Ok(true),
    }
}
pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    step: AuthStep,
) -> Result<AuthResponse, PluginError> {
    match step {
        AuthStep::Start { .. } => {
            // 设备码签发早于登录，必须不带旧令牌和组织头。
            let mut unauthenticated = provider.clone();
            unauthenticated.credentials.clear();
            unauthenticated.options.remove("organizationId");
            let value = http_json(host, &unauthenticated, "POST", "/api/device-auth/codes")?;
            let code = value
                .get("code")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| fail(ErrorKind::Invalid, "Kilo device code missing"))?;
            let url = value
                .get("verificationUrl")
                .and_then(Value::as_str)
                .filter(|s| s.starts_with("https://"))
                .ok_or_else(|| {
                    fail(
                        ErrorKind::Invalid,
                        "Kilo verification URL missing or insecure",
                    )
                })?;
            let seconds = value
                .get("expiresIn")
                .and_then(Value::as_i64)
                .filter(|v| *v > 0)
                .ok_or_else(|| fail(ErrorKind::Invalid, "Kilo device expiry missing"))?;
            let deadline_ms = seconds
                .checked_mul(1000)
                .and_then(|v| chrono::Utc::now().timestamp_millis().checked_add(v))
                .ok_or_else(|| fail(ErrorKind::Invalid, "Kilo expiry overflow"))?;
            host.write_private_state(
                &serde_json::to_vec(&DeviceState {
                    code: code.into(),
                    deadline_ms,
                })
                .map_err(|e| fail(ErrorKind::Invalid, e.to_string()))?,
            )?;
            Ok(AuthResponse::Authorization {
                url: url.into(),
                user_code: Some(code.into()),
                verification_uri: Some(url.into()),
                interval_seconds: Some(3),
                state: None,
            })
        }
        AuthStep::Poll => {
            let bytes = host
                .read_private_state()?
                .ok_or_else(|| fail(ErrorKind::Auth, "Start Kilo login first"))?;
            let state: DeviceState = serde_json::from_slice(&bytes)
                .map_err(|e| fail(ErrorKind::Invalid, e.to_string()))?;
            if state.deadline_ms <= chrono::Utc::now().timestamp_millis() {
                return Err(fail(ErrorKind::Auth, "Kilo device code expired"));
            }
            let response = host.http_start(stravia_vendor_sdk::net::Request {
                method: "GET".into(),
                url: format!("{ORIGIN}/api/device-auth/codes/{}", escape(&state.code)),
                headers: crate::net_fields(vec![]),
                body: vec![],
            }, &crate::net_options())?;
            let status = response.status()?;
            if !poll_status(status)? {
                return Ok(AuthResponse::Pending {
                    retry_after_seconds: Some(3),
                });
            }
            let headers = response.headers()?;
            let bytes = read_http_body(&response, MAX_BODY)?;
            if !(200..300).contains(&status) {
                return Err(common::upstream_error(status, &headers, &bytes));
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| fail(ErrorKind::Invalid, e.to_string()))?;
            match value.get("status").and_then(Value::as_str) {
                Some("pending") => {
                    return Ok(AuthResponse::Pending {
                        retry_after_seconds: Some(3),
                    });
                }
                Some("denied") | Some("expired") => {
                    return Err(fail(
                        ErrorKind::Auth,
                        "Kilo authorization denied or expired",
                    ));
                }
                Some("approved") => {}
                _ => {
                    return Err(fail(
                        ErrorKind::Invalid,
                        "Unknown Kilo device authorization status",
                    ));
                }
            }
            let token = value
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(ErrorKind::Auth, "Kilo authorization token missing"))?;
            let result = approved(
                host,
                provider,
                token,
                Some(chrono::Utc::now().timestamp_millis() + 365 * 24 * 60 * 60 * 1000),
            )?;
            host.write_private_state(b"null")?;
            Ok(result)
        }
        AuthStep::Refresh => {
            let token = token(provider)?;
            approved(
                host,
                provider,
                token,
                provider
                    .credentials
                    .get("expiresAtUnixMs")
                    .and_then(Value::as_i64),
            )
        }
        _ => Err(fail(
            ErrorKind::Unsupported,
            "Kilo has no manual input, refresh grant, revoke or callback exchange endpoint",
        )),
    }
}
