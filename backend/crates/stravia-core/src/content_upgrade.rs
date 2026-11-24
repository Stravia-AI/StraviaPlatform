//! 0037 与 0038 之间的离线数据转换：把 v3 `turn_contents` / 未压缩 Marker 段载荷
//! 改写成共享正文存储。
//!
//! 转换需要理解 Turn Chain 与 History Marker 的载荷语义，因此放在共享正文存储之外，
//! `content_store` 本身保持与语义无关。
//!
//! 转换是可恢复的：`content_upgrade` 只有一行，stage 0 按 `last_node_pk` 转换节点，
//! stage 1 按 `last_marker_reference` 转换 Marker，stage 2 校验完成。每个批次的数据
//! 与游标在同一事务提交；中断只会重做当前批次。旧表在 0038 才删除。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, ensure};
use serde_json::Value;

use crate::content_store::{
    Content, ContentDigest, decode_descriptor, restore_slots, validate_slots,
};
use crate::history_marker::HiddenHistorySegment;
use crate::history_marker::content as marker_content;
use crate::turn_chain::content;

const NODE_BATCH: usize = 128;
const MARKER_BATCH: usize = 128;
const LEGACY_CACHE_BYTES: usize = 32 * 1024 * 1024;
const CONTENT_STORE_PHASE: &str = "content_store";
const CONTENT_STORE_LABEL: &str = "Converting shared content storage";

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
    bodies: HashMap<i64, Arc<Value>>,
}

#[derive(Default)]
struct LegacyContentCache {
    entries: HashMap<i64, Arc<Value>>,
    bytes: usize,
}

impl LegacyContentCache {
    fn get(&self, id: i64) -> Option<Arc<Value>> {
        self.entries.get(&id).cloned()
    }

    fn insert(&mut self, id: i64, value: Arc<Value>) {
        let entry_bytes = crate::memory::json_retained_bytes(&value);
        if entry_bytes > LEGACY_CACHE_BYTES / 2 {
            return;
        }
        if self.bytes.saturating_add(entry_bytes) > LEGACY_CACHE_BYTES {
            self.entries.clear();
            self.bytes = 0;
        }
        if self.entries.insert(id, value).is_none() {
            self.bytes = self.bytes.saturating_add(entry_bytes);
        }
    }
}

fn decode_legacy_body((id, encoded): (i64, Vec<u8>)) -> anyhow::Result<(i64, Arc<Value>)> {
    let raw = crate::storage_codec::decode(&encoded)?;
    let value: Value = serde_json::from_slice(&raw)?;
    Ok((id, Arc::new(value)))
}

/// 用旧的 JSON 正文还原 v3 descriptor，再按新规则重新外置。
fn convert_node(work: NodeWork) -> anyhow::Result<PreparedNode> {
    let descriptor = decode_descriptor(&work.payload)?;
    let mut mapping: Vec<i64> = vec![0; work.refs.len()];
    for (ordinal, content_id) in work.refs {
        let ordinal = usize::try_from(ordinal).context("invalid legacy content ordinal")?;
        ensure!(
            ordinal < mapping.len(),
            "legacy content ordinal out of range"
        );
        mapping[ordinal] = content_id;
    }
    let mut data = descriptor.data;
    restore_slots(&mut data, &descriptor.slots, |ordinal| {
        let content_id = *mapping
            .get(ordinal)
            .context("legacy slot ordinal out of range")?;
        let value = work
            .bodies
            .get(&content_id)
            .context("missing legacy turn content")?;
        Ok((**value).clone())
    })?;
    let encoded = content::encode(data)?;
    Ok(PreparedNode {
        node_pk: work.node_pk,
        scope_id: work.scope_id,
        payload: encoded.payload,
        contents: encoded.contents,
    })
}

