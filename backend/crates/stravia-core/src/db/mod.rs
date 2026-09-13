pub mod models;

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

pub async fn init_pool(data_dir: &Path) -> anyhow::Result<SqlitePool> {
    let root = crate::data_paths::resolve_data_dir(data_dir)?;
    let paths = crate::data_paths::DataPaths::new(&root);
    paths.prepare()?;
    std::fs::create_dir_all(paths.database_dir())?;
    let db_path = paths.database();

    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));

    Ok(SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?)
}
