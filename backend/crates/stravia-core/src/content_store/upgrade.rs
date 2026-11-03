//! 0037 与 0038 之间的离线数据转换：把 v3 `turn_contents` / 未压缩 Marker 段载荷
//! 改写成共享正文存储。
//!
//! 转换是可恢复的：`content_upgrade` 只有一行，stage 0 按 `last_node_pk` 转换节点，
//! stage 1 按 `last_marker_reference` 转换 Marker，stage 2 校验完成。每个批次的数据
//! 与游标在同一事务提交；中断只会重做当前批次。旧表在 0038 才删除。

use std::collections::HashMap;

use anyhow::{Context, ensure};

use super::{Content, decode_descriptor, restore_slots, validate_slots};
use crate::history_marker::HiddenHistorySegment;
use crate::history_marker::content as marker_content;
use crate::turn_chain::content;

const NODE_BATCH: usize = 64;
const MARKER_BATCH: usize = 64;

#[derive(Default)]
struct UpgradeState {
    stage: i64,
    last_node_pk: i64,
    last_marker_reference: Option<String>,
}

struct PreparedNode {
    node_pk: i64,
    scope_id: i64,
    payload: Vec<u8>,
    contents: Vec<Content>,
}

struct NodeWork {
    node_pk: i64,
    scope_id: i64,
    payload: Vec<u8>,
    refs: Vec<(i64, i64)>,
    bodies: Vec<(i64, Vec<u8>)>,
}

/// SQLite 钩子：先以开启外键的会话改表名，再转换数据。
pub(crate) async fn run_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    sqlite_rename::rename_scopes(connection).await?;
    sqlite::convert_nodes(connection).await?;
    sqlite::convert_markers(connection).await?;
    sqlite::validate(connection).await
}

/// PostgreSQL 钩子：`turn_scopes` 已在 0037 内改名。
pub(crate) async fn run_postgres(connection: &mut sqlx::PgConnection<'_>) -> anyhow::Result<()> {
    postgres::convert_nodes(connection).await?;
    postgres::convert_markers(connection).await?;
    postgres::validate(connection).await
}

