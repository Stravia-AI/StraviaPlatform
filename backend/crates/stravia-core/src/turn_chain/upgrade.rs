//! Startup-only history conversion.
//!
//! Two hooks exist, and neither is a product read path:
//!
//! 1. [`convert_history_sqlite`] / [`convert_history_postgres`] run from the
//!    historical migration-0007 hook, before migration 0032 exists. They turn
//!    format-0/1 legacy rows into the legacy 256-byte-threshold format-2
//!    envelope using the private [`format2`] helper, then drop the legacy
//!    columns/tables exactly as before.
//! 2. [`normalize_sqlite`] / [`normalize_postgres`] run after migration 0032
//!    created the permanent tables and before migration 0033 drops the old
//!    `turn_chain_*` tables. They normalize `turn_chain_*` into the v3 tables.
//!
//! Normalization is resumable from the single `turn_history_upgrade` row:
//!
//! - stage 0 copies thin node metadata (public ids and scope interned, parent
//!   edge still NULL) in bounded batches. Each batch commits its rows and
//!   `last_node_id` atomically; a crash re-reads rows after that cursor.
//! - stage 1 copies legacy content rows with their original numeric ids,
//!   verifies each legacy SHA256 key against the decoded raw body once,
//!   computes the domain-prefixed SHA-256 identity and keeps the codec bytes unchanged. Batches
//!   are metadata/byte bounded (8 MiB plus one oversized row). The PostgreSQL
//!   content identity sequence is advanced explicitly when stage 1 completes,
//!   before stage 2 interns anything.
//! - stage 2 converts each descriptor: opaque item/system/tool content keeps
//!   its verified id, inline semantic units are extracted by the product
//!   module, and externalized profile containers are re-extracted with their
//!   nested slots. One descriptor, its parent edge, retention row and ordinal
//!   mapping commit together with `last_node_id`.
//! - stage 3 validates counts, ownership, parent edges, descriptor ordinals and
//!   link rows, then deletes copied contents that are no longer referenced in
//!   bounded batches. Only after all of that succeeds is the work state marked
//!   stage 4. Migration 0033 then owns dropping old tables and work state.
//!
//! The old tables are authoritative until stage 4; every failure leaves the
//! last committed stage/cursor intact for the next startup. Callers must run
//! normalization after 0032 and only run 0033 after it returns `Ok(())`.

use anyhow::{Context, ensure};
use serde_json::Value;

use std::collections::HashMap;

use format2::{put_postgres_prepared, put_sqlite_prepared};

mod format2;
mod normalize;

/// Startup hook after migration 0032 and before migration 0033.
pub(crate) async fn normalize_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    normalize::run_sqlite(connection, normalize::Limits::default()).await
}

/// Startup hook after migration 0032 and before migration 0033.
pub(crate) async fn normalize_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    normalize::run_postgres(connection, normalize::Limits::default()).await
}

type LegacyRef = (String, String, String);
type LegacyNode = (String, String, i64, String, Vec<LegacyRef>);
type PreparedNode = (String, String, format2::Encoded);

fn workers() -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .clamp(1, 8)
}

