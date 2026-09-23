use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::db::models::{
    ApiKeyStats, CreateProviderRecord, DEFAULT_FIRST_TOKEN_TIMEOUT_MS, DEFAULT_TARGET_COOLDOWN_MS,
    DEFAULT_TARGET_PRIORITY, DEFAULT_TARGET_RETRY_BUDGET, ModelStats, OAuthCredential, Provider,
    ProviderCredentialVersion, ProviderStats, PutRoute, RouteConfig, StatsOverview, StatsSeries,
    TargetConfig, UpdateProvider, UpsertOAuthCredential,
};
use crate::plugin::PluginStore;
use crate::provider_models::{
    NewProviderModelRecord, ProviderModelMutation, ProviderModelReconciliation,
    ProviderModelRecord, ProviderModelSelectionPolicy, ProviderModelSourceKind, SnapshotState,
};

use super::traits::{
    ApiKeyStore, AuthAccessStore, OAuthCredentialStore, ProviderModelStore, ProviderStore,
    ProviderTestResult, RouteStore, SettingsStore, Storage, StorageBackend, StorageBootstrap,
    StorageHealth, UsageStatsStore,
};

use std::sync::Arc;

#[derive(Clone)]
pub struct MemoryStorage {
    providers: Arc<RwLock<Vec<Provider>>>,
    models: Arc<RwLock<Vec<RouteConfig>>>,
    settings: Arc<RwLock<Vec<(String, String)>>>,
    provider_models: Arc<RwLock<Vec<ProviderModelRecord>>>,
    oauth_credentials: Arc<MemoryOAuthCredentialStore>,
    plugin_store: PluginStore,
}

impl MemoryStorage {
    pub fn new(
        providers: Vec<Provider>,
        models: Vec<RouteConfig>,
        settings: Vec<(String, String)>,
    ) -> Self {
        let providers = Arc::new(RwLock::new(providers));
        let provider_models = Arc::new(RwLock::new(Vec::new()));
        let oauth_credentials = Arc::new(MemoryOAuthCredentialStore {
            credentials: RwLock::new(std::collections::HashMap::new()),
        });
        let plugin_store = PluginStore::memory(
            providers.clone(),
            provider_models.clone(),
            oauth_credentials.clone(),
        );
        Self {
            providers,
            models: Arc::new(RwLock::new(models)),
            settings: Arc::new(RwLock::new(settings)),
            provider_models,
            oauth_credentials,
            plugin_store,
        }
    }
}

pub(crate) struct MemoryOAuthCredentialStore {
    pub(crate) credentials: RwLock<std::collections::HashMap<String, OAuthCredential>>,
}

impl Storage for MemoryStorage {
    fn vendor_plugins(&self) -> &PluginStore {
        &self.plugin_store
    }

    fn providers(&self) -> &dyn ProviderStore {
        self
    }
    fn routes(&self) -> &dyn RouteStore {
        self
    }
    fn provider_models(&self) -> &dyn ProviderModelStore {
        self
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
    fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
        None
    }
    fn auth(&self) -> Option<&dyn AuthAccessStore> {
        None
    }
    fn usage_stats(&self) -> &dyn UsageStatsStore {
        self
    }
    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore {
        self.oauth_credentials.as_ref()
    }
    fn bootstrap(&self) -> &dyn StorageBootstrap {
        self
    }
}

