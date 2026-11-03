use super::SqlTurnChainStore;

use anyhow::{Context, ensure};
use std::collections::{HashMap, VecDeque};

const SEED_BATCH: usize = 512;
const NODE_BATCH: usize = 128;
const REFERENCE_BATCH: usize = 2048;

#[derive(sqlx::FromRow)]
struct Candidate {
    node_pk: i64,
    parent_pk: Option<i64>,
    scope_id: i64,
    expires_at: Option<i64>,
    deleting: Option<bool>,
    outside_child: bool,
    has_refs: bool,
}

// 被保留的 child 会阻止其所有祖先删除；批外 child 已由索引 EXISTS 标记。
// 限定在当前小批图上求闭包，不扫描整个 live 历史或依赖插入 ID 的先后。
fn close_deletable(nodes: &[Candidate], allowed: &mut [bool]) {
    let positions: HashMap<_, _> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.node_pk, index))
        .collect();
    let mut blocked: VecDeque<_> = allowed
        .iter()
        .enumerate()
        .filter_map(|(index, allowed)| (!allowed).then_some(index))
        .collect();
    while let Some(index) = blocked.pop_front() {
        if let Some(parent) = nodes[index]
            .parent_pk
            .and_then(|parent| positions.get(&parent).copied())
            && allowed[parent]
        {
            allowed[parent] = false;
            blocked.push_back(parent);
        }
    }
}

