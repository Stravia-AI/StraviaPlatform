use std::collections::HashMap;

use super::{InteractionObservation, ObservationEvent, hydrate, query, store::ObservationStore};

#[derive(Debug, Clone)]
pub(crate) struct RecallChain {
    pub root_id: String,
    pub head_id: String,
    pub last_active_at: i64,
    pub interactions: u64,
    pub status: String,
    pub model: Option<String>,
}
#[derive(Debug, Clone)]
pub(crate) struct RecallChainPage {
    pub total: u64,
    pub chains: Vec<RecallChain>,
    pub has_more: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct RecallMember {
    pub id: String,
    pub parent_id: Option<String>,
    pub parent_listed: bool,
    pub started_at: i64,
    pub status: String,
    pub model: Option<String>,
}
#[derive(Debug, Clone)]
pub(crate) struct RecallMemberPage {
    pub total: u64,
    pub started_at: i64,
    pub members: Vec<RecallMember>,
    pub has_more: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct RecallRun {
    pub model: Option<String>,
    pub started_at: i64,
}
#[derive(Debug, Clone)]
pub(crate) struct RecallInteraction {
    pub id: String,
    pub root_id: String,
    pub parent_id: Option<String>,
    pub started_at: i64,
    pub protocol: String,
    pub runs: HashMap<String, RecallRun>,
}
#[derive(Debug, Clone)]
pub(crate) struct RecallTool {
    pub call: ObservationEvent,
    pub results: Vec<ObservationEvent>,
}

// Interaction 的 `status` 由 Run 汇总而来，永远不是 `failed`：最终失败的交互停在 `interrupted`
// 等非完成终态，失败事实只在 Run 上。成员规则与展示状态因此和管理面
// (`observation-chain-visibility.ts`) 一样，按「出现过失败的请求」判定。
fn failed_request(interaction: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM inference_run_observations r WHERE r.interaction_id={interaction} AND r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ({})))",
        query::CANCELLED_TERMINAL_REASONS
    )
}

/// 出现过失败的请求、且停在非完成终态的 Interaction 展示为 `failed`。
fn display_status(interaction: &str, status: &str) -> String {
    format!(
        "CASE WHEN {status} NOT IN ('completed','running','waiting_client') AND {} THEN 'failed' ELSE {status} END",
        failed_request(interaction)
    )
}

// 所有授权列表与查询共享同一成员定义。
// 清理删除截止时间及以前的记录，因此回看使用严格大于的边界。
// 清理保留运行中的记录，回看则仍会过滤已过期的运行中记录。
fn eligible(alias: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM inference_run_observations root_run WHERE root_run.id={alias}.root_run_id AND root_run.ingress_protocol <> 'mcp') AND ({alias}.status IN ('running','waiting_client') OR EXISTS (SELECT 1 FROM inference_run_observations r WHERE r.interaction_id={alias}.id AND r.client_output_committed) OR NOT {})",
        failed_request(&format!("{alias}.id"))
    )
}

fn membership(root_scoped: bool) -> String {
    let root_filter = if root_scoped { " AND root_id=$3" } else { "" };
    let materialized = if root_scoped { "MATERIALIZED " } else { "" };
    format!(
        "WITH candidates AS {materialized}(SELECT * FROM interaction_observations WHERE principal=$1 AND last_active_at > $2{root_filter}), listed AS (SELECT i.* FROM candidates i WHERE {}) ",
        eligible("i")
    )
}

/// 成员页的一行：id、父 id、父是否在列、开始时间、展示状态、首个模型。
type MemberRow = (String, Option<String>, bool, i64, String, Option<String>);

macro_rules! fetch_all {
    ($store:expr, $sql:expr, $($bind:expr),* $(,)?) => {{
        match $store {
            ObservationStore::Sqlite(pool, _) => sqlx::query_as(sqlx::AssertSqlSafe($sql.as_str()))$(.bind($bind))*.fetch_all(pool).await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_as(sqlx::AssertSqlSafe($sql.as_str()))$(.bind($bind))*.fetch_all(pool).await?,
        }
    }};
}
macro_rules! fetch_optional {
    ($store:expr, $sql:expr, $($bind:expr),* $(,)?) => {{
        match $store {
            ObservationStore::Sqlite(pool, _) => sqlx::query_as(sqlx::AssertSqlSafe($sql.as_str()))$(.bind($bind))*.fetch_optional(pool).await?,
            ObservationStore::Postgres(pool, _) => sqlx::query_as(sqlx::AssertSqlSafe($sql.as_str()))$(.bind($bind))*.fetch_optional(pool).await?,
        }
    }};
}

