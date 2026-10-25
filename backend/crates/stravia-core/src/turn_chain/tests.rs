use super::*;
use std::sync::Arc;

fn principal(id: &str) -> Principal {
    Principal::new(id)
}

async fn content_counts(pool: &sqlx::SqlitePool, principal: &str) -> (i64, i64) {
    let contents: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM turn_chain_contents WHERE principal = ?")
            .bind(principal)
            .fetch_one(pool)
            .await
            .expect("content count");
    let references: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM turn_chain_node_contents WHERE principal = ?")
            .bind(principal)
            .fetch_one(pool)
            .await
            .expect("reference count");
    (contents, references)
}

#[tokio::test]
async fn large_retention_sweep_preserves_live_history_and_native_login() {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite database");
    let store = SqlTurnChainStore::sqlite(pool.clone());
    let owner = principal("retention-owner");
    let live = TurnNodeId::response();
    let payload = serde_json::json!({"effective_system": "shared instructions ".repeat(100)});
    store
        .commit(TurnCommit {
            id: live.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: payload.clone(),
            idle_ttl: Duration::from_secs(3600),
            reusable_prefix: None,
        })
        .await
        .expect("live history");
    // 复制真实 envelope 和引用，构造同一 Principal 下大量独立的到期节点。
    sqlx::query(
        "WITH RECURSIVE expired(n) AS (
             SELECT 1 UNION ALL SELECT n+1 FROM expired WHERE n<8000
         )
         INSERT INTO turn_chain_nodes
             (id, kind, parent_id, principal, payload_version, payload,
              created_at, expires_at, storage_format)
         SELECT 'expired-' || n, kind, NULL, principal, payload_version, payload,
                created_at, 0, storage_format
         FROM expired CROSS JOIN turn_chain_nodes WHERE id=?",
    )
    .bind(live.as_str())
    .execute(&pool)
    .await
    .expect("expired history");
    sqlx::query(
        "INSERT INTO turn_chain_node_contents (node_id, principal, content_id)
         SELECT n.id, r.principal, r.content_id
         FROM turn_chain_nodes n CROSS JOIN turn_chain_node_contents r
         WHERE n.expires_at=0 AND r.node_id=?",
    )
    .bind(live.as_str())
    .execute(&pool)
    .await
    .expect("shared history references");

    {
        let mut connection = pool.acquire().await.expect("maintenance connection");
        // 用 SQLite 指令预算捕获二次方级外键扫描，不依赖机器速度或墙钟超时。
        let mut remaining = 3000_u32;
        connection
            .lock_handle()
            .await
            .expect("SQLite handle")
            .set_progress_handler(1000, move || {
                remaining = remaining.saturating_sub(1);
                remaining > 0
            });
    }
    let swept = store.sweep_expired().await;
    {
        let mut connection = pool.acquire().await.expect("maintenance connection");
        connection
            .lock_handle()
            .await
            .expect("SQLite handle")
            .remove_progress_handler();
    }
    assert_eq!(
        swept.expect("retention must not monopolize the writer"),
        8000
    );
    let history = store
        .materialize(&owner, TurnNodeKind::Response, &live)
        .await
        .expect("live history remains available");
    assert_eq!(history[0].payload, payload);

    let auth = crate::admin::identity::AdminAuth::new(Arc::new(
        crate::storage::SqliteStorage::from_pool(pool),
    ));
    auth.ensure_native_admin().await.expect("native identity");
    let session = auth.login_native().await.expect("native login after sweep");
    assert_eq!(
        auth.authenticate(&session.access_token)
            .await
            .expect("native session remains authenticated")
            .role,
        "admin"
    );
}

#[tokio::test]
async fn sqlite_content_gc_batches_live_contents_and_collects_tail_orphans() {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite database");
    let store = SqlTurnChainStore::sqlite(pool.clone());
    let owner = principal("gc-batch-owner");
    let principal_key = owner.continuation_key();
    let live = TurnNodeId::response();
    let payload = serde_json::json!({
        "items": (0..900)
            .map(|index| format!("live item {index} {}", "x".repeat(300)))
            .collect::<Vec<_>>(),
    });
    store
        .commit(TurnCommit {
            id: live.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: payload.clone(),
            idle_ttl: Duration::from_secs(600),
            reusable_prefix: None,
        })
        .await
        .expect("live history");
    assert_eq!(
        content_counts(&pool, &principal_key).await,
        (900, 900),
        "every externalized item has its own content row"
    );

    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<450) \
         INSERT INTO turn_chain_contents(principal, content_key, content) \
         SELECT ?, 'gc-tail-orphan-' || i, zeroblob(64) FROM n",
    )
    .bind(&principal_key)
    .execute(&pool)
    .await
    .expect("tail orphans");

    // 前两批 400 个候选全是活内容；零删除不能让扫描提前结束。
    assert_eq!(store.sweep_expired().await.expect("sweep"), 0);
    assert_eq!(content_counts(&pool, &principal_key).await, (900, 900));
    let restored = store
        .materialize(&owner, TurnNodeKind::Response, &live)
        .await
        .expect("live history survives");
    assert_eq!(restored[0].payload, payload);
}

#[tokio::test]
async fn sqlite_content_gc_restarts_from_a_reconstructed_store() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .expect("SQLite migrations");
    let owner = principal("gc-restart-owner");
    let principal_key = owner.continuation_key();
    let live = TurnNodeId::response();
    let payload = serde_json::json!({
        "items": (0..450)
            .map(|index| format!("restart item {index} {}", "y".repeat(300)))
            .collect::<Vec<_>>(),
    });
    SqlTurnChainStore::sqlite(pool.clone())
        .commit(TurnCommit {
            id: live.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: payload.clone(),
            idle_ttl: Duration::from_secs(600),
            reusable_prefix: None,
        })
        .await
        .expect("live history");
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<200) \
         INSERT INTO turn_chain_contents(principal, content_key, content) \
         SELECT ?, 'gc-first-orphan-' || i, zeroblob(64) FROM n",
    )
    .bind(&principal_key)
    .execute(&pool)
    .await
    .expect("first orphans");
    let first = SqlTurnChainStore::sqlite(pool.clone());
    assert_eq!(first.sweep_expired().await.expect("first sweep"), 0);
    assert_eq!(content_counts(&pool, &principal_key).await, (450, 450));

    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<450) \
         INSERT INTO turn_chain_contents(principal, content_key, content) \
         SELECT ?, 'gc-second-orphan-' || i, zeroblob(64) FROM n",
    )
    .bind(&principal_key)
    .execute(&pool)
    .await
    .expect("second orphans");

    // 重建后的 store 没有内存游标，必须从表头重新有界扫描并清完剩余孤儿。
    let rebuilt = SqlTurnChainStore::sqlite(pool.clone());
    assert_eq!(rebuilt.sweep_expired().await.expect("rebuilt sweep"), 0);
    assert_eq!(content_counts(&pool, &principal_key).await, (450, 450));
    let restored = rebuilt
        .materialize(&owner, TurnNodeKind::Response, &live)
        .await
        .expect("live history survives reconstruction");
    assert_eq!(restored[0].payload, payload);
}

