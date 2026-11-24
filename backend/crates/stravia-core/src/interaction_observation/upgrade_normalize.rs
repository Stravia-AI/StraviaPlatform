//! Observation v2 形态升级：载荷拆分为 attrs、块、内容引用，合并 Turn 首尾事件。
//!
//! 升级只重写已存在的行，不插入新事件；`0042` 之前 `payload` 仍为 NOT NULL，
//! 因此事件行的 `attrs` 可以逐批写入并从水位续跑。
use anyhow::Context;
use serde_json::Value;
use sqlx::Row;
use std::collections::{HashMap, HashSet};

use crate::content_store::{Content, ContentDigest};

const BATCH: i64 = 512;
const MERGE_BATCH: usize = 200;
const PHASE: &str = "observation_data";
const LABEL: &str = "Normalizing observation history";

pub(crate) async fn normalize_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    sqlite::normalize(connection).await
}

pub(crate) async fn normalize_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    postgres::normalize(connection).await
}

fn attrs_of(payload: &Value) -> Value {
    let Some(object) = payload.as_object() else {
        return Value::Null;
    };
    let mut attrs = object.clone();
    attrs.remove("kind");
    Value::Object(attrs)
}

fn encoded_attrs(payload: &Value) -> anyhow::Result<Option<Vec<u8>>> {
    match attrs_of(payload) {
        Value::Object(object) if object.is_empty() => Ok(None),
        Value::Null => Ok(None),
        attrs => Ok(Some(crate::storage_codec::encode(
            serde_json::to_vec(&attrs)?.into(),
        )?)),
    }
}

fn decode_attrs(bytes: Option<Vec<u8>>) -> anyhow::Result<Value> {
    match bytes {
        Some(bytes) => serde_json::from_slice(&crate::storage_codec::decode(&bytes)?)
            .context("observation attrs are not JSON"),
        None => Ok(Value::Null),
    }
}

struct SplitRow {
    sequence: i64,
    interaction_id: Option<String>,
    run_id: Option<String>,
    payload: Value,
    /// 块行的 item 序号；非块事件与无法解析的历史块标识为 0（后者已计数告警）。
    item_ordinal: i64,
}

/// 有损迁移的历史行计数。
///
/// 已损坏的历史诊断不得阻断整库升级（既有升级测试要求无关升级不因历史诊断解码失败而失败），
/// 因此事件行保留：无法解码的载荷按空对象迁移，无法解析 item 序号的块标识按 0 迁移；
/// 这是有损迁移，逐行与汇总都留下告警。写入路径对同样的块标识是硬错误，这里只放过存量。
#[derive(Default)]
struct LossyMigration {
    unreadable: u64,
    first_unreadable: Option<i64>,
    malformed_block_ids: u64,
    first_malformed_block_id: Option<i64>,
}

impl LossyMigration {
    /// 解码旧载荷；失败时按空对象迁移并计数。
    fn decode(&mut self, sequence: i64, bytes: &[u8]) -> Value {
        match crate::storage_codec::decode(bytes)
            .and_then(|raw| serde_json::from_slice(&raw).map_err(anyhow::Error::from))
        {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(sequence, %error, "observation payload unreadable during normalization; migrated as empty");
                self.unreadable += 1;
                self.first_unreadable.get_or_insert(sequence);
                Value::Object(serde_json::Map::new())
            }
        }
    }

    /// 把一行旧事件读成拆分输入；块事件的 item 序号在这里解析并计数。
    fn split_row(
        &mut self,
        kind: &str,
        sequence: i64,
        interaction_id: Option<String>,
        run_id: Option<String>,
        payload: &[u8],
    ) -> SplitRow {
        let payload = self.decode(sequence, payload);
        let item_ordinal = if matches!(kind, "model_thinking" | "client_visible_content") {
            self.item_ordinal(sequence, payload["block_id"].as_str())
        } else {
            0
        };
        SplitRow {
            sequence,
            interaction_id,
            run_id,
            payload,
            item_ordinal,
        }
    }

    fn item_ordinal(&mut self, sequence: i64, block_id: Option<&str>) -> i64 {
        let parsed = block_id
            .context("block id is missing")
            .and_then(super::types::parse_item_block_id)
            .and_then(|ordinal| Ok(i64::try_from(ordinal)?));
        match parsed {
            Ok(ordinal) => ordinal,
            Err(error) => {
                tracing::warn!(sequence, %error, "observation block id malformed during normalization; migrated as item 0");
                self.malformed_block_ids += 1;
                self.first_malformed_block_id.get_or_insert(sequence);
                0
            }
        }
    }

    fn report(&self) {
        if let Some(first_sequence) = self.first_unreadable {
            tracing::warn!(
                unreadable = self.unreadable,
                first_sequence,
                "observation normalization migrated unreadable payloads as empty objects"
            );
        }
        if let Some(first_sequence) = self.first_malformed_block_id {
            tracing::warn!(
                malformed = self.malformed_block_ids,
                first_sequence,
                "observation normalization migrated malformed block ids as item 0"
            );
        }
    }
}

