use super::*;

#[derive(Clone)]
pub(super) struct PostgresRouteStore {
    pub(super) pool: Pool<Postgres>,
}

impl PostgresRouteStore {
    async fn load_routes(&self, active_only: bool) -> anyhow::Result<Vec<Route>> {
        let where_clause = if active_only {
            " WHERE COALESCE(is_enabled, TRUE) = TRUE"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id, name, COALESCE(balance, 'weighted') AS balance, \
             COALESCE((SELECT provider_id FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_provider, \
             COALESCE((SELECT model FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_model, \
             COALESCE(is_enabled, TRUE) AS is_enabled, \
             to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at \
             FROM models{where_clause} ORDER BY created_at DESC"
        );
        let mut routes = sqlx::query_as::<_, Route>(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        for route in &mut routes {
            route.targets = self.load_targets(&route.id).await?;
            route.refresh_supported_thinking_levels();
        }
        Ok(routes)
    }

    async fn load_targets(&self, route_storage_id: &str) -> anyhow::Result<Vec<Target>> {
        Ok(sqlx::query_as::<_, Target>(
            "SELECT id, model_id, provider_id, model, weight, priority, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, thinking_level_map FROM model_backends WHERE model_id = $1 ORDER BY priority ASC, created_at ASC",
        )
        .bind(route_storage_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn load_route(&self, route_id: &str) -> anyhow::Result<Option<Route>> {
        let route = sqlx::query_as::<_, Route>(
            "SELECT id, name, COALESCE(balance, 'weighted') AS balance, \
             COALESCE((SELECT provider_id FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_provider, \
             COALESCE((SELECT model FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_model, \
             COALESCE(is_enabled, TRUE) AS is_enabled, \
             to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at \
             FROM models WHERE name = $1",
        )
        .bind(route_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(mut route) = route else {
            return Ok(None);
        };
        route.targets = self.load_targets(&route.id).await?;
        route.refresh_supported_thinking_levels();
        Ok(Some(route))
    }
}

#[async_trait]
impl RouteStore for PostgresRouteStore {
    async fn list(&self) -> anyhow::Result<Vec<Route>> {
        self.load_routes(false).await
    }

    async fn list_active(&self) -> anyhow::Result<Vec<Route>> {
        self.load_routes(true).await
    }

    async fn get(&self, route_id: &str) -> anyhow::Result<Option<Route>> {
        self.load_route(route_id).await
    }

    async fn put(&self, route: PutRoute) -> anyhow::Result<Route> {
        if route.targets.is_empty() {
            anyhow::bail!("a Route requires at least one Target");
        }
        let route_storage_id = route
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut tx = self.pool.begin().await?;
        let conflict = sqlx::query_scalar::<_, String>(
            "SELECT id FROM models WHERE name = $1 AND id != $2 LIMIT 1",
        )
        .bind(route.route_id.trim())
        .bind(&route_storage_id)
        .fetch_optional(&mut *tx)
        .await?;
        if conflict.is_some() {
            anyhow::bail!("Route ID already exists: {}", route.route_id.trim());
        }

        if route.id.is_some() {
            let updated = sqlx::query(
                "UPDATE models SET name = $1, balance = $2, is_enabled = $3 WHERE id = $4",
            )
            .bind(route.route_id.trim())
            .bind(route.selection_strategy.trim())
            .bind(route.is_enabled)
            .bind(&route_storage_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                anyhow::bail!("Route not found: {}", route.route_id.trim());
            }
        } else {
            sqlx::query(
                "INSERT INTO models (id, name, balance, is_enabled) VALUES ($1, $2, $3, $4)",
            )
            .bind(&route_storage_id)
            .bind(route.route_id.trim())
            .bind(route.selection_strategy.trim())
            .bind(route.is_enabled)
            .execute(&mut *tx)
            .await?;
        }

        let existing = sqlx::query_as::<_, Target>(
            "SELECT id, model_id, provider_id, model, weight, priority, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, thinking_level_map FROM model_backends WHERE model_id = $1",
        )
        .bind(&route_storage_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM model_backends WHERE model_id = $1")
            .bind(&route_storage_id)
            .execute(&mut *tx)
            .await?;

        for target in &route.targets {
            let id = existing
                .iter()
                .find(|row| {
                    row.provider_id == target.provider_id.trim() && row.model == target.model.trim()
                })
                .map(|row| row.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            sqlx::query(
                "INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority, thinking_level_map) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(id)
            .bind(&route_storage_id)
            .bind(target.provider_id.trim())
            .bind(target.model.trim())
            .bind(target.weight.unwrap_or(100).max(0))
            .bind(target.priority.unwrap_or(1).max(1))
            .bind(sqlx::types::Json(&target.thinking_level_map))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        self.get(route.route_id.trim())
            .await?
            .context("Route missing after put")
    }

    async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM models WHERE name = $1")
            .bind(route_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
