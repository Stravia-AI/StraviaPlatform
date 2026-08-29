use super::*;
use std::sync::Arc;

fn principal(id: &str) -> Principal {
    Principal::new(id)
}

#[tokio::test]
async fn committed_root_materializes_through_the_public_store_interface() {
    let store: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let id = TurnNodeId::agent();
    store
        .commit(TurnCommit {
            id: id.clone(),
            kind: TurnNodeKind::Agent,
            parent_id: None,
            principal: principal("owner"),
            payload_version: 1,
            payload: serde_json::json!({"message": "root"}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .expect("commit root");

    let chain = store
        .materialize(&principal("owner"), TurnNodeKind::Agent, &id)
        .await
        .expect("materialize root");

    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].id, id);
    assert_eq!(chain[0].payload, serde_json::json!({"message": "root"}));
}

#[tokio::test]
async fn sqlite_turn_chain_survives_store_reconstruction() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let id = TurnNodeId::response();
    SqlTurnChainStore::sqlite(pool.clone())
        .commit(TurnCommit {
            id: id.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: principal("owner"),
            payload_version: 1,
            payload: serde_json::json!({"response": "persisted"}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .expect("persist response Turn");

    let reconstructed = SqlTurnChainStore::sqlite(pool);
    let chain = reconstructed
        .materialize(&principal("owner"), TurnNodeKind::Response, &id)
        .await
        .expect("materialize persisted Turn");

    assert_eq!(
        chain[0].payload,
        serde_json::json!({"response": "persisted"})
    );
}

#[tokio::test]
async fn sqlite_rejects_and_renews_expired_ancestor_chains() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let store = SqlTurnChainStore::sqlite(pool.clone());
    let owner = principal("owner");
    let root = TurnNodeId::agent();
    let child = TurnNodeId::agent();
    for (id, parent, message) in [(&root, None, "root"), (&child, Some(root.clone()), "child")] {
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Agent,
                parent_id: parent,
                principal: owner.clone(),
                payload_version: 1,
                payload: serde_json::json!({"message": message}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .expect("commit live ancestor chain");
    }
    sqlx::query("UPDATE turn_chain_nodes SET expires_at = ? WHERE id = ?")
        .bind(chrono::Utc::now().timestamp_millis().saturating_sub(1))
        .bind(root.as_str())
        .execute(&pool)
        .await
        .expect("expire root");

    assert!(matches!(
        store.materialize(&owner, TurnNodeKind::Agent, &child).await,
        Err(TurnUnavailable::Unavailable)
    ));

    let grandchild = TurnNodeId::agent();
    store
        .commit(TurnCommit {
            id: grandchild.clone(),
            kind: TurnNodeKind::Agent,
            parent_id: Some(child.clone()),
            principal: owner.clone(),
            payload_version: 1,
            payload: serde_json::json!({"message": "grandchild"}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .expect("renew expired ancestor while extending live child");

    let chain = store
        .materialize(&owner, TurnNodeKind::Agent, &grandchild)
        .await
        .expect("materialize renewed chain");
    assert_eq!(
        chain.iter().map(|node| &node.id).collect::<Vec<_>>(),
        [&root, &child, &grandchild]
    );
}

#[tokio::test]
async fn sibling_branches_are_isolated_and_principal_scoped() {
    let store = crate::turn_chain::test_store().await;
    let root = TurnNodeId::agent();
    store
        .commit(TurnCommit {
            id: root.clone(),
            kind: TurnNodeKind::Agent,
            parent_id: None,
            principal: principal("owner"),
            payload_version: 1,
            payload: serde_json::json!({"message": "root"}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .expect("commit root");
    let branch_a = TurnNodeId::agent();
    let branch_b = TurnNodeId::agent();
    for (id, message) in [(&branch_a, "a"), (&branch_b, "b")] {
        store
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Agent,
                parent_id: Some(root.clone()),
                principal: principal("owner"),
                payload_version: 1,
                payload: serde_json::json!({"message": message}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .expect("commit branch");
    }

    let chain_a = store
        .materialize(&principal("owner"), TurnNodeKind::Agent, &branch_a)
        .await
        .expect("materialize branch a");
    let chain_b = store
        .materialize(&principal("owner"), TurnNodeKind::Agent, &branch_b)
        .await
        .expect("materialize branch b");
    assert_eq!(chain_a.len(), 2);
    assert_eq!(chain_b.len(), 2);
    assert_eq!(chain_a[1].payload["message"], "a");
    assert_eq!(chain_b[1].payload["message"], "b");
    assert_eq!(
        store
            .materialize(&principal("other"), TurnNodeKind::Agent, &branch_a)
            .await,
        Err(TurnUnavailable::Unavailable)
    );
}

#[tokio::test]
async fn expired_sibling_is_collected_without_breaking_live_prefix() {
    let store = crate::turn_chain::test_store().await;
    let root = TurnNodeId::agent();
    store
        .commit(TurnCommit {
            id: root.clone(),
            kind: TurnNodeKind::Agent,
            parent_id: None,
            principal: principal("owner"),
            payload_version: 1,
            payload: serde_json::json!({"message": "root"}),
            idle_ttl: Duration::from_secs(10),
            reusable_prefix: None,
        })
        .await
        .expect("commit root");
    let stale = TurnNodeId::agent();
    store
        .commit(TurnCommit {
            id: stale.clone(),
            kind: TurnNodeKind::Agent,
            parent_id: Some(root.clone()),
            principal: principal("owner"),
            payload_version: 1,
            payload: serde_json::json!({"message": "stale"}),
            idle_ttl: Duration::from_millis(50),
            reusable_prefix: None,
        })
        .await
        .expect("commit stale branch");
    tokio::time::sleep(Duration::from_millis(75)).await;
    let live = TurnNodeId::agent();
    store
        .commit(TurnCommit {
            id: live.clone(),
            kind: TurnNodeKind::Agent,
            parent_id: Some(root),
            principal: principal("owner"),
            payload_version: 1,
            payload: serde_json::json!({"message": "live"}),
            idle_ttl: Duration::from_secs(10),
            reusable_prefix: None,
        })
        .await
        .expect("commit live branch");

    assert_eq!(store.sweep_expired().await.expect("sweep"), 1);
    assert_eq!(
        store
            .materialize(&principal("owner"), TurnNodeKind::Agent, &stale)
            .await,
        Err(TurnUnavailable::Unavailable)
    );
    assert_eq!(
        store
            .materialize(&principal("owner"), TurnNodeKind::Agent, &live)
            .await
            .expect("live branch")
            .len(),
        2
    );
}

async fn assert_reusable_prefix_store_contract(store: Arc<dyn TurnChainStore>) {
    for (id, owner, namespace, fingerprint, item_count, completed_at, reusable) in [
        ("contract-short", "owner", "target-a", "short", 2, 20, true),
        (
            "contract-long-old",
            "owner",
            "target-a",
            "long",
            4,
            10,
            true,
        ),
        (
            "contract-long-new",
            "owner",
            "target-a",
            "long",
            4,
            30,
            true,
        ),
        (
            "contract-other-target",
            "owner",
            "target-b",
            "long",
            4,
            40,
            true,
        ),
        (
            "contract-other-owner",
            "other",
            "target-a",
            "long",
            4,
            50,
            true,
        ),
        (
            "contract-not-indexed",
            "owner",
            "target-a",
            "long",
            4,
            60,
            false,
        ),
    ] {
        store
            .commit(TurnCommit {
                id: TurnNodeId::new(id),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: principal(owner),
                payload_version: 1,
                payload: serde_json::json!({"response": id}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: reusable.then(|| ReusablePrefixMetadata {
                    namespace: namespace.into(),
                    fingerprint: fingerprint.into(),
                    item_count,
                    completed_at,
                }),
            })
            .await
            .expect("commit contract node");
    }
    let query = ReusablePrefixQuery {
        namespace: "target-a".into(),
        fingerprints: vec![("short".into(), 2), ("long".into(), 4)],
    };
    let candidates = store
        .find_reusable_prefixes(&principal("owner"), TurnNodeKind::Response, &query)
        .await
        .expect("query contract prefixes");
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.node_id.as_str())
            .collect::<Vec<_>>(),
        ["contract-long-new", "contract-long-old", "contract-short"]
    );
    assert_eq!(
        store
            .find_reusable_prefixes(&principal("other"), TurnNodeKind::Response, &query)
            .await
            .expect("query isolated principal")
            .iter()
            .map(|candidate| candidate.node_id.as_str())
            .collect::<Vec<_>>(),
        ["contract-other-owner"]
    );
    assert!(
        store
            .find_reusable_prefixes(
                &principal("owner"),
                TurnNodeKind::Response,
                &ReusablePrefixQuery {
                    namespace: "missing-target".into(),
                    fingerprints: query.fingerprints.clone(),
                },
            )
            .await
            .expect("query isolated Target")
            .is_empty()
    );
}

#[tokio::test]
async fn temporary_sqlite_reusable_prefix_store_contract() {
    assert_reusable_prefix_store_contract(Arc::new(crate::turn_chain::test_store().await)).await;
}

#[tokio::test]
async fn sqlite_reusable_prefix_store_contract() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    assert_reusable_prefix_store_contract(Arc::new(SqlTurnChainStore::sqlite(pool))).await;
}

#[tokio::test]
async fn postgres_reusable_prefix_store_contract_when_configured() {
    let Some(url) = std::env::var("DB_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())
    else {
        return;
    };
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("PostgreSQL admin pool");
    let schema = format!("stravia_prefix_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create isolated PostgreSQL schema");
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().expect("PostgreSQL connection options");
    let options = options.options([("search_path", schema.as_str())]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("isolated PostgreSQL pool");
    crate::migrations::migrate_postgres(&pool)
        .await
        .expect("PostgreSQL migrations");
    assert_reusable_prefix_store_contract(Arc::new(SqlTurnChainStore::postgres(pool.clone())))
        .await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop isolated PostgreSQL schema");
}

#[tokio::test]
async fn reusable_prefix_lookup_is_principal_scoped_and_longest_first() {
    let store = crate::turn_chain::test_store().await;
    for (id, fingerprint, item_count, completed_at) in [
        ("resp_short", "hash-short", 2, 20),
        ("resp_long_old", "hash-long", 4, 10),
        ("resp_long_new", "hash-long", 4, 30),
    ] {
        store
            .commit(TurnCommit {
                id: TurnNodeId::new(id),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: principal("owner"),
                payload_version: 1,
                payload: serde_json::json!({"response": id}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: Some(ReusablePrefixMetadata {
                    namespace: "target-a".into(),
                    fingerprint: fingerprint.into(),
                    item_count,
                    completed_at,
                }),
            })
            .await
            .expect("commit reusable prefix");
    }

    let query = ReusablePrefixQuery {
        namespace: "target-a".into(),
        fingerprints: vec![("hash-short".into(), 2), ("hash-long".into(), 4)],
    };
    let candidates = store
        .find_reusable_prefixes(&principal("owner"), TurnNodeKind::Response, &query)
        .await
        .expect("query reusable prefixes");

    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.node_id.as_str())
            .collect::<Vec<_>>(),
        ["resp_long_new", "resp_long_old", "resp_short"]
    );
    assert!(
        store
            .find_reusable_prefixes(&principal("other"), TurnNodeKind::Response, &query)
            .await
            .expect("query other principal")
            .is_empty()
    );
}

#[tokio::test]
async fn sqlite_reusable_prefix_metadata_survives_store_reconstruction() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    SqlTurnChainStore::sqlite(pool.clone())
        .commit(TurnCommit {
            id: TurnNodeId::new("resp_indexed"),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: principal("owner"),
            payload_version: 1,
            payload: serde_json::json!({"response": "persisted"}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: Some(ReusablePrefixMetadata {
                namespace: "target-a".into(),
                fingerprint: "hash-indexed".into(),
                item_count: 6,
                completed_at: 40,
            }),
        })
        .await
        .expect("commit indexed response");

    let candidates = SqlTurnChainStore::sqlite(pool)
        .find_reusable_prefixes(
            &principal("owner"),
            TurnNodeKind::Response,
            &ReusablePrefixQuery {
                namespace: "target-a".into(),
                fingerprints: vec![("hash-indexed".into(), 6)],
            },
        )
        .await
        .expect("query indexed response");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].node_id.as_str(), "resp_indexed");
}
