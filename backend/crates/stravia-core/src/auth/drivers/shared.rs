use anyhow::{Context, Result, anyhow};
use base64::Engine;
use chrono::{Duration, Utc};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::types::{AuthExchangeInput, AuthSession, OAuthExchangeError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkceAuthState {
    pub code_verifier: String,
    pub state: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Default)]
pub struct OAuthCallbackPayload {
    pub code: Option<String>,
    pub state: Option<String>,
}

pub fn expires_at_after(seconds: i64) -> String {
    (Utc::now() + Duration::seconds(seconds.max(1))).to_rfc3339()
}

pub fn encode_scopes(scope: Option<&str>) -> Vec<String> {
    scope
        .unwrap_or("")
        .split_whitespace()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn generate_code_challenge(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn parse_session_state<T: DeserializeOwned>(session: &AuthSession) -> Result<T> {
    let raw = session
        .state_json
        .as_deref()
        .context("auth session missing state_json")?;
    serde_json::from_str(raw).context("parse auth session state")
}

pub fn parse_oauth_callback(
    input: &AuthExchangeInput,
    expected_state: &str,
    provider: &str,
) -> std::result::Result<OAuthCallbackPayload, OAuthExchangeError> {
    let raw_callback = input.callback_url.trim();
    if raw_callback.is_empty() {
        return Err(OAuthExchangeError::InvalidCallbackUrl);
    }

    let url = Url::parse(raw_callback).map_err(|_| OAuthExchangeError::InvalidCallbackUrl)?;
    let mut payload = OAuthCallbackPayload::default();
    let mut callback_error = None;
    let mut callback_error_description = None;

    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" if payload.code.is_none() => payload.code = Some(value.to_string()),
            "state" if payload.state.is_none() => payload.state = Some(value.to_string()),
            "error" if callback_error.is_none() => callback_error = Some(value.to_string()),
            "error_description" if callback_error_description.is_none() => {
                callback_error_description = Some(value.to_string())
            }
            _ => {}
        }
    }
    if let Some(fragment) = url.fragment() {
        let fragment_url = Url::parse(&format!("https://callback.local/?{fragment}"))
            .map_err(|_| OAuthExchangeError::InvalidCallbackUrl)?;
        for (key, value) in fragment_url.query_pairs() {
            match key.as_ref() {
                "code" if payload.code.is_none() => payload.code = Some(value.to_string()),
                "state" if payload.state.is_none() => payload.state = Some(value.to_string()),
                "error" if callback_error.is_none() => callback_error = Some(value.to_string()),
                "error_description" if callback_error_description.is_none() => {
                    callback_error_description = Some(value.to_string())
                }
                _ => {}
            }
        }
    }

    validate_callback_state(expected_state, payload.state.as_deref(), provider)?;
    if let Some(error) = callback_error {
        let detail = callback_error_description.unwrap_or_else(|| error.clone());
        return match error.as_str() {
            "access_denied" => Err(OAuthExchangeError::AccessDenied(detail)),
            "invalid_grant" => Err(OAuthExchangeError::InvalidGrant(detail)),
            "invalid_client"
            | "unauthorized_client"
            | "invalid_request"
            | "redirect_uri_mismatch" => Err(OAuthExchangeError::Configuration(format!(
                "{error}: {detail}"
            ))),
            _ => Err(OAuthExchangeError::Retryable(format!("{error}: {detail}"))),
        };
    }
    if payload
        .code
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(OAuthExchangeError::MissingAuthorizationCode);
    }
    Ok(payload)
}

