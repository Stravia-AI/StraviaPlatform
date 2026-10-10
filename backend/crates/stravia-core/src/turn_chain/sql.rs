use super::*;
use sqlx::Connection;
use std::sync::Arc;

#[derive(Clone)]
pub enum SqlTurnChainStore {
    Sqlite(SqlitePool, Arc<tokio::sync::Mutex<()>>),
    Postgres(PgPool),
}

impl SqlTurnChainStore {
    /// 同池的 Observation 写入口必须共享 gate；写方法内部取得锁，调用方不得预先持锁。
    pub fn sqlite(pool: SqlitePool, gate: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self::Sqlite(pool, gate)
    }

    pub fn postgres(pool: PgPool) -> Self {
        Self::Postgres(pool)
    }

    async fn remove_unreferenced_contents(&self) -> anyhow::Result<()> {
        const DELETE: &str = "DELETE FROM turn_chain_contents WHERE NOT EXISTS \
            (SELECT 1 FROM turn_chain_node_contents r WHERE r.principal = turn_chain_contents.principal \
             AND r.content_id = turn_chain_contents.id)";
        match self {
            Self::Sqlite(pool, gate) => {
                let _write_gate = gate.lock().await;
                sqlx::query(DELETE).execute(pool).await?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await?;
                // 清理与插入共享内容互斥，避免 MVCC 快照漏看刚建立的引用。
                sqlx::query("LOCK TABLE turn_chain_contents IN SHARE ROW EXCLUSIVE MODE")
                    .execute(&mut *transaction)
                    .await?;
                sqlx::query(DELETE).execute(&mut *transaction).await?;
                transaction.commit().await?;
            }
        }
        Ok(())
    }
}

fn unix_millis_after(ttl: Duration) -> i64 {
    let ttl_millis = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    chrono::Utc::now()
        .timestamp_millis()
        .saturating_add(ttl_millis)
}

fn deadline_from_unix_millis(expires_at: i64, now: i64) -> std::time::Instant {
    let remaining = u64::try_from(expires_at.saturating_sub(now)).unwrap_or(0);
    std::time::Instant::now()
        .checked_add(Duration::from_millis(remaining))
        .unwrap_or_else(std::time::Instant::now)
}

fn decode_node(
    id: TurnNodeId,
    kind: TurnNodeKind,
    parent_id: Option<String>,
    payload_version: i64,
    payload: Vec<u8>,
) -> Result<TurnNode, TurnUnavailable> {
    let payload_version = u32::try_from(payload_version)
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
    let bytes = crate::storage_codec::decode(&payload)
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
    let payload = serde_json::from_slice(&bytes)
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
    Ok(TurnNode {
        id,
        kind,
        parent_id: parent_id.map(TurnNodeId::new),
        payload_version,
        payload,
    })
}

