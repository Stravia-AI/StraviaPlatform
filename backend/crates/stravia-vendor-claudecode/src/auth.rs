//! Claude（Pro/Max）订阅 OAuth：授权码 + PKCE 登录、刷新与账号身份补全。
//!
//! 端点、client id、scope 与请求形态跟随 oh-my-pi v18.4.2 的
//! `catalog/src/compat/rules/auth/anthropic.kdl` 与 `registry/oauth/anthropic.ts`。

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, GuestHost, HttpRequest, PluginError,
    ProviderSnapshot, read_http_body,
};

use crate::request::{DEFAULT_CLIENT_VERSION, OAUTH_BETA, SDK_VERSION};
use crate::{auth_error, invalid, unsupported};

const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const BOOTSTRAP_URL: &str =
    "https://api.anthropic.com/api/claude_cli/bootstrap?entrypoint=cli&model=claude-opus-4-8";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPE: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const MAX_TOKEN_BODY: usize = 512 * 1024;

pub(crate) const ACCOUNT_UUID: &str = "account_uuid";
pub(crate) const DEVICE_ID: &str = "device_id";
const EMAIL: &str = "email";
const ORGANIZATION_UUID: &str = "organization_uuid";
const ORGANIZATION_NAME: &str = "organization_name";
/// 刷新响应不重新下发的身份字段；刷新时从已保存凭据继承。
const INHERITED_FIELDS: [&str; 5] = [
    ACCOUNT_UUID,
    DEVICE_ID,
    EMAIL,
    ORGANIZATION_UUID,
    ORGANIZATION_NAME,
];

#[derive(Debug, Serialize, Deserialize)]
struct AuthState {
    version: u32,
    code_verifier: String,
    state: String,
    redirect_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    account: Option<TokenAccount>,
    organization: Option<TokenOrganization>,
}

#[derive(Debug, Deserialize)]
struct TokenAccount {
    uuid: Option<String>,
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenOrganization {
    uuid: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BootstrapResponse {
    oauth_account: Option<BootstrapAccount>,
}

#[derive(Debug, Deserialize)]
struct BootstrapAccount {
    account_uuid: Option<String>,
    account_email: Option<String>,
    organization_uuid: Option<String>,
    organization_name: Option<String>,
}

/// 字段顺序即线上 JSON 顺序（与 oh-my-pi 的令牌请求体一致）。
#[derive(Serialize)]
struct CodeGrant<'a> {
    grant_type: &'a str,
    client_id: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
    state: &'a str,
}

#[derive(Serialize)]
struct RefreshGrant<'a> {
    grant_type: &'a str,
    client_id: &'a str,
    refresh_token: &'a str,
}

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start { redirect_uri, .. } => start(host, redirect_uri),
        AuthStep::Exchange { callback_url } => exchange(host, callback_url),
        AuthStep::Refresh => refresh(host, provider),
        AuthStep::ManualInput { .. } => Err(unsupported("Claude OAuth requires a callback URL")),
        AuthStep::Poll => Err(unsupported("Claude OAuth is not a device-code flow")),
        AuthStep::Revoke => Err(unsupported(
            "Claude does not expose a token revocation endpoint",
        )),
    }
}

/// claude.ai 的授权提交会以 `Invalid request format` 拒绝宿主默认的 28 位字母
/// state；改用 oh-my-pi `OAuthCallbackFlow.generateState` 的 16 字节小写 hex，
/// 并通过 `AuthResponse::Authorization.state` 交给宿主做回调校验。
fn start(host: &GuestHost, redirect_uri: String) -> Result<AuthResponse, PluginError> {
    if redirect_uri.trim().is_empty() {
        return Err(invalid("OAuth start requires a redirect URI"));
    }
    let state = hex(&random_bytes::<16>());
    let code_verifier = base64url(&random_bytes::<96>());
    let code_challenge = base64url(&Sha256::digest(code_verifier.as_bytes()));
    host.write_private_state(
        &serde_json::to_vec(&AuthState {
            version: 1,
            code_verifier,
            state: state.clone(),
            redirect_uri: redirect_uri.clone(),
        })
        .map_err(|_| invalid("OAuth state could not be encoded"))?,
    )?;
    Ok(AuthResponse::Authorization {
        url: authorize_url(&redirect_uri, &code_challenge, &state),
        user_code: None,
        verification_uri: Some("https://claude.ai".into()),
        interval_seconds: None,
        state: Some(state),
    })
}

