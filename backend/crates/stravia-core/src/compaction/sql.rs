use super::*;
use sqlx::Connection;

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

fn expires_after(now: i64, ttl: Duration) -> i64 {
    now.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX))
}

// 两种 SQL adapter 共享同一事务算法；$N 参数形式也由 SQLite 原生支持。
macro_rules! extend_source {
    ($tx:ident, $principal:expr, $source:expr, $now:expr, $expires:expr) => {{
        if let Some(source) = $source {
            let available: Option<i64> = sqlx::query_scalar(
                "SELECT CAST(1 AS BIGINT) FROM turn_chain_nodes WHERE id = $1 AND principal = $2 AND kind = 'response' AND expires_at > $3",
            ).bind(source).bind($principal).bind($now).fetch_optional(&mut *$tx).await.map_err(storage)?;
            if available.is_none() { return Err(CompactionError::Unavailable); }
            sqlx::query(
                "WITH RECURSIVE ancestors(id, parent_id) AS (
                    SELECT id, parent_id FROM turn_chain_nodes WHERE id = $1 AND principal = $2
                    UNION SELECT n.id, n.parent_id FROM turn_chain_nodes n JOIN ancestors a ON n.id = a.parent_id WHERE n.principal = $2
                 ) UPDATE turn_chain_nodes SET expires_at = CASE WHEN expires_at < $3 THEN $3 ELSE expires_at END
                 WHERE id IN (SELECT id FROM ancestors) AND principal = $2",
            ).bind(source).bind($principal).bind($expires).execute(&mut *$tx).await.map_err(storage)?;
        }
    }};
}

