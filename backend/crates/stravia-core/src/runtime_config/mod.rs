//! 常驻配置投影；集中处理预加载、负查询、写入代际与取消后的权威读取。
//! SQL 仍拥有事实和事务，权限、OAuth、历史与执行状态不进入本投影。

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
};

use parking_lot::Mutex;

use crate::{
    db::models::Provider,
    plugin::PluginStore,
    storage::{DynStorage, traits::*},
};

#[cfg(test)]
mod concurrency_tests;
mod models;
mod providers;
mod settings;
#[cfg(test)]
mod tests;

use models::ModelIndex;

pub(crate) struct RuntimeConfig {
    source: DynStorage,
    projection: Arc<Projection>,
    plugins: PluginStore,
}

#[derive(Default)]
struct Values {
    providers: HashMap<String, Option<Provider>>,
    models: HashMap<String, Arc<ModelIndex>>,
    settings: HashMap<String, Option<String>>,
    invalid_providers: Option<HashSet<String>>,
    suspended_providers: Option<HashSet<String>>,
}

struct State {
    generation: u64,
    mutations: usize,
    trusted: bool,
    values: Values,
}

pub(crate) struct Projection(Mutex<State>);

pub(crate) struct Mutation<'a> {
    projection: &'a Projection,
    finished: bool,
}

impl Projection {
    pub(crate) fn begin(&self) -> Mutation<'_> {
        let mut state = self.0.lock();
        state.generation = state.generation.wrapping_add(1);
        state.mutations += 1;
        state.values = Values::default();
        Mutation {
            projection: self,
            finished: false,
        }
    }
}

impl Mutation<'_> {
    pub(crate) fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        let mut state = self.projection.0.lock();
        state.generation = state.generation.wrapping_add(1);
        state.mutations -= 1;
        state.values = Values::default();
        if !self.finished && state.trusted {
            // 取消可能发生在数据库已收到 COMMIT、调用方尚未收到结果之间。
            // 此后仅委托权威存储，不能让一次较早完成的读取重新固化旧值。
            state.trusted = false;
            tracing::warn!(
                "Runtime configuration mutation interrupted; using authoritative storage until restart"
            );
        }
    }
}

impl RuntimeConfig {
    /// 装载当前实例的小型执行配置；返回同一个存储 Interface，调用者不管理缓存。
    pub(crate) async fn attach(source: DynStorage) -> anyhow::Result<DynStorage> {
        let providers = source.providers().list().await?;
        let mut values = Values::default();
        for provider in providers {
            let models = source
                .provider_models()
                .list_for_provider(&provider.id)
                .await?;
            values
                .models
                .insert(provider.id.clone(), Arc::new(ModelIndex::new(models)));
            values.providers.insert(provider.id.clone(), Some(provider));
        }
        values.invalid_providers =
            Some(source.providers().credential_invalid_provider_ids().await?);
        values.suspended_providers = Some(
            source
                .providers()
                .allowance_suspended_provider_ids()
                .await?,
        );
        values.settings.insert(
            crate::outbound_proxy::SETTINGS_KEY.to_owned(),
            source
                .settings()
                .get(crate::outbound_proxy::SETTINGS_KEY)
                .await?,
        );
        let projection = Arc::new(Projection(Mutex::new(State {
            generation: 0,
            mutations: 0,
            trusted: true,
            values,
        })));
        let plugins = source
            .vendor_plugins()
            .with_config_projection(projection.clone());
        Ok(Arc::new(Self {
            source,
            projection,
            plugins,
        }))
    }

    async fn read<T: Clone, F: Future<Output = anyhow::Result<T>>>(
        &self,
        select: impl FnOnce(&Values) -> Option<T>,
        install: impl FnOnce(&mut Values, T),
        load: impl FnOnce() -> F,
    ) -> anyhow::Result<T> {
        let generation = {
            let state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 {
                if let Some(value) = select(&state.values) {
                    return Ok(value);
                }
                Some(state.generation)
            } else {
                None
            }
        };
        let value = load().await?;
        if let Some(generation) = generation {
            let mut state = self.projection.0.lock();
            if state.trusted && state.mutations == 0 && state.generation == generation {
                install(&mut state.values, value.clone());
            }
        }
        Ok(value)
    }
}

impl Storage for RuntimeConfig {
    fn vendor_plugins(&self) -> &PluginStore {
        &self.plugins
    }
    fn providers(&self) -> &dyn ProviderStore {
        self
    }
    fn web_providers(&self) -> Option<&dyn WebProviderStore> {
        self.source.web_providers()
    }
    fn routes(&self) -> &dyn RouteStore {
        self.source.routes()
    }
    fn provider_models(&self) -> &dyn ProviderModelStore {
        self
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
    fn money(&self) -> &dyn crate::storage::traits::MoneyStore {
        self.source.money()
    }
    fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
        self.source.api_keys()
    }
    fn auth(&self) -> Option<&dyn AuthAccessStore> {
        self.source.auth()
    }
    fn admin_identity(&self) -> Option<&dyn AdminIdentityStore> {
        self.source.admin_identity()
    }
    fn usage_stats(&self) -> &dyn UsageStatsStore {
        self.source.usage_stats()
    }
    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore {
        self.source.oauth_credentials()
    }
    fn bootstrap(&self) -> &dyn StorageBootstrap {
        self.source.bootstrap()
    }
}
