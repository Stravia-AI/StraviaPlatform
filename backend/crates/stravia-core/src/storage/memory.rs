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
use crate::pricing::{CostBasis, CustomCurrency};
use crate::provider_models::{
    ModelChangePage, ModelChangeQuery, ModelChangeRecord, NewProviderModelRecord,
    ProviderModelMutation, ProviderModelReconciliation, ProviderModelRecord, ProviderModelRestore,
    ProviderModelSelectionPolicy, ProviderModelSourceKind, ProviderModelSyncState,
    ProviderModelSyncSummary, RestoreProviderModel, SnapshotState,
};

use super::traits::{
    ApiKeySpend, ApiKeyStore, AuthAccessStore, MoneyStore, NewProviderCostPlan,
    OAuthCredentialStore, ProviderConnectionCheckWrite, ProviderCostPlan, ProviderModelStore,
    ProviderStore, RouteStore, SettingsStore, Storage, StorageBackend, StorageBootstrap,
    StorageHealth, UsageStatsStore,
};
use crate::storage::money;

use std::sync::Arc;

#[derive(Clone)]
pub struct MemoryStorage {
    providers: Arc<RwLock<Vec<Provider>>>,
    models: Arc<RwLock<Vec<RouteConfig>>>,
    settings: Arc<RwLock<Vec<(String, String)>>>,
    provider_models: Arc<RwLock<Vec<ProviderModelRecord>>>,
    model_sync_states: Arc<RwLock<std::collections::HashMap<String, ProviderModelSyncState>>>,
    model_changes: Arc<RwLock<Vec<ModelChangeRecord>>>,
    oauth_credentials: Arc<MemoryOAuthCredentialStore>,
    allowance_guards:
        Arc<RwLock<std::collections::HashMap<String, std::collections::BTreeMap<String, String>>>>,
    allowance_suspensions:
        Arc<RwLock<std::collections::HashMap<String, crate::db::models::AllowanceSuspensionState>>>,
    provider_cost_plans: Arc<RwLock<Vec<ProviderCostPlan>>>,
    api_key_spends: Arc<RwLock<std::collections::HashMap<String, ApiKeySpend>>>,
    plugin_store: PluginStore,
}

