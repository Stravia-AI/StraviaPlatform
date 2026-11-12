//! 历史检索的存储适配器：每个词项分别查询命中 Interaction，再按统一规则交集、排序与选取命中段。
//!
//! SQLite 通过 FTS5 的 rowid 取得段，PostgreSQL 通过 `tsvector` 字面量与 GIN 索引取得段；
//! 两者都只按段行的主键回查元数据，分词与查询短语都由 Rust 生成，不依赖文本搜索配置。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sqlx::{Database, Encode, FromRow, PgPool, QueryBuilder, SqlitePool, Type};

#[cfg(test)]
use super::search_text::parse_query;
use super::search_text::{ParsedQuery, QueryTerm, Token};
use crate::interaction_observation::{
    InteractionObservation, SearchPool, eligible, index_progress_inconsistent,
};

pub(super) const TERM_INTERACTION_LIMIT: i64 = 2_000;
const SEARCH_PAGE: usize = 10;

/// 检索范围：根目录、一条链路或一轮转录。链路由调用方先校验归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchScope<'a> {
    Root,
    Chain(&'a str),
    Interaction(&'a str, &'a str),
}

/// 键集游标：上一页最后一项的（得分、最近命中时间、Interaction id）。
pub(super) type SearchCursor = (i32, i64, String);

