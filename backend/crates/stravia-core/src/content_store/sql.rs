//! 共享正文存储的后端 SQL。两个后端行为一致；锁协议按计划区分：
//! PostgreSQL 用行锁（复用 `FOR KEY SHARE`，GC `FOR UPDATE SKIP LOCKED`），
//! SQLite 用共享写租约与 `BEGIN IMMEDIATE`。

use super::*;

const CONTENT_BATCH: usize = 512;
/// 单条读取语句的 `IN` 参数上限，低于 SQLite 的绑定参数限制。
const READ_BATCH: usize = 256;
const CONTENT_BYTES: i64 = 8 * 1024 * 1024;

/// Existing reverse references; the journal table is checked separately because older
/// migration stages run content GC before that table exists.
macro_rules! referenced {
    ($c:literal) => {
        concat!(
            "EXISTS(SELECT 1 FROM turn_node_contents r WHERE r.content_id=",
            $c,
            ")",
            " OR EXISTS(SELECT 1 FROM history_marker_contents r WHERE r.content_id=",
            $c,
            ")",
            " OR EXISTS(SELECT 1 FROM observation_event_contents r WHERE r.content_id=",
            $c,
            ")",
            " OR EXISTS(SELECT 1 FROM observation_tool_results r WHERE r.content_id=",
            $c,
            ")"
        )
    };
}

macro_rules! unreferenced {
    ($c:literal) => {
        concat!("NOT (", referenced!($c), ")")
    };
}

/// 领取候选的列：被引用的候选字节数记 0，不占批次的字节预算。
macro_rules! claim_columns {
    ($size:literal) => {
        concat!(
            "SELECT c.id, c.scope_id, CASE WHEN ",
            referenced!("c.id"),
            " THEN 0 ELSE ",
            $size,
            " END"
        )
    };
}

/// SQLite 写租约外的只读预读：预热历史与 Observation 引用索引页；journal
/// 表在旧迁移阶段可能不存在，因此只在事务内作权威检查。结果不参与任何判断。
pub(crate) async fn read_ahead_unreferenced(pool: &SqlitePool, ids: &[i64]) -> anyhow::Result<()> {
    for chunk in ids.chunks(400) {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT COUNT(*) FROM contents c WHERE c.id IN (",
        );
        let mut list = query.separated(",");
        for id in chunk {
            list.push_bind(*id);
        }
        list.push_unseparated(") AND ");
        query.push(unreferenced!("c.id"));
        query.build().fetch_one(pool).await?;
    }
    Ok(())
}

