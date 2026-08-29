use super::*;

#[derive(Clone)]
pub(super) struct PostgresModelStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl ModelStore for PostgresModelStore {
    async fn list(&self) -> anyhow::Result<Vec<Model>> {
        Ok(
            sqlx::query_as::<_, Model>(sqlx::AssertSqlSafe(model_select(Some(
                "ORDER BY created_at DESC",
            ))))
            .fetch_all(&self.pool)
            .await?,
        )
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Model>> {
        let sql = format!("{} WHERE id = $1", model_select(None));
        Ok(sqlx::query_as::<_, Model>(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn create(&self, input: CreateModel) -> anyhow::Result<Model> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO models (id, name, balance, target_provider, target_model) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&id)
        .bind(input.name.trim())
        .bind(input.balance.unwrap_or_else(|| "weighted".to_string()))
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
            "UPDATE models SET name=$1, balance=$2, target_provider=$3, target_model=$4, is_enabled=$5 WHERE id=$6",
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
        sqlx::query("DELETE FROM models WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM models WHERE lower(trim(name)) = lower(trim($1)) AND id != $2 LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM models WHERE lower(trim(name)) = lower(trim($1)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.is_some())
    }
}

#[async_trait]
impl ModelSnapshotStore for PostgresModelStore {
    async fn load_active_snapshot(&self) -> anyhow::Result<Vec<Model>> {
        let sql = format!(
            "{} WHERE COALESCE(is_enabled, TRUE) = true",
            model_select(None)
        );
        Ok(sqlx::query_as::<_, Model>(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?)
    }
}

#[derive(Clone)]
pub(super) struct PostgresModelBackendStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl ModelBackendStore for PostgresModelBackendStore {
    async fn list_backends_by_model(&self, model_id: &str) -> anyhow::Result<Vec<ModelBackend>> {
        Ok(sqlx::query_as::<_, ModelBackend>(
            "SELECT id, model_id, provider_id, model, weight, priority, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, thinking_level_map FROM model_backends WHERE model_id = $1 ORDER BY priority ASC, created_at ASC",
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
            "SELECT id, model_id, provider_id, model, weight, priority, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, thinking_level_map FROM model_backends WHERE model_id = $1",
        )
        .bind(model_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM model_backends WHERE model_id = $1")
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
                "INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority, thinking_level_map) VALUES ($1, $2, $3, $4, $5, $6, $7)",
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
        sqlx::query("DELETE FROM model_backends WHERE model_id = $1")
            .bind(model_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
