use sqlx::{Connection, SqliteConnection};
use stravia_runtime_contract::thinking::ThinkingLevel;

use super::*;

#[derive(sqlx::FromRow)]
struct RouteRow {
    id: String,
    model_id: String,
    display_name: Option<String>,
    default_thinking_level: Option<String>,
    balance: String,
    is_enabled: bool,
    created_at: String,
}

impl RouteRow {
    fn into_route(self) -> RouteConfig {
        RouteConfig {
            id: self.id.into(),
            model_id: self.model_id.into(),
            display_name: self.display_name,
            default_thinking_level: self.default_thinking_level,
            balance: self.balance,
            is_enabled: self.is_enabled,
            created_at: self.created_at,
            supported_thinking_levels: Vec::new(),
            context_window: None,
            output_max_tokens: None,
            supports_image_input: false,
            targets: Vec::new(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct TargetRow {
    id: String,
    model_id: String,
    provider_id: String,
    model: Option<String>,
    enabled: bool,
    priority: i32,
    first_token_timeout_ms: i64,
    target_retry_budget: i32,
    target_cooldown_ms: i64,
    created_at: String,
    thinking_level_map: sqlx::types::Json<Vec<crate::thinking::ThinkingLevelMapping>>,
}

impl TargetRow {
    fn into_target(self) -> TargetConfig {
        TargetConfig {
            id: self.id.into(),
            model_id: self.model_id.into(),
            destination: crate::db::identity::TargetDestination::new(
                self.provider_id.into(),
                self.model.map(Into::into),
            ),
            enabled: self.enabled,
            priority: self.priority,
            first_token_timeout_ms: self.first_token_timeout_ms,
            target_retry_budget: self.target_retry_budget,
            target_cooldown_ms: self.target_cooldown_ms,
            created_at: self.created_at,
            thinking_level_map: self.thinking_level_map.0,
        }
    }
}

#[derive(Clone)]
pub(super) struct SqliteRouteStore {
    pub(super) pool: SqlitePool,
}

/// Narrow projection for Generated Thinking Level Mapping refreshes; Target
/// identity and every other column stay untouched.
#[derive(sqlx::FromRow)]
struct TargetMapRow {
    id: String,
    thinking_level_map: sqlx::types::Json<Vec<crate::thinking::ThinkingLevelMapping>>,
}

impl SqliteRouteStore {
    pub(super) async fn load_routes(
        connection: &mut SqliteConnection,
        active_only: bool,
    ) -> anyhow::Result<Vec<RouteConfig>> {
        let where_clause = if active_only {
            " WHERE is_enabled = 1"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id, model_id, display_name, default_thinking_level, balance, \
             is_enabled, created_at \
             FROM models{where_clause} ORDER BY created_at DESC"
        );
        let mut routes = sqlx::query_as::<_, RouteRow>(sqlx::AssertSqlSafe(sql))
            .fetch_all(&mut *connection)
            .await?
            .into_iter()
            .map(RouteRow::into_route)
            .collect::<Vec<_>>();
        for route in &mut routes {
            route.targets = Self::load_targets(&mut *connection, &route.id).await?;
            route.refresh_supported_thinking_levels();
        }
        Ok(routes)
    }

    async fn load_targets(
        connection: &mut SqliteConnection,
        route_storage_id: &str,
    ) -> anyhow::Result<Vec<TargetConfig>> {
        Ok(sqlx::query_as::<_, TargetRow>(
            "SELECT id, model_id, provider_id, model, enabled, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, created_at, thinking_level_map FROM model_backends WHERE model_id = ? ORDER BY priority DESC, created_at ASC",
        )
        .bind(route_storage_id)
        .fetch_all(&mut *connection)
        .await?
        .into_iter()
        .map(TargetRow::into_target)
        .collect())
    }

    async fn load_route(
        connection: &mut SqliteConnection,
        route_id: &str,
    ) -> anyhow::Result<Option<RouteConfig>> {
        let route = sqlx::query_as::<_, RouteRow>(
            "SELECT id, model_id, display_name, default_thinking_level, balance, \
             is_enabled, created_at \
             FROM models WHERE model_id = ?",
        )
        .bind(route_id)
        .fetch_optional(&mut *connection)
        .await?;
        let Some(route) = route else {
            return Ok(None);
        };
        let mut route = route.into_route();
        route.targets = Self::load_targets(&mut *connection, &route.id).await?;
        route.refresh_supported_thinking_levels();
        Ok(Some(route))
    }

    /// Recompute the Generated Thinking Level Mapping rows of every Target bound
    /// to `provider_id` + `provider_model_id`, inside the caller's transaction.
    /// Overridden rows, Target IDs and all other columns are preserved.
    pub(super) async fn refresh_generated_thinking_maps(
        connection: &mut SqliteConnection,
        provider_id: &str,
        provider_model_id: &str,
        metadata: &crate::provider_models::ProviderModelMetadata,
        generated: &[crate::thinking::ThinkingLevelMapping],
        validate_map: &crate::provider_models::ReimportThinkingMapValidator<'_>,
    ) -> anyhow::Result<()> {
        let rows = sqlx::query_as::<_, TargetMapRow>(
            "SELECT id, thinking_level_map FROM model_backends \
             WHERE provider_id = ? AND model = ? ORDER BY id",
        )
        .bind(provider_id)
        .bind(provider_model_id)
        .fetch_all(&mut *connection)
        .await?;
        for row in rows {
            let mut map = row.thinking_level_map.0;
            let changed =
                crate::thinking::refresh_generated_thinking_level_map(&mut map, generated)?;
            validate_map(metadata, &map)?;
            if !changed {
                continue;
            }
            sqlx::query("UPDATE model_backends SET thinking_level_map = ? WHERE id = ?")
                .bind(sqlx::types::Json(map))
                .bind(&row.id)
                .execute(&mut *connection)
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RouteStore for SqliteRouteStore {
    async fn list(&self) -> anyhow::Result<Vec<RouteConfig>> {
        let mut connection = self.pool.acquire().await?;
        Self::load_routes(&mut connection, false).await
    }

    async fn list_active(&self) -> anyhow::Result<Vec<RouteConfig>> {
        let mut connection = self.pool.acquire().await?;
        Self::load_routes(&mut connection, true).await
    }

    async fn get(&self, route_id: &str) -> anyhow::Result<Option<RouteConfig>> {
        let mut connection = self.pool.acquire().await?;
        Self::load_route(&mut connection, route_id).await
    }

    async fn put(&self, route: PutRoute) -> anyhow::Result<RouteConfig> {
        if route
            .targets
            .as_ref()
            .is_some_and(|targets| !targets.iter().any(|target| target.enabled))
        {
            anyhow::bail!("a Route requires at least one enabled Target");
        }
        anyhow::ensure!(
            route.id.is_some() || route.targets.is_some(),
            "a new Route requires Targets"
        );
        let route_storage_id = route
            .id
            .as_ref()
            .map(|id| id.as_str().to_owned())
            .unwrap_or_else(stravia_runtime_contract::identifier::new_id);
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let conflict = sqlx::query_scalar::<_, String>(
            "SELECT id FROM models WHERE model_id = ? AND id != ? LIMIT 1",
        )
        .bind(route.model_id.as_str().trim())
        .bind(&route_storage_id)
        .fetch_optional(&mut *tx)
        .await?;
        if conflict.is_some() {
            anyhow::bail!(
                "Route ID already exists: {}",
                route.model_id.as_str().trim()
            );
        }

        if route.id.is_some() {
            let updated = sqlx::query(
                "UPDATE models SET model_id = ?, display_name = ?, balance = ?, is_enabled = ?, default_thinking_level = ? WHERE id = ?",
            )
            .bind(route.model_id.as_str().trim())
            .bind(route.display_name.as_deref())
            .bind(route.selection_strategy.trim())
            .bind(route.is_enabled)
            .bind(route.default_thinking_level.map(ThinkingLevel::as_str))
            .bind(&route_storage_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                anyhow::bail!("Route not found: {}", route.model_id.as_str().trim());
            }
        } else {
            sqlx::query(
                "INSERT INTO models (id, model_id, display_name, balance, is_enabled, default_thinking_level) VALUES (?, ?, ?, ?, ?, ?)",
            )
                .bind(&route_storage_id)
                .bind(route.model_id.as_str().trim())
                .bind(route.display_name.as_deref())
                .bind(route.selection_strategy.trim())
                .bind(route.is_enabled)
                .bind(route.default_thinking_level.map(ThinkingLevel::as_str))
                .execute(&mut *tx)
                .await?;
        }

        if let Some(targets) = route.targets.as_ref() {
            let existing = sqlx::query_as::<_, TargetRow>(
                "SELECT id, model_id, provider_id, model, enabled, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, created_at, thinking_level_map FROM model_backends WHERE model_id = ?",
            )
            .bind(&route_storage_id)
            .fetch_all(&mut *tx)
            .await?;
            for previous in &existing {
                if !targets.iter().any(|target| {
                    previous.provider_id == target.provider_id.trim()
                        && previous.model.as_deref() == target.model.as_deref().map(str::trim)
                }) {
                    sqlx::query("DELETE FROM model_backends WHERE id = ?")
                        .bind(&previous.id)
                        .execute(&mut *tx)
                        .await?;
                }
            }

            for target in targets {
                let id = existing
                    .iter()
                    .find(|row| {
                        row.provider_id == target.provider_id.trim()
                            && row.model.as_deref() == target.model.as_deref().map(str::trim)
                    })
                    .map(|row| row.id.clone())
                    .unwrap_or_else(stravia_runtime_contract::identifier::new_id);
                sqlx::query(
                    "INSERT INTO model_backends (id, model_id, provider_id, model, enabled, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, thinking_level_map) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, model = excluded.model, enabled = excluded.enabled, priority = excluded.priority, first_token_timeout_ms = excluded.first_token_timeout_ms, target_retry_budget = excluded.target_retry_budget, target_cooldown_ms = excluded.target_cooldown_ms, thinking_level_map = excluded.thinking_level_map",
                )
                .bind(id)
                .bind(&route_storage_id)
                .bind(target.provider_id.trim())
                .bind(target.model.as_deref().map(str::trim))
                .bind(target.enabled)
                .bind(target.priority.unwrap_or(DEFAULT_TARGET_PRIORITY))
                .bind(
                    target
                        .first_token_timeout_ms
                        .unwrap_or(DEFAULT_FIRST_TOKEN_TIMEOUT_MS),
                )
                .bind(
                    target
                        .target_retry_budget
                        .unwrap_or(DEFAULT_TARGET_RETRY_BUDGET),
                )
                .bind(
                    target
                        .target_cooldown_ms
                        .unwrap_or(DEFAULT_TARGET_COOLDOWN_MS),
                )
                .bind(sqlx::types::Json(&target.thinking_level_map))
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        drop(connection);
        self.get(route.model_id.as_str().trim())
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
            enabled: true,
            provider_id: provider_id.into(),
            model: Some(model.into()),
            priority: Some(0),
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
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
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![target("provider-1", "working-model")]),
                default_thinking_level: None,
            })
            .await
            .expect("initial Route");

        let failed = store
            .put(PutRoute {
                id: Some(route.id),
                model_id: "atomic-route".into(),
                display_name: None,
                selection_strategy: "latency_preference".into(),
                is_enabled: true,
                targets: Some(vec![target("missing-provider", "broken-model")]),
                default_thinking_level: None,
            })
            .await;
        assert!(failed.is_err());

        let persisted = store
            .get("atomic-route")
            .await
            .expect("get")
            .expect("Route");
        assert_eq!(persisted.balance, "traffic_equalization");
        assert_eq!(persisted.targets.len(), 1);
        assert_eq!(persisted.targets[0].provider_id(), "provider-1");
        assert_eq!(
            persisted.targets[0].model().map(|model| model.as_str()),
            Some("working-model")
        );

        sqlx::query("CREATE TRIGGER reject_target_delete BEFORE DELETE ON model_backends BEGIN SELECT RAISE(FAIL, 'target deleted'); END")
            .execute(&store.pool).await.expect("delete guard");
        sqlx::query("CREATE TRIGGER reject_target_insert BEFORE INSERT ON model_backends BEGIN SELECT RAISE(FAIL, 'target inserted'); END")
            .execute(&store.pool).await.expect("insert guard");
        let updated = store
            .put(PutRoute {
                id: Some(persisted.id.clone()),
                model_id: persisted.model_id.clone(),
                display_name: Some("Renamed".into()),
                selection_strategy: "latency_preference".into(),
                is_enabled: persisted.is_enabled,
                targets: None,
                default_thinking_level: None,
            })
            .await
            .expect("metadata-only patch must not write Target rows");
        assert_eq!(updated.display_name.as_deref(), Some("Renamed"));
        assert_eq!(updated.targets[0].id, persisted.targets[0].id);
        assert_eq!(
            updated.targets[0].created_at,
            persisted.targets[0].created_at
        );
    }

    #[tokio::test]
    async fn provider_only_target_round_trips_without_a_model_row() {
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
             ) VALUES ('research-provider', 'Research', 'open-responses', 'https://example.com', '', 'apikey')",
        )
        .execute(&pool)
        .await
        .expect("Provider");
        let store = SqliteRouteStore { pool };

        let route = store
            .put(PutRoute {
                id: None,
                model_id: "research-route".into(),
                display_name: None,
                selection_strategy: "traffic_equalization".into(),
                is_enabled: true,
                targets: Some(vec![crate::db::models::CreateTarget {
                    provider_id: "research-provider".into(),
                    model: None,
                    enabled: true,
                    priority: Some(0),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                }]),
                default_thinking_level: None,
            })
            .await
            .expect("Provider-only Route");

        assert!(
            route
                .primary_target()
                .is_some_and(|target| target.model().is_none())
        );
        assert!(route.targets[0].model().is_none());
        assert_eq!(route.targets[0].provider_id(), "research-provider");
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
                    selection_strategy: "traffic_equalization".into(),
                    is_enabled: true,
                    targets: Some(vec![target("provider-1", "provider-model")]),
                    default_thinking_level: None,
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
