use std::time::Duration;

use stravia_credential_protection::store::{DetectedSecret, MappingStore, SqlMappingStore};
use stravia_runtime_contract::Principal;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const HOUR_MS: i64 = 60 * 60 * 1_000;

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

async fn last_used_at(store: &SqlMappingStore, reference: &str) -> i64 {
    let sql = "SELECT last_used_at FROM reversible_redaction_mappings WHERE reference = $1";
    match store {
        SqlMappingStore::Sqlite(pool) => {
            sqlx::query_scalar(sql)
                .bind(reference)
                .fetch_one(pool)
                .await
        }
        SqlMappingStore::Postgres(pool) => {
            sqlx::query_scalar(sql)
                .bind(reference)
                .fetch_one(pool)
                .await
        }
    }
    .unwrap()
}

async fn set_last_used_at(store: &SqlMappingStore, reference: &str, value: i64) {
    let sql = "UPDATE reversible_redaction_mappings SET last_used_at = $2 WHERE reference = $1";
    match store {
        SqlMappingStore::Sqlite(pool) => sqlx::query(sql)
            .bind(reference)
            .bind(value)
            .execute(pool)
            .await
            .map(|_| ()),
        SqlMappingStore::Postgres(pool) => sqlx::query(sql)
            .bind(reference)
            .bind(value)
            .execute(pool)
            .await
            .map(|_| ()),
    }
    .unwrap();
}

