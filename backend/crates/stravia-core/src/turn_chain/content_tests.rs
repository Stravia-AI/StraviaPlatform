use super::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

async fn execute(store: &SqlTurnChainStore, sql: &str) {
    match store {
        SqlTurnChainStore::Sqlite(pool) => {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
                .execute(pool)
                .await
                .unwrap();
        }
        SqlTurnChainStore::Postgres(pool) => {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

async fn scalar(store: &SqlTurnChainStore, sql: &str) -> i64 {
    match store {
        SqlTurnChainStore::Sqlite(pool) => sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(pool)
            .await
            .unwrap(),
        SqlTurnChainStore::Postgres(pool) => {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(pool)
                .await
                .unwrap()
        }
    }
}

fn payload() -> Value {
    let instructions = "保留原始 instructions\n e\u{301} ".repeat(1000);
    let tools = json!([{"name":"tool","parameters":{"description":"schema ".repeat(4000)}}]);
    let profile = json!({"instructions":instructions,"tools":tools,"extra":"opaque ".repeat(100)});
    json!({
        "effective_system":instructions,
        "effective_request":{"instructions":instructions,"tools":tools},
        "effective_output":{"items":[{"unknown":"do not discard"}],"vendor":{"ingress":{
            "__open_responses_response_profile":profile,
            "__open_responses_effective_request":profile
        }}},
        "unclassified":{"data":null,"references":10,"trace_storage":1}
    })
}

async fn contract(store: SqlTurnChainStore) {
    let original = payload();
    let mut different = original.clone();
    different["effective_output"]["vendor"]["ingress"]["__open_responses_effective_request"]["extra"] =
        json!("different ".repeat(120));
    let owner = Principal::new("dedup-owner");
    let other = Principal::new("other-owner");
    let root = TurnNodeId::response();
    let sibling = TurnNodeId::response();
    for (id, principal, value) in [(&root, &owner, &original), (&sibling, &owner, &different)] {
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: principal.clone(),
                payload_version: 6,
                payload: value.clone(),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .unwrap();
    }
    let single_owner_content = scalar(&store, "SELECT COUNT(*) FROM turn_chain_contents").await;
    let foreign = TurnNodeId::response();
    store
        .commit(TurnCommit {
            id: foreign.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: other.clone(),
            payload_version: 6,
            payload: original.clone(),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .unwrap();
    assert!(
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_contents").await > single_owner_content
    );
    for (id, principal, expected) in [
        (&root, &owner, &original),
        (&sibling, &owner, &different),
        (&foreign, &other, &original),
    ] {
        assert_eq!(
            &store
                .materialize(principal, TurnNodeKind::Response, id)
                .await
                .unwrap()[0]
                .payload,
            expected
        );
    }
    assert!(
        store
            .materialize(&other, TurnNodeKind::Response, &root)
            .await
            .is_err()
    );
    let stored = scalar(
        &store,
        "SELECT COALESCE(SUM(length(payload)),0) FROM turn_chain_nodes",
    )
    .await
        + scalar(
            &store,
            "SELECT COALESCE(SUM(length(content)),0) FROM turn_chain_contents",
        )
        .await;
    let raw = serde_json::to_vec(&original).unwrap().len() * 2
        + serde_json::to_vec(&different).unwrap().len();
    assert!(
        stored < (raw / 2) as i64,
        "duplicate content must not scale with its occurrences"
    );

    let branch_a = TurnNodeId::response();
    let branch_b = TurnNodeId::response();
    for (id, payload) in [(&branch_a, &original), (&branch_b, &different)] {
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Response,
                parent_id: Some(root.clone()),
                principal: owner.clone(),
                payload_version: 6,
                payload: payload.clone(),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .unwrap();
    }
    execute(
        &store,
        &format!(
            "UPDATE turn_chain_nodes SET expires_at=0 WHERE id='{}'",
            branch_a.as_str()
        ),
    )
    .await;
    assert_eq!(store.sweep_expired().await.unwrap(), 1);
    let branch = store
        .materialize(&owner, TurnNodeKind::Response, &branch_b)
        .await
        .unwrap();
    assert_eq!(
        branch.iter().map(|node| &node.payload).collect::<Vec<_>>(),
        [&original, &different]
    );
    execute(
        &store,
        &format!(
            "UPDATE turn_chain_nodes SET expires_at=0 WHERE id='{}'",
            branch_b.as_str()
        ),
    )
    .await;
    assert_eq!(store.sweep_expired().await.unwrap(), 1);

    execute(
        &store,
        &format!(
            "UPDATE turn_chain_nodes SET expires_at=0 WHERE id='{}'",
            root.as_str()
        ),
    )
    .await;
    assert_eq!(store.sweep_expired().await.unwrap(), 1);
    assert_eq!(
        store
            .materialize(&owner, TurnNodeKind::Response, &sibling)
            .await
            .unwrap()[0]
            .payload,
        different
    );
    execute(&store, "UPDATE turn_chain_nodes SET expires_at=0").await;
    assert_eq!(store.sweep_expired().await.unwrap(), 2);
    assert_eq!(
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_contents").await,
        0
    );
    assert_eq!(
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_content_refs").await,
        0
    );

    store
        .commit(TurnCommit {
            id: root.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: original,
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .unwrap();
    execute(&store, "UPDATE turn_chain_contents SET content='null'").await;
    assert!(
        store
            .materialize(&owner, TurnNodeKind::Response, &root)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn structural_history_sqlite_contract() {
    contract(super::test_store().await).await;
}

/// Restoring a short chain must visit its own references, not all references
/// belonging to the same Principal. The progress budget covers the real store
/// materialization path while allowing the unrelated rows to be seeded first.
#[tokio::test]
async fn optimization_sqlite_history_restore_ignores_unrelated_references() {
    const UNRELATED_REFERENCES: i64 = 10_000;
    const OPS_PER_TICK: i32 = 1_000;
    const TICK_BUDGET: u32 = 20;

    let store = super::test_store().await;
    let owner = Principal::new("content-restore-owner");
    let root = TurnNodeId::response();
    let child = TurnNodeId::response();
    let shared = payload();
    let plain = json!({"message": "a node without references"});
    for (id, parent_id, value) in [(&root, None, &shared), (&child, Some(root.clone()), &plain)] {
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Response,
                parent_id,
                principal: owner.clone(),
                payload_version: 6,
                payload: value.clone(),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .expect("commit test chain");
    }

    let SqlTurnChainStore::Sqlite(pool) = &store else {
        unreachable!("SQLite test store")
    };
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
         INSERT INTO turn_chain_nodes \
         (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) \
         SELECT 'unrelated-ref-' || i, ?, NULL, ?, 6, '{}', ?, ? FROM n",
    )
    .bind(UNRELATED_REFERENCES)
    .bind(TurnNodeKind::Response.as_str())
    .bind(owner.continuation_key())
    .bind(now)
    .bind(now + 60_000)
    .execute(pool)
    .await
    .expect("seed unrelated nodes");
    sqlx::query(
        "INSERT INTO turn_chain_content_refs (node_id, principal, path, content_key) \
         SELECT n.id, n.principal, '/effective_system', r.content_key \
         FROM turn_chain_nodes n CROSS JOIN turn_chain_content_refs r \
         WHERE n.id LIKE 'unrelated-ref-%' AND r.node_id = ? AND r.path = '/effective_system'",
    )
    .bind(root.as_str())
    .execute(pool)
    .await
    .expect("seed unrelated references");
    assert!(
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_content_refs").await
            >= UNRELATED_REFERENCES,
        "test requires many Principal-owned references"
    );

    let ticks = Arc::new(AtomicU32::new(0));
    {
        let mut connection = pool.acquire().await.expect("SQLite connection");
        let measured_ticks = ticks.clone();
        connection
            .lock_handle()
            .await
            .expect("SQLite handle")
            .set_progress_handler(OPS_PER_TICK, move || {
                measured_ticks.fetch_add(1, Ordering::Relaxed) < TICK_BUDGET
            });
    }
    let nodes = store
        .materialize(&owner, TurnNodeKind::Response, &child)
        .await
        .expect("restore within VM budget");
    assert_eq!(
        nodes.iter().map(|node| &node.payload).collect::<Vec<_>>(),
        [&shared, &plain]
    );
}

#[tokio::test]
async fn structural_history_postgres_contract_when_configured() {
    let Some(url) = std::env::var("DB_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())
    else {
        return;
    };
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("stravia_content_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    crate::migrations::migrate_postgres(&pool).await.unwrap();
    contract(SqlTurnChainStore::postgres(pool.clone())).await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