impl InteractionObservation {
    pub(crate) async fn recall_chains(
        &self,
        principal: &str,
        active_since: i64,
        after: Option<&(i64, String)>,
        limit: usize,
    ) -> anyhow::Result<RecallChainPage> {
        let store = &self.inner.store;
        let base = membership(false);
        let count_sql = format!("{base} SELECT COUNT(DISTINCT root_id) FROM listed");
        let counts: Option<(i64,)> = fetch_optional!(store, count_sql, principal, active_since);
        let (total,) =
            counts.ok_or_else(|| anyhow::anyhow!("recall chain count returned no row"))?;
        let sql = format!(
            "{base}, ranked AS (SELECT listed.*, ROW_NUMBER() OVER (PARTITION BY root_id ORDER BY last_active_at DESC,id DESC) AS active_rank, ROW_NUMBER() OVER (PARTITION BY root_id ORDER BY started_at,id) AS head_rank FROM listed), chains AS (SELECT root_id,MAX(CASE WHEN head_rank=1 THEN id END) AS head_id,MAX(last_active_at) AS last_active_at,COUNT(*) AS interactions,MAX(CASE WHEN active_rank=1 THEN id END) AS latest_id,MAX(CASE WHEN active_rank=1 THEN status END) AS status,MAX(CASE WHEN head_rank=1 THEN first_model_display_name END) AS model FROM ranked GROUP BY root_id), page AS MATERIALIZED (SELECT * FROM chains WHERE ($3 IS NULL OR last_active_at<$3 OR (last_active_at=$3 AND root_id<$4)) ORDER BY last_active_at DESC,root_id DESC LIMIT $5) SELECT root_id,head_id,last_active_at,interactions,{},model FROM page ORDER BY last_active_at DESC,root_id DESC",
            display_status("page.latest_id", "page.status")
        );
        let rows: Vec<(String, String, i64, i64, String, Option<String>)> = fetch_all!(
            store,
            sql,
            principal,
            active_since,
            after.map(|a| a.0),
            after.map(|a| a.1.as_str()),
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
        );
        let has_more = rows.len() > limit;
        let chains = rows
            .into_iter()
            .take(limit)
            .map(
                |(root_id, head_id, last_active_at, interactions, status, model)| RecallChain {
                    root_id,
                    head_id,
                    last_active_at,
                    interactions: interactions as u64,
                    status,
                    model,
                },
            )
            .collect();
        Ok(RecallChainPage {
            total: total as u64,
            chains,
            has_more,
        })
    }

    pub(crate) async fn recall_members(
        &self,
        principal: &str,
        root_id: &str,
        active_since: i64,
        after: Option<&(i64, String)>,
        limit: usize,
    ) -> anyhow::Result<Option<RecallMemberPage>> {
        let store = &self.inner.store;
        let base = membership(true);
        let sql = format!("{base} SELECT COUNT(*),MIN(started_at) FROM listed");
        let counts: Option<(i64, Option<i64>)> =
            fetch_optional!(store, sql, principal, active_since, root_id);
        let (total, started_at) =
            counts.ok_or_else(|| anyhow::anyhow!("recall member count returned no row"))?;
        let Some(started_at) = started_at else {
            return Ok(None);
        };
        let sql = format!(
            "{base}, page AS MATERIALIZED (SELECT i.id,i.parent_interaction_id,i.started_at,i.status,i.first_model_display_name FROM listed i WHERE ($4 IS NULL OR i.started_at>$4 OR (i.started_at=$4 AND i.id>$5)) ORDER BY i.started_at,i.id LIMIT $6) SELECT page.id,page.parent_interaction_id,p.id IS NOT NULL,page.started_at,{},page.first_model_display_name FROM page LEFT JOIN interaction_observations p ON p.id=page.parent_interaction_id AND p.principal=$1 AND p.root_id=$3 AND p.last_active_at>$2 AND {} ORDER BY page.started_at,page.id",
            display_status("page.id", "page.status"),
            eligible("p")
        );
        let rows: Vec<MemberRow> = fetch_all!(
            store,
            sql,
            principal,
            active_since,
            root_id,
            after.map(|a| a.0),
            after.map(|a| a.1.as_str()),
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
        );
        let has_more = rows.len() > limit;
        let members = rows
            .into_iter()
            .take(limit)
            .map(
                |(id, parent_id, parent_listed, started_at, status, model)| RecallMember {
                    id,
                    parent_id,
                    parent_listed,
                    started_at,
                    status,
                    model,
                },
            )
            .collect();
        Ok(Some(RecallMemberPage {
            total: total as u64,
            started_at,
            members,
            has_more,
        }))
    }

