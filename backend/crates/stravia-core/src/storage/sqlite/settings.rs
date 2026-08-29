use super::*;

#[derive(Clone)]
pub(super) struct SqliteSettingsStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl SettingsStore for SqliteSettingsStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE name = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO settings (name, value, updated_at) VALUES (?, ?, datetime('now')) ON CONFLICT(name) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
