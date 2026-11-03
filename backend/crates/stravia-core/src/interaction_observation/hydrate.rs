//! 读取侧把归一化行还原成事件视图。
//!
//! 事件表的 `attrs` 只含 kind 独有属性；thinking / visible 的块元数据在
//! `observation_blocks`，正文在共享内容存储。读取方仍通过 `ObservationEvent.payload`
//! 取完整视图，因此这里在批量读取后统一回填，避免每个查询各写一套拼装逻辑。

use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::Row;

use stravia_runtime_contract::protocol::ir::content::ContentFormat;

use crate::content_store::Body;

use super::{
    blocks::{BlockChannel, fill},
    content_refs::read_scoped,
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
    if sequences.is_empty() {
        return Ok(());
    }
    let (blocks, refs, tool_results) = load_rows(store, &sequences).await?;
    let contents = read_bodies(store, events, &sequences, &refs, &tool_results).await?;

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
                            ContentFormat::Text => Value::String(body.text().unwrap_or_default().to_owned()),
                            ContentFormat::Json => body.json().unwrap_or(Value::Null),
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
    Ok(())
}

/// 按 Principal 分批读到的正文；content id 全局唯一，查找时逐批尝试。
struct BodySet(Vec<crate::content_store::Bodies>);

impl BodySet {
    fn get(&self, id: &i64) -> Option<&Body> {
        self.0.iter().find_map(|bodies| bodies.get(*id))
    }
}

fn bodies_for<'a>(
    refs: Option<&Vec<(i64, i64)>>,
    contents: &'a BodySet,
) -> Vec<&'a Body> {
    refs.into_iter()
        .flatten()
        .filter_map(|(_, content_id)| contents.get(content_id))
        .collect()
}

/// 正文按交互 Principal 所在的 scope 读取；content id 全局唯一，合并成一张表。
async fn read_bodies(
    store: &ObservationStore,
    events: &[ObservationEvent],
    sequences: &[i64],
    refs: &HashMap<i64, Vec<(i64, i64)>>,
    tool_results: &HashMap<i64, Vec<ToolResultRow>>,
) -> anyhow::Result<BodySet> {
    let wanted: std::collections::HashSet<i64> = sequences.iter().copied().collect();
    let interactions: Vec<String> = events
        .iter()
        .filter(|event| wanted.contains(&event.sequence))
        .filter_map(|event| event.interaction_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let principals = store.interaction_principals(&interactions).await?;
    let mut by_principal: HashMap<&str, Vec<i64>> = HashMap::new();
    for event in events.iter().filter(|event| wanted.contains(&event.sequence)) {
        let Some(principal) = event
            .interaction_id
            .as_ref()
            .and_then(|id| principals.get(id))
        else {
            continue;
        };
        let ids = by_principal.entry(principal.as_str()).or_default();
        ids.extend(
            refs.get(&event.sequence)
                .into_iter()
                .flatten()
                .map(|(_, content_id)| *content_id),
        );
        ids.extend(
            tool_results
                .get(&event.sequence)
                .into_iter()
                .flatten()
                .map(|row| row.content_id),
        );
    }
    let mut contents = Vec::with_capacity(by_principal.len());
    for (principal, ids) in by_principal {
        contents.push(read_scoped(store, principal, &ids).await?);
    }
    Ok(BodySet(contents))
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
                                    Ok(serde_json::from_slice(&crate::storage_codec::decode(
                                        &bytes,
                                    )?)?)
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
                                    Ok(serde_json::from_slice(&crate::storage_codec::decode(
                                        &bytes,
                                    )?)?)
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
        refs.entry(sequence)
            .or_default()
            .push((ordinal, content_id));
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
        tool_results
            .entry(sequence)
            .or_default()
            .push(ToolResultRow {
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
        refs.entry(sequence)
            .or_default()
            .push((ordinal, content_id));
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
        tool_results
            .entry(sequence)
            .or_default()
            .push(ToolResultRow {
                tool_id: row.try_get("tool_id")?,
                is_error: row.try_get("is_error")?,
                content_id: row.try_get("content_id")?,
            });
    }
    Ok(())
}
