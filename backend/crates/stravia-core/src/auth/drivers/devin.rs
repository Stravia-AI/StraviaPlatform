//! Devin OAuth driver — PKCE against `app.devin.ai` CLI endpoints.
//!
//! Endpoint contract recovered from `devin.exe` strings: the CLI drives
//! `GET {webapp}/auth/cli/continue` with PKCE parameters and exchanges the
//! code at `POST {webapp}/auth/cli/token`. The manual fallback mirrors the
//! CLI's `chisel-show-auth-token` flow where the webapp renders the session
//! token for the user to paste back.
//!
//! Two credential shapes exist upstream (`session_token` for the Connect
//! api-server, `api_key`/`windsurf_api_key` for legacy Windsurf accounts);
//! normalization accepts either plus the conventional OAuth field names.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::Deserialize;

use super::shared::{
    PkceAuthState, build_authorize_url, classify_oauth_token_exchange_error, encode_scopes,
    expires_at_after, generate_code_challenge, generate_code_verifier, generate_state,
    parse_oauth_callback, parse_session_state, required_http_client,
};
use crate::auth::types::{
    AuthDriver, AuthDriverMetadata, AuthExchangeInput, AuthScheme, AuthSession, CreateAuthSession,
    CredentialBundle, ExchangeAuthContext, OAuthCallbackMode, OAuthCallbackPolicy,
    OAuthCallbackPort, OAuthExchangeError, RefreshAuthContext, RuntimeBinding, StartAuthContext,
    StoredCredential,
};
use crate::db::models::Provider;
use crate::provider::OAuthConfig;
use crate::provider::VendorRegistry;

const DEVIN_PRESET_ID: &str = "devin";
const DEVIN_CHANNEL_ID: &str = "default";
const DEVIN_PROTOCOL_ID: &str = "devin-connect";

/// The CLI's manual-flow redirect URI: the webapp shows the session token
/// instead of redirecting to a loopback listener.
const DEVIN_MANUAL_REDIRECT_URI: &str = "chisel-show-auth-token";
/// Implicit-flow signin page the CLI uses for manual token capture. The host
/// is inferred to be the Devin webapp; `/windsurf/signin` is the literal
/// path fragment found in `devin.exe`.
const DEVIN_MANUAL_SIGNIN_PATH: &str = "/windsurf/signin";

#[derive(Debug, Clone, Copy)]
struct DevinConfig {
    oauth: &'static OAuthConfig,
    api_base_url: &'static str,
    static_models: &'static [&'static str],
}

#[derive(Debug, Default)]
pub struct DevinOAuthDriver;

#[derive(Debug, Deserialize)]
struct DevinTokenResponse {
    session_token: Option<String>,
    api_key: Option<String>,
    windsurf_api_key: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    api_server_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DevinErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
    message: Option<String>,
}

impl DevinOAuthDriver {
    fn devin_config() -> Result<DevinConfig> {
        let metadata = VendorRegistry::global()
            .metadata(DEVIN_PRESET_ID)
            .ok_or_else(|| anyhow!("missing provider preset: {DEVIN_PRESET_ID}"))?;
        let channel = metadata
            .channels
            .iter()
            .find(|c| c.id == DEVIN_CHANNEL_ID)
            .ok_or_else(|| {
                anyhow!("missing provider channel: {DEVIN_PRESET_ID}/{DEVIN_CHANNEL_ID}")
            })?;
        let api_base_url = channel
            .base_urls
            .iter()
            .find(|entry| entry.protocol == DEVIN_PROTOCOL_ID)
            .map(|entry| entry.base_url)
            .ok_or_else(|| {
                anyhow!(
                    "missing base url for protocol {DEVIN_PROTOCOL_ID} in \
                     {DEVIN_PRESET_ID}/{DEVIN_CHANNEL_ID}"
                )
            })?;
        Ok(DevinConfig {
            oauth: channel.oauth.as_ref().ok_or_else(|| {
                anyhow!("missing oauth config for {DEVIN_PRESET_ID}/{DEVIN_CHANNEL_ID}")
            })?,
            api_base_url,
            static_models: channel.static_models,
        })
    }

