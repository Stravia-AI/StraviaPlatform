use super::*;

#[derive(Clone)]
pub(super) enum Store {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

pub(super) struct State {
    pub identity: Option<String>,
    pub fingerprint: String,
    pub payload: Value,
}

pub(super) struct Match {
    pub record: CompactionRecord,
    pub state: Value,
}

#[derive(sqlx::FromRow)]
struct MatchRow {
    payload: String,
    state_payload: String,
}

fn storage(_error: impl std::fmt::Display) -> CompactionError {
    // SQL 错误可能包含绑定值；对外及普通诊断只暴露稳定分类。
    CompactionError::Storage
}

fn pending_until(now: i64) -> i64 {
    now.saturating_add(i64::try_from(PENDING_RETENTION.as_millis()).unwrap_or(i64::MAX))
}

fn retention_ms(retention: Duration) -> i64 {
    i64::try_from(retention.as_millis()).unwrap_or(i64::MAX)
}

const SWEEP_BATCH: i64 = 512;
const WATERMARK_PENDING: &str = "native_compactions.pending_until";
const WATERMARK_CONFIRMED: &str = "native_compactions.confirmed_at";

impl Store {
    /// 登记自己的状态与来源边；来源必须存在且仍在同一 Principal 下。
    pub(super) async fn insert(
        &self,
        principal: &Principal,
        record: &CompactionRecord,
        states: &[State],
    ) -> Result<(), CompactionError> {
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let pending_until = pending_until(now);
        let payload = serde_json::to_string(record).map_err(storage)?;
        let state_payloads = states
            .iter()
            .map(|state| serde_json::to_string(&state.payload).map_err(storage))
            .collect::<Result<Vec<_>, _>>()?;
        macro_rules! insert {
            ($pool:expr, $begin:literal, $for_key_share:literal, $lease_lock:literal) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                if let Some(source) = record.source_generation_id.as_deref() {
                    let query = format!(
                        "SELECT CAST(1 AS BIGINT) FROM turn_nodes n \
                         JOIN content_scopes s ON s.id=n.scope_id \
                         WHERE n.id=$1 AND s.principal=$2 AND n.kind='response' \
                         AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting){}",
                        $for_key_share,
                    );
                    let exists: Option<i64> = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                        .bind(source).bind(&principal).fetch_optional(&mut *tx).await.map_err(storage)?;
                    if exists.is_none() { return Err(CompactionError::Unavailable); }
                }
                for predecessor in &record.source_record_ids {
                    let query = format!(
                        "SELECT CAST(1 AS BIGINT) FROM native_compactions \
                         WHERE id=$1 AND principal=$2{}",
                        $for_key_share,
                    );
                    let exists: Option<i64> = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                        .bind(predecessor).bind(&principal).fetch_optional(&mut *tx).await.map_err(storage)?;
                    if exists.is_none() { return Err(CompactionError::Unavailable); }
                }
                sqlx::query("INSERT INTO native_compactions (id, principal, source_generation_id, operation_id, payload, created_at, pending_until) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                    .bind(&record.id).bind(&principal).bind(&record.source_generation_id).bind(&record.operation_id).bind(&payload).bind(now).bind(pending_until)
                    .execute(&mut *tx).await.map_err(storage)?;
                if let Some(checkpoint) = record.atomic_checkpoint_id {
                    // Hold the exact lease and checkpoint rows through reference insertion.
                    let query = format!(
                        "SELECT l.expires_at_ms FROM vendor_atomic_leases l \
                         JOIN vendor_atomic_checkpoints c ON c.principal=l.principal AND c.admission_id=l.admission_id AND c.lease_id=l.lease_id \
                         WHERE c.principal=$1 AND c.checkpoint_id=$2{}",
                        $lease_lock,
                    );
                    let checkpoint = checkpoint.to_string();
                    let expires_at: Option<i64> = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                        .bind(&principal).bind(&checkpoint).fetch_optional(&mut *tx).await.map_err(storage)?;
                    if !expires_at.is_some_and(|expiry| expiry > chrono::Utc::now().timestamp_millis()) {
                        return Err(CompactionError::Unavailable);
                    }
                    let query = format!("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_checkpoints WHERE principal=$1 AND checkpoint_id=$2{}", $for_key_share);
                    let exists: Option<i64> = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                        .bind(&principal).bind(&checkpoint).fetch_optional(&mut *tx).await.map_err(storage)?;
                    if exists.is_none() { return Err(CompactionError::Unavailable); }
                    sqlx::query("INSERT INTO native_compaction_atomic_checkpoints (record_id, principal, checkpoint_id) VALUES ($1, $2, $3)")
                        .bind(&record.id).bind(&principal).bind(&checkpoint).execute(&mut *tx).await.map_err(storage)?;
                }
                for predecessor in &record.source_record_ids {
                    sqlx::query("INSERT INTO native_compaction_sources (record_id, source_id) VALUES ($1, $2)")
                        .bind(&record.id).bind(predecessor).execute(&mut *tx).await.map_err(storage)?;
                }
                for (state, state_payload) in states.iter().zip(&state_payloads) {
                    sqlx::query("INSERT INTO native_compaction_states (record_id, principal, native_identity, fingerprint, state_payload) VALUES ($1, $2, $3, $4, $5)")
                        .bind(&record.id).bind(&principal).bind(&state.identity).bind(&state.fingerprint).bind(state_payload)
                        .execute(&mut *tx).await.map_err(storage)?;
                }
                tx.commit().await.map_err(storage)?;
                connection.finish().await.map_err(storage)?;
                Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool) => insert!(pool, "BEGIN IMMEDIATE", "", ""),
            Self::Postgres(pool) => insert!(pool, "BEGIN", " FOR KEY SHARE", " FOR UPDATE OF l"),
        }
    }

