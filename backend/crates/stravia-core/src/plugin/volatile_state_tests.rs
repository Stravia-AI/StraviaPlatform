use super::volatile_state::{VolatileCell, VolatileStore};
use std::sync::{Arc, Barrier};

fn store() -> VolatileStore {
    VolatileStore::for_installation(&uuid::Uuid::new_v4().to_string())
}

async fn open(
    store: &VolatileStore,
    vendor: &str,
    provider: &str,
    epoch: i64,
    key: &str,
) -> VolatileCell {
    open_fenced(store, vendor, provider, epoch, 1, key).await
}

async fn open_fenced(
    store: &VolatileStore,
    vendor: &str,
    provider: &str,
    epoch: i64,
    fence: i64,
    key: &str,
) -> VolatileCell {
    store
        .read()
        .await
        .open(vendor, provider, epoch, fence, key.to_owned())
        .unwrap()
}

async fn assert_empty(cell: &VolatileCell) {
    let snapshot = cell.load().await.unwrap();
    assert_eq!(snapshot.revision, 0);
    assert_eq!(snapshot.payload, None);
}

async fn assert_cancelled(cell: &VolatileCell) {
    assert_eq!(cell.load().await.unwrap_err().code, "cancelled");
    assert_eq!(
        cell.compare_exchange(0, Some(b"late".to_vec()))
            .await
            .unwrap_err()
            .code,
        "cancelled"
    );
}

#[tokio::test]
async fn installation_vendor_provider_and_epoch_are_isolated_and_reconstruction_shares_state() {
    let id = uuid::Uuid::new_v4().to_string();
    let first = VolatileStore::for_installation(&id);
    let cell = open(&first, "vendor-a", "provider-a", 1, "key").await;
    assert!(
        cell.compare_exchange(0, Some(b"value".to_vec()))
            .await
            .unwrap()
    );

    let reconstructed = VolatileStore::for_installation(&id);
    let shared = open(&reconstructed, "vendor-a", "provider-a", 1, "key").await;
    assert_eq!(
        shared.load().await.unwrap().payload.as_deref(),
        Some(&b"value"[..])
    );
    assert_empty(&open(&first, "vendor-b", "provider-a", 1, "key").await).await;
    assert_empty(&open(&first, "vendor-a", "provider-b", 1, "key").await).await;
    let next_epoch = open(&first, "vendor-a", "provider-a", 2, "key").await;
    assert_empty(&next_epoch).await;
    assert_cancelled(&cell).await;
    assert_cancelled(&shared).await;
    assert_eq!(
        first
            .read()
            .await
            .open("vendor-a", "provider-a", 1, 1, "key".into())
            .err()
            .unwrap()
            .code,
        "cancelled"
    );
    let next_fence = open_fenced(&first, "vendor-a", "provider-a", 1, 2, "key").await;
    assert_empty(&next_fence).await;
    assert_cancelled(&next_epoch).await;
    assert_eq!(
        first
            .read()
            .await
            .open("vendor-a", "provider-a", 2, 1, "key".into())
            .err()
            .unwrap()
            .code,
        "cancelled"
    );
    assert_empty(&open(&store(), "vendor-a", "provider-a", 1, "key").await).await;
}

