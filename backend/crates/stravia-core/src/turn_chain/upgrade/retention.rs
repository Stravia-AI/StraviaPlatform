//! 0039 与 0040 之间的启动期保留期转换。
//!
//! 幂等、可中断重跑：所有写入都是 `UPDATE` 的确定性重算或
//! `ON CONFLICT DO NOTHING` 的引用回填，不需要额外进度表。
//!
//! - 已发布/已确认依赖的首次时刻取 `max(原值, expires_at − 升级前天数)`，
//!   保证升级不会缩短任何现有保留期。
//! - 未发布/未确认依赖的原 `expires_at` 原样成为 `pending_until`。
//! - 从节点载荷回填 `turn_node_markers` 与 `turn_node_compactions`；映射引用
//!   无法从载荷反推，只能由升级后的新提交登记。

use anyhow::Context;
use serde_json::Value;
use sqlx::{PgConnection, SqliteConnection};
use std::collections::HashSet;
use stravia_runtime_contract::turn_chain::{TurnNodeId, TurnNodeKind};

use super::super::content::{self, StoredNode};
use crate::history_retention::HistoryRetention;

const NODE_BATCH: i64 = 16;

/// `log_retention_days` 是升级前生效的保留期；解析规则与运行期完全一致，
/// 返回毫秒窗口。
fn pre_upgrade_window_millis(value: Option<String>) -> i64 {
    HistoryRetention::from_setting(value.as_deref()).window_millis()
}

pub(crate) async fn retention_sqlite(connection: &mut SqliteConnection<'_>) -> anyhow::Result<()> {
    let days: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE name = 'log_retention_days'")
            .fetch_optional(&mut *connection)
            .await?;
    let window = pre_upgrade_window_millis(days);
    sqlx::query(
        "UPDATE history_markers SET \
             pending_until = CASE WHEN published_at IS NULL THEN expires_at ELSE NULL END, \
             published_at = CASE WHEN published_at IS NULL THEN NULL \
                                 ELSE MAX(published_at, expires_at - ?) END",
    )
    .bind(window)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE reversible_redaction_mappings SET \
             pending_until = CASE WHEN published_at IS NULL THEN expires_at ELSE NULL END, \
             published_at = CASE WHEN published_at IS NULL THEN NULL \
                                 ELSE MAX(published_at, expires_at - ?) END",
    )
    .bind(window)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE native_compactions SET \
             pending_until = CASE WHEN delivered_at IS NULL AND referenced_at IS NULL \
                                  THEN expires_at ELSE NULL END, \
             confirmed_at = CASE WHEN delivered_at IS NULL AND referenced_at IS NULL THEN NULL \
                 ELSE MAX(CASE WHEN delivered_at IS NULL THEN referenced_at \
                               WHEN referenced_at IS NULL THEN delivered_at \
                               WHEN delivered_at < referenced_at THEN delivered_at \
                               ELSE referenced_at END, expires_at - ?) END",
    )
    .bind(window)
    .execute(&mut *connection)
    .await?;
    backfill_references(connection).await
}

pub(crate) async fn retention_postgres(connection: &mut PgConnection<'_>) -> anyhow::Result<()> {
    let days: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE name = 'log_retention_days'")
            .fetch_optional(&mut *connection)
            .await?;
    let window = pre_upgrade_window_millis(days);
    sqlx::query(
        "UPDATE history_markers SET \
             pending_until = CASE WHEN published_at IS NULL THEN expires_at ELSE NULL END, \
             published_at = CASE WHEN published_at IS NULL THEN NULL \
                                 ELSE GREATEST(published_at, expires_at - $1) END",
    )
    .bind(window)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE reversible_redaction_mappings SET \
             pending_until = CASE WHEN published_at IS NULL THEN expires_at ELSE NULL END, \
             published_at = CASE WHEN published_at IS NULL THEN NULL \
                                 ELSE GREATEST(published_at, expires_at - $1) END",
    )
    .bind(window)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE native_compactions SET \
             pending_until = CASE WHEN delivered_at IS NULL AND referenced_at IS NULL \
                                  THEN expires_at ELSE NULL END, \
             confirmed_at = CASE WHEN delivered_at IS NULL AND referenced_at IS NULL THEN NULL \
                 ELSE GREATEST(CASE WHEN delivered_at IS NULL THEN referenced_at \
                                    WHEN referenced_at IS NULL THEN delivered_at \
                                    WHEN delivered_at < referenced_at THEN delivered_at \
                                    ELSE referenced_at END, expires_at - $1) END",
    )
    .bind(window)
    .execute(&mut *connection)
    .await?;
    backfill_references_postgres(connection).await
}

