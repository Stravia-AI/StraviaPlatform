use super::manifest_index::DebugTraceIndex;
use serde_json::Value;
use sqlx::{Connection, PgPool, Row, SqlitePool};
use std::sync::Arc;

use super::types::{
    ConfirmedUsage, IngressStart, ObservationEvent, RejectedOutcome, RunEvent, RunOutcome,
    RunStart, project_event_for_management,
};

/// 每个 Model Turn 的输出在 `visible_tail` 中以空行分隔，预览才能按 Markdown 段落换行。
pub(super) const TURN_SEPARATOR: &str = "\n\n";

#[derive(Clone)]
pub(super) enum ObservationStore {
    Sqlite(
        SqlitePool,
        Arc<DebugTraceIndex>,
        Arc<tokio::sync::Mutex<()>>,
    ),
    Postgres(PgPool, Arc<DebugTraceIndex>),
}

pub(super) struct Admission<'a> {
    pub start: &'a RunStart,
    pub metadata: Option<&'a super::RequestMetadata>,
    pub interaction_id: &'a str,
    pub generation_root_id: Option<&'a str>,
    pub generation_parent_id: Option<&'a str>,
    pub has_new_user: bool,
    /// Ingress receipt stamped by Run Attribution; the run's started_at and the
    /// merge windows derive from it.
    pub ingress_received_at: i64,
    pub parent_run_id: Option<&'a str>,
    pub parent_interaction_id: Option<&'a str>,
    pub debug_enabled: bool,
    pub inferred_retry: bool,
    pub grouping_reason: &'a str,
    pub diagnostic_source_run_id: Option<&'a str>,
    pub interrupt_parent: bool,
    pub now: i64,
    pub expires_at: i64,
}

pub(super) struct Rejection<'a> {
    pub ingress: &'a IngressStart,
    pub outcome: &'a RejectedOutcome,
    pub metadata: &'a super::RequestMetadata,
    pub debug_enabled: bool,
    pub occurred_at: i64,
    pub expires_at: i64,
    pub started_at: i64,
    pub duration_ms: i64,
}

impl ObservationStore {
    pub fn new(
        sqlite: Option<SqlitePool>,
        postgres: Option<PgPool>,
        index: Arc<DebugTraceIndex>,
        sqlite_write_gate: Option<Arc<tokio::sync::Mutex<()>>>,
    ) -> anyhow::Result<Self> {
        match (sqlite, postgres) {
            (Some(pool), None) => Ok(Self::Sqlite(
                pool,
                index,
                sqlite_write_gate.ok_or_else(|| {
                    anyhow::anyhow!("SQLite observation store requires a shared write gate")
                })?,
            )),
            (None, Some(pool)) => Ok(Self::Postgres(pool, index)),
            _ => anyhow::bail!("interaction observation requires exactly one SQL backend"),
        }
    }

    pub(super) fn debug_trace_index(&self) -> &DebugTraceIndex {
        match self {
            Self::Sqlite(_, index, _) | Self::Postgres(_, index) => index,
        }
    }

