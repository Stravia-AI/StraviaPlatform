use std::collections::HashSet;
pub(super) mod failed;

use sqlx::{FromRow, QueryBuilder, Row};

use super::{store::ObservationStore, types::*};

pub(super) struct BundleRunRecord {
    pub id: String,
    pub debug_enabled: bool,
    pub trace: Option<TraceManifest>,
}

pub(super) struct BundleInteractionRecords {
    pub root_id: String,
    pub runs: Vec<BundleRunRecord>,
    pub events: Vec<ObservationEvent>,
}

pub(super) struct BundleRejectionRecords {
    pub id: String,
    pub debug_enabled: bool,
    pub trace: Option<TraceManifest>,
    pub events: Vec<ObservationEvent>,
}

const DAY_MS: i64 = 86_400_000;
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;
// 已确认部分合计逐 attempt 净输入与输出；未知操作数不贡献净输入。
// 不另计 reasoning；估算只补充仍在运行且尚无
// usage 报告的 Model Turn，每轮一次，不能因 Target 重试重复累计或冒充确认用量。
// 按根 DAG（含子孙）一次聚合，列表、计数、分页与 SSE matched 共用同一判定。
const CHAIN_TOKEN_ROOTS: &str = "i.root_id IN (
SELECT tokens.root_id FROM (
    SELECT m.root_id,
        COALESCE(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END END,0)
        + COALESCE(a.output_tokens,0) AS token_count
    FROM interaction_observations m
    JOIN target_attempt_observations a ON a.interaction_id=m.id
    UNION ALL
    SELECT m.root_id,t.estimated_input_tokens AS token_count
    FROM interaction_observations m
    JOIN model_turn_observations t ON t.interaction_id=m.id
    WHERE t.status='running' AND t.estimated_input_tokens IS NOT NULL
        AND NOT EXISTS (
            SELECT 1 FROM target_attempt_observations a
            WHERE a.model_turn_id=t.id AND a.usage_recorded=TRUE
        )
) tokens GROUP BY tokens.root_id HAVING SUM(tokens.token_count)>=";
// 直接从当前窗口的 attempts 派生累计与覆盖信息，旧版持久化的 NULL 汇总无需回填。
const INTERACTION_SELECT: &str = "SELECT i.id,i.root_id,i.parent_interaction_id,i.generation_root_id,i.first_route_id,i.first_model_display_name,i.status,i.started_at,i.last_active_at,i.input_preview,i.visible_tail,
CAST(SUM(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END END) AS BIGINT) input_tokens,
CAST(SUM(a.output_tokens) AS BIGINT) output_tokens,
CAST(SUM(a.cache_read_tokens) AS BIGINT) cache_read_tokens,
CAST(SUM(a.cache_write_tokens) AS BIGINT) cache_write_tokens,
CAST(SUM(a.reasoning_tokens) AS BIGINT) reasoning_tokens,
COUNT(a.id) attempt_count,
COUNT(a.id)-COUNT(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN 1 END) missing_input_tokens,
COUNT(a.id)-COUNT(a.output_tokens) missing_output_tokens,
COUNT(a.id)-COUNT(a.cache_read_tokens) missing_cache_read_tokens,
COUNT(a.id)-COUNT(a.cache_write_tokens) missing_cache_write_tokens,
COUNT(a.id)-COUNT(a.reasoning_tokens) missing_reasoning_tokens,
i.observation_gap,i.last_event_sequence,
CAST(MAX(CASE WHEN r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) THEN 1 ELSE 0 END) AS BIGINT) failed_request,
CAST(MAX(CASE WHEN r.client_output_committed THEN 1 ELSE 0 END) AS BIGINT) client_output_delivered,
CASE WHEN SUM(CASE WHEN r.debug_enabled THEN 1 ELSE 0 END)=0 THEN 'none' ELSE 'partial' END debug_status FROM interaction_observations i JOIN inference_run_observations r ON r.interaction_id=i.id LEFT JOIN target_attempt_observations a ON a.run_id=r.id ";
// PostgreSQL promotes SUM(BIGINT) to NUMERIC; keep the public usage contract i64.
const RUN_SELECT: &str = "SELECT r.id,r.parent_run_id,r.generation_node_id,r.generation_parent_id,r.route_id,r.model_display_name,r.ingress_protocol,r.status,r.terminal_reason,r.user_interrupted,r.debug_enabled,r.client_output_committed,r.started_at,r.finished_at,r.delivery_completed_at,
CAST(SUM(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END END) AS BIGINT) input_tokens,
CAST(SUM(a.output_tokens) AS BIGINT) output_tokens,
CAST(SUM(a.cache_read_tokens) AS BIGINT) cache_read_tokens,
CAST(SUM(a.cache_write_tokens) AS BIGINT) cache_write_tokens,
CAST(SUM(a.reasoning_tokens) AS BIGINT) reasoning_tokens,
COUNT(a.id) attempt_count,
COUNT(a.id)-COUNT(CASE WHEN a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN 1 END) missing_input_tokens,
COUNT(a.id)-COUNT(a.output_tokens) missing_output_tokens,
COUNT(a.id)-COUNT(a.cache_read_tokens) missing_cache_read_tokens,
COUNT(a.id)-COUNT(a.cache_write_tokens) missing_cache_write_tokens,
COUNT(a.id)-COUNT(a.reasoning_tokens) missing_reasoning_tokens
FROM inference_run_observations r LEFT JOIN target_attempt_observations a ON a.run_id=r.id WHERE r.interaction_id=";

struct QueryWindow {
    anchor: i64,
    index: u32,
    start: i64,
    end: i64,
    bounded_end: bool,
}

fn query_window(
    start_at: Option<i64>,
    end_at: Option<i64>,
    anchor_at: Option<i64>,
    window_index: Option<u32>,
) -> anyhow::Result<QueryWindow> {
    match (start_at, end_at) {
        (Some(start), Some(end)) => {
            let duration = end.checked_sub(start);
            anyhow::ensure!(
                duration.is_some_and(|duration| duration > 0 && duration <= DAY_MS),
                ObservationQueryError::InvalidWindow
            );
            Ok(QueryWindow {
                anchor: end,
                index: 0,
                start,
                end,
                bounded_end: true,
            })
        }
        (None, None) => {
            let anchor = anchor_at.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
            let index = window_index.unwrap_or(0);
            let end = anchor.saturating_sub(i64::from(index).saturating_mul(DAY_MS));
            Ok(QueryWindow {
                anchor,
                index,
                start: end.saturating_sub(DAY_MS),
                end,
                bounded_end: index > 0,
            })
        }
        _ => Err(ObservationQueryError::IncompleteWindow.into()),
    }
}

fn forest_window(query: &ForestQuery) -> anyhow::Result<QueryWindow> {
    let mut window = query_window(
        query.start_at,
        query.end_at,
        query.anchor_at,
        query.window_index,
    )?;
    if query.live_window {
        window.bounded_end = false;
    }
    Ok(window)
}

#[derive(FromRow, Clone)]
struct InteractionRow {
    id: String,
    root_id: String,
    parent_interaction_id: Option<String>,
    generation_root_id: Option<String>,
    first_route_id: String,
    first_model_display_name: Option<String>,
    status: String,
    started_at: i64,
    last_active_at: i64,
    input_preview: Option<String>,
    visible_tail: String,
    failed_request: i64,
    client_output_delivered: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    #[sqlx(flatten)]
    coverage: UsageCoverage,
    observation_gap: bool,
    last_event_sequence: i64,
    debug_status: String,
}

#[derive(Clone, Copy)]
enum RunDebugStatus {
    None,
    Partial,
    Complete,
}

impl RunDebugStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Partial => "partial",
            Self::Complete => "complete",
        }
    }
}

struct RootChangesRead {
    sequence: i64,
    first: Option<i64>,
    total: i64,
    rows: Vec<InteractionRow>,
    matched: HashSet<String>,
    debug: std::collections::HashMap<String, RunDebugStatus>,
    events: Vec<ObservationEvent>,
    root_rows: Vec<(String, i64)>,
}

macro_rules! root_changes_reader {
    ($name:ident, $pool:ty, $roots:ident, $members:ident, $matching:ident, $flags:ident, $context:ident, $isolation:expr) => {
        async fn $name(
            pool: &$pool,
            query: &RootChangesQuery,
            window: &QueryWindow,
            forest: bool,
            index: &super::manifest_index::DebugTraceIndex,
        ) -> anyhow::Result<RootChangesRead> {
            let mut tx = pool.begin().await?;
            if let Some(sql) = $isolation {
                sqlx::query(sql).execute(&mut *tx).await?;
            }
            let (sequence, first): (i64, Option<i64>) = sqlx::query_as(
                "SELECT COALESCE(MAX(sequence),0),MIN(sequence) FROM observation_events",
            )
            .fetch_one(&mut *tx)
            .await?;
            let mut filters = query.filters.clone();
            if !forest {
                filters.cursor = None;
            }
            let limit = if forest {
                filters.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64
            } else {
                0
            };
            let (total, root_rows) = $roots(
                &mut *tx,
                &filters,
                window.start,
                window.end,
                window.bounded_end,
                limit,
            )
            .await?;
            let root_ids: Vec<_> = if forest {
                root_rows
                    .iter()
                    .take(limit as usize)
                    .map(|(id, _)| id.clone())
                    .collect()
            } else {
                query
                    .roots
                    .iter()
                    .map(|root| root.root_id.clone())
                    .collect()
            };
            let mut read = RootChangesRead {
                sequence,
                first,
                total,
                rows: Vec::new(),
                matched: HashSet::new(),
                debug: std::collections::HashMap::new(),
                events: Vec::new(),
                root_rows,
            };
            for chunk in root_ids.chunks(900) {
                read.rows.extend($members(&mut *tx, chunk).await?);
                read.matched
                    .extend($matching(&mut *tx, &filters, chunk).await?);
            }
            let ids: Vec<_> = read.rows.iter().map(|row| row.id.as_str()).collect();
            let mut runs = Vec::new();
            for chunk in ids.chunks(900) {
                runs.extend($flags(&mut *tx, chunk).await?);
            }
            read.debug = ObservationStore::debug_statuses_from_runs(index, runs);
            let context_ids: Vec<_> = if forest
                || query
                    .roots
                    .iter()
                    .all(|root| root.known_interactions.is_empty())
            {
                ids
            } else {
                read.rows
                    .iter()
                    .filter(|row| {
                        let known = query
                            .roots
                            .iter()
                            .find(|root| root.root_id == row.root_id)
                            .and_then(|root| {
                                root.known_interactions
                                    .iter()
                                    .find(|known| known.id == row.id)
                            });
                        interaction_changed(
                            row,
                            read.matched.contains(&row.id),
                            read.debug
                                .get(&row.id)
                                .copied()
                                .unwrap_or(RunDebugStatus::None)
                                .as_str(),
                            known,
                        )
                    })
                    .map(|row| row.id.as_str())
                    .collect()
            };
            for chunk in context_ids.chunks(900) {
                read.events
                    .extend($context(&mut *tx, chunk, sequence).await?);
            }
            tx.commit().await?;
            Ok(read)
        }
    };
}

fn interaction_changed(
    row: &InteractionRow,
    matched: bool,
    debug_status: &str,
    known: Option<&KnownInteraction>,
) -> bool {
    known.is_none_or(|known| {
        known.last_event_sequence != row.last_event_sequence
            || known.matched != matched
            || known.debug_status != debug_status
    })
}

fn group_context_events(
    events: Vec<ObservationEvent>,
) -> std::collections::HashMap<String, Vec<ObservationEvent>> {
    let mut grouped: std::collections::HashMap<String, Vec<ObservationEvent>> =
        std::collections::HashMap::new();
    for event in events {
        if let Some(id) = &event.interaction_id {
            grouped.entry(id.clone()).or_default().push(event);
        }
    }
    grouped
}

