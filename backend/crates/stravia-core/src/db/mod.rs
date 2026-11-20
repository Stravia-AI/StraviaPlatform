pub mod identity;
pub mod models;

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const SQLITE_WAL_CHECKPOINT_BYTES: i64 = 32 * 1024 * 1024;
/// 16 MiB per-connection SQLite page cache (allocated lazily on read).
const SQLITE_CACHE_SIZE_KIB: i64 = -16_384;
/// 128 MiB memory-mapped read I/O (OS page cache backed; avoids per-page `pread`).
const SQLITE_MMAP_SIZE_BYTES: i64 = 128 * 1024 * 1024;

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
                query.push(pages).build().execute(&mut *connection).await?;
                sqlx::query("PRAGMA temp_store = MEMORY")
                    .execute(&mut *connection)
                    .await?;
                let mut cache = sqlx::QueryBuilder::<sqlx::Sqlite>::new("PRAGMA cache_size=");
                cache
                    .push(SQLITE_CACHE_SIZE_KIB)
                    .build()
                    .execute(&mut *connection)
                    .await?;
                let mut mmap = sqlx::QueryBuilder::<sqlx::Sqlite>::new("PRAGMA mmap_size=");
                mmap.push(SQLITE_MMAP_SIZE_BYTES)
                    .build()
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        // 同一数据库的写者只能在池写租约内排队；门外写入直接失败，不与租约写者争用写锁。
        .require_write_lease()
        .connect_with(options)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::{SQLITE_CACHE_SIZE_KIB, SQLITE_MMAP_SIZE_BYTES, init_pool};

    #[tokio::test]
    async fn init_pool_applies_read_path_pragmas_without_relaxing_synchronous() -> anyhow::Result<()>
    {
        let root = tempfile::tempdir()?;
        let pool = init_pool(root.path()).await?;

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await?;
        let temp_store: i64 = sqlx::query_scalar("PRAGMA temp_store")
            .fetch_one(&pool)
            .await?;
        let cache_size: i64 = sqlx::query_scalar("PRAGMA cache_size")
            .fetch_one(&pool)
            .await?;
        let mmap_size: i64 = sqlx::query_scalar("PRAGMA mmap_size")
            .fetch_one(&pool)
            .await?;
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await?;

        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(temp_store, 2, "temp_store should be MEMORY");
        assert_eq!(cache_size, SQLITE_CACHE_SIZE_KIB);
        assert!(
            mmap_size == SQLITE_MMAP_SIZE_BYTES || mmap_size == 0,
            "mmap_size should match requested limit (or 0 if unsupported): {mmap_size}"
        );
        assert_eq!(synchronous, 2, "synchronous must remain FULL");
        pool.close().await;
        Ok(())
    }
}
