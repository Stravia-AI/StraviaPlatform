use super::{store::ObservationStore, types::ClearHistoryResult};
use std::collections::HashSet;

const REJECTION_BATCH_SIZE: i64 = 100;

/// SQLite 下一条到期交互；写事务与写租约外的预读共用，预读据此猜测写事务将删除的交互。
const SQLITE_NEXT_EXPIRED: &str = "SELECT id FROM interaction_observations WHERE last_active_at<=? AND status<>'running' ORDER BY last_active_at,id LIMIT 1";
/// SQLite 删除交互 `?1` 后将失去这一处引用的正文。
const SQLITE_EXPIRED_CONTENT_IDS: &str = "SELECT content_id FROM observation_event_contents WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=?1) UNION SELECT content_id FROM observation_tool_results WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=?1)";
/// 删除交互 `?1` 时改写的表，以及选出被删行的谓词；新增被删表时与删除语句一起更新。
/// observation_blocks 不在删除语句里：它随 observation_events 的外键级联删除。
const SQLITE_PURGED_ROWS: [(&str, &str); 13] = [
    ("history_search_segments", "interaction_id=?1"),
    ("history_search_progress", "interaction_id=?1"),
    (
        "observation_event_contents",
        "sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=?1)",
    ),
    (
        "observation_tool_results",
        "sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=?1)",
    ),
    (
        "observation_blocks",
        "sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=?1)",
    ),
    ("observation_events", "interaction_id=?1"),
    ("credential_mapping_reuses", "interaction_id=?1"),
    ("observation_pending_tools", "interaction_id=?1"),
    ("observation_tail_sources", "interaction_id=?1"),
    ("target_attempt_observations", "interaction_id=?1"),
    ("model_turn_observations", "interaction_id=?1"),
    ("inference_run_observations", "interaction_id=?1"),
    ("interaction_observations", "id=?1"),
];

/// 清空历史时将被删除的交互（非 running / waiting_client）引用的全部正文。
const CLEARED_CONTENT_IDS: &str = "SELECT content_id FROM observation_event_contents WHERE sequence IN (SELECT e.sequence FROM observation_events e JOIN interaction_observations i ON i.id=e.interaction_id WHERE i.status NOT IN ('running','waiting_client')) UNION SELECT content_id FROM observation_tool_results WHERE sequence IN (SELECT e.sequence FROM observation_events e JOIN interaction_observations i ON i.id=e.interaction_id WHERE i.status NOT IN ('running','waiting_client'))";

pub(super) struct RetentionBatch {
    pub(super) removed_interactions: Vec<String>,
    pub(super) removed_rejections: u64,
    pub(super) has_more: bool,
}

