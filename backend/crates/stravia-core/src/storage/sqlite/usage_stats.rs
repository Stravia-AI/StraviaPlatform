use crate::storage::RouteSchedulingUsage;

use super::*;

#[derive(Clone)]
pub(super) struct SqliteUsageStatsStore {
    pub(super) pool: SqlitePool,
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
                 SELECT * FROM model_turn_observations WHERE (? IS NULL OR started_at >= ?)
             ), attempts AS (
                 SELECT a.* FROM target_attempt_observations a JOIN turns t ON t.id = a.model_turn_id
             )
             SELECT
                 (SELECT COUNT(*) FROM turns) AS total_requests,
                 (SELECT CASE WHEN COUNT(*) > 0 AND COUNT(input_tokens) = COUNT(*) THEN SUM(input_tokens) END FROM attempts) AS total_input_tokens,
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
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.input_tokens) = COUNT(*) THEN SUM(a.input_tokens) END AS total_input_tokens,
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
                        CASE WHEN COUNT(*) > 0 AND COUNT(a.input_tokens) = COUNT(*) THEN SUM(a.input_tokens) END AS total_input_tokens,
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
                    CASE WHEN COUNT(a.id) > 0 AND COUNT(a.input_tokens) = COUNT(a.id) THEN SUM(a.input_tokens) END AS total_input_tokens,
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
