use std::path::Path;
use tokio::sync::OnceCell;

/// Seed only a fresh ordinary fixture. Tests of first startup, migration, or
/// restart call Gateway::new directly and exercise its real initialization.
pub(super) async fn seed_database(data_dir: &Path) -> anyhow::Result<()> {
    static TEMPLATE: OnceCell<Vec<u8>> = OnceCell::const_new();
    let paths = stravia_core::data_paths::DataPaths::new(data_dir);
    let destination = paths.database();
    if destination.exists() {
        return Ok(());
    }
    let template = TEMPLATE
        .get_or_try_init(|| async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await?;
            stravia_core::migrations::migrate_sqlite(&pool, None).await?;
            let bytes = {
                let mut connection = pool.acquire().await?;
                connection.serialize(None).await?.to_vec()
            };
            pool.close().await;
            Ok::<_, anyhow::Error>(bytes)
        })
        .await?;
    paths.prepare()?;
    tokio::fs::create_dir_all(paths.database_dir()).await?;
    tokio::fs::write(destination, template).await?;
    Ok(())
}
