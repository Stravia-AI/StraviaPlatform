use super::manifest_index::DebugTraceIndex;
use anyhow::Context;
use serde_json::Value;
use sqlx::{PgPool, Row, SqlitePool};
use std::{borrow::Cow, sync::Arc};

use super::content_refs::{text_content, value_content};
use super::event_writes::{BlockRow, EventWrites};
use super::types::{
    ConfirmedUsage, CredentialDiscovery, IngressStart, ObservationEvent, RejectedOutcome, RunEvent,
    RunOutcome, RunStart, parse_item_block_id,
};
use crate::content_store::Content;
use crate::pricing::{AttemptAmounts, BillingOutcome, CostBasis};

/// 读取侧拼接可见输出时，不同 Model Turn 的输出以空行分隔，才能按 Markdown 段落换行。
pub(super) const TURN_SEPARATOR: &str = "\n\n";

#[derive(Clone)]
pub(super) enum ObservationStore {
    Sqlite(SqlitePool, Arc<DebugTraceIndex>),
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
    /// 与准入同一事务落库的随附事件（父链缺口、Retained Tail 诊断），按序紧随 `run_admitted`。
    /// 它们是可选诊断：写入失败只回滚随附部分，准入本身照常提交。
    pub follow_ups: &'a [RunEvent],
}

/// 准入结果。`event`（`run_admitted`）必定已提交；随附事件整体成败，失败原因留在
/// `follow_up_failure`，由调用方决定记缺口还是只告警。
#[derive(Debug)]
pub(super) struct Admitted {
    pub event: ObservationEvent,
    pub follow_ups: Vec<ObservationEvent>,
    pub follow_up_failure: Option<anyhow::Error>,
}

pub(super) struct Rejection<'a> {
    pub ingress: &'a IngressStart,
    pub outcome: &'a RejectedOutcome,
    pub metadata: &'a super::RequestMetadata,
    pub debug_enabled: bool,
    pub occurred_at: i64,
    pub started_at: i64,
    pub duration_ms: i64,
}

/// 一轮 Model Turn 的首个源批次：Turn 开始、首个 attempt 开始、用户输入与工具结果。
pub(super) struct TurnStart<'a> {
    pub turn: &'a RunEvent,
    pub at: i64,
    pub attempt: Option<&'a RunEvent>,
    pub attempt_at: i64,
    pub input: Option<&'a [String]>,
    pub input_at: i64,
    pub tool_results: &'a [(RunEvent, i64)],
    /// 客户端工具结果先于用户输入到达：事件序号按生产方到达顺序分配。
    pub tools_first: bool,
    pub leading: &'a [(RunEvent, i64)],
}

/// 首轮写入里输入事件与工具结果事件在“额外事件”中的位置（0 或 1）。
/// 序号按生产方到达顺序分配，而不是按存储实现的写入顺序。
fn extra_slots(start: &TurnStart<'_>) -> (usize, usize) {
    let has_input = start.input.is_some();
    let has_tools = !start.tool_results.is_empty();
    if start.tools_first {
        (usize::from(has_tools), 0)
    } else {
        (0, usize::from(has_input))
    }
}

/// 一次收到的客户端工具结果在写入前的准备：逐项元数据与待登记正文。
/// 首轮写入与独立的工具结果写入共用，保证两条路径对同一批事件的取舍一致。
struct PreparedTools {
    /// `(tool_id, is_error)`，与 `units` 同序，下标即 ordinal。
    tools: Vec<(String, bool)>,
    units: Vec<Content>,
    /// 合成事件行的发生时间：批内首个结果的到达时间。
    first_at: i64,
    /// 批内最晚的到达时间。
    latest_at: i64,
}

impl PreparedTools {
    fn new(events: &[(RunEvent, i64)]) -> anyhow::Result<Self> {
        let first_at = events
            .first()
            .map(|(_, at)| *at)
            .context("client tool result batch is empty")?;
        let mut tools = Vec::with_capacity(events.len());
        let mut units = Vec::with_capacity(events.len());
        let mut latest_at = first_at;
        for (event, at) in events {
            let RunEvent::ClientToolResult {
                tool_id,
                content,
                is_error,
            } = event
            else {
                anyhow::bail!("client tool result batch contains another event kind");
            };
            tools.push((tool_id.clone(), *is_error));
            units.push(value_content(content));
            latest_at = latest_at.max(*at);
        }
        Ok(Self {
            tools,
            units,
            first_at,
            latest_at,
        })
    }
}

/// Turn 开始前缓冲的普通事件（凭据映射），序列化在开启事务之前完成。
struct PreparedLeading<'a> {
    event: &'a RunEvent,
    at: i64,
    kind: String,
    payload: Value,
}

/// 首轮写入里与后端无关的准备：序列化、拆分正文单元、汇总活动时间与事件条数。
struct PreparedTurnStart<'a> {
    /// 合并了首个 attempt 的 `model_turn_started` 载荷。
    turn_payload: Value,
    leading: Vec<PreparedLeading<'a>>,
    input_units: Option<Vec<Content>>,
    tools: Option<PreparedTools>,
    /// 本次写入涉及的最晚事件时间（不含 attempt 开始时间）。
    latest_at: i64,
    /// 本次写入产生的事件行数，即需要预占的序号个数。
    durable_count: usize,
}

impl<'a> PreparedTurnStart<'a> {
    fn new(start: &TurnStart<'a>) -> anyhow::Result<Self> {
        let mut turn_attrs = attrs_without_kind(start.turn)?;
        if let Some(attempt) = start.attempt {
            turn_attrs.insert(
                "attempt".into(),
                Value::Object(attrs_without_kind(attempt)?),
            );
            turn_attrs.insert("attempt_at".into(), Value::from(start.attempt_at));
        }
        let mut latest_at = start.at;
        let mut leading = Vec::with_capacity(start.leading.len());
        // 不携带发现的凭据映射没有可展示内容，与批量落库路径一样不占事件行。
        for (event, at) in start
            .leading
            .iter()
            .filter(|(event, _)| !is_empty_credential_mapping(event))
        {
            let payload = serde_json::to_value(event)?;
            let kind = event_kind(&payload)?;
            latest_at = latest_at.max(*at);
            leading.push(PreparedLeading {
                event,
                at: *at,
                kind,
                payload,
            });
        }
        let input_units = start.input.map(split_user_input);
        if input_units.is_some() {
            latest_at = latest_at.max(start.input_at);
        }
        let tools = (!start.tool_results.is_empty())
            .then(|| PreparedTools::new(start.tool_results))
            .transpose()?;
        if let Some(tools) = &tools {
            latest_at = latest_at.max(tools.latest_at);
        }
        let durable_count =
            1 + leading.len() + usize::from(input_units.is_some()) + usize::from(tools.is_some());
        Ok(Self {
            turn_payload: Value::Object(turn_attrs),
            leading,
            input_units,
            tools,
            latest_at,
            durable_count,
        })
    }
}

/// 一次首轮写入分到的事件序号按角色排开：Turn 开始，随后是前置事件，
/// 最后是输入与工具结果两个额外事件（先后按生产方到达顺序）。序号取自后端实际分配的值，不假设连续。
struct TurnLayout<'s> {
    turn: i64,
    leading: &'s [i64],
    /// 首个 attempt 并入 Turn 开始行，它的投影行记录最后一个前置事件（无前置则 Turn 开始）的序号。
    attempt: i64,
    input: Option<i64>,
    tools: Option<i64>,
    last: i64,
}

impl<'s> TurnLayout<'s> {
    fn new(start: &TurnStart<'_>, leading: usize, sequences: &'s [i64]) -> anyhow::Result<Self> {
        let has_input = start.input.is_some();
        let has_tools = !start.tool_results.is_empty();
        anyhow::ensure!(
            sequences.len() == 1 + leading + usize::from(has_input) + usize::from(has_tools),
            "turn start allocated an unexpected number of event sequences"
        );
        let (input_slot, tool_slot) = extra_slots(start);
        let extras = &sequences[1 + leading..];
        Ok(Self {
            turn: sequences[0],
            leading: &sequences[1..=leading],
            attempt: sequences[leading],
            input: has_input.then(|| extras[input_slot]),
            tools: has_tools.then(|| extras[tool_slot]),
            last: sequences.iter().copied().max().unwrap_or(sequences[0]),
        })
    }
}

impl ObservationStore {
    pub fn new(
        sqlite: Option<SqlitePool>,
        postgres: Option<PgPool>,
        index: Arc<DebugTraceIndex>,
    ) -> anyhow::Result<Self> {
        match (sqlite, postgres) {
            (Some(pool), None) => Ok(Self::Sqlite(pool, index)),
            (None, Some(pool)) => Ok(Self::Postgres(pool, index)),
            _ => anyhow::bail!("interaction observation requires exactly one SQL backend"),
        }
    }

