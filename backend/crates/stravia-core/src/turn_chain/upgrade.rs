//! Startup-only legacy decoder. Each batch commits its worklist progress atomically.
use super::content;
use anyhow::{Context, ensure};
use serde_json::Value;
use sqlx::Connection;

macro_rules! converter {
    ($name:ident, $conn:ty, $put:ident, $exists:literal) => {
        pub(crate) async fn $name(connection: &mut $conn) -> anyhow::Result<()> {
            let has_legacy: bool = sqlx::query_scalar($exists).fetch_one(&mut *connection).await?;
            if !has_legacy { return Ok(()); }
            loop {
                let mut transaction = connection.begin().await?;
                let rows: Vec<(String, String, i64, String)> = sqlx::query_as("SELECT id, principal, CAST(storage_format AS BIGINT), legacy_payload FROM turn_chain_nodes WHERE storage_format < 2 ORDER BY id LIMIT 100")
                    .fetch_all(&mut *transaction).await?;
                if rows.is_empty() { transaction.commit().await?; break; }
                for (node, principal, format, json) in rows {
                    let mut payload: Value = serde_json::from_str(&json)?;
                    if format == 1 {
                        let expected = payload["references"].as_u64().context("invalid legacy reference count")?;
                        let refs: Vec<(String, String, String)> = sqlx::query_as("SELECT r.path, r.content_key, c.content FROM turn_chain_legacy_refs r JOIN turn_chain_legacy_contents c ON c.principal = r.principal AND c.content_key = r.content_key WHERE r.node_id = $1 AND r.principal = $2 ORDER BY length(r.path), r.path")
                            .bind(&node).bind(&principal).fetch_all(&mut *transaction).await?;
                        ensure!(expected > 0 && expected == refs.len() as u64, "incomplete legacy history references");
                        payload = payload.get_mut("data").context("missing legacy history data")?.take();
                        for (path, key, json) in refs {
                            ensure!(content::content_key(json.as_bytes()) == key, "damaged legacy history content");
                            let slot = payload.pointer_mut(&path).context("invalid legacy history pointer")?;
                            ensure!(slot.is_null(), "legacy history pointer overwrites data");
                            *slot = serde_json::from_str(&json)?;
                        }
                    } else { ensure!(format == 0, "unsupported legacy history format"); }
                    let encoded = content::encode(payload)?;
                    content::$put(&mut transaction, &node, &principal, &encoded).await?;
                    sqlx::query("UPDATE turn_chain_nodes SET payload = $1, storage_format = 2, legacy_payload = NULL WHERE id = $2")
                        .bind(&encoded.payload).bind(&node).execute(&mut *transaction).await?;
                }
                transaction.commit().await?;
            }
            // DROP IF EXISTS permits restart after conversion but before subsequent migrations.
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
    put_sqlite,
    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('turn_chain_nodes') WHERE name = 'legacy_payload')"
);
converter!(
    convert_history_postgres,
    sqlx::PgConnection,
    put_postgres,
    "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'turn_chain_nodes' AND column_name = 'legacy_payload')"
);