#[async_trait]
impl TurnChainStore for SqlTurnChainStore {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<Vec<TurnNode>, TurnUnavailable> {
        Ok(self
            .materialize_with_expiry(principal, kind, id)
            .await?
            .nodes)
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "turn_chain.materialize",
        skip_all,
        fields(node_count)
    )]
    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<MaterializedTurnChain, TurnUnavailable> {
        use tracing::Instrument as _;
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let rows: Vec<(String, Option<String>, i64, Vec<u8>, i64, i64)> = async {
            match self {
            Self::Sqlite(pool, _) => {
                // CROSS JOIN 固定 ancestors 为外层：否则无统计信息的 SQLite 会按
                // (principal, kind) 扫描该主体全部节点，耗时随链深 × 节点数增长。
                sqlx::query_as(
                    "WITH RECURSIVE ancestors(id, parent_id, payload_version, payload, expires_at, depth) AS (\
                     SELECT id, parent_id, payload_version, payload, expires_at, 0 \
                     FROM turn_chain_nodes \
                     WHERE id = ? AND principal = ? AND kind = ? \
                     UNION ALL \
                     SELECT node.id, node.parent_id, node.payload_version, node.payload, node.expires_at, ancestors.depth + 1 \
                     FROM ancestors CROSS JOIN turn_chain_nodes node \
                     WHERE node.id = ancestors.parent_id AND node.principal = ? AND node.kind = ?\
                     ) \
                     SELECT id, parent_id, payload_version, payload, expires_at, depth \
                     FROM ancestors ORDER BY depth DESC",
                )
                .bind(id.as_str())
                .bind(&principal)
                .bind(kind.as_str())
                .bind(&principal)
                .bind(kind.as_str())
                .fetch_all(pool)
                .await
            }
            Self::Postgres(pool) => {
                sqlx::query_as(
                    "WITH RECURSIVE ancestors(id, parent_id, payload_version, payload, expires_at, depth) AS (\
                     SELECT id, parent_id, payload_version, payload, expires_at, 0::BIGINT \
                     FROM turn_chain_nodes \
                     WHERE id = $1 AND principal = $2 AND kind = $3 \
                     UNION ALL \
                     SELECT node.id, node.parent_id, node.payload_version, node.payload, node.expires_at, ancestors.depth + 1 \
                     FROM turn_chain_nodes node \
                     JOIN ancestors ON node.id = ancestors.parent_id \
                     WHERE node.principal = $2 AND node.kind = $3\
                     ) \
                     SELECT id, parent_id, payload_version, payload, expires_at, depth \
                     FROM ancestors ORDER BY depth DESC",
                )
                .bind(id.as_str())
                .bind(&principal)
                .bind(kind.as_str())
                .fetch_all(pool)
                .await
            }
            }
        }
        .instrument(tracing::info_span!(
            target: "stravia::perf",
            "turn_chain.ancestor.select"
        ))
        .await
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
        tracing::Span::current().record("node_count", rows.len() as u64);
        if rows.is_empty()
            || rows
                .first()
                .is_some_and(|(_, parent_id, _, _, _, _)| parent_id.is_some())
            || rows
                .iter()
                .any(|(_, _, _, _, expires_at, _)| *expires_at <= now)
        {
            return Err(TurnUnavailable::Unavailable);
        }
        let expires_at = rows
            .iter()
            .map(|(_, _, _, _, expires_at, _)| deadline_from_unix_millis(*expires_at, now))
            .min()
            .ok_or(TurnUnavailable::Unavailable)?;
        let mut nodes = rows
            .into_iter()
            .map(|(id, parent_id, payload_version, payload, _, _)| {
                decode_node(
                    TurnNodeId::new(id),
                    kind,
                    parent_id,
                    payload_version,
                    payload,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        match self {
            Self::Sqlite(pool, _) => {
                content::restore_sqlite(
                    &mut *pool
                        .acquire()
                        .await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?,
                    &principal,
                    &mut nodes,
                )
                .await
            }
            Self::Postgres(pool) => {
                content::restore_postgres(
                    &mut *pool
                        .acquire()
                        .await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?,
                    &principal,
                    &mut nodes,
                )
                .await
            }
        }
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
        Ok(MaterializedTurnChain { nodes, expires_at })
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "turn_chain.commit",
        skip_all,
        fields(reference_count)
    )]
    async fn commit(&self, commit: TurnCommit) -> Result<TurnNodeId, TurnCommitError> {
        let principal = commit.principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let expires_at = unix_millis_after(commit.idle_ttl);
        let mut encoded = tokio::task::spawn_blocking(move || content::encode(commit.payload))
            .await
            .map_err(|error| TurnCommitError::Storage(error.to_string()))?
            .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
        tracing::Span::current().record("reference_count", encoded.contents.len() as u64);
        if !encoded.contents.is_empty() {
            // Read only key/id metadata before acquiring the write gate or
            // transaction. Release the connection before CPU preparation.
            let ids = match self {
                Self::Sqlite(pool, _) => {
                    let mut connection = pool
                        .acquire()
                        .await
                        .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                    content::lookup_sqlite(&mut connection, &principal, &encoded, false).await
                }
                Self::Postgres(pool) => {
                    let mut connection = pool
                        .acquire()
                        .await
                        .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                    content::lookup_postgres(&mut connection, &principal, &encoded, false).await
                }
            }
            .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
            content::prepare_missing(&mut encoded, &ids)
                .await
                .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
        }
        let payload = &encoded.payload;
        let payload_version = i64::from(commit.payload_version);
        let prefix_namespace = commit
            .reusable_prefix
            .as_ref()
            .map(|prefix| prefix.namespace.as_str());
        let prefix_fingerprint = commit
            .reusable_prefix
            .as_ref()
            .map(|prefix| prefix.fingerprint.as_str());
        let prefix_item_count = commit
            .reusable_prefix
            .as_ref()
            .map(|prefix| i64::from(prefix.item_count));
        let prefix_completed_at = commit
            .reusable_prefix
            .as_ref()
            .map(|prefix| prefix.completed_at);

        match self {
            Self::Sqlite(pool, gate) => {
                // 先与 Observation 协调，再拿连接和写事务，避免进入 SQLite busy handler 竞争。
                let _write_gate = gate.lock().await;
                let mut connection = pool
                    .acquire()
                    .await
                    .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                let mut transaction = connection
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                let duplicate: Option<(i64,)> =
                    sqlx::query_as("SELECT 1 FROM turn_chain_nodes WHERE id = ?")
                        .bind(commit.id.as_str())
                        .fetch_optional(&mut *transaction)
                        .await
                        .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                if duplicate.is_some() {
                    return Err(TurnCommitError::AlreadyExists);
                }
                if let Some(parent_id) = commit.parent_id.as_ref() {
                    // 该语句在写锁内执行；CROSS JOIN 的原因见 materialize_with_expiry。
                    let updated = sqlx::query(
                        "WITH RECURSIVE ancestors(id, parent_id, depth) AS (\
                         SELECT id, parent_id, 0 FROM turn_chain_nodes \
                         WHERE id = ? AND principal = ? AND kind = ? AND expires_at > ? \
                         UNION ALL \
                         SELECT node.id, node.parent_id, ancestors.depth + 1 \
                         FROM ancestors CROSS JOIN turn_chain_nodes node \
                         WHERE node.id = ancestors.parent_id AND node.principal = ? AND node.kind = ?\
                         ) \
                         UPDATE turn_chain_nodes \
                         SET expires_at = CASE WHEN expires_at < ? THEN ? ELSE expires_at END \
                         WHERE id IN (SELECT id FROM ancestors) \
                         AND (SELECT parent_id FROM ancestors ORDER BY depth DESC LIMIT 1) IS NULL",
                    )
                    .bind(parent_id.as_str())
                    .bind(&principal)
                    .bind(commit.kind.as_str())
                    .bind(now)
                    .bind(&principal)
                    .bind(commit.kind.as_str())
                    .bind(expires_at)
                    .bind(expires_at)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| TurnCommitError::Storage(error.to_string()))?
                    .rows_affected();
                    if updated == 0 {
                        return Err(TurnCommitError::ParentUnavailable);
                    }
                }
                sqlx::query(
                    "INSERT INTO turn_chain_nodes \
                     (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at, \
                      prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at, \
                      storage_format) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 2)",
                )
                .bind(commit.id.as_str())
                .bind(commit.kind.as_str())
                .bind(commit.parent_id.as_ref().map(TurnNodeId::as_str))
                .bind(&principal)
                .bind(payload_version)
                .bind(payload)
                .bind(now)
                .bind(expires_at)
                .bind(prefix_namespace)
                .bind(prefix_fingerprint)
                .bind(prefix_item_count)
                .bind(prefix_completed_at)
                .execute(&mut *transaction)
                .await
                .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                content::put_sqlite(
                    &mut transaction,
                    commit.id.as_str(),
                    &principal,
                    &mut encoded,
                )
                .await
                .map_err(|error| TurnCommitError::Storage(error.to_string()))?;

                transaction
                    .commit()
                    .await
                    .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool
                    .begin()
                    .await
                    .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                if !encoded.contents.is_empty() {
                    // Nodes without externalized contents cannot lose a GC
                    // race, so they skip the contents table lock entirely.
                    sqlx::query("LOCK TABLE turn_chain_contents IN ROW EXCLUSIVE MODE")
                        .execute(&mut *transaction)
                        .await
                        .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                }
                let duplicate: Option<(i64,)> =
                    sqlx::query_as("SELECT 1::BIGINT FROM turn_chain_nodes WHERE id = $1")
                        .bind(commit.id.as_str())
                        .fetch_optional(&mut *transaction)
                        .await
                        .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                if duplicate.is_some() {
                    return Err(TurnCommitError::AlreadyExists);
                }
                if let Some(parent_id) = commit.parent_id.as_ref() {
                    let updated = sqlx::query(
                        "WITH RECURSIVE ancestors(id, parent_id, depth) AS (\
                         SELECT id, parent_id, 0::BIGINT FROM turn_chain_nodes \
                         WHERE id = $1 AND principal = $2 AND kind = $3 AND expires_at > $4 \
                         UNION ALL \
                         SELECT node.id, node.parent_id, ancestors.depth + 1 FROM turn_chain_nodes node \
                         JOIN ancestors ON node.id = ancestors.parent_id \
                         WHERE node.principal = $2 AND node.kind = $3\
                         ) \
                         UPDATE turn_chain_nodes \
                         SET expires_at = GREATEST(expires_at, $5) \
                         WHERE id IN (SELECT id FROM ancestors) \
                         AND (SELECT parent_id FROM ancestors ORDER BY depth DESC LIMIT 1) IS NULL",
                    )
                    .bind(parent_id.as_str())
                    .bind(&principal)
                    .bind(commit.kind.as_str())
                    .bind(now)
                    .bind(expires_at)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| TurnCommitError::Storage(error.to_string()))?
                    .rows_affected();
                    if updated == 0 {
                        return Err(TurnCommitError::ParentUnavailable);
                    }
                }
                sqlx::query(
                    "INSERT INTO turn_chain_nodes \
                     (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at, \
                      prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at, \
                      storage_format) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 2)",
                )
                .bind(commit.id.as_str())
                .bind(commit.kind.as_str())
                .bind(commit.parent_id.as_ref().map(TurnNodeId::as_str))
                .bind(&principal)
                .bind(payload_version)
                .bind(payload)
                .bind(now)
                .bind(expires_at)
                .bind(prefix_namespace)
                .bind(prefix_fingerprint)
                .bind(prefix_item_count)
                .bind(prefix_completed_at)
                .execute(&mut *transaction)
                .await
                .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
                content::put_postgres(
                    &mut transaction,
                    commit.id.as_str(),
                    &principal,
                    &mut encoded,
                )
                .await
                .map_err(|error| TurnCommitError::Storage(error.to_string()))?;

                transaction
                    .commit()
                    .await
                    .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
            }
        }
        Ok(commit.id)
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "turn_chain.prefix.lookup",
        skip_all,
        fields(candidate_count = 0_u64)
    )]
    async fn find_reusable_prefixes(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        query: &ReusablePrefixQuery,
    ) -> Result<Vec<ReusablePrefixCandidate>, TurnUnavailable> {
        if query.fingerprints.is_empty() {
            return Ok(Vec::new());
        }
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let rows: Vec<(String, i64, i64)> = match self {
            Self::Sqlite(pool, _) => {
                let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                    "SELECT id, prefix_item_count, prefix_completed_at FROM turn_chain_nodes \
                     WHERE principal = ",
                );
                builder
                    .push_bind(&principal)
                    .push(" AND kind = ")
                    .push_bind(kind.as_str())
                    .push(" AND prefix_namespace = ")
                    .push_bind(&query.namespace)
                    .push(" AND expires_at > ")
                    .push_bind(now)
                    .push(" AND (prefix_fingerprint, prefix_item_count) IN (VALUES ");
                for (index, (fingerprint, item_count)) in query.fingerprints.iter().enumerate() {
                    if index > 0 {
                        builder.push(", ");
                    }
                    builder
                        .push("(")
                        .push_bind(fingerprint)
                        .push(", ")
                        .push_bind(i64::from(*item_count))
                        .push(")");
                }
                builder
                    .push(") ORDER BY prefix_item_count DESC, prefix_completed_at DESC, id DESC");
                builder.build_query_as().fetch_all(pool).await
            }
            Self::Postgres(pool) => {
                let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                    "SELECT id, prefix_item_count, prefix_completed_at FROM turn_chain_nodes \
                     WHERE principal = ",
                );
                builder
                    .push_bind(&principal)
                    .push(" AND kind = ")
                    .push_bind(kind.as_str())
                    .push(" AND prefix_namespace = ")
                    .push_bind(&query.namespace)
                    .push(" AND expires_at > ")
                    .push_bind(now)
                    .push(" AND (prefix_fingerprint, prefix_item_count) IN (");
                for (index, (fingerprint, item_count)) in query.fingerprints.iter().enumerate() {
                    if index > 0 {
                        builder.push(", ");
                    }
                    builder
                        .push("(")
                        .push_bind(fingerprint)
                        .push(", ")
                        .push_bind(i64::from(*item_count))
                        .push(")");
                }
                builder
                    .push(") ORDER BY prefix_item_count DESC, prefix_completed_at DESC, id DESC");
                builder.build_query_as().fetch_all(pool).await
            }
        }
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
        tracing::Span::current().record("candidate_count", rows.len() as u64);
        rows.into_iter()
            .map(|(node_id, item_count, completed_at)| {
                Ok(ReusablePrefixCandidate {
                    node_id: TurnNodeId::new(node_id),
                    item_count: u32::try_from(item_count)
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?,
                    completed_at,
                })
            })
            .collect()
    }

    /// Upgrade derived prefix indexes before accepting requests. The namespace
    /// prefix is the projection version: current indexes require no history
    /// reads. Each transaction preserves payloads, parent edges, expiry and
    /// completion times.
    async fn rebuild_prefixes(
        &self,
        namespace_prefix: &str,
        decode: &(
             dyn Fn(Vec<TurnNode>, i64) -> Result<Option<ReusablePrefixMetadata>, String>
                 + Send
                 + Sync
         ),
    ) -> Result<(), TurnUnavailable> {
        macro_rules! rebuild {
            ($pool:expr, $restore:ident $(, $gate:expr)?) => {{
                // SQLite: 持共享池写闸门跨整个事务，直到 commit 完成。
                $(let _write_gate = $gate.lock().await;)?
                let mut transaction = $pool.begin().await
                    .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                let now = chrono::Utc::now().timestamp_millis();
                let stale = format!("{namespace_prefix}%");
                let heads: Vec<(String, String, i64)> = sqlx::query_as(
                    "SELECT id, principal, prefix_completed_at FROM turn_chain_nodes \
                     WHERE kind = 'response' AND prefix_namespace IS NOT NULL \
                     AND prefix_namespace NOT LIKE $1 AND expires_at > $2"
                ).bind(&stale).bind(now).fetch_all(&mut *transaction).await
                    .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                for (id, principal, completed_at) in heads {
                    let rows: Vec<(String, Option<String>, i64, Vec<u8>, i64)> = sqlx::query_as(
                        "WITH RECURSIVE ancestors(id, parent_id, payload_version, payload, expires_at, depth) AS (\
                         SELECT id, parent_id, payload_version, payload, expires_at, 0 FROM turn_chain_nodes \
                         WHERE id = $1 AND principal = $2 AND kind = 'response' \
                         UNION ALL SELECT node.id, node.parent_id, node.payload_version, node.payload, node.expires_at, ancestors.depth + 1 \
                         FROM ancestors CROSS JOIN turn_chain_nodes node \
                         WHERE node.id = ancestors.parent_id AND node.principal = $2 AND node.kind = 'response') \
                         SELECT id, parent_id, payload_version, payload, expires_at FROM ancestors ORDER BY depth DESC"
                    ).bind(&id).bind(&principal).fetch_all(&mut *transaction).await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                    if rows.first().is_none_or(|row| row.1.is_some()) || rows.iter().any(|row| row.4 <= now) {
                        sqlx::query("UPDATE turn_chain_nodes SET prefix_namespace = NULL, prefix_fingerprint = NULL, prefix_item_count = NULL, prefix_completed_at = NULL WHERE id = $1 AND principal = $2")
                            .bind(&id).bind(&principal).execute(&mut *transaction).await
                            .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                        continue;
                    }
                    let mut nodes = rows.into_iter().map(|(id, parent_id, version, payload, _)| {
                        decode_node(TurnNodeId::new(id), TurnNodeKind::Response, parent_id, version, payload)
                    }).collect::<Result<Vec<_>, _>>()?;
                    content::$restore(&mut transaction, &principal, &mut nodes).await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                    let Some(prefix) = decode(nodes, completed_at)
                        .map_err(TurnUnavailable::Storage)? else { continue };
                    sqlx::query("UPDATE turn_chain_nodes SET prefix_namespace = $1, prefix_fingerprint = $2, prefix_item_count = $3 WHERE id = $4 AND principal = $5")
                        .bind(prefix.namespace).bind(prefix.fingerprint).bind(i64::from(prefix.item_count)).bind(id).bind(principal)
                        .execute(&mut *transaction).await.map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                }
                transaction.commit().await.map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            }};
        }
        match self {
            Self::Sqlite(pool, gate) => rebuild!(pool, restore_sqlite, gate),
            Self::Postgres(pool) => rebuild!(pool, restore_postgres),
        }
        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut removed = 0_u64;
        loop {
            let rows = match self {
                Self::Sqlite(pool, gate) => {
                    let _write_gate = gate.lock().await;
                    sqlx::query(
                        "DELETE FROM turn_chain_nodes WHERE expires_at <= ? \
                         AND NOT EXISTS (SELECT 1 FROM turn_chain_nodes child \
                         WHERE child.parent_id = turn_chain_nodes.id)",
                    )
                    .bind(now)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
                }
                Self::Postgres(pool) => sqlx::query(
                    "DELETE FROM turn_chain_nodes node WHERE expires_at <= $1 \
                     AND NOT EXISTS (SELECT 1 FROM turn_chain_nodes child \
                     WHERE child.parent_id = node.id)",
                )
                .bind(now)
                .execute(pool)
                .await
                .map(|result| result.rows_affected()),
            }
            .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            removed = removed.saturating_add(rows);
            if rows == 0 {
                self.remove_unreferenced_contents()
                    .await
                    .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                return Ok(removed);
            }
        }
    }
}

