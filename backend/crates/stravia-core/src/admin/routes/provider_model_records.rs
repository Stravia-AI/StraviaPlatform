use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::provider_models::{
    CreateManualProviderModel, ModelChangeItem, ModelChangeOutcome, ModelChangePage,
    ModelChangeQuery, ModelChangeRecord, ModelChangeSource, ModelChangeType, ModelModalities,
    ModelSpecificationDiff, NewProviderModelRecord, ProviderModelDetail, ProviderModelField,
    ProviderModelMetadata, ProviderModelMutation, ProviderModelPresence,
    ProviderModelPresenceUpdate, ProviderModelReconciliation, ProviderModelRestore,
    ProviderModelSelectionPolicy, ProviderModelSourceKind, ProviderModelSummary,
    ProviderModelSyncFailure, ProviderModelSyncState, ProviderModelSyncSummary,
    RestoreProviderModel, RestoreProviderModelSync, SnapshotState, SourceStamp,
    UpdateProviderModel, UpdateProviderModelSelection, UpdateProviderModelSyncState,
    apply_provider_model_changes, change_fields, merge_source_specification, normalize_model_id,
    restore_source_fields, same_discovery_provider,
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
        RouteModule::new(&self.gw)
            .list_provider_models(provider_id)
            .await
    }

    pub async fn get_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<ProviderModelDetail> {
        RouteModule::new(&self.gw)
            .get_provider_model(provider_id, model_id)
            .await
    }

    pub async fn prepare_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        template_id: Option<&str>,
    ) -> anyhow::Result<PreparedProviderModel> {
        RouteModule::new(&self.gw)
            .prepare_provider_model(provider_id, model_id, template_id)
            .await
    }

    pub async fn create_manual_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        input: CreateManualProviderModel,
    ) -> anyhow::Result<ProviderModelDetail> {
        RouteModule::new(&self.gw)
            .add_provider_model(provider_id, model_id, input)
            .await
    }

    pub async fn update_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
        input: UpdateProviderModel,
    ) -> anyhow::Result<ProviderModelDetail> {
        RouteModule::new(&self.gw)
            .update_provider_model(provider_id, model_id, input)
            .await
    }

    pub async fn update_provider_model_selection(
        &self,
        provider_id: &str,
        model_id: &str,
        input: UpdateProviderModelSelection,
    ) -> anyhow::Result<ProviderModelDetail> {
        RouteModule::new(&self.gw)
            .update_provider_model_selection(provider_id, model_id, input)
            .await
    }

    pub async fn restore_provider_model_sync(
        &self,
        provider_id: &str,
        model_id: &str,
        input: RestoreProviderModelSync,
    ) -> anyhow::Result<ProviderModelDetail> {
        RouteModule::new(&self.gw)
            .restore_provider_model_sync(provider_id, model_id, input)
            .await
    }

    pub async fn delete_manual_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<()> {
        RouteModule::new(&self.gw)
            .delete_manual_provider_model(provider_id, model_id)
            .await
    }

    pub async fn sync_provider_models(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<ProviderModelSyncSummary> {
        self.gw
            .sync_provider_inventory(provider_id, ModelChangeSource::ManualSync)
            .await
    }

    pub async fn provider_model_sync_state(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<ProviderModelSyncState> {
        let provider = RouteModule::new(&self.gw).get_provider(provider_id).await?;
        let mut state = self
            .gw
            .storage
            .provider_models()
            .sync_state(provider_id)
            .await?;
        state.in_progress = self.gw.provider_model_sync.in_progress(provider_id);
        state.pause_reason = self
            .gw
            .provider_model_pause_reason(&provider, state.automatic_sync);
        Ok(state)
    }

    pub async fn update_provider_model_sync_state(
        &self,
        provider_id: &str,
        input: UpdateProviderModelSyncState,
    ) -> anyhow::Result<ProviderModelSyncState> {
        RouteModule::new(&self.gw).get_provider(provider_id).await?;
        self.gw
            .storage
            .provider_models()
            .set_automatic_sync(provider_id, input.automatic_sync)
            .await?;
        self.provider_model_sync_state(provider_id).await
    }

    pub async fn list_model_changes(
        &self,
        query: ModelChangeQuery,
    ) -> anyhow::Result<ModelChangePage> {
        self.gw.storage.provider_models().list_changes(query).await
    }

    pub async fn get_model_change(&self, id: &str) -> anyhow::Result<ModelChangeRecord> {
        self.gw
            .storage
            .provider_models()
            .get_change(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Model change record not found"))
    }
}

impl RouteModule<'_> {
    pub(crate) async fn list_provider_models(
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

    pub(crate) async fn get_provider_model(
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
            self.gw,
            &provider,
            &detail.metadata,
            &mut detail.thinking_level_map,
        );
        Ok(detail)
    }

    pub(crate) async fn prepare_provider_model(
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
        let (mut metadata, snapshot_state) = match template_id {
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
                        pinned: std::collections::BTreeSet::new(),
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
                            pinned: std::collections::BTreeSet::new(),
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
        metadata
            .modalities
            .get_or_insert_with(ModelModalities::text_only);
        Ok(PreparedProviderModel {
            id: model_id,
            snapshot_state,
            metadata,
            extensions,
        })
    }

    pub(crate) async fn add_provider_model(
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
        let _permit = self.gw.vendor_plugins.write_permit(vendor).await?;
        let model_id = normalize_model_id(model_id)?;
        let metadata = ProviderModelMetadata::from_value(&model_id, input.metadata)?;
        self.ensure_cost_currency(&metadata).await?;
        // 模板选择仍须有效，但手动规格不保留可恢复的同步来源。
        if let Some(template_id) = input.template_id {
            self.gw
                .provider_catalog
                .canonical_model(&template_id)
                .await?;
        }
        let change = manual_record(
            &provider,
            change_item(
                &model_id,
                &metadata,
                None,
                ProviderModelPresence::Present,
                ProviderModelSelectionPolicy::Auto,
                vec![ModelChangeType::ManualAdded],
                vec![],
            ),
        );
        apply_provider_model_mutation(
            self.gw,
            self.gw
                .storage
                .provider_models()
                .create(
                    NewProviderModelRecord {
                        provider_id: provider_id.to_string(),
                        model_id: model_id.clone(),
                        source_kind: ProviderModelSourceKind::Manual,
                        snapshot_state: SnapshotState::Local,
                        source_metadata: None,
                        metadata_source_provider_id: None,
                        presence: ProviderModelPresence::Present,
                        selection_policy: ProviderModelSelectionPolicy::Auto,
                        metadata,
                    },
                    Some(change),
                )
                .await?,
            &provider,
            &model_id,
        )
    }

    pub(crate) async fn update_provider_model(
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
        let _permit = self.gw.vendor_plugins.write_permit(vendor).await?;
        let model_id = normalize_model_id(model_id)?;
        let existing = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .ok_or_else(|| provider_model_not_found(provider_id, &model_id))?;
        let changed_fields = change_fields(&input.changes).map_err(invalid_changes)?;
        if changed_fields.is_empty() && !input.changes.contains_key("cost.peak") {
            return self.get_provider_model(provider_id, &model_id).await;
        }
        let metadata = apply_provider_model_changes(&existing.metadata, &input.changes)
            .map_err(invalid_changes)?;
        self.ensure_cost_currency(&metadata).await?;
        let snapshot_state = match &existing.snapshot_state {
            SnapshotState::Imported { source, pinned } => {
                let mut pinned = pinned.clone();
                pinned.extend(changed_fields);
                SnapshotState::Imported {
                    source: source.clone(),
                    pinned,
                }
            }
            SnapshotState::Unregistered if !changed_fields.is_empty() => SnapshotState::Imported {
                source: SourceStamp::Discovery,
                pinned: changed_fields,
            },
            state => state.clone(),
        };
        apply_provider_model_mutation(
            self.gw,
            self.gw
                .storage
                .provider_models()
                .update_metadata(
                    provider_id,
                    &model_id,
                    metadata,
                    snapshot_state,
                    input.revision,
                )
                .await?,
            &provider,
            &model_id,
        )
    }

    pub(crate) async fn update_provider_model_selection(
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
        let _permit = self.gw.vendor_plugins.write_permit(vendor).await?;
        let model_id = normalize_model_id(model_id)?;
        let current = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .ok_or_else(|| provider_model_not_found(provider_id, &model_id))?;
        let change = (current.selection_policy != input.policy).then(|| {
            manual_record(
                &provider,
                change_item(
                    &model_id,
                    &current.metadata,
                    Some(&current),
                    current.presence,
                    input.policy,
                    vec![ModelChangeType::SelectionPolicyChanged],
                    vec![],
                ),
            )
        });
        apply_provider_model_mutation(
            self.gw,
            self.gw
                .storage
                .provider_models()
                .update_selection_policy(
                    provider_id,
                    &model_id,
                    input.policy,
                    input.revision,
                    change,
                )
                .await?,
            &provider,
            &model_id,
        )
    }

    /// ADR-0084：把来源规格写回选定的已固定字段并取消固定，离线执行。
    /// 目录读取、来源值与 Generated Map 的可写性处理在事务前完成；事务读取
    /// 提交时的最新 Route/Target，仅重算 Generated 行并保留 Overridden。
    /// `model_cache` 写锁跨存储调用持有，`before_commit` 在最终提交前复检
    /// Vendor 写许可。
    pub(crate) async fn restore_provider_model_sync(
        &self,
        provider_id: &str,
        model_id: &str,
        input: RestoreProviderModelSync,
    ) -> anyhow::Result<ProviderModelDetail> {
        let provider = self.get_provider(provider_id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let permit = self.gw.vendor_plugins.write_permit(vendor).await?;
        let model_id = normalize_model_id(model_id)?;
        let existing = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, &model_id)
            .await?
            .ok_or_else(|| provider_model_not_found(provider_id, &model_id))?;
        let SnapshotState::Imported { source, pinned } = &existing.snapshot_state else {
            return Err(provider_model_source_unavailable(provider_id, &model_id));
        };
        let source_metadata = existing
            .source_metadata
            .as_ref()
            .ok_or_else(|| provider_model_source_unavailable(provider_id, &model_id))?;
        let mut fields = Vec::new();
        for field in &input.fields {
            let field = ProviderModelField::parse(field)
                .ok_or_else(|| invalid_restore_fields(&input.fields))?;
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
        if fields.is_empty() {
            return self.get_provider_model(provider_id, &model_id).await;
        }
        let unpinned = fields
            .iter()
            .copied()
            .filter(|field| !pinned.contains(field))
            .collect::<Vec<_>>();
        if !unpinned.is_empty() {
            return Err(provider_model_field_not_pinned(
                provider_id,
                &model_id,
                &unpinned,
            ));
        }
        let metadata = restore_source_fields(&existing.metadata, source_metadata, &fields)?;
        let mut pinned = pinned.clone();
        for field in &fields {
            pinned.remove(field);
        }
        let snapshot_state = SnapshotState::Imported {
            source: source.clone(),
            pinned,
        };
        let generated = (fields.contains(&ProviderModelField::ReasoningEfforts)
            && metadata.reasoning_efforts != existing.metadata.reasoning_efforts)
            .then(|| {
                let mut generated = generate_thinking_level_map(&metadata);
                super::thinking_map::hide_unwritable_generated_controls(
                    self.gw,
                    &provider,
                    &metadata,
                    &mut generated,
                );
                generated
            });

        let mut cache = self.gw.model_cache.write().await;
        let validate_map =
            |model_id: &str,
             metadata: &ProviderModelMetadata,
             map: &[crate::thinking::ThinkingLevelMapping]| {
                super::thinking_map::ensure_thinking_map_representable(
                    self.gw, &provider, model_id, metadata, map,
                )
            };
        let before_commit = || permit.ensure_current();
        let outcome = self
            .gw
            .storage
            .provider_models()
            .restore(
                provider_id,
                &model_id,
                RestoreProviderModel {
                    metadata,
                    snapshot_state,
                    expected_revision: input.revision,
                    generated_thinking_level_map: generated,
                },
                &validate_map,
                &before_commit,
            )
            .await?;
        let mut detail = match outcome {
            ProviderModelRestore::Applied {
                model,
                active_routes,
            } => {
                *cache = crate::router::RouteCache {
                    models: active_routes,
                };
                self.gw.route_policy_state.clear_pricing();
                ProviderModelDetail::from(*model)
            }
            ProviderModelRestore::NotFound => {
                return Err(provider_model_not_found(provider_id, &model_id));
            }
            ProviderModelRestore::Conflict => {
                return Err(provider_model_conflict(provider_id, &model_id));
            }
        };
        super::thinking_map::hide_unwritable_generated_controls(
            self.gw,
            &provider,
            &detail.metadata,
            &mut detail.thinking_level_map,
        );
        drop(cache);
        drop(permit);
        Ok(detail)
    }

    pub(crate) async fn delete_manual_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<()> {
        let provider = self.get_provider(provider_id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let _permit = self.gw.vendor_plugins.write_permit(vendor).await?;
        let model_id = normalize_model_id(model_id)?;
        if self
            .gw
            .storage
            .provider_models()
            .delete_manual(provider_id, &model_id)
            .await?
        {
            self.gw.route_policy_state.clear_pricing();
            Ok(())
        } else {
            Err(provider_model_not_found(provider_id, &model_id))
        }
    }

    pub(crate) async fn sync(
        &self,
        provider_id: &str,
        origin: ModelChangeSource,
    ) -> anyhow::Result<ProviderModelSyncSummary> {
        let provider = self.get_provider(provider_id).await?;
        let mut summary = empty_summary(origin);
        let result = self.reconcile_sync(&provider, &mut summary).await;
        match result {
            Ok(()) => Ok(summary),
            Err(error) => {
                summary = empty_summary(origin);
                summary.outcome = ModelChangeOutcome::Failure;
                summary.error = Some(safe_sync_error(&error));
                let record = sync_record(&provider, &summary, false, vec![]);
                summary.record_id = Some(record.id.clone());
                if let Err(storage_error) = self
                    .gw
                    .storage
                    .provider_models()
                    .save_sync_failure(provider_id, summary.clone(), record)
                    .await
                {
                    tracing::warn!(provider_id, error = ?storage_error, "model sync failure history could not be saved");
                    summary.record_id = None;
                    summary.error = Some(format!(
                        "{}; check history could not be saved",
                        summary.error.as_deref().unwrap_or("Synchronization failed")
                    ));
                }
                Err(error.context(ProviderModelSyncFailure { summary }))
            }
        }
    }

    async fn reconcile_sync(
        &self,
        provider: &Provider,
        summary: &mut ProviderModelSyncSummary,
    ) -> anyhow::Result<()> {
        let provider_id = provider.id.as_str();
        let existing = self
            .gw
            .storage
            .provider_models()
            .list_for_provider(provider_id)
            .await?;
        let state = self
            .gw
            .storage
            .provider_models()
            .sync_state(provider_id)
            .await?;
        let (sources, _publication_guard) = self.discover_provider_model_sources(provider).await?;
        anyhow::ensure!(
            !sources.is_empty(),
            "Provider model discovery returned an empty list"
        );
        let existing_by_id: BTreeMap<_, _> = existing
            .iter()
            .map(|model| (model.model_id.as_str(), model))
            .collect();
        let seen_ids: BTreeSet<String> = sources.keys().cloned().collect();
        let mut reconciliation = ProviderModelReconciliation {
            expected_provider: Some(provider.clone()),
            expected_revisions: existing
                .iter()
                .map(|model| (model.model_id.clone(), model.revision))
                .collect(),
            ..Default::default()
        };
        let mut changes = Vec::new();

        for (model_id, source) in sources {
            let mut metadata = source.metadata;
            let incoming_state = source.snapshot_state;
            if let Some(current) = existing_by_id.get(model_id.as_str()) {
                if current.source_kind == ProviderModelSourceKind::Manual {
                    continue;
                }
                // Missing optional enrichment is not a lifecycle declaration.
                // A Discover status (including explicit null) remains authoritative.
                if source.catalog_metadata_unavailable && !source.discovery_declares_lifecycle {
                    metadata.status = current.metadata.status.clone();
                }
                let mut types = Vec::new();
                if current.presence == ProviderModelPresence::Missing {
                    summary.restored += 1;
                    types.push(ModelChangeType::Restored);
                }
                if current.metadata.status.as_deref() != Some("deprecated")
                    && metadata.status.as_deref() == Some("deprecated")
                {
                    summary.deprecated += 1;
                    types.push(ModelChangeType::Deprecated);
                }
                if current.metadata.status.as_deref() == Some("deprecated")
                    && metadata.status.as_deref() != Some("deprecated")
                {
                    summary.deprecation_cleared += 1;
                    types.push(ModelChangeType::DeprecationCleared);
                }
                // 上游声明的规格决定未固定字段；已固定字段沿用生效规格。
                // Failed catalog enrichment is not evidence that its last valid
                // specification disappeared or changed to a Canonical fallback.
                let retain_catalog_specification = source.catalog_metadata_unavailable
                    && matches!(
                        current.snapshot_state.source(),
                        Some(SourceStamp::ProviderCatalog { .. })
                    );
                let metadata_source_provider_id = if retain_catalog_specification {
                    current.metadata_source_provider_id.clone()
                } else {
                    source.metadata_source_provider_id
                };
                let incoming_has_specification =
                    matches!(incoming_state, SnapshotState::Imported { .. });
                let follows_upstream = !retain_catalog_specification
                    && (incoming_has_specification
                        || matches!(current.snapshot_state, SnapshotState::Unregistered));
                let mut merged = if follows_upstream {
                    merge_source_specification(
                        &current.metadata,
                        &metadata,
                        current.snapshot_state.pinned(),
                    )?
                } else {
                    current.metadata.clone()
                };
                if !retain_catalog_specification
                    && !current
                        .snapshot_state
                        .pinned()
                        .contains(&ProviderModelField::Name)
                {
                    merged.name = metadata.name.clone();
                }
                merged.status = metadata.status.clone();
                merged.provider = metadata.provider.clone();
                merged.experimental = metadata.experimental.clone();
                merged.extensions = metadata.extensions.clone();
                let metadata_changed = merged != current.metadata;
                let source_metadata = (!retain_catalog_specification && incoming_has_specification)
                    .then(|| metadata.clone());
                let source_metadata_changed = source_metadata
                    .as_ref()
                    .is_some_and(|source| current.source_metadata.as_ref() != Some(source));
                let snapshot_state = if !retain_catalog_specification && incoming_has_specification
                {
                    match &incoming_state {
                        SnapshotState::Imported { source, .. } => Some(SnapshotState::Imported {
                            source: source.clone(),
                            pinned: current.snapshot_state.pinned().clone(),
                        }),
                        _ => None,
                    }
                } else {
                    None
                };
                let snapshot_changed = snapshot_state
                    .as_ref()
                    .is_some_and(|state| state != &current.snapshot_state);
                let generated_thinking_level_map = (metadata_changed
                    && current.metadata.reasoning_efforts != merged.reasoning_efforts)
                    .then(|| {
                        let mut generated = generate_thinking_level_map(&merged);
                        super::thinking_map::hide_unwritable_generated_controls(
                            self.gw,
                            provider,
                            &merged,
                            &mut generated,
                        );
                        generated
                    });
                let specification = specification_diff(&current.metadata, &merged)?;
                if !specification.is_empty() {
                    summary.specification_updated += 1;
                    types.push(ModelChangeType::SpecificationUpdated);
                }
                if !types.is_empty() {
                    changes.push(change_item(
                        &model_id,
                        &merged,
                        Some(current),
                        ProviderModelPresence::Present,
                        current.selection_policy,
                        types,
                        specification,
                    ));
                }
                if current.presence != ProviderModelPresence::Present
                    || current.metadata.status != metadata.status
                    || current.metadata_source_provider_id != metadata_source_provider_id
                    || metadata_changed
                    || source_metadata_changed
                    || snapshot_changed
                {
                    reconciliation.updates.push(ProviderModelPresenceUpdate {
                        model_id,
                        expected_revision: current.revision,
                        snapshot_state,
                        metadata_source_provider_id,
                        presence: ProviderModelPresence::Present,
                        lifecycle_status: metadata.status.clone(),
                        metadata: metadata_changed.then_some(merged),
                        source_metadata: source_metadata.filter(|_| source_metadata_changed),
                        generated_thinking_level_map,
                    });
                }
                continue;
            }
            summary.added += 1;
            let mut types = vec![ModelChangeType::Added];
            if metadata.status.as_deref() == Some("deprecated") {
                summary.deprecated += 1;
                types.push(ModelChangeType::Deprecated);
            }
            changes.push(change_item(
                &model_id,
                &metadata,
                None,
                ProviderModelPresence::Present,
                ProviderModelSelectionPolicy::Auto,
                types,
                vec![],
            ));
            let source_metadata =
                matches!(incoming_state, SnapshotState::Imported { .. }).then(|| metadata.clone());
            reconciliation.inserts.push(NewProviderModelRecord {
                provider_id: provider_id.to_string(),
                model_id,
                source_kind: ProviderModelSourceKind::Discovered,
                snapshot_state: incoming_state,
                source_metadata,
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
                changes.push(change_item(
                    &current.model_id,
                    &current.metadata,
                    Some(current),
                    ProviderModelPresence::Missing,
                    current.selection_policy,
                    vec![ModelChangeType::Missing],
                    vec![],
                ));
                reconciliation.updates.push(ProviderModelPresenceUpdate {
                    model_id: current.model_id.clone(),
                    expected_revision: current.revision,
                    snapshot_state: None,
                    metadata_source_provider_id: current.metadata_source_provider_id.clone(),
                    presence: ProviderModelPresence::Missing,
                    lifecycle_status: current.metadata.status.clone(),
                    metadata: None,
                    source_metadata: None,
                    generated_thinking_level_map: None,
                });
            }
        }

        let latest_provider = self.get_provider(provider_id).await?;
        anyhow::ensure!(
            same_discovery_provider(provider, &latest_provider),
            "Provider changed while synchronizing discovered models"
        );
        let reconciled = !reconciliation.updates.is_empty() || !reconciliation.inserts.is_empty();
        summary.checked_at = checked_at();
        if !changes.is_empty() {
            let record = sync_record(provider, summary, !state.has_successful_sync, changes);
            summary.record_id = Some(record.id.clone());
            reconciliation.change_record = Some(record);
        }
        reconciliation.sync_summary = Some(summary.clone());
        let committed_record = reconciliation.change_record.clone();
        let validate_map =
            |model_id: &str,
             metadata: &ProviderModelMetadata,
             map: &[crate::thinking::ThinkingLevelMapping]| {
                super::thinking_map::ensure_thinking_map_representable(
                    self.gw, provider, model_id, metadata, map,
                )
            };
        self.gw
            .storage
            .provider_models()
            .apply_reconciliation(provider_id, reconciliation, &validate_map)
            .await?;
        if reconciled {
            self.gw.route_policy_state.clear_pricing();
        }
        if let Err(error) = self
            .gw
            .vendor_plugins
            .store
            .recovered(provider_id, "models")
            .await
        {
            summary.outcome = ModelChangeOutcome::PartialSuccess;
            tracing::warn!(provider_id, error = ?error, "model inventory committed but plugin recovery failed");
            summary.error = Some("Inventory saved; plugin recovery status could not be updated; check provider status".into());
            let had_committed_record = committed_record.is_some();
            let mut record = committed_record.unwrap_or_else(|| {
                sync_record(provider, summary, !state.has_successful_sync, vec![])
            });
            record.outcome = summary.outcome;
            record.error = summary.error.clone();
            if summary.record_id.is_none() {
                summary.record_id = Some(record.id.clone());
            }
            if let Err(storage_error) = self
                .gw
                .storage
                .provider_models()
                .save_sync_failure(provider_id, summary.clone(), record)
                .await
            {
                tracing::warn!(provider_id, error = ?storage_error, "postcommit model sync status could not be saved");
                if !had_committed_record {
                    summary.record_id = None;
                }
                summary.error = Some(format!(
                    "{}; postcommit status could not be saved",
                    summary.error.as_deref().unwrap_or("Inventory saved")
                ));
            }
        }
        Ok(())
    }

    async fn discover_provider_model_sources(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<(
        BTreeMap<String, DiscoveredModelSource>,
        tokio::sync::OwnedRwLockReadGuard<()>,
    )> {
        let discovered =
            super::model_discovery::discover_provider_models(self.gw, &provider.id).await?;
        // Hold the vendor publication fence until the single Provider Model
        // reconciliation has committed.
        let publication_guard = discovered.write_fence().await?;
        let mut catalog_sources = BTreeMap::new();
        let mut catalog_metadata_unavailable = false;
        if let Some(catalog_provider_id) = provider
            .preset_key
            .as_deref()
            .or(discovered.catalog_id.as_deref())
        {
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
                Err(error)
                    if matches!(
                        error.downcast_ref::<crate::provider_catalog::CatalogError>(),
                        Some(crate::provider_catalog::CatalogError::ScopeRefresh { .. })
                    ) =>
                {
                    // Discover already established the complete inventory. ADR-0073
                    // permits Canonical/bare metadata when optional scoped enrichment
                    // fails; this must not turn a valid upstream discovery into failure
                    // or mark the catalog refresh itself successful.
                    tracing::warn!(
                        provider_id = %provider.id,
                        "Provider Catalog metadata refresh failed; retaining discovered inventory and using Canonical or unregistered metadata"
                    );
                    catalog_metadata_unavailable = true;
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
            let discovery_declares_lifecycle = model.metadata.contains_key("status");
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
                SnapshotState::Imported {
                    source: stamp,
                    pinned: BTreeSet::new(),
                }
            } else {
                SnapshotState::Unregistered
            };
            sources.insert(
                model_id,
                DiscoveredModelSource {
                    metadata,
                    metadata_source_provider_id,
                    snapshot_state,
                    catalog_metadata_unavailable,
                    discovery_declares_lifecycle,
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
    catalog_metadata_unavailable: bool,
    discovery_declares_lifecycle: bool,
}

fn canonical_template_id(template: &Value) -> anyhow::Result<String> {
    template
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Canonical Model template is missing id"))
}

fn checked_at() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn empty_summary(source: ModelChangeSource) -> ProviderModelSyncSummary {
    ProviderModelSyncSummary {
        added: 0,
        missing: 0,
        restored: 0,
        deprecated: 0,
        checked_at: checked_at(),
        source,
        record_id: None,
        deprecation_cleared: 0,
        specification_updated: 0,
        outcome: ModelChangeOutcome::Success,
        error: None,
    }
}

fn safe_sync_error(error: &anyhow::Error) -> String {
    // Upstream diagnostics can contain arbitrary response bodies and credentials.
    // Persist only a bounded category, never plugin/network error text.
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("changed") || text.contains("conflict") || text.contains("revision") {
        "Provider or model changed during discovery; check current settings and sync again".into()
    } else if text.contains("empty")
        || text.contains("invalid")
        || text.contains("duplicate")
        || text.contains("pagination")
    {
        "Discovery did not return a complete valid inventory; previous inventory retained".into()
    } else if text.contains("credential") || text.contains("auth") {
        "Discovery could not authenticate; check provider credentials".into()
    } else if text.contains("cancel") {
        "Model discovery was cancelled".into()
    } else {
        "Model discovery or persistence failed; check provider settings and try again".into()
    }
}

fn change_item(
    model_id: &str,
    metadata: &ProviderModelMetadata,
    before: Option<&crate::provider_models::ProviderModelRecord>,
    presence: ProviderModelPresence,
    policy: ProviderModelSelectionPolicy,
    types: Vec<ModelChangeType>,
    specification: Vec<ModelSpecificationDiff>,
) -> ModelChangeItem {
    ModelChangeItem {
        model_id: model_id.into(),
        name: metadata.name.clone().unwrap_or_else(|| model_id.into()),
        types,
        specification,
        before_presence: before.map(|m| m.presence),
        after_presence: Some(presence),
        before_deprecated: before.map(|m| m.metadata.lifecycle_status() == Some("deprecated")),
        after_deprecated: Some(metadata.lifecycle_status() == Some("deprecated")),
        before_policy: before.map(|m| m.selection_policy),
        after_policy: Some(policy),
    }
}

fn sync_record(
    provider: &Provider,
    summary: &ProviderModelSyncSummary,
    initial_sync: bool,
    changes: Vec<ModelChangeItem>,
) -> ModelChangeRecord {
    ModelChangeRecord {
        id: stravia_runtime_contract::identifier::new_id(),
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        provider_deleted: false,
        source: summary.source,
        checked_at: summary.checked_at.clone(),
        outcome: summary.outcome,
        initial_sync,
        changes,
        error: summary.error.clone(),
    }
}

fn manual_record(provider: &Provider, item: ModelChangeItem) -> ModelChangeRecord {
    sync_record(
        provider,
        &empty_summary(ModelChangeSource::ManualChange),
        false,
        vec![item],
    )
}

fn specification_diff(
    before: &ProviderModelMetadata,
    after: &ProviderModelMetadata,
) -> anyhow::Result<Vec<ModelSpecificationDiff>> {
    fn facts(metadata: &ProviderModelMetadata) -> anyhow::Result<[Value; 6]> {
        Ok([
            serde_json::to_value(&metadata.name)?,
            serde_json::to_value(metadata.limit.as_ref().and_then(|limit| limit.context))?,
            serde_json::to_value(metadata.modalities.as_ref().map(|m| &m.input))?,
            serde_json::to_value(metadata.modalities.as_ref().map(|m| &m.output))?,
            serde_json::to_value(&metadata.reasoning_efforts)?,
            serde_json::to_value(&metadata.cost)?,
        ])
    }
    let before = facts(before)?;
    let after = facts(after)?;
    Ok([
        "display_name",
        "context",
        "input_modalities",
        "output_modalities",
        "reasoning_efforts",
        "price",
    ]
    .into_iter()
    .zip(before.into_iter().zip(after))
    .filter_map(|(field, (before, after))| {
        (before != after).then(|| ModelSpecificationDiff {
            field: field.into(),
            before,
            after,
        })
    })
    .collect())
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
    if capabilities.contains("image_input") {
        ensure_discovered_modality(&mut object, "input", "image")?;
    }
    if capabilities.contains("image_output") {
        ensure_discovered_modality(&mut object, "output", "image")?;
    }
    // 上游直接声明的 context window 覆盖目录默认窗口。
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
        limit.insert("context".into(), Value::from(context));
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
    gw: &Gateway,
    mutation: ProviderModelMutation,
    provider: &Provider,
    model_id: &str,
) -> anyhow::Result<ProviderModelDetail> {
    match mutation {
        ProviderModelMutation::Applied(model) => {
            // The write committed: drop this Gateway's cached prices before
            // returning so the next selection sees the new values.
            gw.route_policy_state.clear_pricing();
            let mut detail = ProviderModelDetail::from(*model);
            super::thinking_map::hide_unwritable_generated_controls(
                gw,
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

impl RouteModule<'_> {
    async fn ensure_cost_currency(&self, metadata: &ProviderModelMetadata) -> anyhow::Result<()> {
        let Some(cost) = &metadata.cost else {
            return Ok(());
        };
        if cost.custom_currency_fields.is_empty() {
            return Ok(());
        }
        let currency = self.gw.storage.money().custom_currency().await?;
        cost.validate_currency(currency.as_ref()).map_err(|error| {
            coded_error(
                "PROVIDER_MODEL_CUSTOM_CURRENCY_REQUIRED",
                "Provider Model cost currency is invalid",
                serde_json::json!({ "error": error.to_string() }),
            )
        })
    }
}

fn invalid_changes(error: anyhow::Error) -> anyhow::Error {
    coded_error(
        "PROVIDER_MODEL_INVALID_CHANGES",
        "Provider Model changes are invalid",
        serde_json::json!({ "error": error.to_string() }),
    )
}

fn invalid_restore_fields(fields: &[String]) -> anyhow::Error {
    coded_error(
        "PROVIDER_MODEL_INVALID_CHANGES",
        "Provider Model restore fields are invalid",
        serde_json::json!({ "fields": fields }),
    )
}

fn provider_model_source_unavailable(provider_id: &str, model_id: &str) -> anyhow::Error {
    coded_error(
        "PROVIDER_MODEL_SOURCE_UNAVAILABLE",
        "Provider Model has no synchronized source specification",
        serde_json::json!({ "provider_id": provider_id, "model_id": model_id }),
    )
}

fn provider_model_field_not_pinned(
    provider_id: &str,
    model_id: &str,
    fields: &[ProviderModelField],
) -> anyhow::Error {
    coded_error(
        "PROVIDER_MODEL_FIELD_NOT_PINNED",
        "Provider Model fields are not pinned",
        serde_json::json!({
            "provider_id": provider_id,
            "model_id": model_id,
            "fields": fields.iter().map(|field| field.as_str()).collect::<Vec<_>>(),
        }),
    )
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
