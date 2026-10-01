//! Startup-only legacy decoder. Each batch commits its worklist progress atomically.
use super::content;
use anyhow::{Context, ensure};
use serde_json::Value;
use sqlx::Connection;
use std::collections::HashMap;

type LegacyRef = (String, String, String);
type LegacyNode = (String, String, i64, String, Vec<LegacyRef>);
type PreparedNode = (String, String, content::Encoded);

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
                content::content_key(json.as_bytes()) == key,
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
    Ok((node, principal, content::encode(payload)?))
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
                    Ok((key, crate::storage_codec::encode(&bytes)?))
                }).await?.into_iter().collect();
                let mut transaction = connection.begin().await?;
                for (node, principal, encoded) in &prepared {
                    content::$put(&mut transaction, node, principal, encoded, &contents).await?;
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
    sqlx::SqliteConnection,
    sqlx::Sqlite,
    put_sqlite_prepared,
    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('turn_chain_nodes') WHERE name = 'legacy_payload')"
);
converter!(
    convert_history_postgres,
    sqlx::PgConnection,
    sqlx::Postgres,
    put_postgres_prepared,
    "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'turn_chain_nodes' AND column_name = 'legacy_payload')"
);

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> anyhow::Result<sqlx::SqliteConnection> {
        let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::raw_sql(
            "CREATE TABLE turn_chain_nodes (id TEXT PRIMARY KEY, principal TEXT NOT NULL, storage_format INTEGER NOT NULL, legacy_payload TEXT, payload BLOB);
             CREATE TABLE turn_chain_legacy_refs (node_id TEXT, principal TEXT, path TEXT, content_key TEXT);
             CREATE TABLE turn_chain_legacy_contents (principal TEXT, content_key TEXT, content TEXT, PRIMARY KEY(principal, content_key));
             CREATE TABLE turn_chain_contents (id INTEGER PRIMARY KEY, principal TEXT, content_key TEXT, content BLOB, UNIQUE(principal, content_key));
             CREATE TABLE turn_chain_node_contents (node_id TEXT, principal TEXT, content_id INTEGER, PRIMARY KEY(node_id, content_id));"
        ).execute(&mut connection).await?;
        Ok(connection)
    }

    async fn restored(connection: &mut sqlx::SqliteConnection, id: &str) -> anyhow::Result<Value> {
        let (principal, bytes): (String, Vec<u8>) =
            sqlx::query_as("SELECT principal, payload FROM turn_chain_nodes WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *connection)
                .await?;
        let mut envelope: Value = serde_json::from_slice(&crate::storage_codec::decode(&bytes)?)?;
        let mut data = envelope["data"].take();
        for slot in envelope["slots"].as_array().context("slots")? {
            let key = envelope["contents"][slot[1].as_u64().context("index")? as usize]
                .as_str()
                .context("key")?;
            let bytes: Vec<u8> = sqlx::query_scalar(
                "SELECT content FROM turn_chain_contents WHERE principal = $1 AND content_key = $2",
            )
            .bind(&principal)
            .bind(key)
            .fetch_one(&mut *connection)
            .await?;
            let raw = crate::storage_codec::decode(&bytes)?;
            ensure!(content::content_key(&raw) == key, "content changed");
            *data
                .pointer_mut(slot[0].as_str().context("pointer")?)
                .context("slot")? = serde_json::from_slice(&raw)?;
        }
        Ok(data)
    }

    #[tokio::test]
    async fn resumes_after_batch_rollback_and_restores_scoped_nested_refs() -> anyhow::Result<()> {
        let mut connection = fixture().await?;
        let item = serde_json::json!({"text": "x".repeat(400), "meta": {"phase": "completed"}});
        let plain = serde_json::json!({"items": [item.clone(), null, item.clone()], "unknown": {"keep": true}});
        for index in 0..101 {
            sqlx::query("INSERT INTO turn_chain_nodes VALUES ($1, 'a', 0, $2, NULL)")
                .bind(format!("{index:03}"))
                .bind(plain.to_string())
                .execute(&mut connection)
                .await?;
        }
        let parent_json = serde_json::json!({"items": [null], "preserved": [null, 1]}).to_string();
        let item_json = item.to_string();
        for (path, json) in [("/nested", &parent_json), ("/nested/items/0", &item_json)] {
            let key = content::content_key(json.as_bytes());
            // Same hash in another principal must never be used for restore.
            sqlx::query("INSERT INTO turn_chain_legacy_contents VALUES ('b', $1, 'false')")
                .bind(&key)
                .execute(&mut connection)
                .await?;
            sqlx::query("INSERT INTO turn_chain_legacy_contents VALUES ('a', $1, $2)")
                .bind(&key)
                .bind(json)
                .execute(&mut connection)
                .await?;
            sqlx::query("INSERT INTO turn_chain_legacy_refs VALUES ('101', 'a', $1, $2)")
                .bind(path)
                .bind(&key)
                .execute(&mut connection)
                .await?;
        }
        sqlx::query("INSERT INTO turn_chain_nodes VALUES ('101', 'a', 1, $1, NULL)")
            .bind(serde_json::json!({"references": 2, "data": {"nested": null}}).to_string())
            .execute(&mut connection)
            .await?;
        sqlx::raw_sql("CREATE TRIGGER fail_batch BEFORE UPDATE ON turn_chain_nodes WHEN NEW.id = '101' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;")
            .execute(&mut connection).await?;
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = events.clone();
        assert!(
            crate::startup_progress::observe_startup(
                move |event| observed.lock().unwrap().push(event),
                convert_history_sqlite(&mut connection),
            )
            .await
            .is_err()
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .map(|event| (event.completed, event.total))
                .collect::<Vec<_>>(),
            vec![(0, Some(102)), (100, Some(102))]
        );
        let formats: Vec<(String, i64)> = sqlx::query_as("SELECT id, CAST(storage_format AS BIGINT) FROM turn_chain_nodes WHERE id >= '099' ORDER BY id")
            .fetch_all(&mut connection).await?;
        assert_eq!(
            formats,
            vec![("099".into(), 2), ("100".into(), 0), ("101".into(), 1)]
        );
        assert_eq!(restored(&mut connection, "000").await?, plain);
        sqlx::query("DROP TRIGGER fail_batch")
            .execute(&mut connection)
            .await?;
        events.lock().unwrap().clear();
        let observed = events.clone();
        crate::startup_progress::observe_startup(
            move |event| observed.lock().unwrap().push(event),
            convert_history_sqlite(&mut connection),
        )
        .await?;
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.total.is_some())
                .map(|event| (event.completed, event.total))
                .collect::<Vec<_>>(),
            vec![(0, Some(2)), (2, Some(2))]
        );
        assert_eq!(restored(&mut connection, "100").await?, plain);
        assert_eq!(
            restored(&mut connection, "101").await?,
            serde_json::json!({"nested": {"items": [item], "preserved": [null, 1]}})
        );
        // Clean cutover permits subsequent startup with no legacy tables/column.
        convert_history_sqlite(&mut connection).await?;
        let legacy_column: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pragma_table_info('turn_chain_nodes') WHERE name = 'legacy_payload')")
            .fetch_one(&mut connection).await?;
        assert!(!legacy_column);
        Ok(())
    }

    #[tokio::test]
    async fn resumes_cleanup_after_legacy_tables_were_removed() -> anyhow::Result<()> {
        let mut connection = fixture().await?;
        let payload = serde_json::json!({"preserved": ["completed", null, 7]});
        let encoded = content::encode(payload.clone())?;
        sqlx::query("INSERT INTO turn_chain_nodes VALUES ('done', 'a', 2, NULL, $1)")
            .bind(&encoded.payload)
            .execute(&mut connection)
            .await?;
        sqlx::raw_sql("DROP TABLE turn_chain_legacy_refs; DROP TABLE turn_chain_legacy_contents;")
            .execute(&mut connection)
            .await?;
        convert_history_sqlite(&mut connection).await?;
        assert_eq!(restored(&mut connection, "done").await?, payload);
        let legacy_column: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('turn_chain_nodes') WHERE name='legacy_payload')",
        )
        .fetch_one(&mut connection)
        .await?;
        assert!(!legacy_column);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_damaged_refs_and_pointer_overwrite_without_committing() -> anyhow::Result<()> {
        for (data, content_json) in [
            (serde_json::json!({"slot": null}), "false"),
            (serde_json::json!({"slot": 7}), "true"),
        ] {
            let mut connection = fixture().await?;
            let key = content::content_key(b"true");
            sqlx::query("INSERT INTO turn_chain_nodes VALUES ('n', 'a', 1, $1, NULL)")
                .bind(serde_json::json!({"references": 1, "data": data}).to_string())
                .execute(&mut connection)
                .await?;
            sqlx::query("INSERT INTO turn_chain_legacy_refs VALUES ('n', 'a', '/slot', $1)")
                .bind(&key)
                .execute(&mut connection)
                .await?;
            sqlx::query("INSERT INTO turn_chain_legacy_contents VALUES ('a', $1, $2)")
                .bind(&key)
                .bind(content_json)
                .execute(&mut connection)
                .await?;
            assert!(convert_history_sqlite(&mut connection).await.is_err());
            let format: i64 = sqlx::query_scalar(
                "SELECT CAST(storage_format AS BIGINT) FROM turn_chain_nodes WHERE id = 'n'",
            )
            .fetch_one(&mut connection)
            .await?;
            assert_eq!(format, 1);
            sqlx::query("UPDATE turn_chain_legacy_contents SET content = 'true'")
                .execute(&mut connection)
                .await?;
            sqlx::query("UPDATE turn_chain_nodes SET legacy_payload = '{\"references\":1,\"data\":{\"slot\":null}}'")
                .execute(&mut connection).await?;
            convert_history_sqlite(&mut connection).await?;
            assert_eq!(
                restored(&mut connection, "n").await?,
                serde_json::json!({"slot": true})
            );
        }
        Ok(())
    }
}
