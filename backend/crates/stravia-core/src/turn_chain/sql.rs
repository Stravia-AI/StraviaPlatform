use super::*;

use futures::TryStreamExt;

#[derive(Clone)]
pub enum SqlTurnChainStore {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

impl SqlTurnChainStore {
    pub fn sqlite(pool: SqlitePool) -> Self {
        Self::Sqlite(pool)
    }

    pub fn postgres(pool: PgPool) -> Self {
        Self::Postgres(pool)
    }

    /// Complete the SQL suffix against a proof checked after the query await.
    /// An expired accelerator is not evidence that the durable ancestor vanished.
    pub(super) async fn finish_context_read(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        context: &mut TurnReadContext,
        rows: Vec<StoredAncestorRow>,
        completed_at: std::time::Instant,
    ) -> Result<SharedMaterializedTurnChain, TurnUnavailable> {
        let now = chrono::Utc::now().timestamp_millis();
        if rows.is_empty() || rows.iter().any(|row| row.4 <= now) {
            return Err(TurnUnavailable::Unavailable);
        }
        let mut prefix = match rows[0].1.as_ref() {
            Some(parent) => match context.get_at(&TurnNodeId::new(parent), completed_at) {
                Some(prefix) => prefix,
                None => {
                    // The query stopped at a proof that expired during its await.
                    // Re-read the full fact chain once, including current ownership
                    // and TTL. Missing, expired and corrupt facts still fail normally.
                    context.begin_read();
                    let head = TurnNodeId::new(&rows.last().expect("nonempty SQL suffix").0);
                    drop(rows);
                    let chain = self.materialize_with_expiry(principal, kind, &head).await?;
                    return Ok(SharedMaterializedTurnChain {
                        expires_at: chain.expires_at,
                        nodes: chain
                            .nodes
                            .into_iter()
                            .map(|node| context.prepare_node(node, chain.expires_at))
                            .collect::<Result<_, _>>()?,
                    });
                }
            },
            None => SharedMaterializedTurnChain {
                nodes: Vec::new(),
                expires_at: deadline_from_unix_millis(rows[0].4, now),
            },
        };
        let principal_key = principal.continuation_key();
        let deadlines: Vec<_> = rows
            .iter()
            .map(|row| deadline_from_unix_millis(row.4, now))
            .collect();
        let nodes = rows
            .into_iter()
            .map(|(id, parent, version, payload, _, _)| {
                decode_node(TurnNodeId::new(id), kind, parent, version, payload)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut deadlines = deadlines.into_iter();
        let finish = |node, context: Option<&TurnReadContext>| {
            context
                .expect("history read context")
                .prepare_node(node, deadlines.next().expect("history node deadline"))
                .map_err(anyhow::Error::new)
        };
        let restored = match self {
            Self::Sqlite(pool) => {
                content::restore_sqlite_context(
                    &mut *pool
                        .acquire()
                        .await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?,
                    &principal_key,
                    nodes,
                    Some(context),
                    finish,
                )
                .await
            }
            Self::Postgres(pool) => {
                content::restore_postgres_context(
                    &mut *pool
                        .acquire()
                        .await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?,
                    &principal_key,
                    nodes,
                    Some(context),
                    finish,
                )
                .await
            }
        };
        let nodes = restored.map_err(|error| {
            context.begin_read();
            error
                .downcast::<TurnUnavailable>()
                .unwrap_or_else(|error| TurnUnavailable::Storage(error.to_string()))
        })?;
        for node in nodes {
            prefix.expires_at = prefix.expires_at.min(node.expires_at);
            prefix.nodes.push(node);
        }
        Ok(prefix)
    }

    pub(super) async fn remove_unreferenced_contents(&self) -> anyhow::Result<()> {
        const ORPHAN: &str = "NOT EXISTS (SELECT 1 FROM turn_chain_node_contents r \
        WHERE r.principal = turn_chain_contents.principal \
        AND r.content_id = turn_chain_contents.id)";
        // 每批最多检查这么多内容行；活内容也计入预算，不能按找到的孤儿数截断。
        const BATCH: usize = 400;
        match self {
            Self::Sqlite(pool) => {
                let mut cursor: Option<(String, i64)> = None;
                loop {
                    // 候选扫描不持写锁。先沿当前 Principal 的 (principal, id)
                    // 覆盖索引继续推进，再跨到更大的 Principal；直接使用
                    // (principal, id) > (?, ?) 的排序游标会让同一 Principal
                    // 每批从头扫描，批数一多就退化为平方级。
                    // NOT EXISTS 只判定本批候选，LIMIT 始终约束检查过的内容行。
                    let mut candidates: Vec<(String, i64, i64)> = Vec::with_capacity(BATCH);
                    if let Some((principal, id)) = &cursor {
                        let mut query =
                            sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT principal, id, ");
                        query
                            .push(ORPHAN)
                            .push(" AS orphan FROM turn_chain_contents WHERE principal = ")
                            .push_bind(principal)
                            .push(" AND id > ")
                            .push_bind(id)
                            .push(" ORDER BY id LIMIT ")
                            .push_bind(BATCH as i64);
                        let read = query.build_query_as::<(String, i64, i64)>();
                        let mut rows = read.fetch(pool);
                        while let Some(row) = rows.try_next().await? {
                            candidates.push(row);
                        }
                    }
                    if candidates.len() < BATCH {
                        let mut query =
                            sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT principal, id, ");
                        query
                            .push(ORPHAN)
                            .push(" AS orphan FROM turn_chain_contents");
                        if let Some((principal, _)) = &cursor {
                            query.push(" WHERE principal > ").push_bind(principal);
                        }
                        query
                            .push(" ORDER BY principal, id LIMIT ")
                            .push_bind((BATCH - candidates.len()) as i64);
                        let read = query.build_query_as::<(String, i64, i64)>();
                        let mut rows = read.fetch(pool);
                        while let Some(row) = rows.try_next().await? {
                            candidates.push(row);
                        }
                    }
                    if candidates.is_empty() {
                        break;
                    }
                    // 只有真正读到表尾才结束；中间批即使零孤儿也继续推进游标。
                    let exhausted = candidates.len() < BATCH;
                    if let Some(first_orphan) = candidates.iter().position(|row| row.2 != 0) {
                        // 判定后可能已有新引用，必须在同一条 DELETE 中重新检查。
                        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                            "DELETE FROM turn_chain_contents WHERE ",
                        );
                        query.push(ORPHAN).push(" AND id IN (");
                        let mut ids = query.separated(",");
                        for (_, id, orphan) in &candidates[first_orphan..] {
                            if *orphan != 0 {
                                ids.push_bind(*id);
                            }
                        }
                        ids.push_unseparated(")");
                        let mut connection = pool.acquire_write().await?;
                        query.build().execute(&mut *connection).await?;
                        connection.finish().await?;
                    }
                    if exhausted {
                        break;
                    }
                    let (principal, id, _) = candidates.pop().expect("non-empty content GC batch");
                    cursor = Some((principal, id));
                }
            }
            Self::Postgres(pool) => {
                let mut cursor: Option<(String, i64)> = None;
                loop {
                    let mut query =
                        sqlx::QueryBuilder::<sqlx::Postgres>::new("SELECT principal,id,");
                    query.push(ORPHAN).push(" FROM turn_chain_contents");
                    if let Some((principal, id)) = &cursor {
                        query
                            .push(" WHERE (principal,id)>(")
                            .push_bind(principal)
                            .push(",")
                            .push_bind(id)
                            .push(")");
                    }
                    query
                        .push(" ORDER BY principal,id LIMIT ")
                        .push_bind(BATCH as i64);
                    let mut candidates: Vec<(String, i64, bool)> =
                        query.build_query_as().fetch_all(pool).await?;
                    if candidates.is_empty() {
                        break;
                    }
                    let exhausted = candidates.len() < BATCH;
                    if let Some(first_orphan) = candidates.iter().position(|row| row.2) {
                        // 每批保留原有的插入互斥；获得锁后的 READ COMMITTED
                        // DELETE 使用新快照重检，不能信任锁外读取的孤儿判定。
                        let mut transaction = pool.begin().await?;
                        sqlx::query("LOCK TABLE turn_chain_contents IN SHARE ROW EXCLUSIVE MODE")
                            .execute(&mut *transaction)
                            .await?;
                        let mut delete = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                            "DELETE FROM turn_chain_contents WHERE ",
                        );
                        delete.push(ORPHAN).push(" AND id IN (");
                        let mut ids = delete.separated(",");
                        for (_, id, orphan) in &candidates[first_orphan..] {
                            if *orphan {
                                ids.push_bind(*id);
                            }
                        }
                        ids.push_unseparated(")");
                        delete.build().execute(&mut *transaction).await?;
                        transaction.commit().await?;
                    }
                    if exhausted {
                        break;
                    }
                    let (principal, id, _) = candidates.pop().expect("non-empty content GC batch");
                    cursor = Some((principal, id));
                }
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
    let remaining =
        u64::try_from(expires_at.saturating_sub(now.max(chrono::Utc::now().timestamp_millis())))
            .unwrap_or(0);
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

struct PrefixRebuildNode {
    id: String,
    principal: String,
    parent_id: Option<String>,
    completed_at: Option<i64>,
    stale: bool,
    state: PrefixRebuildVisit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrefixRebuildVisit {
    Unseen,
    Visiting,
    Ready,
    Unavailable,
}

/// Validate complete chains before reading any payload. Shared ancestors enter
/// the parent-first order only once; unavailable heads never enter it.
fn prefix_rebuild_order(nodes: &mut [PrefixRebuildNode]) -> Vec<usize> {
    let by_id: std::collections::HashMap<_, _> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();
    let mut states = vec![PrefixRebuildVisit::Unseen; nodes.len()];
    let mut order = Vec::new();
    let mut path = Vec::new();
    for head in 0..nodes.len() {
        if !nodes[head].stale || states[head] != PrefixRebuildVisit::Unseen {
            continue;
        }
        path.clear();
        let mut current = head;
        let valid = loop {
            match states[current] {
                PrefixRebuildVisit::Ready => break true,
                PrefixRebuildVisit::Visiting | PrefixRebuildVisit::Unavailable => break false,
                PrefixRebuildVisit::Unseen => {}
            }
            states[current] = PrefixRebuildVisit::Visiting;
            path.push(current);
            let Some(parent_id) = nodes[current].parent_id.as_ref() else {
                break true;
            };
            let Some(&parent) = by_id.get(parent_id.as_str()) else {
                break false;
            };
            if nodes[parent].principal != nodes[current].principal {
                break false;
            }
            current = parent;
        };
        for &index in path.iter().rev() {
            states[index] = if valid {
                PrefixRebuildVisit::Ready
            } else {
                PrefixRebuildVisit::Unavailable
            };
            if valid {
                order.push(index);
            }
        }
    }
    for (node, state) in nodes.iter_mut().zip(states) {
        node.state = state;
    }
    order
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
        let rows: Vec<StoredAncestorRow> = async {
            match self {
            Self::Sqlite(pool) => {
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
        let nodes = rows
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
        let nodes = match self {
            Self::Sqlite(pool) => {
                content::restore_sqlite(
                    &mut *pool
                        .acquire()
                        .await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?,
                    &principal,
                    nodes,
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
                    nodes,
                )
                .await
            }
        }
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
        Ok(MaterializedTurnChain { nodes, expires_at })
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "turn_chain.materialize",
        skip_all,
        fields(node_count = 0_u64)
    )]
    async fn materialize_in_context(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
        context: &mut TurnReadContext,
    ) -> Result<SharedMaterializedTurnChain, TurnUnavailable> {
        use tracing::Instrument as _;
        context.begin_read();
        if !context.matches(principal, kind) {
            return Err(TurnUnavailable::Unavailable);
        }
        if let Some(chain) = context.get(id) {
            return Ok(chain);
        }
        let known = serde_json::to_string(&context.validated_ids())
            .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
        let principal_key = principal.continuation_key();
        macro_rules! select {
        ($pool:expr, $db:ty, $depth:expr, $join:expr, $known_start:expr, $known_end:expr) => {{
            let mut query = sqlx::QueryBuilder::<$db>::new(
                "WITH RECURSIVE ancestors(id, parent_id, payload_version, payload, expires_at, depth) AS (SELECT id, parent_id, payload_version, payload, expires_at, ");
            query.push($depth).push(" FROM turn_chain_nodes WHERE id = ").push_bind(id.as_str())
                .push(" AND principal = ").push_bind(&principal_key)
                .push(" AND kind = ").push_bind(kind.as_str())
                .push(" UNION ALL SELECT node.id, node.parent_id, node.payload_version, node.payload, node.expires_at, ancestors.depth + 1 FROM ")
                .push($join).push(" WHERE node.id = ancestors.parent_id AND node.principal = ")
                .push_bind(&principal_key).push(" AND node.kind = ").push_bind(kind.as_str());
            query.push($known_start).push_bind(&known).push($known_end);
            query.push(") SELECT id, parent_id, payload_version, payload, expires_at, depth FROM ancestors ORDER BY depth DESC");
            query.build_query_as::<StoredAncestorRow>()
                .fetch_all($pool).await
        }};
    }
        let rows = async {
        let rows = match self {
            Self::Sqlite(pool) => select!(pool, sqlx::Sqlite, "0", "ancestors CROSS JOIN turn_chain_nodes node", " AND node.id NOT IN (SELECT value FROM json_each(", "))"),
            Self::Postgres(pool) => select!(pool, sqlx::Postgres, "0::BIGINT", "ancestors CROSS JOIN turn_chain_nodes node", " AND node.id NOT IN (SELECT jsonb_array_elements_text(", "::jsonb))"),
        }.map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
        tracing::Span::current().record("node_count", rows.len() as u64);
        Ok::<_, TurnUnavailable>(rows)
    }.instrument(tracing::info_span!(target: "stravia::perf", "turn_chain.ancestor.select", node_count = 0_u64)).await?;
        tracing::Span::current().record("node_count", rows.len() as u64);
        self.finish_context_read(principal, kind, context, rows, std::time::Instant::now())
            .await
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
                Self::Sqlite(pool) => {
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
        // SQLite can retain this owned blob in its arguments. Borrowing the
        // Vec here makes SQLx copy the entire envelope before the insert.
        let payload = std::mem::take(&mut encoded.payload);
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
        let projection = commit
            .reusable_prefix
            .as_ref()
            .and_then(|prefix| prefix.projection.as_ref());
        let projection_version = projection.map(|proof| i64::from(proof.version));
        let controls_fingerprint = projection.map(|proof| proof.controls_fingerprint.as_str());
        let context_fingerprint = projection.map(|proof| proof.context_fingerprint.as_str());
        let prefix_completed_at = commit
            .reusable_prefix
            .as_ref()
            .map(|prefix| prefix.completed_at);

        match self {
            Self::Sqlite(pool) => {
                // 先取得池级写租约，再开启写事务，避免进入 SQLite busy handler 竞争。
                let mut connection = pool
                    .acquire_write()
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
                      prefix_projection_version, prefix_controls_fingerprint, prefix_context_fingerprint, storage_format) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 2)",
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
                .bind(projection_version)
                .bind(controls_fingerprint)
                .bind(context_fingerprint)
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
                connection
                    .finish()
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
                      prefix_projection_version, prefix_controls_fingerprint, prefix_context_fingerprint, storage_format) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 2)",
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
                .bind(projection_version)
                .bind(controls_fingerprint)
                .bind(context_fingerprint)
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
        macro_rules! push_projection {
        ($builder:ident, $values:expr) => {
            if let Some(proof) = query.projection.as_ref()
                .filter(|proof| proof.version == REUSABLE_PREFIX_PROJECTION_VERSION) {
                // Only a complete, known-version proof can exclude a row.
                // NULL and unknown versions remain authoritative-read candidates.
                $builder.push(" AND (prefix_projection_version IS NULL OR prefix_projection_version <> ")
                    .push_bind(i64::from(REUSABLE_PREFIX_PROJECTION_VERSION))
                    .push(" OR prefix_controls_fingerprint IS NULL OR prefix_context_fingerprint IS NULL OR (")
                    .push("prefix_controls_fingerprint = ").push_bind(&proof.controls_fingerprint);
                if proof.context_fingerprints.is_empty() {
                    $builder.push(" AND 1 = 0");
                } else {
                    $builder.push(" AND (prefix_context_fingerprint, prefix_item_count) IN (").push($values);
                    for (index, (fingerprint, units)) in proof.context_fingerprints.iter().enumerate() {
                        if index > 0 { $builder.push(", "); }
                        $builder.push("(").push_bind(fingerprint).push(", ")
                            .push_bind(i64::from(*units)).push(")");
                    }
                    $builder.push(")");
                }
                $builder.push("))");
            }
        };
    }
        let rows: Vec<(String, i64, i64)> = match self {
            Self::Sqlite(pool) => {
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
                builder.push(")");
                push_projection!(builder, "VALUES ");
                builder.push(" ORDER BY prefix_item_count DESC, prefix_completed_at DESC, id DESC");
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
                builder.push(")");
                push_projection!(builder, "");
                builder.push(" ORDER BY prefix_item_count DESC, prefix_completed_at DESC, id DESC");
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
        decode: &mut (
                 dyn FnMut(TurnNode, Option<i64>) -> Result<Option<ReusablePrefixMetadata>, String>
                     + Send
             ),
    ) -> Result<(), TurnUnavailable> {
        macro_rules! rebuild {
        ($pool:expr, $restore:ident) => {{
            let mut connection = $pool.acquire_write().await
                .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            let mut transaction = connection.begin().await
                .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            let now = chrono::Utc::now().timestamp_millis();
            let stale = format!("{namespace_prefix}%");
            // UNION (not UNION ALL) deduplicates shared ancestors and terminates
            // corrupt cycles. Keep payloads out of this closure query.
            let rows: Vec<(String, String, Option<String>, Option<i64>, i32)> = sqlx::query_as(
                "WITH RECURSIVE ancestors(id, principal, parent_id, prefix_completed_at, stale) AS (\
                 SELECT id, principal, parent_id, prefix_completed_at, 1 FROM turn_chain_nodes \
                 WHERE kind = 'response' AND prefix_namespace IS NOT NULL \
                 AND prefix_namespace NOT LIKE $1 AND expires_at > $2 \
                 UNION SELECT node.id, node.principal, node.parent_id, \
                 CASE WHEN node.prefix_namespace IS NOT NULL THEN node.prefix_completed_at ELSE NULL END, \
                 CASE WHEN node.prefix_namespace IS NOT NULL AND node.prefix_namespace NOT LIKE $1 THEN 1 ELSE 0 END \
                 FROM ancestors JOIN turn_chain_nodes node ON node.id = ancestors.parent_id \
                 AND node.principal = ancestors.principal \
                 WHERE node.kind = 'response' AND node.expires_at > $2) \
                 SELECT id, principal, parent_id, prefix_completed_at, stale FROM ancestors"
            ).bind(&stale).bind(now).fetch_all(&mut *transaction).await
                .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            let mut nodes: Vec<_> = rows.into_iter().map(
                |(id, principal, parent_id, completed_at, stale)| PrefixRebuildNode {
                    id, principal, parent_id, completed_at, stale: stale != 0, state: PrefixRebuildVisit::Unseen,
                }
            ).collect();
            let order = prefix_rebuild_order(&mut nodes);
            for node in nodes.iter().filter(|node| node.stale && node.state == PrefixRebuildVisit::Unavailable) {
                sqlx::query("UPDATE turn_chain_nodes SET prefix_namespace = NULL, prefix_fingerprint = NULL, prefix_item_count = NULL, prefix_completed_at = NULL, prefix_projection_version = NULL, prefix_controls_fingerprint = NULL, prefix_context_fingerprint = NULL WHERE id = $1 AND principal = $2")
                    .bind(&node.id).bind(&node.principal).execute(&mut *transaction).await
                    .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            }
            for index in order {
                let metadata = &nodes[index];
                let (version, payload): (i64, Vec<u8>) = sqlx::query_as(
                    "SELECT payload_version, payload FROM turn_chain_nodes \
                     WHERE id = $1 AND principal = $2 AND kind = 'response'"
                ).bind(&metadata.id).bind(&metadata.principal)
                    .fetch_one(&mut *transaction).await
                    .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                let nodes = vec![decode_node(
                    TurnNodeId::new(metadata.id.clone()), TurnNodeKind::Response,
                    metadata.parent_id.clone(), version, payload,
                )?];
                let restored = content::$restore(&mut transaction, &metadata.principal, nodes).await
                    .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                let node = restored.into_iter().next().expect("restored node");
                let prefix = decode(node, metadata.completed_at)
                    .map_err(TurnUnavailable::Storage)?;
                // Current/no-index ancestors only populate the decoder's domain
                // state. Their existing index and completion time stay untouched.
                if !metadata.stale {
                    continue;
                }
                let Some(prefix) = prefix else {
                    sqlx::query("UPDATE turn_chain_nodes SET prefix_namespace = NULL, prefix_fingerprint = NULL, prefix_item_count = NULL, prefix_completed_at = NULL, prefix_projection_version = NULL, prefix_controls_fingerprint = NULL, prefix_context_fingerprint = NULL WHERE id = $1 AND principal = $2")
                        .bind(&metadata.id).bind(&metadata.principal).execute(&mut *transaction).await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                    continue;
                };
                let proof = prefix.projection.as_ref();
                sqlx::query("UPDATE turn_chain_nodes SET prefix_namespace = $1, prefix_fingerprint = $2, prefix_item_count = $3, prefix_projection_version = $6, prefix_controls_fingerprint = $7, prefix_context_fingerprint = $8 WHERE id = $4 AND principal = $5")
                    .bind(&prefix.namespace).bind(&prefix.fingerprint).bind(i64::from(prefix.item_count)).bind(&metadata.id).bind(&metadata.principal)
                    .bind(proof.map(|proof| i64::from(proof.version)))
                    .bind(proof.map(|proof| proof.controls_fingerprint.as_str()))
                    .bind(proof.map(|proof| proof.context_fingerprint.as_str()))
                    .execute(&mut *transaction).await.map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            }
            transaction.commit().await.map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
            connection.finish().await.map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
        }};
    }
        match self {
            Self::Sqlite(pool) => rebuild!(pool, restore_sqlite),
            Self::Postgres(pool) => rebuild!(pool, restore_postgres),
        }
        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut removed = 0_u64;
        loop {
            let rows = match self {
                Self::Sqlite(pool) => {
                    let mut connection = pool
                        .acquire_write()
                        .await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                    let result = sqlx::query(
                        "DELETE FROM turn_chain_nodes WHERE expires_at <= ? \
                                         AND NOT EXISTS (SELECT 1 FROM turn_chain_nodes child \
                                         WHERE child.parent_id = turn_chain_nodes.id)",
                    )
                    .bind(now)
                    .execute(&mut *connection)
                    .await
                    .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                    connection
                        .finish()
                        .await
                        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
                    Ok(result.rows_affected())
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
            ($pool:expr, $put:ident, $restore:ident, $lock:literal) => {{
                let rows: Vec<(String, String, Vec<u8>)> =
                    sqlx::query_as("SELECT id, principal, payload FROM turn_chain_nodes")
                        .fetch_all($pool)
                        .await?;
                for (id, principal, payload) in rows {
                    let envelope: serde_json::Value =
                        serde_json::from_slice(&crate::storage_codec::decode(&payload)?)?;
                    let nodes = vec![TurnNode {
                        id: TurnNodeId::new(id.clone()),
                        kind: TurnNodeKind::Response,
                        parent_id: None,
                        payload_version: 1,
                        payload: envelope,
                    }];
                    let nodes = {
                        let mut connection = $pool.acquire().await?;
                        content::$restore(&mut connection, &principal, nodes).await?
                    };
                    let mut payload = nodes.into_iter().next().expect("restored node").payload;
                    edit(&mut payload);
                    let mut encoded = tokio::task::spawn_blocking(move || content::encode(payload))
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("history rewrite preparation worker failed: {error}")
                        })??;
                    let mut connection = $pool.acquire_write().await?;
                    let mut transaction = connection.begin().await?;
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
                    connection.finish().await?;
                }
                anyhow::Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool) => {
                rewrite!(pool, put_sqlite, restore_sqlite, "SELECT 1")
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