fn authorize_url(redirect_uri: &str, code_challenge: &str, state: &str) -> String {
    let query = [
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("code", "true"),
    ]
    .iter()
    .map(|(key, value)| format!("{key}={}", form_encode(value)))
    .collect::<Vec<_>>()
    .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

fn exchange(host: &GuestHost, callback_url: String) -> Result<AuthResponse, PluginError> {
    let state = read_state(host)?;
    let callback = parse_callback(&callback_url)?;
    if callback.get("state").map(String::as_str) != Some(state.state.as_str()) {
        return Err(auth_error("Claude OAuth callback state mismatch"));
    }
    if let Some(error) = callback.get("error") {
        let detail = callback.get("error_description").unwrap_or(error);
        return Err(auth_error(format!("Claude OAuth was rejected: {detail}")));
    }
    let code = callback
        .get("code")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("Claude OAuth callback omitted authorization code"))?;
    let body = serde_json::to_vec(&CodeGrant {
        grant_type: "authorization_code",
        client_id: CLIENT_ID,
        code,
        redirect_uri: &state.redirect_uri,
        code_verifier: &state.code_verifier,
        state: &state.state,
    })
    .map_err(|_| invalid("Claude OAuth token request could not be encoded"))?;
    let token = token_request(host, body, Vec::new())?;
    let (mut values, expires_at_unix_ms) = token_values(token, None)?;
    values.insert(DEVICE_ID.into(), Value::String(hex(&random_bytes::<32>())));
    complete_identity(host, &mut values);
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    })
}

fn refresh(host: &GuestHost, provider: &ProviderSnapshot) -> Result<AuthResponse, PluginError> {
    let refresh_token = provider
        .credentials
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("Claude OAuth refresh token is missing"))?;
    let body = serde_json::to_vec(&RefreshGrant {
        grant_type: "refresh_token",
        client_id: CLIENT_ID,
        refresh_token,
    })
    .map_err(|_| invalid("Claude OAuth refresh request could not be encoded"))?;
    let token = token_request(
        host,
        body,
        vec![
            ("anthropic-beta".into(), OAUTH_BETA.into()),
            (
                "user-agent".into(),
                format!("anthropic-sdk-typescript/{SDK_VERSION} userOAuthProvider"),
            ),
        ],
    )?;
    let (mut values, expires_at_unix_ms) = token_values(token, Some(refresh_token))?;
    // 令牌所属组织在登录时确定；刷新只补齐缺失字段，不改写已保存身份。
    for key in INHERITED_FIELDS {
        if let Some(value) = provider
            .credentials
            .get(key)
            .filter(|value| !value.is_null())
        {
            values.insert(key.into(), value.clone());
        }
    }
    if !values.contains_key(DEVICE_ID) {
        values.insert(DEVICE_ID.into(), Value::String(hex(&random_bytes::<32>())));
    }
    complete_identity(host, &mut values);
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    })
}

fn token_request(
    host: &GuestHost,
    body: Vec<u8>,
    extra_headers: Vec<(String, String)>,
) -> Result<TokenResponse, PluginError> {
    let mut headers = vec![
        ("accept".into(), "application/json".into()),
        ("content-type".into(), "application/json".into()),
    ];
    headers.extend(extra_headers);
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: TOKEN_URL.into(),
        headers,
        body,
    })?;
    let status = response.status()?;
    let body = read_http_body(&response, MAX_TOKEN_BODY)?;
    if !(200..300).contains(&status) {
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let detail = parsed
            .get("error_description")
            .or_else(|| parsed.pointer("/error/message"))
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
            message: format!("Claude OAuth token request failed: {detail}"),
            upstream_status: Some(status),
        });
    }
    serde_json::from_slice(&body).map_err(|_| auth_error("Claude token response is invalid"))
}

