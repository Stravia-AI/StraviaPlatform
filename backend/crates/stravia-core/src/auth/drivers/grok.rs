use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::shared::{encode_scopes, expires_at_after, parse_session_state, required_http_client};
use crate::auth::types::{
    AuthDriver, AuthDriverMetadata, AuthPollState, AuthProgress, AuthScheme, AuthSession,
    CreateAuthSession, CredentialBundle, RefreshAuthContext, RuntimeBinding, StartAuthContext,
    StoredCredential,
};
use crate::db::models::Provider;
use crate::provider::{OAuthConfig, RuntimeConfig, VendorRegistry};

const XAI_PRESET_ID: &str = "xai";
const GROK_CHANNEL_ID: &str = "grok";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_POLL_INTERVAL_SECONDS: i32 = 5;
const DEFAULT_TOKEN_LIFETIME_SECONDS: i64 = 3600;
const MAX_DEVICE_POLL_SECONDS: i64 = 30 * 60;

#[derive(Debug, Clone)]
struct GrokConfig {
    issuer: String,
    client_id: String,
    scope: String,
    runtime_base_url: String,
    client_version: String,
    enforce_xai_hosts: bool,
}

impl GrokConfig {
    fn from_registry() -> Result<Self> {
        let metadata = VendorRegistry::global()
            .metadata(XAI_PRESET_ID)
            .ok_or_else(|| anyhow!("missing provider preset: {XAI_PRESET_ID}"))?;
        let channel = metadata
            .channels
            .iter()
            .find(|channel| channel.id == GROK_CHANNEL_ID)
            .ok_or_else(|| {
                anyhow!("missing provider channel: {XAI_PRESET_ID}/{GROK_CHANNEL_ID}")
            })?;
        let OAuthConfig {
            auth_base_url,
            client_id,
            scope,
            ..
        } = channel
            .oauth
            .as_ref()
            .ok_or_else(|| anyhow!("missing oauth config for {XAI_PRESET_ID}/{GROK_CHANNEL_ID}"))?;
        let RuntimeConfig {
            api_base_url,
            models_client_version,
            ..
        } = channel.runtime.as_ref().ok_or_else(|| {
            anyhow!("missing runtime config for {XAI_PRESET_ID}/{GROK_CHANNEL_ID}")
        })?;
        Ok(Self {
            issuer: auth_base_url.trim_end_matches('/').to_string(),
            client_id: (*client_id).to_string(),
            scope: (*scope).to_string(),
            runtime_base_url: (*api_base_url).to_string(),
            client_version: (*models_client_version).to_string(),
            enforce_xai_hosts: true,
        })
    }

    fn discovery_url(&self) -> String {
        format!("{}/.well-known/openid-configuration", self.issuer)
    }
}

#[derive(Debug, Default)]
pub struct GrokOAuthDriver {
    config_override: Option<GrokConfig>,
}

#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    device_authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: i64,
    interval: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct GrokTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GrokDeviceState {
    device_code: String,
    token_endpoint: String,
}

impl GrokOAuthDriver {
    fn config(&self) -> Result<GrokConfig> {
        self.config_override
            .clone()
            .map(Ok)
            .unwrap_or_else(GrokConfig::from_registry)
    }

    fn validate_endpoint(config: &GrokConfig, endpoint: &str, name: &str) -> Result<()> {
        let parsed = Url::parse(endpoint).with_context(|| format!("parse xAI {name}"))?;
        if config.enforce_xai_hosts {
            if parsed.scheme() != "https" {
                bail!("xAI {name} must use HTTPS");
            }
            let host = parsed
                .host_str()
                .ok_or_else(|| anyhow!("xAI {name} is missing a host"))?
                .to_ascii_lowercase();
            if host != "x.ai" && !host.ends_with(".x.ai") {
                bail!("xAI {name} must use x.ai or an x.ai subdomain");
            }
        } else if !matches!(parsed.scheme(), "http" | "https") {
            bail!("xAI {name} must use HTTP or HTTPS");
        }
        Ok(())
    }

