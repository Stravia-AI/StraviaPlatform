use super::*;

const FAILED_SELECT: &str = "
SELECT r.id,'rejection' AS kind,COALESCE(r.started_at,r.occurred_at) AS started_at,
r.duration_ms,r.api_key_id,r.api_key_name,r.request_model AS model,
(SELECT id FROM models WHERE model_id=r.request_model) AS route_id,
NULL AS model_display_name,NULL AS interaction_id,NULL AS root_id,NULL AS run_id,
r.failure_json,r.code,CAST(r.status_code AS BIGINT) AS status_code,
CASE WHEN NOT r.debug_enabled THEN 'none' WHEN m.trace_id IS NULL THEN 'partial' ELSE m.status END AS debug_status,
r.started_at IS NULL AS observation_gap,r.expires_at
FROM rejected_request_observations r LEFT JOIN debug_trace_manifests m ON m.rejection_id=r.id
WHERE r.status_code<>499 AND r.code NOT IN ('request_aborted','cancelled','client_disconnected')
UNION ALL
SELECT r.id,'run' AS kind,r.started_at,r.finished_at-r.started_at AS duration_ms,
i.api_key_id,i.api_key_name,r.request_model AS model,r.route_id,r.model_display_name,
i.id AS interaction_id,i.root_id,r.id AS run_id,r.failure_json,r.terminal_reason AS code,
CAST(NULL AS BIGINT) AS status_code,
CASE WHEN NOT r.debug_enabled THEN 'none' WHEN m.trace_id IS NULL THEN 'partial' ELSE m.status END AS debug_status,
i.observation_gap OR r.failure_json IS NULL AS observation_gap,r.expires_at
FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id
LEFT JOIN debug_trace_manifests m ON m.run_id=r.id
WHERE r.status='failed' AND r.finished_at IS NOT NULL
AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted'))";

#[derive(FromRow)]
struct FailedRow {
    id: String,
    kind: String,
    started_at: i64,
    duration_ms: Option<i64>,
    api_key_id: Option<String>,
    api_key_name: Option<String>,
    model: Option<String>,
    model_display_name: Option<String>,
    interaction_id: Option<String>,
    root_id: Option<String>,
    run_id: Option<String>,
    failure_json: Option<String>,
    code: Option<String>,
    status_code: Option<i64>,
    debug_status: String,
    observation_gap: bool,
}

impl FailedRow {
    fn summary(self) -> anyhow::Result<FailedRequestSummary> {
        let error = match self.failure_json {
            Some(json) => serde_json::from_str(&json)?,
            None => FailureDiagnostic {
                source: (self.kind == "rejection").then(|| "platform".into()),
                code: self.code,
                message: None,
                status_code: self.status_code.and_then(|code| u16::try_from(code).ok()),
                upstream_code: None,
            },
        };
        Ok(FailedRequestSummary {
            request_id: self.id.clone(),
            id: self.id,
            kind: self.kind,
            started_at: self.started_at,
            duration_ms: self.duration_ms,
            api_key_id: self.api_key_id,
            api_key_name: self.api_key_name,
            client: None,
            model: self.model,
            model_display_name: self.model_display_name,
            services: Vec::new(),
            error,
            interaction_id: self.interaction_id,
            root_id: self.root_id,
            run_id: self.run_id,
            debug_status: self.debug_status,
            observation_gap: self.observation_gap,
        })
    }
}

type FailedCursor = (i64, String, String);

