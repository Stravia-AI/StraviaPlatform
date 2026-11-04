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

use anyhow::{Context, ensure};
use serde_json::Value;
use sqlx::{PgConnection, SqliteConnection};
use std::collections::HashSet;
use stravia_runtime_contract::turn_chain::{TurnNodeId, TurnNodeKind};

use super::super::content::{self, StoredNode};
use crate::history_retention::HistoryRetention;

const NODE_BATCH: i64 = 64;
const BATCH_BYTES: usize = 8 * 1024 * 1024;
const REFERENCE_BATCH: usize = 200;

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

fn node_select(references: &'static str) -> String {
    format!(
        "SELECT n.node_pk,n.scope_id,n.id,parent.id AS parent_id,n.payload_version,pl.payload,{references} \
         FROM turn_nodes n \
         JOIN turn_node_payloads pl ON pl.node_pk=n.node_pk \
         LEFT JOIN turn_nodes parent ON parent.node_pk=n.parent_pk \
         WHERE n.kind='response' AND n.node_pk>$1 AND n.node_pk<=$2 ORDER BY n.node_pk"
    )
}

#[derive(sqlx::FromRow)]
struct NodeSize {
    node_pk: i64,
    encoded_len: i64,
    trailer: Vec<u8>,
    content_bytes: i64,
}

fn batch_end(rows: Vec<NodeSize>) -> anyhow::Result<Option<i64>> {
    let mut bytes = 0usize;
    let mut end = None;
    for row in rows {
        let descriptor = usize::try_from(row.encoded_len)?
            .max(crate::storage_codec::decoded_size_hint(&row.trailer)?);
        let cost = descriptor.saturating_add(usize::try_from(row.content_bytes)?);
        // 与历史归一化相同：首个不可拆节点可以超预算，后续节点留到下一批。
        if end.is_some() && bytes.saturating_add(cost) > BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(cost);
        end = Some(row.node_pk);
    }
    Ok(end)
}

fn report_references(completed: u64, total: u64) {
    crate::startup_progress::report(
        "retention_data",
        "Converting dependency retention",
        completed,
        Some(total),
    );
}

macro_rules! backfill_backend {
    ($backfill:ident, $db:ty, $conn:ty, $restore:ident, $backend:ident) => {
        async fn $backfill(connection: &mut $conn) -> anyhow::Result<()> {
            let total: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM turn_nodes WHERE kind='response'")
                    .fetch_one(&mut *connection)
                    .await?;
            let total = u64::try_from(total)?;
            let query = node_select(content::reference_projection!($backend));
            let sizes = format!(
                "SELECT n.node_pk,CAST(length(pl.payload) AS BIGINT) AS encoded_len,\
                 substr(pl.payload,length(pl.payload)-13,14) AS trailer,\
                 CAST(COALESCE((SELECT SUM(CASE WHEN c.raw_len>length(c.body) \
                     THEN c.raw_len ELSE length(c.body) END) \
                     FROM turn_node_contents r JOIN contents c ON c.id=r.content_id \
                     WHERE r.node_pk=n.node_pk),0) AS BIGINT) AS content_bytes \
                 FROM turn_nodes n JOIN turn_node_payloads pl ON pl.node_pk=n.node_pk \
                 WHERE n.kind='response' AND n.node_pk>$1 \
                 ORDER BY n.node_pk LIMIT {NODE_BATCH}"
            );
            let mut cursor = 0i64;
            let mut completed = 0u64;
            report_references(completed, total);
            loop {
                let metadata: Vec<NodeSize> = sqlx::query_as(sqlx::AssertSqlSafe(sizes.as_str()))
                    .bind(cursor)
                    .fetch_all(&mut *connection)
                    .await?;
                let Some(end) = batch_end(metadata)? else {
                    ensure!(
                        completed == total,
                        "missing history payload during retention upgrade"
                    );
                    return Ok(());
                };
                let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(cursor)
                    .bind(end)
                    .fetch_all(&mut *connection)
                    .await?;
                let count = rows.len() as u64;
                let pks: Vec<_> = rows.iter().map(|row| row.node_pk).collect();
                let stored = rows
                    .into_iter()
                    .map(NodeRow::into_stored)
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let restored = content::$restore(&mut *connection, stored).await?;
                let (markers, compactions) = tokio::task::spawn_blocking(move || {
                    let mut markers = Vec::new();
                    let mut compactions = Vec::new();
                    for (pk, node) in pks.into_iter().zip(restored) {
                        markers.extend(
                            payload_marker_references(&node.payload)
                                .into_iter()
                                .map(|reference| (pk, reference)),
                        );
                        compactions.extend(
                            payload_compaction_record_ids(&node.payload)
                                .into_iter()
                                .map(|reference| (pk, reference)),
                        );
                    }
                    (markers, compactions)
                })
                .await
                .context("retention reference worker failed")?;
                let mut transaction = connection.begin().await?;
                for (references, table, column, target, key) in [
                    (
                        &markers,
                        "turn_node_markers",
                        "reference",
                        "history_markers",
                        "reference",
                    ),
                    (
                        &compactions,
                        "turn_node_compactions",
                        "record_id",
                        "native_compactions",
                        "id",
                    ),
                ] {
                    for chunk in references.chunks(REFERENCE_BATCH) {
                        let mut insert =
                            sqlx::QueryBuilder::<$db>::new("WITH refs(node_pk,reference) AS (");
                        insert.push_values(chunk, |mut values, (pk, reference)| {
                            values.push_bind(*pk).push_bind(reference);
                        });
                        insert.push(format!(
                            ") INSERT INTO {table}(node_pk,{column}) \
                             SELECT r.node_pk,r.reference FROM refs r \
                             JOIN {target} d ON d.{key}=r.reference WHERE 1=1 \
                             ON CONFLICT DO NOTHING"
                        ));
                        insert.build().execute(&mut *transaction).await?;
                    }
                }
                transaction.commit().await?;
                cursor = end;
                completed += count;
                report_references(completed, total);
            }
        }
    };
}