    async fn discover(config: &GrokConfig, client: &reqwest::Client) -> Result<OidcDiscovery> {
        let discovery_url = config.discovery_url();
        Self::validate_endpoint(config, &discovery_url, "discovery URL")?;
        let response = client
            .get(&discovery_url)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .context("discover xAI OAuth endpoints")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("xAI OAuth discovery failed: HTTP {status} {body}");
        }
        let discovery: OidcDiscovery =
            serde_json::from_str(&body).context("parse xAI OAuth discovery response")?;
        Self::validate_endpoint(
            config,
            &discovery.device_authorization_endpoint,
            "device authorization endpoint",
        )?;
        Self::validate_endpoint(config, &discovery.token_endpoint, "token endpoint")?;
        Ok(discovery)
    }

    fn parse_error(body: &str) -> OAuthErrorResponse {
        serde_json::from_str(body).unwrap_or(OAuthErrorResponse {
            error: None,
            error_description: None,
            message: None,
        })
    }

    fn error_detail(error: &OAuthErrorResponse, fallback: &str) -> String {
        error
            .error_description
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                error
                    .message
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })
            .or_else(|| {
                error
                    .error
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or(fallback)
            .to_string()
    }

    fn decode_jwt_identity(id_token: Option<&str>) -> Option<String> {
        let payload = id_token?.split('.').nth(1)?;
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
            .ok()?;
        let claims: Value = serde_json::from_slice(&decoded).ok()?;
        claims
            .get("email")
            .and_then(Value::as_str)
            .or_else(|| claims.get("sub").and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    }

    fn normalize_token_response(
        body: &str,
        fallback_refresh_token: Option<&str>,
        token_endpoint: &str,
        runtime_base_url: &str,
    ) -> Result<CredentialBundle> {
        let token: GrokTokenResponse =
            serde_json::from_str(body).context("parse xAI OAuth token response")?;
        let access_token = token
            .access_token
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("xAI OAuth token response missing access_token"))?;
        let refresh_token = token
            .refresh_token
            .filter(|value| !value.trim().is_empty())
            .or_else(|| fallback_refresh_token.map(ToString::to_string));
        let expires_in = token
            .expires_in
            .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS)
            .max(1);
        let subject_id = Self::decode_jwt_identity(token.id_token.as_deref());
        let scopes = encode_scopes(token.scope.as_deref());
        let mut raw = match serde_json::from_str::<Value>(body) {
            Ok(Value::Object(raw)) => raw,
            _ => Map::new(),
        };
        raw.insert(
            "token_endpoint".to_string(),
            Value::String(token_endpoint.to_string()),
        );
        if let Some(token_type) = token.token_type {
            raw.insert("token_type".to_string(), Value::String(token_type));
        }

        Ok(CredentialBundle {
            access_token: Some(access_token),
            refresh_token,
            expires_at: Some(expires_at_after(expires_in)),
            resource_url: Some(runtime_base_url.to_string()),
            subject_id,
            scopes,
            raw: Value::Object(raw),
        })
    }

    fn pending_progress(session: &AuthSession, interval: i32) -> AuthProgress {
        AuthProgress {
            user_code: session.user_code.clone(),
            verification_uri: session.verification_uri.clone(),
            verification_uri_complete: session.verification_uri_complete.clone(),
            expires_at: session.expires_at.clone(),
            poll_interval_seconds: Some(interval),
        }
    }
}

#[async_trait]
impl AuthDriver for GrokOAuthDriver {
    fn metadata(&self) -> AuthDriverMetadata {
        AuthDriverMetadata {
            key: "grok",
            label: "Grok",
            scheme: AuthScheme::OAuthDeviceCode,
            supports_new_provider: true,
            supports_existing_provider: true,
            callback: None,
        }
    }