#[async_trait]
impl ProviderStore for MemoryStorage {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        Ok(self.providers.read().await.clone())
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        Ok(self
            .providers
            .read()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned())
    }

    async fn create(&self, input: CreateProviderRecord) -> anyhow::Result<Provider> {
        let now = now_rfc3339();
        let provider = Provider {
            id: stravia_runtime_contract::identifier::new_id(),
            name: input.name,
            vendor: input.vendor,
            protocol: input.protocol,
            base_url: input.base_url,
            preset_key: input.preset_key,
            channel: input.channel,
            models_source: input.models_source,
            static_models: input.static_models,
            api_key: input.api_key,
            adapter_credentials: input.adapter_credentials,
            vendor_options: input.vendor_options,
            auth_mode: input.auth_mode,
            use_proxy: input.use_proxy,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            credential_status: "ok".to_string(),
            credential_invalid_at: None,
            revision: 0,
            created_at: now.clone(),
            updated_at: now,
        };
        self.providers.write().await.push(provider.clone());
        Ok(provider)
    }

    async fn update(&self, id: &str, input: UpdateProvider) -> anyhow::Result<Provider> {
        // ADR-0073：黑名单字段之外的写入视为新凭据证据，恢复 ok。
        let reset_credential_status =
            !input.preserve_credential_status && input.resets_credential_status();
        let mut providers = self.providers.write().await;
        let provider = providers
            .iter_mut()
            .find(|provider| provider.id == id)
            .context("provider not found for update")?;
        if let Some(value) = input.name {
            provider.name = value;
        }
        if let Some(value) = input.vendor {
            provider.vendor = Some(value);
        }
        if let Some(value) = input.protocol {
            provider.protocol = value;
        }
        if let Some(value) = input.base_url {
            provider.base_url = value;
        }
        if let Some(value) = input.preset_key {
            provider.preset_key = Some(value);
        }
        if let Some(value) = input.channel {
            provider.channel = Some(value);
        }
        if let Some(value) = input.models_source {
            provider.models_source = Some(value);
        }
        if let Some(value) = input.static_models {
            provider.static_models = Some(value);
        }
        if let Some(value) = input.api_key {
            provider.api_key = value;
        }
        if let Some(value) = input.adapter_credentials {
            provider.adapter_credentials = serde_json::to_string(&value)?;
        }
        if let Some(value) = input.vendor_options {
            provider.vendor_options = serde_json::to_string(&value)?;
        }
        if let Some(value) = input.auth_mode {
            provider.auth_mode = value;
        }
        if let Some(value) = input.use_proxy {
            provider.use_proxy = value;
        }
        if let Some(value) = input.is_enabled {
            provider.is_enabled = value;
        }
        if reset_credential_status {
            provider.credential_status = "ok".to_string();
            provider.credential_invalid_at = None;
        }
        provider.revision += 1;
        provider.updated_at = now_rfc3339();
        Ok(provider.clone())
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut providers = self.providers.write().await;
        let mut routes = self.models.write().await;
        let mut provider_models = self.provider_models.write().await;
        let mut oauth_credentials = self.oauth_credentials.credentials.write().await;
        self.plugin_store.remove_memory_provider_data(id).await;
        providers.retain(|provider| provider.id != id);
        provider_models.retain(|model| model.provider_id != id);
        oauth_credentials.remove(id);
        for route in routes.iter_mut() {
            route
                .targets
                .retain(|target| target.provider_id().as_str() != id);
            route.refresh_supported_thinking_levels();
        }
        routes.retain(|route| !route.targets.is_empty());
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let providers = self.providers.read().await;
        Ok(providers
            .iter()
            .any(|p| p.name == name && exclude_id.is_none_or(|eid| p.id != eid)))
    }

    async fn record_test_result(
        &self,
        provider_id: &str,
        result: ProviderTestResult,
    ) -> anyhow::Result<()> {
        let mut providers = self.providers.write().await;
        let provider = providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .context("provider not found for test result")?;
        provider.last_test_success = Some(result.success);
        provider.last_test_at = Some(result.tested_at);
        // ADR-0073：测试成功是恢复路径——上游接受了当前凭据，清除失效。
        if result.success {
            provider.credential_status = "ok".to_string();
            provider.credential_invalid_at = None;
        }
        provider.revision += 1;
        provider.updated_at = now_rfc3339();
        Ok(())
    }

    async fn mark_credential_invalid(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
    ) -> anyhow::Result<bool> {
        // 条件写：凭据代际未变才把拒绝证据归到当前凭据上。
        let mut providers = self.providers.write().await;
        let Some(index) = providers.iter().position(|provider| provider.id == id) else {
            return Ok(false);
        };
        if providers[index].revision != expected.provider_revision {
            return Ok(false);
        }
        let oauth_status_version = self
            .oauth_credentials
            .credentials
            .read()
            .await
            .get(id)
            .map(|credential| credential.status_version);
        if oauth_status_version != expected.oauth_status_version {
            return Ok(false);
        }
        let provider = &mut providers[index];
        provider.credential_status = "invalid".to_string();
        if provider.credential_invalid_at.is_none() {
            provider.credential_invalid_at = Some(now_rfc3339());
        }
        provider.revision += 1;
        Ok(true)
    }

    async fn clear_credential_invalid(&self, id: &str) -> anyhow::Result<()> {
        let mut providers = self.providers.write().await;
        // 仅在失效时落写，与 SQL 实现一致——避免主动刷新空转 revision。
        if let Some(provider) = providers
            .iter_mut()
            .find(|provider| provider.id == id && provider.credential_invalid())
        {
            provider.credential_status = "ok".to_string();
            provider.credential_invalid_at = None;
            provider.revision += 1;
        }
        Ok(())
    }

    async fn credential_invalid_provider_ids(
        &self,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        Ok(self
            .providers
            .read()
            .await
            .iter()
            .filter(|provider| provider.credential_invalid())
            .map(|provider| provider.id.clone())
            .collect())
    }
}

#[async_trait]
impl RouteStore for MemoryStorage {
    async fn list(&self) -> anyhow::Result<Vec<RouteConfig>> {
        Ok(self.models.read().await.clone())
    }

    async fn list_active(&self) -> anyhow::Result<Vec<RouteConfig>> {
        let models = self.models.read().await;
        Ok(models
            .iter()
            .filter(|route| route.is_enabled)
            .cloned()
            .collect())
    }

    async fn get(&self, route_id: &str) -> anyhow::Result<Option<RouteConfig>> {
        Ok(self
            .models
            .read()
            .await
            .iter()
            .find(|route| route.model_id == route_id)
            .cloned())
    }

