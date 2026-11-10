use super::atomic_test_fixtures::{install, provider};
use super::*;
use crate::storage::{DynStorage, MemoryStorage, SqliteStorage};

const ROOT_LEASE: &str = "00000000-0000-4000-8000-000000000001";
const BOB_LEASE: &str = "00000000-0000-4000-8000-000000000002";
const A_LEASE: &str = "00000000-0000-4000-8000-000000000003";
const B_LEASE: &str = "00000000-0000-4000-8000-000000000004";
const REOPEN_LEASE: &str = "00000000-0000-4000-8000-000000000005";

async fn payload_capacity_scenario(storage: &DynStorage, provider_id: &str) -> anyhow::Result<()> {
    let store = storage.vendor_plugins();
    store
        .fork_atomic_state("capacity", "capacity", None, ROOT_LEASE)
        .await?;
    for conversation in [false, true] {
        let key = crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id,
            data_epoch: 1,
            principal: conversation.then_some("capacity"),
            admission: conversation.then_some("capacity"),
            lease: conversation.then_some(ROOT_LEASE),
            key: "capacity",
        };
        let payload = vec![7; 1_048_576];
        assert!(
            store
                .compare_exchange_atomic_state(&key, 0, Some(&payload))
                .await?
        );
        let saved = store.load_atomic_state(&key).await?;
        assert_eq!(saved.payload.as_deref(), Some(payload.as_slice()));
        assert!(matches!(
            store
                .compare_exchange_atomic_state(&key, saved.revision, Some(&vec![0; 1_048_577]))
                .await,
            Err(PluginStorageError::Limit)
        ));
        let unchanged = store.load_atomic_state(&key).await?;
        assert_eq!(unchanged.revision, saved.revision);
        assert_eq!(unchanged.payload, saved.payload);
    }
    Ok(())
}

async fn scenario(storage: DynStorage) -> anyhow::Result<()> {
    install(&storage, "test-vendor").await?;
    install(&storage, "other-vendor").await?;
    let a = storage
        .providers()
        .create(provider("provider a", "test-vendor"))
        .await?;
    let b = storage
        .providers()
        .create(provider("provider b", "test-vendor"))
        .await?;
    let other = storage
        .providers()
        .create(provider("other", "other-vendor"))
        .await?;
    let store = storage.vendor_plugins();
    payload_capacity_scenario(&storage, &a.id).await?;

    let empty = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &a.id,
            data_epoch: 1,
            principal: None,
            admission: None,
            lease: None,
            key: "cell",
        })
        .await?;
    assert_eq!(empty.revision, 0);
    assert_eq!(empty.payload, None);
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                0,
                Some(b"first"),
            )
            .await?
    );
    let first = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &a.id,
            data_epoch: 1,
            principal: None,
            admission: None,
            lease: None,
            key: "cell",
        })
        .await?;
    assert_ne!(first.revision, 0);
    assert_eq!(first.payload.as_deref(), Some(&b"first"[..]));
    assert!(
        !store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                0,
                Some(b"stale"),
            )
            .await?
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"first"[..])
    );
    let race_key = crate::plugin::store::AtomicStateKey {
        vendor_id: "test-vendor",
        provider_id: &b.id,
        data_epoch: 1,
        principal: None,
        admission: None,
        lease: None,
        key: "race",
    };
    let (left, right) = tokio::join!(
        store.compare_exchange_atomic_state(&race_key, 0, Some(b"left")),
        store.compare_exchange_atomic_state(&race_key, 0, Some(b"right")),
    );
    assert_ne!(
        left?, right?,
        "exactly one concurrent compare-exchange must win"
    );
    assert!(matches!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: &"x".repeat(129),
            },)
            .await,
        Err(PluginStorageError::Limit)
    ));
    assert!(matches!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "oversized",
                },
                0,
                Some(&vec![0; 1_048_577]),
            )
            .await,
        Err(PluginStorageError::Limit)
    ));

    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &b.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: "cell",
            },)
            .await?
            .revision,
        0
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "other-vendor",
                provider_id: &other.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: "cell",
            },)
            .await?
            .revision,
        0
    );
    assert!(matches!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "other-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: "cell",
            },)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 2,
                principal: None,
                admission: None,
                lease: None,
                key: "cell",
            },)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));

    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                first.revision,
                None,
            )
            .await?
    );
    let deleted = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &a.id,
            data_epoch: 1,
            principal: None,
            admission: None,
            lease: None,
            key: "cell",
        })
        .await?;
    assert!(deleted.revision != 0 && deleted.revision != first.revision);
    assert_eq!(deleted.payload, None);
    assert!(
        !store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                first.revision,
                Some(b"resurrected"),
            )
            .await?
    );
    assert!(
        !store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                0,
                Some(b"aba"),
            )
            .await?
    );
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                deleted.revision,
                Some(b"new"),
            )
            .await?
    );

    store
        .fork_atomic_state("alice", "root", None, ROOT_LEASE)
        .await?;
    store
        .fork_atomic_state("bob", "root", None, BOB_LEASE)
        .await?;
    store
        .renew_atomic_state_lease("alice", "root", ROOT_LEASE)
        .await?;
    assert!(matches!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("root"),
                lease: Some(BOB_LEASE),
                key: "cell",
            },)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .checkpoint_atomic_state("alice", "root", "invalid-checkpoint", BOB_LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"checkpoint"),
            )
            .await?
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: Some("bob"),
                admission: Some("root"),
                lease: Some(BOB_LEASE),
                key: "cell",
            },)
            .await?
            .revision,
        0
    );
    store
        .checkpoint_atomic_state("alice", "root", "checkpoint-1", ROOT_LEASE)
        .await?;
    let root = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &a.id,
            data_epoch: 1,
            principal: Some("alice"),
            admission: Some("root"),
            lease: Some(ROOT_LEASE),
            key: "cell",
        })
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                root.revision,
                Some(b"later"),
            )
            .await?
    );
    store
        .fork_atomic_state("alice", "branch-a", Some("checkpoint-1"), A_LEASE)
        .await?;
    store
        .fork_atomic_state("alice", "branch-b", Some("checkpoint-1"), B_LEASE)
        .await?;
    let branch = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &a.id,
            data_epoch: 1,
            principal: Some("alice"),
            admission: Some("branch-a"),
            lease: Some(A_LEASE),
            key: "cell",
        })
        .await?;
    assert_eq!(branch.payload.as_deref(), Some(&b"checkpoint"[..]));
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("branch-a"),
                    lease: Some(A_LEASE),
                    key: "cell",
                },
                branch.revision,
                Some(b"branch-a"),
            )
            .await?
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("branch-b"),
                lease: Some(B_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"checkpoint"[..])
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("root"),
                lease: Some(ROOT_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"later"[..])
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &b.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("branch-a"),
                lease: Some(A_LEASE),
                key: "cell",
            },)
            .await?
            .revision,
        0
    );

    storage.providers().delete(&a.id).await?;
    assert!(matches!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &a.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: "cell",
            },)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                deleted.revision,
                Some(b"late"),
            )
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    Ok(())
}

