use super::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

async fn execute(store: &SqlTurnChainStore, sql: &str) {
    match store {
        SqlTurnChainStore::Sqlite(pool, gate) => {
            let _write_gate = gate.lock().await;
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
        SqlTurnChainStore::Sqlite(pool, _) => {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(pool)
                .await
                .unwrap()
        }
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
    let item = json!({"role":"user","content":[{"type":"text","text":"context item ".repeat(80)}]});
    let system = "client delta system instructions ".repeat(80);
    json!({
        "client_delta":{"system":system,"messages":[item.clone(),{"role":"user","content":"short"}]},
        "client_output":[item.clone()],
        "client_history_mutation":{"type":"replace","items":[item.clone()]},
        "effective_history_mutation":{"type":"append","items":[item.clone()]},
        "effective_system":instructions,
        "effective_request":{"instructions":instructions,"tools":tools},
        "effective_output":{"items":[{"unknown":"do not discard"},item.clone()],"vendor":{"ingress":{
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
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_node_contents").await,
        0
    );

    // The chain carries 402 distinct contents (one shared plus one per node),
    // so content reads must batch independently of node reads.
    let shared = "shared instructions ".repeat(20);
    let mut expected = Vec::new();
    let mut head = None;
    for index in 0..402 {
        let id = TurnNodeId::response();
        let value = if index == 401 {
            json!({"legacy": "no references"})
        } else {
            json!({
                "effective_system": shared,
                "effective_request": {"instructions": format!("{index}: {}", "unique ".repeat(50))}
            })
        };
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Response,
                parent_id: head,
                principal: owner.clone(),
                payload_version: 6,
                payload: value.clone(),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .unwrap();
        head = Some(id);
        expected.push(value);
    }
    let restored = store
        .materialize(&owner, TurnNodeKind::Response, head.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        restored
            .into_iter()
            .map(|node| node.payload)
            .collect::<Vec<_>>(),
        expected
    );

    // Concurrent commits inserting overlapping new keys resolve through the
    // deterministic insert order plus the targeted fallback lookup: exactly one
    // contents row may exist for the shared key.
    let racing_shared = format!("racing {}", "shared system ".repeat(60));
    let racing_key =
        super::content::content_key(&serde_json::to_vec(&json!(racing_shared)).unwrap());
    let racing = |suffix: &str| {
        json!({
            "client_delta": {"system": racing_shared},
            "effective_output": {"items": [{"role":"assistant","content":[{"type":"text","text": format!("{suffix} {}", "turn output ".repeat(60))}]}]},
        })
    };
    let racing_left = TurnNodeId::response();
    let racing_right = TurnNodeId::response();
    let (left, right) = tokio::join!(
        store.commit(TurnCommit {
            id: racing_left.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: racing("left"),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        }),
        store.commit(TurnCommit {
            id: racing_right.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: racing("right"),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        }),
    );
    left.unwrap();
    right.unwrap();
    assert_eq!(
        scalar(
            &store,
            &format!("SELECT COUNT(*) FROM turn_chain_contents WHERE content_key = '{racing_key}'"),
        )
        .await,
        1,
        "concurrent commits must share one content row"
    );
    assert_eq!(
        store
            .materialize(&owner, TurnNodeKind::Response, &racing_left)
            .await
            .unwrap()[0]
            .payload["client_delta"]["system"],
        json!(racing_shared)
    );

    // A missing content row must fail restore rather than splice wrong bytes.
    let untouched = TurnNodeId::response();
    store
        .commit(TurnCommit {
            id: untouched.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: different.clone(),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .unwrap();
    let victim: i64 = scalar(
        &store,
        &format!(
            "SELECT MIN(content_id) FROM turn_chain_node_contents WHERE node_id = '{}'",
            untouched.as_str()
        ),
    )
    .await;
    execute(
        &store,
        &format!("DELETE FROM turn_chain_node_contents WHERE content_id = {victim}"),
    )
    .await;
    execute(
        &store,
        &format!("DELETE FROM turn_chain_contents WHERE id = {victim}"),
    )
    .await;
    assert!(
        store
            .materialize(&owner, TurnNodeKind::Response, &untouched)
            .await
            .is_err(),
        "missing history content must fail restore"
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
    // Content bytes that survive the codec but fail the content_key digest must
    // still be rejected at restore.
    let victim: i64 = scalar(
        &store,
        &format!(
            "SELECT MIN(content_id) FROM turn_chain_node_contents WHERE node_id = '{}'",
            root.as_str()
        ),
    )
    .await;
    execute(
        &store,
        &format!(
            "UPDATE turn_chain_contents SET content = (SELECT content FROM turn_chain_contents AS c2 WHERE c2.id <> {victim} LIMIT 1) WHERE id = {victim}"
        ),
    )
    .await;
    assert!(
        store
            .materialize(&owner, TurnNodeKind::Response, &root)
            .await
            .is_err(),
        "tampered history content must fail digest verification"
    );
}

#[tokio::test]
async fn complete_items_preserve_order_repeats_and_agent_transcript() {
    let store = super::test_store().await;
    let principal = Principal::new("complete-item-owner");
    let item = json!({"role":"assistant", "parts":[{"type":"thinking", "text":"raw thinking ".repeat(24), "signature":"opaque"}, {"type":"image", "data":"AA=="}], "meta":{"unknown":true}});
    let tool = json!({"role":"tool","content":"ordinary result ".repeat(20)});
    let original = json!({"client":{"items":[item.clone(), item.clone()]},"effective_mutation":{"type":"replace","items":[item.clone()]},"effective_output":{"items":[item.clone()]},"transcript":[item.clone(),tool.clone()],"markers":[{"type":"append","index":2}]});
    for (kind, id) in [
        (TurnNodeKind::Response, TurnNodeId::response()),
        (TurnNodeKind::Agent, TurnNodeId::new("agent-item-test")),
    ] {
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind,
                parent_id: None,
                principal: principal.clone(),
                payload_version: 6,
                payload: original.clone(),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .unwrap();
        assert_eq!(
            store.materialize(&principal, kind, &id).await.unwrap()[0].payload,
            original
        );
    }
    // Six slot references across the payload collapse to two distinct contents;
    // the link table records each node only once per distinct content.
    assert_eq!(
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_contents").await,
        2
    );
    assert_eq!(
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_node_contents").await,
        4
    );
    let SqlTurnChainStore::Sqlite(pool, gate) = &store else {
        unreachable!()
    };
    let _write_gate = gate.lock().await;
    let foreign: i64 = sqlx::query_scalar(
        "INSERT INTO turn_chain_contents (principal, content_key, content) VALUES ('foreign-owner', 'foreign-content', X'00') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let error = sqlx::query("INSERT INTO turn_chain_node_contents (node_id, principal, content_id) VALUES ('agent-item-test', 'foreign-owner', ?)")
        .bind(foreign)
        .execute(pool)
        .await;
    assert!(
        error.is_err(),
        "node composite foreign key must reject foreign principals"
    );
    let error = sqlx::query("INSERT INTO turn_chain_node_contents (node_id, principal, content_id) VALUES ('agent-item-test', ?, ?)")
        .bind(principal.continuation_key())
        .bind(foreign)
        .execute(pool)
        .await;
    assert!(
        error.is_err(),
        "content composite foreign key must reject foreign contents"
    );
}

#[tokio::test]
async fn raw_json_externalization_boundary_preserves_payload() {
    let store = super::test_store().await;
    let principal = Principal::new("boundary-owner");
    // JSON string quotes contribute two bytes to the extraction threshold.
    for (length, expected_contents) in [(253, 0), (254, 1)] {
        let id = TurnNodeId::response();
        let original = json!({"client_delta": {"system": "x".repeat(length)}});
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: principal.clone(),
                payload_version: 6,
                payload: original.clone(),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .materialize(&principal, TurnNodeKind::Response, &id)
                .await
                .unwrap()[0]
                .payload,
            original
        );
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM turn_chain_contents").await,
            expected_contents
        );
    }
}

#[tokio::test]
async fn structural_history_sqlite_contract() {
    contract(super::test_store().await).await;
}

/// Restoring a short chain must decode only the contents its envelopes name,
/// never scan the Principal's content tables or unrelated nodes. The progress
/// budget covers the real store materialization path while the unrelated rows
/// are seeded first.
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

    let SqlTurnChainStore::Sqlite(pool, gate) = &store else {
        unreachable!("SQLite test store")
    };
    let _write_gate = gate.lock().await;
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
         INSERT INTO turn_chain_nodes \
         (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) \
         SELECT 'unrelated-ref-' || i, ?, NULL, ?, 6, ?, ?, ? FROM n",
    )
    .bind(UNRELATED_REFERENCES)
    .bind(TurnNodeKind::Response.as_str())
    .bind(owner.continuation_key())
    .bind(super::content::encode(json!({})).unwrap().payload)
    .bind(now)
    .bind(now + 60_000)
    .execute(pool)
    .await
    .expect("seed unrelated nodes");
    sqlx::query(
        "INSERT INTO turn_chain_node_contents (node_id, principal, content_id) \
         SELECT n.id, n.principal, r.content_id \
         FROM turn_chain_nodes n CROSS JOIN turn_chain_node_contents r \
         WHERE n.id LIKE 'unrelated-ref-%' AND r.node_id = ? LIMIT 10000",
    )
    .bind(root.as_str())
    .execute(pool)
    .await
    .expect("seed unrelated references");
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
         INSERT INTO turn_chain_contents (principal, content_key, content) \
         SELECT ?, 'unrelated-content-' || i, X'00' FROM n",
    )
    .bind(UNRELATED_REFERENCES)
    .bind(owner.continuation_key())
    .execute(pool)
    .await
    .expect("seed unrelated contents");
    assert!(
        scalar(&store, "SELECT COUNT(*) FROM turn_chain_node_contents").await
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
    crate::migrations::migrate_postgres(&pool, None)
        .await
        .unwrap();
    contract(SqlTurnChainStore::postgres(pool.clone())).await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
