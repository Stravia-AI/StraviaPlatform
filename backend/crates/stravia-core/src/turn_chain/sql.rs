use super::*;

use std::collections::HashMap;

/// 统一保留期由存储持有；读写不按节点到期时间过滤，缓存有效期按头节点
/// `created_at + N` 现算，因此修改 N 后下一次读取立即按新值判断。
#[derive(Clone)]
pub struct SqlTurnChainStore {
    pub(super) backend: Backend,
    pub(super) retention: crate::history_retention::HistoryRetention,
}

#[derive(Clone)]
pub(super) enum Backend {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

fn storage(error: impl std::fmt::Display) -> TurnUnavailable {
    TurnUnavailable::Storage(error.to_string())
}

fn commit_storage(error: impl std::fmt::Display) -> TurnCommitError {
    TurnCommitError::Storage(error.to_string())
}

/// 在同一事务内按 Principal 锁定依赖行；SQLite 由 `BEGIN IMMEDIATE` 串行化。
async fn dependency_references_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    principal: &str,
    table: &str,
    column: &str,
    references: &[String],
) -> Result<Vec<String>, TurnCommitError> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT ");
    query
        .push(column)
        .push(" FROM ")
        .push(table)
        .push(" WHERE principal=");
    query
        .push_bind(principal)
        .push(" AND ")
        .push(column)
        .push(" IN (");
    let mut list = query.separated(",");
    for reference in references {
        list.push_bind(reference);
    }
    list.push_unseparated(")");
    query
        .build_query_scalar::<String>()
        .fetch_all(&mut *connection)
        .await
        .map_err(commit_storage)
}

async fn dependency_references_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    principal: &str,
    table: &str,
    column: &str,
    references: &[String],
) -> Result<Vec<String>, TurnCommitError> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new("SELECT ");
    query
        .push(column)
        .push(" FROM ")
        .push(table)
        .push(" WHERE principal=");
    query
        .push_bind(principal)
        .push(" AND ")
        .push(column)
        .push(" IN (");
    let mut list = query.separated(",");
    for reference in references {
        list.push_bind(reference);
    }
    list.push_unseparated(") FOR KEY SHARE");
    query
        .build_query_scalar::<String>()
        .fetch_all(&mut *connection)
        .await
        .map_err(commit_storage)
}

fn deadline_from_unix_millis(expires_at: i64, now: i64) -> std::time::Instant {
    let remaining =
        u64::try_from(expires_at.saturating_sub(now.max(chrono::Utc::now().timestamp_millis())))
            .unwrap_or(0);
    std::time::Instant::now()
        .checked_add(Duration::from_millis(remaining))
        .unwrap_or_else(std::time::Instant::now)
}

impl StoredAncestorRow {
    fn into_stored(self, kind: TurnNodeKind) -> Result<content::StoredNode, TurnUnavailable> {
        if self.parent_pk.is_some() && self.parent_id.is_none() {
            return Err(TurnUnavailable::Unavailable);
        }
        Ok(content::StoredNode {
            node_pk: self.node_pk,
            scope_id: self.scope_id,
            references: self.content_references.0,
            id: TurnNodeId::new(self.id),
            kind,
            parent_id: self.parent_id.map(TurnNodeId::new),
            payload_version: u32::try_from(self.payload_version).map_err(storage)?,
            payload: self
                .payload
                .ok_or_else(|| storage("missing history payload"))?,
        })
    }
}

// CTE 只传递窄身份，UNION 同时终止损坏的环；正文不进入递归工作表或排序。
// SQL 不承诺递归输出顺序，按持久父边恢复顺序，不依赖数字 ID 的分配顺序。
fn order_ancestors(
    rows: Vec<StoredAncestorRow>,
    head: &TurnNodeId,
) -> Result<Vec<StoredAncestorRow>, TurnUnavailable> {
    let Some(head_pk) = rows
        .iter()
        .find(|row| row.id == head.as_str())
        .map(|row| row.node_pk)
    else {
        return Err(TurnUnavailable::Unavailable);
    };
    let mut remaining: HashMap<_, _> = rows.into_iter().map(|row| (row.node_pk, row)).collect();
    let mut chain = Vec::with_capacity(remaining.len());
    let mut next = Some(head_pk);
    while let Some(node_pk) = next {
        let Some(row) = remaining.remove(&node_pk) else {
            break;
        };
        if row.parent_pk.is_some() && row.parent_id.is_none() {
            return Err(TurnUnavailable::Unavailable);
        }
        next = row.parent_pk;
        chain.push(row);
    }
    if !remaining.is_empty()
        || next.is_some_and(|parent| chain.iter().any(|row| row.node_pk == parent))
    {
        return Err(TurnUnavailable::Unavailable);
    }
    chain.reverse();
    Ok(chain)
}

impl SqlTurnChainStore {
    pub(crate) fn sqlite(
        pool: SqlitePool,
        retention: crate::history_retention::HistoryRetention,
    ) -> Self {
        Self {
            backend: Backend::Sqlite(pool),
            retention,
        }
    }
    pub(crate) fn postgres(
        pool: PgPool,
        retention: crate::history_retention::HistoryRetention,
    ) -> Self {
        Self {
            backend: Backend::Postgres(pool),
            retention,
        }
    }

    /// 共享正文回收句柄，供 `HistoryMaintenance` 在所有引用方回收之后使用。
    pub(crate) fn content_gc(&self) -> crate::history_retention::ContentGc {
        match &self.backend {
            Backend::Sqlite(pool) => crate::history_retention::ContentGc::Sqlite(pool.clone()),
            Backend::Postgres(pool) => crate::history_retention::ContentGc::Postgres(pool.clone()),
        }
    }

