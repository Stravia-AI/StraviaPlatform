use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteOwnedBuf, SqlitePoolOptions};
use tokio::sync::OnceCell;

/// Restore a real migrated schema into a new, independently writable database.
/// Migration/upgrade/restart tests deliberately keep their own initialization paths.
pub(crate) async fn migrated_sqlite_pool() -> anyhow::Result<SqlitePool> {
    static TEMPLATE: OnceCell<Vec<u8>> = OnceCell::const_new();
    let template = TEMPLATE
        .get_or_try_init(|| async {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await?;
            crate::migrations::migrate_sqlite(&pool, None).await?;
            let bytes = {
                let mut connection = pool.acquire().await?;
                connection.serialize(None).await?.to_vec()
            };
            pool.close().await;
            Ok::<_, anyhow::Error>(bytes)
        })
        .await?;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    {
        let mut connection = pool.acquire().await?;
        connection
            .deserialize(None, SqliteOwnedBuf::try_from(template.as_slice())?, false)
            .await?;
    }
    Ok(pool)
}