    async fn attempt_usage(&self, attempt_id: &str) -> anyhow::Result<Option<ConfirmedUsage>> {
        type UsageRow = (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        );
        let row: Option<UsageRow> = match self {
            Self::Sqlite(pool, _, _) => sqlx::query_as("SELECT input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens FROM target_attempt_observations WHERE id=? AND usage_recorded=1").bind(attempt_id).fetch_optional(pool).await?,
            Self::Postgres(pool, _) => sqlx::query_as("SELECT input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens FROM target_attempt_observations WHERE id=$1 AND usage_recorded").bind(attempt_id).fetch_optional(pool).await?,
        };
        Ok(row.map(
            |(
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
            )| ConfirmedUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                coverage: None,
            },
        ))
    }

    pub(super) async fn artifact_available(&self, id: &str, now: i64) -> anyhow::Result<bool> {
        Ok(match self {
            Self::Sqlite(pool, _, _) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=? AND state='ready' AND expires_at>?)")
                .bind(id).bind(now).fetch_one(pool).await?,
            Self::Postgres(pool, _) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=$1 AND state='ready' AND expires_at>$2)")
                .bind(id).bind(now).fetch_one(pool).await?,
        })
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.persist_tail_source",
        skip_all
    )]
    pub(super) async fn persist_tail_source(
        &self,
        run_id: &str,
        interaction_id: &str,
        principal: &str,
        last_unit_hash: &str,
        pending_tool_ids: &[String],
        delivered_at: i64,
        expires_at: i64,
    ) -> anyhow::Result<()> {
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin().await?;
                sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,?) WHERE id=?")
                    .bind(delivered_at).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO observation_tail_sources (run_id,interaction_id,principal,last_unit_hash,expires_at) VALUES (?,?,?,?,?) ON CONFLICT(run_id) DO UPDATE SET interaction_id=excluded.interaction_id,principal=excluded.principal,last_unit_hash=excluded.last_unit_hash,expires_at=excluded.expires_at")
                    .bind(run_id).bind(interaction_id).bind(principal).bind(last_unit_hash).bind(expires_at).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM observation_pending_tools WHERE run_id=?")
                    .bind(run_id)
                    .execute(&mut *tx)
                    .await?;
                for tool_id in pending_tool_ids {
                    sqlx::query("INSERT INTO observation_pending_tools (principal,tool_id,run_id,interaction_id,expires_at) VALUES (?,?,?,?,?)")
                        .bind(principal).bind(tool_id).bind(run_id).bind(interaction_id).bind(expires_at).execute(&mut *tx).await?;
                }
                tx.commit().await?;
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,$1) WHERE id=$2")
                    .bind(delivered_at).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO observation_tail_sources (run_id,interaction_id,principal,last_unit_hash,expires_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT(run_id) DO UPDATE SET interaction_id=EXCLUDED.interaction_id,principal=EXCLUDED.principal,last_unit_hash=EXCLUDED.last_unit_hash,expires_at=EXCLUDED.expires_at")
                    .bind(run_id).bind(interaction_id).bind(principal).bind(last_unit_hash).bind(expires_at).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM observation_pending_tools WHERE run_id=$1")
                    .bind(run_id)
                    .execute(&mut *tx)
                    .await?;
                for tool_id in pending_tool_ids {
                    sqlx::query("INSERT INTO observation_pending_tools (principal,tool_id,run_id,interaction_id,expires_at) VALUES ($1,$2,$3,$4,$5)")
                        .bind(principal).bind(tool_id).bind(run_id).bind(interaction_id).bind(expires_at).execute(&mut *tx).await?;
                }
                tx.commit().await?;
            }
        }
        Ok(())
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.set_tail_generation_node",
        skip_all
    )]
    pub(super) async fn set_tail_generation_node(
        &self,
        run_id: &str,
        generation_node_id: &str,
    ) -> anyhow::Result<()> {
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                sqlx::query(
                    "UPDATE observation_tail_sources SET generation_node_id=? WHERE run_id=?",
                )
                .bind(generation_node_id)
                .bind(run_id)
                .execute(pool)
                .await?;
            }
            Self::Postgres(pool, _) => {
                sqlx::query(
                    "UPDATE observation_tail_sources SET generation_node_id=$1 WHERE run_id=$2",
                )
                .bind(generation_node_id)
                .bind(run_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn tail_sources_by_hashes(
        &self,
        principal: &str,
        excluding: &str,
        hashes: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        Ok(match self {
            Self::Sqlite(pool, _, _) => {
                let mut builder = sqlx::QueryBuilder::new(
                    "SELECT run_id,interaction_id,generation_node_id FROM observation_tail_sources WHERE principal=",
                );
                builder.push_bind(principal);
                builder.push(" AND run_id<>");
                builder.push_bind(excluding);
                builder.push(" AND expires_at>");
                builder.push_bind(now);
                builder.push(" AND last_unit_hash IN (");
                let mut separated = builder.separated(", ");
                for hash in hashes {
                    separated.push_bind(hash);
                }
                separated.push_unseparated(")");
                builder.build_query_as().fetch_all(pool).await?
            }
            Self::Postgres(pool, _) => {
                let mut builder = sqlx::QueryBuilder::new(
                    "SELECT run_id,interaction_id,generation_node_id FROM observation_tail_sources WHERE principal=",
                );
                builder.push_bind(principal);
                builder.push(" AND run_id<>");
                builder.push_bind(excluding);
                builder.push(" AND expires_at>");
                builder.push_bind(now);
                builder.push(" AND last_unit_hash IN (");
                let mut separated = builder.separated(", ");
                for hash in hashes {
                    separated.push_bind(hash);
                }
                separated.push_unseparated(")");
                builder.build_query_as().fetch_all(pool).await?
            }
        })
    }

    pub(super) async fn pending_tool_sources(
        &self,
        principal: &str,
        tool_ids: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        if tool_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(match self {
            Self::Sqlite(pool, _, _) => {
                let mut builder = sqlx::QueryBuilder::new(
                    "SELECT tool_id,run_id,interaction_id FROM observation_pending_tools WHERE principal=",
                );
                builder.push_bind(principal);
                builder.push(" AND expires_at>");
                builder.push_bind(now);
                builder.push(" AND tool_id IN (");
                let mut separated = builder.separated(", ");
                for id in tool_ids {
                    separated.push_bind(id);
                }
                separated.push_unseparated(")");
                builder.build_query_as().fetch_all(pool).await?
            }
            Self::Postgres(pool, _) => {
                let mut builder = sqlx::QueryBuilder::new(
                    "SELECT tool_id,run_id,interaction_id FROM observation_pending_tools WHERE principal=",
                );
                builder.push_bind(principal);
                builder.push(" AND expires_at>");
                builder.push_bind(now);
                builder.push(" AND tool_id IN (");
                let mut separated = builder.separated(", ");
                for id in tool_ids {
                    separated.push_bind(id);
                }
                separated.push_unseparated(")");
                builder.build_query_as().fetch_all(pool).await?
            }
        })
    }

    pub(super) async fn client_tool_result_sources(
        &self,
        principal: &str,
        tool_ids: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<(String, Option<String>)>> {
        if tool_ids.is_empty() {
            return Ok(Vec::new());
        }
        // 保留部分索引的 IN 谓词；完整输入证明由 Attribution 校验，不能按 ID 全局去重。
        const SELECT: &str = "SELECT DISTINCT r.id,r.generation_node_id
            FROM observation_events e
            JOIN inference_run_observations r ON r.id=e.run_id
            JOIN interaction_observations i ON i.id=r.interaction_id
            WHERE e.kind IN ('client_tool_handoff','client_tool_result')
                AND e.kind='client_tool_result' AND i.principal=";
        Ok(match self {
            Self::Sqlite(pool, _, _) => {
                let mut builder = sqlx::QueryBuilder::new(SELECT);
                builder.push_bind(principal);
                for column in ["e.expires_at", "r.expires_at", "i.expires_at"] {
                    builder.push(" AND ").push(column).push(">").push_bind(now);
                }
                builder.push(" AND e.tool_id IN (");
                let mut separated = builder.separated(", ");
                for id in tool_ids {
                    separated.push_bind(id);
                }
                separated.push_unseparated(")");
                builder.push(" ORDER BY r.id LIMIT ");
                builder.push(super::tail::MAX_CANDIDATES + 1);
                builder.build_query_as().fetch_all(pool).await?
            }
            Self::Postgres(pool, _) => {
                let mut builder = sqlx::QueryBuilder::new(SELECT);
                builder.push_bind(principal);
                for column in ["e.expires_at", "r.expires_at", "i.expires_at"] {
                    builder.push(" AND ").push(column).push(">").push_bind(now);
                }
                builder.push(" AND e.tool_id IN (");
                let mut separated = builder.separated(", ");
                for id in tool_ids {
                    separated.push_bind(id);
                }
                separated.push_unseparated(")");
                builder.push(" ORDER BY r.id LIMIT ");
                builder.push(super::tail::MAX_CANDIDATES + 1);
                builder.build_query_as().fetch_all(pool).await?
            }
        })
    }

    pub(super) async fn tail_generation_node(
        &self,
        run_id: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(match self {
            Self::Sqlite(pool, _, _) => sqlx::query_scalar(
                "SELECT generation_node_id FROM observation_tail_sources WHERE run_id=?",
            )
            .bind(run_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
            Self::Postgres(pool, _) => sqlx::query_scalar(
                "SELECT generation_node_id FROM observation_tail_sources WHERE run_id=$1",
            )
            .bind(run_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
        })
    }

    pub(super) async fn delivery_completed_at(&self, run_id: &str) -> anyhow::Result<Option<i64>> {
        Ok(match self {
            Self::Sqlite(pool, _, _) => sqlx::query_scalar(
                "SELECT delivery_completed_at FROM inference_run_observations WHERE id=?",
            )
            .bind(run_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
            Self::Postgres(pool, _) => sqlx::query_scalar(
                "SELECT delivery_completed_at FROM inference_run_observations WHERE id=$1",
            )
            .bind(run_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
        })
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.admit_persist",
        skip_all
    )]
    pub async fn admit(&self, admission: Admission<'_>) -> anyhow::Result<ObservationEvent> {
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                // 先取得写锁，避免读取父状态后升级事务因并发写入而丢失子 Interaction。
                let _write_gate = write_gate.lock().await;
                let mut connection = pool.acquire().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                if admission.interrupt_parent
                    && let Some(parent) = admission.parent_interaction_id
                {
                    interrupt_predecessors_sqlite(&mut tx, parent, admission.now).await?;
                }
                if admission.parent_interaction_id.is_none()
                    && let Some(parent_run) = admission.parent_run_id
                {
                    supersede_waiting_parent_sqlite(
                        &mut tx,
                        admission.interaction_id,
                        parent_run,
                        &admission.start.id,
                        admission.now,
                    )
                    .await?;
                }
                let sequence = next_sqlite(&mut tx).await?;
                sqlx::query("INSERT OR IGNORE INTO interaction_observations (id,principal,api_key_id,api_key_name,generation_root_id,parent_interaction_id,root_id,root_run_id,first_route_id,first_model_display_name,status,started_at,last_active_at,last_event_sequence,expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,'running',?,?,?,?)")
                    .bind(admission.interaction_id).bind(&admission.start.principal).bind(&admission.start.api_key_id).bind(&admission.start.api_key_name)
                    .bind(admission.generation_root_id).bind(admission.parent_interaction_id)
                    .bind(admission.generation_root_id.unwrap_or(admission.interaction_id)).bind(&admission.start.id)
                    .bind(&admission.start.route_id).bind(&admission.start.model_display_name).bind(admission.now).bind(admission.now).bind(sequence).bind(admission.expires_at).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET status='running',last_active_at=?,last_event_sequence=?,expires_at=? WHERE id=?").bind(admission.now).bind(sequence).bind(admission.expires_at).bind(admission.interaction_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,generation_parent_id,ingress_protocol,route_id,model_display_name,status,debug_enabled,started_at,last_active_at,last_event_sequence,expires_at,request_model) VALUES (?,?,?,?,?,?,?,'running',?,?,?,?,?,?)")
                    .bind(&admission.start.id).bind(admission.interaction_id).bind(admission.parent_run_id).bind(admission.generation_parent_id)
                    .bind(&admission.start.ingress_protocol).bind(&admission.start.route_id).bind(&admission.start.model_display_name)
                    .bind(admission.debug_enabled).bind(admission.ingress_received_at).bind(admission.now).bind(sequence).bind(admission.expires_at)
                    .bind(admission.metadata.and_then(|metadata| metadata.model.as_deref())).execute(&mut *tx).await?;
                let payload = serde_json::json!({"route_id": admission.start.route_id, "model_display_name": admission.start.model_display_name, "debug_enabled": admission.debug_enabled, "inferred_retry": admission.inferred_retry, "grouping_reason": admission.grouping_reason, "ingress_received_at": admission.ingress_received_at, "parent_run_id": admission.parent_run_id, "generation_parent_id": admission.generation_parent_id, "diagnostic_source_run_id": admission.diagnostic_source_run_id, "has_new_user": admission.has_new_user, "parent_interaction_id": admission.parent_interaction_id, "root_id": admission.generation_root_id.unwrap_or(admission.interaction_id)});
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence,
                        occurred_at: admission.now,
                        interaction_id: Some(admission.interaction_id),
                        run_id: Some(&admission.start.id),
                        rejection_id: None,
                        kind: "run_admitted",
                        payload: &payload,
                        expires_at: admission.expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    sequence,
                    admission.now,
                    Some(admission.interaction_id),
                    Some(&admission.start.id),
                    None,
                    "run_admitted",
                    payload,
                ))
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                if admission.interrupt_parent
                    && let Some(parent) = admission.parent_interaction_id
                {
                    interrupt_predecessors_postgres(&mut tx, parent, admission.now).await?;
                }
                if admission.parent_interaction_id.is_none()
                    && let Some(parent_run) = admission.parent_run_id
                {
                    supersede_waiting_parent_postgres(
                        &mut tx,
                        admission.interaction_id,
                        parent_run,
                        &admission.start.id,
                        admission.now,
                    )
                    .await?;
                }
                let sequence: i64 =
                    sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                        .fetch_one(&mut *tx)
                        .await?;
                sqlx::query("INSERT INTO interaction_observations (id,principal,api_key_id,api_key_name,generation_root_id,parent_interaction_id,root_id,root_run_id,first_route_id,first_model_display_name,status,started_at,last_active_at,last_event_sequence,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'running',$11,$12,$13,$14) ON CONFLICT (id) DO NOTHING")
                    .bind(admission.interaction_id).bind(&admission.start.principal).bind(&admission.start.api_key_id).bind(&admission.start.api_key_name)
                    .bind(admission.generation_root_id).bind(admission.parent_interaction_id)
                    .bind(admission.generation_root_id.unwrap_or(admission.interaction_id)).bind(&admission.start.id)
                    .bind(&admission.start.route_id).bind(&admission.start.model_display_name).bind(admission.now).bind(admission.now).bind(sequence).bind(admission.expires_at).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET status='running',last_active_at=$1,last_event_sequence=$2,expires_at=$3 WHERE id=$4").bind(admission.now).bind(sequence).bind(admission.expires_at).bind(admission.interaction_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,generation_parent_id,ingress_protocol,route_id,model_display_name,status,debug_enabled,started_at,last_active_at,last_event_sequence,expires_at,request_model) VALUES ($1,$2,$3,$4,$5,$6,$7,'running',$8,$9,$10,$11,$12,$13)")
                    .bind(&admission.start.id).bind(admission.interaction_id).bind(admission.parent_run_id).bind(admission.generation_parent_id)
                    .bind(&admission.start.ingress_protocol).bind(&admission.start.route_id).bind(&admission.start.model_display_name)
                    .bind(admission.debug_enabled).bind(admission.ingress_received_at).bind(admission.now).bind(sequence).bind(admission.expires_at)
                    .bind(admission.metadata.and_then(|metadata| metadata.model.as_deref())).execute(&mut *tx).await?;
                let payload = serde_json::json!({"route_id": admission.start.route_id, "model_display_name": admission.start.model_display_name, "debug_enabled": admission.debug_enabled, "inferred_retry": admission.inferred_retry, "grouping_reason": admission.grouping_reason, "ingress_received_at": admission.ingress_received_at, "parent_run_id": admission.parent_run_id, "generation_parent_id": admission.generation_parent_id, "diagnostic_source_run_id": admission.diagnostic_source_run_id, "has_new_user": admission.has_new_user, "parent_interaction_id": admission.parent_interaction_id, "root_id": admission.generation_root_id.unwrap_or(admission.interaction_id)});
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence,
                        occurred_at: admission.now,
                        interaction_id: Some(admission.interaction_id),
                        run_id: Some(&admission.start.id),
                        rejection_id: None,
                        kind: "run_admitted",
                        payload: &payload,
                        expires_at: admission.expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    sequence,
                    admission.now,
                    Some(admission.interaction_id),
                    Some(&admission.start.id),
                    None,
                    "run_admitted",
                    payload,
                ))
            }
        }
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.mark_gap",
        skip_all
    )]
    pub async fn mark_observation_gap(&self, interaction_id: &str) -> anyhow::Result<bool> {
        let affected = match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                sqlx::query("UPDATE interaction_observations SET observation_gap=1 WHERE id=?")
                    .bind(interaction_id)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
            Self::Postgres(pool, _) => {
                sqlx::query("UPDATE interaction_observations SET observation_gap=TRUE WHERE id=$1")
                    .bind(interaction_id)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
        };
        Ok(affected != 0)
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.persist_input_preview",
        skip_all
    )]
    pub(super) async fn persist_input_preview(
        &self,
        interaction_id: &str,
        run_id: &str,
        preview: &str,
        now: i64,
        expires_at: i64,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let kind = "input_preview_recorded";
        let payload = serde_json::json!({"kind": kind, "text": preview});
        let sequence = match self {
            Self::Sqlite(pool, _, write_gate) => {
                // 没有 schema 级唯一约束，必须与所有事件写入串行化查询，事务锁即每个 run 的幂等边界。
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                let root_run_id: Option<String> = sqlx::query_scalar(
                    "SELECT i.root_run_id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id=? AND i.id=?",
                )
                .bind(run_id)
                .bind(interaction_id)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(root_run_id) = root_run_id else {
                    return Ok(None);
                };
                let recorded: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM observation_events WHERE interaction_id=? AND run_id=? AND kind=?)",
                )
                .bind(interaction_id)
                .bind(run_id)
                .bind(kind)
                .fetch_one(&mut *tx)
                .await?;
                if recorded {
                    return Ok(None);
                }
                if root_run_id == run_id {
                    sqlx::query("UPDATE interaction_observations SET input_preview=? WHERE id=? AND input_preview IS NULL")
                        .bind(preview).bind(interaction_id).execute(&mut *tx).await?;
                }
                let sequence = next_sqlite(&mut tx).await?;
                sqlx::query("UPDATE interaction_observations SET last_event_sequence=? WHERE id=?")
                    .bind(sequence)
                    .bind(interaction_id)
                    .execute(&mut *tx)
                    .await?;
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence,
                        occurred_at: now,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind,
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                sequence
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                // 只锁已持久化且属于该 interaction 的 run，再查事件，避免并发发布同时判定为空。
                let root_run_id: Option<String> = sqlx::query_scalar(
                    "SELECT i.root_run_id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id=$1 AND i.id=$2 FOR UPDATE OF r",
                )
                .bind(run_id)
                .bind(interaction_id)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(root_run_id) = root_run_id else {
                    return Ok(None);
                };
                let recorded: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM observation_events WHERE interaction_id=$1 AND run_id=$2 AND kind=$3)",
                )
                .bind(interaction_id)
                .bind(run_id)
                .bind(kind)
                .fetch_one(&mut *tx)
                .await?;
                if recorded {
                    return Ok(None);
                }
                if root_run_id == run_id {
                    sqlx::query("UPDATE interaction_observations SET input_preview=$1 WHERE id=$2 AND input_preview IS NULL")
                        .bind(preview).bind(interaction_id).execute(&mut *tx).await?;
                }
                let sequence = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                sqlx::query(
                    "UPDATE interaction_observations SET last_event_sequence=$1 WHERE id=$2",
                )
                .bind(sequence)
                .bind(interaction_id)
                .execute(&mut *tx)
                .await?;
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence,
                        occurred_at: now,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind,
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                sequence
            }
        };
        Ok(Some(ObservationEvent {
            sequence,
            occurred_at: now,
            interaction_id: Some(interaction_id.to_owned()),
            run_id: Some(run_id.to_owned()),
            rejection_id: None,
            kind: kind.to_owned(),
            payload,
        }))
    }

    /// The observation writer is the sole caller: one lineage lookup per received batch,
    /// then persist in input order. Only live, same-principal parent_run_id ancestors qualify.
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.filter_client_tool_results",
        skip_all
    )]
    pub(super) async fn filter_client_tool_results(
        &self,
        interaction_id: &str,
        run_id: &str,
        events: Vec<RunEvent>,
        now: i64,
    ) -> anyhow::Result<Vec<RunEvent>> {
        let mut ids: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ClientToolResult { tool_id, .. } if !tool_id.is_empty() => {
                    Some(tool_id.as_str())
                }
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let ids = serde_json::to_string(&ids)?;
        let encoded_rows: Vec<Vec<u8>> = match self {
            Self::Sqlite(pool, _, _) => {
                // SQLite 的部分索引匹配依赖 IN 列表顺序，必须与迁移中的谓词保持一致。
                let rows: Vec<Vec<u8>> = sqlx::query_scalar(
                    "WITH RECURSIVE lineage(id,parent_run_id,principal) AS (
                        SELECT r.id,r.parent_run_id,i.principal
                        FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                        WHERE r.id=?1 AND r.interaction_id=?2 AND r.expires_at>?3 AND i.expires_at>?3
                        UNION
                        SELECT r.id,r.parent_run_id,i.principal
                        FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                        JOIN lineage child ON r.id=child.parent_run_id AND i.principal=child.principal
                        WHERE r.expires_at>?3 AND i.expires_at>?3
                    ), ranked AS (
                        SELECT e.sequence,
                            ROW_NUMBER() OVER (PARTITION BY e.tool_id ORDER BY e.sequence DESC) AS position,
                            MAX(CASE WHEN e.kind='client_tool_handoff' THEN e.sequence END)
                                OVER (PARTITION BY e.tool_id) AS handoff_sequence
                        FROM lineage l JOIN observation_events e ON e.run_id=l.id
                        WHERE e.kind IN ('client_tool_handoff','client_tool_result') AND e.expires_at>?3
                            AND e.tool_id IN (SELECT value FROM json_each(?4))
                    )
                    SELECT e.payload FROM ranked r JOIN observation_events e ON e.sequence=r.sequence
                    WHERE r.position=1 AND r.handoff_sequence IS NOT NULL",
                )
                    .bind(run_id).bind(interaction_id).bind(now).bind(&ids).fetch_all(pool).await?;
                rows
            }
            Self::Postgres(pool, _) => sqlx::query_scalar(
                "WITH RECURSIVE lineage(id,parent_run_id,principal) AS (
                    SELECT r.id,r.parent_run_id,i.principal
                    FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                    WHERE r.id=$1 AND r.interaction_id=$2 AND r.expires_at>$3 AND i.expires_at>$3
                    UNION
                    SELECT r.id,r.parent_run_id,i.principal
                    FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                    JOIN lineage child ON r.id=child.parent_run_id AND i.principal=child.principal
                    WHERE r.expires_at>$3 AND i.expires_at>$3
                ), ranked AS (
                    SELECT e.sequence,
                        ROW_NUMBER() OVER (PARTITION BY e.tool_id ORDER BY e.sequence DESC) AS position,
                        MAX(CASE WHEN e.kind='client_tool_handoff' THEN e.sequence END)
                            OVER (PARTITION BY e.tool_id) AS handoff_sequence
                    FROM lineage l JOIN observation_events e ON e.run_id=l.id
                    WHERE e.kind IN ('client_tool_handoff','client_tool_result') AND e.expires_at>$3
                        AND e.tool_id IN (SELECT jsonb_array_elements_text($4::jsonb))
                )
                SELECT e.payload FROM ranked r JOIN observation_events e ON e.sequence=r.sequence
                WHERE r.position=1 AND r.handoff_sequence IS NOT NULL",
            )
                .bind(run_id).bind(interaction_id).bind(now).bind(&ids).fetch_all(pool).await?,
        };
        let rows: Vec<Value> = encoded_rows
            .into_iter()
            .map(|row| -> anyhow::Result<Value> {
                Ok(serde_json::from_slice(&crate::storage_codec::decode(
                    &row,
                )?)?)
            })
            .collect::<anyhow::Result<_>>()?;
        enum PreviousResult {
            Stored(Value, bool),
            Batch(usize),
        }
        let mut latest = std::collections::HashMap::with_capacity(rows.len());
        let mut known_calls = std::collections::HashSet::with_capacity(rows.len());
        for mut row in rows {
            if let Some(id) = row["tool_id"].as_str() {
                known_calls.insert(id.to_owned());
            }
            if let (Some(id), Some(is_error)) = (row["tool_id"].as_str(), row["is_error"].as_bool())
                && !row["content"].is_null()
            {
                let id = id.to_owned();
                latest.insert(id, PreviousResult::Stored(row["content"].take(), is_error));
            }
        }
        let mut persisted: Vec<RunEvent> = Vec::new();
        // 批内只保留已接收事件的位置，避免历史工具正文在逐项去重时再次复制。
        for event in events {
            let RunEvent::ClientToolResult {
                tool_id,
                content,
                is_error,
            } = &event
            else {
                anyhow::bail!("client tool result batch contains another event kind");
            };
            if !tool_id.is_empty()
                && latest.get(tool_id).is_some_and(|previous| match previous {
                    PreviousResult::Stored(previous, error) => {
                        previous == content && error == is_error
                    }
                    PreviousResult::Batch(index) => matches!(
                        &persisted[*index],
                        RunEvent::ClientToolResult { content: previous, is_error: error, .. }
                            if previous == content && error == is_error
                    ),
                })
            {
                continue;
            }
            if content.is_null() {
                latest.remove(tool_id);
            } else if known_calls.contains(tool_id) {
                latest.insert(tool_id.clone(), PreviousResult::Batch(persisted.len()));
            }
            persisted.push(event);
        }
        Ok(persisted)
    }

    pub async fn persist_run_event(
        &self,
        interaction_id: &str,
        run_id: &str,
        run_event: &RunEvent,
        now: i64,
        expires_at: i64,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        Ok(self
            .persist_run_events(
                interaction_id,
                run_id,
                &[(run_event.clone(), None, now)],
                expires_at,
            )
            .await?
            .pop())
    }
    #[tracing::instrument(target = "stravia::perf", name = "observation.writer.persist_events", skip_all, fields(event_count = events.len()))]
    pub(super) async fn persist_run_events(
        &self,
        interaction_id: &str,
        run_id: &str,
        events: &[(RunEvent, Option<String>, i64)],
        expires_at: i64,
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        let mut prepared = Vec::with_capacity(events.len());
        for (run_event, block_id, now) in events {
            let now = *now;
            if matches!(run_event, RunEvent::CredentialMappingsCreated { discoveries } if discoveries.is_empty())
            {
                continue;
            }
            if matches!(
                run_event,
                RunEvent::Wire { .. }
                    | RunEvent::ModelThinkingDelta { .. }
                    | RunEvent::ModelThinkingFinished { .. }
                    | RunEvent::ClientVisibleContentDelta { .. }
            ) {
                continue;
            }
            let mut payload = serde_json::to_value(run_event)?;
            if let RunEvent::TargetAttemptFinished {
                attempt_id,
                usage: None,
                ..
            } = run_event
                && let Some(usage) = events
                    .iter()
                    .rev()
                    .find_map(|(event, _, _)| match event {
                        RunEvent::UsageConfirmed {
                            attempt_id: id,
                            usage,
                            ..
                        } if id == attempt_id => Some(usage.clone()),
                        _ => None,
                    })
                    .or(self.attempt_usage(attempt_id).await?)
            {
                payload["usage"] = serde_json::to_value(usage)?;
            }
            if let RunEvent::UsageConfirmed {
                attempt_id, usage, ..
            } = run_event
            {
                let candidates: Vec<Vec<u8>> = match self {
                    Self::Sqlite(pool, _, _) => sqlx::query_scalar("SELECT payload FROM observation_events WHERE run_id=? AND kind='target_attempt_finished' ORDER BY sequence DESC").bind(run_id).fetch_all(pool).await?,
                    Self::Postgres(pool, _) => sqlx::query_scalar("SELECT payload FROM observation_events WHERE run_id=$1 AND kind='target_attempt_finished' ORDER BY sequence DESC").bind(run_id).fetch_all(pool).await?,
                };
                let mut terminal = None;
                for bytes in candidates {
                    let value: Value =
                        serde_json::from_slice(&crate::storage_codec::decode(&bytes)?)?;
                    if value["attempt_id"].as_str() == Some(attempt_id) {
                        terminal = Some(value);
                        break;
                    }
                }
                if let Some(terminal) = terminal {
                    payload = terminal;
                    payload["usage"] = serde_json::to_value(usage)?;
                    payload["kind"] = Value::String("target_attempt_finished".into());
                }
            }
            if matches!(run_event, RunEvent::DeliveryFinished { .. }) {
                let terminal: Option<Vec<u8>> = match self {
                    Self::Sqlite(pool, _, _) => sqlx::query_scalar("SELECT payload FROM observation_events WHERE run_id=? AND kind='run_finished' ORDER BY sequence DESC LIMIT 1").bind(run_id).fetch_optional(pool).await?,
                    Self::Postgres(pool, _) => sqlx::query_scalar("SELECT payload FROM observation_events WHERE run_id=$1 AND kind='run_finished' ORDER BY sequence DESC LIMIT 1").bind(run_id).fetch_optional(pool).await?,
                };
                if let Some(terminal) = terminal {
                    payload = serde_json::from_slice(&crate::storage_codec::decode(&terminal)?)?;
                    match run_event {
                        RunEvent::GenerationAssociated {
                            root_id, parent_id, ..
                        } => {
                            payload["generation_root_id"] = serde_json::to_value(root_id)?;
                            payload["generation_parent_id"] = serde_json::to_value(parent_id)?;
                        }
                        RunEvent::ClientOutputCommitted => {
                            payload["client_output_committed"] = Value::Bool(true)
                        }
                        RunEvent::DeliveryFinished { status, reason } => {
                            payload["delivery"] = serde_json::json!({"status":status,"reason":reason,"completed_at":now})
                        }
                        _ => unreachable!(),
                    }
                    payload["kind"] = Value::String("run_finished".into());
                }
            }
            if let Some(id) = block_id {
                payload["block_id"] = Value::String(id.clone());
            }
            if let RunEvent::NativeCompactionAssociated {
                source_generation_id,
                source_operation_id,
                ..
            } = run_event
            {
                let source = if let Some(generation) = source_generation_id {
                    self.generation_parent(generation).await?
                } else if let Some(operation) = source_operation_id {
                    match self {
                    Self::Sqlite(pool, _, _) => sqlx::query_as("SELECT e.interaction_id,e.run_id FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.kind='compaction_operation' AND e.operation_id=? AND r.expires_at>? ORDER BY e.sequence LIMIT 1").bind(operation).bind(now).fetch_optional(pool).await?,
                    Self::Postgres(pool, _) => sqlx::query_as("SELECT e.interaction_id,e.run_id FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.kind='compaction_operation' AND e.operation_id=$1 AND r.expires_at>$2 ORDER BY e.sequence LIMIT 1").bind(operation).bind(now).fetch_optional(pool).await?,
                }
                } else {
                    None
                };
                if let Some((interaction, run)) = source {
                    payload["source_interaction_id"] = Value::String(interaction);
                    payload["source_run_id"] = Value::String(run);
                }
            }
            let kind = payload["kind"]
                .as_str()
                .unwrap_or("observation_gap")
                .to_owned();

            if projection_signal(run_event)
                && !matches!(kind.as_str(), "target_attempt_finished" | "run_finished")
                && !matches!(run_event, RunEvent::UsageConfirmed { .. })
            {
                continue;
            }
            prepared.push((run_event, now, kind, payload));
        }
        if prepared.is_empty() {
            return Ok(Vec::new());
        }
        let durable_count = prepared
            .iter()
            .filter(|(_, _, kind, _)| kind != "usage_confirmed")
            .count();
        let mut result = Vec::with_capacity(durable_count);
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                // Acquire the writer lock before reading the projection cursor: a deferred
                // WAL read cannot upgrade after a concurrent admission/storage commit.
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                // 过滤后才按批预占：被跳过的 Wire/空 credential 不占号。
                let mut current_sequence: i64 = sqlx::query_scalar(
                    "SELECT last_event_sequence FROM inference_run_observations WHERE id=?",
                )
                .bind(run_id)
                .fetch_one(&mut *tx)
                .await?;
                let mut next_sequence = if durable_count > 0 {
                    next_sqlite_batch(&mut tx, durable_count).await?
                } else {
                    current_sequence
                };
                let mut status_changed = false;
                let mut visible = String::new();
                for (run_event, at, kind, payload) in prepared {
                    if kind == "usage_confirmed" {
                        apply_sqlite(
                            &mut tx,
                            interaction_id,
                            run_id,
                            run_event,
                            current_sequence,
                            at,
                        )
                        .await?;
                        continue;
                    }
                    let sequence = next_sequence;
                    next_sequence += 1;
                    current_sequence = sequence;
                    if let RunEvent::ClientVisibleContent { text, .. } = run_event {
                        visible.push_str(text);
                    } else {
                        if matches!(run_event, RunEvent::ModelTurnStarted { .. }) {
                            if visible.is_empty() {
                                sqlx::query("UPDATE interaction_observations SET visible_tail=visible_tail || ?1 WHERE id=?2 AND visible_tail<>'' AND substr(visible_tail,-2)<>?1").bind(TURN_SEPARATOR).bind(interaction_id).execute(&mut *tx).await?;
                            } else if !visible.ends_with(TURN_SEPARATOR) {
                                visible.push_str(TURN_SEPARATOR);
                            }
                        }
                        apply_sqlite(&mut tx, interaction_id, run_id, run_event, sequence, at)
                            .await?;
                    }
                    status_changed |= status_event(run_event);
                    {
                        insert_event_sqlite(
                            &mut tx,
                            EventInsert {
                                sequence,
                                occurred_at: at,
                                interaction_id: Some(interaction_id),
                                run_id: Some(run_id),
                                rejection_id: None,
                                kind: &kind,
                                payload: &payload,
                                expires_at,
                            },
                        )
                        .await?;
                    }
                    result.push(event(
                        sequence,
                        at,
                        Some(interaction_id),
                        Some(run_id),
                        None,
                        &kind,
                        payload,
                    ));
                }
                if let Some(last) = result.last() {
                    if status_changed {
                        recompute_status_sqlite(&mut tx, interaction_id, last.sequence).await?;
                    }
                    if !visible.is_empty() {
                        sqlx::query("UPDATE interaction_observations SET visible_tail=substr(visible_tail || ?, -4096) WHERE id=?").bind(&visible).bind(interaction_id).execute(&mut *tx).await?;
                    }
                    sqlx::query("UPDATE inference_run_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=?,expires_at=? WHERE id=?").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(run_id).execute(&mut *tx).await?;
                    sqlx::query("UPDATE interaction_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=?,expires_at=? WHERE id=?").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(interaction_id).execute(&mut *tx).await?;
                }
                tx.commit().await?;
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                // 按批取回实际序列值再配对：并发写入下 nextval 跨批不连续，不能自行累加。
                let mut current_sequence: i64 = sqlx::query_scalar(
                    "SELECT last_event_sequence FROM inference_run_observations WHERE id=$1",
                )
                .bind(run_id)
                .fetch_one(&mut *tx)
                .await?;
                let sequences: Vec<i64> = if durable_count > 0 {
                    sqlx::query_scalar(
                        "SELECT nextval('observation_event_sequence') FROM generate_series(1,$1)",
                    )
                    .bind(checked_event_count(durable_count)?)
                    .fetch_all(&mut *tx)
                    .await?
                } else {
                    Vec::new()
                };
                let mut sequences = sequences.into_iter();
                let mut status_changed = false;
                let mut visible = String::new();
                for (run_event, at, kind, payload) in prepared {
                    if kind == "usage_confirmed" {
                        apply_postgres(
                            &mut tx,
                            interaction_id,
                            run_id,
                            run_event,
                            current_sequence,
                            at,
                        )
                        .await?;
                        continue;
                    }
                    let sequence = sequences.next().expect("allocated durable event sequence");
                    current_sequence = sequence;
                    if let RunEvent::ClientVisibleContent { text, .. } = run_event {
                        visible.push_str(text);
                    } else {
                        if matches!(run_event, RunEvent::ModelTurnStarted { .. }) {
                            if visible.is_empty() {
                                sqlx::query("UPDATE interaction_observations SET visible_tail=visible_tail || $1 WHERE id=$2 AND visible_tail<>'' AND RIGHT(visible_tail,2)<>$1").bind(TURN_SEPARATOR).bind(interaction_id).execute(&mut *tx).await?;
                            } else if !visible.ends_with(TURN_SEPARATOR) {
                                visible.push_str(TURN_SEPARATOR);
                            }
                        }
                        apply_postgres(&mut tx, interaction_id, run_id, run_event, sequence, at)
                            .await?;
                    }
                    status_changed |= status_event(run_event);
                    {
                        insert_event_postgres(
                            &mut tx,
                            EventInsert {
                                sequence,
                                occurred_at: at,
                                interaction_id: Some(interaction_id),
                                run_id: Some(run_id),
                                rejection_id: None,
                                kind: &kind,
                                payload: &payload,
                                expires_at,
                            },
                        )
                        .await?;
                    }
                    result.push(event(
                        sequence,
                        at,
                        Some(interaction_id),
                        Some(run_id),
                        None,
                        &kind,
                        payload,
                    ));
                }
                if let Some(last) = result.last() {
                    if status_changed {
                        recompute_status_postgres(&mut tx, interaction_id, last.sequence).await?;
                    }
                    if !visible.is_empty() {
                        sqlx::query("UPDATE interaction_observations SET visible_tail=RIGHT(visible_tail || $1,4096) WHERE id=$2").bind(&visible).bind(interaction_id).execute(&mut *tx).await?;
                    }
                    sqlx::query("UPDATE inference_run_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2,expires_at=$3 WHERE id=$4").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(run_id).execute(&mut *tx).await?;
                    sqlx::query("UPDATE interaction_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2,expires_at=$3 WHERE id=$4").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(interaction_id).execute(&mut *tx).await?;
                }
                tx.commit().await?;
            }
        }
        Ok(result)
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.disconnect_waiting_client",
        skip_all
    )]
    pub(super) async fn disconnect_waiting_client(
        &self,
        run_id: &str,
        now: i64,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let payload = serde_json::json!({"status":"disconnected","reason":"client_disconnected"});
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                let interaction: Option<String> = sqlx::query_scalar("SELECT interaction_id FROM inference_run_observations WHERE id=? AND status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id)")
                    .bind(run_id).fetch_optional(&mut *tx).await?;
                let Some(interaction) = interaction else {
                    return Ok(None);
                };
                let evidence = client_tool_evidence_sqlite(&mut tx, &interaction).await?;
                if resolved_evidence(&evidence).contains(run_id) {
                    return Ok(None);
                }
                let row: Option<(String, i64)> = sqlx::query_as(
                    "UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_disconnected' WHERE id=? AND status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) RETURNING interaction_id,expires_at",
                ).bind(run_id).fetch_optional(&mut *tx).await?;
                let Some((interaction, expiry)) = row else {
                    return Ok(None);
                };
                let seq = next_sqlite(&mut tx).await?;
                sqlx::query(
                    "UPDATE inference_run_observations SET last_event_sequence=? WHERE id=?",
                )
                .bind(seq)
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
                recompute_status_sqlite(&mut tx, &interaction, seq).await?;
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: now,
                        interaction_id: Some(&interaction),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "run_state_changed",
                        payload: &payload,
                        expires_at: expiry,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(Some(event(
                    seq,
                    now,
                    Some(&interaction),
                    Some(run_id),
                    None,
                    "run_state_changed",
                    payload,
                )))
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let interaction: Option<String> = sqlx::query_scalar("SELECT interaction_id FROM inference_run_observations WHERE id=$1 AND status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id)")
                    .bind(run_id).fetch_optional(&mut *tx).await?;
                let Some(interaction) = interaction else {
                    return Ok(None);
                };
                let evidence = client_tool_evidence_postgres(&mut tx, &interaction).await?;
                if resolved_evidence(&evidence).contains(run_id) {
                    return Ok(None);
                }
                let row: Option<(String, i64)> = sqlx::query_as(
                    "UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_disconnected' WHERE id=$1 AND status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) RETURNING interaction_id,expires_at",
                ).bind(run_id).fetch_optional(&mut *tx).await?;
                let Some((interaction, expiry)) = row else {
                    return Ok(None);
                };
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                sqlx::query(
                    "UPDATE inference_run_observations SET last_event_sequence=$1 WHERE id=$2",
                )
                .bind(seq)
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
                recompute_status_postgres(&mut tx, &interaction, seq).await?;
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: now,
                        interaction_id: Some(&interaction),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "run_state_changed",
                        payload: &payload,
                        expires_at: expiry,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(Some(event(
                    seq,
                    now,
                    Some(&interaction),
                    Some(run_id),
                    None,
                    "run_state_changed",
                    payload,
                )))
            }
        }
    }

    /// 超时未收到工具回传的等待叶 Run 按 client_wait_expired 转为 disconnected。
    /// HTTP 客户端没有连接关闭信号，闲置窗口是平台唯一能用的判定；
    /// 已收齐回传或仍有 child 的 Run 不算等待叶，不在此转换。
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.expire_idle_waiting_client",
        skip_all
    )]
    pub(super) async fn expire_idle_waiting_client(
        &self,
        now: i64,
        idle_ms: i64,
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        let cutoff = now.saturating_sub(idle_ms);
        let payload = serde_json::json!({"status":"disconnected","reason":"client_wait_expired"});
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                let candidates: Vec<(String, String)> = sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations r WHERE r.status='waiting_client' AND r.last_active_at<=? AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id) ORDER BY r.interaction_id,r.id").bind(cutoff).fetch_all(&mut *tx).await?;
                let mut events = Vec::new();
                let mut index = 0;
                while index < candidates.len() {
                    let iid = candidates[index].0.clone();
                    let evidence = client_tool_evidence_sqlite(&mut tx, &iid).await?;
                    let resolved = resolved_evidence(&evidence);
                    let mut last_seq = None;
                    while index < candidates.len() && candidates[index].0 == iid {
                        let rid = candidates[index].1.clone();
                        index += 1;
                        if resolved.contains(rid.as_str()) {
                            continue;
                        }
                        let expiry: Option<i64> = sqlx::query_scalar("UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_wait_expired' WHERE id=? AND status='waiting_client' RETURNING expires_at").bind(&rid).fetch_optional(&mut *tx).await?;
                        let Some(expiry) = expiry else { continue };
                        let seq = next_sqlite(&mut tx).await?;
                        sqlx::query("UPDATE inference_run_observations SET last_event_sequence=? WHERE id=?").bind(seq).bind(&rid).execute(&mut *tx).await?;
                        insert_event_sqlite(
                            &mut tx,
                            EventInsert {
                                sequence: seq,
                                occurred_at: now,
                                interaction_id: Some(&iid),
                                run_id: Some(&rid),
                                rejection_id: None,
                                kind: "run_state_changed",
                                payload: &payload,
                                expires_at: expiry,
                            },
                        )
                        .await?;
                        events.push(event(
                            seq,
                            now,
                            Some(&iid),
                            Some(&rid),
                            None,
                            "run_state_changed",
                            payload.clone(),
                        ));
                        last_seq = Some(seq);
                    }
                    if let Some(seq) = last_seq {
                        recompute_status_sqlite(&mut tx, &iid, seq).await?;
                    }
                }
                tx.commit().await?;
                Ok(events)
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let candidates: Vec<(String, String)> = sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations r WHERE r.status='waiting_client' AND r.last_active_at<=$1 AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id) ORDER BY r.interaction_id,r.id").bind(cutoff).fetch_all(&mut *tx).await?;
                let mut events = Vec::new();
                let mut index = 0;
                while index < candidates.len() {
                    let iid = candidates[index].0.clone();
                    let evidence = client_tool_evidence_postgres(&mut tx, &iid).await?;
                    let resolved = resolved_evidence(&evidence);
                    let mut last_seq = None;
                    while index < candidates.len() && candidates[index].0 == iid {
                        let rid = candidates[index].1.clone();
                        index += 1;
                        if resolved.contains(rid.as_str()) {
                            continue;
                        }
                        let expiry: Option<i64> = sqlx::query_scalar("UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_wait_expired' WHERE id=$1 AND status='waiting_client' RETURNING expires_at").bind(&rid).fetch_optional(&mut *tx).await?;
                        let Some(expiry) = expiry else { continue };
                        let seq: i64 =
                            sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                                .fetch_one(&mut *tx)
                                .await?;
                        sqlx::query("UPDATE inference_run_observations SET last_event_sequence=$1 WHERE id=$2").bind(seq).bind(&rid).execute(&mut *tx).await?;
                        insert_event_postgres(
                            &mut tx,
                            EventInsert {
                                sequence: seq,
                                occurred_at: now,
                                interaction_id: Some(&iid),
                                run_id: Some(&rid),
                                rejection_id: None,
                                kind: "run_state_changed",
                                payload: &payload,
                                expires_at: expiry,
                            },
                        )
                        .await?;
                        events.push(event(
                            seq,
                            now,
                            Some(&iid),
                            Some(&rid),
                            None,
                            "run_state_changed",
                            payload.clone(),
                        ));
                        last_seq = Some(seq);
                    }
                    if let Some(seq) = last_seq {
                        recompute_status_postgres(&mut tx, &iid, seq).await?;
                    }
                }
                tx.commit().await?;
                Ok(events)
            }
        }
    }

    /// 最后一个观察句柄释放后才收口残留活动，不能在响应结束时取消合法后台工具。
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.finalize_activity",
        skip_all
    )]
    pub(super) async fn finalize_activity(
        &self,
        interaction_id: &str,
        run_id: &str,
        now: i64,
        expires_at: i64,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let previous: Option<i64> = match self {
            Self::Sqlite(pool, _, _) => sqlx::query_scalar("SELECT last_active_at FROM inference_run_observations r WHERE r.id=? AND (r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations m WHERE m.run_id=r.id AND m.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations a WHERE a.run_id=r.id AND a.status='running'))").bind(run_id).fetch_optional(pool).await?,
            Self::Postgres(pool, _) => sqlx::query_scalar("SELECT last_active_at FROM inference_run_observations r WHERE r.id=$1 AND (r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations m WHERE m.run_id=r.id AND m.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations a WHERE a.run_id=r.id AND a.status='running'))").bind(run_id).fetch_optional(pool).await?,
        };
        let Some(previous) = previous else {
            return Ok(None);
        };
        let at = now.max(previous);
        let payload = serde_json::to_value(RunEvent::ObservationGap {
            reason: "unfinished_observation_activity".into(),
        })?;
        let seq = match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin().await?;
                let attempts = sqlx::query("SELECT id,model_turn_id,started_at,first_token_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens,usage_recorded FROM target_attempt_observations WHERE run_id=? AND status='running' ORDER BY id").bind(run_id).fetch_all(&mut *tx).await?;
                for attempt in attempts {
                    let recorded: bool = attempt.try_get("usage_recorded")?;
                    let usage = if recorded {
                        Some(ConfirmedUsage {
                            input_tokens: attempt.try_get("input_tokens")?,
                            output_tokens: attempt.try_get("output_tokens")?,
                            cache_read_tokens: attempt.try_get("cache_read_tokens")?,
                            cache_write_tokens: attempt.try_get("cache_write_tokens")?,
                            reasoning_tokens: attempt.try_get("reasoning_tokens")?,
                            coverage: None,
                        })
                    } else {
                        None
                    };
                    let finished_at = at.max(attempt.try_get::<i64, _>("started_at")?);
                    let terminal = RunEvent::TargetAttemptFinished {
                        model_turn_id: attempt.try_get("model_turn_id")?,
                        attempt_id: attempt.try_get("id")?,
                        status: "interrupted".into(),
                        status_code: None,
                        error_code: Some("observation_gap".into()),
                        error: None,
                        duration_ms: finished_at.saturating_sub(attempt.try_get("started_at")?),
                        first_token_ms: attempt.try_get("first_token_ms")?,
                        usage,
                    };
                    let mut terminal_payload = serde_json::to_value(terminal)?;
                    terminal_payload["finished_at"] = Value::from(finished_at);
                    let terminal_sequence: i64 = next_sqlite(&mut tx).await?;
                    insert_event_sqlite(
                        &mut tx,
                        EventInsert {
                            sequence: terminal_sequence,
                            occurred_at: at,
                            interaction_id: Some(interaction_id),
                            run_id: Some(run_id),
                            rejection_id: None,
                            kind: "target_attempt_finished",
                            payload: &terminal_payload,
                            expires_at,
                        },
                    )
                    .await?;
                }

                let seq = next_sqlite(&mut tx).await?;
                sqlx::query("UPDATE target_attempt_observations SET status='interrupted',error_code=COALESCE(error_code,'observation_gap'),finished_at=MAX(started_at,?),last_event_sequence=? WHERE run_id=? AND status='running'").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE model_turn_observations SET status='interrupted',finished_at=MAX(started_at,?),last_event_sequence=? WHERE run_id=? AND status='running'").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE inference_run_observations SET background_active=0,last_event_sequence=? WHERE id=?").bind(seq).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET observation_gap=1 WHERE id=?")
                    .bind(interaction_id)
                    .execute(&mut *tx)
                    .await?;
                recompute_status_sqlite(&mut tx, interaction_id, seq).await?;
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "observation_gap",
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                seq
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let attempts = sqlx::query("SELECT id,model_turn_id,started_at,first_token_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens,usage_recorded FROM target_attempt_observations WHERE run_id=$1 AND status='running' ORDER BY id").bind(run_id).fetch_all(&mut *tx).await?;
                for attempt in attempts {
                    let recorded: bool = attempt.try_get("usage_recorded")?;
                    let usage = if recorded {
                        Some(ConfirmedUsage {
                            input_tokens: attempt.try_get("input_tokens")?,
                            output_tokens: attempt.try_get("output_tokens")?,
                            cache_read_tokens: attempt.try_get("cache_read_tokens")?,
                            cache_write_tokens: attempt.try_get("cache_write_tokens")?,
                            reasoning_tokens: attempt.try_get("reasoning_tokens")?,
                            coverage: None,
                        })
                    } else {
                        None
                    };
                    let finished_at = at.max(attempt.try_get::<i64, _>("started_at")?);
                    let terminal = RunEvent::TargetAttemptFinished {
                        model_turn_id: attempt.try_get("model_turn_id")?,
                        attempt_id: attempt.try_get("id")?,
                        status: "interrupted".into(),
                        status_code: None,
                        error_code: Some("observation_gap".into()),
                        error: None,
                        duration_ms: finished_at.saturating_sub(attempt.try_get("started_at")?),
                        first_token_ms: attempt.try_get("first_token_ms")?,
                        usage,
                    };
                    let mut terminal_payload = serde_json::to_value(terminal)?;
                    terminal_payload["finished_at"] = Value::from(finished_at);
                    let terminal_sequence: i64 =
                        sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                            .fetch_one(&mut *tx)
                            .await?;
                    insert_event_postgres(
                        &mut tx,
                        EventInsert {
                            sequence: terminal_sequence,
                            occurred_at: at,
                            interaction_id: Some(interaction_id),
                            run_id: Some(run_id),
                            rejection_id: None,
                            kind: "target_attempt_finished",
                            payload: &terminal_payload,
                            expires_at,
                        },
                    )
                    .await?;
                }

                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                sqlx::query("UPDATE target_attempt_observations SET status='interrupted',error_code=COALESCE(error_code,'observation_gap'),finished_at=GREATEST(started_at,$1),last_event_sequence=$2 WHERE run_id=$3 AND status='running'").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE model_turn_observations SET status='interrupted',finished_at=GREATEST(started_at,$1),last_event_sequence=$2 WHERE run_id=$3 AND status='running'").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE inference_run_observations SET background_active=0,last_event_sequence=$1 WHERE id=$2").bind(seq).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET observation_gap=TRUE WHERE id=$1")
                    .bind(interaction_id)
                    .execute(&mut *tx)
                    .await?;
                recompute_status_postgres(&mut tx, interaction_id, seq).await?;
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "observation_gap",
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                seq
            }
        };
        Ok(Some(event(
            seq,
            at,
            Some(interaction_id),
            Some(run_id),
            None,
            "observation_gap",
            payload,
        )))
    }

    pub async fn finish_run(
        &self,
        interaction_id: &str,
        run_id: &str,
        outcome: &RunOutcome,
        now: i64,
        expires_at: i64,
    ) -> anyhow::Result<ObservationEvent> {
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin().await?;
                let seq = next_sqlite(&mut tx).await?;
                // 耗时采用实际结束时刻；终态事件不能排到延迟记录的前序事件之前。
                let (interrupted, recorded_at): (bool, i64) = sqlx::query_as("UPDATE inference_run_observations SET status=CASE WHEN user_interrupted=1 AND ?1!='failed' THEN 'user_interrupted' WHEN ?1='waiting_client' AND EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) THEN 'superseded' ELSE ?1 END,terminal_reason=CASE WHEN user_interrupted=1 AND ?1!='failed' THEN 'user_interrupted' ELSE ?2 END,generation_node_id=COALESCE(?3,generation_node_id),finished_at=?4,last_active_at=MAX(last_active_at,?5),last_event_sequence=?6 WHERE id=?7 RETURNING user_interrupted,last_active_at").bind(&outcome.status).bind(&outcome.terminal_reason).bind(&outcome.generation_node_id).bind(now).bind(now).bind(seq).bind(run_id).fetch_one(&mut *tx).await?;
                let (committed, parent, status, reason, delivered_at): (bool, Option<String>, String, Option<String>, Option<i64>) = sqlx::query_as("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,?),client_output_committed=MAX(client_output_committed,?),terminal_reason=CASE WHEN status='superseded' THEN 'superseded' ELSE terminal_reason END WHERE id=? RETURNING client_output_committed,generation_parent_id,status,terminal_reason,delivery_completed_at").bind(outcome.delivery_completed_at).bind(outcome.client_output_committed).bind(run_id).fetch_one(&mut *tx).await?;
                let mut payload = finish_payload(outcome, interrupted)?;
                payload["finished_at"] = Value::from(now);
                payload["status"] = Value::String(status);
                payload["terminal_reason"] = serde_json::to_value(reason)?;
                payload["delivery_completed_at"] = serde_json::to_value(delivered_at)?;
                payload["client_output_committed"] = Value::Bool(committed);
                payload["generation_parent_id"] = serde_json::to_value(parent)?;
                recompute_status_sqlite(&mut tx, interaction_id, seq).await?;
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: recorded_at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "run_finished",
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    seq,
                    recorded_at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_finished",
                    payload,
                ))
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                let (interrupted, recorded_at): (bool, i64) = sqlx::query_as("UPDATE inference_run_observations SET status=CASE WHEN user_interrupted AND $1!='failed' THEN 'user_interrupted' WHEN $1='waiting_client' AND EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) THEN 'superseded' ELSE $1 END,terminal_reason=CASE WHEN user_interrupted AND $1!='failed' THEN 'user_interrupted' ELSE $2 END,generation_node_id=COALESCE($3,generation_node_id),finished_at=$4,last_active_at=GREATEST(last_active_at,$5),last_event_sequence=$6 WHERE id=$7 RETURNING user_interrupted,last_active_at").bind(&outcome.status).bind(&outcome.terminal_reason).bind(&outcome.generation_node_id).bind(now).bind(now).bind(seq).bind(run_id).fetch_one(&mut *tx).await?;
                let (committed, parent, status, reason, delivered_at): (bool, Option<String>, String, Option<String>, Option<i64>) = sqlx::query_as("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,$1),client_output_committed=client_output_committed OR $2,terminal_reason=CASE WHEN status='superseded' THEN 'superseded' ELSE terminal_reason END WHERE id=$3 RETURNING client_output_committed,generation_parent_id,status,terminal_reason,delivery_completed_at").bind(outcome.delivery_completed_at).bind(outcome.client_output_committed).bind(run_id).fetch_one(&mut *tx).await?;
                let mut payload = finish_payload(outcome, interrupted)?;
                payload["finished_at"] = Value::from(now);
                payload["status"] = Value::String(status);
                payload["terminal_reason"] = serde_json::to_value(reason)?;
                payload["delivery_completed_at"] = serde_json::to_value(delivered_at)?;
                payload["client_output_committed"] = Value::Bool(committed);
                payload["generation_parent_id"] = serde_json::to_value(parent)?;
                recompute_status_postgres(&mut tx, interaction_id, seq).await?;
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: recorded_at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "run_finished",
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    seq,
                    recorded_at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_finished",
                    payload,
                ))
            }
        }
    }

    #[tracing::instrument(target = "stravia::perf", name = "observation.writer.reject", skip_all)]
    pub async fn reject(&self, rejection: Rejection<'_>) -> anyhow::Result<ObservationEvent> {
        let Rejection {
            ingress,
            outcome,
            metadata,
            debug_enabled: debug,
            occurred_at: now,
            expires_at,
            started_at,
            duration_ms,
        } = rejection;
        let payload = serde_json::to_value(outcome)?;
        let failure = outcome
            .failure
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin().await?;
                let seq = next_sqlite(&mut tx).await?;
                sqlx::query("INSERT INTO rejected_request_observations (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,expires_at,started_at,duration_ms,failure_json,request_model,api_key_id,api_key_name) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
                    .bind(&ingress.id).bind(now).bind(&ingress.method).bind(&ingress.path).bind(&ingress.protocol).bind(&outcome.stage).bind(&outcome.code).bind(outcome.status_code).bind(debug).bind(seq).bind(expires_at)
                    .bind(started_at).bind(duration_ms).bind(&failure).bind(&metadata.model).bind(&metadata.api_key_id).bind(&metadata.api_key_name).execute(&mut *tx).await?;
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: now,
                        interaction_id: None,
                        run_id: None,
                        rejection_id: Some(&ingress.id),
                        kind: "request_rejected",
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    seq,
                    now,
                    None,
                    None,
                    Some(&ingress.id),
                    "request_rejected",
                    payload,
                ))
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO rejected_request_observations (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,expires_at,started_at,duration_ms,failure_json,request_model,api_key_id,api_key_name) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)")
                    .bind(&ingress.id).bind(now).bind(&ingress.method).bind(&ingress.path).bind(&ingress.protocol).bind(&outcome.stage).bind(&outcome.code).bind(i64::from(outcome.status_code)).bind(debug).bind(seq).bind(expires_at)
                    .bind(started_at).bind(duration_ms).bind(&failure).bind(&metadata.model).bind(&metadata.api_key_id).bind(&metadata.api_key_name).execute(&mut *tx).await?;
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: now,
                        interaction_id: None,
                        run_id: None,
                        rejection_id: Some(&ingress.id),
                        kind: "request_rejected",
                        payload: &payload,
                        expires_at,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    seq,
                    now,
                    None,
                    None,
                    Some(&ingress.id),
                    "request_rejected",
                    payload,
                ))
            }
        }
    }

    pub async fn observed_generation_parent(
        &self,
        generation_node_id: &str,
        principal: &str,
    ) -> anyhow::Result<Option<super::grouping::ObservedParent>> {
        // Immutable transport evidence survives late observation work and restart recovery.
        // Old rows without this evidence cannot qualify for time-based grouping.
        match self {
            Self::Sqlite(pool, _, _) => Ok(sqlx::query_as("SELECT r.interaction_id,r.id AS run_id,r.delivery_completed_at AS delivery_completed_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.generation_node_id=? AND i.principal=? LIMIT 1")
                .bind(generation_node_id).bind(principal).fetch_optional(pool).await?),
            Self::Postgres(pool, _) => Ok(sqlx::query_as("SELECT r.interaction_id,r.id AS run_id,r.delivery_completed_at AS delivery_completed_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.generation_node_id=$1 AND i.principal=$2 LIMIT 1")
                .bind(generation_node_id).bind(principal).fetch_optional(pool).await?),
        }
    }

    pub async fn generation_parent(
        &self,
        generation_node_id: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        match self {
            Self::Sqlite(pool, _, _) => Ok(sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations WHERE generation_node_id=? LIMIT 1").bind(generation_node_id).fetch_optional(pool).await?),
            Self::Postgres(pool, _) => Ok(sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations WHERE generation_node_id=$1 LIMIT 1").bind(generation_node_id).fetch_optional(pool).await?),
        }
    }

    pub async fn recover_after_restart(&self) -> anyhow::Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        match self {
            Self::Sqlite(pool, _, write_gate) => {
                let _write_gate = write_gate.lock().await;
                let mut tx = pool.begin().await?;
                let mut runs:Vec<(String,String,String,i64)>=sqlx::query_as("SELECT r.interaction_id,r.id,r.status,r.expires_at FROM inference_run_observations r WHERE r.status='running' OR r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations mt WHERE mt.run_id=r.id AND mt.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations ta WHERE ta.run_id=r.id AND ta.status='running')").fetch_all(&mut *tx).await?;
                let old_interactions: Vec<(String, i64)> = sqlx::query_as("SELECT id,last_event_sequence FROM interaction_observations WHERE status IN ('running','waiting_client')").fetch_all(&mut *tx).await?;
                let mut affected: std::collections::HashMap<String, i64> =
                    old_interactions.into_iter().collect();
                let waiting: Vec<(String, String, String, i64)> = sqlx::query_as("SELECT r.interaction_id,r.id,r.status,r.expires_at FROM inference_run_observations r WHERE r.status='waiting_client' AND EXISTS(SELECT 1 FROM interaction_observations i WHERE i.id=r.interaction_id AND i.status IN ('running','waiting_client')) AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id AND c.interaction_id=r.interaction_id) ORDER BY r.interaction_id,r.id").fetch_all(&mut *tx).await?;
                let existing: std::collections::HashSet<_> =
                    runs.iter().map(|run| run.1.as_str()).collect();
                let mut pending = Vec::new();
                let mut unresolved = std::collections::HashSet::new();
                let mut index = 0;
                while index < waiting.len() {
                    let iid = &waiting[index].0;
                    let evidence = client_tool_evidence_sqlite(&mut tx, iid).await?;
                    let resolved = resolved_evidence(&evidence);
                    while index < waiting.len() && &waiting[index].0 == iid {
                        if !resolved.contains(waiting[index].1.as_str()) {
                            unresolved.insert(waiting[index].1.as_str());
                            if !existing.contains(waiting[index].1.as_str()) {
                                pending.push(waiting[index].clone());
                            }
                        }
                        index += 1;
                    }
                }
                drop(existing);
                runs.extend(pending);

                for (iid, rid, old_status, expires_at) in &runs {
                    let seq = next_sqlite(&mut tx).await?;
                    let status = if old_status == "running" || unresolved.contains(rid.as_str()) {
                        "interrupted"
                    } else {
                        old_status.as_str()
                    };
                    sqlx::query("UPDATE inference_run_observations SET status=?,terminal_reason=CASE WHEN status='running' OR (status='waiting_client' AND ?='interrupted') THEN 'process_restarted' ELSE terminal_reason END,finished_at=CASE WHEN status='running' THEN COALESCE(finished_at,?) ELSE finished_at END,background_active=0,last_event_sequence=? WHERE id=?").bind(status).bind(status).bind(now).bind(seq).bind(rid).execute(&mut *tx).await?;
                    affected.insert(iid.clone(), seq);
                    let payload = serde_json::json!({"status":status,"reason":"process_restarted"});
                    insert_event_sqlite(
                        &mut tx,
                        EventInsert {
                            sequence: seq,
                            occurred_at: now,
                            interaction_id: Some(iid),
                            run_id: Some(rid),
                            rejection_id: None,
                            kind: "run_state_changed",
                            payload: &payload,
                            expires_at: *expires_at,
                        },
                    )
                    .await?;
                }
                sqlx::query("UPDATE target_attempt_observations SET status='interrupted',error_code=COALESCE(error_code,'process_restarted'),finished_at=COALESCE(finished_at,?),last_event_sequence=(SELECT r.last_event_sequence FROM inference_run_observations r WHERE r.id=target_attempt_observations.run_id) WHERE status='running'").bind(now).execute(&mut *tx).await?;
                sqlx::query("UPDATE model_turn_observations SET status='interrupted',finished_at=COALESCE(finished_at,?),last_event_sequence=(SELECT r.last_event_sequence FROM inference_run_observations r WHERE r.id=model_turn_observations.run_id) WHERE status='running'").bind(now).execute(&mut *tx).await?;
                for (id, seq) in affected {
                    recompute_status_sqlite(&mut tx, &id, seq).await?;
                }
                tx.commit().await?;
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let mut runs:Vec<(String,String,String,i64)>=sqlx::query_as("SELECT r.interaction_id,r.id,r.status,r.expires_at FROM inference_run_observations r WHERE r.status='running' OR r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations mt WHERE mt.run_id=r.id AND mt.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations ta WHERE ta.run_id=r.id AND ta.status='running')").fetch_all(&mut *tx).await?;
                let old_interactions: Vec<(String, i64)> = sqlx::query_as("SELECT id,last_event_sequence FROM interaction_observations WHERE status IN ('running','waiting_client')").fetch_all(&mut *tx).await?;
                let mut affected: std::collections::HashMap<String, i64> =
                    old_interactions.into_iter().collect();
                let waiting: Vec<(String, String, String, i64)> = sqlx::query_as("SELECT r.interaction_id,r.id,r.status,r.expires_at FROM inference_run_observations r WHERE r.status='waiting_client' AND EXISTS(SELECT 1 FROM interaction_observations i WHERE i.id=r.interaction_id AND i.status IN ('running','waiting_client')) AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id AND c.interaction_id=r.interaction_id) ORDER BY r.interaction_id,r.id").fetch_all(&mut *tx).await?;
                let existing: std::collections::HashSet<_> =
                    runs.iter().map(|run| run.1.as_str()).collect();
                let mut pending = Vec::new();
                let mut unresolved = std::collections::HashSet::new();
                let mut index = 0;
                while index < waiting.len() {
                    let iid = &waiting[index].0;
                    let evidence = client_tool_evidence_postgres(&mut tx, iid).await?;
                    let resolved = resolved_evidence(&evidence);
                    while index < waiting.len() && &waiting[index].0 == iid {
                        if !resolved.contains(waiting[index].1.as_str()) {
                            unresolved.insert(waiting[index].1.as_str());
                            if !existing.contains(waiting[index].1.as_str()) {
                                pending.push(waiting[index].clone());
                            }
                        }
                        index += 1;
                    }
                }
                drop(existing);
                runs.extend(pending);

                for (iid, rid, old_status, expires_at) in &runs {
                    let seq: i64 =
                        sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                            .fetch_one(&mut *tx)
                            .await?;
                    let status = if old_status == "running" || unresolved.contains(rid.as_str()) {
                        "interrupted"
                    } else {
                        old_status.as_str()
                    };
                    sqlx::query("UPDATE inference_run_observations SET status=$1,terminal_reason=CASE WHEN status='running' OR (status='waiting_client' AND $1='interrupted') THEN 'process_restarted' ELSE terminal_reason END,finished_at=CASE WHEN status='running' THEN COALESCE(finished_at,$2) ELSE finished_at END,background_active=0,last_event_sequence=$3 WHERE id=$4").bind(status).bind(now).bind(seq).bind(rid).execute(&mut *tx).await?;
                    affected.insert(iid.clone(), seq);
                    let payload = serde_json::json!({"status":status,"reason":"process_restarted"});
                    insert_event_postgres(
                        &mut tx,
                        EventInsert {
                            sequence: seq,
                            occurred_at: now,
                            interaction_id: Some(iid),
                            run_id: Some(rid),
                            rejection_id: None,
                            kind: "run_state_changed",
                            payload: &payload,
                            expires_at: *expires_at,
                        },
                    )
                    .await?;
                }
                sqlx::query("UPDATE target_attempt_observations ta SET status='interrupted',error_code=COALESCE(ta.error_code,'process_restarted'),finished_at=COALESCE(ta.finished_at,$1),last_event_sequence=r.last_event_sequence FROM inference_run_observations r WHERE ta.status='running' AND r.id=ta.run_id").bind(now).execute(&mut *tx).await?;
                sqlx::query("UPDATE model_turn_observations mt SET status='interrupted',finished_at=COALESCE(mt.finished_at,$1),last_event_sequence=r.last_event_sequence FROM inference_run_observations r WHERE mt.status='running' AND r.id=mt.run_id").bind(now).execute(&mut *tx).await?;
                for (id, seq) in affected {
                    recompute_status_postgres(&mut tx, &id, seq).await?;
                }
                tx.commit().await?;
            }
        }
        Ok(())
    }

    pub async fn max_sequence(&self) -> anyhow::Result<i64> {
        match self{Self::Sqlite(p, _, _)=>Ok(sqlx::query_scalar("SELECT next_sequence-1 FROM observation_sequence WHERE singleton_id=1").fetch_one(p).await?),Self::Postgres(p, _)=>Ok(sqlx::query_scalar("SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM observation_event_sequence").fetch_one(p).await?)}
    }
    pub async fn min_sequence(&self) -> anyhow::Result<Option<i64>> {
        match self {
            Self::Sqlite(p, _, _) => Ok(sqlx::query_scalar(
                "SELECT MIN(sequence) FROM observation_events",
            )
            .fetch_one(p)
            .await?),
            Self::Postgres(p, _) => Ok(sqlx::query_scalar(
                "SELECT MIN(sequence) FROM observation_events",
            )
            .fetch_one(p)
            .await?),
        }
    }
    pub async fn replay(&self, after: i64) -> anyhow::Result<Vec<ObservationEvent>> {
        match self {
            Self::Sqlite(p, _, _) => load_events_sqlite(p, after).await,
            Self::Postgres(p, _) => load_events_postgres(p, after).await,
        }
    }
}

