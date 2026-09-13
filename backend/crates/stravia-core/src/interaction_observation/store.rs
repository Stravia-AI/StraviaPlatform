use serde_json::Value;
use sqlx::{Connection, PgPool, Row, SqlitePool};

use super::types::{
    ConfirmedUsage, IngressStart, ObservationEvent, RejectedOutcome, RunEvent, RunOutcome,
    RunStart, TraceManifest,
};

#[derive(Clone)]
pub(super) enum ObservationStore {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

pub(super) struct Admission<'a> {
    pub start: &'a RunStart,
    pub interaction_id: &'a str,
    pub parent_run_id: Option<&'a str>,
    pub parent_interaction_id: Option<&'a str>,
    pub debug_enabled: bool,
    pub inferred_retry: bool,
    pub grouping_reason: &'a str,
    pub now: i64,
    pub expires_at: i64,
}

impl ObservationStore {
    pub fn new(sqlite: Option<SqlitePool>, postgres: Option<PgPool>) -> anyhow::Result<Self> {
        match (sqlite, postgres) {
            (Some(pool), None) => Ok(Self::Sqlite(pool)),
            (None, Some(pool)) => Ok(Self::Postgres(pool)),
            _ => anyhow::bail!("interaction observation requires exactly one SQL backend"),
        }
    }