root_changes_reader!(
    root_changes_sqlite,
    sqlx::SqlitePool,
    forest_roots_sqlite,
    interactions_for_roots_sqlite,
    matching_in_roots_sqlite,
    debug_flags_sqlite,
    context_events_sqlite,
    None::<&str>
);
root_changes_reader!(
    root_changes_postgres,
    sqlx::PgPool,
    forest_roots_postgres,
    interactions_for_roots_postgres,
    matching_in_roots_postgres,
    debug_flags_postgres,
    context_events_postgres,
    Some("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
);

macro_rules! interaction_batch_queries {
    ($db:ty, $connection:ty, $flags:ident, $context:ident, $map:ident) => {
        async fn $flags(executor: &mut $connection, ids: &[&str])
            -> anyhow::Result<Vec<(String, String, bool)>> {
            let mut query = QueryBuilder::<$db>::new(
                "SELECT interaction_id,id,debug_enabled FROM inference_run_observations WHERE interaction_id IN (");
            let mut separated = query.separated(",");
            for id in ids { separated.push_bind(*id); }
            query.push(")");
            Ok(query.build_query_as().fetch_all(executor).await?)
        }
        async fn $context(executor: &mut $connection, ids: &[&str], through: i64)
            -> anyhow::Result<Vec<ObservationEvent>> {
            let mut query = QueryBuilder::<$db>::new(
                "SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE interaction_id IN (");
            let mut separated = query.separated(",");
            for id in ids { separated.push_bind(*id); }
            query.push(") AND sequence<=").push_bind(through);
            query.push(" AND kind IN ('compaction_operation','native_compaction_associated','retained_tail_associated') ORDER BY interaction_id,sequence");
            $map(query.build().fetch_all(executor).await?)
        }
    };
}
interaction_batch_queries!(
    sqlx::Sqlite,
    sqlx::SqliteConnection,
    debug_flags_sqlite,
    context_events_sqlite,
    map_sqlite_events
);
interaction_batch_queries!(
    sqlx::Postgres,
    sqlx::PgConnection,
    debug_flags_postgres,
    context_events_postgres,
    map_postgres_events
);

#[derive(FromRow)]
struct RunRow {
    id: String,
    parent_run_id: Option<String>,
    generation_node_id: Option<String>,
    generation_parent_id: Option<String>,
    route_id: String,
    model_display_name: Option<String>,
    ingress_protocol: String,
    status: String,
    terminal_reason: Option<String>,
    user_interrupted: bool,
    debug_enabled: bool,
    client_output_committed: bool,
    started_at: i64,
    finished_at: Option<i64>,
    delivery_completed_at: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    #[sqlx(flatten)]
    coverage: UsageCoverage,
}
#[derive(FromRow)]
struct RejectionRow {
    id: String,
    occurred_at: i64,
    method: String,
    path: String,
    ingress_protocol: String,
    stage: String,
    code: String,
    status_code: i64,
    debug_enabled: bool,
    debug_status: String,
}

impl super::InteractionObservation {
    pub async fn credential_discoveries(
        &self,
        query: CredentialDiscoveryQuery,
    ) -> anyhow::Result<CredentialDiscoveryPage> {
        self.flush().await?;
        let mut page = self.inner.store.credential_discoveries(query).await?;
        page.observation_gap |= self.inner.unpersisted_gaps.lock().visible(
            chrono::Utc::now().timestamp_millis(),
            self.inner
                .retention_days
                .load(std::sync::atomic::Ordering::Acquire),
        );
        Ok(page)
    }
}

#[derive(FromRow)]
struct DiscoveryRow {
    interaction_id: String,
    api_key_name: Option<String>,
    discovered_at: i64,
    status: String,
    observation_gap: bool,
}

const DISCOVERY_SELECT: &str = "SELECT i.id AS interaction_id,i.api_key_name,MAX(e.occurred_at) AS discovered_at,i.status,i.observation_gap FROM interaction_observations i JOIN observation_events e ON e.interaction_id=i.id WHERE e.kind='credential_mappings_created' AND i.expires_at>";

impl ObservationStore {
    pub(super) async fn observation_root_id(
        &self,
        interaction: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(match self {
            Self::Sqlite(pool, _, _) => {
                sqlx::query_scalar("SELECT root_id FROM interaction_observations WHERE id=?")
                    .bind(interaction)
                    .fetch_optional(pool)
                    .await?
            }
            Self::Postgres(pool, _) => {
                sqlx::query_scalar("SELECT root_id FROM interaction_observations WHERE id=$1")
                    .bind(interaction)
                    .fetch_optional(pool)
                    .await?
            }
        })
    }

    pub(super) async fn contains_gap_run(&self, run_id: &str) -> anyhow::Result<bool> {
        Ok(match self {
            Self::Sqlite(pool, _, _) => {
                sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM inference_run_observations WHERE id=?)",
                )
                .bind(run_id)
                .fetch_one(pool)
                .await?
            }
            Self::Postgres(pool, _) => {
                sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM inference_run_observations WHERE id=$1)",
                )
                .bind(run_id)
                .fetch_one(pool)
                .await?
            }
        })
    }

    pub async fn credential_discoveries(
        &self,
        query: CredentialDiscoveryQuery,
    ) -> anyhow::Result<CredentialDiscoveryPage> {
        let cursor: Option<(i64, String)> = query
            .cursor
            .as_deref()
            .map(|cursor| {
                anyhow::ensure!(cursor.len() <= 512, "凭据发现游标过长");
                serde_json::from_str(cursor).map_err(|_| anyhow::anyhow!("凭据发现游标无效"))
            })
            .transpose()?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
        let now = chrono::Utc::now().timestamp_millis();
        let (mut rows, observation_gap): (Vec<DiscoveryRow>, bool) = match self {
            Self::Sqlite(pool, _, _) => {
                let gap: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM interaction_observations WHERE observation_gap=1 AND expires_at>?)")
                    .bind(now).fetch_one(pool).await?;
                let mut builder = QueryBuilder::<sqlx::Sqlite>::new(DISCOVERY_SELECT);
                builder
                    .push_bind(now)
                    .push(" AND e.expires_at>")
                    .push_bind(now)
                    .push(" GROUP BY i.id,i.api_key_name,i.status,i.observation_gap");
                if let Some((time, id)) = &cursor {
                    builder
                        .push(" HAVING (MAX(e.occurred_at),i.id)<(")
                        .push_bind(*time)
                        .push(",")
                        .push_bind(id)
                        .push(")");
                }
                builder
                    .push(" ORDER BY discovered_at DESC,i.id DESC LIMIT ")
                    .push_bind((limit + 1) as i64);
                (builder.build_query_as().fetch_all(pool).await?, gap)
            }
            Self::Postgres(pool, _) => {
                let gap: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM interaction_observations WHERE observation_gap=TRUE AND expires_at>$1)")
                    .bind(now).fetch_one(pool).await?;
                let mut builder = QueryBuilder::<sqlx::Postgres>::new(DISCOVERY_SELECT);
                builder
                    .push_bind(now)
                    .push(" AND e.expires_at>")
                    .push_bind(now)
                    .push(" GROUP BY i.id,i.api_key_name,i.status,i.observation_gap");
                if let Some((time, id)) = &cursor {
                    builder
                        .push(" HAVING (MAX(e.occurred_at),i.id)<(")
                        .push_bind(*time)
                        .push(",")
                        .push_bind(id)
                        .push(")");
                }
                builder
                    .push(" ORDER BY discovered_at DESC,i.id DESC LIMIT ")
                    .push_bind((limit + 1) as i64);
                (builder.build_query_as().fetch_all(pool).await?, gap)
            }
        };
        let next_cursor = if rows.len() > limit {
            let last = &rows[limit - 1];
            Some(serde_json::to_string(&(
                last.discovered_at,
                &last.interaction_id,
            ))?)
        } else {
            None
        };
        rows.truncate(limit);
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            // 只读取普通发现事件的安全元数据，不读取请求正文或秘密映射。
            let payloads: Vec<Vec<u8>> = match self {
                Self::Sqlite(pool, _, _) => sqlx::query_scalar("SELECT payload FROM observation_events WHERE interaction_id=? AND kind='credential_mappings_created' AND expires_at>? AND occurred_at<=?")
                    .bind(&row.interaction_id).bind(now).bind(row.discovered_at).fetch_all(pool).await?,
                Self::Postgres(pool, _) => sqlx::query_scalar("SELECT payload FROM observation_events WHERE interaction_id=$1 AND kind='credential_mappings_created' AND expires_at>$2 AND occurred_at<=$3")
                    .bind(&row.interaction_id).bind(now).bind(row.discovered_at).fetch_all(pool).await?,
            };
            let mut count = 0i64;
            let mut rule_ids = std::collections::BTreeSet::new();
            let mut source_types = std::collections::BTreeSet::new();
            for payload in payloads {
                #[derive(serde::Deserialize)]
                struct Payload {
                    discoveries: Vec<CredentialDiscovery>,
                }
                let payload: Payload =
                    serde_json::from_slice(&crate::storage_codec::decode(&payload)?)?;
                count += payload.discoveries.len() as i64;
                for discovery in payload.discoveries {
                    rule_ids.extend(discovery.rule_ids);
                    source_types.extend(discovery.source_types);
                }
            }
            if count > 0 {
                items.push(CredentialDiscoverySummary {
                    interaction_id: row.interaction_id,
                    api_key_name: row.api_key_name,
                    discovered_at: row.discovered_at,
                    new_credential_count: count,
                    rule_ids: rule_ids.into_iter().collect(),
                    source_types: source_types.into_iter().collect(),
                    status: row.status,
                    observation_gap: row.observation_gap,
                });
            }
        }
        Ok(CredentialDiscoveryPage {
            items,
            next_cursor,
            observation_gap,
        })
    }

    pub async fn query_root_changes(
        &self,
        query: RootChangesQuery,
    ) -> anyhow::Result<RootChangesPage> {
        let window = forest_window(&query.filters)?;
        let read = match self {
            Self::Sqlite(pool, _, _) => {
                root_changes_sqlite(pool, &query, &window, false, self.debug_trace_index()).await?
            }
            Self::Postgres(pool, _) => {
                root_changes_postgres(pool, &query, &window, false, self.debug_trace_index())
                    .await?
            }
        };
        let mut page = RootChangesPage {
            snapshot_sequence: read.sequence,
            root_total: read.total,
            reset_required: false,
            changes: Vec::new(),
        };
        let mut seen_roots = HashSet::new();
        if query.roots.iter().any(|root| {
            !seen_roots.insert(root.root_id.as_str())
                || root.after_sequence < 0
                || root.after_sequence > read.sequence
                || (!root.known_interactions.is_empty()
                    && read
                        .first
                        .is_some_and(|first| root.after_sequence < first - 1))
                || root.known_interactions.iter().any(|known| {
                    known.last_event_sequence < 0 || known.last_event_sequence > root.after_sequence
                })
        }) {
            page.reset_required = true;
            return Ok(page);
        }
        let mut rows_by_root: std::collections::HashMap<String, Vec<InteractionRow>> =
            std::collections::HashMap::new();
        for row in read.rows {
            if let Some(members) = rows_by_root.get_mut(&row.root_id) {
                members.push(row);
            } else {
                rows_by_root.insert(row.root_id.clone(), vec![row]);
            }
        }
        let matched = read.matched;
        let mut debug = read.debug;
        let mut context = group_context_events(read.events);
        for baseline in query.roots {
            let members = rows_by_root.remove(&baseline.root_id).unwrap_or_default();
            let last = members
                .iter()
                .map(|row| row.last_active_at)
                .max()
                .unwrap_or(0);
            let reason = if members.is_empty() {
                Some("deleted")
            } else if !members.iter().any(|row| matched.contains(&row.id)) {
                Some("filter")
            } else if last < window.start || (window.bounded_end && last >= window.end) {
                Some("window")
            } else {
                None
            };
            let mut change = RootChange {
                root_id: baseline.root_id,
                last_active_at: last,
                interactions: Vec::new(),
                removed_interaction_ids: Vec::new(),
                removal_reason: reason.map(str::to_owned),
            };
            for known in &baseline.known_interactions {
                if matches!(reason, Some("deleted" | "filter"))
                    || !members.iter().any(|row| row.id == known.id)
                {
                    change.removed_interaction_ids.push(known.id.clone());
                }
            }
            if reason.is_none()
                || (reason == Some("window") && !baseline.known_interactions.is_empty())
            {
                for row in members {
                    let is_matched = matched.contains(&row.id);
                    let debug_status = debug
                        .get(&row.id)
                        .copied()
                        .unwrap_or(RunDebugStatus::None)
                        .as_str();
                    let known = baseline
                        .known_interactions
                        .iter()
                        .find(|known| known.id == row.id);
                    if interaction_changed(&row, is_matched, debug_status, known) {
                        let events = context.remove(&row.id).unwrap_or_default();
                        let status = debug.remove(&row.id).unwrap_or(RunDebugStatus::None);
                        let mut interaction = summary(row, is_matched);
                        interaction.context_events = events;
                        interaction.debug_status = status.as_str().into();
                        change.interactions.push(interaction);
                    }
                }
            }
            page.changes.push(change);
        }
        Ok(page)
    }

    pub async fn query_forest(&self, q: ForestQuery) -> anyhow::Result<ForestPage> {
        let window = forest_window(&q)?;
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
        use tracing::Instrument as _;
        let span = tracing::info_span!(target: "stravia::perf", "observation.query.forest_roots", status = tracing::field::Empty);
        let query = RootChangesQuery {
            filters: q.clone(),
            roots: Vec::new(),
        };
        let result = async {
            match self {
                Self::Sqlite(p, _, _) => {
                    root_changes_sqlite(p, &query, &window, true, self.debug_trace_index()).await
                }
                Self::Postgres(p, _) => {
                    root_changes_postgres(p, &query, &window, true, self.debug_trace_index()).await
                }
            }
        }
        .instrument(span.clone())
        .await;
        span.record("status", if result.is_ok() { "completed" } else { "error" });
        drop(span);
        let read = result?;
        let root_total = read.total;
        let root_rows = read.root_rows;
        let rows = read.rows;
        let matched = read.matched;
        let mut roots: Vec<ForestRoot> = root_rows
            .iter()
            .take(limit as usize)
            .map(|(id, last)| {
                let mut interactions: Vec<_> = rows
                    .iter()
                    .filter(|row| &row.root_id == id)
                    .cloned()
                    .map(|row| {
                        let hit = matched.contains(&row.id);
                        summary(row, hit)
                    })
                    .collect();
                interactions.sort_by(|a, b| {
                    a.started_at
                        .cmp(&b.started_at)
                        .then_with(|| a.id.cmp(&b.id))
                });
                ForestRoot {
                    id: id.clone(),
                    last_active_at: *last,
                    interactions,
                }
            })
            .collect();
        let snapshot_sequence = read.sequence;
        let mut context = group_context_events(read.events);
        let mut debug = read.debug;
        for root in &mut roots {
            for interaction in &mut root.interactions {
                interaction.context_events = context.remove(&interaction.id).unwrap_or_default();
                interaction.debug_status = debug
                    .remove(&interaction.id)
                    .unwrap_or(RunDebugStatus::None)
                    .as_str()
                    .into();
            }
        }
        let next_cursor =
            (root_rows.len() > limit as usize).then(|| root_rows[limit as usize - 1].0.clone());
        Ok(ForestPage {
            anchor_at: window.anchor,
            window_index: window.index,
            window_start: window.start,
            window_end: window.end,
            roots,
            root_total,
            next_cursor,
            snapshot_sequence,
        })
    }

    pub async fn get_interaction_summary(
        &self,
        id: &str,
        filters: ForestQuery,
    ) -> anyhow::Result<Option<InteractionSnapshot>> {
        let window = forest_window(&filters)?;
        let Some(root_id) = self.observation_root_id(id).await? else {
            return Ok(None);
        };
        let query = RootChangesQuery {
            filters,
            roots: vec![RootChangesBaseline {
                root_id: root_id.clone(),
                after_sequence: 0,
                known_interactions: Vec::new(),
            }],
        };
        let read = match self {
            Self::Sqlite(p, _, _) => {
                root_changes_sqlite(p, &query, &window, false, self.debug_trace_index()).await?
            }
            Self::Postgres(p, _) => {
                root_changes_postgres(p, &query, &window, false, self.debug_trace_index()).await?
            }
        };
        let root_rows = read.rows;
        let Some(selected_row) = root_rows.iter().find(|row| row.id == id).cloned() else {
            return Ok(None);
        };
        let matched = read.matched;
        let mut selected = summary(selected_row.clone(), matched.contains(id));
        let mut root_interactions: Vec<_> = root_rows
            .into_iter()
            .map(|row| {
                let hit = matched.contains(&row.id);
                summary(row, hit)
            })
            .collect();
        let snapshot_sequence = read.sequence;
        let mut context = group_context_events(read.events);
        let mut debug = read.debug;
        for interaction in &mut root_interactions {
            interaction.context_events = context.remove(&interaction.id).unwrap_or_default();
            interaction.debug_status = debug
                .remove(&interaction.id)
                .unwrap_or(RunDebugStatus::None)
                .as_str()
                .into();
            if interaction.id == id {
                selected.context_events = interaction.context_events.clone();
                selected.debug_status = interaction.debug_status.clone();
            }
        }
        root_interactions.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let last = root_interactions
            .iter()
            .map(|i| i.last_active_at)
            .max()
            .unwrap_or(selected_row.last_active_at);
        Ok(Some(InteractionSnapshot {
            interaction: selected,
            root: ForestRoot {
                id: root_id,
                last_active_at: last,
                interactions: root_interactions,
            },
            snapshot_sequence,
        }))
    }

    pub async fn get_interaction(
        &self,
        id: &str,
        filters: ForestQuery,
    ) -> anyhow::Result<Option<InteractionDetail>> {
        // 与 interaction 行相同：run 行必须在快照序列之前读取，否则 finish_run
        // 在快照与行读取之间提交时，行状态已见的 run_finished 会被事件分页截掉。
        let runs = self.runs(id).await?;
        let Some(snapshot) = self.get_interaction_summary(id, filters).await? else {
            return Ok(None);
        };
        let snapshot_sequence = snapshot.snapshot_sequence;
        let (events, older_events_cursor) = self
            .event_page(id, &InteractionEventsQuery::default(), snapshot_sequence)
            .await?;
        let runs = self.run_details(runs, events).await?;
        Ok(Some(InteractionDetail {
            interaction: snapshot.interaction,
            root: snapshot.root,
            runs,
            snapshot_sequence,
            older_events_cursor,
        }))
    }

    async fn run_details(
        &self,
        runs: Vec<RunRow>,
        events: Vec<ObservationEvent>,
    ) -> anyhow::Result<Vec<RunDetail>> {
        let mut by_run = std::collections::HashMap::<String, Vec<ObservationEvent>>::new();
        for mut event in events {
            if let Some(run_id) = &event.run_id {
                // 落盘和 Bundle 保持原始 payload；只在管理 RunDetail 读取边界投影一次。
                if event.kind == "target_attempt_finished"
                    && let Some(usage) = event
                        .payload
                        .get_mut("usage")
                        .and_then(|value| value.as_object_mut())
                {
                    let input = usage.get("input_tokens").and_then(|value| value.as_i64());
                    let cache_read = usage
                        .get("cache_read_tokens")
                        .and_then(|value| value.as_i64());
                    usage.insert(
                        "input_tokens".into(),
                        management_input_tokens(input, cache_read).into(),
                    );
                }
                by_run.entry(run_id.clone()).or_default().push(event);
            }
        }
        let mut details = Vec::with_capacity(runs.len());
        for run in runs {
            let trace = self.manifest_for_run(&run.id).await?;
            let run_events = by_run.remove(&run.id).unwrap_or_default();
            details.push(RunDetail {
                id: run.id,
                parent_run_id: run.parent_run_id,
                generation_node_id: run.generation_node_id,
                generation_parent_id: run.generation_parent_id,
                route_id: run.route_id,
                model_display_name: run.model_display_name,
                ingress_protocol: run.ingress_protocol,
                status: run.status,
                terminal_reason: run.terminal_reason,
                user_interrupted: run.user_interrupted,
                debug_enabled: run.debug_enabled,
                client_output_committed: run.client_output_committed,
                started_at: run.started_at,
                finished_at: run.finished_at,
                delivery_completed_at: run.delivery_completed_at,
                usage: ConfirmedUsage {
                    input_tokens: run.input_tokens,
                    output_tokens: run.output_tokens,
                    cache_read_tokens: run.cache_read_tokens,
                    cache_write_tokens: run.cache_write_tokens,
                    reasoning_tokens: run.reasoning_tokens,
                    coverage: Some(run.coverage),
                },
                events: run_events,
                trace,
            });
        }
        Ok(details)
    }

    pub async fn get_interaction_events(
        &self,
        id: &str,
        query: InteractionEventsQuery,
    ) -> anyhow::Result<Option<InteractionEventsPage>> {
        // 快照前读取 run 行：through_sequence 缺省时快照上界必须不早于行状态已见的终态事件。
        let runs = self.runs(id).await?;
        let current_sequence = self.max_sequence().await?;
        let snapshot_sequence = query.through_sequence.unwrap_or(current_sequence);
        anyhow::ensure!(
            snapshot_sequence >= 0
                && snapshot_sequence <= current_sequence
                && !(query.after_sequence.is_some() && query.before_sequence.is_some())
                && query
                    .after_sequence
                    .is_none_or(|cursor| cursor >= 0 && cursor <= snapshot_sequence)
                && query
                    .before_sequence
                    .is_none_or(|cursor| cursor >= 0 && cursor <= snapshot_sequence)
                && query.limit.is_none_or(|limit| (1..=500).contains(&limit)),
            ObservationQueryError::InvalidEventPage
        );
        let exists = match self {
            Self::Sqlite(pool, _, _) => interaction_sqlite(pool, id).await?.is_some(),
            Self::Postgres(pool, _) => interaction_postgres(pool, id).await?.is_some(),
        };
        if !exists {
            return Ok(None);
        }
        let (events, next_cursor) = self.event_page(id, &query, snapshot_sequence).await?;
        Ok(Some(InteractionEventsPage {
            runs: self.run_details(runs, events).await?,
            snapshot_sequence,
            next_cursor,
        }))
    }

    async fn event_page(
        &self,
        id: &str,
        query: &InteractionEventsQuery,
        through: i64,
    ) -> anyhow::Result<(Vec<ObservationEvent>, Option<i64>)> {
        use tracing::Instrument as _;
        let limit = query.limit.unwrap_or(200) as usize;
        let forward = query.after_sequence.is_some();
        let mut events = match self {
            Self::Sqlite(pool, _, _) => {
                let mut sql = QueryBuilder::<sqlx::Sqlite>::new(
                    "SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE interaction_id=",
                );
                sql.push_bind(id).push(" AND sequence<=").push_bind(through);
                if let Some(after) = query.after_sequence {
                    sql.push(" AND sequence>").push_bind(after);
                }
                if let Some(before) = query.before_sequence {
                    sql.push(" AND sequence<").push_bind(before);
                }
                sql.push(if forward {
                    " ORDER BY sequence ASC LIMIT "
                } else {
                    " ORDER BY sequence DESC LIMIT "
                })
                .push_bind((limit + 1) as i64);
                let span = tracing::info_span!(target: "stravia::perf", "observation.query.interaction_events", status = tracing::field::Empty);
                let rows = sql.build().fetch_all(pool).instrument(span.clone()).await;
                span.record("status", if rows.is_ok() { "completed" } else { "error" });
                drop(span);
                map_sqlite_events(rows?)?
            }
            Self::Postgres(pool, _) => {
                let mut sql = QueryBuilder::<sqlx::Postgres>::new(
                    "SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE interaction_id=",
                );
                sql.push_bind(id).push(" AND sequence<=").push_bind(through);
                if let Some(after) = query.after_sequence {
                    sql.push(" AND sequence>").push_bind(after);
                }
                if let Some(before) = query.before_sequence {
                    sql.push(" AND sequence<").push_bind(before);
                }
                sql.push(if forward {
                    " ORDER BY sequence ASC LIMIT "
                } else {
                    " ORDER BY sequence DESC LIMIT "
                })
                .push_bind((limit + 1) as i64);
                let span = tracing::info_span!(target: "stravia::perf", "observation.query.interaction_events", status = tracing::field::Empty);
                let rows = sql.build().fetch_all(pool).instrument(span.clone()).await;
                span.record("status", if rows.is_ok() { "completed" } else { "error" });
                drop(span);
                map_postgres_events(rows?)?
            }
        };
        let more = events.len() > limit;
        events.truncate(limit);
        let next_cursor = if more {
            events.last().map(|event| event.sequence)
        } else {
            None
        };
        if !forward {
            events.reverse();
        }
        Ok((events, next_cursor))
    }

    pub(super) async fn bundle_interaction_records(
        &self,
        id: &str,
        through: i64,
    ) -> anyhow::Result<Option<BundleInteractionRecords>> {
        let root_id: Option<String> = match self {
            Self::Sqlite(pool, _, _) => {
                sqlx::query_scalar("SELECT root_id FROM interaction_observations WHERE id=?")
                    .bind(id)
                    .fetch_optional(pool)
                    .await?
            }
            Self::Postgres(pool, _) => {
                sqlx::query_scalar("SELECT root_id FROM interaction_observations WHERE id=$1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await?
            }
        };
        let Some(root_id) = root_id else {
            return Ok(None);
        };
        // 准入只决定截止水位前有哪些 Run；生命周期、用量和正文解释由导出模块拥有。
        let runs: Vec<(String, bool)> = match self {
            Self::Sqlite(pool, _, _) => sqlx::query_as(
                "SELECT r.id,r.debug_enabled FROM inference_run_observations r WHERE r.interaction_id=? AND EXISTS (SELECT 1 FROM observation_events e WHERE e.run_id=r.id AND e.kind='run_admitted' AND e.sequence<=?) ORDER BY r.started_at,r.id",
            )
            .bind(id)
            .bind(through)
            .fetch_all(pool)
            .await?,
            Self::Postgres(pool, _) => sqlx::query_as(
                "SELECT r.id,r.debug_enabled FROM inference_run_observations r WHERE r.interaction_id=$1 AND EXISTS (SELECT 1 FROM observation_events e WHERE e.run_id=r.id AND e.kind='run_admitted' AND e.sequence<=$2) ORDER BY r.started_at,r.id",
            )
            .bind(id)
            .bind(through)
            .fetch_all(pool)
            .await?,
        };
        let events = match self {
            Self::Sqlite(pool, _, _) => map_sqlite_events(
                sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE interaction_id=? AND sequence<=? ORDER BY sequence")
                    .bind(id).bind(through).fetch_all(pool).await?,
            )?,
            Self::Postgres(pool, _) => map_postgres_events(
                sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE interaction_id=$1 AND sequence<=$2 ORDER BY sequence")
                    .bind(id).bind(through).fetch_all(pool).await?,
            )?,
        };
        let runs = runs
            .into_iter()
            .map(|(id, debug_enabled)| BundleRunRecord {
                trace: self.debug_trace_index().for_run(&id),
                id,
                debug_enabled,
            })
            .collect();
        Ok(Some(BundleInteractionRecords {
            root_id,
            runs,
            events,
        }))
    }

    pub(super) async fn bundle_rejection_records(
        &self,
        id: &str,
        through: i64,
    ) -> anyhow::Result<Option<BundleRejectionRecords>> {
        let row: Option<(String, bool)> = match self {
            Self::Sqlite(pool, _, _) => {
                sqlx::query_as(
                    "SELECT id,debug_enabled FROM rejected_request_observations WHERE id=?",
                )
                .bind(id)
                .fetch_optional(pool)
                .await?
            }
            Self::Postgres(pool, _) => {
                sqlx::query_as(
                    "SELECT id,debug_enabled FROM rejected_request_observations WHERE id=$1",
                )
                .bind(id)
                .fetch_optional(pool)
                .await?
            }
        };
        let Some((id, debug_enabled)) = row else {
            return Ok(None);
        };
        let events = self.rejection_events(&id, through).await?;
        Ok(Some(BundleRejectionRecords {
            trace: self.debug_trace_index().for_rejection(&id),
            id,
            debug_enabled,
            events,
        }))
    }

    pub async fn query_rejections(&self, q: RejectionQuery) -> anyhow::Result<RejectionPage> {
        let QueryWindow {
            start,
            end,
            bounded_end,
            ..
        } = query_window(q.start_at, q.end_at, q.anchor_at, q.window_index)?;
        let snapshot_sequence = self.max_sequence().await?;
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
        let (total, mut rows) = match self {
            Self::Sqlite(p, _, _) => {
                rejections_sqlite(p, &q, start, end, bounded_end, limit).await?
            }
            Self::Postgres(p, _) => {
                rejections_postgres(p, &q, start, end, bounded_end, limit).await?
            }
        };
        let next_cursor =
            (rows.len() > limit as usize).then(|| rows[limit as usize - 1].id.clone());
        rows.truncate(limit as usize);
        for row in &mut rows {
            if row.debug_enabled {
                row.debug_status = self
                    .debug_trace_index()
                    .for_rejection(&row.id)
                    .map_or_else(|| "partial".into(), |trace| trace.status);
            }
        }
        Ok(RejectionPage {
            items: rows.into_iter().map(rejection_summary).collect(),
            total,
            next_cursor,
            snapshot_sequence,
        })
    }
    pub async fn get_rejection(&self, id: &str) -> anyhow::Result<Option<RejectionDetail>> {
        let row = match self {
            Self::Sqlite(p, _, _) => rejection_sqlite(p, id).await?,
            Self::Postgres(p, _) => rejection_postgres(p, id).await?,
        };
        let Some(mut row) = row else { return Ok(None) };
        if row.debug_enabled {
            row.debug_status = self
                .debug_trace_index()
                .for_rejection(&row.id)
                .map_or_else(|| "partial".into(), |trace| trace.status);
        }
        // 与 interaction 详情相同：快照序列在行读取之后获取，避免截掉行状态已见的终态事件。
        let snapshot_sequence = self.max_sequence().await?;
        let events = self.rejection_events(id, snapshot_sequence).await?;
        Ok(Some(RejectionDetail {
            rejection: rejection_summary(row),
            events,
            trace: self.manifest_for_rejection(id).await?,
            snapshot_sequence,
        }))
    }

    async fn runs(&self, id: &str) -> anyhow::Result<Vec<RunRow>> {
        match self {
            Self::Sqlite(p, _, _) => {
                let mut b = QueryBuilder::<sqlx::Sqlite>::new(RUN_SELECT);
                b.push_bind(id)
                    .push(" GROUP BY r.id ORDER BY r.started_at,r.id");
                Ok(b.build_query_as().fetch_all(p).await?)
            }
            Self::Postgres(p, _) => {
                let mut b = QueryBuilder::<sqlx::Postgres>::new(RUN_SELECT);
                b.push_bind(id)
                    .push(" GROUP BY r.id ORDER BY r.started_at,r.id");
                Ok(b.build_query_as().fetch_all(p).await?)
            }
        }
    }
    async fn manifest_for_run(&self, id: &str) -> anyhow::Result<Option<TraceManifest>> {
        Ok(self.debug_trace_index().for_run(id))
    }
    async fn manifest_for_rejection(&self, id: &str) -> anyhow::Result<Option<TraceManifest>> {
        Ok(self.debug_trace_index().for_rejection(id))
    }

    fn debug_statuses_from_runs(
        index: &super::manifest_index::DebugTraceIndex,
        runs: Vec<(String, String, bool)>,
    ) -> std::collections::HashMap<String, RunDebugStatus> {
        let mut grouped: std::collections::HashMap<String, RunDebugStatus> =
            std::collections::HashMap::new();
        for (interaction, run, enabled) in runs {
            match grouped.entry(interaction) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(if !enabled {
                        RunDebugStatus::None
                    } else if index.run_complete(&run) {
                        RunDebugStatus::Complete
                    } else {
                        RunDebugStatus::Partial
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => match *entry.get() {
                    RunDebugStatus::None if enabled => {
                        entry.insert(RunDebugStatus::Partial);
                    }
                    RunDebugStatus::Complete if !enabled || !index.run_complete(&run) => {
                        entry.insert(RunDebugStatus::Partial);
                    }
                    RunDebugStatus::None | RunDebugStatus::Partial | RunDebugStatus::Complete => {}
                },
            }
        }
        grouped
    }
    async fn rejection_events(
        &self,
        id: &str,
        through: i64,
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        match self {
            Self::Sqlite(pool, _, _) => map_sqlite_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE rejection_id=? AND sequence<=? ORDER BY sequence").bind(id).bind(through).fetch_all(pool).await?),
            Self::Postgres(pool, _) => map_postgres_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE rejection_id=$1 AND sequence<=$2 ORDER BY sequence").bind(id).bind(through).fetch_all(pool).await?),
        }
    }
}

fn add_filters_sqlite(b: &mut QueryBuilder<sqlx::Sqlite>, q: &ForestQuery) {
    if let Some(v) = &q.status {
        b.push(" AND i.status=").push_bind(v);
    }
    if let Some(v) = &q.api_key {
        b.push(" AND (i.api_key_id=")
            .push_bind(v)
            .push(" OR i.api_key_name=")
            .push_bind(v)
            .push(")");
    }
    if let Some(v) = &q.model {
        b.push(" AND EXISTS (SELECT 1 FROM model_turn_observations mt WHERE mt.interaction_id=i.id AND (mt.route_id=").push_bind(v).push(" OR mt.model_display_name=").push_bind(v).push("))");
    }
    if let Some(v) = &q.provider {
        b.push(" AND EXISTS (SELECT 1 FROM target_attempt_observations ta WHERE ta.interaction_id=i.id AND (ta.provider_id=").push_bind(v).push(" OR ta.provider_name=").push_bind(v).push("))");
    }
    add_chain_token_filter_sqlite(b, q);
}
fn add_filters_postgres(b: &mut QueryBuilder<sqlx::Postgres>, q: &ForestQuery) {
    if let Some(v) = &q.status {
        b.push(" AND i.status=").push_bind(v);
    }
    if let Some(v) = &q.api_key {
        b.push(" AND (i.api_key_id=")
            .push_bind(v)
            .push(" OR i.api_key_name=")
            .push_bind(v)
            .push(")");
    }
    if let Some(v) = &q.model {
        b.push(" AND EXISTS (SELECT 1 FROM model_turn_observations mt WHERE mt.interaction_id=i.id AND (mt.route_id=").push_bind(v).push(" OR mt.model_display_name=").push_bind(v).push("))");
    }
    if let Some(v) = &q.provider {
        b.push(" AND EXISTS (SELECT 1 FROM target_attempt_observations ta WHERE ta.interaction_id=i.id AND (ta.provider_id=").push_bind(v).push(" OR ta.provider_name=").push_bind(v).push("))");
    }
    add_chain_token_filter_postgres(b, q);
}
fn add_root_filters_sqlite(b: &mut QueryBuilder<sqlx::Sqlite>, q: &ForestQuery) {
    b.push(" AND i.root_id IN (SELECT f.root_id FROM interaction_observations f WHERE 1=1");
    if let Some(v) = &q.status {
        b.push(" AND f.status=").push_bind(v);
    }
    if let Some(v) = &q.api_key {
        b.push(" AND (f.api_key_id=")
            .push_bind(v)
            .push(" OR f.api_key_name=")
            .push_bind(v)
            .push(")");
    }
    if let Some(v) = &q.model {
        b.push(" AND EXISTS (SELECT 1 FROM model_turn_observations mt WHERE mt.interaction_id=f.id AND (mt.route_id=").push_bind(v).push(" OR mt.model_display_name=").push_bind(v).push("))");
    }
    if let Some(v) = &q.provider {
        b.push(" AND EXISTS (SELECT 1 FROM target_attempt_observations ta WHERE ta.interaction_id=f.id AND (ta.provider_id=").push_bind(v).push(" OR ta.provider_name=").push_bind(v).push("))");
    }
    b.push(")");
    add_chain_token_filter_sqlite(b, q);
}
fn add_root_filters_postgres(b: &mut QueryBuilder<sqlx::Postgres>, q: &ForestQuery) {
    b.push(" AND i.root_id IN (SELECT f.root_id FROM interaction_observations f WHERE TRUE");
    if let Some(v) = &q.status {
        b.push(" AND f.status=").push_bind(v);
    }
    if let Some(v) = &q.api_key {
        b.push(" AND (f.api_key_id=")
            .push_bind(v)
            .push(" OR f.api_key_name=")
            .push_bind(v)
            .push(")");
    }
    if let Some(v) = &q.model {
        b.push(" AND EXISTS (SELECT 1 FROM model_turn_observations mt WHERE mt.interaction_id=f.id AND (mt.route_id=").push_bind(v).push(" OR mt.model_display_name=").push_bind(v).push("))");
    }
    if let Some(v) = &q.provider {
        b.push(" AND EXISTS (SELECT 1 FROM target_attempt_observations ta WHERE ta.interaction_id=f.id AND (ta.provider_id=").push_bind(v).push(" OR ta.provider_name=").push_bind(v).push("))");
    }
    b.push(")");
    add_chain_token_filter_postgres(b, q);
}

fn add_chain_token_filter_sqlite(b: &mut QueryBuilder<sqlx::Sqlite>, q: &ForestQuery) {
    let Some(min) = q.min_tokens.filter(|value| *value > 0) else {
        return;
    };
    b.push(" AND ")
        .push(CHAIN_TOKEN_ROOTS)
        .push_bind(min)
        .push(")");
}

fn add_chain_token_filter_postgres(b: &mut QueryBuilder<sqlx::Postgres>, q: &ForestQuery) {
    let Some(min) = q.min_tokens.filter(|value| *value > 0) else {
        return;
    };
    b.push(" AND ")
        .push(CHAIN_TOKEN_ROOTS)
        .push_bind(min)
        .push(")");
}

async fn forest_roots_sqlite(
    connection: &mut sqlx::SqliteConnection,
    q: &ForestQuery,
    start: i64,
    end: i64,
    bounded_end: bool,
    limit: i64,
) -> anyhow::Result<(i64, Vec<(String, i64)>)> {
    let mut base = QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT COUNT(*) FROM (SELECT i.root_id FROM interaction_observations i WHERE 1=1",
    );
    add_root_filters_sqlite(&mut base, q);
    base.push(" GROUP BY i.root_id HAVING MAX(i.last_active_at)>=")
        .push_bind(start);
    if bounded_end {
        base.push(" AND MAX(i.last_active_at)<").push_bind(end);
    }
    base.push(") matched_roots");
    let total = base
        .build_query_scalar::<i64>()
        .fetch_one(&mut *connection)
        .await?;
    let mut page = QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT i.root_id,MAX(i.last_active_at) latest FROM interaction_observations i WHERE 1=1",
    );
    add_root_filters_sqlite(&mut page, q);
    if let Some(c) = &q.cursor {
        page.push(" AND i.root_id>").push_bind(c);
    }
    page.push(" GROUP BY i.root_id HAVING MAX(i.last_active_at)>=")
        .push_bind(start);
    if bounded_end {
        page.push(" AND MAX(i.last_active_at)<").push_bind(end);
    }
    page.push(" ORDER BY i.root_id LIMIT ").push_bind(limit + 1);
    Ok((
        total,
        page.build_query_as().fetch_all(&mut *connection).await?,
    ))
}
async fn forest_roots_postgres(
    connection: &mut sqlx::PgConnection,
    q: &ForestQuery,
    start: i64,
    end: i64,
    bounded_end: bool,
    limit: i64,
) -> anyhow::Result<(i64, Vec<(String, i64)>)> {
    let mut base = QueryBuilder::<sqlx::Postgres>::new(
        "SELECT COUNT(*) FROM (SELECT i.root_id FROM interaction_observations i WHERE TRUE",
    );
    add_root_filters_postgres(&mut base, q);
    base.push(" GROUP BY i.root_id HAVING MAX(i.last_active_at)>=")
        .push_bind(start);
    if bounded_end {
        base.push(" AND MAX(i.last_active_at)<").push_bind(end);
    }
    base.push(") matched_roots");
    let total = base
        .build_query_scalar::<i64>()
        .fetch_one(&mut *connection)
        .await?;
    let mut page = QueryBuilder::<sqlx::Postgres>::new(
        "SELECT i.root_id,MAX(i.last_active_at) latest FROM interaction_observations i WHERE TRUE",
    );
    add_root_filters_postgres(&mut page, q);
    if let Some(c) = &q.cursor {
        page.push(" AND i.root_id>").push_bind(c);
    }
    page.push(" GROUP BY i.root_id HAVING MAX(i.last_active_at)>=")
        .push_bind(start);
    if bounded_end {
        page.push(" AND MAX(i.last_active_at)<").push_bind(end);
    }
    page.push(" ORDER BY i.root_id LIMIT ").push_bind(limit + 1);
    Ok((
        total,
        page.build_query_as().fetch_all(&mut *connection).await?,
    ))
}

