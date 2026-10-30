use std::collections::BTreeMap;

use serde::Deserialize;

use super::*;
use crate::provider_catalog::{
    CatalogError, CatalogProviderScope, OfficialPriceIdentity, has_model_prices,
};
use crate::provider_models::{ModelCost, ProviderModelMetadata};

pub(super) struct OfficialPriceResolver<'a> {
    gw: &'a Gateway,
    scopes: BTreeMap<String, Result<Option<CatalogProviderScope>, ()>>,
}

impl<'a> OfficialPriceResolver<'a> {
    pub(super) fn new(gw: &'a Gateway) -> Self {
        Self {
            gw,
            scopes: BTreeMap::new(),
        }
    }

    /// 只补整组缺失的价格；返回 true 表示目录暂不可用，调用方须保留旧来源价格。
    pub(super) async fn enrich(
        &mut self,
        model_id: &str,
        metadata: &mut ProviderModelMetadata,
        canonical_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        if metadata.cost.as_ref().is_some_and(has_model_prices) {
            return Ok(false);
        }
        let canonical_id = canonical_id.or_else(|| {
            metadata
                .extensions
                .get("canonical_id")
                .and_then(Value::as_str)
        });
        let Some(identity) = self
            .gw
            .provider_catalog
            .official_price_identity(model_id, metadata.name.as_deref(), canonical_id)
            .await
        else {
            return Ok(false);
        };
        self.apply(identity, metadata).await
    }

    async fn apply(
        &mut self,
        identity: OfficialPriceIdentity,
        metadata: &mut ProviderModelMetadata,
    ) -> anyhow::Result<bool> {
        if !self.scopes.contains_key(&identity.provider_id) {
            let scope = match self
                .gw
                .catalog_sync
                .provider_scope(&identity.provider_id)
                .await
            {
                Ok(scope) => Ok(Some(scope)),
                Err(error)
                    if matches!(
                        error.downcast_ref::<CatalogError>(),
                        Some(CatalogError::ProviderNotFound { .. })
                    ) =>
                {
                    Ok(None)
                }
                Err(_) => {
                    tracing::warn!(
                        catalog_provider_id = %identity.provider_id,
                        "Official API reference pricing is unavailable; retaining previously synchronized prices"
                    );
                    Err(())
                }
            };
            self.scopes.insert(identity.provider_id.clone(), scope);
        }
        let scope = match self
            .scopes
            .get(&identity.provider_id)
            .expect("loaded pricing scope")
        {
            Ok(Some(scope)) => scope,
            Ok(None) => return Ok(false),
            Err(()) => return Ok(true),
        };
        let Some(value) = identity.model(scope).and_then(|model| model.get("cost")) else {
            return Ok(false);
        };
        if value.is_null() {
            return Ok(false);
        }
        let mut cost = ModelCost::deserialize(value)?;
        if has_model_prices(&cost) {
            // 手动模型草稿可以已有本地峰谷设置；来源美元价不改变这项人工意图。
            cost.peak = metadata.cost.as_mut().and_then(|cost| cost.peak.take());
            metadata.cost = Some(cost);
        }
        Ok(false)
    }
}
