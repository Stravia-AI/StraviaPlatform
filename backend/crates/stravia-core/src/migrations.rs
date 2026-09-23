use anyhow::ensure;
use sqlx::{PgPool, Sqlite, SqlitePool, migrate::Migrator, pool::PoolConnection};

static SQLITE_MIGRATOR: Migrator = sqlx::migrate!("./migrations/sqlite");
static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");

const INCOMPATIBLE_HISTORY: &str =
    "database has unknown, missing, or failed Stravia migration history; refusing to migrate";
const UNRECOGNIZED_TABLES: &str = "database contains tables but no Stravia migration history; refusing to initialize over unrecognized data";

/// An installed history must be a successful, checksum-matching contiguous
/// prefix of the migrations supplied by the caller (including offline copies).
fn supported_history(rows: &[(i64, bool, Vec<u8>)], migrator: &Migrator) -> anyhow::Result<()> {
    ensure!(
        rows.iter()
            .zip(migrator.iter())
            .all(|((version, success, checksum), migration)| {
                *success
                    && *version == migration.version
                    && checksum.as_slice() == migration.checksum.as_ref()
            }),
        "{INCOMPATIBLE_HISTORY}"
    );
    ensure!(
        rows.len() <= migrator.iter().count(),
        "{INCOMPATIBLE_HISTORY}"
    );
    Ok(())
}

/// Offline copy accepts an existing supported prefix; startup upgrades the copy.
pub async fn check_sqlite_source_history(
    connection: &mut sqlx::SqliteConnection,
) -> anyhow::Result<()> {
    reject_incompatible_sqlite(connection, &SQLITE_MIGRATOR).await
}

struct SqliteMigrationConnection {
    connection: PoolConnection<Sqlite>,
    foreign_keys_may_be_off: bool,
}

impl Drop for SqliteMigrationConnection {
    fn drop(&mut self) {
        if self.foreign_keys_may_be_off {
            self.connection.close_on_drop();
        }
    }
}

pub async fn migrate_sqlite(pool: &SqlitePool) -> anyhow::Result<()> {
    run_sqlite_migrator(pool, &SQLITE_MIGRATOR).await
}

/// Used by the isolated schema exporter with its runtime-resolved migration directory.
pub async fn run_sqlite_migrator(pool: &SqlitePool, migrator: &Migrator) -> anyhow::Result<()> {
    let mut pinned = SqliteMigrationConnection {
        connection: pool.acquire().await?,
        // Arm before the first await which can disable FK enforcement.
        foreign_keys_may_be_off: true,
    };
    reject_incompatible_sqlite(&mut pinned.connection, migrator).await?;
    let existing_violations: Vec<(String, i64, String, i64)> =
        sqlx::query_as("PRAGMA foreign_key_check")
            .fetch_all(&mut *pinned.connection)
            .await?;
    ensure!(
        existing_violations.is_empty(),
        "database already has foreign key violations: {existing_violations:?}"
    );
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut *pinned.connection)
        .await?;
    migrator
        .run_direct(None, &mut *pinned.connection, false)
        .await?;
    let violations: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(&mut *pinned.connection)
        .await?;
    ensure!(
        violations.is_empty(),
        "migration created foreign key violations: {violations:?}"
    );
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut *pinned.connection)
        .await?;
    let restored: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut *pinned.connection)
        .await?;
    ensure!(restored == 1, "SQLite foreign keys could not be restored");
    pinned.foreign_keys_may_be_off = false;
    Ok(())
}

pub async fn migrate_postgres(pool: &PgPool) -> anyhow::Result<()> {
    run_postgres_migrator(pool, &POSTGRES_MIGRATOR).await
}

/// Isolate SQLx's session-level advisory lock, including in schema exports.
pub async fn run_postgres_migrator(pool: &PgPool, migrator: &Migrator) -> anyhow::Result<()> {
    let mut connection = pool.acquire().await?;
    // SQLx 0.9 can leave its session-level advisory lock held after a failed
    // migration. Never return this migration session to the pool.
    connection.close_on_drop();
    reject_incompatible_postgres(&mut connection, migrator).await?;
    migrator.run_direct(None, &mut *connection, false).await?;
    Ok(())
}

async fn reject_incompatible_sqlite(
    connection: &mut sqlx::SqliteConnection,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let has_history: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(&mut *connection)
    .await?;
    if has_history {
        let rows: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as(
            "SELECT version, success = 1, checksum FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            let tables: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
            )
            .fetch_one(&mut *connection)
            .await?;
            ensure!(tables == 0, "{UNRECOGNIZED_TABLES}");
        }
        supported_history(&rows, migrator)?;
    } else {
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_one(&mut *connection)
        .await?;
        ensure!(tables == 0, "{UNRECOGNIZED_TABLES}");
    }
    Ok(())
}

