use anyhow::Context;
use chrono::{DateTime, Utc};
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Gateway;
use crate::auth;
use crate::auth::types::{
    AuthBindingStatus, AuthPollState, AuthScheme, AuthSession, AuthSessionInitData,
    AuthSessionStatus, AuthSessionStatusData, CredentialBundle, ExchangeAuthContext,
    OAuthCallbackMode, OAuthSessionStartOptions, RefreshAuthContext, RuntimeBinding,
    StartAuthContext, StoredCredential, UpdateAuthSession,
};
use crate::db::models::*;
use crate::provider::metadata::CapabilitiesSource;
use crate::provider::{VendorRegistry, google_vertex};
use crate::storage::traits::ProviderTestResult;

mod api_keys;
mod auth_data;
mod extensions;
mod media;
mod model_catalog;
mod model_data;
mod oauth;
mod observability;
mod provider_connection;
mod routes;
pub mod settings;
mod web_access;
mod web_search;

pub use media::{
    EligibleMediaModel, MediaUnderstandingConfigError, MediaUnderstandingConfigUpdate,
    MediaUnderstandingConfigView, MediaUnderstandingState,
};
pub use routes::{BindRouteInput, UnbindRouteInput};
pub use web_search::{
    CompatibleCodexModel, CompatibleCodexProvider, EligibleSearchModel, WebSearchConfigError,
    WebSearchConfigView, WebSearchLimits,
};

use auth_data::*;
use model_catalog::*;
use model_data::*;

#[cfg(test)]
mod session_tests;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CopyProviderOptions {
    #[serde(default)]
    pub append_targets: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderOAuthStatusData {
    pub provider_id: String,
    pub provider_name: String,
    pub driver_key: String,
    pub status: String,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: Option<String>,
    pub has_refresh_token: bool,
}

#[derive(Clone)]
pub struct AdminService {
    gw: Gateway,
}

#[derive(Clone)]
pub(crate) struct ResolvedProviderRuntime {
    pub access_token: String,
    pub binding: RuntimeBinding,
}

impl AdminService {
    pub fn new(gw: Gateway) -> Self {
        Self { gw }
    }
}

pub(super) fn format_connectivity_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "Connection timeout (10s), please check Base URL or network settings".to_string();
    }
    if error.is_connect() {
        return "Unable to connect to the host, please check DNS/network settings".to_string();
    }
    error.to_string()
}

pub(super) fn coded_error(code: &str, message: &str, params: Value) -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        serde_json::json!({
            "code": code,
            "message": message,
            "params": params,
        })
    )
}
pub(super) fn normalize_name(name: &str, field: &str) -> anyhow::Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{field} cannot be empty");
    }
    Ok(trimmed.to_string())
}

pub(super) fn normalize_vendor(vendor: Option<&str>) -> Option<String> {
    vendor
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "custom")
        .map(|v| v.to_lowercase())
}