async fn expire_memory_lease(
    store: &PluginStore,
    pool: &sqlx::SqlitePool,
    principal: &str,
    admission: &str,
    lease: &str,
) -> anyhow::Result<()> {
    let PluginStoreBackend::Memory(memory) = &store.backend else {
        unreachable!()
    };
    let key = (principal.to_owned(), admission.to_owned(), lease.to_owned());
    *memory
        .data
        .write()
        .await
        .atomic_leases
        .get_mut(&key)
        .expect("active lease") = 0;
    sqlx::query("UPDATE vendor_atomic_leases SET expires_at_ms=0 WHERE principal=$1 AND admission_id=$2 AND lease_id=$3")
        .bind(principal).bind(admission).bind(lease).execute(pool).await?;
    Ok(())
}

#[tokio::test]
async fn memory_checkpoint_survives_turn_and_compaction_references() -> anyhow::Result<()> {
    use crate::compaction::{Compaction, CompactionRegistration, CompactionTarget};
    use stravia_runtime_contract::{
        Principal,
        turn_chain::{TurnChainStore, TurnCommit, TurnNodeId, TurnNodeKind},
    };

    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    store
        .attach_atomic_history_pool(pool.clone(), storage.prompt_admissions().as_ref())
        .await?;
    install(&storage, "test-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("retained", "test-vendor"))
        .await?;
    let owner = Principal::new("memory-retention");
    let principal = owner.continuation_key();
    let checkpoint = uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000010")?;
    let checkpoint_id = checkpoint.to_string();
    store
        .fork_atomic_state(&principal, "root", None, ROOT_LEASE)
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "provider",
                },
                0,
                Some(b"provider-state"),
            )
            .await?
    );
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some(&principal),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"immutable"),
            )
            .await?
    );
    store
        .checkpoint_atomic_state(&principal, "root", &checkpoint_id, ROOT_LEASE)
        .await?;
    let admissions = storage.prompt_admissions();
    store
        .sweep_conversation_atomic_state(i64::MAX, admissions.as_ref())
        .await?;

    let turns = crate::turn_chain::SqlTurnChainStore::sqlite(
        pool.clone(),
        crate::history_retention::HistoryRetention::for_tests(),
    );
    turns
        .commit(TurnCommit {
            id: TurnNodeId::response(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: 1,
            payload: serde_json::json!({"message":"retained"}),
            marker_references: Vec::new(),
            redaction_references: Vec::new(),
            compaction_record_ids: Vec::new(),
            atomic_checkpoint_id: Some(checkpoint_id.clone()),
            reusable_prefix: None,
        })
        .await?;
    let state = stravia_protocol_codec::codec::open_responses::decoder::decode_input_item(
        &serde_json::json!({"type":"compaction", "encrypted_content":"retained-state"}),
    )?
    .expect("compaction item");
    let compaction = Compaction::sqlite(pool.clone());
    compaction
        .register(
            &owner,
            CompactionRegistration {
                logical_turn: None,
                user_prompt_metadata: None,
                source_history: None,
                source_window: None,
                atomic_checkpoint_id: Some(checkpoint),
                source_generation_id: None,
                source_record_ids: Vec::new(),
                operation_id: "retention-test".into(),
                target: CompactionTarget {
                    target_key: "target".into(),
                    namespace: "namespace".into(),
                    model: "model".into(),
                    protocol: "open-responses/responses/2026-04-24".into(),
                },
                window: vec![state.clone()],
                state_items: vec![state],
            },
        )
        .await?;
    expire_memory_lease(store, &pool, &principal, "root", ROOT_LEASE).await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, admissions.as_ref())
        .await?;
    store
        .fork_atomic_state(&principal, "probe", Some(&checkpoint_id), A_LEASE)
        .await?;
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some(&principal),
                admission: Some("probe"),
                lease: Some(A_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"immutable"[..])
    );
    expire_memory_lease(store, &pool, &principal, "probe", A_LEASE).await?;

    let future = chrono::Utc::now().timestamp_millis() + 30 * 24 * 60 * 60 * 1000;
    turns.sweep_history(future).await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, admissions.as_ref())
        .await?;
    store
        .fork_atomic_state(&principal, "probe-2", Some(&checkpoint_id), B_LEASE)
        .await?;
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some(&principal),
                admission: Some("probe-2"),
                lease: Some(B_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"immutable"[..])
    );
    expire_memory_lease(store, &pool, &principal, "probe-2", B_LEASE).await?;
    compaction
        .sweep(future, std::time::Duration::from_secs(1))
        .await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, admissions.as_ref())
        .await?;
    assert!(matches!(
        store
            .fork_atomic_state(&principal, "expired", Some(&checkpoint_id), REOPEN_LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: "provider",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"provider-state"[..])
    );
    Ok(())
}

