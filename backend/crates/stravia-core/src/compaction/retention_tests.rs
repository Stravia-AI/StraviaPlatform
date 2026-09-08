use super::*;

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_retention_contracts() {
    let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
        .expect("set STRAVIA_TEST_POSTGRES_URL to run PostgreSQL retention contracts");
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("PostgreSQL admin pool");
    let schema = format!("stravia_retention_test_{}", uuid::Uuid::new_v4().simple());
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().expect("PostgreSQL connection options");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("isolated PostgreSQL pool");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create isolated PostgreSQL schema");

    // Keep cleanup outside the task so migration or contract panics still close
    // the pool and drop the schema before the failure is propagated.
    let scenario_pool = pool.clone();
    let result = tokio::spawn(async move {
        crate::migrations::migrate_postgres(&scenario_pool)
            .await
            .expect("PostgreSQL migrations");
        assert_resolving_a_branch_renews_its_predecessors_but_not_its_sibling(
            Compaction::postgres(scenario_pool.clone()),
            RetentionPool::Postgres(scenario_pool.clone()),
        )
        .await;
        reset_postgres_retention(&scenario_pool).await;
        assert_explicit_reference_retention_renews_every_branch_of_native_ancestry(
            Compaction::postgres(scenario_pool.clone()),
            RetentionPool::Postgres(scenario_pool.clone()),
        )
        .await;
        reset_postgres_retention(&scenario_pool).await;
        assert_expired_branch_cannot_resolve_or_supply_a_new_registration_source(
            Compaction::postgres(scenario_pool.clone()),
            RetentionPool::Postgres(scenario_pool.clone()),
        )
        .await;
        reset_postgres_retention(&scenario_pool).await;
        assert_same_native_id_with_different_content_is_conflicting_not_a_source_match(
            Compaction::postgres(scenario_pool),
        )
        .await;
    })
    .await;

    pool.close().await;
    let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await;
    admin.close().await;
    cleanup.expect("drop isolated PostgreSQL schema");
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("PostgreSQL retention task failed: {error}");
    }
}

async fn reset_postgres_retention(pool: &sqlx::PgPool) {
    sqlx::query("TRUNCATE native_compaction_sources, native_compaction_states, native_compactions")
        .execute(pool)
        .await
        .expect("reset retention scenarios within isolated schema");
}

fn state(id: &str, content: &str) -> AiItem {
    let wire = serde_json::json!({
        "type": "compaction",
        "id": id,
        "encrypted_content": content,
    });
    crate::protocol::codec::open_responses::decoder::decode_input_item(&wire)
        .expect("native state")
        .expect("item")
}

async fn store() -> (Compaction, sqlx::SqlitePool) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    (Compaction::sqlite(pool.clone()), pool)
}

fn registration(
    operation: &str,
    item: &AiItem,
    predecessors: &[&CompactionRecord],
) -> CompactionRegistration {
    CompactionRegistration {
        source_generation_id: None,
        source_record_ids: predecessors
            .iter()
            .map(|record| record.id.clone())
            .collect(),
        operation_id: operation.into(),
        target: CompactionTarget {
            target_key: "local-target".into(),
            namespace: "local-account-generation".into(),
            model: "local-model".into(),
            protocol: "open-responses/responses/2026-04-24".into(),
        },
        window: vec![item.clone()],
        state_items: vec![item.clone()],
    }
}

// Advance every stored deadline together, preserving the distinction between
// pending and renewed records without sleeps or assertions on storage fields.
enum RetentionPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

