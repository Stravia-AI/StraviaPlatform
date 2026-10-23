use std::collections::BTreeMap;

use base64::Engine as _;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
    read_http_body,
};

const CLIENT_ID: &str = "3GUryQ7ldAeKEuD2obYnppsnmj58eP5u";
const WEBAPP_URL: &str = "https://app.devin.ai";
const AUTHORIZE_URL: &str = "https://app.devin.ai/auth/cli/continue";
const TOKEN_URL: &str = "https://server.codeium.com/exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode";
const MANUAL_REDIRECT_URI: &str = "chisel-show-auth-token";
const MAX_AUTH_BODY: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct PendingAuth {
    state: String,
    redirect_uri: String,
    code_verifier: String,
    manual: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenResponse {
    session_token: Option<String>,
    api_key: Option<String>,
    windsurf_api_key: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    api_server_url: Option<String>,
}

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start {
            redirect_uri,
            state,
        } => start(host, redirect_uri, state),
        AuthStep::Exchange { callback_url } => exchange(host, provider, &callback_url),
        AuthStep::Refresh => refresh(host, provider),
        AuthStep::ManualInput { value } => exchange(host, provider, &value),
        AuthStep::Poll => Err(error(
            ErrorKind::Unsupported,
            "Devin OAuth does not use device polling",
        )),
        AuthStep::Revoke => Err(error(
            ErrorKind::Unsupported,
            "Devin exposes no token revocation endpoint",
        )),
    }
}

fn start(
    host: &GuestHost,
    redirect_uri: String,
    state: String,
) -> Result<AuthResponse, PluginError> {
    let redirect_uri = if redirect_uri.trim().is_empty() {
        MANUAL_REDIRECT_URI.to_string()
    } else {
        redirect_uri
    };
    if state.trim().is_empty() {
        return Err(error(ErrorKind::Invalid, "OAuth state must not be empty"));
    }
    let manual = redirect_uri == MANUAL_REDIRECT_URI;
    let code_verifier = derive_verifier(&state, &redirect_uri);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(code_verifier.as_bytes()));
    let auth_url = if manual {
        // 与 `devin auth login --force-manual-token-flow` 打印的链接一致：不带 redirect_uri，
        // 页面登录后直接展示授权码，由用户粘贴回来再用 code_verifier 兑换。
        build_url(
            AUTHORIZE_URL,
            &[
                ("state", &state),
                ("prompt", "select_account"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("cli_pkce_marker", "1"),
            ],
        )
    } else {
        build_url(
            AUTHORIZE_URL,
            &[
                ("response_type", "code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", &redirect_uri),
                ("state", &state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("cli_pkce_marker", "1"),
                ("prompt", "select_account"),
                ("redirect_parameters_type", "query"),
            ],
        )
    };
    let pending = serde_json::to_vec(&PendingAuth {
        state,
        redirect_uri,
        code_verifier,
        manual,
    })
    .map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Devin auth state: {err}"),
        )
    })?;
    host.write_private_state(&pending)?;
    Ok(AuthResponse::Authorization {
        url: auth_url,
        user_code: None,
        verification_uri: Some(WEBAPP_URL.to_string()),
        interval_seconds: None,
        state: None,
    })
}

fn exchange(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    callback_url: &str,
) -> Result<AuthResponse, PluginError> {
    let pending = read_pending(host)?;
    if pending.manual {
        let code = manual_code(callback_url).ok_or_else(|| {
            error(
                ErrorKind::Invalid,
                "manual Devin input is missing the authorization code",
            )
        })?;
        return exchange_code(host, provider, &pending.code_verifier, &code);
    }
    let callback = url::Url::parse(callback_url)
        .map_err(|_| error(ErrorKind::Invalid, "invalid Devin OAuth callback URL"))?;
    let mut params: BTreeMap<String, String> = callback.query_pairs().into_owned().collect();
    if let Some(fragment) = callback.fragment() {
        for (key, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
            params
                .entry(key.into_owned())
                .or_insert_with(|| value.into_owned());
        }
    }
    if params.get("state") != Some(&pending.state) {
        return Err(error(
            ErrorKind::Invalid,
            "Devin OAuth callback state mismatch",
        ));
    }
    if let Some(message) = params
        .get("error_description")
        .or_else(|| params.get("error"))
    {
        return Err(error(
            ErrorKind::Auth,
            format!("Devin authorization failed: {message}"),
        ));
    }
    let code = params
        .get("code")
        .map(String::as_str)
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .ok_or_else(|| {
            error(
                ErrorKind::Invalid,
                "Devin OAuth callback is missing the authorization code",
            )
        })?;
    exchange_code(host, provider, &pending.code_verifier, code)
}

fn exchange_code(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    code_verifier: &str,
    code: &str,
) -> Result<AuthResponse, PluginError> {
    let body = serde_json::to_vec(&serde_json::json!({
        "code": code,
        "codeVerifier": code_verifier,
    }))
    .map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Devin token request: {err}"),
        )
    })?;
    token_request(host, TOKEN_URL, body, None, provider, true)
}

fn refresh(host: &GuestHost, provider: &ProviderSnapshot) -> Result<AuthResponse, PluginError> {
    let refresh_token =
        credential(provider, &["refresh_token", "refreshToken"]).ok_or_else(|| {
            error(
                ErrorKind::Auth,
                "Devin session token is not refreshable; authenticate again",
            )
        })?;
    let body = serde_json::to_vec(&serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    }))
    .map_err(|err| {
        error(
            ErrorKind::Trapped,
            format!("encode Devin refresh request: {err}"),
        )
    })?;
    token_request(host, TOKEN_URL, body, Some(refresh_token), provider, false)
}

