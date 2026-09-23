use anyhow::ensure;
use sqlx::{PgPool, SqlitePool, migrate::Migrator};

// The schema is a single baseline. Databases carrying an older migration
// history are rejected instead of upgraded: there is no incremental path.
static SQLITE_MIGRATOR: Migrator = sqlx::migrate!("./migrations/sqlite");
static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");

/// The only migration version this release applies or accepts.
pub const BASELINE_VERSION: i64 = 1;

const INCOMPATIBLE_HISTORY: &str = "database was created by an older Stravia version and cannot be upgraded by this release; start with a fresh database or restore a backup";
const UNRECOGNIZED_TABLES: &str = "database contains tables but no Stravia migration history; refusing to initialize over unrecognized data";

pub async fn migrate_sqlite(pool: &SqlitePool) -> anyhow::Result<()> {
    reject_incompatible_sqlite(pool).await?;
    SQLITE_MIGRATOR.run(pool).await?;
    Ok(())
}

pub async fn migrate_postgres(pool: &PgPool) -> anyhow::Result<()> {
    reject_incompatible_postgres(pool).await?;
    POSTGRES_MIGRATOR.run(pool).await?;
    Ok(())
}

async fn reject_incompatible_sqlite(pool: &SqlitePool) -> anyhow::Result<()> {
    let has_history: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if has_history {
        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(pool)
                .await?;
        ensure!(versions == [BASELINE_VERSION], "{INCOMPATIBLE_HISTORY}");
    } else {
        let user_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_one(pool)
        .await?;
        ensure!(user_tables == 0, "{UNRECOGNIZED_TABLES}");
    }
    Ok(())
}

async fn reject_incompatible_postgres(pool: &PgPool) -> anyhow::Result<()> {
    let has_history: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = current_schema() AND table_name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if has_history {
        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(pool)
                .await?;
        ensure!(versions == [BASELINE_VERSION], "{INCOMPATIBLE_HISTORY}");
    } else {
        let user_tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'",
        )
        .fetch_one(pool)
        .await?;
        ensure!(user_tables == 0, "{UNRECOGNIZED_TABLES}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BASELINE_VERSION, migrate_postgres, migrate_sqlite};
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
        let (enabled, mcp): (bool, bool) =
            sqlx::query_as("SELECT is_enabled, mcp_access_enabled FROM api_keys WHERE id = 'k'")
                .fetch_one(&pool)
                .await
                .expect("defaults materialized");
        assert!(enabled);
        assert!(!mcp);
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
        .bind(BASELINE_VERSION + 1)
        .execute(&pool)
        .await
        .unwrap();
        let error = migrate_sqlite(&pool).await.unwrap_err().to_string();
        assert!(error.contains("older Stravia version"), "{error}");
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
        let result = migrate_postgres(&pool).await;
        let rerun = migrate_postgres(&pool).await;
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        result.expect("fresh baseline");
        rerun.expect("baseline re-run is a no-op");
    }
}
