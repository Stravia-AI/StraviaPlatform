use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::db::models::{
    ApiKeyStats, CreateProviderRecord, LogPage, LogQuery, ModelStats, OAuthCredential, Provider,
    ProviderStats, PutRoute, RequestLog, Route, StatsHourly, StatsOverview, Target, UpdateProvider,
    UpsertOAuthCredential,
};
use crate::logging::LogEntry;
use crate::provider_models::{
    NewProviderModelRecord, ProviderModelMutation, ProviderModelReconciliation,
    ProviderModelRecord, ProviderModelSelectionPolicy, ProviderModelSourceKind,
};

use super::traits::{
    ApiKeyStore, AuthAccessStore, LogStore, OAuthCredentialStore, ProviderModelStore,
    ProviderStore, ProviderTestResult, RouteStore, SettingsStore, Storage, StorageBackend,
    StorageBootstrap, StorageHealth,
};

use std::sync::Arc;

#[derive(Clone)]
pub struct MemoryStorage {
    providers: Arc<RwLock<Vec<Provider>>>,
    models: Arc<RwLock<Vec<Route>>>,
    settings: Arc<RwLock<Vec<(String, String)>>>,
    provider_models: Arc<RwLock<Vec<ProviderModelRecord>>>,
    oauth_credentials: Arc<MemoryOAuthCredentialStore>,
}

impl MemoryStorage {
    pub fn new(
        providers: Vec<Provider>,
        models: Vec<Route>,
        settings: Vec<(String, String)>,
    ) -> Self {
        Self {
            providers: Arc::new(RwLock::new(providers)),
            models: Arc::new(RwLock::new(models)),
            settings: Arc::new(RwLock::new(settings)),
            provider_models: Arc::new(RwLock::new(Vec::new())),
            oauth_credentials: Arc::new(MemoryOAuthCredentialStore {
                credentials: RwLock::new(std::collections::HashMap::new()),
            }),
        }
    }
}

pub struct MemoryOAuthCredentialStore {
    credentials: RwLock<std::collections::HashMap<String, OAuthCredential>>,
}

impl Storage for MemoryStorage {
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
    fn logs(&self) -> &dyn LogStore {
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
            id: uuid::Uuid::new_v4().to_string(),
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
            auth_mode: input.auth_mode,
            use_proxy: input.use_proxy,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: now.clone(),
            updated_at: now,
        };
        self.providers.write().await.push(provider.clone());
        Ok(provider)
    }

    async fn update(&self, id: &str, input: UpdateProvider) -> anyhow::Result<Provider> {
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
        if let Some(value) = input.auth_mode {
            provider.auth_mode = value;
        }
        if let Some(value) = input.use_proxy {
            provider.use_proxy = value;
        }
        if let Some(value) = input.is_enabled {
            provider.is_enabled = value;
        }
        provider.updated_at = now_rfc3339();
        Ok(provider.clone())
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut providers = self.providers.write().await;
        let mut routes = self.models.write().await;
        let mut provider_models = self.provider_models.write().await;
        providers.retain(|provider| provider.id != id);
        provider_models.retain(|model| model.provider_id != id);
        for route in routes.iter_mut() {
            route.targets.retain(|target| target.provider_id != id);
            if let Some(primary) = route.targets.first() {
                route.target_provider.clone_from(&primary.provider_id);
                route.target_model.clone_from(&primary.model);
            }
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
        provider.updated_at = now_rfc3339();
        Ok(())
    }
}

#[async_trait]
impl RouteStore for MemoryStorage {
    async fn list(&self) -> anyhow::Result<Vec<Route>> {
        Ok(self.models.read().await.clone())
    }

    async fn list_active(&self) -> anyhow::Result<Vec<Route>> {
        let models = self.models.read().await;
        Ok(models
            .iter()
            .filter(|route| route.is_enabled)
            .cloned()
            .collect())
    }

    async fn get(&self, route_id: &str) -> anyhow::Result<Option<Route>> {
        Ok(self
            .models
            .read()
            .await
            .iter()
            .find(|route| route.name == route_id)
            .cloned())
    }

