use std::time::Duration;

use async_trait::async_trait;

use super::RedactionError;
use stravia_runtime_contract::Principal;

/// 历史回收的 PostgreSQL 协调锁（独占）。回收事务的释放触发器会按行删除顺序对被引用对象依次
/// 加 `FOR NO KEY UPDATE`；两个事务以不同顺序取得同一批行的锁就会死锁。回收事务先取独占锁，
/// 批量更新对象行的写入方（发布、确认）先取共享锁，二者互斥，取锁顺序因此不会成环。
/// 只加 `FOR KEY SHARE` 的提交路径与单行更新不取此锁。
pub const PG_GC_LOCK: &str = "SELECT pg_advisory_xact_lock(8900089)";
/// 同一把锁的共享模式：写入方之间并行，只与回收事务互斥。
pub const PG_GC_LOCK_SHARED: &str = "SELECT pg_advisory_xact_lock_shared(8900089)";

const PENDING_RETENTION: Duration = Duration::from_secs(60 * 60);

/// 空闲过期起点的最小推进间隔。映射在一个回合里每被使用一次都写库代价过高，
/// 所以该时间内的重复使用只读不写；回收时再把这段时间加回保留期，保证实际空闲不少于保留期。
pub const RENEWAL_GRANULARITY: Duration = Duration::from_secs(60 * 60);

// Intentionally no Debug: mapping plaintext must never enter diagnostics.
#[derive(Clone, sqlx::FromRow)]
pub struct Mapping {
    pub reference: String,
    pub secret: String,
    /// 创建映射时检测出它的规则；创建之前的映射为空，无法归因。
    #[sqlx(try_from = "String")]
    pub rule_ids: RuleIds,
}

/// 映射表里以 JSON 数组文本保存的规则 ID。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RuleIds(Vec<String>);

impl RuleIds {
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl From<Vec<String>> for RuleIds {
    fn from(rule_ids: Vec<String>) -> Self {
        Self(rule_ids)
    }
}

impl TryFrom<String> for RuleIds {
    type Error = serde_json::Error;

    fn try_from(encoded: String) -> Result<Self, Self::Error> {
        serde_json::from_str(&encoded).map(Self)
    }
}

/// 待登记的秘密及检测出它的规则；规则只在新建映射时写入，已有映射保持原样。
pub struct DetectedSecret {
    pub secret: String,
    pub rule_ids: Vec<String>,
}

impl From<&str> for DetectedSecret {
    fn from(secret: &str) -> Self {
        Self {
            secret: secret.to_owned(),
            rule_ids: Vec::new(),
        }
    }
}

// 新建下标只在事务成功提交后返回；调用者不能用扫描前状态推断创建归属。
pub struct InternedMappings {
    pub mappings: Vec<Mapping>,
    pub created: Vec<usize>,
}

#[async_trait]
pub trait MappingStore: Send + Sync {
    async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError>;
    /// 实例内是否存在映射（映射不再有过期时间，回收前都有效）。没有映射时读取路径直接返回原响应，不做主体解析与载荷遍历。
    /// 默认返回 `true`，让未知实现保持保守行为。
    async fn any_active(&self) -> Result<bool, RedactionError> {
        Ok(true)
    }
    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[DetectedSecret],
    ) -> Result<InternedMappings, RedactionError>;
    /// 发布只记录首次发布时刻并清空未发布下限，不延长任何保留期。
    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<(), RedactionError>;
    /// 期限扫描入队后由回收步骤删除；`retention` 是当前统一天数。
    async fn sweep(&self, now: i64, retention: Duration) -> Result<u64, RedactionError>;
    /// 按占位符读取映射明文。占位符是映射的唯一标识，观察事件的 principal
    /// 只保存 API Key ID，与映射表的 `api-key:<id>` 存储键并不相同。
    async fn plaintext(&self, reference: &str) -> Result<Option<String>, RedactionError>;
}

