use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::Context;
use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Sqlite, SqliteConnection, Transaction};

use super::SqliteStorage;
use super::routes::SqliteRouteStore;
use crate::provider_models::{
    ModelChangePage, ModelChangeQuery, ModelChangeRecord, NewProviderModelRecord, PriceComponents,
    ProviderModelCostRule, ProviderModelCostRuleKind, ProviderModelMetadata, ProviderModelMutation,
    ProviderModelPresence, ProviderModelPresenceUpdate, ProviderModelReconciliation,
    ProviderModelRecord, ProviderModelRestore, ProviderModelSelectionPolicy,
    ProviderModelSourceKind, ProviderModelSyncState, ProviderModelSyncSummary,
    RestoreProviderModel, SnapshotState,
};
use crate::storage::CONFIG_EPOCH_KEY;
use crate::storage::traits::ProviderModelStore;

#[derive(sqlx::FromRow)]
struct ProviderModelRow {
    provider_id: String,
    model_id: String,
    source_kind: String,
    snapshot_state: String,
    source_metadata_json: Option<String>,
    metadata_source_provider_id: Option<String>,
    presence: String,
    selection_policy: String,
    metadata_json: String,
    revision: i64,
    created_at: String,
    updated_at: String,
}

#[derive(sqlx::FromRow)]
struct CostRuleRow {
    provider_id: String,
    model_id: String,
    rule_index: i64,
    rule_kind: String,
    threshold_tokens: i64,
    cost_input: Option<String>,
    cost_output: Option<String>,
    cost_cache_read: Option<String>,
    cost_cache_write: Option<String>,
}

#[async_trait]
impl ProviderModelStore for SqliteStorage {
    async fn list_for_provider(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderModelRecord>> {
        let mut connection = self.pool.acquire().await?;
        let rows = sqlx::query_as::<_, ProviderModelRow>(
    r#"SELECT provider_id, model_id, source_kind, snapshot_state, source_metadata_json, metadata_source_provider_id,
              presence, selection_policy, metadata_json, revision,
              created_at, updated_at
       FROM provider_models
       WHERE provider_id = ?
       ORDER BY COALESCE(name, model_id) COLLATE NOCASE, model_id"#,
            )
            .bind(provider_id)
            .fetch_all(&mut *connection)
            .await?;
        let rules = load_rules_for_provider(&mut connection, provider_id).await?;
        rows.into_iter()
            .map(|row| {
                let cost_rules = rules.get(&row.model_id).cloned().unwrap_or_default();
                decode_record(row, cost_rules)
            })
            .collect()
    }

    async fn get(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<Option<ProviderModelRecord>> {
        let mut connection = self.pool.acquire().await?;
        get_record(&mut connection, provider_id, model_id).await
    }

    async fn apply_reconciliation(
        &self,
        provider_id: &str,
        reconciliation: ProviderModelReconciliation,
        validate_map: &crate::provider_models::ProviderModelThinkingMapValidator<'_>,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(expected) = &reconciliation.expected_provider {
            let provider = sqlx::query_as::<_, crate::db::models::Provider>(
                "SELECT * FROM providers WHERE id = ?",
            )
            .bind(provider_id)
            .fetch_optional(&mut *tx)
            .await?;
            anyhow::ensure!(
                provider.as_ref().is_some_and(|provider| {
                    crate::provider_models::same_discovery_provider(provider, expected)
                }),
                "Provider changed while synchronizing discovered models"
            );
        }
        for (model_id, revision) in &reconciliation.expected_revisions {
            let current = sqlx::query_scalar::<_, i64>(
                "SELECT revision FROM provider_models WHERE provider_id = ? AND model_id = ?",
            )
            .bind(provider_id)
            .bind(model_id)
            .fetch_optional(&mut *tx)
            .await?;
            anyhow::ensure!(
                current == Some(*revision),
                "Provider Model has changed while synchronizing discovered models"
            );
        }
        let has_writes = !reconciliation.updates.is_empty() || !reconciliation.inserts.is_empty();
        for update in &reconciliation.updates {
            let revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM provider_models WHERE provider_id = ? AND model_id = ? AND source_kind = 'discovered'",
                    )
                    .bind(provider_id)
                    .bind(&update.model_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            anyhow::ensure!(
                revision == Some(update.expected_revision),
                "Provider Model has changed while synchronizing discovered models"
            );
        }
        for input in &reconciliation.inserts {
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM provider_models WHERE provider_id = ? AND model_id = ?",
            )
            .bind(provider_id)
            .bind(&input.model_id)
            .fetch_one(&mut *tx)
            .await?;
            anyhow::ensure!(
                exists == 0,
                "Provider Model has changed while synchronizing discovered models"
            );
        }
        for update in reconciliation.updates {
            apply_discovered_metadata_update(&mut tx, provider_id, &update).await?;
            if let Some(generated) = update.generated_thinking_level_map.as_deref() {
                let metadata = update
                    .metadata
                    .as_ref()
                    .context("Provider Model thinking map update is missing metadata")?;
                SqliteRouteStore::refresh_generated_thinking_maps(
                    &mut tx,
                    provider_id,
                    &update.model_id,
                    metadata,
                    generated,
                    validate_map,
                )
                .await?;
            }
        }
        for input in reconciliation.inserts {
            insert_record(&mut tx, input).await?;
        }
        if let Some(summary) = reconciliation.sync_summary {
            save_summary(&mut tx, provider_id, &summary, true).await?;
        }
        if let Some(record) = reconciliation.change_record {
            insert_change(&mut tx, &record).await?;
        }
        // Presence/metadata changes alter model resolution and prices; notify
        // other replicas in the same commit. An empty reconciliation writes
        // nothing and must not publish a spurious epoch.
        if has_writes {
            bump_config_epoch(&mut tx).await?;
        }
        tx.commit().await?;
        connection.finish().await?;
        Ok(())
    }

