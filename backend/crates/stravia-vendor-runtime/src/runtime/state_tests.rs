use super::*;
use crate::host::{HostStateCell, StateSnapshot};
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestCell {
    snapshot: Mutex<StateSnapshot>,
    drops: Arc<AtomicUsize>,
}

impl Drop for TestCell {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl HostStateCell for TestCell {
    async fn load(&self) -> Result<StateSnapshot, StateError> {
        Ok(self.snapshot.lock().unwrap().clone())
    }

    async fn compare_exchange(
        &self,
        expected: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, StateError> {
        let mut snapshot = self.snapshot.lock().unwrap();
        if expected != snapshot.revision {
            return Ok(false);
        }
        snapshot.revision += 1;
        snapshot.payload = payload;
        Ok(true)
    }

    async fn journal_compare_exchange(
        &self,
        expected: u64,
        payload: Option<Vec<u8>>,
        head: Option<Arc<dyn crate::host::HostJournalEntry>>,
    ) -> Result<bool, StateError> {
        assert!(head.is_none());
        self.compare_exchange(expected, payload).await
    }
}

#[tokio::test]
async fn state_open_rejects_invalid_and_unsupported_contexts() {
    let mut state = StoreState {
        wasi: WasiCtx::builder().build(),
        table: ResourceTable::new(),
        services: Arc::new(DenyServices),
        cancellation: CancellationToken::new(),
        deadline: Deadline::from_now(Duration::from_secs(60)),
        generation: 0,
        operation: Some(Operation::ConnectionCheck),
        catalog_sync: false,
        upstream_starts: 0,
        received_check_response: false,
    };
    let invalid = wit_state::Host::open(&mut state, wit_state::Scope::Provider, "x".repeat(129))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(invalid.code, "invalid");
    let unsupported =
        wit_state::Host::open(&mut state, wit_state::Scope::Conversation, "key".into())
            .await
            .unwrap()
            .unwrap_err();
    assert_eq!(unsupported.code, "unsupported");
    let invalid_volatile = wit_volatile::Host::open(&mut state, "x".repeat(129))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(invalid_volatile.code, "invalid");
    let unsupported_volatile = wit_volatile::Host::open(&mut state, "key".into())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(unsupported_volatile.code, "unsupported");
    state.operation = None;
    let pure = wit_state::Host::open(&mut state, wit_state::Scope::Provider, "key".into())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(pure.code, "unsupported");
}

#[tokio::test]
async fn state_cell_preserves_conflict_tombstone_cancellation_and_cleanup() {
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Arc::new(TestCell {
        snapshot: Mutex::new(StateSnapshot {
            revision: 0,
            payload: None,
        }),
        drops: drops.clone(),
    });
    let mut state = StoreState {
        wasi: WasiCtx::builder().build(),
        table: ResourceTable::new(),
        services: Arc::new(DenyServices),
        cancellation: CancellationToken::new(),
        deadline: Deadline::from_now(Duration::from_secs(60)),
        generation: 0,
        operation: Some(Operation::ConnectionCheck),
        catalog_sync: false,
        upstream_starts: 0,
        received_check_response: false,
    };
    let resource = state.table.push(StateCellResource(cell.clone())).unwrap();
    let rep = resource.rep();
    let borrow = || Resource::new_borrow(rep);
    let initial = wit_state::HostCell::load(&mut state, borrow())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial.revision, 0);
    assert!(initial.payload.is_none());
    assert!(
        wit_state::HostCell::compare_exchange(&mut state, borrow(), 0, Some(vec![1]))
            .await
            .unwrap()
            .unwrap()
    );
    assert!(
        !wit_state::HostCell::compare_exchange(&mut state, borrow(), 0, None)
            .await
            .unwrap()
            .unwrap()
    );
    assert!(
        wit_state::HostCell::compare_exchange(&mut state, borrow(), 1, None)
            .await
            .unwrap()
            .unwrap()
    );
    let deleted = wit_state::HostCell::load(&mut state, borrow())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.revision, 2);
    assert!(deleted.payload.is_none());
    let payload = vec![7; 1_048_576];
    assert!(
        wit_state::HostCell::compare_exchange(&mut state, borrow(), 2, Some(payload.clone()))
            .await
            .unwrap()
            .unwrap()
    );
    let too_large =
        wit_state::HostCell::compare_exchange(&mut state, borrow(), 3, Some(vec![0; 1_048_577]))
            .await
            .unwrap()
            .unwrap_err();
    assert_eq!(too_large.code, "resource-exhausted");
    let saved = wit_state::HostCell::load(&mut state, borrow())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.revision, 3);
    assert_eq!(saved.payload, Some(payload));
    let journal_payload = vec![8; 1_048_576];
    assert!(
        wit_journal::Host::compare_exchange(
            &mut state,
            borrow(),
            3,
            Some(journal_payload.clone()),
            None,
        )
        .await
        .unwrap()
        .unwrap()
    );
    let too_large = wit_journal::Host::compare_exchange(
        &mut state,
        borrow(),
        4,
        Some(vec![0; 1_048_577]),
        None,
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(too_large.code, "resource-exhausted");
    let saved = wit_state::HostCell::load(&mut state, borrow())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.revision, 4);
    assert_eq!(saved.payload, Some(journal_payload));
    state.cancellation.cancel();
    let cancelled = wit_state::HostCell::load(&mut state, borrow())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(cancelled.code, "cancelled");
    wit_state::HostCell::drop(&mut state, resource)
        .await
        .unwrap();
    drop(cell);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