async fn queued_for_reclamation(store: &SqlMappingStore, reference: &str, enqueue: bool) -> i64 {
    let insert =
        "INSERT INTO redaction_mapping_gc(reference) VALUES ($1) ON CONFLICT(reference) DO NOTHING";
    let count = "SELECT COUNT(*) FROM redaction_mapping_gc WHERE reference = $1";
    match store {
        SqlMappingStore::Sqlite(pool) => {
            if enqueue {
                sqlx::query(insert)
                    .bind(reference)
                    .execute(pool)
                    .await
                    .unwrap();
            }
            sqlx::query_scalar(count)
                .bind(reference)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        SqlMappingStore::Postgres(pool) => {
            if enqueue {
                sqlx::query(insert)
                    .bind(reference)
                    .execute(pool)
                    .await
                    .unwrap();
            }
            sqlx::query_scalar(count)
                .bind(reference)
                .fetch_one(pool)
                .await
                .unwrap()
        }
    }
}

async fn published_mapping(store: &SqlMappingStore, owner: &Principal, secret: &str) -> String {
    let reference = store
        .intern(owner, &[detect(secret)])
        .await
        .unwrap()
        .mappings
        .remove(0)
        .reference;
    store
        .publish(owner, std::slice::from_ref(&reference))
        .await
        .unwrap();
    reference
}

/// 重复使用在续期粒度内只读不写；超出粒度后才把起点推到当前。
async fn assert_use_renews_the_idle_window_at_most_once_per_hour(store: &SqlMappingStore) {
    let owner = Principal::new("renewal-owner");
    let reference = published_mapping(store, &owner, "renewal-secret").await;
    let first = last_used_at(store, &reference).await;

    store
        .publish(&owner, std::slice::from_ref(&reference))
        .await
        .unwrap();
    assert_eq!(
        last_used_at(store, &reference).await,
        first,
        "粒度内的重复使用不写库"
    );

    let stale = first - 2 * HOUR_MS;
    set_last_used_at(store, &reference, stale).await;
    store
        .publish(&owner, std::slice::from_ref(&reference))
        .await
        .unwrap();
    assert!(
        last_used_at(store, &reference).await >= first,
        "超过粒度的使用把起点推到当前"
    );
}

/// 保留期从最近一次使用起算：被使用过的映射活过首次发布后的 N 天，
/// 并且要超过 N 加续期粒度才被回收。
async fn assert_idle_window_slides_from_the_last_use(store: &SqlMappingStore) {
    let owner = Principal::new("sliding-owner");
    let reference = published_mapping(store, &owner, "sliding-secret").await;
    let used = now();
    // 先把水位线拉回到本场景之前。
    store.sweep(used - 400 * DAY_MS, DAY * 7).await.unwrap();
    set_last_used_at(store, &reference, used - 6 * DAY_MS).await;
    store.sweep(used, DAY * 7).await.unwrap();
    assert!(store.plaintext(&reference).await.unwrap().is_some());

    // 在第 6 天再次使用：窗口从此刻重新计算，早已超过首次发布后的 N 天仍保留。
    store
        .publish(&owner, std::slice::from_ref(&reference))
        .await
        .unwrap();
    store.sweep(used + 6 * DAY_MS, DAY * 7).await.unwrap();
    assert!(store.plaintext(&reference).await.unwrap().is_some());

    store
        .sweep(used + 7 * DAY_MS + HOUR_MS - 1_000, DAY * 7)
        .await
        .unwrap();
    assert!(
        store.plaintext(&reference).await.unwrap().is_some(),
        "未超过 N 加续期粒度不回收"
    );
    store
        .sweep(used + 7 * DAY_MS + HOUR_MS + 60_000, DAY * 7)
        .await
        .unwrap();
    assert_eq!(store.plaintext(&reference).await.unwrap(), None);
}

/// 已被扫描入队的映射在领取前被使用：领取时按最新的使用时刻复查，保留映射并移出队列。
async fn assert_reused_mapping_leaves_the_reclamation_queue(store: &SqlMappingStore) {
    let owner = Principal::new("queued-owner");
    let reference = published_mapping(store, &owner, "queued-secret").await;
    set_last_used_at(store, &reference, now() - 30 * DAY_MS).await;
    assert_eq!(queued_for_reclamation(store, &reference, true).await, 1);

    store
        .publish(&owner, std::slice::from_ref(&reference))
        .await
        .unwrap();
    store.sweep(now(), DAY * 7).await.unwrap();

    assert_eq!(
        store.plaintext(&reference).await.unwrap().as_deref(),
        Some("queued-secret")
    );
    assert_eq!(queued_for_reclamation(store, &reference, false).await, 0);
}

/// 并发 intern 只有一个事务可声明新建；规则随映射创建持久化，再次命中不改写归因；
/// 不同 Principal 的映射互相隔离。
async fn assert_concurrent_intern_keeps_rules_and_isolates_principals(store: &SqlMappingStore) {
    let owner = Principal::new("contract-owner");
    let other = Principal::new("contract-other");
    // 长明文同时覆盖 PostgreSQL 的索引大小边界。
    let secrets = vec![DetectedSecret {
        secret: (0..200)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect::<String>(),
        rule_ids: vec!["github-pat".into(), "custom_rule".into()],
    }];
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
    assert_eq!(first.reference, concurrent.reference);
    assert!(stravia_credential_protection::marker::valid_reference(
        &first.reference
    ));
    assert_eq!(first.secret, secrets[0].secret);
    assert_eq!(first.rule_ids.as_slice(), secrets[0].rule_ids);
    assert_eq!(concurrent.rule_ids.as_slice(), secrets[0].rule_ids);

    let redetected = [DetectedSecret {
        secret: secrets[0].secret.clone(),
        rule_ids: vec!["another-rule".into()],
    }];
    let again = store.intern(&owner, &redetected).await.unwrap();
    assert!(again.created.is_empty());
    assert_eq!(again.mappings[0].rule_ids.as_slice(), secrets[0].rule_ids);
    assert_eq!(
        store.active(&owner).await.unwrap()[0].rule_ids.as_slice(),
        secrets[0].rule_ids
    );

    assert!(store.active(&other).await.unwrap().is_empty());
    let mut foreign = store.intern(&other, &secrets).await.unwrap();
    assert_eq!(
        foreign.created.len(),
        1,
        "the same plaintext is a new mapping for another principal"
    );
    let foreign = foreign.mappings.remove(0);
    assert_ne!(first.reference, foreign.reference);
    assert_eq!(store.active(&owner).await.unwrap().len(), 1);
    assert_eq!(store.active(&other).await.unwrap().len(), 1);
}

/// 批量发布去重，忽略不存在的占位符，且只作用于调用方自己的映射。
async fn assert_batch_publish_is_scoped_to_the_principal(store: &SqlMappingStore) {
    let owner = Principal::new("batch-owner");
    let other = Principal::new("batch-other");
    let secrets: Vec<_> = (0..407)
        .map(|index| detect(&format!("batch-secret-{index}")))
        .collect();
    let references: Vec<_> = store
        .intern(&owner, &secrets)
        .await
        .unwrap()
        .mappings
        .into_iter()
        .map(|mapping| mapping.reference)
        .collect();
    let foreign = store
        .intern(&other, &[detect("foreign-batch-secret")])
        .await
        .unwrap()
        .mappings
        .remove(0)
        .reference;

    let mut requested = references[..405].to_vec();
    requested.extend_from_within(..);
    requested.push("missing-mapping".into());
    requested.push(foreign.clone());
    store.publish(&owner, &requested).await.unwrap();
    // 他人发布不影响本主体的映射。
    store
        .publish(&other, std::slice::from_ref(&references[405]))
        .await
        .unwrap();
    store.publish(&owner, &[]).await.unwrap();

    let published_at = now();
    // 先把水位线拉回到本场景之前，再越过未发布下限窗口扫描。
    store
        .sweep(published_at + 1_000, Duration::from_secs(3600))
        .await
        .unwrap();
    store
        .sweep(published_at + 2 * 3600 * 1_000, DAY * 7)
        .await
        .unwrap();
    for reference in &references[..405] {
        assert!(
            store.plaintext(reference).await.unwrap().is_some(),
            "published mappings outlive the pending window"
        );
    }
    for reference in [&references[405], &references[406], &foreign] {
        assert_eq!(
            store.plaintext(reference).await.unwrap(),
            None,
            "mappings the caller did not publish are reclaimed after the pending window"
        );
    }
    assert_eq!(store.active(&owner).await.unwrap().len(), 405);
    assert!(store.active(&other).await.unwrap().is_empty());

    store
        .sweep(published_at + 8 * 24 * 3600 * 1_000, DAY * 7)
        .await
        .unwrap();
    assert!(store.active(&owner).await.unwrap().is_empty());
}

#[tokio::test]
async fn sqlite_mapping_contract_concurrency_isolation_and_batch_publish() {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("migrated sqlite");
    let store = SqlMappingStore::sqlite(pool.clone());
    assert_concurrent_intern_keeps_rules_and_isolates_principals(&store).await;
    assert_batch_publish_is_scoped_to_the_principal(&store).await;
    pool.close().await;
}

#[tokio::test]
async fn sqlite_mapping_survives_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .unwrap();
    let owner = Principal::new("restart-owner");
    let mapping = SqlMappingStore::sqlite(pool.clone())
        .intern(&owner, &[detect("restart-secret")])
        .await
        .unwrap()
        .mappings
        .remove(0);
    pool.close().await;

    let reopened = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&reopened, None)
        .await
        .unwrap();
    let store = SqlMappingStore::sqlite(reopened.clone());
    let restored = store.active(&owner).await.unwrap();
    assert_eq!(restored[0].reference, mapping.reference);
    assert_eq!(restored[0].secret, "restart-secret");
    assert_eq!(restored[0].rule_ids.as_slice(), ["test-rule"]);
    reopened.close().await;
}

