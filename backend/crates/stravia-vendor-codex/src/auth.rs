use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
};

use super::{auth_error, invalid, unsupported};

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email offline_access";
const API_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const MAX_TOKEN_BODY: usize = 512 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct AuthState {
    version: u32,
    code_verifier: String,
    state: String,
    redirect_uri: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start {
            redirect_uri,
            state,
        } => start(host, redirect_uri, state),
        AuthStep::Exchange { callback_url } => exchange(host, callback_url),
        AuthStep::ManualInput { .. } => {
            Err(unsupported("OpenAI Codex OAuth requires a callback URL"))
        }
        AuthStep::Refresh => refresh(host, provider),
        AuthStep::Poll => Err(unsupported("OpenAI Codex OAuth is not a device-code flow")),
        AuthStep::Revoke => Err(unsupported(
            "OpenAI Codex does not expose a token revocation endpoint",
        )),
    }
}

fn start(
    host: &GuestHost,
    redirect_uri: String,
    state: String,
) -> Result<AuthResponse, PluginError> {
    if redirect_uri.trim().is_empty() || state.trim().is_empty() {
        return Err(invalid("OAuth start requires a redirect URI and state"));
    }
    let code_verifier = generate_verifier();
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(code_verifier.as_bytes()));
    let auth_state = AuthState {
        version: 1,
        code_verifier,
        state: state.clone(),
        redirect_uri: redirect_uri.clone(),
        account_id: None,
    };
    host.write_private_state(
        &serde_json::to_vec(&auth_state)
            .map_err(|_| invalid("OAuth state could not be encoded"))?,
    )?;
    let url = build_url(
        AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", &redirect_uri),
            ("scope", SCOPE),
            ("code_challenge", &code_challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
        ],
    );
    Ok(AuthResponse::Authorization {
        url,
        user_code: None,
        verification_uri: Some("https://auth.openai.com".into()),
        interval_seconds: None,
    })
}

fn exchange(host: &GuestHost, callback_url: String) -> Result<AuthResponse, PluginError> {
    let mut state = read_state(host)?;
    let callback = parse_callback(&callback_url)?;
    if callback.get("state").map(String::as_str) != Some(state.state.as_str()) {
        return Err(auth_error("OpenAI OAuth callback state mismatch"));
    }
    if let Some(error) = callback.get("error") {
        let detail = callback.get("error_description").unwrap_or(error);
        return Err(auth_error(format!("OpenAI OAuth was rejected: {detail}")));
    }
    let code = callback
        .get("code")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("OpenAI OAuth callback omitted authorization code"))?;
    let form = encode_form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &state.redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", &state.code_verifier),
    ]);
    let token = token_request(host, form)?;
    let response = credentials_response(token, None)?;
    if let AuthResponse::Credentials { values, .. } = &response {
        state.account_id = values
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        host.write_private_state(
            &serde_json::to_vec(&state)
                .map_err(|_| invalid("OAuth binding could not be encoded"))?,
        )?;
    }
    Ok(response)
}

fn refresh(host: &GuestHost, provider: &ProviderSnapshot) -> Result<AuthResponse, PluginError> {
    let refresh_token = provider
        .credentials
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("OpenAI OAuth refresh token is missing"))?;
    let form = encode_form(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
        ("scope", SCOPE),
    ]);
    credentials_response(token_request(host, form)?, Some(refresh_token))
}

fn token_request(host: &GuestHost, form: String) -> Result<TokenResponse, PluginError> {
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: TOKEN_URL.into(),
        headers: vec![
            (
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("accept".into(), "application/json".into()),
        ],
        body: form.into_bytes(),
    })?;
    let status = response.status()?;
    let body = stravia_vendor_sdk::read_http_body(&response, MAX_TOKEN_BODY)?;
    if !(200..300).contains(&status) {
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let detail = parsed
            .get("error_description")
            .or_else(|| parsed.get("message"))
            .or_else(|| parsed.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("token endpoint rejected the request");
        let code = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = if matches!(code, "invalid_grant" | "access_denied" | "invalid_client")
            || matches!(status, 400 | 401 | 403)
        {
            ErrorKind::Auth
        } else {
            ErrorKind::upstream_unknown()
        };
        return Err(PluginError {
            kind,
            message: format!("OpenAI OAuth token exchange failed: {detail}"),
            upstream_status: Some(status),
        });
    }
    serde_json::from_slice(&body).map_err(|_| auth_error("OpenAI token response is invalid"))
}

