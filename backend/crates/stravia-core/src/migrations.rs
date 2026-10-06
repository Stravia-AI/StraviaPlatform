use std::path::Path;

use crate::startup_progress::report;
use anyhow::ensure;
use sha2::{Digest, Sha384};
use sqlx::{
    PgPool, Sqlite, SqlitePool,
    migrate::{Migrate, Migrator},
    pool::PoolConnection,
};

static SQLITE_MIGRATOR: Migrator = sqlx::migrate!("./migrations/sqlite");
static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");

const INCOMPATIBLE_HISTORY: &str =
    "database has unknown, missing, or failed Stravia migration history; refusing to migrate";
const UNRECOGNIZED_TABLES: &str = "database contains tables but no Stravia migration history; refusing to initialize over unrecognized data";

fn unlocked_runner(migrator: &Migrator) -> Migrator {
    Migrator {
        migrations: migrator.migrations.clone(),
        ignore_missing: migrator.ignore_missing,
        locking: false,
        no_tx: migrator.no_tx,
        table_name: migrator.table_name.clone(),
        create_schemas: migrator.create_schemas.clone(),
    }
}

fn matches_line_ending_checksum(sql: &str, checksum: &[u8]) -> bool {
    let mut lf = Sha384::new();
    let mut crlf = Sha384::new();
    for line in sql.as_bytes().split_inclusive(|byte| *byte == b'\n') {
        if let Some(content) = line.strip_suffix(b"\n") {
            let content = content.strip_suffix(b"\r").unwrap_or(content);
            lf.update(content);
            lf.update(b"\n");
            crlf.update(content);
            crlf.update(b"\r\n");
        } else {
            lf.update(line);
            crlf.update(line);
        }
    }
    checksum == &lf.finalize()[..] || checksum == &crlf.finalize()[..]
}

/// 接受成功、连续的迁移前缀；只有同一 SQL 的 LF/CRLF 表示可使用历史校验和。
fn supported_history(rows: &[(i64, bool, Vec<u8>)], migrator: &mut Migrator) -> anyhow::Result<()> {
    ensure!(
        rows.len() <= migrator.iter().count(),
        "{INCOMPATIBLE_HISTORY}"
    );
    for (index, (version, success, checksum)) in rows.iter().enumerate() {
        let migration = &migrator.migrations[index];
        ensure!(
            *success && *version == migration.version,
            "{INCOMPATIBLE_HISTORY}"
        );
        if checksum.as_slice() != migration.checksum.as_ref() {
            ensure!(
                matches_line_ending_checksum(migration.sql.as_str(), checksum),
                "{INCOMPATIBLE_HISTORY}"
            );
            // SQLx 会再次校验历史。只调整本次 runner，保留数据库中的原始记录。
            migrator.migrations.to_mut()[index].checksum = checksum.clone().into();
        }
    }
    Ok(())
}

