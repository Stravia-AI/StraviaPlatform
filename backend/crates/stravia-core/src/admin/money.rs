use super::*;
use crate::pricing::{CostBasis, CurrencyKind, CustomCurrency, SubscriptionPeriod};
use crate::storage::traits::{ApiKeySpend, NewProviderCostPlan, ProviderCostPlan};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCostPlanPriceInput {
    pub amount: String,
    pub currency: CurrencyKind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCostPlanInput {
    pub basis: CostBasis,
    #[serde(default)]
    pub period: Option<SubscriptionPeriod>,
    #[serde(default)]
    pub price: Option<ProviderCostPlanPriceInput>,
}

impl ProviderCostPlanInput {
    fn into_new(self) -> anyhow::Result<NewProviderCostPlan> {
        let currency = self.price.as_ref().map(|price| price.currency);
        let price = self
            .price
            .map(|price| {
                Decimal::from_str(price.amount.trim())
                    .context("price.amount must be a decimal number")
            })
            .transpose()?;
        Ok(NewProviderCostPlan {
            basis: self.basis,
            period: self.period,
            price,
            currency,
        })
    }
}

impl AdminService {
    pub async fn custom_currency(&self) -> anyhow::Result<Option<CustomCurrency>> {
        self.gw.storage.money().custom_currency().await
    }

    pub async fn save_custom_currency(
        &self,
        symbol: &str,
        usd_rate: &str,
    ) -> anyhow::Result<CustomCurrency> {
        let symbol = symbol.trim();
        anyhow::ensure!(!symbol.is_empty(), "currency symbol cannot be empty");
        let usd_rate =
            Decimal::from_str(usd_rate.trim()).context("usd_rate must be a decimal number")?;
        anyhow::ensure!(usd_rate > Decimal::ZERO, "usd_rate must be positive");
        let _save = crate::rpm::CONFIG_WRITE.lock().await;
        let currency = self
            .gw
            .storage
            .money()
            .save_custom_currency(symbol, usd_rate)
            .await?;
        self.gw.apply_custom_currency(Some(currency.clone()));
        crate::storage::bump_config_epoch(self.gw.storage.settings()).await?;
        Ok(currency)
    }

    pub async fn provider_cost_plans(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderCostPlan>> {
        self.gw
            .storage
            .providers()
            .get(provider_id)
            .await?
            .context("provider not found")?;
        self.gw
            .storage
            .money()
            .provider_cost_plans(provider_id)
            .await
    }

    pub async fn current_provider_cost_plan(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<ProviderCostPlan>> {
        self.gw
            .storage
            .money()
            .current_provider_cost_plan(provider_id)
            .await
    }

    /// The plan in effect per Provider, keyed by Provider id; batch form of
    /// [`AdminService::current_provider_cost_plan`].
    pub async fn current_provider_cost_plans(
        &self,
    ) -> anyhow::Result<HashMap<String, ProviderCostPlan>> {
        self.gw.storage.money().current_provider_cost_plans().await
    }

    pub async fn append_provider_cost_plan(
        &self,
        provider_id: &str,
        input: ProviderCostPlanInput,
    ) -> anyhow::Result<ProviderCostPlan> {
        let input = input.into_new()?;
        crate::storage::money::validate_new_cost_plan(&input)?;
        anyhow::ensure!(
            input.currency != Some(CurrencyKind::Custom)
                || self.gw.custom_currency_snapshot().is_some(),
            "custom currency is not configured"
        );
        self.gw
            .storage
            .providers()
            .get(provider_id)
            .await?
            .context("provider not found")?;
        let _save = crate::rpm::CONFIG_WRITE.lock().await;
        let plan = self
            .gw
            .storage
            .money()
            .append_provider_cost_plan(provider_id, input)
            .await?;
        crate::storage::bump_config_epoch(self.gw.storage.settings()).await?;
        Ok(plan)
    }

    pub async fn api_key_spend(&self, id: &str) -> anyhow::Result<ApiKeySpend> {
        self.api_keys_store()?
            .get(id)
            .await?
            .context("api key not found")?;
        self.gw.storage.money().api_key_spend(id).await
    }

    pub async fn reset_api_key_spend(&self, id: &str) -> anyhow::Result<ApiKeySpend> {
        self.gw.storage.money().reset_api_key_spend(id).await
    }

    pub async fn api_key_spend_json(
        &self,
        spend: &ApiKeySpend,
    ) -> anyhow::Result<serde_json::Value> {
        let custom = self.custom_currency().await?;
        Self::api_key_spend_json_with_currency(spend, custom.as_ref())
    }

    pub fn api_key_spend_json_with_currency(
        spend: &ApiKeySpend,
        custom: Option<&CustomCurrency>,
    ) -> anyhow::Result<serde_json::Value> {
        let spent = crate::pricing::display_nanos(
            spend.spent_custom_nanos,
            spend.spent_usd_unconverted_nanos,
            custom,
        )
        .context("spent amount exceeds the supported range")?;
        Ok(serde_json::json!({
            "spent": crate::pricing::nanos_to_decimal_string(spent),
            "reset_at": spend.reset_at,
        }))
    }

    /// Management projection with display-currency strings for spend.
    pub async fn api_key_json(
        &self,
        key: &crate::db::models::ApiKeyWithBindings,
    ) -> anyhow::Result<serde_json::Value> {
        let custom = self.custom_currency().await?;
        self.api_key_json_with_currency(key, custom.as_ref())
    }

    pub fn api_key_json_with_currency(
        &self,
        key: &crate::db::models::ApiKeyWithBindings,
        custom: Option<&CustomCurrency>,
    ) -> anyhow::Result<serde_json::Value> {
        let spend = Self::api_key_spend_json_with_currency(&key.spend, custom)?;
        let mut value = serde_json::to_value(key)?;
        let object = value
            .as_object_mut()
            .context("api key projection is not an object")?;
        object.insert(
            "spend_limit".to_string(),
            key.spend_limit_nanos
                .map(|nanos| {
                    serde_json::Value::String(crate::pricing::nanos_to_decimal_string(nanos))
                })
                .unwrap_or(serde_json::Value::Null),
        );
        object.insert("spend".to_string(), spend);
        Ok(value)
    }
}

/// Management projection for one Provider cost plan. `Custom` prices must
/// carry their frozen custom amount; a missing side is an internal error, never
/// a silent USD fallback.
pub fn provider_cost_plan_value(plan: &ProviderCostPlan) -> anyhow::Result<serde_json::Value> {
    let price = match (plan.price.as_ref(), plan.price_currency) {
        (None, _) => None,
        (Some(_), None) => {
            anyhow::bail!("cost plan price currency is missing");
        }
        (Some(amount), Some(currency)) => Some(serde_json::json!({
            "amount": crate::storage::money::amount_in_currency(amount, currency)?,
            "currency": currency,
        })),
    };
    Ok(serde_json::json!({
        "id": plan.id,
        "provider_id": plan.provider_id,
        "effective_from": plan.effective_from,
        "basis": plan.basis,
        "period": plan.period,
        "price": price,
        "created_at": plan.created_at,
    }))
}
