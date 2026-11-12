//! 按 Interaction 在事务内维护历史检索索引。
//!
//! 索引只存规范化词元；进度与段一起提交，正文回填或写入失败时保留原索引。

use sqlx::{PgPool, SqlitePool};

use super::{
    render::indexable_event_text,
    search_text::{Token, tokenize},
};
use crate::interaction_observation::{eligible, index_events_postgres, index_events_sqlite};

const EVENT_BATCH: i64 = 500;
const SEGMENT_TOKENS: usize = 256;
const SEGMENT_OVERLAP: usize = 15;

struct EventMetadata<'a> {
    interaction: &'a str,
    principal: &'a str,
    root: &'a str,
    sequence: i64,
    occurred_at: i64,
    weight: i32,
}

struct Segment<'a> {
    ordinal: i64,
    start: i64,
    end: i64,
    tokens: &'a [Token],
}

fn segments(tokens: &[Token]) -> Vec<Segment<'_>> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        let end = (start + SEGMENT_TOKENS).min(tokens.len());
        let slice = &tokens[start..end];
        result.push(Segment {
            ordinal: result.len() as i64,
            start: slice.iter().map(|token| token.range.start).min().unwrap() as i64,
            end: slice.iter().map(|token| token.range.end).max().unwrap() as i64,
            tokens: slice,
        });
        if end == tokens.len() {
            break;
        }
        start = end - SEGMENT_OVERLAP;
    }
    result
}

