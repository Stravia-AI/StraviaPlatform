use std::{collections::HashMap, fmt};

use super::{InteractionObservation, ObservationEvent, hydrate, query, store::ObservationStore};

#[derive(Debug, Clone)]
pub(crate) struct RecallChain {
    pub root_id: String,
    pub head_id: String,
    /// 代表链路的最近活动成员，也是链路列表的游标键。
    pub latest_id: String,
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
pub(crate) struct RecallTool {
    pub call: ObservationEvent,
    pub results: Vec<ObservationEvent>,
}

/// 绑定单个 Principal 与保留期截止时间的只读回看作用域。
#[derive(Clone, Copy)]
pub(crate) struct RecallScope<'a> {
    store: &'a ObservationStore,
    principal: &'a str,
    active_since: i64,
}

/// 已通过 Principal、链路根、保留期与成员资格校验的 Interaction 回看句柄。
/// 所有事件与工具读取均挂在该句柄上，无法对未校验的裸 ID 发起读取。
#[derive(Clone)]
pub(crate) struct RecallInteraction<'a> {
    store: &'a ObservationStore,
    id: String,
    root_id: String,
    parent_id: Option<String>,
    started_at: i64,
    protocol: String,
    runs: HashMap<String, RecallRun>,
}

impl fmt::Debug for RecallInteraction<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecallInteraction")
            .field("id", &self.id)
            .field("root_id", &self.root_id)
            .field("parent_id", &self.parent_id)
            .field("started_at", &self.started_at)
            .field("protocol", &self.protocol)
            .field("runs", &self.runs)
            .finish()
    }
}

impl RecallInteraction<'_> {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn root_id(&self) -> &str {
        &self.root_id
    }

    pub(crate) fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }

    pub(crate) fn started_at(&self) -> i64 {
        self.started_at
    }

    pub(crate) fn protocol(&self) -> &str {
        &self.protocol
    }

    pub(crate) fn runs(&self) -> &HashMap<String, RecallRun> {
        &self.runs
    }
}

// Interaction 的 `status` 由 Run 汇总而来，永远不是 `failed`：最终失败的交互停在 `interrupted`
// 等非完成终态，失败事实只在 Run 上。成员规则与展示状态因此和管理面
// (`observation-chain-visibility.ts`) 一样，按「出现过失败的请求」判定。
//
// Run 子查询的 WHERE 只按 `interaction_id` 关联，条件放进 CASE 聚合：没有统计信息的 SQLite
// 会为 `status='failed'` 选用 `inference_runs_status_idx`，每行都扫描全库的失败 Run。
fn failed_run() -> String {
    format!(
        "r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ({}))",
        query::CANCELLED_TERMINAL_REASONS
    )
}

fn failed_request(interaction: &str) -> String {
    format!(
        "(SELECT MAX(CASE WHEN {} THEN 1 ELSE 0 END) FROM inference_run_observations r WHERE r.interaction_id={interaction})=1",
        failed_run()
    )
}

/// 出现过失败的请求、且停在非完成终态的 Interaction 展示为 `failed`。
fn display_status(interaction: &str, status: &str) -> String {
    format!(
        "CASE WHEN {status} NOT IN ('completed','running','waiting_client') AND {} THEN 'failed' ELSE {status} END",
        failed_request(interaction)
    )
}

// 根 Run 不是 MCP，且 Interaction 仍在进行、有已提交的输出或没有失败请求。
// 一次扫描该 Interaction 的 Run：2 表示有已提交输出，1 表示只有失败请求。
pub(crate) fn eligible(alias: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM inference_run_observations root_run WHERE root_run.id={alias}.root_run_id AND root_run.ingress_protocol <> 'mcp') AND ({alias}.status IN ('running','waiting_client') OR COALESCE((SELECT MAX(CASE WHEN r.client_output_committed THEN 2 WHEN {} THEN 1 ELSE 0 END) FROM inference_run_observations r WHERE r.interaction_id={alias}.id),0)<>1)",
        failed_run()
    )
}

// 所有授权列表与查询共享同一成员定义：`$1` 是 Principal，`$2` 是保留期截止时间。
// 清理删除截止时间及以前的记录，因此回看使用严格大于的边界。
// 清理保留运行中的记录，回看则仍会过滤已过期的运行中记录。
fn listed(alias: &str, root: Option<&str>) -> String {
    let root = root
        .map(|root| format!(" AND {alias}.root_id={root}"))
        .unwrap_or_default();
    format!(
        "{alias}.principal=$1 AND {alias}.last_active_at>$2{root} AND {}",
        eligible(alias)
    )
}

