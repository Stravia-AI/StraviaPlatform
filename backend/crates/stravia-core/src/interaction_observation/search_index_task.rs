//! 后台维护历史检索索引，并在通知丢失或升级后逐批对账。

use std::collections::BTreeSet;
use std::time::Duration;

use stravia_runtime_contract::CancellationToken;
use tokio::sync::{broadcast, mpsc};

use super::{ObservationUpdate, eligible, store::ObservationStore};
use crate::history_recall::index::{index_postgres, index_sqlite, index_sqlite_batch};

const RECONCILE_BATCH: i64 = 64;
/// 合并同一请求生命周期内的多次观测事件与并发请求更新，减少 SQLite 写事务与 fsync 次数。
const COALESCE_DELAY: Duration = Duration::from_millis(50);
const INDEX_BATCH_LIMIT: usize = 64;

pub(crate) fn index_progress_inconsistent(interaction: &str, progress: &str) -> String {
    format!(
        "{progress}.interaction_id IS NULL OR {interaction}.last_event_sequence<>{progress}.last_event_sequence OR ({interaction}.status NOT IN ('running','waiting_client'))<>COALESCE({progress}.indexed_terminal,FALSE)"
    )
}

fn reconcile_sql() -> String {
    let member = eligible("i");
    let inconsistent = index_progress_inconsistent("i", "p");
    format!(
        "SELECT i.id FROM interaction_observations i LEFT JOIN history_search_progress p ON p.interaction_id=i.id WHERE i.id>$1 AND ((({member}) AND ({inconsistent})) OR (NOT ({member}) AND (p.interaction_id IS NOT NULL OR EXISTS (SELECT 1 FROM history_search_segments s WHERE s.interaction_id=i.id)))) ORDER BY i.id LIMIT $2"
    )
}

/// Gateway lifecycle 启动本任务，启动对账不阻塞 Gateway 就绪。
pub(super) async fn run(
    db: ObservationStore,
    mut updates: broadcast::Receiver<ObservationUpdate>,
    mut sweeps: mpsc::Receiver<()>,
    cancellation: CancellationToken,
) {
    let mut reconcile_needed = true;
    let mut reconciliation: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    let mut pending = BTreeSet::<String>::new();
    let mut flush_at: Option<tokio::time::Instant> = None;
    loop {
        if reconcile_needed && reconciliation.is_none() {
            let db = db.clone();
            let cancellation = cancellation.clone();
            reconciliation = Some(tokio::spawn(
                async move { reconcile(&db, &cancellation).await },
            ));
            reconcile_needed = false;
        }
        let flush_ready = !pending.is_empty()
            && (pending.len() >= INDEX_BATCH_LIMIT
                || flush_at.is_some_and(|deadline| tokio::time::Instant::now() >= deadline));
        if flush_ready {
            let mut batch = Vec::with_capacity(pending.len().min(INDEX_BATCH_LIMIT));
            while batch.len() < INDEX_BATCH_LIMIT {
                let Some(id) = pending.pop_first() else {
                    break;
                };
                batch.push(id);
            }
            let remaining = index_batch(&db, &batch, &cancellation).await;
            pending.extend(remaining);
            if cancellation.is_cancelled() {
                break;
            }
            // 仍有积压或大 Interaction 分页续批时立即继续，不再追加合并等待。
            flush_at = (!pending.is_empty()).then(tokio::time::Instant::now);
            if !pending.is_empty() {
                tokio::task::yield_now().await;
                continue;
            }
        } else if !pending.is_empty() && flush_at.is_none() {
            flush_at = Some(tokio::time::Instant::now() + COALESCE_DELAY);
        }
        tokio::select! {
            _ = cancellation.cancelled() => break,
            _ = async {
                match flush_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {}
            result = async {
                match reconciliation.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(error = ?error, "history search reconciliation failed"),
                    Err(error) => tracing::warn!(error = ?error, "history search reconciliation task failed"),
                }
                reconciliation = None;
            }
            update = updates.recv() => match update {
                Ok(ObservationUpdate::Event(event)) => pending.extend(event.interaction_id),
                Ok(ObservationUpdate::Change(change)) => pending.extend(change.interaction_id),
                Ok(ObservationUpdate::ResetRequired { .. })
                | Err(broadcast::error::RecvError::Lagged(_)) => reconcile_needed = true,
                Ok(_) => {},
                Err(broadcast::error::RecvError::Closed) => break,
            },
            sweep = sweeps.recv() => match sweep {
                Some(()) => reconcile_needed = true,
                None => break,
            },
        }
        // 合并已排队的通知；丢失通知时全量对账覆盖所有受影响的 Interaction。
        loop {
            match updates.try_recv() {
                Ok(ObservationUpdate::Event(event)) => pending.extend(event.interaction_id),
                Ok(ObservationUpdate::Change(change)) => pending.extend(change.interaction_id),
                Ok(ObservationUpdate::ResetRequired { .. })
                | Err(broadcast::error::TryRecvError::Lagged(_)) => reconcile_needed = true,
                Ok(_) => {}
                Err(
                    broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed,
                ) => break,
            }
        }
        while sweeps.try_recv().is_ok() {
            reconcile_needed = true;
        }
    }
    // 不让子任务脱离 Gateway 生命周期；关闭时中断事务并等待回滚清理。
    if let Some(task) = reconciliation {
        task.abort();
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            tracing::warn!(error = ?error, "history search reconciliation task failed");
        }
    }
}

