use super::*;

#[derive(Clone)]
pub(super) struct PostgresSettingsStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl SettingsStore for PostgresSettingsStore {
    async fn get_or_insert(&self, key: &str, value: &str) -> anyhow::Result<String> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await?;
        let stored = sqlx::query_scalar("SELECT value FROM settings WHERE name = $1")
            .bind(key)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(stored)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE name = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
    "INSERT INTO settings (name, value, updated_at) VALUES ($1, $2, CURRENT_TIMESTAMP) ON CONFLICT(name) DO UPDATE SET value=EXCLUDED.value, updated_at=EXCLUDED.updated_at",
            )
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
