use super::*;

#[derive(Clone)]
pub(super) struct SqliteBootstrap {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl StorageBootstrap for SqliteBootstrap {
    async fn health(&self) -> anyhow::Result<StorageHealth> {
        let can_connect = sqlx::query("SELECT 1").execute(&self.pool).await.is_ok();
        // The SQLx migration creates `models`; its presence is the minimum schema check.
        let schema_compatible = if can_connect {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='models'",
            )
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0)
                > 0
        } else {
            false
        };
        Ok(StorageHealth {
            backend: StorageBackend::Sqlite,
            can_connect,
            schema_compatible,
            writable: can_connect,
        })
    }
}