    async fn create(
        &self,
        input: NewProviderModelRecord,
        change_record: Option<ModelChangeRecord>,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM provider_models WHERE provider_id = ? AND model_id = ?",
        )
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .fetch_one(&mut *tx)
        .await?
            > 0;
        if exists {
            tx.rollback().await?;
            connection.finish().await?;
            return Ok(ProviderModelMutation::Conflict);
        }
        let provider_id = input.provider_id.clone();
        let model_id = input.model_id.clone();
        insert_record(&mut tx, input).await?;
        if let Some(change) = change_record {
            insert_change(&mut tx, &change).await?;
        }
        bump_config_epoch(&mut tx).await?;
        let record = get_record(&mut tx, &provider_id, &model_id)
            .await?
            .context("created Provider Model not found")?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(ProviderModelMutation::Applied(Box::new(record)))
    }

    async fn update_metadata(
        &self,
        provider_id: &str,
        model_id: &str,
        metadata: ProviderModelMetadata,
        snapshot_state: SnapshotState,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN").await?;
        let result = update_record_metadata(
            &mut tx,
            provider_id,
            model_id,
            &metadata,
            &snapshot_state,
            expected_revision,
        )
        .await?;
        if !result {
            let exists = model_exists(&mut tx, provider_id, model_id).await?;
            tx.rollback().await?;
            connection.finish().await?;
            return Ok(if exists {
                ProviderModelMutation::Conflict
            } else {
                ProviderModelMutation::NotFound
            });
        }
        replace_cost_rules(&mut tx, provider_id, model_id, &metadata.cost_rules()).await?;
        bump_config_epoch(&mut tx).await?;
        let record = get_record(&mut tx, provider_id, model_id)
            .await?
            .context("updated Provider Model not found")?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(ProviderModelMutation::Applied(Box::new(record)))
    }

    async fn restore(
        &self,
        provider_id: &str,
        model_id: &str,
        input: RestoreProviderModel,
        validate_map: &crate::provider_models::ProviderModelThinkingMapValidator<'_>,
        before_commit: &(dyn Fn() -> anyhow::Result<()> + Send + Sync),
    ) -> anyhow::Result<ProviderModelRestore> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let updated = update_record_metadata(
            &mut tx,
            provider_id,
            model_id,
            &input.metadata,
            &input.snapshot_state,
            input.expected_revision,
        )
        .await?;
        if !updated {
            let exists = model_exists(&mut tx, provider_id, model_id).await?;
            tx.rollback().await?;
            connection.finish().await?;
            return Ok(if exists {
                ProviderModelRestore::Conflict
            } else {
                ProviderModelRestore::NotFound
            });
        }
        replace_cost_rules(&mut tx, provider_id, model_id, &input.metadata.cost_rules()).await?;
        if let Some(generated) = input.generated_thinking_level_map.as_deref() {
            SqliteRouteStore::refresh_generated_thinking_maps(
                &mut tx,
                provider_id,
                model_id,
                &input.metadata,
                generated,
                validate_map,
            )
            .await?;
        }
        bump_config_epoch(&mut tx).await?;
        let model = get_record(&mut tx, provider_id, model_id)
            .await?
            .context("restored Provider Model not found")?;
        let active_routes = SqliteRouteStore::load_routes(&mut tx, true).await?;
        before_commit()?;
        tx.commit().await?;
        connection.finish().await?;
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
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(record) = get_record(&mut tx, provider_id, model_id).await? {
            if record.revision != expected_revision {
                tx.rollback().await?;
                connection.finish().await?;
                return Ok(ProviderModelMutation::Conflict);
            }
            if record.selection_policy == policy {
                tx.rollback().await?;
                connection.finish().await?;
                return Ok(ProviderModelMutation::Applied(Box::new(record)));
            }
        } else {
            tx.rollback().await?;
            connection.finish().await?;
            return Ok(ProviderModelMutation::NotFound);
        }
        let result = sqlx::query(
            r#"UPDATE provider_models
       SET selection_policy = ?, revision = revision + 1, updated_at = datetime('now')
       WHERE provider_id = ? AND model_id = ? AND revision = ?"#,
        )
        .bind(policy.as_str())
        .bind(provider_id)
        .bind(model_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            let exists = get_record(&mut tx, provider_id, model_id).await?.is_some();
            tx.rollback().await?;
            connection.finish().await?;
            return Ok(if exists {
                ProviderModelMutation::Conflict
            } else {
                ProviderModelMutation::NotFound
            });
        }
        // Selection policy gates whether the resolved model may serve traffic;
        // publish the change atomically with the write it accompanies.
        bump_config_epoch(&mut tx).await?;
        if let Some(change) = change_record {
            insert_change(&mut tx, &change).await?;
        }
        let record = get_record(&mut tx, provider_id, model_id)
            .await?
            .context("updated Provider Model not found")?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(ProviderModelMutation::Applied(Box::new(record)))
    }

    async fn sync_state(&self, provider_id: &str) -> anyhow::Result<ProviderModelSyncState> {
        load_sync_state(&mut *self.pool.acquire().await?, provider_id).await
    }

    async fn set_automatic_sync(
        &self,
        provider_id: &str,
        enabled: bool,
    ) -> anyhow::Result<ProviderModelSyncState> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO provider_model_sync_state (provider_id, automatic_sync) VALUES (?, ?) ON CONFLICT(provider_id) DO UPDATE SET automatic_sync = excluded.automatic_sync")
            .bind(provider_id).bind(enabled).execute(&mut *tx).await?;
        let state = load_sync_state(&mut tx, provider_id).await?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(state)
    }

    async fn save_sync_failure(
        &self,
        provider_id: &str,
        summary: ProviderModelSyncSummary,
        mut record: ModelChangeRecord,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        if sqlx::query_scalar::<_, String>("SELECT id FROM providers WHERE id = ?")
            .bind(provider_id)
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            save_summary(&mut tx, provider_id, &summary, false).await?;
        }
        if let Some(json) =
            sqlx::query_scalar::<_, String>("SELECT record_json FROM model_changes WHERE id = ?")
                .bind(&record.id)
                .fetch_optional(&mut *tx)
                .await?
        {
            let mut existing: ModelChangeRecord = serde_json::from_str(&json)?;
            existing.outcome = record.outcome;
            existing.error = record.error;
            record = existing;
            sqlx::query("UPDATE model_changes SET record_json = ? WHERE id = ?")
                .bind(serde_json::to_string(&record)?)
                .bind(&record.id)
                .execute(&mut *tx)
                .await?;
        } else {
            insert_change(&mut tx, &record).await?;
        }
        tx.commit().await?;
        connection.finish().await?;
        Ok(())
    }

    async fn list_changes(&self, query: ModelChangeQuery) -> anyhow::Result<ModelChangePage> {
        let cursor = crate::storage::model_changes::cursor(&query)?;
        let limit = query.limit.unwrap_or(25).clamp(1, 100);
        let mut builder = sqlx::QueryBuilder::<Sqlite>::new(
            "SELECT c.record_json, NOT EXISTS(SELECT 1 FROM providers p WHERE p.id = c.provider_id) AS deleted FROM model_changes c WHERE c.checked_at >= ",
        );
        builder.push_bind(crate::storage::model_changes::cutoff());
        if let Some(id) = query.provider_id {
            builder.push(" AND c.provider_id = ").push_bind(id);
        }
        if let Some(source) = query.source {
            builder.push(" AND c.source = ").push_bind(source.as_str());
        }
        if let Some((time, id)) = cursor {
            builder
                .push(" AND (c.checked_at, c.id) < (")
                .push_bind(time)
                .push(", ")
                .push_bind(id)
                .push(")");
        }
        if let Some(kind) = query.change_type {
            let kind = kind.as_str();
            builder.push(" AND (EXISTS(SELECT 1 FROM model_change_items item WHERE item.record_id = c.id AND item.change_type = ").push_bind(kind).push(")");
            if kind == "sync_failed" {
                builder.push(" OR json_extract(c.record_json, '$.error') IS NOT NULL");
            }
            builder.push(")");
        }
        if let Some(q) = query.q {
            let q = q.to_lowercase();
            builder.push(" AND EXISTS(SELECT 1 FROM model_change_items item WHERE item.record_id = c.id AND (instr(item.name_key, ").push_bind(q.clone())
                .push(") > 0 OR instr(item.model_id_key, ").push_bind(q).push(") > 0))");
        }
        builder
            .push(" ORDER BY c.checked_at DESC, c.id DESC LIMIT ")
            .push_bind((limit + 1) as i64);
        let rows: Vec<(String, bool)> = builder.build_query_as().fetch_all(&self.pool).await?;
        let records = rows
            .into_iter()
            .map(|(json, deleted)| {
                let mut record: ModelChangeRecord = serde_json::from_str(&json)?;
                record.provider_deleted = deleted;
                Ok(record)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        crate::storage::model_changes::page(records, limit)
    }

    async fn get_change(&self, id: &str) -> anyhow::Result<Option<ModelChangeRecord>> {
        let row = sqlx::query_as::<_, (String, bool)>("SELECT c.record_json, NOT EXISTS(SELECT 1 FROM providers p WHERE p.id = c.provider_id) FROM model_changes c WHERE c.id = ? AND c.checked_at >= ?")
            .bind(id).bind(crate::storage::model_changes::cutoff()).fetch_optional(&self.pool).await?;
        row.map(|(json, deleted)| {
            let mut record: ModelChangeRecord = serde_json::from_str(&json)?;
            record.provider_deleted = deleted;
            Ok(record)
        })
        .transpose()
    }

    async fn prune_changes(&self, before: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire_write().await?;
        sqlx::query("DELETE FROM model_changes WHERE checked_at < ?")
            .bind(before)
            .execute(&mut *connection)
            .await?;
        connection.finish().await?;
        Ok(())
    }

    async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query(
    "DELETE FROM provider_models WHERE provider_id = ? AND model_id = ? AND source_kind = 'manual'",
            )
            .bind(provider_id)
            .bind(model_id)
            .execute(&mut *tx)
            .await?;
        let deleted = result.rows_affected() == 1;
        if deleted {
            // Removing a manual record changes loose resolution results;
            // other replicas must reload together with this commit.
            bump_config_epoch(&mut tx).await?;
        }
        tx.commit().await?;
        connection.finish().await?;
        Ok(deleted)
    }
}

