use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::Context;
use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{Connection, Sqlite, SqliteConnection, Transaction};

use super::SqliteStorage;
use super::routes::SqliteRouteStore;
use crate::provider_models::{
    NewProviderModelRecord, PriceComponents, ProviderModelCostRule, ProviderModelCostRuleKind,
    ProviderModelMetadata, ProviderModelMutation, ProviderModelPresence,
    ProviderModelPresenceUpdate, ProviderModelReconciliation, ProviderModelRecord,
    ProviderModelReimport, ProviderModelSelectionPolicy, ProviderModelSourceKind,
    ReimportProviderModel, SnapshotState, SourceStamp,
};
use crate::storage::CONFIG_EPOCH_KEY;
use crate::storage::traits::ProviderModelStore;

#[derive(sqlx::FromRow)]
struct ProviderModelRow {
    provider_id: String,
    model_id: String,
    source_kind: String,
    snapshot_state: String,
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
    cost_reasoning: Option<String>,
    cost_cache_read: Option<String>,
    cost_cache_write: Option<String>,
    cost_input_audio: Option<String>,
    cost_output_audio: Option<String>,
}

#[async_trait]
impl ProviderModelStore for SqliteStorage {
    async fn list_for_provider(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderModelRecord>> {
        let mut connection = self.pool.acquire().await?;
        let rows = sqlx::query_as::<_, ProviderModelRow>(
            r#"SELECT provider_id, model_id, source_kind, snapshot_state, metadata_source_provider_id,
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
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
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
            if update.metadata.is_some() {
                apply_discovered_metadata_update(&mut tx, provider_id, &update).await?;
                continue;
            }
            let metadata_json = sqlx::query_scalar::<_, String>(
                "SELECT metadata_json FROM provider_models WHERE provider_id = ? AND model_id = ? AND source_kind = 'discovered'",
            )
            .bind(provider_id)
            .bind(&update.model_id)
            .fetch_optional(&mut *tx)
            .await?;
            let metadata_json =
                metadata_json.context("Provider Model disappeared during reconciliation")?;
            let mut metadata: ProviderModelMetadata = serde_json::from_str(&metadata_json)
                .context("decode Provider Model metadata during reconciliation")?;
            metadata.status = update.lifecycle_status.clone();
            let result = sqlx::query(
                r#"UPDATE provider_models
                   SET presence = ?, lifecycle_status = ?, metadata_source_provider_id = ?, metadata_json = ?,
                       revision = revision + 1, updated_at = datetime('now')
                   WHERE provider_id = ? AND model_id = ? AND source_kind = 'discovered' AND revision = ?"#,
            )
            .bind(update.presence.as_str())
            .bind(update.lifecycle_status)
            .bind(update.metadata_source_provider_id)
            .bind(serde_json::to_string(&metadata)?)
            .bind(provider_id)
            .bind(&update.model_id)
            .bind(update.expected_revision)
            .execute(&mut *tx)
            .await?;
            anyhow::ensure!(
                result.rows_affected() == 1,
                "Provider Model has changed while synchronizing discovered models"
            );
        }
        for input in reconciliation.inserts {
            insert_record(&mut tx, input).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn create(&self, input: NewProviderModelRecord) -> anyhow::Result<ProviderModelMutation> {
        let mut connection = self.pool.acquire().await?;
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
            return Ok(ProviderModelMutation::Conflict);
        }
        let provider_id = input.provider_id.clone();
        let model_id = input.model_id.clone();
        insert_record(&mut tx, input).await?;
        tx.commit().await?;
        Ok(ProviderModelMutation::Applied(Box::new(
            get_record(&mut connection, &provider_id, &model_id)
                .await?
                .context("created Provider Model not found")?,
        )))
    }

    async fn update_metadata(
        &self,
        provider_id: &str,
        model_id: &str,
        metadata: ProviderModelMetadata,
        snapshot_state: SnapshotState,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation> {
        let mut connection = self.pool.acquire().await?;
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
            return Ok(if exists {
                ProviderModelMutation::Conflict
            } else {
                ProviderModelMutation::NotFound
            });
        }
        replace_cost_rules(&mut tx, provider_id, model_id, &metadata.cost_rules()).await?;
        tx.commit().await?;
        Ok(ProviderModelMutation::Applied(Box::new(
            get_record(&mut connection, provider_id, model_id)
                .await?
                .context("updated Provider Model not found")?,
        )))
    }

    async fn reimport(
        &self,
        provider_id: &str,
        model_id: &str,
        input: ReimportProviderModel,
        validate_map: &crate::provider_models::ReimportThinkingMapValidator<'_>,
        before_commit: &(dyn Fn() -> anyhow::Result<()> + Send + Sync),
    ) -> anyhow::Result<ProviderModelReimport> {
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let snapshot_state = SnapshotState::Imported {
            source: SourceStamp::ProviderCatalog {
                provider_id: input.source_provider_id,
            },
        };
        let updated = update_record_metadata(
            &mut tx,
            provider_id,
            model_id,
            &input.metadata,
            &snapshot_state,
            input.expected_revision,
        )
        .await?;
        if !updated {
            return Ok(if model_exists(&mut tx, provider_id, model_id).await? {
                ProviderModelReimport::Conflict
            } else {
                ProviderModelReimport::NotFound
            });
        }
        replace_cost_rules(&mut tx, provider_id, model_id, &input.metadata.cost_rules()).await?;
        SqliteRouteStore::refresh_generated_thinking_maps(
            &mut tx,
            provider_id,
            model_id,
            &input.metadata,
            &input.generated_thinking_level_map,
            validate_map,
        )
        .await?;
        bump_config_epoch(&mut tx).await?;
        let model = get_record(&mut tx, provider_id, model_id)
            .await?
            .context("reimported Provider Model not found")?;
        let active_routes = SqliteRouteStore::load_routes(&mut tx, true).await?;
        before_commit()?;
        tx.commit().await?;
        Ok(ProviderModelReimport::Applied {
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
    ) -> anyhow::Result<ProviderModelMutation> {
        let mut connection = self.pool.acquire().await?;
        let result = sqlx::query(
            r#"UPDATE provider_models
               SET selection_policy = ?, revision = revision + 1, updated_at = datetime('now')
               WHERE provider_id = ? AND model_id = ? AND revision = ?"#,
        )
        .bind(policy.as_str())
        .bind(provider_id)
        .bind(model_id)
        .bind(expected_revision)
        .execute(&mut *connection)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(
                if get_record(&mut connection, provider_id, model_id)
                    .await?
                    .is_some()
                {
                    ProviderModelMutation::Conflict
                } else {
                    ProviderModelMutation::NotFound
                },
            );
        }
        Ok(ProviderModelMutation::Applied(Box::new(
            get_record(&mut connection, provider_id, model_id)
                .await?
                .context("updated Provider Model not found")?,
        )))
    }

    async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "DELETE FROM provider_models WHERE provider_id = ? AND model_id = ? AND source_kind = 'manual'",
        )
        .bind(provider_id)
        .bind(model_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

async fn get_record(
    connection: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<Option<ProviderModelRecord>> {
    let row = sqlx::query_as::<_, ProviderModelRow>(
        r#"SELECT provider_id, model_id, source_kind, snapshot_state, metadata_source_provider_id,
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
    connection: &mut SqliteConnection,
    provider_id: &str,
) -> anyhow::Result<BTreeMap<String, Vec<ProviderModelCostRule>>> {
    let rows = sqlx::query_as::<_, CostRuleRow>(
        r#"SELECT provider_id, model_id, rule_index, rule_kind, threshold_tokens,
                  cost_input, cost_output, cost_reasoning, cost_cache_read,
                  cost_cache_write, cost_input_audio, cost_output_audio
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
    connection: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<Vec<ProviderModelCostRule>> {
    sqlx::query_as::<_, CostRuleRow>(
        r#"SELECT provider_id, model_id, rule_index, rule_kind, threshold_tokens,
                  cost_input, cost_output, cost_reasoning, cost_cache_read,
                  cost_cache_write, cost_input_audio, cost_output_audio
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
            reasoning: parse_decimal(row.cost_reasoning)?,
            cache_read: parse_decimal(row.cost_cache_read)?,
            cache_write: parse_decimal(row.cost_cache_write)?,
            input_audio: parse_decimal(row.cost_input_audio)?,
            output_audio: parse_decimal(row.cost_output_audio)?,
        },
    })
}

async fn apply_discovered_metadata_update(
    tx: &mut Transaction<'_, Sqlite>,
    provider_id: &str,
    update: &ProviderModelPresenceUpdate,
) -> anyhow::Result<()> {
    let Some(metadata) = update.metadata.as_ref() else {
        return Ok(());
    };
    let limit = metadata.limit.as_ref();
    let prices = metadata.cost.as_ref().map(|cost| &cost.prices);
    let result = sqlx::query(
        r#"UPDATE provider_models SET
               presence = ?, lifecycle_status = ?, name = ?, family = ?, attachment = ?, reasoning = ?,
               tool_call = ?, open_weights = ?, structured_output = ?, temperature = ?,
               limit_context = ?, limit_input = ?, limit_output = ?,
               cost_input = ?, cost_output = ?, cost_reasoning = ?, cost_cache_read = ?,
               cost_cache_write = ?, cost_input_audio = ?, cost_output_audio = ?,
               metadata_json = ?, snapshot_state = COALESCE(?, snapshot_state), metadata_source_provider_id = ?, revision = revision + 1, updated_at = datetime('now')
           WHERE provider_id = ? AND model_id = ? AND source_kind = 'discovered' AND revision = ?"#,
    )
    .bind(update.presence.as_str())
    .bind(&metadata.status)
    .bind(&metadata.name)
    .bind(&metadata.family)
    .bind(metadata.attachment)
    .bind(metadata.reasoning)
    .bind(metadata.tool_call)
    .bind(metadata.open_weights)
    .bind(metadata.structured_output)
    .bind(metadata.temperature)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(limit.and_then(|limit| to_i64(limit.input)).transpose()?)
    .bind(limit.and_then(|limit| to_i64(limit.output)).transpose()?)
    .bind(decimal_text(prices.and_then(|prices| prices.input)))
    .bind(decimal_text(prices.and_then(|prices| prices.output)))
    .bind(decimal_text(prices.and_then(|prices| prices.reasoning)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_read)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_write)))
    .bind(decimal_text(prices.and_then(|prices| prices.input_audio)))
    .bind(decimal_text(prices.and_then(|prices| prices.output_audio)))
    .bind(serde_json::to_string(metadata)?)
    .bind(update.snapshot_state.as_ref().map(serde_json::to_string).transpose()?)
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
               provider_id, model_id, source_kind, snapshot_state, metadata_source_provider_id,
               presence, lifecycle_status, selection_policy, name, family,
               attachment, reasoning, tool_call, open_weights, structured_output, temperature,
               limit_context, limit_input, limit_output,
               cost_input, cost_output, cost_reasoning, cost_cache_read, cost_cache_write,
               cost_input_audio, cost_output_audio, metadata_json
           ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&input.provider_id)
    .bind(&input.model_id)
    .bind(input.source_kind.as_str())
    .bind(serde_json::to_string(&input.snapshot_state)?)
    .bind(&input.metadata_source_provider_id)
    .bind(input.presence.as_str())
    .bind(&input.metadata.status)
    .bind(input.selection_policy.as_str())
    .bind(&input.metadata.name)
    .bind(&input.metadata.family)
    .bind(input.metadata.attachment)
    .bind(input.metadata.reasoning)
    .bind(input.metadata.tool_call)
    .bind(input.metadata.open_weights)
    .bind(input.metadata.structured_output)
    .bind(input.metadata.temperature)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(limit.and_then(|limit| to_i64(limit.input)).transpose()?)
    .bind(limit.and_then(|limit| to_i64(limit.output)).transpose()?)
    .bind(decimal_text(prices.and_then(|prices| prices.input)))
    .bind(decimal_text(prices.and_then(|prices| prices.output)))
    .bind(decimal_text(prices.and_then(|prices| prices.reasoning)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_read)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_write)))
    .bind(decimal_text(prices.and_then(|prices| prices.input_audio)))
    .bind(decimal_text(prices.and_then(|prices| prices.output_audio)))
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
               lifecycle_status = ?, name = ?, family = ?, attachment = ?, reasoning = ?,
               tool_call = ?, open_weights = ?, structured_output = ?, temperature = ?,
               limit_context = ?, limit_input = ?, limit_output = ?,
               cost_input = ?, cost_output = ?, cost_reasoning = ?, cost_cache_read = ?,
               cost_cache_write = ?, cost_input_audio = ?, cost_output_audio = ?,
               metadata_json = ?, snapshot_state = ?, revision = revision + 1, updated_at = datetime('now')
           WHERE provider_id = ? AND model_id = ? AND revision = ?"#,
    )
    .bind(&metadata.status)
    .bind(&metadata.name)
    .bind(&metadata.family)
    .bind(metadata.attachment)
    .bind(metadata.reasoning)
    .bind(metadata.tool_call)
    .bind(metadata.open_weights)
    .bind(metadata.structured_output)
    .bind(metadata.temperature)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(limit.and_then(|limit| to_i64(limit.input)).transpose()?)
    .bind(limit.and_then(|limit| to_i64(limit.output)).transpose()?)
    .bind(decimal_text(prices.and_then(|prices| prices.input)))
    .bind(decimal_text(prices.and_then(|prices| prices.output)))
    .bind(decimal_text(prices.and_then(|prices| prices.reasoning)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_read)))
    .bind(decimal_text(prices.and_then(|prices| prices.cache_write)))
    .bind(decimal_text(prices.and_then(|prices| prices.input_audio)))
    .bind(decimal_text(prices.and_then(|prices| prices.output_audio)))
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
                   cost_input, cost_output, cost_reasoning, cost_cache_read, cost_cache_write,
                   cost_input_audio, cost_output_audio
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        .bind(decimal_text(rule.prices.reasoning))
        .bind(decimal_text(rule.prices.cache_read))
        .bind(decimal_text(rule.prices.cache_write))
        .bind(decimal_text(rule.prices.input_audio))
        .bind(decimal_text(rule.prices.output_audio))
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

    use super::*;
    use crate::provider_models::{ModelCost, ReasoningOption};
    use crate::thinking::{ThinkingLevelMapping, ThinkingMappingSource};

    #[tokio::test]
    async fn stale_reconciliation_rolls_back_all_sqlite_updates() {
        let data_dir = tempfile::tempdir().unwrap();
        let pool = crate::db::init_pool(data_dir.path()).await.unwrap();
        crate::migrations::migrate_sqlite(&pool).await.unwrap();
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
                .create(NewProviderModelRecord {
                    provider_id: "provider".into(),
                    model_id: model_id.into(),
                    source_kind: ProviderModelSourceKind::Discovered,
                    snapshot_state: SnapshotState::Unregistered,
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Present,
                    selection_policy: ProviderModelSelectionPolicy::Auto,
                    metadata: ProviderModelMetadata::bare(model_id),
                })
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
                SnapshotState::Edited { source: None },
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
                }),
                metadata_source_provider_id: None,
                presence: ProviderModelPresence::Missing,
                lifecycle_status: None,
                metadata: Some(ProviderModelMetadata::bare(model_id)),
            })
            .collect();
        assert!(
            storage
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
        let first = storage.get("provider", "first").await.unwrap().unwrap();
        let second = storage.get("provider", "second").await.unwrap().unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(first.presence, ProviderModelPresence::Present);
        assert_eq!(first.snapshot_state, SnapshotState::Unregistered);
        assert_eq!(second.metadata.name.as_deref(), Some("Admin correction"));
        assert_eq!(
            second.snapshot_state,
            SnapshotState::Edited { source: None }
        );
    }

    #[tokio::test]
    async fn create_waits_for_a_concurrent_sqlite_writer() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
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
                .create(NewProviderModelRecord {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    source_kind: ProviderModelSourceKind::Manual,
                    snapshot_state: SnapshotState::Edited { source: None },
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Present,
                    selection_policy: ProviderModelSelectionPolicy::Auto,
                    metadata: ProviderModelMetadata::bare("model"),
                })
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

    async fn reimport_fixture() -> (tempfile::TempDir, SqliteStorage) {
        let data_dir = tempfile::tempdir().unwrap();
        let pool = crate::db::init_pool(data_dir.path()).await.unwrap();
        crate::migrations::migrate_sqlite(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key)
             VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let storage = SqliteStorage::from_pool(pool);
        let mutation = storage
            .create(NewProviderModelRecord {
                provider_id: "provider".into(),
                model_id: "model".into(),
                source_kind: ProviderModelSourceKind::Discovered,
                snapshot_state: SnapshotState::Unregistered,
                metadata_source_provider_id: Some("catalog".into()),
                presence: ProviderModelPresence::Present,
                selection_policy: ProviderModelSelectionPolicy::Auto,
                metadata: ProviderModelMetadata::bare("model"),
            })
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
            reasoning_options: Some(vec![ReasoningOption::Effort {
                values: vec![Some("low".into()), Some("high".into())],
            }]),
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

    fn reimport_input(
        metadata: &ProviderModelMetadata,
        expected_revision: i64,
    ) -> ReimportProviderModel {
        ReimportProviderModel {
            metadata: metadata.clone(),
            source_provider_id: "catalog".into(),
            expected_revision,
            generated_thinking_level_map: crate::thinking::generate_thinking_level_map(metadata),
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
    async fn reimport_commits_model_targets_routes_and_epoch_atomically() {
        let (_data_dir, storage) = reimport_fixture().await;
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
        let input = reimport_input(&metadata, 1);

        // A manual override committed after the reimport input was prepared —
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

        let target_a_before = target_row(pool, "target-a").await;
        let target_c_map_before = target_map(pool, "target-c").await;

        let result = storage
            .reimport("provider", "model", input, &|_, _| Ok(()), &|| Ok(()))
            .await
            .unwrap();
        let ProviderModelReimport::Applied {
            model,
            active_routes,
        } = result
        else {
            panic!("reimport must apply")
        };

        assert_eq!(model.revision, 2);
        assert_eq!(model.metadata.name.as_deref(), Some("Refreshed Model"));
        assert_eq!(
            model.snapshot_state,
            SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog".into(),
                },
            }
        );
        assert_eq!(model.cost_rules.len(), 1);
        assert_eq!(
            model.cost_rules[0].kind,
            ProviderModelCostRuleKind::ContextOver200k
        );
        assert_eq!(config_epoch(pool).await.as_deref(), Some("1"));

        // Generated rows follow the reimported map; the Overridden row survives.
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
    async fn reimport_rolls_back_everything_when_a_target_update_fails() {
        let (_data_dir, storage) = reimport_fixture().await;
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
        sqlx::query("INSERT INTO settings (name, value) VALUES ('config_epoch', '7')")
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
            .reimport(
                "provider",
                "model",
                reimport_input(&refreshed_metadata(), 1),
                &|_, _| Ok(()),
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
    async fn reimport_rolls_back_when_the_epoch_write_fails() {
        let (_data_dir, storage) = reimport_fixture().await;
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
        sqlx::query("INSERT INTO settings (name, value) VALUES ('config_epoch', '3')")
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

        let result = storage
            .reimport(
                "provider",
                "model",
                reimport_input(&refreshed_metadata(), 1),
                &|_, _| Ok(()),
                &|| Ok(()),
            )
            .await;
        assert!(result.is_err());

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert_eq!(config_epoch(pool).await.as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn reimport_rolls_back_when_the_final_permit_check_fails() {
        let (_data_dir, storage) = reimport_fixture().await;
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
        sqlx::query("INSERT INTO settings (name, value) VALUES ('config_epoch', '4')")
            .execute(pool)
            .await
            .unwrap();

        let model_before = storage.get("provider", "model").await.unwrap().unwrap();
        let map_before = target_map(pool, "target-a").await;

        let result = storage
            .reimport(
                "provider",
                "model",
                reimport_input(&refreshed_metadata(), 1),
                &|_, _| Ok(()),
                &|| anyhow::bail!("vendor write permit revoked"),
            )
            .await;
        assert!(result.is_err());

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert_eq!(config_epoch(pool).await.as_deref(), Some("4"));
    }

    #[tokio::test]
    async fn reimport_conflict_and_not_found_write_nothing() {
        let (_data_dir, storage) = reimport_fixture().await;
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

        let stale = storage
            .reimport(
                "provider",
                "model",
                reimport_input(&refreshed_metadata(), 99),
                &|_, _| Ok(()),
                &|| Ok(()),
            )
            .await
            .unwrap();
        assert!(matches!(stale, ProviderModelReimport::Conflict));
        let missing = storage
            .reimport(
                "provider",
                "absent",
                reimport_input(&refreshed_metadata(), 1),
                &|_, _| Ok(()),
                &|| Ok(()),
            )
            .await
            .unwrap();
        assert!(matches!(missing, ProviderModelReimport::NotFound));

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert!(config_epoch(pool).await.is_none());
    }

    #[tokio::test]
    async fn reimport_rolls_back_when_the_generated_map_is_incomplete() {
        let (_data_dir, storage) = reimport_fixture().await;
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

        let mut input = reimport_input(&refreshed_metadata(), 1);
        input
            .generated_thinking_level_map
            .retain(|row| row.level != ThinkingLevel::Low);
        let result = storage
            .reimport("provider", "model", input, &|_, _| Ok(()), &|| Ok(()))
            .await;
        assert!(result.is_err());

        assert_eq!(
            storage.get("provider", "model").await.unwrap().unwrap(),
            model_before
        );
        assert_eq!(target_map(pool, "target-a").await, map_before);
        assert!(config_epoch(pool).await.is_none());
    }

    #[tokio::test]
    async fn reimport_treats_an_invalid_epoch_as_zero() {
        let (_data_dir, storage) = reimport_fixture().await;
        let pool = storage.pool();
        sqlx::query("INSERT INTO settings (name, value) VALUES ('config_epoch', 'bogus')")
            .execute(pool)
            .await
            .unwrap();

        let result = storage
            .reimport(
                "provider",
                "model",
                reimport_input(&refreshed_metadata(), 1),
                &|_, _| Ok(()),
                &|| Ok(()),
            )
            .await
            .unwrap();
        assert!(matches!(result, ProviderModelReimport::Applied { .. }));
        assert_eq!(config_epoch(pool).await.as_deref(), Some("1"));
    }
}