fn non_empty(value: Option<String>) -> Option<Value> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(Value::String)
}

fn token_values(
    token: TokenResponse,
    fallback_refresh_token: Option<&str>,
) -> Result<(BTreeMap<String, Value>, Option<i64>), PluginError> {
    let access_token = non_empty(token.access_token)
        .ok_or_else(|| auth_error("Claude token response omitted access_token"))?;
    let mut values = BTreeMap::from([("access_token".to_owned(), access_token)]);
    if let Some(refresh_token) = non_empty(token.refresh_token)
        .or_else(|| fallback_refresh_token.map(|value| Value::String(value.to_owned())))
    {
        values.insert("refresh_token".into(), refresh_token);
    }
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
    let (account_uuid, email) = token
        .account
        .map(|account| (account.uuid, account.email_address))
        .unwrap_or_default();
    let (organization_uuid, organization_name) = token
        .organization
        .map(|organization| (organization.uuid, organization.name))
        .unwrap_or_default();
    for (key, value) in [
        (ACCOUNT_UUID, account_uuid),
        (EMAIL, email),
        (ORGANIZATION_UUID, organization_uuid),
        (ORGANIZATION_NAME, organization_name),
    ] {
        if let Some(value) = non_empty(value) {
            values.insert(key.into(), value);
        }
    }
    let expires_at_unix_ms = token
        .expires_in
        .map(|seconds| unix_millis().saturating_add(seconds.max(1).saturating_mul(1000)));
    Ok((values, expires_at_unix_ms))
}

/// 令牌响应缺少账号身份时向 Claude Code 启动接口补取。身份只用于请求元数据，
/// 缺失不影响推理，因此该接口失败时保持登录成功，只是不写入这些字段。
fn complete_identity(host: &GuestHost, values: &mut BTreeMap<String, Value>) {
    if values.contains_key(ACCOUNT_UUID) && values.contains_key(EMAIL) {
        return;
    }
    let Some(access_token) = values.get("access_token").and_then(Value::as_str) else {
        return;
    };
    let Some(account) = fetch_bootstrap(host, access_token) else {
        return;
    };
    for (key, value) in [
        (ACCOUNT_UUID, account.account_uuid),
        (EMAIL, account.account_email),
        (ORGANIZATION_UUID, account.organization_uuid),
        (ORGANIZATION_NAME, account.organization_name),
    ] {
        if let Some(value) = non_empty(value) {
            values.entry(key.into()).or_insert(value);
        }
    }
}

fn fetch_bootstrap(host: &GuestHost, access_token: &str) -> Option<BootstrapAccount> {
    let response = host
        .http_start(HttpRequest {
            method: "GET".into(),
            url: BOOTSTRAP_URL.into(),
            headers: vec![
                ("accept".into(), "application/json, text/plain, */*".into()),
                ("authorization".into(), format!("Bearer {access_token}")),
                ("content-type".into(), "application/json".into()),
                (
                    "user-agent".into(),
                    format!("claude-code/{DEFAULT_CLIENT_VERSION}"),
                ),
                ("anthropic-beta".into(), OAUTH_BETA.into()),
            ],
            body: Vec::new(),
        })
        .ok()?;
    if !(200..300).contains(&response.status().ok()?) {
        return None;
    }
    let body = read_http_body(&response, MAX_TOKEN_BODY).ok()?;
    serde_json::from_slice::<BootstrapResponse>(&body)
        .ok()?
        .oauth_account
}

