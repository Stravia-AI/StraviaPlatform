//! 事务内事件写入缓冲：同一事务产生的事件行、块行、内容引用行与工具结果行先收集，
//! 再各用多行 INSERT 落库（事件 → 块 → 引用 → 工具结果，满足外键先后）。
//!
//! 绑定参数数按每条语句不超过 900 估算，给 SQLite 旧版本的 999 个上限留余量。

use serde_json::Value;
use sqlx::QueryBuilder;

use super::store::{attrs_value, encode_attrs, event_operation_id, event_tool_id};

/// 事件行每条语句 9 列。
const EVENT_ROWS_PER_INSERT: usize = 100;
/// 块行每条语句 9 列。
const BLOCK_ROWS_PER_INSERT: usize = 100;
/// 引用行每条语句 3 列。
const REF_ROWS_PER_INSERT: usize = 300;
/// 工具结果行每条语句 5 列。
const TOOL_ROWS_PER_INSERT: usize = 180;

struct EventRow {
    sequence: i64,
    occurred_at: i64,
    kind: String,
    attrs: Option<Vec<u8>>,
    tool_id: Option<String>,
    operation_id: Option<String>,
}

pub(super) struct BlockRow {
    pub sequence: i64,
    pub model_turn_id: Option<String>,
    pub attempt_id: Option<String>,
    pub item_ordinal: i64,
    pub channel: &'static str,
    pub complete: bool,
    pub envelope: Option<Vec<u8>>,
}

struct RefRow {
    sequence: i64,
    ordinal: i64,
    content_id: i64,
}

pub(super) struct ToolRow {
    pub sequence: i64,
    pub ordinal: i64,
    pub tool_id: String,
    pub is_error: bool,
    pub content_id: i64,
}

/// 同一 Run 的一个事务内的待写行；事件行的 Interaction 与 Run 对整批相同。
pub(super) struct EventWrites<'a> {
    interaction_id: &'a str,
    run_id: &'a str,
    events: Vec<EventRow>,
    blocks: Vec<BlockRow>,
    refs: Vec<RefRow>,
    tools: Vec<ToolRow>,
}

impl<'a> EventWrites<'a> {
    pub(super) fn new(interaction_id: &'a str, run_id: &'a str) -> Self {
        Self {
            interaction_id,
            run_id,
            events: Vec::new(),
            blocks: Vec::new(),
            refs: Vec::new(),
            tools: Vec::new(),
        }
    }

    pub(super) fn push_event(
        &mut self,
        sequence: i64,
        occurred_at: i64,
        kind: &str,
        payload: &Value,
    ) -> anyhow::Result<()> {
        let attrs = attrs_value(payload);
        self.events.push(EventRow {
            sequence,
            occurred_at,
            kind: kind.to_owned(),
            attrs: encode_attrs(payload)?,
            tool_id: event_tool_id(attrs).map(str::to_owned),
            operation_id: event_operation_id(attrs).map(str::to_owned),
        });
        Ok(())
    }

    pub(super) fn push_block(&mut self, block: BlockRow) {
        self.blocks.push(block);
    }

    pub(super) fn push_refs(&mut self, sequence: i64, content_ids: &[i64]) -> anyhow::Result<()> {
        for (ordinal, content_id) in content_ids.iter().enumerate() {
            self.refs.push(RefRow {
                sequence,
                ordinal: i64::try_from(ordinal)?,
                content_id: *content_id,
            });
        }
        Ok(())
    }

    pub(super) fn push_tool(&mut self, tool: ToolRow) {
        self.tools.push(tool);
    }
}

macro_rules! flush_writes {
    ($name:ident, $db:ty) => {
        impl EventWrites<'_> {
            /// 把已收集的行写入并清空缓冲；后续语句要读取这些行时（如用量修订回写）先调用。
            pub(super) async fn $name(
                &mut self,
                tx: &mut sqlx::Transaction<'_, $db>,
            ) -> anyhow::Result<()> {
                let events = std::mem::take(&mut self.events);
                let blocks = std::mem::take(&mut self.blocks);
                let refs = std::mem::take(&mut self.refs);
                let tools = std::mem::take(&mut self.tools);
                let (interaction_id, run_id) = (self.interaction_id, self.run_id);
                for rows in events.chunks(EVENT_ROWS_PER_INSERT) {
                    let mut insert = QueryBuilder::<$db>::new(
                        "INSERT INTO observation_events (sequence,occurred_at,interaction_id,run_id,rejection_id,kind,attrs,tool_id,operation_id) ",
                    );
                    insert.push_values(rows, |mut row, event| {
                        row.push_bind(event.sequence)
                            .push_bind(event.occurred_at)
                            .push_bind(interaction_id)
                            .push_bind(run_id)
                            .push_bind(None::<&str>)
                            .push_bind(&event.kind)
                            .push_bind(&event.attrs)
                            .push_bind(&event.tool_id)
                            .push_bind(&event.operation_id);
                    });
                    insert.build().execute(&mut **tx).await?;
                }
                for rows in blocks.chunks(BLOCK_ROWS_PER_INSERT) {
                    let mut insert = QueryBuilder::<$db>::new(
                        "INSERT INTO observation_blocks(sequence,interaction_id,run_id,model_turn_id,attempt_id,item_ordinal,channel,complete,envelope) ",
                    );
                    insert.push_values(rows, |mut row, block| {
                        row.push_bind(block.sequence)
                            .push_bind(interaction_id)
                            .push_bind(run_id)
                            .push_bind(&block.model_turn_id)
                            .push_bind(&block.attempt_id)
                            .push_bind(block.item_ordinal)
                            .push_bind(block.channel)
                            .push_bind(block.complete)
                            .push_bind(&block.envelope);
                    });
                    insert.build().execute(&mut **tx).await?;
                }
                for rows in refs.chunks(REF_ROWS_PER_INSERT) {
                    let mut insert = QueryBuilder::<$db>::new(
                        "INSERT INTO observation_event_contents(sequence,ordinal,content_id) ",
                    );
                    insert.push_values(rows, |mut row, reference| {
                        row.push_bind(reference.sequence)
                            .push_bind(reference.ordinal)
                            .push_bind(reference.content_id);
                    });
                    insert.build().execute(&mut **tx).await?;
                }
                for rows in tools.chunks(TOOL_ROWS_PER_INSERT) {
                    let mut insert = QueryBuilder::<$db>::new(
                        "INSERT INTO observation_tool_results(sequence,ordinal,tool_id,is_error,content_id) ",
                    );
                    insert.push_values(rows, |mut row, tool| {
                        row.push_bind(tool.sequence)
                            .push_bind(tool.ordinal)
                            .push_bind(&tool.tool_id)
                            .push_bind(tool.is_error)
                            .push_bind(tool.content_id);
                    });
                    insert.build().execute(&mut **tx).await?;
                }
                Ok(())
            }
        }
    };
}

flush_writes!(flush_sqlite, sqlx::Sqlite);
flush_writes!(flush_postgres, sqlx::Postgres);