async fn index_batch(
    db: &ObservationStore,
    batch: &[String],
    cancellation: &CancellationToken,
) -> Vec<String> {
    if batch.is_empty() {
        return Vec::new();
    }
    match db {
        ObservationStore::Sqlite(pool, _) => {
            let result = tokio::select! {
                _ = cancellation.cancelled() => return Vec::new(),
                result = index_sqlite_batch(pool, batch) => result,
            };
            match result {
                Ok(more) => more,
                Err(error) => {
                    // 单个失败不阻断同批其它 Interaction：回退到逐条索引隔离故障。
                    tracing::debug!(error = ?error, batch_len = batch.len(), "batched history search indexing failed; falling back to per-interaction indexing");
                    for id in batch {
                        index_until_current(db, id, cancellation).await;
                        if cancellation.is_cancelled() {
                            break;
                        }
                    }
                    Vec::new()
                }
            }
        }
        ObservationStore::Postgres(_, _) => {
            for id in batch {
                index_until_current(db, id, cancellation).await;
                if cancellation.is_cancelled() {
                    break;
                }
            }
            Vec::new()
        }
    }
}

async fn index_until_current(db: &ObservationStore, id: &str, cancellation: &CancellationToken) {
    loop {
        let result = tokio::select! {
            _ = cancellation.cancelled() => return,
            result = index_one(db, id) => result,
        };
        match result {
            Ok(true) => tokio::task::yield_now().await,
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(error = ?error, interaction = id, "history search indexing failed");
                return;
            }
        }
    }
}

/// 键集分页保证单个失败不阻断后续 Interaction；下一次对账重试失败目标。
/// PostgreSQL 的索引事务先锁 Interaction 再锁进度，多实例重复选择不会重复建段。
pub(super) async fn reconcile(
    db: &ObservationStore,
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    let sql = reconcile_sql();
    let mut cursor = String::new();
    loop {
        let rows: Vec<String> = tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            result = behind_rows(db, &sql, &cursor) => result?,
        };
        let Some(last) = rows.last() else {
            return Ok(());
        };
        cursor.clone_from(last);
        let mut remaining = index_batch(db, &rows, cancellation).await;
        while !remaining.is_empty() && !cancellation.is_cancelled() {
            tokio::task::yield_now().await;
            remaining = index_batch(db, &remaining, cancellation).await;
        }
        if cancellation.is_cancelled() {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}

async fn behind_rows(
    db: &ObservationStore,
    sql: &str,
    cursor: &str,
) -> anyhow::Result<Vec<String>> {
    match db {
        ObservationStore::Sqlite(pool, _) => Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(cursor)
            .bind(RECONCILE_BATCH)
            .fetch_all(pool)
            .await?),
        ObservationStore::Postgres(pool, _) => Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(cursor)
            .bind(RECONCILE_BATCH)
            .fetch_all(pool)
            .await?),
    }
}

async fn index_one(db: &ObservationStore, interaction: &str) -> anyhow::Result<bool> {
    match db {
        ObservationStore::Sqlite(pool, _) => index_sqlite(pool, interaction).await,
        ObservationStore::Postgres(pool, _) => index_postgres(pool, interaction).await,
    }
}
