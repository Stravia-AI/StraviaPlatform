use crate::client;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};
use stravia_vendor_common::common::{plugin_error, upstream_error};
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
    read_http_body,
};

// 官方 CLI 1.2.16 安装包中的公开 native-client 参数，不是用户凭据。
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REDIRECT_URI: &str = "https://antigravity.google/oauth-callback";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs https://www.googleapis.com/auth/aicode openid";

#[derive(Serialize, Deserialize)]
pub(crate) struct Session {
    version: u32,
    state: String,
    verifier: String,
    redirect_uri: String,
    #[serde(default)]
    pub(crate) values: BTreeMap<String, Value>,
    #[serde(default)]
    expires_at_unix_ms: Option<i64>,
    #[serde(default)]
    pub(crate) operation: Option<String>,
    provider_id: String,
}

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start { .. } => start(host, provider),
        AuthStep::Exchange { callback_url } => {
            let session = load(host, provider)?;
            let callback = callback(&callback_url)?;
            if callback.get("state") != Some(&session.state) {
                return Err(auth("Google OAuth callback state mismatch"));
            }
            if callback.contains_key("error") {
                return Err(auth("Google OAuth authorization was rejected"));
            }
            let code = callback
                .get("code")
                .ok_or_else(|| auth("Google OAuth callback omitted authorization code"))?;
            exchange(host, provider, session, code)
        }
        AuthStep::ManualInput { value } => {
            let session = load(host, provider)?;
            exchange(host, provider, session, value.trim())
        }
        AuthStep::Refresh => refresh(host, provider),
        AuthStep::Poll | AuthStep::Revoke => Err(plugin_error(
            ErrorKind::Unsupported,
            "Antigravity OAuth does not support this authentication step",
        )),
    }
}

fn auth(message: &str) -> PluginError {
    plugin_error(ErrorKind::Auth, message)
}
fn invalid(message: &str) -> PluginError {
    plugin_error(ErrorKind::Invalid, message)
}

fn start(host: &GuestHost, provider: &ProviderSnapshot) -> Result<AuthResponse, PluginError> {
    let session = Session {
        version: 1,
        state: random(),
        verifier: random(),
        redirect_uri: REDIRECT_URI.into(),
        values: BTreeMap::new(),
        expires_at_unix_ms: None,
        operation: None,
        provider_id: provider.provider_id.clone(),
    };
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(session.verifier.as_bytes()));
    let mut authorize = url::Url::parse(AUTHORIZE_URL)
        .map_err(|_| invalid("OAuth authorization endpoint is invalid"))?;
    authorize.query_pairs_mut().extend_pairs([
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("response_type", "code"),
        ("scope", SCOPES),
        ("state", &session.state),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    save(host, &session)?;
    Ok(AuthResponse::Authorization {
        url: authorize.into(),
        user_code: None,
        verification_uri: None,
        interval_seconds: None,
        state: Some(session.state),
    })
}

fn exchange(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    mut session: Session,
    code: &str,
) -> Result<AuthResponse, PluginError> {
    if code.trim().is_empty() || code.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(invalid(
            "Google OAuth authorization code is missing or malformed",
        ));
    }
    if session.values.is_empty() {
        if session.verifier.is_empty() {
            return Err(auth("OAuth authorization session is invalid"));
        }
        let token = token_request(
            host,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("code", code),
                ("redirect_uri", &session.redirect_uri),
                ("code_verifier", &session.verifier),
            ],
        )?;
        let (values, expires) = token_values(&token, BTreeMap::new(), true)?;
        session.values = values;
        session.expires_at_unix_ms = expires;
        session.verifier.clear();
        session.state.clear();
        save(host, &session)?;
    }
    // 宿主只允许 Credentials 完成手动授权；项目异步创建由后续真实发现请求推进。
    let mut account = provider.clone();
    account.credentials = session.values.clone();
    let discovery = client::discover_project(host, &account, &mut session)?;
    if let Some(project) = discovery {
        session.values.insert("project_id".into(), json!(project));
    }
    save(host, &session)?;
    Ok(AuthResponse::Credentials {
        values: session.values,
        expires_at_unix_ms: session.expires_at_unix_ms,
    })
}