impl Store {
    pub(super) async fn insert(
        &self,
        principal: &Principal,
        record: &CompactionRecord,
        states: &[State],
        ttl: Duration,
    ) -> Result<(), CompactionError> {
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let expires = expires_after(now, ttl);
        let payload = serde_json::to_string(record).map_err(storage)?;
        macro_rules! insert {
            ($pool:expr, $begin:literal) => {{
                let mut connection = $pool.acquire().await.map_err(storage)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                extend_source!(tx, &principal, record.source_generation_id.as_deref(), now, expires);
                for predecessor in &record.source_record_ids {
                    let exists: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM native_compactions WHERE id = $1 AND principal = $2 AND expires_at > $3")
                        .bind(predecessor).bind(&principal).bind(now).fetch_optional(&mut *tx).await.map_err(storage)?;
                    if exists.is_none() { return Err(CompactionError::Unavailable); }
                }
                sqlx::query("INSERT INTO native_compactions (id, principal, source_generation_id, operation_id, payload, created_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                    .bind(&record.id).bind(&principal).bind(&record.source_generation_id).bind(&record.operation_id).bind(&payload).bind(now).bind(expires)
                    .execute(&mut *tx).await.map_err(storage)?;
                for predecessor in &record.source_record_ids {
                    sqlx::query("INSERT INTO native_compaction_sources (record_id, source_id) VALUES ($1, $2)")
                        .bind(&record.id).bind(predecessor).execute(&mut *tx).await.map_err(storage)?;
                }
                for state in states {
                    let state_payload = serde_json::to_string(&state.payload).map_err(storage)?;
                    sqlx::query("INSERT INTO native_compaction_states (record_id, principal, native_identity, fingerprint, state_payload) VALUES ($1, $2, $3, $4, $5)")
                        .bind(&record.id).bind(&principal).bind(&state.identity).bind(&state.fingerprint).bind(state_payload)
                        .execute(&mut *tx).await.map_err(storage)?;
                }
                tx.commit().await.map_err(storage)?;
                Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool) => insert!(pool, "BEGIN IMMEDIATE"),
            Self::Postgres(pool) => insert!(pool, "BEGIN"),
        }
    }

    pub(super) async fn lookup(
        &self,
        principal: &Principal,
        identity: Option<&str>,
        fingerprint: &str,
    ) -> Result<Vec<Match>, CompactionError> {
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        const SQL: &str = "SELECT c.payload, s.state_payload FROM native_compaction_states s
            JOIN native_compactions c ON c.id = s.record_id
            WHERE s.principal = $1 AND c.principal = $1 AND c.expires_at > $2
            AND (s.fingerprint = $3 OR ($4 IS NOT NULL AND s.native_identity = $4)) LIMIT 257";
        macro_rules! fetch {
            ($pool:expr) => {
                sqlx::query_as::<_, MatchRow>(SQL)
                    .bind(&principal)
                    .bind(now)
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
        let now = chrono::Utc::now().timestamp_millis();
        const SQL: &str = "SELECT payload FROM native_compactions WHERE id = $1 AND principal = $2 AND expires_at > $3";
        macro_rules! fetch {
            ($pool:expr) => {
                sqlx::query_scalar::<_, String>(SQL)
                    .bind(id)
                    .bind(&principal)
                    .bind(now)
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

    pub(super) async fn extend(
        &self,
        principal: &Principal,
        ids: &[String],
        ttl: Duration,
        delivered: bool,
    ) -> Result<(), CompactionError> {
        if ids.is_empty() {
            return Ok(());
        }
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let expires = expires_after(now, ttl);
        macro_rules! extend {
            ($pool:expr, $begin:literal) => {{
                // 先取得 SQLite 写锁，避免交付确认与立即回传的快照升级竞争。
                let mut connection = $pool.acquire().await.map_err(storage)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                let mut queue = ids.to_vec();
                let mut visited = HashSet::new();
                while let Some(id) = queue.pop() {
                    if !visited.insert(id.clone()) { continue; }
                    if visited.len() > MAX_RESOLUTION_RECORDS { return Err(CompactionError::Conflict); }
                    let payload: Option<String> = sqlx::query_scalar("SELECT payload FROM native_compactions WHERE id = $1 AND principal = $2 AND expires_at > $3")
                        .bind(&id).bind(&principal).bind(now).fetch_optional(&mut *tx).await.map_err(storage)?;
                    let record: CompactionRecord = serde_json::from_str(&payload.ok_or(CompactionError::Unavailable)?).map_err(storage)?;
                    extend_source!(tx, &principal, record.source_generation_id.as_deref(), now, expires);
                    let referenced_at = (!delivered || !ids.contains(&id)).then_some(now);
                    sqlx::query("UPDATE native_compactions SET expires_at = CASE WHEN expires_at < $3 THEN $3 ELSE expires_at END, referenced_at = COALESCE(referenced_at, $4) WHERE id = $1 AND principal = $2")
                        .bind(&id).bind(&principal).bind(expires).bind(referenced_at).execute(&mut *tx).await.map_err(storage)?;
                    if delivered && ids.contains(&id) {
                        sqlx::query("UPDATE native_compactions SET delivered_at = COALESCE(delivered_at, $3) WHERE id = $1 AND principal = $2")
                            .bind(&id).bind(&principal).bind(now).execute(&mut *tx).await.map_err(storage)?;
                    }
                    queue.extend(record.source_record_ids);
                }
                tx.commit().await.map_err(storage)?;
                Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool) => extend!(pool, "BEGIN IMMEDIATE"),
            Self::Postgres(pool) => extend!(pool, "BEGIN"),
        }
    }

    pub(super) async fn cleanup(&self) -> Result<(), CompactionError> {
        let now = chrono::Utc::now().timestamp_millis();
        macro_rules! cleanup {
            ($pool:expr, $begin:literal) => {{
                let mut connection = $pool.acquire().await.map_err(storage)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage)?;
                // 先删除到期边，再删记录；仍存活的后继阻止来源被回收。
                sqlx::query("DELETE FROM native_compaction_sources WHERE record_id IN (SELECT id FROM native_compactions WHERE expires_at <= $1)")
                    .bind(now).execute(&mut *tx).await.map_err(storage)?;
                sqlx::query("DELETE FROM native_compactions WHERE expires_at <= $1 AND NOT EXISTS (SELECT 1 FROM native_compaction_sources s WHERE s.source_id = native_compactions.id)")
                    .bind(now).execute(&mut *tx).await.map_err(storage)?;
                tx.commit().await.map_err(storage)?;
                Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool) => cleanup!(pool, "BEGIN IMMEDIATE"),
            Self::Postgres(pool) => cleanup!(pool, "BEGIN"),
        }
    }
}