async fn advance_expiry(pool: &RetentionPool, elapsed: Duration) {
    let elapsed = i64::try_from(elapsed.as_millis()).unwrap();
    let query = "UPDATE native_compactions SET expires_at = expires_at - $1";
    match pool {
        RetentionPool::Sqlite(pool) => {
            sqlx::query(query)
                .bind(elapsed)
                .execute(pool)
                .await
                .unwrap();
        }
        RetentionPool::Postgres(pool) => {
            sqlx::query(query)
                .bind(elapsed)
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn resolving_a_branch_renews_its_predecessors_but_not_its_sibling() {
    let (store, pool) = store().await;
    assert_resolving_a_branch_renews_its_predecessors_but_not_its_sibling(
        store,
        RetentionPool::Sqlite(pool),
    )
    .await;
}

async fn assert_resolving_a_branch_renews_its_predecessors_but_not_its_sibling(
    store: Compaction,
    pool: RetentionPool,
) {
    let owner = Principal::new("owner");
    let root_state = state("root", "root-state");
    let left_state = state("left", "left-state");
    let right_state = state("right", "right-state");
    let tip_state = state("tip", "tip-state");
    let root = store
        .register(&owner, registration("root", &root_state, &[]))
        .await
        .unwrap();
    let left = store
        .register(&owner, registration("left", &left_state, &[&root]))
        .await
        .unwrap();
    store
        .register(&owner, registration("right", &right_state, &[&root]))
        .await
        .unwrap();
    store
        .register(&owner, registration("tip", &tip_state, &[&left]))
        .await
        .unwrap();

    store
        .resolve(&owner, &[tip_state.clone()])
        .await
        .unwrap()
        .unwrap();
    advance_expiry(&pool, PENDING_RETENTION * 2).await;
    store.cleanup_expired().await.unwrap();

    let resolved = store
        .resolve(&owner, &[root_state.clone(), left_state, tip_state.clone()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.operation_id, "tip");
    assert!(canonical::history_items_equal(
        &resolved.window,
        &[tip_state]
    ));
    assert_eq!(
        store
            .resolve(&owner, &[root_state])
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        "root"
    );
    assert!(
        store
            .resolve(&owner, &[right_state])
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn explicit_reference_retention_renews_every_branch_of_native_ancestry() {
    let (store, pool) = store().await;
    assert_explicit_reference_retention_renews_every_branch_of_native_ancestry(
        store,
        RetentionPool::Sqlite(pool),
    )
    .await;
}

async fn assert_explicit_reference_retention_renews_every_branch_of_native_ancestry(
    store: Compaction,
    pool: RetentionPool,
) {
    let owner = Principal::new("owner");
    let root_state = state("root", "root-state");
    let left_state = state("left", "left-state");
    let right_state = state("right", "right-state");
    let merged_state = state("merged", "merged-state");
    let root = store
        .register(&owner, registration("root", &root_state, &[]))
        .await
        .unwrap();
    let left = store
        .register(&owner, registration("left", &left_state, &[&root]))
        .await
        .unwrap();
    let right = store
        .register(&owner, registration("right", &right_state, &[&root]))
        .await
        .unwrap();
    let merged = store
        .register(
            &owner,
            registration("merged", &merged_state, &[&left, &right]),
        )
        .await
        .unwrap();

    store.extend_retention(&owner, &[merged.id]).await.unwrap();
    advance_expiry(&pool, PENDING_RETENTION * 2).await;
    store.cleanup_expired().await.unwrap();

    // Both branches are required by this descendant; neither registration order
    // nor traversal order may turn one branch into the selected source.
    let resolved = store
        .resolve(
            &owner,
            &[
                root_state,
                left_state.clone(),
                right_state.clone(),
                merged_state.clone(),
            ],
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.operation_id, "merged");
    assert!(canonical::history_items_equal(
        &resolved.window,
        &[merged_state]
    ));
    assert!(matches!(
        store.resolve(&owner, &[left_state, right_state]).await,
        Err(CompactionError::Conflict)
    ));
}

#[tokio::test]
async fn expired_branch_cannot_resolve_or_supply_a_new_registration_source() {
    let (store, pool) = store().await;
    assert_expired_branch_cannot_resolve_or_supply_a_new_registration_source(
        store,
        RetentionPool::Sqlite(pool),
    )
    .await;
}

async fn assert_expired_branch_cannot_resolve_or_supply_a_new_registration_source(
    store: Compaction,
    pool: RetentionPool,
) {
    let owner = Principal::new("owner");
    let root_state = state("root", "root-state");
    let old_state = state("old", "old-state");
    let live_state = state("live", "live-state");
    let root = store
        .register(&owner, registration("root", &root_state, &[]))
        .await
        .unwrap();
    let old = store
        .register(&owner, registration("old", &old_state, &[&root]))
        .await
        .unwrap();
    let live = store
        .register(&owner, registration("live", &live_state, &[&root]))
        .await
        .unwrap();
    store.confirm_delivery(&owner, &[live.id]).await.unwrap();
    advance_expiry(&pool, PENDING_RETENTION * 2).await;

    // Expiry must be respected even before the cleanup worker runs.
    assert!(
        store
            .resolve(&owner, &[old_state.clone()])
            .await
            .unwrap()
            .is_none()
    );
    store.cleanup_expired().await.unwrap();
    store.cleanup_expired().await.unwrap();
    assert!(store.resolve(&owner, &[old_state]).await.unwrap().is_none());
    let continuation = state("continuation", "continuation-state");
    assert!(matches!(
        store
            .register(&owner, registration("continuation", &continuation, &[&old]))
            .await,
        Err(CompactionError::Unavailable)
    ));
    assert!(
        store
            .resolve(&owner, &[continuation])
            .await
            .unwrap()
            .is_none()
    );
    let resolved = store
        .resolve(&owner, &[root_state, live_state.clone()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.operation_id, "live");
    assert!(canonical::history_items_equal(
        &resolved.window,
        &[live_state]
    ));
}

#[tokio::test]
async fn same_native_id_with_different_content_is_conflicting_not_a_source_match() {
    let (store, _) = store().await;
    assert_same_native_id_with_different_content_is_conflicting_not_a_source_match(store).await;
}

async fn assert_same_native_id_with_different_content_is_conflicting_not_a_source_match(
    store: Compaction,
) {
    let owner = Principal::new("owner");
    let original = state("shared-id", "original-ciphertext");
    let changed = state("shared-id", "different-ciphertext");
    store
        .register(&owner, registration("original", &original, &[]))
        .await
        .unwrap();
    assert!(matches!(
        store.resolve(&owner, &[changed.clone()]).await,
        Err(CompactionError::Conflict)
    ));

    // An exact fingerprint match must not hide a second, incompatible payload
    // with the same native identity, whichever payload the client submits.
    store
        .register(&owner, registration("changed", &changed, &[]))
        .await
        .unwrap();
    assert!(matches!(
        store.resolve(&owner, &[original]).await,
        Err(CompactionError::Conflict)
    ));
    assert!(matches!(
        store.resolve(&owner, &[changed]).await,
        Err(CompactionError::Conflict)
    ));
}