    pub(super) fn debug_trace_index(&self) -> &DebugTraceIndex {
        match self {
            Self::Sqlite(_, index) | Self::Postgres(_, index) => index,
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
            Self::Sqlite(pool, _) => sqlx::query_as("SELECT input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens FROM target_attempt_observations WHERE id=? AND usage_recorded=1").bind(attempt_id).fetch_optional(pool).await?,
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
            Self::Sqlite(pool, _) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=? AND state='ready' AND expires_at>?)")
                .bind(id).bind(now).fetch_one(pool).await?,
            Self::Postgres(pool, _) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=$1 AND state='ready' AND expires_at>$2)")
                .bind(id).bind(now).fetch_one(pool).await?,
        })
    }

    /// 成功交付先结算客户端分支，尾源与待回传工具 ID 在同一事务落盘；Finish 再补齐执行度量，
    /// 后台活动独立保留。交付时间是幂等键，既有失败和真实 User 中断均不可被晚到收据覆盖。
    /// 事务失败时结算与尾源一起回滚，由调用方记为一个观察缺口。
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.persist_client_completion",
        skip_all
    )]
    pub(super) async fn persist_client_completion(
        &self,
        run_id: &str,
        interaction_id: &str,
        delivered_at: i64,
        waiting_client: bool,
        tail: Option<TailSource<'_>>,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let completion = ClientCompletionWrite {
            delivered_at,
            waiting_client,
            tail,
        };
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin().await?;
                let seq = next_sqlite(&mut tx).await?;
                let Some((status, recorded_at)) = apply_client_completion_sqlite(
                    &mut tx,
                    run_id,
                    interaction_id,
                    &completion,
                    seq,
                )
                .await?
                else {
                    tx.commit().await?;
                    connection.finish().await?;
                    return Ok(None);
                };
                recompute_status_sqlite(&mut tx, interaction_id, seq).await?;
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: recorded_at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "run_state_changed",
                        payload: &Value::Null,
                    },
                )
                .await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(Some(event(
                    seq,
                    recorded_at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_state_changed",
                    serde_json::json!({"status": status, "superseded_by": null}),
                )))
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                let Some((status, recorded_at)) = apply_client_completion_postgres(
                    &mut tx,
                    run_id,
                    interaction_id,
                    &completion,
                    seq,
                )
                .await?
                else {
                    tx.commit().await?;
                    return Ok(None);
                };
                recompute_status_postgres(&mut tx, interaction_id, seq).await?;
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: recorded_at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "run_state_changed",
                        payload: &Value::Null,
                    },
                )
                .await?;
                tx.commit().await?;
                Ok(Some(event(
                    seq,
                    recorded_at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "run_state_changed",
                    serde_json::json!({"status": status, "superseded_by": null}),
                )))
            }
        }
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
            Self::Sqlite(pool, _) => {
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
            Self::Sqlite(pool, _) => {
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
    ) -> anyhow::Result<Vec<(String, Option<String>)>> {
        if tool_ids.is_empty() {
            return Ok(Vec::new());
        }
        // 保留部分索引的 IN 谓词；完整输入证明由 Attribution 校验，不能按 ID 全局去重。
        const SELECT: &str = "SELECT DISTINCT r.id,r.generation_node_id
            FROM observation_events e
            JOIN inference_run_observations r ON r.id=e.run_id
            JOIN interaction_observations i ON i.id=r.interaction_id
            JOIN observation_tool_results tr ON tr.sequence=e.sequence
            WHERE e.kind='client_tool_result' AND i.principal=";
        Ok(match self {
            Self::Sqlite(pool, _) => {
                let mut builder = sqlx::QueryBuilder::new(SELECT);
                builder.push_bind(principal);
                builder.push(" AND tr.tool_id IN (");
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
                builder.push(" AND tr.tool_id IN (");
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
            Self::Sqlite(pool, _) => sqlx::query_scalar(
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
            Self::Sqlite(pool, _) => sqlx::query_scalar(
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
    pub(super) async fn admit(&self, admission: Admission<'_>) -> anyhow::Result<Admitted> {
        match self {
            Self::Sqlite(pool, _) => {
                // 先取得写锁，避免读取父状态后升级事务因并发写入而丢失子 Interaction。
                let mut connection = pool.acquire_write().await?;
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
                // 准入事件与随附事件一次预占连续序号。
                let first = next_sqlite_batch(&mut tx, 1 + admission.follow_ups.len()).await?;
                let sequence = first;
                let follow_up_sequences: Vec<i64> =
                    (first + 1..=first + i64::try_from(admission.follow_ups.len())?).collect();
                sqlx::query("INSERT OR IGNORE INTO interaction_observations (id,principal,api_key_id,api_key_name,generation_root_id,parent_interaction_id,root_id,root_run_id,first_route_id,first_model_display_name,status,started_at,last_active_at,last_event_sequence) VALUES (?,?,?,?,?,?,?,?,?,?,'running',?,?,?)")
                    .bind(admission.interaction_id).bind(&admission.start.principal).bind(&admission.start.api_key_id).bind(&admission.start.api_key_name)
                    .bind(admission.generation_root_id).bind(admission.parent_interaction_id)
                    .bind(admission.generation_root_id.unwrap_or(admission.interaction_id)).bind(&admission.start.id)
                    .bind(&admission.start.route_id).bind(&admission.start.model_display_name).bind(admission.now).bind(admission.now).bind(sequence).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET status='running',last_active_at=MAX(last_active_at,?),last_event_sequence=? WHERE id=?").bind(admission.now).bind(sequence).bind(admission.interaction_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,generation_parent_id,ingress_protocol,route_id,model_display_name,status,debug_enabled,started_at,last_active_at,last_event_sequence,request_model,inferred_retry,grouping_reason,diagnostic_source_run_id,has_new_user) VALUES (?,?,?,?,?,?,?,'running',?,?,?,?,?,?,?,?,?)")
                    .bind(&admission.start.id).bind(admission.interaction_id).bind(admission.parent_run_id).bind(admission.generation_parent_id)
                    .bind(&admission.start.ingress_protocol).bind(&admission.start.route_id).bind(&admission.start.model_display_name)
                    .bind(admission.debug_enabled).bind(admission.ingress_received_at).bind(admission.now).bind(sequence)
                    .bind(admission.metadata.and_then(|metadata| metadata.model.as_deref()))
                    .bind(admission.inferred_retry).bind(admission.grouping_reason)
                    .bind(admission.diagnostic_source_run_id).bind(admission.has_new_user)
                    .execute(&mut *tx).await?;
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence,
                        occurred_at: admission.now,
                        interaction_id: Some(admission.interaction_id),
                        run_id: Some(&admission.start.id),
                        rejection_id: None,
                        kind: "run_admitted",
                        payload: &Value::Null,
                    },
                )
                .await?;
                let mut follow_ups = Vec::new();
                let mut follow_up_failure = None;
                if !admission.follow_ups.is_empty() {
                    // 随附事件是可选诊断：失败时只回滚到保存点，准入照常提交。
                    let mut savepoint = tx.begin().await?;
                    match write_follow_ups_sqlite(&mut savepoint, &admission, &follow_up_sequences)
                        .await
                    {
                        Ok(events) => {
                            savepoint.commit().await?;
                            follow_ups = events;
                        }
                        Err(error) => {
                            savepoint.rollback().await?;
                            follow_up_failure = Some(error);
                        }
                    }
                }
                tx.commit().await?;
                connection.finish().await?;
                Ok(Admitted {
                    event: event(
                        sequence,
                        admission.now,
                        Some(admission.interaction_id),
                        Some(&admission.start.id),
                        None,
                        "run_admitted",
                        Value::Null,
                    ),
                    follow_ups,
                    follow_up_failure,
                })
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
                // 准入事件与随附事件一次取回实际序列值：并发写入下 nextval 跨批不连续。
                let mut sequences: Vec<i64> = sqlx::query_scalar(
                    "SELECT nextval('observation_event_sequence') FROM generate_series(1,$1)",
                )
                .bind(checked_event_count(1 + admission.follow_ups.len())?)
                .fetch_all(&mut *tx)
                .await?;
                let sequence = sequences.remove(0);
                let follow_up_sequences = sequences;
                sqlx::query("INSERT INTO interaction_observations (id,principal,api_key_id,api_key_name,generation_root_id,parent_interaction_id,root_id,root_run_id,first_route_id,first_model_display_name,status,started_at,last_active_at,last_event_sequence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'running',$11,$12,$13) ON CONFLICT (id) DO NOTHING")
                    .bind(admission.interaction_id).bind(&admission.start.principal).bind(&admission.start.api_key_id).bind(&admission.start.api_key_name)
                    .bind(admission.generation_root_id).bind(admission.parent_interaction_id)
                    .bind(admission.generation_root_id.unwrap_or(admission.interaction_id)).bind(&admission.start.id)
                    .bind(&admission.start.route_id).bind(&admission.start.model_display_name).bind(admission.now).bind(admission.now).bind(sequence).execute(&mut *tx).await?;
                sqlx::query("UPDATE interaction_observations SET status='running',last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2 WHERE id=$3").bind(admission.now).bind(sequence).bind(admission.interaction_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,generation_parent_id,ingress_protocol,route_id,model_display_name,status,debug_enabled,started_at,last_active_at,last_event_sequence,request_model,inferred_retry,grouping_reason,diagnostic_source_run_id,has_new_user) VALUES ($1,$2,$3,$4,$5,$6,$7,'running',$8,$9,$10,$11,$12,$13,$14,$15,$16)")
                    .bind(&admission.start.id).bind(admission.interaction_id).bind(admission.parent_run_id).bind(admission.generation_parent_id)
                    .bind(&admission.start.ingress_protocol).bind(&admission.start.route_id).bind(&admission.start.model_display_name)
                    .bind(admission.debug_enabled).bind(admission.ingress_received_at).bind(admission.now).bind(sequence)
                    .bind(admission.metadata.and_then(|metadata| metadata.model.as_deref()))
                    .bind(admission.inferred_retry).bind(admission.grouping_reason)
                    .bind(admission.diagnostic_source_run_id).bind(admission.has_new_user)
                    .execute(&mut *tx).await?;
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence,
                        occurred_at: admission.now,
                        interaction_id: Some(admission.interaction_id),
                        run_id: Some(&admission.start.id),
                        rejection_id: None,
                        kind: "run_admitted",
                        payload: &Value::Null,
                    },
                )
                .await?;
                let mut follow_ups = Vec::new();
                let mut follow_up_failure = None;
                if !admission.follow_ups.is_empty() {
                    // 随附事件是可选诊断：失败时只回滚到保存点，准入照常提交。
                    let mut savepoint = tx.begin().await?;
                    match write_follow_ups_postgres(
                        &mut savepoint,
                        &admission,
                        &follow_up_sequences,
                    )
                    .await
                    {
                        Ok(events) => {
                            savepoint.commit().await?;
                            follow_ups = events;
                        }
                        Err(error) => {
                            savepoint.rollback().await?;
                            follow_up_failure = Some(error);
                        }
                    }
                }
                tx.commit().await?;
                Ok(Admitted {
                    event: event(
                        sequence,
                        admission.now,
                        Some(admission.interaction_id),
                        Some(&admission.start.id),
                        None,
                        "run_admitted",
                        Value::Null,
                    ),
                    follow_ups,
                    follow_up_failure,
                })
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
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let affected =
                    sqlx::query("UPDATE interaction_observations SET observation_gap=1 WHERE id=?")
                        .bind(interaction_id)
                        .execute(&mut *connection)
                        .await?
                        .rows_affected();
                connection.finish().await?;
                affected
            }
            Self::Postgres(pool, _) => {
                sqlx::query("UPDATE interaction_observations SET observation_gap=TRUE WHERE id=$1")
                    .bind(interaction_id)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
        };
        if affected == 0 {
            return Ok(false);
        }
        // 缺口先提交，避免统计写故障抹掉故障本身；投影失败仍向上传播，清理屏障不能放行。
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin().await?;
                crate::storage::sql::recovery::refresh_sqlite(&mut tx, interaction_id).await?;
                tx.commit().await?;
                connection.finish().await?;
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                crate::storage::sql::recovery::refresh_postgres(&mut tx, interaction_id).await?;
                tx.commit().await?;
            }
        }
        Ok(true)
    }

    /// 首轮写入：Turn 开始行合并首个 attempt 开始，并携带用户输入与客户端工具结果。
    /// 四类事实来自同一源批次，必须一次事务落库；任何一步失败都不能留下半轮观测。
    /// Run 行不存在（准入未能落库）时返回 `None`：没有父行就没有可挂靠的子事实。
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.writer.persist_turn_start",
        skip_all
    )]
    pub(super) async fn persist_turn_start(
        &self,
        interaction_id: &str,
        run_id: &str,
        start: TurnStart<'_>,
    ) -> anyhow::Result<Option<Vec<ObservationEvent>>> {
        let prepared = PreparedTurnStart::new(&start)?;
        let count = prepared.durable_count;
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                let owner: Option<(String, String)> = sqlx::query_as(
                    "SELECT i.principal,i.root_run_id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id=? AND i.id=?",
                )
                .bind(run_id)
                .bind(interaction_id)
                .fetch_optional(&mut *tx)
                .await?;
                let Some((principal, root_run_id)) = owner else {
                    tx.rollback().await?;
                    connection.finish().await?;
                    return Ok(None);
                };
                let first = next_sqlite_batch(&mut tx, count).await?;
                let sequences: Vec<i64> = (first..first + i64::try_from(count)?).collect();
                let result = write_turn_start_sqlite(
                    &mut tx,
                    interaction_id,
                    run_id,
                    &principal,
                    root_run_id == run_id,
                    &start,
                    prepared,
                    &sequences,
                )
                .await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(Some(result))
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let owner: Option<(String, String)> = sqlx::query_as(
                    "SELECT i.principal,i.root_run_id FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id=$1 AND i.id=$2 FOR UPDATE OF r",
                )
                .bind(run_id)
                .bind(interaction_id)
                .fetch_optional(&mut *tx)
                .await?;
                let Some((principal, root_run_id)) = owner else {
                    return Ok(None);
                };
                let sequences: Vec<i64> = sqlx::query_scalar(
                    "SELECT nextval('observation_event_sequence') FROM generate_series(1,$1)",
                )
                .bind(checked_event_count(count)?)
                .fetch_all(&mut *tx)
                .await?;
                let result = write_turn_start_postgres(
                    &mut tx,
                    interaction_id,
                    run_id,
                    &principal,
                    root_run_id == run_id,
                    &start,
                    prepared,
                    &sequences,
                )
                .await?;
                tx.commit().await?;
                Ok(Some(result))
            }
        }
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
        _now: i64,
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
        // 同一工具在血缘里只取最新一行结果；handoff 行用于确认这些结果确实被要求过。
        // (tool_id, kind, is_error, content_id, 是否有 handoff)
        type LineageResultRow = (String, String, Option<bool>, Option<i64>, bool);
        let rows: Vec<LineageResultRow> = match self {
            Self::Sqlite(pool, _) => sqlx::query_as(
                "WITH RECURSIVE lineage(id,parent_run_id,principal) AS (
                    SELECT r.id,r.parent_run_id,i.principal
                    FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                    WHERE r.id=?1 AND r.interaction_id=?2
                    UNION
                    SELECT r.id,r.parent_run_id,i.principal
                    FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                    JOIN lineage child ON r.id=child.parent_run_id AND i.principal=child.principal
                ), ranked AS (
                    SELECT COALESCE(tr.tool_id, e.tool_id) AS tool_id, e.kind, tr.is_error, tr.content_id,
                        ROW_NUMBER() OVER (PARTITION BY COALESCE(tr.tool_id, e.tool_id) ORDER BY e.sequence DESC, tr.ordinal DESC) AS position,
                        MAX(CASE WHEN e.kind='client_tool_handoff' THEN e.sequence END)
                            OVER (PARTITION BY COALESCE(tr.tool_id, e.tool_id)) AS handoff_sequence
                    FROM lineage l
                    JOIN observation_events e ON e.run_id=l.id
                    LEFT JOIN observation_tool_results tr ON tr.sequence=e.sequence
                    WHERE e.kind IN ('client_tool_handoff','client_tool_result')
                        AND COALESCE(tr.tool_id, e.tool_id) IN (SELECT value FROM json_each(?3))
                )
                SELECT tool_id,kind,is_error,content_id,handoff_sequence IS NOT NULL FROM ranked WHERE position=1",
            )
                .bind(run_id).bind(interaction_id).bind(&ids).fetch_all(pool).await?,
            Self::Postgres(pool, _) => sqlx::query_as(
                "WITH RECURSIVE lineage(id,parent_run_id,principal) AS (
                    SELECT r.id,r.parent_run_id,i.principal
                    FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                    WHERE r.id=$1 AND r.interaction_id=$2
                    UNION
                    SELECT r.id,r.parent_run_id,i.principal
                    FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
                    JOIN lineage child ON r.id=child.parent_run_id AND i.principal=child.principal
                ), ranked AS (
                    SELECT COALESCE(tr.tool_id, e.tool_id) AS tool_id, e.kind, tr.is_error, tr.content_id,
                        ROW_NUMBER() OVER (PARTITION BY COALESCE(tr.tool_id, e.tool_id) ORDER BY e.sequence DESC, tr.ordinal DESC) AS position,
                        MAX(CASE WHEN e.kind='client_tool_handoff' THEN e.sequence END)
                            OVER (PARTITION BY COALESCE(tr.tool_id, e.tool_id)) AS handoff_sequence
                    FROM lineage l
                    JOIN observation_events e ON e.run_id=l.id
                    LEFT JOIN observation_tool_results tr ON tr.sequence=e.sequence
                    WHERE e.kind IN ('client_tool_handoff','client_tool_result')
                        AND COALESCE(tr.tool_id, e.tool_id) IN (SELECT jsonb_array_elements_text($3::jsonb))
                )
                SELECT tool_id,kind,is_error,content_id,handoff_sequence IS NOT NULL FROM ranked WHERE position=1",
            )
                .bind(run_id).bind(interaction_id).bind(&ids).fetch_all(pool).await?,
        };
        enum PreviousResult {
            Stored(Value, bool),
            Batch(usize),
        }
        let mut latest = std::collections::HashMap::new();
        let mut known_calls = std::collections::HashSet::new();
        let mut stored = Vec::new();
        // 每个 tool_id 只看最近一条（调用或结果）：最近的是调用，说明同 ID 重新发起，
        // 先前的结果不再用于去重。
        for (tool_id, kind, is_error, content_id, has_handoff) in rows {
            if has_handoff {
                known_calls.insert(tool_id.clone());
            }
            if kind == "client_tool_result"
                && let (Some(is_error), Some(content_id)) = (is_error, content_id)
            {
                stored.push((tool_id, is_error, content_id));
            }
        }
        // 祖先链只含同 Principal 的 Run，正文都在该 Principal 的 scope 内。
        let principals = self
            .interaction_principals(&[interaction_id.to_owned()])
            .await?;
        let bodies = match principals.get(interaction_id) {
            Some(principal) => {
                super::content_refs::read_scoped(
                    self,
                    principal,
                    &stored.iter().map(|(_, _, id)| *id).collect::<Vec<_>>(),
                )
                .await?
            }
            None => crate::content_store::Bodies::from_rows(Vec::new()),
        };
        for (tool_id, is_error, content_id) in stored {
            let Some(body) = bodies.get(content_id) else {
                continue;
            };
            let content = body.value()?;
            // 空正文的结果只是占位，不作为去重基准。
            if content.is_null() {
                continue;
            }
            latest.insert(tool_id, PreviousResult::Stored(content, is_error));
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
    ) -> anyhow::Result<Option<ObservationEvent>> {
        Ok(self
            .persist_run_events(interaction_id, run_id, &[(run_event.clone(), None, now)])
            .await?
            .pop())
    }
    #[tracing::instrument(target = "stravia::perf", name = "observation.writer.persist_events", skip_all, fields(event_count = events.len()))]
    pub(super) async fn persist_run_events(
        &self,
        interaction_id: &str,
        run_id: &str,
        events: &[(RunEvent, Option<String>, i64)],
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        self.persist_ordered_run_events(interaction_id, run_id, events, &[])
            .await
    }

    /// 一次收到的客户端工具结果合成一行事件，逐项结果与正文落到子表。
    pub(super) async fn persist_tool_results(
        &self,
        interaction_id: &str,
        run_id: &str,
        events: &[(RunEvent, i64)],
    ) -> anyhow::Result<Option<ObservationEvent>> {
        if events.is_empty() {
            return Ok(None);
        }
        let mut tools = PreparedTools::new(events)?;
        let sequence = match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                let mut writes = EventWrites::new(interaction_id, run_id);
                let principal: String =
                    sqlx::query_scalar("SELECT principal FROM interaction_observations WHERE id=?")
                        .bind(interaction_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let sequence = next_sqlite(&mut tx).await?;
                // 同批工具结果一次解析所有正文，避免逐条往返。
                let content_ids =
                    super::content_refs::intern_sqlite(&mut tx, &principal, &mut tools.units)
                        .await?;
                writes.push_event(sequence, tools.first_at, "client_tool_result", &Value::Null)?;
                writes.push_tool_results(sequence, &tools.tools, &content_ids)?;
                writes.flush_sqlite(&mut tx).await?;
                sqlx::query("UPDATE inference_run_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=? WHERE id=?").bind(tools.latest_at).bind(sequence).bind(run_id).execute(&mut *tx).await?;
                recompute_status_sqlite(&mut tx, interaction_id, sequence).await?;
                tx.commit().await?;
                connection.finish().await?;
                sequence
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let mut writes = EventWrites::new(interaction_id, run_id);
                let principal: String = sqlx::query_scalar(
                    "SELECT principal FROM interaction_observations WHERE id=$1",
                )
                .bind(interaction_id)
                .fetch_one(&mut *tx)
                .await?;
                let sequence: i64 =
                    sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                        .fetch_one(&mut *tx)
                        .await?;
                let content_ids =
                    super::content_refs::intern_postgres(&mut tx, &principal, &mut tools.units)
                        .await?;
                writes.push_event(sequence, tools.first_at, "client_tool_result", &Value::Null)?;
                writes.push_tool_results(sequence, &tools.tools, &content_ids)?;
                writes.flush_postgres(&mut tx).await?;
                sqlx::query("UPDATE inference_run_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2 WHERE id=$3").bind(tools.latest_at).bind(sequence).bind(run_id).execute(&mut *tx).await?;
                recompute_status_postgres(&mut tx, interaction_id, sequence).await?;
                tx.commit().await?;
                sequence
            }
        };
        Ok(Some(event(
            sequence,
            tools.first_at,
            Some(interaction_id),
            Some(run_id),
            None,
            "client_tool_result",
            Value::Null,
        )))
    }

    /// 轮次结算：最后一个 attempt 的结束与用量并入 Turn 结束行，同一事务只写一行事件。
    /// 合入属性与 `upgrade_normalize` 产出的存量布局一致。
    pub(super) async fn persist_turn_finish(
        &self,
        interaction_id: &str,
        run_id: &str,
        turn: &RunEvent,
        at: i64,
        attempt: Option<(&RunEvent, i64)>,
        usage: Option<&ConfirmedUsage>,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let mut attrs = attrs_without_kind(turn)?;
        // 用量按时间顺序逐字段合并：先前已持久的确认 ← 结束事件自带 ← 缓冲期间的后续修订。
        // 合并结果同时写入 attempt 行与合并事件的 attrs，二者字段一致。
        let mut merged_usage: Option<ConfirmedUsage> = None;
        if let Some((
            RunEvent::TargetAttemptFinished {
                attempt_id,
                usage: own,
                ..
            },
            _,
        )) = attempt
        {
            if let Some(durable) = self.attempt_usage(attempt_id).await? {
                coalesce_usage(&mut merged_usage, &durable);
            }
            if let Some(own) = own {
                coalesce_usage(&mut merged_usage, own);
            }
        }
        if let Some(later) = usage {
            coalesce_usage(&mut merged_usage, later);
        }
        let usage = merged_usage.as_ref();
        if let Some((attempt, attempt_at)) = attempt {
            let mut attempt_attrs = attrs_without_kind(attempt)?;
            if let Some(usage) = usage {
                attempt_attrs.insert("usage".into(), serde_json::to_value(usage)?);
            }
            if let Some(usage) = attempt_attrs.get("usage") {
                attrs.insert("usage".into(), usage.clone());
            }
            attrs.insert("attempt".into(), Value::Object(attempt_attrs));
            attrs.insert("attempt_at".into(), Value::from(attempt_at));
        }
        let payload = Value::Object(attrs);
        let seq = match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                let seq = next_sqlite(&mut tx).await?;
                apply_sqlite(&mut tx, interaction_id, run_id, turn, seq, at, None).await?;
                if let Some((attempt, attempt_at)) = attempt {
                    apply_sqlite(
                        &mut tx,
                        interaction_id,
                        run_id,
                        attempt,
                        seq,
                        attempt_at,
                        usage,
                    )
                    .await?;
                }
                insert_event_sqlite(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "model_turn_finished",
                        payload: &payload,
                    },
                )
                .await?;
                sqlx::query("UPDATE inference_run_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=? WHERE id=?").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                recompute_status_sqlite(&mut tx, interaction_id, seq).await?;
                tx.commit().await?;
                connection.finish().await?;
                seq
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                apply_postgres(&mut tx, interaction_id, run_id, turn, seq, at, None).await?;
                if let Some((attempt, attempt_at)) = attempt {
                    apply_postgres(
                        &mut tx,
                        interaction_id,
                        run_id,
                        attempt,
                        seq,
                        attempt_at,
                        usage,
                    )
                    .await?;
                }
                insert_event_postgres(
                    &mut tx,
                    EventInsert {
                        sequence: seq,
                        occurred_at: at,
                        interaction_id: Some(interaction_id),
                        run_id: Some(run_id),
                        rejection_id: None,
                        kind: "model_turn_finished",
                        payload: &payload,
                    },
                )
                .await?;
                sqlx::query("UPDATE inference_run_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2 WHERE id=$3").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                recompute_status_postgres(&mut tx, interaction_id, seq).await?;
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
            "model_turn_finished",
            payload,
        )))
    }

    /// 轮次开始前的用户输入登记；正文按内容单元寻址，只有根 Run 的输入成为中断恢复输入。
    pub(super) async fn persist_user_input(
        &self,
        interaction_id: &str,
        run_id: &str,
        units: &[String],
        at: i64,
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let mut units = split_user_input(units);
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                let mut writes = EventWrites::new(interaction_id, run_id);
                let (principal, is_root): (String, bool) = sqlx::query_as("SELECT i.principal,i.root_run_id=? FROM interaction_observations i WHERE i.id=?")
                    .bind(run_id)
                    .bind(interaction_id)
                    .fetch_one(&mut *tx)
                    .await?;
                let seq = next_sqlite(&mut tx).await?;
                let content_ids =
                    super::content_refs::intern_sqlite(&mut tx, &principal, &mut units).await?;
                writes.push_event(seq, at, "user_input_recorded", &Value::Null)?;
                writes.push_refs(seq, &content_ids)?;
                // Run 先推进活动时间，Interaction 的状态重算随后一次写入状态、活动、序号与输入序号。
                writes.flush_sqlite(&mut tx).await?;
                sqlx::query("UPDATE inference_run_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=? WHERE id=?").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                refresh_interaction_sqlite(&mut tx, interaction_id, seq, is_root.then_some(seq))
                    .await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(Some(event(
                    seq,
                    at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "user_input_recorded",
                    Value::Null,
                )))
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let mut writes = EventWrites::new(interaction_id, run_id);
                let (principal, is_root): (String, bool) = sqlx::query_as(
                    "SELECT i.principal,i.root_run_id=$1 FROM interaction_observations i WHERE i.id=$2",
                )
                .bind(run_id)
                .bind(interaction_id)
                .fetch_one(&mut *tx)
                .await?;
                let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
                    .fetch_one(&mut *tx)
                    .await?;
                let content_ids =
                    super::content_refs::intern_postgres(&mut tx, &principal, &mut units).await?;
                writes.push_event(seq, at, "user_input_recorded", &Value::Null)?;
                writes.push_refs(seq, &content_ids)?;
                writes.flush_postgres(&mut tx).await?;
                sqlx::query("UPDATE inference_run_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2 WHERE id=$3").bind(at).bind(seq).bind(run_id).execute(&mut *tx).await?;
                refresh_interaction_postgres(&mut tx, interaction_id, seq, is_root.then_some(seq))
                    .await?;
                tx.commit().await?;
                Ok(Some(event(
                    seq,
                    at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "user_input_recorded",
                    Value::Null,
                )))
            }
        }
    }

    pub(super) async fn persist_ordered_run_events(
        &self,
        interaction_id: &str,
        run_id: &str,
        events: &[(RunEvent, Option<String>, i64)],
        received: &[(i64, u64)],
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        #[cfg(feature = "test-support")]
        crate::storage_test_faults::inject_io("observation")?;
        #[cfg(feature = "test-support")]
        match self {
            Self::Sqlite(_, _) => crate::storage_test_faults::inject_sqlite("observation").await?,
            Self::Postgres(pool, _) => {
                crate::storage_test_faults::inject_postgres(pool, "observation").await?;
            }
        }
        let batch = self.prepare_ordered_events(events, received).await?;
        if batch.events.is_empty() {
            return Ok(Vec::new());
        }
        let count = batch.durable_count;
        match self {
            Self::Sqlite(pool, _) => {
                // 先取得写锁再读投影游标：延迟的 WAL 读事务无法在并发的准入/存储提交之后升级。
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                // 过滤后才按批预占：被跳过的 Wire/空 credential 不占号。
                let current_sequence: i64 = sqlx::query_scalar(
                    "SELECT last_event_sequence FROM inference_run_observations WHERE id=?",
                )
                .bind(run_id)
                .fetch_one(&mut *tx)
                .await?;
                let sequences: Vec<i64> = if count > 0 {
                    let first = next_sqlite_batch(&mut tx, count).await?;
                    (first..first + i64::try_from(count)?).collect()
                } else {
                    Vec::new()
                };
                let principal: String =
                    sqlx::query_scalar("SELECT principal FROM interaction_observations WHERE id=?")
                        .bind(interaction_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let result = write_ordered_sqlite(
                    &mut tx,
                    interaction_id,
                    run_id,
                    &principal,
                    batch,
                    &sequences,
                    current_sequence,
                )
                .await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(result)
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let current_sequence: i64 = sqlx::query_scalar(
                    "SELECT last_event_sequence FROM inference_run_observations WHERE id=$1",
                )
                .bind(run_id)
                .fetch_one(&mut *tx)
                .await?;
                // 按批取回实际序列值再配对：并发写入下 nextval 跨批不连续，不能自行累加。
                let sequences: Vec<i64> = if count > 0 {
                    sqlx::query_scalar(
                        "SELECT nextval('observation_event_sequence') FROM generate_series(1,$1)",
                    )
                    .bind(checked_event_count(count)?)
                    .fetch_all(&mut *tx)
                    .await?
                } else {
                    Vec::new()
                };
                let principal: String = sqlx::query_scalar(
                    "SELECT principal FROM interaction_observations WHERE id=$1",
                )
                .bind(interaction_id)
                .fetch_one(&mut *tx)
                .await?;
                let result = write_ordered_postgres(
                    &mut tx,
                    interaction_id,
                    run_id,
                    &principal,
                    batch,
                    &sequences,
                    current_sequence,
                )
                .await?;
                tx.commit().await?;
                Ok(result)
            }
        }
    }

    /// 事务外的批准备：过滤、序列化、折叠用量、解析来源。两个后端共用同一份结果。
    async fn prepare_ordered_events<'a>(
        &self,
        events: &'a [(RunEvent, Option<String>, i64)],
        received: &[(i64, u64)],
    ) -> anyhow::Result<PreparedBatch<'a>> {
        let mut prepared = Vec::with_capacity(events.len());
        let mut tool_units = Vec::new();
        let mut absorbed_usage = std::collections::HashSet::new();
        for (index, (run_event, _block_id, now)) in events.iter().enumerate() {
            let now = *now;
            if is_empty_credential_mapping(run_event) {
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
            let kind = event_kind(&payload)?;
            let mut block = None;
            let mut tool_result = None;
            let mut tool_unit = None;
            match run_event {
                RunEvent::ModelThinking {
                    model_turn_id,
                    attempt_id,
                    text,
                    block_id,
                    item,
                    complete,
                    ..
                } => {
                    let split = super::blocks::split(Some(item), text);
                    block = Some(PreparedBlock {
                        channel: "thinking",
                        model_turn_id: Some(model_turn_id.clone()),
                        attempt_id: Some(attempt_id.clone()),
                        item_ordinal: i64::try_from(parse_item_block_id(block_id)?)?,
                        complete: *complete,
                        envelope: split.envelope,
                        units: split.units,
                    });
                    payload = Value::Null;
                }
                RunEvent::ClientVisibleContent {
                    text,
                    block_id,
                    item,
                    complete,
                    ..
                } => {
                    let split = super::blocks::split(Some(item), text);
                    block = Some(PreparedBlock {
                        channel: "visible",
                        model_turn_id: None,
                        attempt_id: None,
                        item_ordinal: i64::try_from(parse_item_block_id(block_id)?)?,
                        complete: *complete,
                        envelope: split.envelope,
                        units: split.units,
                    });
                    payload = Value::Null;
                }
                RunEvent::ClientToolResult {
                    tool_id,
                    content,
                    is_error,
                } => {
                    tool_result = Some((tool_id.clone(), *is_error));
                    tool_unit = Some(value_content(content));
                    payload = Value::Null;
                }
                _ => {}
            }
            let mut finish_usage = None;
            let mut absorbs_batch_usage = false;
            if let RunEvent::TargetAttemptFinished {
                attempt_id, usage, ..
            } = run_event
            {
                if let Some(usage) = usage {
                    // The confirmation already preceded the finish; write both
                    // facts with the same attempt UPDATE.
                    finish_usage = Some(Cow::Borrowed(usage));
                } else {
                    // 事件 attrs 里的用量与 attempt 行一致：已持久的确认 ← 同批先于本次
                    // 结束的确认 ← 随后折叠进来的确认，逐字段后者优先。
                    let mut merged_usage = self.attempt_usage(attempt_id).await?;
                    for (event, _, _) in &events[..index] {
                        if let RunEvent::UsageConfirmed {
                            attempt_id: id,
                            usage,
                            ..
                        } = event
                            && id == attempt_id
                        {
                            coalesce_usage(&mut merged_usage, usage);
                        }
                    }
                    // Fold confirmations that precede this attempt's next finish.
                    // Later confirmations keep their own revision order, and the
                    // fold coalesces fields exactly like per-event COALESCE writes.
                    let end = events[index + 1..]
                        .iter()
                        .position(|(event, _, _)| {
                            matches!(
                                event,
                                RunEvent::TargetAttemptFinished { attempt_id: id, .. }
                                    if id == attempt_id
                            )
                        })
                        .map_or(events.len(), |offset| index + 1 + offset);
                    for (offset, (event, _, _)) in events[index + 1..end].iter().enumerate() {
                        if let RunEvent::UsageConfirmed {
                            attempt_id: id,
                            usage,
                            ..
                        } = event
                            && id == attempt_id
                        {
                            merge_confirmed_usage(&mut finish_usage, usage);
                            coalesce_usage(&mut merged_usage, usage);
                            absorbed_usage.insert(index + 1 + offset);
                        }
                    }
                    if let Some(usage) = &merged_usage {
                        payload["usage"] = serde_json::to_value(usage)?;
                    }
                    absorbs_batch_usage = finish_usage.is_some();
                }
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
                    Self::Sqlite(pool, _) => sqlx::query_as("SELECT e.interaction_id,e.run_id FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.kind='compaction_operation' AND e.operation_id=? ORDER BY e.sequence LIMIT 1").bind(operation).fetch_optional(pool).await?,
                    Self::Postgres(pool, _) => sqlx::query_as("SELECT e.interaction_id,e.run_id FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.kind='compaction_operation' AND e.operation_id=$1 ORDER BY e.sequence LIMIT 1").bind(operation).fetch_optional(pool).await?,
                }
                } else {
                    None
                };
                if let Some((interaction, run)) = source {
                    payload["source_interaction_id"] = Value::String(interaction);
                    payload["source_run_id"] = Value::String(run);
                }
            }
            // 首次接收时间与接收顺序随事件落盘：块与迟到的规范收尾可能晚于其后到达的事件写入，
            // 读取方只能靠这两个值还原客户端实际看到的先后。批落库独有：Turn 开始前的前置事件
            // 与准入随附事件在到达时已按序落库，没有迟到问题，不带这两个值。
            if let Some((first_received_at, receive_order)) = received.get(index)
                && !matches!(run_event, RunEvent::UsageConfirmed { .. })
            {
                if !payload.is_object() {
                    payload = Value::Object(serde_json::Map::new());
                }
                payload["first_received_at"] = Value::from(*first_received_at);
                payload["receive_order"] = Value::from(*receive_order);
            }
            // 投影信号只更新行，不产生事件；用量确认在收尾事务里随 attempt 一起写。
            if projection_signal(run_event) {
                if matches!(run_event, RunEvent::UsageConfirmed { .. }) {
                    prepared.push(PreparedEvent {
                        source_index: index,
                        event: run_event,
                        at: now,
                        kind,
                        payload: Value::Null,
                        block,
                        tool_result,
                        finish_usage,
                        absorbs_batch_usage,
                    });
                }
                continue;
            }
            // request_failed 的错误正文只落在 run.failure_json；事件行不带 attrs。
            if matches!(run_event, RunEvent::RequestFailed { .. }) {
                payload = Value::Null;
            }
            // 工具结果正文按事件顺序排列，写入时一次登记整批。
            tool_units.extend(tool_unit);
            prepared.push(PreparedEvent {
                source_index: index,
                event: run_event,
                at: now,
                kind,
                payload,
                block,
                tool_result,
                finish_usage,
                absorbs_batch_usage,
            });
        }
        let (durable_count, latest_at) = prepared
            .iter()
            .filter(|prepared| prepared.kind != "usage_confirmed")
            .fold((0, i64::MIN), |(count, latest), prepared| {
                (count + 1, latest.max(prepared.at))
            });
        Ok(PreparedBatch {
            events: prepared,
            tool_units,
            absorbed_usage,
            durable_count,
            latest_at,
        })
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
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                let interaction: Option<String> = sqlx::query_scalar("SELECT interaction_id FROM inference_run_observations WHERE id=? AND status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id)")
                    .bind(run_id)
                    .fetch_optional(&mut *tx).await?;
                let Some(interaction) = interaction else {
                    tx.rollback().await?;
                    connection.finish().await?;
                    return Ok(None);
                };
                let evidence = client_tool_evidence_sqlite(&mut tx, &interaction).await?;
                if resolved_evidence(&evidence).contains(run_id) {
                    tx.rollback().await?;
                    connection.finish().await?;
                    return Ok(None);
                }
                let row: Option<String> = sqlx::query_scalar(
                    "UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_disconnected' WHERE id=? AND status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) RETURNING interaction_id",
                ).bind(run_id).fetch_optional(&mut *tx).await?;
                let Some(interaction) = row else {
                    tx.rollback().await?;
                    connection.finish().await?;
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
                        payload: &Value::Null,
                    },
                )
                .await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(Some(event(
                    seq,
                    now,
                    Some(&interaction),
                    Some(run_id),
                    None,
                    "run_state_changed",
                    serde_json::json!({"status": "disconnected", "superseded_by": null}),
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
                let row: Option<String> = sqlx::query_scalar(
                    "UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_disconnected' WHERE id=$1 AND status='waiting_client' AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) RETURNING interaction_id",
                ).bind(run_id).fetch_optional(&mut *tx).await?;
                let Some(interaction) = row else {
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
                        payload: &Value::Null,
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
                    serde_json::json!({"status": "disconnected", "superseded_by": null}),
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
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                let candidates: Vec<(String, String)> = sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations r WHERE r.status='waiting_client' AND r.last_active_at<=? AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id) ORDER BY r.interaction_id,r.id")
                .bind(cutoff)
                .fetch_all(&mut *tx).await?;
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
                        let changed = sqlx::query("UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_wait_expired' WHERE id=? AND status='waiting_client'").bind(&rid).execute(&mut *tx).await?;
                        if changed.rows_affected() == 0 {
                            continue;
                        }
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
                                payload: &Value::Null,
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
                            serde_json::json!({"status": "disconnected", "superseded_by": null}),
                        ));
                        last_seq = Some(seq);
                    }
                    if let Some(seq) = last_seq {
                        recompute_status_sqlite(&mut tx, &iid, seq).await?;
                    }
                }
                tx.commit().await?;
                connection.finish().await?;
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
                        let changed = sqlx::query("UPDATE inference_run_observations SET status='disconnected',terminal_reason='client_wait_expired' WHERE id=$1 AND status='waiting_client'").bind(&rid).execute(&mut *tx).await?;
                        if changed.rows_affected() == 0 {
                            continue;
                        }
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
                                payload: &Value::Null,
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
                            serde_json::json!({"status": "disconnected", "superseded_by": null}),
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
    ) -> anyhow::Result<Option<ObservationEvent>> {
        let previous: Option<i64> = match self {
            Self::Sqlite(pool, _) => sqlx::query_scalar("SELECT last_active_at FROM inference_run_observations r WHERE r.id=? AND (r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations m WHERE m.run_id=r.id AND m.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations a WHERE a.run_id=r.id AND a.status='running'))").bind(run_id).fetch_optional(pool).await?,
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
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin().await?;
                let attempts = sqlx::query("SELECT id,model_turn_id,started_at,first_token_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens,usage_recorded FROM target_attempt_observations WHERE run_id=? AND status='running' ORDER BY id")
                .bind(run_id)
                .fetch_all(&mut *tx).await?;
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
                        amounts: None,
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
                crate::storage::sql::recovery::refresh_sqlite(&mut tx, interaction_id).await?;
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
                    },
                )
                .await?;
                tx.commit().await?;
                connection.finish().await?;
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
                        amounts: None,
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
                crate::storage::sql::recovery::refresh_postgres(&mut tx, interaction_id).await?;
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

    #[cfg(test)]
    pub async fn finish_run(
        &self,
        interaction_id: &str,
        run_id: &str,
        outcome: &RunOutcome,
        now: i64,
        rejected_before_target: bool,
    ) -> anyhow::Result<ObservationEvent> {
        self.finish_run_with_completion(
            interaction_id,
            run_id,
            outcome,
            now,
            rejected_before_target,
            None,
        )
        .await
    }

    /// 收尾事务：客户端交付结算与尾源、尾源的 Generation 节点、`run_finished` 同事务提交。
    /// 交付结算在同一事务内不再单独写 `run_state_changed` 行，状态随 `run_finished` 发布。
    pub(super) async fn finish_run_with_completion(
        &self,
        interaction_id: &str,
        run_id: &str,
        outcome: &RunOutcome,
        now: i64,
        rejected_before_target: bool,
        completion: Option<ClientCompletionWrite<'_>>,
    ) -> anyhow::Result<ObservationEvent> {
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin().await?;
                let seq = next_sqlite(&mut tx).await?;
                if let Some(completion) = &completion {
                    apply_client_completion_sqlite(
                        &mut tx,
                        run_id,
                        interaction_id,
                        completion,
                        seq,
                    )
                    .await?;
                }
                // 耗时采用实际结束时刻；终态事件不能排到延迟记录的前序事件之前。
                let (interrupted, recorded_at): (bool, i64) = sqlx::query_as("UPDATE inference_run_observations SET status=CASE WHEN user_interrupted=1 AND ?1!='failed' THEN 'user_interrupted' WHEN ?1='waiting_client' AND EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) THEN 'superseded' ELSE ?1 END,terminal_reason=CASE WHEN user_interrupted=1 AND ?1!='failed' THEN 'user_interrupted' ELSE ?2 END,generation_node_id=COALESCE(?3,generation_node_id),finished_at=?4,last_active_at=MAX(last_active_at,?5),last_event_sequence=?6,rejected_before_target=MAX(rejected_before_target,?8) WHERE id=?7 RETURNING user_interrupted,last_active_at").bind(&outcome.status).bind(&outcome.terminal_reason).bind(&outcome.generation_node_id).bind(now).bind(now).bind(seq).bind(run_id).bind(rejected_before_target).fetch_one(&mut *tx).await?;
                let (committed, parent, status, reason, delivered_at): (bool, Option<String>, String, Option<String>, Option<i64>) = sqlx::query_as("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,?),client_output_committed=MAX(client_output_committed,?),terminal_reason=CASE WHEN status='superseded' THEN 'superseded' ELSE terminal_reason END WHERE id=? RETURNING client_output_committed,generation_parent_id,status,terminal_reason,delivery_completed_at")
                        .bind(outcome.delivery_completed_at)
                        .bind(outcome.client_output_committed)
                        .bind(run_id)
                        .fetch_one(&mut *tx).await?;
                let mut payload = finish_payload(outcome, interrupted)?;
                payload["finished_at"] = Value::from(now);
                payload["status"] = Value::String(status);
                payload["terminal_reason"] = serde_json::to_value(reason)?;
                payload["delivery_completed_at"] = serde_json::to_value(delivered_at)?;
                payload["client_output_committed"] = Value::Bool(committed);
                payload["generation_parent_id"] = serde_json::to_value(parent)?;
                recompute_status_sqlite(&mut tx, interaction_id, seq).await?;
                if let Some(node) = outcome.generation_node_id.as_deref() {
                    sqlx::query(
                        "UPDATE observation_tail_sources SET generation_node_id=? WHERE run_id=?",
                    )
                    .bind(node)
                    .bind(run_id)
                    .execute(&mut *tx)
                    .await?;
                }
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
                    },
                )
                .await?;
                tx.commit().await?;
                connection.finish().await?;
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
                if let Some(completion) = &completion {
                    apply_client_completion_postgres(
                        &mut tx,
                        run_id,
                        interaction_id,
                        completion,
                        seq,
                    )
                    .await?;
                }
                let (interrupted, recorded_at): (bool, i64) = sqlx::query_as("UPDATE inference_run_observations SET status=CASE WHEN user_interrupted AND $1!='failed' THEN 'user_interrupted' WHEN $1='waiting_client' AND EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) THEN 'superseded' ELSE $1 END,terminal_reason=CASE WHEN user_interrupted AND $1!='failed' THEN 'user_interrupted' ELSE $2 END,generation_node_id=COALESCE($3,generation_node_id),finished_at=$4,last_active_at=GREATEST(last_active_at,$5),last_event_sequence=$6,rejected_before_target=rejected_before_target OR $8 WHERE id=$7 RETURNING user_interrupted,last_active_at").bind(&outcome.status).bind(&outcome.terminal_reason).bind(&outcome.generation_node_id).bind(now).bind(now).bind(seq).bind(run_id).bind(rejected_before_target).fetch_one(&mut *tx).await?;
                let (committed, parent, status, reason, delivered_at): (bool, Option<String>, String, Option<String>, Option<i64>) = sqlx::query_as("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,$1),client_output_committed=client_output_committed OR $2,terminal_reason=CASE WHEN status='superseded' THEN 'superseded' ELSE terminal_reason END WHERE id=$3 RETURNING client_output_committed,generation_parent_id,status,terminal_reason,delivery_completed_at").bind(outcome.delivery_completed_at).bind(outcome.client_output_committed).bind(run_id).fetch_one(&mut *tx).await?;
                let mut payload = finish_payload(outcome, interrupted)?;
                payload["finished_at"] = Value::from(now);
                payload["status"] = Value::String(status);
                payload["terminal_reason"] = serde_json::to_value(reason)?;
                payload["delivery_completed_at"] = serde_json::to_value(delivered_at)?;
                payload["client_output_committed"] = Value::Bool(committed);
                payload["generation_parent_id"] = serde_json::to_value(parent)?;
                recompute_status_postgres(&mut tx, interaction_id, seq).await?;
                if let Some(node) = outcome.generation_node_id.as_deref() {
                    sqlx::query(
                        "UPDATE observation_tail_sources SET generation_node_id=$1 WHERE run_id=$2",
                    )
                    .bind(node)
                    .bind(run_id)
                    .execute(&mut *tx)
                    .await?;
                }
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
        #[cfg(feature = "test-support")]
        crate::storage_test_faults::inject_io("observation")?;
        #[cfg(feature = "test-support")]
        match self {
            Self::Sqlite(_, _) => crate::storage_test_faults::inject_sqlite("observation").await?,
            Self::Postgres(pool, _) => {
                crate::storage_test_faults::inject_postgres(pool, "observation").await?;
            }
        }
        let Rejection {
            ingress,
            outcome,
            metadata,
            debug_enabled: debug,
            occurred_at: now,
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
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin().await?;
                let seq = next_sqlite(&mut tx).await?;
                sqlx::query("INSERT INTO rejected_request_observations (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,started_at,duration_ms,failure_json,request_model,api_key_id,api_key_name) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
                    .bind(&ingress.id)
                    .bind(now)
                    .bind(&ingress.method)
                    .bind(&ingress.path)
                    .bind(&ingress.protocol)
                    .bind(&outcome.stage)
                    .bind(&outcome.code)
                    .bind(outcome.status_code)
                    .bind(debug)
                    .bind(seq)
                    .bind(started_at)
                    .bind(duration_ms)
                    .bind(&failure)
                    .bind(&metadata.model)
                    .bind(&metadata.api_key_id)
                    .bind(&metadata.api_key_name)
                    .execute(&mut *tx).await?;
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
                    },
                )
                .await?;
                tx.commit().await?;
                connection.finish().await?;
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
                sqlx::query("INSERT INTO rejected_request_observations (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,started_at,duration_ms,failure_json,request_model,api_key_id,api_key_name) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)")
                    .bind(&ingress.id).bind(now).bind(&ingress.method).bind(&ingress.path).bind(&ingress.protocol).bind(&outcome.stage).bind(&outcome.code).bind(i64::from(outcome.status_code)).bind(debug).bind(seq)
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
            Self::Sqlite(pool, _) => Ok(sqlx::query_as("SELECT r.interaction_id,r.id AS run_id,r.delivery_completed_at AS delivery_completed_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.generation_node_id=? AND i.principal=? LIMIT 1")
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
            Self::Sqlite(pool, _) => Ok(sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations WHERE generation_node_id=? LIMIT 1").bind(generation_node_id).fetch_optional(pool).await?),
            Self::Postgres(pool, _) => Ok(sqlx::query_as("SELECT interaction_id,id FROM inference_run_observations WHERE generation_node_id=$1 LIMIT 1").bind(generation_node_id).fetch_optional(pool).await?),
        }
    }

    pub async fn recover_after_restart(&self) -> anyhow::Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        match self {
            Self::Sqlite(pool, _) => {
                let mut connection = pool.acquire_write().await?;
                let mut tx = connection.begin().await?;
                let mut runs:Vec<(String,String,String)>=sqlx::query_as("SELECT r.interaction_id,r.id,r.status FROM inference_run_observations r WHERE r.status='running' OR r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations mt WHERE mt.run_id=r.id AND mt.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations ta WHERE ta.run_id=r.id AND ta.status='running')")
                    .fetch_all(&mut *tx).await?;
                let old_interactions: Vec<(String, i64)> = sqlx::query_as("SELECT id,last_event_sequence FROM interaction_observations WHERE status IN ('running','waiting_client')")
                    .fetch_all(&mut *tx).await?;
                let mut affected: std::collections::HashMap<String, i64> =
                    old_interactions.into_iter().collect();
                let waiting: Vec<(String, String, String)> = sqlx::query_as("SELECT r.interaction_id,r.id,r.status FROM inference_run_observations r WHERE r.status='waiting_client' AND EXISTS(SELECT 1 FROM interaction_observations i WHERE i.id=r.interaction_id AND i.status IN ('running','waiting_client')) AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id AND c.interaction_id=r.interaction_id) ORDER BY r.interaction_id,r.id")
                    .fetch_all(&mut *tx).await?;
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

                for (iid, rid, old_status) in &runs {
                    let seq = next_sqlite(&mut tx).await?;
                    let status = if old_status == "running" || unresolved.contains(rid.as_str()) {
                        "interrupted"
                    } else {
                        old_status.as_str()
                    };
                    sqlx::query("UPDATE inference_run_observations SET status=?,terminal_reason=CASE WHEN status='running' OR (status='waiting_client' AND ?='interrupted') THEN 'process_restarted' ELSE terminal_reason END,finished_at=CASE WHEN status='running' THEN COALESCE(finished_at,?) ELSE finished_at END,background_active=0,last_event_sequence=? WHERE id=?").bind(status).bind(status).bind(now).bind(seq).bind(rid).execute(&mut *tx).await?;
                    affected.insert(iid.clone(), seq);
                    insert_event_sqlite(
                        &mut tx,
                        EventInsert {
                            sequence: seq,
                            occurred_at: now,
                            interaction_id: Some(iid),
                            run_id: Some(rid),
                            rejection_id: None,
                            kind: "run_state_changed",
                            payload: &Value::Null,
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
                connection.finish().await?;
            }
            Self::Postgres(pool, _) => {
                let mut tx = pool.begin().await?;
                let mut runs:Vec<(String,String,String)>=sqlx::query_as("SELECT r.interaction_id,r.id,r.status FROM inference_run_observations r WHERE r.status='running' OR r.background_active>0 OR EXISTS(SELECT 1 FROM model_turn_observations mt WHERE mt.run_id=r.id AND mt.status='running') OR EXISTS(SELECT 1 FROM target_attempt_observations ta WHERE ta.run_id=r.id AND ta.status='running')").fetch_all(&mut *tx).await?;
                let old_interactions: Vec<(String, i64)> = sqlx::query_as("SELECT id,last_event_sequence FROM interaction_observations WHERE status IN ('running','waiting_client')").fetch_all(&mut *tx).await?;
                let mut affected: std::collections::HashMap<String, i64> =
                    old_interactions.into_iter().collect();
                let waiting: Vec<(String, String, String)> = sqlx::query_as("SELECT r.interaction_id,r.id,r.status FROM inference_run_observations r WHERE r.status='waiting_client' AND EXISTS(SELECT 1 FROM interaction_observations i WHERE i.id=r.interaction_id AND i.status IN ('running','waiting_client')) AND NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=r.id AND c.interaction_id=r.interaction_id) ORDER BY r.interaction_id,r.id").fetch_all(&mut *tx).await?;
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

                for (iid, rid, old_status) in &runs {
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
                    insert_event_postgres(
                        &mut tx,
                        EventInsert {
                            sequence: seq,
                            occurred_at: now,
                            interaction_id: Some(iid),
                            run_id: Some(rid),
                            rejection_id: None,
                            kind: "run_state_changed",
                            payload: &Value::Null,
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
        match self{Self::Sqlite(p, _)=>Ok(sqlx::query_scalar("SELECT next_sequence-1 FROM observation_sequence WHERE singleton_id=1").fetch_one(p).await?),Self::Postgres(p, _)=>Ok(sqlx::query_scalar("SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM observation_event_sequence").fetch_one(p).await?)}
    }
    pub async fn min_sequence(&self) -> anyhow::Result<Option<i64>> {
        match self {
            Self::Sqlite(p, _) => Ok(sqlx::query_scalar(
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
        let mut events = match self {
            Self::Sqlite(p, _) => load_events_sqlite(p, after).await?,
            Self::Postgres(p, _) => load_events_postgres(p, after).await?,
        };
        super::hydrate::hydrate(self, &mut events).await?;
        Ok(events)
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
    let runs:Vec<(String,String,bool)>=sqlx::query_as("SELECT id,status,NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) FROM inference_run_observations WHERE interaction_id=? AND status IN ('running','waiting_client')").bind(interaction_id).fetch_all(&mut **tx).await?;
    let mut last = None;
    for (rid, old_status, leaf) in runs {
        let seq = next_sqlite(tx).await?;
        let (status, reason) = match (old_status.as_str(), leaf) {
            ("waiting_client", true) => ("user_interrupted", "user_interrupted"),
            ("waiting_client", false) => ("superseded", "superseded"),
            _ => (old_status.as_str(), "user_interrupted"),
        };
        let interrupted = reason == "user_interrupted";
        sqlx::query("UPDATE inference_run_observations SET user_interrupted=?,status=?,terminal_reason=?,finished_at=CASE WHEN ?='user_interrupted' THEN COALESCE(finished_at,?) ELSE finished_at END,last_event_sequence=? WHERE id=?").bind(interrupted).bind(status).bind(reason).bind(status).bind(now).bind(seq).bind(&rid).execute(&mut **tx).await?;
        insert_event_sqlite(
            tx,
            EventInsert {
                sequence: seq,
                occurred_at: now,
                interaction_id: Some(interaction_id),
                run_id: Some(&rid),
                rejection_id: None,
                kind: "run_state_changed",
                payload: &Value::Null,
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
    let runs:Vec<(String,String,bool)>=sqlx::query_as("SELECT id,status,NOT EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) FROM inference_run_observations WHERE interaction_id=$1 AND status IN ('running','waiting_client')").bind(interaction_id).fetch_all(&mut **tx).await?;
    let mut last = None;
    for (rid, old_status, leaf) in runs {
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
        insert_event_postgres(
            tx,
            EventInsert {
                sequence: seq,
                occurred_at: now,
                interaction_id: Some(interaction_id),
                run_id: Some(&rid),
                rejection_id: None,
                kind: "run_state_changed",
                payload: &Value::Null,
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
    let parent: Option<String> = sqlx::query_scalar(
        "SELECT status FROM inference_run_observations WHERE id=? AND interaction_id=?",
    )
    .bind(parent_run_id)
    .bind(interaction_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(status) = parent else {
        return Ok(());
    };
    if status != "waiting_client" {
        return Ok(());
    }
    let seq = next_sqlite(tx).await?;
    let changed = sqlx::query("UPDATE inference_run_observations SET status='superseded',terminal_reason='superseded',superseded_by=?,last_event_sequence=? WHERE id=? AND status='waiting_client'")
        .bind(superseded_by).bind(seq).bind(parent_run_id).execute(&mut **tx).await?.rows_affected();
    if changed == 0 {
        return Ok(());
    }
    insert_event_sqlite(
        tx,
        EventInsert {
            sequence: seq,
            occurred_at: now,
            interaction_id: Some(interaction_id),
            run_id: Some(parent_run_id),
            rejection_id: None,
            kind: "run_state_changed",
            payload: &Value::Null,
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
    let parent: Option<String> = sqlx::query_scalar(
        "SELECT status FROM inference_run_observations WHERE id=$1 AND interaction_id=$2",
    )
    .bind(parent_run_id)
    .bind(interaction_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(status) = parent else {
        return Ok(());
    };
    if status != "waiting_client" {
        return Ok(());
    }
    let seq: i64 = sqlx::query_scalar("SELECT nextval('observation_event_sequence')")
        .fetch_one(&mut **tx)
        .await?;
    let changed = sqlx::query("UPDATE inference_run_observations SET status='superseded',terminal_reason='superseded',superseded_by=$1,last_event_sequence=$2 WHERE id=$3 AND status='waiting_client'")
        .bind(superseded_by).bind(seq).bind(parent_run_id).execute(&mut **tx).await?.rows_affected();
    if changed == 0 {
        return Ok(());
    }
    insert_event_postgres(
        tx,
        EventInsert {
            sequence: seq,
            occurred_at: now,
            interaction_id: Some(interaction_id),
            run_id: Some(parent_run_id),
            rejection_id: None,
            kind: "run_state_changed",
            payload: &Value::Null,
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
    /// kind 独有的小属性；无属性的 kind 传 `Value::Null`，落库为 NULL。
    payload: &'a Value,
}

/// 事件的工具关联列从 attrs 派生，查询侧不再解析正文。
pub(super) fn event_tool_id(attrs: Option<&Value>) -> Option<&str> {
    attrs
        .and_then(|attrs| attrs.get("tool_id"))
        .and_then(Value::as_str)
}

pub(super) fn event_operation_id(attrs: Option<&Value>) -> Option<&str> {
    attrs
        .and_then(|attrs| attrs.get("operation_id"))
        .and_then(Value::as_str)
}

/// 只保留 kind 独有属性；`kind` 由列单独保存，读取时再拼回事件视图。
pub(super) fn encode_attrs(payload: &Value) -> anyhow::Result<Option<Vec<u8>>> {
    let Value::Object(mut object) = payload.clone() else {
        return Ok(None);
    };
    object.remove("kind");
    Ok(Some(crate::storage_codec::encode(
        serde_json::to_vec(&Value::Object(object))?.into(),
    )?))
}

pub(super) fn attrs_value(payload: &Value) -> Option<&Value> {
    match payload {
        Value::Null => None,
        other => Some(other),
    }
}

/// 事件去掉 `kind` 后的独有属性；合并行（如 Turn 开始）用它拼装 attrs。
fn attrs_without_kind(event: &RunEvent) -> anyhow::Result<serde_json::Map<String, Value>> {
    let Value::Object(mut object) = serde_json::to_value(event)? else {
        return Ok(serde_json::Map::new());
    };
    object.remove("kind");
    Ok(object)
}

/// 用户 item 的文本正文；空字符串同样寻址。
fn split_user_input(units: &[String]) -> Vec<Content> {
    units.iter().map(text_content).collect()
}

async fn insert_event_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: EventInsert<'_>,
) -> anyhow::Result<()> {
    let attrs = attrs_value(event.payload);
    sqlx::query("INSERT INTO observation_events (sequence,occurred_at,interaction_id,run_id,rejection_id,kind,attrs,tool_id,operation_id) VALUES (?,?,?,?,?,?,?,?,?)").bind(event.sequence).bind(event.occurred_at).bind(event.interaction_id).bind(event.run_id).bind(event.rejection_id).bind(event.kind).bind(encode_attrs(event.payload)?).bind(event_tool_id(attrs)).bind(event_operation_id(attrs)).execute(&mut **tx).await?;
    Ok(())
}
async fn insert_event_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: EventInsert<'_>,
) -> anyhow::Result<()> {
    let attrs = attrs_value(event.payload);
    sqlx::query("INSERT INTO observation_events (sequence,occurred_at,interaction_id,run_id,rejection_id,kind,attrs,tool_id,operation_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(event.sequence).bind(event.occurred_at).bind(event.interaction_id).bind(event.run_id).bind(event.rejection_id).bind(event.kind).bind(encode_attrs(event.payload)?).bind(event_tool_id(attrs)).bind(event_operation_id(attrs)).execute(&mut **tx).await?;
    Ok(())
}

/// 一个复用事件最多 50 个映射 × 50 个位置；按块写入，绑定参数数留有余量。
const REUSE_ROW_BATCH: usize = 100;

struct ReuseRow<'a> {
    marker: &'a str,
    interaction_id: &'a str,
    run_id: Option<&'a str>,
    source_type: &'a str,
    tool_id: &'a str,
    latest_input: bool,
    last_reused_at: i64,
}

/// 把复用事件展开成登记行。没有占位符的映射无法关联回创建行，不登记；
/// `tool_id` 与 `latest_input` 按去重键归一，与读取侧返回的来源字段一致。
/// 位置携带 `rule_id`，不同规则可以在同一位置命中同一秘密，归一后就会落到同一主键；
/// 一条 INSERT 不能对同一行执行两次 `ON CONFLICT`（PostgreSQL 直接报错），
/// 而同一事件的登记行共享时间与 Run，所以这里按主键先取第一条。
fn reuse_rows<'a>(
    interaction_id: &'a str,
    run_id: Option<&'a str>,
    occurred_at: i64,
    discoveries: &'a [CredentialDiscovery],
) -> Vec<ReuseRow<'a>> {
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for discovery in discoveries {
        let Some(marker) = discovery
            .marker
            .as_deref()
            .filter(|marker| !marker.is_empty())
        else {
            continue;
        };
        for locator in &discovery.locators {
            let source_type = locator.source_type.as_str();
            let tool_id = locator.tool_id.as_deref().unwrap_or_default();
            let latest_input = locator.tool_id.is_none() && locator.latest_input.unwrap_or(false);
            if !seen.insert((marker, interaction_id, source_type, tool_id, latest_input)) {
                continue;
            }
            rows.push(ReuseRow {
                marker,
                interaction_id,
                run_id,
                source_type,
                tool_id,
                latest_input,
                last_reused_at: occurred_at,
            });
        }
    }
    rows
}

/// 登记行的写入语句；两种后端只差绑定风格，冲突处理必须完全一致。
const REUSE_INSERT: &str = "INSERT INTO credential_mapping_reuses(marker,interaction_id,run_id,source_type,tool_id,latest_input,last_reused_at)";
/// 只有更晚的复用能覆盖时间与 Run：乱序到达的旧事件不得回退最新事实，时间相同时保留先写入的 Run。
const REUSE_CONFLICT: &str = " ON CONFLICT(marker,interaction_id,source_type,tool_id,latest_input) DO UPDATE SET run_id=excluded.run_id,last_reused_at=excluded.last_reused_at WHERE excluded.last_reused_at>credential_mapping_reuses.last_reused_at";

fn insert_reuse_sql(builder: &mut sqlx::QueryBuilder<sqlx::Sqlite>, rows: &[ReuseRow<'_>]) {
    builder.push(REUSE_INSERT);
    builder.push_values(rows, |mut values, row| {
        values
            .push_bind(row.marker)
            .push_bind(row.interaction_id)
            .push_bind(row.run_id)
            .push_bind(row.source_type)
            .push_bind(row.tool_id)
            .push_bind(row.latest_input)
            .push_bind(row.last_reused_at);
    });
    builder.push(REUSE_CONFLICT);
}

fn insert_reuse_sql_postgres(
    builder: &mut sqlx::QueryBuilder<sqlx::Postgres>,
    rows: &[ReuseRow<'_>],
) {
    builder.push(REUSE_INSERT);
    builder.push_values(rows, |mut values, row| {
        values
            .push_bind(row.marker)
            .push_bind(row.interaction_id)
            .push_bind(row.run_id)
            .push_bind(row.source_type)
            .push_bind(row.tool_id)
            .push_bind(row.latest_input)
            .push_bind(row.last_reused_at);
    });
    builder.push(REUSE_CONFLICT);
}

/// 与复用事件同一事务登记，任何一步失败都会连同事件一起回滚。
pub(super) async fn upsert_credential_mapping_reuses_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    interaction_id: &str,
    run_id: Option<&str>,
    occurred_at: i64,
    discoveries: &[CredentialDiscovery],
) -> anyhow::Result<()> {
    for chunk in
        reuse_rows(interaction_id, run_id, occurred_at, discoveries).chunks(REUSE_ROW_BATCH)
    {
        let mut insert = sqlx::QueryBuilder::<sqlx::Sqlite>::new("");
        insert_reuse_sql(&mut insert, chunk);
        insert.build().execute(&mut **tx).await?;
    }
    Ok(())
}

pub(super) async fn upsert_credential_mapping_reuses_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    interaction_id: &str,
    run_id: Option<&str>,
    occurred_at: i64,
    discoveries: &[CredentialDiscovery],
) -> anyhow::Result<()> {
    for chunk in
        reuse_rows(interaction_id, run_id, occurred_at, discoveries).chunks(REUSE_ROW_BATCH)
    {
        let mut insert = sqlx::QueryBuilder::<sqlx::Postgres>::new("");
        insert_reuse_sql_postgres(&mut insert, chunk);
        insert.build().execute(&mut **tx).await?;
    }
    Ok(())
}

fn cost_basis_column(basis: Option<CostBasis>) -> Option<&'static str> {
    match basis?.attempt_basis()? {
        CostBasis::Usage => Some("usage"),
        CostBasis::Subscription => Some("subscription"),
        // attempt_basis 已把不计算成本的 Provider 归为 None。
        CostBasis::Untracked => None,
    }
}

type AttemptAmountColumns = (
    Option<&'static str>,
    Option<i64>,
    Option<i64>,
    Option<&'static str>,
    Option<i64>,
    Option<i64>,
);

/// Recovery terminals preserve the start-time basis without fabricating amounts.
fn attempt_amount_columns(amounts: &AttemptAmounts) -> AttemptAmountColumns {
    // 不计算成本的 Provider 不写基准也不写金额；Route 计费与成本基准无关，照常记录。
    let basis = cost_basis_column(Some(amounts.cost_basis));
    let (cost_usd, cost_custom) = match amounts
        .cost_basis
        .attempt_basis()
        .and(amounts.cost.as_ref())
    {
        Some(amount) => (Some(amount.usd_nanos), amount.custom_nanos),
        None => (None, None),
    };
    let (billing_state, billing_usd, billing_custom) = match &amounts.billing {
        Some(BillingOutcome::Charged(amount)) => {
            (Some("charged"), Some(amount.usd_nanos), amount.custom_nanos)
        }
        Some(BillingOutcome::Unknown) => (Some("unknown"), None, None),
        None => (None, None, None),
    };
    (
        basis,
        cost_usd,
        cost_custom,
        billing_state,
        billing_usd,
        billing_custom,
    )
}

/// 把较新的用量修订并入已落盘的 attempt 结束事实：独立的 `target_attempt_finished` 行，
/// 或合并进 `model_turn_finished` 的 attempt 属性。找不到（结束尚未落盘）则无事可做。
fn merge_usage_into_finish_attrs(
    kind: &str,
    attrs: &mut Value,
    attempt_id: &str,
    revision: &ConfirmedUsage,
) -> anyhow::Result<bool> {
    let target = match kind {
        "target_attempt_finished" => Some(&mut *attrs),
        "model_turn_finished" => attrs.get_mut("attempt"),
        _ => None,
    };
    let Some(target) = target else {
        return Ok(false);
    };
    if target["attempt_id"].as_str() != Some(attempt_id) {
        return Ok(false);
    }
    let mut merged = match target.get("usage") {
        Some(existing) if !existing.is_null() => {
            Some(serde_json::from_value::<ConfirmedUsage>(existing.clone())?)
        }
        _ => None,
    };
    coalesce_usage(&mut merged, revision);
    let usage = serde_json::to_value(&merged)?;
    target["usage"] = usage.clone();
    if kind == "model_turn_finished" {
        attrs["usage"] = usage;
    }
    Ok(true)
}

async fn refresh_finished_usage_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    attempt_id: &str,
    revision: &ConfirmedUsage,
) -> anyhow::Result<()> {
    let rows: Vec<(i64, String, Vec<u8>)> = sqlx::query_as("SELECT sequence,kind,attrs FROM observation_events WHERE run_id=? AND kind IN ('target_attempt_finished','model_turn_finished') AND attrs IS NOT NULL ORDER BY sequence DESC")
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await?;
    for (sequence, kind, bytes) in rows {
        let mut attrs: Value = serde_json::from_slice(&crate::storage_codec::decode(&bytes)?)?;
        if merge_usage_into_finish_attrs(&kind, &mut attrs, attempt_id, revision)? {
            sqlx::query("UPDATE observation_events SET attrs=? WHERE sequence=?")
                .bind(encode_attrs(&attrs)?)
                .bind(sequence)
                .execute(&mut **tx)
                .await?;
            break;
        }
    }
    Ok(())
}

async fn refresh_finished_usage_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    attempt_id: &str,
    revision: &ConfirmedUsage,
) -> anyhow::Result<()> {
    let rows: Vec<(i64, String, Vec<u8>)> = sqlx::query_as("SELECT sequence,kind,attrs FROM observation_events WHERE run_id=$1 AND kind IN ('target_attempt_finished','model_turn_finished') AND attrs IS NOT NULL ORDER BY sequence DESC")
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await?;
    for (sequence, kind, bytes) in rows {
        let mut attrs: Value = serde_json::from_slice(&crate::storage_codec::decode(&bytes)?)?;
        if merge_usage_into_finish_attrs(&kind, &mut attrs, attempt_id, revision)? {
            sqlx::query("UPDATE observation_events SET attrs=$1 WHERE sequence=$2")
                .bind(encode_attrs(&attrs)?)
                .bind(sequence)
                .execute(&mut **tx)
                .await?;
            break;
        }
    }
    Ok(())
}

/// 把较新的用量修订并入累计值：已知字段覆盖旧值，未知字段保留旧值。
pub(super) fn coalesce_usage(into: &mut Option<ConfirmedUsage>, newer: &ConfirmedUsage) {
    match into {
        Some(current) => {
            current.input_tokens = newer.input_tokens.or(current.input_tokens);
            current.output_tokens = newer.output_tokens.or(current.output_tokens);
            current.cache_read_tokens = newer.cache_read_tokens.or(current.cache_read_tokens);
            current.cache_write_tokens = newer.cache_write_tokens.or(current.cache_write_tokens);
            current.reasoning_tokens = newer.reasoning_tokens.or(current.reasoning_tokens);
            if newer.coverage.is_some() {
                current.coverage = newer.coverage.clone();
            }
        }
        None => *into = Some(newer.clone()),
    }
}

/// 批内合并与逐事件 `COALESCE` 同义：后到的已知字段覆盖先前值，未知字段保留先前已知值。
fn merge_confirmed_usage<'a>(
    into: &mut Option<Cow<'a, ConfirmedUsage>>,
    usage: &'a ConfirmedUsage,
) {
    match into {
        Some(current) => {
            let current = current.to_mut();
            current.input_tokens = usage.input_tokens.or(current.input_tokens);
            current.output_tokens = usage.output_tokens.or(current.output_tokens);
            current.cache_read_tokens = usage.cache_read_tokens.or(current.cache_read_tokens);
            current.cache_write_tokens = usage.cache_write_tokens.or(current.cache_write_tokens);
            current.reasoning_tokens = usage.reasoning_tokens.or(current.reasoning_tokens);
        }
        None => *into = Some(Cow::Borrowed(usage)),
    }
}

async fn apply_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
    rid: &str,
    e: &RunEvent,
    seq: i64,
    now: i64,
    finish_usage: Option<&ConfirmedUsage>,
) -> anyhow::Result<bool> {
    let mut wrote_usage = false;
    match e {
        RunEvent::RequestFailed { error } => {
            sqlx::query("UPDATE inference_run_observations SET failure_json=? WHERE id=?")
                .bind(serde_json::to_string(error)?)
                .bind(rid)
                .execute(&mut **tx)
                .await?;
        }
        RunEvent::ModelTurnStarted {
            retention_detached,
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
            if !retention_detached {
                sqlx::query("UPDATE inference_run_observations SET background_active=background_active+1 WHERE id=?").bind(rid).execute(&mut **tx).await?;
            }
        }
        RunEvent::ModelTurnFinished {
            retention_detached,
            model_turn_id,
            status,
        } => {
            sqlx::query("UPDATE model_turn_observations SET status=?,finished_at=?,last_event_sequence=? WHERE id=?").bind(status).bind(now).bind(seq).bind(model_turn_id).execute(&mut **tx).await?;
            if !retention_detached {
                sqlx::query("UPDATE inference_run_observations SET background_active=MAX(background_active-1,0) WHERE id=?").bind(rid).execute(&mut **tx).await?;
            }
        }
        RunEvent::TargetAttemptStarted {
            model_turn_id,
            attempt_id,
            target_id,
            provider_id,
            provider_name,
            upstream_model,
            protocol,
            cost_basis,
            ..
        } => {
            sqlx::query("INSERT OR IGNORE INTO target_attempt_observations (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,cost_basis,status,started_at,last_event_sequence) VALUES (?,?,?,?,?,?,?,?,?,?,'running',?,?)").bind(attempt_id).bind(model_turn_id).bind(rid).bind(iid).bind(target_id).bind(provider_id).bind(provider_name).bind(upstream_model).bind(protocol).bind(cost_basis_column(*cost_basis)).bind(now).bind(seq).execute(&mut **tx).await?;
        }
        RunEvent::TargetAttemptFinished {
            attempt_id,
            status,
            status_code,
            error_code,
            error,
            duration_ms,
            first_token_ms,
            usage,
            amounts,
            ..
        } => {
            let usage = finish_usage.or(usage.as_ref());
            let sql = match (amounts.is_some(), usage.is_some()) {
                (true, true) => {
                    "UPDATE target_attempt_observations SET status=?,status_code=?,error_code=?,error_source=?,error_message=?,upstream_error_code=?,finished_at=?,duration_ms=?,first_token_ms=?,cost_basis=?,cost_usd_nanos=?,cost_custom_nanos=?,billing_state=?,billing_usd_nanos=?,billing_custom_nanos=?,input_tokens=COALESCE(?,input_tokens),output_tokens=COALESCE(?,output_tokens),cache_read_tokens=COALESCE(?,cache_read_tokens),cache_write_tokens=COALESCE(?,cache_write_tokens),reasoning_tokens=COALESCE(?,reasoning_tokens),usage_recorded=1,last_event_sequence=? WHERE id=?"
                }
                (true, false) => {
                    "UPDATE target_attempt_observations SET status=?,status_code=?,error_code=?,error_source=?,error_message=?,upstream_error_code=?,finished_at=?,duration_ms=?,first_token_ms=?,cost_basis=?,cost_usd_nanos=?,cost_custom_nanos=?,billing_state=?,billing_usd_nanos=?,billing_custom_nanos=?,last_event_sequence=? WHERE id=?"
                }
                (false, true) => {
                    "UPDATE target_attempt_observations SET status=?,status_code=?,error_code=?,error_source=?,error_message=?,upstream_error_code=?,finished_at=?,duration_ms=?,first_token_ms=?,input_tokens=COALESCE(?,input_tokens),output_tokens=COALESCE(?,output_tokens),cache_read_tokens=COALESCE(?,cache_read_tokens),cache_write_tokens=COALESCE(?,cache_write_tokens),reasoning_tokens=COALESCE(?,reasoning_tokens),usage_recorded=1,last_event_sequence=? WHERE id=?"
                }
                (false, false) => {
                    "UPDATE target_attempt_observations SET status=?,status_code=?,error_code=?,error_source=?,error_message=?,upstream_error_code=?,finished_at=?,duration_ms=?,first_token_ms=?,last_event_sequence=? WHERE id=?"
                }
            };
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(status)
                .bind(status_code.map(i64::from))
                .bind(error_code)
                .bind(error.as_ref().and_then(|error| error.source.as_deref()))
                .bind(error.as_ref().and_then(|error| error.message.as_deref()))
                .bind(
                    error
                        .as_ref()
                        .and_then(|error| error.upstream_code.as_deref()),
                )
                .bind(now)
                .bind(duration_ms)
                .bind(first_token_ms);
            if let Some(amounts) = amounts {
                let (cost_basis, cost_usd, cost_custom, billing_state, billing_usd, billing_custom) =
                    attempt_amount_columns(amounts);
                query = query
                    .bind(cost_basis)
                    .bind(cost_usd)
                    .bind(cost_custom)
                    .bind(billing_state)
                    .bind(billing_usd)
                    .bind(billing_custom);
            }
            if let Some(usage) = usage {
                query = query
                    .bind(usage.input_tokens)
                    .bind(usage.output_tokens)
                    .bind(usage.cache_read_tokens)
                    .bind(usage.cache_write_tokens)
                    .bind(usage.reasoning_tokens);
            }
            let changed = query
                .bind(seq)
                .bind(attempt_id)
                .execute(&mut **tx)
                .await?
                .rows_affected()
                > 0;
            wrote_usage = changed && usage.is_some();
        }
        RunEvent::UsageConfirmed {
            attempt_id, usage, ..
        } => wrote_usage = usage_sqlite(tx, attempt_id, usage, seq).await?,
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
            crate::storage::sql::recovery::refresh_sqlite(tx, iid).await?;
        }
        _ => {}
    }
    Ok(wrote_usage)
}

async fn apply_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
    rid: &str,
    e: &RunEvent,
    seq: i64,
    now: i64,
    finish_usage: Option<&ConfirmedUsage>,
) -> anyhow::Result<bool> {
    let mut wrote_usage = false;
    match e {
        RunEvent::RequestFailed { error } => {
            sqlx::query("UPDATE inference_run_observations SET failure_json=$1 WHERE id=$2")
                .bind(serde_json::to_string(error)?)
                .bind(rid)
                .execute(&mut **tx)
                .await?;
        }
        RunEvent::ModelTurnStarted {
            retention_detached,
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
            if !retention_detached {
                sqlx::query("UPDATE inference_run_observations SET background_active=background_active+1 WHERE id=$1").bind(rid).execute(&mut **tx).await?;
            }
        }
        RunEvent::ModelTurnFinished {
            retention_detached,
            model_turn_id,
            status,
        } => {
            sqlx::query("UPDATE model_turn_observations SET status=$1,finished_at=$2,last_event_sequence=$3 WHERE id=$4").bind(status).bind(now).bind(seq).bind(model_turn_id).execute(&mut **tx).await?;
            if !retention_detached {
                sqlx::query("UPDATE inference_run_observations SET background_active=GREATEST(background_active-1,0) WHERE id=$1").bind(rid).execute(&mut **tx).await?;
            }
        }
        RunEvent::TargetAttemptStarted {
            model_turn_id,
            attempt_id,
            target_id,
            provider_id,
            provider_name,
            upstream_model,
            protocol,
            cost_basis,
            ..
        } => {
            sqlx::query("INSERT INTO target_attempt_observations (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,cost_basis,status,started_at,last_event_sequence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'running',$11,$12) ON CONFLICT (id) DO NOTHING").bind(attempt_id).bind(model_turn_id).bind(rid).bind(iid).bind(target_id).bind(provider_id).bind(provider_name).bind(upstream_model).bind(protocol).bind(cost_basis_column(*cost_basis)).bind(now).bind(seq).execute(&mut **tx).await?;
        }
        RunEvent::TargetAttemptFinished {
            attempt_id,
            status,
            status_code,
            error_code,
            error,
            duration_ms,
            first_token_ms,
            usage,
            amounts,
            ..
        } => {
            let usage = finish_usage.or(usage.as_ref());
            let sql = match (amounts.is_some(), usage.is_some()) {
                (true, true) => {
                    "UPDATE target_attempt_observations SET status=$1,status_code=$2,error_code=$3,error_source=$4,error_message=$5,upstream_error_code=$6,finished_at=$7,duration_ms=$8,first_token_ms=$9,cost_basis=$10,cost_usd_nanos=$11,cost_custom_nanos=$12,billing_state=$13,billing_usd_nanos=$14,billing_custom_nanos=$15,input_tokens=COALESCE($16,input_tokens),output_tokens=COALESCE($17,output_tokens),cache_read_tokens=COALESCE($18,cache_read_tokens),cache_write_tokens=COALESCE($19,cache_write_tokens),reasoning_tokens=COALESCE($20,reasoning_tokens),usage_recorded=TRUE,last_event_sequence=$21 WHERE id=$22"
                }
                (true, false) => {
                    "UPDATE target_attempt_observations SET status=$1,status_code=$2,error_code=$3,error_source=$4,error_message=$5,upstream_error_code=$6,finished_at=$7,duration_ms=$8,first_token_ms=$9,cost_basis=$10,cost_usd_nanos=$11,cost_custom_nanos=$12,billing_state=$13,billing_usd_nanos=$14,billing_custom_nanos=$15,last_event_sequence=$16 WHERE id=$17"
                }
                (false, true) => {
                    "UPDATE target_attempt_observations SET status=$1,status_code=$2,error_code=$3,error_source=$4,error_message=$5,upstream_error_code=$6,finished_at=$7,duration_ms=$8,first_token_ms=$9,input_tokens=COALESCE($10,input_tokens),output_tokens=COALESCE($11,output_tokens),cache_read_tokens=COALESCE($12,cache_read_tokens),cache_write_tokens=COALESCE($13,cache_write_tokens),reasoning_tokens=COALESCE($14,reasoning_tokens),usage_recorded=TRUE,last_event_sequence=$15 WHERE id=$16"
                }
                (false, false) => {
                    "UPDATE target_attempt_observations SET status=$1,status_code=$2,error_code=$3,error_source=$4,error_message=$5,upstream_error_code=$6,finished_at=$7,duration_ms=$8,first_token_ms=$9,last_event_sequence=$10 WHERE id=$11"
                }
            };
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(status)
                .bind(status_code.map(i64::from))
                .bind(error_code)
                .bind(error.as_ref().and_then(|error| error.source.as_deref()))
                .bind(error.as_ref().and_then(|error| error.message.as_deref()))
                .bind(
                    error
                        .as_ref()
                        .and_then(|error| error.upstream_code.as_deref()),
                )
                .bind(now)
                .bind(duration_ms)
                .bind(first_token_ms);
            if let Some(amounts) = amounts {
                let (cost_basis, cost_usd, cost_custom, billing_state, billing_usd, billing_custom) =
                    attempt_amount_columns(amounts);
                query = query
                    .bind(cost_basis)
                    .bind(cost_usd)
                    .bind(cost_custom)
                    .bind(billing_state)
                    .bind(billing_usd)
                    .bind(billing_custom);
            }
            if let Some(usage) = usage {
                query = query
                    .bind(usage.input_tokens)
                    .bind(usage.output_tokens)
                    .bind(usage.cache_read_tokens)
                    .bind(usage.cache_write_tokens)
                    .bind(usage.reasoning_tokens);
            }
            let changed = query
                .bind(seq)
                .bind(attempt_id)
                .execute(&mut **tx)
                .await?
                .rows_affected()
                > 0;
            wrote_usage = changed && usage.is_some();
        }
        RunEvent::UsageConfirmed {
            attempt_id, usage, ..
        } => wrote_usage = usage_postgres(tx, attempt_id, usage, seq).await?,
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
            crate::storage::sql::recovery::refresh_postgres(tx, iid).await?;
        }
        _ => {}
    }
    Ok(wrote_usage)
}

// attempt 明细是唯一用量事实；interaction/model_turn 的汇总列按读取时聚合，
// 写路径不再重算，避免每次确认都跨表 SUM 并刷新无读者的投影。
async fn usage_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    aid: &str,
    u: &ConfirmedUsage,
    seq: i64,
) -> anyhow::Result<bool> {
    Ok(sqlx::query("UPDATE target_attempt_observations SET input_tokens=COALESCE(?,input_tokens),output_tokens=COALESCE(?,output_tokens),cache_read_tokens=COALESCE(?,cache_read_tokens),cache_write_tokens=COALESCE(?,cache_write_tokens),reasoning_tokens=COALESCE(?,reasoning_tokens),usage_recorded=1,last_event_sequence=? WHERE id=?").bind(u.input_tokens).bind(u.output_tokens).bind(u.cache_read_tokens).bind(u.cache_write_tokens).bind(u.reasoning_tokens).bind(seq).bind(aid).execute(&mut **tx).await?.rows_affected() > 0)
}
async fn usage_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    aid: &str,
    u: &ConfirmedUsage,
    seq: i64,
) -> anyhow::Result<bool> {
    Ok(sqlx::query("UPDATE target_attempt_observations SET input_tokens=COALESCE($1,input_tokens),output_tokens=COALESCE($2,output_tokens),cache_read_tokens=COALESCE($3,cache_read_tokens),cache_write_tokens=COALESCE($4,cache_write_tokens),reasoning_tokens=COALESCE($5,reasoning_tokens),usage_recorded=TRUE,last_event_sequence=$6 WHERE id=$7").bind(u.input_tokens).bind(u.output_tokens).bind(u.cache_read_tokens).bind(u.cache_write_tokens).bind(u.reasoning_tokens).bind(seq).bind(aid).execute(&mut **tx).await?.rows_affected() > 0)
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
    Ok(sqlx::query_as("SELECT e.sequence,e.run_id,COALESCE(tr.tool_id,e.tool_id),e.kind='client_tool_handoff' FROM observation_events e LEFT JOIN observation_tool_results tr ON tr.sequence=e.sequence WHERE e.interaction_id=? AND e.run_id IS NOT NULL AND e.kind IN ('client_tool_handoff','client_tool_result') ORDER BY e.sequence,tr.ordinal")
        .bind(iid).fetch_all(&mut **tx).await?)
}

/// 活动时间只聚合 Run 行上真实请求事件写入的 last_active_at；断连判定、闲置超时、
/// 重启恢复、残留收口等终态收尾只重算状态与序列，不得推进窗口归属时间。
async fn recompute_status_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
    seq: i64,
) -> anyhow::Result<()> {
    refresh_interaction_sqlite(tx, iid, seq, None).await
}

/// 重算 Interaction 状态、活动时间与末事件序号，并可在同一条 UPDATE 里登记输入事件序号，
/// 使一个事务对 Interaction 行只写一次。
async fn refresh_interaction_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    iid: &str,
    seq: i64,
    input_sequence: Option<i64>,
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
    sqlx::query("UPDATE interaction_observations SET status=?,last_active_at=MAX(last_active_at,COALESCE(?,last_active_at)),last_event_sequence=?,input_sequence=COALESCE(?,input_sequence) WHERE id=?")
        .bind(status).bind(activity).bind(seq).bind(input_sequence).bind(iid).execute(&mut **tx).await?;
    Ok(())
}

async fn client_tool_evidence_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
) -> anyhow::Result<Vec<(i64, String, Option<String>, bool)>> {
    Ok(sqlx::query_as("SELECT e.sequence,e.run_id,COALESCE(tr.tool_id,e.tool_id),e.kind='client_tool_handoff' FROM observation_events e LEFT JOIN observation_tool_results tr ON tr.sequence=e.sequence WHERE e.interaction_id=$1 AND e.run_id IS NOT NULL AND e.kind IN ('client_tool_handoff','client_tool_result') ORDER BY e.sequence,tr.ordinal")
        .bind(iid).fetch_all(&mut **tx).await?)
}

async fn recompute_status_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
    seq: i64,
) -> anyhow::Result<()> {
    refresh_interaction_postgres(tx, iid, seq, None).await
}

async fn refresh_interaction_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    iid: &str,
    seq: i64,
    input_sequence: Option<i64>,
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
    sqlx::query("UPDATE interaction_observations SET status=$1,last_active_at=GREATEST(last_active_at,COALESCE($2,last_active_at)),last_event_sequence=$3,input_sequence=COALESCE($4,input_sequence) WHERE id=$5")
        .bind(status).bind(activity).bind(seq).bind(input_sequence).bind(iid).execute(&mut **tx).await?;
    Ok(())
}

/// 客户端交付结算：Run 交付时刻与终态候选、可选的尾源。可单独提交，
/// 也可并入 `finish_run_with_completion` 的收尾事务。
pub(super) struct ClientCompletionWrite<'a> {
    pub delivered_at: i64,
    pub waiting_client: bool,
    pub tail: Option<TailSource<'a>>,
}

/// 交付结算的事务内语句（不含 Interaction 状态重算与事件行）。
/// 返回 `(状态, 记录时刻)`；Run 已结算过交付时返回 `None`，尾源仍照常写入。
async fn apply_client_completion_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    interaction_id: &str,
    completion: &ClientCompletionWrite<'_>,
    seq: i64,
) -> anyhow::Result<Option<(String, i64)>> {
    let requested = if completion.waiting_client {
        "waiting_client"
    } else {
        "completed"
    };
    let row: Option<(String, i64)> = sqlx::query_as(
        "UPDATE inference_run_observations SET
            status=CASE
                WHEN status='failed' THEN status
                WHEN user_interrupted=1 THEN 'user_interrupted'
                WHEN status NOT IN ('running','waiting_client') THEN status
                WHEN ?1='waiting_client' AND EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) THEN 'superseded'
                ELSE ?1 END,
            delivery_completed_at=?2,finished_at=COALESCE(finished_at,MAX(started_at,?2)),
            last_active_at=MAX(last_active_at,?2),last_event_sequence=?3
        WHERE id=?4 AND delivery_completed_at IS NULL
        RETURNING status,last_active_at",
    )
    .bind(requested)
    .bind(completion.delivered_at)
    .bind(seq)
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(tail) = &completion.tail {
        write_tail_source_sqlite(tx, run_id, interaction_id, tail).await?;
    }
    let Some((status, recorded_at)) = row else {
        return Ok(None);
    };
    if matches!(status.as_str(), "user_interrupted" | "superseded") {
        sqlx::query("UPDATE inference_run_observations SET terminal_reason=? WHERE id=?")
            .bind(&status)
            .bind(run_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(Some((status, recorded_at)))
}

async fn apply_client_completion_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    interaction_id: &str,
    completion: &ClientCompletionWrite<'_>,
    seq: i64,
) -> anyhow::Result<Option<(String, i64)>> {
    let requested = if completion.waiting_client {
        "waiting_client"
    } else {
        "completed"
    };
    let row: Option<(String, i64)> = sqlx::query_as(
        "UPDATE inference_run_observations SET
            status=CASE
                WHEN status='failed' THEN status
                WHEN user_interrupted THEN 'user_interrupted'
                WHEN status NOT IN ('running','waiting_client') THEN status
                WHEN $1='waiting_client' AND EXISTS(SELECT 1 FROM inference_run_observations c WHERE c.parent_run_id=inference_run_observations.id AND c.interaction_id=inference_run_observations.interaction_id) THEN 'superseded'
                ELSE $1 END,
            delivery_completed_at=$2,finished_at=COALESCE(finished_at,GREATEST(started_at,$2)),
            last_active_at=GREATEST(last_active_at,$2),last_event_sequence=$3
        WHERE id=$4 AND delivery_completed_at IS NULL
        RETURNING status,last_active_at",
    )
    .bind(requested)
    .bind(completion.delivered_at)
    .bind(seq)
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(tail) = &completion.tail {
        write_tail_source_postgres(tx, run_id, interaction_id, tail).await?;
    }
    let Some((status, recorded_at)) = row else {
        return Ok(None);
    };
    if matches!(status.as_str(), "user_interrupted" | "superseded") {
        sqlx::query("UPDATE inference_run_observations SET terminal_reason=$1 WHERE id=$2")
            .bind(&status)
            .bind(run_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(Some((status, recorded_at)))
}

/// 尾源与其待回传工具 ID；随客户端交付事务一起落盘。
pub(super) struct TailSource<'a> {
    pub principal: &'a str,
    pub last_unit_hash: &'a str,
    pub pending_tool_ids: &'a [String],
    pub expires_at: i64,
}

const INSERT_PENDING_TOOLS: &str =
    "INSERT INTO observation_pending_tools (principal,tool_id,run_id,interaction_id,expires_at) ";
// 五列一组，给 SQLite 旧版本的 999 个绑定参数上限留余量。
const PENDING_TOOLS_BATCH_SIZE: usize = 180;

async fn write_tail_source_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    interaction_id: &str,
    tail: &TailSource<'_>,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO observation_tail_sources (run_id,interaction_id,principal,last_unit_hash,expires_at) VALUES (?,?,?,?,?) ON CONFLICT(run_id) DO UPDATE SET interaction_id=excluded.interaction_id,principal=excluded.principal,last_unit_hash=excluded.last_unit_hash,expires_at=excluded.expires_at")
        .bind(run_id)
        .bind(interaction_id)
        .bind(tail.principal)
        .bind(tail.last_unit_hash)
        .bind(tail.expires_at)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM observation_pending_tools WHERE run_id=?")
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    for tools in tail.pending_tool_ids.chunks(PENDING_TOOLS_BATCH_SIZE) {
        let mut insert = sqlx::QueryBuilder::<sqlx::Sqlite>::new(INSERT_PENDING_TOOLS);
        insert.push_values(tools, |mut row, tool_id| {
            row.push_bind(tail.principal)
                .push_bind(tool_id)
                .push_bind(run_id)
                .push_bind(interaction_id)
                .push_bind(tail.expires_at);
        });
        insert.build().execute(&mut **tx).await?;
    }
    Ok(())
}

async fn write_tail_source_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    interaction_id: &str,
    tail: &TailSource<'_>,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO observation_tail_sources (run_id,interaction_id,principal,last_unit_hash,expires_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT(run_id) DO UPDATE SET interaction_id=EXCLUDED.interaction_id,principal=EXCLUDED.principal,last_unit_hash=EXCLUDED.last_unit_hash,expires_at=EXCLUDED.expires_at")
        .bind(run_id)
        .bind(interaction_id)
        .bind(tail.principal)
        .bind(tail.last_unit_hash)
        .bind(tail.expires_at)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM observation_pending_tools WHERE run_id=$1")
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    for tools in tail.pending_tool_ids.chunks(PENDING_TOOLS_BATCH_SIZE) {
        let mut insert = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_PENDING_TOOLS);
        insert.push_values(tools, |mut row, tool_id| {
            row.push_bind(tail.principal)
                .push_bind(tool_id)
                .push_bind(run_id)
                .push_bind(interaction_id)
                .push_bind(tail.expires_at);
        });
        insert.build().execute(&mut **tx).await?;
    }
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
/// 把 `attrs` 行解码成事件视图；`kind` 由列拼回 payload。
pub(super) fn observation_event_from_row(
    sequence: i64,
    occurred_at: i64,
    interaction_id: Option<String>,
    run_id: Option<String>,
    rejection_id: Option<String>,
    kind: String,
    attrs: Option<Vec<u8>>,
) -> anyhow::Result<ObservationEvent> {
    let mut payload = match attrs {
        Some(bytes) => serde_json::from_slice::<Value>(&crate::storage_codec::decode(&bytes)?)?,
        None => Value::Object(serde_json::Map::new()),
    };
    payload["kind"] = Value::String(kind.clone());
    Ok(ObservationEvent {
        sequence,
        occurred_at,
        interaction_id,
        run_id,
        rejection_id,
        kind,
        payload,
    })
}