    #[cfg(test)]
    pub(crate) fn sqlite_pool_for_test(&self) -> Option<&sqlx::SqlitePool> {
        match &self.backend {
            Backend::Sqlite(pool) => Some(pool),
            Backend::Postgres(_) => None,
        }
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "turn_chain.ancestor.select",
        skip_all,
        fields(node_count)
    )]
    async fn select_ancestors(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
        known: Option<&str>,
    ) -> Result<Vec<StoredAncestorRow>, TurnUnavailable> {
        let principal = principal.continuation_key();
        macro_rules! select {
            ($pool:expr, $db:ty, $backend:ident, $known_start:literal, $known_end:literal) => {{
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "WITH RECURSIVE ancestors(node_pk,parent_pk,scope_id,kind,created_at) AS (\
                     SELECT n.node_pk,n.parent_pk,n.scope_id,n.kind,n.created_at FROM turn_nodes n \
                     JOIN content_scopes s ON s.id=n.scope_id \
                     WHERE n.id=",
                );
                query.push_bind(id.as_str()).push(" AND s.principal=").push_bind(&principal)
                    .push(" AND n.kind=").push_bind(kind.as_str())
                    .push(" AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting) \
                           UNION SELECT n.node_pk,n.parent_pk,n.scope_id,n.kind,n.created_at FROM ancestors a \
                           CROSS JOIN turn_nodes n \
                           WHERE n.node_pk=a.parent_pk AND n.scope_id=a.scope_id AND n.kind=a.kind \
                           AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting)");
                if let Some(known) = known {
                    query.push($known_start).push_bind(known).push($known_end);
                }
                query.push(") SELECT n.node_pk,n.scope_id,n.parent_pk,n.id,parent.id AS parent_id,n.payload_version,\
                            p.payload,a.created_at,").push(content::reference_projection!($backend))
                    .push(" FROM ancestors a JOIN turn_nodes n ON n.node_pk=a.node_pk \
                            LEFT JOIN turn_nodes parent ON parent.node_pk=n.parent_pk \
                            LEFT JOIN turn_node_payloads p ON p.node_pk=n.node_pk");
                query.build_query_as::<StoredAncestorRow>().fetch_all($pool).await
            }};
        }
        let rows = match &self.backend {
            Backend::Sqlite(pool) => select!(
                pool,
                sqlx::Sqlite,
                sqlite,
                " AND n.id NOT IN (SELECT value FROM json_each(",
                "))"
            ),
            Backend::Postgres(pool) => select!(
                pool,
                sqlx::Postgres,
                postgres,
                " AND n.id NOT IN (SELECT jsonb_array_elements_text(",
                "::jsonb))"
            ),
        }
        .map_err(storage)?;
        tracing::Span::current().record("node_count", rows.len() as u64);
        order_ancestors(rows, id)
    }

    pub(super) async fn finish_context_read(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        context: &mut TurnReadContext,
        rows: Vec<StoredAncestorRow>,
        completed_at: std::time::Instant,
    ) -> Result<SharedMaterializedTurnChain, TurnUnavailable> {
        let now = chrono::Utc::now().timestamp_millis();
        if rows.is_empty() {
            return Err(TurnUnavailable::Unavailable);
        }
        // 整条链共用头节点（最新节点）的计时；祖先因有子节点而继续保留。
        let head_created_at = rows.last().expect("nonempty SQL suffix").created_at;
        let expires_at = deadline_from_unix_millis(self.retention.deadline(head_created_at), now);
        let mut prefix = match rows[0].parent_id.as_ref() {
            Some(parent) => match context.get_at(&TurnNodeId::new(parent), completed_at) {
                Some(prefix) => prefix,
                None => {
                    // SQL await 期间局部证明过期，重新读取完整事实而不是把缓存失效当成丢失。
                    context.begin_read();
                    let head = TurnNodeId::new(&rows.last().expect("nonempty SQL suffix").id);
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
                expires_at,
            },
        };
        let nodes = rows
            .into_iter()
            .map(|row| row.into_stored(kind))
            .collect::<Result<Vec<_>, _>>()?;
        let finish = |node, context: Option<&TurnReadContext>| {
            context
                .expect("history read context")
                .prepare_node(node, expires_at)
                .map_err(anyhow::Error::new)
        };
        let restored = match &self.backend {
            Backend::Sqlite(pool) => {
                content::restore_sqlite_context(
                    &mut *pool.acquire().await.map_err(storage)?,
                    nodes,
                    Some(context),
                    finish,
                )
                .await
            }
            Backend::Postgres(pool) => {
                content::restore_postgres_context(
                    &mut *pool.acquire().await.map_err(storage)?,
                    nodes,
                    Some(context),
                    finish,
                )
                .await
            }
        };
        let nodes = restored.map_err(|error| {
            context.begin_read();
            error.downcast::<TurnUnavailable>().unwrap_or_else(storage)
        })?;
        // 整条链共用头节点的计时：缓存前缀来自更早的头节点，不能把链的有效期拉回祖先自己的期限。
        prefix.expires_at = expires_at;
        for node in nodes {
            prefix.nodes.push(node);
        }
        Ok(prefix)
    }
}

