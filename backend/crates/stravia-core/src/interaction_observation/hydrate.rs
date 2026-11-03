//! 读取侧把归一化行还原成事件视图。
//!
//! 事件表的 `attrs` 只含 kind 独有属性；thinking / visible 的块元数据在
//! `observation_blocks`，正文在共享内容存储。读取方仍通过 `ObservationEvent.payload`
//! 取完整视图，因此这里在批量读取后统一回填，避免每个查询各写一套拼装逻辑。

use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::Row;

use stravia_runtime_contract::protocol::ir::content::ContentFormat;

use super::{
    blocks::{BlockChannel, fill},
    content_refs::{StoredContent, read_contents},
    store::ObservationStore,
    types::{ObservationEvent, canonical_item_block_id},
};

struct BlockRow {
    interaction_id: String,
    run_id: String,
    model_turn_id: Option<String>,
    attempt_id: Option<String>,
    item_ordinal: i64,
    channel: String,
    complete: bool,
    envelope: Option<Value>,
}

/// 批量回填需要正文的 kind；其余事件只把 attrs 解码成 payload。
pub(super) async fn hydrate(
    store: &ObservationStore,
    events: &mut [ObservationEvent],
) -> anyhow::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let sequences: Vec<i64> = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "model_thinking"
                    | "client_visible_content"
                    | "user_input_recorded"
                    | "client_tool_result"
            )
        })
        .map(|event| event.sequence)
        .collect();
    if !sequences.is_empty() {
        let (blocks, refs, tool_results) = load_rows(store, &sequences).await?;
        let content_ids: Vec<i64> = refs
            .values()
            .flatten()
            .map(|(_, content_id)| *content_id)
            .chain(tool_results.values().flatten().map(|row| row.content_id))
            .collect();
        let contents = read_contents(store, &content_ids).await?;

        for event in events.iter_mut() {
            match event.kind.as_str() {
                "model_thinking" | "client_visible_content" => {
                    let Some(block) = blocks.get(&event.sequence) else {
                        continue;
                    };
                    let bodies = bodies_for(refs.get(&event.sequence), &contents);
                    let channel = if block.channel == "thinking" {
                        BlockChannel::Thinking
                    } else {
                        BlockChannel::Visible
                    };
                    let filled = fill(block.envelope.as_ref(), &bodies, channel);
                    let scope = block.attempt_id.as_deref().unwrap_or(&block.run_id);
                    let block_id = canonical_item_block_id(
                        scope,
                        usize::try_from(block.item_ordinal).unwrap_or(usize::MAX),
                    );
                    let mut payload = json!({
                        "kind": event.kind,
                        "text": filled.text,
                        "parts": filled.parts,
                        "block_id": block_id,
                        "item": filled.item,
                        "complete": block.complete,
                    });
                    if channel == BlockChannel::Thinking {
                        payload["model_turn_id"] = json!(block.model_turn_id);
                        payload["attempt_id"] = json!(block.attempt_id);
                    }
                    payload["interaction_id"] = json!(block.interaction_id);
                    payload["run_id"] = json!(block.run_id);
                    event.payload = payload;
                }
                "user_input_recorded" => {
                    let bodies = bodies_for(refs.get(&event.sequence), &contents);
                    let text = bodies
                        .iter()
                        .map(|body| body.text().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n");
                    event.payload["text"] = Value::String(text);
                }
                "client_tool_result" => {
                    let results = tool_results
                        .get(&event.sequence)
                        .into_iter()
                        .flatten()
                        .filter_map(|row| {
                            let body = contents.get(&row.content_id)?;
                            let content = match body.format {
                                ContentFormat::Text => {
                                    Value::String(body.text().unwrap_or_default())
                                }
                                ContentFormat::Json => {
                                    body.json().unwrap_or(Value::Null)
                                }
                            };
                            Some(json!({
                                "tool_id": row.tool_id,
                                "content": content,
                                "is_error": row.is_error,
                            }))
                        })
                        .collect::<Vec<_>>();
                    event.payload["results"] = Value::Array(results);
                }
                _ => {}
            }
        }
    }
    fill_run_payloads(store, events).await
}

