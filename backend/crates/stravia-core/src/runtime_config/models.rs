use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

use super::RuntimeConfig;
use crate::provider_models::*;
use crate::storage::traits::ProviderModelStore;

pub(super) struct ModelIndex {
    records: Vec<ProviderModelRecord>,
    exact: HashMap<String, usize>,
    normalized: HashMap<String, Option<usize>>,
}

impl ModelIndex {
    pub(super) fn new(records: Vec<ProviderModelRecord>) -> Self {
        let mut exact = HashMap::with_capacity(records.len());
        let mut normalized = HashMap::with_capacity(records.len());
        for (position, record) in records.iter().enumerate() {
            exact.entry(record.model_id.clone()).or_insert(position);
            let key = model_id_match_key(&record.model_id);
            if !key.is_empty() {
                normalized
                    .entry(key)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(position));
            }
        }
        Self {
            records,
            exact,
            normalized,
        }
    }

    fn get(&self, model_id: &str) -> Option<ProviderModelRecord> {
        self.exact
            .get(model_id)
            .map(|&position| self.records[position].clone())
    }

    fn find(&self, model_id: &str) -> Option<ProviderModelRecord> {
        if let Some(record) = self.get(model_id.trim()) {
            return Some(record);
        }
        let key = model_id_match_key(model_id);
        self.normalized
            .get(&key)
            .copied()
            .flatten()
            .map(|position| self.records[position].clone())
    }
}

impl RuntimeConfig {
    async fn model_index(&self, provider_id: &str) -> anyhow::Result<Arc<ModelIndex>> {
        self.read(
            |values| values.models.get(provider_id).cloned(),
            |values, index| {
                values.models.insert(provider_id.to_owned(), index);
            },
            || async {
                let records = self
                    .source
                    .provider_models()
                    .list_for_provider(provider_id)
                    .await?;
                Ok(Arc::new(ModelIndex::new(records)))
            },
        )
        .await
    }
}