#[tokio::test]
async fn sqlite_mapping_reclaims_only_after_pending_or_published_floor() {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("migrated sqlite");
    let store = SqlMappingStore::sqlite(pool.clone());
    assert_unpublished_mapping_waits_for_pending_window(&store).await;
    assert_published_mapping_is_kept_for_retention(&store).await;
    assert_use_renews_the_idle_window_at_most_once_per_hour(&store).await;
    assert_idle_window_slides_from_the_last_use(&store).await;
    assert_reused_mapping_leaves_the_reclamation_queue(&store).await;
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
    let options_for_restart = options.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("isolated pool");
    crate::migrations::migrate_postgres(&pool, None)
        .await
        .unwrap();
    let store = SqlMappingStore::postgres(pool.clone());
    assert_unpublished_mapping_waits_for_pending_window(&store).await;
    assert_published_mapping_is_kept_for_retention(&store).await;
    assert_use_renews_the_idle_window_at_most_once_per_hour(&store).await;
    assert_idle_window_slides_from_the_last_use(&store).await;
    assert_reused_mapping_leaves_the_reclamation_queue(&store).await;
    assert_concurrent_intern_keeps_rules_and_isolates_principals(&store).await;
    assert_batch_publish_is_scoped_to_the_principal(&store).await;
    let restart_owner = Principal::new("restart-owner");
    let restart_mapping = store
        .intern(&restart_owner, &[detect("restart-secret")])
        .await
        .unwrap()
        .mappings
        .remove(0);
    pool.close().await;
    // 重启后仍可读：同一 schema 重新连接。
    let reopened = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options_for_restart)
        .await
        .expect("reopened pool");
    let restored = SqlMappingStore::postgres(reopened.clone())
        .active(&restart_owner)
        .await
        .unwrap();
    assert_eq!(restored[0].reference, restart_mapping.reference);
    reopened.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop schema");
    admin.close().await;
}
