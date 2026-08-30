use super::*;
use sqlx::Connection;

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
    payload: String,
) -> Result<TurnNode, TurnUnavailable> {
    let payload_version = u32::try_from(payload_version)
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
    let payload = serde_json::from_str(&payload)
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

    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<MaterializedTurnChain, TurnUnavailable> {
        let principal = principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let rows: Vec<(String, Option<String>, i64, String, i64, i64)> = match self {
            Self::Sqlite(pool) => {
                sqlx::query_as(
                    "WITH RECURSIVE ancestors(id, parent_id, payload_version, payload, expires_at, depth) AS (\
                     SELECT id, parent_id, payload_version, payload, expires_at, 0 \
                     FROM turn_chain_nodes \
                     WHERE id = ? AND principal = ? AND kind = ? \
                     UNION ALL \
                     SELECT node.id, node.parent_id, node.payload_version, node.payload, node.expires_at, ancestors.depth + 1 \
                     FROM turn_chain_nodes node \
                     JOIN ancestors ON node.id = ancestors.parent_id \
                     WHERE node.principal = ? AND node.kind = ?\
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
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
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
        Ok(MaterializedTurnChain { nodes, expires_at })
    }

    async fn commit(&self, commit: TurnCommit) -> Result<TurnNodeId, TurnCommitError> {
        let principal = commit.principal.continuation_key();
        let now = chrono::Utc::now().timestamp_millis();
        let expires_at = unix_millis_after(commit.idle_ttl);
        let payload = serde_json::to_string(&commit.payload)
            .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
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
            Self::Sqlite(pool) => {
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
                    let updated = sqlx::query(
                        "WITH RECURSIVE ancestors(id, parent_id, depth) AS (\
                         SELECT id, parent_id, 0 FROM turn_chain_nodes \
                         WHERE id = ? AND principal = ? AND kind = ? AND expires_at > ? \
                         UNION ALL \
                         SELECT node.id, node.parent_id, ancestors.depth + 1 FROM turn_chain_nodes node \
                         JOIN ancestors ON node.id = ancestors.parent_id \
                         WHERE node.principal = ? AND node.kind = ?\
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
                      prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(commit.id.as_str())
                .bind(commit.kind.as_str())
                .bind(commit.parent_id.as_ref().map(TurnNodeId::as_str))
                .bind(&principal)
                .bind(payload_version)
                .bind(&payload)
                .bind(now)
                .bind(expires_at)
                .bind(prefix_namespace)
                .bind(prefix_fingerprint)
                .bind(prefix_item_count)
                .bind(prefix_completed_at)
                .execute(&mut *transaction)
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
                      prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                )
                .bind(commit.id.as_str())
                .bind(commit.kind.as_str())
                .bind(commit.parent_id.as_ref().map(TurnNodeId::as_str))
                .bind(&principal)
                .bind(payload_version)
                .bind(&payload)
                .bind(now)
                .bind(expires_at)
                .bind(prefix_namespace)
                .bind(prefix_fingerprint)
                .bind(prefix_item_count)
                .bind(prefix_completed_at)
                .execute(&mut *transaction)
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
                    .push(" AND (");
                for (index, (fingerprint, item_count)) in query.fingerprints.iter().enumerate() {
                    if index > 0 {
                        builder.push(" OR ");
                    }
                    builder
                        .push("(prefix_fingerprint = ")
                        .push_bind(fingerprint)
                        .push(" AND prefix_item_count = ")
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
                    .push(" AND (");
                for (index, (fingerprint, item_count)) in query.fingerprints.iter().enumerate() {
                    if index > 0 {
                        builder.push(" OR ");
                    }
                    builder
                        .push("(prefix_fingerprint = ")
                        .push_bind(fingerprint)
                        .push(" AND prefix_item_count = ")
                        .push_bind(i64::from(*item_count))
                        .push(")");
                }
                builder
                    .push(") ORDER BY prefix_item_count DESC, prefix_completed_at DESC, id DESC");
                builder.build_query_as().fetch_all(pool).await
            }
        }
        .map_err(|error| TurnUnavailable::Storage(error.to_string()))?;
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

    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut removed = 0_u64;
        loop {
            let rows = match self {
                Self::Sqlite(pool) => sqlx::query(
                    "DELETE FROM turn_chain_nodes WHERE expires_at <= ? \
                     AND NOT EXISTS (SELECT 1 FROM turn_chain_nodes child \
                     WHERE child.parent_id = turn_chain_nodes.id)",
                )
                .bind(now)
                .execute(pool)
                .await
                .map(|result| result.rows_affected()),
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
                return Ok(removed);
            }
        }
    }
}