#[async_trait]
impl ProviderModelStore for RuntimeConfig {
    async fn list_for_provider(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderModelRecord>> {
        Ok(self.model_index(provider_id).await?.records.clone())
    }

    async fn get(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<Option<ProviderModelRecord>> {
        Ok(self.model_index(provider_id).await?.get(model_id))
    }

    async fn find(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<Option<ProviderModelRecord>> {
        Ok(self.model_index(provider_id).await?.find(model_id))
    }

    async fn apply_reconciliation(
        &self,
        provider_id: &str,
        reconciliation: ProviderModelReconciliation,
        validate_map: &ProviderModelThinkingMapValidator<'_>,
    ) -> anyhow::Result<()> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .provider_models()
            .apply_reconciliation(provider_id, reconciliation, validate_map)
            .await;
        mutation.finish();
        result
    }

    async fn create(
        &self,
        input: NewProviderModelRecord,
        change_record: Option<ModelChangeRecord>,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .provider_models()
            .create(input, change_record)
            .await;
        mutation.finish();
        result
    }

    async fn update_metadata(
        &self,
        provider_id: &str,
        model_id: &str,
        metadata: ProviderModelMetadata,
        snapshot_state: SnapshotState,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .provider_models()
            .update_metadata(
                provider_id,
                model_id,
                metadata,
                snapshot_state,
                expected_revision,
            )
            .await;
        mutation.finish();
        result
    }

    async fn restore(
        &self,
        provider_id: &str,
        model_id: &str,
        input: RestoreProviderModel,
        validate_map: &ProviderModelThinkingMapValidator<'_>,
        before_commit: &(dyn Fn() -> anyhow::Result<()> + Send + Sync),
    ) -> anyhow::Result<ProviderModelRestore> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .provider_models()
            .restore(provider_id, model_id, input, validate_map, before_commit)
            .await;
        mutation.finish();
        result
    }

    async fn update_selection_policy(
        &self,
        provider_id: &str,
        model_id: &str,
        policy: ProviderModelSelectionPolicy,
        expected_revision: i64,
        change_record: Option<ModelChangeRecord>,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .provider_models()
            .update_selection_policy(
                provider_id,
                model_id,
                policy,
                expected_revision,
                change_record,
            )
            .await;
        mutation.finish();
        result
    }

    async fn sync_state(&self, provider_id: &str) -> anyhow::Result<ProviderModelSyncState> {
        self.source.provider_models().sync_state(provider_id).await
    }

    async fn set_automatic_sync(
        &self,
        provider_id: &str,
        enabled: bool,
    ) -> anyhow::Result<ProviderModelSyncState> {
        self.source
            .provider_models()
            .set_automatic_sync(provider_id, enabled)
            .await
    }

    async fn save_sync_failure(
        &self,
        provider_id: &str,
        summary: ProviderModelSyncSummary,
        record: ModelChangeRecord,
    ) -> anyhow::Result<()> {
        self.source
            .provider_models()
            .save_sync_failure(provider_id, summary, record)
            .await
    }

    async fn list_changes(&self, query: ModelChangeQuery) -> anyhow::Result<ModelChangePage> {
        self.source.provider_models().list_changes(query).await
    }

    async fn get_change(&self, id: &str) -> anyhow::Result<Option<ModelChangeRecord>> {
        self.source.provider_models().get_change(id).await
    }

    async fn prune_changes(&self, before: &str) -> anyhow::Result<()> {
        self.source.provider_models().prune_changes(before).await
    }

    async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .provider_models()
            .delete_manual(provider_id, model_id)
            .await;
        mutation.finish();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{DynStorage, MemoryStorage};

    fn model(model_id: &str) -> NewProviderModelRecord {
        NewProviderModelRecord {
            provider_id: "provider".into(),
            model_id: model_id.into(),
            source_kind: ProviderModelSourceKind::Manual,
            snapshot_state: SnapshotState::Unregistered,
            source_metadata: None,
            metadata_source_provider_id: None,
            presence: ProviderModelPresence::Present,
            selection_policy: ProviderModelSelectionPolicy::Auto,
            metadata: ProviderModelMetadata::bare(model_id),
        }
    }

    async fn storage(model_ids: &[&str]) -> anyhow::Result<DynStorage> {
        let source: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        for model_id in model_ids {
            source
                .provider_models()
                .create(model(model_id), None)
                .await?;
        }
        RuntimeConfig::attach(source).await
    }

    #[tokio::test]
    async fn exact_identity_wins_over_ambiguous_normalization() -> anyhow::Result<()> {
        let storage = storage(&["Model", "vendor/model"]).await?;
        let models = storage.provider_models();
        assert_eq!(
            models
                .find("provider", "  Model  ")
                .await?
                .unwrap()
                .model_id,
            "Model"
        );
        assert!(models.get("provider", " Model ").await?.is_none());
        assert!(models.find("provider", "MODEL").await?.is_none());
        assert!(models.find("provider", "other/model").await?.is_none());
        assert!(models.find("provider", " ").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn normalized_lookup_matches_segment_and_case() -> anyhow::Result<()> {
        let storage = storage(&["vendor/Model"]).await?;
        assert_eq!(
            storage
                .provider_models()
                .find("provider", " MODEL ")
                .await?
                .unwrap()
                .model_id,
            "vendor/Model"
        );
        Ok(())
    }

    #[tokio::test]
    async fn updated_and_deleted_models_do_not_resurrect() -> anyhow::Result<()> {
        let storage = storage(&["vendor/Model"]).await?;
        let models = storage.provider_models();
        let original = models.find("provider", "model").await?.unwrap();
        let updated_metadata = ProviderModelMetadata::bare("updated");
        assert!(matches!(
            models
                .update_metadata(
                    "provider",
                    &original.model_id,
                    updated_metadata.clone(),
                    SnapshotState::Unregistered,
                    original.revision
                )
                .await?,
            ProviderModelMutation::Applied(_)
        ));
        for _ in 0..2 {
            assert_eq!(
                models
                    .get("provider", &original.model_id)
                    .await?
                    .unwrap()
                    .metadata,
                updated_metadata
            );
            assert_eq!(
                models.find("provider", "MODEL").await?.unwrap().metadata,
                updated_metadata
            );
        }
        assert!(models.delete_manual("provider", &original.model_id).await?);
        for _ in 0..2 {
            assert!(models.get("provider", &original.model_id).await?.is_none());
            assert!(models.find("provider", "model").await?.is_none());
            assert!(models.list_for_provider("provider").await?.is_empty());
        }
        Ok(())
    }
}