fn refresh(host: &GuestHost, provider: &ProviderSnapshot) -> Result<AuthResponse, PluginError> {
    let refresh = client::credential(provider, "refresh_token").ok_or_else(|| {
        auth("Google OAuth refresh token is missing; authorize this account again")
    })?;
    let mut binding = optional_session(host, provider)?;
    let mut previous = provider.credentials.clone();
    if let Some(session) = &binding {
        for key in ["project_id", "tier", "account_id", "email", "name"] {
            if let Some(value) = session.values.get(key) {
                previous.entry(key.into()).or_insert_with(|| value.clone());
            }
        }
    }
    let token = token_request(
        host,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("refresh_token", refresh),
        ],
    )?;
    let (values, expires_at_unix_ms) = token_values(&token, previous, false)?;
    if let Some(session) = &mut binding {
        session.values = values.clone();
        session.expires_at_unix_ms = expires_at_unix_ms;
        save(host, session)?;
    }
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    })
}

fn token_request(host: &GuestHost, params: &[(&str, &str)]) -> Result<Value, PluginError> {
    let form: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().copied())
        .finish();
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: TOKEN_URL.into(),
        headers: vec![(
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        )],
        body: form.into_bytes(),
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, client::MAX_JSON_BODY)?;
    if !(200..300).contains(&status) {
        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let permanent = status < 500
            && matches!(
                value.get("error").and_then(Value::as_str),
                Some(
                    "invalid_grant"
                        | "invalid_client"
                        | "unauthorized_client"
                        | "access_denied"
                        | "invalid_scope"
                )
            );
        let mut error = upstream_error(status, &headers, &body);
        if permanent {
            error.kind = ErrorKind::Auth;
            error.message =
                "Google OAuth authorization is no longer valid; authorize this account again"
                    .into();
        }
        return Err(error);
    }
    serde_json::from_slice(&body).map_err(|_| auth("Google OAuth token response is malformed"))
}