/// 无载荷 kind 的字段来自投影列：读取时按事件所属 Run 批量回填。
async fn fill_run_payloads(
    store: &ObservationStore,
    events: &mut [ObservationEvent],
) -> anyhow::Result<()> {
    let sequences: Vec<i64> = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "run_admitted" | "run_state_changed" | "request_failed"
            )
        })
        .map(|event| event.sequence)
        .collect();
    if sequences.is_empty() {
        return Ok(());
    }
    let mut rows: HashMap<i64, RunPayloadRow> = HashMap::new();
    for chunk in sequences.chunks(400) {
        match store {
            ObservationStore::Sqlite(pool, _) => {
                let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                    "SELECT e.sequence,r.status,r.failure_json,r.superseded_by,r.inferred_retry,r.grouping_reason,r.diagnostic_source_run_id,r.has_new_user FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.sequence IN (",
                );
                let mut separated = query.separated(",");
                for sequence in chunk {
                    separated.push_bind(*sequence);
                }
                separated.push_unseparated(")");
                for row in query.build().fetch_all(pool).await? {
                    rows.insert(row.try_get("sequence")?, RunPayloadRow::from_sqlite(&row)?);
                }
            }
            ObservationStore::Postgres(pool, _) => {
                let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                    "SELECT e.sequence,r.status,r.failure_json,r.superseded_by,r.inferred_retry,r.grouping_reason,r.diagnostic_source_run_id,r.has_new_user FROM observation_events e JOIN inference_run_observations r ON r.id=e.run_id WHERE e.sequence IN (",
                );
                let mut separated = query.separated(",");
                for sequence in chunk {
                    separated.push_bind(*sequence);
                }
                separated.push_unseparated(")");
                for row in query.build().fetch_all(pool).await? {
                    rows.insert(row.try_get("sequence")?, RunPayloadRow::from_postgres(&row)?);
                }
            }
        }
    }
    for event in events {
        let Some(row) = rows.get(&event.sequence) else {
            continue;
        };
        match event.kind.as_str() {
            "run_admitted" => {
                event.payload["inferred_retry"] = json!(row.inferred_retry);
                event.payload["grouping_reason"] = json!(row.grouping_reason);
                event.payload["diagnostic_source_run_id"] = json!(row.diagnostic_source_run_id);
                event.payload["has_new_user"] = json!(row.has_new_user);
            }
            "run_state_changed" => {
                event.payload["status"] = json!(row.status);
                event.payload["superseded_by"] = json!(row.superseded_by);
            }
            "request_failed" => {
                let error: Value = match row.failure_json.as_deref() {
                    Some(json) => serde_json::from_str(json)
                        .unwrap_or_else(|_| Value::Null),
                    None => Value::Null,
                };
                event.payload["error"] = error;
            }
            _ => {}
        }
    }
    Ok(())
}

struct RunPayloadRow {
    status: String,
    failure_json: Option<String>,
    superseded_by: Option<String>,
    inferred_retry: bool,
    grouping_reason: Option<String>,
    diagnostic_source_run_id: Option<String>,
    has_new_user: bool,
}

impl RunPayloadRow {
    fn from_sqlite(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Self> {
        Ok(Self {
            status: row.try_get("status")?,
            failure_json: row.try_get("failure_json")?,
            superseded_by: row.try_get("superseded_by")?,
            inferred_retry: row.try_get("inferred_retry")?,
            grouping_reason: row.try_get("grouping_reason")?,
            diagnostic_source_run_id: row.try_get("diagnostic_source_run_id")?,
            has_new_user: row.try_get("has_new_user")?,
        })
    }

    fn from_postgres(row: &sqlx::postgres::PgRow) -> anyhow::Result<Self> {
        Ok(Self {
            status: row.try_get("status")?,
            failure_json: row.try_get("failure_json")?,
            superseded_by: row.try_get("superseded_by")?,
            inferred_retry: row.try_get("inferred_retry")?,
            grouping_reason: row.try_get("grouping_reason")?,
            diagnostic_source_run_id: row.try_get("diagnostic_source_run_id")?,
            has_new_user: row.try_get("has_new_user")?,
        })
    }
}
fn bodies_for(
    refs: Option<&Vec<(i64, i64)>>,
    contents: &HashMap<i64, StoredContent>,
) -> Vec<StoredContent> {
    refs.into_iter()
        .flatten()
        .filter_map(|(_, content_id)| contents.get(content_id).cloned())
        .collect()
}

#[derive(Debug)]
struct ToolResultRow {
    tool_id: String,
    is_error: bool,
    content_id: i64,
}

#[allow(clippy::type_complexity)]
async fn load_rows(
    store: &ObservationStore,
    sequences: &[i64],
) -> anyhow::Result<(
    HashMap<i64, BlockRow>,
    HashMap<i64, Vec<(i64, i64)>>,
    HashMap<i64, Vec<ToolResultRow>>,
)> {
    let mut blocks = HashMap::new();
    let mut refs: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    let mut tool_results: HashMap<i64, Vec<ToolResultRow>> = HashMap::new();
    for chunk in sequences.chunks(400) {
        match store {
            ObservationStore::Sqlite(pool, _) => {
                let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                    "SELECT sequence,interaction_id,run_id,model_turn_id,attempt_id,item_ordinal,channel,complete,envelope FROM observation_blocks WHERE sequence IN (",
                );
                let mut separated = query.separated(",");
                for sequence in chunk {
                    separated.push_bind(*sequence);
                }
                separated.push_unseparated(")");
                for row in query.build().fetch_all(pool).await? {
                    let sequence: i64 = row.try_get("sequence")?;
                    let envelope: Option<Vec<u8>> = row.try_get("envelope")?;
                    blocks.insert(
                        sequence,
                        BlockRow {
                            interaction_id: row.try_get("interaction_id")?,
                            run_id: row.try_get("run_id")?,
                            model_turn_id: row.try_get("model_turn_id")?,
                            attempt_id: row.try_get("attempt_id")?,
                            item_ordinal: row.try_get("item_ordinal")?,
                            channel: row.try_get("channel")?,
                            complete: row.try_get("complete")?,
                            envelope: envelope
                                .map(|bytes| -> anyhow::Result<Value> {
                                    Ok(serde_json::from_slice(
                                        &crate::storage_codec::decode(&bytes)?,
                                    )?)
                                })
                                .transpose()?,
                        },
                    );
                }
                read_refs_sqlite(pool, chunk, &mut refs, &mut tool_results).await?;
            }
            ObservationStore::Postgres(pool, _) => {
                let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                    "SELECT sequence,interaction_id,run_id,model_turn_id,attempt_id,item_ordinal,channel,complete,envelope FROM observation_blocks WHERE sequence IN (",
                );
                let mut separated = query.separated(",");
                for sequence in chunk {
                    separated.push_bind(*sequence);
                }
                separated.push_unseparated(")");
                for row in query.build().fetch_all(pool).await? {
                    let sequence: i64 = row.try_get("sequence")?;
                    let envelope: Option<Vec<u8>> = row.try_get("envelope")?;
                    blocks.insert(
                        sequence,
                        BlockRow {
                            interaction_id: row.try_get("interaction_id")?,
                            run_id: row.try_get("run_id")?,
                            model_turn_id: row.try_get("model_turn_id")?,
                            attempt_id: row.try_get("attempt_id")?,
                            item_ordinal: row.try_get("item_ordinal")?,
                            channel: row.try_get("channel")?,
                            complete: row.try_get("complete")?,
                            envelope: envelope
                                .map(|bytes| -> anyhow::Result<Value> {
                                    Ok(serde_json::from_slice(
                                        &crate::storage_codec::decode(&bytes)?,
                                    )?)
                                })
                                .transpose()?,
                        },
                    );
                }
                read_refs_postgres(pool, chunk, &mut refs, &mut tool_results).await?;
            }
        }
    }
    Ok((blocks, refs, tool_results))
}