macro_rules! backend {
    ($module:ident, $db:ty, $conn:ty) => {
        pub(super) mod $module {
            use super::*;

            async fn state(connection: &mut $conn) -> anyhow::Result<UpgradeState> {
                let row: Option<(i64, i64, Option<String>)> = sqlx::query_as(
                    "SELECT CAST(stage AS BIGINT), last_node_pk, last_marker_reference \
                     FROM content_upgrade WHERE phase = 1",
                )
                .fetch_optional(&mut *connection)
                .await?;
                Ok(match row {
                    Some((stage, last_node_pk, last_marker_reference)) => UpgradeState {
                        stage,
                        last_node_pk,
                        last_marker_reference,
                    },
                    None => UpgradeState::default(),
                })
            }

            async fn write_state(
                transaction: &mut $conn,
                stage: i64,
                node_pk: i64,
                marker: Option<&str>,
            ) -> anyhow::Result<()> {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "UPDATE content_upgrade SET stage = ",
                );
                query
                    .push_bind(stage)
                    .push(", last_node_pk = ")
                    .push_bind(node_pk)
                    .push(", last_marker_reference = ")
                    .push_bind(marker)
                    .push(" WHERE phase = 1");
                query.build().execute(&mut *transaction).await?;
                Ok(())
            }

            async fn read_nodes(
                connection: &mut $conn,
                cursor: i64,
                limit: usize,
            ) -> anyhow::Result<Vec<NodeWork>> {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT n.node_pk, n.scope_id, p.payload FROM turn_nodes n \
                     JOIN turn_node_payloads p ON p.node_pk = n.node_pk \
                     WHERE n.node_pk > ",
                );
                query
                    .push_bind(cursor)
                    .push(" ORDER BY n.node_pk LIMIT ")
                    .push_bind(limit as i64);
                let rows: Vec<(i64, i64, Vec<u8>)> = query
                    .build_query_as()
                    .fetch_all(&mut *connection)
                    .await?;
                let mut work = Vec::with_capacity(rows.len());
                for (node_pk, scope_id, payload) in rows {
                    let refs: Vec<(i64, i64)> = sqlx::query_as(
                        "SELECT ordinal, content_id FROM turn_node_contents \
                         WHERE node_pk = $1 ORDER BY ordinal",
                    )
                    .bind(node_pk)
                    .fetch_all(&mut *connection)
                    .await?;
                    let mut bodies = Vec::with_capacity(refs.len());
                    for (_, content_id) in &refs {
                        let body: Option<Vec<u8>> =
                            sqlx::query_scalar("SELECT content FROM turn_contents WHERE id = $1")
                                .bind(content_id)
                                .fetch_optional(&mut *connection)
                                .await?;
                        bodies.push((
                            *content_id,
                            body.context("missing legacy turn content")?,
                        ));
                    }
                    work.push(NodeWork {
                        node_pk,
                        scope_id,
                        payload,
                        refs,
                        bodies,
                    });
                }
                Ok(work)
            }

            /// 用旧的 JSON 正文还原 v3 descriptor，再按新规则重新外置。
            fn convert_node(work: NodeWork) -> anyhow::Result<PreparedNode> {
                let descriptor = decode_descriptor(&work.payload)?;
                let mut mapping: Vec<i64> = vec![0; work.refs.len()];
                for (ordinal, content_id) in work.refs {
                    let ordinal =
                        usize::try_from(ordinal).context("invalid legacy content ordinal")?;
                    ensure!(
                        ordinal < mapping.len(),
                        "legacy content ordinal out of range"
                    );
                    mapping[ordinal] = content_id;
                }
                let bodies: HashMap<i64, Vec<u8>> = work.bodies.into_iter().collect();
                let mut data = descriptor.data;
                restore_slots(&mut data, &descriptor.slots, |ordinal| {
                    let content_id = *mapping
                        .get(ordinal)
                        .context("legacy slot ordinal out of range")?;
                    let encoded = bodies
                        .get(&content_id)
                        .context("missing legacy turn content")?;
                    let raw = crate::storage_codec::decode(encoded)?;
                    Ok(serde_json::from_slice(&raw)?)
                })?;
                let encoded = content::encode(data)?;
                Ok(PreparedNode {
                    node_pk: work.node_pk,
                    scope_id: work.scope_id,
                    payload: encoded.payload,
                    contents: encoded.contents,
                })
            }

            async fn insert_refs(
                transaction: &mut $conn,
                node_pk: i64,
                scope_id: i64,
                ids: &[i64],
            ) -> anyhow::Result<()> {
                for (base, chunk) in ids.chunks(200).enumerate() {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "INSERT INTO turn_node_contents_v4 (node_pk, scope_id, ordinal, content_id) ",
                    );
                    query.push_values(
                        chunk.iter().enumerate(),
                        |mut row, (offset, id)| {
                            row.push_bind(node_pk)
                                .push_bind(scope_id)
                                .push_bind((base * 200 + offset) as i64)
                                .push_bind(*id);
                        },
                    );
                    query.build().execute(&mut *transaction).await?;
                }
                Ok(())
            }

            pub(super) async fn convert_nodes(connection: &mut $conn) -> anyhow::Result<()> {
                let state = state(connection).await?;
                if state.stage > 0 {
                    return Ok(());
                }
                let mut cursor = state.last_node_pk;
                loop {
                    let batch = read_nodes(connection, cursor, NODE_BATCH).await?;
                    if batch.is_empty() {
                        let mut transaction = connection.begin().await?;
                        write_state(
                            &mut transaction,
                            1,
                            cursor,
                            state.last_marker_reference.as_deref(),
                        )
                        .await?;
                        transaction.commit().await?;
                        return Ok(());
                    }
                    let next_cursor = batch.last().map(|work| work.node_pk).unwrap_or(cursor);
                    let prepared = tokio::task::spawn_blocking(move || {
                        batch
                            .into_iter()
                            .map(convert_node)
                            .collect::<anyhow::Result<Vec<_>>>()
                    })
                    .await
                    .context("content conversion worker failed")??;
                    let mut transaction = connection.begin().await?;
                    for mut node in prepared {
                        let ids = crate::content_store::$module::intern(
                            &mut transaction,
                            node.scope_id,
                            &mut node.contents,
                        )
                        .await?;
                        // 重跑同一节点时先清空旧的新引用，避免唯一键冲突。
                        sqlx::query("DELETE FROM turn_node_contents_v4 WHERE node_pk = $1")
                            .bind(node.node_pk)
                            .execute(&mut *transaction)
                            .await?;
                        insert_refs(&mut transaction, node.node_pk, node.scope_id, &ids).await?;
                        sqlx::query(
                            "UPDATE turn_node_payloads SET payload = $1 WHERE node_pk = $2",
                        )
                        .bind(&node.payload)
                        .bind(node.node_pk)
                        .execute(&mut *transaction)
                        .await?;
                        sqlx::query("DELETE FROM turn_node_contents WHERE node_pk = $1")
                            .bind(node.node_pk)
                            .execute(&mut *transaction)
                            .await?;
                        cursor = node.node_pk;
                    }
                    write_state(
                        &mut transaction,
                        0,
                        next_cursor.max(cursor),
                        state.last_marker_reference.as_deref(),
                    )
                    .await?;
                    transaction.commit().await?;
                }
            }

            pub(super) async fn convert_markers(connection: &mut $conn) -> anyhow::Result<()> {
                let state = state(connection).await?;
                if state.stage != 1 {
                    return Ok(());
                }
                let mut cursor = state.last_marker_reference.clone();
                loop {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT reference, principal, segment_payload FROM history_markers \
                         WHERE segment_payload IS NOT NULL AND segment_payload_encoded IS NULL",
                    );
                    if let Some(cursor) = &cursor {
                        query.push(" AND reference > ").push_bind(cursor);
                    }
                    query
                        .push(" ORDER BY reference LIMIT ")
                        .push_bind(MARKER_BATCH as i64);
                    let rows: Vec<(String, String, String)> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    if rows.is_empty() {
                        let mut transaction = connection.begin().await?;
                        write_state(
                            &mut transaction,
                            2,
                            state.last_node_pk,
                            cursor.as_deref(),
                        )
                        .await?;
                        transaction.commit().await?;
                        return Ok(());
                    }
                    let next = rows.last().map(|row| row.0.clone());
                    let prepared = tokio::task::spawn_blocking(move || {
                        rows.into_iter()
                            .map(|(reference, principal, raw)| {
                                let segment: HiddenHistorySegment = serde_json::from_str(&raw)
                                    .context("decoding legacy marker segment")?;
                                let payload = marker_content::encode_segment(&segment)?;
                                Ok((reference, principal, payload))
                            })
                            .collect::<anyhow::Result<Vec<_>>>()
                    })
                    .await
                    .context("marker conversion worker failed")??;
                    let mut transaction = connection.begin().await?;
                    for (reference, principal, payload) in prepared {
                        let (envelope, ids) = crate::history_marker::payload::$module::
                            intern_segment(&mut transaction, &principal, payload)
                            .await?;
                        crate::history_marker::payload::$module::replace_refs(
                            &mut transaction,
                            &reference,
                            &ids,
                        )
                        .await?;
                        sqlx::query(
                            "UPDATE history_markers SET segment_payload_encoded = $1 \
                             WHERE reference = $2",
                        )
                        .bind(&envelope)
                        .bind(&reference)
                        .execute(&mut *transaction)
                        .await?;
                        cursor = Some(reference);
                    }
                    write_state(
                        &mut transaction,
                        1,
                        state.last_node_pk,
                        cursor.as_deref(),
                    )
                    .await?;
                    transaction.commit().await?;
                    cursor = next;
                }
            }

            pub(super) async fn validate(connection: &mut $conn) -> anyhow::Result<()> {
                let state = state(connection).await?;
                if state.stage != 2 {
                    return Ok(());
                }
                let leftovers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_node_contents")
                    .fetch_one(&mut *connection)
                    .await?;
                ensure!(
                    leftovers == 0,
                    "content upgrade left legacy node references"
                );

                let mut cursor = 0i64;
                loop {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT n.node_pk, n.scope_id, p.payload FROM turn_nodes n \
                         JOIN turn_node_payloads p ON p.node_pk = n.node_pk WHERE n.node_pk > ",
                    );
                    query
                        .push_bind(cursor)
                        .push(" ORDER BY n.node_pk LIMIT ")
                        .push_bind(NODE_BATCH as i64);
                    let nodes: Vec<(i64, i64, Vec<u8>)> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    if nodes.is_empty() {
                        break;
                    }
                    for (node_pk, scope_id, payload) in nodes {
                        cursor = node_pk;
                        let descriptor = decode_descriptor(&payload)?;
                        let refs: Vec<(i64, i64)> = sqlx::query_as(
                            "SELECT ordinal, content_id FROM turn_node_contents_v4 \
                             WHERE node_pk = $1 ORDER BY ordinal",
                        )
                        .bind(node_pk)
                        .fetch_all(&mut *connection)
                        .await?;
                        let mut mapping = Vec::with_capacity(refs.len());
                        for (ordinal, content_id) in refs {
                            let ordinal = usize::try_from(ordinal)?;
                            ensure!(
                                ordinal == mapping.len(),
                                "non-contiguous converted content ordinal"
                            );
                            mapping.push(content_id);
                        }
                        validate_slots(&descriptor, mapping.len())
                            .context("converted node slot validation")?;
                        ensure_content_rows(
                            connection,
                            scope_id,
                            &mapping,
                            "converted node content is missing",
                        )
                        .await?;
                    }
                }

                let mut cursor: Option<String> = None;
                loop {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT reference, principal, segment_payload_encoded FROM history_markers \
                         WHERE segment_payload_encoded IS NOT NULL",
                    );
                    if let Some(cursor) = &cursor {
                        query.push(" AND reference > ").push_bind(cursor);
                    }
                    query
                        .push(" ORDER BY reference LIMIT ")
                        .push_bind(MARKER_BATCH as i64);
                    let rows: Vec<(String, String, Vec<u8>)> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    if rows.is_empty() {
                        break;
                    }
                    for (reference, principal, envelope) in rows {
                        cursor = Some(reference.clone());
                        let scope: Option<i64> = sqlx::query_scalar(
                            "SELECT id FROM content_scopes WHERE principal = $1",
                        )
                        .bind(&principal)
                        .fetch_optional(&mut *connection)
                        .await?;
                        let refs: Vec<(i64, i64)> = sqlx::query_as(
                            "SELECT ordinal, content_id FROM history_marker_contents \
                             WHERE reference = $1 ORDER BY ordinal",
                        )
                        .bind(&reference)
                        .fetch_all(&mut *connection)
                        .await?;
                        let mut mapping = Vec::with_capacity(refs.len());
                        for (ordinal, content_id) in refs {
                            let ordinal = usize::try_from(ordinal)?;
                            ensure!(
                                ordinal == mapping.len(),
                                "non-contiguous converted marker ordinal"
                            );
                            mapping.push(content_id);
                        }
                        let scope = scope.context("converted marker content scope is missing")?;
                        ensure_content_rows(
                            connection,
                            scope,
                            &mapping,
                            "converted marker content is missing",
                        )
                        .await?;
                        let descriptor = decode_descriptor(&envelope)?;
                        validate_slots(&descriptor, mapping.len())
                            .context("converted marker slot validation")?;
                    }
                }
                Ok(())
            }

            async fn ensure_content_rows(
                connection: &mut $conn,
                scope_id: i64,
                ids: &[i64],
                message: &'static str,
            ) -> anyhow::Result<()> {
                if ids.is_empty() {
                    return Ok(());
                }
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT COUNT(*) FROM contents WHERE scope_id = ",
                );
                query.push_bind(scope_id).push(" AND id IN (");
                let mut list = query.separated(",");
                for id in ids {
                    list.push_bind(*id);
                }
                list.push_unseparated(")");
                let found: i64 = query.build_query_scalar().fetch_one(&mut *connection).await?;
                let mut unique: Vec<i64> = ids.to_vec();
                unique.sort_unstable();
                unique.dedup();
                ensure!(
                    found == unique.len() as i64,
                    message
                );
                Ok(())
            }
        }
    };
}

