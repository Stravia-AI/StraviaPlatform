use super::cache::CachedMappingStore;
use crate::runtime_cache::RuntimeCache;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stravia_credential_protection::store::{
    DetectedSecret, InternedMappings, Mapping, MappingSnapshot, MappingStore,
};
use stravia_runtime_contract::{Principal, redaction::RedactionError};
use tokio::sync::oneshot;

#[derive(Default)]
struct State {
    mappings: HashMap<String, Vec<Mapping>>,
    next_reference: usize,
    fail_load: bool,
    fail_intern_after_write: bool,
    sweep: Sweep,
    gate: Option<LoadGate>,
    mutation_gate: Option<LoadGate>,
}

#[derive(Default)]
enum Sweep {
    #[default]
    None,
    Delete,
    DeleteThenFail,
}

struct LoadGate {
    captured: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[derive(Default)]
struct MemoryStore(Mutex<State>);

impl MemoryStore {
    fn gate_next_load(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (captured_tx, captured_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        self.0.lock().unwrap().gate = Some(LoadGate {
            captured: captured_tx,
            release: release_rx,
        });
        (captured_rx, release_tx)
    }

    fn gate_next_mutation(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (captured, committed) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        self.0.lock().unwrap().mutation_gate = Some(LoadGate {
            captured,
            release: blocked,
        });
        (committed, release)
    }
}

#[async_trait]
impl MappingStore for MemoryStore {
    async fn active(&self, principal: &Principal) -> Result<MappingSnapshot, RedactionError> {
        let (snapshot, gate) = {
            let mut state = self.0.lock().unwrap();
            if std::mem::take(&mut state.fail_load) {
                return Err(RedactionError::Storage);
            }
            let snapshot = state
                .mappings
                .get(&principal.continuation_key())
                .cloned()
                .unwrap_or_default();
            (Arc::new(snapshot), state.gate.take())
        };
        if let Some(gate) = gate {
            gate.captured.send(()).expect("load observer is alive");
            gate.release.await.expect("load gate is released");
        }
        Ok(snapshot)
    }

    async fn any_active(&self) -> Result<bool, RedactionError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .mappings
            .values()
            .any(|items| !items.is_empty()))
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[DetectedSecret],
    ) -> Result<InternedMappings, RedactionError> {
        let (result, gate) = {
            let mut state = self.0.lock().unwrap();
            let key = principal.continuation_key();
            let mut mappings = Vec::new();
            let mut created = Vec::new();
            for secret in secrets {
                let existing = state
                    .mappings
                    .get(&key)
                    .and_then(|items| items.iter().find(|item| item.secret == secret.secret))
                    .cloned();
                let mapping = if let Some(mapping) = existing {
                    mapping
                } else {
                    state.next_reference += 1;
                    let mapping = Mapping {
                        reference: format!("test-reference-{}", state.next_reference),
                        secret: secret.secret.clone(),
                        rule_ids: secret.rule_ids.clone().into(),
                        last_detected_at: 0,
                    };
                    state
                        .mappings
                        .entry(key.clone())
                        .or_default()
                        .push(mapping.clone());
                    created.push(mappings.len());
                    mapping
                };
                mappings.push(mapping);
            }
            if std::mem::take(&mut state.fail_intern_after_write) {
                return Err(RedactionError::Storage);
            }
            (
                InternedMappings {
                    mappings,
                    created,
                    renewed: false,
                },
                state.mutation_gate.take(),
            )
        };
        if let Some(gate) = gate {
            gate.captured.send(()).expect("mutation observer is alive");
            gate.release.await.expect("mutation gate is released");
        }
        Ok(result)
    }

    async fn publish(
        &self,
        _principal: &Principal,
        _references: &[String],
    ) -> Result<(), RedactionError> {
        Ok(())
    }