#[tokio::test]
async fn advancing_generation_clears_old_cookie_jar_and_rejects_stale_open() {
    use stravia_vendor_sdk::net::{CookiePolicy, Field};
    let store = store();
    let policy = CookiePolicy::parse(r#"{"domains":["example.com"],"names":["sid"]}"#).unwrap();
    let url = url::Url::parse("https://example.com/").unwrap();
    let guard = store.read().await;
    let old = guard.cookies("vendor", "provider", 1, 1).unwrap();
    old.absorb(&policy, &url, [b"sid=secret; Path=/".as_slice()])
        .unwrap();
    let mut fields = vec![Field {
        name: "Cookie".into(),
        bytes: vec![],
    }];
    old.apply(&policy, &url, &mut fields);
    assert_eq!(fields[0].bytes, b"sid=secret");
    let fresh = guard.cookies("vendor", "provider", 2, 1).unwrap();
    let mut fields = vec![Field {
        name: "Cookie".into(),
        bytes: vec![],
    }];
    old.apply(&policy, &url, &mut fields);
    assert!(fields.is_empty());
    let mut fields = vec![Field {
        name: "Cookie".into(),
        bytes: vec![],
    }];
    fresh.apply(&policy, &url, &mut fields);
    assert!(fields.is_empty());
    assert_eq!(
        guard
            .cookies("vendor", "provider", 1, 1)
            .err()
            .unwrap()
            .code,
        "cancelled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_exchange_has_one_winner_and_tombstones_prevent_aba() {
    let store = store();
    let cell = open(&store, "vendor", "provider", 1, "key").await;
    let barrier = Arc::new(Barrier::new(2));
    let mut tasks = Vec::new();
    for value in [b"left".to_vec(), b"right".to_vec()] {
        let cell = cell.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait();
            cell.compare_exchange(0, Some(value)).await.unwrap()
        }));
    }
    let left = tasks.remove(0).await.unwrap();
    let right = tasks.remove(0).await.unwrap();
    assert_ne!(left, right);
    let written = cell.load().await.unwrap();
    assert_ne!(written.revision, 0);
    assert!(matches!(
        written.payload.as_deref(),
        Some(b"left") | Some(b"right")
    ));

    assert!(cell.compare_exchange(written.revision, None).await.unwrap());
    let deleted = cell.load().await.unwrap();
    assert_eq!(deleted.payload, None);
    assert_ne!(deleted.revision, 0);
    assert_ne!(deleted.revision, written.revision);
    assert!(
        !cell
            .compare_exchange(0, Some(b"aba".to_vec()))
            .await
            .unwrap()
    );
    assert!(
        !cell
            .compare_exchange(written.revision, Some(b"stale".to_vec()))
            .await
            .unwrap()
    );
    assert!(
        cell.compare_exchange(deleted.revision, Some(b"rewritten".to_vec()))
            .await
            .unwrap()
    );
    assert_eq!(
        cell.load().await.unwrap().payload.as_deref(),
        Some(&b"rewritten"[..])
    );
}

#[tokio::test]
async fn quota_counts_tombstones_and_limits_payload_size_per_scope() {
    let store = store();
    let first = open(&store, "vendor", "provider", 1, "0").await;
    assert_eq!(
        first
            .compare_exchange(0, Some(vec![0; 1_048_577]))
            .await
            .unwrap_err()
            .code,
        "resource-exhausted"
    );
    assert_empty(&first).await;
    assert!(
        first
            .compare_exchange(0, Some(vec![7; 1_048_576]))
            .await
            .unwrap()
    );
    let saved = first.load().await.unwrap();
    assert_eq!(
        saved.payload.as_deref(),
        Some(vec![7; 1_048_576].as_slice())
    );
    assert_eq!(
        first
            .compare_exchange(saved.revision, Some(vec![0; 1_048_577]))
            .await
            .unwrap_err()
            .code,
        "resource-exhausted"
    );
    let unchanged = first.load().await.unwrap();
    assert_eq!(unchanged.revision, saved.revision);
    assert_eq!(unchanged.payload, saved.payload);
    let revision = saved.revision;
    assert!(first.compare_exchange(revision, None).await.unwrap());
    for index in 1..16 {
        let cell = open(&store, "vendor", "provider", 1, &index.to_string()).await;
        assert!(cell.compare_exchange(0, Some(vec![1])).await.unwrap());
    }
    let extra = open(&store, "vendor", "provider", 1, "extra").await;
    assert_eq!(
        extra
            .compare_exchange(0, Some(vec![1]))
            .await
            .unwrap_err()
            .code,
        "resource-exhausted"
    );
    assert_empty(&extra).await;
    let tombstone = first.load().await.unwrap();
    assert!(
        first
            .compare_exchange(tombstone.revision, Some(vec![2]))
            .await
            .unwrap()
    );
    let other = open(&store, "vendor", "other-provider", 1, "extra").await;
    assert!(other.compare_exchange(0, Some(vec![1])).await.unwrap());
}