fn finish_payload(outcome: &RunOutcome, interrupted: bool) -> anyhow::Result<Value> {
    let mut payload = serde_json::to_value(outcome)?;
    if interrupted && outcome.status != "failed" {
        payload["status"] = Value::String("user_interrupted".into());
        payload["terminal_reason"] = Value::String("user_interrupted".into());
    }
    Ok(payload)
}

/// 新增 User 输入打断父 Interaction 时的前驱清扫：仍在执行的 Run 仅打标记、终态
/// 由 finish 写入；叶子等待分支真正被输入中断记 user_interrupted；已有续接 Run 的
/// 等待分支其实早被接替，记 superseded 而非误标中断。
async fn interrupt_predecessors_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    interaction_id: &str,
    now: i64,
) -> anyhow::Result<()> {
    let runs:Vec<(String,String,i64,bool)>=sqlx::query_as("SELECT id,status,expires_at,NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) FROM inference_run_observations WHERE interaction_id=? AND status IN ('running','waiting_client')").bind(interaction_id).fetch_all(&mut **tx).await?;
    let mut last = None;
    for (rid, old_status, expires_at, leaf) in runs {
        let seq = next_sqlite(tx).await?;
        let (status, reason) = match (old_status.as_str(), leaf) {
            ("waiting_client", true) => ("user_interrupted", "user_interrupted"),
            ("waiting_client", false) => ("superseded", "superseded"),
            _ => (old_status.as_str(), "user_interrupted"),
        };
        let interrupted = reason == "user_interrupted";
        sqlx::query("UPDATE inference_run_observations SET user_interrupted=?,status=?,terminal_reason=?,finished_at=CASE WHEN ?='user_interrupted' THEN COALESCE(finished_at,?) ELSE finished_at END,last_event_sequence=? WHERE id=?").bind(interrupted).bind(status).bind(reason).bind(status).bind(now).bind(seq).bind(&rid).execute(&mut **tx).await?;
        let payload =
            serde_json::json!({"status":status,"user_interrupted":interrupted,"reason":reason});
        insert_event_sqlite(
            tx,
            EventInsert {
                sequence: seq,
                occurred_at: now,
                interaction_id: Some(interaction_id),
                run_id: Some(&rid),
                rejection_id: None,
                kind: "run_state_changed",
                payload: &payload,
                expires_at,
            },
        )
        .await?;
        last = Some(seq);
    }
    if let Some(seq) = last {
        recompute_status_sqlite(tx, interaction_id, seq).await?;
    }
    Ok(())
}
async fn interrupt_predecessors_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    interaction_id: &str,
    now: i64,
) -> anyhow::Result<()> {
    let runs:Vec<(String,String,i64,bool)>=sqlx::query_as("SELECT id,status,expires_at,NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) FROM inference_run_observations WHERE interaction_id=$1 AND status IN ('running','waiting_client')").bind(interaction_id).fetch_all(&mut **tx).await?;
    let mut last = None;
    for (rid, old_status, expires_at, leaf) in runs {
        let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
            .fetch_one(&mut **tx)
            .await?;
        let (status, reason) = match (old_status.as_str(), leaf) {
            ("waiting_client", true) => ("user_interrupted", "user_interrupted"),
            ("waiting_client", false) => ("superseded", "superseded"),
            _ => (old_status.as_str(), "user_interrupted"),
        };
        let interrupted = reason == "user_interrupted";
        sqlx::query("UPDATE inference_run_observations SET user_interrupted=$1,status=$2,terminal_reason=$3,finished_at=CASE WHEN $2='user_interrupted' THEN COALESCE(finished_at,$4) ELSE finished_at END,last_event_sequence=$5 WHERE id=$6").bind(interrupted).bind(status).bind(reason).bind(now).bind(seq).bind(&rid).execute(&mut **tx).await?;
        let payload =
            serde_json::json!({"status":status,"user_interrupted":interrupted,"reason":reason});
        insert_event_postgres(
            tx,
            EventInsert {
                sequence: seq,
                occurred_at: now,
                interaction_id: Some(interaction_id),
                run_id: Some(&rid),
                rejection_id: None,
                kind: "run_state_changed",
                payload: &payload,
                expires_at,
            },
        )
        .await?;
        last = Some(seq);
    }
    if let Some(seq) = last {
        recompute_status_postgres(tx, interaction_id, seq).await?;
    }
    Ok(())
}