async fn load_events_sqlite(
    pool: &SqlitePool,
    after: i64,
) -> anyhow::Result<Vec<ObservationEvent>> {
    let rows=sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,attrs FROM observation_events WHERE sequence > ? ORDER BY sequence LIMIT 512").bind(after).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            observation_event_from_row(
                r.try_get(0)?,
                r.try_get(1)?,
                r.try_get(2)?,
                r.try_get(3)?,
                r.try_get(4)?,
                r.try_get(5)?,
                r.try_get(6)?,
            )
        })
        .collect()
}
async fn load_events_postgres(pool: &PgPool, after: i64) -> anyhow::Result<Vec<ObservationEvent>> {
    let rows=sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,attrs FROM observation_events WHERE sequence > $1 ORDER BY sequence LIMIT 512").bind(after).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            observation_event_from_row(
                r.try_get(0)?,
                r.try_get(1)?,
                r.try_get(2)?,
                r.try_get(3)?,
                r.try_get(4)?,
                r.try_get(5)?,
                r.try_get(6)?,
            )
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

/// 序列化后的事件载荷里的 `kind`。`RunEvent` 恒带 `kind` 标签；缺失说明序列化契约被破坏，
/// 直接报错，不再落成 `observation_gap` 掩盖缺陷。
fn event_kind(payload: &Value) -> anyhow::Result<String> {
    payload["kind"]
        .as_str()
        .map(str::to_owned)
        .context("serialized run event has no kind")
}

