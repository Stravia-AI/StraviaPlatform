//! 统一历史保留期：一个内存中的天数 N 与唯一的扫描入口。
//!
//! Turn Chain 与 Observation 共用管理员的「历史保留」天数。Marker、可逆脱敏
//! 映射与原生压缩登记不保存到期时间：仍被 Turn 节点引用时不回收，失去引用后
//! 才按未发布窗口 `pending_until` 或首次发布/确认时刻 + N 回收。读取没有时间
//! 条件，扫描是唯一的删除方。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use stravia_runtime_contract::turn_chain::TurnChainStore;

use stravia_credential_protection::store::MappingStore;

pub(crate) const DEFAULT_RETENTION_DAYS: u32 = 7;
pub(crate) const MIN_RETENTION_DAYS: u32 = 1;

/// 共享正文存储的回收入口；必须在所有引用方回收之后调用。
#[derive(Clone)]
pub(crate) enum ContentGc {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

impl ContentGc {
    async fn sweep(&self) -> anyhow::Result<u64> {
        match self {
            Self::Sqlite(pool) => crate::content_store::sweep_sqlite(pool).await,
            Self::Postgres(pool) => crate::content_store::sweep_postgres(pool).await,
        }
    }
}

pub(crate) const DAY_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// Gateway 持有的保留期句柄；各域读取同一份内存天数。
#[derive(Clone)]
pub(crate) struct HistoryRetention {
    days: Arc<AtomicU32>,
}

impl HistoryRetention {
    pub(crate) fn new(days: u32) -> Self {
        Self {
            days: Arc::new(AtomicU32::new(days.max(MIN_RETENTION_DAYS))),
        }
    }

    /// 启动时读取存储设置；缺失、无法解析或低于最小值的值按最小值处理。
    pub(crate) fn from_setting(value: Option<&str>) -> Self {
        let days = value
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        Self::new(days)
    }

    pub(crate) fn days(&self) -> u32 {
        self.days.load(Ordering::Acquire)
    }

    pub(crate) fn set_days(&self, days: u32) -> u32 {
        let days = days.max(MIN_RETENTION_DAYS);
        self.days.store(days, Ordering::Release);
        days
    }

    pub(crate) fn window_millis(&self) -> i64 {
        i64::from(self.days()) * DAY_MILLIS
    }

    pub(crate) fn window(&self) -> Duration {
        Duration::from_millis(u64::try_from(self.window_millis()).unwrap_or(u64::MAX))
    }

    /// 根对象计时：`since_ms + N`。
    pub(crate) fn deadline(&self, since_ms: i64) -> i64 {
        since_ms.saturating_add(self.window_millis())
    }
}

/// Core 的单一历史维护入口。顺序固定为 Observation、Turn 节点、依赖、正文，
/// 先删引用方，同一轮释放的依赖与正文在同一轮删除。
pub(crate) struct HistoryMaintenance {
    pub(crate) retention: HistoryRetention,
    pub(crate) observation: crate::interaction_observation::InteractionObservation,
    pub(crate) turn_chain: Arc<dyn TurnChainStore>,
    pub(crate) history_markers: Arc<dyn crate::history_marker::HistoryMarkerStore>,
    pub(crate) mappings: Arc<dyn MappingStore>,
    pub(crate) compaction: crate::compaction::Compaction,
    pub(crate) content: ContentGc,
}

impl HistoryMaintenance {
    #[tracing::instrument(
        target = "stravia::perf",
        name = "history.sweep",
        skip_all,
        fields(deleted)
    )]
    pub(crate) async fn sweep_history(&self, now: i64) -> anyhow::Result<u64> {
        let retention = self.retention.window();
        let mut deleted = self.observation.sweep_history(now, retention).await?;
        deleted = deleted.saturating_add(self.turn_chain.sweep_history(now).await?);
        deleted = deleted.saturating_add(self.history_markers.sweep(now, retention).await?);
        deleted = deleted.saturating_add(self.mappings.sweep(now, retention).await?);
        deleted = deleted.saturating_add(self.compaction.sweep(now, retention).await?);
        deleted = deleted.saturating_add(self.content.sweep().await?);
        tracing::Span::current().record("deleted", deleted);
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests;