struct RawEventRow {
    sequence: i64,
    interaction_id: Option<String>,
    run_id: Option<String>,
    kind: String,
    payload: Vec<u8>,
}

struct PreparedBlock {
    sequence: i64,
    interaction_id: Option<String>,
    run_id: Option<String>,
    model_turn_id: Option<String>,
    attempt_id: Option<String>,
    item_ordinal: i64,
    channel: &'static str,
    complete: bool,
    envelope: Option<Vec<u8>>,
}

struct PendingEventContent {
    sequence: i64,
    ordinal: i64,
    interaction_id: String,
    digest: ContentDigest,
}

struct PendingToolResult {
    sequence: i64,
    tool_id: String,
    is_error: bool,
    interaction_id: String,
    digest: ContentDigest,
}

struct RunAdmittedUpdate {
    run_id: String,
    inferred_retry: bool,
    grouping_reason: Option<String>,
    diagnostic_source_run_id: Option<String>,
    has_new_user: bool,
}

struct PreparedSplitBatch {
    last_sequence: i64,
    row_count: u64,
    blocks: Vec<PreparedBlock>,
    event_contents: Vec<PendingEventContent>,
    tool_results: Vec<PendingToolResult>,
    /// `(interaction_id, Content)`，随后按交互对应 principal 去重后批量 intern。
    contents: Vec<(String, Content)>,
    needed_interactions: Vec<String>,
    run_admitted: Vec<RunAdmittedUpdate>,
    run_superseded: Vec<(String, String)>,
    event_updates: Vec<(i64, String, Option<Vec<u8>>)>,
}