    async fn start(&self, ctx: StartAuthContext) -> Result<CreateAuthSession> {
        let config = self.config()?;
        let client = required_http_client(ctx.http_client)?;
        let discovery = Self::discover(&config, &client).await?;
        let response = client
            .post(&discovery.device_authorization_endpoint)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("scope", config.scope.as_str()),
            ])
            .send()
            .await
            .context("start xAI device authorization")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let error = Self::parse_error(&body);
            bail!(
                "xAI device authorization failed: HTTP {status} {}",
                Self::error_detail(&error, &body)
            );
        }
        let device: DeviceAuthorizationResponse =
            serde_json::from_str(&body).context("parse xAI device authorization response")?;
        if device.device_code.trim().is_empty()
            || device.user_code.trim().is_empty()
            || (device.verification_uri.trim().is_empty()
                && device
                    .verification_uri_complete
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()))
        {
            bail!("xAI device authorization response is incomplete");
        }
        let interval = device
            .interval
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
            .max(DEFAULT_POLL_INTERVAL_SECONDS);
        let state = GrokDeviceState {
            device_code: device.device_code,
            token_endpoint: discovery.token_endpoint,
        };

        Ok(CreateAuthSession {
            provider_id: ctx.provider_id,
            driver_key: self.metadata().key.to_string(),
            scheme: self.metadata().scheme.as_str().to_string(),
            status: "pending".to_string(),
            use_proxy: ctx.use_proxy,
            user_code: Some(device.user_code),
            verification_uri: Some(device.verification_uri),
            verification_uri_complete: device.verification_uri_complete,
            state_json: Some(serde_json::to_string(&state)?),
            context_json: None,
            result_json: None,
            expires_at: Some(expires_at_after(
                device.expires_in.clamp(1, MAX_DEVICE_POLL_SECONDS),
            )),
            poll_interval_seconds: Some(interval),
            last_error: None,
        })
    }

    async fn poll(&self, session: &AuthSession, ctx: RefreshAuthContext) -> Result<AuthPollState> {
        let config = self.config()?;
        let state: GrokDeviceState = parse_session_state(session)?;
        Self::validate_endpoint(&config, &state.token_endpoint, "token endpoint")?;
        let client = required_http_client(ctx.http_client)?;
        let response = client
            .post(&state.token_endpoint)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .form(&[
                ("grant_type", DEVICE_GRANT_TYPE),
                ("device_code", state.device_code.as_str()),
                ("client_id", config.client_id.as_str()),
            ])
            .send()
            .await
            .context("poll xAI device authorization")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(AuthPollState::Ready(Self::normalize_token_response(
                &body,
                None,
                &state.token_endpoint,
                &config.runtime_base_url,
            )?));
        }

        let error = Self::parse_error(&body);
        match error.error.as_deref() {
            Some("authorization_pending") => Ok(AuthPollState::Pending(Self::pending_progress(
                session,
                session
                    .poll_interval_seconds
                    .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
                    .max(DEFAULT_POLL_INTERVAL_SECONDS),
            ))),
            Some("slow_down") => Ok(AuthPollState::Pending(Self::pending_progress(
                session,
                session
                    .poll_interval_seconds
                    .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
                    .max(DEFAULT_POLL_INTERVAL_SECONDS)
                    + 5,
            ))),
            Some("expired_token") => Ok(AuthPollState::Error {
                code: "AUTH_DEVICE_CODE_EXPIRED".to_string(),
                message: Self::error_detail(&error, "xAI device code expired"),
            }),
            Some("access_denied") => Ok(AuthPollState::Error {
                code: "AUTH_ACCESS_DENIED".to_string(),
                message: Self::error_detail(&error, "xAI authorization was denied"),
            }),
            _ => Ok(AuthPollState::Error {
                code: if status == StatusCode::UNAUTHORIZED {
                    "AUTH_UNAUTHORIZED"
                } else {
                    "AUTH_DEVICE_TOKEN_ERROR"
                }
                .to_string(),
                message: format!(
                    "xAI device token request failed: HTTP {status} {}",
                    Self::error_detail(&error, &body)
                ),
            }),
        }
    }

    async fn refresh(
        &self,
        credential: &StoredCredential,
        ctx: RefreshAuthContext,
    ) -> Result<CredentialBundle> {
        let config = self.config()?;
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("xAI OAuth refresh token is missing"))?;
        let client = required_http_client(ctx.http_client)?;
        let token_endpoint = match credential
            .meta
            .get("token_endpoint")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(endpoint) => {
                Self::validate_endpoint(&config, endpoint, "token endpoint")?;
                endpoint.to_string()
            }
            None => Self::discover(&config, &client).await?.token_endpoint,
        };
        let response = client
            .post(&token_endpoint)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", config.client_id.as_str()),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .context("refresh xAI OAuth token")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let error = Self::parse_error(&body);
            bail!(
                "xAI OAuth token refresh failed: HTTP {status} {}",
                Self::error_detail(&error, &body)
            );
        }
        Self::normalize_token_response(
            &body,
            Some(refresh_token),
            &token_endpoint,
            &config.runtime_base_url,
        )
    }

    fn bind_runtime(
        &self,
        _provider: &Provider,
        credential: &StoredCredential,
    ) -> Result<RuntimeBinding> {
        let config = self.config()?;
        let access_token = credential
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("xAI OAuth access token is empty in bind_runtime"))?;
        let mut extra_headers = HashMap::new();
        extra_headers.insert(
            "authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        extra_headers.insert("x-xai-token-auth".to_string(), "xai-grok-cli".to_string());
        extra_headers.insert(
            "x-grok-client-version".to_string(),
            config.client_version.clone(),
        );
        extra_headers.insert(
            "user-agent".to_string(),
            format!("xai-grok-workspace/{}", config.client_version),
        );
        extra_headers.insert(
            "x-grok-client-identifier".to_string(),
            "grok-shell".to_string(),
        );
        extra_headers.insert(
            "x-authenticateresponse".to_string(),
            "authenticate-response".to_string(),
        );

        Ok(RuntimeBinding {
            base_url_override: credential
                .resource_url
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| Some(config.runtime_base_url)),
            extra_headers,
            model_aliases: HashMap::new(),
            models_source_override: None,
            disable_default_auth: true,
            static_models_override: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::StatusCode as AxumStatusCode;
    use axum::routing::{get, post};

    use super::*;
    use crate::auth::types::OAuthCallbackMode;

    #[derive(Clone, Default)]
    struct FixtureState {
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn fixture() -> (String, FixtureState) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Grok OAuth fixture");
        let address = listener.local_addr().expect("fixture address");
        let base_url = format!("http://{address}");
        let state = FixtureState::default();
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get({
                    let base_url = base_url.clone();
                    move || {
                        let base_url = base_url.clone();
                        async move {
                            axum::Json(serde_json::json!({
                                "device_authorization_endpoint": format!("{base_url}/device"),
                                "token_endpoint": format!("{base_url}/token")
                            }))
                        }
                    }
                }),
            )
            .route(
                "/device",
                post(
                    |State(state): State<FixtureState>, body: Bytes| async move {
                        state
                            .requests
                            .lock()
                            .expect("requests")
                            .push(String::from_utf8_lossy(&body).into_owned());
                        axum::Json(serde_json::json!({
                            "device_code": "device-code",
                            "user_code": "ABCD-EFGH",
                            "verification_uri": "https://auth.x.ai/device",
                            "verification_uri_complete": "https://auth.x.ai/device?code=ABCD-EFGH",
                            "expires_in": 600,
                            "interval": 1
                        }))
                    },
                ),
            )
            .route(
                "/token",
                post(
                    |State(state): State<FixtureState>, body: Bytes| async move {
                        let body = String::from_utf8_lossy(&body).into_owned();
                        state.requests.lock().expect("requests").push(body.clone());
                        if body.contains("device_code=device-code") {
                            (
                                AxumStatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "access_token": "access-token",
                                    "refresh_token": "refresh-token",
                                    "expires_in": 3600,
                                    "scope": "openid offline_access"
                                })),
                            )
                        } else {
                            (
                                AxumStatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "access_token": "refreshed-access-token",
                                    "expires_in": 3600
                                })),
                            )
                        }
                    },
                ),
            )
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Grok OAuth fixture");
        });
        (base_url, state)
    }

    fn driver(base_url: String) -> GrokOAuthDriver {
        GrokOAuthDriver {
            config_override: Some(GrokConfig {
                issuer: base_url,
                client_id: "test-client".to_string(),
                scope: "openid offline_access".to_string(),
                runtime_base_url: "https://cli-chat-proxy.grok.com/v1".to_string(),
                client_version: "0.2.120".to_string(),
                enforce_xai_hosts: false,
            }),
        }
    }

    #[tokio::test]
    async fn device_flow_starts_polls_and_refreshes_with_cpa_wire_contract() {
        let (base_url, fixture) = fixture().await;
        let driver = driver(base_url);
        let client = reqwest::Client::new();
        let created = driver
            .start(StartAuthContext {
                http_client: Some(client.clone()),
                ..Default::default()
            })
            .await
            .expect("start device flow");
        assert_eq!(created.scheme, "oauth_device_code");
        assert_eq!(created.user_code.as_deref(), Some("ABCD-EFGH"));
        assert_eq!(created.poll_interval_seconds, Some(5));

        let session = AuthSession {
            id: "session".to_string(),
            provider_id: None,
            driver_key: created.driver_key,
            scheme: created.scheme,
            status: created.status,
            use_proxy: false,
            callback_mode: OAuthCallbackMode::Auto,
            listener_state: "not_required".to_string(),
            listener_port: None,
            redirect_uri: String::new(),
            fallback_reason: None,
            user_code: created.user_code,
            verification_uri: created.verification_uri,
            verification_uri_complete: created.verification_uri_complete,
            state_json: created.state_json,
            context_json: None,
            result_json: None,
            expires_at: created.expires_at,
            poll_interval_seconds: created.poll_interval_seconds,
            last_error: None,
            error_code: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let ready = driver
            .poll(
                &session,
                RefreshAuthContext {
                    http_client: Some(client.clone()),
                    ..Default::default()
                },
            )
            .await
            .expect("poll device flow");
        let AuthPollState::Ready(bundle) = ready else {
            panic!("expected ready device flow")
        };
        assert_eq!(bundle.access_token.as_deref(), Some("access-token"));
        assert_eq!(
            bundle.resource_url.as_deref(),
            Some("https://cli-chat-proxy.grok.com/v1")
        );

        let refreshed = driver
            .refresh(
                &StoredCredential {
                    driver_key: "grok".to_string(),
                    scheme: "oauth_device_code".to_string(),
                    access_token: bundle.access_token,
                    refresh_token: bundle.refresh_token,
                    expires_at: bundle.expires_at,
                    resource_url: bundle.resource_url,
                    subject_id: None,
                    scopes: bundle.scopes,
                    meta: bundle.raw,
                },
                RefreshAuthContext {
                    http_client: Some(client),
                    ..Default::default()
                },
            )
            .await
            .expect("refresh Grok token");
        assert_eq!(
            refreshed.access_token.as_deref(),
            Some("refreshed-access-token")
        );
        assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh-token"));

        let requests = fixture.requests.lock().expect("requests");
        assert!(requests[0].contains("client_id=test-client"));
        assert!(requests[0].contains("scope=openid+offline_access"));
        assert!(
            requests[1]
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
        );
        assert!(requests[1].contains("device_code=device-code"));
        assert!(requests[2].contains("grant_type=refresh_token"));
        assert!(requests[2].contains("refresh_token=refresh-token"));
    }

    #[test]
    fn runtime_binding_matches_grok_build_identity_headers() {
        let driver = driver("http://127.0.0.1".to_string());
        let binding = driver
            .bind_runtime(
                &Provider {
                    id: "provider".to_string(),
                    name: "Grok".to_string(),
                    vendor: Some("xai".to_string()),
                    protocol: "open-responses".to_string(),
                    base_url: String::new(),
                    preset_key: Some("xai".to_string()),
                    channel: Some("grok".to_string()),
                    models_source: Some("catalog".to_string()),
                    static_models: None,
                    api_key: String::new(),
                    adapter_credentials: "{}".to_string(),
                    auth_mode: "oauth".to_string(),
                    use_proxy: false,
                    last_test_success: None,
                    last_test_at: None,
                    is_enabled: true,
                    created_at: String::new(),
                    updated_at: String::new(),
                },
                &StoredCredential {
                    access_token: Some("access-token".to_string()),
                    resource_url: Some("https://cli-chat-proxy.grok.com/v1".to_string()),
                    ..Default::default()
                },
            )
            .expect("Grok runtime binding");
        assert!(binding.disable_default_auth);
        assert_eq!(
            binding
                .extra_headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer access-token")
        );
        assert_eq!(
            binding
                .extra_headers
                .get("x-xai-token-auth")
                .map(String::as_str),
            Some("xai-grok-cli")
        );
        assert_eq!(
            binding.base_url_override.as_deref(),
            Some("https://cli-chat-proxy.grok.com/v1")
        );
    }
}