/// Offline copy accepts an existing supported prefix; startup upgrades the copy.
pub async fn check_sqlite_source_history(
    connection: &mut sqlx::SqliteConnection,
) -> anyhow::Result<()> {
    let mut runner = unlocked_runner(&SQLITE_MIGRATOR);
    reject_incompatible_sqlite(connection, &mut runner).await
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

/// 应用受支持的增量迁移并转换历史数据。旧 Debug manifest 非空时必须提供诊断目录。
pub async fn migrate_sqlite(pool: &SqlitePool, diagnostics: Option<&Path>) -> anyhow::Result<()> {
    run_sqlite_migrator(pool, &SQLITE_MIGRATOR, diagnostics).await
}

/// Used by the isolated schema exporter with its runtime-resolved migration directory.
pub async fn run_sqlite_migrator(
    pool: &SqlitePool,
    migrator: &Migrator,
    diagnostics: Option<&Path>,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    tracing::info!(backend = "sqlite", "Starting database migration");
    report(
        "migration_validate",
        "Checking database migration history",
        0,
        None,
    );
    let mut pinned = SqliteMigrationConnection {
        connection: pool.acquire().await?,
        // Arm before the first await which can disable FK enforcement.
        foreign_keys_may_be_off: true,
    };
    let mut runner = unlocked_runner(migrator);
    reject_incompatible_sqlite(&mut pinned.connection, &mut runner).await?;
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
    report(
        "migration_validate",
        "Waiting for database migration lock",
        0,
        None,
    );
    Migrate::lock(&mut *pinned.connection).await?;
    if runner.version_exists(7) {
        report("history_schema", "Preparing history schema", 0, None);
        runner
            .run_direct(Some(6), &mut *pinned.connection, false)
            .await?;
        runner
            .run_direct(Some(7), &mut *pinned.connection, false)
            .await?;
        report("history_data", "Checking history conversion", 0, None);
        crate::turn_chain::upgrade::convert_history_sqlite(&mut pinned.connection).await?;
    }
    if runner.version_exists(8) {
        report("debug_manifests", "Checking diagnostic manifests", 0, None);
        crate::interaction_observation::upgrade::export_debug_manifests_sqlite(
            &mut pinned.connection,
            diagnostics,
        )
        .await?;
        report(
            "observation_schema",
            "Preparing observation schema",
            0,
            None,
        );
        runner
            .run_direct(Some(8), &mut *pinned.connection, false)
            .await?;
        report(
            "observation_data",
            "Checking observation conversion",
            0,
            None,
        );
        crate::interaction_observation::upgrade::convert_event_storage_sqlite(
            &mut pinned.connection,
        )
        .await?;
    }
    report(
        "schema_migrations",
        "Applying database schema migrations",
        0,
        None,
    );
    runner
        .run_direct(None, &mut *pinned.connection, false)
        .await?;
    report("migration_verify", "Verifying database integrity", 0, None);
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
    Migrate::unlock(&mut *pinned.connection).await?;
    pinned.foreign_keys_may_be_off = false;
    tracing::info!(
        backend = "sqlite",
        elapsed_ms = started.elapsed().as_millis(),
        "Database migration completed"
    );
    Ok(())
}

/// 应用受支持的增量迁移并转换历史数据。旧 Debug manifest 非空时必须提供诊断目录。
pub async fn migrate_postgres(pool: &PgPool, diagnostics: Option<&Path>) -> anyhow::Result<()> {
    run_postgres_migrator(pool, &POSTGRES_MIGRATOR, diagnostics).await
}

/// Isolate SQLx's session-level advisory lock, including in schema exports.
pub async fn run_postgres_migrator(
    pool: &PgPool,
    migrator: &Migrator,
    diagnostics: Option<&Path>,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    tracing::info!(backend = "postgres", "Starting database migration");
    report(
        "migration_validate",
        "Waiting for database migration lock",
        0,
        None,
    );
    let mut connection = pool.acquire().await?;
    // SQLx 0.9 can leave its session-level advisory lock held after a failed
    // migration. Never return this migration session to the pool.
    connection.close_on_drop();
    Migrate::lock(&mut *connection).await?;
    report(
        "migration_validate",
        "Checking database migration history",
        0,
        None,
    );
    let mut runner = unlocked_runner(migrator);
    reject_incompatible_postgres(&mut connection, &mut runner).await?;
    if runner.version_exists(7) {
        report("history_schema", "Preparing history schema", 0, None);
        runner.run_direct(Some(6), &mut *connection, false).await?;
        runner.run_direct(Some(7), &mut *connection, false).await?;
        report("history_data", "Checking history conversion", 0, None);
        crate::turn_chain::upgrade::convert_history_postgres(&mut connection).await?;
    }
    if runner.version_exists(8) {
        report("debug_manifests", "Checking diagnostic manifests", 0, None);
        crate::interaction_observation::upgrade::export_debug_manifests_postgres(
            &mut connection,
            diagnostics,
        )
        .await?;
        report(
            "observation_schema",
            "Preparing observation schema",
            0,
            None,
        );
        runner.run_direct(Some(8), &mut *connection, false).await?;
        report(
            "observation_data",
            "Checking observation conversion",
            0,
            None,
        );
        crate::interaction_observation::upgrade::convert_event_storage_postgres(&mut connection)
            .await?;
    }
    report(
        "schema_migrations",
        "Applying database schema migrations",
        0,
        None,
    );
    runner.run_direct(None, &mut *connection, false).await?;
    report(
        "migration_verify",
        "Releasing database migration lock",
        0,
        None,
    );
    Migrate::unlock(&mut *connection).await?;
    tracing::info!(
        backend = "postgres",
        elapsed_ms = started.elapsed().as_millis(),
        "Database migration completed"
    );
    Ok(())
}

async fn reject_incompatible_sqlite(
    connection: &mut sqlx::SqliteConnection,
    migrator: &mut Migrator,
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
    migrator: &mut Migrator,
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
    use sqlx::{
        SqlSafeStr,
        migrate::{Migration, MigrationType, Migrator},
        sqlite::SqlitePoolOptions,
    };

    const RETAINED_VALUE_SQL: &str = "CREATE TABLE retained_value (\n    id INTEGER PRIMARY KEY,\n    value TEXT NOT NULL\n);\nINSERT INTO retained_value VALUES (1, 'kept');\n";

    fn retained_value_migrator(sql: String, upgrade: bool) -> Migrator {
        let mut migrations = vec![Migration::new(
            1,
            "retained value".into(),
            MigrationType::Simple,
            sqlx::AssertSqlSafe(sql).into_sql_str(),
            false,
        )];
        if upgrade {
            migrations.push(Migration::new(
                2,
                "upgrade version".into(),
                MigrationType::Simple,
                "ALTER TABLE retained_value ADD COLUMN upgrade_version INTEGER NOT NULL DEFAULT 2;\n"
                    .into_sql_str(),
                false,
            ));
        }
        Migrator::with_migrations(migrations)
    }

    async fn sqlite_pool() -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool")
    }

    #[tokio::test]
    async fn sqlite_crlf_history_upgrades_without_rewriting_applied_migrations() {
        for (historical, current_sql) in [
            (
                RETAINED_VALUE_SQL.replace('\n', "\r\n"),
                RETAINED_VALUE_SQL.to_owned(),
            ),
            (
                RETAINED_VALUE_SQL.to_owned(),
                RETAINED_VALUE_SQL.replace('\n', "\r\n"),
            ),
        ] {
            let pool = sqlite_pool().await;
            retained_value_migrator(historical, false)
                .run(&pool)
                .await
                .unwrap();
            let installed: (Vec<u8>, String, i64, bool) = sqlx::query_as(
                "SELECT checksum, CAST(installed_on AS TEXT), execution_time, success FROM _sqlx_migrations WHERE version=1",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            let current = retained_value_migrator(current_sql, true);

            super::run_sqlite_migrator(&pool, &current, None)
                .await
                .expect("the same SQL must accept its other line-ending checksum");
            super::run_sqlite_migrator(&pool, &current, None)
                .await
                .expect("restarting must preserve the historical checksum");
            let retained: (String, i64) =
                sqlx::query_as("SELECT value, upgrade_version FROM retained_value WHERE id=1")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(retained, ("kept".into(), 2));
            let after: (Vec<u8>, String, i64, bool) = sqlx::query_as(
                "SELECT checksum, CAST(installed_on AS TEXT), execution_time, success FROM _sqlx_migrations WHERE version=1",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(after, installed);
        }
    }

    #[tokio::test]
    async fn sqlite_rejects_changes_beyond_lf_crlf_without_modifying_data() {
        for historical in [
            RETAINED_VALUE_SQL.replace("'kept'", "'edited'"),
            RETAINED_VALUE_SQL.trim_end().to_owned(),
            format!("{RETAINED_VALUE_SQL}\n"),
            RETAINED_VALUE_SQL.replacen('\n', "\r", 1),
            RETAINED_VALUE_SQL.replacen('\n', "\r\n", 1),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(directory.path().join("history.db"))
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            retained_value_migrator(historical, false)
                .run(&pool)
                .await
                .unwrap();
            let before: Vec<(i64, String)> = sqlx::query_as("SELECT id, value FROM retained_value")
                .fetch_all(&pool)
                .await
                .unwrap();
            let installed: Vec<(i64, Vec<u8>)> =
                sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let current = retained_value_migrator(RETAINED_VALUE_SQL.to_owned(), true);

            assert!(
                super::run_sqlite_migrator(&pool, &current, None)
                    .await
                    .is_err()
            );
            let after: Vec<(i64, String)> = sqlx::query_as("SELECT id, value FROM retained_value")
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(after, before);
            let columns: Vec<String> = sqlx::query_scalar(
                "SELECT name FROM pragma_table_info('retained_value') ORDER BY cid",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(columns, ["id", "value"]);
            let history: Vec<(i64, Vec<u8>)> =
                sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(history, installed);
        }
    }

    #[tokio::test]
    async fn sqlite_offline_copy_and_startup_preserve_crlf_allowance_migration_history() {
        let pool = sqlite_pool().await;
        let mut historical = super::unlocked_runner(&SQLITE_MIGRATOR);
        let migration = historical
            .migrations
            .to_mut()
            .iter_mut()
            .find(|migration| migration.version == 10)
            .unwrap();
        *migration = Migration::new(
            migration.version,
            migration.description.clone(),
            migration.migration_type,
            sqlx::AssertSqlSafe(migration.sql.as_str().replace('\n', "\r\n")).into_sql_str(),
            migration.no_tx,
        );
        super::run_sqlite_migrator(&pool, &historical, None)
            .await
            .unwrap();
        let installed: Vec<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();

        super::check_sqlite_source_history(&mut pool.acquire().await.unwrap())
            .await
            .expect("offline copies must accept historical CRLF SQL without repairing metadata");
        migrate_sqlite(&pool, None).await.unwrap();
        let after: Vec<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(after, installed);
    }

    #[tokio::test]
    async fn postgres_crlf_history_upgrades_but_sql_changes_are_rejected_when_configured() {
        let Some(url) = std::env::var("DB_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())
        else {
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.unwrap();
        let schema = format!("stravia_newline_test_{}", uuid::Uuid::new_v4().simple());
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
        retained_value_migrator(RETAINED_VALUE_SQL.replace('\n', "\r\n"), false)
            .run(&pool)
            .await
            .unwrap();
        let installed: (Vec<u8>, String, i64, bool) = sqlx::query_as(
            "SELECT checksum, CAST(installed_on AS TEXT), execution_time, success FROM _sqlx_migrations WHERE version=1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let current = retained_value_migrator(RETAINED_VALUE_SQL.to_owned(), true);

        super::run_postgres_migrator(&pool, &current, None)
            .await
            .unwrap();
        super::run_postgres_migrator(&pool, &current, None)
            .await
            .unwrap();
        let changed =
            retained_value_migrator(RETAINED_VALUE_SQL.replace("'kept'", "'edited'"), true);
        assert!(
            super::run_postgres_migrator(&pool, &changed, None)
                .await
                .is_err()
        );
        let retained: (String, i32) =
            sqlx::query_as("SELECT value, upgrade_version FROM retained_value WHERE id=1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(retained, ("kept".into(), 2));
        let after: (Vec<u8>, String, i64, bool) = sqlx::query_as(
            "SELECT checksum, CAST(installed_on AS TEXT), execution_time, success FROM _sqlx_migrations WHERE version=1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after, installed);
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }

    #[tokio::test]
    async fn sqlite_baseline_applies_to_empty_database_and_is_idempotent() {
        let pool = sqlite_pool().await;
        migrate_sqlite(&pool, None).await.expect("fresh baseline");
        migrate_sqlite(&pool, None)
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
        sqlx::query("INSERT INTO model_backends (id, model_id, provider_id, model, priority, target_retry_budget) VALUES ('t', 'r', 'p', 'm', -3, 3)")
            .execute(&pool).await.unwrap();
        let expires_at = chrono::Utc::now().timestamp_millis() + 60_000;
        for (id, parent, payload, format) in [
            ("parent", None, r#"{"items":[1]}"#, 0),
            (
                "child",
                Some("parent"),
                r#"{"data":{"items":[null]},"references":1}"#,
                1,
            ),
        ] {
            sqlx::query("INSERT INTO turn_chain_nodes (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at, storage_format) VALUES (?, 'response', ?, 'api-key:alice', 1, ?, 1, ?, ?)")
                .bind(id).bind(parent).bind(payload).bind(expires_at).bind(format).execute(&pool).await.unwrap();
        }
        use sha2::Digest;
        let marker_reference = "abcdefghijklmnopqrstuvwxyzab";
        let artifact_id = stravia_runtime_contract::identifier::encode_digest(
            &sha2::Sha256::digest(b"isolated migration media").into(),
        );
        let media = serde_json::json!({
            "type": "image", "source": {"type": "url", "url": format!("stravia://artifacts/{artifact_id}")}, "detail": "high"
        });
        let historical = serde_json::json!({
            "id": "raw-item-id", "phase": "final", "meta": {"source": "fixture", "reference": marker_reference},
            "content": [
                {"type": "text", "text": "historical payload".repeat(100)},
                media.clone(),
                {"type": "text", "text": format!("<!--sh:{marker_reference}-->\n")}
            ]
        });
        let content = serde_json::to_string(&historical).unwrap();
        let key = stravia_runtime_contract::identifier::encode_digest(
            &sha2::Sha256::digest(content.as_bytes()).into(),
        );
        sqlx::query("INSERT INTO turn_chain_contents (principal, content_key, content) VALUES ('api-key:alice', ?, ?)")
            .bind(&key).bind(&content).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO turn_chain_content_refs VALUES ('child', 'api-key:alice', '/items/0', ?)",
        )
        .bind(&key)
        .execute(&pool)
        .await
        .unwrap();
        let call = serde_json::json!({"id": "call-fixture", "name": "fixture-tool", "arguments": "{\"query\":\"rust\"}"});
        let segment = serde_json::json!({
            "kind": "platform", "call": call,
            "result": {"type": "tool_result", "tool_use_id": "call-fixture", "content": [media], "content_kind": "content_blocks", "is_error": false}
        });
        let segment_bytes = serde_json::to_string(&segment).unwrap();
        sqlx::query("INSERT INTO history_markers (reference, principal, kind, activity, tool_id, call_payload, segment_payload, execution_state, execution_deadline, published_at, created_at, updated_at, expires_at) VALUES (?, 'api-key:alice', 'platform', 'Fixture platform call', 'call-fixture', ?, ?, 'completed', ?, 1, 1, 1, ?)")
            .bind(marker_reference).bind(serde_json::to_string(&call).unwrap()).bind(&segment_bytes).bind(expires_at).bind(expires_at).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO native_compactions (id, principal, source_generation_id, operation_id, payload, created_at, expires_at) VALUES ('compaction-fixture', 'api-key:alice', 'child', 'op-fixture', '{}', 1, ?)")
            .bind(expires_at).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO interaction_observations (id, principal, root_id, root_run_id, first_route_id, status, started_at, last_active_at, expires_at) VALUES ('interaction', 'api-key:alice', 'interaction', 'run', 'r', 'active', 1, 1, 100)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO inference_run_observations (id, interaction_id, ingress_protocol, route_id, status, background_active, debug_enabled, started_at, last_active_at, expires_at) VALUES ('run', 'interaction', 'openai', 'r', 'active', 2, 0, 1, 1, 100)")
            .execute(&pool).await.unwrap();

        migrate_sqlite(&pool, None).await.unwrap();
        migrate_sqlite(&pool, None).await.unwrap();
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
        use stravia_runtime_contract::turn_chain::{TurnChainStore, TurnNodeId, TurnNodeKind};
        let nodes = crate::turn_chain::SqlTurnChainStore::sqlite(
            pool.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        )
        .materialize(
            &stravia_runtime_contract::Principal::new("alice"),
            TurnNodeKind::Response,
            &TurnNodeId::new("child"),
        )
        .await
        .unwrap();
        assert_eq!(nodes[0].payload, serde_json::json!({"items":[1]}));
        assert_eq!(nodes[1].payload, serde_json::json!({"items":[historical]}));
        use crate::history_marker::{
            HiddenHistorySegment, HistoryMarkerStore, PlatformExecutionState, SqlHistoryMarkerStore,
        };
        let restored = SqlHistoryMarkerStore::sqlite(pool.clone())
            .resolve(
                &stravia_runtime_contract::Principal::new("alice"),
                marker_reference,
            )
            .await
            .unwrap()
            .expect("retained platform marker");
        assert_eq!(
            restored.execution_state,
            Some(PlatformExecutionState::Completed)
        );
        assert!(restored.published);
        let expected_segment: HiddenHistorySegment = serde_json::from_value(segment).unwrap();
        assert_eq!(
            serde_json::to_value(restored.segment.unwrap()).unwrap(),
            serde_json::to_value(expected_segment).unwrap()
        );
        assert!(
            SqlHistoryMarkerStore::sqlite(pool.clone())
                .resolve(
                    &stravia_runtime_contract::Principal::new("bob"),
                    marker_reference
                )
                .await
                .unwrap()
                .is_none()
        );
        let (native_source, native_payload): (String, String) = sqlx::query_as("SELECT source_generation_id, payload FROM native_compactions WHERE id='compaction-fixture'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(native_source, "child");
        assert_eq!(native_payload, "{}");
        let stored_segment: String =
            sqlx::query_scalar("SELECT segment_payload FROM history_markers WHERE reference=?")
                .bind(marker_reference)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_segment, segment_bytes);
        assert!(
            sqlx::query("DELETE FROM turn_chain_nodes WHERE id='child'")
                .execute(&pool)
                .await
                .is_err()
        );
        let retained: i64 = sqlx::query_scalar(
            "SELECT background_active FROM inference_run_observations WHERE id='run'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retained, 2);
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
        let valid_payload =
            crate::storage_codec::encode(br#"{"data":{},"slots":[],"contents":[]}"#).unwrap();
        for (id, kind, principal) in [
            ("foreign", "response", "api-key:bob"),
            ("wrong-kind", "agent", "api-key:alice"),
        ] {
            let error = sqlx::query("INSERT INTO turn_chain_nodes (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at, storage_format) VALUES (?, ?, 'parent', ?, 1, ?, 1, 100, 2)")
                .bind(id).bind(kind).bind(principal).bind(&valid_payload).execute(&pool).await.unwrap_err();
            assert!(
                error
                    .as_database_error()
                    .unwrap()
                    .is_foreign_key_violation(),
                "{error}"
            );
        }
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
        let error = migrate_sqlite(&pool, None).await.unwrap_err().to_string();
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
        let error = migrate_sqlite(&pool, None).await.unwrap_err().to_string();
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
        migrate_sqlite(&pool, None).await.unwrap();
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
        let mut runner = super::unlocked_runner(&SQLITE_MIGRATOR);
        super::supported_history(&[], &mut runner).unwrap();
        super::supported_history(std::slice::from_ref(&v1), &mut runner).unwrap();
        super::supported_history(&[v1.clone(), v2.clone()], &mut runner).unwrap();
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
            assert!(super::supported_history(&history, &mut runner).is_err());
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
        assert!(migrate_sqlite(&pool, None).await.is_err());
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
        sqlx::query("INSERT INTO turn_chain_nodes (id, kind, principal, payload_version, payload, created_at, expires_at) VALUES ('parent', 'response', 'api-key:alice', 1, '{}', 1, 2)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO turn_chain_nodes (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) VALUES ('child', 'response', 'parent', 'bob', 1, '{}', 1, 2)")
            .execute(&pool).await.unwrap();
        assert!(migrate_sqlite(&pool, None).await.is_err());
        let legacy_parent: String =
            sqlx::query_scalar("SELECT parent_id FROM turn_chain_nodes WHERE id='child'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(legacy_parent, "parent");
        sqlx::query("UPDATE turn_chain_nodes SET principal='api-key:alice' WHERE id='child'")
            .execute(&pool)
            .await
            .unwrap();
        migrate_sqlite(&pool, None).await.unwrap();
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
        let expires_at = chrono::Utc::now().timestamp_millis() + 600_000;
        let marker_reference = "abcdefghijklmnopqrstuvwxyzab";
        let item = serde_json::json!({
            "id": "pg-raw-item", "phase": "final", "meta": {"reference": marker_reference},
            "content": [
                {"type": "text", "text": "迁移媒体😀".repeat(100)},
                {"type": "image", "source": {"type": "url", "url": "https://example.invalid/fixture.png"}, "detail": "high"},
                {"type": "text", "text": format!("<!--sh:{marker_reference}-->\n")}
            ]
        });
        let parent_payload = serde_json::json!({"items": [item.clone()]});
        sqlx::query("INSERT INTO turn_chain_nodes (id,kind,principal,payload_version,payload,created_at,expires_at,storage_format) VALUES ('pg-parent','response','api-key:alice',6,$1,1,$2,0)")
            .bind(serde_json::to_string(&parent_payload).unwrap()).bind(expires_at).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO turn_chain_nodes (id,kind,parent_id,principal,payload_version,payload,created_at,expires_at,storage_format) VALUES ('pg-child','response','pg-parent','api-key:alice',6,'{\"data\":{\"items\":[null]},\"references\":1}',1,$1,1)")
            .bind(expires_at).execute(&pool).await.unwrap();
        use sha2::Digest;
        let item_bytes = serde_json::to_string(&item).unwrap();
        let content_key = stravia_runtime_contract::identifier::encode_digest(
            &sha2::Sha256::digest(item_bytes.as_bytes()).into(),
        );
        sqlx::query("INSERT INTO turn_chain_contents (principal,content_key,content) VALUES ('api-key:alice',$1,$2)")
            .bind(&content_key).bind(&item_bytes).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO turn_chain_content_refs (node_id,principal,path,content_key) VALUES ('pg-child','api-key:alice','/items/0',$1)")
            .bind(&content_key).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO native_compactions (id,principal,source_generation_id,operation_id,payload,created_at,expires_at) VALUES ('pg-compaction','api-key:alice','pg-child','pg-op','{}',1,$1)")
            .bind(expires_at).execute(&pool).await.unwrap();
        let call = serde_json::json!({"id":"pg-call","name":"fixture-tool","arguments":"{}"});
        let segment = serde_json::json!({"kind":"platform","call":call,"result":{"type":"tool_result","tool_use_id":"pg-call","content":[item["content"][0].clone(),item["content"][1].clone()],"content_kind":"content_blocks","is_error":false}});
        let segment_bytes = serde_json::to_string(&segment).unwrap();
        sqlx::query("INSERT INTO history_markers (reference,principal,kind,activity,tool_id,call_payload,segment_payload,execution_state,execution_deadline,published_at,created_at,updated_at,expires_at) VALUES ($1,'api-key:alice','platform','Fixture platform call','pg-call',$2,$3,'completed',$4,1,1,1,$4)")
            .bind(marker_reference).bind(serde_json::to_string(&call).unwrap()).bind(&segment_bytes).bind(expires_at).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO models (id, model_id, balance) VALUES ('invalid', 'invalid', 'unknown')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                migrate_postgres(&pool, None)
            )
            .await
            .expect("migration failure must not hang")
            .is_err()
        );
        sqlx::query("UPDATE models SET balance='traffic_equalization' WHERE id='invalid'")
            .execute(&pool)
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            migrate_postgres(&pool, None),
        )
        .await
        .expect("failed migration leaked advisory lock")
        .expect("v1 upgrade");
        migrate_postgres(&pool, None)
            .await
            .expect("upgrade re-run is a no-op");
        use stravia_runtime_contract::turn_chain::{TurnChainStore, TurnNodeId, TurnNodeKind};
        let principal = stravia_runtime_contract::Principal::new("alice");
        let restored = crate::turn_chain::SqlTurnChainStore::postgres(pool.clone())
            .materialize(
                &principal,
                TurnNodeKind::Response,
                &TurnNodeId::new("pg-child"),
            )
            .await
            .unwrap();
        assert_eq!(restored[0].payload, parent_payload);
        assert_eq!(restored[1].payload, serde_json::json!({"items": [item]}));
        let distinct: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_chain_contents")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(distinct, 1);
        use crate::history_marker::{
            HiddenHistorySegment, HistoryMarkerStore, SqlHistoryMarkerStore,
        };
        let marker = SqlHistoryMarkerStore::postgres(pool.clone())
            .resolve(&principal, marker_reference)
            .await
            .unwrap()
            .unwrap();
        let expected_segment: HiddenHistorySegment = serde_json::from_value(segment).unwrap();
        assert_eq!(
            serde_json::to_value(marker.segment.unwrap()).unwrap(),
            serde_json::to_value(expected_segment).unwrap()
        );
        let raw_segment: String =
            sqlx::query_scalar("SELECT segment_payload FROM history_markers WHERE reference=$1")
                .bind(marker_reference)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(raw_segment, segment_bytes);
        let source: String = sqlx::query_scalar(
            "SELECT source_generation_id FROM native_compactions WHERE id='pg-compaction'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(source, "pg-child");
        sqlx::query("INSERT INTO interaction_observations (id, principal, root_id, root_run_id, first_route_id, status, started_at, last_active_at, expires_at) VALUES ('interaction', 'api-key:alice', 'interaction', 'run', 'r', 'active', 1, 1, 100)")
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