/// 供渲染器换算为 `path:L-M` 的命中段；字节区间相对该事件的未脱敏索引文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchSegment {
    pub event_sequence: i64,
    pub segment_ordinal: i64,
    pub byte_start: i64,
    pub byte_end: i64,
    pub occurred_at: i64,
    /// 本段命中的查询词项下标；用于优先选择覆盖更多词项的段。
    pub terms: Vec<usize>,
    pub role_weight: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchInteraction {
    pub id: String,
    pub root_id: String,
    pub has_observation_gap: bool,
    pub score: i32,
    pub latest_hit_at: i64,
    /// 全部命中段，按覆盖词项数、时间与段位置的优先顺序排列；渲染时再按命中位置去重截取。
    pub segments: Vec<SearchSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchPage {
    pub interactions: Vec<SearchInteraction>,
    pub has_more: bool,
    pub unindexed: i64,
    pub truncated_terms: Vec<String>,
}

/// 一个词项的命中 Interaction：最高角色权重与最近命中时间。
#[derive(Debug, FromRow)]
struct TermHitRow {
    interaction_id: String,
    root_id: String,
    weight: i64,
    latest_hit_at: i64,
}

/// 一个候选 Interaction 的累计排序键。
#[derive(Debug, Default)]
struct Ranked {
    root_id: String,
    score: i32,
    latest: i64,
    matched: BTreeSet<usize>,
}

/// FTS5 用 Rust 输出的词元组成短语。词元只含字母、数字和 CJK 字符；
/// 不加 `^` 前缀，否则短语只能从段首开始。词首前缀由最后的 `*` 表示。
fn sqlite_match(term: &QueryTerm) -> String {
    let phrase = phrase(term);
    format!("\"{phrase}\"{}", if term.prefix { "*" } else { "" })
}

/// `::tsquery` 字面量；显式 `<->` 表达相邻，`:*` 只用于单个 CJK 字前缀。
fn postgres_tsquery(term: &QueryTerm) -> String {
    let mut result = String::new();
    for (index, Token { text, .. }) in term.tokens.iter().enumerate() {
        if index != 0 {
            result.push_str(" <-> ");
        }
        result.push('\'');
        result.push_str(&text.replace('\'', "''"));
        result.push('\'');
        if term.prefix && index + 1 == term.tokens.len() {
            result.push_str(":*");
        }
    }
    result
}

fn phrase(term: &QueryTerm) -> String {
    let mut phrase = String::new();
    for token in &term.tokens {
        if !phrase.is_empty() {
            phrase.push(' ');
        }
        phrase.push_str(&token.text);
    }
    phrase
}

#[derive(Clone, Copy)]
enum SearchBackend {
    Sqlite,
    Postgres,
}

fn common_predicates<DB: Database>(
    builder: &mut QueryBuilder<DB>,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
) where
    DB::Arguments: Default,
    String: for<'q> Encode<'q, DB> + Type<DB>,
    i64: for<'q> Encode<'q, DB> + Type<DB>,
{
    builder
        .push(" AND s.principal=")
        .push_bind(principal.to_owned());
    builder
        .push(" AND i.last_active_at>")
        .push_bind(active_since);
    builder.push(" AND (").push(eligible("i")).push(")");
    match scope {
        SearchScope::Root => {}
        SearchScope::Chain(root_id) => {
            builder
                .push(" AND s.root_id=")
                .push_bind(root_id.to_owned());
        }
        SearchScope::Interaction(root_id, interaction_id) => {
            builder
                .push(" AND s.root_id=")
                .push_bind(root_id.to_owned());
            builder
                .push(" AND s.interaction_id=")
                .push_bind(interaction_id.to_owned());
        }
    }
}

/// 构造每个词项的完整查询：动态值全部通过 QueryBuilder 绑定，不生成 SQL 字面量。
/// 授权、范围、保留期与 P1 成员规则全部在 GROUP BY 之前过滤，LIMIT 在分组之后。
fn term_query<DB: Database>(
    builder: &mut QueryBuilder<DB>,
    backend: SearchBackend,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    term: &QueryTerm,
) where
    DB::Arguments: Default,
    String: for<'q> Encode<'q, DB> + Type<DB>,
    i64: for<'q> Encode<'q, DB> + Type<DB>,
{
    match backend {
        SearchBackend::Sqlite => {
            // CROSS JOIN 固定连接顺序：FTS 命中在外层，按 rowid 回表，避免规划器改走 scope 索引扫描整个 Principal。
            builder.push("WITH hits AS MATERIALIZED (SELECT rowid FROM history_search_segments_fts WHERE history_search_segments_fts MATCH ")
                .push_bind(sqlite_match(term))
                .push(") SELECT s.interaction_id,s.root_id,CAST(MAX(s.role_weight) AS BIGINT) AS weight,MAX(s.occurred_at) AS latest_hit_at FROM hits CROSS JOIN history_search_segments s ON s.id=hits.rowid JOIN interaction_observations i ON i.id=s.interaction_id WHERE 1=1");
        }
        SearchBackend::Postgres => {
            builder.push("SELECT s.interaction_id,s.root_id,CAST(MAX(s.role_weight) AS BIGINT) AS weight,MAX(s.occurred_at) AS latest_hit_at FROM history_search_segments s JOIN interaction_observations i ON i.id=s.interaction_id WHERE s.tokens @@ ")
                .push_bind(postgres_tsquery(term))
                .push("::tsquery");
        }
    }
    common_predicates(builder, principal, active_since, scope);
    builder.push(" GROUP BY s.interaction_id,s.root_id ORDER BY MAX(s.occurred_at) DESC,s.interaction_id LIMIT ")
        .push(TERM_INTERACTION_LIMIT + 1);
}

async fn sqlite_term_hits(
    pool: &SqlitePool,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    term: &QueryTerm,
) -> anyhow::Result<Vec<TermHitRow>> {
    let mut builder = QueryBuilder::<sqlx::Sqlite>::new("");
    term_query(
        &mut builder,
        SearchBackend::Sqlite,
        principal,
        active_since,
        scope,
        term,
    );
    let query = builder.build_query_as::<TermHitRow>();
    Ok(query.fetch_all(pool).await?)
}

async fn postgres_term_hits(
    pool: &PgPool,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    term: &QueryTerm,
) -> anyhow::Result<Vec<TermHitRow>> {
    let mut builder = QueryBuilder::<sqlx::Postgres>::new("");
    term_query(
        &mut builder,
        SearchBackend::Postgres,
        principal,
        active_since,
        scope,
        term,
    );
    let query = builder.build_query_as::<TermHitRow>();
    Ok(query.fetch_all(pool).await?)
}

fn count_unindexed_query<DB: Database>(
    builder: &mut QueryBuilder<DB>,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    current: &[&str],
) where
    DB::Arguments: Default,
    String: for<'q> Encode<'q, DB> + Type<DB>,
    i64: for<'q> Encode<'q, DB> + Type<DB>,
{
    builder.push("WITH candidates AS MATERIALIZED (SELECT i.id,i.root_run_id,i.status FROM interaction_observations i LEFT JOIN history_search_progress p ON p.interaction_id=i.id WHERE i.principal=").push_bind(principal.to_owned());
    builder
        .push(" AND i.last_active_at>")
        .push_bind(active_since);
    builder
        .push(" AND (")
        .push(index_progress_inconsistent("i", "p"))
        .push(")");
    match scope {
        SearchScope::Root => {}
        SearchScope::Chain(root) => {
            builder.push(" AND i.root_id=").push_bind(root.to_owned());
        }
        SearchScope::Interaction(root, id) => {
            builder.push(" AND i.root_id=").push_bind(root.to_owned());
            builder.push(" AND i.id=").push_bind(id.to_owned());
        }
    }
    if !current.is_empty() {
        builder.push(" AND i.id NOT IN (");
        let mut separated = builder.separated(",");
        for id in current {
            separated.push_bind((*id).to_owned());
        }
        separated.push_unseparated(")");
    }
    builder
        .push(") SELECT COUNT(*) FROM candidates i WHERE (")
        .push(eligible("i"))
        .push(")");
}

async fn count_unindexed(
    observation: &InteractionObservation,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    current_interaction: &[&str],
) -> anyhow::Result<i64> {
    match observation.search_pool() {
        SearchPool::Sqlite(pool) => {
            let mut builder = QueryBuilder::<sqlx::Sqlite>::new("");
            count_unindexed_query(
                &mut builder,
                principal,
                active_since,
                scope,
                current_interaction,
            );
            Ok(builder.build_query_scalar().fetch_one(pool).await?)
        }
        SearchPool::Postgres(pool) => {
            let mut builder = QueryBuilder::<sqlx::Postgres>::new("");
            count_unindexed_query(
                &mut builder,
                principal,
                active_since,
                scope,
                current_interaction,
            );
            Ok(builder.build_query_scalar().fetch_one(pool).await?)
        }
    }
}

pub(super) async fn search(
    observation: &InteractionObservation,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    query: &ParsedQuery,
    after: Option<SearchCursor>,
    current_interaction: &[&str],
) -> anyhow::Result<SearchPage> {
    let unindexed = count_unindexed(
        observation,
        principal,
        active_since,
        scope,
        current_interaction,
    )
    .await?;
    let mut truncated_terms = Vec::new();
    let pool = observation.search_pool();
    let mut ranked: BTreeMap<String, Ranked> = BTreeMap::new();
    for (index, term) in query.terms.iter().enumerate() {
        let mut rows = match pool {
            SearchPool::Sqlite(pool) => {
                sqlite_term_hits(pool, principal, active_since, scope, term).await?
            }
            SearchPool::Postgres(pool) => {
                postgres_term_hits(pool, principal, active_since, scope, term).await?
            }
        };
        if rows.len() > TERM_INTERACTION_LIMIT as usize {
            truncated_terms.push(term.text.clone());
            rows.truncate(TERM_INTERACTION_LIMIT as usize);
        }
        let mut term_hits: HashMap<String, (i32, i64, String)> = HashMap::with_capacity(rows.len());
        for TermHitRow {
            interaction_id,
            root_id,
            weight,
            latest_hit_at,
        } in rows
        {
            let weight = i32::try_from(weight)?;
            term_hits.insert(interaction_id, (weight, latest_hit_at, root_id));
        }
        if index == 0 {
            for (id, (weight, latest, root_id)) in term_hits.iter() {
                ranked.insert(
                    id.clone(),
                    Ranked {
                        root_id: root_id.clone(),
                        score: *weight,
                        latest: *latest,
                        matched: BTreeSet::from([index]),
                    },
                );
            }
        } else {
            ranked.retain(|id, entry| {
                let Some(&(weight, latest, _)) = term_hits.get(id) else {
                    return false;
                };
                entry.score += weight;
                entry.latest = entry.latest.max(latest);
                entry.matched.insert(index);
                true
            });
        }
    }
    let mut ordered: Vec<(String, Ranked)> = ranked
        .into_iter()
        .filter(|(id, hit)| {
            !current_interaction.contains(&id.as_str())
                && hit.matched.len() == query.terms.len()
                && after.as_ref().is_none_or(|(score, at, last_id)| {
                    hit.score < *score
                        || (hit.score == *score
                            && (hit.latest < *at || (hit.latest == *at && id > last_id)))
                })
        })
        .collect();
    ordered.sort_by(|(left_id, left), (right_id, right)| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.latest.cmp(&left.latest))
            .then_with(|| left_id.cmp(right_id))
    });
    let has_more = ordered.len() > SEARCH_PAGE;
    ordered.truncate(SEARCH_PAGE);
    if ordered.is_empty() {
        return Ok(SearchPage {
            interactions: Vec::new(),
            has_more: false,
            unindexed,
            truncated_terms,
        });
    }
    let ids: Vec<String> = ordered.iter().map(|(id, _)| id.clone()).collect();
    let gap_ids: std::collections::BTreeSet<String> = {
        fn build<DB: Database>(builder: &mut QueryBuilder<DB>, ids: &[String])
        where
            DB::Arguments: Default,
            String: for<'q> Encode<'q, DB> + Type<DB>,
        {
            builder
                .push("SELECT id FROM interaction_observations WHERE observation_gap AND id IN (");
            let mut separated = builder.separated(",");
            for id in ids {
                separated.push_bind(id.clone());
            }
            separated.push_unseparated(")");
        }
        match pool {
            SearchPool::Sqlite(pool) => {
                let mut builder = QueryBuilder::<sqlx::Sqlite>::new("");
                build(&mut builder, &ids);
                builder
                    .build_query_scalar()
                    .fetch_all(pool)
                    .await?
                    .into_iter()
                    .collect()
            }
            SearchPool::Postgres(pool) => {
                let mut builder = QueryBuilder::<sqlx::Postgres>::new("");
                build(&mut builder, &ids);
                builder
                    .build_query_scalar()
                    .fetch_all(pool)
                    .await?
                    .into_iter()
                    .collect()
            }
        }
    };
    let segment_rows = match pool {
        SearchPool::Sqlite(pool) => {
            sqlite_segments(pool, principal, active_since, scope, query, &ids).await?
        }
        SearchPool::Postgres(pool) => {
            postgres_segments(pool, principal, active_since, scope, query, &ids).await?
        }
    };
    let mut segments_by_id: HashMap<String, Vec<SearchSegment>> = HashMap::new();
    for (row, terms) in segment_rows {
        segments_by_id
            .entry(row.interaction_id.clone())
            .or_default()
            .push(SearchSegment {
                event_sequence: row.event_sequence,
                segment_ordinal: row.segment_ordinal,
                byte_start: row.byte_start,
                byte_end: row.byte_end,
                occurred_at: row.occurred_at,
                terms,
                role_weight: row.role_weight,
            });
    }
    let interactions = ordered
        .into_iter()
        .map(|(id, rank)| {
            let mut segments = segments_by_id.remove(&id).unwrap_or_default();
            segments.sort_by(|left, right| {
                right
                    .terms
                    .len()
                    .cmp(&left.terms.len())
                    .then_with(|| right.occurred_at.cmp(&left.occurred_at))
                    .then_with(|| right.event_sequence.cmp(&left.event_sequence))
                    .then_with(|| right.segment_ordinal.cmp(&left.segment_ordinal))
                    .then_with(|| left.byte_start.cmp(&right.byte_start))
                    .then_with(|| left.byte_end.cmp(&right.byte_end))
            });
            segments.dedup_by(|left, right| {
                left.event_sequence == right.event_sequence
                    && left.segment_ordinal == right.segment_ordinal
                    && left.byte_start == right.byte_start
                    && left.byte_end == right.byte_end
            });
            let has_observation_gap = gap_ids.contains(&id);
            SearchInteraction {
                id,
                root_id: rank.root_id,
                has_observation_gap,
                score: rank.score,
                latest_hit_at: rank.latest,
                segments,
            }
        })
        .collect();
    Ok(SearchPage {
        interactions,
        has_more,
        unindexed,
        truncated_terms,
    })
}