    fn normalize_token_response(
        body: &str,
        fallback_refresh_token: Option<&str>,
        config: DevinConfig,
    ) -> Result<CredentialBundle> {
        let token: DevinTokenResponse =
            serde_json::from_str(body).context("parse devin oauth token response")?;
        // The Connect runtime consumes the session token; legacy Windsurf
        // accounts mint `api_key`/`windsurf_api_key` on the same surface.
        let access_token = [
            token.session_token,
            token.api_key,
            token.windsurf_api_key,
            token.access_token,
        ]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("devin oauth token response missing session token"))?;
        let resource_url = token
            .api_server_url
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| config.api_base_url.to_string());

        Ok(CredentialBundle {
            access_token: Some(access_token),
            refresh_token: token
                .refresh_token
                .filter(|value| !value.trim().is_empty())
                .or_else(|| fallback_refresh_token.map(ToString::to_string)),
            expires_at: token
                .expires_in
                .map(|seconds| expires_at_after(seconds.max(1))),
            resource_url: Some(resource_url),
            subject_id: None,
            scopes: encode_scopes(token.scope.as_deref()),
            raw: serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
        })
    }

    fn parse_error(body: &str) -> Option<String> {
        let parsed: DevinErrorResponse = serde_json::from_str(body).ok()?;
        parsed
            .error_description
            .filter(|value| !value.trim().is_empty())
            .or_else(|| parsed.error.filter(|value| !value.trim().is_empty()))
            .or_else(|| parsed.message.filter(|value| !value.trim().is_empty()))
    }

    /// Manual callback input is the token the webapp rendered — a bare string,
    /// a `name=value` pair list, or a URL whose query/fragment carries it.
    fn manual_token(callback: &str) -> Option<String> {
        let raw = callback.trim();
        if raw.is_empty() {
            return None;
        }
        if raw.contains('=') {
            for pair in raw.split(['?', '#', '&']) {
                if let Some((key, value)) = pair.split_once('=')
                    && matches!(key, "session_token" | "access_token" | "api_key" | "token")
                    && !value.trim().is_empty()
                {
                    return Some(value.trim().to_string());
                }
            }
            return None;
        }
        Some(raw.to_string())
    }
}

