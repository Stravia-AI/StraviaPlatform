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
pub(super) struct PostgresRouteStore {
    pub(super) pool: Pool<Postgres>,
}

impl PostgresRouteStore {
    async fn load_routes(&self, active_only: bool) -> anyhow::Result<Vec<RouteConfig>> {
        let mut conn = self.pool.acquire().await?;
        load_routes(&mut conn, active_only).await
    }

    async fn load_route(&self, route_id: &str) -> anyhow::Result<Option<RouteConfig>> {
        let mut conn = self.pool.acquire().await?;
        load_route(&mut conn, route_id).await
    }
}

/// Load Route rows together with their Targets on an existing connection, so a
/// caller transaction reads the same snapshot it is about to commit.
pub(super) async fn load_routes(
    conn: &mut sqlx::PgConnection,
    active_only: bool,
) -> anyhow::Result<Vec<RouteConfig>> {
    let where_clause = if active_only { " WHERE is_enabled" } else { "" };
    let sql = format!(
        "SELECT id, model_id, display_name, default_thinking_level, balance, \
         is_enabled, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at \
         FROM models{where_clause} ORDER BY created_at DESC"
    );
    let mut routes = sqlx::query_as::<_, RouteRow>(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(RouteRow::into_route)
        .collect::<Vec<_>>();
    for route in &mut routes {
        route.targets = load_targets(conn, &route.id).await?;
        route.refresh_supported_thinking_levels();
    }
    Ok(routes)
}

async fn load_targets(
    conn: &mut sqlx::PgConnection,
    route_storage_id: &str,
) -> anyhow::Result<Vec<TargetConfig>> {
    Ok(sqlx::query_as::<_, TargetRow>(
        "SELECT id, model_id, provider_id, model, enabled, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, thinking_level_map FROM model_backends WHERE model_id = $1 ORDER BY priority DESC, created_at ASC",
    )
    .bind(route_storage_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(TargetRow::into_target)
    .collect())
}

pub(super) async fn load_route(
    conn: &mut sqlx::PgConnection,
    route_id: &str,
) -> anyhow::Result<Option<RouteConfig>> {
    let route = sqlx::query_as::<_, RouteRow>(
        "SELECT id, model_id, display_name, default_thinking_level, balance, \
         is_enabled, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at \
         FROM models WHERE model_id = $1",
    )
    .bind(route_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(route) = route else {
        return Ok(None);
    };
    let mut route = route.into_route();
    route.targets = load_targets(conn, &route.id).await?;
    route.refresh_supported_thinking_levels();
    Ok(Some(route))
}

/// 调用方已在事务中按 models → model_backends 顺序阻止并发 Route 写入；
/// 此处读取最新绑定（含禁用项），只更新 Generated 行，不重建 Target。
pub(super) async fn refresh_generated_target_maps(
    conn: &mut sqlx::PgConnection,
    provider_id: &str,
    provider_model_id: &str,
    metadata: &crate::provider_models::ProviderModelMetadata,
    generated: &[crate::thinking::ThinkingLevelMapping],
    validate_map: &crate::provider_models::ReimportThinkingMapValidator<'_>,
) -> anyhow::Result<()> {
    let targets = sqlx::query_as::<_, (
        String,
        sqlx::types::Json<Vec<crate::thinking::ThinkingLevelMapping>>,
    )>(
        "SELECT id, thinking_level_map FROM model_backends WHERE provider_id = $1 AND model = $2 ORDER BY id",
    )
    .bind(provider_id)
    .bind(provider_model_id)
    .fetch_all(&mut *conn)
    .await?;
    for (id, map) in targets {
        let mut map = map.0;
        let changed = crate::thinking::refresh_generated_thinking_level_map(&mut map, generated)?;
        validate_map(metadata, &map)?;
        if changed {
            sqlx::query("UPDATE model_backends SET thinking_level_map = $1 WHERE id = $2")
                .bind(sqlx::types::Json(&map))
                .bind(&id)
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

#[async_trait]
impl RouteStore for PostgresRouteStore {
    async fn list(&self) -> anyhow::Result<Vec<RouteConfig>> {
        self.load_routes(false).await
    }

    async fn list_active(&self) -> anyhow::Result<Vec<RouteConfig>> {
        self.load_routes(true).await
    }

    async fn get(&self, route_id: &str) -> anyhow::Result<Option<RouteConfig>> {
        self.load_route(route_id).await
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
        let mut tx = self.pool.begin().await?;
        let conflict = sqlx::query_scalar::<_, String>(
            "SELECT id FROM models WHERE model_id = $1 AND id != $2 LIMIT 1",
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
                "UPDATE models SET model_id = $1, display_name = $2, balance = $3, is_enabled = $4, default_thinking_level = $5 WHERE id = $6",
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
                "INSERT INTO models (id, model_id, display_name, balance, is_enabled, default_thinking_level) VALUES ($1, $2, $3, $4, $5, $6)",
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
                "SELECT id, model_id, provider_id, model, enabled, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, thinking_level_map FROM model_backends WHERE model_id = $1",
            )
            .bind(&route_storage_id)
            .fetch_all(&mut *tx)
            .await?;
            for previous in &existing {
                if !targets.iter().any(|target| {
                    previous.provider_id == target.provider_id.trim()
                        && previous.model.as_deref() == target.model.as_deref().map(str::trim)
                }) {
                    sqlx::query("DELETE FROM model_backends WHERE id = $1")
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
                    "INSERT INTO model_backends (id, model_id, provider_id, model, enabled, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, thinking_level_map) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT(id) DO UPDATE SET provider_id = EXCLUDED.provider_id, model = EXCLUDED.model, enabled = EXCLUDED.enabled, priority = EXCLUDED.priority, first_token_timeout_ms = EXCLUDED.first_token_timeout_ms, target_retry_budget = EXCLUDED.target_retry_budget, target_cooldown_ms = EXCLUDED.target_cooldown_ms, thinking_level_map = EXCLUDED.thinking_level_map",
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
        self.get(route.model_id.as_str().trim())
            .await?
            .context("Route missing after put")
    }

    async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM models WHERE model_id = $1")
            .bind(route_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
