use crate::{
    db::models::{StatsQueryError, StatsWindow},
    storage::RouteSchedulingUsage,
};

use super::*;

#[derive(Clone)]
pub(super) struct SqliteUsageStatsStore {
    pub(super) pool: SqlitePool,
    pub(super) last_route_snapshot:
        Arc<parking_lot::RwLock<Vec<crate::router::TargetSchedulingSnapshot>>>,
}

const BASE_BUCKET_MS: i64 = 900_000;

const MONEY_TOTALS_COLUMNS: &str = "COALESCE(SUM(cost_custom_nanos), 0) AS cost_custom_nanos,
       COALESCE(SUM(cost_usd_unconverted_nanos), 0) AS cost_usd_unconverted_nanos,
       COALESCE(SUM(cost_known_count), 0) AS cost_known_count,
       COALESCE(SUM(cost_unknown_count), 0) AS cost_unknown_count,
       COALESCE(SUM(payg_cost_custom_nanos), 0) AS payg_cost_custom_nanos,
       COALESCE(SUM(payg_cost_usd_unconverted_nanos), 0) AS payg_cost_usd_unconverted_nanos,
       COALESCE(SUM(payg_cost_known_count), 0) AS payg_cost_known_count,
       COALESCE(SUM(payg_cost_unknown_count), 0) AS payg_cost_unknown_count,
       COALESCE(SUM(billing_custom_nanos), 0) AS billing_custom_nanos,
       COALESCE(SUM(billing_usd_unconverted_nanos), 0) AS billing_usd_unconverted_nanos,
       COALESCE(SUM(billing_charged_count), 0) AS billing_charged_count,
       COALESCE(SUM(billing_unknown_count), 0) AS billing_unknown_count";

// 调度只消费最近一小时：用量 SUM 逐维度忽略未知，全未知才为 NULL；
// 派生 uncached 仅以 input 是否已知为门槛，cache_read 未知时按 0 抵扣，不回写原始 SUM；
// 状态完整性仍用 FILTER 限定在成功样本上。只设下界，不排除本机时钟偏差导致的未来记录。
const ROUTE_SCHEDULING_SQL: &str =
    "SELECT provider_id || ':' || upstream_model AS target_key,
        SUM(input_tokens) AS input_tokens_1h,
        SUM(output_tokens) AS output_tokens_1h,
        SUM(cache_read_tokens) AS cache_read_tokens_1h,
        SUM(cache_write_tokens) AS cache_write_tokens_1h,
        SUM(CASE WHEN input_tokens IS NOT NULL
                 THEN MAX(input_tokens - COALESCE(cache_read_tokens, 0), 0) END) AS uncached_input_tokens_1h,
        COUNT(*) AS attempts_1h,
        COUNT(*) FILTER (WHERE status = 'completed') AS successes_1h,
        CASE WHEN COUNT(*) FILTER (WHERE status = 'completed') > 0
                   AND COUNT(output_tokens) FILTER (WHERE status = 'completed')
                     = COUNT(*) FILTER (WHERE status = 'completed')
             THEN SUM(output_tokens) FILTER (WHERE status = 'completed') END AS successful_output_tokens_1h,
        CASE WHEN COUNT(*) FILTER (WHERE status = 'completed') > 0
                   AND COUNT(duration_ms) FILTER (WHERE status = 'completed')
                     = COUNT(*) FILTER (WHERE status = 'completed')
             THEN SUM(duration_ms) FILTER (WHERE status = 'completed') END AS successful_upstream_ms_1h,
        NULL AS cost_input, NULL AS cost_output,
        NULL AS cost_cache_read, NULL AS cost_cache_write
             FROM target_attempt_observations
             WHERE started_at >= ?1
               AND provider_id <> '' AND upstream_model <> ''
             GROUP BY provider_id, upstream_model";

// Display buckets start at the selected boundary and retain the final partial bucket.
fn display_bucket_ms(start: i64, end: i64, requested: i64) -> i64 {
    let span = i128::from(end) - i128::from(start);
    let minimum = ((span + 511) / 512).max(i128::from(requested));
    (((minimum + i128::from(BASE_BUCKET_MS) - 1) / i128::from(BASE_BUCKET_MS))
        * i128::from(BASE_BUCKET_MS)) as i64
}

