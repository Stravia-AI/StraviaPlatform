use std::time::Duration;
use stravia_credential_protection::store::{MappingStore, SqlMappingStore};
use stravia_runtime_contract::Principal;

const PUBLISHED_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn after(now: i64, retention: Duration) -> i64 {
    now.saturating_add(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX))
}

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
    assert!(stravia_credential_protection::marker::valid_reference(
        &first.reference
    ));
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
async fn sqlite_redaction_marker_format_migrates_strict_legacy_references_without_data_loss() {
    const OWNER_ID: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_ID: &str = "fedcba9876543210fedcba9876543210";
    const ALREADY_NEW: &str =
        "<!-- stravia-redaction-marker:rm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb -->";
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../migrations/sqlite/0035_reversible_redaction.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let expires_at = now() + 24 * 60 * 60 * 1_000;
    let owner_legacy = format!("~stravia-secret:{OWNER_ID}~");
    let other_legacy = format!("~stravia-secret:{OTHER_ID}~");
    for (reference, principal, secret, published_at, created_at, updated_at) in [
        (
            owner_legacy.as_str(),
            "migration-owner",
            "owner-value",
            Some(41_i64),
            11_i64,
            31_i64,
        ),
        (
            other_legacy.as_str(),
            "migration-other",
            "other-value",
            None,
            12,
            32,
        ),
        (
            "~stravia-secret:0123456789abcdef0123456789abcdeF~",
            "uppercase",
            "uppercase-value",
            None,
            13,
            33,
        ),
        (
            "~stravia-secret:0123456789abcdef0123456789abcdeg~",
            "non-hex",
            "non-hex-value",
            None,
            14,
            34,
        ),
        (ALREADY_NEW, "already-new", "new-value", None, 15, 35),
    ] {
        sqlx::query(
            "INSERT INTO reversible_redaction_mappings
             (reference, principal, secret, published_at, created_at, updated_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(reference)
        .bind(Principal::new(principal).continuation_key())
        .bind(secret)
        .bind(published_at)
        .bind(created_at)
        .bind(updated_at)
        .bind(expires_at)
        .execute(&pool)
        .await
        .unwrap();
    }

    let migration = include_str!("../../migrations/sqlite/0046_redaction_marker_format.sql");
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();

    let owner_reference = format!("<!-- stravia-redaction-marker:rm_{OWNER_ID} -->");
    let other_reference = format!("<!-- stravia-redaction-marker:rm_{OTHER_ID} -->");
    let migrated: (String, String, String, Option<i64>, i64, i64, i64) = sqlx::query_as(
        "SELECT reference, principal, secret, published_at, created_at, updated_at, expires_at
         FROM reversible_redaction_mappings WHERE principal = 'api-key:migration-owner'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(migrated.0, owner_reference);
    assert_eq!(
        migrated.1,
        Principal::new("migration-owner").continuation_key()
    );
    assert!(migrated.2 == "owner-value", "secret must be preserved");
    assert_eq!(migrated.3, Some(41));
    assert_eq!((migrated.4, migrated.5, migrated.6), (11, 31, expires_at));
    let pending: (Option<i64>, i64, i64, i64) = sqlx::query_as(
        "SELECT published_at, created_at, updated_at, expires_at
         FROM reversible_redaction_mappings WHERE principal = 'api-key:migration-other'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending, (None, 12, 32, expires_at));

    for (principal, unchanged) in [
        (
            "uppercase",
            "~stravia-secret:0123456789abcdef0123456789abcdeF~",
        ),
        (
            "non-hex",
            "~stravia-secret:0123456789abcdef0123456789abcdeg~",
        ),
        ("already-new", ALREADY_NEW),
    ] {
        let reference: String = sqlx::query_scalar(
            "SELECT reference FROM reversible_redaction_mappings WHERE principal = ?",
        )
        .bind(Principal::new(principal).continuation_key())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(reference, unchanged);
    }

    let store = SqlMappingStore::sqlite(pool.clone());
    let owner = Principal::new("migration-owner");
    let other = Principal::new("migration-other");
    let owner_mappings = store.active(&owner).await.unwrap();
    let other_mappings = store.active(&other).await.unwrap();
    assert_eq!(owner_mappings.len(), 1);
    assert_eq!(owner_mappings[0].reference, owner_reference);
    assert!(
        owner_mappings[0].secret == "owner-value",
        "secret must remain readable"
    );
    assert_eq!(other_mappings.len(), 1);
    assert_eq!(other_mappings[0].reference, other_reference);
    assert!(
        store
            .active(&Principal::new("migration-unrelated"))
            .await
            .unwrap()
            .is_empty()
    );

    let long = Duration::from_secs(30 * 24 * 60 * 60);
    store
        .extend_retention(&other, std::slice::from_ref(&owner_reference), long)
        .await
        .unwrap();
    assert_eq!(
        store.active(&owner).await.unwrap()[0].expires_at,
        expires_at
    );
    store
        .extend_retention(&owner, std::slice::from_ref(&owner_reference), long)
        .await
        .unwrap();
    assert!(store.active(&owner).await.unwrap()[0].expires_at > expires_at);
    let publication: Option<i64> = sqlx::query_scalar(
        "SELECT published_at FROM reversible_redaction_mappings WHERE reference = ?",
    )
    .bind(&owner_reference)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(publication, Some(41));
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
