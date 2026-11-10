use super::atomic_test_fixtures::{install, provider};
use super::*;
use crate::storage::{DynStorage, PostgresStorage, SqliteStorage};

const VENDOR: &str = "journal-test-vendor";
const ROOT_LEASE: &str = "00000000-0000-4000-8000-000000000001";
const A_LEASE: &str = "00000000-0000-4000-8000-000000000002";
const B_LEASE: &str = "00000000-0000-4000-8000-000000000003";
const ROOT_ADMISSION: &str = "00000000-0000-4000-8000-000000000033";
const BOUNDARY_ADMISSION: &str = "00000000-0000-4000-8000-000000000030";
const CHILD_ADMISSION: &str = "00000000-0000-4000-8000-000000000031";
const MEMORY_ADMISSION: &str = "00000000-0000-4000-8000-000000000032";

async fn setup(storage: &DynStorage) -> anyhow::Result<String> {
    install(storage, VENDOR).await?;
    create_provider(storage, "journal provider").await
}

async fn create_provider(storage: &DynStorage, name: &str) -> anyhow::Result<String> {
    let provider = storage.providers().create(provider(name, VENDOR)).await?;
    Ok(provider.id)
}

fn scope(
    provider: &str,
    provider_fence: i64,
    admission: &str,
    lease: &str,
    key: &str,
) -> atomic_journal::JournalScope {
    atomic_journal::JournalScope {
        vendor_id: VENDOR.into(),
        provider_id: provider.into(),
        data_epoch: 1,
        provider_fence,
        principal: "alice".into(),
        admission: admission.into(),
        lease: lease.into(),
        key: key.into(),
        candidate: None,
    }
}

fn candidate(id: u128, admission: &str) -> crate::generation_chain::HistoryBoundaryCandidate {
    crate::generation_chain::HistoryBoundaryCandidate {
        id: uuid::Uuid::from_u128(id),
        window_id: uuid::Uuid::from_u128(42),
        source_admission_id: uuid::Uuid::parse_str(admission).unwrap(),
        source_item_gap: id as u64,
        source_unit_gap: id as u64 + 1,
        source_prefix_hash: [id as u8; 32],
        source_controls_hash: [7; 32],
    }
}

async fn payload_capacity_scenario(
    store: &PluginStore,
    mut cell: atomic_journal::JournalScope,
) -> anyhow::Result<()> {
    cell.key = "capacity".into();
    let head = store.journal_append(&cell, "capacity".into(), None).await?;
    let payload = vec![7; 1_048_576];
    assert!(
        store
            .journal_compare_exchange(&cell, 0, Some(&payload), Some(&head))
            .await?
    );
    let saved = store.journal_load(&cell).await?;
    assert_eq!(saved.payload.as_deref(), Some(payload.as_slice()));
    assert_eq!(saved.journal_head.as_deref(), Some(head.as_str()));
    assert!(matches!(
        store
            .journal_compare_exchange(&cell, saved.revision, Some(&vec![0; 1_048_577]), None)
            .await,
        Err(PluginStorageError::Limit)
    ));
    let unchanged = store.journal_load(&cell).await?;
    assert_eq!(unchanged.revision, saved.revision);
    assert_eq!(unchanged.payload, saved.payload);
    assert_eq!(unchanged.journal_head, saved.journal_head);
    Ok(())
}