/// 收集载荷中出现的 Marker reference，包括跨协议投影定界符携带的 reference。
fn payload_marker_references(payload: &Value) -> Vec<String> {
    fn walk(value: &Value, found: &mut HashSet<String>) {
        match value {
            Value::String(text) => found.extend(
                crate::history_marker::history_marker_references_in_text(text),
            ),
            Value::Array(items) => {
                for item in items {
                    walk(item, found);
                }
            }
            Value::Object(map) => {
                for value in map.values() {
                    walk(value, found);
                }
            }
            _ => {}
        }
    }
    let mut found = HashSet::new();
    // 只回填节点自身的请求增量与响应；祖先窗口中的 Marker 已由祖先节点引用。
    let mut scoped = false;
    for key in ["client_delta", "client_output"] {
        if let Some(val) = payload.get(key) {
            scoped = true;
            walk(val, &mut found);
        }
    }
    if !scoped {
        walk(payload, &mut found);
    }
    let mut references: Vec<String> = found.into_iter().collect();
    references.sort();
    references
}

fn payload_compaction_record_ids(payload: &Value) -> Vec<String> {
    payload
        .get("compaction_record_ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    node_pk: i64,
    scope_id: i64,
    id: String,
    parent_id: Option<String>,
    payload_version: i64,
    payload: Vec<u8>,
    content_references: sqlx::types::Json<Vec<[i64; 3]>>,
}

impl NodeRow {
    fn into_stored(self) -> anyhow::Result<StoredNode> {
        Ok(StoredNode {
            node_pk: self.node_pk,
            scope_id: self.scope_id,
            references: self.content_references.0,
            id: TurnNodeId::new(self.id),
            kind: TurnNodeKind::Response,
            parent_id: self.parent_id.map(TurnNodeId::new),
            payload_version: u32::try_from(self.payload_version)
                .context("invalid history payload version")?,
            payload: self.payload,
        })
    }
}

fn node_select(value: &'static str, references: &'static str) -> String {
    format!(
        "SELECT n.node_pk,n.scope_id,n.id,parent.id AS parent_id,n.payload_version,pl.payload,{references} \
         FROM turn_nodes n \
         JOIN turn_node_payloads pl ON pl.node_pk=n.node_pk \
         LEFT JOIN turn_nodes parent ON parent.node_pk=n.parent_pk \
         WHERE n.kind='response' AND n.node_pk>{value} ORDER BY n.node_pk LIMIT {NODE_BATCH}"
    )
}

async fn backfill_references(connection: &mut SqliteConnection<'_>) -> anyhow::Result<()> {
    let query = node_select("$1", content::reference_projection!(sqlite));
    let mut cursor = 0i64;
    loop {
        let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(query.as_str()))
            .bind(cursor)
            .fetch_all(&mut *connection)
            .await?;
        let Some(last) = rows.last() else { break };
        cursor = last.node_pk;
        let mut restored = Vec::with_capacity(rows.len());
        for row in rows {
            let node_pk = row.node_pk;
            let stored = row.into_stored()?;
            let nodes = content::restore_sqlite(&mut *connection, vec![stored]).await?;
            let node = nodes.into_iter().next().context("restored node")?;
            restored.push((node_pk, node.payload));
        }
        for (node_pk, payload) in restored {
            insert_references_sqlite(&mut *connection, node_pk, &payload).await?;
        }
    }
    Ok(())
}