#[tokio::test]
async fn memory_failed_admission_retains_state_until_pruned() -> anyhow::Result<()> {
    use crate::storage::PromptAdmission;
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    store
        .attach_atomic_history_pool(pool.clone(), storage.prompt_admissions().as_ref())
        .await?;
    install(&storage, "test-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("failed", "test-vendor"))
        .await?;
    let admissions = storage.prompt_admissions();
    admissions
        .insert(&PromptAdmission {
            id: "failed".into(),
            principal: "alice".into(),
            parent_id: None,
            atomic_lineage_created: true,
            ordinal: 1,
            user_turn_index: Some(1),
            dialogue_prompt_index: Some(1),
            context_window_id: None,
            context_window_index: None,
            created_at_ms: 1,
            conversation_id: "conversation".into(),
            conversation_created_at_ms: 1,
            logical_turn_id: "turn".into(),
            logical_turn_created_at_ms: 1,
            user_prompt_id: Some("turn".into()),
            user_prompt_created_at_ms: Some(1),
            history_count: 1,
            prefix_digest: "prefix".into(),
            controls_digest: "controls".into(),
        })
        .await?;
    store
        .fork_atomic_state("alice", "failed", None, ROOT_LEASE)
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("failed"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"failed-state"),
            )
            .await?
    );
    store
        .checkpoint_atomic_state("alice", "failed", "failed-checkpoint", ROOT_LEASE)
        .await?;
    expire_memory_lease(store, &pool, "alice", "failed", ROOT_LEASE).await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, admissions.as_ref())
        .await?;
    store
        .acquire_atomic_state_lease("alice", "failed", A_LEASE)
        .await?;
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("failed"),
                lease: Some(A_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"failed-state"[..])
    );
    expire_memory_lease(store, &pool, "alice", "failed", A_LEASE).await?;
    admissions.prune_before(i64::MAX).await?;
    store
        .sweep_conversation_atomic_state(i64::MAX, admissions.as_ref())
        .await?;
    assert!(
        store
            .acquire_atomic_state_lease("alice", "failed", B_LEASE)
            .await
            .is_err()
    );
    assert!(
        store
            .fork_atomic_state("alice", "child", Some("failed-checkpoint"), B_LEASE)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn memory_clear_revokes_leases_but_preserves_referenced_snapshot() -> anyhow::Result<()> {
    use stravia_runtime_contract::{
        Principal,
        turn_chain::{TurnChainStore, TurnCommit, TurnNodeId, TurnNodeKind},
    };
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    store
        .attach_atomic_history_pool(pool.clone(), storage.prompt_admissions().as_ref())
        .await?;
    install(&storage, "test-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("clear", "test-vendor"))
        .await?;
    let owner = Principal::new("clear-owner");
    let principal = owner.continuation_key();
    store
        .fork_atomic_state(&principal, "root", None, ROOT_LEASE)
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "provider",
                },
                0,
                Some(b"provider"),
            )
            .await?
    );
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some(&principal),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"retained"),
            )
            .await?
    );
    let snapshot = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: Some(&principal),
            admission: Some("root"),
            lease: Some(ROOT_LEASE),
            key: "cell",
        })
        .await?;
    store
        .checkpoint_atomic_state(&principal, "root", "clear-checkpoint", ROOT_LEASE)
        .await?;
    let turns = crate::turn_chain::SqlTurnChainStore::sqlite(
        pool.clone(),
        crate::history_retention::HistoryRetention::for_tests(),
    );
    turns
        .commit(TurnCommit {
            id: TurnNodeId::response(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner,
            payload_version: 1,
            payload: serde_json::json!({"message":"retained"}),
            marker_references: Vec::new(),
            redaction_references: Vec::new(),
            compaction_record_ids: Vec::new(),
            atomic_checkpoint_id: Some("clear-checkpoint".into()),
            reusable_prefix: None,
        })
        .await?;
    let admissions = storage.prompt_admissions();
    store
        .clear_conversation_atomic_state(admissions.as_ref())
        .await?;
    assert!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some(&principal),
                admission: Some("root"),
                lease: Some(ROOT_LEASE),
                key: "cell",
            },)
            .await
            .is_err()
    );
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some(&principal),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                snapshot.revision,
                Some(b"late"),
            )
            .await
            .is_err()
    );
    assert!(
        store
            .checkpoint_atomic_state(&principal, "root", "late", ROOT_LEASE)
            .await
            .is_err()
    );
    assert!(
        store
            .renew_atomic_state_lease(&principal, "root", ROOT_LEASE)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: None,
                admission: None,
                lease: None,
                key: "provider",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"provider"[..])
    );
    store
        .fork_atomic_state(
            &principal,
            "referenced-child",
            Some("clear-checkpoint"),
            A_LEASE,
        )
        .await?;
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some(&principal),
                admission: Some("referenced-child"),
                lease: Some(A_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"retained"[..])
    );
    Ok(())
}