    async fn put(&self, input: PutRoute) -> anyhow::Result<RouteConfig> {
        anyhow::ensure!(
            input
                .targets
                .as_ref()
                .is_none_or(|targets| targets.iter().any(|target| target.enabled)),
            "a Route requires at least one enabled Target"
        );
        anyhow::ensure!(
            input.id.is_some() || input.targets.is_some(),
            "a new Route requires Targets"
        );
        let mut routes = self.models.write().await;
        let storage_id = input
            .id
            .as_ref()
            .map(|id| id.as_str().to_owned())
            .unwrap_or_else(stravia_runtime_contract::identifier::new_id);
        anyhow::ensure!(
            !routes
                .iter()
                .any(|route| route.model_id == input.model_id.as_str() && route.id != storage_id),
            "Route ID already exists: {}",
            input.model_id
        );
        if input.targets.is_none() {
            let route = routes
                .iter_mut()
                .find(|route| route.id == storage_id)
                .context("Route not found for update")?;
            route.model_id = input.model_id;
            route.display_name = input.display_name;
            route.balance = input.selection_strategy;
            route.is_enabled = input.is_enabled;
            route.default_thinking_level = input
                .default_thinking_level
                .map(|level| level.as_str().to_owned());
            return Ok(route.clone());
        }
        let created_at = routes
            .iter()
            .find(|route| route.id == storage_id)
            .map(|route| route.created_at.clone())
            .unwrap_or_else(now_rfc3339);
        let existing_targets = routes
            .iter()
            .find(|route| route.id == storage_id)
            .map(|route| route.targets.as_slice())
            .unwrap_or(&[]);
        let targets = input
            .targets
            .expect("checked target replacement")
            .into_iter()
            .map(|target| {
                let destination = crate::db::identity::TargetDestination::new(
                    target.provider_id.trim().into(),
                    target.model.as_deref().map(str::trim).map(Into::into),
                );
                let previous = existing_targets
                    .iter()
                    .find(|current| current.destination == destination);
                TargetConfig {
                    id: previous
                        .map(|current| current.id.clone())
                        .unwrap_or_else(|| stravia_runtime_contract::identifier::new_id().into()),
                    model_id: storage_id.clone().into(),
                    destination,
                    enabled: target.enabled,
                    priority: target.priority.unwrap_or(DEFAULT_TARGET_PRIORITY),
                    first_token_timeout_ms: target
                        .first_token_timeout_ms
                        .unwrap_or(DEFAULT_FIRST_TOKEN_TIMEOUT_MS),
                    target_retry_budget: target
                        .target_retry_budget
                        .unwrap_or(DEFAULT_TARGET_RETRY_BUDGET),
                    target_cooldown_ms: target
                        .target_cooldown_ms
                        .unwrap_or(DEFAULT_TARGET_COOLDOWN_MS),
                    created_at: previous
                        .map(|current| current.created_at.clone())
                        .unwrap_or_else(now_rfc3339),
                    thinking_level_map: target.thinking_level_map,
                }
            })
            .collect::<Vec<_>>();
        let mut route = RouteConfig {
            id: storage_id.clone().into(),
            model_id: input.model_id,
            display_name: input.display_name,
            balance: input.selection_strategy,
            is_enabled: input.is_enabled,
            created_at,
            default_thinking_level: input
                .default_thinking_level
                .map(|level| level.as_str().to_string()),
            supported_thinking_levels: Vec::new(),
            context_window: None,
            output_max_tokens: None,
            supports_image_input: false,
            targets,
        };
        route.refresh_supported_thinking_levels();
        if let Some(current) = routes.iter_mut().find(|route| route.id == storage_id) {
            *current = route.clone();
        } else {
            routes.push(route.clone());
        }
        Ok(route)
    }

    async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        self.models
            .write()
            .await
            .retain(|route| route.model_id != route_id);
        Ok(())
    }
}

#[async_trait]
impl SettingsStore for MemoryStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let settings = self.settings.read().await;
        Ok(settings
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone()))
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut settings = self.settings.write().await;
        if let Some(entry) = settings.iter_mut().find(|(k, _)| k == key) {
            entry.1 = value.to_string();
        } else {
            settings.push((key.to_string(), value.to_string()));
        }
        Ok(())
    }
}

#[async_trait]
impl UsageStatsStore for MemoryStorage {
    async fn route_scheduling_snapshot(&self) -> super::traits::RouteSchedulingUsage {
        super::traits::RouteSchedulingUsage::default()
    }

    async fn stats_overview(&self, _hours: Option<i64>) -> anyhow::Result<StatsOverview> {
        Ok(StatsOverview::default())
    }

    async fn stats_series(
        &self,
        _hours: i64,
        _bucket_ms: i64,
        _tz_offset_ms: i64,
    ) -> anyhow::Result<Vec<StatsSeries>> {
        Ok(vec![])
    }