/// 续接准入证明客户端已经回来：仍停在 waiting_client 的父 Run 直接终结为
/// superseded，不再滞留到被后续新输入清扫或断连清扫误标。
async fn supersede_waiting_parent_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    interaction_id: &str,
    parent_run_id: &str,
    superseded_by: &str,
    now: i64,
) -> anyhow::Result<()> {
    let parent: Option<(String, i64)> = sqlx::query_as(
        "SELECT status,expires_at FROM inference_run_observations WHERE id=? AND interaction_id=?",
    )
    .bind(parent_run_id)
    .bind(interaction_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((status, expires_at)) = parent else {
        return Ok(());
    };
    if status != "waiting_client" {
        return Ok(());
    }
    let seq = next_sqlite(tx).await?;
    let changed = sqlx::query("UPDATE inference_run_observations SET status='superseded',terminal_reason='superseded',last_event_sequence=? WHERE id=? AND status='waiting_client'")
        .bind(seq).bind(parent_run_id).execute(&mut **tx).await?.rows_affected();
    if changed == 0 {
        return Ok(());
    }
    let payload = serde_json::json!({"status":"superseded","reason":"superseded","superseded_by":superseded_by});
    insert_event_sqlite(
        tx,
        EventInsert {
            sequence: seq,
            occurred_at: now,
            interaction_id: Some(interaction_id),
            run_id: Some(parent_run_id),
            rejection_id: None,
            kind: "run_state_changed",
            payload: &payload,
            expires_at,
        },
    )
    .await?;
    Ok(())
}
async fn supersede_waiting_parent_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    interaction_id: &str,
    parent_run_id: &str,
    superseded_by: &str,
    now: i64,
) -> anyhow::Result<()> {
    let parent: Option<(String, i64)> = sqlx::query_as(
        "SELECT status,expires_at FROM inference_run_observations WHERE id=$1 AND interaction_id=$2",
    )
    .bind(parent_run_id)
    .bind(interaction_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((status, expires_at)) = parent else {
        return Ok(());
    };
    if status != "waiting_client" {
        return Ok(());
    }
    let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
        .fetch_one(&mut **tx)
        .await?;
    let changed = sqlx::query("UPDATE inference_run_observations SET status='superseded',terminal_reason='superseded',last_event_sequence=$1 WHERE id=$2 AND status='waiting_client'")
        .bind(seq).bind(parent_run_id).execute(&mut **tx).await?.rows_affected();
    if changed == 0 {
        return Ok(());
    }
    let payload = serde_json::json!({"status":"superseded","reason":"superseded","superseded_by":superseded_by});
    insert_event_postgres(
        tx,
        EventInsert {
            sequence: seq,
            occurred_at: now,
            interaction_id: Some(interaction_id),
            run_id: Some(parent_run_id),
            rejection_id: None,
            kind: "run_state_changed",
            payload: &payload,
            expires_at,
        },
    )
    .await?;
    Ok(())
}

async fn next_sqlite(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> anyhow::Result<i64> {
    next_sqlite_batch(tx, 1).await
}

fn checked_event_count(len: usize) -> anyhow::Result<i64> {
    let count = i64::try_from(len)
        .map_err(|_| anyhow::anyhow!("observation event batch size overflows i64"))?;
    anyhow::ensure!(count > 0, "observation event batch allocates no sequence");
    Ok(count)
}

/// WHERE 先比较上界再进位：序列耗尽显式报错，i64::MAX 边界也不会隐式提升为
/// REAL；RETURNING 的末端减回条数即首序列，不存在 end+1 溢出点。
async fn next_sqlite_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    count: usize,
) -> anyhow::Result<i64> {
    let count = checked_event_count(count)?;
    sqlx::query_scalar(
        "UPDATE observation_sequence SET next_sequence=next_sequence+?1 WHERE singleton_id=1 AND next_sequence<=?2 RETURNING next_sequence-?1",
    )
    .bind(count)
    .bind(i64::MAX - count)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| anyhow::anyhow!("observation event sequence is exhausted"))
}

