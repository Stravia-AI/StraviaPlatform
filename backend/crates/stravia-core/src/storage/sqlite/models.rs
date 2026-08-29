use super::*;

#[derive(Clone)]
pub(super) struct SqliteModelStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl ModelStore for SqliteModelStore {
    async fn list(&self) -> anyhow::Result<Vec<Model>> {
        Ok(sqlx::query_as::<_, Model>(
            "SELECT id, name, COALESCE(balance, 'weighted') AS balance, target_provider, target_model, COALESCE(is_enabled, 1) AS is_enabled, created_at FROM models ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Model>> {
        Ok(sqlx::query_as::<_, Model>(
            "SELECT id, name, COALESCE(balance, 'weighted') AS balance, target_provider, target_model, COALESCE(is_enabled, 1) AS is_enabled, created_at FROM models WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn create(&self, input: CreateModel) -> anyhow::Result<Model> {
        let id = uuid::Uuid::new_v4().to_string();
        let balance = input.balance.unwrap_or_else(|| "weighted".to_string());
        sqlx::query(
            "INSERT INTO models (id, name, balance, target_provider, target_model) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.name.trim())
        .bind(balance)
        .bind(input.target_provider.trim())
        .bind(input.target_model.trim())
        .execute(&self.pool)
        .await?;
        self.get(&id).await?.context("model missing after create")
    }

    async fn update(&self, id: &str, input: UpdateModel) -> anyhow::Result<Model> {
        let current = self.get(id).await?.context("model not found for update")?;
        let name = input.name.unwrap_or(current.name);
        let balance = input.balance.unwrap_or(current.balance);
        let target_provider = input.target_provider.unwrap_or(current.target_provider);
        let target_model = input.target_model.unwrap_or(current.target_model);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);

        sqlx::query(
            "UPDATE models SET name=?, balance=?, target_provider=?, target_model=?, is_enabled=? WHERE id=?",
        )
        .bind(name.trim())
        .bind(balance.trim().to_lowercase())
        .bind(target_provider.trim())
        .bind(target_model.trim())
        .bind(is_enabled)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get(id).await?.context("model missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM models WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let sql = if exclude_id.is_some() {
            "SELECT id FROM models WHERE lower(trim(name)) = lower(trim(?)) AND id != ? LIMIT 1"
        } else {
            "SELECT id FROM models WHERE lower(trim(name)) = lower(trim(?)) LIMIT 1"
        };
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(sql)
                .bind(name)
                .bind(exclude_id)
                .fetch_optional(&self.pool)
                .await?
        } else {
            sqlx::query_scalar::<_, String>(sql)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
        };
        Ok(row.is_some())
    }
}

#[async_trait]
impl ModelSnapshotStore for SqliteModelStore {
    async fn load_active_snapshot(&self) -> anyhow::Result<Vec<Model>> {
        Ok(sqlx::query_as::<_, Model>(
            r#"SELECT
                id, name,
                COALESCE(balance, 'weighted') AS balance,
                target_provider, target_model,
                COALESCE(is_enabled, 1) AS is_enabled,
                created_at
            FROM models
            WHERE COALESCE(is_enabled, 1) = 1"#,
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

#[derive(Clone)]
pub(super) struct SqliteModelBackendStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl ModelBackendStore for SqliteModelBackendStore {
    async fn list_backends_by_model(&self, model_id: &str) -> anyhow::Result<Vec<ModelBackend>> {
        Ok(sqlx::query_as::<_, ModelBackend>(
            "SELECT id, model_id, provider_id, model, weight, priority, created_at, thinking_level_map FROM model_backends WHERE model_id = ? ORDER BY priority ASC, created_at ASC",
        )
        .bind(model_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn set_backends(
        &self,
        model_id: &str,
        backends: &[CreateModelBackend],
    ) -> anyhow::Result<Vec<ModelBackend>> {
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, ModelBackend>(
            "SELECT id, model_id, provider_id, model, weight, priority, created_at, thinking_level_map FROM model_backends WHERE model_id = ?",
        )
        .bind(model_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM model_backends WHERE model_id = ?")
            .bind(model_id)
            .execute(&mut *tx)
            .await?;

        for backend in backends {
            let id = existing
                .iter()
                .find(|row| {
                    row.provider_id == backend.provider_id.trim()
                        && row.model == backend.model.trim()
                })
                .map(|row| row.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            sqlx::query(
                "INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority, thinking_level_map) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(model_id)
            .bind(backend.provider_id.trim())
            .bind(backend.model.trim())
            .bind(backend.weight.unwrap_or(100).max(0))
            .bind(backend.priority.unwrap_or(1).max(1))
            .bind(sqlx::types::Json(&backend.thinking_level_map))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        self.list_backends_by_model(model_id).await
    }

    async fn delete_backends_by_model(&self, model_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM model_backends WHERE model_id = ?")
            .bind(model_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