backfill_backend!(
    backfill_references,
    sqlx::Sqlite,
    SqliteConnection<'_>,
    restore_upgrade_sqlite,
    sqlite
);
backfill_backend!(
    backfill_references_postgres,
    sqlx::Postgres,
    PgConnection<'_>,
    restore_upgrade_postgres,
    postgres
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_retention::{DAY_MILLIS, DEFAULT_RETENTION_DAYS};

    async fn fixture(nodes: usize) -> anyhow::Result<SqliteConnection<'static>> {
        let mut connection = SqliteConnection::connect("sqlite::memory:").await?;
        let all = sqlx::migrate!("./migrations/sqlite");
        let migrator = sqlx::migrate::Migrator::with_migrations(
            all.iter()
                .filter(|migration| migration.version <= 39)
                .cloned()
                .collect(),
        );
        migrator
            .run_direct(Some(37), &mut connection, false)
            .await?;
        crate::content_upgrade::run_sqlite(&mut connection).await?;
        migrator.run_direct(None, &mut connection, false).await?;
        let reference = "a".repeat(stravia_runtime_contract::identifier::ID_LEN);
        sqlx::query(
            "INSERT INTO history_markers(reference,principal,kind,activity,segment_payload,\
             published_at,created_at,updated_at,expires_at) \
             VALUES($1,'api-key:alice','thinking','fixture',X'00',1,1,1,$2)",
        )
        .bind(&reference)
        .bind(10 * DAY_MILLIS)
        .execute(&mut connection)
        .await?;
        sqlx::query(
            "INSERT INTO native_compactions(id,principal,operation_id,payload,created_at,\
             delivered_at,expires_at) VALUES('record-a','api-key:alice','operation','{}',1,1,$1)",
        )
        .bind(10 * DAY_MILLIS)
        .execute(&mut connection)
        .await?;
        let scope: i64 = sqlx::query_scalar(
            "INSERT INTO content_scopes(principal) VALUES('api-key:alice') RETURNING id",
        )
        .fetch_one(&mut connection)
        .await?;
        let marker = crate::history_marker::render_history_marker_reference(&reference);
        let text = format!("{marker}{}", "shared text ".repeat(100));
        for index in 0..nodes {
            let mut encoded = content::encode(serde_json::json!({
                "client_delta": {
                    "items": [stravia_runtime_contract::protocol::ir::AiItem::output_text(&text)]
                },
                "client_output": {"items": []},
                "compaction_record_ids": ["record-a", "record-missing"]
            }))?;
            let node_pk: i64 = sqlx::query_scalar(
                "INSERT INTO turn_nodes(id,scope_id,kind,payload_version,created_at) \
                 VALUES($1,$2,'response',6,1) RETURNING node_pk",
            )
            .bind(format!("node-{index}"))
            .bind(scope)
            .fetch_one(&mut connection)
            .await?;
            sqlx::query("INSERT INTO turn_node_payloads(node_pk,payload) VALUES($1,$2)")
                .bind(node_pk)
                .bind(encoded.payload)
                .execute(&mut connection)
                .await?;
            content::put_sqlite(&mut connection, node_pk, scope, &mut encoded.contents).await?;
        }
        Ok(connection)
    }

    #[tokio::test]
    async fn retention_progress_tracks_committed_references_and_reruns() -> anyhow::Result<()> {
        use std::sync::{Arc, Mutex};
        let mut connection = fixture(129).await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        crate::startup_progress::observe_startup(
            move |event| observed.lock().unwrap().push(event),
            retention_sqlite(&mut connection),
        )
        .await?;
        let references: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT m.node_pk,m.reference,c.record_id FROM turn_node_markers m \
             JOIN turn_node_compactions c ON c.node_pk=m.node_pk ORDER BY m.node_pk",
        )
        .fetch_all(&mut connection)
        .await?;
        let reference = "a".repeat(stravia_runtime_contract::identifier::ID_LEN);
        let expected: Vec<_> = (1..=129)
            .map(|node_pk| (node_pk, reference.clone(), "record-a".to_owned()))
            .collect();
        assert_eq!(references, expected);
        retention_sqlite(&mut connection).await?;
        let after_rerun: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT m.node_pk,m.reference,c.record_id FROM turn_node_markers m \
             JOIN turn_node_compactions c ON c.node_pk=m.node_pk ORDER BY m.node_pk",
        )
        .fetch_all(&mut connection)
        .await?;
        assert_eq!(after_rerun, expected);
        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|event| event.phase == "retention_data"
                && event.completed > 0
                && event.completed < 129
                && event.total == Some(129)),
            "long conversions must publish committed intermediate progress"
        );
        assert_eq!(
            events.last().map(|event| (event.completed, event.total)),
            Some((129, Some(129)))
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_reference_batch_rolls_back_before_progress_and_can_resume() -> anyhow::Result<()>
    {
        use std::sync::{Arc, Mutex};
        let mut connection = fixture(3).await?;
        sqlx::query(
            "CREATE TRIGGER fail_compactions BEFORE INSERT ON turn_node_compactions \
             BEGIN SELECT RAISE(ABORT,'injected reference failure'); END",
        )
        .execute(&mut connection)
        .await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        let error = crate::startup_progress::observe_startup(
            move |event| observed.lock().unwrap().push(event),
            retention_sqlite(&mut connection),
        )
        .await
        .expect_err("failed batch must be returned to startup");
        assert!(
            error.to_string().contains("injected reference failure"),
            "{error:#}"
        );
        let markers: Vec<(i64, String)> =
            sqlx::query_as("SELECT node_pk,reference FROM turn_node_markers")
                .fetch_all(&mut connection)
                .await?;
        assert!(
            markers.is_empty(),
            "marker and compaction references must commit together"
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|event| event.completed == 0)
        );
        sqlx::query("DROP TRIGGER fail_compactions")
            .execute(&mut connection)
            .await?;
        retention_sqlite(&mut connection).await?;
        let restored: Vec<(i64, String)> =
            sqlx::query_as("SELECT node_pk,record_id FROM turn_node_compactions ORDER BY node_pk")
                .fetch_all(&mut connection)
                .await?;
        assert_eq!(
            restored,
            vec![
                (1, "record-a".into()),
                (2, "record-a".into()),
                (3, "record-a".into())
            ]
        );
        Ok(())
    }

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
