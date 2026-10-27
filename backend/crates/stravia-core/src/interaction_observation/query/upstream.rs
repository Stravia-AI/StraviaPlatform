use super::*;

// 上游错误按 Target 尝试计：尝试终态失败且错误来源为上游，含 capability 尝试。
// 本地准备失败、未发送、取消、中断与观察丢失的尝试 error_source 不是上游或状态
// 不是 failed，因此不会出现。明细随所属运行过期（尝试表本身不存 expires_at）。
fn upstream_error_select() -> String {
    format!(
        "
SELECT a.id,a.run_id,a.interaction_id,i.root_id,r.route_id,
r.request_model AS model,r.model_display_name,
a.started_at,a.duration_ms,a.provider_id,a.provider_name,a.upstream_model,
i.api_key_id,i.api_key_name,
CAST(a.status_code AS BIGINT) AS status_code,
a.error_code,a.error_source,a.error_message,a.upstream_error_code,
-- 已交付客户端工具调用的 waiting_client 与随后被续接的 superseded 都是成功响应；
-- in_progress 只保留尚未结束的运行，取消/断线类才归为 interrupted。
CASE
 WHEN r.status IN ('completed','superseded') THEN 'recovered'
 WHEN r.status='waiting_client' AND r.finished_at IS NOT NULL THEN 'recovered'
 WHEN r.status='failed' AND r.finished_at IS NOT NULL
  AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ({CANCELLED_TERMINAL_REASONS}))
  THEN 'caused_failure'
 WHEN r.status IN ('cancelled','user_interrupted','interrupted','disconnected')
  OR (r.finished_at IS NOT NULL AND COALESCE(r.terminal_reason,'') IN ({CANCELLED_TERMINAL_REASONS}))
  THEN 'interrupted'
 ELSE 'in_progress'
END AS outcome,
CASE WHEN NOT r.debug_enabled THEN 'none' ELSE 'partial' END AS debug_status,
i.observation_gap AS observation_gap,r.expires_at
FROM target_attempt_observations a
JOIN inference_run_observations r ON r.id=a.run_id
JOIN interaction_observations i ON i.id=a.interaction_id
WHERE a.status='failed' AND a.error_source='upstream'"
    )
}

#[derive(FromRow)]
struct UpstreamErrorRow {
    id: String,
    run_id: String,
    interaction_id: Option<String>,
    root_id: Option<String>,
    model: Option<String>,
    model_display_name: Option<String>,
    started_at: i64,
    duration_ms: Option<i64>,
    provider_id: Option<String>,
    provider_name: String,
    upstream_model: Option<String>,
    api_key_id: Option<String>,
    api_key_name: Option<String>,
    status_code: Option<i64>,
    error_code: Option<String>,
    error_source: Option<String>,
    error_message: Option<String>,
    upstream_error_code: Option<String>,
    outcome: String,
    debug_status: String,
    observation_gap: bool,
}

impl UpstreamErrorRow {
    fn summary(mut self, store: &ObservationStore) -> anyhow::Result<UpstreamErrorSummary> {
        if self.debug_status != "none" {
            self.debug_status = store
                .debug_trace_index()
                .for_run(&self.run_id)
                .map_or_else(|| "partial".into(), |trace| trace.status);
        }
        Ok(UpstreamErrorSummary {
            request_id: self.run_id.clone(),
            id: self.id,
            run_id: self.run_id,
            interaction_id: self.interaction_id,
            root_id: self.root_id,
            started_at: self.started_at,
            duration_ms: self.duration_ms,
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            upstream_model: self.upstream_model,
            model: self.model,
            model_display_name: self.model_display_name,
            api_key_id: self.api_key_id,
            api_key_name: self.api_key_name,
            client: None,
            error: FailureDiagnostic {
                source: self.error_source,
                code: self.error_code,
                message: self.error_message,
                status_code: self.status_code.and_then(|code| u16::try_from(code).ok()),
                upstream_code: self.upstream_error_code,
            },
            outcome: self.outcome,
            debug_status: self.debug_status,
            observation_gap: self.observation_gap,
        })
    }
}

type UpstreamErrorCursor = (i64, String);

// 两个后端使用同一查询形状，仅绑定参数的编码由 SQLx 后端决定。
// 内层 WHERE 与部分索引 (started_at,id) WHERE status='failed' AND
// error_source='upstream' 的谓词一致，窗口与排序可走该索引。
macro_rules! upstream_error_rows {
    ($pool:expr, $db:ty, $query:expr, $cursor:expr, $window:expr, $limit:expr, $identity:expr) => {{
        let q: &UpstreamErrorQuery = $query;
        let window: &QueryWindow = $window;
        let identity: Option<&str> = $identity;
        let select = upstream_error_select();
        let now = chrono::Utc::now().timestamp_millis();
        let filtered = |columns: &str| {
            let mut sql = QueryBuilder::<$db>::new("SELECT ");
            sql.push(columns)
                .push(" FROM (")
                .push(&select)
                .push(") f WHERE f.expires_at>")
                .push_bind(now);
            if let Some(id) = identity {
                sql.push(" AND f.id=").push_bind(id);
            } else {
                sql.push(" AND f.started_at>=").push_bind(window.start);
                if window.bounded_end {
                    sql.push(" AND f.started_at<").push_bind(window.end);
                }
                if let Some(provider) = &q.provider {
                    sql.push(" AND f.provider_id=").push_bind(provider);
                }
                if let Some(model) = &q.model {
                    sql.push(" AND f.route_id=").push_bind(model);
                }
                if let Some(key) = &q.api_key {
                    sql.push(" AND (f.api_key_id=")
                        .push_bind(key)
                        .push(" OR f.api_key_name=")
                        .push_bind(key)
                        .push(")");
                }
                if let Some(outcome) = &q.outcome {
                    sql.push(" AND f.outcome=").push_bind(outcome);
                }
            }
            sql
        };
        let total = if identity.is_none() {
            let mut count = filtered("COUNT(*)");
            count.build_query_scalar::<i64>().fetch_one($pool).await?
        } else {
            0
        };
        let mut sql = filtered("f.*");
        if let Some((started_at, id)) = $cursor {
            sql.push(" AND (f.started_at<")
                .push_bind(*started_at)
                .push(" OR (f.started_at=")
                .push_bind(*started_at)
                .push(" AND f.id>")
                .push_bind(id)
                .push("))");
        }
        sql.push(" ORDER BY f.started_at DESC,f.id LIMIT ")
            .push_bind($limit);
        (
            total,
            sql.build_query_as::<UpstreamErrorRow>()
                .fetch_all($pool)
                .await?,
        )
    }};
}