async fn insert_sqlite(
    tx: &mut sqlx::SqliteConnection<'_>,
    metadata: &EventMetadata<'_>,
    segment: &Segment<'_>,
) -> anyhow::Result<()> {
    let id: i64 = sqlx::query_scalar("INSERT INTO history_search_segments (interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(metadata.interaction).bind(metadata.principal).bind(metadata.root).bind(metadata.sequence).bind(segment.ordinal)
        .bind(metadata.weight).bind(metadata.occurred_at).bind(segment.start).bind(segment.end)
        .fetch_one(&mut *tx).await?;
    let mut text = String::new();
    for token in segment.tokens {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&token.text);
    }
    sqlx::query("INSERT INTO history_search_segments_fts (rowid,tokens) VALUES ($1,$2)")
        .bind(id)
        .bind(text)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

async fn insert_postgres(
    tx: &mut sqlx::PgConnection<'_>,
    metadata: &EventMetadata<'_>,
    segment: &Segment<'_>,
) -> anyhow::Result<()> {
    // 词元只含字母与数字；显式位置保留短语相邻关系，不依赖 PostgreSQL 文本检索配置。
    let mut vector = String::new();
    for (position, token) in segment.tokens.iter().enumerate() {
        use std::fmt::Write;
        if position != 0 {
            vector.push(' ');
        }
        write!(vector, "'{}':{}", token.text, position + 1)
            .expect("writing to a String cannot fail");
    }
    sqlx::query("INSERT INTO history_search_segments (interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end,tokens) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::tsvector)")
        .bind(metadata.interaction).bind(metadata.principal).bind(metadata.root).bind(metadata.sequence).bind(segment.ordinal)
        .bind(metadata.weight).bind(metadata.occurred_at).bind(segment.start).bind(segment.end).bind(vector)
        .execute(&mut *tx).await?;
    Ok(())
}

// SQLite 由 begin_write 排队写租约并以 BEGIN IMMEDIATE 串行化写入；PostgreSQL 先锁
// Interaction 再锁进度行，与删除操作的锁顺序一致，并在修改段之前锁住进度。
// 返回值表示本批之后是否还需再调用一次。进度统一记录 Interaction 的末事件序号，
// 避免状态重算写入的无事件序号让对账永不收敛。
macro_rules! index_backend {
    ($name:ident, $pool:ty, $interaction_lock:literal, $progress_lock:literal, $fetch:ident, $insert:ident) => {
        pub(crate) async fn $name(
            pool: &$pool,
            interaction: &str,
        ) -> anyhow::Result<bool> {
            let mut tx = pool.begin_write().await?;
            let sql = format!("SELECT i.principal,i.root_id,i.status,i.last_event_sequence,({}) FROM interaction_observations i WHERE i.id=$1{}", eligible("i"), $interaction_lock);
            let row: Option<(String, String, String, i64, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(interaction).fetch_optional(&mut *tx).await?;
            let Some((principal, root, status, last_event_sequence, is_member)) = row else {
                tx.commit().await?;
                return Ok(false);
            };
            if !is_member {
                sqlx::query("DELETE FROM history_search_segments WHERE interaction_id=$1")
                    .bind(interaction).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM history_search_progress WHERE interaction_id=$1")
                    .bind(interaction).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(false);
            }
            sqlx::query("INSERT INTO history_search_progress (interaction_id) VALUES ($1) ON CONFLICT (interaction_id) DO NOTHING")
                .bind(interaction).execute(&mut *tx).await?;
            let progress_sql = concat!("SELECT last_event_sequence,indexed_terminal FROM history_search_progress WHERE interaction_id=$1", $progress_lock);
            let (last, indexed_terminal): (i64, bool) = sqlx::query_as(progress_sql)
                .bind(interaction).fetch_one(&mut *tx).await?;
            let terminal = status != "running" && status != "waiting_client";
            let latest: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(sequence),0) FROM observation_events WHERE interaction_id=$1")
                .bind(interaction).fetch_one(&mut *tx).await?;
            // 若进度越过了当前末事件，重建以恢复一致性；终结态也需重建完整文本。
            let rebuild = (last > last_event_sequence && latest <= last_event_sequence)
                || (terminal && (!indexed_terminal || latest > last || last_event_sequence > last));
            if terminal && !rebuild {
                tx.commit().await?;
                return Ok(false);
            }
            if rebuild {
                sqlx::query("DELETE FROM history_search_segments WHERE interaction_id=$1")
                    .bind(interaction).execute(&mut *tx).await?;
            }
            let after = if rebuild { 0 } else { last };
            let limit = if rebuild { i64::MAX } else { EVENT_BATCH };
            let events = $fetch(&mut *tx, interaction, after, limit).await?;
            let more = !rebuild && events.len() as i64 == EVENT_BATCH;
            for event in events.iter() {
                if let Some(text) = indexable_event_text(event) {
                    let tokens = tokenize(&text.text);
                    let metadata = EventMetadata {
                        interaction,
                        principal: &principal,
                        root: &root,
                        sequence: event.sequence,
                        occurred_at: event.occurred_at,
                        weight: text.weight,
                    };
                    for segment in segments(&tokens) {
                        $insert(&mut *tx, &metadata, &segment).await?;
                    }
                }
            }
            // 还有下一批时保留事件游标；否则统一记为末事件序号，即使该序号
            // 属于其它 Interaction 也不会造成重复读取。
            let through = if more {
                events.last().expect("full batch has events").sequence
            } else {
                last_event_sequence.max(events.last().map_or(after, |event| event.sequence))
            };
            sqlx::query("UPDATE history_search_progress SET last_event_sequence=$2,indexed_terminal=$3 WHERE interaction_id=$1")
                .bind(interaction).bind(through).bind(terminal)
                .execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(more)
        }
    };
}

index_backend!(
    index_sqlite,
    SqlitePool,
    "",
    "",
    index_events_sqlite,
    insert_sqlite
);
index_backend!(
    index_postgres,
    PgPool,
    " FOR UPDATE OF i",
    " FOR UPDATE",
    index_events_postgres,
    insert_postgres
);

#[cfg(test)]
mod tests {
    use super::segments;

    #[test]
    fn segments_overlap_and_preserve_source_ranges() {
        let text = (0..500)
            .map(|n| format!("word{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let tokens = super::tokenize(&text);
        let segments = segments(&tokens);
        assert_eq!(
            segments.iter().map(|s| s.tokens.len()).collect::<Vec<_>>(),
            [256, 256, 18]
        );
        assert_eq!(&segments[0].tokens[241..], &segments[1].tokens[..15]);
        assert_eq!(&segments[1].tokens[241..], &segments[2].tokens[..15]);
        for segment in &segments {
            let excerpt = &text[segment.start as usize..segment.end as usize];
            assert_eq!(super::tokenize(excerpt).len(), segment.tokens.len());
        }
    }
}