macro_rules! backend {
    ($seed:ident, $collect:ident, $prune:ident, $prune_scopes:path, $enqueue:path, $pool:ty, $conn:ty, $db:ty, $begin:literal, $lock:literal) => {
        async fn $prune(connection: &mut $conn, scopes: &mut Vec<i64>) -> anyhow::Result<()> {
            let scopes = std::mem::take(scopes);
            $prune_scopes(connection, &scopes).await
        }

        async fn $seed(pool: &$pool, now: i64, after: Option<(i64,i64)>) -> anyhow::Result<Option<(i64,i64)>> {
            let mut connection = pool.acquire_write().await?;
            let mut transaction = connection.begin_with($begin).await?;
            if !$lock.is_empty() { sqlx::query($lock).execute(&mut *transaction).await?; }
            let mut query = sqlx::QueryBuilder::<$db>::new("SELECT expires_at,node_pk FROM turn_node_retention WHERE expires_at<=");
            query.push_bind(now);
            query.push(" AND NOT EXISTS(SELECT 1 FROM turn_nodes child WHERE child.parent_pk=turn_node_retention.node_pk)");
            if let Some((expires,node_pk)) = after {
                query.push(" AND (expires_at,node_pk)>(").push_bind(expires).push(",").push_bind(node_pk).push(")");
            }
            query.push(" ORDER BY expires_at,node_pk LIMIT ").push(SEED_BATCH);
            let rows: Vec<(i64,i64)> = query.build_query_as().fetch_all(&mut *transaction).await?;
            if !rows.is_empty() {
                let mut insert = sqlx::QueryBuilder::<$db>::new("INSERT INTO turn_node_gc(node_pk) ");
                insert.push_values(rows.iter(), |mut values, row| { values.push_bind(row.1); });
                insert.push(" ON CONFLICT(node_pk) DO NOTHING");
                insert.build().execute(&mut *transaction).await?;
            }
            let next = rows.last().copied();
            transaction.commit().await?;
            connection.finish().await?;
            Ok(next)
        }

        async fn $collect(pool: &$pool, now: i64) -> anyhow::Result<(usize,u64)> {
            let mut connection = pool.acquire_write().await?;
            let mut transaction = connection.begin_with($begin).await?;
            // 普通提交和 native compaction 续期先持有 nodes ROW EXCLUSIVE。
            // 顺序固定为 nodes -> contents，资格重检到删除之间不允许写入穿插。
            if !$lock.is_empty() { sqlx::query($lock).execute(&mut *transaction).await?; }
            let mut query = sqlx::QueryBuilder::<$db>::new(
                "WITH RECURSIVE seeds AS (SELECT node_pk FROM turn_node_gc ORDER BY node_pk DESC LIMIT ",
            );
            // 只共享有限的 selected；SQLite 不应因多次引用递归结果而先遍历全部祖先。
            query.push(NODE_BATCH).push("), candidates(node_pk) AS NOT MATERIALIZED (\
                SELECT node_pk FROM seeds UNION \
                SELECT n.parent_pk FROM candidates c JOIN turn_nodes n ON n.node_pk=c.node_pk WHERE n.parent_pk IS NOT NULL\
                ), selected AS MATERIALIZED (SELECT node_pk FROM candidates LIMIT ").push(NODE_BATCH).push(") \
                SELECT n.node_pk,n.parent_pk,n.scope_id,r.expires_at,r.deleting,\
                (EXISTS(SELECT 1 FROM turn_nodes child WHERE child.parent_pk=n.node_pk \
                        AND child.node_pk NOT IN (SELECT node_pk FROM selected)) \
                 OR EXISTS(SELECT 1 FROM native_compactions c WHERE c.source_generation_id=n.id)) AS outside_child,\
                EXISTS(SELECT 1 FROM turn_node_contents refs WHERE refs.node_pk=n.node_pk) AS has_refs \
                FROM selected g JOIN turn_nodes n ON n.node_pk=g.node_pk \
                LEFT JOIN turn_node_retention r ON r.node_pk=n.node_pk");
            let nodes: Vec<Candidate> = query.build_query_as().fetch_all(&mut *transaction).await?;
            let visited = nodes.len();
            let mut eligible = nodes.iter().map(|node| {
                let expiry = node.expires_at.context("missing history retention during collection")?;
                let deleting = node.deleting.context("missing history retention during collection")?;
                Ok((deleting || expiry <= now) && !node.outside_child)
            }).collect::<anyhow::Result<Vec<bool>>>()?;
            close_deletable(&nodes, &mut eligible);
            let targets: Vec<_> = nodes.iter().zip(&eligible).filter_map(|(node,allowed)| allowed.then_some(node.node_pk)).collect();
            let mut removed_refs = 0;
            if nodes.iter().zip(&eligible).any(|(node,allowed)| *allowed && node.has_refs) {
                // 与第一次引用删除原子提交；延期写入或时钟回拨不能复活半清理节点。
                let mut mark = sqlx::QueryBuilder::<$db>::new("UPDATE turn_node_retention SET deleting=TRUE WHERE NOT deleting AND node_pk IN (");
                let mut list = mark.separated(",");
                for node_pk in &targets { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                mark.build().execute(&mut *transaction).await?;
                // 删除引用并取回失去引用的正文 ID，再在同一事务内按四类反向引用
                // 重检后入队；跨 driver 回传一次 ID 列表，避免逐行查询。
                let mut remove = sqlx::QueryBuilder::<$db>::new(
                    "DELETE FROM turn_node_contents WHERE (node_pk,ordinal) IN \
                     (SELECT node_pk,ordinal FROM turn_node_contents WHERE node_pk IN (",
                );
                let mut list = remove.separated(",");
                for node_pk in &targets { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                remove.push(" ORDER BY node_pk,ordinal LIMIT ").push(REFERENCE_BATCH).push(") RETURNING content_id");
                let released: Vec<i64> = remove.build_query_scalar().fetch_all(&mut *transaction).await?;
                removed_refs = released.len();
                $enqueue(&mut *transaction, &released).await?;
            }
            let mut deletable = eligible.clone();
            if !targets.is_empty() {
                // 每个候选仅做一次索引 EXISTS，避免 DISTINCT 扫描大节点的全部剩余引用。
                let mut pending = sqlx::QueryBuilder::<$db>::new("SELECT n.node_pk FROM turn_nodes n WHERE n.node_pk IN (");
                let mut list = pending.separated(",");
                for node_pk in &targets { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                pending.push(" AND EXISTS(SELECT 1 FROM turn_node_contents refs WHERE refs.node_pk=n.node_pk)");
                let pending: Vec<i64> = pending.build_query_scalar().fetch_all(&mut *transaction).await?;
                for (node, allowed) in nodes.iter().zip(&mut deletable) {
                    *allowed &= !pending.contains(&node.node_pk);
                }
                close_deletable(&nodes, &mut deletable);
            }
            let deleted: Vec<_> = nodes.iter().zip(&deletable).filter_map(|(node,allowed)| allowed.then_some(node.node_pk)).collect();
            let mut removed = 0;
            if !deleted.is_empty() {
                // 父 FK 在提交时验证，允许一个已证明封闭的小批子图集合删除。
                // 批外父节点重新成为候选；仍有 live child 时下一次会重检并退出。
                let mut parents = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_node_gc(node_pk) SELECT DISTINCT parent_pk FROM turn_nodes WHERE parent_pk IS NOT NULL AND node_pk IN (",
                );
                let mut list = parents.separated(",");
                for node_pk in &deleted { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                parents.push(" ON CONFLICT(node_pk) DO NOTHING");
                parents.build().execute(&mut *transaction).await?;
                let mut remove = sqlx::QueryBuilder::<$db>::new("DELETE FROM turn_nodes WHERE node_pk IN (");
                let mut list = remove.separated(",");
                for node_pk in &deleted { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                removed = remove.build().execute(&mut *transaction).await?.rows_affected();
            }
            let blocked: Vec<_> = nodes.iter().zip(&eligible).filter_map(|(node,allowed)| (!allowed).then_some(node.node_pk)).collect();
            if !blocked.is_empty() {
                let mut remove = sqlx::QueryBuilder::<$db>::new("DELETE FROM turn_node_gc WHERE node_pk IN (");
                let mut list = remove.separated(",");
                for node_pk in &blocked { list.push_bind(*node_pk); }
                list.push_unseparated(")");
                remove.build().execute(&mut *transaction).await?;
            }
            let mut scopes: Vec<i64> = nodes
                .iter()
                .zip(&deletable)
                .filter_map(|(node, allowed)| allowed.then_some(node.scope_id))
                .collect();
            ensure!(
                visited == 0 || removed > 0 || removed_refs > 0 || !blocked.is_empty(),
                "history collection made no progress"
            );
            // 节点与引用各自的上限不变，在同一小事务内完成；内容回收在节点收集
            // 结束后单独进行，两个入口共享同一套内容存储锁协议。
            $prune(&mut transaction, &mut scopes).await?;
            transaction.commit().await?;
            connection.finish().await?;
            Ok((visited, removed))
        }
    };
}

backend!(
    seed_sqlite,
    collect_sqlite,
    prune_sqlite,
    crate::content_store::sqlite::prune_scopes,
    crate::content_store::sqlite::enqueue_unreferenced,
    sqlx::SqlitePool,
    sqlx::SqliteConnection<'_>,
    sqlx::Sqlite,
    "BEGIN IMMEDIATE",
    ""
);
backend!(
    seed_postgres,
    collect_postgres,
    prune_postgres,
    crate::content_store::postgres::prune_scopes,
    crate::content_store::postgres::enqueue_unreferenced,
    sqlx::PgPool,
    sqlx::PgConnection<'_>,
    sqlx::Postgres,
    "BEGIN",
    "LOCK TABLE turn_nodes IN SHARE ROW EXCLUSIVE MODE"
);

pub(super) async fn sweep(store: &SqlTurnChainStore) -> anyhow::Result<u64> {
    let now = chrono::Utc::now().timestamp_millis();
    let mut cursor = None;
    loop {
        let next = match store {
            SqlTurnChainStore::Sqlite(pool) => seed_sqlite(pool, now, cursor).await?,
            SqlTurnChainStore::Postgres(pool) => seed_postgres(pool, now, cursor).await?,
        };
        let Some(next) = next else { break };
        cursor = Some(next);
    }
    let mut removed = 0u64;
    loop {
        let (visited, count) = match store {
            SqlTurnChainStore::Sqlite(pool) => collect_sqlite(pool, now).await?,
            SqlTurnChainStore::Postgres(pool) => collect_postgres(pool, now).await?,
        };
        removed = removed.saturating_add(count);
        if visited == 0 {
            break;
        }
    }
    // 节点收集只负责登记；内容摘要在本批节点的全部引用操作之内重检后删除。
    match store {
        SqlTurnChainStore::Sqlite(pool) => crate::content_store::sweep_sqlite(pool).await?,
        SqlTurnChainStore::Postgres(pool) => crate::content_store::sweep_postgres(pool).await?,
    };
    Ok(removed)
}