#[cfg(test)]
impl SqlTurnChainStore {
    /// Test-only rewrite hook: restores every stored node payload, applies
    /// `edit`, then re-encodes it back into format 2 storage in one transaction
    /// per node. Lets fixture-style tests simulate pre-existing payload shapes
    /// without reaching into the binary envelope or the contents tables.
    pub(crate) async fn rewrite_payloads(
        &self,
        mut edit: impl FnMut(&mut serde_json::Value),
    ) -> anyhow::Result<()> {
        macro_rules! rewrite {
            ($pool:expr, $put:ident, $restore:ident, $lock:literal $(, $gate:expr)?) => {{
                let rows: Vec<(String, String, Vec<u8>)> =
                    sqlx::query_as("SELECT id, principal, payload FROM turn_chain_nodes")
                        .fetch_all($pool)
                        .await?;
                for (id, principal, payload) in rows {
                    let envelope: serde_json::Value =
                        serde_json::from_slice(&crate::storage_codec::decode(&payload)?)?;
                    let mut nodes = vec![TurnNode {
                        id: TurnNodeId::new(id.clone()),
                        kind: TurnNodeKind::Response,
                        parent_id: None,
                        payload_version: 1,
                        payload: envelope,
                    }];
                    {
                        let mut connection = $pool.acquire().await?;
                        content::$restore(&mut connection, &principal, &mut nodes).await?;
                    }
                    let mut payload = nodes.into_iter().next().expect("restored node").payload;
                    edit(&mut payload);
                    let mut encoded = tokio::task::spawn_blocking(move || content::encode(payload))
                        .await
                        .map_err(|error| anyhow::anyhow!("history rewrite preparation worker failed: {error}"))??;
                    $(let _write_gate = $gate.lock().await;)?
                    let mut transaction = $pool.begin().await?;
                    if !encoded.contents.is_empty() {
                        sqlx::query($lock).execute(&mut *transaction).await?;
                    }
                    sqlx::query("DELETE FROM turn_chain_node_contents WHERE node_id = $1")
                        .bind(&id)
                        .execute(&mut *transaction)
                        .await?;
                    content::$put(&mut transaction, &id, &principal, &mut encoded).await?;
                    sqlx::query("UPDATE turn_chain_nodes SET payload = $1 WHERE id = $2")
                        .bind(encoded.payload.as_slice())
                        .bind(&id)
                        .execute(&mut *transaction)
                        .await?;
                    transaction.commit().await?;
                }
                anyhow::Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool, gate) => {
                rewrite!(pool, put_sqlite, restore_sqlite, "SELECT 1", gate)
            }
            Self::Postgres(pool) => rewrite!(
                pool,
                put_postgres,
                restore_postgres,
                "LOCK TABLE turn_chain_contents IN ROW EXCLUSIVE MODE"
            ),
        }
    }
}
