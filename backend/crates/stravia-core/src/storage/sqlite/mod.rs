use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use sqlx::SqlitePool;
use std::time::Duration;

use crate::db::models::{
    ApiKey, ApiKeyStats, ApiKeyWithBindings, CreateApiKey, CreateProviderRecord,
    DEFAULT_FIRST_TOKEN_TIMEOUT_MS, DEFAULT_TARGET_COOLDOWN_MS, DEFAULT_TARGET_PRIORITY,
    DEFAULT_TARGET_RETRY_BUDGET, ModelStats, OAuthCredential, Provider, ProviderStats, PutRoute,
    Route, StatsHourly, StatsOverview, Target, UpdateApiKey, UpdateProvider, UpsertOAuthCredential,
    is_valid_provider_auth_mode,
};
use crate::storage::traits::{
    AdminIdentityStore, ApiKeyAccessRecord, ApiKeyStore, AuthAccessStore, OAuthCredentialStore,
    ProviderModelStore, ProviderStore, ProviderTestResult, RouteStore, SettingsStore, Storage,
    StorageBackend, StorageBootstrap, StorageHealth, UsageStatsStore, WebProviderStore,
};
mod admin_identity;
mod provider_models;
mod web_providers;

use web_providers::SqliteWebProviderStore;

#[derive(Clone)]
pub struct SqliteStorage {
    pool: SqlitePool,
    provider_store: Arc<SqliteProviderStore>,
    web_provider_store: Arc<SqliteWebProviderStore>,
    model_store: Arc<SqliteRouteStore>,
    settings_store: Arc<SqliteSettingsStore>,
    api_key_store: Arc<SqliteApiKeyStore>,
    auth_store: Arc<SqliteAuthAccessStore>,
    admin_identity_store: Arc<SqliteAdminIdentityStore>,
    oauth_credential_store: Arc<SqliteOAuthCredentialStore>,
    usage_stats_store: Arc<SqliteUsageStatsStore>,
    bootstrap: Arc<SqliteBootstrap>,
}

impl SqliteStorage {
    pub fn from_pool(pool: SqlitePool) -> Self {
        let provider_store = Arc::new(SqliteProviderStore { pool: pool.clone() });
        let web_provider_store = Arc::new(SqliteWebProviderStore { pool: pool.clone() });
        let model_store = Arc::new(SqliteRouteStore { pool: pool.clone() });
        let settings_store = Arc::new(SqliteSettingsStore { pool: pool.clone() });
        let api_key_store = Arc::new(SqliteApiKeyStore { pool: pool.clone() });
        let auth_store = Arc::new(SqliteAuthAccessStore { pool: pool.clone() });
        let admin_identity_store = Arc::new(SqliteAdminIdentityStore { pool: pool.clone() });
        let oauth_credential_store = Arc::new(SqliteOAuthCredentialStore { pool: pool.clone() });
        let usage_stats_store = Arc::new(SqliteUsageStatsStore {
            pool: pool.clone(),
            last_route_snapshot: Arc::new(std::sync::RwLock::new(Vec::new())),
        });
        let bootstrap = Arc::new(SqliteBootstrap { pool: pool.clone() });
        Self {
            pool,
            provider_store,
            web_provider_store,
            model_store,
            settings_store,
            api_key_store,
            auth_store,
            admin_identity_store,
            oauth_credential_store,
            usage_stats_store,
            bootstrap,
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

impl Storage for SqliteStorage {
    fn providers(&self) -> &dyn ProviderStore {
        self.provider_store.as_ref()
    }

    fn web_providers(&self) -> Option<&dyn WebProviderStore> {
        Some(self.web_provider_store.as_ref())
    }

    fn routes(&self) -> &dyn RouteStore {
        self.model_store.as_ref()
    }

    fn settings(&self) -> &dyn SettingsStore {
        self.settings_store.as_ref()
    }

    fn provider_models(&self) -> &dyn ProviderModelStore {
        self
    }

    fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
        Some(self.api_key_store.as_ref())
    }

    fn auth(&self) -> Option<&dyn AuthAccessStore> {
        Some(self.auth_store.as_ref())
    }

    fn admin_identity(&self) -> Option<&dyn AdminIdentityStore> {
        Some(self.admin_identity_store.as_ref())
    }

    fn usage_stats(&self) -> &dyn UsageStatsStore {
        self.usage_stats_store.as_ref()
    }

    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore {
        self.oauth_credential_store.as_ref()
    }

    fn bootstrap(&self) -> &dyn StorageBootstrap {
        self.bootstrap.as_ref()
    }
}

mod api_keys;
mod bootstrap;
mod oauth;
mod providers;
mod routes;
mod settings;
mod usage_stats;

use admin_identity::*;
use api_keys::*;
use bootstrap::*;
use oauth::*;
use providers::*;
use routes::*;
use settings::*;
use usage_stats::*;
