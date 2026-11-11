use super::{SearchScope, count_unindexed_query};
use sqlx::{QueryBuilder, SqlitePool};

#[test]
fn count_filters_are_materialized_before_eligibility_on_both_backends() {
    fn assert_shape(sql: &str) {
        let (candidates, eligibility) = sql
            .split_once(") SELECT COUNT(*) FROM candidates i WHERE (")
            .unwrap();
        assert!(candidates.starts_with("WITH candidates AS MATERIALIZED ("));
        for filter in [
            "i.principal=",
            "i.last_active_at>",
            "i.root_id=",
            "i.id=",
            "i.id NOT IN (",
            "p.interaction_id IS NULL",
            "i.last_event_sequence<>p.last_event_sequence",
            "p.indexed_terminal",
        ] {
            assert!(candidates.contains(filter), "missing {filter}: {sql}");
        }
        assert!(!candidates.contains("inference_run_observations"));
        assert!(eligibility.contains("inference_run_observations"));
        assert!(!sql.contains(" LIMIT "));
    }
    let mut sqlite = QueryBuilder::<sqlx::Sqlite>::new("");
    count_unindexed_query(
        &mut sqlite,
        "owner",
        10,
        SearchScope::Interaction("root", "id"),
        &["current"],
    );
    assert_shape(sqlite.sql().as_str());
    let mut postgres = QueryBuilder::<sqlx::Postgres>::new("");
    count_unindexed_query(
        &mut postgres,
        "owner",
        10,
        SearchScope::Interaction("root", "id"),
        &["current"],
    );
    assert_shape(postgres.sql().as_str());
}

async fn count(pool: &SqlitePool, scope: SearchScope<'_>, current: &[&str]) -> i64 {
    let mut builder = QueryBuilder::<sqlx::Sqlite>::new("");
    count_unindexed_query(&mut builder, "owner", 10, scope, current);
    builder.build_query_scalar().fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn count_preserves_filters_inconsistencies_and_unlimited_results() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::raw_sql("CREATE TABLE interaction_observations (id TEXT PRIMARY KEY, principal TEXT, root_id TEXT, root_run_id TEXT, status TEXT, last_active_at INTEGER, last_event_sequence INTEGER);
        CREATE TABLE history_search_progress (interaction_id TEXT PRIMARY KEY, last_event_sequence INTEGER, indexed_terminal BOOLEAN);
        CREATE TABLE inference_run_observations (id TEXT PRIMARY KEY, interaction_id TEXT, ingress_protocol TEXT, client_output_committed BOOLEAN, status TEXT, finished_at INTEGER, failure_json TEXT, terminal_reason TEXT);
        INSERT INTO inference_run_observations (id,ingress_protocol) VALUES ('run','http'),('mcp','mcp');
        INSERT INTO interaction_observations VALUES
        ('missing','owner','root','run','running',11,1),
        ('behind','owner','root','run','running',11,2),
        ('terminal','owner','root','run','completed',11,1),
        ('current','owner','root','run','running',11,1),
        ('equal','owner','root','run','running',11,1),
        ('nullseq','owner','root','run','running',11,NULL),
        ('foreign','other','root','run','running',11,1),
        ('expired','owner','root','run','running',10,1),
        ('otherroot','owner','other','run','running',11,1),
        ('ineligible','owner','root','mcp','running',11,1);
        INSERT INTO history_search_progress VALUES ('behind',1,0),('terminal',1,0),('equal',1,0),('nullseq',1,0);
        WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<110)
        INSERT INTO interaction_observations SELECT 'bulk'||x,'owner','root','run','running',11,1 FROM n;")
        .execute(&pool).await.unwrap();
    assert_eq!(
        count(&pool, SearchScope::Chain("root"), &["current"]).await,
        113
    );
    assert_eq!(count(&pool, SearchScope::Root, &["current"]).await, 114);
    assert_eq!(
        count(&pool, SearchScope::Interaction("root", "behind"), &[]).await,
        1
    );
    assert_eq!(
        count(&pool, SearchScope::Interaction("root", "nullseq"), &[]).await,
        0
    );
    assert_eq!(
        count(&pool, SearchScope::Interaction("other", "behind"), &[]).await,
        0
    );
    assert_eq!(
        count(
            &pool,
            SearchScope::Interaction("root", "behind"),
            &["behind"]
        )
        .await,
        0
    );
}
