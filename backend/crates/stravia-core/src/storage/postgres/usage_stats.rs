use crate::storage::RouteSchedulingUsage;

use super::*;

#[derive(Clone)]
pub(super) struct PostgresUsageStatsStore {
    pub(super) pool: Pool<Postgres>,
    pub(super) last_route_snapshot:
        Arc<std::sync::RwLock<Vec<crate::router::TargetSchedulingSnapshot>>>,
}

fn cutoff_ms(hours: Option<i64>) -> Option<i64> {
    hours.map(|hours| {
        chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(hours.saturating_mul(60 * 60 * 1_000))
    })
}

#[async_trait]
impl UsageStatsStore for PostgresUsageStatsStore {
    async fn route_scheduling_snapshot(&self) -> RouteSchedulingUsage {
        let now = chrono::Utc::now().timestamp_millis();
        let hour_ago = now.saturating_sub(60 * 60 * 1_000);
        let day_ago = now.saturating_sub(24 * 60 * 60 * 1_000);
        let result = sqlx::query_as::<_, crate::router::TargetSchedulingSnapshot>(
            "SELECT provider_id || ':' || upstream_model AS target_key,
                    CASE WHEN COUNT(*) > 0 AND COUNT(input_tokens) = COUNT(*) THEN SUM(input_tokens)::BIGINT END AS input_tokens_24h,
                    CASE WHEN COUNT(*) > 0 AND COUNT(output_tokens) = COUNT(*) THEN SUM(output_tokens)::BIGINT END AS output_tokens_24h,
                    CASE WHEN COUNT(*) > 0 AND COUNT(cache_read_tokens) = COUNT(*) THEN SUM(cache_read_tokens)::BIGINT END AS cache_read_tokens_24h,
                    CASE WHEN COUNT(*) > 0 AND COUNT(cache_write_tokens) = COUNT(*) THEN SUM(cache_write_tokens)::BIGINT END AS cache_write_tokens_24h,
                    SUM(CASE WHEN started_at >= $1 THEN 1 ELSE 0 END)::BIGINT AS attempts_1h,
                    SUM(CASE WHEN started_at >= $1 AND status = 'completed' THEN 1 ELSE 0 END)::BIGINT AS successes_1h,
                    CASE WHEN COUNT(*) FILTER (WHERE started_at >= $1 AND status = 'completed') > 0
                               AND COUNT(*) FILTER (WHERE started_at >= $1 AND status = 'completed' AND output_tokens IS NULL) = 0
                         THEN (SUM(output_tokens) FILTER (WHERE started_at >= $1 AND status = 'completed'))::BIGINT END AS successful_output_tokens_1h,
                    CASE WHEN COUNT(*) FILTER (WHERE started_at >= $1 AND status = 'completed') > 0
                               AND COUNT(*) FILTER (WHERE started_at >= $1 AND status = 'completed' AND duration_ms IS NULL) = 0
                         THEN (SUM(duration_ms) FILTER (WHERE started_at >= $1 AND status = 'completed'))::BIGINT END AS successful_upstream_ms_1h,
                    NULL::DOUBLE PRECISION AS cost_input,
                    NULL::DOUBLE PRECISION AS cost_output,
                    NULL::DOUBLE PRECISION AS cost_cache_read,
                    NULL::DOUBLE PRECISION AS cost_cache_write
             FROM target_attempt_observations
             WHERE started_at >= $2
               AND provider_id <> '' AND upstream_model <> ''
             GROUP BY provider_id, upstream_model",
        )
        .bind(hour_ago)
        .bind(day_ago)
        .fetch_all(&self.pool)
        .await;
        match result {
            Ok(targets) => {
                if let Ok(mut cached) = self.last_route_snapshot.write() {
                    *cached = targets.clone();
                }
                RouteSchedulingUsage {
                    targets,
                    stale: false,
                }
            }
            Err(error) => {
                tracing::warn!(%error, "failed to refresh confirmed route scheduling usage");
                let targets = self
                    .last_route_snapshot
                    .read()
                    .map(|cached| cached.clone())
                    .unwrap_or_default();
                RouteSchedulingUsage {
                    targets,
                    stale: true,
                }
            }
        }
    }