#[tokio::test]
async fn attaching_memory_history_fences_preexisting_operations() -> anyhow::Result<()> {
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    let admissions = storage.prompt_admissions();
    install(&storage, "test-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("pre-attach", "test-vendor"))
        .await?;
    let old_epoch = admissions.history_epoch().await?;
    store
        .fork_atomic_state_at_epoch("alice", "old", None, ROOT_LEASE, old_epoch)
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("old"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"old"),
            )
            .await?
    );
    store
        .attach_atomic_history_pool(pool.clone(), admissions.as_ref())
        .await?;
    let current = admissions.history_epoch().await?;
    assert!(current > old_epoch);
    let persisted: i64 = sqlx::query_scalar("SELECT epoch FROM prompt_admission_epoch WHERE id=1")
        .fetch_one(&pool)
        .await?;
    assert_eq!(persisted, current);
    assert!(persisted > 0);
    let sql_storage = SqliteStorage::from_pool(pool.clone());
    assert_eq!(
        crate::storage::Storage::prompt_admissions(&sql_storage)
            .history_epoch()
            .await?,
        current
    );
    assert!(
        store
            .fork_atomic_state_at_epoch("alice", "late", None, A_LEASE, old_epoch)
            .await
            .is_err()
    );
    assert!(
        store
            .acquire_atomic_state_lease_at_epoch("alice", "old", A_LEASE, old_epoch)
            .await
            .is_err()
    );
    assert!(
        store
            .acquire_atomic_state_lease_at_epoch("alice", "old", A_LEASE, current)
            .await
            .is_err()
    );
    assert!(
        store
            .renew_atomic_state_lease("alice", "old", ROOT_LEASE)
            .await
            .is_err()
    );
    assert!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("old"),
                lease: Some(ROOT_LEASE),
                key: "cell",
            },)
            .await
            .is_err()
    );
    store
        .fork_atomic_state_at_epoch("alice", "fresh", None, B_LEASE, current)
        .await?;
    Ok(())
}

