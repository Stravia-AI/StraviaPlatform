use super::{store::ObservationStore, types::ClearHistoryResult};
use std::collections::HashSet;

const REJECTION_BATCH_SIZE: i64 = 100;

pub(super) struct RetentionBatch {
    pub(super) removed_interactions: Vec<String>,
    pub(super) has_more: bool,
}

impl ObservationStore {
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.update_retention",
        skip_all
    )]
    pub async fn update_retention(&self, days: u32) -> anyhow::Result<()> {
        let ttl = i64::from(days).saturating_mul(86_400_000);
        match self {
            Self::Sqlite(p, _) => {
                let mut connection = p.acquire_write().await?;
                let mut tx = connection.begin().await?;
                for sql in [
                    "UPDATE interaction_observations SET expires_at=last_active_at+?",
                    "UPDATE inference_run_observations SET expires_at=last_active_at+?",
                    "UPDATE observation_events SET expires_at=occurred_at+?",
                    "UPDATE rejected_request_observations SET expires_at=occurred_at+?",
                ] {
                    sqlx::query(sql).bind(ttl).execute(&mut *tx).await?;
                }
                tx.commit().await?;
                connection.finish().await?;
            }
            Self::Postgres(p, _) => {
                let mut tx = p.begin().await?;
                for sql in [
                    "UPDATE interaction_observations SET expires_at=last_active_at+$1",
                    "UPDATE inference_run_observations SET expires_at=last_active_at+$1",
                    "UPDATE observation_events SET expires_at=occurred_at+$1",
                    "UPDATE rejected_request_observations SET expires_at=occurred_at+$1",
                ] {
                    sqlx::query(sql).bind(ttl).execute(&mut *tx).await?;
                }
                tx.commit().await?
            }
        }
        self.debug_trace_index().update_retention(days).await?;
        Ok(())
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.purge_expired",
        skip_all
    )]
    pub(super) async fn purge_expired_batch(&self, now: i64) -> anyhow::Result<RetentionBatch> {
        match self {
            Self::Sqlite(p, _) => {
                // 一个交互可能拥有大量 Run；不把多个交互的级联工作并入同一写租约。
                let mut connection = p.acquire_write().await?;
                let mut tx = connection.begin().await?;
                let rejected = sqlx::query("DELETE FROM rejected_request_observations WHERE expires_at<=?1 AND id IN (SELECT id FROM rejected_request_observations WHERE expires_at<=?1 ORDER BY expires_at,id LIMIT ?2)")
                    .bind(now)
                    .bind(REJECTION_BATCH_SIZE)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                let interaction: Option<String> = sqlx::query_scalar("SELECT id FROM interaction_observations WHERE expires_at<=? AND status<>'running' ORDER BY expires_at,id LIMIT 1")
                    .bind(now)
                    .fetch_optional(&mut *tx)
                    .await?;
                if let Some(id) = &interaction {
                    // 先集合删除叶子，避免父级逐 Run 重复触发多层外键查找。
                    // 仍在同一事务中收口，不能向续接或详情读取暴露半个交互。
                    for sql in [
                        "DELETE FROM observation_events WHERE interaction_id=?",
                        "DELETE FROM observation_pending_tools WHERE interaction_id=?",
                        "DELETE FROM observation_tail_sources WHERE interaction_id=?",
                        "DELETE FROM target_attempt_observations WHERE interaction_id=?",
                        "DELETE FROM model_turn_observations WHERE interaction_id=?",
                        "DELETE FROM inference_run_observations WHERE interaction_id=?",
                        "DELETE FROM interaction_observations WHERE id=?",
                    ] {
                        sqlx::query(sql).bind(id).execute(&mut *tx).await?;
                    }
                }
                tx.commit().await?;
                connection.finish().await?;

                Ok(RetentionBatch {
                    has_more: rejected == REJECTION_BATCH_SIZE as u64 || interaction.is_some(),
                    removed_interactions: interaction.into_iter().collect(),
                })
            }
            Self::Postgres(p, _) => {
                let mut tx = p.begin().await?;
                let rejected = sqlx::query("DELETE FROM rejected_request_observations WHERE expires_at<=$1 AND id IN (SELECT id FROM rejected_request_observations WHERE expires_at<=$1 ORDER BY expires_at,id LIMIT $2)")
                    .bind(now)
                    .bind(REJECTION_BATCH_SIZE)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                // 锁住通过资格核验的父记录，直到子记录与父记录一起提交；
                // 等待行锁后的 READ COMMITTED 重查保留已经续接的交互。
                let interaction: Option<String> = sqlx::query_scalar("SELECT id FROM interaction_observations WHERE expires_at<=$1 AND status<>'running' ORDER BY expires_at,id LIMIT 1 FOR UPDATE")
                    .bind(now)
                    .fetch_optional(&mut *tx)
                    .await?;
                if let Some(id) = &interaction {
                    for sql in [
                        "DELETE FROM observation_events WHERE interaction_id=$1",
                        "DELETE FROM observation_pending_tools WHERE interaction_id=$1",
                        "DELETE FROM observation_tail_sources WHERE interaction_id=$1",
                        "DELETE FROM target_attempt_observations WHERE interaction_id=$1",
                        "DELETE FROM model_turn_observations WHERE interaction_id=$1",
                        "DELETE FROM inference_run_observations WHERE interaction_id=$1",
                        "DELETE FROM interaction_observations WHERE id=$1",
                    ] {
                        sqlx::query(sql).bind(id).execute(&mut *tx).await?;
                    }
                }
                tx.commit().await?;

                Ok(RetentionBatch {
                    has_more: rejected == REJECTION_BATCH_SIZE as u64 || interaction.is_some(),
                    removed_interactions: interaction.into_iter().collect(),
                })
            }
        }
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
                    let deleted = sqlx::query_scalar("DELETE FROM interaction_observations WHERE status NOT IN ('running','waiting_client') RETURNING id").fetch_all(&mut *tx).await?;
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
                let deleted = sqlx::query_scalar("DELETE FROM interaction_observations WHERE status NOT IN ('running','waiting_client') RETURNING id").fetch_all(&mut *tx).await?;
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