/// 链路总数仍随保留期内的成员数线性增长：逐个链路找到一个可列成员即计入。
pub(super) fn chain_count_sql() -> String {
    format!(
        "SELECT COUNT(*) FROM (SELECT DISTINCT root_id FROM interaction_observations WHERE principal=$1 AND last_active_at>$2) d WHERE EXISTS (SELECT 1 FROM interaction_observations m WHERE {})",
        listed("m", Some("d.root_id"))
    )
}

/// 每条链路由它最近活动的可列成员代表（同一时刻取 id 最小者）。代表行沿
/// (principal, last_active_at DESC, id) 索引顺序扫描并在 LIMIT 处停止，首成员与成员数只为本页链路
/// 计算，因此翻页耗时取决于页大小，不随保留期内的总量增长。游标是代表成员的 (last_active_at, id)；
/// `<= COALESCE($3, ..)` 让索引直接定位到游标位置。
pub(super) fn chain_page_sql() -> String {
    format!(
        "WITH page AS MATERIALIZED (SELECT i.id,i.root_id,i.last_active_at,i.status FROM interaction_observations i WHERE {} AND i.last_active_at<=COALESCE($3,9223372036854775807) AND ($3 IS NULL OR i.last_active_at<$3 OR (i.last_active_at=$3 AND i.id>$4)) AND NOT EXISTS (SELECT 1 FROM interaction_observations n WHERE {} AND (n.last_active_at>i.last_active_at OR (n.last_active_at=i.last_active_at AND n.id<i.id))) ORDER BY i.last_active_at DESC,i.id LIMIT $5), chains AS MATERIALIZED (SELECT page.*,(SELECT h.id FROM interaction_observations h WHERE {} ORDER BY h.started_at,h.id LIMIT 1) AS head_id,(SELECT COUNT(*) FROM interaction_observations c WHERE {}) AS interactions FROM page) SELECT chains.root_id,chains.head_id,chains.last_active_at,chains.interactions,{},h.first_model_display_name,chains.id FROM chains JOIN interaction_observations h ON h.id=chains.head_id ORDER BY chains.last_active_at DESC,chains.id",
        listed("i", None),
        listed("n", Some("i.root_id")),
        listed("h", Some("page.root_id")),
        listed("c", Some("page.root_id")),
        display_status("chains.id", "chains.status")
    )
}

pub(super) fn member_count_sql() -> String {
    format!(
        "SELECT COUNT(*),MIN(i.started_at) FROM interaction_observations i WHERE {}",
        listed("i", Some("$3"))
    )
}

pub(super) fn member_page_sql() -> String {
    format!(
        "WITH page AS MATERIALIZED (SELECT i.id,i.parent_interaction_id,i.started_at,i.status,i.first_model_display_name FROM interaction_observations i WHERE {} AND ($4 IS NULL OR i.started_at>$4 OR (i.started_at=$4 AND i.id>$5)) ORDER BY i.started_at,i.id LIMIT $6) SELECT page.id,page.parent_interaction_id,p.id IS NOT NULL,page.started_at,{},page.first_model_display_name FROM page LEFT JOIN interaction_observations p ON p.id=page.parent_interaction_id AND {} ORDER BY page.started_at,page.id",
        listed("i", Some("$3")),
        display_status("page.id", "page.status"),
        listed("p", Some("$3"))
    )
}

pub(super) fn interaction_sql() -> String {
    format!(
        "SELECT i.id,i.root_id,i.parent_interaction_id,i.started_at,r.ingress_protocol FROM interaction_observations i JOIN inference_run_observations r ON r.id=i.root_run_id WHERE i.id=$4 AND {}",
        listed("i", Some("$3"))
    )
}

/// 链路页的一行：根 id、首个成员、最近活动时间、成员数、展示状态、首个模型、最近活动的成员。
type ChainRow = (String, String, i64, i64, String, Option<String>, String);

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

