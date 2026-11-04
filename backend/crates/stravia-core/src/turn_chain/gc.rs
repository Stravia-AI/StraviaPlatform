use super::SqlTurnChainStore;
use super::sql::Backend;

use anyhow::ensure;

const NODE_BATCH: i64 = 128;
const REFERENCE_BATCH: i64 = 2048;
const WATERMARK: &str = "turn_nodes.created_at";

#[derive(sqlx::FromRow)]
struct Candidate {
    node_pk: i64,
    created_at: i64,
    scope_id: i64,
    deleting: Option<bool>,
    has_child: bool,
    has_source_ref: bool,
    has_contents: bool,
}

impl Candidate {
    /// `deleting` 表示引用清理已开始；此后只需继续删正文引用，不再重算下限。
    fn eligible(&self, threshold: i64) -> bool {
        let floor = self.deleting == Some(true) || self.created_at <= threshold;
        floor && !self.has_child && !self.has_source_ref
    }
}

macro_rules! backend {
    ($scan:ident, $collect:ident, $prune:ident, $prune_scopes:path, $enqueue:path, $pool:ty, $conn:ty, $db:ty, $begin:literal, $locks:expr, $skip_locked:literal) => {
        async fn $prune(connection: &mut $conn, scopes: &mut Vec<i64>) -> anyhow::Result<()> {
            let scopes = std::mem::take(scopes);
            $prune_scopes(connection, &scopes).await
        }

        /// 期限跨越扫描：只扫 `(W, T]`，没有子节点且不是原生登记来源的节点入队，同一事务推进水位。
        async fn $scan(pool: &$pool, now: i64, window: i64) -> anyhow::Result<()> {
            let threshold = now.saturating_sub(window);
            let mut connection = pool.acquire_write().await?;
            let mut transaction = connection.begin_with($begin).await?;
            let watermark: i64 = sqlx::query_scalar(
                "SELECT position FROM history_retention_watermarks WHERE name = $1",
            ).bind(WATERMARK).fetch_one(&mut *transaction).await?;
            if threshold < watermark {
                sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                    .bind(WATERMARK).bind(threshold).execute(&mut *transaction).await?;
                transaction.commit().await?;
                connection.finish().await?;
                return Ok(());
            }
            let mut query = sqlx::QueryBuilder::<$db>::new(
                "INSERT INTO turn_node_gc(node_pk) \
                 SELECT n.node_pk FROM turn_nodes n \
                 WHERE n.created_at > ",
            );
            query.push_bind(watermark).push(" AND n.created_at <= ").push_bind(threshold);
            query.push(" AND NOT EXISTS(SELECT 1 FROM turn_nodes child WHERE child.parent_pk=n.node_pk) \
                 AND NOT EXISTS(SELECT 1 FROM native_compactions c WHERE c.source_generation_id=n.id) \
                 ON CONFLICT(node_pk) DO NOTHING");
            query.build().execute(&mut *transaction).await?;
            sqlx::query("UPDATE history_retention_watermarks SET position = $2 WHERE name = $1")
                .bind(WATERMARK).bind(threshold).execute(&mut *transaction).await?;
            transaction.commit().await?;
            connection.finish().await?;
            Ok(())
        }

        /// 从队列领取候选，加锁重检后删除；返回 `(访问数, 删除节点数, 进展数)`。
        async fn $collect(pool: &$pool, now: i64, window: i64) -> anyhow::Result<(usize,u64,u64)> {
            let threshold = now.saturating_sub(window);
            let mut connection = pool.acquire_write().await?;
            let mut transaction = connection.begin_with($begin).await?;
            // 与提交共享的锁序：turn_nodes -> content_scopes；SHARE ROW EXCLUSIVE 阻止写入穿插。
            // 表锁在前、咨询锁在后：等待在途提交结束期间不持有咨询锁，Marker/映射发布与原生压缩
            // 确认（取咨询锁共享模式）不会排在这次等待之后；取得表锁后再取咨询锁，使并发回收事务的
            // 释放触发器取锁顺序串行（见 `PG_GC_LOCK`）。持咨询锁的事务从不申请 turn_nodes 的
            // ROW EXCLUSIVE 及以上表锁，所以这个顺序不会成环。
            for lock in $locks { sqlx::query(lock).execute(&mut *transaction).await?; }
            let mut query = sqlx::QueryBuilder::<$db>::new(
                "SELECT n.node_pk,n.created_at,n.scope_id,r.deleting,\
                 EXISTS(SELECT 1 FROM turn_nodes child WHERE child.parent_pk=n.node_pk) AS has_child,\
                 EXISTS(SELECT 1 FROM native_compactions c WHERE c.source_generation_id=n.id) AS has_source_ref,\
                 EXISTS(SELECT 1 FROM turn_node_contents refs WHERE refs.node_pk=n.node_pk) AS has_contents \
                 FROM turn_node_gc g JOIN turn_nodes n ON n.node_pk=g.node_pk \
                 LEFT JOIN turn_node_retention r ON r.node_pk=n.node_pk \
                 ORDER BY n.node_pk LIMIT ",
            );
            query.push(NODE_BATCH).push($skip_locked);
            let nodes: Vec<Candidate> = query.build_query_as().fetch_all(&mut *transaction).await?;
            let visited = nodes.len();
            let mut targets = Vec::new();
            let mut blocked = Vec::new();
            for node in &nodes {
                if node.eligible(threshold) { targets.push(node.node_pk); } else { blocked.push(node.node_pk); }
            }
            let mut removed_refs = 0u64;
            let mut marked = 0u64;
            // 本轮释放的正文 ID。入队放在节点删除之后：先对象行（节点删除触发器锁的 Marker/映射/
            // 登记行）、后正文行，与 Marker 回收和 Marker 单行写入的锁序一致。
            let mut released: Vec<i64> = Vec::new();
            let with_contents: Vec<_> = nodes.iter()
                .filter(|node| node.eligible(threshold) && node.has_contents)
                .map(|node| node.node_pk)
                .collect();
            if !with_contents.is_empty() {
                let mut mark = sqlx::QueryBuilder::<$db>::new(
                    "UPDATE turn_node_retention SET deleting=TRUE WHERE NOT deleting AND node_pk IN (",
                );
                let mut list = mark.separated(",");
                for node_pk in &with_contents { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                marked = mark.build().execute(&mut *transaction).await?.rows_affected();
                // 删除引用并取回失去引用的正文 ID（稍后在同一事务内重检入队）；
                // 一个巨大节点的级联删除会长时间占用写锁，所以按批移除。
                let mut remove = sqlx::QueryBuilder::<$db>::new(
                    "DELETE FROM turn_node_contents WHERE (node_pk,ordinal) IN \
                     (SELECT node_pk,ordinal FROM turn_node_contents WHERE node_pk IN (",
                );
                let mut list = remove.separated(",");
                for node_pk in &with_contents { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                remove.push(" ORDER BY node_pk,ordinal LIMIT ").push(REFERENCE_BATCH).push(") RETURNING content_id");
                released = remove.build_query_scalar().fetch_all(&mut *transaction).await?;
                removed_refs = released.len() as u64;
                // 本轮不删除带正文的节点；它们留在队列里由后续轮次继续。
                targets.retain(|node_pk| !with_contents.contains(node_pk));
            }
            let mut removed = 0u64;
            if !targets.is_empty() {
                let mut remove = sqlx::QueryBuilder::<$db>::new(
                    "DELETE FROM turn_nodes WHERE node_pk IN (",
                );
                let mut list = remove.separated(",");
                for node_pk in &targets { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                remove.push(" AND NOT EXISTS(SELECT 1 FROM turn_nodes child WHERE child.parent_pk=turn_nodes.node_pk)");
                removed = remove.build().execute(&mut *transaction).await?.rows_affected();
            }
            if !blocked.is_empty() {
                let mut remove = sqlx::QueryBuilder::<$db>::new("DELETE FROM turn_node_gc WHERE node_pk IN (");
                let mut list = remove.separated(",");
                for node_pk in &blocked { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                remove.build().execute(&mut *transaction).await?;
            }
            let mut scopes: Vec<i64> = nodes.iter()
                .filter(|node| targets.contains(&node.node_pk))
                .map(|node| node.scope_id)
                .collect();
            ensure!(visited == 0 || removed > 0 || removed_refs > 0 || marked > 0 || !blocked.is_empty(), "history collection made no progress");
            // 正文回收在节点收集结束后由共享内容存储单独进行；这里只登记失去引用的正文。
            $enqueue(&mut *transaction, &released).await?;
            $prune(&mut transaction, &mut scopes).await?;
            transaction.commit().await?;
            connection.finish().await?;
            Ok((visited, removed, removed + removed_refs + marked + blocked.len() as u64))
        }
    };
}

backend!(
    scan_sqlite,
    collect_sqlite,
    prune_sqlite,
    crate::content_store::sqlite::prune_scopes,
    crate::content_store::sqlite::enqueue_unreferenced,
    sqlx::SqlitePool,
    sqlx::SqliteConnection<'_>,
    sqlx::Sqlite,
    "BEGIN IMMEDIATE",
    [] as [&str; 0],
    ""
);
backend!(
    scan_postgres,
    collect_postgres,
    prune_postgres,
    crate::content_store::postgres::prune_scopes,
    crate::content_store::postgres::enqueue_unreferenced,
    sqlx::PgPool,
    sqlx::PgConnection<'_>,
    sqlx::Postgres,
    "BEGIN",
    [
        "LOCK TABLE turn_nodes IN SHARE ROW EXCLUSIVE MODE",
        crate::history_retention::PG_GC_LOCK
    ],
    " FOR UPDATE OF n SKIP LOCKED"
);

pub(super) async fn sweep(store: &SqlTurnChainStore, now: i64) -> anyhow::Result<u64> {
    let window = store.retention.window_millis();
    match &store.backend {
        Backend::Sqlite(pool) => scan_sqlite(pool, now, window).await?,
        Backend::Postgres(pool) => scan_postgres(pool, now, window).await?,
    }
    let mut removed = 0u64;
    loop {
        let (visited, count, progress) = match &store.backend {
            Backend::Sqlite(pool) => collect_sqlite(pool, now, window).await?,
            Backend::Postgres(pool) => collect_postgres(pool, now, window).await?,
        };
        removed = removed.saturating_add(count);
        if progress == 0 {
            if visited == 0 {
                break;
            }
            anyhow::bail!("history collection claimed candidates without progress");
        }
    }
    // 节点回收只负责登记；正文在节点引用全部移除后重检并删除。
    // 依赖（Marker）释放的正文由 `HistoryMaintenance` 在最后再回收一次。
    match &store.backend {
        Backend::Sqlite(pool) => crate::content_store::sweep_sqlite(pool).await?,
        Backend::Postgres(pool) => crate::content_store::sweep_postgres(pool).await?,
    };
    Ok(removed)
}