/// 段表中的一行，两个后端共用。
#[derive(Debug, FromRow)]
struct SegmentRow {
    id: i64,
    role_weight: i32,
    interaction_id: String,
    event_sequence: i64,
    segment_ordinal: i64,
    occurred_at: i64,
    byte_start: i64,
    byte_end: i64,
}

fn segment_query<DB: Database>(
    builder: &mut QueryBuilder<DB>,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    ids: &[String],
) where
    DB::Arguments: Default,
    String: for<'q> Encode<'q, DB> + Type<DB>,
    i64: for<'q> Encode<'q, DB> + Type<DB>,
{
    common_predicates(builder, principal, active_since, scope);
    builder.push(" AND s.interaction_id IN (");
    let mut separated = builder.separated(",");
    for id in ids {
        separated.push_bind(id.clone());
    }
    separated.push_unseparated(")");
}

async fn sqlite_segments(
    pool: &SqlitePool,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    query: &ParsedQuery,
    ids: &[String],
) -> anyhow::Result<Vec<(SegmentRow, Vec<usize>)>> {
    let mut result = Vec::new();
    for (index, term) in query.terms.iter().enumerate() {
        let mut builder = QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT s.id,s.role_weight,s.interaction_id,s.event_sequence,s.segment_ordinal,s.occurred_at,s.byte_start,s.byte_end FROM history_search_segments_fts JOIN history_search_segments s ON s.id=history_search_segments_fts.rowid JOIN interaction_observations i ON i.id=s.interaction_id WHERE history_search_segments_fts MATCH ",
        );
        builder.push_bind(sqlite_match(term));
        segment_query(&mut builder, principal, active_since, scope, ids);
        let query = builder.build_query_as::<SegmentRow>();
        let rows = query.fetch_all(pool).await?;
        for row in rows {
            result.push((row, vec![index]));
        }
    }
    Ok(merge_segments(result))
}

