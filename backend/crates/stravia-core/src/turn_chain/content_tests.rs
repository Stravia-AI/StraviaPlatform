use super::*;
use serde_json::{Value, json};

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

    // 把一条旧格式节点插入真实存储，验证迁移保持未知字段、身份和保留期。
    let legacy = TurnNodeId::response();
    let text = serde_json::to_string(&original).unwrap();
    let deadline = chrono::Utc::now().timestamp_millis() + 60_000;
    macro_rules! legacy {
        ($pool:expr) => { sqlx::query("INSERT INTO turn_chain_nodes (id,kind,principal,payload_version,payload,created_at,expires_at) VALUES ($1,'response',$2,6,$3,0,$4)")
            .bind(legacy.as_str()).bind(owner.continuation_key()).bind(&text).bind(deadline).execute($pool).await.unwrap() };
    }
    match &store {
        SqlTurnChainStore::Sqlite(pool) => {
            legacy!(pool);
        }
        SqlTurnChainStore::Postgres(pool) => {
            legacy!(pool);
        }
    }
    assert_eq!(store.optimize_storage().await.unwrap(), 1);
    assert_eq!(store.optimize_storage().await.unwrap(), 0);
    assert_eq!(
        store
            .materialize(&owner, TurnNodeKind::Response, &legacy)
            .await
            .unwrap()[0]
            .payload,
        original
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
    assert_eq!(store.sweep_expired().await.unwrap(), 3);
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
