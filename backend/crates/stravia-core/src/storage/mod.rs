pub mod maintenance;
pub mod memory;
mod model_changes;
pub mod money;
pub mod postgres;
#[cfg(test)]
mod prompt_admission_tests;
pub mod sql;
pub mod sqlite;
pub mod traits;

pub use money::CUSTOM_CURRENCY_SETTING_KEY;

fn validate_admission(admission: &PromptAdmission) -> anyhow::Result<Option<i64>> {
    anyhow::ensure!(
        (admission.ordinal == 0
            && admission.user_prompt_id.is_none()
            && admission.user_prompt_created_at_ms.is_none())
            || (admission.ordinal > 0
                && admission.user_prompt_id.is_some()
                && admission.user_prompt_created_at_ms.is_some()),
        "admission prompt identity does not match ordinal"
    );
    anyhow::ensure!(
        admission.ordinal == 0
            || (admission.user_prompt_id.as_deref() == Some(admission.logical_turn_id.as_str())
                && admission.user_prompt_created_at_ms
                    == Some(admission.logical_turn_created_at_ms)),
        "logical turn does not match user prompt"
    );
    anyhow::ensure!(
        admission.context_window_id.is_some() == admission.context_window_index.is_some(),
        "incomplete context window position"
    );
    admission
        .context_window_index
        .map(i64::try_from)
        .transpose()
        .map_err(Into::into)
}

pub(crate) struct MergedApiKeyUpdate {
    pub(crate) token: String,
    pub(crate) name: String,
    pub(crate) rpm_limit: Option<i32>,
    pub(crate) spend_limit_nanos: Option<i64>,
    pub(crate) is_enabled: bool,
    pub(crate) mcp_access_enabled: bool,
    pub(crate) transparent_injection_enabled: bool,
    pub(crate) transparent_injections: crate::db::models::TransparentInjectionSet,
    pub(crate) expires_at: Option<String>,
}

pub(crate) fn merge_api_key_update(
    current: crate::db::models::ApiKey,
    input: crate::db::models::UpdateApiKey,
) -> anyhow::Result<MergedApiKeyUpdate> {
    let spend_limit_nanos = match input.spend_limit {
        Some(value) => crate::storage::money::parse_spend_limit(value.as_deref())?,
        None => current.spend_limit_nanos,
    };
    Ok(MergedApiKeyUpdate {
        token: input.key.unwrap_or(current.token),
        name: input.name.unwrap_or(current.name),
        rpm_limit: input.rpm_limit.unwrap_or(current.rpm_limit),
        spend_limit_nanos,
        is_enabled: input.is_enabled.unwrap_or(current.is_enabled),
        mcp_access_enabled: input
            .mcp_access_enabled
            .unwrap_or(current.mcp_access_enabled),
        transparent_injection_enabled: input
            .transparent_injection_enabled
            .unwrap_or(current.transparent_injection_enabled),
        transparent_injections: input
            .transparent_injections
            .unwrap_or(current.transparent_injections),
        expires_at: input.expires_at.or(current.expires_at),
    })
}

pub(crate) fn normalized_api_key_model_ids(model_ids: &[String]) -> Vec<&str> {
    let mut seen = std::collections::HashSet::new();
    let mut normalized = Vec::new();
    for id in model_ids {
        let trimmed = id.trim();
        if !trimmed.is_empty() && seen.insert(trimmed) {
            normalized.push(trimmed);
        }
    }
    normalized
}

/// 供运行实例发现已提交配置变化的存储键。
pub const CONFIG_EPOCH_KEY: &str = "config_epoch";