/// 不携带任何发现的凭据映射事件没有可展示的内容，所有写入路径一律不落库。
/// 事件在到达写入路径之前（Turn 开始前置缓冲、批量落库）就按此谓词过滤，
/// 保证各路径对同一事件的取舍一致。
pub(super) fn is_empty_credential_mapping(event: &RunEvent) -> bool {
    matches!(
        event,
        RunEvent::CredentialMappingsCreated { discoveries, .. }
            | RunEvent::CredentialMappingsReused { discoveries, .. }
            if discoveries.is_empty()
    )
}

/// 事务内要落库的一个普通事件（不含正文块、工具结果与用量确认：它们各有专门的写入路径）。
struct PlainEvent<'a> {
    event: &'a RunEvent,
    sequence: i64,
    at: i64,
    kind: &'a str,
    payload: Value,
    /// 随事件写入 attempt 行的用量（仅收尾事件）。
    finish_usage: Option<&'a ConfirmedUsage>,
    /// `false`：投影已由同批前序事件吸收（用量折叠），只写事件行。
    apply: bool,
}

/// 各后端的“普通事件落库”与“准入随附事件落库”。
///
/// 普通事件的写入步骤——更新投影行、收集事件行、同事务登记凭据映射复用、产出给订阅方的
/// 事件——在 Turn 开始前置事件、批量落库与准入随附事件之间共用同一份定义；
/// 各路径的差异只体现在传入的 `PlainEvent` 上（是否应用投影、是否带收尾用量）。
macro_rules! backend_event_writers {
    (
        $record:ident,
        $follow_ups:ident,
        $db:ty,
        $apply:ident,
        $upsert:ident,
        $flush:ident,
        $touch_run:literal,
        $touch_interaction:literal
    ) => {
        /// 返回 `(本事件是否写入了用量, 订阅方事件)`。
        async fn $record(
            tx: &mut sqlx::Transaction<'_, $db>,
            writes: &mut EventWrites<'_>,
            interaction_id: &str,
            run_id: &str,
            record: PlainEvent<'_>,
        ) -> anyhow::Result<(bool, ObservationEvent)> {
            let wrote_usage = if record.apply {
                $apply(
                    tx,
                    interaction_id,
                    run_id,
                    record.event,
                    record.sequence,
                    record.at,
                    record.finish_usage,
                )
                .await?
            } else {
                false
            };
            writes.push_event(record.sequence, record.at, record.kind, &record.payload)?;
            // 复用与事件同一事务登记：侧栏只读登记表，事件与登记行不能分叉。
            if let RunEvent::CredentialMappingsReused { discoveries, .. } = record.event {
                $upsert(tx, interaction_id, Some(run_id), record.at, discoveries).await?;
            }
            let observed = event(
                record.sequence,
                record.at,
                Some(interaction_id),
                Some(run_id),
                None,
                record.kind,
                record.payload,
            );
            Ok((wrote_usage, observed))
        }

        /// 准入事务内写随附事件，并把 Run 与 Interaction 的最新事件序号推进到最后一个。
        async fn $follow_ups(
            tx: &mut sqlx::Transaction<'_, $db>,
            admission: &Admission<'_>,
            sequences: &[i64],
        ) -> anyhow::Result<Vec<ObservationEvent>> {
            let interaction_id = admission.interaction_id;
            let run_id = admission.start.id.as_str();
            let mut writes = EventWrites::new(interaction_id, run_id);
            let mut events = Vec::with_capacity(sequences.len());
            for (follow_up, sequence) in admission.follow_ups.iter().zip(sequences) {
                let payload = serde_json::to_value(follow_up)?;
                let kind = event_kind(&payload)?;
                let (_, observed) = $record(
                    tx,
                    &mut writes,
                    interaction_id,
                    run_id,
                    PlainEvent {
                        event: follow_up,
                        sequence: *sequence,
                        at: admission.now,
                        kind: &kind,
                        payload,
                        finish_usage: None,
                        apply: true,
                    },
                )
                .await?;
                events.push(observed);
            }
            writes.$flush(tx).await?;
            let last = *sequences
                .last()
                .context("admission follow-ups allocate no sequence")?;
            sqlx::query($touch_run)
                .bind(last)
                .bind(run_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query($touch_interaction)
                .bind(last)
                .bind(interaction_id)
                .execute(&mut **tx)
                .await?;
            Ok(events)
        }
    };
}

backend_event_writers!(
    record_event_sqlite,
    write_follow_ups_sqlite,
    sqlx::Sqlite,
    apply_sqlite,
    upsert_credential_mapping_reuses_sqlite,
    flush_sqlite,
    "UPDATE inference_run_observations SET last_event_sequence=? WHERE id=?",
    "UPDATE interaction_observations SET last_event_sequence=? WHERE id=?"
);
backend_event_writers!(
    record_event_postgres,
    write_follow_ups_postgres,
    sqlx::Postgres,
    apply_postgres,
    upsert_credential_mapping_reuses_postgres,
    flush_postgres,
    "UPDATE inference_run_observations SET last_event_sequence=$1 WHERE id=$2",
    "UPDATE interaction_observations SET last_event_sequence=$1 WHERE id=$2"
);

/// 首轮写入在事务内的全部写入：Turn 开始（含首个 attempt）、前置事件、输入与工具结果，
/// 最后各推进一次 Run 与 Interaction 的活动字段。
macro_rules! backend_turn_start_writer {
    (
        $write:ident,
        $db:ty,
        $record:ident,
        $apply:ident,
        $intern:path,
        $flush:ident,
        $refresh:ident,
        $touch_run:literal,
        $touch_interaction:literal
    ) => {
        #[allow(clippy::too_many_arguments)]
        async fn $write(
            tx: &mut sqlx::Transaction<'_, $db>,
            interaction_id: &str,
            run_id: &str,
            principal: &str,
            is_root_run: bool,
            start: &TurnStart<'_>,
            prepared: PreparedTurnStart<'_>,
            sequences: &[i64],
        ) -> anyhow::Result<Vec<ObservationEvent>> {
            let layout = TurnLayout::new(start, prepared.leading.len(), sequences)?;
            let PreparedTurnStart {
                turn_payload,
                leading,
                input_units,
                tools,
                latest_at,
                ..
            } = prepared;
            let mut writes = EventWrites::new(interaction_id, run_id);
            let mut result = Vec::with_capacity(sequences.len());
            $apply(
                tx,
                interaction_id,
                run_id,
                start.turn,
                layout.turn,
                start.at,
                None,
            )
            .await?;
            if let Some(attempt) = start.attempt {
                $apply(
                    tx,
                    interaction_id,
                    run_id,
                    attempt,
                    layout.attempt,
                    start.attempt_at,
                    None,
                )
                .await?;
            }
            writes.push_event(layout.turn, start.at, "model_turn_started", &turn_payload)?;
            result.push(event(
                layout.turn,
                start.at,
                Some(interaction_id),
                Some(run_id),
                None,
                "model_turn_started",
                turn_payload,
            ));
            for (item, sequence) in leading.into_iter().zip(layout.leading) {
                let (_, observed) = $record(
                    tx,
                    &mut writes,
                    interaction_id,
                    run_id,
                    PlainEvent {
                        event: item.event,
                        sequence: *sequence,
                        at: item.at,
                        kind: &item.kind,
                        payload: item.payload,
                        finish_usage: None,
                        apply: true,
                    },
                )
                .await?;
                result.push(observed);
            }
            let mut root_input_sequence = None;
            if let (Some(mut units), Some(sequence)) = (input_units, layout.input) {
                let content_ids = $intern(tx, principal, &mut units).await?;
                writes.push_event(
                    sequence,
                    start.input_at,
                    "user_input_recorded",
                    &Value::Null,
                )?;
                writes.push_refs(sequence, &content_ids)?;
                if is_root_run {
                    root_input_sequence = Some(sequence);
                }
                result.push(event(
                    sequence,
                    start.input_at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "user_input_recorded",
                    Value::Null,
                ));
            }
            if let (Some(mut tools), Some(sequence)) = (tools, layout.tools) {
                let content_ids = $intern(tx, principal, &mut tools.units).await?;
                writes.push_event(sequence, tools.first_at, "client_tool_result", &Value::Null)?;
                writes.push_tool_results(sequence, &tools.tools, &content_ids)?;
                result.push(event(
                    sequence,
                    tools.first_at,
                    Some(interaction_id),
                    Some(run_id),
                    None,
                    "client_tool_result",
                    Value::Null,
                ));
            }
            // Run 与 Interaction 的活动字段各只写一次：先写 Run，Interaction 的状态重算
            // 读取到的就是已推进的 Run 活动时间。
            writes.$flush(tx).await?;
            sqlx::query($touch_run)
                .bind(latest_at)
                .bind(layout.last)
                .bind(run_id)
                .execute(&mut **tx)
                .await?;
            if status_event(start.turn) || !start.tool_results.is_empty() {
                $refresh(tx, interaction_id, layout.last, root_input_sequence).await?;
            } else {
                sqlx::query($touch_interaction)
                    .bind(latest_at)
                    .bind(layout.last)
                    .bind(root_input_sequence)
                    .bind(interaction_id)
                    .execute(&mut **tx)
                    .await?;
            }
            result.sort_by_key(|event| event.sequence);
            Ok(result)
        }
    };
}

backend_turn_start_writer!(
    write_turn_start_sqlite,
    sqlx::Sqlite,
    record_event_sqlite,
    apply_sqlite,
    super::content_refs::intern_sqlite,
    flush_sqlite,
    refresh_interaction_sqlite,
    "UPDATE inference_run_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=? WHERE id=?",
    "UPDATE interaction_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=?,input_sequence=COALESCE(?,input_sequence) WHERE id=?"
);
backend_turn_start_writer!(
    write_turn_start_postgres,
    sqlx::Postgres,
    record_event_postgres,
    apply_postgres,
    super::content_refs::intern_postgres,
    flush_postgres,
    refresh_interaction_postgres,
    "UPDATE inference_run_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2 WHERE id=$3",
    "UPDATE interaction_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2,input_sequence=COALESCE($3,input_sequence) WHERE id=$4"
);

struct PreparedBlock {
    channel: &'static str,
    model_turn_id: Option<String>,
    attempt_id: Option<String>,
    item_ordinal: i64,
    complete: bool,
    envelope: Option<Value>,
    units: Vec<Content>,
}

struct PreparedEvent<'a> {
    source_index: usize,
    event: &'a RunEvent,
    at: i64,
    kind: String,
    payload: Value,
    block: Option<PreparedBlock>,
    /// `(tool_id, is_error)`；正文单元在 `PreparedBatch::tool_units` 里按事件顺序排列。
    tool_result: Option<(String, bool)>,
    /// Usage written by this event's single attempt UPDATE. A finish with
    /// `None` here keeps the durable row's already recorded usage.
    finish_usage: Option<Cow<'a, ConfirmedUsage>>,
    /// A finish whose folded usage came from same-batch confirmations;
    /// those confirmations must not write the attempt row a second time.
    absorbs_batch_usage: bool,
}

