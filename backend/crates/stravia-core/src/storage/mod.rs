pub mod maintenance;
pub mod memory;
mod model_changes;
pub mod money;
pub mod postgres;
pub mod sql;
pub mod sqlite;
pub mod traits;

pub use money::CUSTOM_CURRENCY_SETTING_KEY;

/// 供运行实例发现已提交配置变化的存储键。
pub const CONFIG_EPOCH_KEY: &str = "config_epoch";

/// 普通配置写入后的刷新通知；原子操作应在自身事务中更新。
pub(crate) async fn bump_config_epoch(store: &dyn SettingsStore) -> anyhow::Result<()> {
    let current: i64 = store
        .get(CONFIG_EPOCH_KEY)
        .await?
        .as_deref()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    store
        .set(CONFIG_EPOCH_KEY, &(current + 1).to_string())
        .await
}

pub use memory::MemoryStorage;
pub use postgres::PostgresStorage;
pub use sqlite::SqliteStorage;
pub use traits::{
    AdminIdentityRecord, AdminIdentityStore, AdminSessionRecord, ApiKeyAccessRecord, ApiKeySpend,
    ApiKeyStore, AuthAccessStore, DynStorage, MoneyStore, NewAdminIdentity, NewAdminSession,
    NewProviderCostPlan, ProviderCostPlan, ProviderModelStore, ProviderStore, RouteSchedulingUsage,
    RouteStore, SettingsStore, Storage, StorageBootstrap, UsageStatsStore,
};