struct EventInsert<'a> {
    sequence: i64,
    occurred_at: i64,
    interaction_id: Option<&'a str>,
    run_id: Option<&'a str>,
    rejection_id: Option<&'a str>,
    kind: &'a str,
    payload: &'a Value,
    expires_at: i64,
}

async fn insert_event_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: EventInsert<'_>,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO observation_events (sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload,expires_at,tool_id,operation_id) VALUES (?,?,?,?,?,?,?,?,?,?)").bind(event.sequence).bind(event.occurred_at).bind(event.interaction_id).bind(event.run_id).bind(event.rejection_id).bind(event.kind).bind(crate::storage_codec::encode(&serde_json::to_vec(event.payload)?)?).bind(event.expires_at).bind(event.payload.get("tool_id").and_then(Value::as_str)).bind(event.payload.get("operation_id").and_then(Value::as_str)).execute(&mut **tx).await?;
    Ok(())
}
async fn insert_event_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: EventInsert<'_>,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO observation_events (sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload,expires_at,tool_id,operation_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)").bind(event.sequence).bind(event.occurred_at).bind(event.interaction_id).bind(event.run_id).bind(event.rejection_id).bind(event.kind).bind(crate::storage_codec::encode(&serde_json::to_vec(event.payload)?)?).bind(event.expires_at).bind(event.payload.get("tool_id").and_then(Value::as_str)).bind(event.payload.get("operation_id").and_then(Value::as_str)).execute(&mut **tx).await?;
    Ok(())
}

async fn apply_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
    rid: &str,
    e: &RunEvent,
    seq: i64,
    now: i64,
) -> anyhow::Result<()> {
    match e {
        RunEvent::RequestFailed { error } => {
            sqlx::query("UPDATE inference_run_observations SET failure_json=? WHERE id=?")
                .bind(serde_json::to_string(error)?)
                .bind(rid)
                .execute(&mut **tx)
                .await?;
        }
        RunEvent::ModelTurnStarted {
            model_turn_id,
            route_id,
            model_display_name,
            estimated_input_tokens,
        } => {
            let key: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT api_key_id,api_key_name FROM interaction_observations WHERE id=?",
            )
            .bind(iid)
            .fetch_one(&mut **tx)
            .await?;
            sqlx::query("INSERT INTO model_turn_observations (id,run_id,interaction_id,route_id,model_display_name,api_key_id,api_key_name,status,started_at,last_event_sequence,estimated_input_tokens) VALUES (?,?,?,?,?,?,?,'running',?,?,?)").bind(model_turn_id).bind(rid).bind(iid).bind(route_id).bind(model_display_name).bind(key.0).bind(key.1).bind(now).bind(seq).bind(estimated_input_tokens).execute(&mut **tx).await?;
            sqlx::query("UPDATE inference_run_observations SET background_active=background_active+1 WHERE id=?").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::ModelTurnFinished {
            model_turn_id,
            status,
        } => {
            sqlx::query("UPDATE model_turn_observations SET status=?,finished_at=?,last_event_sequence=? WHERE id=?").bind(status).bind(now).bind(seq).bind(model_turn_id).execute(&mut **tx).await?;
            sqlx::query("UPDATE inference_run_observations SET background_active=MAX(background_active-1,0) WHERE id=?").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::TargetAttemptStarted {
            model_turn_id,
            attempt_id,
            target_id,
            provider_id,
            provider_name,
            upstream_model,
            protocol,
            ..
        } => {
            sqlx::query("INSERT OR IGNORE INTO target_attempt_observations (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,status,started_at,last_event_sequence) VALUES (?,?,?,?,?,?,?,?,?,'running',?,?)").bind(attempt_id).bind(model_turn_id).bind(rid).bind(iid).bind(target_id).bind(provider_id).bind(provider_name).bind(upstream_model).bind(protocol).bind(now).bind(seq).execute(&mut **tx).await?;
        }
        RunEvent::TargetAttemptFinished {
            attempt_id,
            status,
            status_code,
            error_code,
            duration_ms,
            first_token_ms,
            usage,
            ..
        } => {
            sqlx::query("UPDATE target_attempt_observations SET status=?,status_code=?,error_code=?,finished_at=?,duration_ms=?,first_token_ms=?,last_event_sequence=? WHERE id=?").bind(status).bind(status_code.map(i64::from)).bind(error_code).bind(now).bind(duration_ms).bind(first_token_ms).bind(seq).bind(attempt_id).execute(&mut **tx).await?;
            if let Some(usage) = usage {
                usage_sqlite(tx, iid, attempt_id, usage, seq).await?;
            }
        }
        RunEvent::UsageConfirmed {
            attempt_id, usage, ..
        } => usage_sqlite(tx, iid, attempt_id, usage, seq).await?,
        RunEvent::PlatformToolStarted { .. } => {
            sqlx::query("UPDATE inference_run_observations SET background_active=background_active+1 WHERE id=?").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::PlatformToolFinished { .. } => {
            sqlx::query("UPDATE inference_run_observations SET background_active=MAX(background_active-1,0) WHERE id=?").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::ClientToolHandoff { .. } => {
            sqlx::query("UPDATE inference_run_observations SET status='waiting_client' WHERE id=?")
                .bind(rid)
                .execute(&mut **tx)
                .await?;
        }
        RunEvent::ObservationGap { .. } => {
            sqlx::query("UPDATE interaction_observations SET observation_gap=1 WHERE id=?")
                .bind(iid)
                .execute(&mut **tx)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn apply_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
    rid: &str,
    e: &RunEvent,
    seq: i64,
    now: i64,
) -> anyhow::Result<()> {
    match e {
        RunEvent::RequestFailed { error } => {
            sqlx::query("UPDATE inference_run_observations SET failure_json=$1 WHERE id=$2")
                .bind(serde_json::to_string(error)?)
                .bind(rid)
                .execute(&mut **tx)
                .await?;
        }
        RunEvent::ModelTurnStarted {
            model_turn_id,
            route_id,
            model_display_name,
            estimated_input_tokens,
        } => {
            let key: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT api_key_id,api_key_name FROM interaction_observations WHERE id=$1",
            )
            .bind(iid)
            .fetch_one(&mut **tx)
            .await?;
            sqlx::query("INSERT INTO model_turn_observations (id,run_id,interaction_id,route_id,model_display_name,api_key_id,api_key_name,status,started_at,last_event_sequence,estimated_input_tokens) VALUES ($1,$2,$3,$4,$5,$6,$7,'running',$8,$9,$10)").bind(model_turn_id).bind(rid).bind(iid).bind(route_id).bind(model_display_name).bind(key.0).bind(key.1).bind(now).bind(seq).bind(estimated_input_tokens).execute(&mut **tx).await?;
            sqlx::query("UPDATE inference_run_observations SET background_active=background_active+1 WHERE id=$1").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::ModelTurnFinished {
            model_turn_id,
            status,
        } => {
            sqlx::query("UPDATE model_turn_observations SET status=$1,finished_at=$2,last_event_sequence=$3 WHERE id=$4").bind(status).bind(now).bind(seq).bind(model_turn_id).execute(&mut **tx).await?;
            sqlx::query("UPDATE inference_run_observations SET background_active=GREATEST(background_active-1,0) WHERE id=$1").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::TargetAttemptStarted {
            model_turn_id,
            attempt_id,
            target_id,
            provider_id,
            provider_name,
            upstream_model,
            protocol,
            ..
        } => {
            sqlx::query("INSERT INTO target_attempt_observations (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,status,started_at,last_event_sequence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'running',$10,$11) ON CONFLICT (id) DO NOTHING").bind(attempt_id).bind(model_turn_id).bind(rid).bind(iid).bind(target_id).bind(provider_id).bind(provider_name).bind(upstream_model).bind(protocol).bind(now).bind(seq).execute(&mut **tx).await?;
        }
        RunEvent::TargetAttemptFinished {
            attempt_id,
            status,
            status_code,
            error_code,
            duration_ms,
            first_token_ms,
            usage,
            ..
        } => {
            sqlx::query("UPDATE target_attempt_observations SET status=$1,status_code=$2,error_code=$3,finished_at=$4,duration_ms=$5,first_token_ms=$6,last_event_sequence=$7 WHERE id=$8").bind(status).bind(status_code.map(i64::from)).bind(error_code).bind(now).bind(duration_ms).bind(first_token_ms).bind(seq).bind(attempt_id).execute(&mut **tx).await?;
            if let Some(usage) = usage {
                usage_postgres(tx, iid, attempt_id, usage, seq).await?;
            }
        }
        RunEvent::UsageConfirmed {
            attempt_id, usage, ..
        } => usage_postgres(tx, iid, attempt_id, usage, seq).await?,
        RunEvent::PlatformToolStarted { .. } => {
            sqlx::query("UPDATE inference_run_observations SET background_active=background_active+1 WHERE id=$1").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::PlatformToolFinished { .. } => {
            sqlx::query("UPDATE inference_run_observations SET background_active=GREATEST(background_active-1,0) WHERE id=$1").bind(rid).execute(&mut **tx).await?;
        }
        RunEvent::ClientToolHandoff { .. } => {
            sqlx::query(
                "UPDATE inference_run_observations SET status='waiting_client' WHERE id=$1",
            )
            .bind(rid)
            .execute(&mut **tx)
            .await?;
        }
        RunEvent::ObservationGap { .. } => {
            sqlx::query("UPDATE interaction_observations SET observation_gap=TRUE WHERE id=$1")
                .bind(iid)
                .execute(&mut **tx)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn usage_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
    aid: &str,
    u: &ConfirmedUsage,
    seq: i64,
) -> anyhow::Result<()> {
    let changed=sqlx::query("UPDATE target_attempt_observations SET input_tokens=?,output_tokens=?,cache_read_tokens=?,cache_write_tokens=?,reasoning_tokens=?,usage_recorded=1,last_event_sequence=? WHERE id=? AND usage_recorded=0").bind(u.input_tokens).bind(u.output_tokens).bind(u.cache_read_tokens).bind(u.cache_write_tokens).bind(u.reasoning_tokens).bind(seq).bind(aid).execute(&mut **tx).await?.rows_affected();
    if changed > 0 {
        sqlx::query("UPDATE model_turn_observations SET (input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens)=(SELECT SUM(input_tokens),SUM(output_tokens),SUM(cache_read_tokens),SUM(cache_write_tokens),SUM(reasoning_tokens) FROM target_attempt_observations WHERE model_turn_id=model_turn_observations.id) WHERE id=(SELECT model_turn_id FROM target_attempt_observations WHERE id=?)").bind(aid).execute(&mut **tx).await?;
        recompute_usage_sqlite(tx, iid).await?;
    }
    Ok(())
}
async fn usage_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
    aid: &str,
    u: &ConfirmedUsage,
    seq: i64,
) -> anyhow::Result<()> {
    let changed=sqlx::query("UPDATE target_attempt_observations SET input_tokens=$1,output_tokens=$2,cache_read_tokens=$3,cache_write_tokens=$4,reasoning_tokens=$5,usage_recorded=TRUE,last_event_sequence=$6 WHERE id=$7 AND usage_recorded=FALSE").bind(u.input_tokens).bind(u.output_tokens).bind(u.cache_read_tokens).bind(u.cache_write_tokens).bind(u.reasoning_tokens).bind(seq).bind(aid).execute(&mut **tx).await?.rows_affected();
    if changed > 0 {
        sqlx::query("UPDATE model_turn_observations SET (input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens)=(SELECT SUM(input_tokens),SUM(output_tokens),SUM(cache_read_tokens),SUM(cache_write_tokens),SUM(reasoning_tokens) FROM target_attempt_observations WHERE model_turn_id=(SELECT model_turn_id FROM target_attempt_observations WHERE id=$1)) WHERE id=(SELECT model_turn_id FROM target_attempt_observations WHERE id=$1)").bind(aid).execute(&mut **tx).await?;
        recompute_usage_postgres(tx, iid).await?;
    }
    Ok(())
}
async fn recompute_usage_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET (input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens)=(SELECT SUM(input_tokens),SUM(output_tokens),SUM(cache_read_tokens),SUM(cache_write_tokens),SUM(reasoning_tokens) FROM target_attempt_observations WHERE interaction_id=?) WHERE id=?").bind(iid).bind(iid).execute(&mut **tx).await?;
    Ok(())
}
async fn recompute_usage_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET (input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens)=(SELECT SUM(input_tokens),SUM(output_tokens),SUM(cache_read_tokens),SUM(cache_write_tokens),SUM(reasoning_tokens) FROM target_attempt_observations WHERE interaction_id=$1) WHERE id=$1").bind(iid).execute(&mut **tx).await?;
    Ok(())
}
fn resolved_evidence(
    events: &[(i64, String, Option<String>, bool)],
) -> std::collections::HashSet<&str> {
    super::grouping::resolved_client_tool_runs(events.iter().map(
        |(sequence, run_id, tool_id, is_handoff)| super::grouping::ClientToolEvidence {
            sequence: *sequence,
            run_id,
            tool_id: tool_id.as_deref(),
            is_handoff: *is_handoff,
        },
    ))
}

async fn client_tool_evidence_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
) -> anyhow::Result<Vec<(i64, String, Option<String>, bool)>> {
    Ok(sqlx::query_as("SELECT sequence,run_id,tool_id,kind='client_tool_handoff' FROM observation_events WHERE interaction_id=? AND run_id IS NOT NULL AND kind IN ('client_tool_handoff','client_tool_result') ORDER BY sequence")
        .bind(iid).fetch_all(&mut **tx).await?)
}

/// 活动时间只聚合 Run 行上真实请求事件写入的 last_active_at；断连判定、闲置超时、
/// 重启恢复、残留收口等终态收尾只重算状态与序列，不得推进窗口归属时间。
async fn recompute_status_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
    seq: i64,
) -> anyhow::Result<()> {
    let runs: Vec<(String, String, bool, bool, i64)> = sqlx::query_as("SELECT r.id,r.status,r.background_active>0,NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id AND c.interaction_id=r.interaction_id),r.last_active_at FROM inference_run_observations r WHERE r.interaction_id=?")
        .bind(iid).fetch_all(&mut **tx).await?;
    let evidence = if runs
        .iter()
        .any(|(_, status, _, leaf, _)| status == "waiting_client" && *leaf)
    {
        client_tool_evidence_sqlite(tx, iid).await?
    } else {
        Vec::new()
    };
    let resolved = resolved_evidence(&evidence);
    let status =
        super::grouping::rollup_status(runs.iter().map(|(id, status, background, leaf, _)| {
            (
                if *background {
                    "running"
                } else {
                    status.as_str()
                },
                *leaf && !(status == "waiting_client" && resolved.contains(id.as_str())),
            )
        }));
    let activity: Option<i64> = runs.iter().map(|run| run.4).max();
    sqlx::query("UPDATE interaction_observations SET status=?,last_active_at=MAX(last_active_at,COALESCE(?,last_active_at)),last_event_sequence=? WHERE id=?")
        .bind(status).bind(activity).bind(seq).bind(iid).execute(&mut **tx).await?;
    Ok(())
}

async fn client_tool_evidence_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
) -> anyhow::Result<Vec<(i64, String, Option<String>, bool)>> {
    Ok(sqlx::query_as("SELECT sequence,run_id,tool_id,kind='client_tool_handoff' FROM observation_events WHERE interaction_id=$1 AND run_id IS NOT NULL AND kind IN ('client_tool_handoff','client_tool_result') ORDER BY sequence")
        .bind(iid).fetch_all(&mut **tx).await?)
}

async fn recompute_status_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
    seq: i64,
) -> anyhow::Result<()> {
    let runs: Vec<(String, String, bool, bool, i64)> = sqlx::query_as("SELECT r.id,r.status,r.background_active>0,NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id AND c.interaction_id=r.interaction_id),r.last_active_at FROM inference_run_observations r WHERE r.interaction_id=$1")
        .bind(iid).fetch_all(&mut **tx).await?;
    let evidence = if runs
        .iter()
        .any(|(_, status, _, leaf, _)| status == "waiting_client" && *leaf)
    {
        client_tool_evidence_postgres(tx, iid).await?
    } else {
        Vec::new()
    };
    let resolved = resolved_evidence(&evidence);
    let status =
        super::grouping::rollup_status(runs.iter().map(|(id, status, background, leaf, _)| {
            (
                if *background {
                    "running"
                } else {
                    status.as_str()
                },
                *leaf && !(status == "waiting_client" && resolved.contains(id.as_str())),
            )
        }));
    let activity: Option<i64> = runs.iter().map(|run| run.4).max();
    sqlx::query("UPDATE interaction_observations SET status=$1,last_active_at=GREATEST(last_active_at,COALESCE($2,last_active_at)),last_event_sequence=$3 WHERE id=$4")
        .bind(status).bind(activity).bind(seq).bind(iid).execute(&mut **tx).await?;
    Ok(())
}

fn event(
    seq: i64,
    at: i64,
    interaction_id: Option<&str>,
    run_id: Option<&str>,
    rejection_id: Option<&str>,
    kind: &str,
    payload: Value,
) -> ObservationEvent {
    ObservationEvent {
        sequence: seq,
        occurred_at: at,
        interaction_id: interaction_id.map(str::to_owned),
        run_id: run_id.map(str::to_owned),
        rejection_id: rejection_id.map(str::to_owned),
        kind: kind.to_owned(),
        payload,
    }
}
async fn load_events_sqlite(
    pool: &SqlitePool,
    after: i64,
) -> anyhow::Result<Vec<ObservationEvent>> {
    let rows=sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE sequence > ? ORDER BY sequence LIMIT 512").bind(after).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok(project_event_for_management(ObservationEvent {
                sequence: r.try_get(0)?,
                occurred_at: r.try_get(1)?,
                interaction_id: r.try_get(2)?,
                run_id: r.try_get(3)?,
                rejection_id: r.try_get(4)?,
                kind: r.try_get(5)?,
                payload: serde_json::from_slice(&crate::storage_codec::decode(
                    &r.try_get::<Vec<u8>, _>(6)?,
                )?)?,
            }))
        })
        .collect()
}
async fn load_events_postgres(pool: &PgPool, after: i64) -> anyhow::Result<Vec<ObservationEvent>> {
    let rows=sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE sequence > $1 ORDER BY sequence LIMIT 512").bind(after).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok(project_event_for_management(ObservationEvent {
                sequence: r.try_get(0)?,
                occurred_at: r.try_get(1)?,
                interaction_id: r.try_get(2)?,
                run_id: r.try_get(3)?,
                rejection_id: r.try_get(4)?,
                kind: r.try_get(5)?,
                payload: serde_json::from_slice(&crate::storage_codec::decode(
                    &r.try_get::<Vec<u8>, _>(6)?,
                )?)?,
            }))
        })
        .collect()
}

