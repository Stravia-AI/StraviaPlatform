use std::sync::Arc;

use sqlx::{PgPool, SqlitePool};
use tokio::sync::RwLock;

use super::{ProviderAllowanceSnapshot, ProviderAllowanceStatus};

pub(super) const SAMPLE_RETENTION_MILLIS: i64 = 14 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub(crate) struct AllowanceSample {
    pub id: String,
    pub provider_id: String,
    pub allowance_key: String,
    pub sampled_at: i64,
    pub used_value: Option<f64>,
    pub remaining_value: Option<f64>,
    pub limit_value: Option<f64>,
    pub used_percent: Option<f64>,
    pub amount_unit: Option<String>,
    pub currency: Option<String>,
    pub reset_at: Option<i64>,
}

#[derive(Clone)]
pub(crate) enum AllowanceSampleStore {
    Memory(Arc<RwLock<Vec<AllowanceSample>>>),
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

impl AllowanceSampleStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(Arc::new(RwLock::new(Vec::new())))
    }

    pub(crate) fn sqlite(pool: SqlitePool) -> Self {
        Self::Sqlite(pool)
    }

    pub(crate) fn postgres(pool: PgPool) -> Self {
        Self::Postgres(pool)
    }

    pub(crate) async fn record_snapshot_at(
        &self,
        snapshot: &ProviderAllowanceSnapshot,
        sampled_at: i64,
    ) -> anyhow::Result<()> {
        if snapshot.status != ProviderAllowanceStatus::Fresh {
            return Ok(());
        }

        let samples = snapshot
            .allowances
            .iter()
            .map(|allowance| {
                let amount = allowance
                    .remaining
                    .as_ref()
                    .or(allowance.used.as_ref())
                    .or(allowance.limit.as_ref());
                AllowanceSample {
                    id: uuid::Uuid::new_v4().to_string(),
                    provider_id: snapshot.provider_id.clone(),
                    allowance_key: allowance.key.clone(),
                    sampled_at,
                    used_value: allowance.used.as_ref().map(|value| value.value),
                    remaining_value: allowance.remaining.as_ref().map(|value| value.value),
                    limit_value: allowance.limit.as_ref().map(|value| value.value),
                    used_percent: allowance.used_percent,
                    amount_unit: amount.map(|value| value.unit.clone()),
                    currency: amount.and_then(|value| value.currency.clone()),
                    reset_at: allowance.reset_at,
                }
            })
            .collect::<Vec<_>>();

        let cutoff = sampled_at.saturating_sub(SAMPLE_RETENTION_MILLIS);
        match self {
            Self::Memory(rows) => {
                let mut rows = rows.write().await;
                rows.extend(samples);
                rows.retain(|sample| sample.sampled_at >= cutoff);
            }
            Self::Sqlite(pool) => {
                let mut transaction = pool.begin().await?;
                for sample in samples {
                    sqlx::query(
                        "INSERT INTO provider_allowance_samples (
                            id, provider_id, allowance_key, sampled_at, used_value,
                            remaining_value, limit_value, used_percent, amount_unit,
                            currency, reset_at
                         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(sample.id)
                    .bind(sample.provider_id)
                    .bind(sample.allowance_key)
                    .bind(sample.sampled_at)
                    .bind(sample.used_value)
                    .bind(sample.remaining_value)
                    .bind(sample.limit_value)
                    .bind(sample.used_percent)
                    .bind(sample.amount_unit)
                    .bind(sample.currency)
                    .bind(sample.reset_at)
                    .execute(&mut *transaction)
                    .await?;
                }
                sqlx::query("DELETE FROM provider_allowance_samples WHERE sampled_at < ?")
                    .bind(cutoff)
                    .execute(&mut *transaction)
                    .await?;
                transaction.commit().await?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await?;
                for sample in samples {
                    sqlx::query(
                        "INSERT INTO provider_allowance_samples (
                            id, provider_id, allowance_key, sampled_at, used_value,
                            remaining_value, limit_value, used_percent, amount_unit,
                            currency, reset_at
                         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                    )
                    .bind(sample.id)
                    .bind(sample.provider_id)
                    .bind(sample.allowance_key)
                    .bind(sample.sampled_at)
                    .bind(sample.used_value)
                    .bind(sample.remaining_value)
                    .bind(sample.limit_value)
                    .bind(sample.used_percent)
                    .bind(sample.amount_unit)
                    .bind(sample.currency)
                    .bind(sample.reset_at)
                    .execute(&mut *transaction)
                    .await?;
                }
                sqlx::query("DELETE FROM provider_allowance_samples WHERE sampled_at < $1")
                    .bind(cutoff)
                    .execute(&mut *transaction)
                    .await?;
                transaction.commit().await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn cleanup_at(&self, now: i64) -> anyhow::Result<()> {
        let cutoff = now.saturating_sub(SAMPLE_RETENTION_MILLIS);
        match self {
            Self::Memory(rows) => {
                rows.write()
                    .await
                    .retain(|sample| sample.sampled_at >= cutoff);
            }
            Self::Sqlite(pool) => {
                sqlx::query("DELETE FROM provider_allowance_samples WHERE sampled_at < ?")
                    .bind(cutoff)
                    .execute(pool)
                    .await?;
            }
            Self::Postgres(pool) => {
                sqlx::query("DELETE FROM provider_allowance_samples WHERE sampled_at < $1")
                    .bind(cutoff)
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn list_for_item(
        &self,
        provider_id: &str,
        allowance_key: &str,
        since: i64,
    ) -> anyhow::Result<Vec<AllowanceSample>> {
        let mut samples = match self {
            Self::Memory(rows) => rows
                .read()
                .await
                .iter()
                .filter(|sample| {
                    sample.provider_id == provider_id
                        && sample.allowance_key == allowance_key
                        && sample.sampled_at >= since
                })
                .cloned()
                .collect(),
            Self::Sqlite(pool) => {
                sqlx::query_as::<_, AllowanceSample>(
                    "SELECT id, provider_id, allowance_key, sampled_at, used_value,
                            remaining_value, limit_value, used_percent, amount_unit,
                            currency, reset_at
                     FROM provider_allowance_samples
                     WHERE provider_id = ? AND allowance_key = ? AND sampled_at >= ?
                     ORDER BY sampled_at ASC, id ASC",
                )
                .bind(provider_id)
                .bind(allowance_key)
                .bind(since)
                .fetch_all(pool)
                .await?
            }
            Self::Postgres(pool) => {
                sqlx::query_as::<_, AllowanceSample>(
                    "SELECT id, provider_id, allowance_key, sampled_at, used_value,
                            remaining_value, limit_value, used_percent, amount_unit,
                            currency, reset_at
                     FROM provider_allowance_samples
                     WHERE provider_id = $1 AND allowance_key = $2 AND sampled_at >= $3
                     ORDER BY sampled_at ASC, id ASC",
                )
                .bind(provider_id)
                .bind(allowance_key)
                .bind(since)
                .fetch_all(pool)
                .await?
            }
        };
        samples.sort_by(|left, right| {
            left.sampled_at
                .cmp(&right.sampled_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::provider_allowance::{
        Allowance, AllowanceKind, ExhaustionForecast, ModelAllowance,
    };

    fn allowance(key: &str) -> Allowance {
        Allowance {
            key: key.into(),
            label: key.into(),
            kind: AllowanceKind::QuotaWindow,
            used: None,
            remaining: None,
            limit: None,
            used_percent: Some(25.0),
            window_seconds: Some(86_400),
            reset_at: Some(86_400_000),
            condition: None,
            forecast: ExhaustionForecast::default(),
        }
    }

    fn snapshot(status: ProviderAllowanceStatus) -> ProviderAllowanceSnapshot {
        ProviderAllowanceSnapshot {
            provider_id: "provider-1".into(),
            provider_name: "Provider 1".into(),
            catalog_provider_id: "catalog".into(),
            channel: "default".into(),
            plan_label: None,
            status,
            allowances: vec![allowance("account")],
            models: vec![ModelAllowance {
                model: "model-1".into(),
                allowances: vec![allowance("model")],
            }],
            fetched_at: Some("1970-01-01T00:00:00Z".into()),
            error: None,
        }
    }

    #[tokio::test]
    async fn samples_only_fresh_account_allowances() -> anyhow::Result<()> {
        let store = AllowanceSampleStore::memory();

        store
            .record_snapshot_at(&snapshot(ProviderAllowanceStatus::Fresh), 100)
            .await?;
        store
            .record_snapshot_at(&snapshot(ProviderAllowanceStatus::Stale), 200)
            .await?;
        store
            .record_snapshot_at(&snapshot(ProviderAllowanceStatus::Error), 300)
            .await?;

        assert_eq!(
            store.list_for_item("provider-1", "account", 0).await?.len(),
            1
        );
        assert!(
            store
                .list_for_item("provider-1", "model", 0)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_expires_samples_without_a_new_fresh_snapshot() -> anyhow::Result<()> {
        let store = AllowanceSampleStore::memory();
        store
            .record_snapshot_at(&snapshot(ProviderAllowanceStatus::Fresh), 100)
            .await?;

        store.cleanup_at(100 + SAMPLE_RETENTION_MILLIS + 1).await?;

        assert!(
            store
                .list_for_item("provider-1", "account", 0)
                .await?
                .is_empty()
        );
        Ok(())
    }

    async fn assert_sql_store_contract(store: AllowanceSampleStore) -> anyhow::Result<()> {
        store
            .record_snapshot_at(&snapshot(ProviderAllowanceStatus::Fresh), 100)
            .await?;
        let samples = store.list_for_item("provider-1", "account", 0).await?;
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].used_percent, Some(25.0));
        assert_eq!(samples[0].reset_at, Some(86_400_000));
        Ok(())
    }

    #[tokio::test]
    async fn sqlite_sample_store_contract_and_provider_cascade() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let pool = crate::db::init_pool(data_dir.path()).await?;
        crate::migrations::migrate_sqlite(&pool).await?;
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind("provider-1")
        .bind("Provider 1")
        .bind("openai-compatible")
        .bind("https://example.test")
        .bind("secret")
        .execute(&pool)
        .await?;

        let store = AllowanceSampleStore::sqlite(pool.clone());
        assert_sql_store_contract(store.clone()).await?;
        sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind("provider-1")
            .execute(&pool)
            .await?;
        assert!(
            store
                .list_for_item("provider-1", "account", 0)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn postgres_sample_store_contract_when_configured() -> anyhow::Result<()> {
        let Some(url) = std::env::var("DB_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())
        else {
            return Ok(());
        };
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_allowance_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let options: sqlx::postgres::PgConnectOptions = url.parse()?;
        let options = options.options([("search_path", schema.as_str())]);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        crate::migrations::migrate_postgres(&pool).await?;
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("provider-1")
        .bind("Provider 1")
        .bind("openai-compatible")
        .bind("https://example.test")
        .bind("secret")
        .execute(&pool)
        .await?;

        let store = AllowanceSampleStore::postgres(pool.clone());
        assert_sql_store_contract(store.clone()).await?;
        sqlx::query("DELETE FROM providers WHERE id = $1")
            .bind("provider-1")
            .execute(&pool)
            .await?;
        assert!(
            store
                .list_for_item("provider-1", "account", 0)
                .await?
                .is_empty()
        );

        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await?;
        Ok(())
    }
}
