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

    async fn increment_u64(&self, key: &str) -> anyhow::Result<u64> {
        let next: i64 = sqlx::query_scalar(
            "INSERT INTO settings (name, value, updated_at) VALUES ($1, '1', CURRENT_TIMESTAMP) \
             ON CONFLICT(name) DO UPDATE SET \
             value = (CASE WHEN settings.value ~ '^[0-9]+$' THEN settings.value::bigint ELSE 0 END + 1)::text, \
             updated_at = EXCLUDED.updated_at \
             RETURNING value::bigint",
        )
        .bind(key)
        .fetch_one(&self.pool)
        .await?;
        Ok(next.max(0) as u64)
    }
}