#[tokio::test]
async fn memory_acquire_does_not_recreate_missing_mirrored_lineage() -> anyhow::Result<()> {
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    store
        .attach_atomic_history_pool(pool.clone(), storage.prompt_admissions().as_ref())
        .await?;
    install(&storage, "test-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("missing-mirror", "test-vendor"))
        .await?;
    store
        .fork_atomic_state("alice", "root", None, ROOT_LEASE)
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"original"),
            )
            .await?
    );
    let cell = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: Some("alice"),
            admission: Some("root"),
            lease: Some(ROOT_LEASE),
            key: "cell",
        })
        .await?;
    sqlx::query("DELETE FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2")
        .bind("alice")
        .bind("root")
        .execute(&pool)
        .await?;
    assert!(matches!(
        store
            .renew_atomic_state_lease("alice", "root", ROOT_LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(
        store
            .acquire_atomic_state_lease("alice", "root", A_LEASE)
            .await
            .is_err()
    );
    assert!(matches!(
        store
            .fork_atomic_state("alice", "root", None, B_LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("root"),
                lease: Some(ROOT_LEASE),
                key: "cell",
            },)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                cell.revision,
                Some(b"stale mutation"),
            )
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .checkpoint_atomic_state("alice", "root", "stale-checkpoint", ROOT_LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2",
    )
    .bind("alice")
    .bind("root")
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn memory_fork_rejects_mismatched_mirrored_checkpoint_provenance() -> anyhow::Result<()> {
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    store
        .attach_atomic_history_pool(pool.clone(), storage.prompt_admissions().as_ref())
        .await?;
    store
        .fork_atomic_state("alice", "root", None, ROOT_LEASE)
        .await?;
    store
        .checkpoint_atomic_state("alice", "root", "parent", ROOT_LEASE)
        .await?;
    sqlx::query(
        "UPDATE vendor_atomic_checkpoints SET lease_id=$1 WHERE principal=$2 AND checkpoint_id=$3",
    )
    .bind(BOB_LEASE)
    .bind("alice")
    .bind("parent")
    .execute(&pool)
    .await?;
    assert!(matches!(
        store
            .fork_atomic_state("alice", "wrong-lease", Some("parent"), A_LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    sqlx::query("UPDATE vendor_atomic_checkpoints SET lease_id=$1, admission_id=$2 WHERE principal=$3 AND checkpoint_id=$4")
        .bind(ROOT_LEASE).bind("wrong-admission").bind("alice").bind("parent").execute(&pool).await?;
    assert!(matches!(
        store
            .fork_atomic_state("alice", "wrong-admission", Some("parent"), B_LEASE)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    Ok(())
}

#[tokio::test]
async fn volatile_cells_are_revoked_by_reset_uninstall_and_provider_deletion() -> anyhow::Result<()>
{
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    let store = storage.vendor_plugins();
    let volatile = super::super::volatile_state::VolatileStore::for_installation(
        &uuid::Uuid::new_v4().to_string(),
    );
    store.attach_volatile_store(volatile.clone());
    install(&storage, "test-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("volatile", "test-vendor"))
        .await?;
    let fence = store
        .open_atomic_provider_fence("test-vendor", &provider.id, 1)
        .await?;
    let guard = volatile.read().await;
    let before_reset = guard
        .open("test-vendor", &provider.id, 1, fence, "cell".into())
        .unwrap();
    assert!(
        before_reset
            .compare_exchange(0, Some(b"old".to_vec()))
            .await
            .is_ok_and(|changed| changed)
    );
    drop(guard);
    let installed = store
        .list()
        .await?
        .into_iter()
        .find(|p| p.vendor_id == "test-vendor")
        .unwrap();
    let mut replacement = installed.clone();
    replacement.revision = 2;
    store
        .install(
            &replacement,
            Some(1),
            &[ProviderReset {
                impact: DataReset {
                    provider_id: provider.id.clone(),
                    private_state: true,
                    ..DataReset::default()
                },
                expected_vendor: "test-vendor".into(),
                retained_adapter_credentials: "{}".into(),
                retained_options: "{}".into(),
            }],
        )
        .await?;
    assert!(before_reset.load().await.is_err());
    assert!(
        before_reset
            .compare_exchange(1, Some(b"late".to_vec()))
            .await
            .is_err()
    );
    let fence = store
        .open_atomic_provider_fence("test-vendor", &provider.id, 1)
        .await?;
    let guard = volatile.read().await;
    let before_uninstall = guard
        .open("test-vendor", &provider.id, 1, fence, "cell".into())
        .unwrap();
    assert_eq!(before_uninstall.load().await.unwrap().revision, 0);
    drop(guard);
    store.uninstall("test-vendor", 2).await?;
    assert!(before_uninstall.load().await.is_err());
    install(&storage, "test-vendor").await?;
    let fence = store
        .open_atomic_provider_fence("test-vendor", &provider.id, 1)
        .await?;
    let guard = volatile.read().await;
    let before_delete = guard
        .open("test-vendor", &provider.id, 1, fence, "cell".into())
        .unwrap();
    drop(guard);
    storage.providers().delete(&provider.id).await?;
    assert!(before_delete.load().await.is_err());
    assert!(
        before_delete
            .compare_exchange(0, Some(b"late".to_vec()))
            .await
            .is_err()
    );
    Ok(())
}

async fn same_epoch_reset_invalidates_absent_cell_fence(
    storage: &DynStorage,
) -> anyhow::Result<()> {
    install(storage, "reset-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("reset provider", "reset-vendor"))
        .await?;
    let store = storage.vendor_plugins();
    let (first, second) = tokio::join!(
        store.open_atomic_provider_fence("reset-vendor", &provider.id, 1),
        store.open_atomic_provider_fence("reset-vendor", &provider.id, 1),
    );
    let old = first?;
    assert_eq!(old, second?);
    assert!(matches!(
        store
            .open_atomic_provider_fence("reset-vendor", &provider.id, 2)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .open_atomic_provider_fence("wrong-vendor", &provider.id, 1)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert_eq!(
        store
            .load_atomic_state_fenced(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "reset-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "absent",
                },
                old,
            )
            .await?
            .revision,
        0
    );
    let mut next = store
        .list()
        .await?
        .into_iter()
        .find(|p| p.vendor_id == "reset-vendor")
        .unwrap();
    next.revision = 2;
    store
        .install(
            &next,
            Some(1),
            &[ProviderReset {
                impact: DataReset {
                    provider_id: provider.id.clone(),
                    private_state: true,
                    ..DataReset::default()
                },
                expected_vendor: "reset-vendor".into(),
                retained_adapter_credentials: "{}".into(),
                retained_options: "{}".into(),
            }],
        )
        .await?;
    assert!(matches!(
        store
            .load_atomic_state_fenced(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "reset-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "absent",
                },
                old,
            )
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    assert!(matches!(
        store
            .compare_exchange_atomic_state_fenced(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "reset-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "absent",
                },
                0,
                Some(b"late"),
                old,
            )
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    let fresh = store
        .open_atomic_provider_fence("reset-vendor", &provider.id, 1)
        .await?;
    assert_ne!(fresh, old);
    assert!(
        store
            .compare_exchange_atomic_state_fenced(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "reset-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "absent",
                },
                0,
                Some(b"fresh"),
                fresh,
            )
            .await?
    );
    Ok(())
}

async fn same_admission_checkpoint_scenario(storage: &DynStorage) -> anyhow::Result<()> {
    install(storage, "checkpoint-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("checkpoint provider", "checkpoint-vendor"))
        .await?;
    let store = storage.vendor_plugins();
    store
        .fork_atomic_state("owner", "parent", None, ROOT_LEASE)
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "checkpoint-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("owner"),
                    admission: Some("parent"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"before"),
            )
            .await?
    );
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "checkpoint-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("owner"),
                    admission: Some("parent"),
                    lease: Some(ROOT_LEASE),
                    key: "second",
                },
                0,
                Some(b"second-before"),
            )
            .await?
    );
    store
        .acquire_atomic_state_lease("owner", "parent", A_LEASE)
        .await?;
    store
        .checkpoint_atomic_state("owner", "parent", "snapshot", A_LEASE)
        .await?;
    store
        .fork_atomic_state("owner", "child", Some("snapshot"), B_LEASE)
        .await?;
    let parent = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "checkpoint-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: Some("owner"),
            admission: Some("parent"),
            lease: Some(ROOT_LEASE),
            key: "cell",
        })
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "checkpoint-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("owner"),
                    admission: Some("parent"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                parent.revision,
                Some(b"after"),
            )
            .await?
    );
    let second = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "checkpoint-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: Some("owner"),
            admission: Some("parent"),
            lease: Some(ROOT_LEASE),
            key: "second",
        })
        .await?;
    assert!(
        store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "checkpoint-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("owner"),
                    admission: Some("parent"),
                    lease: Some(ROOT_LEASE),
                    key: "second",
                },
                second.revision,
                Some(b"second-after"),
            )
            .await?
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "checkpoint-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some("owner"),
                admission: Some("child"),
                lease: Some(B_LEASE),
                key: "second",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"second-before"[..])
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "checkpoint-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some("owner"),
                admission: Some("child"),
                lease: Some(B_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"before"[..])
    );
    let parent = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "checkpoint-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: Some("owner"),
            admission: Some("parent"),
            lease: Some(ROOT_LEASE),
            key: "cell",
        })
        .await?;
    let parent_key = crate::plugin::store::AtomicStateKey {
        vendor_id: "checkpoint-vendor",
        provider_id: &provider.id,
        data_epoch: 1,
        principal: Some("owner"),
        admission: Some("parent"),
        lease: Some(ROOT_LEASE),
        key: "cell",
    };
    let (changed, checkpoint) = tokio::join!(
        store.compare_exchange_atomic_state(&parent_key, parent.revision, Some(b"concurrent"),),
        store.checkpoint_atomic_state("owner", "parent", "concurrent-snapshot", A_LEASE),
    );
    assert!(changed?);
    checkpoint?;
    store
        .fork_atomic_state(
            "owner",
            "concurrent-child",
            Some("concurrent-snapshot"),
            REOPEN_LEASE,
        )
        .await?;
    let snapshot = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "checkpoint-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: Some("owner"),
            admission: Some("concurrent-child"),
            lease: Some(REOPEN_LEASE),
            key: "cell",
        })
        .await?;
    assert!(
        snapshot.payload.as_deref() == Some(&b"after"[..])
            || snapshot.payload.as_deref() == Some(&b"concurrent"[..])
    );
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "checkpoint-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some("owner"),
                admission: Some("parent"),
                lease: Some(ROOT_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"concurrent"[..])
    );
    Ok(())
}