    pub(crate) async fn recall_interaction(
        &self,
        principal: &str,
        root_id: &str,
        interaction_id: &str,
        active_since: i64,
    ) -> anyhow::Result<Option<RecallInteraction>> {
        let store = &self.inner.store;
        let sql = format!(
            "{} SELECT i.id,i.root_id,i.parent_interaction_id,i.started_at,r.ingress_protocol FROM listed i JOIN inference_run_observations r ON r.id=i.root_run_id WHERE i.id=$4",
            membership(true)
        );
        let row: Option<(String, String, Option<String>, i64, String)> =
            fetch_optional!(store, sql, principal, active_since, root_id, interaction_id);
        let Some((id, root_id, parent_id, started_at, protocol)) = row else {
            return Ok(None);
        };
        let sql = "SELECT id,model_display_name,started_at FROM inference_run_observations WHERE interaction_id=$1".to_string();
        let rows: Vec<(String, Option<String>, i64)> = fetch_all!(store, sql, interaction_id);
        let runs = rows
            .into_iter()
            .map(|(id, model, started_at)| (id, RecallRun { model, started_at }))
            .collect();
        Ok(Some(RecallInteraction {
            id,
            root_id,
            parent_id,
            started_at,
            protocol,
            runs,
        }))
    }

    /// 调用方必须先通过 `recall_interaction` 确认该 Interaction 属于同一 Principal。
    pub(crate) async fn recall_events(
        &self,
        interaction_id: &str,
        after_sequence: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        let sql = "SELECT e.sequence,e.occurred_at,e.interaction_id,e.run_id,e.rejection_id,e.kind,e.attrs FROM observation_events e LEFT JOIN observation_blocks b ON b.sequence=e.sequence WHERE e.interaction_id=$1 AND e.sequence>$2 AND (e.kind IN ('user_input_recorded','client_tool_handoff','platform_tool_started','client_tool_result','platform_tool_finished','observation_gap','request_failed') OR (e.kind='client_visible_content' AND b.channel='visible' AND b.complete)) ORDER BY e.sequence LIMIT $3";
        let store = &self.inner.store;
        let mut events = match store {
            ObservationStore::Sqlite(pool, _) => query::map_sqlite_events(
                sqlx::query(sql)
                    .bind(interaction_id)
                    .bind(after_sequence)
                    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                    .fetch_all(pool)
                    .await?,
            )?,
            ObservationStore::Postgres(pool, _) => query::map_postgres_events(
                sqlx::query(sql)
                    .bind(interaction_id)
                    .bind(after_sequence)
                    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                    .fetch_all(pool)
                    .await?,
            )?,
        };
        hydrate::hydrate(store, &mut events).await?;
        Ok(events)
    }