    async fn sweep(&self, _now: i64, _retention: Duration) -> Result<u64, RedactionError> {
        let (result, gate) = {
            let mut state = self.0.lock().unwrap();
            let result = match std::mem::take(&mut state.sweep) {
                Sweep::None => Ok(0),
                mode => {
                    let count = state
                        .mappings
                        .values()
                        .map(|items| items.len() as u64)
                        .sum();
                    state.mappings.clear();
                    if matches!(mode, Sweep::DeleteThenFail) {
                        Err(RedactionError::Storage)
                    } else {
                        Ok(count)
                    }
                }
            };
            (result, state.mutation_gate.take())
        };
        if let Some(gate) = gate {
            gate.captured.send(()).expect("mutation observer is alive");
            gate.release.await.expect("mutation gate is released");
        }
        result
    }

    async fn plaintext(&self, reference: &str) -> Result<Option<String>, RedactionError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .mappings
            .values()
            .flatten()
            .find(|item| item.reference == reference)
            .map(|item| item.secret.clone()))
    }
}

fn assert_plaintexts(snapshot: &MappingSnapshot, expected: &[&str]) {
    let mut actual: Vec<_> = snapshot.iter().map(|item| item.secret.as_str()).collect();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn isolates_principals_and_updates_a_cached_empty_snapshot() {
    let inner = Arc::new(MemoryStore::default());
    let cache = CachedMappingStore::new(inner, RuntimeCache::tinyufo(1024 * 1024));
    let alice = Principal::new("cache-alice");
    let bob = Principal::new("cache-bob");
    assert!(!cache.any_active().await.unwrap());
    assert_plaintexts(&cache.active(&alice).await.unwrap(), &[]);
    assert_plaintexts(&cache.active(&bob).await.unwrap(), &[]);
    let added = cache
        .intern(&alice, &["synthetic-alice".into()])
        .await
        .unwrap();
    assert_eq!(added.created, vec![0]);
    assert!(cache.any_active().await.unwrap());
    assert_plaintexts(&cache.active(&alice).await.unwrap(), &["synthetic-alice"]);
    assert_plaintexts(&cache.active(&bob).await.unwrap(), &[]);
    assert_eq!(
        cache
            .plaintext(&added.mappings[0].reference)
            .await
            .unwrap()
            .as_deref(),
        Some("synthetic-alice")
    );
    cache.intern(&bob, &["synthetic-bob".into()]).await.unwrap();
    assert_eq!(cache.sweep(0, Duration::ZERO).await.unwrap(), 0);
    cache
        .publish(&alice, &[added.mappings[0].reference.clone()])
        .await
        .unwrap();
    let after = cache.active(&alice).await.unwrap();
    assert_plaintexts(&after, &["synthetic-alice"]);
    assert_plaintexts(&cache.active(&bob).await.unwrap(), &["synthetic-bob"]);
}

#[tokio::test]
async fn concurrent_cold_load_cannot_restore_pre_intern_snapshot() {
    let inner = Arc::new(MemoryStore::default());
    let cache = Arc::new(CachedMappingStore::new(
        inner.clone(),
        RuntimeCache::tinyufo(1024 * 1024),
    ));
    let principal = Principal::new("race-intern");
    inner
        .intern(&principal, &["synthetic-existing".into()])
        .await
        .unwrap();
    let (captured, release) = inner.gate_next_load();
    let reader = {
        let cache = cache.clone();
        let principal = principal.clone();
        tokio::spawn(async move { cache.active(&principal).await })
    };
    captured.await.unwrap();
    let secrets = ["synthetic-new".into()];
    let mutation = cache.intern(&principal, &secrets);
    tokio::pin!(mutation);
    // Poll once before releasing the old read: implementations may serialize the writer.
    let completed = futures::poll!(&mut mutation);
    release.send(()).unwrap();
    match completed {
        std::task::Poll::Ready(result) => {
            result.unwrap();
        }
        std::task::Poll::Pending => {
            mutation.await.unwrap();
        }
    }
    reader.await.unwrap().unwrap();
    assert_plaintexts(
        &cache.active(&principal).await.unwrap(),
        &["synthetic-existing", "synthetic-new"],
    );
}

#[tokio::test]
async fn concurrent_cold_load_cannot_restore_swept_data() {
    let inner = Arc::new(MemoryStore::default());
    let cache = Arc::new(CachedMappingStore::new(
        inner.clone(),
        RuntimeCache::tinyufo(1024 * 1024),
    ));
    let principal = Principal::new("race-sweep");
    inner
        .intern(&principal, &["synthetic-expired".into()])
        .await
        .unwrap();
    inner.0.lock().unwrap().sweep = Sweep::Delete;
    let (captured, release) = inner.gate_next_load();
    let reader = {
        let cache = cache.clone();
        let principal = principal.clone();
        tokio::spawn(async move { cache.active(&principal).await })
    };
    captured.await.unwrap();
    let mutation = cache.sweep(0, Duration::ZERO);
    tokio::pin!(mutation);
    let completed = futures::poll!(&mut mutation);
    release.send(()).unwrap();
    match completed {
        std::task::Poll::Ready(result) => {
            assert_eq!(result.unwrap(), 1);
        }
        std::task::Poll::Pending => {
            assert_eq!(mutation.await.unwrap(), 1);
        }
    }
    reader.await.unwrap().unwrap();
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());
}