async fn read_refs_sqlite(
    pool: &sqlx::SqlitePool,
    chunk: &[i64],
    refs: &mut HashMap<i64, Vec<(i64, i64)>>,
    tool_results: &mut HashMap<i64, Vec<ToolResultRow>>,
) -> anyhow::Result<()> {
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT sequence,ordinal,content_id FROM observation_event_contents WHERE sequence IN (",
    );
    let mut separated = query.separated(",");
    for sequence in chunk {
        separated.push_bind(*sequence);
    }
    separated.push_unseparated(") ORDER BY sequence,ordinal");
    for row in query.build().fetch_all(pool).await? {
        let sequence: i64 = row.try_get("sequence")?;
        let ordinal: i64 = row.try_get("ordinal")?;
        let content_id: i64 = row.try_get("content_id")?;
        refs.entry(sequence).or_default().push((ordinal, content_id));
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT sequence,ordinal,tool_id,is_error,content_id FROM observation_tool_results WHERE sequence IN (",
    );
    let mut separated = query.separated(",");
    for sequence in chunk {
        separated.push_bind(*sequence);
    }
    separated.push_unseparated(") ORDER BY sequence,ordinal");
    for row in query.build().fetch_all(pool).await? {
        let sequence: i64 = row.try_get("sequence")?;
        tool_results.entry(sequence).or_default().push(ToolResultRow {
            tool_id: row.try_get("tool_id")?,
            is_error: row.try_get("is_error")?,
            content_id: row.try_get("content_id")?,
        });
    }
    Ok(())
}

async fn read_refs_postgres(
    pool: &sqlx::PgPool,
    chunk: &[i64],
    refs: &mut HashMap<i64, Vec<(i64, i64)>>,
    tool_results: &mut HashMap<i64, Vec<ToolResultRow>>,
) -> anyhow::Result<()> {
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT sequence,ordinal,content_id FROM observation_event_contents WHERE sequence IN (",
    );
    let mut separated = query.separated(",");
    for sequence in chunk {
        separated.push_bind(*sequence);
    }
    separated.push_unseparated(") ORDER BY sequence,ordinal");
    for row in query.build().fetch_all(pool).await? {
        let sequence: i64 = row.try_get("sequence")?;
        let ordinal: i64 = row.try_get("ordinal")?;
        let content_id: i64 = row.try_get("content_id")?;
        refs.entry(sequence).or_default().push((ordinal, content_id));
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT sequence,ordinal,tool_id,is_error,content_id FROM observation_tool_results WHERE sequence IN (",
    );
    let mut separated = query.separated(",");
    for sequence in chunk {
        separated.push_bind(*sequence);
    }
    separated.push_unseparated(") ORDER BY sequence,ordinal");
    for row in query.build().fetch_all(pool).await? {
        let sequence: i64 = row.try_get("sequence")?;
        tool_results.entry(sequence).or_default().push(ToolResultRow {
            tool_id: row.try_get("tool_id")?,
            is_error: row.try_get("is_error")?,
            content_id: row.try_get("content_id")?,
        });
    }
    Ok(())
}