pub fn classify_oauth_token_exchange_error(
    provider: &str,
    status: u16,
    body: &str,
    detail: String,
) -> anyhow::Error {
    let value = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
    let error = value.get("error");
    let code = error
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            error
                .and_then(|value| value.get("code"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            error
                .and_then(|value| value.get("type"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| value.get("error_code").and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    let message = format!("{provider} OAuth token exchange failed: HTTP {status} {detail}");

    match code {
        "access_denied" => OAuthExchangeError::AccessDenied(message).into(),
        "invalid_grant" => OAuthExchangeError::InvalidGrant(message).into(),
        "invalid_client" | "unauthorized_client" | "invalid_request" | "redirect_uri_mismatch" => {
            OAuthExchangeError::Configuration(message).into()
        }
        _ if status == 401 || status == 403 => OAuthExchangeError::Configuration(message).into(),
        _ => anyhow!(message),
    }
}

pub fn validate_callback_state(
    expected_state: &str,
    actual_state: Option<&str>,
    _provider: &str,
) -> std::result::Result<(), OAuthExchangeError> {
    if expected_state.trim().is_empty() {
        return Ok(());
    }
    let Some(actual_state) = actual_state.map(str::trim).filter(|v| !v.is_empty()) else {
        return Err(OAuthExchangeError::MissingState);
    };
    if actual_state != expected_state {
        return Err(OAuthExchangeError::StateMismatch);
    }
    Ok(())
}

pub fn build_authorize_url(base_url: &str, params: &[(&str, &str)]) -> Result<String> {
    let mut url =
        Url::parse(base_url).with_context(|| format!("parse authorize url: {base_url}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in params {
            pairs.append_pair(key, value);
        }
    }
    Ok(url.to_string())
}

pub fn required_http_client(client: Option<reqwest::Client>) -> Result<reqwest::Client> {
    client.ok_or_else(|| anyhow!("missing auth http client"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::AuthExchangeInput;

    fn callback_input(callback: &str) -> AuthExchangeInput {
        AuthExchangeInput {
            callback_url: callback.to_string(),
        }
    }

    #[test]
    fn bare_code_hash_state_is_not_a_callback_url() {
        assert!(
            parse_oauth_callback(
                &callback_input("auth_abc123#state_xyz789"),
                "xyz789",
                "test"
            )
            .is_err()
        );
    }

    #[test]
    fn parse_callback_url_form_still_works() {
        let payload = parse_oauth_callback(
            &callback_input("https://example.com/cb?code=abc&state=xyz"),
            "xyz",
            "test",
        )
        .unwrap();
        assert_eq!(payload.code.as_deref(), Some("abc"));
        assert_eq!(payload.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn callback_error_without_matching_state_is_rejected() {
        let error = parse_oauth_callback(
            &callback_input("https://example.com/cb?error=access_denied"),
            "expected",
            "test",
        )
        .unwrap_err();
        assert!(matches!(error, OAuthExchangeError::MissingState));
    }

    #[test]
    fn validate_state_rejects_missing() {
        assert!(validate_callback_state("expected", None, "claude").is_err());
    }

    #[test]
    fn transient_callback_error_is_retryable() {
        let error = parse_oauth_callback(
            &callback_input(
                "https://example.com/cb?error=temporarily_unavailable&error_description=retry&state=expected",
            ),
            "expected",
            "test",
        )
        .unwrap_err();
        assert!(matches!(error, OAuthExchangeError::Retryable(_)));
    }

    #[test]
    fn token_exchange_errors_distinguish_terminal_and_retryable_failures() {
        let invalid_grant = classify_oauth_token_exchange_error(
            "OpenAI",
            400,
            r#"{"error":"invalid_grant","error_description":"expired code"}"#,
            "expired code".to_string(),
        );
        assert!(matches!(
            invalid_grant.downcast_ref::<OAuthExchangeError>(),
            Some(OAuthExchangeError::InvalidGrant(_))
        ));

        let unavailable = classify_oauth_token_exchange_error(
            "Claude",
            503,
            r#"{"error":"temporarily_unavailable"}"#,
            "temporarily unavailable".to_string(),
        );
        assert!(unavailable.downcast_ref::<OAuthExchangeError>().is_none());
    }
}