async fn postgres_segments(
    pool: &PgPool,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    query: &ParsedQuery,
    ids: &[String],
) -> anyhow::Result<Vec<(SegmentRow, Vec<usize>)>> {
    let mut result = Vec::new();
    for (index, term) in query.terms.iter().enumerate() {
        let mut builder = QueryBuilder::<sqlx::Postgres>::new(
            "SELECT s.id,s.role_weight,s.interaction_id,s.event_sequence,s.segment_ordinal,s.occurred_at,s.byte_start,s.byte_end FROM history_search_segments s JOIN interaction_observations i ON i.id=s.interaction_id WHERE s.tokens @@ ",
        );
        builder.push_bind(postgres_tsquery(term)).push("::tsquery");
        segment_query(&mut builder, principal, active_since, scope, ids);
        let query = builder.build_query_as::<SegmentRow>();
        let rows = query.fetch_all(pool).await?;
        for row in rows {
            result.push((row, vec![index]));
        }
    }
    Ok(merge_segments(result))
}

/// 两个后端把被多个词项命中的同一段合并；段 id 是后端内稳定且唯一的坐标。
fn merge_segments(rows: Vec<(SegmentRow, Vec<usize>)>) -> Vec<(SegmentRow, Vec<usize>)> {
    let mut merged: BTreeMap<i64, (SegmentRow, Vec<usize>)> = BTreeMap::new();
    for (row, terms) in rows {
        let entry = merged.entry(row.id).or_insert_with(|| {
            let terms = Vec::new();
            (row, terms)
        });
        for term in terms {
            if !entry.1.contains(&term) {
                entry.1.push(term);
            }
        }
    }
    merged
        .into_values()
        .map(|(row, mut terms)| {
            terms.sort_unstable();
            (row, terms)
        })
        .collect()
}

