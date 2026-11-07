use super::*;

#[derive(Clone)]
pub(super) struct SqliteSettingsStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl SettingsStore for SqliteSettingsStore {
    async fn get_or_insert(&self, key: &str, value: &str) -> anyhow::Result<String> {
        let mut connection = self.pool.acquire_write().await?;
        sqlx::query(
            "INSERT INTO settings (name, value) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *connection)
        .await?;
        let stored = sqlx::query_scalar("SELECT value FROM settings WHERE name = ?")
            .bind(key)
            .fetch_one(&mut *connection)
            .await?;
        connection.finish().await?;
        Ok(stored)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE name = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire_write().await?;
        sqlx::query(
    "INSERT INTO settings (name, value, updated_at) VALUES (?, ?, datetime('now')) ON CONFLICT(name) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *connection)
            .await?;
        connection.finish().await?;
        Ok(())
    }
}