async fn interactions_for_roots_sqlite(
    p: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> anyhow::Result<Vec<InteractionRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut b = QueryBuilder::<sqlx::Sqlite>::new(INTERACTION_SELECT);
    b.push("WHERE i.root_id IN (");
    let mut s = b.separated(",");
    for id in ids {
        s.push_bind(id);
    }
    s.push_unseparated(") GROUP BY i.id");
    Ok(b.build_query_as().fetch_all(p).await?)
}
async fn interactions_for_roots_postgres(
    p: &mut sqlx::PgConnection,
    ids: &[String],
) -> anyhow::Result<Vec<InteractionRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut b = QueryBuilder::<sqlx::Postgres>::new(INTERACTION_SELECT);
    b.push("WHERE i.root_id IN (");
    let mut s = b.separated(",");
    for id in ids {
        s.push_bind(id);
    }
    s.push_unseparated(") GROUP BY i.id");
    Ok(b.build_query_as().fetch_all(p).await?)
}
async fn interaction_sqlite(
    p: &sqlx::SqlitePool,
    id: &str,
) -> anyhow::Result<Option<InteractionRow>> {
    let mut b = QueryBuilder::<sqlx::Sqlite>::new(INTERACTION_SELECT);
    b.push("WHERE i.id=").push_bind(id).push(" GROUP BY i.id");
    Ok(b.build_query_as().fetch_optional(p).await?)
}
async fn interaction_postgres(
    p: &sqlx::PgPool,
    id: &str,
) -> anyhow::Result<Option<InteractionRow>> {
    let mut b = QueryBuilder::<sqlx::Postgres>::new(INTERACTION_SELECT);
    b.push("WHERE i.id=").push_bind(id).push(" GROUP BY i.id");
    Ok(b.build_query_as().fetch_optional(p).await?)
}
async fn matching_in_roots_sqlite(
    p: &mut sqlx::SqliteConnection,
    q: &ForestQuery,
    ids: &[String],
) -> anyhow::Result<HashSet<String>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let mut b = QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT i.id FROM interaction_observations i WHERE i.root_id IN (",
    );
    let mut s = b.separated(",");
    for id in ids {
        s.push_bind(id);
    }
    s.push_unseparated(")");
    add_filters_sqlite(&mut b, q);
    Ok(b.build_query_scalar::<String>()
        .fetch_all(p)
        .await?
        .into_iter()
        .collect())
}
async fn matching_in_roots_postgres(
    p: &mut sqlx::PgConnection,
    q: &ForestQuery,
    ids: &[String],
) -> anyhow::Result<HashSet<String>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let mut b = QueryBuilder::<sqlx::Postgres>::new(
        "SELECT i.id FROM interaction_observations i WHERE i.root_id IN (",
    );
    let mut s = b.separated(",");
    for id in ids {
        s.push_bind(id);
    }
    s.push_unseparated(")");
    add_filters_postgres(&mut b, q);
    Ok(b.build_query_scalar::<String>()
        .fetch_all(p)
        .await?
        .into_iter()
        .collect())
}

