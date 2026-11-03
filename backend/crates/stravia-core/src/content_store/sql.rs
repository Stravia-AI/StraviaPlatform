//! 共享正文存储的后端 SQL。两个后端行为一致；锁协议按计划区分：
//! PostgreSQL 用行锁（复用 `FOR KEY SHARE`，GC `FOR UPDATE SKIP LOCKED`），
//! SQLite 用共享写租约与 `BEGIN IMMEDIATE`。

use super::*;

const CONTENT_BATCH: usize = 512;
const CONTENT_BYTES: i64 = 8 * 1024 * 1024;

/// 四类反向引用都必须重检；新增引用类型时同步修改这里与 [`enqueue_unreferenced`]。
macro_rules! unreferenced {
    ($c:literal) => {
        concat!(
            "NOT EXISTS(SELECT 1 FROM turn_node_contents r WHERE r.content_id=", $c, ")",
            " AND NOT EXISTS(SELECT 1 FROM history_marker_contents r WHERE r.content_id=", $c, ")",
            " AND NOT EXISTS(SELECT 1 FROM observation_event_contents r WHERE r.content_id=", $c, ")",
            " AND NOT EXISTS(SELECT 1 FROM observation_tool_results r WHERE r.content_id=", $c, ")"
        )
    };
}

macro_rules! backend {
    (
        $module:ident,
        $pool:ty,
        $db:ty,
        $conn:ty,
        $begin:literal,
        $key_share:literal,
        $claim_before:literal,
        $claim_after:literal
    ) => {
        pub(crate) mod $module {
            use super::*;

            /// 取得 Principal 的 scope；不存在时插入。调用方持有写事务。
            pub(crate) async fn resolve_scope(
                connection: &mut $conn,
                principal: &str,
            ) -> anyhow::Result<i64> {
                let scope: Option<i64> = {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id FROM content_scopes WHERE principal = ",
                    );
                    query.push_bind(principal);
                    query.push($key_share);
                    query.build_query_scalar().fetch_optional(&mut *connection).await?
                };
                if let Some(scope) = scope {
                    return Ok(scope);
                }
                let inserted: Option<i64> = {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "INSERT INTO content_scopes(principal) VALUES(",
                    );
                    query.push_bind(principal);
                    query.push(") ON CONFLICT(principal) DO NOTHING RETURNING id");
                    query.build_query_scalar().fetch_optional(&mut *connection).await?
                };
                match inserted {
                    Some(scope) => Ok(scope),
                    None => {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "SELECT id FROM content_scopes WHERE principal = ",
                        );
                        query.push_bind(principal);
                        query
                            .build_query_scalar()
                            .fetch_one(&mut *connection)
                            .await
                            .map_err(anyhow::Error::from)
                    }
                }
            }

            /// 只读的 scope 查询。
            pub(crate) async fn scope_id(
                connection: &mut $conn,
                principal: &str,
            ) -> anyhow::Result<Option<i64>> {
                let mut query =
                    sqlx::QueryBuilder::<$db>::new("SELECT id FROM content_scopes WHERE principal = ");
                query.push_bind(principal);
                query
                    .build_query_scalar()
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(anyhow::Error::from)
            }

            /// 存在性探查。没有调用方事务锁时只是提示；`lock` 在行锁后端加共享锁。
            pub(crate) async fn lookup(
                connection: &mut $conn,
                scope: LookupScope<'_>,
                contents: &[Content],
                lock: bool,
            ) -> anyhow::Result<Vec<Option<i64>>> {
                let mut ids = vec![None; contents.len()];
                if contents.is_empty() {
                    return Ok(ids);
                }
                let positions = DigestPositions::new(contents);
                let mut digests: Vec<ContentDigest> = positions.digests().collect();
                digests.sort_unstable();
                for chunk in digests.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, digest FROM contents WHERE scope_id = ",
                    );
                    match scope {
                        LookupScope::Id(id) => {
                            query.push_bind(id);
                        }
                        LookupScope::Principal(principal) => {
                            query.push("(SELECT id FROM content_scopes WHERE principal = ")
                                .push_bind(principal)
                                .push(")");
                        }
                    }
                    query.push(" AND digest IN (");
                    let mut list = query.separated(",");
                    for digest in chunk {
                        list.push_bind(digest.as_slice());
                    }
                    list.push_unseparated(")");
                    // 并发提交按固定摘要顺序取锁。
                    query.push(" ORDER BY digest");
                    if lock {
                        query.push($key_share);
                    }
                    let rows: Vec<(i64, Vec<u8>)> =
                        query.build_query_as().fetch_all(&mut *connection).await?;
                    for (id, digest) in rows {
                        positions.observe(&mut ids, &digest, id)?;
                    }
                }
                Ok(ids)
            }

            /// 事务内权威查找加缺失行插入。按输入位置返回 ID。
            ///
            /// 先复用（`FOR KEY SHARE`），再 `INSERT … ON CONFLICT DO NOTHING RETURNING id`。
            /// 冲突但未返回的行用一条新的加锁查询重读；若该行已被 GC 删除，重读为空，
            /// 重新插入。正确性不依赖捕获外键错误后重试。
            pub(crate) async fn intern(
                connection: &mut $conn,
                scope_id: i64,
                contents: &mut [Content],
            ) -> anyhow::Result<Vec<i64>> {
                let mut ids =
                    lookup(connection, LookupScope::Id(scope_id), contents, true).await?;
                for (content, id) in contents.iter_mut().zip(&ids) {
                    if id.is_some() {
                        content.release();
                    }
                }
                prepare_missing(contents, &ids).await?;
                let positions = DigestPositions::new(contents);

                let mut unresolved: Vec<usize> = ids
                    .iter()
                    .enumerate()
                    .filter_map(|(index, id)| id.is_none().then_some(index))
                    .collect();
                // 有界重试只覆盖「重读后行已被 GC 删除」；不是外键错误重试。
                for _ in 0..4 {
                    if unresolved.is_empty() {
                        break;
                    }
                    // 固定插入顺序：并发写入方按相同摘要顺序取投机插入锁。
                    let mut unique: Vec<usize> = unresolved.clone();
                    unique.sort_unstable_by(|left, right| {
                        contents[*left].digest.cmp(&contents[*right].digest)
                    });
                    unique.dedup_by(|left, right| contents[*left].digest == contents[*right].digest);
                    for chunk in unique.chunks(200) {
                        for index in chunk {
                            ensure!(
                                contents[*index].encoded && !contents[*index].bytes.is_empty(),
                                "missing prepared content"
                            );
                        }
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "INSERT INTO contents (scope_id, digest, format, raw_len, body) ",
                        );
                        query.push_values(chunk.iter(), |mut row, index| {
                            row.push_bind(scope_id)
                                .push_bind(contents[*index].digest.as_slice())
                                .push_bind(contents[*index].format.as_database_value())
                                .push_bind(contents[*index].raw_len)
                                .push_bind(contents[*index].bytes.as_slice());
                        });
                        query.push(
                            " ON CONFLICT (scope_id, digest) DO NOTHING RETURNING id, digest",
                        );
                        let inserted: Vec<(i64, Vec<u8>)> = query
                            .build_query_as()
                            .fetch_all(&mut *connection)
                            .await?;
                        for (id, digest) in inserted {
                            positions.observe(&mut ids, &digest, id)?;
                        }
                    }
                    unresolved = ids
                        .iter()
                        .enumerate()
                        .filter_map(|(index, id)| id.is_none().then_some(index))
                        .collect();
                    if unresolved.is_empty() {
                        break;
                    }
                    // 冲突行由一条新的加锁查询解析；若已被删除，下一轮重新插入。
                    let mut keys: Vec<ContentDigest> = unresolved
                        .iter()
                        .map(|index| contents[*index].digest)
                        .collect();
                    keys.sort_unstable();
                    keys.dedup();
                    for chunk in keys.chunks(400) {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "SELECT id, digest FROM contents WHERE scope_id = ",
                        );
                        query.push_bind(scope_id).push(" AND digest IN (");
                        let mut list = query.separated(",");
                        for digest in chunk {
                            list.push_bind(digest.as_slice());
                        }
                        list.push_unseparated(")");
                        query.push(" ORDER BY digest");
                        query.push($key_share);
                        let rows: Vec<(i64, Vec<u8>)> =
                            query.build_query_as().fetch_all(&mut *connection).await?;
                        for (id, digest) in rows {
                            positions.observe(&mut ids, &digest, id)?;
                        }
                    }
                    unresolved = ids
                        .iter()
                        .enumerate()
                        .filter_map(|(index, id)| id.is_none().then_some(index))
                        .collect();
                }
                let ids = ids
                    .into_iter()
                    .map(|id| id.context("missing content"))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                for content in contents.iter_mut() {
                    content.release();
                }
                Ok(ids)
            }

            /// 批量读取正文并逐条校验身份与长度。
            pub(crate) async fn read(
                connection: &mut $conn,
                scope_id: i64,
                ids: &[i64],
            ) -> anyhow::Result<Vec<Body>> {
                let mut unique: Vec<i64> = ids.to_vec();
                unique.sort_unstable();
                unique.dedup();
                let mut bodies = Vec::with_capacity(unique.len());
                for chunk in unique.chunks(256) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, digest, CAST(format AS BIGINT), raw_len, body FROM contents WHERE scope_id = ",
                    );
                    query.push_bind(scope_id).push(" AND id IN (");
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(*id);
                    }
                    list.push_unseparated(") ORDER BY id");
                    let rows: Vec<(i64, Vec<u8>, i64, i64, Vec<u8>)> =
                        query.build_query_as().fetch_all(&mut *connection).await?;
                    for (id, digest, format, raw_len, encoded) in rows {
                        let format = ContentFormat::from_database_value(format)
                            .context("invalid stored content format")?;
                        let raw = crate::storage_codec::decode(&encoded)?;
                        ensure!(
                            i64::try_from(raw.len()).unwrap_or(i64::MAX) == raw_len,
                            "stored content length mismatch"
                        );
                        ensure!(
                            content_digest(format, &raw) == digest.as_slice(),
                            "damaged content"
                        );
                        bodies.push(Body {
                            id,
                            format,
                            bytes: raw.into_owned(),
                        });
                    }
                }
                Ok(bodies)
            }

            /// 引用方删除引用后，在同一事务内登记失去最后引用的内容。
            pub(crate) async fn enqueue_unreferenced(
                connection: &mut $conn,
                ids: &[i64],
            ) -> anyhow::Result<u64> {
                if ids.is_empty() {
                    return Ok(0);
                }
                let mut unique: Vec<i64> = ids.to_vec();
                unique.sort_unstable();
                unique.dedup();
                let mut enqueued = 0;
                for chunk in unique.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "INSERT INTO content_gc(content_id) SELECT c.id FROM contents c WHERE c.id IN (",
                    );
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(*id);
                    }
                    list.push_unseparated(")");
                    query.push(" AND ");
                    query.push(unreferenced!("c.id"));
                    query.push(" ON CONFLICT(content_id) DO NOTHING");
                    enqueued += query
                        .build()
                        .execute(&mut *connection)
                        .await?
                        .rows_affected();
                }
                Ok(enqueued)
            }

            /// 删除既无节点、也无正文的 scope。
            pub(crate) async fn prune_scopes(
                connection: &mut $conn,
                scopes: &[i64],
            ) -> anyhow::Result<()> {
                if scopes.is_empty() {
                    return Ok(());
                }
                let mut unique: Vec<i64> = scopes.to_vec();
                unique.sort_unstable();
                unique.dedup();
                for chunk in unique.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "DELETE FROM content_scopes WHERE id IN (",
                    );
                    let mut list = query.separated(",");
                    for scope in chunk {
                        list.push_bind(*scope);
                    }
                    list.push_unseparated(")");
                    query.push(
                        " AND NOT EXISTS(SELECT 1 FROM turn_nodes n WHERE n.scope_id=content_scopes.id) \
                         AND NOT EXISTS(SELECT 1 FROM contents c WHERE c.scope_id=content_scopes.id)",
                    );
                    query.build().execute(&mut *connection).await?;
                }
                Ok(())
            }

            /// 内容回收：领取候选，重检四类反向引用后删除。批次数量与字节上限
            /// 沿用 ADR-0076。
            pub(crate) async fn sweep(pool: &$pool) -> anyhow::Result<u64> {
                let mut connection = pool.acquire_write().await?;
                let mut transaction = connection.begin_with($begin).await?;
                let mut removed = 0u64;
                loop {
                    let claim_sql = sqlx::AssertSqlSafe(format!(
                        "{}{CONTENT_BATCH}{}",
                        $claim_before, $claim_after
                    ));
                    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(claim_sql)
                        .fetch_all(&mut *transaction)
                        .await?;
                    if rows.is_empty() {
                        break;
                    }
                    let mut selected = Vec::with_capacity(rows.len());
                    let mut bytes = 0i64;
                    for row in rows {
                        if !selected.is_empty() && bytes.saturating_add(row.2) > CONTENT_BYTES {
                            break;
                        }
                        bytes = bytes.saturating_add(row.2);
                        selected.push(row);
                    }
                    let mut delete = sqlx::QueryBuilder::<$db>::new(
                        "DELETE FROM contents WHERE id IN (",
                    );
                    let mut list = delete.separated(",");
                    for row in &selected {
                        list.push_bind(row.0);
                    }
                    list.push_unseparated(")");
                    delete.push(" AND ");
                    delete.push(unreferenced!("contents.id"));
                    removed += delete
                        .build()
                        .execute(&mut *transaction)
                        .await?
                        .rows_affected();
                    // 仍被引用的候选只移出队列；最后一条引用删除时会再次入队。
                    let mut queue =
                        sqlx::QueryBuilder::<$db>::new("DELETE FROM content_gc WHERE content_id IN (");
                    let mut list = queue.separated(",");
                    for row in &selected {
                        list.push_bind(row.0);
                    }
                    list.push_unseparated(")");
                    queue.build().execute(&mut *transaction).await?;
                    let mut scopes: Vec<i64> = selected.iter().map(|row| row.1).collect();
                    prune_scopes(&mut transaction, &mut scopes).await?;
                }
                transaction.commit().await?;
                connection.finish().await?;
                Ok(removed)
            }
        }
    };
}

