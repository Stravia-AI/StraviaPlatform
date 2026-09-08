use super::{
    store::ObservationStore,
    types::{ClearHistoryResult, TraceManifest},
};
use sqlx::Connection;
use std::collections::HashSet;

impl ObservationStore {
    pub async fn update_retention(&self, days: u32) -> anyhow::Result<()> {
        let ttl = i64::from(days).saturating_mul(86_400_000);
        match self {
            Self::Sqlite(p) => {
                let mut tx = p.begin().await?;
                for sql in [
                    "UPDATE interaction_observations SET expires_at=last_active_at+?",
                    "UPDATE inference_run_observations SET expires_at=last_active_at+?",
                    "UPDATE observation_events SET expires_at=occurred_at+?",
                    "UPDATE rejected_request_observations SET expires_at=occurred_at+?",
                    "UPDATE debug_trace_manifests SET expires_at=created_at+?",
                ] {
                    sqlx::query(sql).bind(ttl).execute(&mut *tx).await?;
                }
                tx.commit().await?
            }
            Self::Postgres(p) => {
                let mut tx = p.begin().await?;
                for sql in [
                    "UPDATE interaction_observations SET expires_at=last_active_at+$1",
                    "UPDATE inference_run_observations SET expires_at=last_active_at+$1",
                    "UPDATE observation_events SET expires_at=occurred_at+$1",
                    "UPDATE rejected_request_observations SET expires_at=occurred_at+$1",
                    "UPDATE debug_trace_manifests SET expires_at=created_at+$1",
                ] {
                    sqlx::query(sql).bind(ttl).execute(&mut *tx).await?;
                }
                tx.commit().await?
            }
        }
        Ok(())
    }

    pub async fn manifest_ids(&self) -> anyhow::Result<(HashSet<String>, HashSet<String>)> {
        let rows: Vec<(String, bool)> = match self {
            Self::Sqlite(p) => {
                sqlx::query_as("SELECT trace_id,tombstoned FROM debug_trace_manifests")
                    .fetch_all(p)
                    .await?
            }
            Self::Postgres(p) => {
                sqlx::query_as("SELECT trace_id,tombstoned FROM debug_trace_manifests")
                    .fetch_all(p)
                    .await?
            }
        };
        let mut retained = HashSet::new();
        let mut tomb = HashSet::new();
        for (id, is_tombstoned) in rows {
            if is_tombstoned {
                tomb.insert(id);
            } else {
                retained.insert(id);
            }
        }
        Ok((retained, tomb))
    }
    pub async fn delete_manifests(&self, ids: &[String]) -> anyhow::Result<()> {
        for id in ids {
            match self {
                Self::Sqlite(p) => {
                    sqlx::query(
                        "DELETE FROM debug_trace_manifests WHERE trace_id=? AND tombstoned=1",
                    )
                    .bind(id)
                    .execute(p)
                    .await?;
                }
                Self::Postgres(p) => {
                    sqlx::query(
                        "DELETE FROM debug_trace_manifests WHERE trace_id=$1 AND tombstoned=TRUE",
                    )
                    .bind(id)
                    .execute(p)
                    .await?;
                }
            };
        }
        Ok(())
    }