async fn scenario(db: Database) -> anyhow::Result<()> {
    let storage = db.storage();
    let provider = setup(&storage).await?;
    boundary_scenario(db.clone(), storage.clone(), &provider).await?;
    let store = storage.vendor_plugins();
    let fence = store
        .open_atomic_provider_fence(VENDOR, &provider, 1)
        .await?;
    store
        .fork_atomic_state("alice", ROOT_ADMISSION, None, ROOT_LEASE)
        .await?;
    let root = scope(&provider, fence, ROOT_ADMISSION, ROOT_LEASE, "journal");
    payload_capacity_scenario(store, root.clone()).await?;
    let initial = store.journal_load(&root).await?;
    assert_eq!(initial.revision, 0);
    assert_eq!(initial.journal_head, None);
    let first = store.journal_append(&root, "first".into(), None).await?;
    let second = store
        .journal_append(&root, "second".into(), Some(&first))
        .await?;
    assert!(
        store
            .journal_compare_exchange(
                &root,
                initial.revision,
                Some(b"at checkpoint"),
                Some(&second)
            )
            .await?
    );
    store
        .checkpoint_atomic_state("alice", ROOT_ADMISSION, "checkpoint", ROOT_LEASE)
        .await?;
    let saved = store.journal_load(&root).await?;
    let later = store
        .journal_append(&root, "later".into(), Some(&second))
        .await?;
    assert!(
        store
            .journal_compare_exchange(
                &root,
                saved.revision,
                Some(b"after checkpoint"),
                Some(&later)
            )
            .await?
    );
    store
        .fork_atomic_state("alice", "branch-a", Some("checkpoint"), A_LEASE)
        .await?;
    store
        .fork_atomic_state("alice", "branch-b", Some("checkpoint"), B_LEASE)
        .await?;
    let a = scope(&provider, fence, "branch-a", A_LEASE, "journal");
    let b = scope(&provider, fence, "branch-b", B_LEASE, "journal");
    let a_before = store.journal_load(&a).await?;
    let b_before = store.journal_load(&b).await?;
    assert_eq!(a_before.payload.as_deref(), Some(&b"at checkpoint"[..]));
    assert_eq!(b_before.payload, a_before.payload);
    assert_eq!(a_before.journal_head.as_deref(), Some(second.as_str()));
    assert_eq!(b_before.journal_head, a_before.journal_head);
    assert_eq!(
        store.journal_read(&a, &second).await?,
        ("second".into(), Some(first.clone()))
    );
    assert_eq!(
        store.journal_read(&a, &first).await?,
        ("first".into(), None)
    );
    let branch = store
        .journal_append(&a, "branch".into(), Some(&second))
        .await?;
    assert!(
        store
            .journal_compare_exchange(&a, a_before.revision, Some(b"branch a"), Some(&branch))
            .await?
    );
    assert!(matches!(
        store.journal_read(&b, &branch).await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .journal_compare_exchange(&b, b_before.revision, None, Some(&branch))
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert_eq!(
        store.journal_load(&b).await?.journal_head.as_deref(),
        Some(second.as_str())
    );
    assert_eq!(
        store.journal_load(&root).await?.journal_head.as_deref(),
        Some(later.as_str())
    );

    let race = scope(&provider, fence, ROOT_ADMISSION, ROOT_LEASE, "race");
    let left = store.journal_append(&race, "left".into(), None).await?;
    let right = store.journal_append(&race, "right".into(), None).await?;
    let (one, two) = tokio::join!(
        store.journal_compare_exchange(&race, 0, Some(b"left"), Some(&left)),
        store.journal_compare_exchange(&race, 0, Some(b"right"), Some(&right)),
    );
    assert_ne!(one?, two?, "exactly one writer must publish");
    let winner = store.journal_load(&race).await?;
    if winner.journal_head.as_deref() == Some(left.as_str()) {
        assert_eq!(winner.payload.as_deref(), Some(&b"left"[..]));
    } else {
        assert_eq!(winner.journal_head.as_deref(), Some(right.as_str()));
        assert_eq!(winner.payload.as_deref(), Some(&b"right"[..]));
    }

    // Reconnect with a new pool and store, as on process restart.
    drop(storage);
    let db = db.reconnect().await?;
    let reopened = db.storage();
    let store = reopened.vendor_plugins();
    let after = store.journal_load(&root).await?;
    assert_eq!(after.journal_head.as_deref(), Some(later.as_str()));
    assert_eq!(after.payload.as_deref(), Some(&b"after checkpoint"[..]));
    assert_eq!(
        store.journal_read(&root, &later).await?,
        ("later".into(), Some(second.clone()))
    );
    store
        .fork_atomic_state(
            "alice",
            "after-reopen",
            Some("checkpoint"),
            "00000000-0000-4000-8000-000000000004",
        )
        .await?;
    let reopened_branch = scope(
        &provider,
        fence,
        "after-reopen",
        "00000000-0000-4000-8000-000000000004",
        "journal",
    );
    assert_eq!(
        store
            .journal_load(&reopened_branch)
            .await?
            .journal_head
            .as_deref(),
        Some(second.as_str())
    );

    const ORPHAN_LEASE: &str = "00000000-0000-4000-8000-000000000008";
    store
        .fork_atomic_state("alice", "orphan", None, ORPHAN_LEASE)
        .await?;
    let epoch = reopened.prompt_admissions().history_epoch().await?;
    store
        .acquire_atomic_state_lease_at_epoch("alice", "orphan", A_LEASE, epoch)
        .await?;
    let orphan = scope(&provider, fence, "orphan", ORPHAN_LEASE, "journal");
    store
        .journal_append(&orphan, "orphan-only body".into(), None)
        .await?;
    let before_drop = db.content_count().await?;
    db.expire_admission("orphan").await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, reopened.prompt_admissions().as_ref())
        .await?;
    while store.sweep_atomic_journal().await? > 0 {}
    db.sweep_content().await?;
    assert_eq!(db.content_count().await?, before_drop - 1);
    assert!(
        store
            .acquire_atomic_state_lease("alice", "orphan", ORPHAN_LEASE)
            .await
            .is_err()
    );
    assert!(
        store
            .acquire_atomic_state_lease_at_epoch("alice", "orphan", A_LEASE, epoch)
            .await
            .is_err()
    );
    assert_eq!(store.journal_read(&root, &later).await?.0, "later");

    // Staging is charged even before publication. A fresh lease can stage a
    // byte, but neither an oversized chain nor an oversized aggregate may publish.
    const LIMIT: usize = 64 * 1024 * 1024;
    const QUOTA_LEASE: &str = "00000000-0000-4000-8000-000000000005";
    const CHILD_LEASE: &str = "00000000-0000-4000-8000-000000000006";
    store
        .fork_atomic_state("alice", "quota", None, QUOTA_LEASE)
        .await?;
    let quota = scope(&provider, fence, "quota", QUOTA_LEASE, "quota");
    let big = store
        .journal_append(&quota, "x".repeat(LIMIT), None)
        .await?;
    assert!(matches!(
        store.journal_append(&quota, "y".into(), None).await,
        Err(PluginStorageError::Limit)
    ));
    assert!(
        store
            .journal_compare_exchange(&quota, 0, None, Some(&big))
            .await?
    );
    store
        .checkpoint_atomic_state("alice", "quota", "quota-checkpoint", QUOTA_LEASE)
        .await?;
    store
        .fork_atomic_state(
            "alice",
            "quota-child",
            Some("quota-checkpoint"),
            CHILD_LEASE,
        )
        .await?;
    let child = scope(&provider, fence, "quota-child", CHILD_LEASE, "quota");
    let child_snapshot = store.journal_load(&child).await?;
    assert_eq!(child_snapshot.journal_head.as_deref(), Some(big.as_str()));
    assert!(matches!(
        store.journal_append(&child, "y".into(), Some(&big)).await,
        Err(PluginStorageError::Limit)
    ));
    let other_cell = scope(&provider, fence, "quota-child", CHILD_LEASE, "other");
    let extra = store.journal_append(&other_cell, "y".into(), None).await?;
    assert!(matches!(
        store
            .journal_compare_exchange(&other_cell, 0, None, Some(&extra))
            .await,
        Err(PluginStorageError::Limit)
    ));
    assert_eq!(
        store.journal_load(&child).await?.journal_head.as_deref(),
        Some(big.as_str())
    );

    db.expire_leases().await?;
    assert!(matches!(
        store.journal_read(&root, &later).await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store.journal_append(&root, "stale".into(), None).await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .journal_compare_exchange(&root, after.revision, None, Some(&later))
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    const NEW_LEASE: &str = "00000000-0000-4000-8000-000000000007";
    store
        .acquire_atomic_state_lease("alice", ROOT_ADMISSION, NEW_LEASE)
        .await?;
    let current = scope(&provider, fence, ROOT_ADMISSION, NEW_LEASE, "journal");
    assert_eq!(
        store.journal_load(&current).await?.journal_head.as_deref(),
        Some(later.as_str())
    );
    let reset_candidate = candidate(99, ROOT_ADMISSION);
    let mut before_reset = current.clone();
    before_reset.candidate = Some(reset_candidate.clone());
    let next = store
        .journal_append(&before_reset, "candidate before reset".into(), Some(&later))
        .await?;
    let revision = store.journal_load(&before_reset).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&before_reset, revision, None, Some(&next))
            .await?
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, &provider, "alice", ROOT_ADMISSION, NEW_LEASE)
            .await?,
        vec![reset_candidate.clone()]
    );
    let facts_before_reset = db.boundary_count().await?;

    let mut replacement = store
        .list()
        .await?
        .into_iter()
        .find(|p| p.vendor_id == VENDOR)
        .unwrap();
    replacement.revision = 2;
    store
        .install(
            &replacement,
            Some(1),
            &[ProviderReset {
                impact: DataReset {
                    provider_id: provider.clone(),
                    private_state: true,
                    ..DataReset::default()
                },
                expected_vendor: VENDOR.into(),
                retained_adapter_credentials: "{}".into(),
                retained_options: "{}".into(),
            }],
        )
        .await?;
    assert!(matches!(
        store.journal_load(&current).await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store.journal_read(&current, &later).await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .journal_compare_exchange(&current, after.revision, None, Some(&later))
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    let new_fence = store
        .open_atomic_provider_fence(VENDOR, &provider, 1)
        .await?;
    assert_ne!(new_fence, fence);
    assert!(
        store
            .history_boundary_candidates(VENDOR, &provider, "alice", ROOT_ADMISSION, NEW_LEASE)
            .await?
            .is_empty()
    );
    let mut fresh = scope(&provider, new_fence, ROOT_ADMISSION, NEW_LEASE, "journal");
    fresh.candidate = Some(reset_candidate.clone());
    let cleared = store.journal_load(&fresh).await?;
    assert_eq!(cleared.revision, 0);
    assert_eq!(cleared.payload, None);
    assert_eq!(cleared.journal_head, None);
    let fresh_head = store
        .journal_append(&fresh, "after reset".into(), None)
        .await?;
    assert!(
        store
            .journal_compare_exchange(&fresh, 0, None, Some(&fresh_head))
            .await?
    );
    assert_eq!(
        store.journal_read(&fresh, &fresh_head).await?.0,
        "after reset"
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, &provider, "alice", ROOT_ADMISSION, NEW_LEASE)
            .await?,
        vec![reset_candidate]
    );
    assert_eq!(db.boundary_count().await?, facts_before_reset + 1);

    assert!(
        db.content_count().await? > 0,
        "journal bodies were persisted"
    );
    db.expire_leases().await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, reopened.prompt_admissions().as_ref())
        .await?;
    while store.sweep_atomic_journal().await? > 0 {}
    db.sweep_content().await?;
    assert_eq!(
        db.content_count().await?,
        0,
        "unreferenced journal content must eventually be reclaimed"
    );
    Ok(())
}