#[derive(sqlx::FromRow)]
struct PrefixRebuildNode {
    node_pk: i64,
    id: String,
    scope_id: i64,
    parent_pk: Option<i64>,
    parent_id: Option<String>,
    completed_at: Option<i64>,
    stale: i32,
    #[sqlx(skip)]
    state: PrefixRebuildVisit,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum PrefixRebuildVisit {
    #[default]
    Unseen,
    Visiting,
    Ready,
    Unavailable,
}

fn prefix_rebuild_order(nodes: &mut [PrefixRebuildNode]) -> Vec<usize> {
    let by_id: HashMap<_, _> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.node_pk, index))
        .collect();
    let mut states = vec![PrefixRebuildVisit::Unseen; nodes.len()];
    let mut order = Vec::new();
    let mut path = Vec::new();
    for head in 0..nodes.len() {
        if nodes[head].stale == 0 || states[head] != PrefixRebuildVisit::Unseen {
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
            let Some(parent_id) = nodes[current].parent_pk else {
                break true;
            };
            let Some(&parent) = by_id.get(&parent_id) else {
                break false;
            };
            if nodes[parent].scope_id != nodes[current].scope_id {
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

const PREFIX_REBUILD_STALE_EXISTS_SQL: &str = "\
    SELECT EXISTS(\
        SELECT 1 FROM turn_nodes n \
        WHERE n.kind='response' \
        AND n.prefix_namespace IS NOT NULL AND n.prefix_namespace NOT LIKE $1 \
        AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting)\
    )";

const PREFIX_REBUILD_ANCESTORS_SQL: &str = "\
    WITH RECURSIVE ancestors(node_pk,scope_id,parent_pk,completed_at,stale) AS (\
        SELECT n.node_pk,n.scope_id,n.parent_pk,n.prefix_completed_at,1 FROM turn_nodes n \
        WHERE n.kind='response' \
        AND n.prefix_namespace IS NOT NULL AND n.prefix_namespace NOT LIKE $1 \
        AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting) \
        UNION SELECT n.node_pk,n.scope_id,n.parent_pk,\
        CASE WHEN n.prefix_namespace IS NOT NULL THEN n.prefix_completed_at ELSE NULL END,\
        CASE WHEN n.prefix_namespace IS NOT NULL AND n.prefix_namespace NOT LIKE $1 THEN 1 ELSE 0 END \
        FROM ancestors a JOIN turn_nodes n ON n.node_pk=a.parent_pk AND n.scope_id=a.scope_id \
        WHERE n.kind='response' \
        AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting)\
    ) \
    SELECT n.node_pk,n.id,n.scope_id,n.parent_pk,p.id AS parent_id,a.completed_at,a.stale \
    FROM ancestors a JOIN turn_nodes n ON n.node_pk=a.node_pk \
    LEFT JOIN turn_nodes p ON p.node_pk=n.parent_pk";

struct PrefixUpdateRow {
    node_pk: i64,
    namespace: String,
    fingerprint: String,
    item_count: i64,
    projection_version: Option<i64>,
    controls_fingerprint: Option<String>,
    context_fingerprint: Option<String>,
}

macro_rules! prefix_rebuild_io {
    ($fetch:ident, $clear:ident, $update:ident, $db:ty, $conn:ty) => {
        async fn $fetch(
            connection: &mut $conn,
            nodes: &[PrefixRebuildNode],
            indices: &[usize],
        ) -> Result<Vec<content::StoredNode>, TurnUnavailable> {
            if indices.is_empty() {
                return Ok(Vec::new());
            }
            let mut payload_query = sqlx::QueryBuilder::<$db>::new(
                "SELECT n.node_pk, n.payload_version, p.payload \
                 FROM turn_nodes n LEFT JOIN turn_node_payloads p ON p.node_pk=n.node_pk \
                 WHERE n.node_pk IN (",
            );
            let mut list = payload_query.separated(",");
            for &index in indices {
                list.push_bind(nodes[index].node_pk);
            }
            list.push_unseparated(")");
            let payload_rows: Vec<(i64, i64, Option<Vec<u8>>)> = payload_query
                .build_query_as()
                .fetch_all(&mut *connection)
                .await
                .map_err(storage)?;
            let mut payloads: HashMap<i64, (i64, Option<Vec<u8>>)> =
                HashMap::with_capacity(payload_rows.len());
            for (node_pk, version, payload) in payload_rows {
                payloads.insert(node_pk, (version, payload));
            }

            let mut ref_query = sqlx::QueryBuilder::<$db>::new(
                "SELECT node_pk, scope_id, ordinal, content_id \
                 FROM turn_node_contents WHERE node_pk IN (",
            );
            let mut list = ref_query.separated(",");
            for &index in indices {
                list.push_bind(nodes[index].node_pk);
            }
            list.push_unseparated(")");
            let ref_rows: Vec<(i64, i64, i64, i64)> = ref_query
                .build_query_as()
                .fetch_all(&mut *connection)
                .await
                .map_err(storage)?;
            let mut refs_by_pk: HashMap<i64, Vec<[i64; 3]>> = HashMap::with_capacity(indices.len());
            for (node_pk, scope_id, ordinal, content_id) in ref_rows {
                refs_by_pk
                    .entry(node_pk)
                    .or_default()
                    .push([scope_id, ordinal, content_id]);
            }

            let mut stored = Vec::with_capacity(indices.len());
            for &index in indices {
                let metadata = &nodes[index];
                let (version, payload) = payloads
                    .remove(&metadata.node_pk)
                    .ok_or_else(|| storage("missing history node"))?;
                stored.push(content::StoredNode {
                    node_pk: metadata.node_pk,
                    scope_id: metadata.scope_id,
                    references: refs_by_pk.remove(&metadata.node_pk).unwrap_or_default(),
                    id: TurnNodeId::new(metadata.id.clone()),
                    kind: TurnNodeKind::Response,
                    parent_id: metadata.parent_id.as_ref().map(TurnNodeId::new),
                    payload_version: u32::try_from(version).map_err(storage)?,
                    payload: payload.ok_or_else(|| storage("missing history payload"))?,
                });
            }
            Ok(stored)
        }

        async fn $clear(
            connection: &mut $conn,
            node_pks: &[i64],
            stale_guard: Option<&str>,
        ) -> Result<(), TurnUnavailable> {
            for chunk in node_pks.chunks(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "UPDATE turn_nodes SET prefix_namespace=NULL,prefix_fingerprint=NULL,\
                     prefix_item_count=NULL,prefix_completed_at=NULL,prefix_projection_version=NULL,\
                     prefix_controls_fingerprint=NULL,prefix_context_fingerprint=NULL \
                     WHERE node_pk IN (",
                );
                let mut list = query.separated(",");
                for &pk in chunk {
                    list.push_bind(pk);
                }
                list.push_unseparated(")");
                if let Some(stale) = stale_guard {
                    query
                        .push(" AND prefix_namespace IS NOT NULL AND prefix_namespace NOT LIKE ")
                        .push_bind(stale);
                }
                query.build().execute(&mut *connection).await.map_err(storage)?;
            }
            Ok(())
        }

        async fn $update(
            connection: &mut $conn,
            updates: &[PrefixUpdateRow],
            stale_guard: Option<&str>,
        ) -> Result<(), TurnUnavailable> {
            for chunk in updates.chunks(100) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "WITH v(pk,ns,fp,cnt,pv,cf,xf) AS (",
                );
                query.push_values(chunk, |mut row, item| {
                    row.push_bind(item.node_pk)
                        .push_bind(&item.namespace)
                        .push_bind(&item.fingerprint)
                        .push_bind(item.item_count)
                        .push_bind(item.projection_version)
                        .push_bind(item.controls_fingerprint.as_deref())
                        .push_bind(item.context_fingerprint.as_deref());
                });
                query.push(
                    ") UPDATE turn_nodes SET \
                     prefix_namespace=v.ns,\
                     prefix_fingerprint=v.fp,\
                     prefix_item_count=v.cnt,\
                     prefix_projection_version=v.pv,\
                     prefix_controls_fingerprint=v.cf,\
                     prefix_context_fingerprint=v.xf \
                     FROM v WHERE turn_nodes.node_pk=v.pk",
                );
                if let Some(stale) = stale_guard {
                    query
                        .push(
                            " AND turn_nodes.prefix_namespace IS NOT NULL \
                             AND turn_nodes.prefix_namespace NOT LIKE ",
                        )
                        .push_bind(stale);
                }
                query.build().execute(&mut *connection).await.map_err(storage)?;
            }
            Ok(())
        }
    };
}

