use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::Context;
use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::{PgConnection, Postgres, Transaction};

use super::PostgresStorage;
use crate::provider_models::{
    NewProviderModelRecord, PriceComponents, ProviderModelCostRule, ProviderModelCostRuleKind,
    ProviderModelMetadata, ProviderModelMutation, ProviderModelPresence,
    ProviderModelPresenceUpdate, ProviderModelReconciliation, ProviderModelRecord,
    ProviderModelReimport, ProviderModelSelectionPolicy, ProviderModelSourceKind,
    ReimportProviderModel, SnapshotState, SourceStamp,
};
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
    rule_index: i32,
    rule_kind: String,
    threshold_tokens: i64,
    cost_input: Option<Decimal>,
    cost_output: Option<Decimal>,
    cost_reasoning: Option<Decimal>,
    cost_cache_read: Option<Decimal>,
    cost_cache_write: Option<Decimal>,
    cost_input_audio: Option<Decimal>,
    cost_output_audio: Option<Decimal>,
}

#[async_trait]
impl ProviderModelStore for PostgresStorage {
    async fn list_for_provider(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderModelRecord>> {
        let mut conn = self.pool.acquire().await?;
        let rows = sqlx::query_as::<_, ProviderModelRow>(
            r#"SELECT provider_id, model_id, source_kind, snapshot_state::text AS snapshot_state, metadata_source_provider_id,
                      presence, selection_policy, metadata_json::text AS metadata_json,
                      revision, created_at::text AS created_at, updated_at::text AS updated_at
               FROM provider_models
               WHERE provider_id = $1
               ORDER BY LOWER(COALESCE(name, model_id)), model_id"#,
        )
        .bind(provider_id)
        .fetch_all(&mut *conn)
        .await?;
        let rules = load_rules_for_provider(&mut conn, provider_id).await?;
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
        let mut conn = self.pool.acquire().await?;
        get_record(&mut conn, provider_id, model_id).await
    }

    async fn apply_reconciliation(
        &self,
        provider_id: &str,
        reconciliation: ProviderModelReconciliation,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let has_writes = !reconciliation.updates.is_empty() || !reconciliation.inserts.is_empty();
        for update in &reconciliation.updates {
            let revision = sqlx::query_scalar::<_, i64>(
                "SELECT revision FROM provider_models WHERE provider_id = $1 AND model_id = $2 AND source_kind = 'discovered' FOR UPDATE",
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
                "SELECT COUNT(*) FROM provider_models WHERE provider_id = $1 AND model_id = $2",
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
                "SELECT metadata_json::text FROM provider_models WHERE provider_id = $1 AND model_id = $2 AND source_kind = 'discovered'",
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
                   SET presence = $1, lifecycle_status = $2, metadata_source_provider_id = $3, metadata_json = $4::jsonb,
                       revision = revision + 1, updated_at = NOW()
                   WHERE provider_id = $5 AND model_id = $6 AND source_kind = 'discovered' AND revision = $7"#,
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
        // Presence/metadata changes alter model resolution and prices; notify
        // other replicas in the same commit. An empty reconciliation writes
        // nothing and must not publish a spurious epoch.
        if has_writes {
            bump_config_epoch(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn create(&self, input: NewProviderModelRecord) -> anyhow::Result<ProviderModelMutation> {
        let mut tx = self.pool.begin().await?;
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM provider_models WHERE provider_id = $1 AND model_id = $2",
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
        bump_config_epoch(&mut tx).await?;
        let record = get_record(&mut tx, &provider_id, &model_id)
            .await?
            .context("created Provider Model not found")?;
        tx.commit().await?;
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
        let mut tx = self.pool.begin().await?;
        let updated = update_record_metadata(
            &mut tx,
            provider_id,
            model_id,
            &metadata,
            &snapshot_state,
            expected_revision,
        )
        .await?;
        if !updated {
            let exists = model_exists(&mut tx, provider_id, model_id).await?;
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
        Ok(ProviderModelMutation::Applied(Box::new(record)))
    }

    async fn reimport(
        &self,
        provider_id: &str,
        model_id: &str,
        input: ReimportProviderModel,
        validate_map: &crate::provider_models::ReimportThinkingMapValidator<'_>,
        before_commit: &(dyn Fn() -> anyhow::Result<()> + Send + Sync),
    ) -> anyhow::Result<ProviderModelReimport> {
        let mut tx = self.pool.begin().await?;
        // 先冻结 Route 写集合，才能覆盖等待期间新绑定的 Target，并让
        // 返回的完整快照处于同一写入边界。锁序与 RouteStore::put 一致。
        sqlx::query("LOCK TABLE models, model_backends IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        // Every Provider Model writer takes this row lock before touching cost
        // rules, so the locked read serializes the revision check.
        let revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM provider_models WHERE provider_id = $1 AND model_id = $2 FOR UPDATE",
        )
        .bind(provider_id)
        .bind(model_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(revision) = revision else {
            return Ok(ProviderModelReimport::NotFound);
        };
        if revision != input.expected_revision {
            return Ok(ProviderModelReimport::Conflict);
        }
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
        anyhow::ensure!(updated, "Provider Model changed during reimport");
        replace_cost_rules(&mut tx, provider_id, model_id, &input.metadata.cost_rules()).await?;
        super::routes::refresh_generated_target_maps(
            &mut tx,
            provider_id,
            model_id,
            &input.metadata,
            &input.generated_thinking_level_map,
            validate_map,
        )
        .await?;
        bump_config_epoch(&mut tx).await?;
        // Prepare the full return snapshot inside the transaction: no fallible
        // reads happen after commit.
        let model = get_record(&mut tx, provider_id, model_id)
            .await?
            .context("reimported Provider Model not found")?;
        let active_routes = super::routes::load_routes(&mut tx, true).await?;
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
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"UPDATE provider_models
               SET selection_policy = $1, revision = revision + 1, updated_at = NOW()
               WHERE provider_id = $2 AND model_id = $3 AND revision = $4"#,
        )
        .bind(policy.as_str())
        .bind(provider_id)
        .bind(model_id)
        .bind(expected_revision)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(
                if get_record(&mut tx, provider_id, model_id).await?.is_some() {
                    ProviderModelMutation::Conflict
                } else {
                    ProviderModelMutation::NotFound
                },
            );
        }
        // Selection policy gates whether the resolved model may serve traffic;
        // publish the change atomically with the write it accompanies.
        bump_config_epoch(&mut tx).await?;
        let record = get_record(&mut tx, provider_id, model_id)
            .await?
            .context("updated Provider Model not found")?;
        tx.commit().await?;
        Ok(ProviderModelMutation::Applied(Box::new(record)))
    }

    async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "DELETE FROM provider_models WHERE provider_id = $1 AND model_id = $2 AND source_kind = 'manual'",
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
        Ok(deleted)
    }
}

async fn get_record(
    conn: &mut PgConnection,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<Option<ProviderModelRecord>> {
    let row = sqlx::query_as::<_, ProviderModelRow>(
        r#"SELECT provider_id, model_id, source_kind, snapshot_state::text AS snapshot_state, metadata_source_provider_id,
                  presence, selection_policy, metadata_json::text AS metadata_json,
                  revision, created_at::text AS created_at, updated_at::text AS updated_at
           FROM provider_models
           WHERE provider_id = $1 AND model_id = $2"#,
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let rules = load_rules_for_model(conn, provider_id, model_id).await?;
    decode_record(row, rules).map(Some)
}

async fn load_rules_for_provider(
    conn: &mut PgConnection,
    provider_id: &str,
) -> anyhow::Result<BTreeMap<String, Vec<ProviderModelCostRule>>> {
    let rows = sqlx::query_as::<_, CostRuleRow>(
        r#"SELECT provider_id, model_id, rule_index, rule_kind, threshold_tokens,
                  cost_input, cost_output, cost_reasoning, cost_cache_read,
                  cost_cache_write, cost_input_audio, cost_output_audio
           FROM provider_model_cost_rules
           WHERE provider_id = $1
           ORDER BY model_id, rule_index"#,
    )
    .bind(provider_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut rules = BTreeMap::<String, Vec<ProviderModelCostRule>>::new();
    for row in rows {
        let model_id = row.model_id.clone();
        rules.entry(model_id).or_default().push(decode_rule(row)?);
    }
    Ok(rules)
}

async fn load_rules_for_model(
    conn: &mut PgConnection,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<Vec<ProviderModelCostRule>> {
    sqlx::query_as::<_, CostRuleRow>(
        r#"SELECT provider_id, model_id, rule_index, rule_kind, threshold_tokens,
                  cost_input, cost_output, cost_reasoning, cost_cache_read,
                  cost_cache_write, cost_input_audio, cost_output_audio
           FROM provider_model_cost_rules
           WHERE provider_id = $1 AND model_id = $2
           ORDER BY rule_index"#,
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_all(&mut *conn)
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
        rule_index: i64::from(row.rule_index),
        kind: ProviderModelCostRuleKind::from_str(&row.rule_kind)?,
        threshold_tokens: u64::try_from(row.threshold_tokens)
            .context("negative Provider Model cost threshold")?,
        prices: PriceComponents {
            input: row.cost_input,
            output: row.cost_output,
            reasoning: row.cost_reasoning,
            cache_read: row.cost_cache_read,
            cache_write: row.cost_cache_write,
            input_audio: row.cost_input_audio,
            output_audio: row.cost_output_audio,
        },
    })
}

async fn apply_discovered_metadata_update(
    tx: &mut Transaction<'_, Postgres>,
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
               presence = $1, lifecycle_status = $2, name = $3, family = $4, open_weights = $5, limit_context = $6, cost_input = $7, cost_output = $8, cost_reasoning = $9, cost_cache_read = $10,
               cost_cache_write = $11, cost_input_audio = $12, cost_output_audio = $13,
               metadata_json = $14::jsonb, snapshot_state = COALESCE($15::jsonb, snapshot_state), metadata_source_provider_id = $16, revision = revision + 1, updated_at = NOW()
           WHERE provider_id = $17 AND model_id = $18 AND source_kind = 'discovered' AND revision = $19"#,
    )
    .bind(update.presence.as_str())
    .bind(&metadata.status)
    .bind(&metadata.name)
    .bind(&metadata.family)
    .bind(metadata.open_weights)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(prices.and_then(|prices| prices.input))
    .bind(prices.and_then(|prices| prices.output))
    .bind(prices.and_then(|prices| prices.reasoning))
    .bind(prices.and_then(|prices| prices.cache_read))
    .bind(prices.and_then(|prices| prices.cache_write))
    .bind(prices.and_then(|prices| prices.input_audio))
    .bind(prices.and_then(|prices| prices.output_audio))
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
    tx: &mut Transaction<'_, Postgres>,
    input: NewProviderModelRecord,
) -> anyhow::Result<()> {
    let metadata_json = serde_json::to_string(&input.metadata)?;
    let limit = input.metadata.limit.as_ref();
    let prices = input.metadata.cost.as_ref().map(|cost| &cost.prices);
    sqlx::query(
        r#"INSERT INTO provider_models (
               provider_id, model_id, source_kind, snapshot_state, metadata_source_provider_id,
               presence, lifecycle_status, selection_policy, name, family,
               open_weights, limit_context, cost_input, cost_output, cost_reasoning, cost_cache_read, cost_cache_write,
               cost_input_audio, cost_output_audio, metadata_json
           ) VALUES ($1, $2, $3, $4::jsonb, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20::jsonb)"#,
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
    .bind(input.metadata.open_weights)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(prices.and_then(|prices| prices.input))
    .bind(prices.and_then(|prices| prices.output))
    .bind(prices.and_then(|prices| prices.reasoning))
    .bind(prices.and_then(|prices| prices.cache_read))
    .bind(prices.and_then(|prices| prices.cache_write))
    .bind(prices.and_then(|prices| prices.input_audio))
    .bind(prices.and_then(|prices| prices.output_audio))
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
    tx: &mut Transaction<'_, Postgres>,
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
               lifecycle_status = $1, name = $2, family = $3, open_weights = $4, limit_context = $5, cost_input = $6, cost_output = $7, cost_reasoning = $8, cost_cache_read = $9,
               cost_cache_write = $10, cost_input_audio = $11, cost_output_audio = $12,
               metadata_json = $13::jsonb, snapshot_state = $14::jsonb, revision = revision + 1, updated_at = NOW()
           WHERE provider_id = $15 AND model_id = $16 AND revision = $17"#,
    )
    .bind(&metadata.status)
    .bind(&metadata.name)
    .bind(&metadata.family)
    .bind(metadata.open_weights)
    .bind(limit.and_then(|limit| to_i64(limit.context)).transpose()?)
    .bind(prices.and_then(|prices| prices.input))
    .bind(prices.and_then(|prices| prices.output))
    .bind(prices.and_then(|prices| prices.reasoning))
    .bind(prices.and_then(|prices| prices.cache_read))
    .bind(prices.and_then(|prices| prices.cache_write))
    .bind(prices.and_then(|prices| prices.input_audio))
    .bind(prices.and_then(|prices| prices.output_audio))
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
    tx: &mut Transaction<'_, Postgres>,
    provider_id: &str,
    model_id: &str,
    rules: &[ProviderModelCostRule],
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM provider_model_cost_rules WHERE provider_id = $1 AND model_id = $2")
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
               ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
        )
        .bind(provider_id)
        .bind(model_id)
        .bind(i32::try_from(rule.rule_index).context("cost rule index exceeds database range")?)
        .bind(rule.kind.as_str())
        .bind(
            i64::try_from(rule.threshold_tokens)
                .context("cost threshold exceeds database range")?,
        )
        .bind(rule.prices.input)
        .bind(rule.prices.output)
        .bind(rule.prices.reasoning)
        .bind(rule.prices.cache_read)
        .bind(rule.prices.cache_write)
        .bind(rule.prices.input_audio)
        .bind(rule.prices.output_audio)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn model_exists(
    tx: &mut Transaction<'_, Postgres>,
    provider_id: &str,
    model_id: &str,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM provider_models WHERE provider_id = $1 AND model_id = $2",
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_one(&mut **tx)
    .await?
        > 0)
}

/// 持有 epoch 行锁直至整笔配置提交；解析与溢出行为与其他后端一致。
async fn bump_config_epoch(tx: &mut Transaction<'_, Postgres>) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO settings (name, value, updated_at)
           VALUES ($1, '0', CURRENT_TIMESTAMP)
           ON CONFLICT (name) DO NOTHING"#,
    )
    .bind(crate::storage::CONFIG_EPOCH_KEY)
    .execute(&mut **tx)
    .await?;
    let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE name = $1 FOR UPDATE")
        .bind(crate::storage::CONFIG_EPOCH_KEY)
        .fetch_one(&mut **tx)
        .await?;
    let next_epoch = value
        .parse::<i64>()
        .unwrap_or(0)
        .checked_add(1)
        .context("config epoch overflow")?;
    sqlx::query("UPDATE settings SET value = $1, updated_at = CURRENT_TIMESTAMP WHERE name = $2")
        .bind(next_epoch.to_string())
        .bind(crate::storage::CONFIG_EPOCH_KEY)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn to_i64(value: Option<u64>) -> Option<anyhow::Result<i64>> {
    value.map(|value| {
        i64::try_from(value).context("Provider Model token limit exceeds database range")
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

    #[tokio::test]
    async fn model_specification_upgrade_preserves_data_and_target_maps() {
        let Some((admin, pool, schema, _)) = postgres_storage_before_cutover(true).await else {
            return;
        };
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
        sqlx::query("UPDATE model_backends SET thinking_level_map = $1::jsonb WHERE id = 'target'")
            .bind(retained_map)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(r#"INSERT INTO provider_models (provider_id, model_id, source_kind, presence, metadata_json)
            VALUES ('provider', 'toggle-only', 'manual', 'present', '{"id":"toggle-only","reasoning":true,"reasoning_options":[{"type":"toggle"},{"type":"effort","values":[null,"default",""]}]}'),
                   ('provider', 'already-explicit', 'manual', 'present', '{"id":"already-explicit","reasoning_efforts":[null,"default","null","xhigh","vendor-effort","xhigh"],"reasoning_options":[{"type":"effort","values":["xhigh","stale-effort"]}]}'),
                   ('provider', 'explicit-empty', 'manual', 'present', '{"id":"explicit-empty","reasoning_efforts":[],"reasoning_options":[{"type":"effort","values":["stale-effort"]}]}');"#)
            .execute(&pool).await.unwrap();
        let original_map: String = sqlx::query_scalar(
            "SELECT thinking_level_map::text FROM model_backends WHERE id = 'target'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../migrations/postgres/0006_model_specification.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let json: String = sqlx::query_scalar("SELECT metadata_json::text FROM provider_models WHERE provider_id = 'provider' AND model_id = 'model'").fetch_one(&pool).await.unwrap();
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
            "SELECT metadata_json::text FROM provider_models WHERE model_id = 'toggle-only'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let empty: serde_json::Value = serde_json::from_str(&empty_json).unwrap();
        assert!(empty.get("reasoning_efforts").is_none());
        assert!(empty.get("reasoning_options").is_none());
        let explicit_json: String = sqlx::query_scalar(
            "SELECT metadata_json::text FROM provider_models WHERE model_id = 'already-explicit'",
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
            "SELECT metadata_json::text FROM provider_models WHERE model_id = 'explicit-empty'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let explicit_empty: serde_json::Value = serde_json::from_str(&explicit_empty_json).unwrap();
        assert!(explicit_empty.get("reasoning_efforts").is_none());
        let map: String = sqlx::query_scalar(
            "SELECT thinking_level_map::text FROM model_backends WHERE id = 'target'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(map, original_map);
        let state: (i64, Option<String>) = sqlx::query_as("SELECT revision, metadata_source_provider_id FROM provider_models WHERE model_id = 'model'").fetch_one(&pool).await.unwrap();
        assert_eq!(state, (7, Some("catalog".into())));
        cleanup(admin, pool, &schema).await;
    }

    use super::*;
    use crate::provider_models::ModelCost;
    use crate::thinking::{ThinkingLevelMapping, ThinkingMappingSource};

    async fn postgres_storage() -> Option<(PgPool, PgPool, String, PostgresStorage)> {
        postgres_storage_before_cutover(false).await
    }

    async fn postgres_storage_before_cutover(
        before_cutover: bool,
    ) -> Option<(PgPool, PgPool, String, PostgresStorage)> {
        let Ok(url) = std::env::var("DB_URL") else {
            eprintln!("skip PostgreSQL reimport verification: DB_URL is not set");
            return None;
        };
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("PostgreSQL admin pool");
        let schema = format!("stravia_reimport_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create isolated PostgreSQL schema");
        let options: sqlx::postgres::PgConnectOptions =
            url.parse().expect("PostgreSQL connection options");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await
            .expect("isolated PostgreSQL pool");
        if before_cutover {
            for migration in [
                include_str!("../../../migrations/postgres/0001_baseline.sql"),
                include_str!("../../../migrations/postgres/0002_data_contracts.sql"),
                include_str!("../../../migrations/postgres/0003_estimated_input_tokens.sql"),
                include_str!("../../../migrations/postgres/0004_observation_recovery_indexes.sql"),
                include_str!("../../../migrations/postgres/0005_credential_custom_rules.sql"),
            ] {
                sqlx::raw_sql(migration)
                    .execute(&pool)
                    .await
                    .expect("pre-cutover schema");
            }
        } else {
            crate::migrations::migrate_postgres(&pool, None)
                .await
                .expect("PostgreSQL migrations");
        }
        let storage = PostgresStorage::from_pool(pool.clone());
        Some((admin, pool, schema, storage))
    }

    async fn cleanup(admin: PgPool, pool: PgPool, schema: &str) {
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .expect("drop isolated PostgreSQL schema");
        admin.close().await;
    }

    async fn seed_provider_and_model(storage: &PostgresStorage, pool: &PgPool) {
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key) VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key')",
        )
        .execute(pool)
        .await
        .expect("seed Provider");
        storage
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
            .expect("seed Provider Model");
    }

    async fn insert_route(pool: &PgPool, storage_id: &str, route_id: &str, enabled: bool) {
        sqlx::query("INSERT INTO models (id, model_id, is_enabled) VALUES ($1, $2, $3)")
            .bind(storage_id)
            .bind(route_id)
            .bind(enabled)
            .execute(pool)
            .await
            .expect("insert Route");
    }

    async fn insert_target(
        pool: &PgPool,
        target_id: &str,
        route_storage_id: &str,
        model: Option<&str>,
        priority: i32,
        map: &[ThinkingLevelMapping],
    ) {
        sqlx::query(
            "INSERT INTO model_backends (id, model_id, provider_id, model, enabled, priority, thinking_level_map) VALUES ($1, $2, 'provider', $3, true, $4, $5)",
        )
        .bind(target_id)
        .bind(route_storage_id)
        .bind(model)
        .bind(priority)
        .bind(sqlx::types::Json(map))
        .execute(pool)
        .await
        .expect("insert Target");
    }

    async fn target_map_in_db(pool: &PgPool, target_id: &str) -> Vec<ThinkingLevelMapping> {
        sqlx::query_scalar::<_, sqlx::types::Json<Vec<ThinkingLevelMapping>>>(
            "SELECT thinking_level_map FROM model_backends WHERE id = $1",
        )
        .bind(target_id)
        .fetch_one(pool)
        .await
        .expect("Target Thinking Level Map")
        .0
    }

    async fn config_epoch(pool: &PgPool) -> Option<i64> {
        sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE name = $1")
            .bind(crate::storage::CONFIG_EPOCH_KEY)
            .fetch_optional(pool)
            .await
            .expect("config_epoch")
            .map(|value| value.parse().expect("numeric config_epoch"))
    }

    /// Every level Generated+Hidden except the listed Overridden rows.
    fn target_map(
        overrides: &[(ThinkingLevel, TargetThinkingControl)],
    ) -> Vec<ThinkingLevelMapping> {
        ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                if let Some((_, control)) =
                    overrides.iter().find(|(candidate, _)| *candidate == level)
                {
                    ThinkingLevelMapping {
                        level,
                        control: control.clone(),
                        source: ThinkingMappingSource::Overridden,
                    }
                } else {
                    ThinkingLevelMapping {
                        level,
                        control: TargetThinkingControl::Hidden,
                        source: ThinkingMappingSource::Generated,
                    }
                }
            })
            .collect()
    }

    /// A catalog-fresh Generated map: `low` becomes Enabled, the rest Hidden.
    fn fresh_generated_map() -> Vec<ThinkingLevelMapping> {
        ThinkingLevel::ALL
            .into_iter()
            .map(|level| ThinkingLevelMapping {
                level,
                control: if level == ThinkingLevel::Low {
                    TargetThinkingControl::Enabled
                } else {
                    TargetThinkingControl::Hidden
                },
                source: ThinkingMappingSource::Generated,
            })
            .collect()
    }

    fn reimport_input(expected_revision: i64) -> ReimportProviderModel {
        ReimportProviderModel {
            metadata: ProviderModelMetadata {
                name: Some("Fresh catalog name".into()),
                cost: Some(ModelCost {
                    prices: PriceComponents::default(),
                    context_over_200k: Some(PriceComponents {
                        input: Some(Decimal::new(3, 0)),
                        ..PriceComponents::default()
                    }),
                    tiers: Vec::new(),
                }),
                ..ProviderModelMetadata::bare("model")
            },
            source_provider_id: "catalog".into(),
            expected_revision,
            generated_thinking_level_map: fresh_generated_map(),
        }
    }

    fn map_row(map: &[ThinkingLevelMapping], level: ThinkingLevel) -> &ThinkingLevelMapping {
        map.iter().find(|row| row.level == level).expect("map row")
    }

    #[tokio::test]
    async fn reimport_applies_snapshot_targets_and_epoch_atomically() {
        let Some((admin, pool, schema, storage)) = postgres_storage().await else {
            return;
        };
        seed_provider_and_model(&storage, &pool).await;
        insert_route(&pool, "route-1", "route-a", true).await;
        insert_route(&pool, "route-2", "route-b", false).await;
        insert_target(
            &pool,
            "target-1",
            "route-1",
            Some("model"),
            5,
            &target_map(&[(
                ThinkingLevel::High,
                TargetThinkingControl::Effort {
                    value: "high".into(),
                },
            )]),
        )
        .await;
        insert_target(
            &pool,
            "target-2",
            "route-2",
            Some("model"),
            0,
            &target_map(&[]),
        )
        .await;
        insert_target(
            &pool,
            "target-3",
            "route-1",
            Some("other-model"),
            0,
            &target_map(&[]),
        )
        .await;
        insert_target(&pool, "target-4", "route-1", None, 0, &[]).await;
        let epoch_before = config_epoch(&pool).await.unwrap();

        let result = storage
            .reimport(
                "provider",
                "model",
                reimport_input(1),
                &|_, _| Ok(()),
                &|| anyhow::Ok(()),
            )
            .await
            .expect("reimport");
        let ProviderModelReimport::Applied {
            model,
            active_routes,
        } = result
        else {
            panic!("reimport must apply");
        };

        assert_eq!(model.revision, 2);
        assert_eq!(model.metadata.name.as_deref(), Some("Fresh catalog name"));
        assert_eq!(
            model.metadata_source_provider_id.as_deref(),
            Some("catalog")
        );
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
        assert_eq!(config_epoch(&pool).await, Some(epoch_before + 1));

        // The complete active Route snapshot was prepared inside the transaction.
        assert_eq!(active_routes.len(), 1);
        let route = &active_routes[0];
        assert_eq!(route.model_id.as_str(), "route-a");
        let target = route
            .targets
            .iter()
            .find(|target| target.id.as_str() == "target-1")
            .expect("matched Target");
        // The Target row was updated in place: identity and priority survive.
        assert_eq!(target.priority, 5);
        let high = map_row(&target.thinking_level_map, ThinkingLevel::High);
        assert_eq!(high.source, ThinkingMappingSource::Overridden);
        assert_eq!(
            high.control,
            TargetThinkingControl::Effort {
                value: "high".into()
            }
        );
        let low = map_row(&target.thinking_level_map, ThinkingLevel::Low);
        assert_eq!(low.source, ThinkingMappingSource::Generated);
        assert_eq!(low.control, TargetThinkingControl::Enabled);

        // Unrelated and provider-only Targets stay untouched; the disabled
        // Route's matched Target was still refreshed in place.
        let other = target_map_in_db(&pool, "target-3").await;
        assert_eq!(
            map_row(&other, ThinkingLevel::Low).control,
            TargetThinkingControl::Hidden
        );
        assert!(target_map_in_db(&pool, "target-4").await.is_empty());
        let disabled = target_map_in_db(&pool, "target-2").await;
        assert_eq!(
            map_row(&disabled, ThinkingLevel::Low).control,
            TargetThinkingControl::Enabled
        );

        cleanup(admin, pool, &schema).await;
    }

    #[tokio::test]
    async fn reimport_conflict_and_not_found_leave_state_untouched() {
        let Some((admin, pool, schema, storage)) = postgres_storage().await else {
            return;
        };
        seed_provider_and_model(&storage, &pool).await;
        insert_route(&pool, "route-1", "route-a", true).await;
        insert_target(
            &pool,
            "target-1",
            "route-1",
            Some("model"),
            0,
            &target_map(&[]),
        )
        .await;

        let epoch_before = config_epoch(&pool).await;
        let conflict = storage
            .reimport(
                "provider",
                "model",
                reimport_input(99),
                &|_, _| Ok(()),
                &|| anyhow::Ok(()),
            )
            .await
            .expect("stale revision check");
        assert!(matches!(conflict, ProviderModelReimport::Conflict));
        let missing = storage
            .reimport(
                "provider",
                "ghost",
                reimport_input(1),
                &|_, _| Ok(()),
                &|| anyhow::Ok(()),
            )
            .await
            .expect("missing model check");
        assert!(matches!(missing, ProviderModelReimport::NotFound));

        let model = storage
            .get("provider", "model")
            .await
            .unwrap()
            .expect("Provider Model");
        assert_eq!(model.revision, 1);
        assert_eq!(model.snapshot_state, SnapshotState::Unregistered);
        assert_eq!(model.metadata.name.as_deref(), Some("model"));
        assert_eq!(config_epoch(&pool).await, epoch_before);
        assert_eq!(
            map_row(
                &target_map_in_db(&pool, "target-1").await,
                ThinkingLevel::Low
            )
            .control,
            TargetThinkingControl::Hidden
        );

        cleanup(admin, pool, &schema).await;
    }

    #[tokio::test]
    async fn reimport_permit_failure_rolls_back_everything() {
        let Some((admin, pool, schema, storage)) = postgres_storage().await else {
            return;
        };
        seed_provider_and_model(&storage, &pool).await;
        insert_route(&pool, "route-1", "route-a", true).await;
        insert_target(
            &pool,
            "target-1",
            "route-1",
            Some("model"),
            0,
            &target_map(&[]),
        )
        .await;

        let epoch_before = config_epoch(&pool).await;
        // A permit rejection must surface verbatim, not as a storage/sqlx error.
        let error = storage
            .reimport(
                "provider",
                "model",
                reimport_input(1),
                &|_, _| Ok(()),
                &|| anyhow::bail!("vendor write permit revoked"),
            )
            .await
            .expect_err("permit failure must abort reimport");
        assert!(
            error.to_string().contains("vendor write permit revoked"),
            "{error}"
        );

        let model = storage
            .get("provider", "model")
            .await
            .unwrap()
            .expect("Provider Model");
        assert_eq!(model.revision, 1);
        assert_eq!(model.snapshot_state, SnapshotState::Unregistered);
        assert_eq!(model.metadata.name.as_deref(), Some("model"));
        assert!(model.cost_rules.is_empty());
        assert_eq!(config_epoch(&pool).await, epoch_before);
        assert_eq!(
            map_row(
                &target_map_in_db(&pool, "target-1").await,
                ThinkingLevel::Low
            )
            .control,
            TargetThinkingControl::Hidden
        );

        cleanup(admin, pool, &schema).await;
    }

    #[tokio::test]
    async fn reimport_map_failure_rolls_back_everything() {
        let Some((admin, pool, schema, storage)) = postgres_storage().await else {
            return;
        };
        seed_provider_and_model(&storage, &pool).await;
        insert_route(&pool, "route-1", "route-a", true).await;
        let original_map = target_map(&[(
            ThinkingLevel::High,
            TargetThinkingControl::Budget { value: 2048 },
        )]);
        insert_target(
            &pool,
            "target-1",
            "route-1",
            Some("model"),
            0,
            &original_map,
        )
        .await;

        let epoch_before = config_epoch(&pool).await;
        // Generated 行未变，也必须检查协议无法表达的现有 Overridden 行；
        // 此时快照与成本规则已写入事务，校验失败必须全部回滚。
        let mut input = reimport_input(1);
        input.metadata.reasoning_efforts = None;
        input.generated_thinking_level_map = target_map(&[]);
        let result = storage
            .reimport(
                "provider",
                "model",
                input,
                &|metadata, map| {
                    anyhow::ensure!(
                        map.iter().all(|row| crate::thinking::control_is_writable(
                            "openai-compatible",
                            metadata,
                            false,
                            &row.control
                        )),
                        "unsupported manual control"
                    );
                    Ok(())
                },
                &|| Ok(()),
            )
            .await;
        assert!(result.is_err());

        let model = storage
            .get("provider", "model")
            .await
            .unwrap()
            .expect("Provider Model");
        assert_eq!(model.revision, 1);
        assert_eq!(model.snapshot_state, SnapshotState::Unregistered);
        assert!(model.cost_rules.is_empty());
        assert_eq!(config_epoch(&pool).await, epoch_before);
        assert_eq!(target_map_in_db(&pool, "target-1").await, original_map);

        cleanup(admin, pool, &schema).await;
    }

    #[tokio::test]
    async fn reimport_waits_for_route_writer_and_uses_latest_override() {
        let Some((admin, pool, schema, storage)) = postgres_storage().await else {
            return;
        };
        seed_provider_and_model(&storage, &pool).await;
        insert_route(&pool, "route-1", "route-a", true).await;
        insert_target(&pool, "target-1", "route-1", None, 0, &[]).await;

        // 此前没有关联 Target；并发编辑将已有 Target 绑定到该模型，
        // 同时手工覆盖一行。重导入必须等待并读取新的绑定集合。
        let mut blocker = pool.begin().await.expect("blocker transaction");
        sqlx::query("UPDATE models SET display_name = 'blocking writer' WHERE id = 'route-1'")
            .execute(&mut *blocker)
            .await
            .expect("lock Route row");
        sqlx::query("UPDATE model_backends SET model = 'model', thinking_level_map = $1 WHERE id = 'target-1'")
            .bind(sqlx::types::Json(target_map(&[(
                ThinkingLevel::High,
                TargetThinkingControl::Effort {
                    value: "high".into(),
                },
            )])))
            .execute(&mut *blocker)
            .await
            .expect("edit Target map");
        let blocker_xid: String = sqlx::query_scalar("SELECT txid_current()::text")
            .fetch_one(&mut *blocker)
            .await
            .expect("blocker transaction id");

        let reimport_storage = storage.clone();
        let mut task = tokio::spawn(async move {
            reimport_storage
                .reimport(
                    "provider",
                    "model",
                    reimport_input(1),
                    &|_, _| Ok(()),
                    &|| anyhow::Ok(()),
                )
                .await
        });

        // 根据实际锁等待释放并发写入，不用固定延迟猜测调度顺序。
        let relation = format!("{schema}.models");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_locks WHERE NOT granted AND \
                 ((locktype IN ('relation', 'tuple') AND relation = to_regclass($1)) OR \
                  (locktype = 'transactionid' AND transactionid::text = $2))",
            )
            .bind(&relation)
            .bind(&blocker_xid)
            .fetch_one(&pool)
            .await
            .expect("inspect pg_locks");
            if waiting > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reimport never waited on the Route row lock"
            );
        }
        blocker.commit().await.expect("commit blocking write");

        let result = tokio::time::timeout(Duration::from_secs(10), &mut task)
            .await
            .expect("reimport finishes once the Route writer commits")
            .expect("reimport task")
            .expect("reimport result");
        let ProviderModelReimport::Applied {
            model: _,
            active_routes,
        } = result
        else {
            panic!("reimport must apply");
        };

        // The returned snapshot was read after the blocker committed.
        assert_eq!(
            active_routes[0].display_name.as_deref(),
            Some("blocking writer")
        );
        let map = &active_routes[0].targets[0].thinking_level_map;
        let high = map_row(map, ThinkingLevel::High);
        assert_eq!(high.source, ThinkingMappingSource::Overridden);
        assert_eq!(
            high.control,
            TargetThinkingControl::Effort {
                value: "high".into()
            }
        );
        assert_eq!(
            map_row(map, ThinkingLevel::Low).control,
            TargetThinkingControl::Enabled
        );

        cleanup(admin, pool, &schema).await;
    }
}