#[cfg(test)]
#[path = "count_unindexed_tests.rs"]
mod count_unindexed_tests;

#[cfg(test)]
mod tests {
    use super::parse_query;
    use super::{SearchScope, search};
    use sqlx::{PgPool, SqlitePool};

    struct SearchFixture {
        observation: crate::interaction_observation::InteractionObservation,
        backend: Backend,
        _directory: tempfile::TempDir,
    }

    enum Backend {
        Sqlite(SqlitePool),
        Postgres {
            pool: PgPool,
            admin: PgPool,
            schema: String,
        },
    }

    impl SearchFixture {
        async fn sqlite() -> anyhow::Result<Self> {
            let pool = crate::test_support::migrated_sqlite_pool().await?;
            let directory = tempfile::tempdir()?;
            let observation = crate::interaction_observation::InteractionObservation::new(
                Some(pool.clone()),
                None,
                directory.path().to_path_buf(),
                crate::history_retention::HistoryRetention::new(1),
                true,
                crate::generation_chain::test_chain().await,
            )
            .await;
            Ok(Self {
                observation,
                backend: Backend::Sqlite(pool),
                _directory: directory,
            })
        }

        async fn postgres() -> anyhow::Result<Self> {
            let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
                .expect("set STRAVIA_TEST_POSTGRES_URL to an isolated test database");
            let directory = tempfile::tempdir()?;
            let admin = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await?;
            let schema = format!("stravia_search_test_{}", uuid::Uuid::new_v4().simple());
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&admin)
                .await?;
            let options: sqlx::postgres::PgConnectOptions = url.parse()?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .connect_with(options.options([("search_path", schema.as_str())]))
                .await?;
            crate::migrations::migrate_postgres(&pool, None).await?;
            let observation = crate::interaction_observation::InteractionObservation::new(
                None,
                Some(pool.clone()),
                directory.path().to_path_buf(),
                crate::history_retention::HistoryRetention::new(1),
                true,
                crate::generation_chain::test_chain().await,
            )
            .await;
            Ok(Self {
                observation,
                backend: Backend::Postgres {
                    pool,
                    admin,
                    schema,
                },
                _directory: directory,
            })
        }

