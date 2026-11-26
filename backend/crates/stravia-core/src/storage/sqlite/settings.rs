use super::*;

#[derive(Clone)]
pub(super) struct SqliteSettingsStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl SettingsStore for SqliteSettingsStore {
    async fn get_or_insert(&self, key: &str, value: &str) -> anyhow::Result<String> {
        let mut connection = self.pool.begin_write().await?;
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
        connection.commit().await?;
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
        let mut connection = self.pool.begin_write().await?;
        sqlx::query(
    "INSERT INTO settings (name, value, updated_at) VALUES (?, ?, datetime('now')) ON CONFLICT(name) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *connection)
            .await?;
        connection.commit().await?;
        Ok(())
    }

    async fn increment_u64(&self, key: &str) -> anyhow::Result<u64> {
        let mut connection = self.pool.begin_write().await?;
        let next: i64 = sqlx::query_scalar(
            "INSERT INTO settings (name, value, updated_at) VALUES (?, '1', datetime('now')) \
             ON CONFLICT(name) DO UPDATE SET \
             value = CAST(COALESCE(CAST(settings.value AS INTEGER), 0) + 1 AS TEXT), \
             updated_at = excluded.updated_at \
             RETURNING CAST(value AS INTEGER)",
        )
        .bind(key)
        .fetch_one(&mut *connection)
        .await?;
        connection.commit().await?;
        Ok(next.max(0) as u64)
    }
}
