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

/// PostgreSQL 依赖回收事务的全局互斥（事务级咨询锁）。
///
/// 释放触发器会在事务内对被引用对象依次加 `FOR NO KEY UPDATE`；两个同时回收的事务
/// 以不同顺序取得同一批对象的锁就会互相死锁。回收事务先取此锁，使取锁顺序总是
/// 串行的。批量更新对象行的写入方取同一把锁的共享模式（见 [`PG_GC_LOCK_SHARED`]），
/// 提交路径只对依赖行加不冲突的 `FOR KEY SHARE`，不取此锁。节点回收先取 `turn_nodes`
/// 的表锁、再取此锁，使等待在途提交的期间不阻塞发布；取锁层级见 ADR-0089 §PostgreSQL 锁协议。
pub(crate) const PG_GC_LOCK: &str = stravia_credential_protection::store::PG_GC_LOCK;
/// 批量更新对象行的写入方（Marker 发布、原生压缩确认、映射发布、启动恢复）取同一把锁的共享模式。
pub(crate) const PG_GC_LOCK_SHARED: &str = stravia_credential_protection::store::PG_GC_LOCK_SHARED;

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

    /// 测试用的默认保留期：与生产默认值相同，但生产构造必须显式传入句柄。
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::new(DEFAULT_RETENTION_DAYS)
    }

    /// 启动时读取存储设置：缺失按默认值；无法解析时告警并回退默认值；
    /// 低于最小值的值按最小值处理。
    pub(crate) fn from_setting(value: Option<&str>) -> Self {
        let days = match value {
            None => DEFAULT_RETENTION_DAYS,
            Some(raw) => raw.trim().parse::<u32>().unwrap_or_else(|_| {
                tracing::warn!(
                    value = raw,
                    "history retention setting is not a day count; using the default"
                );
                DEFAULT_RETENTION_DAYS
            }),
        };
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

    #[cfg(test)]
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
    pub(crate) prompt_admissions: Arc<dyn crate::storage::PromptAdmissionStore>,
    pub(crate) atomic_state: crate::plugin::PluginStore,
    pub(crate) history_markers: Arc<dyn crate::history_marker::HistoryMarkerStore>,
    pub(crate) mappings: Arc<dyn MappingStore>,
    pub(crate) compaction: crate::compaction::Compaction,
    pub(crate) content: ContentGc,
}

impl HistoryMaintenance {
    pub(crate) async fn clear_history(
        &self,
    ) -> anyhow::Result<crate::interaction_observation::ClearHistoryResult> {
        let history = self.observation.clear_history().await;
        let atomic = self
            .atomic_state
            .clear_conversation_atomic_state(self.prompt_admissions.as_ref())
            .await;
        let journal = self.atomic_state.sweep_atomic_journal().await;
        let mut failures = Vec::new();
        for (step, error) in [
            ("observation", history.as_ref().err()),
            ("atomic conversation state", atomic.as_ref().err()),
            ("atomic state journal", journal.as_ref().err()),
        ] {
            if let Some(error) = error {
                tracing::warn!(step, error = %format!("{error:#}"), "history clear step failed");
                failures.push(format!("{step}: {error:#}"));
            }
        }
        if failures.is_empty() {
            history
        } else {
            anyhow::bail!(
                "history clear partially failed; some cleanup may have succeeded: {}",
                failures.join("; ")
            )
        }
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "history.sweep",
        skip_all,
        fields(deleted)
    )]
    /// 每一步都会尝试：某一步失败只记录并在最后统一返回错误，不会让其余步骤
    /// （尤其是依赖已成功回收之后的正文回收）被跳过。
    pub(crate) async fn sweep_history(&self, now: i64) -> anyhow::Result<u64> {
        let window_ms = self.retention.window_millis();
        let retention = Duration::from_millis(u64::try_from(window_ms).unwrap_or(u64::MAX));
        let mut failures: Vec<String> = Vec::new();
        macro_rules! step {
            ($name:literal, $call:expr) => {
                match $call.await {
                    Ok(count) => count,
                    Err(error) => {
                        let error = anyhow::Error::from(error);
                        tracing::warn!(step = $name, error = %format!("{error:#}"), "history sweep step failed");
                        failures.push(format!("{}: {error:#}", $name));
                        0
                    }
                }
            };
        }
        let mut deleted = step!(
            "observation",
            self.observation.sweep_history(now, retention)
        );
        // 原生登记删除会释放来源节点，节点删除又会释放 Marker 与映射，因此依赖回收
        // 重复到某一轮没有任何删除为止（每轮至少删除一行，必然终止）；失败的一轮不再重复。
        loop {
            let failed_before = failures.len();
            let mut round = step!("turn nodes", self.turn_chain.sweep_history(now));
            round = round.saturating_add(step!(
                "history markers",
                self.history_markers.sweep(now, retention)
            ));
            round = round.saturating_add(step!("mappings", self.mappings.sweep(now, retention)));
            round = round.saturating_add(step!(
                "native compactions",
                self.compaction.sweep(now, retention)
            ));
            deleted = deleted.saturating_add(round);
            if round == 0 || failures.len() > failed_before {
                break;
            }
        }
        deleted = deleted.saturating_add(step!(
            "prompt admissions",
            self.prompt_admissions
                .prune_before(now.saturating_sub(window_ms))
        ));
        deleted = deleted.saturating_add(step!(
            "atomic conversation state",
            self.atomic_state.sweep_conversation_atomic_state(
                now.saturating_sub(window_ms),
                self.prompt_admissions.as_ref()
            )
        ));
        deleted = deleted.saturating_add(step!(
            "atomic state journal",
            self.atomic_state.sweep_atomic_journal()
        ));
        deleted = deleted.saturating_add(step!("content", self.content.sweep()));
        tracing::Span::current().record("deleted", deleted);
        if failures.is_empty() {
            Ok(deleted)
        } else {
            anyhow::bail!("history sweep failed: {}", failures.join("; "))
        }
    }
}

#[cfg(test)]
mod tests;