async fn boundary_scenario(
    db: Database,
    storage: DynStorage,
    provider: &str,
) -> anyhow::Result<()> {
    const LEASE: &str = "00000000-0000-4000-8000-000000000020";
    const CHILD: &str = "00000000-0000-4000-8000-000000000021";
    let store = storage.vendor_plugins();
    let fence = store
        .open_atomic_provider_fence(VENDOR, provider, 1)
        .await?;
    let other_provider = create_provider(&storage, "other journal owner").await?;
    let other_fence = store
        .open_atomic_provider_fence(VENDOR, &other_provider, 1)
        .await?;
    store
        .fork_atomic_state("alice", BOUNDARY_ADMISSION, None, LEASE)
        .await?;
    let a = candidate(1, BOUNDARY_ADMISSION);
    let b = candidate(2, BOUNDARY_ADMISSION);
    let c = candidate(3, BOUNDARY_ADMISSION);
    // The child's own candidate may describe a source in its parent admission.
    let child_candidate = candidate(4, BOUNDARY_ADMISSION);
    let mut cell = scope(
        provider,
        fence,
        BOUNDARY_ADMISSION,
        LEASE,
        BOUNDARY_ADMISSION,
    );
    cell.candidate = Some(a.clone());
    let first = store
        .journal_append(&cell, "boundary first".into(), None)
        .await?;
    assert!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?
            .is_empty()
    );
    // A successful payload-only update and a failed head update publish nothing.
    assert!(
        store
            .journal_compare_exchange(&cell, 0, Some(b"payload"), None)
            .await?
    );
    assert!(
        !store
            .journal_compare_exchange(&cell, 0, None, Some(&first))
            .await?
    );
    assert!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?
            .is_empty()
    );
    let revision = store.journal_load(&cell).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&cell, revision, None, Some(&first))
            .await?
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?,
        vec![a.clone()]
    );
    cell.candidate = Some(b.clone());
    let revision = store.journal_load(&cell).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&cell, revision, Some(b"changed"), Some(&first))
            .await?
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?,
        vec![a.clone()]
    );
    let second = store
        .journal_append(&cell, "boundary second".into(), Some(&first))
        .await?;
    let revision = store.journal_load(&cell).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&cell, revision, None, Some(&second))
            .await?
    );
    cell.candidate = Some(a.clone());
    let third = store
        .journal_append(&cell, "boundary third".into(), Some(&second))
        .await?;
    let revision = store.journal_load(&cell).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&cell, revision, None, Some(&third))
            .await?
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?,
        vec![a.clone(), b.clone()]
    );

    let mut other = scope(
        &other_provider,
        other_fence,
        BOUNDARY_ADMISSION,
        LEASE,
        BOUNDARY_ADMISSION,
    );
    other.candidate = Some(a.clone());
    let other_head = store
        .journal_append(&other, "other owner".into(), None)
        .await?;
    assert!(
        store
            .journal_compare_exchange(&other, 0, None, Some(&other_head))
            .await?
    );
    assert_eq!(
        store
            .history_boundary_candidates(
                VENDOR,
                &other_provider,
                "alice",
                BOUNDARY_ADMISSION,
                LEASE
            )
            .await?,
        vec![a.clone()]
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?,
        vec![a.clone(), b.clone()]
    );
    assert!(matches!(
        store
            .history_boundary_candidates(VENDOR, provider, "bob", BOUNDARY_ADMISSION, LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    store
        .checkpoint_atomic_state("alice", BOUNDARY_ADMISSION, "boundary-checkpoint", LEASE)
        .await?;
    store
        .fork_atomic_state("alice", CHILD_ADMISSION, Some("boundary-checkpoint"), CHILD)
        .await?;
    cell.candidate = Some(c.clone());
    let fourth = store
        .journal_append(&cell, "boundary fourth".into(), Some(&third))
        .await?;
    let revision = store.journal_load(&cell).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&cell, revision, None, Some(&fourth))
            .await?
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?,
        vec![a.clone(), b.clone(), c.clone()]
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", CHILD_ADMISSION, CHILD)
            .await?,
        vec![a.clone(), b.clone()]
    );
    let mut child_cell = scope(provider, fence, CHILD_ADMISSION, CHILD, BOUNDARY_ADMISSION);
    child_cell.candidate = Some(child_candidate.clone());
    let child_snapshot = store.journal_load(&child_cell).await?;
    let child_head = store
        .journal_append(
            &child_cell,
            "child boundary".into(),
            child_snapshot.journal_head.as_deref(),
        )
        .await?;
    assert!(
        store
            .journal_compare_exchange(
                &child_cell,
                child_snapshot.revision,
                None,
                Some(&child_head)
            )
            .await?
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", CHILD_ADMISSION, CHILD)
            .await?,
        vec![a.clone(), b.clone(), child_candidate.clone()]
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", BOUNDARY_ADMISSION, LEASE)
            .await?,
        vec![a.clone(), b.clone(), c.clone()]
    );
    assert!(matches!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", CHILD_ADMISSION, LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    let mut wrong_owner = child_cell.clone();
    wrong_owner.lease = LEASE.into();
    assert!(matches!(
        store
            .journal_compare_exchange(
                &wrong_owner,
                child_snapshot.revision,
                None,
                Some(&child_head)
            )
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", CHILD_ADMISSION, CHILD)
            .await?,
        vec![a.clone(), b.clone(), child_candidate.clone()]
    );

    // Reopen against the same durable database, then collect the parent while
    // the child lease retains its inherited checkpoint history.
    let reopened_db = db.reconnect().await?;
    let reopened = reopened_db.storage();
    let store = reopened.vendor_plugins();
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", CHILD_ADMISSION, CHILD)
            .await?,
        vec![a.clone(), b.clone(), child_candidate.clone()]
    );
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, &other_provider, "alice", CHILD_ADMISSION, CHILD)
            .await?,
        vec![a.clone()]
    );
    reopened_db.expire_admission(BOUNDARY_ADMISSION).await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, reopened.prompt_admissions().as_ref())
        .await?;
    while store.sweep_atomic_journal().await? > 0 {}
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, provider, "alice", CHILD_ADMISSION, CHILD)
            .await?,
        vec![a, b, child_candidate]
    );
    assert!(reopened_db.boundary_count().await? > 0);
    reopened_db.expire_admission(CHILD_ADMISSION).await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, reopened.prompt_admissions().as_ref())
        .await?;
    while store.sweep_atomic_journal().await? > 0 {}
    reopened_db.sweep_content().await?;
    assert_eq!(reopened_db.boundary_count().await?, 0);
    Ok(())
}