async fn load_sync_state(
    connection: &mut SqliteConnection<'_>,
    provider_id: &str,
) -> anyhow::Result<ProviderModelSyncState> {
    let row = sqlx::query_as::<_, (bool, bool, Option<String>)>("SELECT automatic_sync, has_successful_sync, last_check FROM provider_model_sync_state WHERE provider_id = ?")
        .bind(provider_id).fetch_optional(connection).await?;
    let Some((automatic_sync, has_successful_sync, last_check)) = row else {
        return Ok(Default::default());
    };
    Ok(ProviderModelSyncState {
        automatic_sync,
        has_successful_sync,
        last_check: last_check
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        ..Default::default()
    })
}

async fn save_summary(
    tx: &mut Transaction<'_, Sqlite>,
    provider_id: &str,
    summary: &ProviderModelSyncSummary,
    success: bool,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO provider_model_sync_state (provider_id, last_check, has_successful_sync) VALUES (?, ?, ?) ON CONFLICT(provider_id) DO UPDATE SET last_check = excluded.last_check, has_successful_sync = provider_model_sync_state.has_successful_sync OR excluded.has_successful_sync")
        .bind(provider_id).bind(serde_json::to_string(summary)?).bind(success).execute(&mut **tx).await?;
    Ok(())
}

async fn insert_change(
    tx: &mut Transaction<'_, Sqlite>,
    record: &ModelChangeRecord,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO model_changes (id, provider_id, checked_at, source, record_json) VALUES (?, ?, ?, ?, ?)")
    .bind(&record.id)
    .bind(&record.provider_id)
    .bind(&record.checked_at)
    .bind(record.source.as_str())
    .bind(serde_json::to_string(record)?)
    .execute(&mut **tx).await?;
    for item in &record.changes {
        let name_key = item.name.to_lowercase();
        let model_id_key = item.model_id.to_lowercase();
        for kind in &item.types {
            sqlx::query("INSERT INTO model_change_items (record_id, name_key, model_id_key, change_type) VALUES (?, ?, ?, ?)")
            .bind(&record.id)
            .bind(&name_key)
            .bind(&model_id_key)
            .bind(kind.as_str())
            .execute(&mut **tx).await?;
        }
    }
    Ok(())
}

async fn get_record(
    connection: &mut SqliteConnection<'_>,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<Option<ProviderModelRecord>> {
    let row = sqlx::query_as::<_, ProviderModelRow>(
        r#"SELECT provider_id, model_id, source_kind, snapshot_state, source_metadata_json, metadata_source_provider_id,
                  presence, selection_policy, metadata_json, revision,
                  created_at, updated_at
           FROM provider_models
           WHERE provider_id = ? AND model_id = ?"#,
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let rules = load_rules_for_model(&mut *connection, provider_id, model_id).await?;
    decode_record(row, rules).map(Some)
}

async fn load_rules_for_provider(
    connection: &mut SqliteConnection<'_>,
    provider_id: &str,
) -> anyhow::Result<BTreeMap<String, Vec<ProviderModelCostRule>>> {
    let rows = sqlx::query_as::<_, CostRuleRow>(
        r#"SELECT provider_id, model_id, rule_index, rule_kind, threshold_tokens,
                  cost_input, cost_output, cost_cache_read,
                  cost_cache_write
           FROM provider_model_cost_rules
           WHERE provider_id = ?
           ORDER BY model_id, rule_index"#,
    )
    .bind(provider_id)
    .fetch_all(&mut *connection)
    .await?;
    let mut rules = BTreeMap::<String, Vec<ProviderModelCostRule>>::new();
    for row in rows {
        let model_id = row.model_id.clone();
        rules.entry(model_id).or_default().push(decode_rule(row)?);
    }
    Ok(rules)
}

async fn load_rules_for_model(
    connection: &mut SqliteConnection<'_>,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<Vec<ProviderModelCostRule>> {
    sqlx::query_as::<_, CostRuleRow>(
        r#"SELECT provider_id, model_id, rule_index, rule_kind, threshold_tokens,
                  cost_input, cost_output, cost_cache_read,
                  cost_cache_write
           FROM provider_model_cost_rules
           WHERE provider_id = ? AND model_id = ?
           ORDER BY rule_index"#,
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(decode_rule)
    .collect()
}

fn decode_record(
    row: ProviderModelRow,
    cost_rules: Vec<ProviderModelCostRule>,
) -> anyhow::Result<ProviderModelRecord> {
    Ok(ProviderModelRecord {
        provider_id: row.provider_id,
        model_id: row.model_id,
        source_kind: ProviderModelSourceKind::from_str(&row.source_kind)?,
        snapshot_state: serde_json::from_str(&row.snapshot_state)
            .context("decode Provider Model snapshot state")?,
        source_metadata: row
            .source_metadata_json
            .map(|json| {
                serde_json::from_str(&json).context("decode Provider Model source metadata")
            })
            .transpose()?,
        metadata_source_provider_id: row.metadata_source_provider_id,
        presence: ProviderModelPresence::from_str(&row.presence)?,
        selection_policy: ProviderModelSelectionPolicy::from_str(&row.selection_policy)?,
        metadata: serde_json::from_str(&row.metadata_json)
            .context("decode Provider Model metadata")?,
        revision: row.revision,
        created_at: row.created_at,
        updated_at: row.updated_at,
        cost_rules,
    })
}

fn decode_rule(row: CostRuleRow) -> anyhow::Result<ProviderModelCostRule> {
    let _ = row.provider_id;
    Ok(ProviderModelCostRule {
        rule_index: row.rule_index,
        kind: ProviderModelCostRuleKind::from_str(&row.rule_kind)?,
        threshold_tokens: u64::try_from(row.threshold_tokens)
            .context("negative Provider Model cost threshold")?,
        prices: PriceComponents {
            input: parse_decimal(row.cost_input)?,
            output: parse_decimal(row.cost_output)?,
            cache_read: parse_decimal(row.cost_cache_read)?,
            cache_write: parse_decimal(row.cost_cache_write)?,
        },
    })
}

async fn apply_discovered_metadata_update(
    tx: &mut Transaction<'_, Sqlite>,
    provider_id: &str,
    update: &ProviderModelPresenceUpdate,
) -> anyhow::Result<()> {
    let (metadata_json, source_metadata_json, snapshot_state) =
        sqlx::query_as::<_, (String, Option<String>, String)>(
            "SELECT metadata_json, source_metadata_json, snapshot_state FROM provider_models \
             WHERE provider_id = ? AND model_id = ? AND source_kind = 'discovered'",
        )
        .bind(provider_id)
        .bind(&update.model_id)
        .fetch_optional(&mut **tx)
        .await?
        .context("Provider Model disappeared during reconciliation")?;
    let mut metadata = match &update.metadata {
        Some(updated) => updated.clone(),
        None => serde_json::from_str(&metadata_json)
            .context("decode Provider Model metadata during reconciliation")?,
    };
    metadata.status = update.lifecycle_status.clone();
    let source_metadata_json = match &update.source_metadata {
        Some(source) => Some(serde_json::to_string(source)?),
        None => source_metadata_json,
    };
    let snapshot_state = match &update.snapshot_state {
        Some(state) => serde_json::to_string(state)?,
        None => snapshot_state,
    };
    let limit = metadata.limit.as_ref();
    let prices = metadata.cost.as_ref().map(|cost| &cost.prices);
    let result = sqlx::query(
        r#"UPDATE provider_models SET
               presence = ?, lifecycle_status = ?, name = ?, family = ?, open_weights = ?, limit_context = ?, cost_input = ?, cost_output = ?, cost_cache_read = ?,
               cost_cache_write = ?, metadata_json = ?, source_metadata_json = ?, snapshot_state = ?, metadata_source_provider_id = ?, revision = revision + 1, updated_at = datetime('now')
           WHERE provider_id = ? AND model_id = ? AND source_kind = 'discovered' AND revision = ?"#,
    )
    .bind(update.presence.as_str())
    .bind(&metadata.status)
    .bind(&metadata.name)
    .bind(&metadata.family)
    .bind(metadata.open_weights)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(decimal_text(prices.and_then(|prices| prices.input)))
    .bind(decimal_text(prices.and_then(|prices| prices.output)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_read)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_write)))
    .bind(serde_json::to_string(&metadata)?)
    .bind(source_metadata_json)
    .bind(snapshot_state)
    .bind(&update.metadata_source_provider_id)
    .bind(provider_id)
    .bind(&update.model_id)
    .bind(update.expected_revision)
    .execute(&mut **tx)
    .await?;
    anyhow::ensure!(
        result.rows_affected() == 1,
        "Provider Model has changed while synchronizing discovered models"
    );
    replace_cost_rules(tx, provider_id, &update.model_id, &metadata.cost_rules()).await
}

