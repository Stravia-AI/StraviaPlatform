use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use sqlx::{Pool, Postgres, postgres::PgPoolOptions};
use std::time::Duration;

use crate::db::models::{
    ApiKey, ApiKeyStats, ApiKeyWithBindings, CreateApiKey, CreateModel, CreateModelBackend,
    CreateProviderRecord, LogPage, LogQuery, Model, ModelBackend, ModelStats, OAuthCredential,
    Provider, ProviderStats, RequestLog, StatsHourly, StatsOverview, UpdateApiKey, UpdateModel,
    UpdateProvider, UpsertOAuthCredential, is_valid_provider_auth_mode,
};
use crate::logging::LogEntry;
use crate::storage::sql::config::SqlBackendConfig;
use crate::storage::traits::{
    ApiKeyAccessRecord, ApiKeyStore, AuthAccessStore, LogStore, ModelBackendStore,
    ModelSnapshotStore, ModelStore, OAuthCredentialStore, ProviderModelStore, ProviderStore,
    ProviderTestResult, SettingsStore, Storage, StorageBackend, StorageBootstrap, StorageHealth,
    WebProviderStore,
};
mod provider_models;
mod web_providers;

use web_providers::PostgresWebProviderStore;

#[derive(Clone)]
pub struct PostgresAdapter {
    pool: Pool<Postgres>,
}

#[derive(Debug, Clone)]
pub struct PostgresHealth {
    pub can_connect: bool,
    pub schema_compatible: bool,
}

impl PostgresAdapter {
    pub async fn connect(config: SqlBackendConfig) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(config.idle_timeout)
            .connect(&config.url)
            .await
            .with_context(|| format!("failed to connect postgres: {}", config.url))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn health(&self) -> PostgresHealth {
        let can_connect = self.ping().await.is_ok();
        // schema_compatible: verify the final-state `models` table exists,
        // which confirms migrations have completed (routes → models rename done).
        let schema_compatible = if can_connect {
            pg_table_exists(&self.pool, "models").await.unwrap_or(false)
        } else {
            false
        };
        PostgresHealth {
            can_connect,
            schema_compatible,
        }
    }
}

#[derive(Clone)]
pub struct PostgresStorage {
    pool: Pool<Postgres>,
    provider_store: Arc<PostgresProviderStore>,
    web_provider_store: Arc<PostgresWebProviderStore>,
    model_store: Arc<PostgresModelStore>,
    model_backend_store: Arc<PostgresModelBackendStore>,
    settings_store: Arc<PostgresSettingsStore>,
    api_key_store: Arc<PostgresApiKeyStore>,
    auth_store: Arc<PostgresAuthAccessStore>,
    oauth_credential_store: Arc<PostgresOAuthCredentialStore>,
    log_store: Arc<PostgresLogStore>,
    bootstrap: Arc<PostgresBootstrap>,
}

impl PostgresStorage {
    pub async fn connect(config: SqlBackendConfig) -> anyhow::Result<Self> {
        let adapter = PostgresAdapter::connect(config).await?;
        let pool = adapter.pool().clone();
        let provider_store = Arc::new(PostgresProviderStore { pool: pool.clone() });
        let web_provider_store = Arc::new(PostgresWebProviderStore { pool: pool.clone() });
        let model_store = Arc::new(PostgresModelStore { pool: pool.clone() });
        let model_backend_store = Arc::new(PostgresModelBackendStore { pool: pool.clone() });
        let settings_store = Arc::new(PostgresSettingsStore { pool: pool.clone() });
        let api_key_store = Arc::new(PostgresApiKeyStore { pool: pool.clone() });
        let auth_store = Arc::new(PostgresAuthAccessStore { pool: pool.clone() });
        let oauth_credential_store = Arc::new(PostgresOAuthCredentialStore { pool: pool.clone() });
        let log_store = Arc::new(PostgresLogStore { pool: pool.clone() });
        let bootstrap = Arc::new(PostgresBootstrap { adapter });
        Ok(Self {
            pool,
            provider_store,
            web_provider_store,
            model_store,
            model_backend_store,
            settings_store,
            api_key_store,
            auth_store,
            oauth_credential_store,
            log_store,
            bootstrap,
        })
    }

    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }
}

impl Storage for PostgresStorage {
    fn providers(&self) -> &dyn ProviderStore {
        self.provider_store.as_ref()
    }

    fn web_providers(&self) -> Option<&dyn WebProviderStore> {
        Some(self.web_provider_store.as_ref())
    }

    fn models(&self) -> &dyn ModelStore {
        self.model_store.as_ref()
    }

    fn snapshots(&self) -> &dyn ModelSnapshotStore {
        self.model_store.as_ref()
    }

    fn settings(&self) -> &dyn SettingsStore {
        self.settings_store.as_ref()
    }

    fn model_backends(&self) -> Option<&dyn ModelBackendStore> {
        Some(self.model_backend_store.as_ref())
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

    fn logs(&self) -> &dyn LogStore {
        self.log_store.as_ref()
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
mod logs;
mod models;
mod oauth;
mod providers;
mod settings;

use api_keys::*;
use bootstrap::*;
use logs::*;
use models::*;
use oauth::*;
use providers::*;
use settings::*;
