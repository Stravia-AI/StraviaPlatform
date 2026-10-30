pub mod identity;
pub mod models;

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const SQLITE_WAL_CHECKPOINT_BYTES: i64 = 32 * 1024 * 1024;

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
        .acquire_time_level(log::LevelFilter::Debug)
        .after_connect(|connection, _| {
            Box::pin(async move {
                let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
                    .fetch_one(&mut *connection)
                    .await?;
                if page_size <= 0 {
                    return Err(sqlx::Error::Protocol("invalid SQLite page size".into()));
                }
                // 小批事务反复修改相同索引页；延后 checkpoint 合并回写，不放宽提交同步或写租约边界。
                // 按实际页大小换算，既有非默认页大小的数据库也使用同一字节触发阈值。
                let pages = (SQLITE_WAL_CHECKPOINT_BYTES / page_size).max(1);
                let mut query =
                    sqlx::QueryBuilder::<sqlx::Sqlite>::new("PRAGMA wal_autocheckpoint=");
                query.push(pages).build().execute(connection).await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await?)
}
