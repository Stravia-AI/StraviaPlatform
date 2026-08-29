use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::provider_catalog::{CatalogError, CatalogModelSource};
use crate::provider_models::{
    CreateManualProviderModel, NewProviderModelRecord, ProviderModelDetail, ProviderModelMetadata,
    ProviderModelMutation, ProviderModelPresence, ProviderModelPresenceUpdate,
    ProviderModelReconciliation, ProviderModelSelectionPolicy, ProviderModelSourceKind,
    ProviderModelSummary, ProviderModelSyncSummary, UpdateProviderModel,
    UpdateProviderModelSelection, normalize_model_id,
};

#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelList {
    pub models: Vec<ProviderModelSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparedProviderModel {
    pub id: String,
    pub metadata: ProviderModelMetadata,
    pub extensions: Value,
}

impl AdminService {
    pub async fn list_provider_models(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<ProviderModelList> {
        self.get_provider(provider_id).await?;
        let mut models: Vec<_> = self
            .gw
            .storage
            .provider_models()
            .list_for_provider(provider_id)
            .await?
            .iter()
            .map(ProviderModelSummary::from)
            .collect();
        models.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(ProviderModelList { models })
    }

    pub async fn get_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<ProviderModelDetail> {
        self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        self.gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .map(ProviderModelDetail::from)
            .ok_or_else(|| provider_model_not_found(provider_id, &model_id))
    }

    pub async fn prepare_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        template_id: Option<&str>,
    ) -> anyhow::Result<PreparedProviderModel> {
        RouteModule::new(self)
            .prepare_provider_model(provider_id, model_id, template_id)
            .await
    }

    pub(super) async fn prepare_provider_model_record(
        &self,
        provider_id: &str,
        model_id: &str,
        template_id: Option<&str>,
    ) -> anyhow::Result<PreparedProviderModel> {
        self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        if self
            .gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .is_some()
        {
            return Err(provider_model_conflict(provider_id, &model_id));
        }
        let metadata = match template_id {
            Some(template_id) => {
                let mut template = self
                    .gw
                    .provider_catalog
                    .canonical_model(template_id)
                    .await?;
                template
                    .as_object_mut()
                    .expect("ProviderCatalog validates Canonical Model objects")
                    .insert("id".to_string(), Value::String(model_id.clone()));
                ProviderModelMetadata::from_source_value(&model_id, template)?
            }
            None => ProviderModelMetadata::bare(&model_id),
        };
        let extensions = metadata.extension_value();
        Ok(PreparedProviderModel {
            id: model_id,
            metadata,
            extensions,
        })
    }