// 两个后端使用同一查询形状，仅绑定参数的编码由 SQLx 后端决定。
macro_rules! failed_rows {
    ($pool:expr, $db:ty, $query:expr, $cursor:expr, $window:expr, $limit:expr, $identity:expr) => {{
        let q: &FailedRequestQuery = $query;
        let window: &QueryWindow = $window;
        let identity: Option<(&str, &str)> = $identity;
        let now = chrono::Utc::now().timestamp_millis();
        let filtered = |columns: &str| {
            let mut sql = QueryBuilder::<$db>::new("SELECT ");
            sql.push(columns).push(" FROM (").push(FAILED_SELECT).push(") f WHERE f.expires_at>").push_bind(now);
            if let Some((kind, id)) = identity {
                sql.push(" AND f.kind=").push_bind(kind).push(" AND f.id=").push_bind(id);
            } else {
                sql.push(" AND f.started_at>=").push_bind(window.start);
                if window.bounded_end {
                    sql.push(" AND f.started_at<").push_bind(window.end);
                }
                if let Some(model) = &q.model {
                    sql.push(" AND f.route_id=").push_bind(model);
                }
                if let Some(key) = &q.api_key {
                    sql.push(" AND (f.api_key_id=").push_bind(key)
                        .push(" OR f.api_key_name=").push_bind(key).push(")");
                }
                if let Some(provider) = &q.provider {
                    sql.push(" AND EXISTS(SELECT 1 FROM target_attempt_observations a WHERE a.run_id=f.run_id AND a.provider_id=")
                        .push_bind(provider).push(")");
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
        if let Some((started_at, kind, id)) = $cursor {
            sql.push(" AND (f.started_at<").push_bind(*started_at)
                .push(" OR (f.started_at=").push_bind(*started_at)
                .push(" AND (f.kind>").push_bind(kind)
                .push(" OR (f.kind=").push_bind(kind)
                .push(" AND f.id>").push_bind(id).push("))))");
        }
        sql.push(" ORDER BY f.started_at DESC,f.kind,f.id LIMIT ").push_bind($limit);
        (total, sql.build_query_as::<FailedRow>().fetch_all($pool).await?)
    }};
}

impl super::super::InteractionObservation {
    pub(crate) async fn failed_requests(
        &self,
        q: FailedRequestQuery,
    ) -> anyhow::Result<FailedRequestPage> {
        self.inner.store.failed_requests(q).await
    }

    pub(crate) async fn failed_request_detail(
        &self,
        kind: &str,
        id: &str,
    ) -> anyhow::Result<Option<FailedRequestDetail>> {
        self.inner.store.failed_request_detail(kind, id).await
    }
}

impl ObservationStore {
    pub async fn failed_requests(
        &self,
        q: FailedRequestQuery,
    ) -> anyhow::Result<FailedRequestPage> {
        let window = query_window(q.start_at, q.end_at, q.anchor_at, q.window_index)?;
        let cursor: Option<FailedCursor> = q
            .cursor
            .as_deref()
            .map(|value| {
                anyhow::ensure!(value.len() <= 1024, ObservationQueryError::InvalidEventPage);
                let cursor: FailedCursor = serde_json::from_str(value)
                    .map_err(|_| ObservationQueryError::InvalidEventPage)?;
                anyhow::ensure!(
                    matches!(cursor.1.as_str(), "rejection" | "run"),
                    ObservationQueryError::InvalidEventPage
                );
                Ok::<_, anyhow::Error>(cursor)
            })
            .transpose()?;
        let limit = i64::from(q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT));
        let (total, mut rows) = match self {
            Self::Sqlite(pool) => failed_rows!(
                pool,
                sqlx::Sqlite,
                &q,
                cursor.as_ref(),
                &window,
                limit + 1,
                None
            ),
            Self::Postgres(pool) => failed_rows!(
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
            Some(serde_json::to_string(&(
                last.started_at,
                &last.kind,
                &last.id,
            ))?)
        } else {
            None
        };
        rows.truncate(limit as usize);
        let mut items = rows
            .into_iter()
            .map(FailedRow::summary)
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.failed_services(&mut items).await?;
        Ok(FailedRequestPage {
            items,
            total,
            next_cursor,
            snapshot_sequence: self.max_sequence().await?,
        })
    }

    async fn failed_services(&self, items: &mut [FailedRequestSummary]) -> anyhow::Result<()> {
        let ids: Vec<&str> = items
            .iter()
            .filter_map(|item| item.run_id.as_deref())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let rows: Vec<(String, String, String)> = match self {
            Self::Sqlite(pool) => {
                let mut sql = QueryBuilder::<sqlx::Sqlite>::new(
                    "SELECT DISTINCT run_id,provider_id,provider_name FROM target_attempt_observations WHERE run_id IN (",
                );
                let mut separated = sql.separated(",");
                for id in &ids {
                    separated.push_bind(*id);
                }
                sql.push(") ORDER BY run_id,provider_id,provider_name");
                sql.build_query_as().fetch_all(pool).await?
            }
            Self::Postgres(pool) => {
                let mut sql = QueryBuilder::<sqlx::Postgres>::new(
                    "SELECT DISTINCT run_id,provider_id,provider_name FROM target_attempt_observations WHERE run_id IN (",
                );
                let mut separated = sql.separated(",");
                for id in &ids {
                    separated.push_bind(*id);
                }
                sql.push(") ORDER BY run_id,provider_id,provider_name");
                sql.build_query_as().fetch_all(pool).await?
            }
        };
        let mut services = std::collections::HashMap::<String, Vec<FailedRequestService>>::new();
        for (run, id, name) in rows {
            services
                .entry(run)
                .or_default()
                .push(FailedRequestService { id, name });
        }
        for item in items {
            if let Some(run) = &item.run_id {
                item.services = services.remove(run).unwrap_or_default();
            }
        }
        Ok(())
    }

    pub async fn failed_request_detail(
        &self,
        kind: &str,
        id: &str,
    ) -> anyhow::Result<Option<FailedRequestDetail>> {
        if !matches!(kind, "rejection" | "run") {
            return Ok(None);
        }
        let q = FailedRequestQuery::default();
        let window = query_window(None, None, None, None)?;
        let cursor: Option<&FailedCursor> = None;
        let (_, rows) = match self {
            Self::Sqlite(pool) => failed_rows!(
                pool,
                sqlx::Sqlite,
                &q,
                cursor,
                &window,
                1i64,
                Some((kind, id))
            ),
            Self::Postgres(pool) => failed_rows!(
                pool,
                sqlx::Postgres,
                &q,
                cursor,
                &window,
                1i64,
                Some((kind, id))
            ),
        };
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let mut request = row.summary()?;
        self.failed_services(std::slice::from_mut(&mut request))
            .await?;
        let snapshot_sequence = self.max_sequence().await?;
        let (events, trace) = if kind == "rejection" {
            (
                self.rejection_events(id, snapshot_sequence).await?,
                self.manifest_for_rejection(id).await?,
            )
        } else {
            let events = match self {
                Self::Sqlite(pool) => map_sqlite_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE run_id=? AND sequence<=? ORDER BY sequence")
                    .bind(id).bind(snapshot_sequence).fetch_all(pool).await?)?,
                Self::Postgres(pool) => map_postgres_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload::text FROM observation_events WHERE run_id=$1 AND sequence<=$2 ORDER BY sequence")
                    .bind(id).bind(snapshot_sequence).fetch_all(pool).await?)?,
            };
            (events, self.manifest_for_run(id).await?)
        };
        Ok(Some(FailedRequestDetail {
            request,
            events,
            trace,
            snapshot_sequence,
        }))
    }
}
