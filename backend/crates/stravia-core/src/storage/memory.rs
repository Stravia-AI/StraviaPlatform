use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::db::models::{
    ApiKeyStats, CreateModel, CreateProviderRecord, LogPage, LogQuery, Model, ModelStats,
    OAuthCredential, Provider, ProviderStats, RequestLog, StatsHourly, StatsOverview, UpdateModel,
    UpdateProvider, UpsertOAuthCredential,
};
use crate::logging::LogEntry;
use crate::provider_models::{
    NewProviderModelRecord, ProviderModelMutation, ProviderModelReconciliation,
    ProviderModelRecord, ProviderModelSelectionPolicy, ProviderModelSourceKind,
};

use super::traits::{
    ApiKeyStore, AuthAccessStore, LogStore, ModelBackendStore, ModelSnapshotStore, ModelStore,
    OAuthCredentialStore, ProviderModelStore, ProviderStore, ProviderTestResult, SettingsStore,
    Storage, StorageBackend, StorageBootstrap, StorageHealth,
};

use std::sync::Arc;

#[derive(Clone)]
pub struct MemoryStorage {
    providers: Arc<RwLock<Vec<Provider>>>,
    models: Arc<RwLock<Vec<Model>>>,
    settings: Arc<RwLock<Vec<(String, String)>>>,
    provider_models: Arc<RwLock<Vec<ProviderModelRecord>>>,
    oauth_credentials: Arc<MemoryOAuthCredentialStore>,
}

impl MemoryStorage {
    pub fn new(
        providers: Vec<Provider>,
        models: Vec<Model>,
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
    fn models(&self) -> &dyn ModelStore {
        self
    }
    fn snapshots(&self) -> &dyn ModelSnapshotStore {
        self
    }
    fn model_backends(&self) -> Option<&dyn ModelBackendStore> {
        None
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

    async fn create(&self, _input: CreateProviderRecord) -> anyhow::Result<Provider> {
        anyhow::bail!("create not supported by memory storage")
    }

    async fn update(&self, _id: &str, _input: UpdateProvider) -> anyhow::Result<Provider> {
        anyhow::bail!("update not supported by memory storage")
    }

    async fn delete(&self, _id: &str) -> anyhow::Result<()> {
        anyhow::bail!("delete not supported by memory storage")
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let providers = self.providers.read().await;
        Ok(providers
            .iter()
            .any(|p| p.name == name && exclude_id.is_none_or(|eid| p.id != eid)))
    }

    async fn record_test_result(
        &self,
        _provider_id: &str,
        _result: ProviderTestResult,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ModelStore for MemoryStorage {
    async fn list(&self) -> anyhow::Result<Vec<Model>> {
        Ok(self.models.read().await.clone())
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Model>> {
        Ok(self
            .models
            .read()
            .await
            .iter()
            .find(|m| m.id == id)
            .cloned())
    }

    async fn create(&self, _input: CreateModel) -> anyhow::Result<Model> {
        anyhow::bail!("create not supported by memory storage")
    }

    async fn update(&self, _id: &str, _input: UpdateModel) -> anyhow::Result<Model> {
        anyhow::bail!("update not supported by memory storage")
    }

    async fn delete(&self, _id: &str) -> anyhow::Result<()> {
        anyhow::bail!("delete not supported by memory storage")
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let models = self.models.read().await;
        Ok(models
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(name) && exclude_id.is_none_or(|eid| m.id != eid)))
    }
}

#[async_trait]
impl ModelSnapshotStore for MemoryStorage {
    async fn load_active_snapshot(&self) -> anyhow::Result<Vec<Model>> {
        let models = self.models.read().await;
        Ok(models.iter().filter(|m| m.is_enabled).cloned().collect())
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
