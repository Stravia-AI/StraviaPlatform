//! Observation v2 形态升级：载荷拆分为 attrs、块、内容引用，合并 Turn 首尾事件。
//!
//! 升级只重写已存在的行，不插入新事件；`0042` 之前 `payload` 仍为 NOT NULL，
//! 因此事件行的 `attrs` 可以逐批写入并从水位续跑。
use anyhow::Context;
use serde_json::Value;
use sqlx::Row;
use std::collections::HashMap;

const BATCH: i64 = 512;

pub(crate) async fn normalize_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    let stage: i64 =
        sqlx::query_scalar("SELECT stage FROM observation_normalization_upgrade WHERE phase=1")
            .fetch_one(&mut *connection)
            .await?;
    if stage < 1 {
        split_events_sqlite(connection).await?;
        set_stage_sqlite(connection, 1).await?;
    }
    if stage < 2 {
        merge_turn_start_sqlite(connection).await?;
        set_stage_sqlite(connection, 2).await?;
    }
    if stage < 3 {
        merge_turn_finish_sqlite(connection).await?;
        set_stage_sqlite(connection, 3).await?;
    }
    if stage < 4 {
        backfill_input_sequence_sqlite(connection).await?;
        set_stage_sqlite(connection, 4).await?;
    }
    Ok(())
}

pub(crate) async fn normalize_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    let stage: i64 =
        sqlx::query_scalar("SELECT stage FROM observation_normalization_upgrade WHERE phase=1")
            .fetch_one(&mut *connection)
            .await?;
    if stage < 1 {
        split_events_postgres(connection).await?;
        set_stage_postgres(connection, 1).await?;
    }
    if stage < 2 {
        merge_turn_start_postgres(connection).await?;
        set_stage_postgres(connection, 2).await?;
    }
    if stage < 3 {
        merge_turn_finish_postgres(connection).await?;
        set_stage_postgres(connection, 3).await?;
    }
    if stage < 4 {
        backfill_input_sequence_postgres(connection).await?;
        set_stage_postgres(connection, 4).await?;
    }
    Ok(())
}

async fn set_stage_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    stage: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE observation_normalization_upgrade SET stage=?,last_sequence=0 WHERE phase=1",
    )
    .bind(stage)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn set_stage_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    stage: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE observation_normalization_upgrade SET stage=$1,last_sequence=0 WHERE phase=1",
    )
    .bind(stage)
    .execute(&mut *connection)
    .await?;
    Ok(())
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
            .map_err(anyhow::Error::from)
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

/// 第一段：事件载荷拆分为 attrs / 块 / 内容引用，Run 列就地回填。
async fn split_events_sqlite(connection: &mut sqlx::SqliteConnection<'_>) -> anyhow::Result<()> {
    let mut last: i64 = sqlx::query_scalar(
        "SELECT last_sequence FROM observation_normalization_upgrade WHERE phase=1",
    )
    .fetch_one(&mut *connection)
    .await?;
    let mut principals = HashMap::new();
    let mut unreadable = LossyMigration::default();
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query("SELECT sequence,interaction_id,run_id,kind,payload FROM observation_events WHERE sequence>? ORDER BY sequence LIMIT ?")
            .bind(last)
            .bind(BATCH)
            .fetch_all(&mut *tx)
            .await?;
        if rows.is_empty() {
            tx.commit().await?;
            break;
        }
        let mut batch = Vec::with_capacity(rows.len());
        for row in rows {
            let kind: String = row.try_get("kind")?;
            let payload: Vec<u8> = row.try_get("payload")?;
            let row = unreadable.split_row(
                &kind,
                row.try_get("sequence")?,
                row.try_get("interaction_id")?,
                row.try_get("run_id")?,
                &payload,
            );
            batch.push((kind, row));
        }
        for (kind, row) in batch {
            last = row.sequence;
            split_event_sqlite(&mut tx, &kind, &row, &mut principals).await?;
        }
        sqlx::query("UPDATE observation_normalization_upgrade SET last_sequence=? WHERE phase=1")
            .bind(last)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    unreadable.report();
    Ok(())
}