    async fn put(&self, input: PutRoute) -> anyhow::Result<Route> {
        anyhow::ensure!(
            !input.targets.is_empty(),
            "a Route requires at least one Target"
        );
        let mut routes = self.models.write().await;
        let storage_id = input
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        anyhow::ensure!(
            !routes
                .iter()
                .any(|route| route.name == input.route_id && route.id != storage_id),
            "Route ID already exists: {}",
            input.route_id
        );
        let created_at = routes
            .iter()
            .find(|route| route.id == storage_id)
            .map(|route| route.created_at.clone())
            .unwrap_or_else(now_rfc3339);
        let existing_targets = routes
            .iter()
            .find(|route| route.id == storage_id)
            .map(|route| route.targets.clone())
            .unwrap_or_default();
        let targets = input
            .targets
            .into_iter()
            .map(|target| Target {
                id: existing_targets
                    .iter()
                    .find(|current| {
                        current.provider_id == target.provider_id && current.model == target.model
                    })
                    .map(|current| current.id.clone())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                model_id: storage_id.clone(),
                provider_id: target.provider_id,
                model: target.model,
                weight: target.weight.unwrap_or(100).max(0),
                priority: target.priority.unwrap_or(1).max(1),
                created_at: now_rfc3339(),
                thinking_level_map: sqlx::types::Json(target.thinking_level_map),
            })
            .collect::<Vec<_>>();
        let mut route = Route {
            id: storage_id.clone(),
            name: input.route_id,
            balance: input.selection_strategy,
            target_provider: targets[0].provider_id.clone(),
            target_model: targets[0].model.clone(),
            is_enabled: input.is_enabled,
            created_at,
            supported_thinking_levels: sqlx::types::Json(Vec::new()),
            context_window: None,
            output_max_tokens: None,
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
            .retain(|route| route.name != route_id);
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
impl LogStore for MemoryStorage {
    async fn append_batch(&self, _entries: Vec<LogEntry>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn query(&self, _query: LogQuery) -> anyhow::Result<LogPage> {
        Ok(LogPage {
            items: vec![],
            total: 0,
        })
    }

    async fn find_by_id(&self, _id: &str) -> anyhow::Result<Option<RequestLog>> {
        Ok(None)
    }

    async fn cleanup_before(&self, _cutoff: &str) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn clear_all(&self) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn stats_overview(&self, _hours: Option<i64>) -> anyhow::Result<StatsOverview> {
        Ok(StatsOverview::default())
    }

    async fn stats_hourly(&self, _hours: i64) -> anyhow::Result<Vec<StatsHourly>> {
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
        for update in reconciliation.updates {
            if let Some(item) = items.iter_mut().find(|item| {
                item.provider_id == provider_id
                    && item.model_id == update.model_id
                    && item.source_kind == ProviderModelSourceKind::Discovered
            }) {
                let changed = item.presence != update.presence
                    || item.metadata.status != update.lifecycle_status;
                if changed {
                    item.presence = update.presence;
                    item.metadata.status = update.lifecycle_status;
                    item.revision += 1;
                    item.updated_at = now.clone();
                }
            }
        }
        for input in reconciliation.inserts {
            if items.iter().any(|item| {
                item.provider_id == input.provider_id && item.model_id == input.model_id
            }) {
                continue;
            }
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
            connection_id: uuid::Uuid::new_v4().to_string(),
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

    async fn list_expiring(&self, _before: Duration) -> anyhow::Result<Vec<OAuthCredential>> {
        let map = self.credentials.read().await;
        Ok(map
            .values()
            .filter(|c| c.status == "connected")
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
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        }
    }

    fn target(provider_id: &str, model: &str) -> crate::db::models::CreateTarget {
        crate::db::models::CreateTarget {
            provider_id: provider_id.into(),
            model: model.into(),
            weight: Some(100),
            priority: Some(1),
            thinking_level_map: Vec::new(),
        }
    }

    #[tokio::test]
    async fn route_put_is_an_exact_aggregate_contract() {
        let storage = MemoryStorage::new(Vec::new(), Vec::new(), Vec::new());
        let route = storage
            .put(PutRoute {
                id: None,
                route_id: "CaseRoute".into(),
                selection_strategy: "priority".into(),
                is_enabled: true,
                targets: vec![target("p1", "m1"), target("p2", "m2")],
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

        let updated = storage
            .put(PutRoute {
                id: Some(route.id),
                route_id: "CaseRoute".into(),
                selection_strategy: "weighted".into(),
                is_enabled: true,
                targets: vec![target("p2", "m2")],
            })
            .await
            .expect("replace Route aggregate");
        assert_eq!(updated.targets.len(), 1);
        assert_eq!(updated.targets[0].provider_id, "p2");
    }

    #[tokio::test]
    async fn provider_delete_prunes_targets_and_routes_atomically() {
        let storage =
            MemoryStorage::new(vec![provider("p1"), provider("p2")], Vec::new(), Vec::new());
        storage
            .put(PutRoute {
                id: None,
                route_id: "empty-after-delete".into(),
                selection_strategy: "weighted".into(),
                is_enabled: true,
                targets: vec![target("p1", "m1")],
            })
            .await
            .expect("put disposable Route");
        storage
            .put(PutRoute {
                id: None,
                route_id: "survives-delete".into(),
                selection_strategy: "priority".into(),
                is_enabled: true,
                targets: vec![target("p1", "m1"), target("p2", "m2")],
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
        assert_eq!(survivor.targets[0].provider_id, "p2");
    }

    #[tokio::test]
    async fn cancelled_refresh_lease_is_retryable_and_cas_safe() {
        let store = MemoryOAuthCredentialStore {
            credentials: RwLock::new(std::collections::HashMap::new()),
        };
        let original = UpsertOAuthCredential {
            driver_key: "codex".into(),
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
                    driver_key: "codex".into(),
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
                    driver_key: "codex".into(),
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
                    driver_key: "codex".into(),
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
}