/// 一批有序事件在进入事务前的准备结果。
struct PreparedBatch<'a> {
    events: Vec<PreparedEvent<'a>>,
    /// 全批工具结果正文，写入时一次登记。
    tool_units: Vec<Content>,
    /// 其用量已折叠进同批收尾事件的确认事件下标。
    absorbed_usage: std::collections::HashSet<usize>,
    /// 产生事件行的条数（用量确认不占行）。
    durable_count: usize,
    latest_at: i64,
}

/// 有序事件批在事务内的写入。块、工具结果与用量确认各有专门步骤，其余事件走共用的
/// `record_event_*`。
macro_rules! backend_ordered_writer {
    (
        $write:ident,
        $db:ty,
        $record:ident,
        $apply:ident,
        $intern:path,
        $flush:ident,
        $refresh_usage:ident,
        $recompute:ident,
        $turn_exists:literal,
        $touch_run:literal,
        $touch_interaction:literal
    ) => {
        async fn $write(
            tx: &mut sqlx::Transaction<'_, $db>,
            interaction_id: &str,
            run_id: &str,
            principal: &str,
            batch: PreparedBatch<'_>,
            sequences: &[i64],
            mut current_sequence: i64,
        ) -> anyhow::Result<Vec<ObservationEvent>> {
            let PreparedBatch {
                events,
                mut tool_units,
                absorbed_usage,
                latest_at,
                ..
            } = batch;
            let mut sequences = sequences.iter().copied();
            // 全批工具结果一次登记，按事件顺序领取。
            let mut tool_content_ids = $intern(tx, principal, &mut tool_units)
                .await?
                .into_iter();
            let mut result = Vec::with_capacity(events.len());
            let mut status_changed = false;
            let mut folded_attempts = std::collections::HashSet::new();
            let mut writes = EventWrites::new(interaction_id, run_id);
            for PreparedEvent {
                source_index,
                event: run_event,
                at,
                mut kind,
                mut payload,
                block,
                tool_result,
                finish_usage,
                absorbs_batch_usage,
            } in events
            {
                let attempt_id = match run_event {
                    RunEvent::TargetAttemptFinished { attempt_id, .. }
                    | RunEvent::UsageConfirmed { attempt_id, .. } => Some(attempt_id.as_str()),
                    _ => None,
                };
                let absorbed = absorbed_usage.contains(&source_index)
                    && matches!(run_event, RunEvent::UsageConfirmed { attempt_id, .. }
                        if folded_attempts.contains(attempt_id.as_str()));
                // 有界队列可能丢失父 Turn 开始；不能捏造父事实，也不能让子事件外键回滚整批。
                // 检查位于事务内，能观察到本批已写入的父 Turn。
                let gap;
                let run_event = if let RunEvent::TargetAttemptStarted { model_turn_id, .. } =
                    run_event
                {
                    let exists: bool = sqlx::query_scalar($turn_exists)
                        .bind(model_turn_id)
                        .bind(run_id)
                        .fetch_one(&mut **tx)
                        .await?;
                    if exists {
                        run_event
                    } else {
                        gap = RunEvent::ObservationGap {
                            reason: "model_turn_start_unavailable".into(),
                        };
                        kind = "observation_gap".into();
                        payload = serde_json::to_value(&gap)?;
                        &gap
                    }
                } else {
                    run_event
                };
                if kind == "usage_confirmed" {
                    if !absorbed {
                        $apply(
                            tx,
                            interaction_id,
                            run_id,
                            run_event,
                            current_sequence,
                            at,
                            None,
                        )
                        .await?;
                        if let RunEvent::UsageConfirmed {
                            attempt_id, usage, ..
                        } = run_event
                        {
                            writes.$flush(tx).await?;
                            $refresh_usage(tx, run_id, attempt_id, usage).await?;
                        }
                    }
                    continue;
                }
                let sequence = sequences
                    .next()
                    .context("ordered batch ran out of allocated event sequences")?;
                current_sequence = sequence;
                if let Some(mut block) = block {
                    writes.push_event(sequence, at, &kind, &payload)?;
                    let content_ids = $intern(tx, principal, &mut block.units).await?;
                    let envelope = block
                        .envelope
                        .map(|envelope| {
                            crate::storage_codec::encode(serde_json::to_vec(&envelope)?.into())
                        })
                        .transpose()?;
                    writes.push_block(BlockRow {
                        sequence,
                        model_turn_id: block.model_turn_id.clone(),
                        attempt_id: block.attempt_id.clone(),
                        item_ordinal: block.item_ordinal,
                        channel: block.channel,
                        complete: block.complete,
                        envelope,
                    });
                    writes.push_refs(sequence, &content_ids)?;
                    result.push(event(
                        sequence,
                        at,
                        Some(interaction_id),
                        Some(run_id),
                        None,
                        &kind,
                        Value::Null,
                    ));
                    continue;
                }
                if let Some(tool) = tool_result {
                    let content_id = tool_content_ids
                        .next()
                        .context("ordered batch ran out of interned tool contents")?;
                    writes.push_event(sequence, at, &kind, &payload)?;
                    writes.push_tool_results(sequence, &[tool], &[content_id])?;
                    status_changed = true;
                    result.push(event(
                        sequence,
                        at,
                        Some(interaction_id),
                        Some(run_id),
                        None,
                        &kind,
                        Value::Null,
                    ));
                    continue;
                }
                let (wrote_usage, observed) = $record(
                    tx,
                    &mut writes,
                    interaction_id,
                    run_id,
                    PlainEvent {
                        event: run_event,
                        sequence,
                        at,
                        kind: &kind,
                        payload,
                        finish_usage: finish_usage.as_deref(),
                        apply: !absorbed,
                    },
                )
                .await?;
                if wrote_usage
                    && absorbs_batch_usage
                    && let Some(attempt_id) = attempt_id
                {
                    folded_attempts.insert(attempt_id);
                }
                status_changed |= status_event(run_event);
                result.push(observed);
            }
            writes.$flush(tx).await?;
            if let Some(last) = result.last() {
                sqlx::query($touch_run)
                    .bind(latest_at)
                    .bind(last.sequence)
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await?;
                if status_changed {
                    $recompute(tx, interaction_id, last.sequence).await?;
                } else {
                    sqlx::query($touch_interaction)
                        .bind(latest_at)
                        .bind(last.sequence)
                        .bind(interaction_id)
                        .execute(&mut **tx)
                        .await?;
                }
            }
            Ok(result)
        }
    };
}

