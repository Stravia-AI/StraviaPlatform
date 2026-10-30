use super::*;

use std::collections::HashMap;

#[derive(Clone)]
pub enum SqlTurnChainStore {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

fn storage(error: impl std::fmt::Display) -> TurnUnavailable {
    TurnUnavailable::Storage(error.to_string())
}

fn commit_storage(error: impl std::fmt::Display) -> TurnCommitError {
    TurnCommitError::Storage(error.to_string())
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

impl StoredAncestorRow {
    fn expiry(&self) -> Result<i64, TurnUnavailable> {
        self.expires_at
            .ok_or_else(|| storage("missing history retention"))
    }

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

macro_rules! retain_parent {
    ($name:ident, $conn:ty, $db:ty) => {
        /// 调用者持有写事务；PostgreSQL 还须先持有 turn_nodes ROW EXCLUSIVE，
        /// 与 GC 的锁序一致，保证资格检查与续期之间不会物理删除祖先。
        pub(crate) async fn $name(
            connection: &mut $conn,
            principal: &str,
            kind: TurnNodeKind,
            parent: &str,
            now: i64,
            expires_at: i64,
        ) -> Result<i64, TurnCommitError> {
            let rows: Vec<(i64, Option<i64>, Option<i64>, i64)> = sqlx::query_as(
                "WITH RECURSIVE seed AS (\
                 SELECT n.node_pk,n.parent_pk,n.scope_id,n.kind,r.expires_at FROM turn_nodes n \
                 JOIN turn_scopes s ON s.id=n.scope_id \
                 LEFT JOIN turn_node_retention r ON r.node_pk=n.node_pk \
                 WHERE n.id=$1 AND s.principal=$2 AND n.kind=$3 AND (r.node_pk IS NULL OR NOT r.deleting)\
                 ), ancestors(node_pk,parent_pk,scope_id,kind,expires_at) AS (\
                 SELECT node_pk,parent_pk,scope_id,kind,expires_at FROM seed UNION \
                 SELECT n.node_pk,n.parent_pk,n.scope_id,n.kind,r.expires_at FROM ancestors a \
                 CROSS JOIN turn_nodes n LEFT JOIN turn_node_retention r ON r.node_pk=n.node_pk \
                 WHERE n.node_pk=a.parent_pk AND n.scope_id=a.scope_id AND n.kind=a.kind \
                 AND (r.node_pk IS NULL OR NOT r.deleting)) \
                 SELECT a.node_pk,a.parent_pk,a.expires_at,(SELECT node_pk FROM seed) FROM ancestors a",
            )
            .bind(parent).bind(principal).bind(kind.as_str())
            .fetch_all(&mut *connection).await.map_err(commit_storage)?;
            let head = rows.first().map(|row| row.3).ok_or(TurnCommitError::ParentUnavailable)?;
            let mut head_live = false;
            let mut complete = false;
            let mut extend = Vec::new();
            for (node_pk, parent_pk, expiry, _) in rows {
                let expiry = expiry.ok_or_else(|| commit_storage("missing history retention"))?;
                head_live |= node_pk == head && expiry > now;
                complete |= parent_pk.is_none();
                if expiry < expires_at { extend.push(node_pk); }
            }
            if !head_live || !complete { return Err(TurnCommitError::ParentUnavailable); }
            for nodes in extend.chunks(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new("UPDATE turn_node_retention SET expires_at=");
                query.push_bind(expires_at).push(" WHERE node_pk IN (");
                let mut list = query.separated(",");
                for node_pk in nodes { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                query.push(" AND NOT deleting AND expires_at<").push_bind(expires_at);
                query.build().execute(&mut *connection).await.map_err(commit_storage)?;
            }
            Ok(head)
        }
    };
}

impl SqlTurnChainStore {
    pub fn sqlite(pool: SqlitePool) -> Self {
        Self::Sqlite(pool)
    }
    pub fn postgres(pool: PgPool) -> Self {
        Self::Postgres(pool)
    }

    retain_parent!(retain_sqlite, sqlx::SqliteConnection<'_>, sqlx::Sqlite);
    retain_parent!(retain_postgres, sqlx::PgConnection<'_>, sqlx::Postgres);

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
                    "WITH RECURSIVE ancestors(node_pk,parent_pk,scope_id,kind,expires_at) AS (\
                     SELECT n.node_pk,n.parent_pk,n.scope_id,n.kind,r.expires_at FROM turn_nodes n \
                     JOIN turn_scopes s ON s.id=n.scope_id \
                     LEFT JOIN turn_node_retention r ON r.node_pk=n.node_pk WHERE n.id=",
                );
                query.push_bind(id.as_str()).push(" AND s.principal=").push_bind(&principal)
                    .push(" AND n.kind=").push_bind(kind.as_str())
                    .push(" AND (r.node_pk IS NULL OR NOT r.deleting) \
                           UNION SELECT n.node_pk,n.parent_pk,n.scope_id,n.kind,r.expires_at FROM ancestors a \
                           CROSS JOIN turn_nodes n LEFT JOIN turn_node_retention r ON r.node_pk=n.node_pk \
                           WHERE n.node_pk=a.parent_pk AND n.scope_id=a.scope_id AND n.kind=a.kind \
                           AND (r.node_pk IS NULL OR NOT r.deleting)");
                if let Some(known) = known {
                    query.push($known_start).push_bind(known).push($known_end);
                }
                query.push(") SELECT n.node_pk,n.scope_id,n.parent_pk,n.id,parent.id AS parent_id,n.payload_version,\
                            p.payload,a.expires_at,").push(content::reference_projection!($backend))
                    .push(" FROM ancestors a JOIN turn_nodes n ON n.node_pk=a.node_pk \
                            LEFT JOIN turn_nodes parent ON parent.node_pk=n.parent_pk \
                            LEFT JOIN turn_node_payloads p ON p.node_pk=n.node_pk");
                query.build_query_as::<StoredAncestorRow>().fetch_all($pool).await
            }};
        }
        let rows = match self {
            Self::Sqlite(pool) => select!(
                pool,
                sqlx::Sqlite,
                sqlite,
                " AND n.id NOT IN (SELECT value FROM json_each(",
                "))"
            ),
            Self::Postgres(pool) => select!(
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
        for row in &rows {
            if row.expiry()? <= now {
                return Err(TurnUnavailable::Unavailable);
            }
        }
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
                expires_at: deadline_from_unix_millis(rows[0].expiry()?, now),
            },
        };
        let deadlines = rows
            .iter()
            .map(|row| {
                row.expiry()
                    .map(|expiry| deadline_from_unix_millis(expiry, now))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let nodes = rows
            .into_iter()
            .map(|row| row.into_stored(kind))
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
                    &mut *pool.acquire().await.map_err(storage)?,
                    nodes,
                    Some(context),
                    finish,
                )
                .await
            }
            Self::Postgres(pool) => {
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
        for node in nodes {
            prefix.expires_at = prefix.expires_at.min(node.expires_at);
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
        let rows = self.select_ancestors(principal, kind, id, None).await?;
        tracing::Span::current().record("node_count", rows.len() as u64);
        if rows.first().is_none_or(|row| row.parent_pk.is_some()) {
            return Err(TurnUnavailable::Unavailable);
        }
        let now = chrono::Utc::now().timestamp_millis();
        let mut minimum = i64::MAX;
        for row in &rows {
            let expiry = row.expiry()?;
            if expiry <= now {
                return Err(TurnUnavailable::Unavailable);
            }
            minimum = minimum.min(expiry);
        }
        let expires_at = deadline_from_unix_millis(minimum, now);
        let nodes = rows
            .into_iter()
            .map(|row| row.into_stored(kind))
            .collect::<Result<Vec<_>, _>>()?;
        let nodes = match self {
            Self::Sqlite(pool) => {
                content::restore_sqlite(&mut *pool.acquire().await.map_err(storage)?, nodes).await
            }
            Self::Postgres(pool) => {
                content::restore_postgres(&mut *pool.acquire().await.map_err(storage)?, nodes).await
            }
        }
        .map_err(storage)?;
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
        let expires_at = unix_millis_after(commit.idle_ttl);
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
            let ids = match self {
                Self::Sqlite(pool) => hint!(pool, lookup_sqlite),
                Self::Postgres(pool) => hint!(pool, lookup_postgres),
            };
            content::prepare_missing(&mut encoded.contents, &ids)
                .await
                .map_err(commit_storage)?;
        }
        // descriptor 不含数据库分配的 ID，编码和大正文压缩都在写协调之前完成。
        let payload = std::mem::take(&mut encoded.payload);
        let prefix = commit.reusable_prefix.as_ref();
        let projection = prefix.and_then(|prefix| prefix.projection.as_ref());
        macro_rules! persist {
            ($pool:expr, $begin:literal, $lock:literal, $scope_lock:literal, $retain:ident, $put:ident) => {{
                let mut connection = $pool.acquire_write().await.map_err(commit_storage)?;
                let mut transaction = connection.begin_with($begin).await.map_err(commit_storage)?;
                if !$lock.is_empty() { sqlx::query($lock).execute(&mut *transaction).await.map_err(commit_storage)?; }
                let scope_sql = concat!("SELECT id FROM turn_scopes WHERE principal=$1", $scope_lock);
                let (duplicate, scope): (Option<i64>, Option<i64>) = sqlx::query_as(concat!(
                    "SELECT (SELECT node_pk FROM turn_nodes WHERE id=$1), ",
                    "(SELECT id FROM turn_scopes WHERE principal=$2", $scope_lock, ")",
                )).bind(commit.id.as_str()).bind(&principal)
                    .fetch_one(&mut *transaction).await.map_err(commit_storage)?;
                if duplicate.is_some() { return Err(TurnCommitError::AlreadyExists); }
                let scope = match scope {
                    Some(scope) => scope,
                    None => {
                        let inserted: Option<i64> = sqlx::query_scalar(
                            "INSERT INTO turn_scopes(principal) VALUES($1) ON CONFLICT(principal) DO NOTHING RETURNING id",
                        ).bind(&principal).fetch_optional(&mut *transaction).await.map_err(commit_storage)?;
                        match inserted {
                            Some(scope) => scope,
                            None => sqlx::query_scalar(scope_sql).bind(&principal)
                                .fetch_one(&mut *transaction).await.map_err(commit_storage)?,
                        }
                    }
                };
                let parent_pk = match commit.parent_id.as_ref() {
                    Some(parent) => Some(Self::$retain(
                        &mut transaction, &principal, commit.kind, parent.as_str(), now, expires_at,
                    ).await?),
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
                sqlx::query("INSERT INTO turn_node_retention(node_pk,expires_at) VALUES($1,$2)")
                    .bind(node_pk).bind(expires_at).execute(&mut *transaction).await.map_err(commit_storage)?;
                content::$put(&mut transaction, node_pk, scope, &mut encoded.contents).await.map_err(commit_storage)?;
                transaction.commit().await.map_err(commit_storage)?;
                connection.finish().await.map_err(commit_storage)?;
            }};
        }
        match self {
            Self::Sqlite(pool) => {
                persist!(pool, "BEGIN IMMEDIATE", "", "", retain_sqlite, put_sqlite)
            }
            Self::Postgres(pool) => persist!(
                pool,
                "BEGIN",
                "LOCK TABLE turn_nodes,turn_contents IN ROW EXCLUSIVE MODE",
                " FOR KEY SHARE",
                retain_postgres,
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
        let now = chrono::Utc::now().timestamp_millis();
        macro_rules! select {
            ($pool:expr, $db:ty, $values:literal) => {{
                let mut builder = sqlx::QueryBuilder::<$db>::new(
                    "SELECT n.id,n.prefix_item_count,n.prefix_completed_at FROM turn_nodes n \
                     JOIN turn_node_retention r ON r.node_pk=n.node_pk \
                     WHERE n.scope_id=(SELECT id FROM turn_scopes WHERE principal=",
                );
                builder.push_bind(&principal).push(") AND n.kind=").push_bind(kind.as_str())
                    .push(" AND prefix_namespace=").push_bind(&query.namespace)
                    .push(" AND NOT r.deleting AND r.expires_at>").push_bind(now)
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
        let rows = match self {
            Self::Sqlite(pool) => select!(pool, sqlx::Sqlite, "VALUES "),
            Self::Postgres(pool) => select!(pool, sqlx::Postgres, ""),
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
        macro_rules! rebuild {
            ($pool:expr, $restore:ident, $backend:ident, $lock:literal) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage)?;
                let mut transaction = connection.begin().await.map_err(storage)?;
                if !$lock.is_empty() { sqlx::query($lock).execute(&mut *transaction).await.map_err(storage)?; }
                let now = chrono::Utc::now().timestamp_millis();
                let stale = format!("{namespace_prefix}%");
                let mut nodes: Vec<PrefixRebuildNode> = sqlx::query_as(
                    "WITH RECURSIVE ancestors(node_pk,scope_id,parent_pk,completed_at,stale) AS (\
                     SELECT n.node_pk,n.scope_id,n.parent_pk,n.prefix_completed_at,1 FROM turn_nodes n \
                     JOIN turn_node_retention r ON r.node_pk=n.node_pk WHERE n.kind='response' \
                     AND n.prefix_namespace IS NOT NULL AND n.prefix_namespace NOT LIKE $1 AND r.expires_at>$2 AND NOT r.deleting \
                     UNION SELECT n.node_pk,n.scope_id,n.parent_pk,\
                     CASE WHEN n.prefix_namespace IS NOT NULL THEN n.prefix_completed_at ELSE NULL END,\
                     CASE WHEN n.prefix_namespace IS NOT NULL AND n.prefix_namespace NOT LIKE $1 THEN 1 ELSE 0 END \
                     FROM ancestors a JOIN turn_nodes n ON n.node_pk=a.parent_pk AND n.scope_id=a.scope_id \
                     JOIN turn_node_retention r ON r.node_pk=n.node_pk \
                     WHERE n.kind='response' AND r.expires_at>$2 AND NOT r.deleting) \
                     SELECT n.node_pk,n.id,n.scope_id,n.parent_pk,p.id AS parent_id,a.completed_at,a.stale \
                     FROM ancestors a JOIN turn_nodes n ON n.node_pk=a.node_pk \
                     LEFT JOIN turn_nodes p ON p.node_pk=n.parent_pk",
                ).bind(&stale).bind(now).fetch_all(&mut *transaction).await.map_err(storage)?;
                let order = prefix_rebuild_order(&mut nodes);
                for node in nodes.iter().filter(|node| node.stale != 0 && node.state == PrefixRebuildVisit::Unavailable) {
                    sqlx::query("UPDATE turn_nodes SET prefix_namespace=NULL,prefix_fingerprint=NULL,prefix_item_count=NULL,prefix_completed_at=NULL,prefix_projection_version=NULL,prefix_controls_fingerprint=NULL,prefix_context_fingerprint=NULL WHERE node_pk=$1")
                        .bind(node.node_pk).execute(&mut *transaction).await.map_err(storage)?;
                }
                for index in order {
                    let metadata = &nodes[index];
                    let (version,payload,references): (i64,Option<Vec<u8>>,sqlx::types::Json<Vec<[i64; 3]>>) = sqlx::query_as(
                        concat!("SELECT n.payload_version,p.payload,",content::reference_projection!($backend),
                        " FROM turn_nodes n LEFT JOIN turn_node_payloads p ON p.node_pk=n.node_pk WHERE n.node_pk=$1"),
                    ).bind(metadata.node_pk).fetch_one(&mut *transaction).await.map_err(storage)?;
                    let node = content::StoredNode {
                        node_pk: metadata.node_pk,
                        scope_id: metadata.scope_id,
                        references: references.0,
                        id: TurnNodeId::new(metadata.id.clone()),
                        kind: TurnNodeKind::Response,
                        parent_id: metadata.parent_id.as_ref().map(TurnNodeId::new),
                        payload_version: u32::try_from(version).map_err(storage)?,
                        payload: payload.ok_or_else(|| storage("missing history payload"))?,
                    };
                    let restored = content::$restore(&mut transaction, vec![node]).await.map_err(storage)?;
                    let node = restored.into_iter().next().expect("restored node");
                    let prefix = decode(node, metadata.completed_at).map_err(TurnUnavailable::Storage)?;
                    if metadata.stale == 0 { continue; }
                    let Some(prefix) = prefix else {
                        sqlx::query("UPDATE turn_nodes SET prefix_namespace=NULL,prefix_fingerprint=NULL,prefix_item_count=NULL,prefix_completed_at=NULL,prefix_projection_version=NULL,prefix_controls_fingerprint=NULL,prefix_context_fingerprint=NULL WHERE node_pk=$1")
                            .bind(metadata.node_pk).execute(&mut *transaction).await.map_err(storage)?;
                        continue;
                    };
                    let proof = prefix.projection.as_ref();
                    sqlx::query("UPDATE turn_nodes SET prefix_namespace=$1,prefix_fingerprint=$2,prefix_item_count=$3,prefix_projection_version=$5,prefix_controls_fingerprint=$6,prefix_context_fingerprint=$7 WHERE node_pk=$4")
                        .bind(&prefix.namespace).bind(&prefix.fingerprint).bind(i64::from(prefix.item_count)).bind(metadata.node_pk)
                        .bind(proof.map(|proof| i64::from(proof.version)))
                        .bind(proof.map(|proof| proof.controls_fingerprint.as_str()))
                        .bind(proof.map(|proof| proof.context_fingerprint.as_str()))
                        .execute(&mut *transaction).await.map_err(storage)?;
                }
                transaction.commit().await.map_err(storage)?;
                connection.finish().await.map_err(storage)?;
            }};
        }
        match self {
            Self::Sqlite(pool) => rebuild!(pool, restore_sqlite, sqlite, ""),
            Self::Postgres(pool) => rebuild!(
                pool,
                restore_postgres,
                postgres,
                "LOCK TABLE turn_nodes,turn_contents IN ROW EXCLUSIVE MODE"
            ),
        }
        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable> {
        super::gc::sweep(self).await.map_err(storage)
    }
}

#[cfg(test)]
impl SqlTurnChainStore {
    /// 只供旧领域 payload fixture 使用；每次读写一个节点，不依赖内部 descriptor 布局。
    pub(crate) async fn rewrite_payloads(
        &self,
        mut edit: impl FnMut(&mut serde_json::Value),
    ) -> anyhow::Result<()> {
        macro_rules! rewrite {
            ($pool:expr, $put:ident, $restore:ident, $backend:ident, $lock:literal) => {{
                let nodes: Vec<(i64,String,i64,String)> = sqlx::query_as(
                    "SELECT node_pk,id,scope_id,kind FROM turn_nodes",
                ).fetch_all($pool).await?;
                for (node_pk,id,scope,kind) in nodes {
                    let kind = match kind.as_str() {
                        "response" => TurnNodeKind::Response,
                        "agent" => TurnNodeKind::Agent,
                        "web_search" => TurnNodeKind::WebSearch,
                        _ => anyhow::bail!("invalid history kind"),
                    };
                    let (version,parent,payload,references): (i64,Option<String>,Vec<u8>,sqlx::types::Json<Vec<[i64; 3]>>) = sqlx::query_as(
                        concat!("SELECT n.payload_version,parent.id,p.payload,",content::reference_projection!($backend),
                        " FROM turn_nodes n \
                         JOIN turn_node_payloads p ON p.node_pk=n.node_pk \
                         LEFT JOIN turn_nodes parent ON parent.node_pk=n.parent_pk WHERE n.node_pk=$1"),
                    ).bind(node_pk).fetch_one($pool).await?;
                    let node = content::StoredNode { node_pk,scope_id:scope,references:references.0,id:TurnNodeId::new(id),kind,
                        parent_id:parent.map(TurnNodeId::new),payload_version:u32::try_from(version)?,payload };
                    let restored = {
                        let mut connection = $pool.acquire().await?;
                        content::$restore(&mut connection, vec![node]).await?
                    };
                    let mut payload = restored.into_iter().next().expect("restored node").payload;
                    edit(&mut payload);
                    let mut encoded = tokio::task::spawn_blocking(move || content::encode(payload)).await??;
                    let mut connection = $pool.acquire_write().await?;
                    let mut transaction = connection.begin().await?;
                    if !$lock.is_empty() { sqlx::query($lock).execute(&mut *transaction).await?; }
                    sqlx::query("INSERT INTO turn_content_gc(content_id) SELECT content_id FROM turn_node_contents WHERE node_pk=$1 ON CONFLICT(content_id) DO NOTHING")
                        .bind(node_pk).execute(&mut *transaction).await?;
                    sqlx::query("DELETE FROM turn_node_contents WHERE node_pk=$1").bind(node_pk).execute(&mut *transaction).await?;
                    content::$put(&mut transaction, node_pk, scope, &mut encoded.contents).await?;
                    sqlx::query("UPDATE turn_node_payloads SET payload=$1 WHERE node_pk=$2")
                        .bind(encoded.payload).bind(node_pk).execute(&mut *transaction).await?;
                    transaction.commit().await?;
                    connection.finish().await?;
                }
                anyhow::Ok(())
            }};
        }
        match self {
            Self::Sqlite(pool) => rewrite!(pool, put_sqlite, restore_sqlite, sqlite, ""),
            Self::Postgres(pool) => rewrite!(
                pool,
                put_postgres,
                restore_postgres,
                postgres,
                "LOCK TABLE turn_nodes,turn_contents IN ROW EXCLUSIVE MODE"
            ),
        }
    }
}