async fn insert_record(
    tx: &mut Transaction<'_, Sqlite>,
    input: NewProviderModelRecord,
) -> anyhow::Result<()> {
    let metadata_json = serde_json::to_string(&input.metadata)?;
    let limit = input.metadata.limit.as_ref();
    let prices = input.metadata.cost.as_ref().map(|cost| &cost.prices);
    sqlx::query(
        r#"INSERT INTO provider_models (
               provider_id, model_id, source_kind, snapshot_state, source_metadata_json, metadata_source_provider_id,
               presence, lifecycle_status, selection_policy, name, family,
               open_weights, limit_context, cost_input, cost_output, cost_cache_read, cost_cache_write,
               metadata_json
           ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&input.provider_id)
    .bind(&input.model_id)
    .bind(input.source_kind.as_str())
    .bind(serde_json::to_string(&input.snapshot_state)?)
    .bind(
        input
            .source_metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
    )
    .bind(&input.metadata_source_provider_id)
    .bind(input.presence.as_str())
    .bind(&input.metadata.status)
    .bind(input.selection_policy.as_str())
    .bind(&input.metadata.name)
    .bind(&input.metadata.family)
    .bind(input.metadata.open_weights)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(decimal_text(prices.and_then(|prices| prices.input)))
    .bind(decimal_text(prices.and_then(|prices| prices.output)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_read)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_write)))
    .bind(metadata_json)
    .execute(&mut **tx)
    .await?;
    replace_cost_rules(
        tx,
        &input.provider_id,
        &input.model_id,
        &input.metadata.cost_rules(),
    )
    .await
}

