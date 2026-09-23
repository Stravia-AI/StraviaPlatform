use anyhow::Context;
use chrono::{DateTime, Utc};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Gateway;
use crate::auth::types::{
    AuthBindingStatus, AuthCompletionInput, AuthCompletionValue, AuthScheme, AuthSession,
    AuthSessionCandidate, AuthSessionInitData, AuthSessionStatus, AuthSessionStatusData,
    CredentialBundle, OAuthCallbackMode, OAuthSessionStartOptions, StoredCredential,
    UpdateAuthSession,
};
use crate::db::models::*;
use crate::storage::traits::ProviderTestResult;

mod api_keys;
mod auth_data;
pub(crate) mod browser;
mod credential_protection;
mod extensions;
pub mod identity;
mod media;
mod media_generation;
mod model_data;
mod oauth;
mod observability;
pub mod provider_allowance;
mod provider_connection;
mod routes;
pub mod settings;
pub mod updates;
mod web_access;
mod web_search;

pub use crate::interaction_observation::{
    BundleRequest, BundleResourceKind, BundleStream, ClearHistoryResult, ConfirmedUsage,
    CredentialDiscoveryPage, CredentialDiscoveryQuery, CredentialDiscoverySummary, DebugState,
    DownloadTicket, FailedRequestDetail, FailedRequestPage, FailedRequestQuery,
    FailedRequestSummary, ForestPage, ForestQuery, ForestRoot, InteractionDetail,
    InteractionEventsPage, InteractionEventsQuery, InteractionSnapshot, InteractionSummary,
    ObservationEvent, ObservationQueryError, ObservationStream, ObservationUpdate, RejectionDetail,
    RejectionPage, RejectionQuery, RejectionSummary, RunDetail, TraceManifest, UsageCoverage,
};
pub use browser::{BrowserSettings, BrowserSettingsUpdate, BrowserSource};
pub use provider_connection::{
    ProviderConfigurationPreview, ProviderConfigurationPreviewInput, ProviderNetworkPermission,
};
pub use routes::{BindRouteInput, RouteTargetStatus, UnbindRouteInput};

use auth_data::*;
use model_data::*;

#[cfg(test)]
#[path = "tests/media_generation.rs"]
mod media_generation_tests;
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

impl AdminService {
    pub fn new(gw: Gateway) -> Self {
        Self { gw }
    }
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