#[tokio::test]
async fn sqlite_content_gc_rechecks_references_added_after_candidate_read() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(data_dir.path().join("turn-chain.sqlite"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .expect("SQLite migrations");
    let store = SqlTurnChainStore::sqlite(pool.clone());
    let owner = principal("gc-recheck-owner");
    let principal_key = owner.continuation_key();
    let node = TurnNodeId::response();
    store
        .commit(TurnCommit {
            id: node.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 6,
            payload: serde_json::json!({"message": "reference owner"}),
            idle_ttl: Duration::from_secs(600),
            reusable_prefix: None,
        })
        .await
        .expect("reference owner");
    // 目标内容排在最前，候选扫描会先把它判定为孤儿；随后插入的填充内容
    // 保证同一条 SELECT 仍有多行可读，进度回调在判定之后暂停。
    let content_id: i64 = sqlx::query_scalar(
        "INSERT INTO turn_chain_contents(principal, content_key, content) \
         VALUES (?, 'rechecked-content', X'00') RETURNING id",
    )
    .bind(&principal_key)
    .fetch_one(&pool)
    .await
    .expect("unreferenced content");
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<400) \
         INSERT INTO turn_chain_contents(principal, content_key, content) \
         SELECT ?, 'gc-recheck-padding-' || i, zeroblob(64) FROM n",
    )
    .bind(&principal_key)
    .execute(&pool)
    .await
    .expect("candidate padding");

    // 占住一条连接作为并发引用写入者；另一条是 GC 唯一可用的空闲连接。
    let mut reference_writer = pool.acquire().await.expect("reference connection");
    let mut gc_connection = pool.acquire().await.expect("GC connection");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
    {
        let mut handle = gc_connection.lock_handle().await.expect("SQLite handle");
        let started_tx = std::sync::Mutex::new(Some(started_tx));
        let resume_rx = std::sync::Mutex::new(resume_rx);
        // 候选 SELECT 判定若干行后暂停，让测试在 DELETE 前提交新引用。
        handle.set_progress_handler(1000, move || {
            if let Some(started) = started_tx.lock().expect("progress sender").take() {
                if started.send(()).is_ok() {
                    let _ = resume_rx.lock().expect("progress resume").recv();
                }
            }
            true
        });
    }
    drop(gc_connection);
    let sweep = tokio::spawn({
        let store = store.clone();
        async move { store.remove_unreferenced_contents().await }
    });
    started_rx.await.expect("candidate scan starts");
    let reference = sqlx::query(
        "INSERT INTO turn_chain_node_contents(node_id, principal, content_id) VALUES (?, ?, ?)",
    )
    .bind(node.as_str())
    .bind(&principal_key)
    .bind(content_id)
    .execute(&mut *reference_writer)
    .await;
    resume_tx.send(()).expect("resume candidate scan");
    reference.expect("late reference");
    sweep
        .await
        .expect("GC task")
        .expect("GC must not trust the stale candidate");
    drop(reference_writer);
    let mut cleanup = Vec::new();
    for _ in 0..2 {
        cleanup.push(pool.acquire().await.expect("cleanup connection"));
    }
    for connection in &mut cleanup {
        connection
            .lock_handle()
            .await
            .expect("SQLite handle")
            .remove_progress_handler();
    }
    drop(cleanup);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM turn_chain_contents \
             WHERE principal = ? AND content_key LIKE 'gc-recheck-padding-%'",
        )
        .bind(&principal_key)
        .fetch_one(&pool)
        .await
        .expect("padding count"),
        0,
        "all padding candidates are collected"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turn_chain_contents WHERE id = ?")
            .bind(content_id)
            .fetch_one(&pool)
            .await
            .expect("content count"),
        1,
        "the late reference must protect its content"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM turn_chain_node_contents WHERE node_id = ? AND content_id = ?",
        )
        .bind(node.as_str())
        .bind(content_id)
        .fetch_one(&pool)
        .await
        .expect("reference count"),
        1
    );
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
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .expect("SQLite migrations");
    let id = TurnNodeId::response();
    // 同一池上的多个 store 自动共享写协调。
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
    crate::migrations::migrate_sqlite(&pool, None)
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
            "contract-long-new-z",
            "owner",
            "target-a",
            "long",
            4,
            30,
            true,
        ),
        (
            "contract-cross-short",
            "owner",
            "target-a",
            "short",
            4,
            90,
            true,
        ),
        (
            "contract-cross-long",
            "owner",
            "target-a",
            "long",
            2,
            90,
            true,
        ),
        ("contract-expired", "owner", "target-a", "long", 4, 90, true),
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
                idle_ttl: if id == "contract-expired" {
                    Duration::ZERO
                } else {
                    Duration::from_secs(60)
                },
                reusable_prefix: reusable.then(|| ReusablePrefixMetadata {
                    projection: None,
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
        projection: None,
        namespace: "target-a".into(),
        fingerprints: vec![("short".into(), 2), ("long".into(), 4), ("long".into(), 4)],
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
        [
            "contract-long-new-z",
            "contract-long-new",
            "contract-long-old",
            "contract-short",
        ]
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
                    projection: None,
                    namespace: "target-a".into(),
                    fingerprints: Vec::new(),
                },
            )
            .await
            .expect("query empty prefix set")
            .is_empty()
    );
    assert!(
        store
            .find_reusable_prefixes(&principal("owner"), TurnNodeKind::Agent, &query)
            .await
            .expect("query isolated node kind")
            .is_empty()
    );
    assert!(
        store
            .find_reusable_prefixes(
                &principal("owner"),
                TurnNodeKind::Response,
                &ReusablePrefixQuery {
                    projection: None,
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
    crate::migrations::migrate_sqlite(&pool, None)
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
    crate::migrations::migrate_postgres(&pool, None)
        .await
        .expect("PostgreSQL migrations");
    let store = SqlTurnChainStore::postgres(pool.clone());
    assert_prefix_rebuild_decodes_shared_ancestors_once(&store).await;
    assert_reusable_prefix_store_contract(Arc::new(store.clone())).await;
    assert_request_context_contract(&store).await;
    assert_expired_query_cutoff_proof_falls_back_to_current_facts(&store).await;
    assert_projection_proofs_exclude_only_known_mismatches_and_rebuild_old_indexes(&store).await;
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
                    projection: None,
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
        projection: None,
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
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .expect("SQLite migrations");
    // 同一池上的多个 store 自动共享写协调。
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
                projection: None,
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
                projection: None,
                namespace: "target-a".into(),
                fingerprints: vec![("hash-indexed".into(), 6)],
            },
        )
        .await
        .expect("query indexed response");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].node_id.as_str(), "resp_indexed");
}