pub(super) fn build_previews_query<'a, DB>(
    principal: &'a str,
    active_since: i64,
    chunk: &'a [String],
) -> sqlx::QueryBuilder<DB>
where
    DB: sqlx::Database,
    &'a str: sqlx::Encode<'a, DB> + sqlx::Type<DB>,
    &'a String: sqlx::Encode<'a, DB> + sqlx::Type<DB>,
    i64: sqlx::Encode<'a, DB> + sqlx::Type<DB>,
{
    let mut sql = sqlx::QueryBuilder::<DB>::new("WITH requested(id) AS (VALUES ");
    let mut ids = sql.separated(",");
    for id in chunk {
        ids.push("(CAST(")
            .push_bind_unseparated(id)
            .push_unseparated(" AS TEXT))");
    }
    ids.push_unseparated(
        "), eligible_ids AS MATERIALIZED (SELECT DISTINCT i.id FROM requested req CROSS JOIN interaction_observations i WHERE i.id=req.id AND i.principal=",
    );
    sql.push_bind(principal)
        .push(" AND i.last_active_at>")
        .push_bind(active_since)
        .push(" AND (")
        .push(eligible("i"))
        .push(")) SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,attrs FROM (SELECT e.sequence,e.occurred_at,e.interaction_id,e.run_id,e.rejection_id,e.kind,e.attrs,ROW_NUMBER() OVER (PARTITION BY e.interaction_id ORDER BY CASE WHEN e.kind='user_input_recorded' THEN 0 ELSE 1 END,e.sequence) AS rn FROM eligible_ids m JOIN observation_events e ON e.interaction_id=m.id LEFT JOIN observation_blocks b ON b.sequence=e.sequence WHERE e.kind='user_input_recorded' OR (e.kind='client_visible_content' AND b.channel='visible' AND b.complete)) ranked WHERE rn=1");
    sql
}

impl InteractionObservation {
    /// 打开绑定单个 Principal 与保留期截止时间的只读回看作用域。
    pub(crate) fn recall<'a>(&'a self, principal: &'a str, active_since: i64) -> RecallScope<'a> {
        RecallScope {
            store: &self.inner.store,
            principal,
            active_since,
        }
    }
}

impl<'a> RecallScope<'a> {
    pub(crate) async fn chains(
        &self,
        after: Option<&(i64, String)>,
        limit: usize,
    ) -> anyhow::Result<RecallChainPage> {
        let store = self.store;
        let count_sql = chain_count_sql();
        let counts: Option<(i64,)> =
            fetch_optional!(store, count_sql, self.principal, self.active_since);
        let (total,) =
            counts.ok_or_else(|| anyhow::anyhow!("recall chain count returned no row"))?;
        let sql = chain_page_sql();
        let rows: Vec<ChainRow> = fetch_all!(
            store,
            sql,
            self.principal,
            self.active_since,
            after.map(|a| a.0),
            after.map(|a| a.1.as_str()),
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
        );
        let has_more = rows.len() > limit;
        let chains = rows
            .into_iter()
            .take(limit)
            .map(
                |(root_id, head_id, last_active_at, interactions, status, model, latest_id)| {
                    RecallChain {
                        root_id,
                        head_id,
                        latest_id,
                        last_active_at,
                        interactions: interactions as u64,
                        status,
                        model,
                    }
                },
            )
            .collect();
        Ok(RecallChainPage {
            total: total as u64,
            chains,
            has_more,
        })
    }