    async fn stats_by_model(&self, _hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>> {
        Ok(vec![])
    }

    async fn stats_by_provider(&self, _hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>> {
        Ok(vec![])
    }

    async fn stats_by_api_key(&self, _hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>> {
        Ok(vec![])
    }
}

#[async_trait]
impl StorageBootstrap for MemoryStorage {
    async fn health(&self) -> anyhow::Result<StorageHealth> {
        Ok(StorageHealth {
            backend: StorageBackend::Sqlite,
            can_connect: true,
            schema_compatible: true,
            writable: false,
        })
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn parse_datetime_utc(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|value| {
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(value, chrono::Utc)
                })
        })
}

#[async_trait]
impl ProviderModelStore for MemoryStorage {
    async fn list_for_provider(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderModelRecord>> {
        Ok(self
            .provider_models
            .read()
            .await
            .iter()
            .filter(|item| item.provider_id == provider_id)
            .cloned()
            .collect())
    }

    async fn get(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<Option<ProviderModelRecord>> {
        Ok(self
            .provider_models
            .read()
            .await
            .iter()
            .find(|item| item.provider_id == provider_id && item.model_id == model_id)
            .cloned())
    }

    async fn apply_reconciliation(
        &self,
        provider_id: &str,
        reconciliation: ProviderModelReconciliation,
    ) -> anyhow::Result<()> {
        let now = now_rfc3339();
        let mut items = self.provider_models.write().await;
        for update in &reconciliation.updates {
            anyhow::ensure!(
                items.iter().any(|item| item.provider_id == provider_id
                    && item.model_id == update.model_id
                    && item.source_kind == ProviderModelSourceKind::Discovered
                    && item.revision == update.expected_revision),
                "Provider Model has changed while synchronizing discovered models"
            );
        }
        for input in &reconciliation.inserts {
            anyhow::ensure!(
                !items
                    .iter()
                    .any(|item| item.provider_id == provider_id && item.model_id == input.model_id),
                "Provider Model has changed while synchronizing discovered models"
            );
        }
        for update in reconciliation.updates {
            if let Some(item) = items.iter_mut().find(|item| {
                item.provider_id == provider_id
                    && item.model_id == update.model_id
                    && item.source_kind == ProviderModelSourceKind::Discovered
            }) {
                let changed = item.presence != update.presence
                    || item.metadata.status != update.lifecycle_status
                    || update.metadata.is_some()
                    || update.snapshot_state.is_some()
                    || item.metadata_source_provider_id != update.metadata_source_provider_id;
                if changed {
                    item.presence = update.presence;
                    item.metadata_source_provider_id = update.metadata_source_provider_id;
                    if let Some(snapshot_state) = update.snapshot_state {
                        item.snapshot_state = snapshot_state;
                    }
                    if let Some(metadata) = update.metadata {
                        item.metadata = metadata;
                        item.cost_rules = item.metadata.cost_rules();
                    } else {
                        item.metadata.status = update.lifecycle_status;
                    }
                    item.revision += 1;
                    item.updated_at = now.clone();
                }
            }
        }
        for input in reconciliation.inserts {
            items.push(memory_provider_model(input, now.clone()));
        }
        Ok(())
    }

    async fn create(&self, input: NewProviderModelRecord) -> anyhow::Result<ProviderModelMutation> {
        let mut items = self.provider_models.write().await;
        if items
            .iter()
            .any(|item| item.provider_id == input.provider_id && item.model_id == input.model_id)
        {
            return Ok(ProviderModelMutation::Conflict);
        }
        let record = memory_provider_model(input, now_rfc3339());
        items.push(record.clone());
        Ok(ProviderModelMutation::Applied(Box::new(record)))
    }

    async fn update_metadata(
        &self,
        provider_id: &str,
        model_id: &str,
        metadata: crate::provider_models::ProviderModelMetadata,
        snapshot_state: SnapshotState,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mut items = self.provider_models.write().await;
        let Some(item) = items
            .iter_mut()
            .find(|item| item.provider_id == provider_id && item.model_id == model_id)
        else {
            return Ok(ProviderModelMutation::NotFound);
        };
        if item.revision != expected_revision {
            return Ok(ProviderModelMutation::Conflict);
        }
        item.metadata = metadata;
        item.snapshot_state = snapshot_state;
        item.cost_rules = item.metadata.cost_rules();
        item.revision += 1;
        item.updated_at = now_rfc3339();
        Ok(ProviderModelMutation::Applied(Box::new(item.clone())))
    }

    async fn update_selection_policy(
        &self,
        provider_id: &str,
        model_id: &str,
        policy: ProviderModelSelectionPolicy,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mut items = self.provider_models.write().await;
        let Some(item) = items
            .iter_mut()
            .find(|item| item.provider_id == provider_id && item.model_id == model_id)
        else {
            return Ok(ProviderModelMutation::NotFound);
        };
        if item.revision != expected_revision {
            return Ok(ProviderModelMutation::Conflict);
        }
        item.selection_policy = policy;
        item.revision += 1;
        item.updated_at = now_rfc3339();
        Ok(ProviderModelMutation::Applied(Box::new(item.clone())))
    }

    async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let mut items = self.provider_models.write().await;
        let old_len = items.len();
        items.retain(|item| {
            item.provider_id != provider_id
                || item.model_id != model_id
                || item.source_kind != ProviderModelSourceKind::Manual
        });
        Ok(items.len() != old_len)
    }
}

fn memory_provider_model(input: NewProviderModelRecord, now: String) -> ProviderModelRecord {
    let cost_rules = input.metadata.cost_rules();
    ProviderModelRecord {
        provider_id: input.provider_id,
        model_id: input.model_id,
        source_kind: input.source_kind,
        snapshot_state: input.snapshot_state,
        metadata_source_provider_id: input.metadata_source_provider_id,
        presence: input.presence,
        selection_policy: input.selection_policy,
        metadata: input.metadata,
        revision: 1,
        created_at: now.clone(),
        updated_at: now,
        cost_rules,
    }
}

#[async_trait]
impl OAuthCredentialStore for MemoryOAuthCredentialStore {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>> {
        Ok(self.credentials.read().await.get(provider_id).cloned())
    }

    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        let now = now_rfc3339();
        let mut map = self.credentials.write().await;
        let version = map
            .get(provider_id)
            .map(|c| c.status_version + 1)
            .unwrap_or(0);
        let cred = OAuthCredential {
            provider_id: provider_id.to_string(),
            connection_id: stravia_runtime_contract::identifier::new_id(),
            driver_key: input.driver_key,
            scheme: input.scheme,
            access_token: input.access_token,
            refresh_token: input.refresh_token,
            expires_at: input.expires_at,
            resource_url: input.resource_url,
            subject_id: input.subject_id,
            scopes: input.scopes.unwrap_or_else(|| "[]".to_string()),
            meta: input.meta.unwrap_or_else(|| "{}".to_string()),
            status: "connected".to_string(),
            status_version: version,
            last_error: None,
            last_refresh_at: map.get(provider_id).and_then(|c| c.last_refresh_at.clone()),
            created_at: map
                .get(provider_id)
                .map(|c| c.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        map.insert(provider_id.to_string(), cred.clone());
        Ok(cred)
    }

    async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        self.credentials.write().await.remove(provider_id);
        Ok(())
    }

    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        let mut map = self.credentials.write().await;
        let Some(cred) = map.get_mut(provider_id) else {
            return Ok(None);
        };
        if cred.status != "connected" || cred.status_version != expected_version {
            return Ok(None);
        }
        cred.status = "refreshing".to_string();
        cred.status_version += 1;
        cred.updated_at = now_rfc3339();
        Ok(Some(cred.clone()))
    }

    async fn cancel_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<bool> {
        let mut map = self.credentials.write().await;
        let Some(credential) = map.get_mut(provider_id) else {
            return Ok(false);
        };
        if credential.status != "refreshing" || credential.status_version != expected_version {
            return Ok(false);
        }
        credential.status = "connected".to_string();
        credential.status_version += 1;
        credential.updated_at = now_rfc3339();
        Ok(true)
    }

    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        let mut map = self.credentials.write().await;
        let cred = map.get_mut(provider_id).context("credential not found")?;
        anyhow::ensure!(
            cred.status == "refreshing" && cred.status_version == expected_version,
            "credential refresh lease is no longer current"
        );
        let now = now_rfc3339();
        cred.driver_key = input.driver_key;
        cred.scheme = input.scheme;
        cred.access_token = input.access_token;
        cred.refresh_token = input.refresh_token;
        cred.expires_at = input.expires_at;
        cred.resource_url = input.resource_url;
        cred.subject_id = input.subject_id;
        if let Some(scopes) = input.scopes {
            cred.scopes = scopes;
        }
        if let Some(meta) = input.meta {
            cred.meta = meta;
        }
        cred.status = "connected".to_string();
        cred.status_version += 1;
        cred.last_error = None;
        cred.last_refresh_at = Some(now.clone());
        cred.updated_at = now;
        Ok(cred.clone())
    }

    async fn fail_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
    ) -> anyhow::Result<bool> {
        let mut map = self.credentials.write().await;
        let Some(cred) = map.get_mut(provider_id) else {
            return Ok(false);
        };
        if cred.status != "refreshing" || cred.status_version != expected_version {
            return Ok(false);
        }
        cred.status = "error".to_string();
        cred.last_error = Some(error_message.to_string());
        cred.status_version += 1;
        cred.updated_at = now_rfc3339();
        Ok(true)
    }

    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>> {
        let cutoff = chrono::Utc::now() + chrono::Duration::from_std(before)?;
        let map = self.credentials.read().await;
        Ok(map
            .values()
            .filter(|credential| {
                credential.status == "connected"
                    && credential
                        .expires_at
                        .as_deref()
                        .and_then(parse_datetime_utc)
                        .is_some_and(|expires_at| expires_at <= cutoff)
            })
            .cloned()
            .collect())
    }

