use sqlx::Connection;

use super::*;

#[derive(Clone)]
pub(super) struct SqliteRouteStore {
    pub(super) pool: SqlitePool,
}

impl SqliteRouteStore {
    async fn load_routes(&self, active_only: bool) -> anyhow::Result<Vec<Route>> {
        let where_clause = if active_only {
            " WHERE COALESCE(is_enabled, 1) = 1"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id, model_id, display_name, COALESCE(balance, 'weighted') AS balance, \
             COALESCE((SELECT provider_id FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_provider, \
             COALESCE((SELECT model FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_model, \
             COALESCE(is_enabled, 1) AS is_enabled, created_at \
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
            "SELECT id, model_id, provider_id, model, weight, priority, created_at, thinking_level_map FROM model_backends WHERE model_id = ? ORDER BY priority ASC, created_at ASC",
        )
        .bind(route_storage_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn load_route(&self, route_id: &str) -> anyhow::Result<Option<Route>> {
        let route = sqlx::query_as::<_, Route>(
            "SELECT id, model_id, display_name, COALESCE(balance, 'weighted') AS balance, \
             COALESCE((SELECT provider_id FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_provider, \
             COALESCE((SELECT model FROM model_backends WHERE model_id = models.id ORDER BY priority ASC, created_at ASC LIMIT 1), '') AS target_model, \
             COALESCE(is_enabled, 1) AS is_enabled, created_at \
             FROM models WHERE model_id = ?",
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
impl RouteStore for SqliteRouteStore {
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
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let conflict = sqlx::query_scalar::<_, String>(
            "SELECT id FROM models WHERE model_id = ? AND id != ? LIMIT 1",
        )
        .bind(route.model_id.trim())
        .bind(&route_storage_id)
        .fetch_optional(&mut *tx)
        .await?;
        if conflict.is_some() {
            anyhow::bail!("Route ID already exists: {}", route.model_id.trim());
        }

        if route.id.is_some() {
            let updated = sqlx::query(
                "UPDATE models SET model_id = ?, display_name = ?, balance = ?, is_enabled = ? WHERE id = ?",
            )
            .bind(route.model_id.trim())
            .bind(route.display_name.as_deref())
            .bind(route.selection_strategy.trim())
            .bind(route.is_enabled)
            .bind(&route_storage_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                anyhow::bail!("Route not found: {}", route.model_id.trim());
            }
        } else {
            sqlx::query(
                "INSERT INTO models (id, model_id, display_name, balance, is_enabled) VALUES (?, ?, ?, ?, ?)",
            )
                .bind(&route_storage_id)
                .bind(route.model_id.trim())
                .bind(route.display_name.as_deref())
                .bind(route.selection_strategy.trim())
                .bind(route.is_enabled)
                .execute(&mut *tx)
                .await?;
        }

        let existing = sqlx::query_as::<_, Target>(
            "SELECT id, model_id, provider_id, model, weight, priority, created_at, thinking_level_map FROM model_backends WHERE model_id = ?",
        )
        .bind(&route_storage_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM model_backends WHERE model_id = ?")
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
                "INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority, thinking_level_map) VALUES (?, ?, ?, ?, ?, ?, ?)",
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
        drop(connection);
        self.get(route.model_id.trim())
            .await?
            .context("Route missing after put")
    }

    async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM models WHERE model_id = ?")
            .bind(route_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    fn target(provider_id: &str, model: &str) -> crate::db::models::CreateTarget {
        crate::db::models::CreateTarget {
            provider_id: provider_id.into(),
            model: model.into(),
            weight: Some(100),
            priority: Some(1),
            thinking_level_map: Vec::new(),
        }
    }

    #[tokio::test]
    async fn failed_route_put_keeps_the_previous_aggregate() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("foreign keys");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("migrations");
        sqlx::query(
            "INSERT INTO providers (
                id, name, protocol, base_url, api_key, auth_mode
             ) VALUES ('provider-1', 'Provider 1', 'openai-compatible', 'https://example.com', '', 'apikey')",
        )
        .execute(&pool)
        .await
        .expect("Provider");
        let store = SqliteRouteStore { pool };
        let route = store
            .put(PutRoute {
                id: None,
                model_id: "atomic-route".into(),
                display_name: None,
                selection_strategy: "weighted".into(),
                is_enabled: true,
                targets: vec![target("provider-1", "working-model")],
            })
            .await
            .expect("initial Route");

        let failed = store
            .put(PutRoute {
                id: Some(route.id),
                model_id: "atomic-route".into(),
                display_name: None,
                selection_strategy: "priority".into(),
                is_enabled: true,
                targets: vec![target("missing-provider", "broken-model")],
            })
            .await;
        assert!(failed.is_err());

        let persisted = store
            .get("atomic-route")
            .await
            .expect("get")
            .expect("Route");
        assert_eq!(persisted.balance, "weighted");
        assert_eq!(persisted.targets.len(), 1);
        assert_eq!(persisted.targets[0].provider_id, "provider-1");
        assert_eq!(persisted.targets[0].model, "working-model");
    }

    #[tokio::test]
    async fn route_put_waits_for_a_concurrent_sqlite_writer() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("migrations");
        sqlx::query(
            "INSERT INTO providers (
                id, name, protocol, base_url, api_key, auth_mode
             ) VALUES ('provider-1', 'Provider 1', 'openai-compatible', 'https://example.com', '', 'apikey')",
        )
        .execute(&pool)
        .await
        .expect("Provider");
        let store = SqliteRouteStore { pool: pool.clone() };

        let mut writer = pool.acquire().await.expect("writer connection");
        let writer_tx = writer
            .begin_with("BEGIN IMMEDIATE")
            .await
            .expect("writer transaction");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let put_task = tokio::spawn(async move {
            started_tx.send(()).expect("signal Route put start");
            store
                .put(PutRoute {
                    id: None,
                    model_id: "concurrent-route".into(),
                    display_name: None,
                    selection_strategy: "weighted".into(),
                    is_enabled: true,
                    targets: vec![target("provider-1", "provider-model")],
                })
                .await
        });
        started_rx.await.expect("Route put start");

        let mut put_task = put_task;
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut put_task)
                .await
                .is_err(),
            "Route put must wait while another write transaction owns the database"
        );
        writer_tx
            .commit()
            .await
            .expect("release writer transaction");

        let route = put_task
            .await
            .expect("Route put task")
            .expect("create Route");
        assert_eq!(route.model_id, "concurrent-route");
    }
}