#[tokio::test]
async fn provider_invalidation_revokes_unwritten_and_rewritten_handles_across_epochs() {
    let store = store();
    let never_written = open(&store, "vendor", "provider", 1, "unwritten").await;
    let rewritten = open(&store, "vendor", "provider", 1, "rewritten").await;
    let unaffected = open(&store, "vendor", "other", 1, "key").await;
    assert!(
        rewritten
            .compare_exchange(0, Some(b"old".to_vec()))
            .await
            .unwrap()
    );
    let revision = rewritten.load().await.unwrap().revision;
    assert!(
        rewritten
            .compare_exchange(revision, Some(b"new".to_vec()))
            .await
            .unwrap()
    );
    assert!(
        unaffected
            .compare_exchange(0, Some(b"keep".to_vec()))
            .await
            .unwrap()
    );
    let other_epoch = open(&store, "vendor", "provider", 2, "key").await;
    assert_cancelled(&never_written).await;
    assert_cancelled(&rewritten).await;
    assert!(
        other_epoch
            .compare_exchange(0, Some(b"epoch".to_vec()))
            .await
            .unwrap()
    );

    store
        .lock()
        .await
        .invalidate_provider("vendor", "provider")
        .await;
    assert_cancelled(&other_epoch).await;
    assert_empty(&open(&store, "vendor", "provider", 2, "rewritten").await).await;
    assert_eq!(
        store
            .read()
            .await
            .open("vendor", "provider", 1, 1, "rewritten".into())
            .err()
            .unwrap()
            .code,
        "cancelled"
    );
    assert_eq!(
        unaffected.load().await.unwrap().payload.as_deref(),
        Some(&b"keep"[..])
    );
}

#[tokio::test]
async fn provider_id_and_vendor_deletions_only_revoke_matching_scopes() {
    let store = store();
    let a = open(&store, "vendor-a", "shared-provider", 1, "key").await;
    let b = open(&store, "vendor-b", "shared-provider", 1, "key").await;
    let c = open(&store, "vendor-a", "other-provider", 1, "key").await;
    let d = open(&store, "vendor-b", "other-provider", 1, "key").await;
    for cell in [&a, &b, &c, &d] {
        assert!(
            cell.compare_exchange(0, Some(b"value".to_vec()))
                .await
                .unwrap()
        );
    }
    store
        .lock()
        .await
        .invalidate_provider_id("shared-provider")
        .await;
    assert_cancelled(&a).await;
    assert_cancelled(&b).await;
    assert_eq!(
        c.load().await.unwrap().payload.as_deref(),
        Some(&b"value"[..])
    );
    assert_eq!(
        d.load().await.unwrap().payload.as_deref(),
        Some(&b"value"[..])
    );
    assert_empty(&open(&store, "vendor-a", "shared-provider", 1, "key").await).await;

    store.lock().await.invalidate_vendor("vendor-a").await;
    assert_cancelled(&c).await;
    assert_eq!(
        d.load().await.unwrap().payload.as_deref(),
        Some(&b"value"[..])
    );
    assert_empty(&open(&store, "vendor-a", "other-provider", 1, "key").await).await;
}

fn provider_record(name: &str) -> crate::db::models::CreateProviderRecord {
    crate::db::models::CreateProviderRecord {
        name: name.into(),
        vendor: Some("volatile-test-vendor".into()),
        protocol: "openai-compatible".into(),
        base_url: "http://127.0.0.1:9".into(),
        preset_key: None,
        channel: Some("default".into()),
        models_source: None,
        static_models: None,
        api_key: String::new(),
        adapter_credentials: "{}".into(),
        vendor_options: "{}".into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
    }
}

fn installed_plugin() -> super::store::InstalledPlugin {
    super::store::InstalledPlugin {
        vendor_id: "volatile-test-vendor".into(),
        version: "test".into(),
        source: "local".into(),
        descriptor: "{}".into(),
        component: bytes::Bytes::new(),
        digest: "test".into(),
        revision: 1,
        data_epoch: 1,
        installed_at: 1,
    }
}