fn convert_marker(
    (reference, principal, raw): (String, String, String),
) -> anyhow::Result<(String, String, marker_content::SegmentPayload)> {
    let segment: HiddenHistorySegment =
        serde_json::from_str(&raw).context("decoding legacy marker segment")?;
    let payload = marker_content::encode_segment(&segment)?;
    Ok((reference, principal, payload))
}

fn validate_node_descriptor((payload, count): (Vec<u8>, usize)) -> anyhow::Result<()> {
    let descriptor = decode_descriptor(&payload)?;
    validate_slots(&descriptor, count).context("converted node slot validation")?;
    Ok(())
}

fn validate_marker_descriptor((envelope, count): (Vec<u8>, usize)) -> anyhow::Result<()> {
    let descriptor = decode_descriptor(&envelope)?;
    validate_slots(&descriptor, count).context("converted marker slot validation")?;
    Ok(())
}

/// SQLite 钩子：先以开启外键的会话改表名，再转换数据。
pub(crate) async fn run_sqlite(connection: &mut sqlx::SqliteConnection<'_>) -> anyhow::Result<()> {
    sqlite_rename::rename_scopes(connection).await?;
    sqlite::run(connection).await
}

/// PostgreSQL 钩子：`turn_scopes` 已在 0037 内改名。
pub(crate) async fn run_postgres(connection: &mut sqlx::PgConnection<'_>) -> anyhow::Result<()> {
    postgres::run(connection).await
}