    async fn stats_overview(&self, hours: Option<i64>) -> anyhow::Result<StatsOverview> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, StatsOverview>(
            "WITH turns AS (
                 SELECT * FROM model_turn_observations WHERE ($1::BIGINT IS NULL OR started_at >= $1)
             ), attempts AS (
                 SELECT a.* FROM target_attempt_observations a JOIN turns t ON t.id = a.model_turn_id
             )
             SELECT
                 (SELECT COUNT(*)::BIGINT FROM turns) AS total_requests,
                 (SELECT CASE WHEN COUNT(*) > 0
                              AND COUNT(input_tokens) = COUNT(*)
                              AND COUNT(cache_read_tokens) = COUNT(*)
                         THEN SUM(GREATEST(input_tokens - cache_read_tokens, 0))::BIGINT END FROM attempts) AS total_input_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(output_tokens) = COUNT(*) THEN SUM(output_tokens)::BIGINT END FROM attempts) AS total_output_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(cache_read_tokens) = COUNT(*) THEN SUM(cache_read_tokens)::BIGINT END FROM attempts) AS total_cache_read_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(cache_write_tokens) = COUNT(*) THEN SUM(cache_write_tokens)::BIGINT END FROM attempts) AS total_cache_write_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(reasoning_tokens) = COUNT(*) THEN SUM(reasoning_tokens)::BIGINT END FROM attempts) AS total_reasoning_tokens,
                 (SELECT AVG((finished_at - started_at)::FLOAT8) FROM turns WHERE finished_at IS NOT NULL) AS avg_duration_ms,
                 (SELECT AVG(first_token_ms::FLOAT8) FROM attempts) AS avg_first_token_ms,
                 (SELECT COALESCE(SUM(CASE WHEN status <> 'completed' THEN 1 ELSE 0 END), 0)::BIGINT FROM turns) AS error_count",
        )
        .bind(cutoff)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn stats_series(
        &self,
        hours: i64,
        bucket_ms: i64,
        tz_offset_ms: i64,
    ) -> anyhow::Result<Vec<StatsSeries>> {
        let cutoff = cutoff_ms(Some(hours));
        let bucket_ms = bucket_ms.max(1);
        Ok(sqlx::query_as::<_, StatsSeries>(
            "WITH turns AS (
                 SELECT *, (started_at + $2) / $3 * $3 - $2 AS bucket_start
                 FROM model_turn_observations WHERE started_at >= $1
             ), turn_stats AS (
                 SELECT bucket_start, COUNT(*)::BIGINT AS request_count,
                        SUM(CASE WHEN status <> 'completed' THEN 1 ELSE 0 END)::BIGINT AS error_count,
                        AVG((finished_at - started_at)::FLOAT8) FILTER (WHERE finished_at IS NOT NULL) AS avg_duration_ms
                 FROM turns GROUP BY bucket_start
             ), attempt_stats AS (
                 SELECT t.bucket_start,
                        CASE WHEN COUNT(*) > 0
                                  AND COUNT(a.input_tokens) = COUNT(*)
                                  AND COUNT(a.cache_read_tokens) = COUNT(*)
                             THEN SUM(GREATEST(a.input_tokens - a.cache_read_tokens, 0))::BIGINT END AS total_input_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.output_tokens) = COUNT(*) THEN SUM(a.output_tokens)::BIGINT END AS total_output_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.cache_read_tokens) = COUNT(*) THEN SUM(a.cache_read_tokens)::BIGINT END AS total_cache_read_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.cache_write_tokens) = COUNT(*) THEN SUM(a.cache_write_tokens)::BIGINT END AS total_cache_write_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.reasoning_tokens) = COUNT(*) THEN SUM(a.reasoning_tokens)::BIGINT END AS total_reasoning_tokens,
                        AVG(a.first_token_ms::FLOAT8) AS avg_first_token_ms
                 FROM turns t JOIN target_attempt_observations a ON a.model_turn_id = t.id GROUP BY t.bucket_start
             )
             SELECT t.bucket_start, t.request_count, t.error_count,
                    a.total_input_tokens, a.total_output_tokens, a.total_cache_read_tokens,
                    a.total_cache_write_tokens, a.total_reasoning_tokens,
                    t.avg_duration_ms, a.avg_first_token_ms
             FROM turn_stats t LEFT JOIN attempt_stats a ON a.bucket_start = t.bucket_start
             ORDER BY t.bucket_start ASC",
        )
        .bind(cutoff)
        .bind(tz_offset_ms)
        .bind(bucket_ms)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_model(&self, hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, ModelStats>(
            "WITH turns AS (
                 SELECT *, COALESCE(NULLIF(model_display_name, ''), route_id) AS model
                 FROM model_turn_observations WHERE ($1::BIGINT IS NULL OR started_at >= $1)
             ), turn_stats AS (
                 SELECT model, COUNT(*)::BIGINT AS request_count,
                        AVG((finished_at - started_at)::FLOAT8) FILTER (WHERE finished_at IS NOT NULL) AS avg_duration_ms
                 FROM turns GROUP BY model
             ), attempt_stats AS (
                 SELECT t.model,
                        CASE WHEN COUNT(*) > 0
                                  AND COUNT(a.input_tokens) = COUNT(*)
                                  AND COUNT(a.cache_read_tokens) = COUNT(*)
                             THEN SUM(GREATEST(a.input_tokens - a.cache_read_tokens, 0))::BIGINT END AS total_input_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.output_tokens) = COUNT(*) THEN SUM(a.output_tokens)::BIGINT END AS total_output_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.reasoning_tokens) = COUNT(*) THEN SUM(a.reasoning_tokens)::BIGINT END AS total_reasoning_tokens
                 FROM turns t JOIN target_attempt_observations a ON a.model_turn_id = t.id GROUP BY t.model
             )
             SELECT t.model, t.request_count, a.total_input_tokens, a.total_output_tokens,
                    a.total_reasoning_tokens, t.avg_duration_ms
             FROM turn_stats t LEFT JOIN attempt_stats a ON a.model = t.model
             ORDER BY t.request_count DESC",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_provider(&self, hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, ProviderStats>(
            "SELECT COALESCE(NULLIF(provider_name, ''), provider_id) AS provider,
                    COUNT(*)::BIGINT AS request_count,
                    SUM(CASE WHEN status <> 'completed' THEN 1 ELSE 0 END)::BIGINT AS error_count,
                    AVG(duration_ms::FLOAT8) AS avg_duration_ms
             FROM target_attempt_observations
             WHERE ($1::BIGINT IS NULL OR started_at >= $1)
             GROUP BY COALESCE(NULLIF(provider_name, ''), provider_id)
             ORDER BY request_count DESC",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, ApiKeyStats>(
            "SELECT t.api_key_id,
                    COALESCE(MAX(NULLIF(t.api_key_name, '')), t.api_key_id) AS api_key_name,
                    COUNT(DISTINCT t.id)::BIGINT AS request_count,
                    CASE WHEN COUNT(a.id) > 0
                              AND COUNT(a.input_tokens) = COUNT(a.id)
                              AND COUNT(a.cache_read_tokens) = COUNT(a.id)
                         THEN SUM(GREATEST(a.input_tokens - a.cache_read_tokens, 0))::BIGINT END AS total_input_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.output_tokens) = COUNT(a.id) THEN SUM(a.output_tokens)::BIGINT END AS total_output_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.cache_read_tokens) = COUNT(a.id) THEN SUM(a.cache_read_tokens)::BIGINT END AS cache_read_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.cache_write_tokens) = COUNT(a.id) THEN SUM(a.cache_write_tokens)::BIGINT END AS cache_write_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.reasoning_tokens) = COUNT(a.id) THEN SUM(a.reasoning_tokens)::BIGINT END AS reasoning_tokens,
                    MAX(t.started_at)::BIGINT AS last_used_at
             FROM model_turn_observations t
             LEFT JOIN target_attempt_observations a ON a.model_turn_id = t.id
             WHERE t.api_key_id IS NOT NULL AND t.api_key_id <> ''
               AND ($1::BIGINT IS NULL OR t.started_at >= $1)
             GROUP BY t.api_key_id ORDER BY request_count DESC",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_turn(
        pool: &Pool<Postgres>,
        id: &str,
        started_at: i64,
        model: &str,
        api_key_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO interaction_observations
             (id,principal,api_key_id,api_key_name,root_id,root_run_id,first_route_id,status,started_at,last_active_at,expires_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(id)
        .bind("owner")
        .bind(api_key_id)
        .bind("Test key")
        .bind(id)
        .bind(id)
        .bind("route")
        .bind("completed")
        .bind(started_at)
        .bind(started_at)
        .bind(i64::MAX)
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO inference_run_observations
             (id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,finished_at,last_event_sequence,expires_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(id)
        .bind(id)
        .bind("responses")
        .bind("route")
        .bind("completed")
        .bind(false)
        .bind(started_at)
        .bind(started_at)
        .bind(started_at + 10)
        .bind(0_i64)
        .bind(i64::MAX)
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO model_turn_observations
             (id,run_id,interaction_id,route_id,model_display_name,api_key_id,api_key_name,status,started_at,finished_at,last_event_sequence)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(id)
        .bind(id)
        .bind(id)
        .bind("route")
        .bind(model)
        .bind(api_key_id)
        .bind("Test key")
        .bind("completed")
        .bind(started_at)
        .bind(started_at + 10)
        .bind(0_i64)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn insert_attempt(
        pool: &Pool<Postgres>,
        id: &str,
        turn_id: &str,
        started_at: i64,
        input_tokens: i64,
        cache_read_tokens: Option<i64>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO target_attempt_observations
             (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,status,started_at,finished_at,duration_ms,input_tokens,output_tokens,cache_read_tokens,reasoning_tokens,usage_recorded,last_event_sequence)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)",
        )
        .bind(id)
        .bind(turn_id)
        .bind(turn_id)
        .bind(turn_id)
        .bind("target")
        .bind("provider")
        .bind("Provider")
        .bind("upstream")
        .bind("responses")
        .bind("completed")
        .bind(started_at)
        .bind(started_at + 10)
        .bind(10_i64)
        .bind(input_tokens)
        .bind(3_i64)
        .bind(cache_read_tokens)
        .bind(1_i64)
        .bind(true)
        .bind(0_i64)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn postgres_management_stats_project_net_input_per_attempt() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("DB_URL") else {
            eprintln!("skip PostgreSQL usage stats verification: DB_URL is not set");
            return Ok(());
        };
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_usage_stats_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let result = async {
            let options: sqlx::postgres::PgConnectOptions = url.parse()?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with(options.options([("search_path", schema.as_str())]))
                .await?;
            let result = async {
                crate::migrations::migrate_postgres(&pool).await?;
                let now = chrono::Utc::now().timestamp_millis();
                let recent = now - 1_000;
                let old = now - 2 * 60 * 60 * 1_000;
                insert_turn(&pool, "recent", recent, "model", "key").await?;
                insert_attempt(&pool, "recent-a", "recent", recent, 12, Some(5)).await?;
                insert_attempt(&pool, "recent-b", "recent", recent, 3, Some(9)).await?;
                insert_turn(&pool, "old", old, "unknown-cache", "old-key").await?;
                insert_attempt(&pool, "old-a", "old", old, 8, None).await?;

                let store = PostgresUsageStatsStore {
                    pool: pool.clone(),
                    last_route_snapshot: Arc::new(std::sync::RwLock::new(Vec::new())),
                };
                let overview = store.stats_overview(Some(1)).await?;
                assert_eq!(overview.total_input_tokens, Some(7));
                assert_eq!(overview.total_output_tokens, Some(6));
                assert_eq!(overview.total_reasoning_tokens, Some(2));
                assert_eq!(store.stats_overview(None).await?.total_input_tokens, None);

                let series = store.stats_series(1, 3_600_000, 0).await?;
                assert_eq!(series.len(), 1);
                assert_eq!(series[0].total_input_tokens, Some(7));
                assert_eq!(series[0].total_output_tokens, Some(6));
                assert_eq!(series[0].total_reasoning_tokens, Some(2));
                assert_eq!(series[0].bucket_start % 3_600_000, 0);

                let models = store.stats_by_model(Some(1)).await?;
                assert_eq!(models.len(), 1);
                assert_eq!(models[0].total_input_tokens, Some(7));
                assert_eq!(models[0].total_output_tokens, Some(6));
                assert_eq!(models[0].total_reasoning_tokens, Some(2));

                let api_keys = store.stats_by_api_key(Some(1)).await?;
                assert_eq!(api_keys.len(), 1);
                assert_eq!(api_keys[0].total_input_tokens, Some(7));
                assert_eq!(api_keys[0].total_output_tokens, Some(6));
                assert_eq!(api_keys[0].reasoning_tokens, Some(2));

                let raw_input: Option<i64> = sqlx::query_scalar(
                    "SELECT SUM(input_tokens)::BIGINT FROM target_attempt_observations WHERE model_turn_id='recent'",
                )
                .fetch_one(&pool)
                .await?;
                assert_eq!(raw_input, Some(15));
                Ok::<_, anyhow::Error>(())
            }
            .await;
            pool.close().await;
            result
        }
        .await;
        let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await;
        admin.close().await;
        result?;
        cleanup?;
        Ok(())
    }
}