    /// 只按行存在读取；到期但尚未扫描的记录仍可解析。
    pub(super) async fn lookup(
        &self,
        principal: &Principal,
        identity: Option<&str>,
        fingerprint: &str,
    ) -> Result<Vec<Match>, CompactionError> {
        let principal = principal.continuation_key();
        const SQL: &str = "SELECT c.payload, s.state_payload FROM native_compaction_states s
            JOIN native_compactions c ON c.id = s.record_id
            WHERE s.principal = $1 AND c.principal = $1
            AND (s.fingerprint = $2 OR ($3 IS NOT NULL AND s.native_identity = $3)) LIMIT 257";
        macro_rules! fetch {
            ($pool:expr) => {
                sqlx::query_as::<_, MatchRow>(SQL)
                    .bind(&principal)
                    .bind(fingerprint)
                    .bind(identity)
                    .fetch_all($pool)
                    .await
                    .map_err(storage)?
            };
        }
        let rows = match self {
            Self::Sqlite(pool) => fetch!(pool),
            Self::Postgres(pool) => fetch!(pool),
        };
        rows.into_iter()
            .map(|row| {
                Ok(Match {
                    record: serde_json::from_str(&row.payload).map_err(storage)?,
                    state: serde_json::from_str(&row.state_payload).map_err(storage)?,
                })
            })
            .collect()
    }

    pub(super) async fn get(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<CompactionRecord>, CompactionError> {
        let principal = principal.continuation_key();
        const SQL: &str = "SELECT payload FROM native_compactions WHERE id = $1 AND principal = $2";
        macro_rules! fetch {
            ($pool:expr) => {
                sqlx::query_scalar::<_, String>(SQL)
                    .bind(id)
                    .bind(&principal)
                    .fetch_optional($pool)
                    .await
                    .map_err(storage)?
            };
        }
        let payload = match self {
            Self::Sqlite(pool) => fetch!(pool),
            Self::Postgres(pool) => fetch!(pool),
        };
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(storage))
            .transpose()
    }