macro_rules! backend {
    ($module:ident, $db:ty, $conn:ty) => {
        pub(super) mod $module {
            use super::*;

            pub(super) async fn run(connection: &mut $conn) -> anyhow::Result<()> {
                let current = state(connection).await?;
                if current.stage > 2 {
                    return Ok(());
                }
                let total_nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_nodes")
                    .fetch_one(&mut *connection)
                    .await?;
                let total_markers: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM history_markers WHERE segment_payload IS NOT NULL",
                )
                .fetch_one(&mut *connection)
                .await?;
                let total = (total_nodes + total_markers).max(0) as u64;
                if total > 0 && current.stage < 2 {
                    let initial_done = if current.stage == 0 {
                        let done_nodes: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM turn_nodes WHERE node_pk <= $1",
                        )
                        .bind(current.last_node_pk)
                        .fetch_one(&mut *connection)
                        .await?;
                        done_nodes.max(0) as u64
                    } else {
                        let done_markers: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM history_markers \
                             WHERE segment_payload IS NOT NULL \
                             AND segment_payload_encoded IS NOT NULL",
                        )
                        .fetch_one(&mut *connection)
                        .await?;
                        (total_nodes + done_markers).max(0) as u64
                    };
                    crate::startup_progress::report(
                        CONTENT_STORE_PHASE,
                        CONTENT_STORE_LABEL,
                        initial_done.min(total),
                        Some(total),
                    );
                }
                convert_nodes(connection, total, total_nodes.max(0) as u64).await?;
                convert_markers(connection, total, total_nodes.max(0) as u64).await?;
                validate(connection).await
            }

            async fn state(connection: &mut $conn) -> anyhow::Result<UpgradeState> {
                // 0037 插入唯一的进度行；缺失说明迁移状态已损坏，不能当作从头开始。
                let (stage, last_node_pk, last_marker_reference): (i64, i64, Option<String>) =
                    sqlx::query_as(
                        "SELECT CAST(stage AS BIGINT), last_node_pk, last_marker_reference \
                         FROM content_upgrade WHERE phase = 1",
                    )
                    .fetch_one(&mut *connection)
                    .await
                    .context("content_upgrade progress row is missing")?;
                Ok(UpgradeState {
                    stage,
                    last_node_pk,
                    last_marker_reference,
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
                let updated = query.build().execute(&mut *transaction).await?.rows_affected();
                ensure!(updated == 1, "content_upgrade progress row was not updated");
                Ok(())
            }

            async fn read_nodes(
                connection: &mut $conn,
                cursor: i64,
                limit: usize,
                cache: &mut LegacyContentCache,
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
                if rows.is_empty() {
                    return Ok(Vec::new());
                }

                let mut ref_query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT node_pk, ordinal, content_id FROM turn_node_contents \
                     WHERE node_pk IN (",
                );
                let mut list = ref_query.separated(",");
                for (node_pk, _, _) in &rows {
                    list.push_bind(*node_pk);
                }
                list.push_unseparated(") ORDER BY node_pk, ordinal");
                let ref_rows: Vec<(i64, i64, i64)> = ref_query
                    .build_query_as()
                    .fetch_all(&mut *connection)
                    .await?;

                let mut refs_by_pk: HashMap<i64, Vec<(i64, i64)>> =
                    HashMap::with_capacity(rows.len());
                let mut batch_bodies: HashMap<i64, Arc<Value>> = HashMap::new();
                let mut missing: Vec<i64> = Vec::new();
                for (node_pk, ordinal, content_id) in ref_rows {
                    refs_by_pk
                        .entry(node_pk)
                        .or_default()
                        .push((ordinal, content_id));
                    if !batch_bodies.contains_key(&content_id) {
                        if let Some(value) = cache.get(content_id) {
                            batch_bodies.insert(content_id, value);
                        } else {
                            missing.push(content_id);
                        }
                    }
                }
                missing.sort_unstable();
                missing.dedup();
                for chunk in missing.chunks(256) {
                    let mut body_query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, content FROM turn_contents WHERE id IN (",
                    );
                    let mut list = body_query.separated(",");
                    for &id in chunk {
                        list.push_bind(id);
                    }
                    list.push_unseparated(")");
                    let encoded_rows: Vec<(i64, Vec<u8>)> = body_query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    ensure!(
                        encoded_rows.len() == chunk.len(),
                        "missing legacy turn content"
                    );
                    for (id, value) in content::upgrade_cpu(encoded_rows, decode_legacy_body).await?
                    {
                        cache.insert(id, Arc::clone(&value));
                        batch_bodies.insert(id, value);
                    }
                }

                let mut work = Vec::with_capacity(rows.len());
                for (node_pk, scope_id, payload) in rows {
                    let refs = refs_by_pk.remove(&node_pk).unwrap_or_default();
                    let mut bodies = HashMap::with_capacity(refs.len());
                    for (_, content_id) in &refs {
                        let value = batch_bodies
                            .get(content_id)
                            .context("missing legacy turn content")?;
                        bodies.insert(*content_id, Arc::clone(value));
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

            async fn insert_refs_batch(
                transaction: &mut $conn,
                refs: &[(i64, i64, i64, i64)],
            ) -> anyhow::Result<()> {
                for chunk in refs.chunks(200) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "INSERT INTO turn_node_contents_v4 (node_pk, scope_id, ordinal, content_id) ",
                    );
                    query.push_values(chunk, |mut row, &(node_pk, scope_id, ordinal, content_id)| {
                        row.push_bind(node_pk)
                            .push_bind(scope_id)
                            .push_bind(ordinal)
                            .push_bind(content_id);
                    });
                    query.build().execute(&mut *transaction).await?;
                }
                Ok(())
            }

            async fn delete_node_refs_batch(
                transaction: &mut $conn,
                table: &str,
                node_pks: &[i64],
            ) -> anyhow::Result<()> {
                for chunk in node_pks.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new("DELETE FROM ");
                    query.push(table).push(" WHERE node_pk IN (");
                    let mut list = query.separated(",");
                    for &pk in chunk {
                        list.push_bind(pk);
                    }
                    list.push_unseparated(")");
                    query.build().execute(&mut *transaction).await?;
                }
                Ok(())
            }

            async fn update_payloads_batch(
                transaction: &mut $conn,
                updates: &[(i64, &[u8])],
            ) -> anyhow::Result<()> {
                for chunk in updates.chunks(128) {
                    let mut query = sqlx::QueryBuilder::<$db>::new("WITH v(pk,payload) AS (");
                    query.push_values(chunk, |mut row, &(pk, payload)| {
                        row.push_bind(pk).push_bind(payload);
                    });
                    query.push(
                        ") UPDATE turn_node_payloads SET payload = v.payload \
                         FROM v WHERE turn_node_payloads.node_pk = v.pk",
                    );
                    query.build().execute(&mut *transaction).await?;
                }
                Ok(())
            }

            pub(super) async fn convert_nodes(
                connection: &mut $conn,
                total: u64,
                total_nodes: u64,
            ) -> anyhow::Result<()> {
                let state = state(connection).await?;
                if state.stage > 0 {
                    return Ok(());
                }
                let mut cursor = state.last_node_pk;
                let mut completed: u64 = if cursor > 0 {
                    let done: i64 =
                        sqlx::query_scalar("SELECT COUNT(*) FROM turn_nodes WHERE node_pk <= $1")
                            .bind(cursor)
                            .fetch_one(&mut *connection)
                            .await?;
                    done.max(0) as u64
                } else {
                    0
                };
                let mut cache = LegacyContentCache::default();
                loop {
                    let batch = read_nodes(connection, cursor, NODE_BATCH, &mut cache).await?;
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
                        if total > 0 {
                            crate::startup_progress::report(
                                CONTENT_STORE_PHASE,
                                CONTENT_STORE_LABEL,
                                total_nodes.min(total),
                                Some(total),
                            );
                        }
                        return Ok(());
                    }
                    let batch_len = batch.len() as u64;
                    let next_cursor = batch.last().map(|work| work.node_pk).unwrap_or(cursor);
                    let mut prepared = content::upgrade_cpu(batch, convert_node).await?;

                    // 按 scope 合并整批内容后统一 intern，避免每节点多次往返查重与压缩。
                    let mut scope_order: Vec<i64> = Vec::new();
                    let mut scope_contents: HashMap<i64, Vec<Content>> = HashMap::new();
                    let mut scope_seen: HashMap<i64, HashSet<ContentDigest>> = HashMap::new();
                    let mut node_digests: Vec<Vec<ContentDigest>> =
                        Vec::with_capacity(prepared.len());
                    for node in &mut prepared {
                        let seen = scope_seen.entry(node.scope_id).or_default();
                        let bucket = scope_contents.entry(node.scope_id).or_insert_with(|| {
                            scope_order.push(node.scope_id);
                            Vec::new()
                        });
                        let mut digests = Vec::with_capacity(node.contents.len());
                        for item in std::mem::take(&mut node.contents) {
                            let digest = item.digest();
                            digests.push(digest);
                            if seen.insert(digest) {
                                bucket.push(item);
                            }
                        }
                        node_digests.push(digests);
                    }

                    let mut transaction = connection.begin().await?;
                    let mut interned: HashMap<(i64, ContentDigest), i64> = HashMap::new();
                    for scope_id in scope_order {
                        let Some(mut items) = scope_contents.remove(&scope_id) else {
                            continue;
                        };
                        if items.is_empty() {
                            continue;
                        }
                        let digests: Vec<ContentDigest> =
                            items.iter().map(Content::digest).collect();
                        let ids = crate::content_store::$module::intern(
                            &mut transaction,
                            scope_id,
                            &mut items,
                        )
                        .await?;
                        for (digest, id) in digests.into_iter().zip(ids) {
                            interned.insert((scope_id, digest), id);
                        }
                    }

                    let node_pks: Vec<i64> = prepared.iter().map(|node| node.node_pk).collect();
                    let mut all_refs: Vec<(i64, i64, i64, i64)> = Vec::new();
                    let mut payload_updates: Vec<(i64, &[u8])> =
                        Vec::with_capacity(prepared.len());
                    for (node, digests) in prepared.iter().zip(node_digests.iter()) {
                        for (ordinal, digest) in digests.iter().enumerate() {
                            let content_id = *interned
                                .get(&(node.scope_id, *digest))
                                .context("missing interned content id")?;
                            all_refs.push((
                                node.node_pk,
                                node.scope_id,
                                ordinal as i64,
                                content_id,
                            ));
                        }
                        payload_updates.push((node.node_pk, node.payload.as_slice()));
                    }

                    // 重跑同一批次时先清空旧的新引用，避免唯一键冲突。
                    delete_node_refs_batch(&mut transaction, "turn_node_contents_v4", &node_pks)
                        .await?;
                    insert_refs_batch(&mut transaction, &all_refs).await?;
                    update_payloads_batch(&mut transaction, &payload_updates).await?;
                    delete_node_refs_batch(&mut transaction, "turn_node_contents", &node_pks)
                        .await?;
                    cursor = next_cursor.max(cursor);
                    write_state(
                        &mut transaction,
                        0,
                        cursor,
                        state.last_marker_reference.as_deref(),
                    )
                    .await?;
                    transaction.commit().await?;
                    completed = completed.saturating_add(batch_len).min(total_nodes);
                    if total > 0 {
                        crate::startup_progress::report(
                            CONTENT_STORE_PHASE,
                            CONTENT_STORE_LABEL,
                            completed.min(total),
                            Some(total),
                        );
                    }
                }
            }

            pub(super) async fn convert_markers(
                connection: &mut $conn,
                total: u64,
                total_nodes: u64,
            ) -> anyhow::Result<()> {
                let state = state(connection).await?;
                if state.stage != 1 {
                    return Ok(());
                }
                let mut cursor = state.last_marker_reference.clone();
                let done_markers: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM history_markers \
                     WHERE segment_payload IS NOT NULL AND segment_payload_encoded IS NOT NULL",
                )
                .fetch_one(&mut *connection)
                .await?;
                let mut completed = total_nodes.saturating_add(done_markers.max(0) as u64);
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
                        if total > 0 {
                            crate::startup_progress::report(
                                CONTENT_STORE_PHASE,
                                CONTENT_STORE_LABEL,
                                total,
                                Some(total),
                            );
                        }
                        return Ok(());
                    }
                    let batch_len = rows.len() as u64;
                    let next = rows.last().map(|row| row.0.clone());
                    let prepared = content::upgrade_cpu(rows, convert_marker).await?;
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
                    completed = completed.saturating_add(batch_len).min(total);
                    if total > 0 {
                        crate::startup_progress::report(
                            CONTENT_STORE_PHASE,
                            CONTENT_STORE_LABEL,
                            completed,
                            Some(total),
                        );
                    }
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
                    let mut ref_query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT node_pk, ordinal, content_id FROM turn_node_contents_v4 \
                         WHERE node_pk IN (",
                    );
                    let mut list = ref_query.separated(",");
                    for (node_pk, _, _) in &nodes {
                        list.push_bind(*node_pk);
                    }
                    list.push_unseparated(") ORDER BY node_pk, ordinal");
                    let ref_rows: Vec<(i64, i64, i64)> = ref_query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    let mut refs_by_pk: HashMap<i64, Vec<(i64, i64)>> =
                        HashMap::with_capacity(nodes.len());
                    for (node_pk, ordinal, content_id) in ref_rows {
                        refs_by_pk
                            .entry(node_pk)
                            .or_default()
                            .push((ordinal, content_id));
                    }

                    let mut by_scope: HashMap<i64, Vec<i64>> = HashMap::new();
                    let mut descriptor_jobs = Vec::with_capacity(nodes.len());
                    for (node_pk, scope_id, payload) in nodes {
                        cursor = node_pk;
                        let refs = refs_by_pk.remove(&node_pk).unwrap_or_default();
                        let mut mapping = Vec::with_capacity(refs.len());
                        for (ordinal, content_id) in refs {
                            let ordinal = usize::try_from(ordinal)?;
                            ensure!(
                                ordinal == mapping.len(),
                                "non-contiguous converted content ordinal"
                            );
                            mapping.push(content_id);
                        }
                        let count = mapping.len();
                        by_scope.entry(scope_id).or_default().extend(mapping);
                        descriptor_jobs.push((payload, count));
                    }
                    content::upgrade_cpu(descriptor_jobs, validate_node_descriptor).await?;
                    for (scope_id, ids) in by_scope {
                        ensure_content_rows(
                            connection,
                            scope_id,
                            &ids,
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
                    let mut principals: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
                    principals.sort_unstable();
                    principals.dedup();
                    let mut scope_query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT principal, id FROM content_scopes WHERE principal IN (",
                    );
                    let mut list = scope_query.separated(",");
                    for principal in &principals {
                        list.push_bind(*principal);
                    }
                    list.push_unseparated(")");
                    let scopes: HashMap<String, i64> = scope_query
                        .build_query_as::<(String, i64)>()
                        .fetch_all(&mut *connection)
                        .await?
                        .into_iter()
                        .collect();

                    let mut ref_query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT reference, ordinal, content_id FROM history_marker_contents \
                         WHERE reference IN (",
                    );
                    let mut list = ref_query.separated(",");
                    for (reference, _, _) in &rows {
                        list.push_bind(reference);
                    }
                    list.push_unseparated(") ORDER BY reference, ordinal");
                    let ref_rows: Vec<(String, i64, i64)> = ref_query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    let mut refs_by_ref: HashMap<String, Vec<(i64, i64)>> =
                        HashMap::with_capacity(rows.len());
                    for (reference, ordinal, content_id) in ref_rows {
                        refs_by_ref
                            .entry(reference)
                            .or_default()
                            .push((ordinal, content_id));
                    }

                    let mut by_scope: HashMap<i64, Vec<i64>> = HashMap::new();
                    let mut descriptor_jobs = Vec::with_capacity(rows.len());
                    for (reference, principal, envelope) in rows {
                        cursor = Some(reference.clone());
                        let refs = refs_by_ref.remove(&reference).unwrap_or_default();
                        let mut mapping = Vec::with_capacity(refs.len());
                        for (ordinal, content_id) in refs {
                            let ordinal = usize::try_from(ordinal)?;
                            ensure!(
                                ordinal == mapping.len(),
                                "non-contiguous converted marker ordinal"
                            );
                            mapping.push(content_id);
                        }
                        // 没有正文单元的 Marker（空 Reasoning、RedactedThinking 等）从不创建 scope，
                        // 只有存在引用时才要求 scope 与正文行。
                        if !mapping.is_empty() {
                            let scope = *scopes
                                .get(&principal)
                                .context("converted marker content scope is missing")?;
                            by_scope.entry(scope).or_default().extend(&mapping);
                        }
                        descriptor_jobs.push((envelope, mapping.len()));
                    }
                    content::upgrade_cpu(descriptor_jobs, validate_marker_descriptor).await?;
                    for (scope_id, ids) in by_scope {
                        ensure_content_rows(
                            connection,
                            scope_id,
                            &ids,
                            "converted marker content is missing",
                        )
                        .await?;
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
                let mut unique: Vec<i64> = ids.to_vec();
                unique.sort_unstable();
                unique.dedup();
                for chunk in unique.chunks(256) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT COUNT(*) FROM contents WHERE scope_id = ",
                    );
                    query.push_bind(scope_id).push(" AND id IN (");
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(*id);
                    }
                    list.push_unseparated(")");
                    let found: i64 = query.build_query_scalar().fetch_one(&mut *connection).await?;
                    ensure!(found == chunk.len() as i64, message);
                }
                Ok(())
            }
        }
    };
}

backend!(sqlite, sqlx::Sqlite, sqlx::SqliteConnection<'_>);
backend!(postgres, sqlx::Postgres, sqlx::PgConnection<'_>);

mod sqlite_rename {
    use anyhow::{Context, ensure};

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