fn read_state(host: &GuestHost) -> Result<AuthState, PluginError> {
    let bytes = host
        .read_private_state()?
        .ok_or_else(|| auth_error("Claude OAuth session state is missing"))?;
    let state: AuthState = serde_json::from_slice(&bytes)
        .map_err(|_| auth_error("Claude OAuth session state is invalid"))?;
    if state.version != 1 {
        return Err(auth_error(
            "Claude OAuth session state version is unsupported",
        ));
    }
    Ok(state)
}

/// 宿主的 Wasm 运行时为 `uuid` v4 提供密码学随机源；每个 v4 UUID 贡献 122
/// 位随机量，这里只取足够字节，不依赖额外的随机数 crate。
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0_u8; N];
    for chunk in bytes.chunks_mut(16) {
        let uuid = uuid::Uuid::new_v4();
        chunk.copy_from_slice(&uuid.as_bytes()[..chunk.len()]);
    }
    bytes
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `application/x-www-form-urlencoded`：与浏览器 `URLSearchParams` 相同，空格
/// 编码为 `+`，只保留 `*-._` 与字母数字。
fn form_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                encoded.push(char::from(byte));
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn parse_callback(url: &str) -> Result<BTreeMap<String, String>, PluginError> {
    let raw = url.trim();
    if raw.is_empty() || !raw.contains("://") {
        return Err(invalid("OAuth callback must be an absolute URL"));
    }
    let mut values = BTreeMap::new();
    let Some((_, query)) = raw.split_once('?') else {
        return Ok(values);
    };
    for pair in query.split('#').next().unwrap_or(query).split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        values
            .entry(form_decode(key)?)
            .or_insert(form_decode(value)?);
    }
    Ok(values)
}

fn form_decode(value: &str) -> Result<String, PluginError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| invalid("OAuth callback contains invalid percent-encoding"))?;
                decoded.push(hex);
                index += 2;
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).map_err(|_| invalid("OAuth callback is not valid UTF-8"))
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_matches_claude_code_parameter_order_and_encoding() {
        let url = authorize_url("http://localhost:54545/callback", "challenge", "state-1");
        assert_eq!(
            url,
            "https://claude.ai/oauth/authorize?client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e\
             &response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback\
             &scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload\
             &code_challenge=challenge&code_challenge_method=S256&state=state-1&code=true"
        );
    }

    #[test]
    fn token_request_bodies_keep_wire_field_order() {
        let body = serde_json::to_string(&CodeGrant {
            grant_type: "authorization_code",
            client_id: CLIENT_ID,
            code: "c",
            redirect_uri: "r",
            code_verifier: "v",
            state: "s",
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"grant_type":"authorization_code","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","code":"c","redirect_uri":"r","code_verifier":"v","state":"s"}"#
        );
    }

    #[test]
    fn callback_parsing_decodes_query_and_ignores_fragment() {
        let values =
            parse_callback("http://localhost:54545/callback?code=a%2Bb+c&state=s#fragment")
                .unwrap();
        assert_eq!(values["code"], "a+b c");
        assert_eq!(values["state"], "s");
        assert!(parse_callback("code#state").is_err());
    }

    #[test]
    fn token_values_map_account_and_organization_identity() {
        let token: TokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "sk-ant-oat01-x",
            "expires_in": 3600,
            "scope": "user:inference user:profile",
            "account": {"uuid": "acc", "email_address": "a@example.com"},
            "organization": {"uuid": "org", "name": "Org"}
        }))
        .unwrap();
        let (values, expires_at) = token_values(token, Some("old-refresh")).unwrap();
        assert!(expires_at.is_some_and(|at| at > unix_millis()));
        assert_eq!(values["refresh_token"], "old-refresh");
        assert_eq!(values[ACCOUNT_UUID], "acc");
        assert_eq!(values[EMAIL], "a@example.com");
        assert_eq!(values[ORGANIZATION_UUID], "org");
        assert_eq!(values[ORGANIZATION_NAME], "Org");
        assert_eq!(
            values["scopes"],
            serde_json::json!(["user:inference", "user:profile"])
        );
    }
}