    pub async fn mark_expired_tombstones(&self, now: i64) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Sqlite(p) => {
                sqlx::query("UPDATE debug_trace_manifests SET tombstoned=1 WHERE expires_at<=? AND (rejection_id IS NOT NULL OR run_id IN (SELECT r.id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.status<>'running'))")
                    .bind(now)
                    .execute(p)
                    .await?;
                Ok(sqlx::query_scalar(
                    "SELECT trace_id FROM debug_trace_manifests WHERE tombstoned=1",
                )
                .fetch_all(p)
                .await?)
            }
            Self::Postgres(p) => {
                sqlx::query(
                    "UPDATE debug_trace_manifests SET tombstoned=TRUE WHERE expires_at<=$1 AND (rejection_id IS NOT NULL OR run_id IN (SELECT r.id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.status<>'running'))",
                )
                .bind(now)
                .execute(p)
                .await?;
                Ok(sqlx::query_scalar(
                    "SELECT trace_id FROM debug_trace_manifests WHERE tombstoned=TRUE",
                )
                .fetch_all(p)
                .await?)
            }
        }
    }
    pub async fn purge_expired_rows(&self, now: i64) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Sqlite(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("DELETE FROM rejected_request_observations WHERE expires_at<=? AND NOT EXISTS (SELECT 1 FROM debug_trace_manifests m WHERE m.rejection_id=rejected_request_observations.id)").bind(now).execute(&mut *tx).await?;
                let deleted = sqlx::query_scalar("DELETE FROM interaction_observations WHERE expires_at<=? AND status<>'running' AND NOT EXISTS (SELECT 1 FROM inference_run_observations r JOIN debug_trace_manifests m ON m.run_id=r.id WHERE r.interaction_id=interaction_observations.id) RETURNING id").bind(now).fetch_all(&mut *tx).await?;
                tx.commit().await?;
                Ok(deleted)
            }
            Self::Postgres(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("DELETE FROM rejected_request_observations WHERE expires_at<=$1 AND NOT EXISTS (SELECT 1 FROM debug_trace_manifests m WHERE m.rejection_id=rejected_request_observations.id)").bind(now).execute(&mut *tx).await?;
                let deleted = sqlx::query_scalar("DELETE FROM interaction_observations WHERE expires_at<=$1 AND status<>'running' AND NOT EXISTS (SELECT 1 FROM inference_run_observations r JOIN debug_trace_manifests m ON m.run_id=r.id WHERE r.interaction_id=interaction_observations.id) RETURNING id").bind(now).fetch_all(&mut *tx).await?;
                tx.commit().await?;
                Ok(deleted)
            }
        }
    }

    pub async fn mark_clear_tombstones(&self) -> anyhow::Result<ClearHistoryResult> {
        match self {
            Self::Sqlite(p) => {
                let mut connection = p.acquire().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                let skipped:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let interactions:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status NOT IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let rejected: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM rejected_request_observations")
                        .fetch_one(&mut *tx)
                        .await?;
                sqlx::query("UPDATE debug_trace_manifests SET tombstoned=1 WHERE run_id IN (SELECT r.id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.status NOT IN ('running','waiting_client')) OR rejection_id IS NOT NULL").execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(ClearHistoryResult {
                    deleted_interactions: interactions.max(0) as u64,
                    deleted_rejections: rejected.max(0) as u64,
                    skipped_active: skipped.max(0) as u64,
                })
            }
            Self::Postgres(p) => {
                let mut tx = p.begin().await?;
                let skipped:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let interactions:i64=sqlx::query_scalar("SELECT COUNT(*) FROM interaction_observations WHERE status NOT IN ('running','waiting_client')").fetch_one(&mut *tx).await?;
                let rejected: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM rejected_request_observations")
                        .fetch_one(&mut *tx)
                        .await?;
                sqlx::query("UPDATE debug_trace_manifests SET tombstoned=TRUE WHERE run_id IN (SELECT r.id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.status NOT IN ('running','waiting_client')) OR rejection_id IS NOT NULL").execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(ClearHistoryResult {
                    deleted_interactions: interactions.max(0) as u64,
                    deleted_rejections: rejected.max(0) as u64,
                    skipped_active: skipped.max(0) as u64,
                })
            }
        }
    }
    pub async fn purge_clear_rows(&self) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Sqlite(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("DELETE FROM rejected_request_observations WHERE NOT EXISTS (SELECT 1 FROM debug_trace_manifests m WHERE m.rejection_id=rejected_request_observations.id)").execute(&mut *tx).await?;
                let deleted = sqlx::query_scalar("DELETE FROM interaction_observations WHERE status NOT IN ('running','waiting_client') AND NOT EXISTS (SELECT 1 FROM inference_run_observations r JOIN debug_trace_manifests m ON m.run_id=r.id WHERE r.interaction_id=interaction_observations.id) RETURNING id").fetch_all(&mut *tx).await?;
                tx.commit().await?;
                Ok(deleted)
            }
            Self::Postgres(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("DELETE FROM rejected_request_observations WHERE NOT EXISTS (SELECT 1 FROM debug_trace_manifests m WHERE m.rejection_id=rejected_request_observations.id)").execute(&mut *tx).await?;
                let deleted = sqlx::query_scalar("DELETE FROM interaction_observations WHERE status NOT IN ('running','waiting_client') AND NOT EXISTS (SELECT 1 FROM inference_run_observations r JOIN debug_trace_manifests m ON m.run_id=r.id WHERE r.interaction_id=interaction_observations.id) RETURNING id").fetch_all(&mut *tx).await?;
                tx.commit().await?;
                Ok(deleted)
            }
        }
    }
    pub async fn debug_manifest_counts(&self) -> anyhow::Result<(u64, u64)> {
        let (bytes,partial):(i64,i64)=match self{Self::Sqlite(p)=>sqlx::query_as("SELECT COALESCE(SUM(bytes_written),0),COALESCE(SUM(CASE WHEN status='partial' AND completed_at IS NOT NULL THEN 1 ELSE 0 END),0) FROM debug_trace_manifests WHERE tombstoned=0").fetch_one(p).await?,Self::Postgres(p)=>sqlx::query_as("SELECT COALESCE(SUM(bytes_written),0),COALESCE(SUM(CASE WHEN status='partial' AND completed_at IS NOT NULL THEN 1 ELSE 0 END),0) FROM debug_trace_manifests WHERE tombstoned=FALSE").fetch_one(p).await?};
        Ok((bytes.max(0) as u64, partial.max(0) as u64))
    }

    pub async fn save_manifest(
        &self,
        run_id: Option<&str>,
        rejection_id: Option<&str>,
        m: &TraceManifest,
        created: i64,
        expires: i64,
        completed: bool,
    ) -> anyhow::Result<()> {
        let reason = m.reasons.join(",");
        let completed_at = completed.then_some(created);
        match self {
            Self::Sqlite(p) => {
                sqlx::query("INSERT INTO debug_trace_manifests (trace_id,run_id,rejection_id,relative_directory,bytes_written,event_count,status,partial_reason,created_at,completed_at,expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(trace_id) DO UPDATE SET bytes_written=excluded.bytes_written,event_count=excluded.event_count,status=excluded.status,partial_reason=excluded.partial_reason,completed_at=excluded.completed_at").bind(&m.trace_id).bind(run_id).bind(rejection_id).bind(&m.trace_id).bind(m.bytes_written as i64).bind(m.event_count as i64).bind(&m.status).bind(if reason.is_empty(){None}else{Some(reason)}).bind(created).bind(completed_at).bind(expires).execute(p).await?;
            }
            Self::Postgres(p) => {
                sqlx::query("INSERT INTO debug_trace_manifests (trace_id,run_id,rejection_id,relative_directory,bytes_written,event_count,status,partial_reason,created_at,completed_at,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT(trace_id) DO UPDATE SET bytes_written=EXCLUDED.bytes_written,event_count=EXCLUDED.event_count,status=EXCLUDED.status,partial_reason=EXCLUDED.partial_reason,completed_at=EXCLUDED.completed_at").bind(&m.trace_id).bind(run_id).bind(rejection_id).bind(&m.trace_id).bind(m.bytes_written as i64).bind(m.event_count as i64).bind(&m.status).bind(if reason.is_empty(){None}else{Some(reason)}).bind(created).bind(completed_at).bind(expires).execute(p).await?;
            }
        }
        Ok(())
    }
}
