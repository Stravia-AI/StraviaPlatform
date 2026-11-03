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

fn block_ordinal(block_id: Option<&str>) -> i64 {
    block_id
        .and_then(|id| id.rsplit_once(":item:"))
        .and_then(|(_, ordinal)| ordinal.parse().ok())
        .unwrap_or(0)
}

struct SplitRow {
    sequence: i64,
    interaction_id: Option<String>,
    run_id: Option<String>,
    occurred_at: i64,
    payload: Value,
}

/// 第一段：事件载荷拆分为 attrs / 块 / 内容引用，Run 列就地回填。
async fn split_events_sqlite(connection: &mut sqlx::SqliteConnection<'_>) -> anyhow::Result<()> {
    let mut last: i64 = sqlx::query_scalar(
        "SELECT last_sequence FROM observation_normalization_upgrade WHERE phase=1",
    )
    .fetch_one(&mut *connection)
    .await?;
    let mut principals = HashMap::new();
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query("SELECT sequence,interaction_id,run_id,occurred_at,kind,payload FROM observation_events WHERE sequence>? ORDER BY sequence LIMIT ?")
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
            batch.push((
                kind,
                SplitRow {
                    sequence: row.try_get("sequence")?,
                    interaction_id: row.try_get("interaction_id")?,
                    run_id: row.try_get("run_id")?,
                    occurred_at: row.try_get("occurred_at")?,
                    payload: serde_json::from_slice(&crate::storage_codec::decode(&payload)?)
                        .context("observation payload is not JSON")?,
                },
            ));
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
                .bind(block_ordinal(row.payload["block_id"].as_str()))
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
                let content_id =
                    super::content_refs::intern_sqlite(tx, &principal, std::slice::from_mut(&mut unit))
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
                let content_id =
                    super::content_refs::intern_sqlite(tx, &principal, std::slice::from_mut(&mut unit))
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
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query("SELECT sequence,interaction_id,run_id,occurred_at,kind,payload FROM observation_events WHERE sequence>$1 ORDER BY sequence LIMIT $2")
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
            batch.push((
                kind,
                SplitRow {
                    sequence: row.try_get("sequence")?,
                    interaction_id: row.try_get("interaction_id")?,
                    run_id: row.try_get("run_id")?,
                    occurred_at: row.try_get("occurred_at")?,
                    payload: serde_json::from_slice(&crate::storage_codec::decode(&payload)?)
                        .context("observation payload is not JSON")?,
                },
            ));
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
                .bind(block_ordinal(row.payload["block_id"].as_str()))
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
    for row in attempts {
        let sequence: i64 = row.try_get("sequence")?;
        let attempt_at: i64 = row.try_get("occurred_at")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
        let Some(model_turn_id) = attrs["model_turn_id"].as_str() else {
            continue;
        };
        let Some((turn_sequence, turn_attrs)) =
            finishes.get_mut(&(run_id, model_turn_id.to_owned()))
        else {
            continue;
        };
        if *turn_sequence < sequence {
            continue;
        }
        let Value::Object(object) = turn_attrs else {
            continue;
        };
        if let Some(usage) = attrs.get("usage") {
            object.insert("usage".into(), usage.clone());
        }
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
    for row in attempts {
        let sequence: i64 = row.try_get("sequence")?;
        let attempt_at: i64 = row.try_get("occurred_at")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let attrs = decode_attrs(row.try_get("attrs")?)?;
        let Some(model_turn_id) = attrs["model_turn_id"].as_str() else {
            continue;
        };
        let Some((turn_sequence, turn_attrs)) =
            finishes.get_mut(&(run_id, model_turn_id.to_owned()))
        else {
            continue;
        };
        if *turn_sequence < sequence {
            continue;
        }
        let Value::Object(object) = turn_attrs else {
            continue;
        };
        if let Some(usage) = attrs.get("usage") {
            object.insert("usage".into(), usage.clone());
        }
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
    sqlx::query("UPDATE interaction_observations SET input_sequence=(SELECT MAX(e.sequence) FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE r.interaction_id=interaction_observations.id AND r.parent_run_id IS NULL AND e.kind='user_input_recorded')")
        .execute(&mut *connection)
        .await?;
    Ok(())
}

async fn backfill_input_sequence_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE interaction_observations SET input_sequence=(SELECT MAX(e.sequence) FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE r.interaction_id=interaction_observations.id AND r.parent_run_id IS NULL AND e.kind='user_input_recorded')")
        .execute(&mut *connection)
        .await?;
    Ok(())
}