// SQLite：共享写租约 + BEGIN IMMEDIATE；没有行锁语句。
backend!(
    sqlite,
    sqlx::SqlitePool,
    sqlx::Sqlite,
    sqlx::SqliteConnection<'_>,
    "BEGIN IMMEDIATE",
    "",
    "WITH candidates AS MATERIALIZED (SELECT content_id FROM content_gc ORDER BY content_id LIMIT ",
    ") SELECT c.id, c.scope_id, CASE WHEN EXISTS(SELECT 1 FROM turn_node_contents r WHERE r.content_id=c.id) \
        OR EXISTS(SELECT 1 FROM history_marker_contents r WHERE r.content_id=c.id) \
        OR EXISTS(SELECT 1 FROM observation_event_contents r WHERE r.content_id=c.id) \
        OR EXISTS(SELECT 1 FROM observation_tool_results r WHERE r.content_id=c.id) \
        THEN 0 ELSE length(c.body) END \
        FROM candidates g JOIN contents c ON c.id=g.content_id ORDER BY c.id"
);
// PostgreSQL：行锁协议；GC 跳过在途写入锁住的行。`FOR UPDATE` 必须位于 LIMIT 之后。
backend!(
    postgres,
    sqlx::PgPool,
    sqlx::Postgres,
    sqlx::PgConnection<'_>,
    "BEGIN",
    " FOR KEY SHARE",
    "SELECT c.id, c.scope_id, CASE WHEN EXISTS(SELECT 1 FROM turn_node_contents r WHERE r.content_id=c.id) \
        OR EXISTS(SELECT 1 FROM history_marker_contents r WHERE r.content_id=c.id) \
        OR EXISTS(SELECT 1 FROM observation_event_contents r WHERE r.content_id=c.id) \
        OR EXISTS(SELECT 1 FROM observation_tool_results r WHERE r.content_id=c.id) \
        THEN 0 ELSE CAST(octet_length(c.body) AS BIGINT) END \
        FROM content_gc g JOIN contents c ON c.id=g.content_id ORDER BY c.id LIMIT ",
    " FOR UPDATE OF c SKIP LOCKED"
);