prefix_rebuild_io!(
    fetch_rebuild_batch_sqlite,
    clear_prefixes_batch_sqlite,
    update_prefixes_batch_sqlite,
    sqlx::Sqlite,
    sqlx::SqliteConnection<'_>
);
prefix_rebuild_io!(
    fetch_rebuild_batch_postgres,
    clear_prefixes_batch_postgres,
    update_prefixes_batch_postgres,
    sqlx::Postgres,
    sqlx::PgConnection<'_>
);

impl SqlTurnChainStore {
    /// 启动期前缀重建优化路径：只读快路径判空，按批读取与多核槽位过滤恢复，
    /// 仅在每批写回时短暂持有写事务，批提交后推进真实启动进度；中断可从剩余旧节点续跑。
    pub(crate) async fn rebuild_prefixes_batched(
        &self,
        namespace_prefix: &str,
        slot_filter: Option<content::SlotFilter>,
        progress: Option<(&'static str, &'static str)>,
        decode: &mut (
                 dyn FnMut(TurnNode, Option<i64>) -> Result<Option<ReusablePrefixMetadata>, String>
                     + Send
             ),
    ) -> Result<(), TurnUnavailable> {
        let stale = format!("{namespace_prefix}%");
        let report_progress = |completed: u64, total: Option<u64>| {
            if let Some((phase, label)) = progress {
                crate::startup_progress::report(phase, label, completed, total);
            }
        };
        macro_rules! rebuild_batched {
            ($pool:expr, $fetch:ident, $restore:ident, $clear:ident, $update:ident, $lock:literal) => {{
                let has_stale: bool = sqlx::query_scalar(PREFIX_REBUILD_STALE_EXISTS_SQL)
                    .bind(&stale)
                    .fetch_one($pool)
                    .await
                    .map_err(storage)?;
                if !has_stale {
                    return Ok(());
                }
                report_progress(0, None);
                let mut reader = $pool.acquire().await.map_err(storage)?;
                let mut nodes: Vec<PrefixRebuildNode> =
                    sqlx::query_as(PREFIX_REBUILD_ANCESTORS_SQL)
                        .bind(&stale)
                        .fetch_all(&mut *reader)
                        .await
                        .map_err(storage)?;
                let order = prefix_rebuild_order(&mut nodes);
                let unavailable: Vec<i64> = nodes
                    .iter()
                    .filter(|node| node.stale != 0 && node.state == PrefixRebuildVisit::Unavailable)
                    .map(|node| node.node_pk)
                    .collect();
                let total_stale = nodes.iter().filter(|node| node.stale != 0).count() as u64;
                if total_stale == 0 {
                    return Ok(());
                }
                let mut completed = 0u64;
                report_progress(0, Some(total_stale));
                drop(reader);

                for chunk in unavailable.chunks(400) {
                    let mut transaction = $pool.begin_write().await.map_err(storage)?;
                    if !$lock.is_empty() {
                        sqlx::query($lock)
                            .execute(&mut *transaction)
                            .await
                            .map_err(storage)?;
                    }
                    $clear(&mut transaction, chunk, Some(&stale)).await?;
                    transaction.commit().await.map_err(storage)?;
                    completed += chunk.len() as u64;
                    report_progress(completed, Some(total_stale));
                }

                let mut cache = content::UpgradeContentCache::default();
                for chunk in order.chunks(content::UPGRADE_NODE_BATCH) {
                    let restored = {
                        let mut reader = $pool.acquire().await.map_err(storage)?;
                        let stored = $fetch(&mut reader, &nodes, chunk).await?;
                        content::$restore(&mut reader, stored, Some(&mut cache), slot_filter)
                            .await
                            .map_err(storage)?
                    };
                    let mut to_clear = Vec::new();
                    let mut to_update = Vec::new();
                    let mut batch_stale = 0u64;
                    for (&index, node) in chunk.iter().zip(restored) {
                        let metadata = &nodes[index];
                        let prefix = decode(node, metadata.completed_at)
                            .map_err(TurnUnavailable::Storage)?;
                        if metadata.stale == 0 {
                            continue;
                        }
                        batch_stale += 1;
                        let Some(prefix) = prefix else {
                            to_clear.push(metadata.node_pk);
                            continue;
                        };
                        let proof = prefix.projection.as_ref();
                        to_update.push(PrefixUpdateRow {
                            node_pk: metadata.node_pk,
                            namespace: prefix.namespace,
                            fingerprint: prefix.fingerprint,
                            item_count: i64::from(prefix.item_count),
                            projection_version: proof.map(|p| i64::from(p.version)),
                            controls_fingerprint: proof.map(|p| p.controls_fingerprint.clone()),
                            context_fingerprint: proof.map(|p| p.context_fingerprint.clone()),
                        });
                    }
                    if !to_clear.is_empty() || !to_update.is_empty() {
                        let mut transaction = $pool.begin_write().await.map_err(storage)?;
                        if !$lock.is_empty() {
                            sqlx::query($lock)
                                .execute(&mut *transaction)
                                .await
                                .map_err(storage)?;
                        }
                        if !to_clear.is_empty() {
                            $clear(&mut transaction, &to_clear, Some(&stale)).await?;
                        }
                        if !to_update.is_empty() {
                            $update(&mut transaction, &to_update, Some(&stale)).await?;
                        }
                        transaction.commit().await.map_err(storage)?;
                    }
                    if batch_stale > 0 {
                        completed += batch_stale;
                        report_progress(completed, Some(total_stale));
                    }
                }
            }};
        }
        match &self.backend {
            Backend::Sqlite(pool) => rebuild_batched!(
                pool,
                fetch_rebuild_batch_sqlite,
                restore_upgrade_cached_sqlite,
                clear_prefixes_batch_sqlite,
                update_prefixes_batch_sqlite,
                ""
            ),
            Backend::Postgres(pool) => rebuild_batched!(
                pool,
                fetch_rebuild_batch_postgres,
                restore_upgrade_cached_postgres,
                clear_prefixes_batch_postgres,
                update_prefixes_batch_postgres,
                "LOCK TABLE turn_nodes IN ROW EXCLUSIVE MODE"
            ),
        }
        Ok(())
    }
}