#[async_trait]
impl UsageStatsStore for SqliteUsageStatsStore {
    async fn route_scheduling_snapshot(&self) -> RouteSchedulingUsage {
        let now = chrono::Utc::now().timestamp_millis();
        let hour_ago = now.saturating_sub(60 * 60 * 1_000);
        let result =
            sqlx::query_as::<_, crate::router::TargetSchedulingSnapshot>(ROUTE_SCHEDULING_SQL)
                .bind(hour_ago)
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

    async fn stats_overview(&self, window: StatsWindow) -> anyhow::Result<StatsOverview> {
        window.validate()?;
        Ok(sqlx::query_as::<_, StatsOverview>(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(SUM(request_count), 0) AS total_requests,
       COALESCE(SUM(terminal_request_count), 0) AS terminal_request_count,
       COALESCE(SUM(failed_request_count), 0) AS failed_request_count,
       COALESCE(SUM(upstream_request_count), 0) AS upstream_request_count,
       COALESCE(SUM(upstream_error_count), 0) AS upstream_error_count,
       CASE WHEN SUM(input_tokens_known) > 0 THEN SUM(input_tokens_sum) END AS total_input_tokens,
       CASE WHEN SUM(output_tokens_known) > 0 THEN SUM(output_tokens_sum) END AS total_output_tokens,
       CASE WHEN SUM(cache_read_tokens_known) > 0 THEN SUM(cache_read_tokens_sum) END AS total_cache_read_tokens,
       CASE WHEN SUM(cache_write_tokens_known) > 0 THEN SUM(cache_write_tokens_sum) END AS total_cache_write_tokens,
       CASE WHEN SUM(reasoning_tokens_known) > 0 THEN SUM(reasoning_tokens_sum) END AS total_reasoning_tokens,
       SUM(duration_ms_sum) * 1.0 / NULLIF(SUM(duration_ms_count), 0) AS avg_duration_ms,
       SUM(first_token_ms_sum) * 1.0 / NULLIF(SUM(first_token_ms_count), 0) AS avg_first_token_ms,
       CASE WHEN SUM(successful_attempt_count) > 0 AND SUM(successful_output_known_count) = SUM(successful_attempt_count) AND SUM(successful_duration_known_count) = SUM(successful_attempt_count) AND SUM(successful_duration_sum) > 0 THEN SUM(successful_output_sum) * 1000.0 / SUM(successful_duration_sum) END AS avg_output_tps,
       {MONEY_TOTALS_COLUMNS} FROM stats_usage_buckets WHERE scope = 'global' AND bucket_start >= COALESCE(?1, -9223372036854775808) AND bucket_start < COALESCE(?2, 9223372036854775807)")),
        )
        .bind(window.start_at)
        .bind(window.end_at)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn stats_series(
        &self,
        window: StatsWindow,
        bucket_ms: i64,
    ) -> anyhow::Result<Vec<StatsSeries>> {
        window.validate()?;
        if bucket_ms < BASE_BUCKET_MS || bucket_ms % BASE_BUCKET_MS != 0 {
            return Err(
                StatsQueryError("bucket must be a positive multiple of 900 seconds").into(),
            );
        }
        let (start, end) = match (window.start_at, window.end_at) {
            (Some(start), Some(end)) => (start, end),
            _ => {
                let (start, end): (Option<i64>, Option<i64>) = sqlx::query_as(
                            "SELECT MIN(bucket_start), MAX(bucket_start) FROM stats_usage_buckets WHERE scope = 'global'",
                        ).fetch_one(&self.pool).await?;
                let (Some(start), Some(end)) = (start, end) else {
                    return Ok(Vec::new());
                };
                (start, end.saturating_add(BASE_BUCKET_MS))
            }
        };
        let bucket_ms = display_bucket_ms(start, end, bucket_ms);
        Ok(sqlx::query_as::<_, StatsSeries>(sqlx::AssertSqlSafe(format!(
            "SELECT (bucket_start - ?4) / ?3 * ?3 + ?4 AS bucket_start, ?3 AS bucket_ms, COALESCE(SUM(request_count), 0) AS request_count,
       COALESCE(SUM(terminal_request_count), 0) AS terminal_request_count,
       COALESCE(SUM(failed_request_count), 0) AS failed_request_count,
       COALESCE(SUM(upstream_request_count), 0) AS upstream_request_count,
       COALESCE(SUM(upstream_error_count), 0) AS upstream_error_count,
       CASE WHEN SUM(input_tokens_known) > 0 THEN SUM(input_tokens_sum) END AS total_input_tokens,
       CASE WHEN SUM(output_tokens_known) > 0 THEN SUM(output_tokens_sum) END AS total_output_tokens,
       CASE WHEN SUM(cache_read_tokens_known) > 0 THEN SUM(cache_read_tokens_sum) END AS total_cache_read_tokens,
       CASE WHEN SUM(cache_write_tokens_known) > 0 THEN SUM(cache_write_tokens_sum) END AS total_cache_write_tokens,
       CASE WHEN SUM(reasoning_tokens_known) > 0 THEN SUM(reasoning_tokens_sum) END AS total_reasoning_tokens,
       SUM(duration_ms_sum) * 1.0 / NULLIF(SUM(duration_ms_count), 0) AS avg_duration_ms,
       SUM(first_token_ms_sum) * 1.0 / NULLIF(SUM(first_token_ms_count), 0) AS avg_first_token_ms,
       CASE WHEN SUM(successful_attempt_count) > 0 AND SUM(successful_output_known_count) = SUM(successful_attempt_count) AND SUM(successful_duration_known_count) = SUM(successful_attempt_count) AND SUM(successful_duration_sum) > 0 THEN SUM(successful_output_sum) * 1000.0 / SUM(successful_duration_sum) END AS avg_output_tps,
       {MONEY_TOTALS_COLUMNS} FROM stats_usage_buckets WHERE scope = 'global' AND bucket_start >= COALESCE(?1, -9223372036854775808) AND bucket_start < COALESCE(?2, 9223372036854775807) GROUP BY (bucket_start - ?4) / ?3 * ?3 + ?4 ORDER BY bucket_start")),
        )
        .bind(start)
        .bind(end)
        .bind(bucket_ms)
        .bind(start)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_model(&self, window: StatsWindow) -> anyhow::Result<Vec<ModelStats>> {
        window.validate()?;
        Ok(sqlx::query_as::<_, ModelStats>(sqlx::AssertSqlSafe(format!(
            "SELECT entity_id AS model_id, MAX(label) AS model, NOT EXISTS (SELECT 1 FROM models live WHERE live.id = b.entity_id) AS deleted, COALESCE(SUM(request_count), 0) AS request_count,
       CASE WHEN SUM(input_tokens_known) > 0 THEN SUM(input_tokens_sum) END AS total_input_tokens,
       CASE WHEN SUM(output_tokens_known) > 0 THEN SUM(output_tokens_sum) END AS total_output_tokens,
       CASE WHEN SUM(reasoning_tokens_known) > 0 THEN SUM(reasoning_tokens_sum) END AS total_reasoning_tokens,
       SUM(duration_ms_sum) * 1.0 / NULLIF(SUM(duration_ms_count), 0) AS avg_duration_ms,
       {MONEY_TOTALS_COLUMNS} FROM stats_usage_buckets b WHERE scope = 'model' AND bucket_start >= COALESCE(?1, -9223372036854775808) AND bucket_start < COALESCE(?2, 9223372036854775807) GROUP BY identity, entity_id ORDER BY request_count DESC, identity")),
        )
        .bind(window.start_at)
        .bind(window.end_at)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_provider(&self, window: StatsWindow) -> anyhow::Result<Vec<ProviderStats>> {
        window.validate()?;
        Ok(sqlx::query_as::<_, ProviderStats>(sqlx::AssertSqlSafe(format!(
            "SELECT entity_id AS provider_id, MAX(label) AS provider, NOT EXISTS (SELECT 1 FROM providers live WHERE live.id = b.entity_id) AS deleted, COALESCE(SUM(upstream_request_count), 0) AS upstream_request_count,
       COALESCE(SUM(upstream_error_count), 0) AS upstream_error_count,
       SUM(duration_ms_sum) * 1.0 / NULLIF(SUM(duration_ms_count), 0) AS avg_duration_ms,
       CASE WHEN SUM(successful_attempt_count) > 0 AND SUM(successful_output_known_count) = SUM(successful_attempt_count) AND SUM(successful_duration_known_count) = SUM(successful_attempt_count) AND SUM(successful_duration_sum) > 0 THEN SUM(successful_output_sum) * 1000.0 / SUM(successful_duration_sum) END AS avg_output_tps,
       {MONEY_TOTALS_COLUMNS} FROM stats_usage_buckets b WHERE scope = 'provider' AND bucket_start >= COALESCE(?1, -9223372036854775808) AND bucket_start < COALESCE(?2, 9223372036854775807) GROUP BY identity, entity_id ORDER BY upstream_request_count DESC, identity")),
        )
        .bind(window.start_at)
        .bind(window.end_at)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_api_key(&self, window: StatsWindow) -> anyhow::Result<Vec<ApiKeyStats>> {
        window.validate()?;
        Ok(sqlx::query_as::<_, ApiKeyStats>(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(entity_id, '') AS api_key_id, MAX(label) AS api_key_name, NOT EXISTS (SELECT 1 FROM api_keys live WHERE live.id = b.entity_id) AS deleted, COALESCE(SUM(request_count), 0) AS request_count,
       CASE WHEN SUM(input_tokens_known) > 0 THEN SUM(input_tokens_sum) END AS total_input_tokens,
       CASE WHEN SUM(output_tokens_known) > 0 THEN SUM(output_tokens_sum) END AS total_output_tokens,
       CASE WHEN SUM(cache_read_tokens_known) > 0 THEN SUM(cache_read_tokens_sum) END AS cache_read_tokens,
       CASE WHEN SUM(cache_write_tokens_known) > 0 THEN SUM(cache_write_tokens_sum) END AS cache_write_tokens,
       CASE WHEN SUM(reasoning_tokens_known) > 0 THEN SUM(reasoning_tokens_sum) END AS reasoning_tokens,
       MAX(last_used_at) AS last_used_at,
       {MONEY_TOTALS_COLUMNS} FROM stats_usage_buckets b WHERE scope = 'api_key' AND bucket_start >= COALESCE(?1, -9223372036854775808) AND bucket_start < COALESCE(?2, 9223372036854775807) GROUP BY identity, entity_id ORDER BY request_count DESC, identity")),
        )
        .bind(window.start_at)
        .bind(window.end_at)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(hours: i64) -> StatsWindow {
        let end = chrono::Utc::now().timestamp_millis() / BASE_BUCKET_MS * BASE_BUCKET_MS
            + BASE_BUCKET_MS;
        StatsWindow {
            start_at: Some(end - hours * 3_600_000),
            end_at: Some(end),
        }
    }

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
        .bind(model)
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

    async fn insert_failed_attempt(
        pool: &SqlitePool,
        id: &str,
        turn_id: &str,
        started_at: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO target_attempt_observations
             (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,status,started_at,finished_at,duration_ms,last_event_sequence)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
        .bind("failed")
        .bind(started_at)
        .bind(started_at + 10)
        .bind(10_i64)
        .bind(0_i64)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn management_errors_count_only_final_failed_requests() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let now = chrono::Utc::now().timestamp_millis();
        let recent = now - 1_000;
        for (id, run_status, turn_status, reason, finished, expires_at) in [
            ("recovered", "completed", "completed", None, true, i64::MAX),
            ("running", "running", "running", None, false, i64::MAX),
            (
                "cancelled",
                "cancelled",
                "cancelled",
                Some("cancelled"),
                true,
                i64::MAX,
            ),
            (
                "interrupted",
                "interrupted",
                "interrupted",
                Some("process_restarted"),
                true,
                i64::MAX,
            ),
            (
                "disconnected",
                "failed",
                "failed",
                Some("client_disconnected"),
                true,
                i64::MAX,
            ),
            (
                "unfinished",
                "failed",
                "failed",
                Some("upstream_error"),
                false,
                i64::MAX,
            ),
            (
                "expired",
                "failed",
                "failed",
                Some("upstream_error"),
                true,
                now - 1,
            ),
            (
                "upstream-error",
                "failed",
                "failed",
                Some("upstream_error"),
                true,
                i64::MAX,
            ),
            (
                "delivery-error",
                "failed",
                "completed",
                Some("delivery_error"),
                true,
                i64::MAX,
            ),
        ] {
            insert_turn(&pool, id, recent, "model", "key").await?;
            sqlx::query("UPDATE inference_run_observations SET status=?,terminal_reason=?,finished_at=?,expires_at=? WHERE id=?")
                .bind(run_status)
                .bind(reason)
                .bind(finished.then_some(recent + 10))
                .bind(expires_at)
                .bind(id)
                .execute(&pool).await?;
            sqlx::query("UPDATE model_turn_observations SET status=? WHERE id=?")
                .bind(turn_status)
                .bind(id)
                .execute(&pool)
                .await?;
            insert_failed_attempt(&pool, &format!("{id}-failed"), id, recent).await?;
        }
        insert_attempt(&pool, "recovered-success", "recovered", recent, 12, Some(5)).await?;
        insert_attempt(
            &pool,
            "delivery-success",
            "delivery-error",
            recent,
            12,
            Some(5),
        )
        .await?;
        // 同一请求的多次尝试各自计一次上游错误；隐藏轮次没有尝试，不重复计。
        insert_failed_attempt(&pool, "upstream-error-retry", "upstream-error", recent).await?;
        for id in ["upstream-error-failed", "upstream-error-retry"] {
            sqlx::query(
                "UPDATE target_attempt_observations SET error_source='upstream' WHERE id=?",
            )
            .bind(id)
            .execute(&pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO model_turn_observations
            (id,run_id,interaction_id,route_id,status,started_at,finished_at,last_event_sequence)
            VALUES ('hidden','upstream-error','upstream-error','route','failed',?,?,0)",
        )
        .bind(recent)
        .bind(recent + 10)
        .execute(&pool)
        .await?;
        insert_turn(&pool, "old-error", now - 7_200_000, "model", "key").await?;
        sqlx::query("UPDATE inference_run_observations SET status='failed',terminal_reason='upstream_error' WHERE id='old-error'")
            .execute(&pool).await?;
        insert_failed_attempt(&pool, "old-failed", "old-error", now - 7_200_000).await?;
        // 准入前拒绝属于错误的请求：只进 request_count（旧口径），四个新计数一律为零。
        for (id, code, status_code) in [
            ("rejected", "invalid_api_key", 401),
            ("rejected-cancelled", "cancelled", 400),
            ("rejected-disconnected", "client_disconnected", 499),
        ] {
            sqlx::query("INSERT INTO rejected_request_observations
                (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,expires_at)
                VALUES (?,?,'POST','/v1/responses','responses','authentication',?,?,0,0,?)")
                .bind(id).bind(now - 1_800_000).bind(code).bind(status_code).bind(i64::MAX)
                .execute(&pool).await?;
        }
        let store = SqliteUsageStatsStore {
            pool: pool.clone(),
            last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
        };
        // 已终结请求只含已完成或最终失败、且未在 Target 选择前终止的运行；
        // 取消、断线、进行中与准入前拒绝都不入账。失败的请求是其中失败的部分。
        let recent_overview = store.stats_overview(window(1)).await?;
        assert_eq!(recent_overview.terminal_request_count, 4);
        assert_eq!(recent_overview.failed_request_count, 3);
        // 上游请求 = 已完成尝试 + 上游错误尝试；来源未知的失败尝试不计入任何一方。
        assert_eq!(recent_overview.upstream_request_count, 4);
        assert_eq!(recent_overview.upstream_error_count, 2);
        assert_eq!(recent_overview.total_requests, 12);
        let all_overview = store.stats_overview(StatsWindow::default()).await?;
        assert_eq!(all_overview.terminal_request_count, 5);
        assert_eq!(all_overview.failed_request_count, 4);
        assert_eq!(all_overview.upstream_request_count, 4);
        assert_eq!(all_overview.upstream_error_count, 2);
        assert_eq!(all_overview.total_requests, 13);
        let series = store.stats_series(window(1), 900_000).await?;
        assert_eq!(
            series
                .iter()
                .map(|bucket| bucket.terminal_request_count)
                .sum::<i64>(),
            4
        );
        assert_eq!(
            series
                .iter()
                .map(|bucket| bucket.failed_request_count)
                .sum::<i64>(),
            3
        );
        assert_eq!(
            series
                .iter()
                .map(|bucket| bucket.upstream_request_count)
                .sum::<i64>(),
            4
        );
        assert_eq!(
            series
                .iter()
                .map(|bucket| bucket.upstream_error_count)
                .sum::<i64>(),
            2
        );
        assert_eq!(
            series
                .iter()
                .map(|bucket| bucket.request_count)
                .sum::<i64>(),
            12
        );
        let rejected_bucket = series
            .iter()
            .find(|bucket| bucket.bucket_start == (now - 1_800_000) / 900_000 * 900_000)
            .unwrap();
        assert_eq!(rejected_bucket.request_count, 3);
        assert_eq!(rejected_bucket.terminal_request_count, 0);
        assert_eq!(rejected_bucket.failed_request_count, 0);
        assert_eq!(rejected_bucket.upstream_request_count, 0);
        assert_eq!(rejected_bucket.upstream_error_count, 0);
        // 供应商口径只含上游请求与上游错误，不再复用请求/错误计数。
        let providers = store.stats_by_provider(window(1)).await?;
        assert_eq!(providers[0].upstream_request_count, 4);
        assert_eq!(providers[0].upstream_error_count, 2);
        let all_providers = store.stats_by_provider(StatsWindow::default()).await?;
        assert_eq!(all_providers[0].upstream_request_count, 4);
        assert_eq!(all_providers[0].upstream_error_count, 2);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn route_scheduling_hour_window_status_and_unknown_usage() -> anyhow::Result<()> {
        use sqlx::Row;
        use std::collections::BTreeMap;

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let now = 1_800_000_000_000_i64;
        let hour_ago = now - 3_600_000;
        insert_turn(&pool, "run", now, "model", "key").await?;
        for (id, provider, model, started_at, status, output, duration) in [
            (
                "before-hour",
                "known",
                "model",
                hour_ago - 1,
                "completed",
                Some(3),
                Some(10),
            ),
            (
                "hour-boundary",
                "known",
                "model",
                hour_ago,
                "completed",
                Some(3),
                Some(10),
            ),
            (
                "recent",
                "known",
                "model",
                now - 1,
                "completed",
                Some(3),
                Some(10),
            ),
            // 调度窗口只有 1h 下界，不因本机与记录时钟偏差丢弃未来时间。
            (
                "future",
                "known",
                "model",
                now + 1,
                "completed",
                Some(3),
                Some(10),
            ),
            (
                "failed-known",
                "known",
                "model",
                now - 1,
                "failed",
                Some(3),
                Some(10),
            ),
            (
                "missing-output",
                "unknown-output",
                "model",
                now,
                "completed",
                None,
                Some(10),
            ),
            (
                "known-output",
                "unknown-output",
                "model",
                now,
                "completed",
                Some(3),
                Some(10),
            ),
            (
                "missing-duration",
                "unknown-duration",
                "model",
                now,
                "completed",
                Some(3),
                None,
            ),
            (
                "known-duration",
                "unknown-duration",
                "model",
                now,
                "completed",
                Some(3),
                Some(10),
            ),
            (
                "failed-unknown",
                "unknown-duration",
                "model",
                now,
                "failed",
                None,
                None,
            ),
            (
                "no-success",
                "no-success",
                "model",
                now,
                "failed",
                None,
                None,
            ),
            (
                "all-unknown",
                "all-unknown",
                "model",
                now,
                "failed",
                None,
                None,
            ),
            (
                "missing-cache",
                "unknown-cache",
                "model",
                now,
                "completed",
                Some(3),
                Some(10),
            ),
            (
                "missing-provider",
                "",
                "model",
                now,
                "completed",
                Some(3),
                Some(10),
            ),
            (
                "missing-model",
                "provider",
                "",
                now,
                "completed",
                Some(3),
                Some(10),
            ),
        ] {
            insert_attempt(&pool, id, "run", started_at, 10, Some(2)).await?;
            sqlx::query(
                "UPDATE target_attempt_observations
                 SET provider_id=?,upstream_model=?,status=?,output_tokens=?,duration_ms=?,cache_write_tokens=1
                 WHERE id=?",
            )
            .bind(provider)
            .bind(model)
            .bind(status)
            .bind(output)
            .bind(duration)
            .bind(id)
            .execute(&pool)
            .await?;
        }
        sqlx::query("UPDATE target_attempt_observations SET cache_read_tokens=NULL WHERE id IN ('missing-output','missing-cache')")
            .execute(&pool).await?;
        sqlx::query("UPDATE target_attempt_observations SET input_tokens=NULL,cache_read_tokens=NULL,cache_write_tokens=NULL WHERE id='all-unknown'")
            .execute(&pool).await?;
        // 同一 Provider + 上游模型的另一条 Route 仍聚合同一 target。
        insert_turn(&pool, "run-2", now, "other-route", "key").await?;
        insert_attempt(&pool, "cross-route", "run-2", now, 10, Some(2)).await?;
        sqlx::query(
            "UPDATE target_attempt_observations
             SET provider_id='known',upstream_model='model',status='completed',
                 output_tokens=3,duration_ms=10,cache_write_tokens=1
             WHERE id='cross-route'",
        )
        .execute(&pool)
        .await?;
        let rows = sqlx::query(ROUTE_SCHEDULING_SQL)
            .bind(hour_ago)
            .fetch_all(&pool)
            .await?;
        let columns = [
            "input_tokens_1h",
            "output_tokens_1h",
            "cache_read_tokens_1h",
            "cache_write_tokens_1h",
            "uncached_input_tokens_1h",
            "attempts_1h",
            "successes_1h",
            "successful_output_tokens_1h",
            "successful_upstream_ms_1h",
        ];
        let actual = rows
            .iter()
            .map(|row| {
                let values = columns
                    .iter()
                    .map(|column| row.try_get::<Option<i64>, _>(*column))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((row.try_get::<String, _>("target_key")?, values))
            })
            .collect::<Result<BTreeMap<_, _>, sqlx::Error>>()?;
        let expected = [
            (
                "known:model",
                vec![
                    Some(50),
                    Some(15),
                    Some(10),
                    Some(5),
                    Some(40),
                    Some(5),
                    Some(4),
                    Some(12),
                    Some(40),
                ],
            ),
            (
                "unknown-output:model",
                vec![
                    Some(20),
                    Some(3),
                    Some(2),
                    Some(2),
                    Some(18),
                    Some(2),
                    Some(2),
                    None,
                    Some(20),
                ],
            ),
            (
                "unknown-duration:model",
                vec![
                    Some(30),
                    Some(6),
                    Some(6),
                    Some(3),
                    Some(24),
                    Some(3),
                    Some(2),
                    Some(6),
                    None,
                ],
            ),
            (
                "no-success:model",
                vec![
                    Some(10),
                    None,
                    Some(2),
                    Some(1),
                    Some(8),
                    Some(1),
                    Some(0),
                    None,
                    None,
                ],
            ),
            (
                "unknown-cache:model",
                vec![
                    Some(10),
                    Some(3),
                    None,
                    Some(1),
                    Some(10),
                    Some(1),
                    Some(1),
                    Some(3),
                    Some(10),
                ],
            ),
            (
                "all-unknown:model",
                vec![None, None, None, None, None, Some(1), Some(0), None, None],
            ),
        ]
        .into_iter()
        .map(|(key, values)| (key.to_owned(), values))
        .collect();
        assert_eq!(actual, expected);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn route_scheduling_keeps_confirmed_usage_after_unknown_attempts() -> anyhow::Result<()> {
        use sqlx::Row;

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let now = 1_800_000_000_000_i64;
        let hour_ago = now - 3_600_000;
        insert_turn(&pool, "run", now, "model", "key").await?;
        insert_attempt(&pool, "confirmed", "run", now - 1, 10, Some(2)).await?;
        sqlx::query(
            "UPDATE target_attempt_observations
             SET status='completed',output_tokens=3,cache_write_tokens=1,duration_ms=10
             WHERE id='confirmed'",
        )
        .execute(&pool)
        .await?;

        let confirmed = sqlx::query(ROUTE_SCHEDULING_SQL)
            .bind(hour_ago)
            .fetch_one(&pool)
            .await?;
        for (column, expected) in [
            ("input_tokens_1h", Some(10_i64)),
            ("output_tokens_1h", Some(3_i64)),
            ("cache_read_tokens_1h", Some(2_i64)),
            ("cache_write_tokens_1h", Some(1_i64)),
            ("uncached_input_tokens_1h", Some(8_i64)),
        ] {
            assert_eq!(
                confirmed.try_get::<Option<i64>, _>(column)?,
                expected,
                "confirmed {column}"
            );
        }

        // 失败和进行中的 attempt 即使完全未报告用量，也只让自身维度未知，
        // 不能让同一 Target 已确认的用量整体丢失。
        insert_failed_attempt(&pool, "failed-unknown", "run", now).await?;
        insert_attempt(&pool, "running-unknown", "run", now, 1, Some(1)).await?;
        sqlx::query(
            "UPDATE target_attempt_observations
             SET status='running',input_tokens=NULL,output_tokens=NULL,
                 cache_read_tokens=NULL,cache_write_tokens=NULL,
                 reasoning_tokens=NULL,duration_ms=NULL,first_token_ms=NULL
             WHERE id='running-unknown'",
        )
        .execute(&pool)
        .await?;

        let after = sqlx::query(ROUTE_SCHEDULING_SQL)
            .bind(hour_ago)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            after.try_get::<String, _>("target_key")?,
            "provider:upstream"
        );
        for (column, expected) in [
            ("input_tokens_1h", Some(10_i64)),
            ("output_tokens_1h", Some(3_i64)),
            ("cache_read_tokens_1h", Some(2_i64)),
            ("cache_write_tokens_1h", Some(1_i64)),
            ("uncached_input_tokens_1h", Some(8_i64)),
        ] {
            assert_eq!(
                after.try_get::<Option<i64>, _>(column)?,
                expected,
                "confirmed {column} survives unknown attempts"
            );
        }
        assert_eq!(after.try_get::<i64, _>("attempts_1h")?, 3);
        assert_eq!(after.try_get::<i64, _>("successes_1h")?, 1);
        assert_eq!(
            after.try_get::<Option<i64>, _>("successful_output_tokens_1h")?,
            Some(3)
        );
        assert_eq!(
            after.try_get::<Option<i64>, _>("successful_upstream_ms_1h")?,
            Some(10)
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn management_stats_project_net_input_and_preserve_full_attempt_tps() -> anyhow::Result<()>
    {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = SqliteUsageStatsStore {
            pool: pool.clone(),
            last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
        };
        assert_eq!(store.stats_overview(window(3)).await?.avg_output_tps, None);
        assert_eq!(
            store.stats_overview(window(3)).await?.avg_first_token_ms,
            None
        );
        assert!(store.stats_series(window(3), 3_600_000).await?.is_empty());
        let now = chrono::Utc::now().timestamp_millis();
        let recent = now / 3_600_000 * 3_600_000 - 5_400_000;
        let old = now - 4 * 60 * 60 * 1_000;
        insert_turn(&pool, "recent", recent, "model", "key").await?;
        // 调度只消费最近一小时；管理统计仍由 Turn 开始时间分桶，与 attempt 自身时间无关。
        insert_attempt(&pool, "recent-a", "recent", now - 1_000, 12, Some(5)).await?;
        insert_attempt(&pool, "recent-b", "recent", now - 1_000, 3, Some(9)).await?;
        sqlx::query("UPDATE target_attempt_observations SET output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE model_turn_id='recent'")
            .execute(&pool).await?;
        let scheduling = store.route_scheduling_snapshot().await;
        assert!(!scheduling.stale);
        assert_eq!(scheduling.targets.len(), 1);
        // 后续发布和客户端交付失败不能改写已成功的上游 attempt。
        sqlx::query("UPDATE model_turn_observations SET status='failed' WHERE id='recent'")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE inference_run_observations SET status='failed',terminal_reason='delivery_error' WHERE id='recent'")
            .execute(&pool).await?;
        // 管理统计中，失败和未完成 attempt 即使有用量也不计入；调度用量仍按观测汇总。
        insert_failed_attempt(&pool, "recent-failed", "recent", recent).await?;
        insert_attempt(&pool, "recent-incomplete", "recent", recent, 999, None).await?;
        sqlx::query("UPDATE target_attempt_observations SET status='running',output_tokens=999,duration_ms=1,first_token_ms=1 WHERE id='recent-incomplete'")
            .execute(&pool).await?;
        sqlx::query("UPDATE target_attempt_observations SET output_tokens=999,first_token_ms=1 WHERE id='recent-failed'")
            .execute(&pool).await?;
        insert_turn(&pool, "old", old, "unknown-cache", "old-key").await?;
        // attempt 自身时间在窗口内，但归属窗口由 Model Turn 开始时间决定。
        insert_attempt(&pool, "old-a", "old", recent, 12528, None).await?;
        sqlx::query("UPDATE target_attempt_observations SET duration_ms=NULL,provider_id='old-provider',provider_name='Old Provider' WHERE id='old-a'")
            .execute(&pool).await?;

        let overview = store.stats_overview(window(3)).await?;
        assert_eq!(overview.total_input_tokens, Some(7));
        assert_eq!(overview.total_output_tokens, Some(50));
        assert_eq!(overview.total_reasoning_tokens, Some(2));
        assert_eq!(overview.avg_output_tps, Some(5.0));
        assert_eq!(overview.avg_first_token_ms, Some(4900.0));
        assert_eq!(overview.avg_duration_ms, Some(10.0));
        // 未知字段只跳过该 attempt，不遮蔽其他已确认用量；全部未报告的字段保持 null。
        let all_time = store.stats_overview(StatsWindow::default()).await?;
        assert_eq!(all_time.total_input_tokens, Some(7));
        assert_eq!(all_time.total_output_tokens, Some(53));
        assert_eq!(all_time.total_cache_write_tokens, None);
        assert_eq!(all_time.avg_output_tps, None);

        let series = store.stats_series(window(3), 3_600_000).await?;
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].total_input_tokens, Some(7));
        assert_eq!(series[0].total_output_tokens, Some(50));
        assert_eq!(series[0].total_reasoning_tokens, Some(2));
        assert_eq!(series[0].avg_output_tps, Some(5.0));
        assert_eq!(series[0].avg_first_token_ms, Some(4900.0));
        assert_eq!(series[0].avg_duration_ms, Some(10.0));
        assert!(series[0].bucket_start >= window(3).start_at.unwrap());

        let quarter = store.stats_series(window(3), 900_000).await?;
        assert_eq!(quarter.len(), 1);
        assert_eq!(quarter[0].bucket_start % 900_000, 0);

        let day = store.stats_series(window(3), 86_400_000).await?;
        assert_eq!(day.len(), 1);
        // Display buckets begin at the absolute selected start, independent of browser timezone.
        assert_eq!(day[0].bucket_start, window(3).start_at.unwrap());

        let models = store.stats_by_model(window(3)).await?;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].total_input_tokens, Some(7));
        assert_eq!(models[0].total_output_tokens, Some(50));
        assert_eq!(models[0].total_reasoning_tokens, Some(2));

        let api_keys = store.stats_by_api_key(window(3)).await?;
        assert_eq!(api_keys.len(), 1);
        assert_eq!(api_keys[0].total_input_tokens, Some(7));
        assert_eq!(api_keys[0].total_output_tokens, Some(50));
        assert_eq!(api_keys[0].reasoning_tokens, Some(2));

        let providers = store.stats_by_provider(window(3)).await?;
        // 两个已完成尝试各计一次上游请求；失败与进行中的尝试既不进分子也不进分母。
        // 旧 Turn 的近期尝试不会把供应商贡献移入近期窗口。
        assert_eq!(
            providers
                .iter()
                .map(|provider| (
                    provider.provider_id.as_deref(),
                    provider.provider.as_str(),
                    provider.upstream_request_count,
                    provider.upstream_error_count,
                    provider.avg_output_tps,
                ))
                .collect::<Vec<_>>(),
            vec![(Some("provider"), "Provider", 2, 0, Some(5.0))]
        );
        let old_bucket = old / BASE_BUCKET_MS * BASE_BUCKET_MS;
        let old_providers = store
            .stats_by_provider(StatsWindow {
                start_at: Some(old_bucket),
                end_at: Some(old_bucket + BASE_BUCKET_MS),
            })
            .await?;
        assert_eq!(
            old_providers
                .iter()
                .map(|provider| (
                    provider.provider_id.as_deref(),
                    provider.provider.as_str(),
                    provider.upstream_request_count,
                    provider.upstream_error_count,
                    provider.avg_output_tps,
                ))
                .collect::<Vec<_>>(),
            vec![(Some("old-provider"), "Old Provider", 1, 0, None)]
        );
        let old_model = store
            .stats_by_model(StatsWindow::default())
            .await?
            .into_iter()
            .find(|model| model.model == "unknown-cache")
            .unwrap();
        assert_eq!(old_model.total_input_tokens, None);

        // 同组只缺一条成功样本也使 TPS 未知，但已知 Token 合计仍保留。
        for (change, expected_tps, expected_output, expected_first, expected_input) in [
            (
                "output_tokens=CASE id WHEN 'recent-a' THEN 20 END,input_tokens=CASE id WHEN 'recent-a' THEN 12 END",
                None,
                Some(20),
                Some(4900.0),
                Some(7),
            ),
            (
                "duration_ms=CASE id WHEN 'recent-a' THEN 1000 END",
                None,
                Some(50),
                Some(4900.0),
                Some(7),
            ),
            ("duration_ms=0", None, Some(50), Some(4900.0), Some(7)),
            ("output_tokens=0", Some(0.0), Some(0), Some(4900.0), Some(7)),
            ("first_token_ms=NULL", Some(5.0), Some(50), None, Some(7)),
            (
                "first_token_ms=CASE id WHEN 'recent-a' THEN 900 END",
                Some(5.0),
                Some(50),
                Some(900.0),
                Some(7),
            ),
            (
                "first_token_ms=CASE id WHEN 'recent-a' THEN 0 END",
                Some(5.0),
                Some(50),
                Some(0.0),
                Some(7),
            ),
            ("first_token_ms=0", Some(5.0), Some(50), Some(0.0), Some(7)),
            ("status='failed'", None, None, None, None),
        ] {
            sqlx::query("UPDATE target_attempt_observations SET status='completed',input_tokens=CASE id WHEN 'recent-a' THEN 12 ELSE 3 END,cache_read_tokens=CASE id WHEN 'recent-a' THEN 5 ELSE 9 END,cache_write_tokens=NULL,output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE id IN ('recent-a','recent-b')")
                .execute(&pool).await?;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE target_attempt_observations SET {change} WHERE id IN ('recent-a','recent-b')"
            ))).execute(&pool).await?;
            assert_eq!(
                store
                    .stats_by_provider(window(3))
                    .await?
                    .into_iter()
                    .find(|provider| provider.provider == "Provider")
                    .unwrap()
                    .avg_output_tps,
                expected_tps,
                "{change}"
            );
            let overview = store.stats_overview(window(3)).await?;
            assert_eq!(overview.avg_output_tps, expected_tps, "{change}");
            assert_eq!(overview.total_output_tokens, expected_output, "{change}");
            assert_eq!(overview.avg_first_token_ms, expected_first, "{change}");
            assert_eq!(overview.total_input_tokens, expected_input, "{change}");
            let series = store.stats_series(window(3), 3_600_000).await?;
            assert_eq!(series.len(), 1);
            assert_eq!(series[0].avg_output_tps, expected_tps, "{change}");
            assert_eq!(series[0].total_output_tokens, expected_output, "{change}");
            assert_eq!(series[0].avg_first_token_ms, expected_first, "{change}");
            assert_eq!(series[0].total_input_tokens, expected_input, "{change}");
        }

        sqlx::query("UPDATE target_attempt_observations SET status='completed',input_tokens=CASE id WHEN 'recent-a' THEN 12 ELSE 3 END,cache_read_tokens=CASE id WHEN 'recent-a' THEN 5 ELSE 9 END,cache_write_tokens=NULL,output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE id IN ('recent-a','recent-b')")
            .execute(&pool).await?;
        let raw_input: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(input_tokens) FROM target_attempt_observations WHERE id IN ('recent-a','recent-b')",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(raw_input, Some(15));
        // 缓存写入不扣；部分未知照常累加已知 attempt，全未知与已知零保持不同。
        for (assignment, expected) in [
            ("cache_write_tokens=100", Some(7)),
            (
                "cache_read_tokens=CASE id WHEN 'recent-a' THEN 5 END",
                Some(7),
            ),
            ("cache_read_tokens=NULL", None),
            ("cache_read_tokens=100", Some(0)),
            ("input_tokens=NULL,cache_read_tokens=0", None),
            ("input_tokens=0,cache_read_tokens=0", Some(0)),
        ] {
            sqlx::query("UPDATE target_attempt_observations SET status='completed',input_tokens=CASE id WHEN 'recent-a' THEN 12 ELSE 3 END,cache_read_tokens=CASE id WHEN 'recent-a' THEN 5 ELSE 9 END,cache_write_tokens=NULL,output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE id IN ('recent-a','recent-b')")
                .execute(&pool).await?;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE target_attempt_observations SET {assignment} WHERE id IN ('recent-a','recent-b')"
            )))
            .execute(&pool)
            .await?;
            assert_eq!(
                store.stats_overview(window(3)).await?.total_input_tokens,
                expected
            );
            assert_eq!(
                store.stats_series(window(3), 3_600_000).await?[0].total_input_tokens,
                expected
            );
            assert_eq!(
                store.stats_by_model(window(3)).await?[0].total_input_tokens,
                expected
            );
            assert_eq!(
                store.stats_by_api_key(window(3)).await?[0].total_input_tokens,
                expected
            );
        }
        // 分属两桶仍按原始窗口合计计算，不平均两个桶的 TPS。
        let next = recent + 3_600_000;
        insert_turn(&pool, "next", next, "model", "key").await?;
        sqlx::query("UPDATE target_attempt_observations SET status='completed',input_tokens=CASE id WHEN 'recent-a' THEN 12 ELSE 3 END,cache_read_tokens=CASE id WHEN 'recent-a' THEN 5 ELSE 9 END,cache_write_tokens=NULL,output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE id IN ('recent-a','recent-b')")
            .execute(&pool).await?;
        // attempt 时间故意仍在第一桶，验证按所属 Model Turn 分桶。
        sqlx::query("UPDATE target_attempt_observations SET model_turn_id='next',run_id='next',interaction_id='next' WHERE id='recent-b'")
            .execute(&pool).await?;
        assert_eq!(
            store.stats_overview(window(3)).await?.avg_output_tps,
            Some(5.0)
        );
        let series = store.stats_series(window(3), 3_600_000).await?;
        assert_eq!(series.len(), 2);
        assert!(series[0].bucket_start >= window(3).start_at.unwrap());
        assert_eq!(series[1].bucket_start - series[0].bucket_start, 3_600_000);
        assert_eq!(series[0].avg_output_tps, Some(20.0));
        assert!((series[1].avg_output_tps.unwrap() - 10.0 / 3.0).abs() < 1e-12);
        assert_eq!(series[0].avg_first_token_ms, Some(900.0));
        assert_eq!(series[1].avg_first_token_ms, Some(8900.0));
        pool.close().await;
        Ok(())
    }

    async fn verify_observation_stat_revisions(pool: &SqlitePool) -> anyhow::Result<()> {
        let selected = StatsWindow {
            start_at: Some(900_000),
            end_at: Some(2_700_000),
        };
        let store = SqliteUsageStatsStore {
            pool: pool.clone(),
            last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
        };
        insert_turn(
            pool,
            "revision-a",
            900_000,
            "revision-model-a",
            "revision-key",
        )
        .await?;
        // 无子 attempt 时修订 Turn 身份、标签及时间桶，不能被 guard 漏掉。
        sqlx::query("UPDATE model_turn_observations SET route_id='revision-model-b',model_display_name='Revised model',api_key_id=NULL,api_key_name='',started_at=1800000,finished_at=NULL WHERE id='revision-a'")
            .execute(pool).await?;
        let models = store.stats_by_model(selected).await?;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id.as_deref(), Some("revision-model-b"));
        assert_eq!(models[0].model, "Revised model");
        assert_eq!(models[0].avg_duration_ms, None);
        let keys = store.stats_by_api_key(selected).await?;
        assert_eq!(keys[0].api_key_name, "Unknown API Key");
        assert!(
            store
                .stats_by_model(StatsWindow {
                    start_at: Some(900_000),
                    end_at: Some(1_800_000),
                })
                .await?
                .is_empty()
        );
        sqlx::query("UPDATE model_turn_observations SET finished_at=1800020 WHERE id='revision-a'")
            .execute(pool)
            .await?;
        assert_eq!(
            store.stats_overview(selected).await?.avg_duration_ms,
            Some(20.0)
        );
        insert_attempt(pool, "revision-attempt", "revision-a", 1_800_000, 12, None).await?;
        assert_eq!(
            store.stats_overview(selected).await?.total_input_tokens,
            None
        );
        // NULL -> 已知零以及已知 -> NULL 的覆盖，不把未知当成零。
        sqlx::query("UPDATE target_attempt_observations SET cache_read_tokens=0,output_tokens=0,cache_write_tokens=0,first_token_ms=0 WHERE id='revision-attempt'")
            .execute(pool).await?;
        let overview = store.stats_overview(selected).await?;
        assert_eq!(overview.total_input_tokens, Some(12));
        assert_eq!(overview.total_output_tokens, Some(0));
        assert_eq!(overview.total_cache_write_tokens, Some(0));
        assert_eq!(overview.avg_first_token_ms, Some(0.0));
        sqlx::query(
            "UPDATE target_attempt_observations SET output_tokens=NULL WHERE id='revision-attempt'",
        )
        .execute(pool)
        .await?;
        assert_eq!(
            store.stats_overview(selected).await?.total_output_tokens,
            None
        );
        // Provider 标签也是 run source 的间接依赖，不能只检查用量列。
        sqlx::query("UPDATE target_attempt_observations SET provider_id='revision-provider',provider_name='Revised provider' WHERE id='revision-attempt'")
            .execute(pool).await?;
        let providers = store.stats_by_provider(selected).await?;
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].provider, "Revised provider");
        assert_eq!(providers[0].upstream_request_count, 1);
        assert_eq!(providers[0].upstream_error_count, 0);
        sqlx::query("UPDATE inference_run_observations SET status='failed',terminal_reason='cancelled' WHERE id='revision-a'")
            .execute(pool).await?;
        // 取消既不是已终结请求也不是失败的请求。
        let cancelled = store.stats_overview(selected).await?;
        assert_eq!(cancelled.terminal_request_count, 0);
        assert_eq!(cancelled.failed_request_count, 0);
        sqlx::query(
            "UPDATE inference_run_observations SET failure_json='{}' WHERE id='revision-a'",
        )
        .execute(pool)
        .await?;
        let failed = store.stats_overview(selected).await?;
        assert_eq!(failed.terminal_request_count, 1);
        assert_eq!(failed.failed_request_count, 1);
        // 拒绝记录属于错误的请求：只进 request_count，不改变四个新计数。
        sqlx::query("INSERT INTO rejected_request_observations (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,expires_at) VALUES ('revision-rejection',900000,'POST','/v1/responses','responses','authentication','cancelled',400,false,0,9223372036854775807)")
            .execute(pool).await?;
        assert_eq!(
            store.stats_overview(selected).await?.failed_request_count,
            1
        );
        sqlx::query("UPDATE rejected_request_observations SET code='invalid_api_key',started_at=1800000 WHERE id='revision-rejection'")
            .execute(pool).await?;
        assert_eq!(
            store.stats_overview(selected).await?.failed_request_count,
            1
        );
        sqlx::query("UPDATE rejected_request_observations SET status_code=499 WHERE id='revision-rejection'")
            .execute(pool).await?;
        assert_eq!(
            store.stats_overview(selected).await?.failed_request_count,
            1
        );
        // 无关更新和相关列原值赋值都必须保持公开统计结果不变。
        let before = serde_json::to_value(store.stats_overview(selected).await?)?;
        for statement in [
            "UPDATE model_turn_observations SET last_event_sequence=last_event_sequence+1,api_key_id=api_key_id,finished_at=finished_at WHERE id='revision-a'",
            "UPDATE target_attempt_observations SET last_event_sequence=last_event_sequence+1,output_tokens=output_tokens,provider_name=provider_name WHERE id='revision-attempt'",
            "UPDATE inference_run_observations SET last_event_sequence=last_event_sequence+1,failure_json=failure_json WHERE id='revision-a'",
            "UPDATE rejected_request_observations SET last_event_sequence=last_event_sequence+1,occurred_at=occurred_at WHERE id='revision-rejection'",
        ] {
            sqlx::query(statement).execute(pool).await?;
        }
        assert_eq!(
            serde_json::to_value(store.stats_overview(selected).await?)?,
            before
        );
        // attempt 的 Turn 归属决定模型、API Key 及分桶，与自身 started_at 无关。
        insert_turn(
            pool,
            "revision-b",
            900_000,
            "revision-model-c",
            "revision-key-c",
        )
        .await?;
        sqlx::query("UPDATE target_attempt_observations SET model_turn_id='revision-b',output_tokens=9 WHERE id='revision-attempt'")
            .execute(pool).await?;
        let models = store.stats_by_model(selected).await?;
        let moved = models
            .iter()
            .find(|model| model.model_id.as_deref() == Some("revision-model-c"))
            .unwrap();
        assert_eq!(moved.total_input_tokens, Some(12));
        assert_eq!(moved.total_output_tokens, Some(9));
        let previous = models
            .iter()
            .find(|model| model.model_id.as_deref() == Some("revision-model-b"))
            .unwrap();
        assert_eq!(previous.total_input_tokens, None);
        assert_eq!(
            store.stats_series(selected, BASE_BUCKET_MS).await?[0].total_output_tokens,
            Some(9)
        );
        Ok(())
    }

    async fn verify_fixed_range_survives_detail_cleanup(pool: &SqlitePool) -> anyhow::Result<()> {
        verify_observation_stat_revisions(pool).await?;
        let start = 9_900_000;
        let end = start + 513 * BASE_BUCKET_MS;
        let selected = StatsWindow {
            start_at: Some(start),
            end_at: Some(end),
        };
        for (id, at, route) in [
            ("durable-first", start, "historical-a"),
            ("durable-last", end - BASE_BUCKET_MS, "historical-b"),
            ("durable-excluded", end, "historical-a"),
        ] {
            insert_turn(pool, id, at, "Same name", id).await?;
            sqlx::query("UPDATE model_turn_observations SET route_id=? WHERE id=?")
                .bind(route)
                .bind(id)
                .execute(pool)
                .await?;
            insert_attempt(pool, id, id, at, 12, Some(5)).await?;
        }
        sqlx::query("UPDATE target_attempt_observations SET output_tokens=CASE id WHEN 'durable-first' THEN 20 ELSE 30 END, duration_ms=CASE id WHEN 'durable-first' THEN 1000 ELSE 9000 END, first_token_ms=CASE id WHEN 'durable-first' THEN 0 ELSE 100 END WHERE id IN ('durable-first','durable-last')")
            .execute(pool).await?;
        let store = SqliteUsageStatsStore {
            pool: pool.clone(),
            last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
        };
        let overview = store.stats_overview(selected).await?;
        assert_eq!(overview.total_requests, 2);
        assert_eq!(overview.total_input_tokens, Some(14));
        assert_eq!(overview.total_output_tokens, Some(50));
        assert_eq!(overview.avg_output_tps, Some(5.0));
        assert_eq!(overview.avg_first_token_ms, Some(50.0));
        assert_eq!(overview.avg_duration_ms, Some(10.0));
        assert_eq!(overview.total_cache_write_tokens, None);
        let series = store.stats_series(selected, BASE_BUCKET_MS).await?;
        assert_eq!(series.len(), 2);
        assert!(series.iter().all(|point| point.bucket_ms > BASE_BUCKET_MS));
        assert_eq!(
            series.iter().map(|point| point.request_count).sum::<i64>(),
            2
        );
        assert_eq!(
            series
                .iter()
                .filter_map(|point| point.total_output_tokens)
                .sum::<i64>(),
            50
        );
        assert_eq!(series[0].avg_output_tps, Some(20.0));
        assert!((series[1].avg_output_tps.unwrap() - 10.0 / 3.0).abs() < 1e-12);
        // Quarter-hour start intentionally differs from epoch hour alignment.
        // The 513-quarter range ends with a single-quarter partial hour bucket.
        let hourly = store.stats_series(selected, 3_600_000).await?;
        assert_eq!(hourly.len(), 2);
        assert_eq!(hourly[0].bucket_start, 9_900_000);
        assert_eq!(hourly[1].bucket_start, 470_700_000);
        assert_eq!(hourly[0].bucket_ms, 3_600_000);
        assert_eq!(hourly[1].bucket_ms, 3_600_000);
        assert_eq!(hourly[0].request_count, 1);
        assert_eq!(hourly[1].request_count, 1);
        assert_eq!(hourly[0].total_output_tokens, Some(20));
        assert_eq!(hourly[1].total_output_tokens, Some(30));
        assert_eq!(
            hourly
                .iter()
                .filter_map(|point| point.total_output_tokens)
                .sum::<i64>(),
            50
        );
        let models = store.stats_by_model(selected).await?;
        assert_eq!(models.len(), 2);
        assert!(
            models
                .iter()
                .all(|model| model.model == "Same name" && model.deleted)
        );
        assert_ne!(models[0].model_id, models[1].model_id);
        assert!(store.stats_series(selected, 60_000).await.is_err());
        assert!(store.stats_series(selected, 1_000_000).await.is_err());
        sqlx::query("DELETE FROM interaction_observations WHERE id IN ('durable-first','durable-last','durable-excluded')")
            .execute(pool).await?;
        let retained = store.stats_overview(selected).await?;
        assert_eq!(retained.total_requests, 2);
        assert_eq!(retained.total_input_tokens, Some(14));
        assert_eq!(retained.total_output_tokens, Some(50));
        assert_eq!(retained.avg_output_tps, Some(5.0));
        assert_eq!(store.stats_by_model(selected).await?.len(), 2);
        assert_eq!(store.stats_by_api_key(selected).await?.len(), 2);
        // Legacy/internal turns can legitimately have no API key attribution.
        insert_turn(
            pool,
            "durable-unknown-key",
            start + BASE_BUCKET_MS,
            "Same name",
            "temporary-key",
        )
        .await?;
        sqlx::query("UPDATE model_turn_observations SET api_key_id=NULL,api_key_name='' WHERE id='durable-unknown-key'")
            .execute(pool).await?;
        insert_attempt(
            pool,
            "durable-unknown-key",
            "durable-unknown-key",
            start + BASE_BUCKET_MS,
            12,
            Some(5),
        )
        .await?;
        sqlx::query(
            "UPDATE target_attempt_observations SET output_tokens=0 WHERE id='durable-unknown-key'",
        )
        .execute(pool)
        .await?;
        let keys = store.stats_by_api_key(selected).await?;
        assert_eq!(keys.len(), 3);
        let unknown = keys.iter().find(|key| key.api_key_id.is_empty()).unwrap();
        assert!(unknown.deleted);
        assert_eq!(unknown.api_key_name, "Unknown API Key");
        assert_eq!(unknown.request_count, 1);
        assert_eq!(unknown.total_input_tokens, Some(7));
        assert_eq!(unknown.total_output_tokens, Some(0));
        assert_eq!(unknown.cache_write_tokens, None);
        sqlx::query("DELETE FROM interaction_observations WHERE id='durable-unknown-key'")
            .execute(pool)
            .await?;
        let retained_keys = store.stats_by_api_key(selected).await?;
        assert_eq!(retained_keys.len(), 3);
        let retained_unknown = retained_keys
            .iter()
            .find(|key| key.api_key_id.is_empty())
            .unwrap();
        assert_eq!(retained_unknown.request_count, 1);
        assert_eq!(retained_unknown.total_input_tokens, Some(7));
        assert_eq!(retained_unknown.total_output_tokens, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn fixed_long_range_keeps_weighted_metrics_and_deleted_identities() -> anyhow::Result<()>
    {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        verify_fixed_range_survives_detail_cleanup(&pool).await?;
        pool.close().await;
        Ok(())
    }
}
