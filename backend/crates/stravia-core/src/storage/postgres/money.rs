use chrono::{SecondsFormat, Utc};
use rust_decimal::Decimal;

use super::*;
use crate::pricing::{Amount, CostBasis, CustomCurrency};
use crate::storage::money;
use crate::storage::traits::{ApiKeySpend, MoneyStore, NewProviderCostPlan, ProviderCostPlan};

const CURRENCY_WRITE_LOCK: i64 = 0x0053_5452_4156_4941;

type PlanRow = (
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    String,
);

fn plan_from_row(row: PlanRow) -> anyhow::Result<ProviderCostPlan> {
    let (id, provider_id, effective_from, basis, period, usd, custom, currency, created_at) = row;
    let period = period
        .as_deref()
        .map(money::parse_subscription_period)
        .transpose()?;
    let price_currency = currency
        .as_deref()
        .map(money::parse_currency_kind)
        .transpose()?;
    Ok(ProviderCostPlan {
        id,
        provider_id,
        effective_from,
        basis: money::parse_cost_basis(&basis)?,
        period,
        price: usd.map(|usd_nanos| Amount {
            usd_nanos,
            custom_nanos: custom,
        }),
        price_currency,
        created_at,
    })
}

impl PostgresStorage {
    fn now_rfc3339() -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

#[async_trait]
impl MoneyStore for PostgresStorage {
    async fn custom_currency(&self) -> anyhow::Result<Option<CustomCurrency>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE name = $1")
            .bind(money::CUSTOM_CURRENCY_SETTING_KEY)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|(value,)| money::parse_custom_currency(&value))
            .transpose()
    }

    async fn save_custom_currency(
        &self,
        symbol: &str,
        usd_rate: Decimal,
    ) -> anyhow::Result<CustomCurrency> {
        let symbol = symbol.trim().to_string();
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin().await?;
        // First setup converts every spend limit exactly once; serialize all
        // currency writes with a transaction-scoped advisory lock.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CURRENCY_WRITE_LOCK)
            .execute(&mut *tx)
            .await?;
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT value FROM settings WHERE name = $1")
                .bind(money::CUSTOM_CURRENCY_SETTING_KEY)
                .fetch_optional(&mut *tx)
                .await?;
        let currency = match existing {
            Some((raw,)) => {
                let mut currency = money::parse_custom_currency(&raw)?;
                currency.symbol = symbol;
                currency.usd_rate = usd_rate;
                currency.validate()?;
                currency
            }
            None => {
                let currency = CustomCurrency {
                    symbol,
                    usd_rate,
                    first_rate: usd_rate,
                    configured_at: Self::now_rfc3339(),
                };
                currency.validate()?;
                // The only limit conversion ever performed: management config,
                // in the same transaction as the first setting write.
                let limits: Vec<(String, i64)> = sqlx::query_as(
                    "SELECT id, spend_limit_nanos FROM api_keys WHERE spend_limit_nanos IS NOT NULL",
                )
                .fetch_all(&mut *tx)
                .await?;
                for (id, nanos) in limits {
                    let converted = crate::pricing::display_nanos(0, nanos, Some(&currency))
                        .context("spend limit exceeds the supported range")?;
                    sqlx::query("UPDATE api_keys SET spend_limit_nanos = $1 WHERE id = $2")
                        .bind(converted)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                currency
            }
        };
        let encoded = money::encode_custom_currency(&currency)?;
        sqlx::query(
            "INSERT INTO settings (name, value, updated_at) VALUES ($1, $2, CURRENT_TIMESTAMP) \
             ON CONFLICT(name) DO UPDATE SET value=EXCLUDED.value, \
             updated_at=EXCLUDED.updated_at",
        )
        .bind(money::CUSTOM_CURRENCY_SETTING_KEY)
        .bind(encoded)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(currency)
    }

    async fn provider_cost_plans(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderCostPlan>> {
        let rows: Vec<PlanRow> = sqlx::query_as(
            "SELECT id, provider_id, effective_from, basis, period, price_usd_nanos, \
             price_custom_nanos, price_currency, created_at FROM provider_cost_plans \
             WHERE provider_id = $1 ORDER BY effective_from DESC, id DESC",
        )
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(plan_from_row).collect()
    }

    async fn all_provider_cost_plans(&self) -> anyhow::Result<Vec<ProviderCostPlan>> {
        let rows: Vec<PlanRow> = sqlx::query_as(
            "SELECT id, provider_id, effective_from, basis, period, price_usd_nanos, \
             price_custom_nanos, price_currency, created_at FROM provider_cost_plans \
             ORDER BY provider_id ASC, effective_from ASC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(plan_from_row).collect()
    }

    async fn append_provider_cost_plan(
        &self,
        provider_id: &str,
        input: NewProviderCostPlan,
    ) -> anyhow::Result<ProviderCostPlan> {
        money::validate_new_cost_plan(&input)?;
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin().await?;
        let exists: Option<String> =
            sqlx::query_scalar("SELECT id FROM providers WHERE id = $1 FOR UPDATE")
                .bind(provider_id)
                .fetch_optional(&mut *tx)
                .await?;
        anyhow::ensure!(exists.is_some(), "provider not found: {provider_id}");
        let custom = {
            let row: Option<(String,)> =
                sqlx::query_as("SELECT value FROM settings WHERE name = $1")
                    .bind(money::CUSTOM_CURRENCY_SETTING_KEY)
                    .fetch_optional(&mut *tx)
                    .await?;
            row.map(|(value,)| money::parse_custom_currency(&value))
                .transpose()?
        };
        let (period, price, currency) = match input.basis {
            CostBasis::Usage => (None, None, None),
            CostBasis::Subscription => {
                let period = input.period.context("subscription requires a period")?;
                let amount = input.price.context("subscription requires a price")?;
                let currency = input
                    .currency
                    .context("subscription requires a price currency")?;
                let frozen = crate::pricing::freeze_amount(amount, currency, custom.as_ref())
                    .context("subscription price could not be frozen")?;
                (Some(period), Some(frozen), Some(currency))
            }
        };
        let id = stravia_runtime_contract::identifier::new_id();
        let now = Utc::now();
        let previous: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(effective_from), 0) FROM provider_cost_plans \
             WHERE provider_id = $1",
        )
        .bind(provider_id)
        .fetch_one(&mut *tx)
        .await?;
        let effective_from = now.timestamp_millis().max(
            previous
                .checked_add(1)
                .context("cost plan timestamp overflow")?,
        );
        let created_at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
        sqlx::query(
            "INSERT INTO provider_cost_plans (id, provider_id, effective_from, basis, period, \
             price_usd_nanos, price_custom_nanos, price_currency, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(provider_id)
        .bind(effective_from)
        .bind(money::cost_basis_str(input.basis))
        .bind(period.map(money::subscription_period_str))
        .bind(price.as_ref().map(|price| price.usd_nanos))
        .bind(price.as_ref().and_then(|price| price.custom_nanos))
        .bind(currency.map(money::currency_kind_str))
        .bind(&created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(ProviderCostPlan {
            id,
            provider_id: provider_id.to_string(),
            effective_from,
            basis: input.basis,
            period,
            price,
            price_currency: currency,
            created_at,
        })
    }

    async fn api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        let row: Option<(i64, i64, Option<String>)> = sqlx::query_as(
            "SELECT spent_custom_nanos, spent_usd_unconverted_nanos, reset_at \
             FROM api_key_spend WHERE api_key_id = $1",
        )
        .bind(api_key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(
                |(spent_custom_nanos, spent_usd_unconverted_nanos, reset_at)| ApiKeySpend {
                    spent_custom_nanos,
                    spent_usd_unconverted_nanos,
                    reset_at,
                },
            )
            .unwrap_or_default())
    }

    async fn reset_api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        let now = Self::now_rfc3339();
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin().await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = $1)")
                .bind(api_key_id)
                .fetch_one(&mut *tx)
                .await?;
        anyhow::ensure!(exists, "api key not found: {api_key_id}");
        sqlx::query(
            "INSERT INTO api_key_spend (api_key_id, spent_custom_nanos, \
             spent_usd_unconverted_nanos, reset_at, updated_at) VALUES ($1, 0, 0, $2, $2) \
             ON CONFLICT(api_key_id) DO UPDATE SET spent_custom_nanos = 0, \
             spent_usd_unconverted_nanos = 0, reset_at = EXCLUDED.reset_at, \
             updated_at = EXCLUDED.updated_at",
        )
        .bind(api_key_id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(ApiKeySpend {
            spent_custom_nanos: 0,
            spent_usd_unconverted_nanos: 0,
            reset_at: Some(now),
        })
    }
}
