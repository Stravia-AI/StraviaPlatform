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
                 (SELECT SUM(CASE WHEN input_tokens IS NOT NULL AND cache_read_tokens IS NOT NULL THEN CASE WHEN input_tokens>cache_read_tokens THEN input_tokens-cache_read_tokens ELSE 0 END END)::BIGINT FROM attempts) AS total_input_tokens,
                 (SELECT SUM(output_tokens)::BIGINT FROM attempts) AS total_output_tokens,
                 (SELECT SUM(cache_read_tokens)::BIGINT FROM attempts) AS total_cache_read_tokens,
                 (SELECT SUM(cache_write_tokens)::BIGINT FROM attempts) AS total_cache_write_tokens,
                 (SELECT SUM(reasoning_tokens)::BIGINT FROM attempts) AS total_reasoning_tokens,
                 (SELECT AVG((finished_at - started_at)::FLOAT8) FROM turns WHERE finished_at IS NOT NULL) AS avg_duration_ms,
                 (SELECT AVG(first_token_ms::FLOAT8) FROM attempts) AS avg_first_token_ms,
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
                        SUM(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END END)::BIGINT AS total_input_tokens,
                        SUM(a.output_tokens)::BIGINT AS total_output_tokens,
                        SUM(a.cache_read_tokens)::BIGINT AS total_cache_read_tokens,
                        SUM(a.cache_write_tokens)::BIGINT AS total_cache_write_tokens,
                        SUM(a.reasoning_tokens)::BIGINT AS total_reasoning_tokens,
                        AVG(a.first_token_ms::FLOAT8) AS avg_first_token_ms
                 FROM turns t JOIN target_attempt_observations a ON a.model_turn_id = t.id AND a.status = 'completed' GROUP BY t.bucket_start
             ), buckets AS (
                 SELECT bucket_start FROM turn_stats UNION SELECT bucket_start FROM request_stats
             )
             SELECT b.bucket_start, COALESCE(r.request_count, 0)::BIGINT AS request_count,
                    COALESCE(e.error_count, 0)::BIGINT AS error_count,
                    a.total_input_tokens, a.total_output_tokens, a.total_cache_read_tokens,
                    a.total_cache_write_tokens, a.total_reasoning_tokens,
                    t.avg_duration_ms, a.avg_first_token_ms
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
                        SUM(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END END)::BIGINT AS total_input_tokens,
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
                    SUM(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END END)::BIGINT AS total_input_tokens,
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
    async fn postgres_management_stats_project_net_input_and_preserve_full_attempt_tps()
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
                let now = chrono::Utc::now().timestamp_millis();
                let recent = now - 1_000;
                let old = now - 2 * 60 * 60 * 1_000;
                insert_turn(&pool, "recent", recent, "model", "key").await?;
                insert_attempt(&pool, "recent-a", "recent", recent, 12, Some(5)).await?;
                insert_attempt(&pool, "recent-b", "recent", recent, 3, Some(9)).await?;
                sqlx::query("UPDATE target_attempt_observations SET duration_ms=CASE id WHEN 'recent-a' THEN 1000 ELSE 3000 END,first_token_ms=CASE id WHEN 'recent-a' THEN 900 ELSE 2990 END WHERE model_turn_id='recent'")
                    .execute(&pool).await?;
                let store = PostgresUsageStatsStore {
                    pool: pool.clone(),
                    last_route_snapshot: Arc::new(parking_lot::RwLock::new(Vec::new())),
                };
                let scheduling = store.route_scheduling_snapshot().await;
                assert!(!scheduling.stale);
                assert_eq!(scheduling.targets.len(), 1);
                // 管理统计仍只统计已完成 attempt；失败 attempt 不计入。
                insert_failed_attempt(&pool, "recent-failed", "recent", recent).await?;
                insert_turn(&pool, "old", old, "unknown-cache", "old-key").await?;
                insert_attempt(&pool, "old-a", "old", old, 12528, None).await?;

                let overview = store.stats_overview(Some(1)).await?;
                assert_eq!(overview.total_input_tokens, Some(7));
                assert_eq!(overview.total_output_tokens, Some(6));
                assert_eq!(overview.total_reasoning_tokens, Some(2));
                // 未知字段只跳过该 attempt，不遮蔽其他已确认用量；全部未报告的字段保持 null。
                let all_time = store.stats_overview(None).await?;
                assert_eq!(all_time.total_input_tokens, Some(7));
                assert_eq!(all_time.total_output_tokens, Some(9));
                assert_eq!(all_time.total_cache_write_tokens, None);

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

                let providers = store.stats_by_provider(Some(1)).await?;
                assert_eq!(providers.len(), 1);
                assert_eq!(providers[0].avg_output_tps, Some(1.5));
                let old_model = store.stats_by_model(None).await?.into_iter()
                    .find(|model| model.model == "unknown-cache").unwrap();
                assert_eq!(old_model.total_input_tokens, None);
                for (column, value, expected) in [
                    ("output_tokens", "NULL", None),
                    ("output_tokens", "0", Some(0.0)),
                    ("duration_ms", "NULL", None),
                ] {
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "UPDATE target_attempt_observations SET {column}={value} WHERE model_turn_id='recent'"
                    ))).execute(&pool).await?;
                    assert_eq!(store.stats_by_provider(Some(1)).await?[0].avg_output_tps, expected);
                }

                let raw_input: Option<i64> = sqlx::query_scalar(
                    "SELECT SUM(input_tokens)::BIGINT FROM target_attempt_observations WHERE model_turn_id='recent'",
                )
                .fetch_one(&pool)
                .await?;
                assert_eq!(raw_input, Some(15));
                for (assignment, expected) in [
                    ("cache_write_tokens=100", Some(7)),
                    ("cache_read_tokens=CASE id WHEN 'recent-a' THEN 5 END", Some(7)),
                    ("cache_read_tokens=NULL", None),
                    ("cache_read_tokens=100", Some(0)),
                    ("input_tokens=NULL,cache_read_tokens=0", None),
                    ("input_tokens=0,cache_read_tokens=0", Some(0)),
                ] {
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "UPDATE target_attempt_observations SET {assignment} WHERE model_turn_id='recent'"
                    )))
                    .execute(&pool)
                    .await?;
                    assert_eq!(store.stats_overview(Some(1)).await?.total_input_tokens, expected);
                    assert_eq!(store.stats_series(1, 3_600_000, 0).await?[0].total_input_tokens, expected);
                    assert_eq!(store.stats_by_model(Some(1)).await?[0].total_input_tokens, expected);
                    assert_eq!(store.stats_by_api_key(Some(1)).await?[0].total_input_tokens, expected);
                }
                sqlx::query("DELETE FROM interaction_observations WHERE id IN ('recent','old')")
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
