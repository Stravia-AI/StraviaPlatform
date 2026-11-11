use super::{SearchScope, count_unindexed_query};
use sqlx::{QueryBuilder, SqlitePool};

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