fn prepare_split_batch(
    rows: Vec<RawEventRow>,
    mut lossy: LossyMigration,
) -> anyhow::Result<(PreparedSplitBatch, LossyMigration)> {
    let mut last_sequence = 0i64;
    let row_count = rows.len() as u64;
    let mut blocks = Vec::new();
    let mut event_contents = Vec::new();
    let mut tool_results = Vec::new();
    let mut contents = Vec::new();
    let mut needed_interactions_set = HashSet::new();
    let mut run_admitted_by_id: HashMap<String, usize> = HashMap::new();
    let mut run_admitted: Vec<RunAdmittedUpdate> = Vec::new();
    let mut run_superseded_by_id: HashMap<String, usize> = HashMap::new();
    let mut run_superseded: Vec<(String, String)> = Vec::new();
    let mut event_updates = Vec::with_capacity(rows.len());

    for raw in rows {
        let row = lossy.split_row(
            &raw.kind,
            raw.sequence,
            raw.interaction_id,
            raw.run_id,
            &raw.payload,
        );
        last_sequence = row.sequence;
        let kind = raw.kind.as_str();
        let mut attrs = encoded_attrs(&row.payload)?;
        let mut new_kind = raw.kind.clone();
        match kind {
            "model_thinking" | "client_visible_content" => {
                let text = row.payload["text"].as_str().unwrap_or_default();
                let item = &row.payload["item"];
                let item = (!item.is_null()).then_some(item);
                let split = super::blocks::split(item, text);
                let interaction_id = row
                    .interaction_id
                    .as_deref()
                    .context("content reference requires interaction")?
                    .to_owned();
                needed_interactions_set.insert(interaction_id.clone());
                for (ordinal, unit) in split.units.into_iter().enumerate() {
                    let digest = unit.digest();
                    contents.push((interaction_id.clone(), unit));
                    event_contents.push(PendingEventContent {
                        sequence: row.sequence,
                        ordinal: i64::try_from(ordinal)?,
                        interaction_id: interaction_id.clone(),
                        digest,
                    });
                }
                let envelope = split
                    .envelope
                    .map(|value| crate::storage_codec::encode(serde_json::to_vec(&value)?.into()))
                    .transpose()?;
                let (model_turn_id, attempt_id) = if kind == "model_thinking" {
                    (
                        row.payload["model_turn_id"].as_str().map(str::to_owned),
                        row.payload["attempt_id"].as_str().map(str::to_owned),
                    )
                } else {
                    (None, None)
                };
                blocks.push(PreparedBlock {
                    sequence: row.sequence,
                    interaction_id: row.interaction_id,
                    run_id: row.run_id,
                    model_turn_id,
                    attempt_id,
                    item_ordinal: row.item_ordinal,
                    channel: if kind == "model_thinking" {
                        "thinking"
                    } else {
                        "visible"
                    },
                    complete: row.payload["complete"].as_bool().unwrap_or(false),
                    envelope,
                });
                attrs = None;
            }
            "client_tool_result" => {
                let content = &row.payload["content"];
                if !content.is_null() {
                    let interaction_id = row
                        .interaction_id
                        .as_deref()
                        .context("content reference requires interaction")?
                        .to_owned();
                    needed_interactions_set.insert(interaction_id.clone());
                    let unit = super::content_refs::value_content(content);
                    let digest = unit.digest();
                    contents.push((interaction_id.clone(), unit));
                    tool_results.push(PendingToolResult {
                        sequence: row.sequence,
                        tool_id: row.payload["tool_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                        is_error: row.payload["is_error"].as_bool().unwrap_or(false),
                        interaction_id,
                        digest,
                    });
                }
                attrs = None;
            }
            "input_preview_recorded" => {
                new_kind = "user_input_recorded".into();
                if let Some(text) = row.payload["text"].as_str() {
                    let interaction_id = row
                        .interaction_id
                        .as_deref()
                        .context("content reference requires interaction")?
                        .to_owned();
                    needed_interactions_set.insert(interaction_id.clone());
                    let unit = super::content_refs::text_content(text);
                    let digest = unit.digest();
                    contents.push((interaction_id.clone(), unit));
                    event_contents.push(PendingEventContent {
                        sequence: row.sequence,
                        ordinal: 0,
                        interaction_id,
                        digest,
                    });
                }
                attrs = None;
            }
            "run_admitted" => {
                if let Some(run_id) = row.run_id {
                    let update = RunAdmittedUpdate {
                        run_id: run_id.clone(),
                        inferred_retry: row.payload["inferred_retry"].as_bool().unwrap_or(false),
                        grouping_reason: row.payload["grouping_reason"]
                            .as_str()
                            .map(str::to_owned),
                        diagnostic_source_run_id: row.payload["diagnostic_source_run_id"]
                            .as_str()
                            .map(str::to_owned),
                        has_new_user: row.payload["has_new_user"].as_bool().unwrap_or(false),
                    };
                    if let Some(&slot) = run_admitted_by_id.get(&run_id) {
                        run_admitted[slot] = update;
                    } else {
                        run_admitted_by_id.insert(run_id, run_admitted.len());
                        run_admitted.push(update);
                    }
                }
                attrs = None;
            }
            "run_state_changed" => {
                if let (Some(run_id), Some(superseded)) =
                    (row.run_id, row.payload["superseded_by"].as_str())
                {
                    let entry = (run_id.clone(), superseded.to_owned());
                    if let Some(&slot) = run_superseded_by_id.get(&run_id) {
                        run_superseded[slot] = entry;
                    } else {
                        run_superseded_by_id.insert(run_id, run_superseded.len());
                        run_superseded.push(entry);
                    }
                }
                attrs = None;
            }
            "request_failed" => attrs = None,
            _ => {}
        }
        event_updates.push((row.sequence, new_kind, attrs));
    }

    let mut needed_interactions: Vec<String> = needed_interactions_set.into_iter().collect();
    needed_interactions.sort_unstable();
    Ok((
        PreparedSplitBatch {
            last_sequence,
            row_count,
            blocks,
            event_contents,
            tool_results,
            contents,
            needed_interactions,
            run_admitted,
            run_superseded,
            event_updates,
        },
        lossy,
    ))
}

type TurnKey = (Option<String>, String);

struct AttemptRow {
    sequence: i64,
    at: i64,
    run_id: Option<String>,
    attrs: Value,
}

/// 每个 Turn 只把「先于 Turn 结束行的最后一个 attempt 结束」并入结束行；更早的 attempt
/// 结束（被 failover 取代）保持独立行，信息不丢。返回 `(Turn 结束行 sequence,
/// 被并入的 attempt 行 sequence, 合并后的 attrs)`。
fn merge_last_attempts(
    finishes: &mut HashMap<TurnKey, (i64, Value)>,
    attempts: Vec<AttemptRow>,
) -> Vec<(i64, i64, Value)> {
    // 输入按 sequence 升序，后到的覆盖先到的，剩下的就是每个 Turn 最后一个。
    let mut last: HashMap<TurnKey, AttemptRow> = HashMap::new();
    for attempt in attempts {
        let Some(model_turn_id) = attempt.attrs["model_turn_id"].as_str() else {
            continue;
        };
        let key = (attempt.run_id.clone(), model_turn_id.to_owned());
        let Some((turn_sequence, _)) = finishes.get(&key) else {
            continue;
        };
        if *turn_sequence < attempt.sequence {
            continue;
        }
        last.insert(key, attempt);
    }
    let mut merged = Vec::with_capacity(last.len());
    for (key, attempt) in last {
        let (turn_sequence, turn_attrs) = finishes.get_mut(&key).expect("finish row exists");
        let Value::Object(object) = turn_attrs else {
            continue;
        };
        if let Some(usage) = attempt.attrs.get("usage") {
            object.insert("usage".into(), usage.clone());
        }
        object.insert("attempt".into(), attempt.attrs);
        object.insert("attempt_at".into(), Value::from(attempt.at));
        merged.push((*turn_sequence, attempt.sequence, turn_attrs.clone()));
    }
    merged
}

struct MergedPair {
    turn_sequence: i64,
    attempt_sequence: i64,
    attrs: Option<Vec<u8>>,
}

fn prepare_start_merges(
    turns: Vec<(i64, Option<String>, Option<Vec<u8>>)>,
    attempts: Vec<(i64, i64, Option<String>, Option<Vec<u8>>)>,
) -> anyhow::Result<Vec<MergedPair>> {
    let mut starts: HashMap<TurnKey, (i64, Value)> = HashMap::with_capacity(turns.len());
    for (sequence, run_id, raw_attrs) in turns {
        let attrs = decode_attrs(raw_attrs)?;
        // 中断重试时已合并过首个 attempt 的 Turn 不重复合并后续重试 attempt。
        if attrs.get("attempt").is_some() {
            continue;
        }
        if let Some(model_turn_id) = attrs["model_turn_id"].as_str() {
            starts.insert((run_id, model_turn_id.to_owned()), (sequence, attrs));
        }
    }
    let mut merged = Vec::new();
    for (sequence, attempt_at, run_id, raw_attrs) in attempts {
        let attrs = decode_attrs(raw_attrs)?;
        let Some(model_turn_id) = attrs["model_turn_id"].as_str() else {
            continue;
        };
        let Some((turn_sequence, turn_attrs)) = starts.get_mut(&(run_id, model_turn_id.to_owned()))
        else {
            continue;
        };
        if *turn_sequence > sequence {
            continue;
        }
        let Value::Object(object) = turn_attrs else {
            continue;
        };
        object.insert("attempt".into(), attrs);
        object.insert("attempt_at".into(), Value::from(attempt_at));
        let encoded = encoded_attrs(turn_attrs)?;
        merged.push(MergedPair {
            turn_sequence: *turn_sequence,
            attempt_sequence: sequence,
            attrs: encoded,
        });
        *turn_sequence = i64::MAX;
    }
    Ok(merged)
}

fn prepare_finish_merges(
    turns: Vec<(i64, Option<String>, Option<Vec<u8>>)>,
    attempts: Vec<(i64, i64, Option<String>, Option<Vec<u8>>)>,
) -> anyhow::Result<Vec<MergedPair>> {
    let mut finishes: HashMap<TurnKey, (i64, Value)> = HashMap::with_capacity(turns.len());
    for (sequence, run_id, raw_attrs) in turns {
        let attrs = decode_attrs(raw_attrs)?;
        // 中断重试时已合并过末尾 attempt 的 Turn 跳过，避免误并更早的 failover attempt。
        if attrs.get("attempt").is_some() {
            continue;
        }
        if let Some(model_turn_id) = attrs["model_turn_id"].as_str() {
            finishes.insert((run_id, model_turn_id.to_owned()), (sequence, attrs));
        }
    }
    let mut candidates = Vec::with_capacity(attempts.len());
    for (sequence, at, run_id, raw_attrs) in attempts {
        candidates.push(AttemptRow {
            sequence,
            at,
            run_id,
            attrs: decode_attrs(raw_attrs)?,
        });
    }
    let mut merged = Vec::new();
    for (turn_sequence, attempt_sequence, attrs) in merge_last_attempts(&mut finishes, candidates) {
        merged.push(MergedPair {
            turn_sequence,
            attempt_sequence,
            attrs: encoded_attrs(&attrs)?,
        });
    }
    Ok(merged)
}

macro_rules! normalize_backend {
    ($module:ident, $db:ty, $conn:ty, $intern:ident) => {
        mod $module {
            use super::*;

            pub(super) async fn normalize(connection: &mut $conn) -> anyhow::Result<()> {
                let stage: i64 = sqlx::query_scalar(
                    "SELECT stage FROM observation_normalization_upgrade WHERE phase=1",
                )
                .fetch_one(&mut *connection)
                .await?;
                if stage >= 4 {
                    return Ok(());
                }
                let (total_events, total_starts, total_finishes): (i64, i64, i64) = sqlx::query_as(
                    "SELECT COUNT(*), \
                     COUNT(CASE WHEN kind='model_turn_started' THEN 1 END), \
                     COUNT(CASE WHEN kind='model_turn_finished' THEN 1 END) \
                     FROM observation_events",
                )
                .fetch_one(&mut *connection)
                .await?;
                let total_events = total_events.max(0) as u64;
                let total_starts = total_starts.max(0) as u64;
                let total_finishes = total_finishes.max(0) as u64;
                let total = total_events
                    .saturating_add(total_starts)
                    .saturating_add(total_finishes)
                    .saturating_add(1);

                let mut completed: u64 = match stage {
                    0 => {
                        let last: i64 = sqlx::query_scalar(
                            "SELECT last_sequence FROM observation_normalization_upgrade WHERE phase=1",
                        )
                        .fetch_one(&mut *connection)
                        .await?;
                        if last > 0 {
                            let done: i64 = sqlx::query_scalar(
                                "SELECT COUNT(*) FROM observation_events WHERE sequence <= $1",
                            )
                            .bind(last)
                            .fetch_one(&mut *connection)
                            .await?;
                            done.max(0) as u64
                        } else {
                            0
                        }
                    }
                    1 => total_events,
                    2 => total_events.saturating_add(total_starts),
                    _ => total_events
                        .saturating_add(total_starts)
                        .saturating_add(total_finishes),
                };
                if total_events > 0 {
                    crate::startup_progress::report(PHASE, LABEL, completed.min(total), Some(total));
                }

                if stage < 1 {
                    split_events(connection, &mut completed, total, total_events).await?;
                    set_stage(connection, 1).await?;
                    completed = completed.max(total_events);
                    if total_events > 0 {
                        crate::startup_progress::report(
                            PHASE,
                            LABEL,
                            completed.min(total),
                            Some(total),
                        );
                    }
                }
                if stage < 2 {
                    let stage_cap = total_events.saturating_add(total_starts);
                    merge_turn_start(connection, &mut completed, stage_cap, total, total_events > 0)
                        .await?;
                    set_stage(connection, 2).await?;
                    completed = completed.max(stage_cap);
                    if total_events > 0 {
                        crate::startup_progress::report(
                            PHASE,
                            LABEL,
                            completed.min(total),
                            Some(total),
                        );
                    }
                }
                if stage < 3 {
                    let stage_cap = total_events
                        .saturating_add(total_starts)
                        .saturating_add(total_finishes);
                    merge_turn_finish(
                        connection,
                        &mut completed,
                        stage_cap,
                        total,
                        total_events > 0,
                    )
                    .await?;
                    set_stage(connection, 3).await?;
                    completed = completed.max(stage_cap);
                    if total_events > 0 {
                        crate::startup_progress::report(
                            PHASE,
                            LABEL,
                            completed.min(total),
                            Some(total),
                        );
                    }
                }
                if stage < 4 {
                    backfill_input_sequence(connection).await?;
                    set_stage(connection, 4).await?;
                    if total_events > 0 {
                        crate::startup_progress::report(PHASE, LABEL, total, Some(total));
                    }
                }
                Ok(())
            }

            async fn set_stage(connection: &mut $conn, stage: i64) -> anyhow::Result<()> {
                sqlx::query(
                    "UPDATE observation_normalization_upgrade SET stage=$1,last_sequence=0 WHERE phase=1",
                )
                .bind(stage)
                .execute(&mut *connection)
                .await?;
                Ok(())
            }

            async fn load_principals(
                connection: &mut $conn,
                needed: &[String],
                principals: &mut HashMap<String, String>,
            ) -> anyhow::Result<()> {
                let missing: Vec<&str> = needed
                    .iter()
                    .map(String::as_str)
                    .filter(|id| !principals.contains_key(*id))
                    .collect();
                for chunk in missing.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, principal FROM interaction_observations WHERE id IN (",
                    );
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(*id);
                    }
                    list.push_unseparated(")");
                    let rows: Vec<(String, String)> =
                        query.build_query_as().fetch_all(&mut *connection).await?;
                    for (id, principal) in rows {
                        principals.insert(id, principal);
                    }
                }
                for id in needed {
                    if !principals.contains_key(id) {
                        anyhow::bail!("missing interaction observation: {id}");
                    }
                }
                Ok(())
            }

            async fn apply_merged_pairs(
                connection: &mut $conn,
                pairs: Vec<MergedPair>,
                completed: &mut u64,
                stage_cap: u64,
                total: u64,
                report_progress: bool,
            ) -> anyhow::Result<()> {
                for chunk in pairs.chunks(MERGE_BATCH) {
                    let mut tx = connection.begin().await?;
                    let mut update = sqlx::QueryBuilder::<$db>::new("WITH v(seq,attrs) AS (");
                    update.push_values(chunk, |mut row, pair| {
                        row.push_bind(pair.turn_sequence).push_bind(&pair.attrs);
                    });
                    update.push(
                        ") UPDATE observation_events SET attrs=v.attrs \
                         FROM v WHERE observation_events.sequence=v.seq",
                    );
                    update.build().execute(&mut *tx).await?;

                    let mut delete = sqlx::QueryBuilder::<$db>::new(
                        "DELETE FROM observation_events WHERE sequence IN (",
                    );
                    let mut list = delete.separated(",");
                    for pair in chunk {
                        list.push_bind(pair.attempt_sequence);
                    }
                    list.push_unseparated(")");
                    delete.build().execute(&mut *tx).await?;
                    tx.commit().await?;
                    *completed = completed.saturating_add(chunk.len() as u64).min(stage_cap);
                    if report_progress {
                        crate::startup_progress::report(
                            PHASE,
                            LABEL,
                            (*completed).min(total),
                            Some(total),
                        );
                    }
                }
                Ok(())
            }

            /// 第一段：事件载荷拆分为 attrs / 块 / 内容引用，Run 列就地回填。
            async fn split_events(
                connection: &mut $conn,
                completed: &mut u64,
                total: u64,
                total_events: u64,
            ) -> anyhow::Result<()> {
                let mut last: i64 = sqlx::query_scalar(
                    "SELECT last_sequence FROM observation_normalization_upgrade WHERE phase=1",
                )
                .fetch_one(&mut *connection)
                .await?;
                let mut principals = HashMap::new();
                let mut unreadable = LossyMigration::default();
                loop {
                    let rows = sqlx::query(
                        "SELECT sequence,interaction_id,run_id,kind,payload \
                         FROM observation_events WHERE sequence>$1 ORDER BY sequence LIMIT $2",
                    )
                    .bind(last)
                    .bind(BATCH)
                    .fetch_all(&mut *connection)
                    .await?;
                    if rows.is_empty() {
                        break;
                    }
                    let mut raw_rows = Vec::with_capacity(rows.len());
                    for row in rows {
                        raw_rows.push(RawEventRow {
                            sequence: row.try_get("sequence")?,
                            interaction_id: row.try_get("interaction_id")?,
                            run_id: row.try_get("run_id")?,
                            kind: row.try_get("kind")?,
                            payload: row.try_get("payload")?,
                        });
                    }
                    let lossy_in = std::mem::take(&mut unreadable);
                    let (prepared, lossy_out) = tokio::task::spawn_blocking(move || {
                        prepare_split_batch(raw_rows, lossy_in)
                    })
                    .await
                    .context("observation normalization worker failed")??;
                    unreadable = lossy_out;

                    if !prepared.needed_interactions.is_empty() {
                        load_principals(
                            connection,
                            &prepared.needed_interactions,
                            &mut principals,
                        )
                        .await?;
                    }

                    // 按 principal 去重整批正文，每批每个 principal 只 intern 一次。
                    let mut principal_order: Vec<String> = Vec::new();
                    let mut by_principal: HashMap<String, Vec<Content>> = HashMap::new();
                    let mut seen_by_principal: HashMap<String, HashSet<ContentDigest>> =
                        HashMap::new();
                    for (interaction_id, unit) in prepared.contents {
                        let principal = principals
                            .get(&interaction_id)
                            .context("missing interaction principal")?;
                        let seen = seen_by_principal.entry(principal.clone()).or_default();
                        let bucket = by_principal.entry(principal.clone()).or_insert_with(|| {
                            principal_order.push(principal.clone());
                            Vec::new()
                        });
                        if seen.insert(unit.digest()) {
                            bucket.push(unit);
                        }
                    }

                    let mut tx = connection.begin().await?;
                    let mut interned: HashMap<(String, ContentDigest), i64> = HashMap::new();
                    for principal in principal_order {
                        let Some(mut items) = by_principal.remove(&principal) else {
                            continue;
                        };
                        if items.is_empty() {
                            continue;
                        }
                        let digests: Vec<ContentDigest> =
                            items.iter().map(Content::digest).collect();
                        let ids = super::super::content_refs::$intern(&mut tx, &principal, &mut items).await?;
                        for (digest, id) in digests.into_iter().zip(ids) {
                            interned.insert((principal.clone(), digest), id);
                        }
                    }

                    for chunk in prepared.blocks.chunks(100) {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "INSERT INTO observation_blocks\
                             (sequence,interaction_id,run_id,model_turn_id,attempt_id,\
                              item_ordinal,channel,complete,envelope) ",
                        );
                        query.push_values(chunk, |mut row, block| {
                            row.push_bind(block.sequence)
                                .push_bind(&block.interaction_id)
                                .push_bind(&block.run_id)
                                .push_bind(&block.model_turn_id)
                                .push_bind(&block.attempt_id)
                                .push_bind(block.item_ordinal)
                                .push_bind(block.channel)
                                .push_bind(block.complete)
                                .push_bind(&block.envelope);
                        });
                        query.push(
                            " ON CONFLICT(sequence) DO UPDATE SET envelope=EXCLUDED.envelope",
                        );
                        query.build().execute(&mut *tx).await?;
                    }

                    for chunk in prepared.event_contents.chunks(200) {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "INSERT INTO observation_event_contents(sequence,ordinal,content_id) ",
                        );
                        let mut resolved = Vec::with_capacity(chunk.len());
                        for item in chunk {
                            let principal = principals
                                .get(&item.interaction_id)
                                .context("missing interaction principal")?;
                            let content_id = *interned
                                .get(&(principal.clone(), item.digest))
                                .context("missing interned observation content id")?;
                            resolved.push((item.sequence, item.ordinal, content_id));
                        }
                        query.push_values(resolved, |mut row, (sequence, ordinal, content_id)| {
                            row.push_bind(sequence)
                                .push_bind(ordinal)
                                .push_bind(content_id);
                        });
                        query.push(" ON CONFLICT(sequence,ordinal) DO NOTHING");
                        query.build().execute(&mut *tx).await?;
                    }

                    for chunk in prepared.tool_results.chunks(150) {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "INSERT INTO observation_tool_results\
                             (sequence,ordinal,tool_id,is_error,content_id) ",
                        );
                        let mut resolved = Vec::with_capacity(chunk.len());
                        for item in chunk {
                            let principal = principals
                                .get(&item.interaction_id)
                                .context("missing interaction principal")?;
                            let content_id = *interned
                                .get(&(principal.clone(), item.digest))
                                .context("missing interned observation content id")?;
                            resolved.push((
                                item.sequence,
                                item.tool_id.as_str(),
                                item.is_error,
                                content_id,
                            ));
                        }
                        query.push_values(
                            resolved,
                            |mut row, (sequence, tool_id, is_error, content_id)| {
                                row.push_bind(sequence)
                                    .push_bind(0_i64)
                                    .push_bind(tool_id)
                                    .push_bind(is_error)
                                    .push_bind(content_id);
                            },
                        );
                        query.push(" ON CONFLICT(sequence,ordinal) DO NOTHING");
                        query.build().execute(&mut *tx).await?;
                    }

                    for chunk in prepared.run_admitted.chunks(100) {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "WITH v(id,inferred_retry,grouping_reason,diagnostic_source_run_id,has_new_user) AS (",
                        );
                        query.push_values(chunk, |mut row, item| {
                            row.push_bind(&item.run_id)
                                .push_bind(item.inferred_retry)
                                .push_bind(&item.grouping_reason)
                                .push_bind(&item.diagnostic_source_run_id)
                                .push_bind(item.has_new_user);
                        });
                        query.push(
                            ") UPDATE inference_run_observations SET \
                             inferred_retry=v.inferred_retry,\
                             grouping_reason=v.grouping_reason,\
                             diagnostic_source_run_id=v.diagnostic_source_run_id,\
                             has_new_user=v.has_new_user \
                             FROM v WHERE inference_run_observations.id=v.id",
                        );
                        query.build().execute(&mut *tx).await?;
                    }

                    for chunk in prepared.run_superseded.chunks(200) {
                        let mut query =
                            sqlx::QueryBuilder::<$db>::new("WITH v(id,superseded_by) AS (");
                        query.push_values(chunk, |mut row, (run_id, superseded)| {
                            row.push_bind(run_id).push_bind(superseded);
                        });
                        query.push(
                            ") UPDATE inference_run_observations SET superseded_by=v.superseded_by \
                             FROM v WHERE inference_run_observations.id=v.id",
                        );
                        query.build().execute(&mut *tx).await?;
                    }

                    for chunk in prepared.event_updates.chunks(150) {
                        let mut query =
                            sqlx::QueryBuilder::<$db>::new("WITH v(seq,kind,attrs) AS (");
                        query.push_values(chunk, |mut row, (sequence, kind, attrs)| {
                            row.push_bind(*sequence).push_bind(kind).push_bind(attrs);
                        });
                        query.push(
                            ") UPDATE observation_events SET kind=v.kind,attrs=v.attrs \
                             FROM v WHERE observation_events.sequence=v.seq",
                        );
                        query.build().execute(&mut *tx).await?;
                    }

                    last = prepared.last_sequence;
                    sqlx::query(
                        "UPDATE observation_normalization_upgrade SET last_sequence=$1 WHERE phase=1",
                    )
                    .bind(last)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    *completed = completed
                        .saturating_add(prepared.row_count)
                        .min(total_events);
                    if total_events > 0 {
                        crate::startup_progress::report(
                            PHASE,
                            LABEL,
                            (*completed).min(total),
                            Some(total),
                        );
                    }
                }
                unreadable.report();
                Ok(())
            }

            /// 第二段：首个 attempt 开始并入 Turn 开始行；其余 attempt 保持独立。
            async fn merge_turn_start(
                connection: &mut $conn,
                completed: &mut u64,
                stage_cap: u64,
                total: u64,
                report_progress: bool,
            ) -> anyhow::Result<()> {
                let turns: Vec<(i64, Option<String>, Option<Vec<u8>>)> = sqlx::query_as(
                    "SELECT sequence,run_id,attrs FROM observation_events \
                     WHERE kind='model_turn_started' AND attrs IS NOT NULL ORDER BY sequence",
                )
                .fetch_all(&mut *connection)
                .await?;
                if turns.is_empty() {
                    return Ok(());
                }
                let attempts: Vec<(i64, i64, Option<String>, Option<Vec<u8>>)> = sqlx::query_as(
                    "SELECT sequence,occurred_at,run_id,attrs FROM observation_events \
                     WHERE kind='target_attempt_started' AND attrs IS NOT NULL ORDER BY sequence",
                )
                .fetch_all(&mut *connection)
                .await?;
                if attempts.is_empty() {
                    return Ok(());
                }
                let pairs =
                    tokio::task::spawn_blocking(move || prepare_start_merges(turns, attempts))
                        .await
                        .context("observation start merge worker failed")??;
                apply_merged_pairs(
                    connection,
                    pairs,
                    completed,
                    stage_cap,
                    total,
                    report_progress,
                )
                .await
            }

            /// 第三段：最后一个 attempt 的结束与用量并入 Turn 结束行。
            async fn merge_turn_finish(
                connection: &mut $conn,
                completed: &mut u64,
                stage_cap: u64,
                total: u64,
                report_progress: bool,
            ) -> anyhow::Result<()> {
                let turns: Vec<(i64, Option<String>, Option<Vec<u8>>)> = sqlx::query_as(
                    "SELECT sequence,run_id,attrs FROM observation_events \
                     WHERE kind='model_turn_finished' AND attrs IS NOT NULL ORDER BY sequence",
                )
                .fetch_all(&mut *connection)
                .await?;
                if !turns.is_empty() {
                    let attempts: Vec<(i64, i64, Option<String>, Option<Vec<u8>>)> =
                        sqlx::query_as(
                            "SELECT sequence,occurred_at,run_id,attrs FROM observation_events \
                             WHERE kind='target_attempt_finished' AND attrs IS NOT NULL ORDER BY sequence",
                        )
                        .fetch_all(&mut *connection)
                        .await?;
                    if !attempts.is_empty() {
                        let pairs = tokio::task::spawn_blocking(move || {
                            prepare_finish_merges(turns, attempts)
                        })
                        .await
                        .context("observation finish merge worker failed")??;
                        apply_merged_pairs(
                            connection,
                            pairs,
                            completed,
                            stage_cap,
                            total,
                            report_progress,
                        )
                        .await?;
                    }
                }
                let mut tx = connection.begin().await?;
                sqlx::query("DELETE FROM observation_events WHERE kind='usage_confirmed'")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            }

            /// 第四段：根 Run 最新的用户输入事件成为中断恢复输入。
            async fn backfill_input_sequence(connection: &mut $conn) -> anyhow::Result<()> {
                sqlx::query(
                    "UPDATE interaction_observations SET input_sequence=\
                     (SELECT MAX(e.sequence) FROM observation_events e \
                      JOIN inference_run_observations r ON r.id=e.run_id \
                      WHERE r.interaction_id=interaction_observations.id \
                      AND r.id=interaction_observations.root_run_id \
                      AND e.kind='user_input_recorded')",
                )
                .execute(&mut *connection)
                .await?;
                Ok(())
            }
        }
    };
}