const REJECTION_SELECT: &str = "SELECT r.id,r.occurred_at,r.method,r.path,r.ingress_protocol,r.stage,r.code,r.status_code,r.debug_enabled,CASE WHEN NOT r.debug_enabled THEN 'none' ELSE 'partial' END debug_status FROM rejected_request_observations r ";
async fn rejections_sqlite(
    p: &sqlx::SqlitePool,
    q: &RejectionQuery,
    start: i64,
    end: i64,
    bounded_end: bool,
    limit: i64,
) -> anyhow::Result<(i64, Vec<RejectionRow>)> {
    let total: i64 = if !bounded_end {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM rejected_request_observations WHERE occurred_at>=?",
        )
        .bind(start)
        .fetch_one(p)
        .await?
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM rejected_request_observations WHERE occurred_at>=? AND occurred_at<?").bind(start).bind(end).fetch_one(p).await?
    };
    let mut b = QueryBuilder::<sqlx::Sqlite>::new(REJECTION_SELECT);
    b.push("WHERE r.occurred_at>=").push_bind(start);
    if bounded_end {
        b.push(" AND r.occurred_at<").push_bind(end);
    }
    if let Some(c) = &q.cursor {
        b.push(" AND r.id>").push_bind(c);
    }
    b.push(" ORDER BY r.id LIMIT ").push_bind(limit + 1);
    Ok((total, b.build_query_as().fetch_all(p).await?))
}
async fn rejections_postgres(
    p: &sqlx::PgPool,
    q: &RejectionQuery,
    start: i64,
    end: i64,
    bounded_end: bool,
    limit: i64,
) -> anyhow::Result<(i64, Vec<RejectionRow>)> {
    let total: i64 = if !bounded_end {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM rejected_request_observations WHERE occurred_at>=$1",
        )
        .bind(start)
        .fetch_one(p)
        .await?
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM rejected_request_observations WHERE occurred_at>=$1 AND occurred_at<$2").bind(start).bind(end).fetch_one(p).await?
    };
    let mut b = QueryBuilder::<sqlx::Postgres>::new(REJECTION_SELECT);
    b.push("WHERE r.occurred_at>=").push_bind(start);
    if bounded_end {
        b.push(" AND r.occurred_at<").push_bind(end);
    }
    if let Some(c) = &q.cursor {
        b.push(" AND r.id>").push_bind(c);
    }
    b.push(" ORDER BY r.id LIMIT ").push_bind(limit + 1);
    Ok((total, b.build_query_as().fetch_all(p).await?))
}
async fn rejection_sqlite(p: &sqlx::SqlitePool, id: &str) -> anyhow::Result<Option<RejectionRow>> {
    let mut b = QueryBuilder::<sqlx::Sqlite>::new(REJECTION_SELECT);
    b.push("WHERE r.id=").push_bind(id);
    Ok(b.build_query_as().fetch_optional(p).await?)
}
async fn rejection_postgres(p: &sqlx::PgPool, id: &str) -> anyhow::Result<Option<RejectionRow>> {
    let mut b = QueryBuilder::<sqlx::Postgres>::new(REJECTION_SELECT);
    b.push("WHERE r.id=").push_bind(id);
    Ok(b.build_query_as().fetch_optional(p).await?)
}

