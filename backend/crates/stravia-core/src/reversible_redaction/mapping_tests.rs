use std::time::Duration;

use stravia_credential_protection::store::{DetectedSecret, MappingStore, SqlMappingStore};
use stravia_runtime_contract::Principal;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn detect(value: &str) -> DetectedSecret {
    DetectedSecret {
        secret: value.into(),
        rule_ids: vec!["test-rule".into()],
    }
}

async fn assert_unpublished_mapping_waits_for_pending_window(store: &SqlMappingStore) {
    let owner = Principal::new("mapping-owner");
    let mapping = store
        .intern(&owner, &[detect("Q8n4Vk7sT2p9X5a3Lc6D0h1R")])
        .await
        .unwrap()
        .mappings
        .remove(0);
    let reference = mapping.reference.clone();
    let interned_at = now();
    assert_eq!(
        store
            .active(&owner)
            .await
            .unwrap()
            .first()
            .map(|mapping| mapping.reference.as_str()),
        Some(reference.as_str())
    );
    assert_eq!(
        store.plaintext(&reference).await.unwrap().as_deref(),
        Some("Q8n4Vk7sT2p9X5a3Lc6D0h1R")
    );

    store
        .sweep(interned_at + 1_000, Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(
        store.plaintext(&reference).await.unwrap().as_deref(),
        Some("Q8n4Vk7sT2p9X5a3Lc6D0h1R"),
        "未发布的映射在 pending 窗口内仍可供同一轮提交引用"
    );

    store
        .sweep(interned_at + 2 * 3600 * 1_000, DAY)
        .await
        .unwrap();
    assert_eq!(store.plaintext(&reference).await.unwrap(), None);
    assert!(store.active(&owner).await.unwrap().is_empty());
}

async fn assert_published_mapping_is_kept_for_retention(store: &SqlMappingStore) {
    let owner = Principal::new("published-owner");
    let mapping = store
        .intern(&owner, &[detect("published-secret")])
        .await
        .unwrap()
        .mappings
        .remove(0);
    let reference = mapping.reference.clone();
    store
        .publish(&owner, std::slice::from_ref(&reference))
        .await
        .unwrap();
    let published_at = now();

    store
        .sweep(published_at + 6 * 24 * 3600 * 1_000, DAY * 7)
        .await
        .unwrap();
    assert_eq!(
        store.plaintext(&reference).await.unwrap().as_deref(),
        Some("published-secret")
    );

    store
        .sweep(published_at + 8 * 24 * 3600 * 1_000, DAY * 7)
        .await
        .unwrap();
    assert_eq!(store.plaintext(&reference).await.unwrap(), None);
    assert!(store.active(&owner).await.unwrap().is_empty());
}

#[tokio::test]
async fn sqlite_mapping_reclaims_only_after_pending_or_published_floor() {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("migrated sqlite");
    let store = SqlMappingStore::sqlite(pool.clone());
    assert_unpublished_mapping_waits_for_pending_window(&store).await;
    assert_published_mapping_is_kept_for_retention(&store).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at PostgreSQL"]
async fn postgres_mapping_reclaims_only_after_pending_or_published_floor() {
    let url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("DB_URL"))
        .expect("DATABASE_URL");
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("admin pool");
    let schema = format!("stravia_mapping_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create schema");
    let options: sqlx::postgres::PgConnectOptions = url.parse().expect("options");
    let options = options.options([("search_path", schema.as_str())]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("isolated pool");
    crate::migrations::migrate_postgres(&pool, None)
        .await
        .unwrap();
    let store = SqlMappingStore::postgres(pool.clone());
    assert_unpublished_mapping_waits_for_pending_window(&store).await;
    assert_published_mapping_is_kept_for_retention(&store).await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop schema");
    admin.close().await;
}