#[derive(Clone)]
pub enum SqlMappingStore {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn after(now: i64, retention: Duration) -> i64 {
    now.saturating_add(millis(retention))
}

impl SqlMappingStore {
    pub fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self::Sqlite(pool)
    }

    pub fn postgres(pool: sqlx::PgPool) -> Self {
        Self::Postgres(pool)
    }

    fn storage(&self, _error: sqlx::Error) -> RedactionError {
        RedactionError::Storage
    }

    /// 登记映射在一个成功回合里被使用：首次使用时发布，之后推进空闲过期的起点。
    ///
    /// 同一映射在 [`RENEWAL_GRANULARITY`] 内重复使用不再写库；因此记录的时刻最多落后真实使用
    /// 这么久，回收时把这段宽限加回去（见 `sweep`），保证空闲时长不会被少算。
    /// 先用只读查询挑出确实需要写的引用，全部无需续期时不开写事务：
    /// SQLite 的写事务独占唯一写锁并同步 WAL，热路径上每个回合都付这个代价没有必要。
    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<(), RedactionError> {
        if references.is_empty() {
            return Ok(());
        }
        let mut references: Vec<_> = references.iter().map(String::as_str).collect();
        references.sort_unstable();
        references.dedup();
        let principal = principal.continuation_key();
        let timestamp = now();
        let renew_before = timestamp.saturating_sub(millis(RENEWAL_GRANULARITY));
        match self {
            Self::Sqlite(pool) => {
                let mut due = Vec::new();
                for references in references.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                        "SELECT reference FROM reversible_redaction_mappings WHERE principal = ",
                    );
                    query
                        .push_bind(&principal)
                        .push(" AND (published_at IS NULL OR last_used_at < ")
                        .push_bind(renew_before)
                        .push(") AND reference IN (");
                    let mut list = query.separated(", ");
                    for reference in references {
                        list.push_bind(*reference);
                    }
                    list.push_unseparated(")");
                    due.extend(
                        query
                            .build_query_scalar::<String>()
                            .fetch_all(pool)
                            .await
                            .map_err(|error| self.storage(error))?,
                    );
                }
                if due.is_empty() {
                    return Ok(());
                }
                let mut transaction = pool
                    .begin_write()
                    .await
                    .map_err(|error| self.storage(error))?;
                for references in due.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                        "UPDATE reversible_redaction_mappings SET published_at = COALESCE(published_at, ",
                    );
                    query
                        .push_bind(timestamp)
                        .push("), pending_until = NULL, last_used_at = ")
                        .push_bind(timestamp)
                        .push(" WHERE principal = ")
                        .push_bind(&principal)
                        .push(" AND (published_at IS NULL OR last_used_at < ")
                        .push_bind(renew_before)
                        .push(") AND reference IN (");
                    let mut list = query.separated(", ");
                    for reference in references {
                        list.push_bind(reference.as_str());
                    }
                    list.push_unseparated(")");
                    query
                        .build()
                        .execute(&mut *transaction)
                        .await
                        .map_err(|error| self.storage(error))?;
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
            Self::Postgres(pool) => {
                let mut due = Vec::new();
                for references in references.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                        "SELECT reference FROM reversible_redaction_mappings WHERE principal = ",
                    );
                    query
                        .push_bind(&principal)
                        .push(" AND (published_at IS NULL OR last_used_at < ")
                        .push_bind(renew_before)
                        .push(") AND reference IN (");
                    let mut list = query.separated(", ");
                    for reference in references {
                        list.push_bind(*reference);
                    }
                    list.push_unseparated(")");
                    due.extend(
                        query
                            .build_query_scalar::<String>()
                            .fetch_all(pool)
                            .await
                            .map_err(|error| self.storage(error))?,
                    );
                }
                if due.is_empty() {
                    return Ok(());
                }
                // 按引用排序，使并发批量续期以同一顺序锁行。
                due.sort_unstable();
                let mut transaction = pool.begin().await.map_err(|error| self.storage(error))?;
                // 批量发布按调用方顺序锁映射行，与历史回收触发器的取锁顺序无关；
                // 先取回收协调锁的共享模式，使两者互斥，取锁顺序不会成环。
                sqlx::query(PG_GC_LOCK_SHARED)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                // The same principal lock as intern makes lifecycle decisions after any writer wait.
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(&principal)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                for references in due.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                        "UPDATE reversible_redaction_mappings SET published_at = COALESCE(published_at, ",
                    );
                    query
                        .push_bind(timestamp)
                        .push("), pending_until = NULL, last_used_at = ")
                        .push_bind(timestamp)
                        .push(" WHERE principal = ")
                        .push_bind(&principal)
                        .push(" AND (published_at IS NULL OR last_used_at < ")
                        .push_bind(renew_before)
                        .push(") AND reference IN (");
                    let mut list = query.separated(", ");
                    for reference in references {
                        list.push_bind(reference.as_str());
                    }
                    list.push_unseparated(")");
                    query
                        .build()
                        .execute(&mut *transaction)
                        .await
                        .map_err(|error| self.storage(error))?;
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MappingStore for SqlMappingStore {
    async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError> {
        let principal = principal.continuation_key();
        match self {
            Self::Sqlite(pool) => sqlx::query_as::<_, Mapping>(
                "SELECT reference, secret, rule_ids FROM reversible_redaction_mappings \
                 WHERE principal = ? ORDER BY reference",
            )
            .bind(principal)
            .fetch_all(pool)
            .await
            .map_err(|error| self.storage(error)),
            Self::Postgres(pool) => sqlx::query_as::<_, Mapping>(
                "SELECT reference, secret, rule_ids FROM reversible_redaction_mappings \
                 WHERE principal = $1 ORDER BY reference",
            )
            .bind(principal)
            .fetch_all(pool)
            .await
            .map_err(|error| self.storage(error)),
        }
    }

    async fn any_active(&self) -> Result<bool, RedactionError> {
        match self {
            Self::Sqlite(pool) => {
                sqlx::query_scalar::<_, i64>("SELECT 1 FROM reversible_redaction_mappings LIMIT 1")
                    .fetch_optional(pool)
                    .await
                    .map(|row| row.is_some())
                    .map_err(|error| self.storage(error))
            }
            Self::Postgres(pool) => {
                sqlx::query_scalar::<_, i32>("SELECT 1 FROM reversible_redaction_mappings LIMIT 1")
                    .fetch_optional(pool)
                    .await
                    .map(|row| row.is_some())
                    .map_err(|error| self.storage(error))
            }
        }
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[DetectedSecret],
    ) -> Result<InternedMappings, RedactionError> {
        if secrets.is_empty() {
            return Ok(InternedMappings {
                mappings: Vec::new(),
                created: Vec::new(),
            });
        }
        let principal = principal.continuation_key();
        let mut mappings = Vec::with_capacity(secrets.len());
        let mut created = Vec::new();
        // Serialize lookup-and-insert across processes, not just this store instance.
        // Secrets can be arbitrarily long (e.g. private keys), so do not B-tree index them.
        match self {
            Self::Sqlite(pool) => {
                let mut transaction = pool
                    .begin_write()
                    .await
                    .map_err(|error| self.storage(error))?;
                let now = now();
                for secret in secrets {
                    let existing = sqlx::query_as::<_, Mapping>(
                        "SELECT reference, secret, rule_ids FROM reversible_redaction_mappings \
                         WHERE principal = ? AND secret = ?",
                    )
                    .bind(&principal)
                    .bind(&secret.secret)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                    let mapping = match existing {
                        Some(mapping) => mapping,
                        None => {
                            created.push(mappings.len());
                            let reference = super::marker::new_reference();
                            let rule_ids = serde_json::to_string(&secret.rule_ids)
                                .map_err(|_| RedactionError::Storage)?;
                            sqlx::query(
                                "INSERT INTO reversible_redaction_mappings \
                                 (reference, principal, secret, rule_ids, created_at, last_used_at, pending_until) \
                                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                            ).bind(&reference).bind(&principal).bind(&secret.secret).bind(&rule_ids).bind(now).bind(now)
                                .bind(after(now, PENDING_RETENTION))
                                .execute(&mut *transaction).await.map_err(|error| self.storage(error))?;
                            Mapping {
                                reference,
                                secret: secret.secret.clone(),
                                rule_ids: secret.rule_ids.clone().into(),
                            }
                        }
                    };
                    mappings.push(mapping);
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(|error| self.storage(error))?;
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(&principal)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                let now = now();
                for secret in secrets {
                    let existing = sqlx::query_as::<_, Mapping>(
                        "SELECT reference, secret, rule_ids FROM reversible_redaction_mappings \
                         WHERE principal = $1 AND secret = $2",
                    )
                    .bind(&principal)
                    .bind(&secret.secret)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                    let mapping = match existing {
                        Some(mapping) => mapping,
                        None => {
                            created.push(mappings.len());
                            let reference = super::marker::new_reference();
                            let rule_ids = serde_json::to_string(&secret.rule_ids)
                                .map_err(|_| RedactionError::Storage)?;
                            sqlx::query(
                                "INSERT INTO reversible_redaction_mappings \
                                 (reference, principal, secret, rule_ids, created_at, last_used_at, pending_until) \
                                 VALUES ($1, $2, $3, $4, $5, $5, $6)",
                            ).bind(&reference).bind(&principal).bind(&secret.secret).bind(&rule_ids).bind(now)
                                .bind(after(now, PENDING_RETENTION))
                                .execute(&mut *transaction).await.map_err(|error| self.storage(error))?;
                            Mapping {
                                reference,
                                secret: secret.secret.clone(),
                                rule_ids: secret.rule_ids.clone().into(),
                            }
                        }
                    };
                    mappings.push(mapping);
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
        }
        Ok(InternedMappings { mappings, created })
    }

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
    ) -> Result<(), RedactionError> {
        SqlMappingStore::publish(self, principal, references).await
    }

    async fn plaintext(&self, reference: &str) -> Result<Option<String>, RedactionError> {
        match self {
            Self::Sqlite(pool) => sqlx::query_scalar(
                "SELECT secret FROM reversible_redaction_mappings \
                 WHERE reference = ?",
            )
            .bind(reference)
            .fetch_optional(pool)
            .await
            .map_err(|error| self.storage(error)),
            Self::Postgres(pool) => sqlx::query_scalar(
                "SELECT secret FROM reversible_redaction_mappings \
                 WHERE reference = $1",
            )
            .bind(reference)
            .fetch_optional(pool)
            .await
            .map_err(|error| self.storage(error)),
        }
    }

    /// 期限扫描负责入队，队列回收是唯一删除方；返回本轮删除的映射数。
    ///
    /// 已发布的映射在最近一次使用起超过 `retention`（再加续期粒度）后回收；
    /// 未发布的映射按 `pending_until`。
    ///
    /// `last_used_at` 会被续期推后，但扫描水位仍然安全：续期写入的总是当前时间，
    /// 大于任何已扫描过的视界，所以被续期的行只会在之后的扫描里重新进入范围；
    /// 已入队的行被续期后，领取时按最新的 `last_used_at` 复查并移出队列。
    async fn sweep(&self, now: i64, retention: Duration) -> Result<u64, RedactionError> {
        let threshold = now
            .saturating_sub(millis(retention))
            .saturating_sub(millis(RENEWAL_GRANULARITY));
        macro_rules! sweep {
            ($store:expr, $pool:expr, $skip_locked:literal) => {{
                let store = $store;
                let pool = $pool;
                {
                    let mut tx = pool
                        .begin_write()
                        .await
                        .map_err(|error| store.storage(error))?;
                    for (name, column, horizon, filter) in [
                        (WATERMARK_PENDING, "pending_until", now, ""),
                        // 未发布的行 `last_used_at` 只是创建时刻，由 `pending_until` 负责。
                        (WATERMARK_LAST_USED, "last_used_at", threshold, " AND m.published_at IS NOT NULL"),
                    ] {
                        let watermark: i64 = sqlx::query_scalar(
                            "SELECT position FROM history_retention_watermarks WHERE name = $1",
                        ).bind(name).fetch_one(&mut *tx).await.map_err(|error| store.storage(error))?;
                        if horizon < watermark {
                            sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                                .bind(name).bind(horizon).execute(&mut *tx).await.map_err(|error| store.storage(error))?;
                            continue;
                        }
                        let query = format!(
                            "INSERT INTO redaction_mapping_gc(reference) \
                             SELECT m.reference FROM reversible_redaction_mappings m \
                             WHERE m.{column} > $1 AND m.{column} <= $2{filter} \
                             AND NOT EXISTS(SELECT 1 FROM turn_node_redaction_mappings r WHERE r.reference=m.reference) \
                             ON CONFLICT(reference) DO NOTHING",
                        );
                        sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                            .bind(watermark).bind(horizon)
                            .execute(&mut *tx).await.map_err(|error| store.storage(error))?;
                        sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                            .bind(name).bind(horizon).execute(&mut *tx).await.map_err(|error| store.storage(error))?;
                    }
                    tx.commit().await.map_err(|error| store.storage(error))?;
                }
                let mut removed = 0u64;
                loop {
                    let mut tx = pool
                        .begin_write()
                        .await
                        .map_err(|error| store.storage(error))?;
                    let claim = format!(
                        "SELECT g.reference FROM redaction_mapping_gc g \
                         JOIN reversible_redaction_mappings m ON m.reference = g.reference \
                         ORDER BY g.reference LIMIT {SWEEP_BATCH}{}",
                        $skip_locked,
                    );
                    let claimed: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(claim.as_str()))
                        .fetch_all(&mut *tx).await.map_err(|error| store.storage(error))?;
                    if claimed.is_empty() {
                        tx.commit().await.map_err(|error| store.storage(error))?;
                        break;
                    }
                    for reference in &claimed {
                        let deleted = sqlx::query(
                            "DELETE FROM reversible_redaction_mappings WHERE reference = $1 \
                             AND ((published_at IS NULL AND pending_until <= $2) \
                                  OR (published_at IS NOT NULL AND last_used_at <= $3)) \
                             AND NOT EXISTS(SELECT 1 FROM turn_node_redaction_mappings r WHERE r.reference=reversible_redaction_mappings.reference)",
                        )
                        .bind(reference).bind(now).bind(threshold)
                        .execute(&mut *tx).await.map_err(|error| store.storage(error))?.rows_affected();
                        if deleted == 0 {
                            sqlx::query("DELETE FROM redaction_mapping_gc WHERE reference = $1")
                                .bind(reference).execute(&mut *tx).await.map_err(|error| store.storage(error))?;
                        } else {
                            removed = removed.saturating_add(deleted);
                        }
                    }
                    tx.commit().await.map_err(|error| store.storage(error))?;
                }
                Ok(removed)
            }};
        }
        match self {
            Self::Sqlite(pool) => sweep!(self, pool, ""),
            Self::Postgres(pool) => sweep!(self, pool, " FOR UPDATE OF g, m SKIP LOCKED"),
        }
    }
}

const SWEEP_BATCH: i64 = 512;
const WATERMARK_PENDING: &str = "reversible_redaction_mappings.pending_until";
const WATERMARK_LAST_USED: &str = "reversible_redaction_mappings.last_used_at";
