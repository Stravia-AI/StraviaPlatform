//! 历史检索的存储适配器：每个词项分别查询命中 Interaction，再按统一规则交集、排序与选取命中段。
//!
//! SQLite 通过 FTS5 的 rowid 取得段，PostgreSQL 通过 `tsvector` 字面量与 GIN 索引取得段；
//! 两者都只按段行的主键回查元数据，分词与查询短语都由 Rust 生成，不依赖文本搜索配置。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sqlx::{Database, Encode, FromRow, PgPool, QueryBuilder, SqlitePool, Type};

#[cfg(test)]
use super::search_text::parse_query;
use super::search_text::{ParsedQuery, QueryTerm, Token};
use crate::interaction_observation::{InteractionObservation, eligible};

const TERM_INTERACTION_LIMIT: i64 = 2_000;
const SEARCH_PAGE: usize = 10;
const SEGMENTS_PER_INTERACTION: usize = 3;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchInteraction {
    pub id: String,
    pub root_id: String,
    pub score: i32,
    pub latest_hit_at: i64,
    pub segments: Vec<SearchSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchPage {
    pub interactions: Vec<SearchInteraction>,
    pub has_more: bool,
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
            builder.push("WITH hits AS MATERIALIZED (SELECT rowid FROM history_search_segments_fts WHERE history_search_segments_fts MATCH ")
                .push_bind(sqlite_match(term))
                .push(") SELECT s.interaction_id,s.root_id,CAST(MAX(s.role_weight) AS BIGINT) AS weight,MAX(s.occurred_at) AS latest_hit_at FROM hits JOIN history_search_segments s ON s.id=hits.rowid JOIN interaction_observations i ON i.id=s.interaction_id WHERE 1=1");
        }
        SearchBackend::Postgres => {
            builder.push("SELECT s.interaction_id,s.root_id,CAST(MAX(s.role_weight) AS BIGINT) AS weight,MAX(s.occurred_at) AS latest_hit_at FROM history_search_segments s JOIN interaction_observations i ON i.id=s.interaction_id WHERE s.tokens @@ ")
                .push_bind(postgres_tsquery(term))
                .push("::tsquery");
        }
    }
    common_predicates(builder, principal, active_since, scope);
    builder.push(" GROUP BY s.interaction_id,s.root_id ORDER BY MAX(s.occurred_at) DESC,s.interaction_id LIMIT ")
        .push(TERM_INTERACTION_LIMIT);
}

#[cfg(test)]
fn term_query_sql(backend: SearchBackend, scope: SearchScope<'_>) -> String {
    match backend {
        SearchBackend::Sqlite => {
            let mut builder = QueryBuilder::<sqlx::Sqlite>::new("");
            term_query(
                &mut builder,
                backend,
                "principal",
                0,
                scope,
                &parse_query("run_admitted").unwrap().terms[0],
            );
            builder.sql().as_str().to_owned()
        }
        SearchBackend::Postgres => {
            let mut builder = QueryBuilder::<sqlx::Postgres>::new("");
            term_query(
                &mut builder,
                backend,
                "principal",
                0,
                scope,
                &parse_query("run_admitted").unwrap().terms[0],
            );
            builder.sql().as_str().to_owned()
        }
    }
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

pub(super) async fn search(
    observation: &InteractionObservation,
    principal: &str,
    active_since: i64,
    scope: SearchScope<'_>,
    query: &ParsedQuery,
    after: Option<SearchCursor>,
) -> anyhow::Result<SearchPage> {
    let (sqlite, postgres) = observation.search_pools();
    let mut ranked: BTreeMap<String, Ranked> = BTreeMap::new();
    for (index, term) in query.terms.iter().enumerate() {
        let rows = if let Some(pool) = sqlite {
            sqlite_term_hits(pool, principal, active_since, scope, term).await?
        } else if let Some(pool) = postgres {
            postgres_term_hits(pool, principal, active_since, scope, term).await?
        } else {
            anyhow::bail!("history search has no observation SQL backend");
        };
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
            hit.matched.len() == query.terms.len()
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
        });
    }
    let ids: Vec<String> = ordered.iter().map(|(id, _)| id.clone()).collect();
    let segment_rows = if let Some(pool) = sqlite {
        sqlite_segments(pool, principal, active_since, scope, query, &ids).await?
    } else if let Some(pool) = postgres {
        postgres_segments(pool, principal, active_since, scope, query, &ids).await?
    } else {
        Vec::new()
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
            segments.truncate(SEGMENTS_PER_INTERACTION);
            SearchInteraction {
                id,
                root_id: rank.root_id,
                score: rank.score,
                latest_hit_at: rank.latest,
                segments,
            }
        })
        .collect();
    Ok(SearchPage {
        interactions,
        has_more,
    })
}