async fn reject_incompatible_postgres(
    connection: &mut sqlx::PgConnection,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let has_history: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = current_schema() AND table_name = '_sqlx_migrations')",
    )
    .fetch_one(&mut *connection)
    .await?;
    if has_history {
        let rows: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as(
            "SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            let tables: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' AND table_name <> '_sqlx_migrations'",
            )
            .fetch_one(&mut *connection)
            .await?;
            ensure!(tables == 0, "{UNRECOGNIZED_TABLES}");
        }
        supported_history(&rows, migrator)?;
    } else {
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'",
        )
        .fetch_one(&mut *connection)
        .await?;
        ensure!(tables == 0, "{UNRECOGNIZED_TABLES}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SQLITE_MIGRATOR, migrate_postgres, migrate_sqlite};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn sqlite_pool() -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool")
    }

    #[tokio::test]
    async fn sqlite_baseline_applies_to_empty_database_and_is_idempotent() {
        let pool = sqlite_pool().await;
        migrate_sqlite(&pool).await.expect("fresh baseline");
        migrate_sqlite(&pool)
            .await
            .expect("baseline re-run is a no-op");

        sqlx::query("INSERT INTO api_keys (id, token, name) VALUES ('k', 'sk-test', 'Key')")
            .execute(&pool)
            .await
            .expect("insert relying on column defaults");
        let (enabled, mcp, media): (bool, bool, bool) = sqlx::query_as(
            "SELECT is_enabled, mcp_access_enabled, inject_media_generation FROM api_keys WHERE id = 'k'",
        )
        .fetch_one(&pool)
        .await
        .expect("defaults materialized");
        assert!(enabled);
        assert!(!mcp);
        assert!(!media);

        let (use_proxy, api_key, engines): (bool, Option<String>, String) = sqlx::query_as(
            "SELECT use_proxy, api_key, local_engines FROM web_providers WHERE id = 'web-provider-local'",
        )
        .fetch_one(&pool)
        .await
        .expect("built-in Local Web Provider");
        assert!(!use_proxy);
        assert!(api_key.is_none());
        let engines: serde_json::Value = serde_json::from_str(&engines).unwrap();
        assert_eq!(engines["google"]["enabled"], true);
        assert_eq!(engines["google_scholar"]["enabled"], false);
        for key in [
            "web_access_search_provider_ids",
            "web_access_fetch_provider_ids",
        ] {
            let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE name = ?")
                .bind(key)
                .fetch_one(&pool)
                .await
                .expect("initial Web Access priority");
            assert_eq!(
                serde_json::from_str::<Vec<String>>(&value).unwrap(),
                ["web-provider-local"]
            );
        }
        assert!(
            sqlx::query("INSERT INTO web_providers (id, name, kind, api_key) VALUES ('removed', 'Removed', 'tavily', 'secret')")
                .execute(&pool)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn sqlite_v1_upgrade_preserves_history_and_enforces_contracts() {
        let pool = sqlite_pool().await;
        SQLITE_MIGRATOR.run_to(1, &pool).await.unwrap();
        sqlx::query("INSERT INTO providers (id, name, protocol, base_url, api_key) VALUES ('p', 'Provider', 'openai', 'https://example.invalid', '')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO provider_models (provider_id, model_id, source_kind, presence, metadata_json) VALUES ('p', 'upstream', 'manual', 'present', '{\"name\":\"original\"}')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO models (id, model_id) VALUES ('r', 'route')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO model_backends (id, model_id, provider_id, priority, target_retry_budget) VALUES ('t', 'r', 'p', -3, 3)")
            .execute(&pool).await.unwrap();
        for (id, parent) in [("parent", None), ("child", Some("parent"))] {
            sqlx::query("INSERT INTO turn_chain_nodes (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) VALUES (?, 'response', ?, 'alice', 1, '{\"items\":[1]}', 1, 100)")
                .bind(id).bind(parent).execute(&pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO turn_chain_contents VALUES ('alice', 'content', 'historical payload')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO turn_chain_content_refs VALUES ('child', 'alice', '$.items[0]', 'content')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO interaction_observations (id, principal, root_id, root_run_id, first_route_id, status, started_at, last_active_at, expires_at) VALUES ('interaction', 'alice', 'interaction', 'run', 'r', 'active', 1, 1, 100)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO inference_run_observations (id, interaction_id, ingress_protocol, route_id, status, background_active, debug_enabled, started_at, last_active_at, expires_at) VALUES ('run', 'interaction', 'openai', 'r', 'active', 2, 0, 1, 1, 100)")
            .execute(&pool).await.unwrap();

        migrate_sqlite(&pool).await.unwrap();
        migrate_sqlite(&pool).await.unwrap();
        let (snapshot, metadata): (String, String) = sqlx::query_as("SELECT snapshot_state, metadata_json FROM provider_models WHERE provider_id='p' AND model_id='upstream'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(snapshot, "{\"type\":\"edited\",\"source\":null}");
        assert_eq!(metadata, "{\"name\":\"original\"}");
        assert!(sqlx::query("UPDATE provider_models SET snapshot_state='{\"type\":\"imported\",\"source\":{\"type\":\"canonical\",\"model_id\":\"c\"}}' WHERE provider_id='p' AND model_id='upstream'")
            .execute(&pool).await.is_ok());
        assert!(sqlx::query("UPDATE provider_models SET snapshot_state='{\"type\":\"imported\",\"source\":{\"type\":\"canonical\"}}' WHERE provider_id='p' AND model_id='upstream'")
            .execute(&pool).await.is_err());
        assert!(sqlx::query("UPDATE provider_models SET snapshot_state='not json' WHERE provider_id='p' AND model_id='upstream'")
            .execute(&pool).await.is_err());
        let retained: (String, String, i64) = sqlx::query_as("SELECT n.payload, c.content, (SELECT background_active FROM inference_run_observations WHERE id='run') FROM turn_chain_nodes n JOIN turn_chain_content_refs r ON r.node_id=n.id JOIN turn_chain_contents c ON c.principal=r.principal AND c.content_key=r.content_key WHERE n.id='child'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(
            retained,
            ("{\"items\":[1]}".into(), "historical payload".into(), 2)
        );
        sqlx::query("UPDATE inference_run_observations SET background_active = 3 WHERE id = 'run'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            sqlx::query(
                "UPDATE inference_run_observations SET background_active = -1 WHERE id = 'run'"
            )
            .execute(&pool)
            .await
            .is_err()
        );
        assert!(
            sqlx::query("UPDATE model_backends SET priority = 2147483648 WHERE id = 't'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE model_backends SET thinking_level_map = '{}' WHERE id = 't'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE models SET balance = 'unrecognized' WHERE id = 'r'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE providers SET vendor_options = 'not json' WHERE id = 'p'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(sqlx::query("INSERT INTO turn_chain_nodes (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) VALUES ('foreign', 'response', 'parent', 'bob', 1, '{}', 1, 100)")
            .execute(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO turn_chain_nodes (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) VALUES ('wrong-kind', 'agent', 'parent', 'alice', 1, '{}', 1, 100)")
            .execute(&pool).await.is_err());
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[tokio::test]
    async fn sqlite_rejects_unversioned_tables() {
        let pool = sqlite_pool().await;
        sqlx::query("CREATE TABLE stray (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let error = migrate_sqlite(&pool).await.unwrap_err().to_string();
        assert!(error.contains("unrecognized"), "{error}");
    }

    #[tokio::test]
    async fn sqlite_rejects_foreign_migration_history() {
        let pool = sqlite_pool().await;
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (\
             version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?, 'historical migration', 1, X'00', 0)",
        )
        .bind(SQLITE_MIGRATOR.iter().last().unwrap().version + 1)
        .execute(&pool)
        .await
        .unwrap();
        let error = migrate_sqlite(&pool).await.unwrap_err().to_string();
        assert!(error.contains("unknown, missing, or failed"), "{error}");
    }

    #[tokio::test]
    async fn sqlite_interrupted_migration_never_returns_fk_off_connection() {
        let pool = sqlite_pool().await;
        let mut pinned = super::SqliteMigrationConnection {
            connection: pool.acquire().await.unwrap(),
            foreign_keys_may_be_off: true,
        };
        sqlx::query("PRAGMA foreign_keys=OFF")
            .execute(&mut *pinned.connection)
            .await
            .unwrap();
        drop(pinned); // Equivalent to cancelling after FK-off and before SQLx finishes.
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(foreign_keys, 1);
        migrate_sqlite(&pool).await.unwrap();
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn history_must_be_a_successful_embedded_prefix() {
        let first = SQLITE_MIGRATOR.iter().next().unwrap();
        let second = SQLITE_MIGRATOR.iter().nth(1).unwrap();
        let v1 = (first.version, true, first.checksum.to_vec());
        let v2 = (second.version, true, second.checksum.to_vec());
        super::supported_history(&[], &SQLITE_MIGRATOR).unwrap();
        super::supported_history(std::slice::from_ref(&v1), &SQLITE_MIGRATOR).unwrap();
        super::supported_history(&[v1.clone(), v2.clone()], &SQLITE_MIGRATOR).unwrap();
        for history in [
            vec![(second.version, true, second.checksum.to_vec())],
            vec![(first.version, false, first.checksum.to_vec())],
            vec![
                v1.clone(),
                (second.version + 1, true, second.checksum.to_vec()),
            ],
            vec![
                v1.clone(),
                (second.version, false, second.checksum.to_vec()),
            ],
            vec![(first.version, true, vec![0])],
        ] {
            assert!(super::supported_history(&history, &SQLITE_MIGRATOR).is_err());
        }
    }

    #[tokio::test]
    async fn sqlite_invalid_v1_data_rolls_back_without_dropping_original_tables() {
        let directory = tempfile::tempdir().unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(directory.path().join("legacy.db"))
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        SQLITE_MIGRATOR.run_to(1, &pool).await.unwrap();
        sqlx::query("INSERT INTO models (id, model_id, balance) VALUES ('r', 'route', 'invalid')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(migrate_sqlite(&pool).await.is_err());
        let original: String = sqlx::query_scalar("SELECT balance FROM models WHERE id = 'r'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(original, "invalid");
        let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, 1);
        sqlx::query("UPDATE models SET balance='traffic_equalization' WHERE id='r'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO turn_chain_nodes (id, kind, principal, payload_version, payload, created_at, expires_at) VALUES ('parent', 'response', 'alice', 1, '{}', 1, 2)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO turn_chain_nodes (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) VALUES ('child', 'response', 'parent', 'bob', 1, '{}', 1, 2)")
            .execute(&pool).await.unwrap();
        assert!(migrate_sqlite(&pool).await.is_err());
        let legacy_parent: String =
            sqlx::query_scalar("SELECT parent_id FROM turn_chain_nodes WHERE id='child'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(legacy_parent, "parent");
        sqlx::query("UPDATE turn_chain_nodes SET principal='alice' WHERE id='child'")
            .execute(&pool)
            .await
            .unwrap();
        migrate_sqlite(&pool).await.unwrap();
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[tokio::test]
    async fn postgres_baseline_applies_when_configured() {
        let Some(url) = std::env::var("DB_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())
        else {
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.unwrap();
        let schema = format!("stravia_baseline_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await
            .unwrap();
        super::POSTGRES_MIGRATOR
            .run_to(1, &pool)
            .await
            .expect("v1 baseline");
        sqlx::query(
            "INSERT INTO models (id, model_id, balance) VALUES ('invalid', 'invalid', 'unknown')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), migrate_postgres(&pool))
                .await
                .expect("migration failure must not hang")
                .is_err()
        );
        sqlx::query("UPDATE models SET balance='traffic_equalization' WHERE id='invalid'")
            .execute(&pool)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), migrate_postgres(&pool))
            .await
            .expect("failed migration leaked advisory lock")
            .expect("v1 upgrade");
        migrate_postgres(&pool)
            .await
            .expect("upgrade re-run is a no-op");
        sqlx::query("INSERT INTO interaction_observations (id, principal, root_id, root_run_id, first_route_id, status, started_at, last_active_at, expires_at) VALUES ('interaction', 'alice', 'interaction', 'run', 'r', 'active', 1, 1, 100)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO inference_run_observations (id, interaction_id, ingress_protocol, route_id, status, background_active, debug_enabled, started_at, last_active_at, expires_at) VALUES ('run', 'interaction', 'openai', 'r', 'active', 2, false, 1, 1, 100)")
            .execute(&pool).await.unwrap();
        sqlx::query("UPDATE inference_run_observations SET background_active=3 WHERE id='run'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            sqlx::query(
                "UPDATE inference_run_observations SET background_active=-1 WHERE id='run'"
            )
            .execute(&pool)
            .await
            .is_err()
        );

        let (use_proxy, api_key, engines): (bool, Option<String>, serde_json::Value) =
            sqlx::query_as(
                "SELECT use_proxy, api_key, local_engines FROM web_providers WHERE id = 'web-provider-local'",
            )
            .fetch_one(&pool)
            .await
            .expect("built-in Local Web Provider");
        assert!(!use_proxy);
        assert!(api_key.is_none());
        assert_eq!(engines["google"]["enabled"], true);
        assert_eq!(engines["google_scholar"]["enabled"], false);
        for key in [
            "web_access_search_provider_ids",
            "web_access_fetch_provider_ids",
        ] {
            let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE name = $1")
                .bind(key)
                .fetch_one(&pool)
                .await
                .expect("initial Web Access priority");
            assert_eq!(
                serde_json::from_str::<Vec<String>>(&value).unwrap(),
                ["web-provider-local"]
            );
        }
        assert!(
            sqlx::query("INSERT INTO web_providers (id, name, kind, api_key) VALUES ('removed', 'Removed', 'tavily', 'secret')")
                .execute(&pool)
                .await
                .is_err()
        );
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
