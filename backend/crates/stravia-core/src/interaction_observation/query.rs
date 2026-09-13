use std::collections::HashSet;

use sqlx::{FromRow, QueryBuilder, Row};

use super::{store::ObservationStore, types::*};

const DAY_MS: i64 = 86_400_000;
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;
const INTERACTION_SELECT: &str = "SELECT i.id,i.root_id,i.parent_interaction_id,i.generation_root_id,i.first_route_id,i.first_model_display_name,i.status,i.started_at,i.last_active_at,i.input_preview,i.visible_tail,i.input_tokens,i.output_tokens,i.cache_read_tokens,i.cache_write_tokens,i.reasoning_tokens,i.observation_gap,i.last_event_sequence,CASE WHEN SUM(CASE WHEN r.debug_enabled THEN 1 ELSE 0 END)=0 THEN 'none' WHEN SUM(CASE WHEN r.debug_enabled THEN 1 ELSE 0 END)=COUNT(*) AND COUNT(m.trace_id)=COUNT(*) AND SUM(CASE WHEN m.status='complete' THEN 1 ELSE 0 END)=COUNT(*) THEN 'complete' ELSE 'partial' END debug_status FROM interaction_observations i JOIN inference_run_observations r ON r.interaction_id=i.id LEFT JOIN debug_trace_manifests m ON m.run_id=r.id ";
// PostgreSQL promotes SUM(BIGINT) to NUMERIC; keep the public usage contract i64.
const RUN_SELECT: &str = "SELECT r.id,r.parent_run_id,r.generation_node_id,r.generation_parent_id,r.route_id,r.model_display_name,r.ingress_protocol,r.status,r.terminal_reason,r.user_interrupted,r.debug_enabled,r.client_output_committed,r.started_at,r.finished_at,CASE WHEN COUNT(a.id)=COUNT(a.input_tokens) THEN CAST(SUM(a.input_tokens) AS BIGINT) END input_tokens,CASE WHEN COUNT(a.id)=COUNT(a.output_tokens) THEN CAST(SUM(a.output_tokens) AS BIGINT) END output_tokens,CASE WHEN COUNT(a.id)=COUNT(a.cache_read_tokens) THEN CAST(SUM(a.cache_read_tokens) AS BIGINT) END cache_read_tokens,CASE WHEN COUNT(a.id)=COUNT(a.cache_write_tokens) THEN CAST(SUM(a.cache_write_tokens) AS BIGINT) END cache_write_tokens,CASE WHEN COUNT(a.id)=COUNT(a.reasoning_tokens) THEN CAST(SUM(a.reasoning_tokens) AS BIGINT) END reasoning_tokens FROM inference_run_observations r LEFT JOIN target_attempt_observations a ON a.run_id=r.id WHERE r.interaction_id=";

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
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    observation_gap: bool,
    last_event_sequence: i64,
    debug_status: String,
}
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
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
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
#[derive(FromRow)]
struct ManifestRow {
    trace_id: String,
    status: String,
    bytes_written: i64,
    event_count: i64,
    partial_reason: Option<String>,
}

