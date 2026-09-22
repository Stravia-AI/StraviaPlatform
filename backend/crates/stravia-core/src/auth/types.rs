use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::bail;
use stravia_runtime_contract::CancellationToken;

use crate::plugin::VendorSessionScope;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSessionCandidate {
    pub vendor_id: String,
    pub channel: String,
    pub provider_id: Option<String>,
    pub base_url: String,
    pub protocol: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, Value>,
    #[serde(default)]
    pub credentials: BTreeMap<String, Value>,
    #[serde(default)]
    pub use_proxy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCompletionInput {
    pub input: AuthCompletionValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCompletionValue {
    CallbackUrl { value: String },
    Manual { value: String },
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
pub struct AuthSessionInitData {
    pub session_id: String,
    pub vendor_id: String,
    pub channel: String,
    pub flow: stravia_vendor_sdk::AuthFlow,
    pub auth_url: Option<String>,
    pub user_code: Option<String>,
    pub manual_input: Option<stravia_vendor_sdk::AuthManualInput>,
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
        vendor_id: String,
        channel: String,
        flow: stravia_vendor_sdk::AuthFlow,
        auth_url: Option<String>,
        user_code: Option<String>,
        manual_input: Option<Box<stravia_vendor_sdk::AuthManualInput>>,
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

#[derive(Clone)]
pub(crate) struct VendorAuthSessionRuntime {
    pub(crate) scope: Arc<VendorSessionScope>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) publication: Arc<tokio::sync::Mutex<Option<crate::plugin::VendorPublicationFence>>>,
}

impl std::fmt::Debug for VendorAuthSessionRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VendorAuthSessionRuntime")
            .field("vendor_id", &self.scope.vendor_id)
            .field("data_epoch", &self.scope.data_epoch)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    pub id: String,
    pub provider_id: Option<String>,
    pub driver_key: String,
    pub channel: String,
    pub auth_descriptor: stravia_vendor_sdk::AuthDescriptor,
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
    #[serde(skip)]
    pub(crate) vendor_runtime: Option<Arc<VendorAuthSessionRuntime>>,
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
