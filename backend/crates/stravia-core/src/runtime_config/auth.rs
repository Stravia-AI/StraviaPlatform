use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

use super::RuntimeConfig;
use crate::db::models::{ApiKeyWithBindings, CreateApiKey, PutRoute, RouteConfig, UpdateApiKey};
use crate::pricing::CustomCurrency;
use crate::storage::traits::{
    ApiKeyAccessRecord, ApiKeySpend, ApiKeyStore, AuthAccessStore, MoneyStore, NewProviderCostPlan,
    ProviderCostPlan, RouteStore,
};

fn token_cache_key(raw_key: &str) -> [u8; 32] {
    Sha256::digest(raw_key.as_bytes()).into()
}

#[async_trait]
impl AuthAccessStore for RuntimeConfig {
    async fn find_api_key(&self, raw_key: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>> {
        let Some(auth) = self.source.auth() else {
            return Ok(None);
        };
        let hash = token_cache_key(raw_key);
        let generation = {
            let state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 {
                if let Some(record) = state.values.auth_by_token_hash.get(&hash) {
                    return Ok(Some(record.clone()));
                }
                Some(state.generation)
            } else {
                None
            }
        };
        let loaded = auth.find_api_key(raw_key).await?;
        // Only cache existing keys (bounded by durable key count); never cache
        // arbitrary invalid tokens so random probes cannot grow cache memory.
        if let (Some(generation), Some(record)) = (generation, loaded.as_ref()) {
            let mut state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 && state.generation == generation {
                state.values.auth_by_token_hash.insert(hash, record.clone());
                state
                    .values
                    .auth_by_id
                    .insert(record.id.clone(), record.clone());
            }
        }
        Ok(loaded)
    }

    async fn find_api_key_by_id(&self, id: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>> {
        let Some(auth) = self.source.auth() else {
            return Ok(None);
        };
        let generation = {
            let state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 {
                if let Some(record) = state.values.auth_by_id.get(id) {
                    return Ok(Some(record.clone()));
                }
                Some(state.generation)
            } else {
                None
            }
        };
        let loaded = auth.find_api_key_by_id(id).await?;
        if let (Some(generation), Some(record)) = (generation, loaded.as_ref()) {
            let mut state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 && state.generation == generation {
                state
                    .values
                    .auth_by_id
                    .insert(record.id.clone(), record.clone());
            }
        }
        Ok(loaded)
    }

    async fn model_access_allowed(&self, api_key_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let Some(auth) = self.source.auth() else {
            return Ok(false);
        };
        let generation = {
            let state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 {
                if state.values.auth_by_id.contains_key(api_key_id)
                    && let Some(bound) = state.values.bound_models.get(api_key_id)
                {
                    return Ok(bound.is_empty() || bound.iter().any(|id| id == model_id));
                }
                Some(state.generation)
            } else {
                None
            }
        };
        let Some(generation) = generation else {
            return auth.model_access_allowed(api_key_id, model_id).await;
        };
        let Some(record) = self.find_api_key_by_id(api_key_id).await? else {
            return Ok(false);
        };
        let bound = auth.list_bound_model_ids(&record.id).await?;
        let allowed = bound.is_empty() || bound.iter().any(|id| id == model_id);
        let unchanged = {
            let mut state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 && state.generation == generation {
                state.values.bound_models.insert(record.id, Arc::new(bound));
                true
            } else {
                false
            }
        };
        if !unchanged {
            // A concurrent mutation raced between `find_api_key_by_id` and
            // `list_bound_model_ids`; defer to the authoritative atomic check.
            return auth.model_access_allowed(api_key_id, model_id).await;
        }
        Ok(allowed)
    }

    async fn list_bound_model_ids(&self, api_key_id: &str) -> anyhow::Result<Vec<String>> {
        let Some(auth) = self.source.auth() else {
            return Ok(Vec::new());
        };
        let generation = {
            let state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 {
                if let Some(bound) = state.values.bound_models.get(api_key_id) {
                    return Ok((**bound).clone());
                }
                Some(state.generation)
            } else {
                None
            }
        };
        let bound = auth.list_bound_model_ids(api_key_id).await?;
        if let Some(generation) = generation {
            let mut state = self.projection.0.lock();
            if state.trusted
                && state.mutations == 0
                && state.generation == generation
                && state.values.auth_by_id.contains_key(api_key_id)
            {
                state
                    .values
                    .bound_models
                    .insert(api_key_id.to_owned(), Arc::new(bound.clone()));
            }
        }
        Ok(bound)
    }
}

#[async_trait]
impl ApiKeyStore for RuntimeConfig {
    async fn list(&self) -> anyhow::Result<Vec<ApiKeyWithBindings>> {
        let Some(store) = self.source.api_keys() else {
            anyhow::bail!("API key store is unavailable");
        };
        store.list().await
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<ApiKeyWithBindings>> {
        let Some(store) = self.source.api_keys() else {
            anyhow::bail!("API key store is unavailable");
        };
        store.get(id).await
    }

    async fn create(&self, input: CreateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let Some(store) = self.source.api_keys() else {
            anyhow::bail!("API key store is unavailable");
        };
        let mutation = self.projection.begin();
        let result = store.create(input).await;
        mutation.finish();
        result
    }

    async fn update(&self, id: &str, input: UpdateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let Some(store) = self.source.api_keys() else {
            anyhow::bail!("API key store is unavailable");
        };
        let mutation = self.projection.begin();
        let result = store.update(id, input).await;
        mutation.finish();
        result
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let Some(store) = self.source.api_keys() else {
            anyhow::bail!("API key store is unavailable");
        };
        let mutation = self.projection.begin();
        let result = store.delete(id).await;
        mutation.finish();
        result
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let Some(store) = self.source.api_keys() else {
            anyhow::bail!("API key store is unavailable");
        };
        store.exists_by_name(name, exclude_id).await
    }

    async fn exists_by_key(&self, key: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let Some(store) = self.source.api_keys() else {
            anyhow::bail!("API key store is unavailable");
        };
        store.exists_by_key(key, exclude_id).await
    }
}

#[async_trait]
impl RouteStore for RuntimeConfig {
    async fn list(&self) -> anyhow::Result<Vec<RouteConfig>> {
        self.source.routes().list().await
    }

    async fn list_active(&self) -> anyhow::Result<Vec<RouteConfig>> {
        self.source.routes().list_active().await
    }

    async fn get(&self, route_id: &str) -> anyhow::Result<Option<RouteConfig>> {
        self.source.routes().get(route_id).await
    }

    async fn put(&self, route: PutRoute) -> anyhow::Result<RouteConfig> {
        self.source.routes().put(route).await
    }

    async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        // Deleting a route cascades into `api_key_models`; clear cached key
        // bindings atomically with the delete.
        let mutation = self.projection.begin();
        let result = self.source.routes().delete(route_id).await;
        mutation.finish();
        result
    }
}

#[async_trait]
impl MoneyStore for RuntimeConfig {
    async fn custom_currency(&self) -> anyhow::Result<Option<CustomCurrency>> {
        self.source.money().custom_currency().await
    }

    async fn save_custom_currency(
        &self,
        symbol: &str,
        usd_rate: Decimal,
    ) -> anyhow::Result<CustomCurrency> {
        // Initial custom-currency configuration converts `api_keys.spend_limit_nanos`
        // in the same transaction; invalidate cached API key records.
        let mutation = self.projection.begin();
        let result = self
            .source
            .money()
            .save_custom_currency(symbol, usd_rate)
            .await;
        mutation.finish();
        result
    }

    async fn provider_cost_plans(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderCostPlan>> {
        self.source.money().provider_cost_plans(provider_id).await
    }

    async fn all_provider_cost_plans(&self) -> anyhow::Result<Vec<ProviderCostPlan>> {
        self.source.money().all_provider_cost_plans().await
    }

    async fn append_provider_cost_plan(
        &self,
        provider_id: &str,
        input: NewProviderCostPlan,
    ) -> anyhow::Result<ProviderCostPlan> {
        self.source
            .money()
            .append_provider_cost_plan(provider_id, input)
            .await
    }

    async fn api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        self.source.money().api_key_spend(api_key_id).await
    }

    async fn reset_api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        self.source.money().reset_api_key_spend(api_key_id).await
    }
}