backend!(sqlite, sqlx::Sqlite, sqlx::SqliteConnection<'_>);
backend!(postgres, sqlx::Postgres, sqlx::PgConnection<'_>);

mod sqlite_rename {
    use anyhow::{ensure, Context};

    /// SQLite 在 foreign_keys=OFF 时不会改写子表 REFERENCES，因此改表名必须在
    /// 开启外键的短会话内完成；随后恢复 OFF，交给迁移 runner 收尾。
    pub(super) async fn rename_scopes(
        connection: &mut sqlx::SqliteConnection<'_>,
    ) -> anyhow::Result<()> {
        let pending: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'turn_scopes'",
        )
        .fetch_optional(&mut *connection)
        .await?;
        if pending.is_none() {
            return Ok(());
        }
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&mut *connection)
            .await?;
        let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await?;
        ensure!(enabled == 1, "SQLite foreign keys could not be enabled");
        sqlx::query("ALTER TABLE turn_scopes RENAME TO content_scopes")
            .execute(&mut *connection)
            .await
            .context("renaming turn_scopes")?;
        let violations: Vec<(String, i64, String, i64)> =
            sqlx::query_as("PRAGMA foreign_key_check")
                .fetch_all(&mut *connection)
                .await?;
        ensure!(
            violations.is_empty(),
            "scope rename created foreign key violations: {violations:?}"
        );
        sqlx::query("PRAGMA foreign_keys=OFF")
            .execute(&mut *connection)
            .await?;
        Ok(())
    }
}

