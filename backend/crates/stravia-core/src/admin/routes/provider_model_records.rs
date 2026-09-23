use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::provider_models::{
    CreateManualProviderModel, NewProviderModelRecord, ProviderModelDetail, ProviderModelMetadata,
    ProviderModelMutation, ProviderModelPresence, ProviderModelPresenceUpdate,
    ProviderModelReconciliation, ProviderModelSelectionPolicy, ProviderModelSourceKind,
    ProviderModelSummary, ProviderModelSyncSummary, SnapshotState, SourceStamp,
    UpdateProviderModel, UpdateProviderModelSelection, normalize_model_id,
};

#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelList {
    pub models: Vec<ProviderModelSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparedProviderModel {
    pub id: String,
    pub snapshot_state: SnapshotState,
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
        let provider = self.get_provider(provider_id).await?;
        let model_id = normalize_model_id(model_id)?;
        let mut detail = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .map(ProviderModelDetail::from)
            .ok_or_else(|| provider_model_not_found(provider_id, &model_id))?;
        super::thinking_map::hide_unwritable_generated_controls(
            self,
            &provider,
            &detail.metadata,
            &mut detail.thinking_level_map,
        );
        Ok(detail)
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
        let (metadata, snapshot_state) = match template_id {
            Some(template_id) => {
                let template = self
                    .gw
                    .provider_catalog
                    .canonical_model(template_id)
                    .await?;
                let canonical_id = canonical_template_id(&template)?;
                (
                    metadata_from_canonical_template(&model_id, template)?,
                    SnapshotState::Imported {
                        source: SourceStamp::Canonical {
                            model_id: canonical_id,
                        },
                    },
                )
            }
            None => match self
                .gw
                .provider_catalog
                .canonical_model_matching_upstream_id(&model_id)
                .await
            {
                Some(template) => {
                    let canonical_id = canonical_template_id(&template)?;
                    (
                        metadata_from_canonical_template(&model_id, template)?,
                        SnapshotState::Imported {
                            source: SourceStamp::Canonical {
                                model_id: canonical_id,
                            },
                        },
                    )
                }
                None => (
                    ProviderModelMetadata::bare(&model_id),
                    SnapshotState::Unregistered,
                ),
            },
        };
        let extensions = metadata.extension_value();
        Ok(PreparedProviderModel {
            id: model_id,
            snapshot_state,
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
        let provider = self.get_provider(provider_id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let operation = self.gw.vendor_plugins.operations.begin(vendor)?;
        let _write_fence = operation.write_fence().await?;
        let model_id = normalize_model_id(model_id)?;
        let metadata = ProviderModelMetadata::from_value(&model_id, input.metadata)?;
        let source = match input.template_id {
            Some(template_id) => {
                let template = self
                    .gw
                    .provider_catalog
                    .canonical_model(&template_id)
                    .await?;
                Some(SourceStamp::Canonical {
                    model_id: canonical_template_id(&template)?,
                })
            }
            None => None,
        };
        apply_provider_model_mutation(
            self,
            self.gw
                .storage
                .provider_models()
                .create(NewProviderModelRecord {
                    provider_id: provider_id.to_string(),
                    model_id: model_id.clone(),
                    source_kind: ProviderModelSourceKind::Manual,
                    snapshot_state: SnapshotState::Edited { source },
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Present,
                    selection_policy: ProviderModelSelectionPolicy::Auto,
                    metadata,
                })
                .await?,
            &provider,
            &model_id,
        )
    }

    pub async fn update_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        input: UpdateProviderModel,
    ) -> anyhow::Result<ProviderModelDetail> {
        let provider = self.get_provider(provider_id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let operation = self.gw.vendor_plugins.operations.begin(vendor)?;
        let _write_fence = operation.write_fence().await?;
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
            self,
            self.gw
                .storage
                .provider_models()
                .update_metadata(
                    provider_id,
                    &model_id,
                    metadata,
                    SnapshotState::Edited {
                        source: existing.snapshot_state.source().cloned(),
                    },
                    input.revision,
                )
                .await?,
            &provider,
            &model_id,
        )
    }

    pub async fn update_provider_model_selection(
        &self,
        provider_id: &str,
        model_id: &str,
        input: UpdateProviderModelSelection,
    ) -> anyhow::Result<ProviderModelDetail> {
        let provider = self.get_provider(provider_id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let operation = self.gw.vendor_plugins.operations.begin(vendor)?;
        let _write_fence = operation.write_fence().await?;
        let model_id = normalize_model_id(model_id)?;
        apply_provider_model_mutation(
            self,
            self.gw
                .storage
                .provider_models()
                .update_selection_policy(provider_id, &model_id, input.policy, input.revision)
                .await?,
            &provider,
            &model_id,
        )
    }

    pub async fn reimport_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        revision: i64,
    ) -> anyhow::Result<ProviderModelDetail> {
        let provider = self.get_provider(provider_id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let operation = self.gw.vendor_plugins.operations.begin(vendor)?;
        let _write_fence = operation.write_fence().await?;
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
            .catalog_sync
            .catalog_model_source(source_provider_id, &model_id)
            .await?;
        let metadata = ProviderModelMetadata::from_source_value(&model_id, source.metadata)?;
        super::RouteModule::new(self)
            .refresh_generated_thinking_maps(provider_id, &model_id, &metadata, false)
            .await?;
        let detail = apply_provider_model_mutation(
            self,
            self.gw
                .storage
                .provider_models()
                .update_metadata(
                    provider_id,
                    &model_id,
                    metadata.clone(),
                    SnapshotState::Imported {
                        source: SourceStamp::ProviderCatalog {
                            provider_id: source_provider_id.to_owned(),
                        },
                    },
                    revision,
                )
                .await?,
            &provider,
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
        let provider = self.get_provider(provider_id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let operation = self.gw.vendor_plugins.operations.begin(vendor)?;
        let _write_fence = operation.write_fence().await?;
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
        let (sources, _publication_guard) = self.discover_provider_model_sources(&provider).await?;
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
            let metadata = source.metadata;
            let incoming_state = source.snapshot_state;
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
                // Only explicitly unregistered snapshots may accept a first authoritative
                // specification. All later discovery refreshes plugin-owned runtime fields,
                // never user-editable model specifications or historical provenance.
                let first_import = current.snapshot_state == SnapshotState::Unregistered
                    && matches!(incoming_state, SnapshotState::Imported { .. });
                let mut merged = if first_import {
                    metadata.clone()
                } else {
                    current.metadata.clone()
                };
                merged.status = metadata.status.clone();
                merged.provider = metadata.provider.clone();
                merged.experimental = metadata.experimental.clone();
                merged.extensions = metadata.extensions.clone();
                let refreshed_metadata =
                    (first_import || merged != current.metadata).then_some(merged);
                if current.presence != ProviderModelPresence::Present
                    || current.metadata.status != metadata.status
                    || current.metadata_source_provider_id != source.metadata_source_provider_id
                    || refreshed_metadata.is_some()
                {
                    reconciliation.updates.push(ProviderModelPresenceUpdate {
                        model_id,
                        expected_revision: current.revision,
                        snapshot_state: first_import.then_some(incoming_state),
                        metadata_source_provider_id: source.metadata_source_provider_id,
                        presence: ProviderModelPresence::Present,
                        lifecycle_status: metadata.status.clone(),
                        metadata: refreshed_metadata,
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
                snapshot_state: incoming_state,
                metadata_source_provider_id: source.metadata_source_provider_id,
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
                    expected_revision: current.revision,
                    snapshot_state: None,
                    metadata_source_provider_id: current.metadata_source_provider_id.clone(),
                    presence: ProviderModelPresence::Missing,
                    lifecycle_status: current.metadata.status.clone(),
                    metadata: None,
                });
            }
        }

        let latest_provider = self.get_provider(provider_id).await?;
        anyhow::ensure!(
            same_discovery_provider(&provider, &latest_provider),
            "Provider changed while synchronizing discovered models"
        );
        self.gw
            .storage
            .provider_models()
            .apply_reconciliation(provider_id, reconciliation)
            .await?;
        self.gw
            .vendor_plugins
            .store
            .recovered(provider_id, "models")
            .await?;
        Ok(summary)
    }

    async fn discover_provider_model_sources(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<(
        BTreeMap<String, DiscoveredModelSource>,
        tokio::sync::OwnedRwLockReadGuard<()>,
    )> {
        let discovered =
            super::model_discovery::discover_provider_models(self, &provider.id).await?;
        // Hold the vendor publication fence until the single Provider Model
        // reconciliation has committed.
        let publication_guard = discovered.write_fence().await?;
        let mut catalog_sources = BTreeMap::new();
        if let Some(catalog_provider_id) = provider.preset_key.as_deref() {
            let scope = match self
                .gw
                .catalog_sync
                .provider_scope(catalog_provider_id)
                .await
            {
                Ok(scope) => Some(scope),
                Err(error)
                    if matches!(
                        error.downcast_ref::<crate::provider_catalog::CatalogError>(),
                        Some(crate::provider_catalog::CatalogError::ProviderNotFound { .. })
                    ) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            if let Some(scope) = scope {
                for source in scope.models {
                    let source_id = source
                        .metadata
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("Provider Catalog Entry is missing id"))?;
                    catalog_sources.insert(normalize_model_id(source_id)?, source);
                }
            }
        }

        let mut sources = BTreeMap::new();
        for model in discovered.models {
            let model_id = normalize_model_id(&model.id)?;
            let catalog_source = catalog_sources.remove(&model_id);
            let (template, metadata_source_provider_id, stamp) = match catalog_source {
                Some(source) => {
                    let provider_id = source.provider_id;
                    (
                        Some(source.metadata),
                        Some(provider_id.clone()),
                        SourceStamp::ProviderCatalog { provider_id },
                    )
                }
                None => match self
                    .gw
                    .provider_catalog
                    .canonical_model_matching_upstream_id(&model_id)
                    .await
                {
                    Some(template) => {
                        let canonical_id = canonical_template_id(&template)?;
                        (
                            Some(template),
                            None,
                            SourceStamp::Canonical {
                                model_id: canonical_id,
                            },
                        )
                    }
                    None => (None, None, SourceStamp::Discovery),
                },
            };
            let metadata = metadata_from_discovered_model(&model_id, model, template)?;
            let snapshot_state = if metadata.has_specification() {
                SnapshotState::Imported { source: stamp }
            } else {
                SnapshotState::Unregistered
            };
            sources.insert(
                model_id,
                DiscoveredModelSource {
                    metadata,
                    metadata_source_provider_id,
                    snapshot_state,
                },
            );
        }
        Ok((sources, publication_guard))
    }
}

struct DiscoveredModelSource {
    metadata: ProviderModelMetadata,
    metadata_source_provider_id: Option<String>,
    snapshot_state: SnapshotState,
}

fn canonical_template_id(template: &Value) -> anyhow::Result<String> {
    template
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Canonical Model template is missing id"))
}

fn same_discovery_provider(left: &Provider, right: &Provider) -> bool {
    left.id == right.id
        && left.vendor == right.vendor
        && left.protocol == right.protocol
        && left.base_url == right.base_url
        && left.preset_key == right.preset_key
        && left.channel == right.channel
        && left.models_source == right.models_source
        && left.static_models == right.static_models
        && left.api_key == right.api_key
        && left.adapter_credentials == right.adapter_credentials
        && left.vendor_options == right.vendor_options
        && left.auth_mode == right.auth_mode
        && left.use_proxy == right.use_proxy
        && left.is_enabled == right.is_enabled
        && left.updated_at == right.updated_at
}

fn metadata_from_discovered_model(
    model_id: &str,
    mut model: stravia_vendor_sdk::DiscoveredModel,
    canonical: Option<Value>,
) -> anyhow::Result<ProviderModelMetadata> {
    let has_canonical = canonical.is_some();
    if has_canonical && model.display_name.trim() == model_id {
        model.metadata.remove("name");
    }
    let base = match canonical {
        Some(value) => value,
        None => ProviderModelMetadata::bare(model_id).to_value()?,
    };
    let Value::Object(mut object) = base else {
        anyhow::bail!("Canonical Model metadata must be an object");
    };
    object.extend(model.metadata);
    object.insert("id".into(), Value::String(model_id.to_owned()));
    let discovered_name = model.display_name.trim();
    if !has_canonical || discovered_name != model_id {
        object.insert("name".into(), Value::String(discovered_name.to_owned()));
    }
    if let Some(family) = model
        .family
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        object.insert("family".into(), Value::String(family));
    }
    if let Some(selector) = model
        .selector
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        object.insert("selector".into(), Value::String(selector));
    }
    let capabilities = model
        .capabilities
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>();
    if !capabilities.is_empty() {
        object.insert(
            "capabilities".into(),
            Value::Array(capabilities.iter().cloned().map(Value::String).collect()),
        );
    }
    if !capabilities.is_empty() {
        for (field, names) in [
            ("tool_call", ["tools", "tool_call"]),
            ("reasoning", ["reasoning", "reasoning"]),
            ("attachment", ["image_input", "image_input"]),
            (
                "structured_output",
                ["structured_output", "structured_output"],
            ),
        ] {
            if object.get(field).is_none_or(Value::is_null)
                && names.iter().any(|name| capabilities.contains(*name))
            {
                object.insert(field.into(), Value::Bool(true));
            }
        }
    }
    if capabilities.contains("image_input") {
        ensure_discovered_modality(&mut object, "input", "image")?;
    }
    if capabilities.contains("image_output") {
        ensure_discovered_modality(&mut object, "output", "image")?;
    }
    if let Some(context) = object.get("context_window").and_then(Value::as_u64) {
        let limit = object
            .entry("limit")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if limit.is_null() {
            *limit = Value::Object(serde_json::Map::new());
        }
        let limit = limit
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Discovered Model limit metadata must be an object"))?;
        if limit.get("context").is_none_or(Value::is_null) {
            limit.insert("context".into(), Value::from(context));
        }
    }
    ProviderModelMetadata::from_source_value(model_id, Value::Object(object))
}

fn ensure_discovered_modality(
    metadata: &mut serde_json::Map<String, Value>,
    direction: &str,
    modality: &str,
) -> anyhow::Result<()> {
    let modalities = metadata
        .entry("modalities")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if modalities.is_null() {
        *modalities = Value::Object(serde_json::Map::new());
    }
    let modalities = modalities
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Discovered Model modalities metadata must be an object"))?;
    let values = modalities
        .entry(direction)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("Discovered Model modality list must be an array"))?;
    if !values.iter().any(|value| value.as_str() == Some(modality)) {
        values.push(Value::String(modality.to_owned()));
    }
    Ok(())
}

fn metadata_from_canonical_template(
    model_id: &str,
    mut template: Value,
) -> anyhow::Result<ProviderModelMetadata> {
    template
        .as_object_mut()
        .expect("ProviderCatalog validates Canonical Model objects")
        .insert("id".to_string(), Value::String(model_id.to_string()));
    ProviderModelMetadata::from_source_value(model_id, template)
}

fn apply_provider_model_mutation(
    admin: &AdminService,
    mutation: ProviderModelMutation,
    provider: &Provider,
    model_id: &str,
) -> anyhow::Result<ProviderModelDetail> {
    match mutation {
        ProviderModelMutation::Applied(model) => {
            let mut detail = ProviderModelDetail::from(*model);
            super::thinking_map::hide_unwritable_generated_controls(
                admin,
                provider,
                &detail.metadata,
                &mut detail.thinking_level_map,
            );
            Ok(detail)
        }
        ProviderModelMutation::NotFound => Err(provider_model_not_found(&provider.id, model_id)),
        ProviderModelMutation::Conflict => Err(provider_model_conflict(&provider.id, model_id)),
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