    async fn recover_stale_refreshing(&self, _timeout: Duration) -> anyhow::Result<u64> {
        let mut map = self.credentials.write().await;
        let mut count = 0u64;
        for cred in map.values_mut() {
            if cred.status == "refreshing" {
                cred.status = "connected".to_string();
                cred.last_error = Some("refresh lease expired; retrying is allowed".to_string());
                cred.status_version += 1;
                cred.updated_at = now_rfc3339();
                count += 1;
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_models::ProviderModelPresence;

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.into(),
            name: id.into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "http://localhost".into(),
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            credential_status: "ok".into(),
            credential_invalid_at: None,
            revision: 0,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        }
    }

    fn target(provider_id: &str, model: &str) -> crate::db::models::CreateTarget {
        crate::db::models::CreateTarget {
            enabled: true,
            provider_id: provider_id.into(),
            model: Some(model.into()),
            priority: Some(1),
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
            thinking_level_map: Vec::new(),
        }
    }

    #[tokio::test]
    async fn route_put_is_an_exact_aggregate_contract() {
        let storage = MemoryStorage::new(Vec::new(), Vec::new(), Vec::new());
        let route = storage
            .put(PutRoute {
                id: None,
                model_id: "CaseRoute".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![target("p1", "m1"), target("p2", "m2")]),
                default_thinking_level: None,
            })
            .await
            .expect("put Route");

        assert!(
            RouteStore::get(&storage, "caseroute")
                .await
                .expect("get")
                .is_none()
        );
        assert!(
            RouteStore::get(&storage, &route.id)
                .await
                .expect("get")
                .is_none()
        );
        assert_eq!(
            RouteStore::get(&storage, "CaseRoute")
                .await
                .expect("get")
                .expect("Route")
                .targets
                .len(),
            2
        );

        let renamed = storage
            .put(PutRoute {
                id: Some(route.id.clone()),
                model_id: route.model_id.clone(),
                display_name: Some("Friendly".into()),
                selection_strategy: route.balance.clone(),
                is_enabled: route.is_enabled,
                targets: None,
                default_thinking_level: None,
            })
            .await
            .expect("metadata-only update");
        assert_eq!(renamed.targets[0].id, route.targets[0].id);
        assert_eq!(renamed.targets[1].created_at, route.targets[1].created_at);
        let surviving_id = route.targets[1].id.clone();
        let surviving_created_at = route.targets[1].created_at.clone();
        let updated = storage
            .put(PutRoute {
                id: Some(route.id),
                model_id: "CaseRoute".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![target("p2", "m2")]),
                default_thinking_level: None,
            })
            .await
            .expect("replace Route aggregate");
        assert_eq!(updated.targets.len(), 1);
        assert_eq!(updated.targets[0].provider_id(), "p2");
        assert_eq!(updated.targets[0].id, surviving_id);
        assert_eq!(updated.targets[0].created_at, surviving_created_at);
    }

    #[tokio::test]
    async fn provider_delete_prunes_targets_and_routes_atomically() {
        let storage =
            MemoryStorage::new(vec![provider("p1"), provider("p2")], Vec::new(), Vec::new());
        storage
            .put(PutRoute {
                id: None,
                model_id: "empty-after-delete".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![target("p1", "m1")]),
                default_thinking_level: None,
            })
            .await
            .expect("put disposable Route");
        storage
            .put(PutRoute {
                id: None,
                model_id: "survives-delete".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![
                    target("p1", "m1"),
                    crate::db::models::CreateTarget {
                        enabled: false,
                        ..target("p2", "m2")
                    },
                ]),
                default_thinking_level: None,
            })
            .await
            .expect("put durable Route");

        ProviderStore::delete(&storage, "p1")
            .await
            .expect("delete Provider");

        assert!(
            RouteStore::get(&storage, "empty-after-delete")
                .await
                .expect("get")
                .is_none()
        );
        let survivor = RouteStore::get(&storage, "survives-delete")
            .await
            .expect("get")
            .expect("surviving Route");
        assert_eq!(survivor.targets.len(), 1);
        assert_eq!(survivor.targets[0].provider_id(), "p2");
        assert!(!survivor.targets[0].enabled);
        assert_eq!(
            survivor
                .primary_target()
                .map(|target| target.provider_id().as_str()),
            Some("p2")
        );
    }

    #[tokio::test]
    async fn refresh_selection_only_returns_credentials_inside_the_expiry_window() {
        let store = MemoryOAuthCredentialStore {
            credentials: RwLock::new(std::collections::HashMap::new()),
        };
        for (provider_id, expires_at) in [
            (
                "expired",
                Some((chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339()),
            ),
            (
                "future",
                Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            ),
            ("non-expiring", None),
        ] {
            store
                .upsert(
                    provider_id,
                    UpsertOAuthCredential {
                        driver_key: "openai-codex".into(),
                        scheme: "oauth".into(),
                        access_token: format!("{provider_id}-token"),
                        refresh_token: Some(format!("{provider_id}-refresh")),
                        expires_at,
                        ..Default::default()
                    },
                )
                .await
                .expect("credential");
        }

        let selected = store
            .list_expiring(Duration::from_secs(300))
            .await
            .expect("expiring credentials");
        assert_eq!(
            selected
                .iter()
                .map(|credential| credential.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["expired"]
        );
    }

    #[tokio::test]
    async fn cancelled_refresh_lease_is_retryable_and_cas_safe() {
        let store = MemoryOAuthCredentialStore {
            credentials: RwLock::new(std::collections::HashMap::new()),
        };
        let original = UpsertOAuthCredential {
            driver_key: "openai-codex".into(),
            scheme: "oauth".into(),
            access_token: "expired".into(),
            refresh_token: Some("refresh".into()),
            ..Default::default()
        };
        let credential = store
            .upsert("provider", original)
            .await
            .expect("credential");
        let first_lease = store
            .try_begin_refresh("provider", credential.status_version)
            .await
            .expect("begin refresh")
            .expect("refresh lease");

        assert!(
            store
                .cancel_refresh("provider", first_lease.status_version)
                .await
                .expect("cancel refresh")
        );
        assert!(
            !store
                .cancel_refresh("provider", first_lease.status_version)
                .await
                .expect("stale cancel")
        );

        let retryable = store
            .get("provider")
            .await
            .expect("read credential")
            .expect("credential remains");
        assert_eq!(retryable.status, "connected");
        assert!(
            store
                .try_begin_refresh("provider", retryable.status_version)
                .await
                .expect("retry refresh")
                .is_some()
        );
    }
    #[tokio::test]
    async fn refresh_preserves_connection_id_but_upsert_starts_new_generation() {
        let store = MemoryOAuthCredentialStore {
            credentials: RwLock::new(std::collections::HashMap::new()),
        };
        let credential = store
            .upsert(
                "provider",
                UpsertOAuthCredential {
                    driver_key: "openai-codex".into(),
                    scheme: "oauth".into(),
                    access_token: "old".into(),
                    refresh_token: Some("refresh".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("initial credential");
        let connection_id = credential.connection_id.clone();

        let lease = store
            .try_begin_refresh("provider", credential.status_version)
            .await
            .expect("begin refresh")
            .expect("refresh lease");
        let refreshed = store
            .complete_refresh(
                "provider",
                lease.status_version,
                UpsertOAuthCredential {
                    driver_key: "openai-codex".into(),
                    scheme: "oauth".into(),
                    access_token: "new".into(),
                    refresh_token: Some("refresh-2".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("complete refresh");
        assert_eq!(refreshed.connection_id, connection_id);

        let reconnected = store
            .upsert(
                "provider",
                UpsertOAuthCredential {
                    driver_key: "openai-codex".into(),
                    scheme: "oauth".into(),
                    access_token: "reconnected".into(),
                    refresh_token: Some("refresh-3".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("reconnect credential");
        assert_ne!(reconnected.connection_id, connection_id);
    }

    fn new_provider_model_record(model_id: &str) -> NewProviderModelRecord {
        NewProviderModelRecord {
            provider_id: "provider".into(),
            model_id: model_id.into(),
            source_kind: ProviderModelSourceKind::Discovered,
            snapshot_state: SnapshotState::Unregistered,
            metadata_source_provider_id: None,
            presence: ProviderModelPresence::Present,
            selection_policy: ProviderModelSelectionPolicy::Auto,
            metadata: crate::provider_models::ProviderModelMetadata::bare(model_id),
        }
    }

    #[tokio::test]
    async fn stale_reconciliation_rejects_every_update_atomically() {
        let storage = MemoryStorage::new(vec![], vec![], vec![]);
        let store = storage.provider_models();
        for model_id in ["first", "second"] {
            store
                .create(new_provider_model_record(model_id))
                .await
                .unwrap();
        }
        let second = store.get("provider", "second").await.unwrap().unwrap();
        let changed = store
            .update_metadata(
                "provider",
                "second",
                crate::provider_models::ProviderModelMetadata {
                    name: Some("Manual correction".into()),
                    ..second.metadata
                },
                SnapshotState::Edited { source: None },
                second.revision,
            )
            .await
            .unwrap();
        assert!(matches!(changed, ProviderModelMutation::Applied(_)));

        let updates = ["first", "second"]
            .into_iter()
            .map(
                |model_id| crate::provider_models::ProviderModelPresenceUpdate {
                    model_id: model_id.to_owned(),
                    expected_revision: 1,
                    snapshot_state: Some(SnapshotState::Imported {
                        source: crate::provider_models::SourceStamp::Discovery,
                    }),
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Missing,
                    lifecycle_status: None,
                    metadata: Some(crate::provider_models::ProviderModelMetadata::bare(
                        model_id,
                    )),
                },
            )
            .collect();
        assert!(
            store
                .apply_reconciliation(
                    "provider",
                    ProviderModelReconciliation {
                        inserts: vec![],
                        updates
                    }
                )
                .await
                .is_err()
        );
        let first = store.get("provider", "first").await.unwrap().unwrap();
        let second = store.get("provider", "second").await.unwrap().unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(first.presence, ProviderModelPresence::Present);
        assert_eq!(first.snapshot_state, SnapshotState::Unregistered);
        assert_eq!(second.metadata.name.as_deref(), Some("Manual correction"));
        assert_eq!(
            second.snapshot_state,
            SnapshotState::Edited { source: None }
        );
    }

    #[tokio::test]
    async fn provider_model_find_matches_inventory_by_segment_and_case() {
        let storage = MemoryStorage::new(vec![], vec![], vec![]);
        let store = storage.provider_models();
        for model_id in ["zhipuai/GLM-4.6", "gpt-4o-mini"] {
            store
                .create(new_provider_model_record(model_id))
                .await
                .expect("insert fixture model");
        }

        // 精确命中优先
        assert_eq!(
            store
                .find("provider", "zhipuai/GLM-4.6")
                .await
                .expect("exact find")
                .expect("exact hit")
                .model_id,
            "zhipuai/GLM-4.6"
        );
        // 最右段 + 忽略大小写：命名空间与大小写差异都能命中
        for query in ["GLM-4.6", "glm-4.6", "vendor/GLM-4.6", "GPT-4O-MINI"] {
            assert!(
                store.find("provider", query).await.expect("find").is_some(),
                "query `{query}` should match the inventory"
            );
        }
        // 无法归一到任何清单 ID
        assert!(
            store
                .find("provider", "glm-4.5")
                .await
                .expect("find")
                .is_none()
        );
    }

    #[tokio::test]
    async fn provider_model_find_rejects_ambiguous_segment_matches() {
        let storage = MemoryStorage::new(vec![], vec![], vec![]);
        let store = storage.provider_models();
        for model_id in ["openai/gpt-4o", "azure/gpt-4o"] {
            store
                .create(new_provider_model_record(model_id))
                .await
                .expect("insert fixture model");
        }

        // 多个清单 ID 归一到同一匹配键时视为歧义，保持 None 让错误可见
        assert!(
            store
                .find("provider", "gpt-4o")
                .await
                .expect("find")
                .is_none()
        );
        // 但带命名空间的精确匹配仍然命中
        assert_eq!(
            store
                .find("provider", "openai/gpt-4o")
                .await
                .expect("find")
                .expect("namespaced hit")
                .model_id,
            "openai/gpt-4o"
        );
    }

    // ADR-0073：凭据失效是条件写——凭据代际（providers.revision +
    // OAuth status_version）不一致时放弃标记，避免旧凭据的 401
    // 误杀已更换的新凭据。
    #[tokio::test]
    async fn credential_invalid_marking_is_conditional_on_credential_generation() {
        let storage = MemoryStorage::new(vec![provider("p1")], vec![], vec![]);
        let store = storage.providers();

        let stale = ProviderCredentialVersion {
            provider_revision: 99,
            oauth_status_version: None,
        };
        assert!(
            !store
                .mark_credential_invalid("p1", stale)
                .await
                .expect("mark")
        );
        assert!(
            !store
                .get("p1")
                .await
                .expect("get")
                .expect("provider")
                .credential_invalid()
        );

        let current = ProviderCredentialVersion {
            provider_revision: 0,
            oauth_status_version: None,
        };
        assert!(
            store
                .mark_credential_invalid("p1", current)
                .await
                .expect("mark")
        );
        let marked = store.get("p1").await.expect("get").expect("provider");
        assert!(marked.credential_invalid());
        assert!(marked.credential_invalid_at.is_some());
        assert_eq!(marked.revision, 1);

        // 已失效后再标记：代际已前进，旧证据不再重复写入
        assert!(
            !store
                .mark_credential_invalid("p1", current)
                .await
                .expect("mark")
        );

        assert!(
            store
                .credential_invalid_provider_ids()
                .await
                .expect("ids")
                .contains("p1")
        );
    }

    #[tokio::test]
    async fn credential_invalid_marking_requires_matching_oauth_version() {
        let storage = MemoryStorage::new(vec![provider("p1")], vec![], vec![]);
        storage
            .oauth_credentials()
            .upsert(
                "p1",
                crate::db::models::UpsertOAuthCredential {
                    driver_key: "driver".into(),
                    scheme: "authorization_code".into(),
                    access_token: "token".into(),
                    refresh_token: None,
                    expires_at: None,
                    resource_url: None,
                    subject_id: None,
                    scopes: None,
                    meta: None,
                },
            )
            .await
            .expect("upsert oauth");
        let store = storage.providers();

        // 有 OAuth 行但请求声称没有 → 拒绝标记
        assert!(
            !store
                .mark_credential_invalid(
                    "p1",
                    ProviderCredentialVersion {
                        provider_revision: 0,
                        oauth_status_version: None,
                    },
                )
                .await
                .expect("mark")
        );

        // OAuth 凭据已被刷新（status_version 前进）→ 旧证据不得落库
        let stored_version = storage
            .oauth_credentials()
            .get("p1")
            .await
            .expect("get oauth")
            .expect("oauth")
            .status_version;
        assert!(
            !store
                .mark_credential_invalid(
                    "p1",
                    ProviderCredentialVersion {
                        provider_revision: 0,
                        oauth_status_version: Some(stored_version + 1),
                    },
                )
                .await
                .expect("mark")
        );

        assert!(
            store
                .mark_credential_invalid(
                    "p1",
                    ProviderCredentialVersion {
                        provider_revision: 0,
                        oauth_status_version: Some(stored_version),
                    },
                )
                .await
                .expect("mark")
        );
        assert!(
            store
                .get("p1")
                .await
                .expect("get")
                .expect("provider")
                .credential_invalid()
        );
    }

    #[tokio::test]
    async fn credential_invalid_clears_only_on_credential_evidence() {
        let storage = MemoryStorage::new(vec![provider("p1")], vec![], vec![]);
        let store = storage.providers();
        let version = ProviderCredentialVersion {
            provider_revision: 0,
            oauth_status_version: None,
        };
        assert!(
            store
                .mark_credential_invalid("p1", version)
                .await
                .expect("mark")
        );

        // 仅意图字段（is_enabled）不清除失效
        let updated = store
            .update(
                "p1",
                UpdateProvider {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert!(updated.credential_invalid());
        assert!(updated.credential_invalid_at.is_some());

        // 全字段回填但调用方声明保留 → 仍不清除
        let updated = store
            .update(
                "p1",
                UpdateProvider {
                    api_key: Some("same".into()),
                    preserve_credential_status: true,
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert!(updated.credential_invalid());

        // 凭据字段变更 → 新证据，恢复 ok
        let updated = store
            .update(
                "p1",
                UpdateProvider {
                    api_key: Some("new-key".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert!(!updated.credential_invalid());
        assert!(updated.credential_invalid_at.is_none());
    }

    #[tokio::test]
    async fn successful_test_result_clears_credential_invalid() {
        let storage = MemoryStorage::new(vec![provider("p1")], vec![], vec![]);
        let store = storage.providers();
        assert!(
            store
                .mark_credential_invalid(
                    "p1",
                    ProviderCredentialVersion {
                        provider_revision: 0,
                        oauth_status_version: None,
                    },
                )
                .await
                .expect("mark")
        );

        store
            .record_test_result(
                "p1",
                ProviderTestResult {
                    success: false,
                    tested_at: now_rfc3339(),
                },
            )
            .await
            .expect("record failure");
        assert!(
            store
                .get("p1")
                .await
                .expect("get")
                .expect("provider")
                .credential_invalid()
        );

        store
            .record_test_result(
                "p1",
                ProviderTestResult {
                    success: true,
                    tested_at: now_rfc3339(),
                },
            )
            .await
            .expect("record success");
        let provider = store.get("p1").await.expect("get").expect("provider");
        assert!(!provider.credential_invalid());
        assert!(provider.credential_invalid_at.is_none());
    }
}