fn token_values(
    token: &Value,
    mut values: BTreeMap<String, Value>,
    require_refresh: bool,
) -> Result<(BTreeMap<String, Value>, Option<i64>), PluginError> {
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.trim().is_empty() && !value.bytes().any(|byte| byte.is_ascii_control())
        })
        .ok_or_else(|| auth("Google OAuth token response omitted valid access_token"))?;
    if token
        .get("token_type")
        .and_then(Value::as_str)
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(auth("Google OAuth returned an unsupported token type"));
    }
    values.insert("access_token".into(), Value::String(access.into()));
    if let Some(refresh) = token
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        values.insert("refresh_token".into(), Value::String(refresh.into()));
    }
    if require_refresh && !values.contains_key("refresh_token") {
        return Err(auth(
            "Google OAuth omitted refresh_token; authorize with consent again",
        ));
    }
    if let Some(scope) = token.get("scope").and_then(Value::as_str) {
        values.insert(
            "scopes".into(),
            json!(scope.split_whitespace().collect::<Vec<_>>()),
        );
    }
    // 身份仅供显示和绑定，JWT 声明不参与授权判定；不保存完整 id_token。
    if let Some(claims) = token
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(identity_claims)
    {
        for (claim, key) in [("sub", "account_id"), ("email", "email"), ("name", "name")] {
            if let Some(value) = claims
                .get(claim)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                values.insert(key.into(), json!(value));
            }
        }
    }
    let expires = match token.get("expires_in") {
        Some(value) => {
            let seconds = value
                .as_i64()
                .filter(|seconds| *seconds > 0)
                .ok_or_else(|| auth("Google OAuth token expiry is malformed"))?;
            Some(now().saturating_add(seconds.saturating_mul(1000)))
        }
        None => None,
    };
    Ok((values, expires))
}
fn identity_claims(token: &str) -> Option<Value> {
    let part = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn bound_project(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<Option<String>, PluginError> {
    let mut session = optional_session(host, provider)?.unwrap_or_else(|| Session {
        version: 1,
        state: String::new(),
        verifier: String::new(),
        redirect_uri: REDIRECT_URI.into(),
        values: provider.credentials.clone(),
        expires_at_unix_ms: None,
        operation: None,
        provider_id: provider.provider_id.clone(),
    });
    if let Some(project) = session
        .values
        .get("project_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(project.into()));
    }
    let project = client::discover_project(host, provider, &mut session)?;
    if let Some(project) = &project {
        session.values.insert("project_id".into(), json!(project));
    }
    save(host, &session)?;
    project.map(Some).ok_or_else(|| {
        plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity project onboarding is pending; retry discovery later",
        )
    })
}
pub(crate) fn optional_session(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<Option<Session>, PluginError> {
    let Some(bytes) = host.read_private_state()? else {
        return Ok(None);
    };
    let session = decode_session(provider, &bytes)?;
    if session.values.get("access_token") != provider.credentials.get("access_token") {
        return Ok(None);
    }
    Ok(Some(session))
}

pub(crate) fn default_tier(value: &Value) -> Result<&Value, PluginError> {
    let tiers = value
        .get("allowedTiers")
        .and_then(Value::as_array)
        .ok_or_else(|| auth("Antigravity discovery omitted allowed tiers"))?;
    let mut defaults = tiers
        .iter()
        .filter(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true));
    let tier = defaults
        .next()
        .ok_or_else(|| auth("Antigravity discovery did not select a default tier"))?;
    if defaults.next().is_some()
        || tier
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(|id| id.trim().is_empty())
    {
        return Err(auth(
            "Antigravity discovery returned an invalid default tier",
        ));
    }
    Ok(tier)
}
pub(crate) fn validate_operation(name: &str) -> Result<(), PluginError> {
    if !name.starts_with("operations/")
        || name.len() <= "operations/".len()
        || name.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(auth("Antigravity onboarding operation name is malformed"));
    }
    Ok(())
}
fn callback(raw: &str) -> Result<BTreeMap<String, String>, PluginError> {
    let bytes = raw.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return Err(invalid(
                "OAuth callback contains malformed percent encoding",
            ));
        }
    }
    let callback = url::Url::parse(raw).map_err(|_| invalid("OAuth callback URL is invalid"))?;
    let expected =
        url::Url::parse(REDIRECT_URI).map_err(|_| auth("OAuth redirect URI is invalid"))?;
    if callback.origin() != expected.origin()
        || callback.path() != expected.path()
        || callback.fragment().is_some()
        || !callback.username().is_empty()
        || callback.password().is_some()
    {
        return Err(auth("OAuth callback redirect URI mismatch"));
    }
    let mut values = BTreeMap::new();
    for (key, value) in callback.query_pairs() {
        if values
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(invalid("OAuth callback contains duplicate parameters"));
        }
    }
    Ok(values)
}
fn load(host: &GuestHost, provider: &ProviderSnapshot) -> Result<Session, PluginError> {
    let bytes = host
        .read_private_state()?
        .ok_or_else(|| auth("Antigravity OAuth session is missing"))?;
    decode_session(provider, &bytes)
}
fn decode_session(provider: &ProviderSnapshot, bytes: &[u8]) -> Result<Session, PluginError> {
    let session: Session = serde_json::from_slice(bytes)
        .map_err(|_| auth("Antigravity OAuth session is malformed"))?;
    if session.version != 1 || session.provider_id != provider.provider_id {
        return Err(auth(
            "Antigravity OAuth session does not match this provider",
        ));
    }
    Ok(session)
}
pub(crate) fn save(host: &GuestHost, session: &Session) -> Result<(), PluginError> {
    host.write_private_state(
        &serde_json::to_vec(session)
            .map_err(|_| invalid("Antigravity OAuth state could not be encoded"))?,
    )
}
fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_callback_ambiguity_and_wrong_redirect() {
        assert!(callback("https://antigravity.google/oauth-callback?state=a&state=b").is_err());
        assert!(callback("https://other.example/oauth-callback?state=a").is_err());
        assert!(callback("https://antigravity.google/oauth-callback?code=%GG").is_err());
        assert_eq!(
            callback("https://antigravity.google/oauth-callback?code=a%2Bb&state=s")
                .unwrap()
                .get("code")
                .unwrap(),
            "a+b"
        );
    }
    #[test]
    fn preserves_refresh_identity_and_project() {
        let previous = BTreeMap::from([
            ("refresh_token".into(), json!("old")),
            ("project_id".into(), json!("project")),
            ("account_id".into(), json!("account")),
        ]);
        let (values, _) = token_values(
            &json!({"access_token":"new","expires_in":3600}),
            previous,
            false,
        )
        .unwrap();
        assert_eq!(values["refresh_token"], "old");
        assert_eq!(values["project_id"], "project");
        assert_eq!(values["account_id"], "account");
        assert!(
            token_values(
                &json!({"access_token":"new","expires_in":-1}),
                BTreeMap::new(),
                false
            )
            .is_err()
        );
    }
    #[test]
    fn rejects_fake_tiers_and_unsafe_operation_paths() {
        assert!(default_tier(&json!({"allowedTiers":[{"id":"free"}]})).is_err());
        assert_eq!(
            default_tier(&json!({"allowedTiers":[{"id":"paid"},{"id":"free","isDefault":true}]}))
                .unwrap()["id"],
            "free"
        );
        for name in [
            "operations/../other",
            "operations/x?token=secret",
            "https://other/operations/x",
            "operations/",
        ] {
            assert!(validate_operation(name).is_err());
        }
        assert!(validate_operation("operations/project-123").is_ok());
    }
}