#[async_trait]
impl AuthDriver for DevinOAuthDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "devin",
            label: "Devin",
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
            callback: Some(OAuthCallbackPolicy {
                bind_host: "127.0.0.1",
                redirect_host: "localhost",
                path: "/callback",
                port: OAuthCallbackPort::Dynamic,
                manual_redirect_uri: DEVIN_MANUAL_REDIRECT_URI,
                cancel_path: None,
            }),
        }
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let config = Self::devin_config()?;
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);
        let state = generate_state();
        let manual = ctx
            .redirect_uri
            .as_deref()
            .is_some_and(|uri| uri.trim() == DEVIN_MANUAL_REDIRECT_URI);

        // Manual mode mirrors the CLI's implicit signin page (renders the
        // token); auto mode runs the PKCE continue→token exchange.
        let auth_url = if manual {
            let base = format!(
                "{}{}",
                config.oauth.auth_base_url.trim_end_matches('/'),
                DEVIN_MANUAL_SIGNIN_PATH
            );
            build_authorize_url(
                &base,
                &[
                    ("response_type", "token"),
                    ("client_id", config.oauth.client_id),
                    ("redirect_uri", DEVIN_MANUAL_REDIRECT_URI),
                    ("state", &state),
                ],
            )?
        } else {
            let redirect_uri = ctx
                .redirect_uri
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or(config.oauth.redirect_uri);
            let mut params: Vec<(&str, &str)> = vec![
                ("response_type", "code"),
                ("client_id", config.oauth.client_id),
                ("redirect_uri", redirect_uri),
                ("state", &state),
                ("code_challenge", &code_challenge),
                ("code_challenge_method", "S256"),
                // Observed on the CLI's continue URL; marks the request as
                // the CLI PKCE flow and asks the account picker to show.
                ("cli_pkce_marker", "1"),
                ("prompt", "select_account"),
                ("redirect_parameters_type", "query"),
            ];
            if !config.oauth.scope.is_empty() {
                params.push(("scope", config.oauth.scope));
            }
            build_authorize_url(config.oauth.authorize_url, &params)?
        };
        let session_state = serde_json::to_string(&PkceAuthState {
            code_verifier,
            state,
            redirect_uri: ctx
                .redirect_uri
                .clone()
                .unwrap_or_else(|| config.oauth.redirect_uri.to_string()),
        })?;

        Ok(CreateAuthSession {
            provider_id: ctx.provider_id,
            driver_key: self.metadata().key.to_string(),
            scheme: self.metadata().scheme.as_str().to_string(),
            status: "pending".to_string(),
            use_proxy: ctx.use_proxy,
            user_code: None,
            verification_uri: Some(config.oauth.auth_base_url.to_string()),
            verification_uri_complete: Some(auth_url),
            state_json: Some(session_state),
            context_json: None,
            result_json: None,
            expires_at: Some(expires_at_after(10 * 60)),
            poll_interval_seconds: Some(2),
            last_error: None,
        })
    }

    async fn exchange(
        &self,
        session: &AuthSession,
        input: AuthExchangeInput,
        ctx: ExchangeAuthContext,
    ) -> Result<CredentialBundle> {
        let config = Self::devin_config()?;
        let state: PkceAuthState = parse_session_state(session)?;

        // Manual flow: the pasted value IS the session token, not a code.
        if session.callback_mode == OAuthCallbackMode::Manual {
            let token = Self::manual_token(&input.callback_url)
                .ok_or(OAuthExchangeError::InvalidCallbackUrl)?;
            return Ok(CredentialBundle {
                access_token: Some(token),
                resource_url: Some(config.api_base_url.to_string()),
                ..Default::default()
            });
        }

        let callback = parse_oauth_callback(&input, &state.state, "devin")?;
        let code = callback
            .code
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("missing authorization code"))?;

        let client = required_http_client(ctx.http_client)?;
        let token_body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": config.oauth.client_id,
            "code": code,
            "redirect_uri": state.redirect_uri,
            "code_verifier": state.code_verifier,
        });

        let response = client
            .post(config.oauth.token_url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&token_body)
            .send()
            .await
            .context("exchange devin authorization code")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or_else(|| body.clone());
            return Err(classify_oauth_token_exchange_error(
                "Devin",
                status.as_u16(),
                &body,
                detail,
            ));
        }

        Self::normalize_token_response(&body, None, config)
    }

    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        let config = Self::devin_config()?;
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("devin session tokens are not refreshable; re-authenticate"))?;
        let client = required_http_client(ctx.http_client)?;

        let response = client
            .post(config.oauth.token_url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": config.oauth.client_id,
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .context("refresh devin oauth token")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = Self::parse_error(&body).unwrap_or(body);
            bail!("devin oauth token refresh failed: HTTP {status} {detail}");
        }

        Self::normalize_token_response(&body, Some(refresh_token), config)
    }

    fn bind_runtime(
        &self,
        _provider: &Provider,
        credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        let config = Self::devin_config()?;
        let access_token = credential
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("devin oauth session token is empty in bind_runtime"))?;

        // The vendor's build_request writes the doubled Basic credential and
        // embeds the token in ClientMetadata; the binding only needs to pin
        // the api-server host and suppress default auth injection.
        let mut extra_headers = HashMap::new();
        extra_headers.insert(
            "authorization".to_string(),
            format!("Basic {access_token}-{access_token}"),
        );

        let base_url_override = credential
            .resource_url
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| Some(config.api_base_url.to_string()));
        // Entitlement is plan-dependent; the channel ships the verified
        // selector list rather than treating a catalog as runnable inventory.
        let static_models_override: Option<Vec<String>> = if config.static_models.is_empty() {
            None
        } else {
            Some(config.static_models.iter().map(|s| s.to_string()).collect())
        };

        Ok(RuntimeBinding {
            base_url_override,
            extra_headers,
            model_aliases: HashMap::new(),
            models_source_override: None,
            disable_default_auth: true,
            static_models_override,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider() -> Provider {
        Provider {
            id: "test".into(),
            name: "test".into(),
            vendor: Some("devin".into()),
            protocol: "devin-connect".into(),
            base_url: String::new(),
            preset_key: Some("devin".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn config_loads_from_vendor_registry() {
        let config = DevinOAuthDriver::devin_config().unwrap();
        assert_eq!(config.oauth.auth_base_url, "https://app.devin.ai");
        assert_eq!(
            config.oauth.authorize_url,
            "https://app.devin.ai/auth/cli/continue"
        );
        assert_eq!(
            config.oauth.token_url,
            "https://app.devin.ai/auth/cli/token"
        );
        assert_eq!(config.api_base_url, "https://server.codeium.com");
    }

    #[test]
    fn normalize_prefers_session_token_and_api_server_url() {
        let body = r#"{"session_token":"sess_abc","api_key":"legacy","api_server_url":"https://custom.example.com"}"#;
        let config = DevinOAuthDriver::devin_config().unwrap();
        let bundle = DevinOAuthDriver::normalize_token_response(body, None, config).unwrap();
        assert_eq!(bundle.access_token.as_deref(), Some("sess_abc"));
        assert_eq!(
            bundle.resource_url.as_deref(),
            Some("https://custom.example.com")
        );
    }

    #[test]
    fn normalize_falls_back_to_api_key() {
        let body = r#"{"api_key":"wk_123","api_server_url":"https://server.codeium.com"}"#;
        let config = DevinOAuthDriver::devin_config().unwrap();
        let bundle = DevinOAuthDriver::normalize_token_response(body, None, config).unwrap();
        assert_eq!(bundle.access_token.as_deref(), Some("wk_123"));
    }

    #[test]
    fn manual_token_accepts_bare_and_keyed_forms() {
        assert_eq!(
            DevinOAuthDriver::manual_token("st_123").as_deref(),
            Some("st_123")
        );
        assert_eq!(
            DevinOAuthDriver::manual_token("session_token=st_456").as_deref(),
            Some("st_456")
        );
        assert_eq!(
            DevinOAuthDriver::manual_token("chisel-show-auth-token#access_token=st_789").as_deref(),
            Some("st_789")
        );
        assert!(DevinOAuthDriver::manual_token("").is_none());
    }

    #[test]
    fn bind_runtime_sets_doubled_basic_and_disables_default_auth() {
        let provider = test_provider();
        let credential = StoredCredential {
            access_token: Some("my_token".into()),
            ..Default::default()
        };
        let binding = DevinOAuthDriver
            .bind_runtime(&provider, &credential)
            .unwrap();
        assert_eq!(
            binding.extra_headers.get("authorization").unwrap(),
            "Basic my_token-my_token"
        );
        assert!(binding.disable_default_auth);
        assert_eq!(
            binding.base_url_override.as_deref(),
            Some("https://server.codeium.com")
        );
        let static_models = binding
            .static_models_override
            .as_deref()
            .expect("devin channel ships a curated selector list");
        assert!(static_models.iter().any(|m| m == "swe-1-7"));
    }
}