fn projection_signal(event: &RunEvent) -> bool {
    matches!(
        event,
        RunEvent::GenerationAssociated { .. }
            | RunEvent::UsageConfirmed { .. }
            | RunEvent::ClientOutputCommitted
            | RunEvent::DeliveryFinished { .. }
    )
}

fn status_event(event: &RunEvent) -> bool {
    matches!(
        event,
        RunEvent::ModelTurnStarted { .. }
            | RunEvent::ModelTurnFinished { .. }
            | RunEvent::PlatformToolStarted { .. }
            | RunEvent::PlatformToolFinished { .. }
            | RunEvent::ClientToolHandoff { .. }
            | RunEvent::ClientToolResult { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::interaction_observation::types::{FailedRequestQuery, ForestQuery};

    async fn confirmed_usage_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        use crate::interaction_observation::types::UsageCoverage;

        let id = "partial-usage";
        admit_tool_run(store, id, None, "alice").await?;
        store
            .persist_run_event(
                id,
                id,
                &RunEvent::ModelTurnStarted {
                    model_turn_id: id.into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                2,
                i64::MAX,
            )
            .await?;
        for attempt in ["unknown", "reported", "overcached", "unknown-cache"] {
            store
                .persist_run_event(
                    id,
                    id,
                    &RunEvent::TargetAttemptStarted {
                        model_turn_id: id.into(),
                        attempt_id: format!("{id}-{attempt}"),
                        target_id: "target".into(),
                        provider_id: "provider".into(),
                        provider_name: "provider".into(),
                        upstream_model: "model".into(),
                        protocol: "responses".into(),
                        upstream_url: "http://localhost".into(),
                    },
                    3,
                    i64::MAX,
                )
                .await?;
        }
        let before = store
            .get_interaction(id, ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(before.interaction.usage.input_tokens, None);
        assert_eq!(
            before
                .interaction
                .usage
                .coverage
                .as_ref()
                .unwrap()
                .missing_input_tokens,
            4
        );

        for (attempt, input_tokens, cache_read_tokens) in [
            ("reported", 12, Some(5)),
            ("overcached", 3, Some(9)),
            ("unknown-cache", 8, None),
        ] {
            let event = RunEvent::UsageConfirmed {
                model_turn_id: id.into(),
                attempt_id: format!("{id}-{attempt}"),
                usage: ConfirmedUsage {
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(3),
                    cache_read_tokens,
                    cache_write_tokens: None,
                    reasoning_tokens: Some(1),
                    coverage: None,
                },
            };
            store.persist_run_event(id, id, &event, 4, i64::MAX).await?;
            store.persist_run_event(id, id, &event, 5, i64::MAX).await?;
        }
        for (attempt, status) in [
            ("unknown", "failed"),
            ("reported", "completed"),
            ("overcached", "failed"),
            ("unknown-cache", "completed"),
        ] {
            store
                .persist_run_event(
                    id,
                    id,
                    &RunEvent::TargetAttemptFinished {
                        model_turn_id: id.into(),
                        attempt_id: format!("{id}-{attempt}"),
                        status: status.into(),
                        status_code: None,
                        error_code: (status == "failed").then(|| "attempt_aborted".into()),
                        error: None,
                        duration_ms: 10,
                        first_token_ms: None,
                        usage: None,
                    },
                    6,
                    i64::MAX,
                )
                .await?;
        }
        let detail = store
            .get_interaction(id, ForestQuery::default())
            .await?
            .unwrap();
        let expected = ConfirmedUsage {
            // Management input is projected per attempt: (12 - 5) + max(3 - 9, 0).
            // The attempt with unknown cache reads remains unknown instead of assuming zero.
            input_tokens: Some(7),
            // Output already includes reasoning; reasoning remains a diagnostic breakdown only.
            output_tokens: Some(9),
            cache_read_tokens: Some(14),
            cache_write_tokens: None,
            reasoning_tokens: Some(3),
            coverage: Some(UsageCoverage {
                attempt_count: 4,
                missing_input_tokens: 2,
                missing_output_tokens: 1,
                missing_cache_read_tokens: 2,
                missing_cache_write_tokens: 4,
                missing_reasoning_tokens: 1,
            }),
        };
        assert_eq!(detail.interaction.usage, expected);
        assert_eq!(detail.runs[0].usage, expected);
        let events = &detail.runs[0].events;
        let reported_usage = events
            .iter()
            .find(|event| {
                matches!(
                    event.kind.as_str(),
                    "target_attempt_finished" | "run_finished"
                ) && event.payload["attempt_id"] == "partial-usage-reported"
            })
            .expect("reported management usage event");
        assert_eq!(reported_usage.payload["usage"]["input_tokens"], 7);
        assert_eq!(reported_usage.payload["usage"]["cache_read_tokens"], 5);
        let overcached_usage = events
            .iter()
            .find(|event| {
                matches!(
                    event.kind.as_str(),
                    "target_attempt_finished" | "run_finished"
                ) && event.payload["attempt_id"] == "partial-usage-overcached"
            })
            .expect("overcached management usage event");
        assert_eq!(overcached_usage.payload["usage"]["input_tokens"], 0);
        let unknown_cache_usage = events
            .iter()
            .find(|event| {
                matches!(
                    event.kind.as_str(),
                    "target_attempt_finished" | "run_finished"
                ) && event.payload["attempt_id"] == "partial-usage-unknown-cache"
            })
            .expect("unknown-cache management usage event");
        assert_eq!(
            unknown_cache_usage.payload["usage"]["input_tokens"],
            Value::Null
        );
        Ok(())
    }

    #[tokio::test]
    async fn confirmed_usage_survives_unknown_and_failed_attempts() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        confirmed_usage_scenario(&store).await?;
        let payloads: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT payload FROM observation_events WHERE kind='target_attempt_finished'",
        )
        .fetch_all(&pool)
        .await?;
        let raw = payloads
            .into_iter()
            .map(|bytes| -> anyhow::Result<Value> {
                Ok(serde_json::from_slice(&crate::storage_codec::decode(
                    &bytes,
                )?)?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .find(|value| value["attempt_id"] == "partial-usage-reported")
            .unwrap();
        assert_eq!(raw["usage"]["input_tokens"], 12);
        assert_eq!(raw["usage"]["cache_read_tokens"], 5);
        // 旧版本持久化的未知总计不能遮住仍然存在的 attempt 用量。
        sqlx::query(
            "UPDATE interaction_observations SET input_tokens=NULL WHERE id='partial-usage'",
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            store
                .get_interaction("partial-usage", ForestQuery::default())
                .await?
                .unwrap()
                .interaction
                .usage
                .input_tokens,
            Some(7)
        );
        pool.close().await;
        Ok(())
    }

    async fn admit_tool_run(
        store: &ObservationStore,
        id: &str,
        parent: Option<&str>,
        principal: &str,
    ) -> anyhow::Result<()> {
        store
            .admit(Admission {
                metadata: None,
                start: &RunStart {
                    id: id.into(),
                    principal: principal.into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                interaction_id: id,
                generation_root_id: None,
                generation_parent_id: None,
                has_new_user: true,
                ingress_received_at: 0,
                parent_run_id: parent,
                parent_interaction_id: None,
                debug_enabled: false,
                inferred_retry: false,
                grouping_reason: "new_root",
                diagnostic_source_run_id: None,
                interrupt_parent: false,
                now: 1,
                expires_at: i64::MAX,
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn queued_observations_preserve_terminal_order_without_inflating_duration()
    -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool,
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_tool_run(&store, "queued", None, "owner").await?;
        // writer 在请求结束后才记录排队事件；耗时仍应使用真实结束时刻。
        let failure = store
            .persist_run_event(
                "queued",
                "queued",
                &RunEvent::RequestFailed {
                    error: crate::interaction_observation::FailureDiagnostic::platform(
                        "provider_unavailable",
                        "provider unavailable",
                        503,
                    ),
                },
                20,
                i64::MAX,
            )
            .await?
            .expect("failure event");
        let finished = store
            .finish_run(
                "queued",
                "queued",
                &RunOutcome {
                    status: "failed".into(),
                    terminal_reason: Some("provider_unavailable".into()),
                    delivery_completed_at: None,
                    delivery: None,
                    client_output_committed: false,
                    generation_node_id: None,
                    generation_root_id: None,
                },
                10,
                i64::MAX,
            )
            .await?;
        assert!(finished.sequence > failure.sequence);
        assert!(finished.occurred_at >= failure.occurred_at);
        let page = store
            .failed_requests(FailedRequestQuery {
                start_at: Some(0),
                end_at: Some(30),
                ..Default::default()
            })
            .await?;
        assert_eq!(page.items[0].duration_ms, Some(10));
        Ok(())
    }

    #[tokio::test]
    async fn final_failure_survives_user_interruption_but_pure_cancellation_does_not()
    -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        // 固定中断与终态写入的先后顺序，避免用 HTTP 并发时序制造偶发测试。
        for (id, status) in [("failed", "failed"), ("cancelled", "cancelled")] {
            admit_tool_run(&store, id, None, "owner").await?;
            let mut tx = pool.begin().await?;
            interrupt_predecessors_sqlite(&mut tx, id, 2).await?;
            tx.commit().await?;
            store
                .finish_run(
                    id,
                    id,
                    &RunOutcome {
                        status: status.into(),
                        terminal_reason: Some(
                            if status == "failed" {
                                "upstream_error"
                            } else {
                                "client_disconnected"
                            }
                            .into(),
                        ),
                        delivery_completed_at: None,
                        delivery: None,
                        client_output_committed: false,
                        generation_node_id: None,
                        generation_root_id: None,
                    },
                    3,
                    i64::MAX,
                )
                .await?;
        }
        let page = store
            .failed_requests(FailedRequestQuery {
                start_at: Some(0),
                end_at: Some(10),
                ..Default::default()
            })
            .await?;
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["failed"]
        );
        Ok(())
    }

    async fn admit_waiting_scenario_run(
        store: &ObservationStore,
        interaction: &str,
        id: &str,
        parent: Option<&str>,
    ) -> anyhow::Result<()> {
        store
            .admit(Admission {
                metadata: None,
                start: &RunStart {
                    id: id.into(),
                    principal: "alice".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                interaction_id: interaction,
                generation_root_id: Some(interaction),
                generation_parent_id: parent,
                has_new_user: false,
                ingress_received_at: 1,
                parent_run_id: parent,
                parent_interaction_id: None,
                debug_enabled: false,
                inferred_retry: false,
                grouping_reason: "exact_parent",
                diagnostic_source_run_id: None,
                interrupt_parent: false,
                now: 1,
                expires_at: i64::MAX,
            })
            .await?;
        Ok(())
    }

    async fn finish_scenario_run(
        store: &ObservationStore,
        interaction: &str,
        run: &str,
        status: &str,
    ) -> anyhow::Result<()> {
        store
            .finish_run(
                interaction,
                run,
                &RunOutcome {
                    delivery_completed_at: Some(3),
                    delivery: None,
                    client_output_committed: false,
                    status: status.into(),
                    terminal_reason: None,
                    generation_node_id: Some(run.into()),
                    generation_root_id: Some(interaction.into()),
                },
                3,
                i64::MAX,
            )
            .await?;
        Ok(())
    }

    async fn sibling_result_scenario(
        store: &ObservationStore,
        prefix: &str,
        final_status: &str,
    ) -> anyhow::Result<()> {
        let waiting = format!("{prefix}-waiting");
        let result = format!("{prefix}-result");
        admit_waiting_scenario_run(store, prefix, prefix, None).await?;
        finish_scenario_run(store, prefix, prefix, "failed").await?;
        admit_waiting_scenario_run(store, prefix, &waiting, Some(prefix)).await?;
        for tool in ["a", "b"] {
            store
                .persist_run_event(
                    prefix,
                    &waiting,
                    &RunEvent::ClientToolHandoff {
                        tool_id: tool.into(),
                        name: "probe".into(),
                        input: None,
                    },
                    2,
                    i64::MAX,
                )
                .await?;
        }
        finish_scenario_run(store, prefix, &waiting, "waiting_client").await?;
        admit_waiting_scenario_run(store, prefix, &result, Some(prefix)).await?;
        store
            .persist_run_event(
                prefix,
                &result,
                &RunEvent::ClientToolResult {
                    tool_id: "a".into(),
                    content: Value::Null,
                    is_error: false,
                },
                4,
                i64::MAX,
            )
            .await?;
        finish_scenario_run(store, prefix, &result, final_status).await?;
        let partial = store
            .get_interaction(prefix, ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(partial.interaction.status, "waiting_client");
        store
            .persist_run_event(
                prefix,
                &result,
                &RunEvent::ClientToolResult {
                    tool_id: "b".into(),
                    content: Value::Null,
                    is_error: true,
                },
                5,
                i64::MAX,
            )
            .await?;
        let detail = store
            .get_interaction(prefix, ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(
            detail.interaction.status,
            if final_status == "completed" {
                "completed"
            } else {
                "interrupted"
            }
        );
        let historical = detail.runs.iter().find(|run| run.id == waiting).unwrap();
        assert_eq!(historical.status, "waiting_client");
        assert_eq!(historical.parent_run_id.as_deref(), Some(prefix));
        assert!(
            !detail
                .runs
                .iter()
                .any(|run| run.parent_run_id.as_deref() == Some(waiting.as_str()))
        );
        let forest = store
            .query_forest(ForestQuery {
                anchor_at: Some(10),
                ..Default::default()
            })
            .await?;
        let summary = forest
            .roots
            .iter()
            .flat_map(|root| &root.interactions)
            .find(|item| item.id == prefix)
            .unwrap();
        assert_eq!(summary.status, detail.interaction.status);
        assert!(
            store
                .disconnect_waiting_client(&waiting, 6)
                .await?
                .is_none()
        );
        assert_eq!(
            store
                .get_interaction(prefix, ForestQuery::default())
                .await?
                .unwrap()
                .runs
                .iter()
                .find(|run| run.id == waiting)
                .unwrap()
                .status,
            "waiting_client"
        );
        Ok(())
    }

    #[tokio::test]
    async fn sibling_tool_results_release_waiting_interaction() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        sibling_result_scenario(
            &ObservationStore::Sqlite(
                pool.clone(),
                Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
                Arc::new(tokio::sync::Mutex::new(())),
            ),
            "sibling",
            "completed",
        )
        .await?;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn foreign_interaction_result_does_not_release_waiting_leaf() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_waiting_scenario_run(&store, "owner", "owner", None).await?;
        store
            .persist_run_event(
                "owner",
                "owner",
                &RunEvent::ClientToolHandoff {
                    tool_id: "shared-id".into(),
                    name: "probe".into(),
                    input: None,
                },
                2,
                i64::MAX,
            )
            .await?;
        finish_scenario_run(&store, "owner", "owner", "waiting_client").await?;
        admit_waiting_scenario_run(&store, "foreign", "foreign", None).await?;
        store
            .persist_run_event(
                "foreign",
                "foreign",
                &RunEvent::ClientToolResult {
                    tool_id: "shared-id".into(),
                    content: Value::Null,
                    is_error: false,
                },
                4,
                i64::MAX,
            )
            .await?;
        finish_scenario_run(&store, "foreign", "foreign", "completed").await?;
        // 强制重算原交互，确保不是遗漏触发让错误的跨交互匹配碰巧未生效。
        finish_scenario_run(&store, "owner", "owner", "waiting_client").await?;
        assert_eq!(
            store
                .get_interaction("owner", ForestQuery::default())
                .await?
                .unwrap()
                .interaction
                .status,
            "waiting_client"
        );
        assert!(store.disconnect_waiting_client("owner", 5).await?.is_some());
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn resolved_tools_do_not_fabricate_final_completion() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        sibling_result_scenario(
            &ObservationStore::Sqlite(
                pool.clone(),
                Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
                Arc::new(tokio::sync::Mutex::new(())),
            ),
            "failed-sibling",
            "failed",
        )
        .await?;
        pool.close().await;
        Ok(())
    }

    async fn close_and_priority_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        for close_first in [true, false] {
            let iid = if close_first {
                "close-first"
            } else {
                "result-first"
            };
            let result = format!("{iid}-result");
            admit_waiting_scenario_run(store, iid, iid, None).await?;
            store
                .persist_run_event(
                    iid,
                    iid,
                    &RunEvent::ClientToolHandoff {
                        tool_id: "call".into(),
                        name: "probe".into(),
                        input: None,
                    },
                    2,
                    i64::MAX,
                )
                .await?;
            finish_scenario_run(store, iid, iid, "waiting_client").await?;
            if close_first {
                assert!(store.disconnect_waiting_client(iid, 4).await?.is_some());
            }
            // sibling 结果不能依靠结构 child 消除原等待。
            admit_waiting_scenario_run(store, iid, &result, None).await?;
            store
                .persist_run_event(
                    iid,
                    &result,
                    &RunEvent::ClientToolResult {
                        tool_id: "call".into(),
                        content: Value::Null,
                        is_error: false,
                    },
                    5,
                    i64::MAX,
                )
                .await?;
            if !close_first {
                assert!(store.disconnect_waiting_client(iid, 6).await?.is_none());
            }
            finish_scenario_run(store, iid, &result, "completed").await?;
            let detail = store
                .get_interaction(iid, ForestQuery::default())
                .await?
                .unwrap();
            assert_eq!(detail.interaction.status, "completed");
            assert_eq!(
                detail.runs.iter().find(|run| run.id == iid).unwrap().status,
                if close_first {
                    "disconnected"
                } else {
                    "waiting_client"
                }
            );
            let pending = format!("{iid}-pending");
            admit_waiting_scenario_run(store, iid, &pending, None).await?;
            assert_eq!(
                store
                    .get_interaction(iid, ForestQuery::default())
                    .await?
                    .unwrap()
                    .interaction
                    .status,
                "running"
            );
            store
                .persist_run_event(
                    iid,
                    &pending,
                    &RunEvent::ClientToolHandoff {
                        tool_id: "unreturned".into(),
                        name: "probe".into(),
                        input: None,
                    },
                    7,
                    i64::MAX,
                )
                .await?;
            finish_scenario_run(store, iid, &pending, "waiting_client").await?;
            assert_eq!(
                store
                    .get_interaction(iid, ForestQuery::default())
                    .await?
                    .unwrap()
                    .interaction
                    .status,
                "waiting_client"
            );
            store
                .persist_run_event(
                    iid,
                    &result,
                    &RunEvent::ModelTurnStarted {
                        model_turn_id: format!("{iid}-background"),
                        route_id: "route".into(),
                        model_display_name: None,
                        estimated_input_tokens: None,
                    },
                    8,
                    i64::MAX,
                )
                .await?;
            assert_eq!(
                store
                    .get_interaction(iid, ForestQuery::default())
                    .await?
                    .unwrap()
                    .interaction
                    .status,
                "running"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn resolved_results_and_connection_close_preserve_history_and_priority()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        close_and_priority_scenario(&ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        ))
        .await?;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn idle_waiting_client_leaf_expires_but_active_and_resolved_do_not() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        let handoff = || RunEvent::ClientToolHandoff {
            tool_id: "call".into(),
            name: "probe".into(),
            input: None,
        };
        for id in ["stale", "fresh", "resolved", "parent"] {
            admit_waiting_scenario_run(&store, id, id, None).await?;
            store
                .persist_run_event(id, id, &handoff(), 2, i64::MAX)
                .await?;
            finish_scenario_run(&store, id, id, "waiting_client").await?;
        }
        // resolved：同交互后续 Run 回传了同一工具 ID。
        admit_waiting_scenario_run(&store, "resolved", "resolved-next", None).await?;
        store
            .persist_run_event(
                "resolved",
                "resolved-next",
                &RunEvent::ClientToolResult {
                    tool_id: "call".into(),
                    content: Value::Null,
                    is_error: false,
                },
                4,
                i64::MAX,
            )
            .await?;
        finish_scenario_run(&store, "resolved", "resolved-next", "completed").await?;
        // parent：续接准入已使父 Run 终结为 superseded，超时清理不得改写该终态。
        admit_waiting_scenario_run(&store, "parent", "parent-child", Some("parent")).await?;
        // 固定闲置窗口两侧：stale/resolved/parent 上次活动远在窗口外，fresh 在窗口内。
        // resolved 与 parent 仅靠各自豁免条件存活，不能用活动时间掩盖。
        sqlx::query("UPDATE inference_run_observations SET last_active_at=0 WHERE id IN ('stale','resolved','parent')")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE inference_run_observations SET last_active_at=100 WHERE id='fresh'")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE interaction_observations SET last_active_at=100 WHERE id='fresh'")
            .execute(&pool)
            .await?;
        let events = store.expire_idle_waiting_client(110, 20).await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "run_state_changed");
        assert_eq!(events[0].run_id.as_deref(), Some("stale"));
        assert_eq!(
            events[0].payload,
            serde_json::json!({"status":"disconnected","reason":"client_wait_expired"})
        );
        let detail = store
            .get_interaction("stale", ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(detail.interaction.status, "disconnected");
        let run = detail.runs.iter().find(|run| run.id == "stale").unwrap();
        assert_eq!(run.status, "disconnected");
        assert_eq!(run.terminal_reason.as_deref(), Some("client_wait_expired"));
        for (iid, rid, expected_status) in [
            ("fresh", "fresh", "waiting_client"),
            ("resolved", "resolved", "waiting_client"),
            ("parent", "parent", "superseded"),
        ] {
            let run = store
                .get_interaction(iid, ForestQuery::default())
                .await?
                .unwrap()
                .runs
                .into_iter()
                .find(|run| run.id == rid)
                .unwrap();
            assert_eq!(run.status, expected_status, "{rid}");
        }
        // 超时判定只推进状态与序列，活动时间仍是最后真实请求时刻。
        let activity: (i64, i64) = sqlx::query_as(
            "SELECT i.last_active_at,r.last_active_at FROM interaction_observations i JOIN inference_run_observations r ON r.interaction_id=i.id WHERE r.id='stale'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(activity, (3, 0));
        let recent = store
            .query_forest(ForestQuery {
                start_at: Some(90),
                end_at: Some(120),
                ..Default::default()
            })
            .await?;
        assert_eq!(
            recent
                .roots
                .iter()
                .map(|root| root.id.as_str())
                .collect::<Vec<_>>(),
            ["fresh"],
            "expired waiting run must stay in its historical page"
        );
        pool.close().await;
        Ok(())
    }

    async fn run_activity_snapshot(
        store: &ObservationStore,
    ) -> anyhow::Result<Vec<(String, i64, i64)>> {
        let sql = "SELECT r.id,r.last_active_at,i.last_active_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id IN ('pending','active') ORDER BY r.id";
        Ok(match store {
            ObservationStore::Sqlite(pool, _, _) => sqlx::query_as(sql).fetch_all(pool).await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_as(sql).fetch_all(pool).await?,
        })
    }

    async fn pending_expiry_snapshot(store: &ObservationStore) -> anyhow::Result<(i64, i64, i64)> {
        let sql = "SELECT r.expires_at,i.expires_at,e.expires_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id JOIN observation_events e ON e.run_id=r.id AND e.kind='run_finished' WHERE r.id='pending'";
        Ok(match store {
            ObservationStore::Sqlite(pool, _, _) => sqlx::query_as(sql).fetch_one(pool).await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_as(sql).fetch_one(pool).await?,
        })
    }

    async fn restart_reconciliation_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        sibling_result_scenario(store, "resolved", "completed").await?;
        // 模拟旧版本已收齐结果但未重算的缓存投影。
        match store {
            ObservationStore::Sqlite(pool, _, _) => {
                sqlx::query("UPDATE interaction_observations SET status='waiting_client' WHERE id='resolved'").execute(pool).await?;
            }
            ObservationStore::Postgres(pool, _) => {
                sqlx::query("UPDATE interaction_observations SET status='waiting_client' WHERE id='resolved'").execute(pool).await?;
            }
        }
        admit_waiting_scenario_run(store, "pending", "pending", None).await?;
        store
            .persist_run_event(
                "pending",
                "pending",
                &RunEvent::ClientToolHandoff {
                    tool_id: "pending-call".into(),
                    name: "probe".into(),
                    input: None,
                },
                2,
                i64::MAX,
            )
            .await?;
        finish_scenario_run(store, "pending", "pending", "waiting_client").await?;
        admit_waiting_scenario_run(store, "active", "active", None).await?;
        let before = store
            .get_interaction("resolved", ForestQuery::default())
            .await?
            .unwrap();
        let old_pending = store
            .get_interaction("pending", ForestQuery::default())
            .await?
            .unwrap();
        let expiry_before = pending_expiry_snapshot(store).await?;
        let activity_before = run_activity_snapshot(store).await?;
        let delivery_before = store
            .replay(0)
            .await?
            .into_iter()
            .find(|event| {
                event.run_id.as_deref() == Some("pending") && event.kind == "run_finished"
            })
            .unwrap();
        store.recover_after_restart().await?;
        let resolved = store
            .get_interaction("resolved", ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(resolved.interaction.status, "completed");
        assert_eq!(
            resolved.interaction.last_active_at,
            before.interaction.last_active_at
        );
        assert_eq!(
            resolved.interaction.last_event_sequence,
            before.interaction.last_event_sequence
        );
        for id in ["pending", "active"] {
            let detail = store
                .get_interaction(id, ForestQuery::default())
                .await?
                .unwrap();
            assert_eq!(detail.interaction.status, "interrupted");
            assert_eq!(detail.runs[0].status, "interrupted");
            assert_eq!(
                detail.runs[0].terminal_reason.as_deref(),
                Some("process_restarted")
            );
        }
        // 重启判定只推进状态与序列，活动时间仍是中断前的最后请求时刻。
        assert_eq!(run_activity_snapshot(store).await?, activity_before);
        let recovered_at = chrono::Utc::now().timestamp_millis();
        let recent = store
            .query_forest(ForestQuery {
                start_at: Some(recovered_at - 600_000),
                end_at: Some(recovered_at + 60_000),
                ..Default::default()
            })
            .await?;
        assert!(
            !recent
                .roots
                .iter()
                .any(|root| root.id == "pending" || root.id == "active"),
            "interrupted history must not enter the recent window"
        );
        let pending = store
            .get_interaction("pending", ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(pending.runs[0].finished_at, old_pending.runs[0].finished_at);
        assert_eq!(
            pending.runs[0].generation_node_id,
            old_pending.runs[0].generation_node_id
        );
        assert_eq!(
            pending.runs[0].generation_parent_id,
            old_pending.runs[0].generation_parent_id
        );
        assert_eq!(
            pending.runs[0].client_output_committed,
            old_pending.runs[0].client_output_committed
        );
        assert_eq!(
            serde_json::to_value(&pending.runs[0].usage)?,
            serde_json::to_value(&old_pending.runs[0].usage)?
        );
        assert_eq!(pending_expiry_snapshot(store).await?, expiry_before);
        let events = store.replay(0).await?;
        let delivery_after = events
            .iter()
            .find(|event| event.sequence == delivery_before.sequence)
            .unwrap();
        assert_eq!(delivery_after.payload, delivery_before.payload);
        let restarted: Vec<_> = events
            .iter()
            // PostgreSQL 合同同时包含其他 waiting 场景，只核验本场景的两个 Run。
            .filter(|event| {
                event.kind == "run_state_changed"
                    && event.payload["reason"] == "process_restarted"
                    && matches!(event.run_id.as_deref(), Some("pending" | "active"))
            })
            .collect();
        assert_eq!(restarted.len(), 2);
        assert!(restarted.iter().all(|event| event.payload
            == serde_json::json!({"status":"interrupted","reason":"process_restarted"})));
        let sequence = store.max_sequence().await?;
        store.recover_after_restart().await?;
        assert_eq!(store.max_sequence().await?, sequence);
        admit_waiting_scenario_run(store, "pending", "continuation", Some("pending")).await?;
        assert_eq!(
            store
                .get_interaction("pending", ForestQuery::default())
                .await?
                .unwrap()
                .interaction
                .status,
            "running"
        );
        finish_scenario_run(store, "pending", "continuation", "completed").await?;
        assert_eq!(
            store
                .get_interaction("pending", ForestQuery::default())
                .await?
                .unwrap()
                .interaction
                .status,
            "completed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn restart_reconciles_waiting_interactions() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        restart_reconciliation_scenario(&ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        ))
        .await?;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn postgres_waiting_reconciliation_when_configured() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("DB_URL") else {
            eprintln!("跳过 PostgreSQL 动态验证：未显式设置 DB_URL");
            return Ok(());
        };
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_obs_wait_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let result = async {
            let options: sqlx::postgres::PgConnectOptions = url.parse()?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with(options.options([("search_path", schema.as_str())]))
                .await?;
            let result = async {
                crate::migrations::migrate_postgres(&pool, None).await?;
                let store = ObservationStore::Postgres(
                    pool.clone(),
                    Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
                );
                confirmed_usage_scenario(&store).await?;
                sibling_result_scenario(&store, "failed-sibling", "failed").await?;
                restart_reconciliation_scenario(&store).await?;
                close_and_priority_scenario(&store).await
            }
            .await;
            pool.close().await;
            result
        }
        .await;
        let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await;
        admin.close().await;
        result?;
        cleanup?;
        Ok(())
    }

    async fn store_tool_batch(
        store: &ObservationStore,
        run: &str,
        events: Vec<RunEvent>,
    ) -> anyhow::Result<Vec<Value>> {
        let mut stored = Vec::new();
        for event in store
            .filter_client_tool_results(run, run, events, 2)
            .await?
        {
            let value = store
                .persist_run_event(run, run, &event, 2, i64::MAX)
                .await?
                .expect("result event");
            stored.push(value.payload);
        }
        Ok(stored)
    }

    #[tokio::test]
    async fn content_batches_commit_in_order_and_rollback_together() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_tool_run(&store, "batch", None, "alice").await?;
        let batch = vec![
            (
                RunEvent::ClientVisibleContent {
                    text: "你好".repeat(1024),
                    parts: Vec::new(),
                    block_id: "test-block".into(),
                    item: Value::Null,
                    complete: true,
                },
                Some("block-a".into()),
                2,
            ),
            (
                RunEvent::ClientVisibleContent {
                    text: "世界".into(),
                    parts: Vec::new(),
                    block_id: "test-block".into(),
                    item: Value::Null,
                    complete: true,
                },
                Some("block-b".into()),
                3,
            ),
        ];
        let committed = store
            .persist_run_events("batch", "batch", &batch, i64::MAX)
            .await?;
        assert!(committed[0].sequence < committed[1].sequence);
        let replay = store.replay(committed[0].sequence - 1).await?;
        assert_eq!(replay[0].payload["text"], "你好".repeat(1024));
        assert_eq!(replay[1].payload["block_id"], "block-b");
        let boundary = committed[1].sequence;
        let invalid = vec![
            (
                RunEvent::ClientVisibleContent {
                    text: "must rollback".into(),
                    parts: Vec::new(),
                    block_id: "test-block".into(),
                    item: Value::Null,
                    complete: true,
                },
                Some("rollback".into()),
                4,
            ),
            (
                RunEvent::ModelTurnStarted {
                    model_turn_id: "duplicate".into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                None,
                4,
            ),
            (
                RunEvent::ModelTurnStarted {
                    model_turn_id: "duplicate".into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                None,
                4,
            ),
        ];
        assert!(
            store
                .persist_run_events("batch", "batch", &invalid, i64::MAX)
                .await
                .is_err()
        );
        assert!(store.replay(boundary).await?.is_empty());
        let tail: String = sqlx::query_scalar(
            "SELECT visible_tail FROM interaction_observations WHERE id='batch'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(!tail.contains("must rollback"));
        store
            .persist_run_events(
                "batch",
                "batch",
                &[(
                    RunEvent::ClientVisibleContent {
                        text: "delayed block".into(),
                        parts: Vec::new(),
                        block_id: "test-block".into(),
                        item: Value::Null,
                        complete: true,
                    },
                    Some("late".into()),
                    2,
                )],
                i64::MAX,
            )
            .await?;
        let activity: (i64, i64) = sqlx::query_as(
            "SELECT i.last_active_at,r.last_active_at FROM interaction_observations i JOIN inference_run_observations r ON r.interaction_id=i.id WHERE r.id='batch'",
        ).fetch_one(&pool).await?;
        assert_eq!(
            activity,
            (3, 3),
            "late content must not move activity backwards"
        );
        pool.close().await;
        Ok(())
    }

    /// 事件边界合同：过滤事件不占号，批次与夹入单事件严格递增，
    /// 失败批次整体回滚且游标/last_event_sequence 停在最新提交事件。
    async fn interleaved_batch_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        let wire = || RunEvent::Wire {
            direction: "outbound".into(),
            transport: "sse".into(),
            protocol: "responses".into(),
            message_type: "request".into(),
            model_turn_id: None,
            attempt_id: None,
            status_code: None,
            url: None,
            headers: Value::Null,
            payload: Value::Null,
        };
        let empty_credentials = || RunEvent::CredentialMappingsCreated {
            discoveries: Vec::new(),
        };
        admit_tool_run(store, "interleaved", None, "alice").await?;

        let first = store
            .persist_run_events(
                "interleaved",
                "interleaved",
                &[
                    (wire(), None, 2),
                    (empty_credentials(), None, 2),
                    (
                        RunEvent::ClientVisibleContent {
                            text: "a".into(),
                            parts: Vec::new(),
                            block_id: "test-block".into(),
                            item: Value::Null,
                            complete: true,
                        },
                        None,
                        2,
                    ),
                ],
                i64::MAX,
            )
            .await?;
        assert_eq!(first.len(), 1, "filtered events must not persist");

        let skipped = store
            .persist_run_events(
                "interleaved",
                "interleaved",
                &[(wire(), None, 2), (empty_credentials(), None, 2)],
                i64::MAX,
            )
            .await?;
        assert!(skipped.is_empty());

        let single = store
            .persist_run_event(
                "interleaved",
                "interleaved",
                &RunEvent::ObservationGap {
                    reason: "single".into(),
                },
                3,
                i64::MAX,
            )
            .await?
            .expect("single event");
        assert_eq!(single.sequence, first[0].sequence + 1);

        let second = store
            .persist_run_events(
                "interleaved",
                "interleaved",
                &[
                    (wire(), None, 4),
                    (
                        RunEvent::ModelTurnStarted {
                            model_turn_id: "interleaved-turn".into(),
                            route_id: "route".into(),
                            model_display_name: None,
                            estimated_input_tokens: None,
                        },
                        None,
                        4,
                    ),
                    (
                        RunEvent::ClientVisibleContent {
                            text: "b".into(),
                            parts: Vec::new(),
                            block_id: "test-block".into(),
                            item: Value::Null,
                            complete: true,
                        },
                        None,
                        4,
                    ),
                ],
                i64::MAX,
            )
            .await?;
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].sequence, single.sequence + 1);
        assert!(second[0].sequence < second[1].sequence);

        // 批内 Model Turn 主键冲突令整批回滚。
        let boundary = second[1].sequence;
        let failing = vec![
            (
                RunEvent::ClientVisibleContent {
                    text: "x".into(),
                    parts: Vec::new(),
                    block_id: "test-block".into(),
                    item: Value::Null,
                    complete: true,
                },
                None,
                5,
            ),
            (
                RunEvent::ModelTurnStarted {
                    model_turn_id: "interleaved-turn".into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                None,
                5,
            ),
        ];
        assert!(
            store
                .persist_run_events("interleaved", "interleaved", &failing, i64::MAX)
                .await
                .is_err()
        );
        assert!(store.replay(boundary).await?.is_empty());

        let after = store
            .persist_run_event(
                "interleaved",
                "interleaved",
                &RunEvent::ObservationGap {
                    reason: "single".into(),
                },
                6,
                i64::MAX,
            )
            .await?
            .expect("post-rollback event");
        if matches!(store, ObservationStore::Sqlite(..)) {
            assert_eq!(after.sequence, boundary + 1);
        } else {
            assert!(after.sequence > boundary);
        }
        assert_eq!(store.max_sequence().await?, after.sequence);
        let last_sequence: i64 = match store {
            ObservationStore::Sqlite(pool, _, _) => sqlx::query_scalar(
                "SELECT last_event_sequence FROM inference_run_observations WHERE id='interleaved'",
            )
            .fetch_one(pool)
            .await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_scalar(
                "SELECT last_event_sequence FROM inference_run_observations WHERE id='interleaved'",
            )
            .fetch_one(pool)
            .await?,
        };
        assert_eq!(last_sequence, after.sequence);
        Ok(())
    }

    #[tokio::test]
    async fn filtered_events_and_interleaved_batches_share_one_sequence_order() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        interleaved_batch_scenario(&store).await?;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn postgres_interleaved_batches_when_configured() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("DB_URL") else {
            eprintln!("跳过 PostgreSQL 动态验证：未显式设置 DB_URL");
            return Ok(());
        };
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_obs_batch_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let result = async {
            let options: sqlx::postgres::PgConnectOptions = url.parse()?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with(options.options([("search_path", schema.as_str())]))
                .await?;
            let result = async {
                crate::migrations::migrate_postgres(&pool, None).await?;
                interleaved_batch_scenario(&ObservationStore::Postgres(
                    pool.clone(),
                    Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
                ))
                .await
            }
            .await;
            pool.close().await;
            result
        }
        .await;
        let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await;
        admin.close().await;
        result?;
        cleanup?;
        Ok(())
    }

    #[tokio::test]
    async fn model_turns_separate_visible_tail_paragraphs() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_tool_run(&store, "turns", None, "alice").await?;
        let turn = |id: &str| RunEvent::ModelTurnStarted {
            model_turn_id: id.into(),
            route_id: "route".into(),
            model_display_name: None,
            estimated_input_tokens: None,
        };
        let text = |text: &str| RunEvent::ClientVisibleContent {
            text: text.into(),
            parts: Vec::new(),
            block_id: "test-block".into(),
            item: Value::Null,
            complete: true,
        };
        // 首个 Turn 之前无输出：不产生前导分隔；同批与跨批的 Turn 边界都要分隔；无输出的 Turn 不叠加分隔。
        for batch in [
            vec![turn("t1"), text("first")],
            vec![turn("t2")],
            vec![turn("t3"), text("second"), turn("t4"), text("third")],
        ] {
            let events: Vec<_> = batch.into_iter().map(|event| (event, None, 2)).collect();
            store
                .persist_run_events("turns", "turns", &events, i64::MAX)
                .await?;
        }
        let tail: String = sqlx::query_scalar(
            "SELECT visible_tail FROM interaction_observations WHERE id='turns'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(tail, "first\n\nsecond\n\nthird");
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn unknown_tool_result_breaks_replay_baseline() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_tool_run(&store, "root", None, "alice").await?;
        store
            .persist_run_event(
                "root",
                "root",
                &RunEvent::ClientToolHandoff {
                    tool_id: "call".into(),
                    name: "probe".into(),
                    input: None,
                },
                2,
                i64::MAX,
            )
            .await?;
        let result = |content| RunEvent::ClientToolResult {
            tool_id: "call".into(),
            content,
            is_error: false,
        };
        let known = serde_json::json!("known");
        store_tool_batch(&store, "root", vec![result(known.clone())]).await?;
        let batch = store_tool_batch(
            &store,
            "root",
            vec![result(Value::Null), result(known.clone())],
        )
        .await?;
        assert_eq!(
            batch
                .iter()
                .map(|event| &event["content"])
                .collect::<Vec<_>>(),
            [&Value::Null, &known]
        );
        store_tool_batch(&store, "root", vec![result(Value::Null)]).await?;
        let after_unknown = store_tool_batch(&store, "root", vec![result(known.clone())]).await?;
        assert_eq!(
            after_unknown
                .iter()
                .map(|event| &event["content"])
                .collect::<Vec<_>>(),
            [&known]
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn tool_results_follow_call_boundaries_and_principal_lineage_after_restart()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        let handoff = || RunEvent::ClientToolHandoff {
            tool_id: "call".into(),
            name: "probe".into(),
            input: None,
        };
        let result = |text: &str, is_error| RunEvent::ClientToolResult {
            tool_id: "call".into(),
            content: serde_json::json!({"result": text}),
            is_error,
        };
        admit_tool_run(&store, "root", None, "alice").await?;
        store
            .persist_run_event("root", "root", &handoff(), 2, i64::MAX)
            .await?;
        let first = store_tool_batch(
            &store,
            "root",
            vec![result("original", false), result("original", false)],
        )
        .await?;
        assert_eq!(
            first,
            [
                serde_json::json!({"kind":"client_tool_result", "tool_id":"call", "content":{"result":"original"}, "is_error":false})
            ]
        );
        admit_tool_run(&store, "child", Some("root"), "alice").await?;
        assert!(
            store_tool_batch(&store, "child", vec![result("original", false)])
                .await?
                .is_empty()
        );
        let transitions = store_tool_batch(
            &store,
            "child",
            vec![
                result("changed", false),
                result("changed", false),
                result("changed", true),
                result("original", false),
            ],
        )
        .await?;
        assert_eq!(
            transitions
                .iter()
                .map(|event| (&event["content"]["result"], &event["is_error"]))
                .collect::<Vec<_>>(),
            [
                (&serde_json::json!("changed"), &serde_json::json!(false)),
                (&serde_json::json!("changed"), &serde_json::json!(true)),
                (&serde_json::json!("original"), &serde_json::json!(false)),
            ]
        );
        // Same ID is a new call after a new handoff, even with the same result body.
        store
            .persist_run_event("child", "child", &handoff(), 2, i64::MAX)
            .await?;
        assert_eq!(
            store_tool_batch(&store, "child", vec![result("original", false)]).await?,
            first
        );
        store_tool_batch(&store, "child", vec![result("branch-only", false)]).await?;
        admit_tool_run(&store, "sibling", Some("root"), "alice").await?;
        assert_eq!(
            store_tool_batch(&store, "sibling", vec![result("branch-only", false)]).await?[0]["content"],
            serde_json::json!({"result":"branch-only"})
        );
        admit_tool_run(&store, "foreign", Some("child"), "bob").await?;
        assert_eq!(
            store_tool_batch(&store, "foreign", vec![result("branch-only", false)]).await?[0]["content"],
            serde_json::json!({"result":"branch-only"})
        );
        // No observed handoff means call identity is unknown, not a dedup permission.
        admit_tool_run(&store, "unknown", None, "alice").await?;
        let unknown = store_tool_batch(
            &store,
            "unknown",
            vec![result("original", false), result("original", false)],
        )
        .await?;
        assert_eq!(unknown, [first[0].clone(), first[0].clone()]);
        drop(store);
        pool.close().await;
        let pool = crate::db::init_pool(directory.path()).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_tool_run(&store, "restarted", Some("child"), "alice").await?;
        assert!(
            store_tool_batch(&store, "restarted", vec![result("branch-only", false)])
                .await?
                .is_empty()
        );
        sqlx::query("DROP TABLE observation_events")
            .execute(&pool)
            .await?;
        assert!(
            store
                .filter_client_tool_results(
                    "restarted",
                    "restarted",
                    vec![result("branch-only", false)],
                    2
                )
                .await
                .is_err()
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn event_batch_waits_for_concurrent_sqlite_commit_before_reading_cursor()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_tool_run(&store, "batch-race", None, "alice").await?;
        let mut writer = pool.acquire().await?;
        let mut writer_tx = writer.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE inference_run_observations SET last_active_at=2 WHERE id='batch-race'")
            .execute(&mut *writer_tx)
            .await?;
        let batch_store = store.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut batch = tokio::spawn(async move {
            started_tx.send(()).expect("signal event batch start");
            batch_store
                .persist_run_events(
                    "batch-race",
                    "batch-race",
                    &[(
                        RunEvent::ClientVisibleContent {
                            text: "preserved output".into(),
                            parts: Vec::new(),
                            block_id: "race-block".into(),
                            item: Value::Null,
                            complete: true,
                        },
                        None,
                        3,
                    )],
                    i64::MAX,
                )
                .await
        });
        started_rx.await?;
        let pending = tokio::time::timeout(Duration::from_millis(250), &mut batch).await;
        writer_tx.commit().await?;
        drop(writer);
        assert!(
            pending.is_err(),
            "event batch must wait rather than fail a WAL snapshot upgrade: {pending:?}"
        );
        let committed = batch.await??;
        assert_eq!(committed[0].kind, "client_visible_content");
        let tail: String = sqlx::query_scalar(
            "SELECT visible_tail FROM interaction_observations WHERE id='batch-race'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(tail, "preserved output");
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn child_admission_waits_for_a_concurrent_sqlite_writer() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        let start = |id: &str| RunStart {
            id: id.into(),
            principal: "test-principal".into(),
            api_key_id: None,
            api_key_name: None,
            route_id: "test-route".into(),
            model_display_name: None,
            ingress_protocol: "responses".into(),
        };
        store
            .admit(Admission {
                metadata: None,
                start: &start("parent-run"),
                interaction_id: "parent",
                generation_root_id: Some("root"),
                generation_parent_id: None,
                has_new_user: true,
                ingress_received_at: 0,
                parent_run_id: None,
                parent_interaction_id: None,
                debug_enabled: false,
                inferred_retry: false,
                grouping_reason: "new_root",
                diagnostic_source_run_id: None,
                interrupt_parent: false,
                now: 1,
                expires_at: i64::MAX,
            })
            .await?;

        let mut writer = pool.acquire().await?;
        let writer_tx = writer.begin_with("BEGIN IMMEDIATE").await?;
        let child_store = store.clone();
        let child_start = start("child-run");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut admission = tokio::spawn(async move {
            started_tx.send(()).expect("signal admission start");
            child_store
                .admit(Admission {
                    metadata: None,
                    start: &child_start,
                    interaction_id: "child",
                    generation_root_id: Some("root"),
                    generation_parent_id: None,
                    has_new_user: true,
                    ingress_received_at: 0,
                    parent_run_id: Some("parent-run"),
                    parent_interaction_id: Some("parent"),
                    debug_enabled: false,
                    inferred_retry: false,
                    grouping_reason: "new_root",
                    diagnostic_source_run_id: None,
                    interrupt_parent: true,
                    now: 2,
                    expires_at: i64::MAX,
                })
                .await
        });
        started_rx.await?;
        let pending = tokio::time::timeout(Duration::from_millis(250), &mut admission).await;
        writer_tx.commit().await?;
        drop(writer);
        assert!(
            pending.is_err(),
            "child admission must wait for the writer, not lose its observation: {pending:?}"
        );
        admission.await??;

        let child = store
            .get_interaction("child", ForestQuery::default())
            .await?
            .expect("persisted child Interaction");
        assert_eq!(
            child.interaction.parent_interaction_id.as_deref(),
            Some("parent")
        );
        assert_eq!(
            child
                .root
                .interactions
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["parent", "child"]
        );
        assert_eq!(child.runs[0].parent_run_id.as_deref(), Some("parent-run"));
        let parent = store
            .get_interaction("parent", ForestQuery::default())
            .await?
            .expect("persisted parent Interaction");
        assert!(parent.runs[0].user_interrupted);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn continuation_admission_supersedes_waiting_parent_run() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_waiting_scenario_run(&store, "chain", "parent", None).await?;
        finish_scenario_run(&store, "chain", "parent", "waiting_client").await?;
        admit_waiting_scenario_run(&store, "chain", "child", Some("parent")).await?;
        let detail = store
            .get_interaction("chain", ForestQuery::default())
            .await?
            .unwrap();
        let parent = detail.runs.iter().find(|run| run.id == "parent").unwrap();
        assert_eq!(parent.status, "superseded");
        assert_eq!(parent.terminal_reason.as_deref(), Some("superseded"));
        assert!(!parent.user_interrupted);
        assert!(parent.events.iter().any(|event| {
            event.kind == "run_state_changed"
                && event.payload["status"] == "superseded"
                && event.payload["superseded_by"] == "child"
        }));
        assert_eq!(
            detail
                .runs
                .iter()
                .find(|run| run.id == "child")
                .unwrap()
                .status,
            "running"
        );
        // 接替判定不回写被接替 Run 的请求活动时间（新活动归属续接 Run）。
        let parent_activity: i64 = sqlx::query_scalar(
            "SELECT last_active_at FROM inference_run_observations WHERE id='parent'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(parent_activity, 3);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn interrupt_sweep_marks_only_leaf_waits_interrupted() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_waiting_scenario_run(&store, "swept", "leafwait", None).await?;
        finish_scenario_run(&store, "swept", "leafwait", "waiting_client").await?;
        admit_waiting_scenario_run(&store, "swept", "supwait", None).await?;
        finish_scenario_run(&store, "swept", "supwait", "waiting_client").await?;
        // 修复前的遗留形态：等待 Run 已有续接子节点但状态仍停在 waiting_client。
        sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,expires_at) VALUES ('supchild','swept','supwait','responses','route','running',0,1,1,?)")
            .bind(i64::MAX).execute(&pool).await?;
        admit_waiting_scenario_run(&store, "swept", "crosswait", None).await?;
        finish_scenario_run(&store, "swept", "crosswait", "waiting_client").await?;
        // 其他 Interaction 的新分支不算续接，不解除本交互内的等待叶。
        admit_waiting_scenario_run(&store, "other", "xchild", Some("crosswait")).await?;
        let mut tx = pool.begin().await?;
        interrupt_predecessors_sqlite(&mut tx, "swept", 9).await?;
        tx.commit().await?;
        let detail = store
            .get_interaction("swept", ForestQuery::default())
            .await?
            .unwrap();
        let leaf = detail.runs.iter().find(|run| run.id == "leafwait").unwrap();
        assert_eq!(leaf.status, "user_interrupted");
        assert!(leaf.user_interrupted);
        let superseded = detail.runs.iter().find(|run| run.id == "supwait").unwrap();
        assert_eq!(superseded.status, "superseded");
        assert_eq!(superseded.terminal_reason.as_deref(), Some("superseded"));
        assert!(!superseded.user_interrupted);
        let cross = detail
            .runs
            .iter()
            .find(|run| run.id == "crosswait")
            .unwrap();
        assert_eq!(cross.status, "user_interrupted");
        assert!(cross.user_interrupted);
        // 新输入打断属于新 Interaction 的活动：被打断 Run 与 Interaction
        // 保留各自的最后请求时刻，不被判定时刻推到当前窗口。
        let activity: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT r.id,r.last_active_at,i.last_active_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.interaction_id='swept' ORDER BY r.id",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            activity,
            [
                ("crosswait".to_string(), 3, 3),
                ("leafwait".to_string(), 3, 3),
                ("supchild".to_string(), 1, 3),
                ("supwait".to_string(), 3, 3)
            ]
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn delayed_terminal_cleanup_preserves_last_request_activity() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        // waiting_client Run 的真实活动停在 t=3；连接关闭判定远晚于它。
        admit_waiting_scenario_run(&store, "closed", "closed", None).await?;
        store
            .persist_run_event(
                "closed",
                "closed",
                &RunEvent::ClientToolHandoff {
                    tool_id: "call".into(),
                    name: "probe".into(),
                    input: None,
                },
                2,
                i64::MAX,
            )
            .await?;
        finish_scenario_run(&store, "closed", "closed", "waiting_client").await?;
        // 已完成 Run 残留 running model turn，最后一个观察句柄延迟释放。
        admit_tool_run(&store, "gap", None, "alice").await?;
        store
            .persist_run_event(
                "gap",
                "gap",
                &RunEvent::ModelTurnStarted {
                    model_turn_id: "gap-turn".into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                2,
                i64::MAX,
            )
            .await?;
        finish_scenario_run(&store, "gap", "gap", "completed").await?;
        let disconnected = store
            .disconnect_waiting_client("closed", 1_000_000)
            .await?
            .expect("waiting leaf disconnects");
        // 判定时刻仍记录在事件上用于展示，但不能写成请求活动时间。
        assert_eq!(disconnected.occurred_at, 1_000_000);
        let gap = store
            .finalize_activity("gap", "gap", 2_000_000, i64::MAX)
            .await?
            .expect("residual activity closes as observation gap");
        assert_eq!(gap.kind, "observation_gap");
        assert_eq!(gap.occurred_at, 2_000_000);
        let activity: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT r.id,r.last_active_at,i.last_active_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id IN ('closed','gap') ORDER BY r.id",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            activity,
            [("closed".to_string(), 3, 3), ("gap".to_string(), 3, 3)],
            "delayed cleanup is a judgment, not new request activity"
        );
        let recent = store
            .query_forest(ForestQuery {
                start_at: Some(900_000),
                end_at: Some(2_100_000),
                ..Default::default()
            })
            .await?;
        assert!(
            recent.roots.is_empty(),
            "interrupted history must stay out of the recent window"
        );
        let history = store
            .query_forest(ForestQuery {
                start_at: Some(0),
                end_at: Some(600_000),
                ..Default::default()
            })
            .await?;
        assert_eq!(
            history
                .roots
                .iter()
                .map(|root| root.id.as_str())
                .collect::<Vec<_>>(),
            ["closed", "gap"],
            "interrupted roots stay on their last real activity page"
        );
        pool.close().await;
        Ok(())
    }
}