    /// 调用方必须先通过 `recall_interaction` 确认该 Interaction 属于同一 Principal。
    /// 每个工具开始事件占一个编号，与 `recall_tool` 的 OFFSET 计数一致；缺少 tool_id 的事件为 `None`。
    pub(crate) async fn recall_tool_ids(
        &self,
        interaction_id: &str,
        before_sequence: i64,
    ) -> anyhow::Result<Vec<Option<String>>> {
        let sql = "SELECT tool_id FROM observation_events WHERE interaction_id=$1 AND sequence<$2 AND kind IN ('client_tool_handoff','platform_tool_started') ORDER BY sequence".to_string();
        let rows: Vec<(Option<String>,)> =
            fetch_all!(&self.inner.store, sql, interaction_id, before_sequence);
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// 调用方必须先通过 `recall_interaction` 确认该 Interaction 属于同一 Principal。
    pub(crate) async fn recall_tool(
        &self,
        interaction_id: &str,
        index: u32,
    ) -> anyhow::Result<Option<RecallTool>> {
        if index == 0 {
            return Ok(None);
        }
        let store = &self.inner.store;
        let sql = "SELECT sequence,tool_id FROM observation_events WHERE interaction_id=$1 AND kind IN ('client_tool_handoff','platform_tool_started') ORDER BY sequence LIMIT 1 OFFSET $2".to_string();
        let row: Option<(i64, Option<String>)> =
            fetch_optional!(store, sql, interaction_id, i64::from(index - 1));
        let Some((sequence, tool_id)) = row else {
            return Ok(None);
        };
        let sql = "SELECT e.sequence,e.occurred_at,e.interaction_id,e.run_id,e.rejection_id,e.kind,e.attrs FROM observation_events e WHERE e.interaction_id=$1 AND (e.sequence=$2 OR (e.sequence>$2 AND ((e.kind='platform_tool_finished' AND e.tool_id=$3) OR (e.kind='client_tool_result' AND EXISTS (SELECT 1 FROM observation_tool_results tr WHERE tr.sequence=e.sequence AND tr.tool_id=$3))))) ORDER BY e.sequence";
        let mut events = match store {
            ObservationStore::Sqlite(pool, _) => query::map_sqlite_events(
                sqlx::query(sql)
                    .bind(interaction_id)
                    .bind(sequence)
                    .bind(tool_id.as_deref())
                    .fetch_all(pool)
                    .await?,
            )?,
            ObservationStore::Postgres(pool, _) => query::map_postgres_events(
                sqlx::query(sql)
                    .bind(interaction_id)
                    .bind(sequence)
                    .bind(tool_id.as_deref())
                    .fetch_all(pool)
                    .await?,
            )?,
        };
        hydrate::hydrate(store, &mut events).await?;
        let mut events = events.into_iter();
        let Some(call) = events.next().filter(|event| event.sequence == sequence) else {
            return Ok(None);
        };
        Ok(Some(RecallTool {
            call,
            results: events.collect(),
        }))
    }

    pub(crate) async fn recall_previews(
        &self,
        principal: &str,
        interaction_ids: &[String],
    ) -> anyhow::Result<HashMap<String, String>> {
        let store = &self.inner.store;
        let mut previews = HashMap::new();
        for chunk in interaction_ids.chunks(400) {
            macro_rules! preview_rows {
                ($db:ty, $pool:expr, $mapper:path) => {{
                    let mut sql = sqlx::QueryBuilder::<$db>::new("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,attrs FROM (SELECT e.sequence,e.occurred_at,e.interaction_id,e.run_id,e.rejection_id,e.kind,e.attrs,ROW_NUMBER() OVER (PARTITION BY e.interaction_id ORDER BY CASE WHEN e.kind='user_input_recorded' THEN 0 ELSE 1 END,e.sequence) AS rn FROM observation_events e JOIN interaction_observations i ON i.id=e.interaction_id LEFT JOIN observation_blocks b ON b.sequence=e.sequence WHERE i.principal=");
                    sql.push_bind(principal).push(" AND i.id IN (");
                    let mut ids = sql.separated(",");
                    for id in chunk { ids.push_bind(id); }
                    ids.push_unseparated(")");
                    sql.push(" AND (e.kind='user_input_recorded' OR (e.kind='client_visible_content' AND b.channel='visible' AND b.complete))) ranked WHERE rn=1");
                    $mapper(sql.build().fetch_all($pool).await?)?
                }};
            }
            let mut events = match store {
                ObservationStore::Sqlite(pool, _) => {
                    preview_rows!(sqlx::Sqlite, pool, query::map_sqlite_events)
                }
                ObservationStore::Postgres(pool, _) => {
                    preview_rows!(sqlx::Postgres, pool, query::map_postgres_events)
                }
            };
            hydrate::hydrate(store, &mut events).await?;
            for event in events {
                if let (Some(id), Some(text)) = (
                    event.interaction_id,
                    event.payload.get("text").and_then(|v| v.as_str()),
                ) {
                    previews.insert(id, text.to_string());
                }
            }
        }
        Ok(previews)
    }
}