impl ObservationStore {
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.update_retention",
        skip_all
    )]
    /// 保留期只影响扫描水位与 Debug manifest；Observation 行不再保存到期时间。
    pub async fn update_retention(&self, days: u32) -> anyhow::Result<()> {
        self.debug_trace_index().update_retention(days).await?;
        Ok(())
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.purge_expired",
        skip_all
    )]
    pub(super) async fn purge_expired_batch(&self, cutoff: i64) -> anyhow::Result<RetentionBatch> {
        match self {
            Self::Sqlite(p, _) => {
                // 一个交互可能拥有大量 Run；不把多个交互的级联工作并入同一写租约。
                let mut connection = p.acquire_write().await?;
                let mut tx = connection.begin().await?;
                let rejected = sqlx::query("DELETE FROM rejected_request_observations WHERE occurred_at<=?1 AND id IN (SELECT id FROM rejected_request_observations WHERE occurred_at<=?1 ORDER BY occurred_at,id LIMIT ?2)")
                    .bind(cutoff)
                    .bind(REJECTION_BATCH_SIZE)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                let interaction: Option<String> = sqlx::query_scalar(SQLITE_NEXT_EXPIRED)
                    .bind(cutoff)
                    .fetch_optional(&mut *tx)
                    .await?;
                if let Some(id) = &interaction {
                    // 先记下即将失去引用的内容，删除事件后在同一事务里入 GC 队列。
                    let content_ids: Vec<i64> = sqlx::query_scalar(SQLITE_EXPIRED_CONTENT_IDS)
                        .bind(id)
                        .fetch_all(&mut *tx)
                        .await?;
                    // 先集合删除叶子，避免父级逐 Run 重复触发多层外键查找。
                    // 仍在同一事务中收口，不能向续接或详情读取暴露半个交互。
                    for sql in [
                        "DELETE FROM history_search_segments WHERE interaction_id=?",
                        "DELETE FROM history_search_progress WHERE interaction_id=?",
                        "DELETE FROM observation_event_contents WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=?)",
                        "DELETE FROM observation_tool_results WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=?)",
                        "DELETE FROM observation_events WHERE interaction_id=?",
                        "DELETE FROM credential_mapping_reuses WHERE interaction_id=?",
                        "DELETE FROM observation_pending_tools WHERE interaction_id=?",
                        "DELETE FROM observation_tail_sources WHERE interaction_id=?",
                        "DELETE FROM target_attempt_observations WHERE interaction_id=?",
                        "DELETE FROM model_turn_observations WHERE interaction_id=?",
                        "DELETE FROM inference_run_observations WHERE interaction_id=?",
                        "DELETE FROM interaction_observations WHERE id=?",
                    ] {
                        sqlx::query(sql).bind(id).execute(&mut *tx).await?;
                    }
                    crate::content_store::sqlite::enqueue_unreferenced(&mut tx, &content_ids)
                        .await?;
                }
                tx.commit().await?;
                connection.finish().await?;

                Ok(RetentionBatch {
                    has_more: rejected == REJECTION_BATCH_SIZE as u64 || interaction.is_some(),
                    removed_interactions: interaction.into_iter().collect(),
                    removed_rejections: rejected,
                })
            }
            Self::Postgres(p, _) => {
                let mut tx = p.begin().await?;
                let rejected = sqlx::query("DELETE FROM rejected_request_observations WHERE occurred_at<=$1 AND id IN (SELECT id FROM rejected_request_observations WHERE occurred_at<=$1 ORDER BY occurred_at,id LIMIT $2)")
                    .bind(cutoff)
                    .bind(REJECTION_BATCH_SIZE)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                // 锁住通过资格核验的父记录，直到子记录与父记录一起提交；
                // 等待行锁后的 READ COMMITTED 重查保留已经续接的交互。
                let interaction: Option<String> = sqlx::query_scalar("SELECT id FROM interaction_observations WHERE last_active_at<=$1 AND status<>'running' ORDER BY last_active_at,id LIMIT 1 FOR UPDATE")
                    .bind(cutoff)
                    .fetch_optional(&mut *tx)
                    .await?;
                if let Some(id) = &interaction {
                    let content_ids: Vec<i64> = sqlx::query_scalar(
                        "SELECT content_id FROM observation_event_contents WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=$1) UNION SELECT content_id FROM observation_tool_results WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=$1)",
                    )
                    .bind(id)
                    .fetch_all(&mut *tx)
                    .await?;
                    for sql in [
                        "DELETE FROM history_search_progress WHERE interaction_id=$1",
                        "DELETE FROM history_search_segments WHERE interaction_id=$1",
                        "DELETE FROM observation_event_contents WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=$1)",
                        "DELETE FROM observation_tool_results WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id=$1)",
                        "DELETE FROM observation_events WHERE interaction_id=$1",
                        "DELETE FROM credential_mapping_reuses WHERE interaction_id=$1",
                        "DELETE FROM observation_pending_tools WHERE interaction_id=$1",
                        "DELETE FROM observation_tail_sources WHERE interaction_id=$1",
                        "DELETE FROM target_attempt_observations WHERE interaction_id=$1",
                        "DELETE FROM model_turn_observations WHERE interaction_id=$1",
                        "DELETE FROM inference_run_observations WHERE interaction_id=$1",
                        "DELETE FROM interaction_observations WHERE id=$1",
                    ] {
                        sqlx::query(sql).bind(id).execute(&mut *tx).await?;
                    }
                    crate::content_store::postgres::enqueue_unreferenced(&mut tx, &content_ids)
                        .await?;
                }
                tx.commit().await?;

                Ok(RetentionBatch {
                    has_more: rejected == REJECTION_BATCH_SIZE as u64 || interaction.is_some(),
                    removed_interactions: interaction.into_iter().collect(),
                    removed_rejections: rejected,
                })
            }
        }
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.purge_read_ahead",
        skip_all
    )]
    /// SQLite 删除大交互时，写事务的大部分时间花在冷读被删行的二级索引页上，期间独占全库
    /// 写租约。删除前在写租约外按同一谓词把这些页读一遍，写事务只剩热页上的修改。
    /// 只读，不改变删除语义：写事务重新选择候选，候选在此期间被续接或删除时只是白读。
    /// PostgreSQL 没有全库写租约，不预读。
    pub(super) async fn read_ahead_expired(&self, cutoff: i64) -> anyhow::Result<()> {
        let Self::Sqlite(pool, _) = self else {
            return Ok(());
        };
        let Some(id) = sqlx::query_scalar::<_, String>(SQLITE_NEXT_EXPIRED)
            .bind(cutoff)
            .fetch_optional(pool)
            .await?
        else {
            return Ok(());
        };
        let content_ids: Vec<i64> = sqlx::query_scalar(SQLITE_EXPIRED_CONTENT_IDS)
            .bind(&id)
            .fetch_all(pool)
            .await?;
        crate::content_store::sqlite::read_ahead_unreferenced(pool, &content_ids).await?;
        for (table, rows) in SQLITE_PURGED_ROWS {
            // 部分索引要把谓词写进查询才能 INDEXED BY；实测预读它们对写事务耗时没有可见收益。
            let columns: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT il.name, ii.name FROM pragma_index_list(?1) il JOIN pragma_index_info(il.name) ii WHERE il.partial=0 ORDER BY il.seq, ii.seqno",
            )
            .bind(table)
            .fetch_all(pool)
            .await?;
            for index in columns.chunk_by(|left, right| left.0 == right.0) {
                // 表达式索引的键没有列名，无法按键回查。
                let Some(keys) = index
                    .iter()
                    .map(|(_, column)| column.as_deref().map(quote_identifier))
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                let keys = keys.join(",");
                // 按被删行的键回查该索引，读入写事务删除这些索引项时要改的页。
                let sql = format!(
                    "SELECT COUNT(*) FROM {table} INDEXED BY {} WHERE ({keys}) IN (SELECT {keys} FROM {table} WHERE {rows})",
                    quote_identifier(&index[0].0),
                );
                // 表名与谓词是本文件常量，索引名与列名取自本库 schema 并按标识符转义。
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(&id)
                    .fetch_one(pool)
                    .await?;
            }
        }
        Ok(())
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.manifest.mark_clear_tombstones",
        skip_all
    )]
    pub async fn mark_clear_tombstones(&self) -> anyhow::Result<ClearHistoryResult> {
        match self {
            Self::Sqlite(p, _) => {
                // SQL 仅冻结 owner 快照；文件 tombstone 不占用数据库写锁或协调器。
                let mut tx = p.begin().await?;
                let skipped:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let interactions:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status NOT IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let rejected: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM rejected_request_observations")
                        .fetch_one(&mut *tx)
                        .await?;
                let owners: Vec<String> = sqlx::query_scalar("SELECT r.id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.status NOT IN ('running','waiting_client')").fetch_all(&mut *tx).await?;
                let rejections: Vec<String> =
                    sqlx::query_scalar("SELECT id FROM rejected_request_observations")
                        .fetch_all(&mut *tx)
                        .await?;
                tx.commit().await?;
                self.tombstone_owners(
                    &owners.into_iter().collect(),
                    &rejections.into_iter().collect(),
                )
                .await?;
                Ok(ClearHistoryResult {
                    deleted_interactions: interactions.max(0) as u64,
                    deleted_rejections: rejected.max(0) as u64,
                    skipped_active: skipped.max(0) as u64,
                })
            }
            Self::Postgres(p, _) => {
                let mut tx = p.begin().await?;
                let skipped:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let interactions:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status NOT IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let rejected: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM rejected_request_observations")
                        .fetch_one(&mut *tx)
                        .await?;
                let owners: Vec<String> = sqlx::query_scalar("SELECT r.id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.status NOT IN ('running','waiting_client')").fetch_all(&mut *tx).await?;
                let rejections: Vec<String> =
                    sqlx::query_scalar("SELECT id FROM rejected_request_observations")
                        .fetch_all(&mut *tx)
                        .await?;
                self.tombstone_owners(
                    &owners.into_iter().collect(),
                    &rejections.into_iter().collect(),
                )
                .await?;
                tx.commit().await?;
                Ok(ClearHistoryResult {
                    deleted_interactions: interactions.max(0) as u64,
                    deleted_rejections: rejected.max(0) as u64,
                    skipped_active: skipped.max(0) as u64,
                })
            }
        }
    }
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.purge_clear",
        skip_all
    )]
    pub async fn purge_clear_rows(&self) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Sqlite(p, _) => {
                let deleted = {
                    let mut connection = p.acquire_write().await?;
                    let mut tx = connection.begin().await?;
                    sqlx::query("DELETE FROM rejected_request_observations")
                        .execute(&mut *tx)
                        .await?;
                    // 引用行随交互级联删除；先记下将失去引用的正文，删除后在同一事务入 GC 队列。
                    let content_ids: Vec<i64> = sqlx::query_scalar(CLEARED_CONTENT_IDS)
                        .fetch_all(&mut *tx)
                        .await?;
                    // 叶子显式清理；外键级联只是兜底，不能让登记行先于父行留下孤儿。
                    sqlx::query("DELETE FROM history_search_segments WHERE interaction_id IN (SELECT id FROM interaction_observations WHERE status NOT IN ('running','waiting_client'))")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("DELETE FROM history_search_progress WHERE interaction_id IN (SELECT id FROM interaction_observations WHERE status NOT IN ('running','waiting_client'))")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("DELETE FROM credential_mapping_reuses WHERE interaction_id IN (SELECT id FROM interaction_observations WHERE status NOT IN ('running','waiting_client'))")
                        .execute(&mut *tx)
                        .await?;
                    let deleted = sqlx::query_scalar("DELETE FROM interaction_observations WHERE status NOT IN ('running','waiting_client') RETURNING id").fetch_all(&mut *tx).await?;
                    crate::content_store::sqlite::enqueue_unreferenced(&mut tx, &content_ids)
                        .await?;
                    tx.commit().await?;
                    connection.finish().await?;
                    deleted
                };

                self.remove_orphan_manifests().await?;
                Ok(deleted)
            }
            Self::Postgres(p, _) => {
                let mut tx = p.begin().await?;
                sqlx::query("DELETE FROM rejected_request_observations")
                    .execute(&mut *tx)
                    .await?;
                // Match the indexer's interaction-before-progress lock order.
                let interaction_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM interaction_observations WHERE status NOT IN ('running','waiting_client') ORDER BY id FOR UPDATE")
                    .fetch_all(&mut *tx)
                    .await?;
                // 引用行随交互级联删除；先记下将失去引用的正文，删除后在同一事务入 GC 队列。
                let content_ids: Vec<i64> = sqlx::query_scalar("SELECT content_id FROM observation_event_contents WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id = ANY($1)) UNION SELECT content_id FROM observation_tool_results WHERE sequence IN (SELECT sequence FROM observation_events WHERE interaction_id = ANY($1))")
                    .bind(&interaction_ids)
                    .fetch_all(&mut *tx)
                    .await?;
                // 叶子显式清理；外键级联只是兜底，不能让登记行先于父行留下孤儿。
                sqlx::query("DELETE FROM history_search_progress WHERE interaction_id = ANY($1)")
                    .bind(&interaction_ids)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM history_search_segments WHERE interaction_id = ANY($1)")
                    .bind(&interaction_ids)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM credential_mapping_reuses WHERE interaction_id = ANY($1)")
                    .bind(&interaction_ids)
                    .execute(&mut *tx)
                    .await?;
                let deleted = sqlx::query_scalar(
                    "DELETE FROM interaction_observations WHERE id = ANY($1) RETURNING id",
                )
                .bind(&interaction_ids)
                .fetch_all(&mut *tx)
                .await?;
                crate::content_store::postgres::enqueue_unreferenced(&mut tx, &content_ids).await?;
                tx.commit().await?;

                self.remove_orphan_manifests().await?;
                Ok(deleted)
            }
        }
    }
    pub(super) async fn remove_orphan_manifests(&self) -> anyhow::Result<()> {
        let runs: Vec<String> = match self {
            Self::Sqlite(pool, _) => {
                sqlx::query_scalar("SELECT id FROM inference_run_observations")
                    .fetch_all(pool)
                    .await?
            }
            Self::Postgres(pool, _) => {
                sqlx::query_scalar("SELECT id FROM inference_run_observations")
                    .fetch_all(pool)
                    .await?
            }
        };
        let rejections: Vec<String> = match self {
            Self::Sqlite(pool, _) => {
                sqlx::query_scalar("SELECT id FROM rejected_request_observations")
                    .fetch_all(pool)
                    .await?
            }
            Self::Postgres(pool, _) => {
                sqlx::query_scalar("SELECT id FROM rejected_request_observations")
                    .fetch_all(pool)
                    .await?
            }
        };
        let runs: HashSet<String> = runs.into_iter().collect();
        let rejections: HashSet<String> = rejections.into_iter().collect();
        for entry in self.debug_trace_index().list() {
            if entry.run_id.as_ref().is_some_and(|id| !runs.contains(id))
                || entry
                    .rejection_id
                    .as_ref()
                    .is_some_and(|id| !rejections.contains(id))
            {
                self.debug_trace_index().remove(&entry.trace_id).await?;
            }
        }
        Ok(())
    }
    async fn tombstone_owners(
        &self,
        runs: &HashSet<String>,
        rejections: &HashSet<String>,
    ) -> anyhow::Result<()> {
        for mut entry in self.debug_trace_index().list() {
            if entry.run_id.as_ref().is_some_and(|id| runs.contains(id))
                || entry
                    .rejection_id
                    .as_ref()
                    .is_some_and(|id| rejections.contains(id))
            {
                entry.tombstoned = true;
                self.debug_trace_index().update(entry).await?;
            }
        }
        Ok(())
    }
    pub async fn manifest_ids(&self) -> anyhow::Result<(HashSet<String>, HashSet<String>)> {
        self.debug_trace_index().manifest_ids().await
    }
    pub async fn delete_manifests(&self, ids: &[String]) -> anyhow::Result<()> {
        self.debug_trace_index().delete_manifests(ids).await
    }
    pub async fn mark_expired_tombstones(&self, now: i64) -> anyhow::Result<Vec<String>> {
        let sql = "SELECT r.id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.status<>'running'";
        let eligible: Vec<String> = match self {
            Self::Sqlite(pool, _) => sqlx::query_scalar(sql).fetch_all(pool).await?,
            Self::Postgres(pool, _) => sqlx::query_scalar(sql).fetch_all(pool).await?,
        };
        let eligible: HashSet<String> = eligible.into_iter().collect();
        let mut ids = Vec::new();
        for mut entry in self.debug_trace_index().list() {
            let expired = entry.completed_at.is_some() && entry.expires_at <= now;
            if entry.tombstoned
                || (expired
                    && (entry.rejection_id.is_some()
                        || entry
                            .run_id
                            .as_ref()
                            .is_some_and(|id| eligible.contains(id))))
            {
                entry.tombstoned = true;
                ids.push(entry.trace_id.clone());
                self.debug_trace_index().update(entry).await?;
            }
        }
        Ok(ids)
    }
    pub async fn debug_manifest_counts(&self) -> anyhow::Result<(u64, u64)> {
        self.debug_trace_index().debug_manifest_counts().await
    }
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
