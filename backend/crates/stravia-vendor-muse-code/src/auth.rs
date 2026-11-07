use crate::{CLIENT_USER_AGENT, auth_error, credential, headers, invalid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};
use stravia_vendor_sdk::*;

const CLIENT: &str = "1031625952748946";
#[derive(Default, Serialize, Deserialize)]
struct State {
    device_code: Option<String>,
    expires_ms: Option<u64>,
    interval: u32,
    next_poll_ms: u64,
    // Preserve a successful exchange before returning it. Retried polls must not
    // mint/onboard again if the host has not yet persisted the returned credentials.
    completed: Option<BTreeMap<String, Value>>,
}
fn now() -> Result<u64, PluginError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .map_err(|_| invalid("System clock is before Unix epoch"))
}
fn save(host: &GuestHost, state: &State) -> Result<(), PluginError> {
    host.write_private_state(
        &serde_json::to_vec(state)
            .map_err(|_| invalid("Muse authentication state could not be encoded"))?,
    )
}
fn load(host: &GuestHost) -> Result<State, PluginError> {
    let bytes = host
        .read_private_state()?
        .ok_or_else(|| auth_error("Muse device login has not started"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| auth_error("Muse authentication state is invalid; restart login"))
}
fn text<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
fn form(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut encoder = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        encoder.append_pair(key, value);
    }
    encoder.finish().into_bytes()
}
fn post(
    host: &GuestHost,
    url: &str,
    token: Option<&str>,
    body: Vec<u8>,
) -> Result<(u16, Value), PluginError> {
    let mut request_headers = token.map(headers).unwrap_or_else(|| {
        let mut fields = net::Fields::new();
        fields.append("accept", b"application/json".to_vec());
        fields.append("x-api-version", b"1.0.0".to_vec());
        fields.append("user-agent", CLIENT_USER_AGENT.as_bytes().to_vec());
        fields.append("accept-encoding", b"gzip, deflate, br, zstd".to_vec());
        fields
    });
    request_headers.append("content-type", if token.is_some() {
        b"application/json".as_slice()
    } else {
        b"application/x-www-form-urlencoded".as_slice()
    }.to_vec());
    let response = host.http_start(net::Request {
        method: "POST".into(),
        url: url.into(),
        headers: request_headers,
        body,
    }, &crate::bun_options())?;
    let status = response.status()?;
    let bytes = read_http_body(&response, 1024 * 1024)?;
    // Never reflect auth response text: it may contain account/device/API tokens.
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| auth_error("Meta authentication returned invalid JSON"))?;
    Ok((status, value))
}
fn credentials(values: BTreeMap<String, Value>) -> AuthResponse {
    AuthResponse::Credentials {
        values,
        expires_at_unix_ms: None,
    }
}
pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start { .. } => start(host),
        AuthStep::Poll => {
            if let (Some(key), Some(token)) = (
                credential(provider, "apiKey"),
                credential(provider, "oauthAccessToken"),
            ) {
                let mut values = provider.credentials.clone();
                values.insert("apiKey".into(), json!(key));
                values.insert("oauthAccessToken".into(), json!(token));
                return Ok(credentials(values));
            }
            poll(host)
        }
        _ => Err(crate::common::unsupported(
            "auth step (Meta does not support refresh or revoke)",
            crate::ID,
            &provider.channel,
        )),
    }
}
fn start(host: &GuestHost) -> Result<AuthResponse, PluginError> {
    // A new explicit login replaces old private state, including cached secrets.
    save(host, &State::default())?;
    let (status, value) = post(
        host,
        "https://auth.meta.com/oidc/device/authorization/",
        None,
        form(&[("client_id", CLIENT)]),
    )?;
    if !(200..300).contains(&status) {
        return Err(auth_error("Meta device authorization failed"));
    }
    let device = text(&value, "device_code")
        .ok_or_else(|| auth_error("Meta device response is missing device_code"))?;
    let code = text(&value, "user_code")
        .ok_or_else(|| auth_error("Meta device response is missing user_code"))?;
    let uri = text(&value, "verification_uri")
        .ok_or_else(|| auth_error("Meta device response is missing verification_uri"))?;
    let url = text(&value, "verification_uri_complete").unwrap_or(uri);
    for address in [uri, url] {
        let parsed =
            url::Url::parse(address).map_err(|_| auth_error("Meta verification URL is invalid"))?;
        if parsed.scheme() != "https"
            || parsed.host_str() != Some("auth.meta.com")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port_or_known_default() != Some(443)
        {
            return Err(auth_error(
                "Meta verification URL is outside the approved authentication origin",
            ));
        }
    }
    let interval = value
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, u32::MAX as u64) as u32;
    let expires = value.get("expires_in").and_then(Value::as_u64);
    let time = now()?;
    save(
        host,
        &State {
            device_code: Some(device.into()),
            expires_ms: expires.map(|seconds| time.saturating_add(seconds.saturating_mul(1000))),
            interval,
            next_poll_ms: time.saturating_add(u64::from(interval) * 1000),
            completed: None,
        },
    )?;
    Ok(AuthResponse::Authorization {
        url: url.into(),
        user_code: Some(code.into()),
        verification_uri: Some(uri.into()),
        interval_seconds: Some(interval),
        state: None,
    })
}
fn poll(host: &GuestHost) -> Result<AuthResponse, PluginError> {
    let mut state = load(host)?;
    if let Some(values) = state.completed.take() {
        return Ok(credentials(values));
    }
    let time = now()?;
    if state.expires_ms.is_some_and(|expires| time >= expires) {
        save(host, &State::default())?;
        return Err(auth_error("Meta device code expired; restart login"));
    }
    let device = state
        .device_code
        .as_deref()
        .ok_or_else(|| auth_error("Muse device login has not started"))?;
    if time < state.next_poll_ms {
        return Ok(AuthResponse::Pending {
            retry_after_seconds: Some(
                (state.next_poll_ms - time)
                    .div_ceil(1000)
                    .min(u32::MAX as u64) as u32,
            ),
        });
    }
    let (status, value) = post(
        host,
        "https://auth.meta.com/oidc/device/token/",
        None,
        form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", CLIENT),
            ("device_code", device),
        ]),
    )?;
    match text(&value, "error") {
        Some("authorization_pending") | Some("slow_down") => {
            if text(&value, "error") == Some("slow_down") {
                state.interval = state.interval.saturating_add(5);
            }
            state.next_poll_ms = time.saturating_add(u64::from(state.interval) * 1000);
            save(host, &state)?;
            return Ok(AuthResponse::Pending {
                retry_after_seconds: Some(state.interval),
            });
        }
        Some("access_denied") => {
            save(host, &State::default())?;
            return Err(auth_error("Meta device authorization was denied"));
        }
        Some("expired_token") => {
            save(host, &State::default())?;
            return Err(auth_error("Meta device code expired; restart login"));
        }
        Some(_) => return Err(auth_error("Meta device token exchange failed")),
        None if !(200..300).contains(&status) => {
            return Err(auth_error("Meta device token exchange failed"));
        }
        None => {}
    }
    let token = text(&value, "access_token")
        .filter(|v| !v.contains(['\r', '\n']))
        .ok_or_else(|| auth_error("Meta token response is missing or has invalid access_token"))?;
    // Consume the device grant before onboarding. A failed mint requires a new
    // explicit login, never an automatic second onboarding/key mint.
    save(host, &State::default())?;
    let (status, value) = post(
        host,
        "https://api.meta.ai/muse-code/key",
        Some(token),
        br#"{"onboard":true}"#.to_vec(),
    )?;
    if !(200..300).contains(&status) {
        return Err(auth_error(
            "Muse subscription key exchange failed; sign in again",
        ));
    }
    if !value.is_object()
        || [
            "api_key",
            "user_id",
            "user_email",
            "require_payment_action_url",
        ]
        .iter()
        .any(|field| value.get(*field).is_some_and(|v| !v.is_string()))
        || ["action_url", "subs_tier_id", "subs_tier_name"]
            .iter()
            .any(|field| {
                value
                    .get(*field)
                    .is_some_and(|v| !v.is_null() && !v.is_string())
            })
        || ["is_subs_active", "require_payment"]
            .iter()
            .any(|field| value.get(*field).is_some_and(|v| !v.is_boolean()))
    {
        return Err(auth_error(
            "Muse subscription response has invalid field types",
        ));
    }
    if value.get("is_subs_active").and_then(Value::as_bool) == Some(false) {
        return Err(auth_error("Muse Code subscription is inactive"));
    }
    if value.get("require_payment").and_then(Value::as_bool) == Some(true)
        || text(&value, "action_url").is_some()
        || text(&value, "require_payment_action_url").is_some()
    {
        return Err(auth_error(
            "Muse Code requires payment or subscription action; complete it in your Meta account and sign in again",
        ));
    }
    let key = text(&value, "api_key")
        .filter(|v| !v.contains(['\r', '\n']))
        .ok_or_else(|| {
            auth_error("Muse subscription response is missing or has invalid api_key")
        })?;
    let email = text(&value, "user_email").map(str::to_lowercase);
    let account = text(&value, "user_id")
        .or(email.as_deref())
        .ok_or_else(|| auth_error("Muse subscription response is missing account identity"))?;
    let mut values = BTreeMap::from([
        ("apiKey".into(), json!(key)),
        ("oauthAccessToken".into(), json!(token)),
        ("accountId".into(), json!(account)),
    ]);
    if let Some(email) = email {
        values.insert("email".into(), json!(email));
    }
    save(
        host,
        &State {
            completed: Some(values.clone()),
            ..State::default()
        },
    )?;
    Ok(credentials(values))
}