    pub(crate) async fn members(
        &self,
        root_id: &str,
        after: Option<&(i64, String)>,
        limit: usize,
    ) -> anyhow::Result<Option<RecallMemberPage>> {
        let store = self.store;
        let sql = member_count_sql();
        let counts: Option<(i64, Option<i64>)> =
            fetch_optional!(store, sql, self.principal, self.active_since, root_id);
        let (total, started_at) =
            counts.ok_or_else(|| anyhow::anyhow!("recall member count returned no row"))?;
        let Some(started_at) = started_at else {
            return Ok(None);
        };
        let sql = member_page_sql();
        let rows: Vec<MemberRow> = fetch_all!(
            store,
            sql,
            self.principal,
            self.active_since,
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

    pub(crate) async fn interaction(
        &self,
        root_id: &str,
        interaction_id: &str,
    ) -> anyhow::Result<Option<RecallInteraction<'a>>> {
        let store = self.store;
        let sql = interaction_sql();
        let row: Option<(String, String, Option<String>, i64, String)> = fetch_optional!(
            store,
            sql,
            self.principal,
            self.active_since,
            root_id,
            interaction_id
        );
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
            store,
            id,
            root_id,
            parent_id,
            started_at,
            protocol,
            runs,
        }))
    }

    pub(crate) async fn previews(
        &self,
        interaction_ids: &[String],
    ) -> anyhow::Result<HashMap<String, String>> {
        let store = self.store;
        let mut previews = HashMap::new();
        for chunk in interaction_ids.chunks(400) {
            let mut events = match store {
                ObservationStore::Sqlite(pool, _) => {
                    let mut sql = build_previews_query::<sqlx::Sqlite>(
                        self.principal,
                        self.active_since,
                        chunk,
                    );
                    query::map_sqlite_events(sql.build().fetch_all(pool).await?)?
                }
                ObservationStore::Postgres(pool, _) => {
                    let mut sql = build_previews_query::<sqlx::Postgres>(
                        self.principal,
                        self.active_since,
                        chunk,
                    );
                    query::map_postgres_events(sql.build().fetch_all(pool).await?)?
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

impl RecallInteraction<'_> {
    pub(crate) async fn events(
        &self,
        after_sequence: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<ObservationEvent>> {
        let sql = "SELECT e.sequence,e.occurred_at,e.interaction_id,e.run_id,e.rejection_id,e.kind,e.attrs FROM observation_events e LEFT JOIN observation_blocks b ON b.sequence=e.sequence WHERE e.interaction_id=$1 AND e.sequence>$2 AND (e.kind IN ('user_input_recorded','client_tool_handoff','platform_tool_started','client_tool_result','platform_tool_finished','observation_gap','request_failed') OR (e.kind='client_visible_content' AND b.channel='visible' AND b.complete)) ORDER BY e.sequence LIMIT $3";
        let store = self.store;
        let mut events = match store {
            ObservationStore::Sqlite(pool, _) => query::map_sqlite_events(
                sqlx::query(sql)
                    .bind(&self.id)
                    .bind(after_sequence)
                    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                    .fetch_all(pool)
                    .await?,
            )?,
            ObservationStore::Postgres(pool, _) => query::map_postgres_events(
                sqlx::query(sql)
                    .bind(&self.id)
                    .bind(after_sequence)
                    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                    .fetch_all(pool)
                    .await?,
            )?,
        };
        hydrate::hydrate(store, &mut events).await?;
        Ok(events)
    }

    /// 每个工具开始事件占一个编号，与 `tool` 的 OFFSET 计数一致；缺少 tool_id 的事件为 `None`。
    pub(crate) async fn tool_ids(
        &self,
        before_sequence: i64,
    ) -> anyhow::Result<Vec<Option<String>>> {
        let sql = "SELECT tool_id FROM observation_events WHERE interaction_id=$1 AND sequence<$2 AND kind IN ('client_tool_handoff','platform_tool_started') ORDER BY sequence".to_string();
        let rows: Vec<(Option<String>,)> = fetch_all!(self.store, sql, &self.id, before_sequence);
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    pub(crate) async fn tool(&self, index: u32) -> anyhow::Result<Option<RecallTool>> {
        if index == 0 {
            return Ok(None);
        }
        let store = self.store;
        let sql = "SELECT sequence,tool_id FROM observation_events WHERE interaction_id=$1 AND kind IN ('client_tool_handoff','platform_tool_started') ORDER BY sequence LIMIT 1 OFFSET $2".to_string();
        let row: Option<(i64, Option<String>)> =
            fetch_optional!(store, sql, &self.id, i64::from(index - 1));
        let Some((sequence, tool_id)) = row else {
            return Ok(None);
        };
        let sql = "SELECT e.sequence,e.occurred_at,e.interaction_id,e.run_id,e.rejection_id,e.kind,e.attrs FROM observation_events e WHERE e.interaction_id=$1 AND (e.sequence=$2 OR (e.sequence>$2 AND ((e.kind='platform_tool_finished' AND e.tool_id=$3) OR (e.kind='client_tool_result' AND EXISTS (SELECT 1 FROM observation_tool_results tr WHERE tr.sequence=e.sequence AND tr.tool_id=$3))))) ORDER BY e.sequence";
        let mut events = match store {
            ObservationStore::Sqlite(pool, _) => query::map_sqlite_events(
                sqlx::query(sql)
                    .bind(&self.id)
                    .bind(sequence)
                    .bind(tool_id.as_deref())
                    .fetch_all(pool)
                    .await?,
            )?,
            ObservationStore::Postgres(pool, _) => query::map_postgres_events(
                sqlx::query(sql)
                    .bind(&self.id)
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
}
