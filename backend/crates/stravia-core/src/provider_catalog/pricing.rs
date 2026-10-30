use super::*;
use crate::provider_models::{ModelCost, PriceComponents};

pub(crate) struct OfficialPriceIdentity {
    pub provider_id: String,
    canonical_id: String,
}

impl ProviderCatalog {
    pub(crate) async fn official_price_identity(
        &self,
        model_id: &str,
        name: Option<&str>,
        canonical_id: Option<&str>,
    ) -> Option<OfficialPriceIdentity> {
        let snapshot = self.snapshot.read().await;
        let canonical_id = if let Some(id) = canonical_id {
            validate_canonical_model_id(id).ok()?;
            id
        } else {
            match_canonical(&snapshot.canonical_models, model_id, name)?
                .0
                .as_str()
        };
        let (lab, _) = canonical_id.split_once('/')?;
        // 只采用与模型厂商身份相同的目录，不从转售商或同品牌套餐猜官方 API 价。
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == lab)?;
        Some(OfficialPriceIdentity {
            provider_id: provider.id.clone(),
            canonical_id: canonical_id.to_owned(),
        })
    }
}

impl OfficialPriceIdentity {
    pub(crate) fn model<'a>(&self, scope: &'a CatalogProviderScope) -> Option<&'a Value> {
        let (_, upstream_id) = self.canonical_id.split_once('/')?;
        let compatible = |model: &&CatalogModelSource| {
            model
                .metadata
                .get("canonical_id")
                .and_then(Value::as_str)
                .is_none_or(|id| id.eq_ignore_ascii_case(&self.canonical_id))
        };
        let models = || scope.models.iter().filter(compatible);
        fn id(model: &CatalogModelSource) -> Option<&str> {
            model.metadata.get("id").and_then(Value::as_str)
        }
        let exact = unique(models().filter(|model| id(model) == Some(upstream_id))).ok()?;
        if let Some(model) = exact {
            return Some(&model.metadata);
        }
        let insensitive = unique(
            models()
                .filter(|model| id(model).is_some_and(|id| id.eq_ignore_ascii_case(upstream_id))),
        )
        .ok()?;
        if let Some(model) = insensitive {
            return Some(&model.metadata);
        }
        let canonical = unique(models().filter(|model| {
            model
                .metadata
                .get("canonical_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.eq_ignore_ascii_case(&self.canonical_id))
        }))
        .ok()?;
        if let Some(model) = canonical {
            return Some(&model.metadata);
        }
        // 展示名称可能被不同日期版本复用；官方目录须有相同的模型版本身份。
        unique(
            models().filter(|model| id(model).is_some_and(|id| same_price_name(id, upstream_id))),
        )
        .ok()
        .flatten()
        .map(|model| &model.metadata)
    }
}

fn match_canonical<'a>(
    models: &'a BTreeMap<String, Value>,
    model_id: &str,
    name: Option<&str>,
) -> Option<(&'a String, &'a Value)> {
    let model_id = model_id.trim();
    if let Some(model) = models.get_key_value(model_id) {
        return Some(model);
    }
    fn leaf(id: &str) -> &str {
        id.rsplit('/').next().unwrap_or(id)
    }
    let upstream_id = leaf(model_id);
    let exact = unique(
        models
            .iter()
            .filter(|(id, _)| leaf(id).eq_ignore_ascii_case(upstream_id)),
    )
    .ok()?;
    if exact.is_some() {
        return exact;
    }
    let normalized = unique(
        models
            .iter()
            .filter(|(id, _)| same_price_name(leaf(id), upstream_id)),
    )
    .ok()?;
    if normalized.is_some() {
        return normalized;
    }
    let name = name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(model_id);
    unique(models.iter().filter(|(_, model)| {
        model
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|candidate| same_price_name(candidate, name))
    }))
    .ok()
    .flatten()
}

fn unique<T>(mut matches: impl Iterator<Item = T>) -> Result<Option<T>, ()> {
    let first = matches.next();
    if matches.next().is_some() {
        Err(())
    } else {
        Ok(first)
    }
}

fn same_price_name(left: &str, right: &str) -> bool {
    fn without_thinking(value: &str) -> &str {
        let value = value.trim();
        // Claude 的 thinking 是渠道推理控制；官方输出价已包含推理 Token。
        if value
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("claude"))
        {
            for suffix in ["-thinking", " (thinking)"] {
                if let Some(start) = value.len().checked_sub(suffix.len())
                    && let (Some(base), Some(tail)) = (value.get(..start), value.get(start..))
                    && tail.eq_ignore_ascii_case(suffix)
                {
                    return base;
                }
            }
        }
        value
    }
    fn tokens(value: &str) -> impl Iterator<Item = &str> {
        without_thinking(value)
            .split(|ch: char| ch.is_ascii_whitespace() || matches!(ch, '-' | '_' | '.'))
            .filter(|token| !token.is_empty())
    }
    let mut left = tokens(left);
    let mut right = tokens(right);
    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) if left.eq_ignore_ascii_case(right) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

pub(crate) fn has_model_prices(cost: &ModelCost) -> bool {
    fn has_prices(prices: &PriceComponents) -> bool {
        [
            prices.input,
            prices.output,
            prices.cache_read,
            prices.cache_write,
        ]
        .iter()
        .any(Option::is_some)
    }
    has_prices(&cost.prices)
        || cost.context_over_200k.as_ref().is_some_and(has_prices)
        || cost.tiers.iter().any(|tier| has_prices(&tier.prices))
}