impl MemoryStorage {
    pub fn new(
        providers: Vec<Provider>,
        models: Vec<RouteConfig>,
        settings: Vec<(String, String)>,
    ) -> Self {
        let model_sync_states = providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    ProviderModelSyncState {
                        has_successful_sync: true,
                        ..Default::default()
                    },
                )
            })
            .collect();
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
            model_sync_states: Arc::new(RwLock::new(model_sync_states)),
            model_changes: Arc::new(RwLock::new(Vec::new())),
            oauth_credentials,
            allowance_guards: Arc::new(RwLock::new(std::collections::HashMap::new())),
            allowance_suspensions: Arc::new(RwLock::new(std::collections::HashMap::new())),
            provider_cost_plans: Arc::new(RwLock::new(Vec::new())),
            api_key_spends: Arc::new(RwLock::new(std::collections::HashMap::new())),
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
    fn money(&self) -> &dyn MoneyStore {
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
        let mut providers = self.providers.read().await.clone();
        for provider in &mut providers {
            self.apply_cost_basis(provider).await;
        }
        Ok(providers)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        let mut provider = self
            .providers
            .read()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned();
        if let Some(provider) = provider.as_mut() {
            self.apply_cost_basis(provider).await;
        }
        Ok(provider)
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
            cost_basis: CostBasis::Usage,
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
        {
            let mut plans = self.provider_cost_plans.write().await;
            let latest = plans
                .iter()
                .filter(|plan| plan.provider_id == id)
                .max_by(|left, right| left.effective_from.cmp(&right.effective_from))
                .map(|plan| plan.basis);
            if latest == Some(CostBasis::Subscription) {
                let previous = plans
                    .iter()
                    .filter(|plan| plan.provider_id == id)
                    .map(|plan| plan.effective_from)
                    .max()
                    .unwrap_or(0);
                let effective_from = chrono::Utc::now().timestamp_millis().max(
                    previous
                        .checked_add(1)
                        .context("cost plan timestamp overflow")?,
                );
                plans.push(ProviderCostPlan {
                    id: stravia_runtime_contract::identifier::new_id(),
                    provider_id: id.to_string(),
                    effective_from,
                    basis: CostBasis::Usage,
                    period: None,
                    price: None,
                    price_currency: None,
                    created_at: now_rfc3339(),
                });
            }
        }
        providers.retain(|provider| provider.id != id);
        self.model_sync_states.write().await.remove(id);
        provider_models.retain(|model| model.provider_id != id);
        oauth_credentials.remove(id);
        self.allowance_guards.write().await.remove(id);
        self.allowance_suspensions.write().await.remove(id);
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
        expected: ProviderCredentialVersion,
        result: ProviderConnectionCheckWrite,
    ) -> anyhow::Result<bool> {
        let mut providers = self.providers.write().await;
        let Some(index) = providers
            .iter()
            .position(|provider| provider.id == provider_id)
        else {
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
            .get(provider_id)
            .map(|credential| credential.status_version);
        if oauth_status_version != expected.oauth_status_version {
            return Ok(false);
        }
        let provider = &mut providers[index];
        provider.last_test_success = Some(result.success);
        provider.last_test_at = Some(now_rfc3339());
        // ADR-0073：只有本次检查实际验证了凭据，成功才构成恢复证据。
        if result.success && result.credentials_verified {
            provider.credential_status = "ok".to_string();
            provider.credential_invalid_at = None;
        }
        provider.revision += 1;
        provider.updated_at = now_rfc3339();
        Ok(true)
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

    async fn guarded_allowance_keys(&self, id: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .allowance_guards
            .read()
            .await
            .get(id)
            .map(|keys| keys.keys().cloned().collect())
            .unwrap_or_default())
    }

    async fn replace_guarded_allowance_keys(
        &self,
        id: &str,
        keys: &[String],
    ) -> anyhow::Result<()> {
        let providers = self.providers.read().await;
        anyhow::ensure!(
            providers.iter().any(|provider| provider.id == id),
            "provider not found"
        );
        let mut guards = self.allowance_guards.write().await;
        let mut states = self.allowance_suspensions.write().await;
        let mut keys = keys.to_vec();
        keys.sort();
        keys.dedup();
        if let Some(state) = states.get_mut(id) {
            let previous_trigger_count = state.triggered_keys.len();
            state.triggered_keys.retain(|key| keys.contains(key));
            if state.triggered_keys.len() != previous_trigger_count {
                state.earliest_reset_at = None;
            }
            if state.triggered_keys.is_empty() {
                state.suspended = false;
                state.suspended_at = None;
                state.earliest_reset_at = None;
            }
        }
        let records = guards.entry(id.to_owned()).or_default();
        records.retain(|key, _| keys.contains(key));
        for key in keys {
            records.entry(key).or_insert_with(now_rfc3339);
        }
        Ok(())
    }

    async fn write_allowance_suspension(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
        state: &crate::db::models::AllowanceSuspensionState,
        expected_guarded_keys: &[String],
    ) -> anyhow::Result<bool> {
        let providers = self.providers.read().await;
        if !providers
            .iter()
            .any(|provider| provider.id == id && provider.revision == expected.provider_revision)
        {
            return Ok(false);
        }
        let oauth = self.oauth_credentials.credentials.read().await;
        if oauth.get(id).map(|credential| credential.status_version)
            != expected.oauth_status_version
        {
            return Ok(false);
        }
        let guards = self.allowance_guards.read().await;
        let actual = guards.get(id);
        if actual.is_some_and(|keys| keys.keys().any(|key| !expected_guarded_keys.contains(key)))
            || expected_guarded_keys
                .iter()
                .any(|key| !actual.is_some_and(|keys| keys.contains_key(key)))
        {
            return Ok(false);
        }
        let mut states = self.allowance_suspensions.write().await;
        let previous = states.get(id);
        if previous
            .is_some_and(|previous| previous.evidence_completed_at >= state.evidence_completed_at)
        {
            return Ok(false);
        }
        let mut state = state.clone();
        if state.suspended
            && let Some(previous) = previous.filter(|previous| previous.suspended)
        {
            state.suspended_at = previous.suspended_at.clone();
        }
        states.insert(id.to_owned(), state);
        Ok(true)
    }

    async fn allowance_suspension(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<crate::db::models::AllowanceSuspensionState>> {
        Ok(self.allowance_suspensions.read().await.get(id).cloned())
    }

    async fn allowance_suspended_provider_ids(
        &self,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        Ok(self
            .allowance_suspensions
            .read()
            .await
            .iter()
            .filter(|(_, state)| state.suspended)
            .map(|(id, _)| id.clone())
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
        let billing = input.billing.clone().unwrap_or_else(|| {
            routes
                .iter()
                .find(|route| route.id == storage_id)
                .and_then(|route| route.billing.clone())
        });
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
            route.output_modality = input.output_modality;
            route.image_generation_bound = false;
            route.balance = input.selection_strategy;
            route.is_enabled = input.is_enabled;
            route.default_thinking_level = input
                .default_thinking_level
                .map(|level| level.as_str().to_owned());
            route.billing = billing;
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
                    target.model.trim().into(),
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
                    model_name: None,
                    output_modality_mismatch: false,
                    route_output_modalities: Vec::new(),
                }
            })
            .collect::<Vec<_>>();
        let mut route = RouteConfig {
            id: storage_id.clone().into(),
            model_id: input.model_id,
            display_name: input.display_name,
            output_modality: input.output_modality,
            image_generation_bound: false,
            balance: input.selection_strategy,
            is_enabled: input.is_enabled,
            created_at,
            default_thinking_level: input
                .default_thinking_level
                .map(|level| level.as_str().to_string()),
            billing,
            supported_thinking_levels: Vec::new(),
            context_window: None,
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

    async fn stats_overview(
        &self,
        window: crate::db::models::StatsWindow,
    ) -> anyhow::Result<StatsOverview> {
        window.validate()?;
        Ok(StatsOverview::default())
    }

    async fn stats_series(
        &self,
        window: crate::db::models::StatsWindow,
        bucket_ms: i64,
    ) -> anyhow::Result<Vec<StatsSeries>> {
        window.validate()?;
        if bucket_ms < 900_000 || bucket_ms % 900_000 != 0 {
            return Err(crate::db::models::StatsQueryError(
                "bucket must be a positive multiple of 900 seconds",
            )
            .into());
        }
        Ok(vec![])
    }

    async fn stats_by_model(
        &self,
        window: crate::db::models::StatsWindow,
    ) -> anyhow::Result<Vec<ModelStats>> {
        window.validate()?;
        Ok(vec![])
    }

    async fn stats_by_provider(
        &self,
        window: crate::db::models::StatsWindow,
    ) -> anyhow::Result<Vec<ProviderStats>> {
        window.validate()?;
        Ok(vec![])
    }

    async fn stats_by_api_key(
        &self,
        window: crate::db::models::StatsWindow,
    ) -> anyhow::Result<Vec<ApiKeyStats>> {
        window.validate()?;
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
        validate_map: &crate::provider_models::ProviderModelThinkingMapValidator<'_>,
    ) -> anyhow::Result<()> {
        let refresh_maps = reconciliation
            .updates
            .iter()
            .any(|update| update.generated_thinking_level_map.is_some());
        let now = now_rfc3339();
        let providers = self.providers.read().await;
        if let Some(expected) = &reconciliation.expected_provider {
            anyhow::ensure!(
                providers.iter().any(|provider| provider.id == provider_id
                    && crate::provider_models::same_discovery_provider(provider, expected)),
                "Provider changed while synchronizing discovered models"
            );
        }
        // 锁次序与 restore、Provider 删除一致：routes → provider_models。
        let mut routes = if refresh_maps {
            Some(self.models.write().await)
        } else {
            None
        };
        let mut items = self.provider_models.write().await;
        let mut states = self.model_sync_states.write().await;
        let mut changes = self.model_changes.write().await;
        for (model_id, revision) in &reconciliation.expected_revisions {
            anyhow::ensure!(
                items.iter().any(|item| item.provider_id == provider_id
                    && &item.model_id == model_id
                    && &item.revision == revision),
                "Provider Model has changed while synchronizing discovered models"
            );
        }
        if let Some(record) = &reconciliation.change_record {
            anyhow::ensure!(
                !changes.iter().any(|item| item.id == record.id),
                "duplicate model change record"
            );
        }
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
                    || update.source_metadata.is_some()
                    || update.snapshot_state.is_some()
                    || item.metadata_source_provider_id != update.metadata_source_provider_id;
                if changed {
                    item.presence = update.presence;
                    item.metadata_source_provider_id = update.metadata_source_provider_id;
                    if let Some(snapshot_state) = update.snapshot_state {
                        item.snapshot_state = snapshot_state;
                    }
                    if let Some(source_metadata) = update.source_metadata {
                        item.source_metadata = Some(source_metadata);
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
                if let Some(generated) = update.generated_thinking_level_map.as_deref() {
                    let routes = routes
                        .as_mut()
                        .context("Provider Model thinking map update has no Route lock")?;
                    for route in routes.iter_mut() {
                        let mut route_changed = false;
                        for target in route.targets.iter_mut() {
                            if target.provider_id().as_str() != provider_id
                                || target.model().as_str() != update.model_id
                            {
                                continue;
                            }
                            let mut map = target.thinking_level_map.clone();
                            let changed = crate::thinking::refresh_generated_thinking_level_map(
                                &mut map, generated,
                            )?;
                            validate_map(&update.model_id, &item.metadata, &map)?;
                            if changed {
                                target.thinking_level_map = map;
                                route_changed = true;
                            }
                        }
                        if route_changed {
                            route.refresh_supported_thinking_levels();
                        }
                    }
                }
            }
        }
        for input in reconciliation.inserts {
            items.push(memory_provider_model(input, now.clone()));
        }
        if let Some(summary) = reconciliation.sync_summary {
            let state = states.entry(provider_id.to_owned()).or_default();
            state.last_check = Some(summary);
            state.has_successful_sync = true;
        }
        if let Some(record) = reconciliation.change_record {
            changes.push(record);
        }
        Ok(())
    }

    async fn create(
        &self,
        input: NewProviderModelRecord,
        change_record: Option<ModelChangeRecord>,
    ) -> anyhow::Result<ProviderModelMutation> {
        let _providers = self.providers.read().await;
        let mut items = self.provider_models.write().await;
        if items
            .iter()
            .any(|item| item.provider_id == input.provider_id && item.model_id == input.model_id)
        {
            return Ok(ProviderModelMutation::Conflict);
        }
        let mut changes = self.model_changes.write().await;
        if let Some(change) = &change_record {
            anyhow::ensure!(
                !changes.iter().any(|item| item.id == change.id),
                "duplicate model change record"
            );
        }
        let record = memory_provider_model(input, now_rfc3339());
        items.push(record.clone());
        if let Some(change) = change_record {
            changes.push(change);
        }
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

    async fn restore(
        &self,
        provider_id: &str,
        model_id: &str,
        input: RestoreProviderModel,
        validate_map: &crate::provider_models::ProviderModelThinkingMapValidator<'_>,
        before_commit: &(dyn Fn() -> anyhow::Result<()> + Send + Sync),
    ) -> anyhow::Result<ProviderModelRestore> {
        // 锁次序与既有路径一致：Provider 删除与插件内存后端都按
        // providers → routes → provider_models → oauth → plugin data 加锁；
        // settings 只被本路径与 SettingsStore 单独使用，永远最后获取。
        let mut routes = self.models.write().await;
        let mut provider_models = self.provider_models.write().await;
        let mut settings = self.settings.write().await;

        let Some(index) = provider_models
            .iter()
            .position(|item| item.provider_id == provider_id && item.model_id == model_id)
        else {
            return Ok(ProviderModelRestore::NotFound);
        };
        if provider_models[index].revision != input.expected_revision {
            return Ok(ProviderModelRestore::Conflict);
        }

        // 全部变更先在克隆上准备好；任何失败都不留下半提交状态。
        let mut model = provider_models[index].clone();
        model.metadata = input.metadata;
        model.snapshot_state = input.snapshot_state;
        // metadata_source_provider_id 与来源规格保持同步路径写入的值。
        model.cost_rules = model.metadata.cost_rules();
        model.revision = model
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("provider model revision overflow"))?;
        model.updated_at = now_rfc3339();

        // 匹配目标与读取侧一致：Provider + 上游模型 ID 精确相等；禁用
        // Target/Route 同样刷新。仅替换仍属 Generated 的行，Overridden
        // 与无关字段保留；未变化的 map 不进入提交集。
        let mut prepared = Vec::new();
        if let Some(generated) = input.generated_thinking_level_map.as_deref() {
            for (route_index, route) in routes.iter().enumerate() {
                for (target_index, target) in route.targets.iter().enumerate() {
                    if target.provider_id().as_str() != provider_id
                        || target.model().as_str() != model_id
                    {
                        continue;
                    }
                    let mut map = target.thinking_level_map.clone();
                    let changed =
                        crate::thinking::refresh_generated_thinking_level_map(&mut map, generated)?;
                    validate_map(model_id, &model.metadata, &map)?;
                    if changed {
                        prepared.push((route_index, target_index, map));
                    }
                }
            }
        }

        let current_epoch: i64 = settings
            .iter()
            .find(|(key, _)| key == super::CONFIG_EPOCH_KEY)
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let next_epoch = current_epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("config epoch overflow"))?
            .to_string();

        before_commit()?;

        // 自此无 await、无失败点：一次性写回快照、Target map 与配置通知。
        provider_models[index] = model.clone();
        let mut previous_route: Option<usize> = None;
        for (route_index, target_index, map) in prepared {
            if let Some(previous) = previous_route
                && previous != route_index
            {
                routes[previous].refresh_supported_thinking_levels();
            }
            routes[route_index].targets[target_index].thinking_level_map = map;
            previous_route = Some(route_index);
        }
        if let Some(route_index) = previous_route {
            routes[route_index].refresh_supported_thinking_levels();
        }
        if let Some(entry) = settings
            .iter_mut()
            .find(|(key, _)| key == super::CONFIG_EPOCH_KEY)
        {
            entry.1 = next_epoch;
        } else {
            settings.push((super::CONFIG_EPOCH_KEY.to_string(), next_epoch));
        }

        let active_routes = routes
            .iter()
            .filter(|route| route.is_enabled)
            .cloned()
            .collect();
        Ok(ProviderModelRestore::Applied {
            model: Box::new(model),
            active_routes,
        })
    }

    async fn update_selection_policy(
        &self,
        provider_id: &str,
        model_id: &str,
        policy: ProviderModelSelectionPolicy,
        expected_revision: i64,
        change_record: Option<ModelChangeRecord>,
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
        if item.selection_policy == policy {
            return Ok(ProviderModelMutation::Applied(Box::new(item.clone())));
        }
        let mut changes = self.model_changes.write().await;
        if let Some(change) = &change_record {
            anyhow::ensure!(
                !changes.iter().any(|item| item.id == change.id),
                "duplicate model change record"
            );
        }
        item.selection_policy = policy;
        item.revision += 1;
        item.updated_at = now_rfc3339();
        if let Some(change) = change_record {
            changes.push(change);
        }
        Ok(ProviderModelMutation::Applied(Box::new(item.clone())))
    }

    async fn sync_state(&self, provider_id: &str) -> anyhow::Result<ProviderModelSyncState> {
        Ok(self
            .model_sync_states
            .read()
            .await
            .get(provider_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn set_automatic_sync(
        &self,
        provider_id: &str,
        enabled: bool,
    ) -> anyhow::Result<ProviderModelSyncState> {
        let providers = self.providers.read().await;
        anyhow::ensure!(
            providers.iter().any(|provider| provider.id == provider_id),
            "Provider not found"
        );
        let mut states = self.model_sync_states.write().await;
        let state = states.entry(provider_id.to_owned()).or_default();
        state.automatic_sync = enabled;
        Ok(state.clone())
    }

    async fn save_sync_failure(
        &self,
        provider_id: &str,
        summary: ProviderModelSyncSummary,
        record: ModelChangeRecord,
    ) -> anyhow::Result<()> {
        let providers = self.providers.read().await;
        let mut states = self.model_sync_states.write().await;
        let mut changes = self.model_changes.write().await;
        if providers.iter().any(|provider| provider.id == provider_id) {
            states.entry(provider_id.to_owned()).or_default().last_check = Some(summary);
        }
        if let Some(existing) = changes.iter_mut().find(|item| item.id == record.id) {
            existing.outcome = record.outcome;
            existing.error = record.error;
        } else {
            changes.push(record);
        }
        Ok(())
    }

    async fn list_changes(&self, query: ModelChangeQuery) -> anyhow::Result<ModelChangePage> {
        let cursor = super::model_changes::cursor(&query)?;
        let cutoff = super::model_changes::cutoff();
        let limit = query.limit.unwrap_or(25).clamp(1, 100);
        let needle = query.q.as_ref().map(|q| q.to_lowercase());
        let providers = self.providers.read().await;
        let changes = self.model_changes.read().await;
        let mut records: Vec<_> = changes
            .iter()
            .filter(|record| {
                record.checked_at >= cutoff
                    && cursor
                        .as_ref()
                        .is_none_or(|(time, id)| (&record.checked_at, &record.id) < (time, id))
                    && super::model_changes::matches(record, &query, needle.as_deref())
            })
            .collect();
        records.sort_by(|a, b| (&b.checked_at, &b.id).cmp(&(&a.checked_at, &a.id)));
        records.truncate(limit + 1);
        let mut records: Vec<_> = records.into_iter().cloned().collect();
        for record in &mut records {
            record.provider_deleted = !providers
                .iter()
                .any(|provider| provider.id == record.provider_id);
        }
        super::model_changes::page(records, limit)
    }

    async fn get_change(&self, id: &str) -> anyhow::Result<Option<ModelChangeRecord>> {
        let providers = self.providers.read().await;
        let mut record = self
            .model_changes
            .read()
            .await
            .iter()
            .find(|record| record.id == id && record.checked_at >= super::model_changes::cutoff())
            .cloned();
        if let Some(record) = &mut record {
            record.provider_deleted = !providers
                .iter()
                .any(|provider| provider.id == record.provider_id);
        }
        Ok(record)
    }

    async fn prune_changes(&self, before: &str) -> anyhow::Result<()> {
        self.model_changes
            .write()
            .await
            .retain(|record| record.checked_at.as_str() >= before);
        Ok(())
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
        source_metadata: input.source_metadata,
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
        if !matches!(cred.status.as_str(), "connected" | "error")
            || cred.status_version != expected_version
        {
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
                matches!(credential.status.as_str(), "connected" | "error")
                    && credential
                        .expires_at
                        .as_deref()
                        .and_then(parse_datetime_utc)
                        .is_some_and(|expires_at| expires_at <= cutoff)
            })
            .cloned()
            .collect())
    }

    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64> {
        let cutoff = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::from_std(timeout)?)
            .context("refresh lease timeout is outside the supported timestamp range")?;
        let mut map = self.credentials.write().await;
        let mut count = 0u64;
        for cred in map.values_mut() {
            if cred.status == "refreshing" {
                let updated_at = parse_datetime_utc(&cred.updated_at)
                    .context("refresh lease has an invalid updated_at timestamp")?;
                if updated_at >= cutoff {
                    continue;
                }
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

#[async_trait]
impl MoneyStore for MemoryStorage {
    async fn custom_currency(&self) -> anyhow::Result<Option<CustomCurrency>> {
        SettingsStore::get(self, money::CUSTOM_CURRENCY_SETTING_KEY)
            .await?
            .map(|value| money::parse_custom_currency(&value))
            .transpose()
    }

    async fn save_custom_currency(
        &self,
        symbol: &str,
        usd_rate: rust_decimal::Decimal,
    ) -> anyhow::Result<CustomCurrency> {
        let currency = match self.custom_currency().await? {
            Some(mut currency) => {
                currency.symbol = symbol.trim().to_string();
                currency.usd_rate = usd_rate;
                currency.validate()?;
                currency
            }
            None => {
                let currency = CustomCurrency {
                    symbol: symbol.trim().to_string(),
                    usd_rate,
                    first_rate: usd_rate,
                    configured_at: now_rfc3339(),
                };
                currency.validate()?;
                currency
            }
        };
        self.set(
            money::CUSTOM_CURRENCY_SETTING_KEY,
            &money::encode_custom_currency(&currency)?,
        )
        .await?;
        Ok(currency)
    }

    async fn provider_cost_plans(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderCostPlan>> {
        let mut plans = self
            .provider_cost_plans
            .read()
            .await
            .iter()
            .filter(|plan| plan.provider_id == provider_id)
            .cloned()
            .collect::<Vec<_>>();
        plans.sort_by(|left, right| {
            right
                .effective_from
                .cmp(&left.effective_from)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(plans)
    }

    async fn all_provider_cost_plans(&self) -> anyhow::Result<Vec<ProviderCostPlan>> {
        let mut plans = self.provider_cost_plans.read().await.clone();
        plans.sort_by(|left, right| {
            left.provider_id
                .cmp(&right.provider_id)
                .then_with(|| left.effective_from.cmp(&right.effective_from))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(plans)
    }

    async fn append_provider_cost_plan(
        &self,
        provider_id: &str,
        input: NewProviderCostPlan,
    ) -> anyhow::Result<ProviderCostPlan> {
        money::validate_new_cost_plan(&input)?;
        anyhow::ensure!(
            self.providers
                .read()
                .await
                .iter()
                .any(|provider| provider.id == provider_id),
            "provider not found: {provider_id}"
        );
        let custom = self.custom_currency().await?;
        let (period, price, currency) = match input.basis {
            CostBasis::Untracked | CostBasis::Usage => (None, None, None),
            CostBasis::Subscription => {
                let period = input.period.context("subscription requires a period")?;
                let amount = input.price.context("subscription requires a price")?;
                let currency = input
                    .currency
                    .context("subscription requires a price currency")?;
                let frozen = crate::pricing::freeze_amount(amount, currency, custom.as_ref())
                    .context("subscription price could not be frozen")?;
                (Some(period), Some(frozen), Some(currency))
            }
        };
        let mut plans = self.provider_cost_plans.write().await;
        let previous = plans
            .iter()
            .filter(|plan| plan.provider_id == provider_id)
            .map(|plan| plan.effective_from)
            .max()
            .unwrap_or(0);
        let effective_from = chrono::Utc::now().timestamp_millis().max(
            previous
                .checked_add(1)
                .context("cost plan timestamp overflow")?,
        );
        let plan = ProviderCostPlan {
            id: stravia_runtime_contract::identifier::new_id(),
            provider_id: provider_id.to_string(),
            effective_from,
            basis: input.basis,
            period,
            price,
            price_currency: currency,
            created_at: now_rfc3339(),
        };
        plans.push(plan.clone());
        Ok(plan)
    }

    async fn api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        Ok(self
            .api_key_spends
            .read()
            .await
            .get(api_key_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn reset_api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        let spend = ApiKeySpend {
            spent_custom_nanos: 0,
            spent_usd_unconverted_nanos: 0,
            reset_at: Some(now_rfc3339()),
        };
        self.api_key_spends
            .write()
            .await
            .insert(api_key_id.to_string(), spend.clone());
        Ok(spend)
    }
}

impl MemoryStorage {
    async fn apply_cost_basis(&self, provider: &mut Provider) {
        let plans = self.provider_cost_plans.read().await;
        provider.cost_basis = plans
            .iter()
            .filter(|plan| plan.provider_id == provider.id)
            .max_by(|left, right| {
                left.effective_from
                    .cmp(&right.effective_from)
                    .then_with(|| left.id.cmp(&right.id))
            })
            .map(|plan| plan.basis)
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_models::ProviderModelPresence;
    use crate::provider_models::SourceStamp;
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

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
            cost_basis: CostBasis::Usage,
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
            model: model.into(),
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
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: None,
                model_id: "CaseRoute".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![target("p1", "m1"), target("p2", "m2")]),
                default_thinking_level: None,
                billing: None,
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
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: Some(route.id.clone()),
                model_id: route.model_id.clone(),
                display_name: Some("Friendly".into()),
                selection_strategy: route.balance.clone(),
                is_enabled: route.is_enabled,
                targets: None,
                default_thinking_level: None,
                billing: None,
            })
            .await
            .expect("metadata-only update");
        assert_eq!(renamed.targets[0].id, route.targets[0].id);
        assert_eq!(renamed.targets[1].created_at, route.targets[1].created_at);
        let surviving_id = route.targets[1].id.clone();
        let surviving_created_at = route.targets[1].created_at.clone();
        let updated = storage
            .put(PutRoute {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: Some(route.id),
                model_id: "CaseRoute".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![target("p2", "m2")]),
                default_thinking_level: None,
                billing: None,
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
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: None,
                model_id: "empty-after-delete".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![target("p1", "m1")]),
                default_thinking_level: None,
                billing: None,
            })
            .await
            .expect("put disposable Route");
        storage
            .put(PutRoute {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
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
                billing: None,
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
    async fn oauth_refresh_recovery_storage_contract() -> anyhow::Result<()> {
        let storage = MemoryStorage::new(vec![provider("oauth-recovery")], vec![], vec![]);
        crate::storage::traits::verify_oauth_refresh_recovery_storage_contract(
            storage.oauth_credentials(),
            "oauth-recovery",
        )
        .await
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
            source_metadata: None,
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
                .create(new_provider_model_record(model_id), None)
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
                SnapshotState::Local,
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
                        pinned: std::collections::BTreeSet::new(),
                    }),
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Missing,
                    lifecycle_status: None,
                    metadata: Some(crate::provider_models::ProviderModelMetadata::bare(
                        model_id,
                    )),
                    source_metadata: None,
                    generated_thinking_level_map: None,
                },
            )
            .collect();
        assert!(
            store
                .apply_reconciliation(
                    "provider",
                    ProviderModelReconciliation {
                        inserts: vec![],
                        updates,
                        ..Default::default()
                    },
                    &|_, _, _| Ok(()),
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
        assert_eq!(second.snapshot_state, SnapshotState::Local);
    }

    #[tokio::test]
    async fn provider_model_find_matches_inventory_by_segment_and_case() {
        let storage = MemoryStorage::new(vec![], vec![], vec![]);
        let store = storage.provider_models();
        for model_id in ["zhipuai/GLM-4.6", "gpt-4o-mini"] {
            store
                .create(new_provider_model_record(model_id), None)
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
                .create(new_provider_model_record(model_id), None)
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
    async fn allowance_suspension_storage_contract() -> anyhow::Result<()> {
        let storage = MemoryStorage::new(vec![provider("allowance")], vec![], vec![]);
        let provider = storage.providers().get("allowance").await?.unwrap();
        crate::storage::traits::verify_allowance_storage_contract(
            storage.providers(),
            storage.oauth_credentials(),
            provider,
        )
        .await
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
    async fn successful_test_result_clears_credential_invalid_only_with_credentials_verified() {
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

        let invalid = store.get("p1").await.expect("get").expect("provider");
        assert!(
            store
                .record_test_result(
                    "p1",
                    ProviderCredentialVersion {
                        provider_revision: invalid.revision,
                        oauth_status_version: None,
                    },
                    ProviderConnectionCheckWrite {
                        success: false,
                        credentials_verified: false,
                    },
                )
                .await
                .expect("record failure")
        );
        let failed = store.get("p1").await.expect("get").expect("provider");
        assert!(failed.credential_invalid());
        assert_eq!(failed.last_test_success, Some(false));

        // Endpoint-only success records the probe but cannot clear credential evidence.
        assert!(
            store
                .record_test_result(
                    "p1",
                    ProviderCredentialVersion {
                        provider_revision: failed.revision,
                        oauth_status_version: None,
                    },
                    ProviderConnectionCheckWrite {
                        success: true,
                        credentials_verified: false,
                    },
                )
                .await
                .expect("record endpoint-only")
        );
        let endpoint_only = store.get("p1").await.expect("get").expect("provider");
        assert!(endpoint_only.credential_invalid());
        assert_eq!(endpoint_only.last_test_success, Some(true));

        assert!(
            store
                .record_test_result(
                    "p1",
                    ProviderCredentialVersion {
                        provider_revision: endpoint_only.revision,
                        oauth_status_version: None,
                    },
                    ProviderConnectionCheckWrite {
                        success: true,
                        credentials_verified: true,
                    },
                )
                .await
                .expect("record verified")
        );
        let recovered = store.get("p1").await.expect("get").expect("provider");
        assert!(!recovered.credential_invalid());
        assert!(recovered.credential_invalid_at.is_none());
    }

    // ── Provider Model restore：快照 + 关联 Target Generated Mapping +
    // config_epoch 必须同一提交，失败不得留下半写状态。

    fn discovered_provider_model(model_id: &str) -> NewProviderModelRecord {
        NewProviderModelRecord {
            provider_id: "provider".into(),
            model_id: model_id.into(),
            source_kind: ProviderModelSourceKind::Discovered,
            snapshot_state: SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog".into(),
                },
                pinned: std::collections::BTreeSet::new(),
            },
            source_metadata: Some(crate::provider_models::ProviderModelMetadata::bare(
                model_id,
            )),
            metadata_source_provider_id: Some("catalog".into()),
            presence: ProviderModelPresence::Present,
            selection_policy: ProviderModelSelectionPolicy::Auto,
            metadata: crate::provider_models::ProviderModelMetadata::bare(model_id),
        }
    }

    /// 生成行 + 一条 Overridden 人工行，代表 restore 前刚完成的手工 Mapping。
    fn seeded_thinking_map() -> Vec<crate::thinking::ThinkingLevelMapping> {
        let mut map = crate::thinking::generate_thinking_level_map(
            &crate::provider_models::ProviderModelMetadata::bare("m1"),
        );
        let low = map
            .iter_mut()
            .find(|row| row.level == ThinkingLevel::Low)
            .expect("generated map covers Low");
        low.control = TargetThinkingControl::Effort {
            value: "manual".into(),
        };
        low.source = crate::thinking::ThinkingMappingSource::Overridden;
        map
    }

    fn restore_metadata() -> crate::provider_models::ProviderModelMetadata {
        crate::provider_models::ProviderModelMetadata {
            reasoning_efforts: Some(vec!["minimal".into(), "high".into()]),
            cost: Some(crate::provider_models::ModelCost {
                context_over_200k: Some(crate::provider_models::PriceComponents {
                    input: Some(rust_decimal::Decimal::new(1, 6)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..crate::provider_models::ProviderModelMetadata::bare("m1")
        }
    }

    fn restore_input(
        metadata: crate::provider_models::ProviderModelMetadata,
        expected_revision: i64,
    ) -> RestoreProviderModel {
        RestoreProviderModel {
            generated_thinking_level_map: Some(crate::thinking::generate_thinking_level_map(
                &metadata,
            )),
            metadata,
            snapshot_state: SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog".into(),
                },
                pinned: std::collections::BTreeSet::new(),
            },
            expected_revision,
        }
    }

    #[tokio::test]
    async fn restore_commits_model_targets_and_epoch_atomically() {
        let storage = MemoryStorage::new(
            vec![provider("provider"), provider("other")],
            Vec::new(),
            vec![(crate::storage::CONFIG_EPOCH_KEY.to_string(), "41".into())],
        );
        let models = storage.provider_models();
        models
            .create(discovered_provider_model("m1"), None)
            .await
            .expect("insert model");

        let unrelated_map = crate::thinking::generate_thinking_level_map(
            &crate::provider_models::ProviderModelMetadata::bare("m9"),
        );
        let route = storage
            .put(PutRoute {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: None,
                model_id: "r1".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![
                    crate::db::models::CreateTarget {
                        thinking_level_map: seeded_thinking_map(),
                        ..target("provider", "m1")
                    },
                    crate::db::models::CreateTarget {
                        enabled: false,
                        thinking_level_map: seeded_thinking_map(),
                        ..target("provider", "m1")
                    },
                    crate::db::models::CreateTarget {
                        thinking_level_map: unrelated_map.clone(),
                        ..target("other", "m9")
                    },
                ]),
                default_thinking_level: None,
                billing: None,
            })
            .await
            .expect("put enabled Route");
        let unrelated_target_id = route.targets[2].id.clone();
        // 禁用 Route 的关联 Target 同样刷新，但不进入 active_routes。
        let disabled_route = storage
            .put(PutRoute {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: None,
                model_id: "r2".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: false,
                targets: Some(vec![crate::db::models::CreateTarget {
                    thinking_level_map: seeded_thinking_map(),
                    ..target("provider", "m1")
                }]),
                default_thinking_level: None,
                billing: None,
            })
            .await
            .expect("put disabled Route");

        let metadata = restore_metadata();
        let outcome = models
            .restore(
                "provider",
                "m1",
                restore_input(metadata.clone(), 1),
                &|_, _, _| Ok(()),
                &|| -> anyhow::Result<()> { Ok(()) },
            )
            .await
            .expect("restore");
        let ProviderModelRestore::Applied {
            model,
            active_routes,
        } = outcome
        else {
            panic!("restore must apply");
        };

        assert_eq!(model.revision, 2);
        assert_eq!(model.metadata, metadata);
        assert_eq!(
            model.snapshot_state,
            SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog".into()
                },
                pinned: std::collections::BTreeSet::new(),
            }
        );
        assert_eq!(
            model.metadata_source_provider_id.as_deref(),
            Some("catalog")
        );
        assert_eq!(model.cost_rules.len(), 1);
        assert_eq!(
            model.cost_rules[0].kind,
            crate::provider_models::ProviderModelCostRuleKind::ContextOver200k
        );
        // 返回的 model 就是持久化后的记录。
        let stored = models
            .get("provider", "m1")
            .await
            .expect("get")
            .expect("model");
        assert_eq!(stored, *model);

        // 配置通知与快照同一提交。
        assert_eq!(
            storage
                .settings()
                .get(crate::storage::CONFIG_EPOCH_KEY)
                .await
                .expect("epoch")
                .as_deref(),
            Some("42")
        );

        // active_routes 是提交后的完整快照：只含启用 Route，且 supported
        // thinking levels 已按新 map 重算。
        assert_eq!(active_routes.len(), 1);
        let active = &active_routes[0];
        assert_eq!(active.model_id.as_str(), "r1");
        assert_eq!(active.targets.len(), 3);
        let persisted = RouteStore::get(&storage, "r1")
            .await
            .expect("get route")
            .expect("route");
        assert_eq!(active.targets.len(), persisted.targets.len());
        for (returned, stored) in active.targets.iter().zip(persisted.targets.iter()) {
            assert_eq!(returned.id, stored.id);
            assert_eq!(returned.thinking_level_map, stored.thinking_level_map);
        }
        assert_eq!(
            active.supported_thinking_levels,
            persisted.supported_thinking_levels
        );

        let low = crate::thinking::mapping_control(
            &active.targets[0].thinking_level_map,
            ThinkingLevel::Low,
        );
        assert_eq!(
            low,
            Some(&TargetThinkingControl::Effort {
                value: "manual".into()
            }),
            "Overridden row survives restore"
        );
        let medium = crate::thinking::mapping_control(
            &active.targets[0].thinking_level_map,
            ThinkingLevel::Medium,
        );
        assert_eq!(
            medium,
            Some(&TargetThinkingControl::Hidden),
            "Generated row is refreshed"
        );
        assert_eq!(
            crate::thinking::mapping_control(
                &active.targets[0].thinking_level_map,
                ThinkingLevel::High
            ),
            Some(&TargetThinkingControl::Effort {
                value: "high".into()
            })
        );
        // 禁用 Target 同样刷新。
        assert_eq!(
            crate::thinking::mapping_control(
                &active.targets[1].thinking_level_map,
                ThinkingLevel::Minimal
            ),
            Some(&TargetThinkingControl::Effort {
                value: "minimal".into()
            })
        );
        // 不相关 Target 完全不动：map、Target ID 与其他字段保留。
        assert_eq!(active.targets[2].thinking_level_map, unrelated_map);
        assert_eq!(active.targets[2].id, unrelated_target_id);
        assert_eq!(active.targets[2].provider_id().as_str(), "other");

        assert_eq!(
            active.supported_thinking_levels,
            vec![
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::High
            ]
        );

        let disabled = RouteStore::get(&storage, "r2")
            .await
            .expect("get disabled route")
            .expect("route");
        assert_eq!(
            crate::thinking::mapping_control(
                &disabled.targets[0].thinking_level_map,
                ThinkingLevel::High
            ),
            Some(&TargetThinkingControl::Effort {
                value: "high".into()
            }),
            "disabled Route's Target is refreshed too"
        );
        assert_eq!(disabled.targets[0].id, disabled_route.targets[0].id);
        assert_eq!(
            disabled.supported_thinking_levels,
            vec![
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::High
            ]
        );
    }

    #[tokio::test]
    async fn restore_before_commit_failure_leaves_no_partial_write() {
        let storage = MemoryStorage::new(
            vec![provider("provider")],
            Vec::new(),
            vec![(crate::storage::CONFIG_EPOCH_KEY.to_string(), "7".into())],
        );
        let models = storage.provider_models();
        models
            .create(discovered_provider_model("m1"), None)
            .await
            .expect("insert model");
        storage
            .put(PutRoute {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: None,
                model_id: "r1".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![crate::db::models::CreateTarget {
                    thinking_level_map: seeded_thinking_map(),
                    ..target("provider", "m1")
                }]),
                default_thinking_level: None,
                billing: None,
            })
            .await
            .expect("put Route");

        let metadata = restore_metadata();
        let result = models
            .restore(
                "provider",
                "m1",
                restore_input(metadata.clone(), 1),
                &|_, _, _| Ok(()),
                &|| -> anyhow::Result<()> { anyhow::bail!("vendor write permit revoked") },
            )
            .await;
        assert_eq!(
            result.expect_err("restore must fail").to_string(),
            "vendor write permit revoked"
        );

        // 整笔不变：metadata/revision/snapshot、Target map、epoch 全部保持原值。
        let model = models
            .get("provider", "m1")
            .await
            .expect("get")
            .expect("model");
        assert_eq!(model.revision, 1);
        assert_eq!(
            model.metadata,
            crate::provider_models::ProviderModelMetadata::bare("m1")
        );
        assert_eq!(
            model.snapshot_state,
            SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog".into()
                },
                pinned: std::collections::BTreeSet::new(),
            }
        );
        let route = RouteStore::get(&storage, "r1")
            .await
            .expect("get route")
            .expect("route");
        assert_eq!(route.targets[0].thinking_level_map, seeded_thinking_map());
        assert_eq!(
            storage
                .settings()
                .get(crate::storage::CONFIG_EPOCH_KEY)
                .await
                .expect("epoch")
                .as_deref(),
            Some("7")
        );

        // 失败不消耗 revision：重试仍可提交。
        let retry = models
            .restore(
                "provider",
                "m1",
                restore_input(metadata, 1),
                &|_, _, _| Ok(()),
                &|| -> anyhow::Result<()> { Ok(()) },
            )
            .await
            .expect("retry");
        assert!(matches!(retry, ProviderModelRestore::Applied { .. }));
    }

    #[tokio::test]
    async fn restore_rejects_stale_revision_and_missing_model_without_writes() {
        let storage = MemoryStorage::new(vec![provider("provider")], Vec::new(), Vec::new());
        let models = storage.provider_models();
        models
            .create(discovered_provider_model("m1"), None)
            .await
            .expect("insert model");
        storage
            .put(PutRoute {
                output_modality: Some(crate::db::models::RouteOutputModality::Text),
                id: None,
                model_id: "r1".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![crate::db::models::CreateTarget {
                    thinking_level_map: seeded_thinking_map(),
                    ..target("provider", "m1")
                }]),
                default_thinking_level: None,
                billing: None,
            })
            .await
            .expect("put Route");

        // before_commit 在冲突/缺失路径上不得被调用。
        let called = std::sync::atomic::AtomicBool::new(false);
        let guard = || -> anyhow::Result<()> {
            called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };

        let conflict = models
            .restore(
                "provider",
                "m1",
                restore_input(restore_metadata(), 99),
                &|_, _, _| Ok(()),
                &guard,
            )
            .await
            .expect("conflict result");
        assert!(matches!(conflict, ProviderModelRestore::Conflict));
        let missing = models
            .restore(
                "provider",
                "absent",
                restore_input(restore_metadata(), 1),
                &|_, _, _| Ok(()),
                &guard,
            )
            .await
            .expect("missing result");
        assert!(matches!(missing, ProviderModelRestore::NotFound));
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "before_commit must not run when the write is rejected"
        );

        let model = models
            .get("provider", "m1")
            .await
            .expect("get")
            .expect("model");
        assert_eq!(model.revision, 1);
        assert_eq!(
            model.metadata,
            crate::provider_models::ProviderModelMetadata::bare("m1")
        );
        assert!(
            storage
                .settings()
                .get(crate::storage::CONFIG_EPOCH_KEY)
                .await
                .expect("epoch")
                .is_none(),
            "rejected restore must not touch config_epoch"
        );
    }
}