/// 普通配置写入后的刷新通知；原子操作应在自身事务中更新。
pub async fn bump_config_epoch(store: &dyn SettingsStore) -> anyhow::Result<()> {
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

const ROUTE_SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_secs(1);

struct CachedRouteSnapshot {
    fetched_at: Option<std::time::Instant>,
    targets: Vec<crate::router::TargetSchedulingSnapshot>,
}

/// 路由 1 小时统计快照的单飞短 TTL 缓存：避免高并发下每个请求全表扫描并排序最近一小时的
/// `target_attempt_observations`，刷新失败时仍回退到最近一次确认快照并标记 `stale: true`。
#[derive(Clone)]
pub(crate) struct RouteSnapshotCache {
    state: std::sync::Arc<parking_lot::RwLock<CachedRouteSnapshot>>,
    refresh_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl Default for RouteSnapshotCache {
    fn default() -> Self {
        Self {
            state: std::sync::Arc::new(parking_lot::RwLock::new(CachedRouteSnapshot {
                fetched_at: None,
                targets: Vec::new(),
            })),
            refresh_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl RouteSnapshotCache {
    pub(crate) async fn get_or_refresh<F, Fut>(&self, fetch: F) -> RouteSchedulingUsage
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<crate::router::TargetSchedulingSnapshot>, sqlx::Error>>,
    {
        self.get_or_refresh_with_ttl(ROUTE_SNAPSHOT_TTL, fetch)
            .await
    }

    async fn get_or_refresh_with_ttl<F, Fut>(
        &self,
        ttl: std::time::Duration,
        fetch: F,
    ) -> RouteSchedulingUsage
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<crate::router::TargetSchedulingSnapshot>, sqlx::Error>>,
    {
        let now = std::time::Instant::now();
        {
            let guard = self.state.read();
            if guard
                .fetched_at
                .is_some_and(|at| now.saturating_duration_since(at) < ttl)
            {
                return RouteSchedulingUsage {
                    targets: guard.targets.clone(),
                    stale: false,
                };
            }
        }
        let _refresh = self.refresh_lock.lock().await;
        let now = std::time::Instant::now();
        {
            let guard = self.state.read();
            if guard
                .fetched_at
                .is_some_and(|at| now.saturating_duration_since(at) < ttl)
            {
                return RouteSchedulingUsage {
                    targets: guard.targets.clone(),
                    stale: false,
                };
            }
        }
        match fetch().await {
            Ok(targets) => {
                let mut guard = self.state.write();
                guard.fetched_at = Some(std::time::Instant::now());
                guard.targets = targets.clone();
                RouteSchedulingUsage {
                    targets,
                    stale: false,
                }
            }
            Err(error) => {
                tracing::warn!(%error, "failed to refresh confirmed route scheduling usage");
                let targets = self.state.read().targets.clone();
                RouteSchedulingUsage {
                    targets,
                    stale: true,
                }
            }
        }
    }
}

pub use traits::{
    AdminIdentityRecord, AdminIdentityStore, AdminSessionRecord, ApiKeyAccessRecord, ApiKeySpend,
    ApiKeyStore, AuthAccessStore, DynStorage, MoneyStore, NewAdminIdentity, NewAdminSession,
    NewProviderCostPlan, PromptAdmission, PromptAdmissionStore, ProviderCostPlan,
    ProviderModelStore, ProviderStore, RouteSchedulingUsage, RouteStore, SettingsStore, Storage,
    StorageBootstrap, UsageStatsStore,
};

#[cfg(test)]
mod route_snapshot_cache_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn single_flight_reuses_fresh_snapshot_and_falls_back_stale_on_error() {
        let cache = RouteSnapshotCache::default();
        let fetches = std::sync::Arc::new(AtomicUsize::new(0));
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let cache = cache.clone();
            let fetches = std::sync::Arc::clone(&fetches);
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                cache
                    .get_or_refresh_with_ttl(std::time::Duration::from_secs(60), || {
                        let fetches = std::sync::Arc::clone(&fetches);
                        async move {
                            fetches.fetch_add(1, Ordering::SeqCst);
                            tokio::task::yield_now().await;
                            Ok(vec![
                                crate::router::TargetSchedulingSnapshot::default(),
                                crate::router::TargetSchedulingSnapshot::default(),
                            ])
                        }
                    })
                    .await
            }));
        }
        for handle in handles {
            let usage = handle.await.unwrap();
            assert!(!usage.stale);
            assert_eq!(usage.targets.len(), 2);
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "concurrent misses must coalesce into one single-flight refresh"
        );

        let fallback = cache
            .get_or_refresh_with_ttl(std::time::Duration::ZERO, || async {
                Err(sqlx::Error::PoolTimedOut)
            })
            .await;
        assert!(fallback.stale);
        assert_eq!(fallback.targets.len(), 2);
    }
}
