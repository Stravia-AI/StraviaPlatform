//! Composition extensions for embedding and extending `stravia-server`.
//!
//! Community Edition runs with [`HttpAppExtensions::default()`], installing
//! single-admin authentication/authorization and reporting `edition: "ce"`.
//! External shells (such as `ee/`) inject custom capability snapshots,
//! authentication/authorization providers, post-migration hooks, and extra routes
//! through [`HttpAppExtensions`] without duplicating server bootstrap or middleware.

use std::sync::Arc;

use axum::Router;
use serde::{Deserialize, Serialize};
use stravia_core::PreparedGateway;
use stravia_core::admin::identity::{AdminAuthProvider, AdminAuthorizer};

/// Distribution edition reported by `GET /api/v1/capabilities`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edition {
    Ce,
    Commercial,
}

/// Licensed commercial tier when running a commercial build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseTier {
    Pro,
    Enterprise,
}

/// Offline license verification status when running a commercial build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseStatus {
    Valid,
    Missing,
    Expired,
    Invalid,
}

/// Capability snapshot supplied by a [`CapabilityProvider`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitiesSnapshot {
    pub edition: Edition,
    pub license_tier: Option<LicenseTier>,
    pub license_status: Option<LicenseStatus>,
    pub capabilities: Vec<String>,
    pub degraded_reason: Option<String>,
}

/// Wire JSON payload returned by authenticated `GET /api/v1/capabilities`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesResponse {
    pub edition: Edition,
    pub license_tier: Option<LicenseTier>,
    pub license_status: Option<LicenseStatus>,
    pub capabilities: Vec<String>,
    pub degraded_reason: Option<String>,
    pub auth_mode: String,
}

/// Supplies edition, license status, and active capability keys to `/api/v1/capabilities`.
pub trait CapabilityProvider: Send + Sync {
    fn capabilities(&self) -> CapabilitiesSnapshot;
}

/// Default Community Edition capability provider reporting zero commercial capabilities.
#[derive(Clone, Copy, Debug, Default)]
pub struct CeCapabilityProvider;

impl CapabilityProvider for CeCapabilityProvider {
    fn capabilities(&self) -> CapabilitiesSnapshot {
        CapabilitiesSnapshot {
            edition: Edition::Ce,
            license_tier: None,
            license_status: None,
            capabilities: Vec::new(),
            degraded_reason: None,
        }
    }
}

/// Hook executed immediately after core storage migrations succeed in `Gateway::prepare`.
#[async_trait::async_trait]
pub trait PostPrepareHook: Send + Sync {
    async fn after_core_migrations(&self, prepared: &PreparedGateway) -> anyhow::Result<()>;
}

/// Optional composition extensions injected into `HttpAppConfig` and `ServerStartupConfig`.
#[derive(Clone, Default)]
pub struct HttpAppExtensions {
    pub admin_auth_provider: Option<Arc<dyn AdminAuthProvider>>,
    pub admin_authorizer: Option<Arc<dyn AdminAuthorizer>>,
    pub capability_provider: Option<Arc<dyn CapabilityProvider>>,
    pub extra_admin_routes: Option<Router>,
    pub extra_public_routes: Option<Router>,
    pub post_prepare_hook: Option<Arc<dyn PostPrepareHook>>,
}

impl HttpAppExtensions {
    pub fn with_admin_auth_provider(mut self, provider: Arc<dyn AdminAuthProvider>) -> Self {
        self.admin_auth_provider = Some(provider);
        self
    }

    pub fn with_admin_authorizer(mut self, authorizer: Arc<dyn AdminAuthorizer>) -> Self {
        self.admin_authorizer = Some(authorizer);
        self
    }

    pub fn with_capability_provider(mut self, provider: Arc<dyn CapabilityProvider>) -> Self {
        self.capability_provider = Some(provider);
        self
    }

    /// Mounts extra routes under `/api/v1` protected by `require_admin`, `AdminAuthorizer`,
    /// `AdminEntryPolicy`, and Origin/CSRF validation.
    pub fn with_extra_admin_routes(mut self, routes: Router) -> Self {
        self.extra_admin_routes = Some(routes);
        self
    }

    /// Mounts extra unauthenticated routes under `/api/v1` protected by `AdminEntryPolicy` and CORS.
    pub fn with_extra_public_routes(mut self, routes: Router) -> Self {
        self.extra_public_routes = Some(routes);
        self
    }

    /// Registers a hook executed after core migrations succeed during startup and `/setup`.
    pub fn with_post_prepare_hook(mut self, hook: Arc<dyn PostPrepareHook>) -> Self {
        self.post_prepare_hook = Some(hook);
        self
    }
}