/// 段表中的一行，两个后端共用。
#[derive(Debug, FromRow)]
struct SegmentRow {
    id: i64,
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
            "SELECT s.id,s.interaction_id,s.event_sequence,s.segment_ordinal,s.occurred_at,s.byte_start,s.byte_end FROM history_search_segments_fts JOIN history_search_segments s ON s.id=history_search_segments_fts.rowid JOIN interaction_observations i ON i.id=s.interaction_id WHERE history_search_segments_fts MATCH ",
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
            "SELECT s.id,s.interaction_id,s.event_sequence,s.segment_ordinal,s.occurred_at,s.byte_start,s.byte_end FROM history_search_segments s JOIN interaction_observations i ON i.id=s.interaction_id WHERE s.tokens @@ ",
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
mod tests {
    use super::parse_query;
    use super::{
        SearchBackend, SearchScope, TERM_INTERACTION_LIMIT, postgres_tsquery, sqlite_match,
        term_query_sql,
    };

    #[test]
    fn term_queries_cap_filtered_interactions_in_both_dialects() {
        for backend in [SearchBackend::Sqlite, SearchBackend::Postgres] {
            for scope in [
                SearchScope::Root,
                SearchScope::Chain("root"),
                SearchScope::Interaction("root", "interaction"),
            ] {
                let sql = term_query_sql(backend, scope);
                assert_eq!(sql.matches("ORDER BY").count(), 1);
                assert!(sql.contains(&format!("GROUP BY s.interaction_id,s.root_id ORDER BY MAX(s.occurred_at) DESC,s.interaction_id LIMIT {TERM_INTERACTION_LIMIT}")));
                let where_end = sql.find("GROUP BY").expect("grouping");
                let where_sql = &sql[..where_end];
                assert!(where_sql.contains("s.principal="));
                assert!(where_sql.contains("i.last_active_at>"));
                assert!(where_sql.contains("inference_run_observations"));
                if let SearchScope::Chain(_) = scope {
                    assert!(where_sql.contains("s.root_id="));
                }
                if let SearchScope::Interaction(_, _) = scope {
                    assert!(
                        where_sql.contains("s.root_id=") && where_sql.contains("s.interaction_id=")
                    );
                }
            }
        }
    }

    #[test]
    fn query_expressions_use_unanchored_phrases_and_prefixes() {
        let query = parse_query("\"run admitted\" 中 run_admitted").unwrap();
        assert_eq!(sqlite_match(&query.terms[0]), "\"run admitted\"");
        assert_eq!(sqlite_match(&query.terms[1]), "\"中\"*");
        assert_eq!(sqlite_match(&query.terms[2]), "\"run admitted\"");
        assert_eq!(postgres_tsquery(&query.terms[0]), "'run' <-> 'admitted'");
        assert_eq!(postgres_tsquery(&query.terms[1]), "'中':*");
    }

    #[tokio::test]
    async fn sqlite_search_queries_use_fts_and_keyed_lookups() -> anyhow::Result<()> {
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let query = parse_query("run_admitted 中").unwrap();
        for (index, term) in query.terms.iter().enumerate() {
            let sql = term_query_sql(SearchBackend::Sqlite, SearchScope::Root);
            let detail: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                    .bind(sqlite_match(term))
                    .bind("principal")
                    .bind(0_i64)
                    .fetch_all(&pool)
                    .await?;
            assert!(
                detail.iter().any(|(_, _, _, detail)| {
                    detail.contains("history_search_segments_fts")
                        && detail.contains("VIRTUAL TABLE INDEX")
                }),
                "term {index} must use FTS: {detail:?}"
            );
            assert!(
                detail.iter().any(|(_, _, _, detail)| {
                    detail.contains("SEARCH s USING INTEGER PRIMARY KEY (rowid=?)")
                        || detail.contains("SEARCH s USING INDEX")
                }),
                "term {index} must reach segments by rowid/scope index: {detail:?}"
            );
            assert!(
                detail.iter().all(|(_, _, _, detail)| {
                    !detail.starts_with("SCAN s")
                        && !detail.starts_with("SCAN i")
                        && !detail.starts_with("SCAN interaction_observations")
                        && !detail.contains("inference_runs_status_idx")
                }),
                "term {index} must use keyed lookups: {detail:?}"
            );
        }
        pool.close().await;
        Ok(())
    }
}
