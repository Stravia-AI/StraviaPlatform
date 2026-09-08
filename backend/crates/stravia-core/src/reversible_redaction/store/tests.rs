use super::*;

async fn expire(store: &SqlMappingStore, reference: &str) {
    match store {
        SqlMappingStore::Sqlite(pool) => {
            sqlx::query(
                "UPDATE reversible_redaction_mappings SET expires_at = 0 WHERE reference = ?",
            )
            .bind(reference)
            .execute(pool)
            .await
            .unwrap();
        }
        SqlMappingStore::Postgres(pool) => {
            sqlx::query(
                "UPDATE reversible_redaction_mappings SET expires_at = 0 WHERE reference = $1",
            )
            .bind(reference)
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

async fn assert_store_contract(store: &SqlMappingStore) -> (Principal, String, String) {
    let owner = Principal::new("mapping-owner");
    let other = Principal::new("mapping-other");
    // Large, incompressible-enough plaintext also exercises PostgreSQL's index-size boundary.
    let secrets = vec![
        (0..200)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect::<String>(),
    ];
    let second = store.clone();
    let (left, right) = tokio::join!(
        store.intern(&owner, &secrets),
        second.intern(&owner, &secrets)
    );
    let mut left = left.unwrap();
    let mut right = right.unwrap();
    assert_eq!(
        left.created.len() + right.created.len(),
        1,
        "only the transaction that created the shared mapping may claim discovery"
    );
    let first = left.mappings.remove(0);
    let concurrent = right.mappings.remove(0);
    assert!(
        store
            .intern(&owner, &secrets)
            .await
            .unwrap()
            .created
            .is_empty()
    );
    assert_eq!(first.reference, concurrent.reference);
    assert_eq!(first.secret, secrets[0]);
    assert_eq!(first.expires_at, concurrent.expires_at);
    assert!(store.active(&other).await.unwrap().is_empty());
    let mut foreign = store.intern(&other, &secrets).await.unwrap();
    assert_eq!(foreign.created.len(), 1);
    let foreign = foreign.mappings.remove(0);
    assert_ne!(first.reference, foreign.reference);

    let references = vec![first.reference.clone()];
    let long = Duration::from_secs(30 * 24 * 60 * 60);
    store
        .extend_retention(&owner, &references, long)
        .await
        .unwrap();
    assert_eq!(
        store.active(&owner).await.unwrap()[0].expires_at,
        first.expires_at
    );
    store.publish(&other, &references, long).await.unwrap();
    assert_eq!(
        store.active(&owner).await.unwrap()[0].expires_at,
        first.expires_at
    );

    let before_publish = now();
    store
        .publish(&owner, &references, Duration::ZERO)
        .await
        .unwrap();
    let published = store.active(&owner).await.unwrap().remove(0);
    assert!(published.expires_at >= after(before_publish, PUBLISHED_RETENTION));
    let before_extend = now();
    store
        .extend_retention(&owner, &references, long)
        .await
        .unwrap();
    let extended = store.active(&owner).await.unwrap().remove(0);
    assert!(extended.expires_at >= after(before_extend, long));
    store
        .extend_retention(&owner, &references, Duration::ZERO)
        .await
        .unwrap();
    store
        .publish(&owner, &references, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        store.active(&owner).await.unwrap()[0].expires_at,
        extended.expires_at
    );

    expire(store, &first.reference).await;
    assert!(store.active(&owner).await.unwrap().is_empty());
    store.publish(&owner, &references, long).await.unwrap();
    store
        .extend_retention(&owner, &references, long)
        .await
        .unwrap();
    assert!(store.active(&owner).await.unwrap().is_empty());
    let mut replacement = store.intern(&owner, &secrets).await.unwrap();
    assert_eq!(
        replacement.created.len(),
        1,
        "expired mappings permit a new discovery"
    );
    let replacement = replacement.mappings.remove(0);
    assert_ne!(replacement.reference, first.reference);
    assert_eq!(replacement.secret, secrets[0]);
    assert_eq!(store.cleanup_expired().await.unwrap(), 1);
    assert_eq!(
        store.active(&owner).await.unwrap()[0].reference,
        replacement.reference
    );
    assert_eq!(
        store.active(&other).await.unwrap()[0].reference,
        foreign.reference
    );
    (owner, replacement.reference, replacement.secret)
}

#[tokio::test]
async fn sqlite_mapping_concurrency_lifecycle_isolation_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let store = SqlMappingStore::sqlite(pool.clone());
    let (owner, reference, secret) = assert_store_contract(&store).await;
    drop(store);
    pool.close().await;
    let reopened = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&reopened).await.unwrap();
    let restored = SqlMappingStore::sqlite(reopened.clone())
        .active(&owner)
        .await
        .unwrap();
    assert_eq!(restored[0].reference, reference);
    assert_eq!(restored[0].secret, secret);
    reopened.close().await;
}

#[tokio::test]
async fn postgres_mapping_contract_when_configured() {
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
    let schema = format!("stravia_redaction_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create isolated PostgreSQL schema");
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().expect("PostgreSQL connection options");
    let options = options.options([("search_path", schema.as_str())]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.clone())
        .await
        .expect("isolated PostgreSQL pool");
    crate::migrations::migrate_postgres(&pool).await.unwrap();
    let store = SqlMappingStore::postgres(pool.clone());
    let (owner, reference, secret) = assert_store_contract(&store).await;
    drop(store);
    pool.close().await;
    let reopened = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    crate::migrations::migrate_postgres(&reopened)
        .await
        .unwrap();
    let restored = SqlMappingStore::postgres(reopened.clone())
        .active(&owner)
        .await
        .unwrap();
    assert_eq!(restored[0].reference, reference);
    assert_eq!(restored[0].secret, secret);
    reopened.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop isolated PostgreSQL schema");
    admin.close().await;
}