        async fn execute(&self, sql: &'static str) -> anyhow::Result<()> {
            match &self.backend {
                Backend::Sqlite(pool) => {
                    let mut tx = pool.begin_write().await?;
                    sqlx::raw_sql(sql).execute(&mut *tx).await?;
                    tx.commit().await?;
                }
                Backend::Postgres { pool, .. } => {
                    sqlx::raw_sql(sql).execute(pool).await?;
                }
            }
            Ok(())
        }

        async fn close(self) -> anyhow::Result<()> {
            self.observation.shutdown().await;
            match self.backend {
                Backend::Sqlite(pool) => pool.close().await,
                Backend::Postgres {
                    pool,
                    admin,
                    schema,
                } => {
                    pool.close().await;
                    let result =
                        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                            .execute(&admin)
                            .await;
                    admin.close().await;
                    result?;
                }
            }
            Ok(())
        }
    }

    /// 两个 Interaction 有相同命中词；第三个只缺进度，用来验证搜索如实报告索引滞后。
    async fn limitations_contract(fixture: &SearchFixture) -> anyhow::Result<()> {
        fixture.execute("INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at,last_event_sequence) VALUES ('current','owner','current','current-run','route','completed',1000,1000,1),('past','owner','past','past-run','route','completed',1000,1000,1),('lag','owner','lag','lag-run','route','completed',1000,1000,1); INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,client_output_committed) VALUES ('current-run','current','responses','route','completed',false,1000,1000,true),('past-run','past','responses','route','completed',false,1000,1000,true),('lag-run','lag','responses','route','completed',false,1000,1000,true)").await?;
        match &fixture.backend {
            Backend::Sqlite(pool) => {
                let mut tx = pool.begin_write().await?;
                for (id, event) in [("current", 1_i64), ("past", 2_i64)] {
                    let segment: i64 = sqlx::query_scalar("INSERT INTO history_search_segments(interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end) VALUES ($1,'owner',$1,$2,0,3,1000,0,6) RETURNING id")
                        .bind(id).bind(event).fetch_one(&mut *tx).await?;
                    sqlx::query("INSERT INTO history_search_segments_fts(rowid,tokens) VALUES ($1,'needle')").bind(segment).execute(&mut *tx).await?;
                }
                sqlx::query("INSERT INTO history_search_progress(interaction_id,last_event_sequence,indexed_terminal) VALUES ('current',1,1),('past',1,1)").execute(&mut *tx).await?;
                tx.commit().await?;
            }
            Backend::Postgres { pool, .. } => {
                sqlx::query("INSERT INTO history_search_segments(interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end,tokens) VALUES ('current','owner','current',1,0,3,1000,0,6,'needle:1'),('past','owner','past',2,0,3,1000,0,6,'needle:1')").execute(pool).await?;
                sqlx::query("INSERT INTO history_search_progress(interaction_id,last_event_sequence,indexed_terminal) VALUES ('current',1,true),('past',1,true)").execute(pool).await?;
            }
        }
        let query = parse_query("needle").unwrap();
        let page = search(
            &fixture.observation,
            "owner",
            0,
            SearchScope::Root,
            &query,
            None,
            &["current"],
        )
        .await?;
        assert_eq!(
            page.interactions
                .iter()
                .map(|hit| hit.id.as_str())
                .collect::<Vec<_>>(),
            ["past"]
        );
        assert_eq!(
            page.unindexed, 1,
            "the issuing Interaction is fully indexed, only lag is pending"
        );
        assert!(page.truncated_terms.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn sqlite_search_reports_index_lag_and_excludes_issuing_interaction() -> anyhow::Result<()>
    {
        let fixture = SearchFixture::sqlite().await?;
        limitations_contract(&fixture).await?;
        fixture.close().await
    }

    #[tokio::test]
    #[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test database"]
    async fn postgres_search_reports_index_lag_and_excludes_issuing_interaction()
    -> anyhow::Result<()> {
        let fixture = SearchFixture::postgres().await?;
        limitations_contract(&fixture).await?;
        fixture.close().await
    }

    /// 词项候选上限只计入当前 Principal 在回看时限内、可列出的 Interaction：过期与 MCP 根
    /// Interaction 命中且得分最高，也不能占用上限或出现在结果中。`broad` 命中 2,001 个可列出的
    /// Interaction，超过上限；`edge` 恰好命中 2,000 个，不算过宽。
    async fn candidate_cap_contract(fixture: &SearchFixture) -> anyhow::Result<()> {
        fixture.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<2001) INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at,last_event_sequence) SELECT 'i'||x,'owner','i'||x,'r'||x,'route','completed',1000,1000+x,1 FROM n; WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<2001) INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,client_output_committed) SELECT 'r'||x,'i'||x,'responses','route','completed',false,1000,1000+x,true FROM n; INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at,last_event_sequence) VALUES ('expired','owner','expired','expired-run','route','completed',800,800,1),('mcp','owner','mcp','mcp-run','route','completed',5000,5000,1); INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,client_output_committed) VALUES ('expired-run','expired','responses','route','completed',false,800,800,true),('mcp-run','mcp','mcp','route','completed',false,5000,5000,true)").await?;
        match &fixture.backend {
            Backend::Sqlite(pool) => {
                let mut tx = pool.begin_write().await?;
                sqlx::raw_sql("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<2001) INSERT INTO history_search_segments(interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end) SELECT 'i'||x,'owner','i'||x,x,0,1,1000+x,0,10 FROM n; INSERT INTO history_search_segments(interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end) VALUES ('expired','owner','expired',3001,0,3,800,0,10),('mcp','owner','mcp',3002,0,3,5000,0,10); INSERT INTO history_search_segments_fts(rowid,tokens) SELECT id,CASE WHEN event_sequence=2001 THEN 'broad' ELSE 'broad edge' END FROM history_search_segments").execute(&mut *tx).await?;
                tx.commit().await?;
            }
            Backend::Postgres { pool, .. } => {
                sqlx::raw_sql("INSERT INTO history_search_segments(interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end,tokens) SELECT 'i'||x,'owner','i'||x,x,0,1,1000+x,0,10,CASE WHEN x=2001 THEN 'broad:1'::tsvector ELSE 'broad:1 edge:2'::tsvector END FROM generate_series(1,2001) x; INSERT INTO history_search_segments(interaction_id,principal,root_id,event_sequence,segment_ordinal,role_weight,occurred_at,byte_start,byte_end,tokens) VALUES ('expired','owner','expired',3001,0,3,800,0,10,'broad:1 edge:2'),('mcp','owner','mcp',3002,0,3,5000,0,10,'broad:1 edge:2')").execute(pool).await?;
            }
        }
        // 词项按用户原文回显（大小写、全角不归一化）。
        let query = parse_query("ＢＲＯＡＤ edge").unwrap();
        let page = search(
            &fixture.observation,
            "owner",
            900,
            SearchScope::Root,
            &query,
            None,
            &[],
        )
        .await?;
        assert_eq!(page.truncated_terms, ["ＢＲＯＡＤ"]);
        // `broad` 保留最近的 2,000 个（i2..i2001），与 `edge` 的交集按最近命中倒序分页。
        assert_eq!(
            page.interactions
                .iter()
                .map(|hit| hit.id.as_str())
                .collect::<Vec<_>>(),
            (1991..=2000)
                .rev()
                .map(|x| format!("i{x}"))
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn sqlite_term_cap_counts_only_listable_interactions() -> anyhow::Result<()> {
        let fixture = SearchFixture::sqlite().await?;
        candidate_cap_contract(&fixture).await?;
        fixture.close().await
    }

    #[tokio::test]
    #[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test database"]
    async fn postgres_term_cap_counts_only_listable_interactions() -> anyhow::Result<()> {
        let fixture = SearchFixture::postgres().await?;
        candidate_cap_contract(&fixture).await?;
        fixture.close().await
    }
}