    pub(super) async fn artifact_available(&self, id: &str, now: i64) -> anyhow::Result<bool> {
        Ok(match self {
            Self::Sqlite(pool) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=? AND state='ready' AND expires_at>?)")
                .bind(id).bind(now).fetch_one(pool).await?,
            Self::Postgres(pool) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=$1 AND state='ready' AND expires_at>$2)")
                .bind(id).bind(now).fetch_one(pool).await?,
        })
    }

    pub(super) async fn tail_candidates(
        &self,
        principal: &str,
        excluding: &str,
        now: i64,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let limit = (super::tail::MAX_CANDIDATES + 1) as i64;
        Ok(match self {
            Self::Sqlite(pool) => sqlx::query_as("SELECT r.id,r.interaction_id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.principal=? AND r.id<>? AND r.generation_node_id IS NOT NULL AND r.expires_at>? AND i.expires_at>? LIMIT ?")
                .bind(principal).bind(excluding).bind(now).bind(now).bind(limit).fetch_all(pool).await?,
            Self::Postgres(pool) => sqlx::query_as("SELECT r.id,r.interaction_id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE i.principal=$1 AND r.id<>$2 AND r.generation_node_id IS NOT NULL AND r.expires_at>$3 AND i.expires_at>$3 LIMIT $4")
                .bind(principal).bind(excluding).bind(now).bind(limit).fetch_all(pool).await?,
        })
    }

    pub async fn admit(&self, admission: Admission<'_>) -> anyhow::Result<ObservationEvent> {
        match self {
            Self::Sqlite(pool) => {
                // 先取得写锁，避免读取父状态后升级事务因并发写入而丢失子 Interaction。
                let mut connection = pool.acquire().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                if let Some(parent) = admission.parent_interaction_id {
                    interrupt_predecessors_sqlite(&mut tx, parent, admission.now).await?;
                }
                let sequence = next_sqlite(&mut tx).await?;
                sqlx::query("INSERT OR IGNORE INTO interaction_observations (id,principal,api_key_id,api_key_name,generation_root_id,parent_interaction_id,root_id,root_run_id,first_route_id,first_model_display_name,status,started_at,last_active_at,last_event_sequence,expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,'running',?,?,?,?)")
                    .bind(admission.interaction_id).bind(&admission.start.principal).bind(&admission.start.api_key_id).bind(&admission.start.api_key_name)
                    .bind(&admission.start.generation_root_id).bind(admission.parent_interaction_id)
                    .bind(admission.start.generation_root_id.as_deref().unwrap_or(admission.interaction_id)).bind(&admission.start.id)
                    .bind(&admission.start.route_id).bind(&admission.start.model_display_name).bind(admission.now).bind(admission.now).bind(sequence).bind(admission.expires_at).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET status='running',last_active_at=?,last_event_sequence=?,expires_at=? WHERE id=?").bind(admission.now).bind(sequence).bind(admission.expires_at).bind(admission.interaction_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,generation_parent_id,ingress_protocol,route_id,model_display_name,status,debug_enabled,started_at,last_active_at,last_event_sequence,expires_at) VALUES (?,?,?,?,?,?,?,'running',?,?,?,?,?)")
                    .bind(&admission.start.id).bind(admission.interaction_id).bind(admission.parent_run_id).bind(&admission.start.generation_parent_id)
                    .bind(&admission.start.ingress_protocol).bind(&admission.start.route_id).bind(&admission.start.model_display_name)
                    .bind(admission.debug_enabled).bind(admission.now).bind(admission.now).bind(sequence).bind(admission.expires_at).execute(&mut *tx).await?;
                let payload = serde_json::json!({"route_id": admission.start.route_id, "model_display_name": admission.start.model_display_name, "debug_enabled": admission.debug_enabled, "inferred_retry": admission.inferred_retry, "grouping_reason": admission.grouping_reason, "ingress_received_at": admission.start.ingress_received_at, "parent_run_id": admission.parent_run_id, "generation_parent_id": admission.start.generation_parent_id, "has_new_user": admission.start.has_new_user, "parent_interaction_id": admission.parent_interaction_id, "root_id": admission.start.generation_root_id.as_deref().unwrap_or(admission.interaction_id)});
                insert_event_sqlite(
                    &mut tx,
                    sequence,
                    admission.now,
                    Some(admission.interaction_id),
                    Some(&admission.start.id),
                    None,
                    "run_admitted",
                    &payload,
                    admission.expires_at,
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
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                if let Some(parent) = admission.parent_interaction_id {
                    interrupt_predecessors_postgres(&mut tx, parent, admission.now).await?;
                }
                let sequence: i64 =
                    sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                        .fetch_one(&mut *tx)
                        .await?;
                sqlx::query("INSERT INTO interaction_observations (id,principal,api_key_id,api_key_name,generation_root_id,parent_interaction_id,root_id,root_run_id,first_route_id,first_model_display_name,status,started_at,last_active_at,last_event_sequence,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'running',$11,$12,$13,$14) ON CONFLICT (id) DO NOTHING")
                    .bind(admission.interaction_id).bind(&admission.start.principal).bind(&admission.start.api_key_id).bind(&admission.start.api_key_name)
                    .bind(&admission.start.generation_root_id).bind(admission.parent_interaction_id)
                    .bind(admission.start.generation_root_id.as_deref().unwrap_or(admission.interaction_id)).bind(&admission.start.id)
                    .bind(&admission.start.route_id).bind(&admission.start.model_display_name).bind(admission.now).bind(admission.now).bind(sequence).bind(admission.expires_at).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET status='running',last_active_at=$1,last_event_sequence=$2,expires_at=$3 WHERE id=$4").bind(admission.now).bind(sequence).bind(admission.expires_at).bind(admission.interaction_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,generation_parent_id,ingress_protocol,route_id,model_display_name,status,debug_enabled,started_at,last_active_at,last_event_sequence,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,'running',$8,$9,$10,$11,$12)")
                    .bind(&admission.start.id).bind(admission.interaction_id).bind(admission.parent_run_id).bind(&admission.start.generation_parent_id)
                    .bind(&admission.start.ingress_protocol).bind(&admission.start.route_id).bind(&admission.start.model_display_name)
                    .bind(admission.debug_enabled).bind(admission.now).bind(admission.now).bind(sequence).bind(admission.expires_at).execute(&mut *tx).await?;
                let payload = serde_json::json!({"route_id": admission.start.route_id, "model_display_name": admission.start.model_display_name, "debug_enabled": admission.debug_enabled, "inferred_retry": admission.inferred_retry, "grouping_reason": admission.grouping_reason, "ingress_received_at": admission.start.ingress_received_at, "parent_run_id": admission.parent_run_id, "generation_parent_id": admission.start.generation_parent_id, "has_new_user": admission.start.has_new_user, "parent_interaction_id": admission.parent_interaction_id, "root_id": admission.start.generation_root_id.as_deref().unwrap_or(admission.interaction_id)});
                insert_event_postgres(
                    &mut tx,
                    sequence,
                    admission.now,
                    Some(admission.interaction_id),
                    Some(&admission.start.id),
                    None,
                    "run_admitted",
                    &payload,
                    admission.expires_at,
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

    pub async fn persist_manifest_event(
        &self,
        interaction_id: &str,
        run_id: &str,
        manifest: &TraceManifest,
        now: i64,
        expires_at: i64,
        completed: bool,
    ) -> anyhow::Result<ObservationEvent> {
        let reason = manifest.reasons.join(",");
        let completed_at = completed.then_some(now);
        let payload = serde_json::json!({
            "trace_id": &manifest.trace_id,
            "status": &manifest.status,
            "bytes_written": manifest.bytes_written,
            "event_count": manifest.event_count,
            "reasons": &manifest.reasons,
        });
        match self {
            Self::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let sequence = next_sqlite(&mut tx).await?;
                sqlx::query("INSERT INTO debug_trace_manifests (trace_id,run_id,rejection_id,relative_directory,bytes_written,event_count,status,partial_reason,created_at,completed_at,expires_at) VALUES (?,?,NULL,?,?,?,?,?,?,?,?) ON CONFLICT(trace_id) DO UPDATE SET bytes_written=excluded.bytes_written,event_count=excluded.event_count,status=excluded.status,partial_reason=excluded.partial_reason,completed_at=excluded.completed_at")
                    .bind(&manifest.trace_id).bind(run_id).bind(&manifest.trace_id)
                    .bind(manifest.bytes_written as i64).bind(manifest.event_count as i64).bind(&manifest.status)
                    .bind(if reason.is_empty(){None}else{Some(reason.as_str())}).bind(now).bind(completed_at).bind(expires_at)
                    .execute(&mut *tx).await?;
                sqlx::query(
                    "UPDATE inference_run_observations SET last_event_sequence=? WHERE id=?",
                )
                .bind(sequence)
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE interaction_observations SET last_event_sequence=? WHERE id=?")
                    .bind(sequence)
                    .bind(interaction_id)
                    .execute(&mut *tx)
                    .await?;
                insert_event_sqlite(
                    &mut tx,
                    sequence,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "trace_manifest_updated",
                    &payload,
                    expires_at,
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    sequence,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "trace_manifest_updated",
                    payload,
                ))
            }
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let sequence: i64 =
                    sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                        .fetch_one(&mut *tx)
                        .await?;
                sqlx::query("INSERT INTO debug_trace_manifests (trace_id,run_id,rejection_id,relative_directory,bytes_written,event_count,status,partial_reason,created_at,completed_at,expires_at) VALUES ($1,$2,NULL,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(trace_id) DO UPDATE SET bytes_written=EXCLUDED.bytes_written,event_count=EXCLUDED.event_count,status=EXCLUDED.status,partial_reason=EXCLUDED.partial_reason,completed_at=EXCLUDED.completed_at")
                    .bind(&manifest.trace_id).bind(run_id).bind(&manifest.trace_id)
                    .bind(manifest.bytes_written as i64).bind(manifest.event_count as i64).bind(&manifest.status)
                    .bind(if reason.is_empty(){None}else{Some(reason.as_str())}).bind(now).bind(completed_at).bind(expires_at)
                    .execute(&mut *tx).await?;
                sqlx::query(
                    "UPDATE inference_run_observations SET last_event_sequence=$1 WHERE id=$2",
                )
                .bind(sequence)
                .bind(run_id)
                .execute(&mut *tx)
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
                    sequence,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "trace_manifest_updated",
                    &payload,
                    expires_at,
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    sequence,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "trace_manifest_updated",
                    payload,
                ))
            }
        }
    }

    // 缺失标记不依赖事件插入或 Debug Trace，事件表写入故障时仍可保留诊断不完整状态。
    pub async fn mark_observation_gap(&self, interaction_id: &str) -> anyhow::Result<bool> {
        let affected = match self {
            Self::Sqlite(pool) => {
                sqlx::query("UPDATE interaction_observations SET observation_gap=1 WHERE id=?")
                    .bind(interaction_id)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
            Self::Postgres(pool) => {
                sqlx::query("UPDATE interaction_observations SET observation_gap=TRUE WHERE id=$1")
                    .bind(interaction_id)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
        };
        Ok(affected != 0)
    }

    pub(super) async fn persist_input_preview(
        &self,
        interaction_id: &str,
        run_id: &str,
        preview: &str,
        now: i64,
        expires_at: i64,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let kind = "input_preview_recorded";
        let payload = serde_json::json!({"kind": kind});
        let sequence = match self {
            Self::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let changed = sqlx::query("UPDATE interaction_observations SET input_preview=? WHERE id=? AND root_run_id=? AND input_preview IS NULL")
                    .bind(preview).bind(interaction_id).bind(run_id).execute(&mut *tx).await?.rows_affected();
                if changed == 0 {
                    return Ok(None);
                }
                let sequence = next_sqlite(&mut tx).await?;
                sqlx::query("UPDATE interaction_observations SET last_event_sequence=? WHERE id=?")
                    .bind(sequence)
                    .bind(interaction_id)
                    .execute(&mut *tx)
                    .await?;
                insert_event_sqlite(
                    &mut tx,
                    sequence,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    kind,
                    &payload,
                    expires_at,
                )
                .await?;
                tx.commit().await?;
                sequence
            }
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let changed = sqlx::query("UPDATE interaction_observations SET input_preview=$1 WHERE id=$2 AND root_run_id=$3 AND input_preview IS NULL")
                    .bind(preview).bind(interaction_id).bind(run_id).execute(&mut *tx).await?.rows_affected();
                if changed == 0 {
                    return Ok(None);
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
                    sequence,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    kind,
                    &payload,
                    expires_at,
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
    pub(super) async fn filter_client_tool_results(
        &self,
        interaction_id: &str,
        run_id: &str,
        events: Vec<RunEvent>,
        now: i64,
    ) -> anyhow::Result<Vec<RunEvent>> {
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::ClientToolResult { tool_id, .. } if !tool_id.is_empty() => {
                    Some(tool_id.as_str())
                }
                _ => None,
            })
            .collect();
        let ids = serde_json::to_string(&ids)?;
        let rows: Vec<Value> = match self {
            Self::Sqlite(pool) => {
                // SQLite 的部分索引匹配依赖 IN 列表顺序，必须与迁移中的谓词保持一致。
                let rows: Vec<String> = sqlx::query_scalar(
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
                            ROW_NUMBER() OVER (PARTITION BY json_extract(e.payload,'$.tool_id') ORDER BY e.sequence DESC) AS position,
                            MAX(CASE WHEN e.kind='client_tool_handoff' THEN e.sequence END)
                                OVER (PARTITION BY json_extract(e.payload,'$.tool_id')) AS handoff_sequence
                        FROM lineage l JOIN observation_events e ON e.run_id=l.id
                        WHERE e.kind IN ('client_tool_handoff','client_tool_result') AND e.expires_at>?3
                            AND json_extract(e.payload,'$.tool_id') IN (SELECT value FROM json_each(?4))
                    )
                    SELECT e.payload FROM ranked r JOIN observation_events e ON e.sequence=r.sequence
                    WHERE r.position=1 AND r.handoff_sequence IS NOT NULL",
                )
                    .bind(run_id).bind(interaction_id).bind(now).bind(&ids).fetch_all(pool).await?;
                rows.into_iter().map(|row| serde_json::from_str(&row)).collect::<Result<_, _>>()?
            }
            Self::Postgres(pool) => sqlx::query_scalar(
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
                        ROW_NUMBER() OVER (PARTITION BY e.payload->>'tool_id' ORDER BY e.sequence DESC) AS position,
                        MAX(CASE WHEN e.kind='client_tool_handoff' THEN e.sequence END)
                            OVER (PARTITION BY e.payload->>'tool_id') AS handoff_sequence
                    FROM lineage l JOIN observation_events e ON e.run_id=l.id
                    WHERE e.kind IN ('client_tool_handoff','client_tool_result') AND e.expires_at>$3
                        AND e.payload->>'tool_id' IN (SELECT jsonb_array_elements_text($4::jsonb))
                )
                SELECT e.payload FROM ranked r JOIN observation_events e ON e.sequence=r.sequence
                WHERE r.position=1 AND r.handoff_sequence IS NOT NULL",
            )
                .bind(run_id).bind(interaction_id).bind(now).bind(&ids).fetch_all(pool).await?,
        };
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
                latest.insert(id, (row["content"].take(), is_error));
            }
        }
        let mut persisted = Vec::new();
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
                && latest
                    .get(tool_id)
                    .is_some_and(|(previous, error)| previous == content && error == is_error)
            {
                continue;
            }
            if content.is_null() {
                latest.remove(tool_id);
            } else if known_calls.contains(tool_id) {
                latest.insert(tool_id.clone(), (content.clone(), *is_error));
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
                RunEvent::Checkpoint { .. } | RunEvent::Wire { .. }
            ) {
                continue;
            }
            let mut payload = serde_json::to_value(run_event)?;
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
                    Self::Sqlite(pool) => sqlx::query_as("SELECT e.interaction_id,e.run_id FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.kind='compaction_operation' AND json_extract(e.payload,'$.operation_id')=? AND r.expires_at>? ORDER BY e.sequence LIMIT 1").bind(operation).bind(now).fetch_optional(pool).await?,
                    Self::Postgres(pool) => sqlx::query_as("SELECT e.interaction_id,e.run_id FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.kind='compaction_operation' AND e.payload->>'operation_id'=$1 AND r.expires_at>$2 ORDER BY e.sequence LIMIT 1").bind(operation).bind(now).fetch_optional(pool).await?,
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
            let encoded = super::codec::encode_payload(&payload)?;
            prepared.push((run_event, now, kind, payload, encoded));
        }
        if prepared.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = Vec::with_capacity(prepared.len());
        match self {
            Self::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let mut status_changed = false;
                let mut visible = String::new();
                for (run_event, at, kind, payload, encoded) in prepared {
                    let sequence = next_sqlite(&mut tx).await?;
                    if let RunEvent::ClientVisibleContentDelta { text } = run_event {
                        visible.push_str(text);
                    } else {
                        apply_sqlite(&mut tx, interaction_id, run_id, run_event, sequence, at)
                            .await?;
                    }
                    status_changed |= status_event(run_event);
                    insert_event_sqlite(
                        &mut tx,
                        sequence,
                        at,
                        Some(interaction_id),
                        Some(run_id),
                        None,
                        &kind,
                        encoded.as_ref().unwrap_or(&payload),
                        expires_at,
                    )
                    .await?;
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
                let last = result.last().expect("nonempty batch");
                if status_changed {
                    recompute_status_sqlite(
                        &mut tx,
                        interaction_id,
                        last.occurred_at,
                        last.sequence,
                    )
                    .await?;
                }
                if !visible.is_empty() {
                    sqlx::query("UPDATE interaction_observations SET visible_tail=substr(visible_tail || ?, -4096) WHERE id=?").bind(&visible).bind(interaction_id).execute(&mut *tx).await?;
                }
                sqlx::query("UPDATE inference_run_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=?,expires_at=? WHERE id=?").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=?,expires_at=? WHERE id=?").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(interaction_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let mut status_changed = false;
                let mut visible = String::new();
                for (run_event, at, kind, payload, encoded) in prepared {
                    let sequence =
                        sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                            .fetch_one(&mut *tx)
                            .await?;
                    if let RunEvent::ClientVisibleContentDelta { text } = run_event {
                        visible.push_str(text);
                    } else {
                        apply_postgres(&mut tx, interaction_id, run_id, run_event, sequence, at)
                            .await?;
                    }
                    status_changed |= status_event(run_event);
                    insert_event_postgres(
                        &mut tx,
                        sequence,
                        at,
                        Some(interaction_id),
                        Some(run_id),
                        None,
                        &kind,
                        encoded.as_ref().unwrap_or(&payload),
                        expires_at,
                    )
                    .await?;
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
                let last = result.last().expect("nonempty batch");
                if status_changed {
                    recompute_status_postgres(
                        &mut tx,
                        interaction_id,
                        last.occurred_at,
                        last.sequence,
                    )
                    .await?;
                }
                if !visible.is_empty() {
                    sqlx::query("UPDATE interaction_observations SET visible_tail=RIGHT(visible_tail || $1,4096) WHERE id=$2").bind(&visible).bind(interaction_id).execute(&mut *tx).await?;
                }
                sqlx::query("UPDATE inference_run_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2,expires_at=$3 WHERE id=$4").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(run_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2,expires_at=$3 WHERE id=$4").bind(last.occurred_at).bind(last.sequence).bind(expires_at).bind(interaction_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
        }
        Ok(result)
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
            Self::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let seq = next_sqlite(&mut tx).await?;
                let interrupted: bool = sqlx::query_scalar("UPDATE inference_run_observations SET status=CASE WHEN user_interrupted=1 THEN 'user_interrupted' ELSE ? END,terminal_reason=CASE WHEN user_interrupted=1 THEN 'user_interrupted' ELSE ? END,generation_node_id=COALESCE(?,generation_node_id),finished_at=?,last_active_at=?,last_event_sequence=? WHERE id=? RETURNING user_interrupted").bind(&outcome.status).bind(&outcome.terminal_reason).bind(&outcome.generation_node_id).bind(now).bind(now).bind(seq).bind(run_id).fetch_one(&mut *tx).await?;
                let payload = finish_payload(outcome, interrupted)?;
                recompute_status_sqlite(&mut tx, interaction_id, now, seq).await?;
                insert_event_sqlite(
                    &mut tx,
                    seq,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_finished",
                    &payload,
                    expires_at,
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    seq,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_finished",
                    payload,
                ))
            }
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                let interrupted: bool = sqlx::query_scalar("UPDATE inference_run_observations SET status=CASE WHEN user_interrupted THEN 'user_interrupted' ELSE $1 END,terminal_reason=CASE WHEN user_interrupted THEN 'user_interrupted' ELSE $2 END,generation_node_id=COALESCE($3,generation_node_id),finished_at=$4,last_active_at=$5,last_event_sequence=$6 WHERE id=$7 RETURNING user_interrupted").bind(&outcome.status).bind(&outcome.terminal_reason).bind(&outcome.generation_node_id).bind(now).bind(now).bind(seq).bind(run_id).fetch_one(&mut *tx).await?;
                let payload = finish_payload(outcome, interrupted)?;
                recompute_status_postgres(&mut tx, interaction_id, now, seq).await?;
                insert_event_postgres(
                    &mut tx,
                    seq,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_finished",
                    &payload,
                    expires_at,
                )
                .await?;
                tx.commit().await?;
                Ok(event(
                    seq,
                    now,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_finished",
                    payload,
                ))
            }
        }
    }

    pub async fn reject(
        &self,
        ingress: &IngressStart,
        outcome: &RejectedOutcome,
        debug: bool,
        now: i64,
        expires_at: i64,
    ) -> anyhow::Result<ObservationEvent> {
        let payload = serde_json::to_value(outcome)?;
        match self {
            Self::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let seq = next_sqlite(&mut tx).await?;
                sqlx::query("INSERT INTO rejected_request_observations (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,debug_status,last_event_sequence,expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)").bind(&ingress.id).bind(now).bind(&ingress.method).bind(&ingress.path).bind(&ingress.protocol).bind(&outcome.stage).bind(&outcome.code).bind(outcome.status_code).bind(debug).bind(if debug{"complete"}else{"none"}).bind(seq).bind(expires_at).execute(&mut *tx).await?;
                insert_event_sqlite(
                    &mut tx,
                    seq,
                    now,
                    None,
                    None,
                    Some(&ingress.id),
                    "request_rejected",
                    &payload,
                    expires_at,
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
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO rejected_request_observations (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,debug_status,last_event_sequence,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)").bind(&ingress.id).bind(now).bind(&ingress.method).bind(&ingress.path).bind(&ingress.protocol).bind(&outcome.stage).bind(&outcome.code).bind(i64::from(outcome.status_code)).bind(debug).bind(if debug{"complete"}else{"none"}).bind(seq).bind(expires_at).execute(&mut *tx).await?;
                insert_event_postgres(
                    &mut tx,
                    seq,
                    now,
                    None,
                    None,
                    Some(&ingress.id),
                    "request_rejected",
                    &payload,
                    expires_at,
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
            Self::Sqlite(pool) => Ok(sqlx::query_as("SELECT r.interaction_id,r.id AS run_id,(SELECT json_extract(e.payload,'$.delivery_completed_at') FROM observation_events e WHERE e.run_id=r.id AND e.kind='run_finished' ORDER BY e.sequence LIMIT 1) AS delivery_completed_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.generation_node_id=? AND i.principal=? LIMIT 1")
                .bind(generation_node_id).bind(principal).fetch_optional(pool).await?),
            Self::Postgres(pool) => Ok(sqlx::query_as("SELECT r.interaction_id,r.id AS run_id,(SELECT (e.payload->>'delivery_completed_at')::bigint FROM observation_events e WHERE e.run_id=r.id AND e.kind='run_finished' ORDER BY e.sequence LIMIT 1) AS delivery_completed_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.generation_node_id=$1 AND i.principal=$2 LIMIT 1")
                .bind(generation_node_id).bind(principal).fetch_optional(pool).await?),
        }
    }

    pub async fn generation_parent(
        &self,
        generation_node_id: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        match self {
            Self::Sqlite(pool) => Ok(sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations WHERE generation_node_id=? LIMIT 1").bind(generation_node_id).fetch_optional(pool).await?),
            Self::Postgres(pool) => Ok(sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations WHERE generation_node_id=$1 LIMIT 1").bind(generation_node_id).fetch_optional(pool).await?),
        }
    }

    pub async fn recover_after_restart(&self) -> anyhow::Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        match self {
            Self::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let runs:Vec<(String,String,String,i64)>=sqlx::query_as("SELECT r.interaction_id,r.id,r.status,r.expires_at FROM inference_run_observations r WHERE r.status='running' OR r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations mt WHERE mt.run_id=r.id AND mt.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations ta WHERE ta.run_id=r.id AND ta.status='running')").fetch_all(&mut *tx).await?;
                sqlx::query("UPDATE debug_trace_manifests SET status='partial',partial_reason='process_interrupted',completed_at=COALESCE(completed_at,?) WHERE status IN ('running','writing')").bind(now).execute(&mut *tx).await?;
                for (iid, rid, old_status, expires_at) in &runs {
                    let seq = next_sqlite(&mut tx).await?;
                    let status = if old_status == "running" {
                        "interrupted"
                    } else {
                        old_status.as_str()
                    };
                    sqlx::query("UPDATE inference_run_observations SET status=?,terminal_reason=CASE WHEN status='running' THEN 'process_restarted' ELSE terminal_reason END,finished_at=CASE WHEN status='running' THEN COALESCE(finished_at,?) ELSE finished_at END,last_active_at=?,background_active=0,last_event_sequence=? WHERE id=?").bind(status).bind(now).bind(now).bind(seq).bind(rid).execute(&mut *tx).await?;
                    let payload = serde_json::json!({"status":status,"reason":"process_restarted"});
                    insert_event_sqlite(
                        &mut tx,
                        seq,
                        now,
                        Some(iid),
                        Some(rid),
                        None,
                        "process_restarted",
                        &payload,
                        *expires_at,
                    )
                    .await?;
                }
                sqlx::query("UPDATE target_attempt_observations SET status='interrupted',error_code=COALESCE(error_code,'process_restarted'),finished_at=COALESCE(finished_at,?),last_event_sequence=(SELECT r.last_event_sequence FROM inference_run_observations r WHERE r.id=target_attempt_observations.run_id) WHERE status='running'").bind(now).execute(&mut *tx).await?;
                sqlx::query("UPDATE model_turn_observations SET status='interrupted',finished_at=COALESCE(finished_at,?),last_event_sequence=(SELECT r.last_event_sequence FROM inference_run_observations r WHERE r.id=model_turn_observations.run_id) WHERE status='running'").bind(now).execute(&mut *tx).await?;
                let affected: std::collections::HashSet<_> =
                    runs.iter().map(|r| r.0.as_str()).collect();
                for id in affected {
                    let seq:i64=sqlx::query_scalar("SELECT MAX(last_event_sequence) FROM inference_run_observations WHERE interaction_id=?").bind(id).fetch_one(&mut *tx).await?;
                    recompute_status_sqlite(&mut tx, id, now, seq).await?;
                }
                tx.commit().await?;
            }
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let runs:Vec<(String,String,String,i64)>=sqlx::query_as("SELECT r.interaction_id,r.id,r.status,r.expires_at FROM inference_run_observations r WHERE r.status='running' OR r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations mt WHERE mt.run_id=r.id AND mt.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations ta WHERE ta.run_id=r.id AND ta.status='running')").fetch_all(&mut *tx).await?;
                sqlx::query("UPDATE debug_trace_manifests SET status='partial',partial_reason='process_interrupted',completed_at=COALESCE(completed_at,$1) WHERE status IN ('running','writing')").bind(now).execute(&mut *tx).await?;
                for (iid, rid, old_status, expires_at) in &runs {
                    let seq: i64 =
                        sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                            .fetch_one(&mut *tx)
                            .await?;
                    let status = if old_status == "running" {
                        "interrupted"
                    } else {
                        old_status.as_str()
                    };
                    sqlx::query("UPDATE inference_run_observations SET status=$1,terminal_reason=CASE WHEN status='running' THEN 'process_restarted' ELSE terminal_reason END,finished_at=CASE WHEN status='running' THEN COALESCE(finished_at,$2) ELSE finished_at END,last_active_at=$2,background_active=0,last_event_sequence=$3 WHERE id=$4").bind(status).bind(now).bind(seq).bind(rid).execute(&mut *tx).await?;
                    let payload = serde_json::json!({"status":status,"reason":"process_restarted"});
                    insert_event_postgres(
                        &mut tx,
                        seq,
                        now,
                        Some(iid),
                        Some(rid),
                        None,
                        "process_restarted",
                        &payload,
                        *expires_at,
                    )
                    .await?;
                }
                sqlx::query("UPDATE target_attempt_observations ta SET status='interrupted',error_code=COALESCE(ta.error_code,'process_restarted'),finished_at=COALESCE(ta.finished_at,$1),last_event_sequence=r.last_event_sequence FROM inference_run_observations r WHERE ta.status='running' AND r.id=ta.run_id").bind(now).execute(&mut *tx).await?;
                sqlx::query("UPDATE model_turn_observations mt SET status='interrupted',finished_at=COALESCE(mt.finished_at,$1),last_event_sequence=r.last_event_sequence FROM inference_run_observations r WHERE mt.status='running' AND r.id=mt.run_id").bind(now).execute(&mut *tx).await?;
                let affected: std::collections::HashSet<_> =
                    runs.iter().map(|r| r.0.as_str()).collect();
                for id in affected {
                    let seq:i64=sqlx::query_scalar("SELECT MAX(last_event_sequence) FROM inference_run_observations WHERE interaction_id=$1").bind(id).fetch_one(&mut *tx).await?;
                    recompute_status_postgres(&mut tx, id, now, seq).await?;
                }
                tx.commit().await?;
            }
        }
        Ok(())
    }

    pub async fn max_sequence(&self) -> anyhow::Result<i64> {
        match self{Self::Sqlite(p)=>Ok(sqlx::query_scalar("SELECT next_sequence-1 FROM observation_sequence WHERE singleton_id=1").fetch_one(p).await?),Self::Postgres(p)=>Ok(sqlx::query_scalar("SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM observation_event_sequence").fetch_one(p).await?)}
    }
    pub async fn min_sequence(&self) -> anyhow::Result<Option<i64>> {
        match self {
            Self::Sqlite(p) => Ok(sqlx::query_scalar(
                "SELECT MIN(sequence) FROM observation_events",
            )
            .fetch_one(p)
            .await?),
            Self::Postgres(p) => Ok(sqlx::query_scalar(
                "SELECT MIN(sequence) FROM observation_events",
            )
            .fetch_one(p)
            .await?),
        }
    }
    pub async fn replay(&self, after: i64) -> anyhow::Result<Vec<ObservationEvent>> {
        match self {
            Self::Sqlite(p) => load_events_sqlite(p, after).await,
            Self::Postgres(p) => load_events_postgres(p, after).await,
        }
    }
}