#[async_trait]
impl TurnChainStore for SqlTurnChainStore {
    async fn redaction_references(
        &self,
        principal: &Principal,
        id: &TurnNodeId,
    ) -> Result<Vec<String>, TurnUnavailable> {
        let principal = principal.continuation_key();
        let query = "SELECT m.reference FROM turn_nodes n \
                     JOIN content_scopes s ON s.id=n.scope_id \
                     LEFT JOIN turn_node_redaction_mappings m ON m.node_pk=n.node_pk \
                     WHERE n.id=$1 AND s.principal=$2 \
                     AND NOT EXISTS(SELECT 1 FROM turn_node_retention r \
                                    WHERE r.node_pk=n.node_pk AND r.deleting) \
                     ORDER BY m.reference";
        let rows: Vec<Option<String>> = match &self.backend {
            Backend::Sqlite(pool) => {
                sqlx::query_scalar(query)
                    .bind(id.as_str())
                    .bind(&principal)
                    .fetch_all(pool)
                    .await
            }
            Backend::Postgres(pool) => {
                sqlx::query_scalar(query)
                    .bind(id.as_str())
                    .bind(&principal)
                    .fetch_all(pool)
                    .await
            }
        }
        .map_err(storage)?;
        if rows.is_empty() {
            return Err(TurnUnavailable::Unavailable);
        }
        Ok(rows.into_iter().flatten().collect())
    }

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
        let rows = self.select_ancestors(principal, kind, id, None).await?;
        tracing::Span::current().record("node_count", rows.len() as u64);
        if rows.first().is_none_or(|row| row.parent_pk.is_some()) {
            return Err(TurnUnavailable::Unavailable);
        }
        let now = chrono::Utc::now().timestamp_millis();
        let expires_at = deadline_from_unix_millis(
            self.retention
                .deadline(rows.last().expect("nonempty history rows").created_at),
            now,
        );
        let nodes = rows
            .into_iter()
            .map(|row| row.into_stored(kind))
            .collect::<Result<Vec<_>, _>>()?;
        let nodes = match &self.backend {
            Backend::Sqlite(pool) => {
                content::restore_sqlite(&mut *pool.acquire().await.map_err(storage)?, nodes).await
            }
            Backend::Postgres(pool) => {
                content::restore_postgres(&mut *pool.acquire().await.map_err(storage)?, nodes).await
            }
        }
        .map_err(storage)?;
        Ok(MaterializedTurnChain { nodes, expires_at })
    }

    /// 与 `materialize` 使用同一可见性规则：Principal 范围、统一保留期和未开始删除。
    /// 只查询一次 `turn_nodes`，不读取正文或祖先链。
    async fn node_kind(
        &self,
        principal: &Principal,
        id: &TurnNodeId,
    ) -> Result<TurnNodeKind, TurnUnavailable> {
        let principal = principal.continuation_key();
        let cutoff = chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(self.retention.window_millis());
        let query = "SELECT n.kind FROM turn_nodes n \
                     JOIN content_scopes s ON s.id=n.scope_id \
                     WHERE n.id=$1 AND s.principal=$2 AND n.created_at>$3 \
                     AND NOT EXISTS(SELECT 1 FROM turn_node_retention r \
                                    WHERE r.node_pk=n.node_pk AND r.deleting)";
        let kind: Option<String> = match &self.backend {
            Backend::Sqlite(pool) => {
                sqlx::query_scalar(query)
                    .bind(id.as_str())
                    .bind(&principal)
                    .bind(cutoff)
                    .fetch_optional(pool)
                    .await
            }
            Backend::Postgres(pool) => {
                sqlx::query_scalar(query)
                    .bind(id.as_str())
                    .bind(&principal)
                    .bind(cutoff)
                    .fetch_optional(pool)
                    .await
            }
        }
        .map_err(storage)?;
        match kind.as_deref() {
            Some("response") => Ok(TurnNodeKind::Response),
            Some("agent") => Ok(TurnNodeKind::Agent),
            Some("web_search") => Ok(TurnNodeKind::WebSearch),
            Some(_) => Err(storage("unknown Turn node kind")),
            None => Err(TurnUnavailable::Unavailable),
        }
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
        context.begin_read();
        if !context.matches(principal, kind) {
            return Err(TurnUnavailable::Unavailable);
        }
        if let Some(chain) = context.get(id) {
            return Ok(chain);
        }
        let known = serde_json::to_string(&context.validated_ids()).map_err(storage)?;
        let rows = self
            .select_ancestors(principal, kind, id, Some(&known))
            .await?;
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
        let mut encoded = tokio::task::spawn_blocking(move || content::encode(commit.payload))
            .await
            .map_err(commit_storage)?
            .map_err(commit_storage)?;
        tracing::Span::current().record("reference_count", encoded.contents.len() as u64);
        if !encoded.contents.is_empty() {
            macro_rules! hint {
                ($pool:expr, $lookup:ident) => {{
                    let mut connection = $pool.acquire().await.map_err(commit_storage)?;
                    content::$lookup(
                        &mut connection,
                        content::LookupScope::Principal(&principal),
                        &encoded.contents,
                        false,
                    )
                    .await
                    .map_err(commit_storage)?
                }};
            }
            let ids = match &self.backend {
                Backend::Sqlite(pool) => hint!(pool, lookup_sqlite),
                Backend::Postgres(pool) => hint!(pool, lookup_postgres),
            };
            content::prepare_missing(&mut encoded.contents, &ids)
                .await
                .map_err(commit_storage)?;
        }
        // descriptor 不含数据库分配的 ID，编码和大正文压缩都在写协调之前完成。
        let payload = std::mem::take(&mut encoded.payload);
        let prefix = commit.reusable_prefix.as_ref();
        let projection = prefix.and_then(|prefix| prefix.projection.as_ref());
        let mut marker_references = commit.marker_references;
        marker_references.sort_unstable();
        marker_references.dedup();
        let mut redaction_references = commit.redaction_references;
        redaction_references.sort_unstable();
        redaction_references.dedup();
        let mut compaction_record_ids = commit.compaction_record_ids;
        compaction_record_ids.sort_unstable();
        compaction_record_ids.dedup();
        macro_rules! persist {
            ($pool:expr, $lock:literal, $checkpoint_lock:literal, $lineage_lock:literal, $resolve_scope:path, $deps:ident, $put:ident) => {{
                let mut transaction = $pool.begin_write().await.map_err(commit_storage)?;
                if !$lock.is_empty() { sqlx::query($lock).execute(&mut *transaction).await.map_err(commit_storage)?; }
                let duplicate: Option<i64> = sqlx::query_scalar(
                    "SELECT node_pk FROM turn_nodes WHERE id=$1",
                ).bind(commit.id.as_str())
                    .fetch_optional(&mut *transaction).await.map_err(commit_storage)?;
                if duplicate.is_some() { return Err(TurnCommitError::AlreadyExists); }
                // 共享正文存储的 scope 协议：已有 scope 加 `FOR KEY SHARE`，防止清理方在提交前删除它。
                let scope = $resolve_scope(&mut transaction, &principal)
                    .await.map_err(commit_storage)?;
                // 祖先链只检查存在与 `deleting`；到期但未扫描的链仍可续接。
                let parent_pk = match commit.parent_id.as_ref() {
                    Some(parent) => {
                        let rows: Vec<(i64, i64, Option<i64>, i64)> = sqlx::query_as(
                            "WITH RECURSIVE seed AS (\
                             SELECT n.node_pk,n.parent_pk,n.scope_id FROM turn_nodes n \
                             JOIN content_scopes s ON s.id=n.scope_id \
                             WHERE n.id=$1 AND s.principal=$2 AND n.kind=$3 \
                             AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting)\
                             ), ancestors(node_pk,parent_pk,scope_id) AS (\
                             SELECT node_pk,parent_pk,scope_id FROM seed UNION \
                             SELECT n.node_pk,n.parent_pk,n.scope_id FROM ancestors a \
                             CROSS JOIN turn_nodes n \
                             WHERE n.node_pk=a.parent_pk AND n.scope_id=a.scope_id AND n.kind=$3 \
                             AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting)) \
                             SELECT a.node_pk,a.scope_id,a.parent_pk,(SELECT node_pk FROM seed) FROM ancestors a",
                        ).bind(parent.as_str()).bind(&principal).bind(commit.kind.as_str())
                            .fetch_all(&mut *transaction).await.map_err(commit_storage)?;
                        let mut roots = 0usize;
                        for (_, scope_id, parent_pk, _) in &rows {
                            if *scope_id != scope { return Err(TurnCommitError::ParentUnavailable); }
                            roots += usize::from(parent_pk.is_none());
                        }
                        if roots != 1 || rows.is_empty() {
                            return Err(TurnCommitError::ParentUnavailable);
                        }
                        Some(rows[0].3)
                    }
                    None => None,
                };
                let node_pk: Option<i64> = sqlx::query_scalar(
                    "INSERT INTO turn_nodes(id,scope_id,kind,parent_pk,payload_version,created_at,\
                     prefix_namespace,prefix_fingerprint,prefix_item_count,prefix_completed_at,\
                     prefix_projection_version,prefix_controls_fingerprint,prefix_context_fingerprint) \
                     VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
                     ON CONFLICT(id) DO NOTHING RETURNING node_pk",
                ).bind(commit.id.as_str()).bind(scope).bind(commit.kind.as_str()).bind(parent_pk)
                    .bind(i64::from(commit.payload_version)).bind(now)
                    .bind(prefix.map(|prefix| prefix.namespace.as_str()))
                    .bind(prefix.map(|prefix| prefix.fingerprint.as_str()))
                    .bind(prefix.map(|prefix| i64::from(prefix.item_count)))
                    .bind(prefix.map(|prefix| prefix.completed_at))
                    .bind(projection.map(|proof| i64::from(proof.version)))
                    .bind(projection.map(|proof| proof.controls_fingerprint.as_str()))
                    .bind(projection.map(|proof| proof.context_fingerprint.as_str()))
                    .fetch_optional(&mut *transaction).await.map_err(commit_storage)?;
                let node_pk = node_pk.ok_or(TurnCommitError::AlreadyExists)?;
                sqlx::query("INSERT INTO turn_node_payloads(node_pk,payload) VALUES($1,$2)")
                    .bind(node_pk).bind(payload).execute(&mut *transaction).await.map_err(commit_storage)?;
                sqlx::query("INSERT INTO turn_node_retention(node_pk,deleting) VALUES($1,FALSE)")
                    .bind(node_pk).execute(&mut *transaction).await.map_err(commit_storage)?;
                // 依赖按 Principal 锁定后再写引用行：Marker 缺失只跳过，受信引用缺失即失败。
                for references in marker_references.chunks(400) {
                    let found = $deps(
                        &mut transaction, &principal, "history_markers", "reference", references,
                    ).await?;
                    for reference in found {
                        sqlx::query("INSERT INTO turn_node_markers(node_pk,reference) VALUES($1,$2) ON CONFLICT DO NOTHING")
                            .bind(node_pk).bind(reference).execute(&mut *transaction).await.map_err(commit_storage)?;
                    }
                }
                for (table, column, references) in [
                    ("reversible_redaction_mappings", "reference", &redaction_references),
                    ("native_compactions", "id", &compaction_record_ids),
                ] {
                    for chunk in references.chunks(400) {
                        let found = $deps(
                            &mut transaction, &principal, table, column, chunk,
                        ).await?;
                        if found.len() != chunk.len() {
                            return Err(TurnCommitError::DependencyUnavailable);
                        }
                        for reference in found {
                            let sql = if table == "native_compactions" {
                                "INSERT INTO turn_node_compactions(node_pk,record_id) VALUES($1,$2) ON CONFLICT DO NOTHING"
                            } else {
                                "INSERT INTO turn_node_redaction_mappings(node_pk,reference) VALUES($1,$2) ON CONFLICT DO NOTHING"
                            };
                            sqlx::query(sql)
                                .bind(node_pk).bind(reference).execute(&mut *transaction).await.map_err(commit_storage)?;
                        }
                    }
                }
                if let Some(checkpoint_id) = commit.atomic_checkpoint_id.as_ref() {
                    // SQLite serializes publication and collection with its write transaction.
                    // PostgreSQL uses the lease and checkpoint row locks below.
                    if !$checkpoint_lock.is_empty() {
                        sqlx::query($checkpoint_lock)
                            .execute(&mut *transaction).await.map_err(commit_storage)?;
                    }
                    let lease_sql = concat!(
                        "SELECT l.expires_at_ms FROM vendor_atomic_leases l ",
                        "JOIN vendor_atomic_checkpoints c ON c.principal=l.principal AND c.admission_id=l.admission_id AND c.lease_id=l.lease_id ",
                        "WHERE c.principal=$1 AND c.checkpoint_id=$2", $lineage_lock
                    );
                    let expires_at: Option<i64> = sqlx::query_scalar(lease_sql)
                        .bind(&principal).bind(checkpoint_id)
                        .fetch_optional(&mut *transaction).await.map_err(commit_storage)?;
                    if !expires_at.is_some_and(|expiry| expiry > chrono::Utc::now().timestamp_millis()) {
                        return Err(TurnCommitError::AtomicStateUnavailable);
                    }
                    let found = $deps(
                        &mut transaction, &principal, "vendor_atomic_checkpoints", "checkpoint_id",
                        std::slice::from_ref(checkpoint_id),
                    ).await?;
                    if found.is_empty() {
                        return Err(TurnCommitError::AtomicStateUnavailable);
                    }
                    sqlx::query("INSERT INTO turn_node_atomic_checkpoints(node_pk,principal,checkpoint_id) VALUES($1,$2,$3)")
                        .bind(node_pk).bind(&principal).bind(checkpoint_id)
                        .execute(&mut *transaction).await.map_err(commit_storage)?;
                }
                content::$put(&mut transaction, node_pk, scope, &mut encoded.contents).await.map_err(commit_storage)?;
                transaction.commit().await.map_err(commit_storage)?;
            }};
        }
        match &self.backend {
            Backend::Sqlite(pool) => {
                persist!(
                    pool,
                    "",
                    "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                    "",
                    crate::content_store::sqlite::resolve_scope,
                    dependency_references_sqlite,
                    put_sqlite
                )
            }
            Backend::Postgres(pool) => persist!(
                pool,
                "LOCK TABLE turn_nodes IN ROW EXCLUSIVE MODE",
                "",
                " FOR UPDATE OF l",
                crate::content_store::postgres::resolve_scope,
                dependency_references_postgres,
                put_postgres
            ),
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
        macro_rules! select {
            ($pool:expr, $db:ty, $values:literal) => {{
                let mut builder = sqlx::QueryBuilder::<$db>::new(
                    "SELECT n.id,n.prefix_item_count,n.prefix_completed_at FROM turn_nodes n \
                     WHERE n.scope_id=(SELECT id FROM content_scopes WHERE principal=",
                );
                builder.push_bind(&principal).push(") AND n.kind=").push_bind(kind.as_str())
                    .push(" AND prefix_namespace=").push_bind(&query.namespace)
                    .push(" AND NOT EXISTS(SELECT 1 FROM turn_node_retention r WHERE r.node_pk=n.node_pk AND r.deleting)")
                    .push(" AND (prefix_fingerprint,prefix_item_count) IN (").push($values);
                for (index, (fingerprint, count)) in query.fingerprints.iter().enumerate() {
                    if index > 0 { builder.push(","); }
                    builder.push("(").push_bind(fingerprint).push(",").push_bind(i64::from(*count)).push(")");
                }
                builder.push(")");
                if let Some(proof) = query.projection.as_ref().filter(|proof| proof.version == REUSABLE_PREFIX_PROJECTION_VERSION) {
                    // 不完整或未知版本的投影仍进入权威读取，不能凭缺失派生值排除历史。
                    builder.push(" AND (prefix_projection_version IS NULL OR prefix_projection_version<>")
                        .push_bind(i64::from(REUSABLE_PREFIX_PROJECTION_VERSION))
                        .push(" OR prefix_controls_fingerprint IS NULL OR prefix_context_fingerprint IS NULL OR (prefix_controls_fingerprint=")
                        .push_bind(&proof.controls_fingerprint);
                    if proof.context_fingerprints.is_empty() {
                        builder.push(" AND 1=0");
                    } else {
                        builder.push(" AND (prefix_context_fingerprint,prefix_item_count) IN (").push($values);
                        for (index, (fingerprint, count)) in proof.context_fingerprints.iter().enumerate() {
                            if index > 0 { builder.push(","); }
                            builder.push("(").push_bind(fingerprint).push(",").push_bind(i64::from(*count)).push(")");
                        }
                        builder.push(")");
                    }
                    builder.push("))");
                }
                builder.push(" ORDER BY prefix_item_count DESC,prefix_completed_at DESC,n.id DESC");
                builder.build_query_as::<(String,i64,i64)>().fetch_all($pool).await
            }};
        }
        let rows = match &self.backend {
            Backend::Sqlite(pool) => select!(pool, sqlx::Sqlite, "VALUES "),
            Backend::Postgres(pool) => select!(pool, sqlx::Postgres, ""),
        }
        .map_err(storage)?;
        tracing::Span::current().record("candidate_count", rows.len() as u64);
        rows.into_iter()
            .map(|(id, count, completed_at)| {
                Ok(ReusablePrefixCandidate {
                    node_id: TurnNodeId::new(id),
                    item_count: u32::try_from(count).map_err(storage)?,
                    completed_at,
                })
            })
            .collect()
    }

    async fn rebuild_prefixes(
        &self,
        namespace_prefix: &str,
        decode: &mut (
                 dyn FnMut(TurnNode, Option<i64>) -> Result<Option<ReusablePrefixMetadata>, String>
                     + Send
             ),
    ) -> Result<(), TurnUnavailable> {
        let stale = format!("{namespace_prefix}%");
        macro_rules! rebuild {
            ($pool:expr, $fetch:ident, $restore:ident, $clear:ident, $update:ident, $lock:literal) => {{
                let has_stale: bool = sqlx::query_scalar(PREFIX_REBUILD_STALE_EXISTS_SQL)
                    .bind(&stale)
                    .fetch_one($pool)
                    .await
                    .map_err(storage)?;
                if !has_stale {
                    return Ok(());
                }
                let mut transaction = $pool.begin_write().await.map_err(storage)?;
                if !$lock.is_empty() {
                    sqlx::query($lock)
                        .execute(&mut *transaction)
                        .await
                        .map_err(storage)?;
                }
                let mut nodes: Vec<PrefixRebuildNode> =
                    sqlx::query_as(PREFIX_REBUILD_ANCESTORS_SQL)
                        .bind(&stale)
                        .fetch_all(&mut *transaction)
                        .await
                        .map_err(storage)?;
                let order = prefix_rebuild_order(&mut nodes);
                let unavailable: Vec<i64> = nodes
                    .iter()
                    .filter(|node| node.stale != 0 && node.state == PrefixRebuildVisit::Unavailable)
                    .map(|node| node.node_pk)
                    .collect();
                if !unavailable.is_empty() {
                    $clear(&mut transaction, &unavailable, None).await?;
                }
                let mut cache = content::UpgradeContentCache::default();
                for chunk in order.chunks(content::UPGRADE_NODE_BATCH) {
                    let stored = $fetch(&mut transaction, &nodes, chunk).await?;
                    let restored =
                        content::$restore(&mut transaction, stored, Some(&mut cache), None)
                            .await
                            .map_err(storage)?;
                    let mut to_clear = Vec::new();
                    let mut to_update = Vec::new();
                    for (&index, node) in chunk.iter().zip(restored) {
                        let metadata = &nodes[index];
                        let prefix = decode(node, metadata.completed_at)
                            .map_err(TurnUnavailable::Storage)?;
                        if metadata.stale == 0 {
                            continue;
                        }
                        let Some(prefix) = prefix else {
                            to_clear.push(metadata.node_pk);
                            continue;
                        };
                        let proof = prefix.projection.as_ref();
                        to_update.push(PrefixUpdateRow {
                            node_pk: metadata.node_pk,
                            namespace: prefix.namespace,
                            fingerprint: prefix.fingerprint,
                            item_count: i64::from(prefix.item_count),
                            projection_version: proof.map(|p| i64::from(p.version)),
                            controls_fingerprint: proof.map(|p| p.controls_fingerprint.clone()),
                            context_fingerprint: proof.map(|p| p.context_fingerprint.clone()),
                        });
                    }
                    if !to_clear.is_empty() {
                        $clear(&mut transaction, &to_clear, None).await?;
                    }
                    if !to_update.is_empty() {
                        $update(&mut transaction, &to_update, None).await?;
                    }
                }
                transaction.commit().await.map_err(storage)?;
            }};
        }
        match &self.backend {
            Backend::Sqlite(pool) => rebuild!(
                pool,
                fetch_rebuild_batch_sqlite,
                restore_upgrade_cached_sqlite,
                clear_prefixes_batch_sqlite,
                update_prefixes_batch_sqlite,
                ""
            ),
            Backend::Postgres(pool) => rebuild!(
                pool,
                fetch_rebuild_batch_postgres,
                restore_upgrade_cached_postgres,
                clear_prefixes_batch_postgres,
                update_prefixes_batch_postgres,
                "LOCK TABLE turn_nodes IN ROW EXCLUSIVE MODE"
            ),
        }
        Ok(())
    }

    async fn sweep_history(&self, now: i64) -> Result<u64, TurnUnavailable> {
        super::gc::sweep(self, now).await.map_err(storage)
    }
}