fn token_request(
    host: &GuestHost,
    url: &str,
    body: Vec<u8>,
    fallback_refresh_token: Option<&str>,
    provider: &ProviderSnapshot,
    connect_protocol: bool,
) -> Result<AuthResponse, PluginError> {
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
    ];
    if connect_protocol {
        headers.push(("connect-protocol-version".into(), "1".into()));
    }
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers,
        body,
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let bytes = read_http_body(&response, MAX_AUTH_BODY)?;
    if !(200..300).contains(&status) {
        return Err(stravia_vendor_common::common::oauth_token_error(
            status, &headers, &bytes,
        ));
    }
    let raw: Value = serde_json::from_slice(&bytes).map_err(|err| {
        error(
            ErrorKind::upstream_unknown(),
            format!("invalid Devin token response: {err}"),
        )
    })?;
    let token: TokenResponse = serde_json::from_value(raw.clone()).map_err(|err| {
        error(
            ErrorKind::upstream_unknown(),
            format!("invalid Devin token response: {err}"),
        )
    })?;
    let access_token = [
        token.session_token,
        token.api_key,
        token.windsurf_api_key,
        token.access_token,
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.trim().is_empty())
    .ok_or_else(|| {
        error(
            ErrorKind::upstream_unknown(),
            "Devin token response is missing a session token",
        )
    })?;
    let refresh_token = token
        .refresh_token
        .filter(|value| !value.trim().is_empty())
        .or_else(|| fallback_refresh_token.map(str::to_string));
    let resource_url = match token
        .api_server_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) if value.trim_end_matches('/') == "https://server.codeium.com" => {
            "https://server.codeium.com".to_string()
        }
        Some(value) => {
            return Err(error(
                ErrorKind::upstream_unknown(),
                format!("Devin token response returned undeclared api_server_url `{value}`"),
            ));
        }
        None => provider_base_url(provider),
    };
    let mut response = credentials_response(
        access_token,
        refresh_token,
        resource_url,
        token.expires_in,
        token.scope,
    );
    if let AuthResponse::Credentials { values, .. } = &mut response {
        values.insert("raw".into(), raw);
    }
    Ok(response)
}

fn credentials_response(
    access_token: String,
    refresh_token: Option<String>,
    resource_url: String,
    expires_in: Option<i64>,
    scope: Option<String>,
) -> AuthResponse {
    let mut values = BTreeMap::new();
    values.insert("session_token".into(), Value::String(access_token.clone()));
    values.insert("access_token".into(), Value::String(access_token.clone()));
    values.insert("apiKey".into(), Value::String(access_token));
    values.insert("resource_url".into(), Value::String(resource_url));
    if let Some(refresh_token) = refresh_token {
        values.insert("refresh_token".into(), Value::String(refresh_token));
    }
    if let Some(scope) = scope.filter(|scope| !scope.trim().is_empty()) {
        values.insert("scope".into(), Value::String(scope));
    }
    if let Some(expires_in) = expires_in {
        values.insert("expires_in".into(), Value::from(expires_in.max(1)));
    }
    AuthResponse::Credentials {
        values,
        expires_at_unix_ms: expires_in.map(|seconds| {
            Utc::now()
                .timestamp_millis()
                .saturating_add(seconds.max(1).saturating_mul(1_000))
        }),
    }
}

fn read_pending(host: &GuestHost) -> Result<PendingAuth, PluginError> {
    let bytes = host
        .read_private_state()?
        .ok_or_else(|| error(ErrorKind::Invalid, "Devin auth session state is missing"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| error(ErrorKind::Invalid, "Devin auth session state is invalid"))
}

fn credential<'a>(provider: &'a ProviderSnapshot, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        provider
            .credentials
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn provider_base_url(_provider: &ProviderSnapshot) -> String {
    "https://server.codeium.com".into()
}

fn derive_verifier(state: &str, redirect_uri: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"stravia:devin:pkce:v1\0");
    digest.update(uuid::Uuid::new_v4().as_bytes());
    digest.update(uuid::Uuid::new_v4().as_bytes());
    digest.update(state.as_bytes());
    digest.update([0]);
    digest.update(redirect_uri.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.finalize())
}

fn build_url(base: &str, params: &[(&str, &str)]) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().copied())
        .finish();
    format!("{base}?{query}")
}

/// 手动模式粘贴的内容：授权页展示的授权码本身，或包含 `code=` 的回调 URL/查询串。
fn manual_code(input: &str) -> Option<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }
    for part in raw.split(['?', '#', '&']) {
        if let Some(value) = part.strip_prefix("code=") {
            let decoded = urlencoding::decode(value).ok()?.trim().to_string();
            return (!decoded.is_empty()).then_some(decoded);
        }
    }
    // 没有 `code=` 的 URL/查询串不是授权码，不能原样送去兑换。
    (!raw.contains("://") && !raw.contains('&')).then(|| raw.to_string())
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::manual_code;

    #[test]
    fn manual_code_accepts_a_bare_code_or_a_code_parameter() {
        assert_eq!(manual_code("  abc-123 \n").as_deref(), Some("abc-123"));
        assert_eq!(manual_code("code=abc%2B123").as_deref(), Some("abc+123"));
        assert_eq!(
            manual_code("http://localhost/cb?state=s&code=abc#frag").as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn manual_code_rejects_input_without_a_code() {
        assert_eq!(manual_code("  "), None);
        assert_eq!(manual_code("code="), None);
        assert_eq!(manual_code("https://app.devin.ai/auth?state=s"), None);
        assert_eq!(manual_code("state=s&other=1"), None);
    }
}
