//! 后台维护历史检索索引，并在通知丢失或升级后逐批对账。

use std::collections::BTreeSet;

use stravia_runtime_contract::CancellationToken;
use tokio::sync::{broadcast, mpsc};

use super::{ObservationUpdate, eligible, store::ObservationStore};
use crate::history_recall::index::{index_postgres, index_sqlite};

const RECONCILE_BATCH: i64 = 64;

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
    loop {
        if reconcile_needed && reconciliation.is_none() {
            let db = db.clone();
            let cancellation = cancellation.clone();
            reconciliation = Some(tokio::spawn(
                async move { reconcile(&db, &cancellation).await },
            ));
            reconcile_needed = false;
        }
        while let Some(id) = pending.pop_first() {
            index_until_current(&db, &id, &cancellation).await;
            if cancellation.is_cancelled() {
                break;
            }
        }
        tokio::select! {
            _ = cancellation.cancelled() => break,
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
        for id in rows {
            index_until_current(db, &id, cancellation).await;
            if cancellation.is_cancelled() {
                return Ok(());
            }
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