fn finish_payload(outcome: &RunOutcome, interrupted: bool) -> anyhow::Result<Value> {
    let mut payload = serde_json::to_value(outcome)?;
    if interrupted {
        payload["status"] = Value::String("user_interrupted".into());
        payload["terminal_reason"] = Value::String("user_interrupted".into());
    }
    Ok(payload)
}

async fn interrupt_predecessors_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    interaction_id: &str,
    now: i64,
) -> anyhow::Result<()> {
    let runs:Vec<(String,String,i64)>=sqlx::query_as("SELECT id,status,expires_at FROM inference_run_observations WHERE interaction_id=? AND status IN ('running','waiting_client')").bind(interaction_id).fetch_all(&mut **tx).await?;
    let mut last = None;
    for (rid, old_status, expires_at) in runs {
        let seq = next_sqlite(tx).await?;
        let status = if old_status == "waiting_client" {
            "user_interrupted"
        } else {
            "running"
        };
        sqlx::query("UPDATE inference_run_observations SET user_interrupted=1,status=?,terminal_reason='user_interrupted',finished_at=CASE WHEN ?='user_interrupted' THEN COALESCE(finished_at,?) ELSE finished_at END,last_active_at=?,last_event_sequence=? WHERE id=?").bind(status).bind(status).bind(now).bind(now).bind(seq).bind(&rid).execute(&mut **tx).await?;
        let payload = serde_json::json!({"status":status,"user_interrupted":true,"reason":"user_interrupted"});
        insert_event_sqlite(
            tx,
            seq,
            now,
            Some(interaction_id),
            Some(&rid),
            None,
            "run_state_changed",
            &payload,
            expires_at,
        )
        .await?;
        last = Some(seq);
    }
    if let Some(seq) = last {
        recompute_status_sqlite(tx, interaction_id, now, seq).await?;
    }
    Ok(())
}
async fn interrupt_predecessors_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    interaction_id: &str,
    now: i64,
) -> anyhow::Result<()> {
    let runs:Vec<(String,String,i64)>=sqlx::query_as("SELECT id,status,expires_at FROM inference_run_observations WHERE interaction_id=$1 AND status IN ('running','waiting_client')").bind(interaction_id).fetch_all(&mut **tx).await?;
    let mut last = None;
    for (rid, old_status, expires_at) in runs {
        let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
            .fetch_one(&mut **tx)
            .await?;
        let status = if old_status == "waiting_client" {
            "user_interrupted"
        } else {
            "running"
        };
        sqlx::query("UPDATE inference_run_observations SET user_interrupted=TRUE,status=$1,terminal_reason='user_interrupted',finished_at=CASE WHEN $1='user_interrupted' THEN COALESCE(finished_at,$2) ELSE finished_at END,last_active_at=$2,last_event_sequence=$3 WHERE id=$4").bind(status).bind(now).bind(seq).bind(&rid).execute(&mut **tx).await?;
        let payload = serde_json::json!({"status":status,"user_interrupted":true,"reason":"user_interrupted"});
        insert_event_postgres(
            tx,
            seq,
            now,
            Some(interaction_id),
            Some(&rid),
            None,
            "run_state_changed",
            &payload,
            expires_at,
        )
        .await?;
        last = Some(seq);
    }
    if let Some(seq) = last {
        recompute_status_postgres(tx, interaction_id, now, seq).await?;
    }
    Ok(())
}