#[derive(Clone)]
enum Database {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

impl Database {
    async fn reconnect(&self) -> anyhow::Result<Self> {
        Ok(match self {
            Self::Sqlite(pool) => Self::Sqlite(
                sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(4)
                    .connect_with(pool.connect_options().as_ref().clone())
                    .await?,
            ),
            Self::Postgres(pool) => Self::Postgres(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(8)
                    .connect_with(pool.connect_options().as_ref().clone())
                    .await?,
            ),
        })
    }

    async fn expire_admission(&self, admission: &str) -> anyhow::Result<()> {
        match self {
            Self::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE vendor_atomic_leases SET expires_at_ms=0 WHERE admission_id=$1",
                )
                .bind(admission)
                .execute(pool)
                .await?;
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    "UPDATE vendor_atomic_leases SET expires_at_ms=0 WHERE admission_id=$1",
                )
                .bind(admission)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    async fn expire_leases(&self) -> anyhow::Result<()> {
        match self {
            Self::Sqlite(pool) => {
                sqlx::query("UPDATE vendor_atomic_leases SET expires_at_ms=0")
                    .execute(pool)
                    .await?;
            }
            Self::Postgres(pool) => {
                sqlx::query("UPDATE vendor_atomic_leases SET expires_at_ms=0")
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }

    async fn boundary_count(&self) -> anyhow::Result<i64> {
        Ok(match self {
            Self::Sqlite(pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM vendor_atomic_boundary_facts")
                    .fetch_one(pool)
                    .await?
            }
            Self::Postgres(pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM vendor_atomic_boundary_facts")
                    .fetch_one(pool)
                    .await?
            }
        })
    }

    async fn content_count(&self) -> anyhow::Result<i64> {
        Ok(match self {
            Self::Sqlite(pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM contents")
                    .fetch_one(pool)
                    .await?
            }
            Self::Postgres(pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM contents")
                    .fetch_one(pool)
                    .await?
            }
        })
    }

    async fn sweep_content(&self) -> anyhow::Result<()> {
        match self {
            Self::Sqlite(pool) => {
                crate::content_store::sweep_sqlite(pool).await?;
            }
            Self::Postgres(pool) => {
                crate::content_store::sweep_postgres(pool).await?;
            }
        }
        Ok(())
    }

    fn storage(&self) -> DynStorage {
        match self {
            Self::Sqlite(pool) => Arc::new(SqliteStorage::from_pool(pool.clone())),
            Self::Postgres(pool) => Arc::new(PostgresStorage::from_pool(pool.clone())),
        }
    }
}

#[tokio::test]
async fn memory_boundary_candidates_follow_checkpoint_lineage() -> anyhow::Result<()> {
    let storage: DynStorage = Arc::new(crate::storage::MemoryStorage::new(vec![], vec![], vec![]));
    let provider = setup(&storage).await?;
    let store = storage.vendor_plugins();
    let fence = store
        .open_atomic_provider_fence(VENDOR, &provider, 1)
        .await?;
    store
        .fork_atomic_state("alice", MEMORY_ADMISSION, None, ROOT_LEASE)
        .await?;
    let mut cell = scope(&provider, fence, MEMORY_ADMISSION, ROOT_LEASE, "journal");
    payload_capacity_scenario(store, cell.clone()).await?;
    let a = candidate(11, MEMORY_ADMISSION);
    let b = candidate(12, MEMORY_ADMISSION);
    cell.candidate = Some(a.clone());
    let first = store.journal_append(&cell, "first".into(), None).await?;
    assert!(
        store
            .journal_compare_exchange(&cell, 0, None, Some(&first))
            .await?
    );
    cell.candidate = Some(b.clone());
    let second = store
        .journal_append(&cell, "second".into(), Some(&first))
        .await?;
    let revision = store.journal_load(&cell).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&cell, revision, None, Some(&second))
            .await?
    );
    cell.candidate = Some(a.clone());
    let third = store
        .journal_append(&cell, "third".into(), Some(&second))
        .await?;
    let revision = store.journal_load(&cell).await?.revision;
    assert!(
        store
            .journal_compare_exchange(&cell, revision, None, Some(&third))
            .await?
    );
    store
        .checkpoint_atomic_state("alice", MEMORY_ADMISSION, "memory-checkpoint", ROOT_LEASE)
        .await?;
    store
        .fork_atomic_state("alice", "memory-child", Some("memory-checkpoint"), A_LEASE)
        .await?;
    assert_eq!(
        store
            .history_boundary_candidates(VENDOR, &provider, "alice", "memory-child", A_LEASE)
            .await?,
        vec![a, b]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_journal_persistence() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let result = scenario(Database::Sqlite(pool.clone())).await;
    pool.close().await;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_journal_persistence() -> anyhow::Result<()> {
    let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")?;
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("journal_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await?;
    let task_pool = pool.clone();
    let result = tokio::spawn(async move {
        crate::migrations::migrate_postgres(&task_pool, None).await?;
        scenario(Database::Postgres(task_pool)).await
    })
    .await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    match result {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(error.into()),
    }
}
