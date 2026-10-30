//! ADR-0084 数据阶段：把带来源的 `edited` 快照改写为全部字段已固定的 `imported`。
//!
//! 在 `0030` 之后、`0031` 之前运行；每批最多 100 行、各自独立事务，SQLite 写
//! 协调随批释放。失败保留已提交进度并显式报错，不延长锁超时、不自动重试。
//! 改写条件本身排除已转换的行，阶段可重入；`0031` 尚未应用即表示阶段未完成，
//! 下次启动会继续。

use std::collections::BTreeSet;

use crate::provider_models::{ProviderModelField, SnapshotState, SourceStamp};
use anyhow::Context;
use serde_json::Value;

const BATCH_SIZE: i64 = 100;

fn all_pinned_fields() -> BTreeSet<ProviderModelField> {
    ProviderModelField::ALL.into_iter().collect()
}

/// 选中的行必然带有来源对象；无法解码的来源视为存储不变量破坏。
fn convert_snapshot(snapshot: &str) -> anyhow::Result<String> {
    let value: Value = serde_json::from_str(snapshot).context("decode Provider Model snapshot")?;
    let source = value
        .get("source")
        .filter(|source| source.is_object())
        .context("edited Provider Model snapshot is missing its source")?;
    let source: SourceStamp =
        serde_json::from_value(source.clone()).context("decode Provider Model source stamp")?;
    let state = SnapshotState::Imported {
        source,
        pinned: all_pinned_fields(),
    };
    serde_json::to_string(&state).context("encode Provider Model snapshot")
}

/// 两个后端共用的单批改写逻辑；调用方负责事务边界与游标推进。
fn converted_batch(
    rows: &[(String, String, String)],
) -> anyhow::Result<Vec<(String, String, String, String)>> {
    rows.iter()
        .map(|(provider_id, model_id, snapshot)| {
            Ok((
                provider_id.clone(),
                model_id.clone(),
                snapshot.clone(),
                convert_snapshot(snapshot)?,
            ))
        })
        .collect()
}

pub async fn run_sqlite(connection: &mut sqlx::SqliteConnection<'_>) -> anyhow::Result<()> {
    let mut cursor = (String::new(), String::new());
    loop {
        let rows = sqlx::query_as::<_, (String, String, String)>(
            r#"SELECT provider_id, model_id, snapshot_state
               FROM provider_models
               WHERE json_extract(snapshot_state, '$.type') = 'edited'
                 AND json_type(snapshot_state, '$.source') = 'object'
                 AND (provider_id, model_id) > (?, ?)
               ORDER BY provider_id, model_id
               LIMIT ?"#,
        )
        .bind(&cursor.0)
        .bind(&cursor.1)
        .bind(BATCH_SIZE)
        .fetch_all(&mut *connection)
        .await?;
        let Some(last) = rows.last() else {
            return Ok(());
        };
        let cursor_next = (last.0.clone(), last.1.clone());
        let updates = converted_batch(&rows)?;
        // 事务在错误路径由 Drop 回滚；不吞掉回滚错误，也不覆盖原始失败。
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        for (provider_id, model_id, previous, converted) in updates {
            sqlx::query(
                r#"UPDATE provider_models
                   SET snapshot_state = ?, revision = revision + 1, updated_at = datetime('now')
                   WHERE provider_id = ? AND model_id = ? AND snapshot_state = ?"#,
            )
            .bind(converted)
            .bind(provider_id)
            .bind(model_id)
            .bind(previous)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        cursor = cursor_next;
    }
}

pub async fn run_postgres(connection: &mut sqlx::PgConnection<'_>) -> anyhow::Result<()> {
    let mut cursor = (String::new(), String::new());
    loop {
        let rows = sqlx::query_as::<_, (String, String, String)>(
            r#"SELECT provider_id, model_id, snapshot_state::text
               FROM provider_models
               WHERE snapshot_state->>'type' = 'edited'
                 AND jsonb_typeof(snapshot_state->'source') = 'object'
                 AND (provider_id, model_id) > ($1, $2)
               ORDER BY provider_id, model_id
               LIMIT $3"#,
        )
        .bind(&cursor.0)
        .bind(&cursor.1)
        .bind(BATCH_SIZE)
        .fetch_all(&mut *connection)
        .await?;
        let Some(last) = rows.last() else {
            return Ok(());
        };
        let cursor_next = (last.0.clone(), last.1.clone());
        let updates = converted_batch(&rows)?;
        let mut transaction = connection.begin().await?;
        for (provider_id, model_id, previous, converted) in updates {
            sqlx::query(
                r#"UPDATE provider_models
                   SET snapshot_state = $1::jsonb, revision = revision + 1, updated_at = NOW()
                   WHERE provider_id = $2 AND model_id = $3 AND snapshot_state = $4::jsonb"#,
            )
            .bind(converted)
            .bind(provider_id)
            .bind(model_id)
            .bind(previous)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        cursor = cursor_next;
    }
}
