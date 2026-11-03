//! Marker 正文读写的后端实现。写操作都在调用方已开启的事务内完成：正文与引用
//! 行、Marker 行同事务提交，删除引用与入队也同事务。

use std::collections::HashMap;

use anyhow::{Context, ensure};

use super::HiddenHistorySegment;
use super::content::SegmentPayload;
use crate::content_store::Bodies;

macro_rules! backend {
    ($module:ident, $db:ty, $pool:ty, $conn:ty) => {
        pub(crate) mod $module {
            use super::*;

            /// 写入段正文并返回（外壳字节, 按 ordinal 排列的内容 ID）。
            pub(crate) async fn intern_segment(
                connection: &mut $conn,
                principal: &str,
                payload: SegmentPayload,
            ) -> anyhow::Result<(Vec<u8>, Vec<i64>)> {
                let SegmentPayload {
                    envelope,
                    mut contents,
                } = payload;
                if contents.is_empty() {
                    return Ok((envelope, Vec::new()));
                }
                let scope =
                    crate::content_store::$module::resolve_scope(connection, principal).await?;
                let ids =
                    crate::content_store::$module::intern(connection, scope, &mut contents)
                        .await?;
                Ok((envelope, ids))
            }

            /// 替换 Marker 的正文引用：先删除旧引用并登记失去最后引用的正文，
            /// 再按 ordinal 插入新引用。新 Marker 的旧引用为空。
            pub(crate) async fn replace_refs(
                connection: &mut $conn,
                reference: &str,
                ids: &[i64],
            ) -> anyhow::Result<()> {
                let mut delete = sqlx::QueryBuilder::<$db>::new(
                    "DELETE FROM history_marker_contents WHERE reference = ",
                );
                delete.push_bind(reference);
                delete.push(" RETURNING content_id");
                let released: Vec<i64> = delete
                    .build_query_scalar()
                    .fetch_all(&mut *connection)
                    .await?;
                crate::content_store::$module::enqueue_unreferenced(
                    &mut *connection,
                    &released,
                )
                .await?;
                if ids.is_empty() {
                    return Ok(());
                }
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO history_marker_contents(reference, ordinal, content_id) ",
                );
                query.push_values(ids.iter().enumerate(), |mut row, (ordinal, id)| {
                    row.push_bind(reference)
                        .push_bind(ordinal as i64)
                        .push_bind(*id);
                });
                query.build().execute(&mut *connection).await?;
                Ok(())
            }

            /// 批量恢复段载荷；引用与正文各一次查询，不逐 Marker 读取。
            pub(crate) async fn read_segments(
                pool: &$pool,
                principal: &str,
                rows: &[(String, Option<Vec<u8>>)],
            ) -> anyhow::Result<Vec<Option<HiddenHistorySegment>>> {
                let mut connection = pool.acquire().await?;
                let scope = crate::content_store::$module::scope_id(&mut *connection, principal)
                    .await?;
                let mut references: Vec<&str> = Vec::new();
                for (reference, envelope) in rows {
                    if envelope.is_some() {
                        references.push(reference.as_str());
                    }
                }
                let mut mappings: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
                for chunk in references.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT reference, ordinal, content_id FROM history_marker_contents WHERE reference IN (",
                    );
                    let mut list = query.separated(",");
                    for reference in chunk {
                        list.push_bind(*reference);
                    }
                    list.push_unseparated(")");
                    let entries: Vec<(String, i64, i64)> =
                        query.build_query_as().fetch_all(&mut *connection).await?;
                    for (reference, ordinal, content_id) in entries {
                        mappings
                            .entry(reference)
                            .or_default()
                            .push((ordinal, content_id));
                    }
                }
                let mut ids: Vec<i64> = Vec::new();
                for entries in mappings.values() {
                    for (_, content_id) in entries {
                        ids.push(*content_id);
                    }
                }
                ids.sort_unstable();
                ids.dedup();
                let bodies = match scope {
                    Some(scope) if !ids.is_empty() => Bodies::from_rows(
                        crate::content_store::$module::read(&mut *connection, scope, &ids)
                            .await?,
                    ),
                    _ => Bodies::from_rows(Vec::new()),
                };
                let mut output = Vec::with_capacity(rows.len());
                for (reference, envelope) in rows {
                    let Some(envelope) = envelope else {
                        output.push(None);
                        continue;
                    };
                    let mut mapping: Vec<i64> = Vec::new();
                    if let Some(entries) = mappings.get(reference) {
                        let mut entries = entries.clone();
                        entries.sort_unstable_by_key(|(ordinal, _)| *ordinal);
                        for (ordinal, content_id) in entries {
                            let ordinal = usize::try_from(ordinal)
                                .context("invalid history marker content ordinal")?;
                            ensure!(
                                ordinal == mapping.len(),
                                "non-contiguous history marker content ordinal"
                            );
                            mapping.push(content_id);
                        }
                    }
                    output.push(Some(crate::history_marker::content::restore_segment(
                        envelope, &mapping, &bodies,
                    )?));
                }
                Ok(output)
            }
        }
    };
}

backend!(
    sqlite,
    sqlx::Sqlite,
    sqlx::SqlitePool,
    sqlx::SqliteConnection<'_>
);
backend!(
    postgres,
    sqlx::Postgres,
    sqlx::PgPool,
    sqlx::PgConnection<'_>
);