#[tokio::test]
async fn sweep_and_partial_failures_invalidate_snapshots_and_existence() {
    let inner = Arc::new(MemoryStore::default());
    let cache = CachedMappingStore::new(inner.clone(), RuntimeCache::tinyufo(1024 * 1024));
    let principal = Principal::new("sweep-owner");
    cache
        .intern(&principal, &["synthetic-one".into()])
        .await
        .unwrap();
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &["synthetic-one"]);
    assert!(cache.any_active().await.unwrap());
    inner.0.lock().unwrap().sweep = Sweep::Delete;
    assert_eq!(cache.sweep(0, Duration::ZERO).await.unwrap(), 1);
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());

    cache
        .intern(&principal, &["synthetic-two".into()])
        .await
        .unwrap();
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &["synthetic-two"]);
    assert!(cache.any_active().await.unwrap());
    inner.0.lock().unwrap().sweep = Sweep::DeleteThenFail;
    assert!(cache.sweep(0, Duration::ZERO).await.is_err());
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());

    inner.0.lock().unwrap().fail_intern_after_write = true;
    assert!(
        cache
            .intern(&principal, &["synthetic-three".into()])
            .await
            .is_err()
    );
    assert_plaintexts(
        &cache.active(&principal).await.unwrap(),
        &["synthetic-three"],
    );
    assert!(cache.any_active().await.unwrap());
}

#[tokio::test]
async fn failed_load_is_not_cached_as_empty() {
    let inner = Arc::new(MemoryStore::default());
    let principal = Principal::new("load-error");
    inner
        .intern(&principal, &["synthetic-loaded".into()])
        .await
        .unwrap();
    inner.0.lock().unwrap().fail_load = true;
    let cache = CachedMappingStore::new(inner, RuntimeCache::tinyufo(1024 * 1024));
    assert!(cache.active(&principal).await.is_err());
    assert_plaintexts(
        &cache.active(&principal).await.unwrap(),
        &["synthetic-loaded"],
    );
}

#[tokio::test]
async fn cancelled_confirmation_after_commit_invalidates_cached_data() {
    let inner = Arc::new(MemoryStore::default());
    let cache = Arc::new(CachedMappingStore::new(
        inner.clone(),
        RuntimeCache::tinyufo(1024 * 1024),
    ));
    let principal = Principal::new("cancelled-commit");
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());

    let (committed, _intern_release) = inner.gate_next_mutation();
    let writer = {
        let cache = cache.clone();
        let principal = principal.clone();
        tokio::spawn(async move {
            cache
                .intern(&principal, &["synthetic-committed".into()])
                .await
        })
    };
    committed.await.unwrap();
    writer.abort();
    assert!(matches!(writer.await, Err(error) if error.is_cancelled()));
    assert_plaintexts(
        &cache.active(&principal).await.unwrap(),
        &["synthetic-committed"],
    );
    assert!(cache.any_active().await.unwrap());

    inner.0.lock().unwrap().sweep = Sweep::Delete;
    let (committed, _sweep_release) = inner.gate_next_mutation();
    let sweeper = {
        let cache = cache.clone();
        tokio::spawn(async move { cache.sweep(0, Duration::ZERO).await })
    };
    committed.await.unwrap();
    sweeper.abort();
    assert!(matches!(sweeper.await, Err(error) if error.is_cancelled()));
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());
}

