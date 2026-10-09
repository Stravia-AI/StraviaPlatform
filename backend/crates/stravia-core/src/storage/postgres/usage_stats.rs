use std::sync::LazyLock;

use crate::{
    interaction_observation::{FAILED_SELECT, REQUEST_SELECT},
    storage::RouteSchedulingUsage,
};

use super::*;

#[derive(Clone)]
pub(super) struct PostgresUsageStatsStore {
    pub(super) pool: Pool<Postgres>,
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
        static SQL: LazyLock<String> = LazyLock::new(|| {
            format!(
            "WITH failures AS ({FAILED_SELECT}), requests AS ({REQUEST_SELECT}), turns AS (
                 SELECT * FROM model_turn_observations WHERE ($1::BIGINT IS NULL OR started_at >= $1)
             ), attempts AS (
                 SELECT a.* FROM target_attempt_observations a JOIN turns t ON t.id = a.model_turn_id
                 WHERE a.status = 'completed'
             )
             SELECT
                 (SELECT COUNT(*)::BIGINT FROM requests WHERE expires_at > $2
                    AND ($1::BIGINT IS NULL OR started_at >= $1)) AS total_requests,
                 (SELECT SUM(input_tokens)::BIGINT FROM attempts) AS total_input_tokens,
                 (SELECT SUM(output_tokens)::BIGINT FROM attempts) AS total_output_tokens,
                 (SELECT SUM(cache_read_tokens)::BIGINT FROM attempts) AS total_cache_read_tokens,
                 (SELECT SUM(cache_write_tokens)::BIGINT FROM attempts) AS total_cache_write_tokens,
                 (SELECT SUM(reasoning_tokens)::BIGINT FROM attempts) AS total_reasoning_tokens,
                 (SELECT AVG((finished_at - started_at)::FLOAT8) FROM turns WHERE finished_at IS NOT NULL) AS avg_duration_ms,
                 (SELECT AVG(first_token_ms::FLOAT8) FROM attempts) AS avg_first_token_ms,
                 (SELECT CASE WHEN COUNT(*) > 0
                                   AND COUNT(output_tokens) = COUNT(*)
                                   AND COUNT(duration_ms) = COUNT(*)
                                   AND SUM(duration_ms) > 0
                              THEN SUM(output_tokens)::FLOAT8 * 1000.0 / SUM(duration_ms)::FLOAT8
                         END FROM attempts) AS avg_output_tps,
                 (SELECT COUNT(*)::BIGINT FROM failures WHERE expires_at > $2
                    AND ($1::BIGINT IS NULL OR started_at >= $1)) AS error_count"
        )
        });
        Ok(sqlx::query_as::<_, StatsOverview>(&**SQL)
            .bind(cutoff)
            .bind(chrono::Utc::now().timestamp_millis())
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
        static SQL: LazyLock<String> = LazyLock::new(|| {
            format!(
            "WITH failures AS ({FAILED_SELECT}), requests AS ({REQUEST_SELECT}), request_stats AS (
                 SELECT (started_at + $2) / $3 * $3 - $2 AS bucket_start,
                        COUNT(*)::BIGINT AS request_count
                 FROM requests WHERE started_at >= $1 AND expires_at > $4
                 GROUP BY bucket_start
             ), error_stats AS (
                 SELECT (started_at + $2) / $3 * $3 - $2 AS bucket_start,
                        COUNT(*)::BIGINT AS error_count
                 FROM failures WHERE started_at >= $1 AND expires_at > $4
                 GROUP BY bucket_start
             ), turns AS (
                 SELECT *, (started_at + $2) / $3 * $3 - $2 AS bucket_start
                 FROM model_turn_observations WHERE started_at >= $1
             ), turn_stats AS (
                 SELECT bucket_start,
                        AVG((finished_at - started_at)::FLOAT8) FILTER (WHERE finished_at IS NOT NULL) AS avg_duration_ms
                 FROM turns GROUP BY bucket_start
             ), attempt_stats AS (
                 SELECT t.bucket_start,
                        SUM(a.input_tokens)::BIGINT AS total_input_tokens,
                        SUM(a.output_tokens)::BIGINT AS total_output_tokens,
                        SUM(a.cache_read_tokens)::BIGINT AS total_cache_read_tokens,
                        SUM(a.cache_write_tokens)::BIGINT AS total_cache_write_tokens,
                        SUM(a.reasoning_tokens)::BIGINT AS total_reasoning_tokens,
                        AVG(a.first_token_ms::FLOAT8) AS avg_first_token_ms,
                        CASE WHEN COUNT(*) > 0
                                  AND COUNT(a.output_tokens) = COUNT(*)
                                  AND COUNT(a.duration_ms) = COUNT(*)
                                  AND SUM(a.duration_ms) > 0
                             THEN SUM(a.output_tokens)::FLOAT8 * 1000.0 / SUM(a.duration_ms)::FLOAT8
                        END AS avg_output_tps
                 FROM turns t JOIN target_attempt_observations a ON a.model_turn_id = t.id AND a.status = 'completed' GROUP BY t.bucket_start
             ), buckets AS (
                 SELECT bucket_start FROM turn_stats UNION SELECT bucket_start FROM request_stats
             )
             SELECT b.bucket_start, COALESCE(r.request_count, 0)::BIGINT AS request_count,
                    COALESCE(e.error_count, 0)::BIGINT AS error_count,
                    a.total_input_tokens, a.total_output_tokens, a.total_cache_read_tokens,
                    a.total_cache_write_tokens, a.total_reasoning_tokens,
                    t.avg_duration_ms, a.avg_first_token_ms, a.avg_output_tps
             FROM buckets b
             LEFT JOIN request_stats r ON r.bucket_start = b.bucket_start
             LEFT JOIN turn_stats t ON t.bucket_start = b.bucket_start
             LEFT JOIN attempt_stats a ON a.bucket_start = b.bucket_start
             LEFT JOIN error_stats e ON e.bucket_start = b.bucket_start
             ORDER BY b.bucket_start ASC"
        )
        });
        Ok(sqlx::query_as::<_, StatsSeries>(&**SQL)
            .bind(cutoff)
            .bind(tz_offset_ms)
            .bind(bucket_ms)
            .bind(chrono::Utc::now().timestamp_millis())
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
                        SUM(a.input_tokens)::BIGINT AS total_input_tokens,
                        SUM(a.output_tokens)::BIGINT AS total_output_tokens,
                        SUM(a.reasoning_tokens)::BIGINT AS total_reasoning_tokens
                 FROM turns t JOIN target_attempt_observations a ON a.model_turn_id = t.id AND a.status = 'completed' GROUP BY t.model
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
        static SQL: LazyLock<String> = LazyLock::new(|| {
            format!(
            "WITH failures AS ({FAILED_SELECT}), requests AS ({REQUEST_SELECT}), request_stats AS (
                 SELECT COALESCE(NULLIF(a.provider_name, ''), a.provider_id) AS provider,
                        COUNT(DISTINCT r.id)::BIGINT AS request_count,
                        COUNT(DISTINCT f.id)::BIGINT AS error_count
                 FROM requests r JOIN target_attempt_observations a ON a.run_id = r.run_id
                 LEFT JOIN failures f ON f.kind = r.kind AND f.id = r.id
                 WHERE r.expires_at > $2 AND ($1::BIGINT IS NULL OR r.started_at >= $1)
                 GROUP BY COALESCE(NULLIF(a.provider_name, ''), a.provider_id)
             ), attempt_stats AS (
             SELECT COALESCE(NULLIF(provider_name, ''), provider_id) AS provider,
                    AVG(duration_ms::FLOAT8) AS avg_duration_ms,
                    CASE WHEN SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END) > 0
                              AND SUM(CASE WHEN status = 'completed' AND (output_tokens IS NULL OR duration_ms IS NULL)
                                           THEN 1 ELSE 0 END) = 0
                              AND SUM(CASE WHEN status = 'completed' THEN duration_ms END) > 0
                         THEN SUM(CASE WHEN status = 'completed' THEN output_tokens END)::FLOAT8 * 1000.0
                              / SUM(CASE WHEN status = 'completed' THEN duration_ms END)::FLOAT8
                    END AS avg_output_tps
             FROM target_attempt_observations
             WHERE ($1::BIGINT IS NULL OR started_at >= $1)
             GROUP BY COALESCE(NULLIF(provider_name, ''), provider_id)
             )
             SELECT a.provider, COALESCE(r.request_count, 0)::BIGINT AS request_count,
                    COALESCE(r.error_count, 0)::BIGINT AS error_count,
                    a.avg_duration_ms, a.avg_output_tps
             FROM attempt_stats a LEFT JOIN request_stats r ON r.provider = a.provider
             ORDER BY request_count DESC"
        )
        });
        Ok(sqlx::query_as::<_, ProviderStats>(&**SQL)
            .bind(cutoff)
            .bind(chrono::Utc::now().timestamp_millis())
            .fetch_all(&self.pool)
            .await?)
    }

    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>> {
        let cutoff = cutoff_ms(hours);
        Ok(sqlx::query_as::<_, ApiKeyStats>(
            "SELECT t.api_key_id,
                    COALESCE(MAX(NULLIF(t.api_key_name, '')), t.api_key_id) AS api_key_name,
                    COUNT(DISTINCT t.id)::BIGINT AS request_count,
                    SUM(a.input_tokens)::BIGINT AS total_input_tokens,
                    SUM(a.output_tokens)::BIGINT AS total_output_tokens,
                    SUM(a.cache_read_tokens)::BIGINT AS cache_read_tokens,
                    SUM(a.cache_write_tokens)::BIGINT AS cache_write_tokens,
                    SUM(a.reasoning_tokens)::BIGINT AS reasoning_tokens,
                    MAX(t.started_at)::BIGINT AS last_used_at
             FROM model_turn_observations t
             LEFT JOIN target_attempt_observations a ON a.model_turn_id = t.id AND a.status = 'completed'
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

    async fn insert_failed_attempt(
        pool: &Pool<Postgres>,
        id: &str,
        turn_id: &str,
        started_at: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO target_attempt_observations
             (id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,status,started_at,finished_at,duration_ms,last_event_sequence)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
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

    async fn verify_final_failure_counts(pool: &Pool<Postgres>) -> anyhow::Result<()> {
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
            insert_turn(pool, id, recent, "model", "key").await?;
            sqlx::query("UPDATE inference_run_observations SET status=$1,terminal_reason=$2,finished_at=$3,expires_at=$4 WHERE id=$5")
                .bind(run_status)
                .bind(reason)
                .bind(finished.then_some(recent + 10))
                .bind(expires_at)
                .bind(id)
                .execute(pool).await?;
            sqlx::query("UPDATE model_turn_observations SET status=$1 WHERE id=$2")
                .bind(turn_status)
                .bind(id)
                .execute(pool)
                .await?;
            insert_failed_attempt(pool, &format!("{id}-failed"), id, recent).await?;
        }
        insert_attempt(pool, "recovered-success", "recovered", recent, 12, Some(5)).await?;
        insert_attempt(
            pool,
            "delivery-success",
            "delivery-error",
            recent,
            12,
            Some(5),
        )
        .await?;
        // 同一请求的多次尝试和隐藏轮次不能重复计错。
        insert_failed_attempt(pool, "upstream-error-retry", "upstream-error", recent).await?;
        sqlx::query(
            "INSERT INTO model_turn_observations
            (id,run_id,interaction_id,route_id,status,started_at,finished_at,last_event_sequence)
            VALUES ('hidden','upstream-error','upstream-error','route','failed',$1,$2,0)",
        )
        .bind(recent)
        .bind(recent + 10)
        .execute(pool)
        .await?;
        insert_turn(pool, "old-error", now - 7_200_000, "model", "key").await?;
        sqlx::query("UPDATE inference_run_observations SET status='failed',terminal_reason='upstream_error' WHERE id='old-error'")
            .execute(pool).await?;
        insert_failed_attempt(pool, "old-failed", "old-error", now - 7_200_000).await?;
        // 准入前拒绝也属于失败请求；单纯取消和 499 不属于。
        for (id, code, status_code) in [
            ("rejected", "invalid_api_key", 401),
            ("rejected-cancelled", "cancelled", 400),
            ("rejected-disconnected", "client_disconnected", 499),
        ] {
            sqlx::query("INSERT INTO rejected_request_observations
                (id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,expires_at)
                VALUES ($1,$2,'POST','/v1/responses','responses','authentication',$3,$4,FALSE,0,$5)")
                .bind(id).bind(now - 1_800_000).bind(code).bind(status_code).bind(i64::MAX)
                .execute(pool).await?;
        }
        let store = PostgresUsageStatsStore {
            pool: pool.clone(),
            last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
        };
        assert_eq!(store.stats_overview(Some(1)).await?.error_count, 3);
        assert_eq!(store.stats_overview(Some(1)).await?.total_requests, 11);
        assert_eq!(store.stats_overview(None).await?.error_count, 4);
        assert_eq!(store.stats_overview(None).await?.total_requests, 12);
        let series = store.stats_series(1, 60_000, 0).await?;
        assert_eq!(
            series.iter().map(|bucket| bucket.error_count).sum::<i64>(),
            3
        );
        assert_eq!(
            series
                .iter()
                .map(|bucket| bucket.request_count)
                .sum::<i64>(),
            11
        );
        assert_eq!(
            series
                .iter()
                .find(|bucket| bucket.bucket_start == (now - 1_800_000) / 60_000 * 60_000)
                .unwrap()
                .error_count,
            1
        );
        let providers = store.stats_by_provider(Some(1)).await?;
        assert_eq!(providers[0].error_count, 2);
        assert_eq!(providers[0].request_count, 8);
        assert_eq!(store.stats_by_provider(None).await?[0].error_count, 3);
        Ok(())
    }

    #[tokio::test]
    async fn postgres_management_stats_preserve_total_input_and_full_attempt_tps()
    -> anyhow::Result<()> {
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
                crate::migrations::migrate_postgres(&pool, None).await?;
                let store = PostgresUsageStatsStore {
                    pool: pool.clone(),
                    last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
                };
                assert_eq!(store.stats_overview(Some(3)).await?.avg_output_tps, None);
                assert_eq!(store.stats_overview(Some(3)).await?.avg_first_token_ms, None);
                assert!(store.stats_series(3, 3_600_000, 0).await?.is_empty());
                let now = chrono::Utc::now().timestamp_millis();
                let recent = now / 3_600_000 * 3_600_000 - 5_400_000;
                let old = now - 4 * 60 * 60 * 1_000;
                insert_turn(&pool, "recent", recent, "model", "key").await?;
                insert_attempt(&pool, "recent-a", "recent", recent, 12, Some(5)).await?;
                insert_attempt(&pool, "recent-b", "recent", recent, 3, Some(9)).await?;
                sqlx::query("UPDATE target_attempt_observations SET output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE model_turn_id='recent'")
                    .execute(&pool).await?;
                // 后续发布和客户端交付失败不能改写已成功的上游 attempt。
                sqlx::query("UPDATE model_turn_observations SET status='failed' WHERE id='recent'")
                    .execute(&pool).await?;
                sqlx::query("UPDATE inference_run_observations SET status='failed',terminal_reason='delivery_error' WHERE id='recent'")
                    .execute(&pool).await?;
                // 失败和未完成 attempt 即使有用量也不计入。
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

                let overview = store.stats_overview(Some(3)).await?;
                assert_eq!(overview.total_input_tokens, Some(15));
                assert_eq!(overview.total_output_tokens, Some(50));
                assert_eq!(overview.total_reasoning_tokens, Some(2));
                assert_eq!(overview.avg_output_tps, Some(5.0));
                assert_eq!(overview.avg_first_token_ms, Some(4900.0));
                assert_eq!(overview.avg_duration_ms, Some(10.0));
                // 未知字段只跳过该 attempt，不遮蔽其他已确认用量；全部未报告的字段保持 null。
                let all_time = store.stats_overview(None).await?;
                assert_eq!(all_time.total_input_tokens, Some(12543));
                assert_eq!(all_time.total_output_tokens, Some(53));
                assert_eq!(all_time.total_cache_write_tokens, None);
                assert_eq!(all_time.avg_output_tps, None);

                let series = store.stats_series(3, 3_600_000, 0).await?;
                assert_eq!(series.len(), 1);
                assert_eq!(series[0].total_input_tokens, Some(15));
                assert_eq!(series[0].total_output_tokens, Some(50));
                assert_eq!(series[0].total_reasoning_tokens, Some(2));
                assert_eq!(series[0].avg_output_tps, Some(5.0));
                assert_eq!(series[0].avg_first_token_ms, Some(4900.0));
                assert_eq!(series[0].avg_duration_ms, Some(10.0));
                assert_eq!(series[0].bucket_start % 3_600_000, 0);

                let quarter = store.stats_series(3, 900_000, 0).await?;
                assert_eq!(quarter.len(), 1);
                assert_eq!(quarter[0].bucket_start % 900_000, 0);
                let day = store.stats_series(3, 86_400_000, 8 * 3_600_000).await?;
                assert_eq!(day.len(), 1);
                // UTC+8 日桶边界对齐本地零点，bucket_start 仍是真实 UTC 时刻。
                assert_eq!(day[0].bucket_start % 86_400_000, 16 * 3_600_000);

                let models = store.stats_by_model(Some(3)).await?;
                assert_eq!(models.len(), 1);
                assert_eq!(models[0].total_input_tokens, Some(15));
                assert_eq!(models[0].total_output_tokens, Some(50));
                assert_eq!(models[0].total_reasoning_tokens, Some(2));

                let api_keys = store.stats_by_api_key(Some(3)).await?;
                assert_eq!(api_keys.len(), 1);
                assert_eq!(api_keys[0].total_input_tokens, Some(15));
                assert_eq!(api_keys[0].total_output_tokens, Some(50));
                assert_eq!(api_keys[0].reasoning_tokens, Some(2));

                let providers = store.stats_by_provider(Some(3)).await?;
                assert_eq!(providers.len(), 2);
                assert_eq!(providers.iter().find(|provider| provider.provider == "Provider").unwrap().avg_output_tps, Some(5.0));
                let old_model = store.stats_by_model(None).await?.into_iter()
                    .find(|model| model.model == "unknown-cache").unwrap();
                assert_eq!(old_model.total_input_tokens, Some(12528));
                // 同组只缺一条成功样本也使 TPS 未知，但已知 Token 合计仍保留。
                for (change, expected_tps, expected_output, expected_first, expected_input) in [
                    ("output_tokens=CASE id WHEN 'recent-a' THEN 20 END,input_tokens=CASE id WHEN 'recent-a' THEN 12 END", None, Some(20), Some(4900.0), Some(12)),
                    ("duration_ms=CASE id WHEN 'recent-a' THEN 1000 END", None, Some(50), Some(4900.0), Some(15)),
                    ("duration_ms=0", None, Some(50), Some(4900.0), Some(15)),
                    ("output_tokens=0", Some(0.0), Some(0), Some(4900.0), Some(15)),
                    ("first_token_ms=NULL", Some(5.0), Some(50), None, Some(15)),
                    ("first_token_ms=CASE id WHEN 'recent-a' THEN 900 END", Some(5.0), Some(50), Some(900.0), Some(15)),
                    ("first_token_ms=CASE id WHEN 'recent-a' THEN 0 END", Some(5.0), Some(50), Some(0.0), Some(15)),
                    ("first_token_ms=0", Some(5.0), Some(50), Some(0.0), Some(15)),
                    ("status='failed'", None, None, None, None),
                ] {
                    sqlx::query("UPDATE target_attempt_observations SET status='completed',input_tokens=CASE id WHEN 'recent-a' THEN 12 ELSE 3 END,output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE id IN ('recent-a','recent-b')")
                        .execute(&pool).await?;
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "UPDATE target_attempt_observations SET {change} WHERE id IN ('recent-a','recent-b')"
                    ))).execute(&pool).await?;
                    assert_eq!(store.stats_by_provider(Some(3)).await?.into_iter().find(|provider| provider.provider == "Provider").unwrap().avg_output_tps, expected_tps, "{change}");
                    let overview = store.stats_overview(Some(3)).await?;
                    assert_eq!(overview.avg_output_tps, expected_tps, "{change}");
                    assert_eq!(overview.total_output_tokens, expected_output, "{change}");
                    assert_eq!(overview.avg_first_token_ms, expected_first, "{change}");
                    assert_eq!(overview.total_input_tokens, expected_input, "{change}");
                    let series = store.stats_series(3, 3_600_000, 0).await?;
                    assert_eq!(series.len(), 1);
                    assert_eq!(series[0].avg_output_tps, expected_tps, "{change}");
                    assert_eq!(series[0].total_output_tokens, expected_output, "{change}");
                    assert_eq!(series[0].avg_first_token_ms, expected_first, "{change}");
                    assert_eq!(series[0].total_input_tokens, expected_input, "{change}");
                }

                // 分属两桶仍按原始窗口合计计算，不平均两个桶的 TPS。
                let next = recent + 3_600_000;
                insert_turn(&pool, "next", next, "model", "key").await?;
                sqlx::query("UPDATE target_attempt_observations SET status='completed',output_tokens=CASE id WHEN 'recent-a' THEN 20 ELSE 30 END,duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 9000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 8900 END WHERE id IN ('recent-a','recent-b')")
                    .execute(&pool).await?;
                // attempt 时间故意仍在第一桶，验证按所属 Model Turn 分桶。
                sqlx::query("UPDATE target_attempt_observations SET model_turn_id='next',run_id='next',interaction_id='next' WHERE id='recent-b'")
                    .execute(&pool).await?;
                assert_eq!(store.stats_overview(Some(3)).await?.avg_output_tps, Some(5.0));
                let series = store.stats_series(3, 3_600_000, 0).await?;
                assert_eq!(series.len(), 2);
                assert_eq!(series[0].bucket_start, recent - 1_800_000);
                assert_eq!(series[1].bucket_start, next - 1_800_000);
                assert_eq!(series[0].avg_output_tps, Some(20.0));
                assert!((series[1].avg_output_tps.unwrap() - 10.0 / 3.0).abs() < 1e-12);
                assert_eq!(series[0].avg_first_token_ms, Some(900.0));
                assert_eq!(series[1].avg_first_token_ms, Some(8900.0));
                sqlx::query("DELETE FROM interaction_observations WHERE id IN ('recent','old','next')")
                    .execute(&pool)
                    .await?;
                verify_final_failure_counts(&pool).await?;
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