async fn split_event_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    row: &SplitRow,
    principals: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    let mut attrs = encoded_attrs(&row.payload)?;
    let mut new_kind = kind.to_owned();
    match kind {
        "model_thinking" | "client_visible_content" => {
            let text = row.payload["text"].as_str().unwrap_or_default();
            let item = &row.payload["item"];
            let item = (!item.is_null()).then_some(item);
            let mut split = super::blocks::split(item, text);
            let principal = principal_sqlite(tx, row.interaction_id.as_deref(), principals).await?;
            let content_ids =
                super::content_refs::intern_sqlite(tx, &principal, &mut split.units).await?;
            let envelope = split
                .envelope
                .map(|value| {
                    Ok::<_, anyhow::Error>(crate::storage_codec::encode(
                        serde_json::to_vec(&value)?.into(),
                    )?)
                })
                .transpose()?;
            let (model_turn_id, attempt_id) = if kind == "model_thinking" {
                (
                    row.payload["model_turn_id"].as_str().map(str::to_owned),
                    row.payload["attempt_id"].as_str().map(str::to_owned),
                )
            } else {
                (None, None)
            };
            sqlx::query("INSERT INTO observation_blocks(sequence,interaction_id,run_id,model_turn_id,attempt_id,item_ordinal,channel,complete,envelope) VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT(sequence) DO UPDATE SET envelope=excluded.envelope")
                .bind(row.sequence)
                .bind(&row.interaction_id)
                .bind(&row.run_id)
                .bind(&model_turn_id)
                .bind(&attempt_id)
                .bind(row.item_ordinal)
                .bind(if kind == "model_thinking" { "thinking" } else { "visible" })
                .bind(row.payload["complete"].as_bool().unwrap_or(false))
                .bind(envelope)
                .execute(&mut **tx)
                .await?;
            for (ordinal, content_id) in content_ids.iter().enumerate() {
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES (?,?,?) ON CONFLICT(sequence,ordinal) DO NOTHING")
                    .bind(row.sequence)
                    .bind(i64::try_from(ordinal)?)
                    .bind(content_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "client_tool_result" => {
            let content = &row.payload["content"];
            if !content.is_null() {
                let principal =
                    principal_sqlite(tx, row.interaction_id.as_deref(), principals).await?;
                let mut unit = super::content_refs::value_content(content);
                let content_id = super::content_refs::intern_sqlite(
                    tx,
                    &principal,
                    std::slice::from_mut(&mut unit),
                )
                .await?[0];
                sqlx::query("INSERT INTO observation_tool_results(sequence,ordinal,tool_id,is_error,content_id) VALUES (?,?,?,?,?) ON CONFLICT(sequence,ordinal) DO NOTHING")
                    .bind(row.sequence)
                    .bind(0_i64)
                    .bind(row.payload["tool_id"].as_str().unwrap_or_default())
                    .bind(row.payload["is_error"].as_bool().unwrap_or(false))
                    .bind(content_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "input_preview_recorded" => {
            new_kind = "user_input_recorded".into();
            if let Some(text) = row.payload["text"].as_str() {
                let principal =
                    principal_sqlite(tx, row.interaction_id.as_deref(), principals).await?;
                let mut unit = super::content_refs::text_content(text);
                let content_id = super::content_refs::intern_sqlite(
                    tx,
                    &principal,
                    std::slice::from_mut(&mut unit),
                )
                .await?[0];
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES (?,?,?) ON CONFLICT(sequence,ordinal) DO NOTHING")
                    .bind(row.sequence)
                    .bind(0_i64)
                    .bind(content_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "run_admitted" => {
            if let Some(run_id) = row.run_id.as_deref() {
                sqlx::query("UPDATE inference_run_observations SET inferred_retry=?,grouping_reason=?,diagnostic_source_run_id=?,has_new_user=? WHERE id=?")
                    .bind(row.payload["inferred_retry"].as_bool().unwrap_or(false))
                    .bind(row.payload["grouping_reason"].as_str())
                    .bind(row.payload["diagnostic_source_run_id"].as_str())
                    .bind(row.payload["has_new_user"].as_bool().unwrap_or(false))
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "run_state_changed" => {
            if let (Some(run_id), Some(superseded)) =
                (row.run_id.as_deref(), row.payload["superseded_by"].as_str())
            {
                sqlx::query("UPDATE inference_run_observations SET superseded_by=? WHERE id=?")
                    .bind(superseded)
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "request_failed" => attrs = None,
        _ => {}
    }
    sqlx::query("UPDATE observation_events SET kind=?,attrs=? WHERE sequence=?")
        .bind(&new_kind)
        .bind(attrs)
        .bind(row.sequence)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn principal_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    interaction_id: Option<&str>,
    principals: &mut HashMap<String, String>,
) -> anyhow::Result<String> {
    let interaction_id = interaction_id.context("content reference requires interaction")?;
    if let Some(principal) = principals.get(interaction_id) {
        return Ok(principal.clone());
    }
    let principal: String =
        sqlx::query_scalar("SELECT principal FROM interaction_observations WHERE id=?")
            .bind(interaction_id)
            .fetch_one(&mut **tx)
            .await?;
    principals.insert(interaction_id.to_owned(), principal.clone());
    Ok(principal)
}

async fn split_events_postgres(connection: &mut sqlx::PgConnection<'_>) -> anyhow::Result<()> {
    let mut last: i64 = sqlx::query_scalar(
        "SELECT last_sequence FROM observation_normalization_upgrade WHERE phase=1",
    )
    .fetch_one(&mut *connection)
    .await?;
    let mut principals = HashMap::new();
    let mut unreadable = LossyMigration::default();
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query("SELECT sequence,interaction_id,run_id,kind,payload FROM observation_events WHERE sequence>$1 ORDER BY sequence LIMIT $2")
            .bind(last)
            .bind(BATCH)
            .fetch_all(&mut *tx)
            .await?;
        if rows.is_empty() {
            tx.commit().await?;
            break;
        }
        let mut batch = Vec::with_capacity(rows.len());
        for row in rows {
            let kind: String = row.try_get("kind")?;
            let payload: Vec<u8> = row.try_get("payload")?;
            let row = unreadable.split_row(
                &kind,
                row.try_get("sequence")?,
                row.try_get("interaction_id")?,
                row.try_get("run_id")?,
                &payload,
            );
            batch.push((kind, row));
        }
        for (kind, row) in batch {
            last = row.sequence;
            split_event_postgres(&mut tx, &kind, &row, &mut principals).await?;
        }
        sqlx::query("UPDATE observation_normalization_upgrade SET last_sequence=$1 WHERE phase=1")
            .bind(last)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    unreadable.report();
    Ok(())
}

async fn split_event_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: &str,
    row: &SplitRow,
    principals: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    let mut attrs = encoded_attrs(&row.payload)?;
    let mut new_kind = kind.to_owned();
    match kind {
        "model_thinking" | "client_visible_content" => {
            let text = row.payload["text"].as_str().unwrap_or_default();
            let item = &row.payload["item"];
            let item = (!item.is_null()).then_some(item);
            let mut split = super::blocks::split(item, text);
            let principal =
                principal_postgres(tx, row.interaction_id.as_deref(), principals).await?;
            let content_ids =
                super::content_refs::intern_postgres(tx, &principal, &mut split.units).await?;
            let envelope = split
                .envelope
                .map(|value| {
                    Ok::<_, anyhow::Error>(crate::storage_codec::encode(
                        serde_json::to_vec(&value)?.into(),
                    )?)
                })
                .transpose()?;
            let (model_turn_id, attempt_id) = if kind == "model_thinking" {
                (
                    row.payload["model_turn_id"].as_str().map(str::to_owned),
                    row.payload["attempt_id"].as_str().map(str::to_owned),
                )
            } else {
                (None, None)
            };
            sqlx::query("INSERT INTO observation_blocks(sequence,interaction_id,run_id,model_turn_id,attempt_id,item_ordinal,channel,complete,envelope) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(sequence) DO UPDATE SET envelope=EXCLUDED.envelope")
                .bind(row.sequence)
                .bind(&row.interaction_id)
                .bind(&row.run_id)
                .bind(&model_turn_id)
                .bind(&attempt_id)
                .bind(row.item_ordinal)
                .bind(if kind == "model_thinking" { "thinking" } else { "visible" })
                .bind(row.payload["complete"].as_bool().unwrap_or(false))
                .bind(envelope)
                .execute(&mut **tx)
                .await?;
            for (ordinal, content_id) in content_ids.iter().enumerate() {
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES ($1,$2,$3) ON CONFLICT(sequence,ordinal) DO NOTHING")
                    .bind(row.sequence)
                    .bind(i64::try_from(ordinal)?)
                    .bind(content_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "client_tool_result" => {
            let content = &row.payload["content"];
            if !content.is_null() {
                let principal =
                    principal_postgres(tx, row.interaction_id.as_deref(), principals).await?;
                let mut unit = super::content_refs::value_content(content);
                let content_id = super::content_refs::intern_postgres(
                    tx,
                    &principal,
                    std::slice::from_mut(&mut unit),
                )
                .await?[0];
                sqlx::query("INSERT INTO observation_tool_results(sequence,ordinal,tool_id,is_error,content_id) VALUES ($1,$2,$3,$4,$5) ON CONFLICT(sequence,ordinal) DO NOTHING")
                    .bind(row.sequence)
                    .bind(0_i64)
                    .bind(row.payload["tool_id"].as_str().unwrap_or_default())
                    .bind(row.payload["is_error"].as_bool().unwrap_or(false))
                    .bind(content_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "input_preview_recorded" => {
            new_kind = "user_input_recorded".into();
            if let Some(text) = row.payload["text"].as_str() {
                let principal =
                    principal_postgres(tx, row.interaction_id.as_deref(), principals).await?;
                let mut unit = super::content_refs::text_content(text);
                let content_id = super::content_refs::intern_postgres(
                    tx,
                    &principal,
                    std::slice::from_mut(&mut unit),
                )
                .await?[0];
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES ($1,$2,$3) ON CONFLICT(sequence,ordinal) DO NOTHING")
                    .bind(row.sequence)
                    .bind(0_i64)
                    .bind(content_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "run_admitted" => {
            if let Some(run_id) = row.run_id.as_deref() {
                sqlx::query("UPDATE inference_run_observations SET inferred_retry=$1,grouping_reason=$2,diagnostic_source_run_id=$3,has_new_user=$4 WHERE id=$5")
                    .bind(row.payload["inferred_retry"].as_bool().unwrap_or(false))
                    .bind(row.payload["grouping_reason"].as_str())
                    .bind(row.payload["diagnostic_source_run_id"].as_str())
                    .bind(row.payload["has_new_user"].as_bool().unwrap_or(false))
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "run_state_changed" => {
            if let (Some(run_id), Some(superseded)) =
                (row.run_id.as_deref(), row.payload["superseded_by"].as_str())
            {
                sqlx::query("UPDATE inference_run_observations SET superseded_by=$1 WHERE id=$2")
                    .bind(superseded)
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await?;
            }
            attrs = None;
        }
        "request_failed" => attrs = None,
        _ => {}
    }
    sqlx::query("UPDATE observation_events SET kind=$1,attrs=$2 WHERE sequence=$3")
        .bind(&new_kind)
        .bind(attrs)
        .bind(row.sequence)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn principal_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    interaction_id: Option<&str>,
    principals: &mut HashMap<String, String>,
) -> anyhow::Result<String> {
    let interaction_id = interaction_id.context("content reference requires interaction")?;
    if let Some(principal) = principals.get(interaction_id) {
        return Ok(principal.clone());
    }
    let principal: String =
        sqlx::query_scalar("SELECT principal FROM interaction_observations WHERE id=$1")
            .bind(interaction_id)
            .fetch_one(&mut **tx)
            .await?;
    principals.insert(interaction_id.to_owned(), principal.clone());
    Ok(principal)
}

/// 第二段：首个 attempt 开始并入 Turn 开始行；其余 attempt 保持独立。
async fn merge_turn_start_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    let mut tx = connection.begin().await?;
    let turns = sqlx::query("SELECT sequence,run_id,attrs FROM observation_events WHERE kind='model_turn_started' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let attempts = sqlx::query("SELECT sequence,occurred_at,run_id,attrs FROM observation_events WHERE kind='target_attempt_started' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let mut starts: HashMap<(Option<String>, String), (i64, Value)> = HashMap::new();
    for row in turns {
        let sequence: i64 = row.try_get("sequence")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
        if let Some(model_turn_id) = attrs["model_turn_id"].as_str() {
            starts.insert((run_id, model_turn_id.to_owned()), (sequence, attrs));
        }
    }
    for row in attempts {
        let sequence: i64 = row.try_get("sequence")?;
        let attempt_at: i64 = row.try_get("occurred_at")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
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
        sqlx::query("UPDATE observation_events SET attrs=? WHERE sequence=?")
            .bind(encoded_attrs(turn_attrs)?)
            .bind(*turn_sequence)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM observation_events WHERE sequence=?")
            .bind(sequence)
            .execute(&mut *tx)
            .await?;
        *turn_sequence = i64::MAX;
    }
    tx.commit().await?;
    Ok(())
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

/// 第三段：最后一个 attempt 的结束与用量并入 Turn 结束行。
async fn merge_turn_finish_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    let mut tx = connection.begin().await?;
    let turns = sqlx::query("SELECT sequence,run_id,attrs FROM observation_events WHERE kind='model_turn_finished' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let attempts = sqlx::query("SELECT sequence,occurred_at,run_id,attrs FROM observation_events WHERE kind='target_attempt_finished' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let mut finishes: HashMap<(Option<String>, String), (i64, Value)> = HashMap::new();
    for row in turns {
        let sequence: i64 = row.try_get("sequence")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
        if let Some(model_turn_id) = attrs["model_turn_id"].as_str() {
            finishes.insert((run_id, model_turn_id.to_owned()), (sequence, attrs));
        }
    }
    let mut candidates = Vec::new();
    for row in attempts {
        candidates.push(AttemptRow {
            sequence: row.try_get("sequence")?,
            at: row.try_get("occurred_at")?,
            run_id: row.try_get("run_id")?,
            attrs: decode_attrs(row.try_get("attrs")?)?,
        });
    }
    for (turn_sequence, attempt_sequence, merged) in merge_last_attempts(&mut finishes, candidates)
    {
        sqlx::query("UPDATE observation_events SET attrs=? WHERE sequence=?")
            .bind(encoded_attrs(&merged)?)
            .bind(turn_sequence)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM observation_events WHERE sequence=?")
            .bind(attempt_sequence)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM observation_events WHERE kind='usage_confirmed'")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn merge_turn_start_postgres(connection: &mut sqlx::PgConnection<'_>) -> anyhow::Result<()> {
    let mut tx = connection.begin().await?;
    let turns = sqlx::query("SELECT sequence,run_id,attrs FROM observation_events WHERE kind='model_turn_started' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let attempts = sqlx::query("SELECT sequence,occurred_at,run_id,attrs FROM observation_events WHERE kind='target_attempt_started' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let mut starts: HashMap<(Option<String>, String), (i64, Value)> = HashMap::new();
    for row in turns {
        let sequence: i64 = row.try_get("sequence")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
        if let Some(model_turn_id) = attrs["model_turn_id"].as_str() {
            starts.insert((run_id, model_turn_id.to_owned()), (sequence, attrs));
        }
    }
    for row in attempts {
        let sequence: i64 = row.try_get("sequence")?;
        let attempt_at: i64 = row.try_get("occurred_at")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
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
        sqlx::query("UPDATE observation_events SET attrs=$1 WHERE sequence=$2")
            .bind(encoded_attrs(turn_attrs)?)
            .bind(*turn_sequence)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM observation_events WHERE sequence=$1")
            .bind(sequence)
            .execute(&mut *tx)
            .await?;
        *turn_sequence = i64::MAX;
    }
    tx.commit().await?;
    Ok(())
}

async fn merge_turn_finish_postgres(connection: &mut sqlx::PgConnection<'_>) -> anyhow::Result<()> {
    let mut tx = connection.begin().await?;
    let turns = sqlx::query("SELECT sequence,run_id,attrs FROM observation_events WHERE kind='model_turn_finished' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let attempts = sqlx::query("SELECT sequence,occurred_at,run_id,attrs FROM observation_events WHERE kind='target_attempt_finished' AND attrs IS NOT NULL ORDER BY sequence")
        .fetch_all(&mut *tx)
        .await?;
    let mut finishes: HashMap<(Option<String>, String), (i64, Value)> = HashMap::new();
    for row in turns {
        let sequence: i64 = row.try_get("sequence")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
        if let Some(model_turn_id) = attrs["model_turn_id"].as_str() {
            finishes.insert((run_id, model_turn_id.to_owned()), (sequence, attrs));
        }
    }
    let mut candidates = Vec::new();
    for row in attempts {
        candidates.push(AttemptRow {
            sequence: row.try_get("sequence")?,
            at: row.try_get("occurred_at")?,
            run_id: row.try_get("run_id")?,
            attrs: decode_attrs(row.try_get("attrs")?)?,
        });
    }
    for (turn_sequence, attempt_sequence, merged) in merge_last_attempts(&mut finishes, candidates)
    {
        sqlx::query("UPDATE observation_events SET attrs=$1 WHERE sequence=$2")
            .bind(encoded_attrs(&merged)?)
            .bind(turn_sequence)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM observation_events WHERE sequence=$1")
            .bind(attempt_sequence)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM observation_events WHERE kind='usage_confirmed'")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// 第四段：根 Run 最新的用户输入事件成为中断恢复输入。
async fn backfill_input_sequence_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET input_sequence=(SELECT MAX(e.sequence) FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE r.interaction_id=interaction_observations.id AND r.id=interaction_observations.root_run_id AND e.kind='user_input_recorded')")
        .execute(&mut *connection)
        .await?;
    Ok(())
}

async fn backfill_input_sequence_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET input_sequence=(SELECT MAX(e.sequence) FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE r.interaction_id=interaction_observations.id AND r.id=interaction_observations.root_run_id AND e.kind='user_input_recorded')")
        .execute(&mut *connection)
        .await?;
    Ok(())
}

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
        assert_eq!(lossy.malformed_block_ids, 2, "non-block events carry no item ordinal");
    }
}