    pub async fn create_manual_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        input: CreateManualProviderModel,
    ) -> anyhow::Result<ProviderModelDetail> {
        RouteModule::new(self)
            .add_provider_model(provider_id, model_id, input)
            .await
    }

    pub(super) async fn create_manual_provider_model_record(
        &self,
        provider_id: &str,
        model_id: &str,
        input: CreateManualProviderModel,
    ) -> anyhow::Result<ProviderModelDetail> {
        self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        let metadata = ProviderModelMetadata::from_value(&model_id, input.metadata)?;
        apply_provider_model_mutation(
            self.gw
                .storage
                .provider_models()
                .create(NewProviderModelRecord {
                    provider_id: provider_id.to_string(),
                    model_id: model_id.clone(),
                    source_kind: ProviderModelSourceKind::Manual,
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Present,
                    selection_policy: ProviderModelSelectionPolicy::Auto,
                    metadata,
                })
                .await?,
            provider_id,
            &model_id,
        )
    }

    pub async fn update_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        input: UpdateProviderModel,
    ) -> anyhow::Result<ProviderModelDetail> {
        self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        let existing = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .ok_or_else(|| provider_model_not_found(provider_id, &model_id))?;
        let mut metadata = ProviderModelMetadata::from_value(&model_id, input.metadata)?;
        metadata.provider = existing.metadata.provider;
        metadata.experimental = existing.metadata.experimental;
        metadata.status = existing.metadata.status;
        metadata.extensions = existing.metadata.extensions;
        apply_provider_model_mutation(
            self.gw
                .storage
                .provider_models()
                .update_metadata(provider_id, &model_id, metadata, input.revision)
                .await?,
            provider_id,
            &model_id,
        )
    }

    pub async fn update_provider_model_selection(
        &self,
        provider_id: &str,
        model_id: &str,
        input: UpdateProviderModelSelection,
    ) -> anyhow::Result<ProviderModelDetail> {
        self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        apply_provider_model_mutation(
            self.gw
                .storage
                .provider_models()
                .update_selection_policy(provider_id, &model_id, input.policy, input.revision)
                .await?,
            provider_id,
            &model_id,
        )
    }

    pub async fn reimport_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        revision: i64,
    ) -> anyhow::Result<ProviderModelDetail> {
        self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        let existing = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .ok_or_else(|| provider_model_not_found(provider_id, &model_id))?;
        let source_provider_id = existing
            .metadata_source_provider_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Provider Model has no Provider Catalog source"))?;
        let source = self
            .gw
            .provider_catalog
            .model_source(source_provider_id, &model_id)
            .await?;
        let metadata = ProviderModelMetadata::from_source_value(&model_id, source.metadata)?;
        super::RouteModule::new(self)
            .refresh_generated_thinking_maps(provider_id, &model_id, &metadata, false)
            .await?;
        let detail = apply_provider_model_mutation(
            self.gw
                .storage
                .provider_models()
                .update_metadata(provider_id, &model_id, metadata.clone(), revision)
                .await?,
            provider_id,
            &model_id,
        )?;
        super::RouteModule::new(self)
            .refresh_generated_thinking_maps(provider_id, &model_id, &metadata, true)
            .await?;
        Ok(detail)
    }

    pub async fn delete_manual_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<()> {
        self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        if self
            .gw
            .storage
            .provider_models()
            .delete_manual(provider_id, &model_id)
            .await?
        {
            Ok(())
        } else {
            Err(provider_model_not_found(provider_id, &model_id))
        }
    }

    pub async fn sync_provider_models(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<ProviderModelSyncSummary> {
        RouteModule::new(self).sync(provider_id).await
    }

    pub(super) async fn sync_provider_models_record(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<ProviderModelSyncSummary> {
        let provider = self.get_provider(provider_id).await?;
        let sources = self.discover_provider_model_sources(&provider).await?;
        if sources.is_empty() {
            anyhow::bail!("Provider model discovery returned an empty list");
        }
        let existing = self
            .gw
            .storage
            .provider_models()
            .list_for_provider(provider_id)
            .await?;
        let existing_by_id: BTreeMap<_, _> = existing
            .iter()
            .map(|model| (model.model_id.as_str(), model))
            .collect();
        let seen_ids: BTreeSet<String> = sources.keys().cloned().collect();
        let mut reconciliation = ProviderModelReconciliation::default();
        let mut summary = ProviderModelSyncSummary {
            added: 0,
            missing: 0,
            restored: 0,
            deprecated: 0,
        };

        for (model_id, source) in sources {
            let metadata = metadata_from_source(&model_id, source.as_ref())?;
            if let Some(current) = existing_by_id.get(model_id.as_str()) {
                if current.source_kind == ProviderModelSourceKind::Manual {
                    continue;
                }
                if current.presence == ProviderModelPresence::Missing {
                    summary.restored += 1;
                }
                if current.metadata.status.as_deref() != Some("deprecated")
                    && metadata.status.as_deref() == Some("deprecated")
                {
                    summary.deprecated += 1;
                }
                if current.presence != ProviderModelPresence::Present
                    || current.metadata.status != metadata.status
                {
                    reconciliation.updates.push(ProviderModelPresenceUpdate {
                        model_id,
                        presence: ProviderModelPresence::Present,
                        lifecycle_status: metadata.status,
                    });
                }
                continue;
            }
            summary.added += 1;
            if metadata.status.as_deref() == Some("deprecated") {
                summary.deprecated += 1;
            }
            reconciliation.inserts.push(NewProviderModelRecord {
                provider_id: provider_id.to_string(),
                model_id,
                source_kind: ProviderModelSourceKind::Discovered,
                metadata_source_provider_id: source
                    .as_ref()
                    .map(|source| source.provider_id.clone()),
                presence: ProviderModelPresence::Present,
                selection_policy: ProviderModelSelectionPolicy::Auto,
                metadata,
            });
        }

        for current in existing
            .iter()
            .filter(|model| model.source_kind == ProviderModelSourceKind::Discovered)
        {
            if !seen_ids.contains(current.model_id.as_str())
                && current.presence != ProviderModelPresence::Missing
            {
                summary.missing += 1;
                reconciliation.updates.push(ProviderModelPresenceUpdate {
                    model_id: current.model_id.clone(),
                    presence: ProviderModelPresence::Missing,
                    lifecycle_status: current.metadata.status.clone(),
                });
            }
        }

        self.gw
            .storage
            .provider_models()
            .apply_reconciliation(provider_id, reconciliation)
            .await?;
        Ok(summary)
    }

    async fn discover_provider_model_sources(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<BTreeMap<String, Option<CatalogModelSource>>> {
        if uses_catalog_inventory(provider) {
            let provider_id = provider
                .preset_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Catalog Provider is missing its catalog ID"))?;
            let channel_id = provider.channel.as_deref().unwrap_or("default");
            let sources = self
                .gw
                .provider_catalog
                .model_sources(provider_id, channel_id)
                .await?;
            return sources
                .into_iter()
                .map(|source| {
                    let model_id = source
                        .metadata
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("Provider Catalog Entry is missing id"))?
                        .to_string();
                    Ok((model_id, Some(source)))
                })
                .collect();
        }

        let ids = self.test_provider_models(&provider.id).await?;
        let mut sources = BTreeMap::new();
        for id in ids {
            let model_id = normalize_model_id(&id)?;
            let source = match provider.preset_key.as_deref() {
                Some(catalog_provider_id) => match self
                    .gw
                    .provider_catalog
                    .model_source(catalog_provider_id, &model_id)
                    .await
                {
                    Ok(source) => Some(source),
                    Err(error)
                        if matches!(
                            error.downcast_ref::<CatalogError>(),
                            Some(CatalogError::EntryNotFound { .. })
                        ) =>
                    {
                        None
                    }
                    Err(error) => return Err(error),
                },
                None => None,
            };
            sources.insert(model_id, source);
        }
        Ok(sources)
    }
}

fn metadata_from_source(
    model_id: &str,
    source: Option<&CatalogModelSource>,
) -> anyhow::Result<ProviderModelMetadata> {
    match source {
        Some(source) => ProviderModelMetadata::from_source_value(model_id, source.metadata.clone()),
        None => Ok(ProviderModelMetadata::bare(model_id)),
    }
}

fn apply_provider_model_mutation(
    mutation: ProviderModelMutation,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<ProviderModelDetail> {
    match mutation {
        ProviderModelMutation::Applied(model) => Ok(ProviderModelDetail::from(*model)),
        ProviderModelMutation::NotFound => Err(provider_model_not_found(provider_id, model_id)),
        ProviderModelMutation::Conflict => Err(provider_model_conflict(provider_id, model_id)),
    }
}

fn provider_model_not_found(provider_id: &str, model_id: &str) -> anyhow::Error {
    coded_error(
        "PROVIDER_MODEL_NOT_FOUND",
        "Provider Model not found",
        serde_json::json!({ "provider_id": provider_id, "model_id": model_id }),
    )
}

fn provider_model_conflict(provider_id: &str, model_id: &str) -> anyhow::Error {
    coded_error(
        "PROVIDER_MODEL_CONFLICT",
        "Provider Model has changed; reload it and retry",
        serde_json::json!({ "provider_id": provider_id, "model_id": model_id }),
    )
}
