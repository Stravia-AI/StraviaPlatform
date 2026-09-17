use crate::storage::RouteSchedulingUsage;

use super::*;

#[derive(Clone)]
pub(super) struct SqliteUsageStatsStore {
    pub(super) pool: SqlitePool,
    pub(super) last_route_snapshot:
        Arc<parking_lot::RwLock<Vec<crate::router::TargetSchedulingSnapshot>>>,
}

fn cutoff_ms(hours: Option<i64>) -> Option<i64> {
    hours.map(|hours| {
        chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(hours.saturating_mul(60 * 60 * 1_000))
    })
}

#[async_trait]
impl UsageStatsStore for SqliteUsageStatsStore {
    async fn route_scheduling_snapshot(&self) -> RouteSchedulingUsage {
        let now = chrono::Utc::now().timestamp_millis();
        let hour_ago = now.saturating_sub(60 * 60 * 1_000);
        let day_ago = now.saturating_sub(24 * 60 * 60 * 1_000);
        let result = sqlx::query_as::<_, crate::router::TargetSchedulingSnapshot>(
            "SELECT provider_id || ':' || upstream_model AS target_key,
                    CASE WHEN COUNT(*) > 0 AND COUNT(input_tokens) = COUNT(*) THEN SUM(input_tokens) END AS input_tokens_24h,
                    CASE WHEN COUNT(*) > 0 AND COUNT(output_tokens) = COUNT(*) THEN SUM(output_tokens) END AS output_tokens_24h,
                    CASE WHEN COUNT(*) > 0 AND COUNT(cache_read_tokens) = COUNT(*) THEN SUM(cache_read_tokens) END AS cache_read_tokens_24h,
                    CASE WHEN COUNT(*) > 0 AND COUNT(cache_write_tokens) = COUNT(*) THEN SUM(cache_write_tokens) END AS cache_write_tokens_24h,
                    SUM(CASE WHEN started_at >= ? THEN 1 ELSE 0 END) AS attempts_1h,
                    SUM(CASE WHEN started_at >= ? AND status = 'completed' THEN 1 ELSE 0 END) AS successes_1h,
                    CASE WHEN SUM(CASE WHEN started_at >= ? AND status = 'completed' THEN 1 ELSE 0 END) > 0
                               AND SUM(CASE WHEN started_at >= ? AND status = 'completed' AND output_tokens IS NULL THEN 1 ELSE 0 END) = 0
                         THEN SUM(CASE WHEN started_at >= ? AND status = 'completed' THEN output_tokens END) END AS successful_output_tokens_1h,
                    CASE WHEN SUM(CASE WHEN started_at >= ? AND status = 'completed' THEN 1 ELSE 0 END) > 0
                               AND SUM(CASE WHEN started_at >= ? AND status = 'completed' AND duration_ms IS NULL THEN 1 ELSE 0 END) = 0
                         THEN SUM(CASE WHEN started_at >= ? AND status = 'completed' THEN duration_ms END) END AS successful_upstream_ms_1h,
                    NULL AS cost_input, NULL AS cost_output,
                    NULL AS cost_cache_read, NULL AS cost_cache_write
             FROM target_attempt_observations
             WHERE started_at >= ?
               AND provider_id <> '' AND upstream_model <> ''
             GROUP BY provider_id, upstream_model",
        )
        .bind(hour_ago)
        .bind(hour_ago)
        .bind(hour_ago)
        .bind(hour_ago)
        .bind(hour_ago)
        .bind(hour_ago)
        .bind(hour_ago)
        .bind(hour_ago)
        .bind(day_ago)
        .fetch_all(&self.pool)
        .await;
        match result {
            Ok(targets) => {
                *self.last_route_snapshot.write() = targets.clone();
                RouteSchedulingUsage {
                    targets,
                    stale: false,
                }
            }
            Err(error) => {
                tracing::warn!(%error, "failed to refresh confirmed route scheduling usage");
                let targets = self.last_route_snapshot.read().clone();
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
                 SELECT * FROM model_turn_observations WHERE (? IS NULL OR started_at >= ?)
             ), attempts AS (
                 SELECT a.* FROM target_attempt_observations a JOIN turns t ON t.id = a.model_turn_id
             )
             SELECT
                 (SELECT COUNT(*) FROM turns) AS total_requests,
                 (SELECT CASE WHEN COUNT(*) > 0
                              AND COUNT(input_tokens) = COUNT(*)
                              AND COUNT(cache_read_tokens) = COUNT(*)
                         THEN SUM(MAX(input_tokens - cache_read_tokens, 0)) END FROM attempts) AS total_input_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(output_tokens) = COUNT(*) THEN SUM(output_tokens) END FROM attempts) AS total_output_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(cache_read_tokens) = COUNT(*) THEN SUM(cache_read_tokens) END FROM attempts) AS total_cache_read_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(cache_write_tokens) = COUNT(*) THEN SUM(cache_write_tokens) END FROM attempts) AS total_cache_write_tokens,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(reasoning_tokens) = COUNT(*) THEN SUM(reasoning_tokens) END FROM attempts) AS total_reasoning_tokens,
                 (SELECT AVG(finished_at - started_at) FROM turns WHERE finished_at IS NOT NULL) AS avg_duration_ms,
                 (SELECT AVG(first_token_ms) FROM attempts) AS avg_first_token_ms,
                 (SELECT COALESCE(SUM(CASE WHEN status <> 'completed' THEN 1 ELSE 0 END), 0) FROM turns) AS error_count",
        )
        .bind(cutoff)
        .bind(cutoff)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn stats_hourly(&self, hours: i64) -> anyhow::Result<Vec<StatsHourly>> {
        let cutoff = cutoff_ms(Some(hours));
        Ok(sqlx::query_as::<_, StatsHourly>(
            "WITH turns AS (
                 SELECT *, strftime('%Y-%m-%d %H:00:00', datetime(started_at / 1000, 'unixepoch')) AS hour
                 FROM model_turn_observations WHERE started_at >= ?
             ), turn_stats AS (
                 SELECT hour, COUNT(*) AS request_count,
                        SUM(CASE WHEN status <> 'completed' THEN 1 ELSE 0 END) AS error_count,
                        AVG(CASE WHEN finished_at IS NOT NULL THEN finished_at - started_at END) AS avg_duration_ms
                 FROM turns GROUP BY hour
             ), attempt_stats AS (
                 SELECT t.hour,
                        CASE WHEN COUNT(*) > 0
                                  AND COUNT(a.input_tokens) = COUNT(*)
                                  AND COUNT(a.cache_read_tokens) = COUNT(*)
                             THEN SUM(MAX(a.input_tokens - a.cache_read_tokens, 0)) END AS total_input_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.output_tokens) = COUNT(*) THEN SUM(a.output_tokens) END AS total_output_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.cache_read_tokens) = COUNT(*) THEN SUM(a.cache_read_tokens) END AS total_cache_read_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.cache_write_tokens) = COUNT(*) THEN SUM(a.cache_write_tokens) END AS total_cache_write_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.reasoning_tokens) = COUNT(*) THEN SUM(a.reasoning_tokens) END AS total_reasoning_tokens,
                        AVG(a.first_token_ms) AS avg_first_token_ms
                 FROM turns t JOIN target_attempt_observations a ON a.model_turn_id = t.id GROUP BY t.hour
             )
             SELECT t.hour, t.request_count, t.error_count,
                    a.total_input_tokens, a.total_output_tokens, a.total_cache_read_tokens,
                    a.total_cache_write_tokens, a.total_reasoning_tokens,
                    t.avg_duration_ms, a.avg_first_token_ms
             FROM turn_stats t LEFT JOIN attempt_stats a ON a.hour = t.hour ORDER BY t.hour ASC",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_model(&self, hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, ModelStats>(
            "WITH turns AS (
                 SELECT *, COALESCE(NULLIF(model_display_name, ''), route_id) AS model
                 FROM model_turn_observations WHERE (? IS NULL OR started_at >= ?)
             ), turn_stats AS (
                 SELECT model, COUNT(*) AS request_count,
                        AVG(CASE WHEN finished_at IS NOT NULL THEN finished_at - started_at END) AS avg_duration_ms
                 FROM turns GROUP BY model
             ), attempt_stats AS (
                 SELECT t.model,
                        CASE WHEN COUNT(*) > 0
                                  AND COUNT(a.input_tokens) = COUNT(*)
                                  AND COUNT(a.cache_read_tokens) = COUNT(*)
                             THEN SUM(MAX(a.input_tokens - a.cache_read_tokens, 0)) END AS total_input_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.output_tokens) = COUNT(*) THEN SUM(a.output_tokens) END AS total_output_tokens,
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.reasoning_tokens) = COUNT(*) THEN SUM(a.reasoning_tokens) END AS total_reasoning_tokens
                 FROM turns t JOIN target_attempt_observations a ON a.model_turn_id = t.id GROUP BY t.model
             )
             SELECT t.model, t.request_count, a.total_input_tokens, a.total_output_tokens,
                    a.total_reasoning_tokens, t.avg_duration_ms
             FROM turn_stats t LEFT JOIN attempt_stats a ON a.model = t.model
             ORDER BY t.request_count DESC",
        )
        .bind(cutoff)
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_provider(&self, hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, ProviderStats>(
            "SELECT COALESCE(NULLIF(provider_name, ''), provider_id) AS provider,
                    COUNT(*) AS request_count,
                    SUM(CASE WHEN status <> 'completed' THEN 1 ELSE 0 END) AS error_count,
                    AVG(duration_ms) AS avg_duration_ms
             FROM target_attempt_observations
             WHERE (? IS NULL OR started_at >= ?)
             GROUP BY COALESCE(NULLIF(provider_name, ''), provider_id)
             ORDER BY request_count DESC",
        )
        .bind(cutoff)
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, ApiKeyStats>(
            "SELECT t.api_key_id,
                    COALESCE(MAX(NULLIF(t.api_key_name, '')), t.api_key_id) AS api_key_name,
                    COUNT(DISTINCT t.id) AS request_count,
                    CASE WHEN COUNT(a.id) > 0
                              AND COUNT(a.input_tokens) = COUNT(a.id)
                              AND COUNT(a.cache_read_tokens) = COUNT(a.id)
                         THEN SUM(MAX(a.input_tokens - a.cache_read_tokens, 0)) END AS total_input_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.output_tokens) = COUNT(a.id) THEN SUM(a.output_tokens) END AS total_output_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.cache_read_tokens) = COUNT(a.id) THEN SUM(a.cache_read_tokens) END AS cache_read_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.cache_write_tokens) = COUNT(a.id) THEN SUM(a.cache_write_tokens) END AS cache_write_tokens,
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.reasoning_tokens) = COUNT(a.id) THEN SUM(a.reasoning_tokens) END AS reasoning_tokens,
                    MAX(t.started_at) AS last_used_at
             FROM model_turn_observations t
             LEFT JOIN target_attempt_observations a ON a.model_turn_id = t.id
             WHERE t.api_key_id IS NOT NULL AND t.api_key_id <> ''
               AND (? IS NULL OR t.started_at >= ?)
             GROUP BY t.api_key_id ORDER BY request_count DESC",
        )
        .bind(cutoff)
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_turn(
        pool: &SqlitePool,
        id: &str,
        started_at: i64,
        model: &str,
        api_key_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO interaction_observations
             (id,principal,api_key_id,api_key_name,root_id,root_run_id,first_route_id,status,started_at,last_active_at,expires_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
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
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
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
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
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
        pool: &SqlitePool,
        id: &str,
        turn_id: &str,
        started_at: i64,
        input_tokens: i64,
        cache_read_tokens: Option<i64>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO target_attempt_observations
             (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,status,started_at,finished_at,duration_ms,input_tokens,output_tokens,cache_read_tokens,reasoning_tokens,usage_recorded,last_event_sequence)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
    async fn management_stats_project_net_input_per_attempt() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool).await?;
        let now = chrono::Utc::now().timestamp_millis();
        let recent = now - 1_000;
        let old = now - 2 * 60 * 60 * 1_000;
        insert_turn(&pool, "recent", recent, "model", "key").await?;
        insert_attempt(&pool, "recent-a", "recent", recent, 12, Some(5)).await?;
        insert_attempt(&pool, "recent-b", "recent", recent, 3, Some(9)).await?;
        insert_turn(&pool, "old", old, "unknown-cache", "old-key").await?;
        insert_attempt(&pool, "old-a", "old", old, 8, None).await?;

        let store = SqliteUsageStatsStore {
            pool: pool.clone(),
            last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
        };
        let overview = store.stats_overview(Some(1)).await?;
        assert_eq!(overview.total_input_tokens, Some(7));
        assert_eq!(overview.total_output_tokens, Some(6));
        assert_eq!(overview.total_reasoning_tokens, Some(2));
        assert_eq!(store.stats_overview(None).await?.total_input_tokens, None);

        let hourly = store.stats_hourly(1).await?;
        assert_eq!(hourly.len(), 1);
        assert_eq!(hourly[0].total_input_tokens, Some(7));
        assert_eq!(hourly[0].total_output_tokens, Some(6));
        assert_eq!(hourly[0].total_reasoning_tokens, Some(2));

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
            "SELECT SUM(input_tokens) FROM target_attempt_observations WHERE model_turn_id='recent'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(raw_input, Some(15));
        pool.close().await;
        Ok(())
    }
}