backend_ordered_writer!(
    write_ordered_sqlite,
    sqlx::Sqlite,
    record_event_sqlite,
    apply_sqlite,
    super::content_refs::intern_sqlite,
    flush_sqlite,
    refresh_finished_usage_sqlite,
    recompute_status_sqlite,
    "SELECT EXISTS(SELECT 1 FROM model_turn_observations WHERE id=? AND run_id=?)",
    "UPDATE inference_run_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=? WHERE id=?",
    "UPDATE interaction_observations SET last_active_at=MAX(last_active_at,?),last_event_sequence=? WHERE id=?"
);
backend_ordered_writer!(
    write_ordered_postgres,
    sqlx::Postgres,
    record_event_postgres,
    apply_postgres,
    super::content_refs::intern_postgres,
    flush_postgres,
    refresh_finished_usage_postgres,
    recompute_status_postgres,
    "SELECT EXISTS(SELECT 1 FROM model_turn_observations WHERE id=$1 AND run_id=$2)",
    "UPDATE inference_run_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2 WHERE id=$3",
    "UPDATE interaction_observations SET last_active_at=GREATEST(last_active_at,$1),last_event_sequence=$2 WHERE id=$3"
);

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::interaction_observation::types::{ClientErrorQuery, ForestQuery};

    #[tokio::test]
    async fn confirmed_usage_aggregates_all_runs_without_foreign_interactions() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool,
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
        );
        for (interaction, run, parent, input, output, cache) in [
            ("usage-root", "usage-first", None, Some(11), None, Some(0)),
            (
                "usage-root",
                "usage-child",
                Some("usage-first"),
                None,
                Some(3),
                None,
            ),
            (
                "usage-foreign",
                "usage-other",
                None,
                Some(99),
                Some(99),
                Some(99),
            ),
        ] {
            admit_waiting_scenario_run(&store, interaction, run, parent).await?;
            store
                .persist_run_event(
                    interaction,
                    run,
                    &RunEvent::ModelTurnStarted {
                        retention_detached: false,
                        model_turn_id: run.into(),
                        route_id: "route".into(),
                        model_display_name: None,
                        estimated_input_tokens: None,
                    },
                    2,
                )
                .await?;
            store
                .persist_run_event(
                    interaction,
                    run,
                    &RunEvent::TargetAttemptStarted {
                        model_turn_id: run.into(),
                        attempt_id: run.into(),
                        target_id: "target".into(),
                        provider_id: "provider".into(),
                        provider_name: "provider".into(),
                        upstream_model: "model".into(),
                        protocol: "responses".into(),
                        upstream_url: "http://localhost".into(),
                        cost_basis: None,
                    },
                    3,
                )
                .await?;
            store
                .persist_run_event(
                    interaction,
                    run,
                    &RunEvent::UsageConfirmed {
                        model_turn_id: run.into(),
                        attempt_id: run.into(),
                        usage: ConfirmedUsage {
                            input_tokens: input,
                            output_tokens: output,
                            cache_read_tokens: cache,
                            cache_write_tokens: None,
                            reasoning_tokens: None,
                            coverage: None,
                        },
                    },
                    4,
                )
                .await?;
            store
                .persist_run_event(
                    interaction,
                    run,
                    &RunEvent::UsageConfirmed {
                        model_turn_id: run.into(),
                        attempt_id: run.into(),
                        usage: ConfirmedUsage {
                            // An identical confirmation is idempotent; changed confirmations
                            // are revisions (covered by durable_usage_revisions_replace_contributions_and_reclaim_work).
                            input_tokens: input,
                            output_tokens: output,
                            cache_read_tokens: cache,
                            cache_write_tokens: None,
                            reasoning_tokens: None,
                            coverage: None,
                        },
                    },
                    5,
                )
                .await?;
        }
        let detail = store
            .get_interaction("usage-root", ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(detail.interaction.usage.input_tokens, Some(11));
        assert_eq!(detail.interaction.usage.output_tokens, Some(3));
        assert_eq!(detail.interaction.usage.cache_read_tokens, Some(0));
        assert_eq!(detail.interaction.usage.cache_write_tokens, None);
        assert_eq!(detail.interaction.usage.reasoning_tokens, None);
        let coverage = detail.interaction.usage.coverage.unwrap();
        assert_eq!(coverage.attempt_count, 2);
        assert_eq!(coverage.missing_input_tokens, 1);
        assert_eq!(coverage.missing_output_tokens, 1);
        assert_eq!(coverage.missing_cache_read_tokens, 1);
        assert_eq!(coverage.missing_cache_write_tokens, 2);
        assert_eq!(coverage.missing_reasoning_tokens, 2);
        Ok(())
    }

    async fn confirmed_usage_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        use crate::interaction_observation::types::UsageCoverage;

        let id = "partial-usage";
        admit_tool_run(store, id, None, "alice").await?;
        store
            .persist_run_event(
                id,
                id,
                &RunEvent::ModelTurnStarted {
                    retention_detached: false,
                    model_turn_id: id.into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                2,
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
                        cost_basis: None,
                    },
                    3,
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
            store.persist_run_event(id, id, &event, 4).await?;
            store.persist_run_event(id, id, &event, 5).await?;
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
                        amounts: None,
                    },
                    6,
                )
                .await?;
        }
        let detail = store
            .get_interaction(id, ForestQuery::default())
            .await?
            .unwrap();
        let expected = ConfirmedUsage {
            // Clamp per attempt before summing; unknown cache leaves net input unknown.
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
        assert!(unknown_cache_usage.payload["usage"]["input_tokens"].is_null());
        // 管理事件分页在历史 watermark 上同样投影；原始 attempt 仍为总输入。
        let through = reported_usage.sequence;
        let historical = store
            .get_interaction_events(
                id,
                crate::interaction_observation::types::InteractionEventsQuery {
                    through_sequence: Some(through),
                    ..Default::default()
                },
            )
            .await?
            .unwrap();
        assert_eq!(historical.snapshot_sequence, through);
        let historical_events = &historical.runs[0].events;
        assert!(
            historical_events
                .iter()
                .all(|event| event.sequence <= through)
        );
        let historical_usage = historical_events
            .iter()
            .find(|event| event.sequence == through)
            .unwrap();
        assert_eq!(historical_usage.payload["usage"]["input_tokens"], 7);
        for (attempt, raw_input) in [("reported", 12), ("overcached", 3), ("unknown-cache", 8)] {
            assert_eq!(
                store
                    .attempt_usage(&format!("{id}-{attempt}"))
                    .await?
                    .unwrap()
                    .input_tokens,
                Some(raw_input)
            );
        }
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
        );
        confirmed_usage_scenario(&store).await?;
        let payloads: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT attrs FROM observation_events WHERE kind='target_attempt_finished'",
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
        // 公共读取只从 attempt 明细派生，不依赖任何持久化汇总投影。
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

    #[tokio::test]
    async fn durable_usage_revisions_replace_contributions_and_reclaim_work() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
        );
        confirmed_usage_scenario(&store).await?;
        let totals = |pool: SqlitePool| async move {
            sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT SUM(request_count), SUM(input_tokens_sum), SUM(output_tokens_sum) FROM stats_usage_buckets WHERE scope='global'",
            ).fetch_one(&pool).await
        };
        assert_eq!(totals(pool.clone()).await?, (1, 7, 6));
        let revision = RunEvent::UsageConfirmed {
            model_turn_id: "partial-usage".into(),
            attempt_id: "partial-usage-reported".into(),
            usage: ConfirmedUsage {
                input_tokens: Some(15),
                output_tokens: Some(0),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
                coverage: None,
            },
        };
        for timestamp in [8, 9] {
            store
                .persist_run_event("partial-usage", "partial-usage", &revision, timestamp)
                .await?;
        }
        assert_eq!(totals(pool.clone()).await?, (1, 10, 3));
        // A transaction interrupted before commit cannot leak a revised contribution.
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE target_attempt_observations SET output_tokens=999 WHERE id='partial-usage-reported'")
            .execute(&mut *tx).await?;
        tx.rollback().await?;
        assert_eq!(totals(pool.clone()).await?, (1, 10, 3));
        sqlx::query("DELETE FROM interaction_observations WHERE id='partial-usage'")
            .execute(&pool)
            .await?;
        assert_eq!(totals(pool.clone()).await?, (1, 10, 3));
        let checkpoints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stats_usage_checkpoints")
            .fetch_one(&pool)
            .await?;
        assert_eq!(checkpoints, 0);
        crate::migrations::migrate_sqlite(&pool, None).await?;
        assert_eq!(totals(pool.clone()).await?, (1, 10, 3));
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
                follow_ups: &[],
                now: 1,
            })
            .await?;
        Ok(())
    }

    /// 尾源随客户端交付事务落盘；这里的 Run 以 `run_id` 同名的交互承载。
    async fn persist_tail(
        store: &ObservationStore,
        run: &str,
        principal: &str,
        hash: &str,
        pending: &[String],
    ) -> anyhow::Result<()> {
        store
            .persist_client_completion(
                run,
                run,
                3,
                false,
                Some(TailSource {
                    principal,
                    last_unit_hash: hash,
                    pending_tool_ids: pending,
                    expires_at: 10,
                }),
            )
            .await?;
        Ok(())
    }

    async fn pending_tools_replacement_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        admit_tool_run(store, "pending-root", None, "alice").await?;
        admit_tool_run(store, "pending-foreign", None, "bob").await?;
        let tools: Vec<String> = (0..367).map(|index| format!("call-{index:03}")).collect();
        persist_tail(store, "pending-root", "alice", "first", &tools).await?;
        persist_tail(store, "pending-foreign", "bob", "foreign", &tools[..1]).await?;
        let mut actual = store.pending_tool_sources("alice", &tools, 9).await?;
        actual.sort();
        assert_eq!(
            actual,
            tools
                .iter()
                .map(|id| (id.clone(), "pending-root".into(), "pending-root".into()))
                .collect::<Vec<_>>()
        );
        persist_tail(store, "pending-root", "alice", "second", &tools[359..]).await?;
        let duplicate = vec!["duplicate".to_owned(); 2];
        assert!(
            persist_tail(store, "pending-root", "alice", "failed", &duplicate)
                .await
                .is_err()
        );
        let mut remaining = store.pending_tool_sources("alice", &tools, 9).await?;
        remaining.sort();
        assert_eq!(
            remaining,
            tools[359..]
                .iter()
                .map(|id| (id.clone(), "pending-root".into(), "pending-root".into()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            store
                .tail_sources_by_hashes("alice", "other-run", &["second".into()], 9)
                .await?,
            [("pending-root".into(), "pending-root".into(), None)]
        );
        assert!(
            store
                .pending_tool_sources("alice", &tools, 10)
                .await?
                .is_empty()
        );
        persist_tail(store, "pending-root", "alice", "empty", &[]).await?;
        assert!(
            store
                .pending_tool_sources("alice", &tools, 9)
                .await?
                .is_empty()
        );
        assert_eq!(
            store.pending_tool_sources("bob", &tools, 9).await?,
            [(
                tools[0].clone(),
                "pending-foreign".into(),
                "pending-foreign".into()
            )]
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_tools_replacement_is_atomic_across_batches() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
        );
        pending_tools_replacement_scenario(&store).await?;
        pool.close().await;
        Ok(())
    }

    /// 首轮写入的事件序号按生产方到达顺序分配：工具结果先到则先于用户输入事件，
    /// 反之亦然；与存储实现里先写哪一块无关。
    #[tokio::test]
    async fn turn_start_sequences_follow_producer_order() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
        );
        let tool = [(
            RunEvent::ClientToolResult {
                tool_id: "call".into(),
                content: serde_json::json!("result"),
                is_error: false,
            },
            3,
        )];
        let input = vec!["question".to_owned()];
        for (run, tools_first) in [("tools-first", true), ("input-first", false)] {
            admit_tool_run(&store, run, None, "owner").await?;
            let turn = RunEvent::ModelTurnStarted {
                retention_detached: false,
                model_turn_id: format!("turn-{run}"),
                route_id: "route".into(),
                model_display_name: None,
                estimated_input_tokens: None,
            };
            let events = store
                .persist_turn_start(
                    run,
                    run,
                    TurnStart {
                        turn: &turn,
                        at: 2,
                        attempt: None,
                        attempt_at: 2,
                        input: Some(&input),
                        input_at: 4,
                        tool_results: &tool,
                        tools_first,
                        leading: &[],
                    },
                )
                .await?
                .expect("run row exists");
            let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
            let expected = if tools_first {
                [
                    "model_turn_started",
                    "client_tool_result",
                    "user_input_recorded",
                ]
            } else {
                [
                    "model_turn_started",
                    "user_input_recorded",
                    "client_tool_result",
                ]
            };
            assert_eq!(kinds, expected, "{run}: published in sequence order");
            assert!(
                events
                    .windows(2)
                    .all(|pair| pair[0].sequence < pair[1].sequence)
            );
            let stored: Vec<String> = store
                .replay(0)
                .await?
                .into_iter()
                .filter(|event| event.run_id.as_deref() == Some(run))
                .map(|event| event.kind)
                .filter(|kind| expected.contains(&kind.as_str()))
                .collect();
            assert_eq!(stored, expected, "{run}: replay follows the same order");
        }
        pool.close().await;
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
                false,
            )
            .await?;
        assert!(finished.sequence > failure.sequence);
        assert!(finished.occurred_at >= failure.occurred_at);
        let page = store
            .client_errors(ClientErrorQuery {
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
                    false,
                )
                .await?;
        }
        let page = store
            .client_errors(ClientErrorQuery {
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
                follow_ups: &[],
                now: 1,
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
                false,
            )
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn new_user_admission_and_finish_order_preserve_interruption_and_failure()
    -> anyhow::Result<()> {
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
        );
        for (case, waiting, user_first, finish_first, failed, expected) in [
            ("sent-admit-finish", false, false, false, false, "completed"),
            ("sent-finish-admit", false, false, true, false, "completed"),
            (
                "admit-sent-finish",
                false,
                true,
                false,
                false,
                "user_interrupted",
            ),
            ("undelivered", false, true, false, false, "user_interrupted"),
            (
                "tool-handoff",
                true,
                false,
                false,
                false,
                "user_interrupted",
            ),
            ("failure-wins", false, true, false, true, "failed"),
        ] {
            admit_waiting_scenario_run(&store, case, case, None).await?;
            store
                .persist_run_event(
                    case,
                    case,
                    &RunEvent::PlatformToolStarted {
                        model_turn_id: "background".into(),
                        tool_id: format!("background-{case}"),
                        name: "probe".into(),
                        input: None,
                    },
                    1,
                )
                .await?;
            if waiting {
                store
                    .persist_run_event(
                        case,
                        case,
                        &RunEvent::ClientToolHandoff {
                            tool_id: format!("call-{case}"),
                            name: "probe".into(),
                            input: None,
                        },
                        1,
                    )
                    .await?;
            }
            if user_first {
                admit_new_user_after_receipt(&store, case, 2).await?;
            }
            if case != "undelivered" {
                assert!(
                    store
                        .persist_client_completion(case, case, 3, waiting, None)
                        .await?
                        .is_some()
                );
                assert!(
                    store
                        .persist_client_completion(case, case, 4, waiting, None)
                        .await?
                        .is_none()
                );
            }
            if finish_first {
                finish_scenario_run(&store, case, case, "completed").await?;
            }
            if !user_first {
                admit_new_user_after_receipt(&store, case, 5).await?;
            }
            if !finish_first {
                store
                    .finish_run(
                        case,
                        case,
                        &RunOutcome {
                            client_output_committed: false,
                            delivery: None,
                            delivery_completed_at: (case != "undelivered").then_some(3),
                            status: if failed {
                                "failed"
                            } else if waiting {
                                "waiting_client"
                            } else if case == "undelivered" {
                                "interrupted"
                            } else {
                                "completed"
                            }
                            .into(),
                            terminal_reason: None,
                            generation_node_id: Some(case.into()),
                            generation_root_id: Some(case.into()),
                        },
                        6,
                        false,
                    )
                    .await?;
            }
            let detail = store
                .get_interaction(case, ForestQuery::default())
                .await?
                .unwrap();
            let run = &detail.runs[0];
            assert_eq!(run.status, expected, "{case}");
            assert_eq!(
                run.delivery_completed_at,
                (case != "undelivered").then_some(3),
                "{case}"
            );
            assert!(run.finished_at.is_some(), "{case}");
            let background: i64 = sqlx::query_scalar(
                "SELECT background_active FROM inference_run_observations WHERE id=?",
            )
            .bind(case)
            .fetch_one(&pool)
            .await?;
            assert_eq!(background, 1, "{case}");
        }
        admit_waiting_scenario_run(&store, "resolved", "resolved-parent", None).await?;
        admit_waiting_scenario_run(
            &store,
            "resolved",
            "resolved-child",
            Some("resolved-parent"),
        )
        .await?;
        store
            .persist_client_completion("resolved-parent", "resolved", 3, true, None)
            .await?;
        store
            .finish_run(
                "resolved",
                "resolved-parent",
                &RunOutcome {
                    delivery_completed_at: Some(3),
                    delivery: None,
                    client_output_committed: true,
                    status: "waiting_client".into(),
                    terminal_reason: None,
                    generation_node_id: Some("resolved-parent".into()),
                    generation_root_id: Some("resolved".into()),
                },
                3,
                false,
            )
            .await?;
        let detail = store
            .get_interaction("resolved", ForestQuery::default())
            .await?
            .unwrap();
        let parent = detail
            .runs
            .iter()
            .find(|run| run.id == "resolved-parent")
            .unwrap();
        assert_eq!(parent.status, "superseded");
        assert!(!parent.user_interrupted);
        // 已经失败的 Run 保持失败，不会被后续的成功交付事实洗成成功。
        admit_waiting_scenario_run(
            &store,
            "failed-before-receipt",
            "failed-before-receipt",
            None,
        )
        .await?;
        store
            .finish_run(
                "failed-before-receipt",
                "failed-before-receipt",
                &RunOutcome {
                    client_output_committed: false,
                    delivery: None,
                    delivery_completed_at: None,
                    status: "failed".into(),
                    terminal_reason: Some("terminal_fault".into()),
                    generation_node_id: None,
                    generation_root_id: None,
                },
                3,
                false,
            )
            .await?;
        assert!(
            store
                .persist_client_completion(
                    "failed-before-receipt",
                    "failed-before-receipt",
                    4,
                    false,
                    None
                )
                .await?
                .is_some()
        );
        let detail = store
            .get_interaction("failed-before-receipt", ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(detail.runs[0].status, "failed");
        admit_waiting_scenario_run(&store, "restart-receipt", "restart-receipt", None).await?;
        store
            .persist_client_completion("restart-receipt", "restart-receipt", 3, false, None)
            .await?;
        store.recover_after_restart().await?;
        let detail = store
            .get_interaction("restart-receipt", ForestQuery::default())
            .await?
            .unwrap();
        assert_eq!(detail.runs[0].status, "completed");
        assert_eq!(detail.runs[0].finished_at, Some(3));
        Ok(())
    }

    async fn admit_new_user_after_receipt(
        store: &ObservationStore,
        parent: &str,
        at: i64,
    ) -> anyhow::Result<()> {
        let child = format!("{parent}-new-user");
        store
            .admit(Admission {
                metadata: None,
                start: &RunStart {
                    id: child.clone(),
                    principal: "alice".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                interaction_id: &child,
                generation_root_id: Some(&child),
                generation_parent_id: Some(parent),
                has_new_user: true,
                ingress_received_at: at,
                parent_run_id: Some(parent),
                parent_interaction_id: Some(parent),
                debug_enabled: false,
                inferred_retry: false,
                grouping_reason: "new_user",
                diagnostic_source_run_id: None,
                interrupt_parent: true,
                follow_ups: &[],
                now: at,
            })
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
                        retention_detached: false,
                        model_turn_id: format!("{iid}-background"),
                        route_id: "route".into(),
                        model_display_name: None,
                        estimated_input_tokens: None,
                    },
                    8,
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
        );
        let handoff = || RunEvent::ClientToolHandoff {
            tool_id: "call".into(),
            name: "probe".into(),
            input: None,
        };
        for id in ["stale", "fresh", "resolved", "parent"] {
            admit_waiting_scenario_run(&store, id, id, None).await?;
            store.persist_run_event(id, id, &handoff(), 2).await?;
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
        assert_eq!(events[0].payload["status"], "disconnected");
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
            ObservationStore::Sqlite(pool, _) => sqlx::query_as(sql).fetch_all(pool).await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_as(sql).fetch_all(pool).await?,
        })
    }

    async fn restart_reconciliation_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        sibling_result_scenario(store, "resolved", "completed").await?;
        // 模拟旧版本已收齐结果但未重算的缓存投影。
        match store {
            ObservationStore::Sqlite(pool, _) => {
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
            )
            .await?;
        // 客户端收到工具调用后的交付结算先于 Run 收尾，已写过一次状态转换（重启前即存在）。
        store
            .persist_client_completion("pending", "pending", 3, true, None)
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
        let activity_before = run_activity_snapshot(store).await?;
        let transitions = |events: &[ObservationEvent], id: &str| -> Vec<i64> {
            events
                .iter()
                .filter(|event| {
                    event.kind == "run_state_changed" && event.run_id.as_deref() == Some(id)
                })
                .map(|event| event.sequence)
                .collect()
        };
        let transitions_before = transitions(&store.replay(0).await?, "pending");
        assert_eq!(transitions_before.len(), 1, "交付结算的状态转换");
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
        let events = store.replay(0).await?;
        let delivery_after = events
            .iter()
            .find(|event| event.sequence == delivery_before.sequence)
            .unwrap();
        assert_eq!(delivery_after.payload, delivery_before.payload);
        // 状态变化以 Run 行为准；重放里每个被重启中断的 Run 都有状态变化事件可供通知。
        for id in ["pending", "active"] {
            assert!(
                events.iter().any(|event| event.kind == "run_state_changed"
                    && event.run_id.as_deref() == Some(id)
                    && event.payload["status"] == "interrupted"),
                "{id}"
            );
        }
        let sequence = store.max_sequence().await?;
        // 每次重启恰好为被中断的等待 Run 追加一次状态转换；已有的转换行原样保留，再次对账不重复追加。
        let transitions_after = transitions(&events, "pending");
        assert_eq!(
            transitions_after.len(),
            transitions_before.len() + 1,
            "{transitions_before:?} -> {transitions_after:?}"
        );
        assert_eq!(
            transitions_after[..transitions_before.len()],
            transitions_before[..]
        );
        store.recover_after_restart().await?;
        assert_eq!(store.max_sequence().await?, sequence);
        assert_eq!(
            transitions(&store.replay(0).await?, "pending"),
            transitions_after
        );
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
            let written = store
                .persist_run_event(run, run, &event, 2)
                .await?
                .expect("result event");
            // 经公开重放视图观察落盘结果：逐项 tool_id / content / is_error。
            let replayed = store.replay(written.sequence - 1).await?;
            let event = replayed
                .into_iter()
                .find(|candidate| candidate.sequence == written.sequence)
                .expect("replayed result event");
            stored.extend(
                event.payload["results"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
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
        );
        admit_tool_run(&store, "batch", None, "alice").await?;
        let batch = vec![
            (
                RunEvent::ClientVisibleContent {
                    text: "你好".repeat(1024),
                    parts: Vec::new(),
                    block_id: "test-block:item:0".into(),
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
                    block_id: "test-block:item:0".into(),
                    item: Value::Null,
                    complete: true,
                },
                Some("block-b".into()),
                3,
            ),
        ];
        let committed = store.persist_run_events("batch", "batch", &batch).await?;
        assert!(committed[0].sequence < committed[1].sequence);
        let replay = store.replay(committed[0].sequence - 1).await?;
        assert_eq!(replay[0].payload["text"], "你好".repeat(1024));
        assert_eq!(replay[1].payload["text"], "世界");
        let boundary = committed[1].sequence;
        let invalid = vec![
            (
                RunEvent::ClientVisibleContent {
                    text: "must rollback".into(),
                    parts: Vec::new(),
                    block_id: "test-block:item:0".into(),
                    item: Value::Null,
                    complete: true,
                },
                Some("rollback".into()),
                4,
            ),
            (
                RunEvent::ModelTurnStarted {
                    retention_detached: false,
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
                    retention_detached: false,
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
                .persist_run_events("batch", "batch", &invalid)
                .await
                .is_err()
        );
        assert!(store.replay(boundary).await?.is_empty());
        assert!(
            !store
                .replay(0)
                .await?
                .iter()
                .any(|event| event.payload.to_string().contains("must rollback"))
        );
        store
            .persist_run_events(
                "batch",
                "batch",
                &[(
                    RunEvent::ClientVisibleContent {
                        text: "delayed block".into(),
                        parts: Vec::new(),
                        block_id: "test-block:item:0".into(),
                        item: Value::Null,
                        complete: true,
                    },
                    Some("late".into()),
                    2,
                )],
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
            payload: Value::Null.into(),
        };
        let empty_credentials = || RunEvent::CredentialMappingsCreated {
            discoveries: Vec::new(),
            omitted_discoveries: 0,
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
                            block_id: "test-block:item:0".into(),
                            item: Value::Null,
                            complete: true,
                        },
                        None,
                        2,
                    ),
                ],
            )
            .await?;
        assert_eq!(first.len(), 1, "filtered events must not persist");

        let skipped = store
            .persist_run_events(
                "interleaved",
                "interleaved",
                &[(wire(), None, 2), (empty_credentials(), None, 2)],
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
                            retention_detached: false,
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
                            block_id: "test-block:item:0".into(),
                            item: Value::Null,
                            complete: true,
                        },
                        None,
                        4,
                    ),
                ],
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
                    block_id: "test-block:item:0".into(),
                    item: Value::Null,
                    complete: true,
                },
                None,
                5,
            ),
            (
                RunEvent::ModelTurnStarted {
                    retention_detached: false,
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
                .persist_run_events("interleaved", "interleaved", &failing)
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
            ObservationStore::Sqlite(pool, _) => sqlx::query_scalar(
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

        // 生产者时间可以与入队顺序不同：保留原事件时间和序号，但活跃时间取最大值。
        let reordered = store
            .persist_run_events(
                "interleaved",
                "interleaved",
                &[
                    (
                        RunEvent::ObservationGap {
                            reason: "newer".into(),
                        },
                        None,
                        10,
                    ),
                    (
                        RunEvent::ObservationGap {
                            reason: "older".into(),
                        },
                        None,
                        9,
                    ),
                ],
            )
            .await?;
        assert_eq!(
            (reordered[0].occurred_at, reordered[1].occurred_at),
            (10, 9)
        );
        assert!(reordered[0].sequence < reordered[1].sequence);
        let sql = "SELECT i.last_active_at,r.last_active_at FROM interaction_observations i JOIN inference_run_observations r ON r.interaction_id=i.id WHERE r.id='interleaved'";
        let activity: (i64, i64) = match store {
            ObservationStore::Sqlite(pool, _) => sqlx::query_as(sql).fetch_one(pool).await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_as(sql).fetch_one(pool).await?,
        };
        assert_eq!(activity, (10, 10), "late events cannot hide newer activity");

        admit_waiting_scenario_run(
            store,
            "interleaved",
            "delayed-admission",
            Some("interleaved"),
        )
        .await?;
        let activity: (i64, i64) = match store {
            ObservationStore::Sqlite(pool, _) => sqlx::query_as(sql).fetch_one(pool).await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_as(sql).fetch_one(pool).await?,
        };
        assert_eq!(
            activity,
            (10, 10),
            "late admission cannot move interaction activity backwards"
        );
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
                let store = ObservationStore::Postgres(
                    pool.clone(),
                    Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
                );
                interleaved_batch_scenario(&store).await?;
                pending_tools_replacement_scenario(&store).await
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
    async fn replay_lists_client_visible_content_across_model_turns() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(DebugTraceIndex::empty(std::path::Path::new("."))),
        );
        admit_tool_run(&store, "turns", None, "alice").await?;
        let turn = |id: &str| RunEvent::ModelTurnStarted {
            retention_detached: false,
            model_turn_id: id.into(),
            route_id: "route".into(),
            model_display_name: None,
            estimated_input_tokens: None,
        };
        let text = |text: &str| RunEvent::ClientVisibleContent {
            text: text.into(),
            parts: Vec::new(),
            block_id: "test-block:item:0".into(),
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
            store.persist_run_events("turns", "turns", &events).await?;
        }
        let visible: Vec<String> = store
            .replay(0)
            .await?
            .into_iter()
            .filter(|event| event.kind == "client_visible_content")
            .map(|event| {
                event.payload["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(visible, ["first", "second", "third"]);
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
            .persist_run_event("root", "root", &handoff(), 2)
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
                serde_json::json!({"tool_id":"call", "content":{"result":"original"}, "is_error":false})
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
            .persist_run_event("child", "child", &handoff(), 2)
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
        );
        admit_tool_run(&store, "batch-race", None, "alice").await?;
        let mut writer = pool.acquire_write().await?;
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
                            block_id: "race-block:item:0".into(),
                            item: Value::Null,
                            complete: true,
                        },
                        None,
                        3,
                    )],
                )
                .await
        });
        started_rx.await?;
        let pending = tokio::time::timeout(Duration::from_millis(250), &mut batch).await;
        writer_tx.commit().await?;
        writer.finish().await?;
        assert!(
            pending.is_err(),
            "event batch must wait rather than fail a WAL snapshot upgrade: {pending:?}"
        );
        let committed = batch.await??;
        assert_eq!(committed[0].kind, "client_visible_content");
        let visible: Vec<String> = store
            .replay(0)
            .await?
            .into_iter()
            .filter(|event| event.kind == "client_visible_content")
            .map(|event| {
                event.payload["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(visible, ["preserved output"]);
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
                follow_ups: &[],
                now: 1,
            })
            .await?;

        let mut writer = pool.acquire_write().await?;
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
                    follow_ups: &[],
                    now: 2,
                })
                .await
        });
        started_rx.await?;
        let pending = tokio::time::timeout(Duration::from_millis(250), &mut admission).await;
        writer_tx.commit().await?;
        writer.finish().await?;
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
        assert!(
            parent
                .events
                .iter()
                .any(|event| event.kind == "run_state_changed")
        );
        let superseded_by: Option<String> = match &store {
            ObservationStore::Sqlite(pool, _) => {
                sqlx::query_scalar(
                    "SELECT superseded_by FROM inference_run_observations WHERE id='parent'",
                )
                .fetch_one(pool)
                .await?
            }
            ObservationStore::Postgres(pool, _) => {
                sqlx::query_scalar(
                    "SELECT superseded_by FROM inference_run_observations WHERE id='parent'",
                )
                .fetch_one(pool)
                .await?
            }
        };
        assert_eq!(superseded_by.as_deref(), Some("child"));
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
        );
        admit_waiting_scenario_run(&store, "swept", "leafwait", None).await?;
        finish_scenario_run(&store, "swept", "leafwait", "waiting_client").await?;
        admit_waiting_scenario_run(&store, "swept", "supwait", None).await?;
        finish_scenario_run(&store, "swept", "supwait", "waiting_client").await?;
        // 修复前的遗留形态：等待 Run 已有续接子节点但状态仍停在 waiting_client。
        sqlx::query("INSERT INTO inference_run_observations (id,interaction_id,parent_run_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at) VALUES ('supchild','swept','supwait','responses','route','running',0,1,1)")
            .execute(&pool).await?;
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
                    retention_detached: false,
                    model_turn_id: "gap-turn".into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                2,
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
            .finalize_activity("gap", "gap", 2_000_000)
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