/// Ancestor walks run inside the commit's write lock, so their cost must follow
/// chain depth rather than how many nodes the principal owns. The VM-step budget
/// makes that deterministic: a walk that rescans every node per ancestor exceeds
/// it by an order of magnitude, while an indexed parent lookup stays far below.
#[tokio::test]
async fn sqlite_ancestor_walks_do_not_scan_unrelated_principal_nodes() {
    const DEPTH: usize = 64;
    const UNRELATED_NODES: i64 = 4_000;
    const OPS_PER_TICK: i32 = 1_000;
    const TICK_BUDGET: u32 = 200;

    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .expect("SQLite migrations");
    let owner = principal("owner");
    let setup = SqlTurnChainStore::sqlite(pool.clone());
    let mut head: Option<TurnNodeId> = None;
    for depth in 0..DEPTH {
        let id = TurnNodeId::agent();
        setup
            .commit(TurnCommit {
                id: id.clone(),
                kind: TurnNodeKind::Agent,
                parent_id: head.take(),
                principal: owner.clone(),
                payload_version: 1,
                payload: serde_json::json!({"depth": depth}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .expect("commit chain node");
        head = Some(id);
    }
    let head = head.expect("chain head");
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
         INSERT INTO turn_chain_nodes \
         (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at) \
         SELECT 'unrelated-' || i, ?, NULL, ?, 1, '{}', ?, ? FROM n",
    )
    .bind(UNRELATED_NODES)
    .bind(TurnNodeKind::Agent.as_str())
    .bind(owner.continuation_key())
    .bind(now)
    .bind(now + 60_000)
    .execute(&pool)
    .await
    .expect("insert unrelated nodes");

    let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let handler_ticks = ticks.clone();
    let budgeted = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _| {
            let ticks = handler_ticks.clone();
            Box::pin(async move {
                connection
                    .lock_handle()
                    .await?
                    .set_progress_handler(OPS_PER_TICK, move || {
                        ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < TICK_BUDGET
                    });
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("budgeted SQLite pool");
    let store = SqlTurnChainStore::sqlite(budgeted);

    let chain = store
        .materialize(&owner, TurnNodeKind::Agent, &head)
        .await
        .expect("materialize within VM budget");
    assert_eq!(chain.len(), DEPTH);
    eprintln!(
        "TICKS materialize={}",
        ticks.load(std::sync::atomic::Ordering::Relaxed)
    );
    ticks.store(0, std::sync::atomic::Ordering::Relaxed);
    store
        .commit(TurnCommit {
            id: TurnNodeId::agent(),
            kind: TurnNodeKind::Agent,
            parent_id: Some(head),
            principal: owner,
            payload_version: 1,
            payload: serde_json::json!({"depth": DEPTH}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .expect("renew ancestors within VM budget");
    eprintln!(
        "TICKS commit={}",
        ticks.load(std::sync::atomic::Ordering::Relaxed)
    );
}

/// Long pair lists must probe the prefix index rather than evaluate every pair
/// against every unrelated node in the same principal and namespace.
#[tokio::test]
async fn sqlite_prefix_lookup_does_not_scan_unrelated_namespace_nodes() {
    const PAIRS: u32 = 104;
    const OPS_PER_TICK: i32 = 1_000;
    const TICK_BUDGET: u32 = 100;

    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .expect("SQLite migrations");
    let owner = principal("owner");
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 4000) \
         INSERT INTO turn_chain_nodes \
         (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at, \
          prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at) \
         SELECT 'prefix-budget-' || i, ?, NULL, ?, 1, '{}', ?, ?, \
          'budget', 'fingerprint-' || i, i, i FROM n",
    )
    .bind(TurnNodeKind::Response.as_str())
    .bind(owner.continuation_key())
    .bind(now)
    .bind(now + 60_000)
    .execute(&pool)
    .await
    .expect("insert matching and unrelated prefix nodes");

    let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let handler_ticks = ticks.clone();
    let budgeted = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _| {
            let ticks = handler_ticks.clone();
            Box::pin(async move {
                connection
                    .lock_handle()
                    .await?
                    .set_progress_handler(OPS_PER_TICK, move || {
                        ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < TICK_BUDGET
                    });
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("budgeted SQLite pool");
    let candidates = SqlTurnChainStore::sqlite(budgeted)
        .find_reusable_prefixes(
            &owner,
            TurnNodeKind::Response,
            &ReusablePrefixQuery {
                projection: None,
                namespace: "budget".into(),
                fingerprints: (1..=PAIRS)
                    .map(|i| (format!("fingerprint-{i}"), i))
                    .collect(),
            },
        )
        .await
        .expect("prefix lookup within VM-step budget");
    assert_eq!(
        candidates
            .into_iter()
            .map(|candidate| {
                (
                    candidate.node_id.as_str().to_owned(),
                    candidate.item_count,
                    candidate.completed_at,
                )
            })
            .collect::<Vec<_>>(),
        (1..=PAIRS)
            .rev()
            .map(|i| { (format!("prefix-budget-{i}"), i, i64::from(i)) })
            .collect::<Vec<_>>()
    );
    assert!(ticks.load(std::sync::atomic::Ordering::Relaxed) <= TICK_BUDGET);
}

#[tokio::test]
async fn request_context_reuses_validated_ancestors_before_payload_fetch() {
    let store = test_store().await;
    let owner = principal("request-context");
    let root = TurnNodeId::response();
    let child = TurnNodeId::response();
    for (id, parent) in [(root.clone(), None), (child.clone(), Some(root.clone()))] {
        store
            .commit(TurnCommit {
                id,
                parent_id: parent,
                principal: owner.clone(),
                kind: TurnNodeKind::Response,
                payload_version: 1,
                payload: serde_json::json!({"items": ["large history ".repeat(100)]}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .unwrap();
    }
    let mut context = TurnReadContext::new(&owner, TurnNodeKind::Response);
    let chain = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &root, &mut context)
        .await
        .unwrap();
    assert!(
        context.is_empty(),
        "materialization alone must not admit data"
    );
    let decoded: Arc<dyn std::any::Any + Send + Sync> = Arc::new("decoded root".to_owned());
    context.admit(&chain, vec![(decoded.clone(), 256)]);
    // A validated ancestor must be reused without fetching or decoding its now
    // damaged persisted payload. Newly read descendants still restore normally.
    let SqlTurnChainStore::Sqlite(pool) = &store else {
        unreachable!()
    };
    sqlx::query("UPDATE turn_chain_nodes SET payload = X'00' WHERE id = ?")
        .bind(root.as_str())
        .execute(pool)
        .await
        .unwrap();
    let chain = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &child, &mut context)
        .await
        .unwrap();
    assert_eq!(chain.nodes.len(), 2);
    assert!(chain.nodes[0].raw.is_none());
    assert!(Arc::ptr_eq(
        chain.nodes[0].decoded.as_ref().unwrap(),
        &decoded
    ));
    assert!(
        chain.nodes[1].raw.as_ref().unwrap().payload["items"][0]
            .as_str()
            .unwrap()
            .len()
            > 256
    );
    assert!(
        store
            .materialize_in_context(
                &principal("other"),
                TurnNodeKind::Response,
                &root,
                &mut context
            )
            .await
            .is_err()
    );
}

#[test]
fn request_context_budget_and_independent_node_ttl_are_conservative() {
    use std::time::Instant;
    let owner = principal("ttl-context");
    let root = TurnNodeId::response();
    let child = TurnNodeId::response();
    let long = Instant::now() + Duration::from_secs(60);
    let expired = Instant::now();
    let node = |id, parent_id, expires_at| SharedTurnNode {
        id,
        parent_id,
        expires_at,
        kind: TurnNodeKind::Response,
        payload_version: 1,
        raw: None,
        decoded: None,
    };
    let chain = SharedMaterializedTurnChain {
        expires_at: long,
        nodes: vec![
            node(root.clone(), None, long),
            node(child.clone(), Some(root.clone()), long),
        ],
    };
    let value: Arc<dyn std::any::Any + Send + Sync> = Arc::new(1usize);
    let mut small = TurnReadContext::with_budget(&owner, TurnNodeKind::Response, 1);
    small.admit(&chain, vec![(value.clone(), 1), (value.clone(), 1)]);
    assert!(
        small.is_empty(),
        "oversized chains must not be partially admitted"
    );
    let mut context = TurnReadContext::new(&owner, TurnNodeKind::Response);
    let short = Instant::now() + Duration::from_secs(10);
    let shorter_child = SharedMaterializedTurnChain {
        expires_at: short,
        nodes: vec![
            node(root.clone(), None, long),
            node(child.clone(), Some(root.clone()), short),
        ],
    };
    context.admit(&shorter_child, vec![(value.clone(), 1), (value.clone(), 1)]);
    assert_eq!(context.get(&root).unwrap().expires_at, long);
    assert_eq!(context.get(&child).unwrap().expires_at, short);
    let mut fallback = TurnReadContext::new(&owner, TurnNodeKind::Response);
    let fallback_chain = SharedMaterializedTurnChain {
        expires_at: short,
        nodes: vec![
            node(root.clone(), None, short),
            node(child.clone(), Some(root.clone()), short),
        ],
    };
    fallback.admit(
        &fallback_chain,
        vec![(value.clone(), 1), (value.clone(), 1)],
    );
    let independently_read_root = SharedMaterializedTurnChain {
        expires_at: long,
        nodes: vec![node(root.clone(), None, long)],
    };
    fallback.admit(&independently_read_root, vec![(value.clone(), 1)]);
    assert_eq!(fallback.get(&root).unwrap().expires_at, long);
    assert_eq!(fallback.get(&child).unwrap().expires_at, short);
    let mut context = TurnReadContext::new(&owner, TurnNodeKind::Response);
    let root_chain = SharedMaterializedTurnChain {
        expires_at: long,
        nodes: vec![node(root.clone(), None, long)],
    };
    context.admit(&root_chain, vec![(value.clone(), 1)]);
    let expired_chain = SharedMaterializedTurnChain {
        expires_at: expired,
        nodes: vec![node(child.clone(), Some(root.clone()), expired)],
    };
    context.admit(&expired_chain, vec![(value, 1)]);
    assert!(context.get(&root).is_some());
    assert!(context.get(&child).is_none());
    let expired_head = SharedMaterializedTurnChain {
        expires_at: long,
        nodes: vec![
            node(root.clone(), None, long),
            node(child.clone(), Some(root.clone()), expired),
        ],
    };
    let value: Arc<dyn std::any::Any + Send + Sync> = Arc::new(1usize);
    context.admit(&expired_head, vec![(value.clone(), 0), (value, 1)]);
    assert!(context.get(&root).is_some());
    assert!(
        context.get(&child).is_none(),
        "each head checks its own node deadline"
    );
    assert!(context.validated_ids().contains(&root.as_str().to_owned()));
    assert!(!context.validated_ids().contains(&child.as_str().to_owned()));
}

async fn assert_expired_query_cutoff_proof_falls_back_to_current_facts(store: &SqlTurnChainStore) {
    let owner = principal("query-cutoff-expiry");
    let root = TurnNodeId::response();
    let child = TurnNodeId::response();
    store
        .commit(TurnCommit {
            id: root.clone(),
            parent_id: None,
            principal: owner.clone(),
            kind: TurnNodeKind::Response,
            payload_version: 1,
            payload: serde_json::json!({"root": true}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .unwrap();
    let mut context = TurnReadContext::new(&owner, TurnNodeKind::Response);
    let mut root_chain = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &root, &mut context)
        .await
        .unwrap();
    let cutoff_deadline = std::time::Instant::now() + Duration::from_secs(10);
    root_chain.expires_at = cutoff_deadline;
    root_chain.nodes[0].expires_at = cutoff_deadline;
    let decoded: Arc<dyn std::any::Any + Send + Sync> = Arc::new("validated root");
    context.admit(&root_chain, vec![(decoded, 128)]);
    assert!(context.validated_ids().contains(&root.as_str().to_owned()));
    // A normal append renews the durable ancestor, not the old request proof.
    store
        .commit(TurnCommit {
            id: child.clone(),
            parent_id: Some(root.clone()),
            principal: owner.clone(),
            kind: TurnNodeKind::Response,
            payload_version: 1,
            payload: serde_json::json!({"child": true}),
            idle_ttl: Duration::from_secs(3600),
            reusable_prefix: None,
        })
        .await
        .unwrap();
    let suffix_query = "SELECT id, parent_id, payload_version, payload, expires_at, CAST(0 AS BIGINT) FROM turn_chain_nodes WHERE id = $1";
    let rows: Vec<StoredAncestorRow> = match store {
        SqlTurnChainStore::Sqlite(pool) => sqlx::query_as(suffix_query)
            .bind(child.as_str())
            .fetch_all(pool)
            .await
            .unwrap(),
        SqlTurnChainStore::Postgres(pool) => sqlx::query_as(suffix_query)
            .bind(child.as_str())
            .fetch_all(pool)
            .await
            .unwrap(),
    };
    assert_eq!(
        rows.len(),
        1,
        "the query stopped at the previously valid root proof"
    );
    let completed_at = cutoff_deadline + Duration::from_secs(1);
    assert!(context.get_at(&root, completed_at).is_none());
    let chain = store
        .finish_context_read(
            &owner,
            TurnNodeKind::Response,
            &mut context,
            rows.clone(),
            completed_at,
        )
        .await
        .unwrap();
    assert_eq!(
        chain.nodes.iter().map(|node| &node.id).collect::<Vec<_>>(),
        vec![&root, &child]
    );
    assert!(
        chain.nodes.iter().all(|node| node.raw.is_some()),
        "lost proof requires complete fresh facts"
    );
    assert_eq!(
        chain.nodes[0].raw.as_ref().unwrap().payload,
        serde_json::json!({"root": true})
    );
    assert!(chain.expires_at > cutoff_deadline);
    let expire_root = "UPDATE turn_chain_nodes SET expires_at = 0 WHERE id = $1";
    match store {
        SqlTurnChainStore::Sqlite(pool) => {
            sqlx::query(expire_root)
                .bind(root.as_str())
                .execute(pool)
                .await
                .unwrap();
        }
        SqlTurnChainStore::Postgres(pool) => {
            sqlx::query(expire_root)
                .bind(root.as_str())
                .execute(pool)
                .await
                .unwrap();
        }
    }
    assert!(
        matches!(
            store
                .finish_context_read(
                    &owner,
                    TurnNodeKind::Response,
                    &mut context,
                    rows,
                    completed_at
                )
                .await,
            Err(TurnUnavailable::Unavailable)
        ),
        "a truly expired durable ancestor cannot be revived by the old proof"
    );
}

#[tokio::test]
async fn expired_query_cutoff_proof_falls_back_to_current_facts() {
    assert_expired_query_cutoff_proof_falls_back_to_current_facts(&test_store().await).await;
}

#[test]
fn pending_external_content_obeys_the_shared_budget_before_admission() {
    let owner = principal("pending-budget");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let id = TurnNodeId::response();
    let chain = SharedMaterializedTurnChain {
        expires_at: deadline,
        nodes: vec![SharedTurnNode {
            id: id.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            payload_version: 1,
            expires_at: deadline,
            raw: None,
            decoded: None,
        }],
    };
    let decoded: Arc<dyn std::any::Any + Send + Sync> = Arc::new("decoded node");
    let mut context = TurnReadContext::with_budget(&owner, TurnNodeKind::Response, 64);
    let admitted = Arc::new(serde_json::json!("admitted"));
    context.stage_content("existing".into(), admitted.clone(), 32);
    context.admit(&chain, vec![(decoded, 1)]);
    assert!(context.cached_content("existing").is_some());
    let boundary = Arc::new(serde_json::json!("pending at boundary"));
    let boundary_lifetime = Arc::downgrade(&boundary);
    context.stage_content("boundary".into(), boundary, 31);
    assert!(
        boundary_lifetime.upgrade().is_some(),
        "exact remaining budget may be staged"
    );
    let excess = Arc::new(serde_json::json!("one byte beyond boundary"));
    let excess_lifetime = Arc::downgrade(&excess);
    context.stage_content("excess".into(), excess, 1);
    assert!(
        boundary_lifetime.upgrade().is_none(),
        "overflow releases optional pending values immediately"
    );
    assert!(excess_lifetime.upgrade().is_none());
    assert!(!context.admission_open());
    assert!(
        context.get(&id).is_some(),
        "already admitted nodes remain reusable"
    );
    assert!(
        context.cached_content("existing").is_some(),
        "already admitted external content remains reusable"
    );
    let mut zero = TurnReadContext::with_budget(&owner, TurnNodeKind::Response, 0);
    let value = Arc::new(serde_json::json!("zero budget"));
    let lifetime = Arc::downgrade(&value);
    zero.stage_content("zero".into(), value, 1);
    assert!(
        lifetime.upgrade().is_none(),
        "zero budget retains no optional content before fold"
    );
    let mut large = TurnReadContext::new(&owner, TurnNodeKind::Response);
    let value = Arc::new(serde_json::Value::String(
        "x".repeat(TurnReadContext::BYTE_LIMIT + 1),
    ));
    let lifetime = Arc::downgrade(&value);
    large.stage_content("large".into(), value, TurnReadContext::BYTE_LIMIT + 1);
    assert!(
        lifetime.upgrade().is_none(),
        "over-limit restored values are not staged until fold"
    );
    assert!(!large.admission_open());
}

async fn assert_projection_proofs_exclude_only_known_mismatches_and_rebuild_old_indexes(
    store: &SqlTurnChainStore,
) {
    let owner = principal("projection-context");
    let proof = ReusablePrefixProjection {
        version: REUSABLE_PREFIX_PROJECTION_VERSION,
        controls_fingerprint: "controls".into(),
        context_fingerprint: "context".into(),
    };
    for (id, projection) in [
        ("legacy", None),
        ("known", Some(proof.clone())),
        (
            "unknown",
            Some(ReusablePrefixProjection {
                version: 999,
                ..proof.clone()
            }),
        ),
    ] {
        store
            .commit(TurnCommit {
                id: TurnNodeId::new(id),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: owner.clone(),
                payload_version: 1,
                payload: serde_json::json!({"items": ["root"]}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: Some(ReusablePrefixMetadata {
                    namespace: "old".into(),
                    fingerprint: "hash".into(),
                    item_count: 1,
                    completed_at: 1,
                    projection,
                }),
            })
            .await
            .unwrap();
    }
    let query = ReusablePrefixQuery {
        namespace: "old".into(),
        fingerprints: vec![("hash".into(), 1)],
        projection: Some(ReusablePrefixProjectionQuery {
            version: REUSABLE_PREFIX_PROJECTION_VERSION,
            controls_fingerprint: "different".into(),
            context_fingerprints: vec![("context".into(), 1)],
        }),
    };
    let candidates = store
        .find_reusable_prefixes(&owner, TurnNodeKind::Response, &query)
        .await
        .unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["unknown", "legacy"]
    );
    let unknown_query = ReusablePrefixQuery {
        projection: Some(ReusablePrefixProjectionQuery {
            version: 999,
            ..query.projection.clone().unwrap()
        }),
        ..query.clone()
    };
    assert_eq!(
        store
            .find_reusable_prefixes(&owner, TurnNodeKind::Response, &unknown_query)
            .await
            .unwrap()
            .len(),
        3
    );
    let matching_query = ReusablePrefixQuery {
        projection: Some(ReusablePrefixProjectionQuery {
            controls_fingerprint: "controls".into(),
            ..query.projection.clone().unwrap()
        }),
        ..query.clone()
    };
    assert_eq!(
        store
            .find_reusable_prefixes(&owner, TurnNodeKind::Response, &matching_query)
            .await
            .unwrap()
            .len(),
        3
    );
    for contexts in [
        vec![("different-context".into(), 1)],
        vec![("context".into(), 2)],
        Vec::new(),
    ] {
        let context_mismatch = ReusablePrefixQuery {
            projection: Some(ReusablePrefixProjectionQuery {
                context_fingerprints: contexts,
                ..matching_query.projection.clone().unwrap()
            }),
            ..matching_query.clone()
        };
        assert_eq!(
            store
                .find_reusable_prefixes(&owner, TurnNodeKind::Response, &context_mismatch)
                .await
                .unwrap()
                .iter()
                .map(|candidate| candidate.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["unknown", "legacy"]
        );
    }
    let partial = "UPDATE turn_chain_nodes SET prefix_context_fingerprint = NULL WHERE id = 'known' AND principal = $1";
    match store {
        SqlTurnChainStore::Sqlite(pool) => {
            sqlx::query(partial)
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
        }
        SqlTurnChainStore::Postgres(pool) => {
            sqlx::query(partial)
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
        }
    }
    assert_eq!(
        store
            .find_reusable_prefixes(&owner, TurnNodeKind::Response, &query)
            .await
            .unwrap()
            .len(),
        3,
        "incomplete known-version projections cannot exclude rows, even when controls differ"
    );
    store
        .rebuild_prefixes("current", &mut |_, completed_at| {
            Ok(Some(ReusablePrefixMetadata {
                namespace: "current".into(),
                fingerprint: "hash".into(),
                item_count: 1,
                completed_at: completed_at.unwrap(),
                projection: Some(proof.clone()),
            }))
        })
        .await
        .unwrap();
    let query = ReusablePrefixQuery {
        namespace: "current".into(),
        ..query
    };
    assert!(
        store
            .find_reusable_prefixes(&owner, TurnNodeKind::Response, &query)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .rebuild_prefixes("next", &mut |_, _| Ok(None))
        .await
        .unwrap();
    let query = ReusablePrefixQuery {
        projection: None,
        ..query
    };
    assert!(
        store
            .find_reusable_prefixes(&owner, TurnNodeKind::Response, &query)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn projection_proofs_exclude_only_known_mismatches_and_rebuild_old_indexes() {
    assert_projection_proofs_exclude_only_known_mismatches_and_rebuild_old_indexes(
        &test_store().await,
    )
    .await;
}

#[tokio::test]
async fn prefix_rebuild_decodes_shared_ancestors_once() {
    assert_prefix_rebuild_decodes_shared_ancestors_once(&test_store().await).await;
}

async fn assert_prefix_rebuild_decodes_shared_ancestors_once(store: &SqlTurnChainStore) {
    let owner = principal("rebuild-owner");
    for (id, parent) in [
        ("root", None),
        ("middle", Some("root")),
        ("left", Some("middle")),
        ("right", Some("middle")),
    ] {
        store
            .commit(TurnCommit {
                id: TurnNodeId::new(id),
                kind: TurnNodeKind::Response,
                parent_id: parent.map(TurnNodeId::new),
                principal: owner.clone(),
                payload_version: 1,
                payload: serde_json::json!({"items": ["retained content ".repeat(100)]}),
                idle_ttl: Duration::from_secs(3600),
                reusable_prefix: Some(ReusablePrefixMetadata {
                    namespace: "previous".into(),
                    fingerprint: id.into(),
                    item_count: 1,
                    completed_at: 123,
                    projection: None,
                }),
            })
            .await
            .unwrap();
    }
    let mut decoded = std::collections::HashMap::<String, usize>::new();
    let mut visited = std::collections::HashSet::new();
    store
        .rebuild_prefixes("current", &mut |node, completed_at| {
            if let Some(parent) = node.parent_id.as_ref() {
                assert!(
                    visited.contains(parent.as_str()),
                    "parents must decode before children",
                );
            }
            visited.insert(node.id.to_string());
            *decoded.entry(node.id.to_string()).or_default() += 1;
            assert_eq!(
                node.payload,
                serde_json::json!({"items": ["retained content ".repeat(100)]}),
            );
            Ok(Some(ReusablePrefixMetadata {
                namespace: "current".into(),
                fingerprint: node.id.to_string(),
                item_count: 1,
                completed_at: completed_at.unwrap(),
                projection: None,
            }))
        })
        .await
        .unwrap();
    assert_eq!(
        decoded,
        std::collections::HashMap::from([
            ("root".into(), 1),
            ("middle".into(), 1),
            ("left".into(), 1),
            ("right".into(), 1),
        ]),
        "startup must not decode shared history again for each stale head",
    );
    let candidates = store
        .find_reusable_prefixes(
            &owner,
            TurnNodeKind::Response,
            &ReusablePrefixQuery {
                namespace: "current".into(),
                fingerprints: vec![("left".into(), 1), ("right".into(), 1)],
                projection: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["right", "left"],
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.completed_at == 123)
    );
    let retained = store
        .materialize(&owner, TurnNodeKind::Response, &TurnNodeId::new("left"))
        .await
        .unwrap();
    assert!(retained.iter().all(
        |node| node.payload == serde_json::json!({"items": ["retained content ".repeat(100)]})
    ));
}

async fn overwrite_context_content(store: &SqlTurnChainStore, owner: &Principal, bytes: Vec<u8>) {
    let query = "UPDATE turn_chain_contents SET content = $1 WHERE principal = $2";
    match store {
        SqlTurnChainStore::Sqlite(pool) => {
            sqlx::query(query)
                .bind(bytes)
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
        }
        SqlTurnChainStore::Postgres(pool) => {
            sqlx::query(query)
                .bind(bytes)
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

async fn assert_request_context_contract(store: &SqlTurnChainStore) {
    let owner = principal("external-context");
    let root = TurnNodeId::response();
    let first = TurnNodeId::response();
    let second = TurnNodeId::response();
    let expired = TurnNodeId::response();
    let item = serde_json::json!({"text": "shared external history ".repeat(100)});
    let key = content::content_key(&serde_json::to_vec(&item).unwrap());
    for (id, parent_id, payload, ttl) in [
        (root.clone(), None, serde_json::json!({"root": true}), 60),
        (
            first.clone(),
            Some(root.clone()),
            serde_json::json!({"items": [item.clone()]}),
            10,
        ),
        (
            second.clone(),
            Some(root.clone()),
            serde_json::json!({"items": [item.clone()]}),
            60,
        ),
        (
            expired.clone(),
            Some(root.clone()),
            serde_json::json!({"items": [item.clone()]}),
            0,
        ),
    ] {
        store
            .commit(TurnCommit {
                id,
                parent_id,
                payload,
                principal: owner.clone(),
                kind: TurnNodeKind::Response,
                payload_version: 1,
                idle_ttl: Duration::from_secs(ttl),
                reusable_prefix: None,
            })
            .await
            .unwrap();
    }
    let mut context = TurnReadContext::new(&owner, TurnNodeKind::Response);
    let chain = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &first, &mut context)
        .await
        .unwrap();
    assert!(
        context.cached_content(&key).is_none(),
        "restoration is not admission"
    );
    let decoded_root: Arc<dyn std::any::Any + Send + Sync> = Arc::new("folded root");
    let decoded_first: Arc<dyn std::any::Any + Send + Sync> = Arc::new("folded first");
    context.admit(
        &chain,
        vec![(decoded_root.clone(), 128), (decoded_first, 128)],
    );
    let content = context
        .cached_content(&key)
        .expect("successful fold admits external decode");
    assert!(context.get(&root).unwrap().expires_at > context.get(&first).unwrap().expires_at);
    overwrite_context_content(store, &owner, vec![0]).await;
    let sibling = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &second, &mut context)
        .await
        .unwrap();
    assert_eq!(
        sibling.nodes[1].raw.as_ref().unwrap().payload["items"][0],
        item
    );
    assert!(Arc::ptr_eq(
        sibling.nodes[0].decoded.as_ref().unwrap(),
        &decoded_root
    ));
    assert!(
        Arc::ptr_eq(&context.cached_content(&key).unwrap(), &content),
        "new suffix reuses external decode"
    );
    assert!(
        sibling.expires_at > chain.expires_at,
        "short sibling does not shorten independent head TTL"
    );
    assert!(matches!(
        store
            .materialize_in_context(&owner, TurnNodeKind::Response, &expired, &mut context)
            .await,
        Err(TurnUnavailable::Unavailable)
    ));
    assert!(matches!(
        store
            .materialize_in_context(
                &owner,
                TurnNodeKind::Response,
                &TurnNodeId::response(),
                &mut context
            )
            .await,
        Err(TurnUnavailable::Unavailable)
    ));
    assert!(matches!(
        store
            .materialize_in_context(
                &principal("foreign"),
                TurnNodeKind::Response,
                &second,
                &mut context
            )
            .await,
        Err(TurnUnavailable::Unavailable)
    ));
    assert!(matches!(
        store
            .materialize_in_context(&owner, TurnNodeKind::Agent, &second, &mut context)
            .await,
        Err(TurnUnavailable::Unavailable)
    ));
    let mut fresh = TurnReadContext::new(&owner, TurnNodeKind::Response);
    assert!(matches!(
        store
            .materialize_in_context(&owner, TurnNodeKind::Response, &second, &mut fresh)
            .await,
        Err(TurnUnavailable::Storage(_))
    ));
    assert!(fresh.is_empty());
    assert!(fresh.cached_content(&key).is_none());
    let foreign = principal("foreign-reader");
    let mut foreign_context = TurnReadContext::new(&foreign, TurnNodeKind::Response);
    assert!(
        matches!(
            store
                .materialize_in_context(
                    &foreign,
                    TurnNodeKind::Response,
                    &root,
                    &mut foreign_context
                )
                .await,
            Err(TurnUnavailable::Unavailable)
        ),
        "a fresh foreign context cannot fetch owner nodes"
    );
    let corrupt = TurnNodeId::response();
    store
        .commit(TurnCommit {
            id: corrupt.clone(),
            parent_id: Some(root.clone()),
            principal: owner.clone(),
            kind: TurnNodeKind::Response,
            payload_version: 1,
            payload: serde_json::json!({"inline": true}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .unwrap();
    let query = "UPDATE turn_chain_nodes SET payload = $1 WHERE id = $2";
    match store {
        SqlTurnChainStore::Sqlite(pool) => {
            sqlx::query(query)
                .bind(vec![0u8])
                .bind(corrupt.as_str())
                .execute(pool)
                .await
                .unwrap();
        }
        SqlTurnChainStore::Postgres(pool) => {
            sqlx::query(query)
                .bind(vec![0u8])
                .bind(corrupt.as_str())
                .execute(pool)
                .await
                .unwrap();
        }
    }
    assert!(
        matches!(
            store
                .materialize_in_context(&owner, TurnNodeKind::Response, &corrupt, &mut context)
                .await,
            Err(TurnUnavailable::Storage(_))
        ),
        "cached ancestry must not conceal a corrupt new suffix"
    );
    assert!(context.get(&corrupt).is_none());

    let encoded = crate::storage_codec::encode(serde_json::to_vec(&item).unwrap().into()).unwrap();
    overwrite_context_content(store, &owner, encoded).await;
    // Abandoning a materialization models a failed domain fold: the next read
    // clears its pending content, so admitting only the root cannot leak it.
    let mut abandoned = TurnReadContext::new(&owner, TurnNodeKind::Response);
    store
        .materialize_in_context(&owner, TurnNodeKind::Response, &first, &mut abandoned)
        .await
        .unwrap();
    let root_chain = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &root, &mut abandoned)
        .await
        .unwrap();
    abandoned.admit(&root_chain, vec![(decoded_root.clone(), 128)]);
    assert!(abandoned.cached_content(&key).is_none());

    let mut budget = TurnReadContext::with_budget(&owner, TurnNodeKind::Response, 256);
    let root_chain = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &root, &mut budget)
        .await
        .unwrap();
    budget.admit(&root_chain, vec![(decoded_root.clone(), 128)]);
    let branch = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &first, &mut budget)
        .await
        .unwrap();
    budget.admit(
        &branch,
        vec![(decoded_root, 0), (Arc::new("folded suffix"), 128)],
    );
    assert!(
        !budget.admission_open(),
        "external content shares the node budget"
    );
    assert!(
        budget.get(&root).is_some(),
        "budget refusal preserves existing hits"
    );
    assert!(budget.get(&first).is_none());
    assert!(budget.cached_content(&key).is_none());
    let fallback = store
        .materialize_in_context(&owner, TurnNodeKind::Response, &second, &mut budget)
        .await
        .unwrap();
    assert_eq!(
        fallback.nodes[1].raw.as_ref().unwrap().payload["items"][0],
        item
    );
    assert!(budget.cached_content(&key).is_none());
    overwrite_context_content(store, &owner, vec![0]).await;
    assert!(
        matches!(
            store
                .materialize_in_context(&owner, TurnNodeKind::Response, &first, &mut budget)
                .await,
            Err(TurnUnavailable::Storage(_))
        ),
        "closed admission must not stage fallback content"
    );
    // A missing owner-scoped row must not be replaced by a foreign row.
    match store {
        SqlTurnChainStore::Sqlite(pool) => {
            sqlx::query("DELETE FROM turn_chain_node_contents WHERE principal = $1")
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM turn_chain_contents WHERE principal = $1")
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
        }
        SqlTurnChainStore::Postgres(pool) => {
            sqlx::query("DELETE FROM turn_chain_node_contents WHERE principal = $1")
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM turn_chain_contents WHERE principal = $1")
                .bind(owner.continuation_key())
                .execute(pool)
                .await
                .unwrap();
        }
    }
    store
        .commit(TurnCommit {
            id: TurnNodeId::response(),
            parent_id: None,
            principal: principal("foreign-content"),
            kind: TurnNodeKind::Response,
            payload_version: 1,
            payload: serde_json::json!({"items": [item]}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        store
            .materialize_in_context(&owner, TurnNodeKind::Response, &second, &mut fresh)
            .await,
        Err(TurnUnavailable::Storage(_))
    ));
    assert!(fresh.cached_content(&key).is_none());
}

#[tokio::test]
async fn request_context_external_suffix_reuse_errors_and_budget_fallback() {
    assert_request_context_contract(&test_store().await).await;
}

#[tokio::test]
async fn sqlite_0007_upgrade_and_generation_v3_rebuild_find_existing_history() {
    use crate::generation_chain::{GenerationChain, GenerationSource};
    use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
    use stravia_runtime_contract::protocol::ir::{
        AiItem, AiRequest, AiResponse, MessageContent, Role,
    };

    let owner = principal("upgraded-generation");
    let question = AiItem {
        role: Role::User,
        content: MessageContent::Text(Arc::new("question".into())),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let request = |items| {
        let mut request = AiRequest::new("model", items);
        request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
        request
    };
    // Obtain a real persisted generation payload through the write interface,
    // rather than constructing a payload coupled to private generation fields.
    let seed_store = Arc::new(test_store().await);
    let seed_chain = GenerationChain::from_turn_chain(
        seed_store.clone(),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    let mut write = seed_chain
        .begin(owner.clone(), request(vec![question.clone()]))
        .await
        .unwrap();
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    let source = GenerationSource::Target(crate::history_marker::ThinkingSource {
        namespace: "provider:model".into(),
        protocol: Some(OPEN_RESPONSES_2026_04_24.into()),
        actual_model: "model".into(),
        target_id: "provider:model".into(),
        authority: Some("provider:model".into()),
        issued: Vec::new(),
    });
    assert!(write.stage(&mut response, &source, None));
    write.persist().await.unwrap();
    let id = TurnNodeId::new(write.id());
    let mut nodes = seed_store
        .materialize(&owner, TurnNodeKind::Response, &id)
        .await
        .unwrap();
    // 升级前的不可变节点没有控制摘要版本，重建不能把它追认为当前版本的证明。
    nodes[0].payload["client_history"]
        .as_object_mut()
        .unwrap()
        .remove("projection_version");
    let mut rebuild = crate::generation_chain::PrefixRebuild::default();
    let mut expected = None;
    for node in nodes.clone() {
        expected = rebuild.decode(node, Some(123)).unwrap();
    }
    let expected = expected.unwrap();

    let data_dir = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(data_dir.path()).await.unwrap();
    let migrator = sqlx::migrate!("./migrations/sqlite");
    migrator.run_to(7, pool.migration_pool()).await.unwrap();
    assert!(
        expected
            .namespace
            .starts_with("stravia-generation-history-v5:")
    );
    let mut encoded = content::encode(nodes[0].payload.clone()).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let mut transaction = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO turn_chain_nodes \
        (id, kind, parent_id, principal, payload_version, payload, legacy_payload, created_at, expires_at, \
         prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at, storage_format) \
        VALUES (?, 'response', NULL, ?, ?, ?, NULL, ?, ?, 'generation:v2:legacy', 'obsolete', 2, 123, 2)")
        .bind(id.as_str()).bind(owner.continuation_key()).bind(i64::from(nodes[0].payload_version))
        .bind(&encoded.payload).bind(now).bind(now + 60_000).execute(&mut *transaction).await.unwrap();
    content::put_sqlite(
        &mut transaction,
        id.as_str(),
        &owner.continuation_key(),
        &mut encoded,
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .unwrap();
    let store = Arc::new(SqlTurnChainStore::sqlite(pool.clone()));
    let proof = expected.projection.clone().unwrap();
    let query = ReusablePrefixQuery {
        namespace: expected.namespace.clone(),
        fingerprints: vec![(expected.fingerprint.clone(), expected.item_count)],
        projection: Some(ReusablePrefixProjectionQuery {
            version: proof.version,
            controls_fingerprint: proof.controls_fingerprint,
            context_fingerprints: vec![(proof.context_fingerprint, expected.item_count)],
        }),
    };
    assert!(
        store
            .find_reusable_prefixes(&owner, TurnNodeKind::Response, &query)
            .await
            .unwrap()
            .is_empty()
    );
    let upgraded_chain = GenerationChain::from_turn_chain(
        store.clone(),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    upgraded_chain.rebuild_prefixes().await.unwrap();
    let candidates = store
        .find_reusable_prefixes(&owner, TurnNodeKind::Response, &query)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].node_id, id);
    assert_eq!(candidates[0].completed_at, 123);
    assert_eq!(
        store
            .materialize(&owner, TurnNodeKind::Response, &id)
            .await
            .unwrap(),
        nodes
    );
    let mut replay = vec![question];
    replay.extend(response.items);
    replay.push(AiItem {
        role: Role::User,
        content: MessageContent::Text(Arc::new("next".into())),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    });
    let resumed = upgraded_chain.begin(owner, request(replay)).await.unwrap();
    assert_eq!(
        resumed.parent_id(),
        Some(id.as_str()),
        "upgraded index drives authoritative history discovery"
    );
}