    /// 客户端回传解析：只确认仍未确认的登记（已确认的不写库）。
    pub(super) async fn confirm_pending(
        &self,
        principal: &Principal,
        ids: &[String],
    ) -> Result<(), CompactionError> {
        let key = principal.continuation_key();
        // 一次读取取回全部登记的确认状态（超长列表按 400 个一批）；缺失的 ID 整体不可用。
        macro_rules! fetch {
            ($pool:expr, $db:ty) => {{
                let mut states: std::collections::HashMap<String, bool> =
                    std::collections::HashMap::with_capacity(ids.len());
                for chunk in ids.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, confirmed_at IS NULL FROM native_compactions WHERE principal = ",
                    );
                    query.push_bind(&key).push(" AND id IN (");
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(id.as_str());
                    }
                    list.push_unseparated(")");
                    let rows: Vec<(String, bool)> =
                        query.build_query_as().fetch_all($pool).await.map_err(storage)?;
                    states.extend(rows);
                }
                states
            }};
        }
        let states = match self {
            Self::Sqlite(pool) => fetch!(pool, sqlx::Sqlite),
            Self::Postgres(pool) => fetch!(pool, sqlx::Postgres),
        };
        let mut pending = Vec::new();
        for id in ids {
            match states.get(id) {
                Some(true) => pending.push(id.clone()),
                Some(false) => {}
                None => return Err(CompactionError::Unavailable),
            }
        }
        self.confirm(principal, &pending).await
    }

    /// 首次交付确认或被客户端回传解析：置 `confirmed_at` 并清空未确认下限。
    pub(super) async fn confirm(
        &self,
        principal: &Principal,
        ids: &[String],
    ) -> Result<(), CompactionError> {
        if ids.is_empty() {
            return Ok(());
        }
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        macro_rules! confirm {
            ($pool:expr, $begin:literal, $lock:expr) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                if !$lock.is_empty() {
                    sqlx::query($lock)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage)?;
                }
                for id in ids {
                    let updated = sqlx::query(
                        "UPDATE native_compactions SET confirmed_at = COALESCE(confirmed_at, $3), \
                         pending_until = NULL WHERE id = $1 AND principal = $2",
                    )
                    .bind(id)
                    .bind(&principal)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?
                    .rows_affected();
                    if updated != 1 {
                        return Err(CompactionError::Unavailable);
                    }
                }
                tx.commit().await.map_err(storage)?;
                connection.finish().await.map_err(storage)?;
                Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool) => confirm!(pool, "BEGIN IMMEDIATE", ""),
            Self::Postgres(pool) => {
                confirm!(pool, "BEGIN", crate::history_retention::PG_GC_LOCK_SHARED)
            }
        }
    }

    /// 期限跨越扫描 + 队列回收：扫描负责入队，回收是唯一删除方。
    pub(super) async fn sweep(
        &self,
        now: i64,
        retention: Duration,
    ) -> Result<u64, CompactionError> {
        let threshold = now.saturating_sub(retention_ms(retention));
        macro_rules! sweep {
            ($pool:expr, $begin:literal, $lock:expr, $skip_locked:literal) => {{
                let mut recovered = 0u64;
                {
                    let mut connection = $pool.acquire_write().await.map_err(storage)?;
                    let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                    if !$lock.is_empty() { sqlx::query($lock).execute(&mut *tx).await.map_err(storage)?; }
                    for (name, column, horizon) in [
                        (WATERMARK_PENDING, "pending_until", now),
                        (WATERMARK_CONFIRMED, "confirmed_at", threshold),
                    ] {
                        let watermark: i64 = sqlx::query_scalar(
                            "SELECT position FROM history_retention_watermarks WHERE name = $1",
                        ).bind(name).fetch_one(&mut *tx).await.map_err(storage)?;
                        if horizon < watermark {
                            sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                                .bind(name).bind(horizon).execute(&mut *tx).await.map_err(storage)?;
                            continue;
                        }
                        let query = format!(
                            "INSERT INTO native_compaction_gc(record_id) \
                             SELECT c.id FROM native_compactions c \
                             WHERE c.{column} > $1 AND c.{column} <= $2 \
                             AND NOT EXISTS(SELECT 1 FROM turn_node_compactions r WHERE r.record_id=c.id) \
                             AND NOT EXISTS(SELECT 1 FROM native_compaction_sources s WHERE s.source_id=c.id) \
                             ON CONFLICT(record_id) DO NOTHING",
                        );
                        sqlx::query(sqlx::AssertSqlSafe(query))
                            .bind(watermark).bind(horizon)
                            .execute(&mut *tx).await.map_err(storage)?;
                        sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                            .bind(name).bind(horizon).execute(&mut *tx).await.map_err(storage)?;
                    }
                    tx.commit().await.map_err(storage)?;
                    connection.finish().await.map_err(storage)?;
                }
                loop {
                    let mut connection = $pool.acquire_write().await.map_err(storage)?;
                    let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                    if !$lock.is_empty() { sqlx::query($lock).execute(&mut *tx).await.map_err(storage)?; }
                    let claim = format!(
                        "SELECT g.record_id FROM native_compaction_gc g \
                         JOIN native_compactions c ON c.id = g.record_id \
                         ORDER BY g.record_id LIMIT {SWEEP_BATCH}{}",
                        $skip_locked,
                    );
                    let claimed: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(claim))
                        .fetch_all(&mut *tx).await.map_err(storage)?;
                    if claimed.is_empty() {
                        tx.commit().await.map_err(storage)?;
                        connection.finish().await.map_err(storage)?;
                        break;
                    }
                    for id in &claimed {
                        let deleted = sqlx::query(
                            "DELETE FROM native_compactions WHERE id = $1 \
                             AND ((confirmed_at IS NULL AND pending_until <= $2) \
                                  OR (confirmed_at IS NOT NULL AND confirmed_at <= $3)) \
                             AND NOT EXISTS(SELECT 1 FROM turn_node_compactions r WHERE r.record_id=native_compactions.id) \
                             AND NOT EXISTS(SELECT 1 FROM native_compaction_sources s WHERE s.source_id=native_compactions.id)",
                        )
                        .bind(id).bind(now).bind(threshold)
                        .execute(&mut *tx).await.map_err(storage)?.rows_affected();
                        if deleted == 0 {
                            sqlx::query("DELETE FROM native_compaction_gc WHERE record_id = $1")
                                .bind(id).execute(&mut *tx).await.map_err(storage)?;
                        } else {
                            // 来源边随记录级联删除，来源记录由释放触发器重新入队。
                            recovered = recovered.saturating_add(deleted);
                        }
                    }
                    tx.commit().await.map_err(storage)?;
                    connection.finish().await.map_err(storage)?;
                }
                Ok(recovered)
            }};
        }
        match self {
            Self::Sqlite(pool) => sweep!(pool, "BEGIN IMMEDIATE", "", ""),
            Self::Postgres(pool) => sweep!(
                pool,
                "BEGIN",
                crate::history_retention::PG_GC_LOCK,
                " FOR UPDATE OF g, c SKIP LOCKED"
            ),
        }
    }
}