normalize_backend!(
    sqlite,
    sqlx::Sqlite,
    sqlx::SqliteConnection<'_>,
    intern_sqlite
);
normalize_backend!(
    postgres,
    sqlx::Postgres,
    sqlx::PgConnection<'_>,
    intern_postgres
);

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(payload: &Value) -> Vec<u8> {
        crate::storage_codec::encode(serde_json::to_vec(payload).unwrap().into()).unwrap()
    }

    /// 存量块事件的 item 序号取自规范块标识；解析不了的存量行仍然迁移（按 item 0），
    /// 但逐行告警并计数，写入路径对同样的标识是硬错误。
    #[test]
    fn malformed_stored_block_ids_migrate_as_item_zero_and_are_counted() {
        let mut lossy = LossyMigration::default();
        let canonical = lossy.split_row(
            "client_visible_content",
            10,
            Some("i".into()),
            Some("r".into()),
            &stored(&serde_json::json!({"block_id": "r:item:7"})),
        );
        assert_eq!(canonical.item_ordinal, 7);
        assert_eq!(lossy.malformed_block_ids, 0);

        for (sequence, payload) in [
            (11, serde_json::json!({"block_id": "plain-block"})),
            (12, serde_json::json!({"text": "no block id"})),
        ] {
            let row = lossy.split_row(
                "model_thinking",
                sequence,
                Some("i".into()),
                Some("r".into()),
                &stored(&payload),
            );
            assert_eq!(row.item_ordinal, 0);
        }
        assert_eq!(lossy.malformed_block_ids, 2);
        assert_eq!(lossy.first_malformed_block_id, Some(11));

        let other = lossy.split_row(
            "run_admitted",
            13,
            Some("i".into()),
            Some("r".into()),
            &stored(&serde_json::json!({"block_id": "plain-block"})),
        );
        assert_eq!(other.item_ordinal, 0);
        assert_eq!(
            lossy.malformed_block_ids, 2,
            "non-block events carry no item ordinal"
        );
    }
}