fn credentials_response(
    token: TokenResponse,
    fallback_refresh_token: Option<&str>,
) -> Result<AuthResponse, PluginError> {
    let raw = serde_json::to_value(&token)
        .map_err(|_| auth_error("OpenAI token response could not be preserved"))?;
    let access_token = token
        .access_token
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("OpenAI token response omitted access_token"))?;
    let mut values = BTreeMap::new();
    if let Some(account_id) = account_id_from_jwt(&access_token) {
        values.insert("account_id".into(), Value::String(account_id));
    }
    values.insert("access_token".into(), Value::String(access_token));
    if let Some(refresh_token) = token
        .refresh_token
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| fallback_refresh_token.map(str::to_owned))
    {
        values.insert("refresh_token".into(), Value::String(refresh_token));
    }
    values.insert("resource_url".into(), Value::String(API_BASE_URL.into()));
    values.insert(
        "scopes".into(),
        Value::Array(
            token
                .scope
                .unwrap_or_default()
                .split_whitespace()
                .map(|scope| Value::String(scope.into()))
                .collect(),
        ),
    );
    values.insert("raw".into(), raw);
    let expires_at_unix_ms = token
        .expires_in
        .map(|seconds| unix_millis().saturating_add(seconds.max(1).saturating_mul(1000)));
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    })
}

fn read_state(host: &GuestHost) -> Result<AuthState, PluginError> {
    let bytes = host
        .read_private_state()?
        .ok_or_else(|| auth_error("OpenAI OAuth session state is missing"))?;
    let state: AuthState = serde_json::from_slice(&bytes)
        .map_err(|_| auth_error("OpenAI OAuth session state is invalid"))?;
    if state.version != 1 {
        return Err(auth_error(
            "OpenAI OAuth session state version is unsupported",
        ));
    }
    Ok(state)
}

fn generate_verifier() -> String {
    format!(
        "{}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
    )
}

fn account_id_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .or_else(|| claims.get("https://api.openai.com/auth.chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn build_url(base: &str, params: &[(&str, &str)]) -> String {
    format!(
        "{base}?{}",
        params
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&")
    )
}

fn encode_form(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn parse_callback(url: &str) -> Result<BTreeMap<String, String>, PluginError> {
    let raw = url.trim();
    if raw.is_empty() || !raw.contains("://") {
        return Err(invalid("OAuth callback must be an absolute URL"));
    }
    let mut values = BTreeMap::new();
    for section in [
        raw.split_once('?')
            .map(|(_, value)| value.split('#').next().unwrap_or(value)),
        raw.split_once('#').map(|(_, value)| value),
    ]
    .into_iter()
    .flatten()
    {
        for pair in section.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            values
                .entry(percent_decode(key)?)
                .or_insert(percent_decode(value)?);
        }
    }
    Ok(values)
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn percent_decode(value: &str) -> Result<String, PluginError> {
    let mut bytes = Vec::with_capacity(value.len());
    let raw = value.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < raw.len() => {
                let hex = std::str::from_utf8(&raw[index + 1..index + 3])
                    .ok()
                    .and_then(|value| u8::from_str_radix(value, 16).ok())
                    .ok_or_else(|| invalid("OAuth callback contains invalid percent encoding"))?;
                bytes.push(hex);
                index += 3;
            }
            b'%' => return Err(invalid("OAuth callback contains invalid percent encoding")),
            byte => {
                bytes.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| invalid("OAuth callback is not UTF-8"))
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}