fn summary(r: InteractionRow, matched: bool) -> InteractionSummary {
    InteractionSummary {
        context_events: Vec::new(),
        id: r.id,
        root_id: r.root_id,
        parent_interaction_id: r.parent_interaction_id,
        generation_root_id: r.generation_root_id,
        first_route_id: r.first_route_id,
        first_model_display_name: r.first_model_display_name,
        status: r.status,
        started_at: r.started_at,
        last_active_at: r.last_active_at,
        input_preview: r.input_preview,
        visible_tail: r.visible_tail,
        failed_request: r.failed_request != 0,
        client_output_delivered: r.client_output_delivered != 0,
        usage: ConfirmedUsage {
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            reasoning_tokens: r.reasoning_tokens,
            coverage: Some(r.coverage),
        },
        debug_status: r.debug_status,
        observation_gap: r.observation_gap,
        matched,
        last_event_sequence: r.last_event_sequence,
    }
}
fn rejection_summary(r: RejectionRow) -> RejectionSummary {
    RejectionSummary {
        id: r.id,
        occurred_at: r.occurred_at,
        method: r.method,
        path: r.path,
        ingress_protocol: r.ingress_protocol,
        stage: r.stage,
        code: r.code,
        status_code: u16::try_from(r.status_code).unwrap_or(500),
        debug_enabled: r.debug_enabled,
        debug_status: r.debug_status,
    }
}
fn map_sqlite_events(rows: Vec<sqlx::sqlite::SqliteRow>) -> anyhow::Result<Vec<ObservationEvent>> {
    rows.into_iter()
        .map(|r| {
            Ok(ObservationEvent {
                sequence: r.try_get(0)?,
                occurred_at: r.try_get(1)?,
                interaction_id: r.try_get(2)?,
                run_id: r.try_get(3)?,
                rejection_id: r.try_get(4)?,
                kind: r.try_get(5)?,
                payload: serde_json::from_slice(&crate::storage_codec::decode(
                    &r.try_get::<Vec<u8>, _>(6)?,
                )?)?,
            })
        })
        .collect()
}