#[tokio::test]
async fn memory_plugin_reset_and_provider_delete_revoke_live_handles() -> anyhow::Result<()> {
    use crate::storage::{DynStorage, MemoryStorage};
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let plugins = storage.vendor_plugins();
    let volatile = store();
    plugins.attach_volatile_store(volatile.clone());
    plugins.install(&installed_plugin(), None, &[]).await?;
    let provider = storage.providers().create(provider_record("reset")).await?;
    let other = storage
        .providers()
        .create(provider_record("unaffected"))
        .await?;
    let provider_fence = plugins
        .open_atomic_provider_fence("volatile-test-vendor", &provider.id, 1)
        .await?;
    let other_fence = plugins
        .open_atomic_provider_fence("volatile-test-vendor", &other.id, 1)
        .await?;
    let unwritten = open_fenced(
        &volatile,
        "volatile-test-vendor",
        &provider.id,
        1,
        provider_fence,
        "unwritten",
    )
    .await;
    let written = open_fenced(
        &volatile,
        "volatile-test-vendor",
        &provider.id,
        1,
        provider_fence,
        "written",
    )
    .await;
    let keep = open_fenced(
        &volatile,
        "volatile-test-vendor",
        &other.id,
        1,
        other_fence,
        "written",
    )
    .await;
    assert!(
        written
            .compare_exchange(0, Some(b"old".to_vec()))
            .await
            .unwrap()
    );
    assert!(
        keep.compare_exchange(0, Some(b"keep".to_vec()))
            .await
            .unwrap()
    );
    let mut replacement = installed_plugin();
    replacement.revision = 2;
    plugins
        .install(
            &replacement,
            Some(1),
            &[super::store::ProviderReset {
                impact: super::store::DataReset {
                    provider_id: provider.id.clone(),
                    private_state: true,
                    ..Default::default()
                },
                expected_vendor: "volatile-test-vendor".into(),
                retained_adapter_credentials: "{}".into(),
                retained_options: "{}".into(),
            }],
        )
        .await?;
    assert_cancelled(&unwritten).await;
    assert_cancelled(&written).await;
    let reset_fence = plugins
        .open_atomic_provider_fence("volatile-test-vendor", &provider.id, 1)
        .await?;
    assert_ne!(reset_fence, provider_fence);
    assert_empty(
        &open_fenced(
            &volatile,
            "volatile-test-vendor",
            &provider.id,
            1,
            reset_fence,
            "written",
        )
        .await,
    )
    .await;
    assert_eq!(
        keep.load().await.unwrap().payload.as_deref(),
        Some(&b"keep"[..])
    );

    let before_delete = open_fenced(
        &volatile,
        "volatile-test-vendor",
        &provider.id,
        1,
        reset_fence,
        "never-written",
    )
    .await;
    storage.providers().delete(&provider.id).await?;
    assert_cancelled(&before_delete).await;
    assert_eq!(
        keep.load().await.unwrap().payload.as_deref(),
        Some(&b"keep"[..])
    );

    let never_written = open_fenced(
        &volatile,
        "volatile-test-vendor",
        &other.id,
        1,
        other_fence,
        "unwritten",
    )
    .await;
    replacement.revision = 3;
    replacement.data_epoch = 2;
    plugins.install(&replacement, Some(2), &[]).await?;
    assert_cancelled(&keep).await;
    assert_cancelled(&never_written).await;
    let new_epoch_fence = plugins
        .open_atomic_provider_fence("volatile-test-vendor", &other.id, 2)
        .await?;
    assert_empty(
        &open_fenced(
            &volatile,
            "volatile-test-vendor",
            &other.id,
            2,
            new_epoch_fence,
            "written",
        )
        .await,
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn sqlite_storage_never_serializes_volatile_payload() -> anyhow::Result<()> {
    use crate::storage::{DynStorage, SqliteStorage};
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool.clone()));
    let plugins = storage.vendor_plugins();
    let volatile = store();
    plugins.attach_volatile_store(volatile.clone());
    plugins.install(&installed_plugin(), None, &[]).await?;
    let provider = storage
        .providers()
        .create(provider_record("sqlite"))
        .await?;
    let fence = plugins
        .open_atomic_provider_fence("volatile-test-vendor", &provider.id, 1)
        .await?;
    let cell = open_fenced(
        &volatile,
        "volatile-test-vendor",
        &provider.id,
        1,
        fence,
        "cell",
    )
    .await;
    let secret = b"volatile-payload-must-never-appear-in-sqlite-8d440874";
    assert!(
        cell.compare_exchange(0, Some(secret.to_vec()))
            .await
            .unwrap()
    );
    assert_eq!(
        cell.load().await.unwrap().payload.as_deref(),
        Some(&secret[..])
    );
    let durable = plugins
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "volatile-test-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: None,
            admission: None,
            lease: None,
            key: "cell",
        })
        .await?;
    assert_eq!(durable.revision, 0);
    assert_eq!(durable.payload, None);
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&pool)
        .await?;
    drop(storage);
    pool.close().await;
    let database = std::fs::read(directory.path().join("db/gateway.db"))?;
    assert!(
        !database
            .windows(secret.len())
            .any(|window| window == secret)
    );
    Ok(())
}