#[tokio::test]
async fn oversized_snapshot_falls_back_to_store() {
    let inner = Arc::new(MemoryStore::default());
    let cache = CachedMappingStore::new(inner.clone(), RuntimeCache::tinyufo(1024));
    let principal = Principal::new("oversized-snapshot");
    let secret = "x".repeat(4096);
    cache
        .intern(
            &principal,
            &[DetectedSecret {
                secret: secret.clone(),
                rule_ids: vec!["large-rule".to_owned()],
            }],
        )
        .await
        .unwrap();
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &[secret.as_str()]);
    inner.0.lock().unwrap().fail_load = true;
    assert!(cache.active(&principal).await.is_err());
    assert_plaintexts(&cache.active(&principal).await.unwrap(), &[secret.as_str()]);
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_REDIS_URL pointing to an isolated Redis instance"]
async fn redis_mapping_snapshots_preserve_data_and_invalidation() {
    let url = std::env::var("STRAVIA_TEST_REDIS_URL")
        .expect("STRAVIA_TEST_REDIS_URL must point to an isolated Redis instance");
    let runtime = RuntimeCache::redis(&url, 1024 * 1024).await.unwrap();
    let inner = Arc::new(MemoryStore::default());
    let cache = Arc::new(CachedMappingStore::new(inner.clone(), runtime.clone()));
    let alice = Principal::new("redis-cache-alice");
    let bob = Principal::new("redis-cache-bob");
    assert_plaintexts(&cache.active(&alice).await.unwrap(), &[]);
    assert_plaintexts(&cache.active(&bob).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());

    cache
        .intern(
            &alice,
            &[DetectedSecret {
                secret: "synthetic-redis-secret".to_owned(),
                rule_ids: vec!["rule-one".to_owned(), "rule-two".to_owned()],
            }],
        )
        .await
        .unwrap();
    let snapshot = cache.active(&alice).await.unwrap();
    assert_plaintexts(&snapshot, &["synthetic-redis-secret"]);
    assert_eq!(snapshot[0].rule_ids.as_slice(), &["rule-one", "rule-two"]);
    // A store reload fails, so this also exercises decoding the Redis snapshot.
    inner.0.lock().unwrap().fail_load = true;
    let hot = cache.active(&alice).await.unwrap();
    assert_plaintexts(&hot, &["synthetic-redis-secret"]);
    assert_eq!(hot[0].rule_ids.as_slice(), &["rule-one", "rule-two"]);
    inner.0.lock().unwrap().fail_load = false;
    assert_plaintexts(&cache.active(&bob).await.unwrap(), &[]);
    assert!(cache.any_active().await.unwrap());

    inner.0.lock().unwrap().sweep = Sweep::DeleteThenFail;
    assert!(cache.sweep(0, Duration::ZERO).await.is_err());
    assert_plaintexts(&cache.active(&alice).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());
    inner.0.lock().unwrap().fail_intern_after_write = true;
    assert!(
        cache
            .intern(&alice, &["synthetic-partial".into()])
            .await
            .is_err()
    );
    assert_plaintexts(&cache.active(&alice).await.unwrap(), &["synthetic-partial"]);
    assert!(cache.any_active().await.unwrap());

    inner.0.lock().unwrap().sweep = Sweep::Delete;
    let (committed, _release) = inner.gate_next_mutation();
    let sweeper = {
        let cache = cache.clone();
        tokio::spawn(async move { cache.sweep(0, Duration::ZERO).await })
    };
    committed.await.unwrap();
    sweeper.abort();
    assert!(matches!(sweeper.await, Err(error) if error.is_cancelled()));
    assert_plaintexts(&cache.active(&alice).await.unwrap(), &[]);
    assert!(!cache.any_active().await.unwrap());
    cache.intern(&bob, &["synthetic-bob".into()]).await.unwrap();
    inner.0.lock().unwrap().sweep = Sweep::Delete;
    assert_eq!(cache.sweep(0, Duration::ZERO).await.unwrap(), 1);
    assert_plaintexts(&cache.active(&bob).await.unwrap(), &[]);
    runtime.shutdown().await;
}