macro_rules! backend {
    (
        $module:ident,
        $pool:ty,
        $db:ty,
        $conn:ty,
        $begin:literal,
        $key_share:literal,
        $no_key_update:literal,
        $skip_locked:literal,
        $journal_exists:literal,
        $claim_before:expr,
        $claim_after:expr
    ) => {
        pub(crate) mod $module {
            use super::*;

            /// 取得 Principal 的 scope；不存在时插入。调用方持有写事务。
            ///
            /// 已有 scope 一律带 `FOR KEY SHARE`（行锁后端），清理方的 `FOR UPDATE` 领取
            /// 因此不会与写入方交错。插入冲突说明别的事务刚创建了它：回到加锁查询重读，
            /// 而不是裸读；该 scope 若恰好被清理删除，则下一轮重新插入。
            pub(crate) async fn resolve_scope(
                connection: &mut $conn,
                principal: &str,
            ) -> anyhow::Result<i64> {
                for _ in 0..4 {
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
                    if let Some(scope) = inserted {
                        return Ok(scope);
                    }
                }
                anyhow::bail!("content scope could not be resolved")
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
                                .push_bind(i64::from(format_code(contents[*index].format)))
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

            /// 批量读取同一 scope 内的正文并逐条校验身份与长度。
            pub(crate) async fn read(
                connection: &mut $conn,
                scope_id: i64,
                ids: &[i64],
            ) -> anyhow::Result<Vec<Body>> {
                read_filtered(connection, Some(scope_id), ids, false).await
            }

            /// Read only rows with a visible observation reference in the same SQL
            /// statement as the body, so an ID cannot be reused between authorization
            /// and retrieval.
            pub(crate) async fn read_observation_visible(
                connection: &mut $conn,
                ids: &[i64],
            ) -> anyhow::Result<Vec<Body>> {
                read_filtered(connection, None, ids, true).await
            }

            /// 内容的原始字节数（`id → raw_len`）；只读元数据列，不读取也不解码正文。
            /// 列表摘要只需要长度，缺失的 id 不出现在结果里，由调用方判定为悬空引用。
            pub(crate) async fn lengths(
                connection: &mut $conn,
                ids: &[i64],
            ) -> anyhow::Result<HashMap<i64, i64>> {
                let mut lengths = HashMap::with_capacity(ids.len());
                for chunk in ids.chunks(READ_BATCH) {
                    let mut query =
                        sqlx::QueryBuilder::<$db>::new("SELECT id, raw_len FROM contents WHERE id IN (");
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(*id);
                    }
                    list.push_unseparated(")");
                    let rows: Vec<(i64, i64)> =
                        query.build_query_as().fetch_all(&mut *connection).await?;
                    lengths.extend(rows);
                }
                Ok(lengths)
            }

            async fn read_filtered(
                connection: &mut $conn,
                scope_id: Option<i64>,
                ids: &[i64],
                observation_visible: bool,
            ) -> anyhow::Result<Vec<Body>> {
                let mut unique: Vec<i64> = ids.to_vec();
                unique.sort_unstable();
                unique.dedup();
                let mut bodies = Vec::with_capacity(unique.len());
                for chunk in unique.chunks(READ_BATCH) {
                    for row in read_encoded_filtered(connection, scope_id, chunk, observation_visible).await? {
                        bodies.push(super::super::decode_body(row)?);
                    }
                }
                Ok(bodies)
            }

            /// 仅供启动迁移：单连接读取有界的一批编码行，CPU 校验由调用方在
            /// blocking worker 上使用 `decode_body` 完成，不能直接暴露为正文。
            pub(crate) async fn read_upgrade_encoded(
                connection: &mut $conn,
                scope_id: i64,
                ids: &[i64],
            ) -> anyhow::Result<Vec<super::super::EncodedBody>> {
                ensure!(ids.len() <= READ_BATCH, "upgrade content batch too large");
                if ids.is_empty() {
                    return Ok(Vec::new());
                }
                read_encoded_filtered(connection, Some(scope_id), ids, false).await
            }

            async fn read_encoded_filtered(
                connection: &mut $conn,
                scope_id: Option<i64>,
                ids: &[i64],
                observation_visible: bool,
            ) -> anyhow::Result<Vec<super::super::EncodedBody>> {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT id, digest, CAST(format AS BIGINT), raw_len, body FROM contents WHERE id IN (",
                );
                let mut list = query.separated(",");
                for id in ids {
                    list.push_bind(*id);
                }
                list.push_unseparated(")");
                if let Some(scope_id) = scope_id {
                    query.push(" AND scope_id = ").push_bind(scope_id);
                }
                if observation_visible {
                    query.push(" AND (EXISTS(SELECT 1 FROM observation_event_contents ec JOIN observation_events e ON e.sequence=ec.sequence WHERE ec.content_id=contents.id AND e.interaction_id IS NOT NULL) OR EXISTS(SELECT 1 FROM observation_tool_results tr JOIN observation_events e ON e.sequence=tr.sequence WHERE tr.content_id=contents.id AND e.interaction_id IS NOT NULL))");
                }
                query.push(" ORDER BY id");
                Ok(query.build_query_as().fetch_all(&mut *connection).await?)
            }

            /// 引用方删除引用后，在同一事务内登记失去最后引用的内容。
            ///
            /// 行锁后端先对候选行加 `FOR NO KEY UPDATE`：它与回收方的 `FOR UPDATE`、以及另一个
            /// 同时删除兄弟引用的事务互斥。这样既不会在回收方「仍有引用→移出队列」的同一刻
            /// 因冲突而漏登记，也不会因看不到兄弟事务尚未提交的删除而两边都漏登记。锁取得
            /// 之后才做引用重检（新语句，新快照）。
            pub(crate) async fn enqueue_unreferenced(
                connection: &mut $conn,
                ids: &[i64],
            ) -> anyhow::Result<u64> {
                if ids.is_empty() {
                    return Ok(0);
                }
                let journal_exists: bool = sqlx::query_scalar($journal_exists)
                    .fetch_one(&mut *connection).await?;
                let mut unique: Vec<i64> = ids.to_vec();
                unique.sort_unstable();
                unique.dedup();
                let mut enqueued = 0;
                for chunk in unique.chunks(400) {
                    if !$no_key_update.is_empty() {
                        let mut lock = sqlx::QueryBuilder::<$db>::new(
                            "SELECT id FROM contents WHERE id IN (",
                        );
                        let mut list = lock.separated(",");
                        for id in chunk {
                            list.push_bind(*id);
                        }
                        list.push_unseparated(") ORDER BY id");
                        lock.push($no_key_update);
                        lock.build().execute(&mut *connection).await?;
                    }
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
                    if journal_exists {
                        query.push(" AND NOT EXISTS(SELECT 1 FROM vendor_atomic_journal_nodes r WHERE r.content_id=c.id)");
                    }
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
            ///
            /// 行锁后端先以 `FOR UPDATE SKIP LOCKED` 领取：写入方持有的 `FOR KEY SHARE`
            /// 会让对应 scope 被跳过（留给之后的回收），因此不会撞上外键错误；领取之后
            /// 的删除语句使用新快照重检节点与正文。
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
                    let claimed: Vec<i64> = if $skip_locked.is_empty() {
                        chunk.to_vec()
                    } else {
                        let mut claim = sqlx::QueryBuilder::<$db>::new(
                            "SELECT id FROM content_scopes WHERE id IN (",
                        );
                        let mut list = claim.separated(",");
                        for scope in chunk {
                            list.push_bind(*scope);
                        }
                        list.push_unseparated(") ORDER BY id");
                        claim.push($skip_locked);
                        claim.build_query_scalar().fetch_all(&mut *connection).await?
                    };
                    if claimed.is_empty() {
                        continue;
                    }
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "DELETE FROM content_scopes WHERE id IN (",
                    );
                    let mut list = query.separated(",");
                    for scope in &claimed {
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

            /// 内容回收：循环领取一批、提交、释放写协调，直到队列排空。每批至多
            /// [`CONTENT_BATCH`] 行或 [`CONTENT_BYTES`] 字节（ADR-0076），因此单次事务与
            /// 写租约占用都有界；ADR-0076 要求批次之间让出写协调。
            pub(crate) async fn sweep(pool: &$pool) -> anyhow::Result<u64> {
                let mut removed = 0u64;
                loop {
                    let mut connection = pool.acquire_write().await?;
                    let mut transaction = connection.begin_with($begin).await?;
                    let journal_exists: bool = sqlx::query_scalar($journal_exists)
                        .fetch_one(&mut *transaction).await?;
                    let claim_sql = sqlx::AssertSqlSafe(format!(
                        "{}{CONTENT_BATCH}{}",
                        $claim_before, $claim_after
                    ));
                    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(claim_sql)
                        .fetch_all(&mut *transaction)
                        .await?;
                    if rows.is_empty() {
                        transaction.commit().await?;
                        connection.finish().await?;
                        return Ok(removed);
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
                    if journal_exists {
                        delete.push(" AND NOT EXISTS(SELECT 1 FROM vendor_atomic_journal_nodes r WHERE r.content_id=contents.id)");
                    }
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
                    let scopes: Vec<i64> = selected.iter().map(|row| row.1).collect();
                    prune_scopes(&mut transaction, &scopes).await?;
                    transaction.commit().await?;
                    connection.finish().await?;
                }
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
    "",
    "",
    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='vendor_atomic_journal_nodes')",
    "WITH candidates AS MATERIALIZED (SELECT content_id FROM content_gc ORDER BY content_id LIMIT ",
    concat!(
        ") ",
        claim_columns!("length(c.body)"),
        " FROM candidates g JOIN contents c ON c.id=g.content_id ORDER BY c.id"
    )
);
// PostgreSQL：行锁协议；GC 跳过在途写入锁住的行。`FOR UPDATE` 必须位于 LIMIT 之后。
backend!(
    postgres,
    sqlx::PgPool,
    sqlx::Postgres,
    sqlx::PgConnection<'_>,
    "BEGIN",
    " FOR KEY SHARE",
    " FOR NO KEY UPDATE",
    " FOR UPDATE SKIP LOCKED",
    "SELECT to_regclass('vendor_atomic_journal_nodes') IS NOT NULL",
    concat!(
        claim_columns!("CAST(octet_length(c.body) AS BIGINT)"),
        " FROM content_gc g JOIN contents c ON c.id=g.content_id ORDER BY c.id LIMIT "
    ),
    " FOR UPDATE OF c SKIP LOCKED"
);