// Always drain every started worker, including after an error or panic.
async fn blocking_chunks<T, R, F>(items: Vec<T>, operation: F) -> anyhow::Result<Vec<R>>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> anyhow::Result<R> + Send + Sync + 'static,
{
    let count = workers().min(items.len());
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut chunks: Vec<Vec<T>> = (0..count).map(|_| Vec::new()).collect();
    for (index, item) in items.into_iter().enumerate() {
        chunks[index % count].push(item);
    }
    let operation = std::sync::Arc::new(operation);
    let handles: Vec<_> = chunks
        .into_iter()
        .map(|chunk| {
            let operation = operation.clone();
            tokio::task::spawn_blocking(move || {
                chunk
                    .into_iter()
                    .map(|item| operation(item))
                    .collect::<anyhow::Result<Vec<R>>>()
            })
        })
        .collect();
    let mut output = Vec::new();
    let mut failure = None;
    for handle in handles {
        match handle
            .await
            .context("history migration worker failed")
            .and_then(|result| result)
        {
            Ok(mut rows) => output.append(&mut rows),
            Err(error) => {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(output)
}

fn prepare((node, principal, format, json, refs): LegacyNode) -> anyhow::Result<PreparedNode> {
    let mut payload: Value = serde_json::from_str(&json)?;
    if format == 1 {
        let expected = payload["references"]
            .as_u64()
            .context("invalid legacy reference count")?;
        ensure!(
            expected > 0 && expected == refs.len() as u64,
            "incomplete legacy history references"
        );
        payload = payload
            .get_mut("data")
            .context("missing legacy history data")?
            .take();
        for (path, key, json) in refs {
            ensure!(
                format2::content_key(json.as_bytes()) == key,
                "damaged legacy history content"
            );
            let slot = payload
                .pointer_mut(&path)
                .context("invalid legacy history pointer")?;
            ensure!(slot.is_null(), "legacy history pointer overwrites data");
            *slot = serde_json::from_str(&json)?;
        }
    } else {
        ensure!(format == 0, "unsupported legacy history format");
    }
    Ok((node, principal, format2::encode(payload)?))
}

macro_rules! converter {
    ($name:ident, $conn:ty, $db:ty, $put:ident, $exists:literal) => {
        pub(crate) async fn $name(connection: &mut $conn) -> anyhow::Result<()> {
            let has_legacy: bool = sqlx::query_scalar($exists).fetch_one(&mut *connection).await?;
            if !has_legacy { return Ok(()); }
            let needs_references: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turn_chain_nodes WHERE storage_format = 1)")
                .fetch_one(&mut *connection).await?;
            if needs_references {
                // Both equality keys must be indexed: SQLite can otherwise choose the
                // principal/content index and scan that principal's refs for every node.
                sqlx::query("CREATE INDEX IF NOT EXISTS turn_chain_legacy_refs_upgrade_node_principal ON turn_chain_legacy_refs(node_id,principal)")
                    .execute(&mut *connection).await?;
            }
            let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_chain_nodes WHERE storage_format < 2")
                .fetch_one(&mut *connection).await?;
            let total = u64::try_from(total)?;
            let mut cursor: Option<String> = None;
            let mut completed = 0u64;
            crate::startup_progress::report("history_data", "Converting history", completed, Some(total));
            loop {
                let mut query = sqlx::QueryBuilder::<$db>::new("SELECT id, principal, CAST(storage_format AS BIGINT), legacy_payload FROM turn_chain_nodes WHERE storage_format < 2");
                if let Some(cursor) = &cursor { query.push(" AND id > ").push_bind(cursor); }
                query.push(" ORDER BY id LIMIT 100");
                let rows: Vec<(String, String, i64, String)> = query.build_query_as().fetch_all(&mut *connection).await?;
                if rows.is_empty() { break; }
                let next_cursor = rows.last().map(|row| row.0.clone());
                // One principal-scoped join for the entire worklist, not one lookup per node.
                let mut refs = HashMap::<(String, String), Vec<LegacyRef>>::new();
                if rows.iter().any(|row| row.2 == 1) {
                    let mut query = sqlx::QueryBuilder::<$db>::new("SELECT r.node_id, r.principal, r.path, r.content_key, c.content FROM turn_chain_legacy_refs r JOIN turn_chain_nodes n ON n.id = r.node_id AND n.principal = r.principal JOIN turn_chain_legacy_contents c ON c.principal = r.principal AND c.content_key = r.content_key WHERE n.storage_format = 1 AND r.node_id IN (");
                    let mut list = query.separated(",");
                    for row in rows.iter().filter(|row| row.2 == 1) { list.push_bind(&row.0); }
                    list.push_unseparated(")");
                    query.push(" ORDER BY r.node_id, r.principal, length(r.path), r.path");
                    let entries: Vec<(String, String, String, String, String)> = query.build_query_as().fetch_all(&mut *connection).await?;
                    for (node, principal, path, key, json) in entries {
                        refs.entry((node, principal)).or_default().push((path, key, json));
                    }
                }
                let legacy = rows.into_iter().map(|(node, principal, format, json)| {
                    let references = refs.remove(&(node.clone(), principal.clone())).unwrap_or_default();
                    (node, principal, format, json, references)
                }).collect();
                let mut prepared = blocking_chunks(legacy, prepare).await?;
                // Deduplicate by content hash across the batch before codec work.
                let mut raw = HashMap::new();
                for (_, _, encoded) in &mut prepared {
                    for (key, bytes) in &mut encoded.contents {
                        let bytes = std::mem::take(bytes);
                        raw.entry(key.clone()).or_insert(bytes);
                    }
                }
                let contents: HashMap<String, Vec<u8>> = blocking_chunks(raw.into_iter().collect(), |(key, bytes)| {
                    Ok((key, crate::storage_codec::encode(bytes.into())?))
                }).await?.into_iter().collect();
                let mut transaction = connection.begin().await?;
                for (node, principal, encoded) in &prepared {
                    $put(&mut transaction, node, principal, encoded, &contents).await?;
                    sqlx::query("UPDATE turn_chain_nodes SET payload = $1, storage_format = 2, legacy_payload = NULL WHERE id = $2")
                        .bind(&encoded.payload).bind(node).execute(&mut *transaction).await?;
                }
                transaction.commit().await?;
                completed += prepared.len() as u64;
                cursor = next_cursor;
                crate::startup_progress::report("history_data", "Converting history", completed, Some(total));
            }
            // Cleanup is restartable and happens only after all worklists committed.
            crate::startup_progress::report("history_data", "Cleaning up migrated history", 0, None);
            sqlx::query("DROP TABLE IF EXISTS turn_chain_legacy_refs").execute(&mut *connection).await?;
            sqlx::query("DROP TABLE IF EXISTS turn_chain_legacy_contents").execute(&mut *connection).await?;
            sqlx::query("ALTER TABLE turn_chain_nodes DROP COLUMN legacy_payload").execute(&mut *connection).await?;
            Ok(())
        }
    }
}
converter!(
    convert_history_sqlite,
    sqlx::SqliteConnection<'_>,
    sqlx::Sqlite,
    put_sqlite_prepared,
    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('turn_chain_nodes') WHERE name = 'legacy_payload')"
);
converter!(
    convert_history_postgres,
    sqlx::PgConnection<'_>,
    sqlx::Postgres,
    put_postgres_prepared,
    "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'turn_chain_nodes' AND column_name = 'legacy_payload')"
);

#[cfg(test)]
mod tests;