#[tokio::test]
async fn memory_atomic_state_isolation_and_lifecycle() -> anyhow::Result<()> {
    let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
    same_epoch_reset_invalidates_absent_cell_fence(&storage).await?;
    same_admission_checkpoint_scenario(&storage).await?;
    scenario(storage).await
}

#[tokio::test]
async fn sqlite_atomic_state_isolation_and_lifecycle() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool));
    same_epoch_reset_invalidates_absent_cell_fence(&storage).await?;
    same_admission_checkpoint_scenario(&storage).await?;
    scenario(storage).await
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_atomic_operations_do_not_serialize_on_unrelated_provider() -> anyhow::Result<()> {
    let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")?;
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("atomic_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await?;
    let result: anyhow::Result<()> = async {
        crate::migrations::migrate_postgres(&pool, None).await?;
        let storage: DynStorage =
            Arc::new(crate::storage::PostgresStorage::from_pool(pool.clone()));
        same_epoch_reset_invalidates_absent_cell_fence(&storage).await?;
        same_admission_checkpoint_scenario(&storage).await?;
        install(&storage, "test-vendor").await?;
        let a = storage
            .providers()
            .create(provider("lock owner", "test-vendor"))
            .await?;
        let b = storage
            .providers()
            .create(provider("unrelated", "test-vendor"))
            .await?;
        let store = storage.vendor_plugins();
        payload_capacity_scenario(&storage, &a.id).await?;
        assert!(
            store
                .compare_exchange_atomic_state(
                    &crate::plugin::store::AtomicStateKey {
                        vendor_id: "test-vendor",
                        provider_id: &a.id,
                        data_epoch: 1,
                        principal: None,
                        admission: None,
                        lease: None,
                        key: "cell",
                    },
                    0,
                    Some(b"first"),
                )
                .await?
        );
        assert!(
            store
                .compare_exchange_atomic_state(
                    &crate::plugin::store::AtomicStateKey {
                        vendor_id: "test-vendor",
                        provider_id: &b.id,
                        data_epoch: 1,
                        principal: None,
                        admission: None,
                        lease: None,
                        key: "cell",
                    },
                    0,
                    Some(b"second"),
                )
                .await?
        );
        let (fence_a, fence_b) = tokio::join!(
            store.open_atomic_provider_fence("test-vendor", &a.id, 1),
            store.open_atomic_provider_fence("test-vendor", &b.id, 1),
        );
        let (fence_a, fence_b) = (fence_a?, fence_b?);
        assert_ne!(
            fence_a, fence_b,
            "concurrent fence creation must allocate distinct revisions"
        );
        let mut transaction = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('provider:' || $1),hashtext($2))")
            .bind("test-vendor")
            .bind(&a.id)
            .execute(&mut *transaction)
            .await?;
        let (first, second, write_b, open_a, open_b) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let cell_a = crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &a.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                };
                let cell_b = crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &b.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                };
                let other_b = crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &b.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "other",
                };
                tokio::join!(
                    store.load_atomic_state(&cell_a),
                    store.load_atomic_state(&cell_b),
                    store.compare_exchange_atomic_state(&other_b, 0, Some(b"b")),
                    store.open_atomic_provider_fence("test-vendor", &a.id, 1),
                    store.open_atomic_provider_fence("test-vendor", &b.id, 1),
                )
            })
            .await?;
        assert!(write_b?);
        assert_eq!(open_a?, fence_a);
        assert_eq!(open_b?, fence_b);
        assert_eq!(first?.payload.as_deref(), Some(&b"first"[..]));
        assert_eq!(second?.payload.as_deref(), Some(&b"second"[..]));
        transaction.rollback().await?;
        Ok(())
    }
    .await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    result
}

