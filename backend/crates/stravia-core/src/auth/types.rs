use std::collections::HashMap;

use anyhow::bail;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::models::Provider;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    ApiKey,
    OAuthAuthCodePkce,
    OAuthDeviceCode,
    SetupToken,
    Custom,
}

impl AuthScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuthAuthCodePkce => "oauth_auth_code_pkce",
            Self::OAuthDeviceCode => "oauth_device_code",
            Self::SetupToken => "setup_token",
            Self::Custom => "custom",
        }
    }
}

impl std::fmt::Display for AuthScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AuthScheme {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "api_key" | "apikey" => Ok(Self::ApiKey),
            "oauth_auth_code_pkce" | "oauth-pkce" | "oauth_pkce" => Ok(Self::OAuthAuthCodePkce),
            "oauth_device_code" | "device_code" | "oauth-device" => Ok(Self::OAuthDeviceCode),
            "setup_token" | "setup-token" => Ok(Self::SetupToken),
            "custom" => Ok(Self::Custom),
            other => bail!("unsupported auth scheme: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthSessionStatus {
    Pending,
    Exchanging,
    Ready,
    Error,
    Cancelled,
}

impl AuthSessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Exchanging => "exchanging",
            Self::Ready => "ready",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthBindingStatus {
    Pending,
    Connected,
    Error,
    Disconnected,
}

impl AuthBindingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connected => "connected",
            Self::Error => "error",
            Self::Disconnected => "disconnected",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthCallbackPort {
    Fixed { primary: u16, fallback: u16 },
    Dynamic,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct OAuthCallbackPolicy {
    pub bind_host: &'static str,
    pub redirect_host: &'static str,
    pub path: &'static str,
    pub port: OAuthCallbackPort,
    pub manual_redirect_uri: &'static str,
    pub cancel_path: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthDriverMetadata {
    pub key: &'static str,
    pub label: &'static str,
    pub scheme: AuthScheme,
    pub supports_new_provider: bool,
    pub supports_existing_provider: bool,
    pub callback: Option<OAuthCallbackPolicy>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthCallbackMode {
    Auto,
    Manual,
}

impl OAuthCallbackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OAuthSessionStartOptions {
    pub callback_mode: OAuthCallbackMode,
    pub redirect_uri: String,
    pub listener_port: Option<u16>,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StartAuthContext {
    pub provider_id: Option<String>,
    pub provider: Option<Provider>,
    pub use_proxy: bool,
    pub redirect_uri: Option<String>,
    pub requested_scopes: Vec<String>,
    pub metadata: Value,
    pub http_client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshAuthContext {
    pub use_proxy: bool,
    pub metadata: Value,
    pub http_client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, Default)]
pub struct ExchangeAuthContext {
    pub use_proxy: bool,
    pub metadata: Value,
    pub http_client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthExchangeInput {
    pub callback_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthExchangeError {
    #[error("callback URL is invalid")]
    InvalidCallbackUrl,
    #[error("callback URL is missing authorization code")]
    MissingAuthorizationCode,
    #[error("OAuth state is missing")]
    MissingState,
    #[error("OAuth state mismatch")]
    StateMismatch,
    #[error("OAuth authorization was denied: {0}")]
    AccessDenied(String),
    #[error("OAuth authorization code is invalid or expired: {0}")]
    InvalidGrant(String),
    #[error("OAuth client or redirect configuration is invalid: {0}")]
    Configuration(String),
    #[error("OAuth authorization failed temporarily: {0}")]
    Retryable(String),
}

impl OAuthExchangeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidCallbackUrl => "AUTH_CALLBACK_URL_INVALID",
            Self::MissingAuthorizationCode => "AUTH_CALLBACK_CODE_MISSING",
            Self::MissingState => "AUTH_CALLBACK_STATE_MISSING",
            Self::StateMismatch => "AUTH_CALLBACK_STATE_MISMATCH",
            Self::AccessDenied(_) => "AUTH_ACCESS_DENIED",
            Self::InvalidGrant(_) => "AUTH_INVALID_GRANT",
            Self::Configuration(_) => "AUTH_CONFIGURATION_ERROR",
            Self::Retryable(_) => "AUTH_EXCHANGE_RETRYABLE",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::AccessDenied(_) | Self::InvalidGrant(_) | Self::Configuration(_)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeBinding {
    pub base_url_override: Option<String>,
    pub extra_headers: HashMap<String, String>,
    pub model_aliases: HashMap<String, String>,
    pub models_source_override: Option<String>,
    pub disable_default_auth: bool,
    /// When `Some`, the admin model-discovery paths return this exact
    /// list and skip every URL/HTTP fallback. OAuth drivers (Claude
    /// Code, future Codex tiers) use this to ship a curated allow-list
    /// since their upstream `/v1/models` either does not accept the
    /// minted Bearer or returns models the OAuth subscription cannot
    /// actually run.
    #[serde(default)]
    pub static_models_override: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialBundle {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredCredential {
    pub driver_key: String,
    pub scheme: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub meta: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProgress {
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSessionInitData {
    pub session_id: String,
    pub vendor: String,
    pub scheme: String,
    pub auth_url: String,
    pub user_code: Option<String>,
    pub callback_mode: OAuthCallbackMode,
    pub listener_state: String,
    pub listener_port: Option<u16>,
    pub redirect_uri: String,
    pub fallback_reason: Option<String>,
    pub expires_in: i64,
    pub interval: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthSessionStatusData {
    Pending {
        scheme: String,
        auth_url: String,
        user_code: Option<String>,
        callback_mode: OAuthCallbackMode,
        listener_state: String,
        listener_port: Option<u16>,
        redirect_uri: String,
        fallback_reason: Option<String>,
        error_code: Option<String>,
        last_error: Option<String>,
        expires_in: i64,
        interval: i32,
    },
    Exchanging {
        expires_in: i64,
    },
    Ready {
        expires_in: i64,
        resource_url: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthPollState {
    Pending(AuthProgress),
    Ready(CredentialBundle),
    Error { code: String, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    pub id: String,
    pub provider_id: Option<String>,
    pub driver_key: String,
    pub scheme: String,
    pub status: String,
    pub use_proxy: bool,
    pub callback_mode: OAuthCallbackMode,
    pub listener_state: String,
    pub listener_port: Option<u16>,
    pub redirect_uri: String,
    pub fallback_reason: Option<String>,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub state_json: Option<String>,
    pub context_json: Option<String>,
    pub result_json: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
    pub last_error: Option<String>,
    pub error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAuthSession {
    pub provider_id: Option<String>,
    pub driver_key: String,
    pub scheme: String,
    pub status: String,
    pub use_proxy: bool,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub state_json: Option<String>,
    pub context_json: Option<String>,
    pub result_json: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateAuthSession {
    pub use_proxy: Option<bool>,
    pub status: Option<String>,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub state_json: Option<String>,
    pub context_json: Option<String>,
    pub result_json: Option<String>,
    pub expires_at: Option<String>,
    pub poll_interval_seconds: Option<i32>,
    pub error_code: Option<String>,
    pub last_error: Option<String>,
}

#[async_trait]
pub trait AuthDriver: Send + Sync {
    fn metadata(&self) -> AuthDriverMetadata;

    async fn start(&self, _ctx: StartAuthContext) -> anyhow::Result<CreateAuthSession> {
        bail!("{} start flow is not implemented yet", self.metadata().key)
    }

    async fn poll(
        &self,
        _session: &AuthSession,
        _ctx: RefreshAuthContext,
    ) -> anyhow::Result<AuthPollState> {
        bail!("{} poll flow is not implemented yet", self.metadata().key)
    }

    async fn exchange(
        &self,
        _session: &AuthSession,
        _input: AuthExchangeInput,
        _ctx: ExchangeAuthContext,
    ) -> anyhow::Result<CredentialBundle> {
        bail!(
            "{} exchange flow is not implemented yet",
            self.metadata().key
        )
    }

    async fn refresh(
        &self,
        _credential: &StoredCredential,
        _ctx: RefreshAuthContext,
    ) -> anyhow::Result<CredentialBundle> {
        bail!(
            "{} refresh flow is not implemented yet",
            self.metadata().key
        )
    }

    fn bind_runtime(
        &self,
        _provider: &Provider,
        _credential: &StoredCredential,
    ) -> anyhow::Result<RuntimeBinding> {
        Ok(RuntimeBinding::default())
    }
}
