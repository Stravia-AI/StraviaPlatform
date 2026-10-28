//! Shared money serialization for the storage/admin boundary.
//!
//! Stored amounts are i64 nanos. Management APIs exchange decimal strings;
//! conversion rounds once, away from zero.

use std::collections::HashMap;

use anyhow::Context;

use crate::pricing::{Amount, CostBasis, CurrencyKind, CustomCurrency, SubscriptionPeriod};
use crate::storage::traits::ProviderCostPlan;

/// Settings row holding the single instance custom currency JSON.
pub const CUSTOM_CURRENCY_SETTING_KEY: &str = "custom_currency";

pub(super) fn parse_spend_limit(value: Option<&str>) -> anyhow::Result<Option<i64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let amount = value
        .parse::<rust_decimal::Decimal>()
        .with_context(|| format!("invalid spend limit: {value}"))?;
    anyhow::ensure!(
        amount > rust_decimal::Decimal::ZERO,
        "spend limit must be positive"
    );
    crate::pricing::decimal_to_nanos(amount)
        .context("spend limit exceeds the supported range")
        .map(Some)
}

pub fn parse_custom_currency(raw: &str) -> anyhow::Result<CustomCurrency> {
    let currency: CustomCurrency =
        serde_json::from_str(raw).context("invalid custom currency setting")?;
    currency.validate()?;
    Ok(currency)
}

pub fn encode_custom_currency(currency: &CustomCurrency) -> anyhow::Result<String> {
    currency.validate()?;
    Ok(serde_json::to_string(currency)?)
}

pub fn cost_basis_str(basis: CostBasis) -> &'static str {
    match basis {
        CostBasis::Usage => "usage",
        CostBasis::Subscription => "subscription",
    }
}

pub fn parse_cost_basis(value: &str) -> anyhow::Result<CostBasis> {
    match value {
        "usage" => Ok(CostBasis::Usage),
        "subscription" => Ok(CostBasis::Subscription),
        other => anyhow::bail!("invalid cost basis: {other}"),
    }
}

pub fn subscription_period_str(period: SubscriptionPeriod) -> &'static str {
    match period {
        SubscriptionPeriod::Month => "month",
        SubscriptionPeriod::Year => "year",
    }
}

pub fn parse_subscription_period(value: &str) -> anyhow::Result<SubscriptionPeriod> {
    match value {
        "month" => Ok(SubscriptionPeriod::Month),
        "year" => Ok(SubscriptionPeriod::Year),
        other => anyhow::bail!("invalid subscription period: {other}"),
    }
}

pub fn currency_kind_str(kind: CurrencyKind) -> &'static str {
    match kind {
        CurrencyKind::Usd => "usd",
        CurrencyKind::Custom => "custom",
    }
}

pub fn parse_currency_kind(value: &str) -> anyhow::Result<CurrencyKind> {
    match value {
        "usd" => Ok(CurrencyKind::Usd),
        "custom" => Ok(CurrencyKind::Custom),
        other => anyhow::bail!("invalid currency: {other}"),
    }
}

/// Display the entered side of a frozen price.
pub fn amount_in_currency(amount: &Amount, currency: CurrencyKind) -> anyhow::Result<String> {
    match currency {
        CurrencyKind::Usd => Ok(crate::pricing::nanos_to_decimal_string(amount.usd_nanos)),
        CurrencyKind::Custom => amount
            .custom_nanos
            .map(crate::pricing::nanos_to_decimal_string)
            .context("custom currency amount is missing"),
    }
}

/// The plan in effect for one Provider: the latest `effective_from`, ties
/// broken by the greater id — the same order backends list plans in.
pub(crate) fn current_provider_cost_plan(
    plans: impl IntoIterator<Item = ProviderCostPlan>,
) -> Option<ProviderCostPlan> {
    plans
        .into_iter()
        .max_by(|left, right| plan_order(left).cmp(&plan_order(right)))
}

/// The plan in effect per Provider, keyed by Provider id. Batch form of
/// [`current_provider_cost_plan`], sharing its selection rule.
pub(crate) fn current_provider_cost_plans(
    plans: impl IntoIterator<Item = ProviderCostPlan>,
) -> HashMap<String, ProviderCostPlan> {
    let mut current: HashMap<String, ProviderCostPlan> = HashMap::new();
    for plan in plans {
        match current.get_mut(plan.provider_id.as_str()) {
            Some(incumbent) => {
                if plan_order(&plan) > plan_order(incumbent) {
                    *incumbent = plan;
                }
            }
            None => {
                current.insert(plan.provider_id.clone(), plan);
            }
        }
    }
    current
}

fn plan_order(plan: &ProviderCostPlan) -> (i64, &str) {
    (plan.effective_from, plan.id.as_str())
}

/// Validate a cost-plan write once, for both the admin boundary and every
/// storage backend.
pub fn validate_new_cost_plan(
    input: &crate::storage::traits::NewProviderCostPlan,
) -> anyhow::Result<()> {
    match input.basis {
        CostBasis::Usage => {
            anyhow::ensure!(
                input.period.is_none() && input.price.is_none() && input.currency.is_none(),
                "a usage cost plan cannot carry a subscription period or price"
            );
        }
        CostBasis::Subscription => {
            anyhow::ensure!(
                input.period.is_some() && input.price.is_some(),
                "a subscription cost plan requires a period and price"
            );
            anyhow::ensure!(
                input
                    .price
                    .is_some_and(|amount| amount > rust_decimal::Decimal::ZERO),
                "subscription price must be positive"
            );
            anyhow::ensure!(
                input.currency.is_some(),
                "a subscription cost plan requires a price currency"
            );
        }
    }
    Ok(())
}