async fn next_sqlite(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> anyhow::Result<i64> {
    sqlx::query(
        "UPDATE observation_sequence SET next_sequence=next_sequence+1 WHERE singleton_id=1",
    )
    .execute(&mut **tx)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT next_sequence-1 FROM observation_sequence WHERE singleton_id=1")
            .fetch_one(&mut **tx)
            .await?,
    )
}

async fn insert_event_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    seq: i64,
    at: i64,
    interaction_id: Option<&str>,
    run_id: Option<&str>,
    rejection_id: Option<&str>,
    kind: &str,
    payload: &Value,
    expires: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO observation_events (sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload,expires_at) VALUES (?,?,?,?,?,?,?,?)").bind(seq).bind(at).bind(interaction_id).bind(run_id).bind(rejection_id).bind(kind).bind(serde_json::to_string(payload)?).bind(expires).execute(&mut **tx).await?;
    Ok(())
}
async fn insert_event_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    seq: i64,
    at: i64,
    interaction_id: Option<&str>,
    run_id: Option<&str>,
    rejection_id: Option<&str>,
    kind: &str,
    payload: &Value,
    expires: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO observation_events (sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)").bind(seq).bind(at).bind(interaction_id).bind(run_id).bind(rejection_id).bind(kind).bind(payload).bind(expires).execute(&mut **tx).await?;
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
        RunEvent::GenerationAssociated {
            root_id, parent_id, ..
        } => {
            sqlx::query("UPDATE inference_run_observations SET generation_parent_id=? WHERE id=?")
                .bind(parent_id)
                .bind(rid)
                .execute(&mut **tx)
                .await?;
            sqlx::query("UPDATE interaction_observations SET generation_root_id=COALESCE(generation_root_id,?),root_id=COALESCE(generation_root_id,?) WHERE id=?").bind(root_id).bind(root_id).bind(iid).execute(&mut **tx).await?;
        }
        RunEvent::ModelTurnStarted {
            model_turn_id,
            route_id,
            model_display_name,
        } => {
            let key: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT api_key_id,api_key_name FROM interaction_observations WHERE id=?",
            )
            .bind(iid)
            .fetch_one(&mut **tx)
            .await?;
            sqlx::query("INSERT INTO model_turn_observations (id,run_id,interaction_id,route_id,model_display_name,api_key_id,api_key_name,status,started_at,last_event_sequence) VALUES (?,?,?,?,?,?,?,'running',?,?)").bind(model_turn_id).bind(rid).bind(iid).bind(route_id).bind(model_display_name).bind(key.0).bind(key.1).bind(now).bind(seq).execute(&mut **tx).await?;
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
            ..
        } => {
            sqlx::query("UPDATE target_attempt_observations SET status=?,status_code=?,error_code=?,finished_at=?,duration_ms=?,first_token_ms=?,last_event_sequence=? WHERE id=?").bind(status).bind(status_code.map(i64::from)).bind(error_code).bind(now).bind(duration_ms).bind(first_token_ms).bind(seq).bind(attempt_id).execute(&mut **tx).await?;
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
        RunEvent::ClientOutputCommitted => {
            sqlx::query(
                "UPDATE inference_run_observations SET client_output_committed=1 WHERE id=?",
            )
            .bind(rid)
            .execute(&mut **tx)
            .await?;
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
        RunEvent::GenerationAssociated {
            root_id, parent_id, ..
        } => {
            sqlx::query(
                "UPDATE inference_run_observations SET generation_parent_id=$1 WHERE id=$2",
            )
            .bind(parent_id)
            .bind(rid)
            .execute(&mut **tx)
            .await?;
            sqlx::query("UPDATE interaction_observations SET generation_root_id=COALESCE(generation_root_id,$1),root_id=COALESCE(generation_root_id,$1) WHERE id=$2").bind(root_id).bind(iid).execute(&mut **tx).await?;
        }
        RunEvent::ModelTurnStarted {
            model_turn_id,
            route_id,
            model_display_name,
        } => {
            let key: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT api_key_id,api_key_name FROM interaction_observations WHERE id=$1",
            )
            .bind(iid)
            .fetch_one(&mut **tx)
            .await?;
            sqlx::query("INSERT INTO model_turn_observations (id,run_id,interaction_id,route_id,model_display_name,api_key_id,api_key_name,status,started_at,last_event_sequence) VALUES ($1,$2,$3,$4,$5,$6,$7,'running',$8,$9)").bind(model_turn_id).bind(rid).bind(iid).bind(route_id).bind(model_display_name).bind(key.0).bind(key.1).bind(now).bind(seq).execute(&mut **tx).await?;
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
            ..
        } => {
            sqlx::query("UPDATE target_attempt_observations SET status=$1,status_code=$2,error_code=$3,finished_at=$4,duration_ms=$5,first_token_ms=$6,last_event_sequence=$7 WHERE id=$8").bind(status).bind(status_code.map(i64::from)).bind(error_code).bind(now).bind(duration_ms).bind(first_token_ms).bind(seq).bind(attempt_id).execute(&mut **tx).await?;
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
        RunEvent::ClientOutputCommitted => {
            sqlx::query(
                "UPDATE inference_run_observations SET client_output_committed=TRUE WHERE id=$1",
            )
            .bind(rid)
            .execute(&mut **tx)
            .await?;
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
        sqlx::query("UPDATE model_turn_observations SET input_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(input_tokens) THEN SUM(input_tokens) END FROM target_attempt_observations WHERE model_turn_id=model_turn_observations.id),output_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(output_tokens) THEN SUM(output_tokens) END FROM target_attempt_observations WHERE model_turn_id=model_turn_observations.id),cache_read_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(cache_read_tokens) THEN SUM(cache_read_tokens) END FROM target_attempt_observations WHERE model_turn_id=model_turn_observations.id),cache_write_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(cache_write_tokens) THEN SUM(cache_write_tokens) END FROM target_attempt_observations WHERE model_turn_id=model_turn_observations.id),reasoning_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(reasoning_tokens) THEN SUM(reasoning_tokens) END FROM target_attempt_observations WHERE model_turn_id=model_turn_observations.id) WHERE id=(SELECT model_turn_id FROM target_attempt_observations WHERE id=?)").bind(aid).execute(&mut **tx).await?;
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
        sqlx::query("UPDATE model_turn_observations SET (input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens)=(SELECT CASE WHEN COUNT(*)=COUNT(input_tokens) THEN SUM(input_tokens) END,CASE WHEN COUNT(*)=COUNT(output_tokens) THEN SUM(output_tokens) END,CASE WHEN COUNT(*)=COUNT(cache_read_tokens) THEN SUM(cache_read_tokens) END,CASE WHEN COUNT(*)=COUNT(cache_write_tokens) THEN SUM(cache_write_tokens) END,CASE WHEN COUNT(*)=COUNT(reasoning_tokens) THEN SUM(reasoning_tokens) END FROM target_attempt_observations WHERE model_turn_id=(SELECT model_turn_id FROM target_attempt_observations WHERE id=$1)) WHERE id=(SELECT model_turn_id FROM target_attempt_observations WHERE id=$1)").bind(aid).execute(&mut **tx).await?;
        recompute_usage_postgres(tx, iid).await?;
    }
    Ok(())
}
async fn recompute_usage_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET input_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(input_tokens) THEN SUM(input_tokens) END FROM target_attempt_observations WHERE interaction_id=?),output_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(output_tokens) THEN SUM(output_tokens) END FROM target_attempt_observations WHERE interaction_id=?),cache_read_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(cache_read_tokens) THEN SUM(cache_read_tokens) END FROM target_attempt_observations WHERE interaction_id=?),cache_write_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(cache_write_tokens) THEN SUM(cache_write_tokens) END FROM target_attempt_observations WHERE interaction_id=?),reasoning_tokens=(SELECT CASE WHEN COUNT(*)=COUNT(reasoning_tokens) THEN SUM(reasoning_tokens) END FROM target_attempt_observations WHERE interaction_id=?) WHERE id=?").bind(iid).bind(iid).bind(iid).bind(iid).bind(iid).bind(iid).execute(&mut **tx).await?;
    Ok(())
}
async fn recompute_usage_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET (input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens)=(SELECT CASE WHEN COUNT(*)=COUNT(input_tokens) THEN SUM(input_tokens) END,CASE WHEN COUNT(*)=COUNT(output_tokens) THEN SUM(output_tokens) END,CASE WHEN COUNT(*)=COUNT(cache_read_tokens) THEN SUM(cache_read_tokens) END,CASE WHEN COUNT(*)=COUNT(cache_write_tokens) THEN SUM(cache_write_tokens) END,CASE WHEN COUNT(*)=COUNT(reasoning_tokens) THEN SUM(reasoning_tokens) END FROM target_attempt_observations WHERE interaction_id=$1) WHERE id=$1").bind(iid).execute(&mut **tx).await?;
    Ok(())
}
async fn recompute_status_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
    now: i64,
    seq: i64,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET status=CASE WHEN EXISTS(SELECT 1 FROM inference_run_observations WHERE interaction_id=? AND (status='running' OR background_active>0)) THEN 'running' WHEN EXISTS(SELECT 1 FROM inference_run_observations r WHERE r.interaction_id=? AND r.status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id)) THEN 'waiting_client' WHEN EXISTS(SELECT 1 FROM inference_run_observations WHERE interaction_id=? AND status='completed') THEN 'completed' ELSE 'interrupted' END,last_active_at=?,last_event_sequence=? WHERE id=?").bind(iid).bind(iid).bind(iid).bind(now).bind(seq).bind(iid).execute(&mut **tx).await?;
    Ok(())
}
async fn recompute_status_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
    now: i64,
    seq: i64,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET status=CASE WHEN EXISTS(SELECT 1 FROM inference_run_observations WHERE interaction_id=$1 AND (status='running' OR background_active>0)) THEN 'running' WHEN EXISTS(SELECT 1 FROM inference_run_observations r WHERE r.interaction_id=$1 AND r.status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id)) THEN 'waiting_client' WHEN EXISTS(SELECT 1 FROM inference_run_observations WHERE interaction_id=$1 AND status='completed') THEN 'completed' ELSE 'interrupted' END,last_active_at=$2,last_event_sequence=$3 WHERE id=$1").bind(iid).bind(now).bind(seq).execute(&mut **tx).await?;
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
            Ok(ObservationEvent {
                sequence: r.try_get(0)?,
                occurred_at: r.try_get(1)?,
                interaction_id: r.try_get(2)?,
                run_id: r.try_get(3)?,
                rejection_id: r.try_get(4)?,
                kind: r.try_get(5)?,
                payload: super::codec::decode_payload(serde_json::from_str(
                    &r.try_get::<String, _>(6)?,
                )?)?,
            })
        })
        .collect()
}
async fn load_events_postgres(pool: &PgPool, after: i64) -> anyhow::Result<Vec<ObservationEvent>> {
    let rows=sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE sequence > $1 ORDER BY sequence LIMIT 512").bind(after).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok(ObservationEvent {
                sequence: r.try_get(0)?,
                occurred_at: r.try_get(1)?,
                interaction_id: r.try_get(2)?,
                run_id: r.try_get(3)?,
                rejection_id: r.try_get(4)?,
                kind: r.try_get(5)?,
                payload: super::codec::decode_payload(r.try_get(6)?)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::interaction_observation::types::ForestQuery;

    async fn admit_tool_run(
        store: &ObservationStore,
        id: &str,
        parent: Option<&str>,
        principal: &str,
    ) -> anyhow::Result<()> {
        store
            .admit(Admission {
                start: &RunStart {
                    id: id.into(),
                    principal: principal.into(),
                    api_key_id: None,
                    api_key_name: None,
                    generation_root_id: None,
                    generation_parent_id: None,
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    ingress_received_at: 0,
                    canonical_fingerprint: id.into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                interaction_id: id,
                parent_run_id: parent,
                parent_interaction_id: None,
                debug_enabled: false,
                inferred_retry: false,
                grouping_reason: "new_root",
                now: 1,
                expires_at: i64::MAX,
            })
            .await?;
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
        crate::migrations::migrate_sqlite(&pool).await?;
        let store = ObservationStore::Sqlite(pool.clone());
        admit_tool_run(&store, "batch", None, "alice").await?;
        let batch = vec![
            (
                RunEvent::ClientVisibleContentDelta {
                    text: "你好".repeat(1024),
                },
                Some("block-a".into()),
                2,
            ),
            (
                RunEvent::ClientVisibleContentDelta {
                    text: "世界".into(),
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
                RunEvent::ClientVisibleContentDelta {
                    text: "must rollback".into(),
                },
                Some("rollback".into()),
                4,
            ),
            (
                RunEvent::ModelTurnStarted {
                    model_turn_id: "duplicate".into(),
                    route_id: "route".into(),
                    model_display_name: None,
                },
                None,
                4,
            ),
            (
                RunEvent::ModelTurnStarted {
                    model_turn_id: "duplicate".into(),
                    route_id: "route".into(),
                    model_display_name: None,
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
                    RunEvent::ClientVisibleContentDelta {
                        text: "delayed block".into(),
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

    #[tokio::test]
    async fn unknown_tool_result_breaks_replay_baseline() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool).await?;
        let store = ObservationStore::Sqlite(pool.clone());
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
        crate::migrations::migrate_sqlite(&pool).await?;
        let store = ObservationStore::Sqlite(pool.clone());
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
        let store = ObservationStore::Sqlite(pool.clone());
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
    async fn child_admission_waits_for_a_concurrent_sqlite_writer() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool).await?;
        let store = ObservationStore::Sqlite(pool.clone());
        let start = |id: &str| RunStart {
            id: id.into(),
            principal: "test-principal".into(),
            api_key_id: None,
            api_key_name: None,
            generation_root_id: Some("root".into()),
            generation_parent_id: None,
            has_new_user: true,
            has_matching_pending_tool_result: false,
            ingress_received_at: 0,
            canonical_fingerprint: id.into(),
            route_id: "test-route".into(),
            model_display_name: None,
            ingress_protocol: "responses".into(),
        };
        store
            .admit(Admission {
                start: &start("parent-run"),
                interaction_id: "parent",
                parent_run_id: None,
                parent_interaction_id: None,
                debug_enabled: false,
                inferred_retry: false,
                grouping_reason: "new_root",
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
                    start: &child_start,
                    interaction_id: "child",
                    parent_run_id: Some("parent-run"),
                    parent_interaction_id: Some("parent"),
                    debug_enabled: false,
                    inferred_retry: false,
                    grouping_reason: "new_root",
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
}

fn status_event(event: &RunEvent) -> bool {
    matches!(
        event,
        RunEvent::ModelTurnStarted { .. }
            | RunEvent::ModelTurnFinished { .. }
            | RunEvent::PlatformToolStarted { .. }
            | RunEvent::PlatformToolFinished { .. }
            | RunEvent::ClientToolHandoff { .. }
    )
}