async fn update_record_metadata(
    tx: &mut Transaction<'_, Sqlite>,
    provider_id: &str,
    model_id: &str,
    metadata: &ProviderModelMetadata,
    snapshot_state: &SnapshotState,
    expected_revision: i64,
) -> anyhow::Result<bool> {
    let limit = metadata.limit.as_ref();
    let prices = metadata.cost.as_ref().map(|cost| &cost.prices);
    let result = sqlx::query(
        r#"UPDATE provider_models SET
               lifecycle_status = ?, name = ?, family = ?, open_weights = ?, limit_context = ?, cost_input = ?, cost_output = ?, cost_cache_read = ?,
               cost_cache_write = ?, metadata_json = ?, snapshot_state = ?, revision = revision + 1, updated_at = datetime('now')
           WHERE provider_id = ? AND model_id = ? AND revision = ?"#,
    )
    .bind(&metadata.status)
    .bind(&metadata.name)
    .bind(&metadata.family)
    .bind(metadata.open_weights)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(decimal_text(prices.and_then(|prices| prices.input)))
    .bind(decimal_text(prices.and_then(|prices| prices.output)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_read)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_write)))
    .bind(serde_json::to_string(metadata)?)
    .bind(serde_json::to_string(snapshot_state)?)
    .bind(provider_id)
    .bind(model_id)
    .bind(expected_revision)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn replace_cost_rules(
    tx: &mut Transaction<'_, Sqlite>,
    provider_id: &str,
    model_id: &str,
    rules: &[ProviderModelCostRule],
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM provider_model_cost_rules WHERE provider_id = ? AND model_id = ?")
        .bind(provider_id)
        .bind(model_id)
        .execute(&mut **tx)
        .await?;
    for rule in rules {
        sqlx::query(
            r#"INSERT INTO provider_model_cost_rules (
                   provider_id, model_id, rule_index, rule_kind, threshold_tokens,
                   cost_input, cost_output, cost_cache_read, cost_cache_write
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(provider_id)
        .bind(model_id)
        .bind(rule.rule_index)
        .bind(rule.kind.as_str())
        .bind(
            i64::try_from(rule.threshold_tokens)
                .context("cost threshold exceeds database range")?,
        )
        .bind(decimal_text(rule.prices.input))
        .bind(decimal_text(rule.prices.output))
        .bind(decimal_text(rule.prices.cache_read))
        .bind(decimal_text(rule.prices.cache_write))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn model_exists(
    tx: &mut Transaction<'_, Sqlite>,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM provider_models WHERE provider_id = ? AND model_id = ?",
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_one(&mut **tx)
    .await?
        > 0)
}

/// Persist the configuration-change notification inside the caller's
/// transaction so other replicas reload their runtime snapshot together with
/// the configuration it announces. A missing or unparsable epoch counts as 0.
async fn bump_config_epoch(tx: &mut Transaction<'_, Sqlite>) -> anyhow::Result<()> {
    let current: i64 = sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE name = ?")
        .bind(CONFIG_EPOCH_KEY)
        .fetch_optional(&mut **tx)
        .await?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let next = current
        .checked_add(1)
        .context("configuration epoch overflow")?;
    sqlx::query(
        "INSERT INTO settings (name, value, updated_at) VALUES (?, ?, datetime('now')) \
         ON CONFLICT(name) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(CONFIG_EPOCH_KEY)
    .bind(next.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn decimal_text(value: Option<Decimal>) -> Option<String> {
    value.map(|value| value.to_string())
}

fn parse_decimal(value: Option<String>) -> anyhow::Result<Option<Decimal>> {
    value
        .map(|value| Decimal::from_str(&value).context("decode Provider Model decimal"))
        .transpose()
}

fn to_i64(value: Option<u64>) -> Option<anyhow::Result<i64>> {
    value.map(|value| {
        i64::try_from(value).context("Provider Model token limit exceeds database range")
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

    #[tokio::test]
    async fn retired_price_upgrade_preserves_snapshot_and_decimal_precision() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../../migrations/sqlite/0001_baseline.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../migrations/sqlite/0002_data_contracts.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(r#"INSERT INTO providers (id, name, protocol, base_url, api_key) VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key');
            INSERT INTO provider_models (provider_id, model_id, source_kind, presence, revision, cost_input, metadata_json)
            VALUES ('provider', 'model', 'manual', 'present', 7, '0.1234567890123456789012345678',
            '{"id":"model","reasoning_efforts":["high","custom"],"modalities":{"input":["text","audio"],"output":["audio"]},"custom":"keep","cost":{"input":0.1234567890123456789012345678,"output":2,"cache_read":3,"cache_write":4,"reasoning":5,"input_audio":6,"output_audio":7,"context_over_200k":{"input":0.1234567890123456789012345678,"reasoning":5,"input_audio":6,"output_audio":7},"tiers":[{"tier":{"type":"context","size":100},"input":0.1234567890123456789012345678,"reasoning":5,"input_audio":6,"output_audio":7},{"tier":{"type":"context","size":200},"output":2,"reasoning":8}]}}');
            INSERT INTO provider_model_cost_rules (provider_id, model_id, rule_index, rule_kind, threshold_tokens, cost_input, cost_reasoning)
            VALUES ('provider','model',0,'tier',100,'0.1234567890123456789012345678','5');"#)
            .execute(&pool).await.unwrap();
        let before: String = sqlx::query_scalar("SELECT metadata_json FROM provider_models")
            .fetch_one(&pool)
            .await
            .unwrap();
        let identity_before: (String, String, String, Option<String>) = sqlx::query_as(
            "SELECT provider_id, model_id, snapshot_state, metadata_source_provider_id FROM provider_models")
            .fetch_one(&pool).await.unwrap();
        let mut expected: serde_json::Value = serde_json::from_str(&before).unwrap();
        let cost = expected["cost"].as_object_mut().unwrap();
        for key in ["reasoning", "input_audio", "output_audio"] {
            cost.remove(key);
        }
        for key in ["reasoning", "input_audio", "output_audio"] {
            cost.get_mut("context_over_200k")
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
        }
        for tier in cost.get_mut("tiers").unwrap().as_array_mut().unwrap() {
            for key in ["reasoning", "input_audio", "output_audio"] {
                tier.as_object_mut().unwrap().remove(key);
            }
        }
        sqlx::raw_sql(include_str!(
            "../../../migrations/sqlite/0013_remove_extra_model_prices.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let (json, revision, price): (String, i64, String) =
            sqlx::query_as("SELECT metadata_json, revision, cost_input FROM provider_models")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            expected
        );
        assert_eq!(revision, 7);
        assert_eq!(price, "0.1234567890123456789012345678");
        let rule_price: String =
            sqlx::query_scalar("SELECT cost_input FROM provider_model_cost_rules")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rule_price, price);
        let identity_after: (String, String, String, Option<String>) = sqlx::query_as(
            "SELECT provider_id, model_id, snapshot_state, metadata_source_provider_id FROM provider_models")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(identity_after, identity_before);
        for table in ["provider_models", "provider_model_cost_rules"] {
            let columns: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT name FROM pragma_table_info('{table}')"
            )))
            .fetch_all(&pool)
            .await
            .unwrap();
            for removed in ["cost_reasoning", "cost_input_audio", "cost_output_audio"] {
                assert!(!columns.iter().any(|column| column == removed));
            }
        }
    }

    #[tokio::test]
    async fn model_specification_upgrade_preserves_data_and_target_maps() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for migration in [
            include_str!("../../../migrations/sqlite/0001_baseline.sql"),
            include_str!("../../../migrations/sqlite/0002_data_contracts.sql"),
            include_str!("../../../migrations/sqlite/0003_estimated_input_tokens.sql"),
            include_str!("../../../migrations/sqlite/0004_observation_recovery_indexes.sql"),
            include_str!("../../../migrations/sqlite/0005_credential_custom_rules.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        sqlx::raw_sql(r#"INSERT INTO providers (id, name, protocol, base_url, api_key) VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key');
            INSERT INTO models (id, model_id) VALUES ('route', 'client-model');
            INSERT INTO model_backends (id, model_id, provider_id, model) VALUES ('target', 'route', 'provider', 'model');
            INSERT INTO provider_models (provider_id, model_id, source_kind, presence, metadata_source_provider_id, revision, metadata_json)
            VALUES ('provider', 'model', 'discovered', 'present', 'catalog', 7,
            '{"id":"model","name":"Retained","attachment":true,"reasoning":true,"tool_call":true,"structured_output":true,"temperature":true,"interleaved":{"field":"reasoning_content"},"reasoning_levels":["low"],"thinking_toggle":true,"limit":{"context":12345,"input":100,"output":200},"reasoning_options":[{"type":"toggle"},{"type":"effort","values":[null,"default","null"," DEFAULT ","","none","high","custom","high"]},{"type":"budget_tokens","min":1024}],"custom":"keep"}');"#)
            .execute(&pool).await.unwrap();
        let retained_map = serde_json::json!([
            {"level":"low","control":{"type":"effort","value":"old-generated"},"source":"generated"},
            {"level":"high","control":{"type":"effort","value":"manual"},"source":"overridden"}
        ]).to_string();
        sqlx::query("UPDATE model_backends SET thinking_level_map = ? WHERE id = 'target'")
            .bind(retained_map)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(r#"INSERT INTO provider_models (provider_id, model_id, source_kind, presence, metadata_json)
            VALUES ('provider', 'toggle-only', 'manual', 'present', '{"id":"toggle-only","reasoning":true,"reasoning_options":[{"type":"toggle"},{"type":"effort","values":[null,"default",""]}]}'),
                   ('provider', 'already-explicit', 'manual', 'present', '{"id":"already-explicit","reasoning_efforts":[null,"default","null","xhigh","vendor-effort","xhigh"],"reasoning_options":[{"type":"effort","values":["xhigh","stale-effort"]}]}'),
                   ('provider', 'explicit-empty', 'manual', 'present', '{"id":"explicit-empty","reasoning_efforts":[],"reasoning_options":[{"type":"effort","values":["stale-effort"]}]}');"#)
            .execute(&pool).await.unwrap();
        let original_map: String =
            sqlx::query_scalar("SELECT thinking_level_map FROM model_backends WHERE id = 'target'")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../migrations/sqlite/0006_model_specification.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let json: String = sqlx::query_scalar("SELECT metadata_json FROM provider_models WHERE provider_id = 'provider' AND model_id = 'model'").fetch_one(&pool).await.unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            metadata["reasoning_efforts"],
            serde_json::json!(["none", "high", "custom"])
        );
        assert_eq!(metadata["limit"], serde_json::json!({"context":12345}));
        assert_eq!(metadata["custom"], "keep");
        assert_eq!(metadata["name"], "Retained");
        for key in [
            "attachment",
            "reasoning",
            "tool_call",
            "structured_output",
            "temperature",
            "interleaved",
            "reasoning_options",
            "reasoning_levels",
            "thinking_toggle",
        ] {
            assert!(metadata.get(key).is_none(), "obsolete key {key}");
        }
        let empty_json: String = sqlx::query_scalar(
            "SELECT metadata_json FROM provider_models WHERE model_id = 'toggle-only'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let empty: serde_json::Value = serde_json::from_str(&empty_json).unwrap();
        assert!(empty.get("reasoning_efforts").is_none());
        assert!(empty.get("reasoning_options").is_none());
        let explicit_json: String = sqlx::query_scalar(
            "SELECT metadata_json FROM provider_models WHERE model_id = 'already-explicit'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let explicit: serde_json::Value = serde_json::from_str(&explicit_json).unwrap();
        assert_eq!(
            explicit["reasoning_efforts"],
            serde_json::json!(["xhigh", "vendor-effort"])
        );
        let explicit_empty_json: String = sqlx::query_scalar(
            "SELECT metadata_json FROM provider_models WHERE model_id = 'explicit-empty'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let explicit_empty: serde_json::Value = serde_json::from_str(&explicit_empty_json).unwrap();
        assert!(explicit_empty.get("reasoning_efforts").is_none());
        let map: String =
            sqlx::query_scalar("SELECT thinking_level_map FROM model_backends WHERE id = 'target'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(map, original_map);
        let state: (i64, Option<String>) = sqlx::query_as("SELECT revision, metadata_source_provider_id FROM provider_models WHERE model_id = 'model'").fetch_one(&pool).await.unwrap();
        assert_eq!(state, (7, Some("catalog".into())));
    }

    use super::*;
    use crate::provider_models::{ModelCost, SourceStamp};
    use crate::thinking::{ThinkingLevelMapping, ThinkingMappingSource};

    #[tokio::test]
    async fn stale_reconciliation_rolls_back_all_sqlite_updates() {
        let data_dir = tempfile::tempdir().unwrap();
        let pool = crate::db::init_pool(data_dir.path()).await.unwrap();
        crate::migrations::migrate_sqlite(&pool, None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key)
                     VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let storage = SqliteStorage::from_pool(pool);
        for model_id in ["first", "second"] {
            storage
                .create(
                    NewProviderModelRecord {
                        provider_id: "provider".into(),
                        model_id: model_id.into(),
                        source_kind: ProviderModelSourceKind::Discovered,
                        snapshot_state: SnapshotState::Unregistered,
                        source_metadata: None,
                        metadata_source_provider_id: None,
                        presence: ProviderModelPresence::Present,
                        selection_policy: ProviderModelSelectionPolicy::Auto,
                        metadata: ProviderModelMetadata::bare(model_id),
                    },
                    None,
                )
                .await
                .unwrap();
        }
        let second = storage.get("provider", "second").await.unwrap().unwrap();
        storage
            .update_metadata(
                "provider",
                "second",
                ProviderModelMetadata {
                    name: Some("Admin correction".into()),
                    ..second.metadata
                },
                SnapshotState::Local,
                second.revision,
            )
            .await
            .unwrap();
        let updates = ["first", "second"]
            .into_iter()
            .map(|model_id| ProviderModelPresenceUpdate {
                model_id: model_id.into(),
                expected_revision: 1,
                snapshot_state: Some(SnapshotState::Imported {
                    source: crate::provider_models::SourceStamp::Discovery,
                    pinned: std::collections::BTreeSet::new(),
                }),
                metadata_source_provider_id: None,
                presence: ProviderModelPresence::Missing,
                lifecycle_status: None,
                metadata: Some(ProviderModelMetadata::bare(model_id)),
                source_metadata: None,
                generated_thinking_level_map: None,
            })
            .collect();
        assert!(
            storage
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
        let first = storage.get("provider", "first").await.unwrap().unwrap();
        let second = storage.get("provider", "second").await.unwrap().unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(first.presence, ProviderModelPresence::Present);
        assert_eq!(first.snapshot_state, SnapshotState::Unregistered);
        assert_eq!(second.metadata.name.as_deref(), Some("Admin correction"));
        assert_eq!(second.snapshot_state, SnapshotState::Local);
    }

    #[tokio::test]
    async fn create_waits_for_a_concurrent_sqlite_writer() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool, None)
            .await
            .expect("SQLite migrations");
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key)
             VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key')",
        )
        .execute(&pool)
        .await
        .expect("test Provider");
        let storage = SqliteStorage::from_pool(pool.clone());

        let mut writer = pool.acquire().await.expect("writer connection");
        let writer_tx = writer
            .begin_with("BEGIN IMMEDIATE")
            .await
            .expect("writer transaction");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let create_task = tokio::spawn(async move {
            started_tx.send(()).expect("signal create start");
            storage
                .create(
                    NewProviderModelRecord {
                        provider_id: "provider".into(),
                        model_id: "model".into(),
                        source_kind: ProviderModelSourceKind::Manual,
                        snapshot_state: SnapshotState::Local,
                        source_metadata: None,
                        metadata_source_provider_id: None,
                        presence: ProviderModelPresence::Present,
                        selection_policy: ProviderModelSelectionPolicy::Auto,
                        metadata: ProviderModelMetadata::bare("model"),
                    },
                    None,
                )
                .await
        });
        started_rx.await.expect("create start");

        let mut create_task = create_task;
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut create_task)
                .await
                .is_err(),
            "create must wait while another write transaction owns the database"
        );
        writer_tx
            .commit()
            .await
            .expect("release writer transaction");

        let mutation = create_task
            .await
            .expect("create task")
            .expect("create Provider Model");
        assert!(matches!(mutation, ProviderModelMutation::Applied(_)));
    }

    async fn restore_fixture() -> (tempfile::TempDir, SqliteStorage) {
        let data_dir = tempfile::tempdir().unwrap();
        let pool = crate::db::init_pool(data_dir.path()).await.unwrap();
        crate::migrations::migrate_sqlite(&pool, None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key)
             VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let storage = SqliteStorage::from_pool(pool);
        let mutation = storage
            .create(
                NewProviderModelRecord {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    source_kind: ProviderModelSourceKind::Discovered,
                    snapshot_state: SnapshotState::Unregistered,
                    source_metadata: None,
                    metadata_source_provider_id: Some("catalog".into()),
                    presence: ProviderModelPresence::Present,
                    selection_policy: ProviderModelSelectionPolicy::Auto,
                    metadata: ProviderModelMetadata::bare("model"),
                },
                None,
            )
            .await
            .unwrap();
        assert!(matches!(mutation, ProviderModelMutation::Applied(_)));
        (data_dir, storage)
    }

    fn all_hidden_map() -> Vec<ThinkingLevelMapping> {
        crate::thinking::generate_thinking_level_map(&ProviderModelMetadata::bare("model"))
    }

    fn refreshed_metadata() -> ProviderModelMetadata {
        ProviderModelMetadata {
            name: Some("Refreshed Model".into()),
            reasoning_efforts: Some(vec!["low".into(), "high".into()]),
            cost: Some(ModelCost {
                context_over_200k: Some(PriceComponents {
                    input: Some(Decimal::new(5, 6)),
                    ..PriceComponents::default()
                }),
                ..ModelCost::default()
            }),
            ..ProviderModelMetadata::bare("model")
        }
    }

    fn restore_input(
        metadata: &ProviderModelMetadata,
        expected_revision: i64,
    ) -> RestoreProviderModel {
        RestoreProviderModel {
            metadata: metadata.clone(),
            snapshot_state: SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog".into(),
                },
                pinned: std::collections::BTreeSet::new(),
            },
            expected_revision,
            generated_thinking_level_map: Some(crate::thinking::generate_thinking_level_map(
                metadata,
            )),
        }
    }

    async fn add_route_target(
        pool: &sqlx::SqlitePool,
        (route, route_enabled): (&str, bool),
        target: &str,
        provider_model: &str,
        target_enabled: bool,
        priority: i32,
        map: &[ThinkingLevelMapping],
    ) {
        sqlx::query(
            "INSERT INTO models (id, model_id, balance, is_enabled)
             VALUES (?, ?, 'traffic_equalization', ?)",
        )
        .bind(route)
        .bind(route)
        .bind(route_enabled)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO model_backends (id, model_id, provider_id, model, enabled, priority,
                first_token_timeout_ms, target_retry_budget, target_cooldown_ms, thinking_level_map)
             VALUES (?, ?, 'provider', ?, ?, ?, 12345, 2, 999, ?)",
        )
        .bind(target)
        .bind(route)
        .bind(provider_model)
        .bind(target_enabled)
        .bind(priority)
        .bind(sqlx::types::Json(map))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn target_row(
        pool: &sqlx::SqlitePool,
        target: &str,
    ) -> (bool, i32, i64, i32, i64, String) {
        sqlx::query_as::<_, (bool, i32, i64, i32, i64, String)>(
            "SELECT enabled, priority, first_token_timeout_ms, target_retry_budget,
                    target_cooldown_ms, created_at
             FROM model_backends WHERE id = ?",
        )
        .bind(target)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn target_map(pool: &sqlx::SqlitePool, target: &str) -> Vec<ThinkingLevelMapping> {
        sqlx::query_scalar::<_, sqlx::types::Json<Vec<ThinkingLevelMapping>>>(
            "SELECT thinking_level_map FROM model_backends WHERE id = ?",
        )
        .bind(target)
        .fetch_one(pool)
        .await
        .unwrap()
        .0
    }

    async fn config_epoch(pool: &sqlx::SqlitePool) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE name = ?")
            .bind(CONFIG_EPOCH_KEY)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn restore_commits_model_targets_routes_and_epoch_atomically() {
        let (_data_dir, storage) = restore_fixture().await;
        let pool = storage.pool();
        add_route_target(
            pool,
            ("route-a", true),
            "target-a",
            "model",
            true,
            7,
            &all_hidden_map(),
        )
        .await;
        // Disabled Routes and disabled Targets still receive the refresh.
        add_route_target(
            pool,
            ("route-b", false),
            "target-b",
            "model",
            false,
            3,
            &all_hidden_map(),
        )
        .await;
        // A Target bound to a different Provider Model must not be touched.
        add_route_target(
            pool,
            ("route-c", true),
            "target-c",
            "other-model",
            true,
            0,
            &all_hidden_map(),
        )
        .await;

        let metadata = refreshed_metadata();
        let input = restore_input(&metadata, 1);

        // A manual override committed after the restore input was prepared —
        // the transaction must read this latest map and keep the row.
        let mut overridden_map = all_hidden_map();
        *overridden_map
            .iter_mut()
            .find(|row| row.level == ThinkingLevel::Medium)
            .unwrap() = ThinkingLevelMapping {
            level: ThinkingLevel::Medium,
            control: TargetThinkingControl::Effort {
                value: "high".into(),
            },
            source: ThinkingMappingSource::Overridden,
        };
        sqlx::query("UPDATE model_backends SET thinking_level_map = ? WHERE id = 'target-a'")
            .bind(sqlx::types::Json(&overridden_map))
            .execute(pool)
            .await
            .unwrap();

        let epoch_before = config_epoch(pool).await.unwrap().parse::<i64>().unwrap();
        let target_a_before = target_row(pool, "target-a").await;
        let target_c_map_before = target_map(pool, "target-c").await;

        let result = storage
            .restore("provider", "model", input, &|_, _, _| Ok(()), &|| Ok(()))
            .await
            .unwrap();
        let ProviderModelRestore::Applied {
            model,
            active_routes,
        } = result
        else {
            panic!("restore must apply")
        };

        assert_eq!(model.revision, 2);
        assert_eq!(model.metadata.name.as_deref(), Some("Refreshed Model"));
        assert_eq!(
            model.snapshot_state,
            SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog".into(),
                },
                pinned: std::collections::BTreeSet::new(),
            }
        );
        assert_eq!(model.cost_rules.len(), 1);
        assert_eq!(
            model.cost_rules[0].kind,
            ProviderModelCostRuleKind::ContextOver200k
        );
        assert_eq!(
            config_epoch(pool).await.unwrap().parse::<i64>().unwrap(),
            epoch_before + 1
        );

        // Generated rows follow the restoreed map; the Overridden row survives.
        let map_a = target_map(pool, "target-a").await;
        let at = |level| map_a.iter().find(|row| row.level == level).unwrap();
        assert_eq!(
            at(ThinkingLevel::Medium),
            &ThinkingLevelMapping {
                level: ThinkingLevel::Medium,
                control: TargetThinkingControl::Effort {
                    value: "high".into(),
                },
                source: ThinkingMappingSource::Overridden,
            }
        );
        assert_eq!(
            at(ThinkingLevel::Low).control,
            TargetThinkingControl::Effort {
                value: "low".into(),
            }
        );
        assert_eq!(
            at(ThinkingLevel::Off).control,
            TargetThinkingControl::Hidden
        );

        let map_b = target_map(pool, "target-b").await;
        assert!(
            map_b
                .iter()
                .all(|row| row.source == ThinkingMappingSource::Generated)
        );
        assert_eq!(
            map_b
                .iter()
                .find(|row| row.level == ThinkingLevel::High)
                .unwrap()
                .control,
            TargetThinkingControl::Effort {
                value: "high".into(),
            }
        );

        // Target IDs and every unrelated column stay untouched.
        assert_eq!(target_row(pool, "target-a").await, target_a_before);
        assert_eq!(target_map(pool, "target-c").await, target_c_map_before);

        // The returned snapshot is the committed state: enabled Routes only,
        // each carrying the refreshed Target map.
        let route_a = active_routes
            .iter()
            .find(|route| route.model_id == "route-a")
            .unwrap();
        assert_eq!(route_a.targets.len(), 1);
        assert_eq!(route_a.targets[0].id, "target-a");
        assert_eq!(route_a.targets[0].thinking_level_map, map_a);
        assert!(
            active_routes
                .iter()
                .all(|route| route.model_id != "route-b")
        );
        assert!(
            active_routes
                .iter()
                .any(|route| route.model_id == "route-c")
        );
    }

    #[tokio::test]
    async fn restore_rolls_back_everything_when_a_target_update_fails() {
        let (_data_dir, storage) = restore_fixture().await;
        let pool = storage.pool();
        add_route_target(
            pool,
            ("route-a", true),
            "target-a",
            "model",
            true,
            0,
            &all_hidden_map(),
        )
        .await;
        add_route_target(
            pool,
            ("route-b", true),
            "target-b",
            "model",
            true,
            0,
            &all_hidden_map(),
        )
        .await;
        sqlx::query("INSERT OR REPLACE INTO settings (name, value) VALUES ('config_epoch', '7')")
            .execute(pool)
            .await
            .unwrap();
        // Fail on the second Target write so a partial refresh is attempted.
        sqlx::query("CREATE TABLE update_attempts (n INTEGER NOT NULL)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO update_attempts (n) VALUES (0)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_second_target_update BEFORE UPDATE ON model_backends BEGIN \
                 UPDATE update_attempts SET n = n + 1; \
                 SELECT RAISE(FAIL, 'target update rejected') \
                 WHERE (SELECT n FROM update_attempts) >= 2; \
             END",
        )
        .execute(pool)
        .await
        .unwrap();

        let model_before = storage.get("provider", "model").await.unwrap().unwrap();
        let map_a_before = target_map(pool, "target-a").await;
        let map_b_before = target_map(pool, "target-b").await;

        let result = storage
            .restore(
                "provider",
                "model",
                restore_input(&refreshed_metadata(), 1),
                &|_, _, _| Ok(()),
                &|| Ok(()),
            )
            .await;
        assert!(result.is_err());

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_a_before);
        assert_eq!(target_map(pool, "target-b").await, map_b_before);
        assert_eq!(config_epoch(pool).await.as_deref(), Some("7"));
    }

    #[tokio::test]
    async fn restore_rolls_back_when_the_epoch_write_fails() {
        let (_data_dir, storage) = restore_fixture().await;
        let pool = storage.pool();
        add_route_target(
            pool,
            ("route-a", true),
            "target-a",
            "model",
            true,
            0,
            &all_hidden_map(),
        )
        .await;
        sqlx::query("INSERT OR REPLACE INTO settings (name, value) VALUES ('config_epoch', '3')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_epoch_update BEFORE UPDATE ON settings \
             WHEN NEW.name = 'config_epoch' \
             BEGIN SELECT RAISE(FAIL, 'epoch write rejected'); END",
        )
        .execute(pool)
        .await
        .unwrap();

        let model_before = storage.get("provider", "model").await.unwrap().unwrap();
        let map_before = target_map(pool, "target-a").await;
        let epoch_before = config_epoch(pool).await;

        let result = storage
            .restore(
                "provider",
                "model",
                restore_input(&refreshed_metadata(), 1),
                &|_, _, _| Ok(()),
                &|| Ok(()),
            )
            .await;
        assert!(result.is_err());

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert_eq!(config_epoch(pool).await, epoch_before);
    }

    #[tokio::test]
    async fn restore_rolls_back_when_the_final_permit_check_fails() {
        let (_data_dir, storage) = restore_fixture().await;
        let pool = storage.pool();
        add_route_target(
            pool,
            ("route-a", true),
            "target-a",
            "model",
            true,
            0,
            &all_hidden_map(),
        )
        .await;
        sqlx::query("INSERT OR REPLACE INTO settings (name, value) VALUES ('config_epoch', '4')")
            .execute(pool)
            .await
            .unwrap();

        let model_before = storage.get("provider", "model").await.unwrap().unwrap();
        let map_before = target_map(pool, "target-a").await;
        let epoch_before = config_epoch(pool).await;

        let result = storage
            .restore(
                "provider",
                "model",
                restore_input(&refreshed_metadata(), 1),
                &|_, _, _| Ok(()),
                &|| anyhow::bail!("vendor write permit revoked"),
            )
            .await;
        assert!(result.is_err());

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert_eq!(config_epoch(pool).await, epoch_before);
    }

    #[tokio::test]
    async fn restore_conflict_and_not_found_write_nothing() {
        let (_data_dir, storage) = restore_fixture().await;
        let pool = storage.pool();
        add_route_target(
            pool,
            ("route-a", true),
            "target-a",
            "model",
            true,
            0,
            &all_hidden_map(),
        )
        .await;

        let model_before = storage.get("provider", "model").await.unwrap().unwrap();
        let map_before = target_map(pool, "target-a").await;
        let epoch_before = config_epoch(pool).await;

        let stale = storage
            .restore(
                "provider",
                "model",
                restore_input(&refreshed_metadata(), 99),
                &|_, _, _| Ok(()),
                &|| Ok(()),
            )
            .await
            .unwrap();
        assert!(matches!(stale, ProviderModelRestore::Conflict));
        let missing = storage
            .restore(
                "provider",
                "absent",
                restore_input(&refreshed_metadata(), 1),
                &|_, _, _| Ok(()),
                &|| Ok(()),
            )
            .await
            .unwrap();
        assert!(matches!(missing, ProviderModelRestore::NotFound));

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert_eq!(config_epoch(pool).await, epoch_before);
    }

    #[tokio::test]
    async fn restore_rolls_back_when_the_generated_map_is_incomplete() {
        let (_data_dir, storage) = restore_fixture().await;
        let pool = storage.pool();
        add_route_target(
            pool,
            ("route-a", true),
            "target-a",
            "model",
            true,
            0,
            &all_hidden_map(),
        )
        .await;

        let model_before = storage.get("provider", "model").await.unwrap().unwrap();
        let map_before = target_map(pool, "target-a").await;
        let epoch_before = config_epoch(pool).await;

        let mut input = restore_input(&refreshed_metadata(), 1);
        input
            .generated_thinking_level_map
            .as_mut()
            .expect("restore fixture has a generated map")
            .retain(|row| row.level != ThinkingLevel::Low);
        let result = storage
            .restore("provider", "model", input, &|_, _, _| Ok(()), &|| Ok(()))
            .await;
        assert!(result.is_err());

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert_eq!(config_epoch(pool).await, epoch_before);
    }

    #[tokio::test]
    async fn restore_treats_an_invalid_epoch_as_zero() {
        let (_data_dir, storage) = restore_fixture().await;
        let pool = storage.pool();
        sqlx::query(
            "INSERT OR REPLACE INTO settings (name, value) VALUES ('config_epoch', 'bogus')",
        )
        .execute(pool)
        .await
        .unwrap();

        let result = storage
            .restore(
                "provider",
                "model",
                restore_input(&refreshed_metadata(), 1),
                &|_, _, _| Ok(()),
                &|| Ok(()),
            )
            .await
            .unwrap();
        assert!(matches!(result, ProviderModelRestore::Applied { .. }));
        assert_eq!(config_epoch(pool).await.as_deref(), Some("1"));
    }
}