impl super::super::InteractionObservation {
    pub(crate) async fn upstream_errors(
        &self,
        q: UpstreamErrorQuery,
    ) -> anyhow::Result<UpstreamErrorPage> {
        self.inner.store.upstream_errors(q).await
    }

    pub(crate) async fn upstream_error_detail(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<UpstreamErrorDetail>> {
        self.inner.store.upstream_error_detail(id).await
    }
}

impl ObservationStore {
    pub async fn upstream_errors(
        &self,
        q: UpstreamErrorQuery,
    ) -> anyhow::Result<UpstreamErrorPage> {
        if let Some(outcome) = &q.outcome {
            anyhow::ensure!(
                matches!(
                    outcome.as_str(),
                    "recovered" | "caused_failure" | "in_progress" | "interrupted"
                ),
                ObservationQueryError::InvalidFilter
            );
        }
        let window = query_window(q.start_at, q.end_at, q.anchor_at, q.window_index)?;
        let cursor: Option<UpstreamErrorCursor> = q
            .cursor
            .as_deref()
            .map(|value| {
                anyhow::ensure!(value.len() <= 1024, ObservationQueryError::InvalidEventPage);
                serde_json::from_str(value)
                    .map_err(|_| ObservationQueryError::InvalidEventPage.into())
            })
            .transpose()?;
        let limit = i64::from(q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT));
        let (total, mut rows) = match self {
            Self::Sqlite(pool, _) => upstream_error_rows!(
                pool,
                sqlx::Sqlite,
                &q,
                cursor.as_ref(),
                &window,
                limit + 1,
                None
            ),
            Self::Postgres(pool, _) => upstream_error_rows!(
                pool,
                sqlx::Postgres,
                &q,
                cursor.as_ref(),
                &window,
                limit + 1,
                None
            ),
        };
        let next_cursor = if rows.len() > limit as usize {
            let last = &rows[limit as usize - 1];
            Some(serde_json::to_string(&(last.started_at, &last.id))?)
        } else {
            None
        };
        rows.truncate(limit as usize);
        let items = rows
            .into_iter()
            .map(|row| row.summary(self))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(UpstreamErrorPage {
            items,
            total,
            next_cursor,
            snapshot_sequence: self.max_sequence().await?,
        })
    }

    pub async fn upstream_error_detail(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<UpstreamErrorDetail>> {
        let q = UpstreamErrorQuery::default();
        let window = query_window(None, None, None, None)?;
        let cursor: Option<&UpstreamErrorCursor> = None;
        let (_, rows) = match self {
            Self::Sqlite(pool, _) => {
                upstream_error_rows!(pool, sqlx::Sqlite, &q, cursor, &window, 1i64, Some(id))
            }
            Self::Postgres(pool, _) => {
                upstream_error_rows!(pool, sqlx::Postgres, &q, cursor, &window, 1i64, Some(id))
            }
        };
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let error = row.summary(self)?;
        let snapshot_sequence = self.max_sequence().await?;
        let attempts = self.run_attempts(&error.run_id).await?;
        let events = match self {
            Self::Sqlite(pool, _) => map_sqlite_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE run_id=? AND sequence<=? ORDER BY sequence")
                .bind(&error.run_id).bind(snapshot_sequence).fetch_all(pool).await?)?,
            Self::Postgres(pool, _) => map_postgres_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE run_id=$1 AND sequence<=$2 ORDER BY sequence")
                .bind(&error.run_id).bind(snapshot_sequence).fetch_all(pool).await?)?,
        };
        let trace = self.manifest_for_run(&error.run_id).await?;
        Ok(Some(UpstreamErrorDetail {
            error,
            attempts,
            events,
            trace,
            snapshot_sequence,
        }))
    }

    /// 同一运行内的全部尝试（含本地失败、取消与 capability 尝试），按开始时间升序。
    async fn run_attempts(&self, run_id: &str) -> anyhow::Result<Vec<UpstreamAttemptTiming>> {
        let rows: Vec<UpstreamAttemptTiming> = match self {
            Self::Sqlite(pool, _) => sqlx::query_as("SELECT id,started_at,duration_ms,status,provider_id,provider_name,upstream_model,CAST(status_code AS BIGINT) AS status_code,error_code,error_source FROM target_attempt_observations WHERE run_id=? ORDER BY started_at,id")
                .bind(run_id).fetch_all(pool).await?,
            Self::Postgres(pool, _) => sqlx::query_as("SELECT id,started_at,duration_ms,status,provider_id,provider_name,upstream_model,CAST(status_code AS BIGINT) AS status_code,error_code,error_source FROM target_attempt_observations WHERE run_id=$1 ORDER BY started_at,id")
                .bind(run_id).fetch_all(pool).await?,
        };
        Ok(rows)
    }
}