fn map_postgres_events(rows: Vec<sqlx::postgres::PgRow>) -> anyhow::Result<Vec<ObservationEvent>> {
    rows.into_iter()
        .map(|r| {
            Ok(ObservationEvent {
                sequence: r.try_get(0)?,
                occurred_at: r.try_get(1)?,
                interaction_id: r.try_get(2)?,
                run_id: r.try_get(3)?,
                rejection_id: r.try_get(4)?,
                kind: r.try_get(5)?,
                payload: serde_json::from_slice(&crate::storage_codec::decode(
                    &r.try_get::<Vec<u8>, _>(6)?,
                )?)?,
            })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction_observation::store::Admission;

    #[tokio::test]
    async fn failed_requests_page_equal_start_times_across_record_kinds() -> anyhow::Result<()> {
        use crate::interaction_observation::store::{Admission, Rejection};

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool,
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        let at = chrono::Utc::now().timestamp_millis();
        let metadata = crate::interaction_observation::RequestMetadata::default();
        // HTTP 到达时间由真实时钟决定；仅在存储边界固定同毫秒，验证跨来源的平局顺序。
        for id in ["b", "a"] {
            let start = RunStart {
                id: id.into(),
                principal: "owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "openai".into(),
            };
            store
                .admit(Admission {
                    start: &start,
                    metadata: None,
                    interaction_id: id,
                    generation_root_id: None,
                    generation_parent_id: None,
                    has_new_user: true,
                    ingress_received_at: at,
                    parent_run_id: None,
                    parent_interaction_id: None,
                    debug_enabled: false,
                    inferred_retry: false,
                    grouping_reason: "new_root",
                    diagnostic_source_run_id: None,
                    interrupt_parent: false,
                    now: at,
                    expires_at: i64::MAX,
                })
                .await?;
            store
                .finish_run(
                    id,
                    id,
                    &RunOutcome {
                        status: "failed".into(),
                        terminal_reason: Some("historical_failure".into()),
                        delivery: None,
                        client_output_committed: false,
                        delivery_completed_at: None,
                        generation_node_id: None,
                        generation_root_id: None,
                    },
                    at + 20,
                    i64::MAX,
                )
                .await?;
            store
                .reject(Rejection {
                    ingress: &IngressStart {
                        id: id.into(),
                        method: "POST".into(),
                        path: "/v1/responses".into(),
                        protocol: "openai".into(),
                    },
                    outcome: &RejectedOutcome {
                        stage: "decode".into(),
                        code: "invalid_request".into(),
                        status_code: 400,
                        failure: None,
                    },
                    metadata: &metadata,
                    debug_enabled: false,
                    occurred_at: at,
                    expires_at: i64::MAX,
                    started_at: at,
                    duration_ms: 0,
                })
                .await?;
        }
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let page = store
                .failed_requests(FailedRequestQuery {
                    start_at: Some(at),
                    end_at: Some(at + 1),
                    cursor,
                    limit: Some(1),
                    ..Default::default()
                })
                .await?;
            assert_eq!(page.total, 4);
            let row = page
                .items
                .into_iter()
                .next()
                .expect("remaining failed request");
            if row.kind == "run" {
                assert_eq!(row.duration_ms, Some(20));
                assert_eq!(row.error.source, None);
                assert_eq!(row.error.message, None);
                assert!(row.observation_gap);
            }
            seen.push((row.kind, row.id));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            assert!(seen.len() < 4);
        }
        assert_eq!(
            seen,
            [
                ("rejection", "a"),
                ("rejection", "b"),
                ("run", "a"),
                ("run", "b")
            ]
            .map(|(kind, id)| (kind.to_owned(), id.to_owned())),
        );
        Ok(())
    }

    #[tokio::test]
    async fn interaction_summary_flags_failed_request_and_client_delivery() -> anyhow::Result<()> {
        use crate::interaction_observation::store::Admission;

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        let at = 1_000i64;
        for (id, status, reason, committed) in [
            (
                "failed-empty",
                "failed",
                Some("protocol_lossy_rejected"),
                false,
            ),
            (
                "failed-delivered",
                "failed",
                Some("protocol_lossy_rejected"),
                true,
            ),
            ("user-stopped", "user_interrupted", None, false),
            ("failed-cancel-reason", "failed", Some("cancelled"), false),
        ] {
            let start = RunStart {
                id: id.into(),
                principal: "owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "openai".into(),
            };
            store
                .admit(Admission {
                    start: &start,
                    metadata: None,
                    interaction_id: id,
                    generation_root_id: None,
                    generation_parent_id: None,
                    has_new_user: true,
                    ingress_received_at: at,
                    parent_run_id: None,
                    parent_interaction_id: None,
                    debug_enabled: false,
                    inferred_retry: false,
                    grouping_reason: "new_root",
                    diagnostic_source_run_id: None,
                    interrupt_parent: false,
                    now: at,
                    expires_at: i64::MAX,
                })
                .await?;
            if committed {
                sqlx::query(
                    "UPDATE inference_run_observations SET client_output_committed=1 WHERE id=?",
                )
                .bind(id)
                .execute(&pool)
                .await?;
            }
            store
                .finish_run(
                    id,
                    id,
                    &RunOutcome {
                        status: status.into(),
                        terminal_reason: reason.map(str::to_owned),
                        delivery: None,
                        client_output_committed: committed,
                        delivery_completed_at: None,
                        generation_node_id: None,
                        generation_root_id: None,
                    },
                    at + 10,
                    i64::MAX,
                )
                .await?;
        }
        for (id, failed_request, delivered) in [
            ("failed-empty", true, false),
            ("failed-delivered", true, true),
            ("user-stopped", false, false),
            ("failed-cancel-reason", false, false),
        ] {
            let snapshot = store
                .get_interaction_summary(id, ForestQuery::default())
                .await?
                .unwrap_or_else(|| panic!("summary for {id}"));
            assert_eq!(snapshot.interaction.failed_request, failed_request, "{id}");
            assert_eq!(
                snapshot.interaction.client_output_delivered, delivered,
                "{id}"
            );
        }
        Ok(())
    }

    async fn admit_chain_node(
        store: &ObservationStore,
        id: &str,
        root_id: &str,
        parent: Option<&str>,
        now: i64,
    ) -> anyhow::Result<()> {
        admit_chain_run(store, id, id, root_id, parent, now, false).await
    }

    async fn admit_chain_run(
        store: &ObservationStore,
        id: &str,
        run_id: &str,
        root_id: &str,
        parent: Option<&str>,
        now: i64,
        debug_enabled: bool,
    ) -> anyhow::Result<()> {
        store
            .admit(Admission {
                metadata: None,
                start: &RunStart {
                    id: run_id.into(),
                    principal: "owner".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                interaction_id: id,
                generation_root_id: (root_id != id).then_some(root_id),
                generation_parent_id: None,
                has_new_user: true,
                ingress_received_at: now,
                parent_run_id: parent,
                parent_interaction_id: parent,
                debug_enabled,
                inferred_retry: false,
                grouping_reason: if parent.is_some() {
                    "new_user"
                } else {
                    "new_root"
                },
                diagnostic_source_run_id: None,
                interrupt_parent: parent.is_some(),
                now,
                expires_at: i64::MAX,
            })
            .await?;
        Ok(())
    }

    async fn confirm_displayed_tokens(
        store: &ObservationStore,
        id: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        at: i64,
    ) -> anyhow::Result<()> {
        store
            .persist_run_event(
                id,
                id,
                &RunEvent::ModelTurnStarted {
                    model_turn_id: id.into(),
                    route_id: "route".into(),
                    model_display_name: None,
                    estimated_input_tokens: None,
                },
                at,
                i64::MAX,
            )
            .await?;
        store
            .persist_run_event(
                id,
                id,
                &RunEvent::TargetAttemptStarted {
                    model_turn_id: id.into(),
                    attempt_id: format!("{id}-a"),
                    target_id: "target".into(),
                    provider_id: "provider".into(),
                    provider_name: "provider".into(),
                    upstream_model: "model".into(),
                    protocol: "responses".into(),
                    upstream_url: "http://localhost".into(),
                },
                at,
                i64::MAX,
            )
            .await?;
        store
            .persist_run_event(
                id,
                id,
                &RunEvent::UsageConfirmed {
                    model_turn_id: id.into(),
                    attempt_id: format!("{id}-a"),
                    usage: ConfirmedUsage {
                        input_tokens: Some(input),
                        output_tokens: Some(output),
                        cache_read_tokens: Some(cache_read),
                        cache_write_tokens: Some(cache_write),
                        reasoning_tokens: None,
                        coverage: None,
                    },
                },
                at,
                i64::MAX,
            )
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn forest_estimate_includes_instructions_and_tool_definitions() -> anyhow::Result<()> {
        use stravia_runtime_contract::protocol::ir::{
            AiItem, AiRequest, MessageContent, Role, ToolSpec,
        };

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool,
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        let request = AiRequest::new(
            "swe-2",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        let mut instructions = request.clone();
        instructions.instructions = Some("system guidance ".repeat(6_000));
        let mut tools = request.clone();
        tools.tools = Some(vec![ToolSpec {
            name: "lookup".into(),
            description: Some("tool guidance ".repeat(3_000)),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "parameter guidance ".repeat(3_000)
                    }
                }
            }),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        for (id, request) in [
            ("short", request),
            ("instructions", instructions),
            ("tools", tools),
        ] {
            admit_chain_node(&store, id, id, None, 1).await?;
            store
                .persist_run_event(
                    id,
                    id,
                    &RunEvent::ModelTurnStarted {
                        model_turn_id: id.into(),
                        route_id: "route".into(),
                        model_display_name: None,
                        estimated_input_tokens: Some(
                            crate::router::selection::estimate_uncached_input_tokens(&request)
                                .try_into()?,
                        ),
                    },
                    2,
                    i64::MAX,
                )
                .await?;
        }
        let query = ForestQuery {
            start_at: Some(0),
            end_at: Some(DAY_MS),
            ..Default::default()
        };
        assert_eq!(store.query_forest(query.clone()).await?.root_total, 3);
        let filtered = store
            .query_forest(ForestQuery {
                min_tokens: Some(20_000),
                ..query
            })
            .await?;
        let ids: Vec<_> = filtered.roots.iter().map(|root| root.id.as_str()).collect();
        assert_eq!(ids, ["instructions", "tools"]);
        assert_eq!(filtered.root_total, 2);
        assert!(filtered.roots.iter().all(|root| {
            root.interactions
                .iter()
                .all(|interaction| interaction.matched && interaction.usage.input_tokens.is_none())
        }));
        Ok(())
    }

    #[tokio::test]
    async fn forest_pending_input_estimate_sqlite() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        pending_input_estimate_scenario(&ObservationStore::Sqlite(
            pool,
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        ))
        .await
    }

    #[tokio::test]
    async fn management_net_input_requires_known_cache_sqlite() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        management_known_input_scenario(&ObservationStore::Sqlite(
            pool,
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        ))
        .await
    }

    async fn management_known_input_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        admit_chain_node(store, "known-input", "known-input", None, 1).await?;
        for event in [
            RunEvent::ModelTurnStarted {
                model_turn_id: "known-input".into(),
                route_id: "route".into(),
                model_display_name: None,
                estimated_input_tokens: None,
            },
            RunEvent::TargetAttemptStarted {
                model_turn_id: "known-input".into(),
                attempt_id: "known-input-a".into(),
                target_id: "target".into(),
                provider_id: "provider".into(),
                provider_name: "provider".into(),
                upstream_model: "model".into(),
                protocol: "responses".into(),
                upstream_url: "http://localhost".into(),
            },
        ] {
            store
                .persist_run_event("known-input", "known-input", &event, 2, i64::MAX)
                .await?;
        }
        store
            .persist_run_event(
                "known-input",
                "known-input",
                &RunEvent::TargetAttemptFinished {
                    model_turn_id: "known-input".into(),
                    attempt_id: "known-input-a".into(),
                    status: "completed".into(),
                    status_code: Some(200),
                    error_code: None,
                    error: None,
                    duration_ms: 13857,
                    first_token_ms: Some(11918),
                    usage: Some(ConfirmedUsage {
                        input_tokens: Some(12528),
                        output_tokens: Some(896),
                        ..Default::default()
                    }),
                },
                3,
                i64::MAX,
            )
            .await?;
        // 已有数据库列的投影不能把真实交付时间替换成更晚的观测完成时间。
        let events = match store {
            ObservationStore::Sqlite(pool, _, _) => {
                sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=13916,finished_at=14000 WHERE id='known-input'")
                    .execute(pool).await?;
                map_sqlite_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE run_id='known-input' AND kind='target_attempt_finished'")
                    .fetch_all(pool).await?)?
            }
            ObservationStore::Postgres(pool, _) => {
                sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=13916,finished_at=14000 WHERE id='known-input'")
                    .execute(pool).await?;
                map_postgres_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE run_id='known-input' AND kind='target_attempt_finished'")
                    .fetch_all(pool).await?)?
            }
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["usage"]["input_tokens"], 12528);
        assert!(events[0].payload["usage"]["cache_read_tokens"].is_null());
        let snapshot = store
            .get_interaction_summary(
                "known-input",
                ForestQuery {
                    start_at: Some(0),
                    end_at: Some(DAY_MS),
                    min_tokens: Some(13000),
                    ..Default::default()
                },
            )
            .await?
            .unwrap();
        assert!(!snapshot.interaction.matched);
        assert_eq!(snapshot.interaction.usage.input_tokens, None);
        let coverage = snapshot.interaction.usage.coverage.unwrap();
        assert_eq!(coverage.missing_input_tokens, 1);
        assert_eq!(coverage.missing_cache_read_tokens, 1);
        let runs = store
            .run_details(store.runs("known-input").await?, events)
            .await?;
        assert_eq!(runs[0].usage.input_tokens, None);
        assert!(runs[0].events[0].payload["usage"]["input_tokens"].is_null());
        assert_eq!(
            runs[0]
                .usage
                .coverage
                .as_ref()
                .unwrap()
                .missing_input_tokens,
            1
        );
        assert_eq!(runs[0].delivery_completed_at, Some(13916));
        assert_eq!(runs[0].finished_at, Some(14000));

        for (id, cache_read, boundary) in
            [("cached-threshold", 9000, 50), ("net-threshold", 40, 110)]
        {
            admit_chain_node(store, id, id, None, 4).await?;
            confirm_displayed_tokens(store, id, 100, 50, cache_read, 9000, 5).await?;
            for (threshold, matched) in [(boundary, true), (boundary + 1, false)] {
                let filters = ForestQuery {
                    start_at: Some(0),
                    end_at: Some(DAY_MS),
                    min_tokens: Some(threshold),
                    ..Default::default()
                };
                let snapshot = store
                    .get_interaction_summary(id, filters.clone())
                    .await?
                    .unwrap();
                assert_eq!(snapshot.interaction.matched, matched);
                assert_eq!(snapshot.interaction.usage.input_tokens, Some(boundary - 50));
                let forest = store.query_forest(filters.clone()).await?;
                assert_eq!(forest.roots.iter().any(|root| root.id == id), matched);
                let changes = store
                    .query_root_changes(RootChangesQuery {
                        filters,
                        roots: vec![RootChangesBaseline {
                            root_id: id.into(),
                            after_sequence: 0,
                            known_interactions: Vec::new(),
                        }],
                    })
                    .await?;
                assert!(!changes.reset_required);
                assert_eq!(changes.root_total, forest.root_total);
                if matched {
                    assert!(changes.changes[0].interactions[0].matched);
                } else {
                    assert_eq!(changes.changes[0].removal_reason.as_deref(), Some("filter"));
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn forest_pending_input_estimate_postgres_when_configured() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("DB_URL") else {
            eprintln!("跳过 PostgreSQL 动态验证：未显式设置 DB_URL");
            return Ok(());
        };
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!(
            "stravia_obs_estimate_test_{}",
            uuid::Uuid::new_v4().simple()
        );
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
                let directory = tempfile::tempdir()?;
                let store = ObservationStore::Postgres(
                    pool.clone(),
                    std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::load(
                        directory.path(),
                    )?),
                );
                cleared_snapshot_scenario(&store).await?;
                pending_input_estimate_scenario(&store).await?;
                root_changes_scenario(&store).await?;
                batched_debug_scenario(&store).await?;
                management_known_input_scenario(&store).await
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

    async fn pending_input_estimate_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        admit_chain_node(store, "chain", "chain", None, 1).await?;
        confirm_displayed_tokens(store, "chain", 4_000, 0, 0, 0, 2).await?;
        admit_chain_node(store, "child", "chain", Some("chain"), 3).await?;
        admit_chain_node(store, "pending", "pending", None, 3).await?;
        admit_chain_node(store, "legacy", "legacy", None, 3).await?;
        for (id, estimate) in [
            ("child", Some(6_000)),
            ("pending", Some(12_000)),
            ("legacy", None),
        ] {
            store
                .persist_run_event(
                    id,
                    id,
                    &RunEvent::ModelTurnStarted {
                        model_turn_id: id.into(),
                        route_id: "route".into(),
                        model_display_name: None,
                        estimated_input_tokens: estimate,
                    },
                    4,
                    i64::MAX,
                )
                .await?;
        }
        let query = ForestQuery {
            start_at: Some(0),
            end_at: Some(DAY_MS),
            min_tokens: Some(10_000),
            limit: Some(1),
            ..Default::default()
        };
        let first = store.query_forest(query.clone()).await?;
        assert_eq!(first.root_total, 2);
        assert_eq!(first.roots[0].id, "chain");
        let second = store
            .query_forest(ForestQuery {
                cursor: first.next_cursor,
                ..query.clone()
            })
            .await?;
        assert_eq!(second.roots[0].id, "pending");
        assert!(second.next_cursor.is_none());
        let pending = store
            .get_interaction_summary("pending", query.clone())
            .await?
            .expect("pending interaction");
        assert!(pending.root.interactions[0].matched);
        assert_eq!(pending.root.interactions[0].usage.input_tokens, None);

        // 一个 Model Turn 的重试不能把输入估算加两遍。
        for attempt_id in ["pending-a", "pending-retry"] {
            store
                .persist_run_event(
                    "pending",
                    "pending",
                    &RunEvent::TargetAttemptStarted {
                        model_turn_id: "pending".into(),
                        attempt_id: attempt_id.into(),
                        target_id: "target".into(),
                        provider_id: "provider".into(),
                        provider_name: "provider".into(),
                        upstream_model: "model".into(),
                        protocol: "responses".into(),
                        upstream_url: "http://localhost".into(),
                    },
                    5,
                    i64::MAX,
                )
                .await?;
        }
        assert_eq!(store.query_forest(query.clone()).await?.root_total, 2);
        assert_eq!(
            store
                .query_forest(ForestQuery {
                    min_tokens: Some(20_000),
                    ..query.clone()
                })
                .await?
                .root_total,
            0
        );

        // 明确报告零也必须替换估算；无需等待 Model Turn 或 Run 结束。
        store
            .persist_run_event(
                "pending",
                "pending",
                &RunEvent::UsageConfirmed {
                    model_turn_id: "pending".into(),
                    attempt_id: "pending-a".into(),
                    usage: ConfirmedUsage {
                        input_tokens: Some(0),
                        output_tokens: Some(0),
                        cache_read_tokens: Some(0),
                        cache_write_tokens: Some(0),
                        ..Default::default()
                    },
                },
                6,
                i64::MAX,
            )
            .await?;
        let page = store.query_forest(query.clone()).await?;
        assert_eq!(page.root_total, 1);
        assert_eq!(page.roots[0].id, "chain");
        let pending = store
            .get_interaction_summary("pending", query.clone())
            .await?
            .expect("pending interaction remains readable");
        assert!(!pending.root.interactions[0].matched);
        assert_eq!(pending.root.interactions[0].usage.input_tokens, Some(0));

        // 终态未报告用量不能永久保留临时估算。
        store
            .persist_run_event(
                "child",
                "child",
                &RunEvent::ModelTurnFinished {
                    model_turn_id: "child".into(),
                    status: "failed".into(),
                },
                7,
                i64::MAX,
            )
            .await?;
        assert_eq!(store.query_forest(query.clone()).await?.root_total, 0);

        // 后续真实用量达到阈值时重新匹配，不依赖最初估算。
        store
            .persist_run_event(
                "pending",
                "pending",
                &RunEvent::UsageConfirmed {
                    model_turn_id: "pending".into(),
                    attempt_id: "pending-retry".into(),
                    usage: ConfirmedUsage {
                        input_tokens: Some(10_000),
                        output_tokens: Some(0),
                        cache_read_tokens: Some(0),
                        cache_write_tokens: Some(0),
                        ..Default::default()
                    },
                },
                8,
                i64::MAX,
            )
            .await?;
        let page = store.query_forest(query).await?;
        assert_eq!(page.root_total, 1);
        assert_eq!(page.roots[0].id, "pending");
        Ok(())
    }

    #[tokio::test]
    async fn batched_debug_status_tracks_final_manifest_and_clear() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let index = std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::load(
            directory.path(),
        )?);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool.clone(),
            index.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        cleared_snapshot_scenario(&store).await?;
        batched_debug_scenario(&store).await
    }

    async fn cleared_snapshot_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        admit_chain_node(store, "clear-root", "clear-root", None, 1).await?;
        store
            .finish_run(
                "clear-root",
                "clear-root",
                &RunOutcome {
                    status: "completed".into(),
                    terminal_reason: None,
                    delivery: None,
                    client_output_committed: false,
                    delivery_completed_at: None,
                    generation_node_id: None,
                    generation_root_id: None,
                },
                2,
                i64::MAX,
            )
            .await?;
        let filters = ForestQuery {
            start_at: Some(0),
            end_at: Some(DAY_MS),
            ..Default::default()
        };
        let before = store.query_forest(filters.clone()).await?;
        store.mark_clear_tombstones().await?;
        store.purge_clear_rows().await?;
        let expired = store
            .query_root_changes(RootChangesQuery {
                filters: filters.clone(),
                roots: vec![RootChangesBaseline {
                    root_id: "clear-root".into(),
                    after_sequence: before.snapshot_sequence,
                    known_interactions: Vec::new(),
                }],
            })
            .await?;
        assert!(expired.reset_required);
        let empty = store.query_forest(filters.clone()).await?;
        assert_eq!(empty.snapshot_sequence, 0);
        assert_eq!(empty.root_total, 0);
        let recovered = store
            .query_root_changes(RootChangesQuery {
                filters,
                roots: vec![RootChangesBaseline {
                    root_id: "clear-root".into(),
                    after_sequence: empty.snapshot_sequence,
                    known_interactions: Vec::new(),
                }],
            })
            .await?;
        assert!(!recovered.reset_required);
        assert_eq!(
            recovered.changes[0].removal_reason.as_deref(),
            Some("deleted")
        );
        Ok(())
    }

    async fn batched_debug_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        for n in 0..903 {
            let id = format!("debug-node-{n:04}");
            admit_chain_run(
                store,
                &id,
                &id,
                "debug-node-0000",
                (n > 0).then_some("debug-node-0000"),
                1,
                n != 0,
            )
            .await?;
            store
                .persist_run_event(
                    &id,
                    &id,
                    &RunEvent::NativeCompactionAssociated {
                        source_generation_id: None,
                        source_operation_id: None,
                        registration_id: id.clone(),
                    },
                    2,
                    i64::MAX,
                )
                .await?;
        }
        let filters = ForestQuery {
            start_at: Some(0),
            end_at: Some(DAY_MS),
            ..Default::default()
        };
        let snapshot = store.query_forest(filters.clone()).await?;
        let root = snapshot
            .roots
            .iter()
            .find(|root| root.id == "debug-node-0000")
            .expect("debug root");
        for item in &root.interactions {
            assert_eq!(
                item.context_events
                    .iter()
                    .map(|event| event.payload["registration_id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec![item.id.as_str()]
            );
            assert!(
                item.context_events
                    .iter()
                    .all(|event| event.sequence <= snapshot.snapshot_sequence
                        && event.interaction_id.as_deref() == Some(item.id.as_str()))
            );
        }
        assert_eq!(
            root.interactions
                .iter()
                .find(|item| item.id == "debug-node-0000")
                .unwrap()
                .debug_status,
            "none"
        );
        assert_eq!(
            root.interactions
                .iter()
                .find(|item| item.id == "debug-node-0900")
                .unwrap()
                .debug_status,
            "partial"
        );
        for (trace_index, n) in [899, 900, 901, 902].into_iter().enumerate() {
            let id = format!("debug-node-{n:04}");
            let manifest = TraceManifest {
                trace_id: char::from(b'a' + trace_index as u8)
                    .to_string()
                    .repeat(stravia_runtime_contract::identifier::ID_LEN),
                enabled: true,
                status: "complete".into(),
                bytes_written: 10,
                event_count: 1,
                reasons: Vec::new(),
            };
            store
                .debug_trace_index()
                .save_manifest(Some(&id), None, &manifest, 1, i64::MAX, true)
                .await?;
        }
        admit_chain_run(
            store,
            "debug-node-0901",
            "debug-disabled-run",
            "debug-node-0000",
            Some("debug-node-0000"),
            2,
            false,
        )
        .await?;
        let page = store
            .query_root_changes(RootChangesQuery {
                filters: filters.clone(),
                roots: vec![RootChangesBaseline {
                    root_id: root.id.clone(),
                    after_sequence: snapshot.snapshot_sequence,
                    known_interactions: root
                        .interactions
                        .iter()
                        .map(|item| KnownInteraction {
                            id: item.id.clone(),
                            last_event_sequence: item.last_event_sequence,
                            matched: item.matched,
                            debug_status: item.debug_status.clone(),
                        })
                        .collect(),
                }],
            })
            .await?;
        assert!(!page.reset_required);
        for id in ["debug-node-0899", "debug-node-0900", "debug-node-0902"] {
            let item = page.changes[0]
                .interactions
                .iter()
                .find(|item| item.id == id)
                .unwrap();
            assert_eq!(
                item.context_events
                    .iter()
                    .map(|event| event.payload["registration_id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec![id]
            );
            assert_eq!(
                page.changes[0]
                    .interactions
                    .iter()
                    .find(|item| item.id == id)
                    .unwrap()
                    .debug_status,
                "complete"
            );
        }
        assert_eq!(
            page.changes[0]
                .interactions
                .iter()
                .find(|item| item.id == "debug-node-0901")
                .unwrap()
                .debug_status,
            "partial"
        );
        let detail = store
            .get_interaction("debug-node-0902", filters.clone())
            .await?
            .expect("debug detail");
        assert_eq!(detail.interaction.debug_status, "complete");
        let tombstones = store
            .debug_trace_index()
            .mark_all_debug_tombstones()
            .await?;
        store
            .debug_trace_index()
            .delete_manifests(&tombstones)
            .await?;
        let detail = store
            .get_interaction("debug-node-0902", filters)
            .await?
            .expect("debug detail");
        assert_eq!(detail.interaction.debug_status, "partial");
        assert_eq!(
            detail
                .root
                .interactions
                .iter()
                .find(|item| item.id == "debug-node-0900")
                .unwrap()
                .debug_status,
            "partial"
        );
        assert_eq!(
            detail
                .root
                .interactions
                .iter()
                .find(|item| item.id == "debug-node-0000")
                .unwrap()
                .debug_status,
            "none"
        );
        Ok(())
    }

    #[tokio::test]
    async fn root_changes_omit_unchanged_siblings_and_update_token_membership() -> anyhow::Result<()>
    {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool,
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        root_changes_scenario(&store).await
    }

    async fn root_changes_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        admit_chain_node(store, "delta-root", "delta-root", None, 1).await?;
        admit_chain_node(store, "delta-child", "delta-root", Some("delta-root"), 2).await?;
        let filters = ForestQuery {
            start_at: Some(0),
            end_at: Some(DAY_MS),
            ..Default::default()
        };
        let snapshot = store.query_forest(filters.clone()).await?;
        let baseline = RootChangesBaseline {
            root_id: "delta-root".into(),
            after_sequence: snapshot.snapshot_sequence,
            known_interactions: snapshot
                .roots
                .iter()
                .find(|root| root.id == "delta-root")
                .expect("delta root")
                .interactions
                .iter()
                .map(|item| KnownInteraction {
                    id: item.id.clone(),
                    last_event_sequence: item.last_event_sequence,
                    matched: item.matched,
                    debug_status: item.debug_status.clone(),
                })
                .collect(),
        };
        confirm_displayed_tokens(store, "delta-child", 3_000, 2_000, 0, 0, 3).await?;
        let page = store
            .query_root_changes(RootChangesQuery {
                filters: filters.clone(),
                roots: vec![baseline.clone()],
            })
            .await?;
        assert!(!page.reset_required);
        assert_eq!(
            page.changes[0]
                .interactions
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["delta-child"]
        );
        let mut nonmatching = baseline;
        for known in &mut nonmatching.known_interactions {
            known.matched = false;
        }
        let page = store
            .query_root_changes(RootChangesQuery {
                filters: ForestQuery {
                    min_tokens: Some(5_000),
                    ..filters
                },
                roots: vec![nonmatching],
            })
            .await?;
        assert!(!page.reset_required);
        assert!(page.root_total >= 1);
        assert_eq!(page.changes[0].interactions.len(), 2);
        assert!(page.changes[0].interactions.iter().all(|item| item.matched));
        let page = store
            .query_root_changes(RootChangesQuery {
                filters: ForestQuery {
                    start_at: Some(10),
                    end_at: Some(20),
                    ..Default::default()
                },
                roots: vec![RootChangesBaseline {
                    root_id: "delta-root".into(),
                    after_sequence: page.snapshot_sequence,
                    known_interactions: Vec::new(),
                }],
            })
            .await?;
        assert_eq!(page.changes[0].removal_reason.as_deref(), Some("window"));
        let page = store
            .query_root_changes(RootChangesQuery {
                filters: ForestQuery {
                    start_at: Some(0),
                    end_at: Some(DAY_MS),
                    min_tokens: Some(i64::MAX),
                    ..Default::default()
                },
                roots: vec![RootChangesBaseline {
                    root_id: "delta-root".into(),
                    after_sequence: page.snapshot_sequence,
                    known_interactions: Vec::new(),
                }],
            })
            .await?;
        assert_eq!(page.changes[0].removal_reason.as_deref(), Some("filter"));
        let page = store
            .query_root_changes(RootChangesQuery {
                filters: ForestQuery::default(),
                roots: vec![RootChangesBaseline {
                    root_id: "missing".into(),
                    after_sequence: page.snapshot_sequence,
                    known_interactions: Vec::new(),
                }],
            })
            .await?;
        assert_eq!(page.changes[0].removal_reason.as_deref(), Some("deleted"));
        let page = store
            .query_root_changes(RootChangesQuery {
                filters: ForestQuery::default(),
                roots: vec![RootChangesBaseline {
                    root_id: "delta-root".into(),
                    after_sequence: page.snapshot_sequence + 1,
                    known_interactions: Vec::new(),
                }],
            })
            .await?;
        assert!(page.reset_required);
        assert!(page.changes.is_empty());
        historical_window_scenario(store).await
    }

    async fn historical_window_scenario(store: &ObservationStore) -> anyhow::Result<()> {
        admit_chain_node(store, "window-root", "window-root", None, 10).await?;
        admit_chain_node(
            store,
            "window-child",
            "window-root",
            Some("window-root"),
            11,
        )
        .await?;
        let filters = ForestQuery {
            start_at: Some(0),
            end_at: Some(100),
            ..Default::default()
        };
        let snapshot = store.query_forest(filters.clone()).await?;
        let root = snapshot
            .roots
            .iter()
            .find(|root| root.id == "window-root")
            .expect("historical root");
        let baseline = RootChangesBaseline {
            root_id: root.id.clone(),
            after_sequence: snapshot.snapshot_sequence,
            known_interactions: root
                .interactions
                .iter()
                .map(|item| KnownInteraction {
                    id: item.id.clone(),
                    last_event_sequence: item.last_event_sequence,
                    matched: item.matched,
                    debug_status: item.debug_status.clone(),
                })
                .collect(),
        };
        store
            .finish_run(
                "window-child",
                "window-child",
                &RunOutcome {
                    status: "completed".into(),
                    terminal_reason: None,
                    delivery: None,
                    client_output_committed: false,
                    delivery_completed_at: None,
                    generation_node_id: None,
                    generation_root_id: None,
                },
                150,
                i64::MAX,
            )
            .await?;
        let historical = store
            .query_root_changes(RootChangesQuery {
                filters: filters.clone(),
                roots: vec![baseline.clone()],
            })
            .await?;
        assert!(!historical.reset_required);
        assert_eq!(
            historical.changes[0].removal_reason.as_deref(),
            Some("window")
        );
        assert!(historical.changes[0].removed_interaction_ids.is_empty());
        assert_eq!(
            historical.changes[0]
                .interactions
                .iter()
                .find(|item| item.id == "window-child")
                .unwrap()
                .status,
            "completed"
        );
        let live_filters = ForestQuery {
            live_window: true,
            ..filters.clone()
        };
        let live = store
            .query_root_changes(RootChangesQuery {
                filters: live_filters.clone(),
                roots: vec![baseline.clone()],
            })
            .await?;
        assert!(!live.reset_required);
        assert_eq!(live.changes[0].removal_reason, None);
        assert_eq!(
            live.changes[0]
                .interactions
                .iter()
                .find(|item| item.id == "window-child")
                .unwrap()
                .status,
            "completed"
        );
        assert!(
            store
                .query_forest(live_filters)
                .await?
                .roots
                .iter()
                .any(|root| root.id == "window-root")
        );
        assert!(
            !store
                .query_forest(filters.clone())
                .await?
                .roots
                .iter()
                .any(|root| root.id == "window-root")
        );
        let filtered = store
            .query_root_changes(RootChangesQuery {
                filters: ForestQuery {
                    status: Some("missing-status".into()),
                    ..filters
                },
                roots: vec![baseline],
            })
            .await?;
        assert_eq!(
            filtered.changes[0].removal_reason.as_deref(),
            Some("filter")
        );
        assert!(filtered.changes[0].interactions.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn forest_hides_roots_below_chain_token_total() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool,
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        admit_chain_node(&store, "small", "small", None, 1).await?;
        confirm_displayed_tokens(&store, "small", 1_000, 1_000, 0, 0, 2).await?;
        admit_chain_node(&store, "large", "large", None, 1).await?;
        confirm_displayed_tokens(&store, "large", 9_000, 2_000, 0, 0, 2).await?;
        admit_chain_node(&store, "split", "split", None, 1).await?;
        confirm_displayed_tokens(&store, "split", 3_000, 2_000, 0, 0, 2).await?;
        admit_chain_node(&store, "split-child", "split", Some("split"), 3).await?;
        confirm_displayed_tokens(&store, "split-child", 4_000, 2_000, 0, 0, 4).await?;

        let window = ForestQuery {
            start_at: Some(0),
            end_at: Some(DAY_MS),
            ..Default::default()
        };
        let unfiltered = store.query_forest(window.clone()).await?;
        assert_eq!(unfiltered.root_total, 3);

        let filtered = store
            .query_forest(ForestQuery {
                min_tokens: Some(10_000),
                ..window.clone()
            })
            .await?;
        let mut ids: Vec<_> = filtered.roots.iter().map(|root| root.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["large", "split"]);
        assert_eq!(filtered.root_total, 2);
        let split = filtered
            .roots
            .iter()
            .find(|root| root.id == "split")
            .expect("split chain");
        assert_eq!(split.interactions.len(), 2);

        let stricter = store
            .query_forest(ForestQuery {
                min_tokens: Some(12_000),
                ..window.clone()
            })
            .await?;
        assert!(stricter.roots.is_empty());
        assert_eq!(stricter.root_total, 0);

        let snapshot = store
            .get_interaction_summary(
                "small",
                ForestQuery {
                    min_tokens: Some(10_000),
                    ..window
                },
            )
            .await?
            .expect("small root still readable");
        assert!(snapshot.root.interactions.iter().all(|item| !item.matched));
        Ok(())
    }

    #[tokio::test]
    async fn forest_chain_token_filter_paginates_and_counts_over_all_matching_roots()
    -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = ObservationStore::Sqlite(
            pool,
            std::sync::Arc::new(super::super::manifest_index::DebugTraceIndex::empty(
                std::path::Path::new(""),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        for index in 0..3 {
            let id = format!("root-{index}");
            admit_chain_node(&store, &id, &id, None, 1).await?;
            confirm_displayed_tokens(&store, &id, 20_000, 0, 0, 0, 2).await?;
        }
        admit_chain_node(&store, "root-tiny", "root-tiny", None, 1).await?;
        confirm_displayed_tokens(&store, "root-tiny", 10, 0, 0, 0, 2).await?;
        admit_chain_node(&store, "root-empty", "root-empty", None, 1).await?;

        let base = ForestQuery {
            start_at: Some(0),
            end_at: Some(DAY_MS),
            min_tokens: Some(10_000),
            ..Default::default()
        };
        let first = store
            .query_forest(ForestQuery {
                limit: Some(2),
                ..base.clone()
            })
            .await?;
        let first_ids: Vec<_> = first.roots.iter().map(|root| root.id.as_str()).collect();
        assert_eq!(first_ids, ["root-0", "root-1"]);
        assert_eq!(first.root_total, 3);
        let cursor = first.next_cursor.clone().expect("more matching roots");

        let second = store
            .query_forest(ForestQuery {
                limit: Some(2),
                cursor: Some(cursor),
                ..base.clone()
            })
            .await?;
        let second_ids: Vec<_> = second.roots.iter().map(|root| root.id.as_str()).collect();
        assert_eq!(second_ids, ["root-2"]);
        assert_eq!(second.root_total, 3);
        assert_eq!(second.next_cursor, None);

        // min_tokens=0 不拼接聚合：没有任何 attempt 的 root-empty 必须仍在结果中。
        let disabled = store
            .query_forest(ForestQuery {
                min_tokens: Some(0),
                ..base
            })
            .await?;
        assert_eq!(disabled.root_total, 5);
        assert!(
            disabled
                .roots
                .iter()
                .any(|root| root.id == "root-empty" && root.interactions.len() == 1)
        );
        Ok(())
    }
}