#[tokio::test]
async fn sqlite_existing_fence_does_not_wait_for_writer() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool.clone()));
    install(&storage, "fence-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("fence provider", "fence-vendor"))
        .await?;
    let store = storage.vendor_plugins();
    let fence = store
        .open_atomic_provider_fence("fence-vendor", &provider.id, 1)
        .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query("UPDATE vendor_atomic_revision SET revision=revision WHERE id=1")
        .execute(&mut *transaction)
        .await?;
    let opened = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.open_atomic_provider_fence("fence-vendor", &provider.id, 1),
    )
    .await??;
    assert_eq!(opened, fence);
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_committed_install_and_uninstall_survive_journal_cleanup_failure()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool.clone()));
    install(&storage, "cleanup-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("cleanup provider", "cleanup-vendor"))
        .await?;
    let store = storage.vendor_plugins();
    let old = store
        .open_atomic_provider_fence("cleanup-vendor", &provider.id, 1)
        .await?;
    // Break only the later GC query, not the install/reset transaction.
    sqlx::query("ALTER TABLE vendor_atomic_boundary_facts RENAME TO unavailable_boundary_facts")
        .execute(&pool)
        .await?;
    assert!(store.sweep_atomic_journal().await.is_err());
    let mut next = store
        .list()
        .await?
        .into_iter()
        .find(|p| p.vendor_id == "cleanup-vendor")
        .unwrap();
    next.revision = 2;
    store
        .install(
            &next,
            Some(1),
            &[ProviderReset {
                impact: DataReset {
                    provider_id: provider.id.clone(),
                    private_state: true,
                    ..DataReset::default()
                },
                expected_vendor: "cleanup-vendor".into(),
                retained_adapter_credentials: "{}".into(),
                retained_options: "{}".into(),
            }],
        )
        .await?;
    assert_eq!(
        store
            .list()
            .await?
            .into_iter()
            .find(|p| p.vendor_id == "cleanup-vendor")
            .unwrap()
            .revision,
        2
    );
    assert!(matches!(
        store
            .validate_atomic_provider_fence("cleanup-vendor", &provider.id, 1, old)
            .await,
        Err(PluginStorageError::StaleOperation)
    ));
    store.uninstall("cleanup-vendor", 2).await?;
    assert!(
        !store
            .list()
            .await?
            .iter()
            .any(|p| p.vendor_id == "cleanup-vendor")
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_atomic_state_survives_reopen() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool.clone()));
    install(&storage, "test-vendor").await?;
    let provider = storage
        .providers()
        .create(provider("persistent", "test-vendor"))
        .await?;
    assert!(
        storage
            .vendor_plugins()
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                0,
                Some(b"persisted"),
            )
            .await?
    );
    let before = storage
        .vendor_plugins()
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: None,
            admission: None,
            lease: None,
            key: "cell",
        })
        .await?;
    storage
        .vendor_plugins()
        .fork_atomic_state("alice", "root", None, ROOT_LEASE)
        .await?;
    assert!(
        storage
            .vendor_plugins()
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: Some("alice"),
                    admission: Some("root"),
                    lease: Some(ROOT_LEASE),
                    key: "cell",
                },
                0,
                Some(b"snapshot"),
            )
            .await?
    );
    storage
        .vendor_plugins()
        .checkpoint_atomic_state("alice", "root", "saved-checkpoint", ROOT_LEASE)
        .await?;
    drop(storage);
    pool.close().await;
    let reopened = crate::db::init_pool(directory.path()).await?;
    let store = PluginStore::sqlite(reopened);
    let after = store
        .load_atomic_state(&crate::plugin::store::AtomicStateKey {
            vendor_id: "test-vendor",
            provider_id: &provider.id,
            data_epoch: 1,
            principal: None,
            admission: None,
            lease: None,
            key: "cell",
        })
        .await?;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.payload.as_deref(), Some(&b"persisted"[..]));
    assert!(
        !store
            .compare_exchange_atomic_state(
                &crate::plugin::store::AtomicStateKey {
                    vendor_id: "test-vendor",
                    provider_id: &provider.id,
                    data_epoch: 1,
                    principal: None,
                    admission: None,
                    lease: None,
                    key: "cell",
                },
                0,
                None,
            )
            .await?
    );
    store
        .fork_atomic_state(
            "alice",
            "after-reopen",
            Some("saved-checkpoint"),
            REOPEN_LEASE,
        )
        .await?;
    assert_eq!(
        store
            .load_atomic_state(&crate::plugin::store::AtomicStateKey {
                vendor_id: "test-vendor",
                provider_id: &provider.id,
                data_epoch: 1,
                principal: Some("alice"),
                admission: Some("after-reopen"),
                lease: Some(REOPEN_LEASE),
                key: "cell",
            },)
            .await?
            .payload
            .as_deref(),
        Some(&b"snapshot"[..])
    );
    Ok(())
}