impl super::InteractionObservation {
    pub async fn credential_discoveries(
        &self,
        query: CredentialDiscoveryQuery,
    ) -> anyhow::Result<CredentialDiscoveryPage> {
        self.flush().await?;
        let mut page = self.inner.store.credential_discoveries(query).await?;
        page.observation_gap |= self
            .inner
            .unpersisted_gaps
            .lock()
            .expect("observation gaps")
            .visible(
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
    pub(super) async fn contains_gap_run(&self, run_id: &str) -> anyhow::Result<bool> {
        Ok(match self {
            Self::Sqlite(pool) => {
                sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM inference_run_observations WHERE id=?)",
                )
                .bind(run_id)
                .fetch_one(pool)
                .await?
            }
            Self::Postgres(pool) => {
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
            Self::Sqlite(pool) => {
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
            Self::Postgres(pool) => {
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
            let payloads: Vec<String> = match self {
                Self::Sqlite(pool) => sqlx::query_scalar("SELECT payload FROM observation_events WHERE interaction_id=? AND kind='credential_mappings_created' AND expires_at>? AND occurred_at<=?")
                    .bind(&row.interaction_id).bind(now).bind(row.discovered_at).fetch_all(pool).await?,
                Self::Postgres(pool) => sqlx::query_scalar("SELECT payload::text FROM observation_events WHERE interaction_id=$1 AND kind='credential_mappings_created' AND expires_at>$2 AND occurred_at<=$3")
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
                let payload: Payload = serde_json::from_value(super::codec::decode_payload(
                    serde_json::from_str(&payload)?,
                )?)?;
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

    async fn context_events(
        &self,
        interactions: &[&str],
        through: i64,
    ) -> anyhow::Result<std::collections::HashMap<String, Vec<ObservationEvent>>> {
        let mut grouped: std::collections::HashMap<String, Vec<ObservationEvent>> =
            std::collections::HashMap::new();
        // Leave room for the sequence bound under SQLite's historical 999-variable limit.
        for ids in interactions.chunks(900) {
            let events = match self {
                Self::Sqlite(pool) => {
                    let mut query = QueryBuilder::<sqlx::Sqlite>::new(
                        "SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE interaction_id IN (",
                    );
                    let mut separated = query.separated(",");
                    for id in ids {
                        separated.push_bind(*id);
                    }
                    query.push(") AND sequence<=").push_bind(through);
                    query.push(" AND kind IN ('compaction_operation','native_compaction_associated','retained_tail_associated') ORDER BY interaction_id,sequence");
                    map_sqlite_events(query.build().fetch_all(pool).await?)?
                }
                Self::Postgres(pool) => {
                    let mut query = QueryBuilder::<sqlx::Postgres>::new(
                        "SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload::text FROM observation_events WHERE interaction_id IN (",
                    );
                    let mut separated = query.separated(",");
                    for id in ids {
                        separated.push_bind(*id);
                    }
                    query.push(") AND sequence<=").push_bind(through);
                    query.push(" AND kind IN ('compaction_operation','native_compaction_associated','retained_tail_associated') ORDER BY interaction_id,sequence");
                    map_postgres_events(query.build().fetch_all(pool).await?)?
                }
            };
            for event in events {
                if let Some(id) = &event.interaction_id {
                    grouped.entry(id.clone()).or_default().push(event);
                }
            }
        }
        Ok(grouped)
    }

    pub async fn query_forest(&self, q: ForestQuery) -> anyhow::Result<ForestPage> {
        let QueryWindow {
            anchor,
            index,
            start,
            end,
            bounded_end,
        } = query_window(q.start_at, q.end_at, q.anchor_at, q.window_index)?;
        let snapshot_sequence = self.max_sequence().await?;
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
        let (root_total, root_rows) = match self {
            Self::Sqlite(p) => forest_roots_sqlite(p, &q, start, end, bounded_end, limit).await?,
            Self::Postgres(p) => {
                forest_roots_postgres(p, &q, start, end, bounded_end, limit).await?
            }
        };
        let root_ids: Vec<String> = root_rows
            .iter()
            .take(limit as usize)
            .map(|(id, _)| id.clone())
            .collect();
        let rows = match self {
            Self::Sqlite(p) => interactions_for_roots_sqlite(p, &root_ids).await?,
            Self::Postgres(p) => interactions_for_roots_postgres(p, &root_ids).await?,
        };
        let matched = match self {
            Self::Sqlite(p) => matching_in_roots_sqlite(p, &q, &root_ids).await?,
            Self::Postgres(p) => matching_in_roots_postgres(p, &q, &root_ids).await?,
        };
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
        let ids: Vec<_> = roots
            .iter()
            .flat_map(|root| &root.interactions)
            .map(|interaction| interaction.id.as_str())
            .collect();
        let mut context = self.context_events(&ids, snapshot_sequence).await?;
        for root in &mut roots {
            for interaction in &mut root.interactions {
                interaction.context_events = context.remove(&interaction.id).unwrap_or_default();
            }
        }
        let next_cursor =
            (root_rows.len() > limit as usize).then(|| root_rows[limit as usize - 1].0.clone());
        Ok(ForestPage {
            anchor_at: anchor,
            window_index: index,
            window_start: start,
            window_end: end,
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
        query_window(
            filters.start_at,
            filters.end_at,
            filters.anchor_at,
            filters.window_index,
        )?;
        let snapshot_sequence = self.max_sequence().await?;
        let Some(selected_row) = (match self {
            Self::Sqlite(p) => interaction_sqlite(p, id).await?,
            Self::Postgres(p) => interaction_postgres(p, id).await?,
        }) else {
            return Ok(None);
        };
        let root_id = selected_row.root_id.clone();
        let root_ids = [root_id.clone()];
        let root_rows = match self {
            Self::Sqlite(p) => interactions_for_roots_sqlite(p, &root_ids).await?,
            Self::Postgres(p) => interactions_for_roots_postgres(p, &root_ids).await?,
        };
        let matched = match self {
            Self::Sqlite(p) => matching_in_roots_sqlite(p, &filters, &root_ids).await?,
            Self::Postgres(p) => matching_in_roots_postgres(p, &filters, &root_ids).await?,
        };
        let mut selected = summary(selected_row.clone(), matched.contains(id));
        let mut root_interactions: Vec<_> = root_rows
            .into_iter()
            .map(|row| {
                let hit = matched.contains(&row.id);
                summary(row, hit)
            })
            .collect();
        let ids: Vec<_> = root_interactions
            .iter()
            .map(|interaction| interaction.id.as_str())
            .collect();
        let mut context = self.context_events(&ids, snapshot_sequence).await?;
        for interaction in &mut root_interactions {
            interaction.context_events = context.remove(&interaction.id).unwrap_or_default();
            if interaction.id == id {
                selected.context_events = interaction.context_events.clone();
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
        let Some(snapshot) = self.get_interaction_summary(id, filters).await? else {
            return Ok(None);
        };
        let snapshot_sequence = snapshot.snapshot_sequence;
        let (events, older_events_cursor) = self
            .event_page(id, &InteractionEventsQuery::default(), snapshot_sequence)
            .await?;
        let runs = self.run_details(id, events).await?;
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
        id: &str,
        events: Vec<ObservationEvent>,
    ) -> anyhow::Result<Vec<RunDetail>> {
        let mut by_run = std::collections::HashMap::<String, Vec<ObservationEvent>>::new();
        for event in events {
            if let Some(run_id) = &event.run_id {
                by_run.entry(run_id.clone()).or_default().push(event);
            }
        }
        let runs = self.runs(id).await?;
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
                usage: ConfirmedUsage {
                    input_tokens: run.input_tokens,
                    output_tokens: run.output_tokens,
                    cache_read_tokens: run.cache_read_tokens,
                    cache_write_tokens: run.cache_write_tokens,
                    reasoning_tokens: run.reasoning_tokens,
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
            Self::Sqlite(pool) => interaction_sqlite(pool, id).await?.is_some(),
            Self::Postgres(pool) => interaction_postgres(pool, id).await?.is_some(),
        };
        if !exists {
            return Ok(None);
        }
        let (events, next_cursor) = self.event_page(id, &query, snapshot_sequence).await?;
        Ok(Some(InteractionEventsPage {
            runs: self.run_details(id, events).await?,
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
        let limit = query.limit.unwrap_or(200) as usize;
        let forward = query.after_sequence.is_some();
        let mut events = match self {
            Self::Sqlite(pool) => {
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
                map_sqlite_events(sql.build().fetch_all(pool).await?)?
            }
            Self::Postgres(pool) => {
                let mut sql = QueryBuilder::<sqlx::Postgres>::new(
                    "SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload::text FROM observation_events WHERE interaction_id=",
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
                map_postgres_events(sql.build().fetch_all(pool).await?)?
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

    pub async fn get_interaction_for_bundle(
        &self,
        id: &str,
        through: i64,
    ) -> anyhow::Result<Option<InteractionDetail>> {
        let Some(snapshot) = self
            .get_interaction_summary(id, ForestQuery::default())
            .await?
        else {
            return Ok(None);
        };
        let snapshot_sequence = through.min(snapshot.snapshot_sequence);
        let mut events = Vec::new();
        let mut after = 0;
        loop {
            let query = InteractionEventsQuery {
                after_sequence: Some(after),
                limit: Some(500),
                ..Default::default()
            };
            let (page, next) = self.event_page(id, &query, snapshot_sequence).await?;
            events.extend(page);
            let Some(next) = next else {
                break;
            };
            after = next;
        }
        Ok(Some(InteractionDetail {
            interaction: snapshot.interaction,
            root: snapshot.root,
            runs: self.run_details(id, events).await?,
            snapshot_sequence,
            older_events_cursor: None,
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
            Self::Sqlite(p) => rejections_sqlite(p, &q, start, end, bounded_end, limit).await?,
            Self::Postgres(p) => rejections_postgres(p, &q, start, end, bounded_end, limit).await?,
        };
        let next_cursor =
            (rows.len() > limit as usize).then(|| rows[limit as usize - 1].id.clone());
        rows.truncate(limit as usize);
        Ok(RejectionPage {
            items: rows.into_iter().map(rejection_summary).collect(),
            total,
            next_cursor,
            snapshot_sequence,
        })
    }
    pub async fn get_rejection(&self, id: &str) -> anyhow::Result<Option<RejectionDetail>> {
        let snapshot_sequence = self.max_sequence().await?;
        let row = match self {
            Self::Sqlite(p) => rejection_sqlite(p, id).await?,
            Self::Postgres(p) => rejection_postgres(p, id).await?,
        };
        let Some(row) = row else { return Ok(None) };
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
            Self::Sqlite(p) => {
                let mut b = QueryBuilder::<sqlx::Sqlite>::new(RUN_SELECT);
                b.push_bind(id)
                    .push(" GROUP BY r.id ORDER BY r.started_at,r.id");
                Ok(b.build_query_as().fetch_all(p).await?)
            }
            Self::Postgres(p) => {
                let mut b = QueryBuilder::<sqlx::Postgres>::new(RUN_SELECT);
                b.push_bind(id)
                    .push(" GROUP BY r.id ORDER BY r.started_at,r.id");
                Ok(b.build_query_as().fetch_all(p).await?)
            }
        }
    }
    async fn manifest_for_run(&self, id: &str) -> anyhow::Result<Option<TraceManifest>> {
        let row=match self{Self::Sqlite(p)=>sqlx::query_as("SELECT trace_id,status,bytes_written,event_count,partial_reason FROM debug_trace_manifests WHERE run_id=?").bind(id).fetch_optional(p).await?,Self::Postgres(p)=>sqlx::query_as("SELECT trace_id,status,bytes_written,event_count,partial_reason FROM debug_trace_manifests WHERE run_id=$1").bind(id).fetch_optional(p).await?};
        Ok(row.map(manifest))
    }
    async fn manifest_for_rejection(&self, id: &str) -> anyhow::Result<Option<TraceManifest>> {
        let row=match self{Self::Sqlite(p)=>sqlx::query_as("SELECT trace_id,status,bytes_written,event_count,partial_reason FROM debug_trace_manifests WHERE rejection_id=?").bind(id).fetch_optional(p).await?,Self::Postgres(p)=>sqlx::query_as("SELECT trace_id,status,bytes_written,event_count,partial_reason FROM debug_trace_manifests WHERE rejection_id=$1").bind(id).fetch_optional(p).await?};
        Ok(row.map(manifest))
    }
    async fn rejection_events(
        &self,
        id: &str,
        through: i64,
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        match self {
            Self::Sqlite(pool) => map_sqlite_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload FROM observation_events WHERE rejection_id=? AND sequence<=? ORDER BY sequence").bind(id).bind(through).fetch_all(pool).await?),
            Self::Postgres(pool) => map_postgres_events(sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload::text FROM observation_events WHERE rejection_id=$1 AND sequence<=$2 ORDER BY sequence").bind(id).bind(through).fetch_all(pool).await?),
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
}

async fn forest_roots_sqlite(
    p: &sqlx::SqlitePool,
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
    let total = base.build_query_scalar::<i64>().fetch_one(p).await?;
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
    Ok((total, page.build_query_as().fetch_all(p).await?))
}
async fn forest_roots_postgres(
    p: &sqlx::PgPool,
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
    let total = base.build_query_scalar::<i64>().fetch_one(p).await?;
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
    Ok((total, page.build_query_as().fetch_all(p).await?))
}

async fn interactions_for_roots_sqlite(
    p: &sqlx::SqlitePool,
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
    p: &sqlx::PgPool,
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
    p: &sqlx::SqlitePool,
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
    p: &sqlx::PgPool,
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

const REJECTION_SELECT: &str = "SELECT r.id,r.occurred_at,r.method,r.path,r.ingress_protocol,r.stage,r.code,r.status_code,r.debug_enabled,CASE WHEN NOT r.debug_enabled THEN 'none' WHEN m.trace_id IS NULL THEN 'partial' ELSE m.status END debug_status FROM rejected_request_observations r LEFT JOIN debug_trace_manifests m ON m.rejection_id=r.id ";
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

fn manifest(r: ManifestRow) -> TraceManifest {
    TraceManifest {
        trace_id: r.trace_id,
        enabled: true,
        status: r.status,
        bytes_written: r.bytes_written.max(0) as u64,
        event_count: r.event_count.max(0) as u64,
        reasons: r.partial_reason.into_iter().collect(),
    }
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
        usage: ConfirmedUsage {
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            reasoning_tokens: r.reasoning_tokens,
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
                payload: super::codec::decode_payload(serde_json::from_str(
                    &r.try_get::<String, _>(6)?,
                )?)?,
            })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn context_batches_preserve_groups_order_and_watermark() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        let store = ObservationStore::Sqlite(pool.clone());
        // An empty root must not touch observation storage at all.
        assert!(store.context_events(&[], 10).await?.is_empty());
        sqlx::query("CREATE TABLE observation_events (sequence INTEGER PRIMARY KEY, occurred_at INTEGER NOT NULL, interaction_id TEXT, run_id TEXT, rejection_id TEXT, kind TEXT NOT NULL, payload TEXT NOT NULL)")
            .execute(&pool).await?;
        let ids: Vec<_> = (0..1_801)
            .map(|index| format!("interaction-{index}"))
            .collect();
        let mut tx = pool.begin().await?;
        for (index, id) in ids.iter().enumerate() {
            sqlx::query("INSERT INTO observation_events VALUES (?,0,?,NULL,NULL,'compaction_operation','{}')")
                .bind(index as i64 + 1).bind(id).execute(&mut *tx).await?;
        }
        for (sequence, kind) in [
            (1_802i64, "retained_tail_associated"),
            (1_803, "native_compaction_associated"),
            (1_804, "client_tool_result"),
            (1_805, "compaction_operation"),
        ] {
            sqlx::query("INSERT INTO observation_events VALUES (?,0,?,NULL,NULL,?,'{}')")
                .bind(sequence)
                .bind(&ids[0])
                .bind(kind)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        let refs: Vec<_> = ids.iter().map(String::as_str).collect();
        let context = store.context_events(&refs, 1_804).await?;
        for (index, id) in ids.iter().enumerate() {
            let sequences: Vec<_> = context[id].iter().map(|event| event.sequence).collect();
            assert_eq!(
                sequences,
                if index == 0 {
                    vec![1, 1_802, 1_803]
                } else {
                    vec![index as i64 + 1]
                }
            );
            assert!(
                context[id]
                    .iter()
                    .all(|event| event.interaction_id.as_ref() == Some(id))
            );
        }
        Ok(())
    }
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
                payload: super::codec::decode_payload(serde_json::from_str(
                    &r.try_get::<String, _>(6)?,
                )?)?,
            })
        })
        .collect()
}