async fn insert_references_sqlite(
    connection: &mut SqliteConnection<'_>,
    node_pk: i64,
    payload: &Value,
) -> anyhow::Result<()> {
    for reference in payload_marker_references(payload) {
        sqlx::query(
            "INSERT INTO turn_node_markers(node_pk, reference) \
             SELECT ?1, ?2 WHERE EXISTS(SELECT 1 FROM history_markers WHERE reference = ?2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(node_pk)
        .bind(reference)
        .execute(&mut *connection)
        .await?;
    }
    for record_id in payload_compaction_record_ids(payload) {
        sqlx::query(
            "INSERT INTO turn_node_compactions(node_pk, record_id) \
             SELECT ?1, ?2 WHERE EXISTS(SELECT 1 FROM native_compactions WHERE id = ?2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(node_pk)
        .bind(record_id)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn backfill_references_postgres(connection: &mut PgConnection<'_>) -> anyhow::Result<()> {
    let query = node_select("$1", content::reference_projection!(postgres));
    let mut cursor = 0i64;
    loop {
        let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(query.as_str()))
            .bind(cursor)
            .fetch_all(&mut *connection)
            .await?;
        let Some(last) = rows.last() else { break };
        cursor = last.node_pk;
        let mut restored = Vec::with_capacity(rows.len());
        for row in rows {
            let node_pk = row.node_pk;
            let stored = row.into_stored()?;
            let nodes = content::restore_postgres(&mut *connection, vec![stored]).await?;
            let node = nodes.into_iter().next().context("restored node")?;
            restored.push((node_pk, node.payload));
        }
        for (node_pk, payload) in restored {
            insert_references_postgres(&mut *connection, node_pk, &payload).await?;
        }
    }
    Ok(())
}

async fn insert_references_postgres(
    connection: &mut PgConnection<'_>,
    node_pk: i64,
    payload: &Value,
) -> anyhow::Result<()> {
    for reference in payload_marker_references(payload) {
        sqlx::query(
            "INSERT INTO turn_node_markers(node_pk, reference) \
             SELECT $1, $2 WHERE EXISTS(SELECT 1 FROM history_markers WHERE reference = $2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(node_pk)
        .bind(reference)
        .execute(&mut *connection)
        .await?;
    }
    for record_id in payload_compaction_record_ids(payload) {
        sqlx::query(
            "INSERT INTO turn_node_compactions(node_pk, record_id) \
             SELECT $1, $2 WHERE EXISTS(SELECT 1 FROM native_compactions WHERE id = $2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(node_pk)
        .bind(record_id)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_retention::{DAY_MILLIS, DEFAULT_RETENTION_DAYS};

    #[test]
    fn pre_upgrade_window_uses_the_configured_days() {
        assert_eq!(pre_upgrade_window_millis(None), 7 * DAY_MILLIS);
        assert_eq!(pre_upgrade_window_millis(Some("3".into())), 3 * DAY_MILLIS);
        assert_eq!(
            pre_upgrade_window_millis(Some("invalid".into())),
            i64::from(DEFAULT_RETENTION_DAYS) * DAY_MILLIS
        );
    }

    #[test]
    fn payload_references_include_projection_delimiters_and_dedupe() {
        let reference = "a".repeat(stravia_runtime_contract::identifier::ID_LEN);
        let marker = crate::history_marker::render_history_marker_reference(&reference);
        let projection = crate::history_marker::render_text_projection_span(&reference, 0, "body");
        let payload = serde_json::json!({
            "client_delta": {"messages": [{"content": format!("{marker}{projection}")}]},
            "client_output": {"items": []},
            "compaction_record_ids": ["record-a"],
        });
        assert_eq!(payload_marker_references(&payload), vec![reference]);
        assert_eq!(
            payload_compaction_record_ids(&payload),
            vec!["record-a".to_owned()]
        );
    }
}
