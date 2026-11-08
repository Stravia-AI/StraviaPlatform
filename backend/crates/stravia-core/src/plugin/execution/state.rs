use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use stravia_runtime_contract::CancellationToken;
use stravia_vendor_runtime::host::{
    HostJournalEntry, HostStateCell, JournalEntryData, JournalSnapshot, StateError,
    StateScope as AtomicStateScope, StateSnapshot,
};

use crate::interaction_observation::ProtectedSecrets;
use crate::plugin::lifecycle::VendorOperation;
use crate::plugin::store::atomic_journal::JournalScope;
use crate::plugin::store::{AtomicStateKey, PluginStorageError, PluginStore};
use crate::plugin::volatile_state::{VolatileCell, VolatileStore};

use super::{ScopedHostServices, StateScope, register_state_secrets};

#[derive(Clone)]
struct AtomicHostCell {
    store: PluginStore,
    vendor_id: String,
    provider_id: String,
    data_epoch: i64,
    provider_fence: i64,
    history_boundary_candidate: Option<crate::generation_chain::HistoryBoundaryCandidate>,
    principal: Option<String>,
    admission_id: Option<String>,
    lease_id: Option<String>,
    key: String,
    operation: Arc<VendorOperation>,
    cancellation: CancellationToken,
    secrets: ProtectedSecrets,
}

struct AtomicJournalEntry {
    bound: AtomicHostCell,
    scope: JournalScope,
    node_id: String,
}

struct VolatileHostCell {
    bound: AtomicHostCell,
    store: VolatileStore,
    cell: VolatileCell,
}

impl AtomicHostCell {
    fn state_key(&self) -> AtomicStateKey<'_> {
        AtomicStateKey {
            vendor_id: &self.vendor_id,
            provider_id: &self.provider_id,
            data_epoch: self.data_epoch,
            principal: self.principal.as_deref(),
            admission: self.admission_id.as_deref(),
            lease: self.lease_id.as_deref(),
            key: &self.key,
        }
    }

    async fn storage<T>(
        &self,
        request: impl Future<Output = Result<T, PluginStorageError>>,
    ) -> Result<T, StateError> {
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(cancelled_state()),
            _ = self.operation.cancellation().cancelled() => Err(cancelled_state()),
            result = request => result.map_err(atomic_state_error),
        }
    }

    fn journal_scope(&self) -> Result<JournalScope, StateError> {
        let (Some(principal), Some(admission), Some(lease)) =
            (&self.principal, &self.admission_id, &self.lease_id)
        else {
            return Err(StateError::new(
                "unsupported",
                "journal requires an admitted conversation",
            ));
        };
        Ok(JournalScope {
            vendor_id: self.vendor_id.clone(),
            provider_id: self.provider_id.clone(),
            data_epoch: self.data_epoch,
            provider_fence: self.provider_fence,
            candidate: self.history_boundary_candidate.clone(),
            principal: principal.clone(),
            admission: admission.clone(),
            lease: lease.clone(),
            key: self.key.clone(),
        })
    }

    fn journal_entry_id<'a>(
        &self,
        scope: &JournalScope,
        entry: &'a Arc<dyn HostJournalEntry>,
    ) -> Result<&'a str, StateError> {
        let entry = entry
            .as_any()
            .downcast_ref::<AtomicJournalEntry>()
            .ok_or_else(|| {
                StateError::new("invalid", "journal entry belongs to another operation")
            })?;
        let other = &entry.scope;
        if !Arc::ptr_eq(&self.operation, &entry.bound.operation)
            || scope.vendor_id != other.vendor_id
            || scope.provider_id != other.provider_id
            || scope.data_epoch != other.data_epoch
            || scope.provider_fence != other.provider_fence
            || scope.principal != other.principal
            || scope.admission != other.admission
            || scope.lease != other.lease
        {
            return Err(StateError::new(
                "invalid",
                "journal entry belongs to another operation",
            ));
        }
        Ok(&entry.node_id)
    }

    fn journal_entry(&self, scope: &JournalScope, node_id: String) -> Arc<dyn HostJournalEntry> {
        Arc::new(AtomicJournalEntry {
            bound: self.clone(),
            scope: scope.clone(),
            node_id,
        })
    }

    fn ensure_current(&self) -> Result<(), StateError> {
        if self.cancellation.is_cancelled() || self.operation.ensure_current().is_err() {
            return Err(cancelled_state());
        }
        Ok(())
    }
}

fn atomic_state_error(error: PluginStorageError) -> StateError {
    match error {
        PluginStorageError::Limit => {
            StateError::new("resource-exhausted", "atomic state limit exceeded")
        }
        PluginStorageError::Changed
        | PluginStorageError::StaleOperation
        | PluginStorageError::MissingLineage => cancelled_state(),
        PluginStorageError::Storage => StateError::new("storage", "atomic state storage failed"),
    }
}

#[async_trait]
impl HostJournalEntry for AtomicJournalEntry {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn read(&self) -> Result<JournalEntryData, StateError> {
        self.bound.ensure_current()?;
        let (text, previous) = self
            .bound
            .storage(self.bound.store.journal_read(&self.scope, &self.node_id))
            .await?;
        self.bound.ensure_current()?;
        register_state_secrets(&self.bound.secrets, text.as_bytes());
        Ok(JournalEntryData {
            text,
            previous: previous.map(|id| self.bound.journal_entry(&self.scope, id)),
        })
    }
}

#[async_trait]
impl HostStateCell for AtomicHostCell {
    async fn load(&self) -> Result<StateSnapshot, StateError> {
        self.ensure_current()?;
        let key = self.state_key();
        let snapshot = self
            .storage(
                self.store
                    .load_atomic_state_fenced(&key, self.provider_fence),
            )
            .await?;
        self.ensure_current()?;
        if let Some(bytes) = &snapshot.payload {
            register_state_secrets(&self.secrets, bytes);
        }
        Ok(StateSnapshot {
            revision: snapshot.revision,
            payload: snapshot.payload,
        })
    }

    async fn compare_exchange(
        &self,
        expected_revision: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, StateError> {
        self.ensure_current()?;
        let fence = self
            .operation
            .write_fence()
            .await
            .map_err(|_| cancelled_state())?;
        self.ensure_current()?;
        if let Some(bytes) = &payload {
            register_state_secrets(&self.secrets, bytes);
        }
        let key = self.state_key();
        let exchanged = self
            .storage(self.store.compare_exchange_atomic_state_fenced(
                &key,
                expected_revision,
                payload.as_deref(),
                self.provider_fence,
            ))
            .await?;
        self.ensure_current()?;
        drop(fence);
        Ok(exchanged)
    }

    async fn journal_load(&self) -> Result<JournalSnapshot, StateError> {
        self.ensure_current()?;
        let scope = self.journal_scope()?;
        let snapshot = self.storage(self.store.journal_load(&scope)).await?;
        self.ensure_current()?;
        if let Some(bytes) = &snapshot.payload {
            register_state_secrets(&self.secrets, bytes);
        }
        Ok(JournalSnapshot {
            revision: snapshot.revision,
            payload: snapshot.payload,
            head: snapshot
                .journal_head
                .map(|id| self.journal_entry(&scope, id)),
        })
    }

    async fn journal_append(
        &self,
        text: String,
        previous: Option<Arc<dyn HostJournalEntry>>,
    ) -> Result<Arc<dyn HostJournalEntry>, StateError> {
        self.ensure_current()?;
        let scope = self.journal_scope()?;
        let previous_id = previous
            .as_ref()
            .map(|entry| self.journal_entry_id(&scope, entry))
            .transpose()?;
        let fence = self
            .operation
            .write_fence()
            .await
            .map_err(|_| cancelled_state())?;
        self.ensure_current()?;
        register_state_secrets(&self.secrets, text.as_bytes());
        let node_id = self
            .storage(self.store.journal_append(&scope, text, previous_id))
            .await?;
        self.ensure_current()?;
        drop(fence);
        Ok(self.journal_entry(&scope, node_id))
    }

    async fn journal_compare_exchange(
        &self,
        expected_revision: u64,
        payload: Option<Vec<u8>>,
        head: Option<Arc<dyn HostJournalEntry>>,
    ) -> Result<bool, StateError> {
        self.ensure_current()?;
        let scope = self.journal_scope()?;
        let head_id = head
            .as_ref()
            .map(|entry| self.journal_entry_id(&scope, entry))
            .transpose()?;
        let fence = self
            .operation
            .write_fence()
            .await
            .map_err(|_| cancelled_state())?;
        self.ensure_current()?;
        if let Some(bytes) = &payload {
            register_state_secrets(&self.secrets, bytes);
        }
        let exchanged = self
            .storage(self.store.journal_compare_exchange(
                &scope,
                expected_revision,
                payload.as_deref(),
                head_id,
            ))
            .await?;
        self.ensure_current()?;
        drop(fence);
        Ok(exchanged)
    }
}

impl VolatileHostCell {
    async fn validate(&self) -> Result<(), StateError> {
        self.bound
            .storage(self.bound.store.validate_atomic_provider_fence(
                &self.bound.vendor_id,
                &self.bound.provider_id,
                self.bound.data_epoch,
                self.bound.provider_fence,
            ))
            .await
    }
}

#[async_trait]
impl HostStateCell for VolatileHostCell {
    async fn load(&self) -> Result<StateSnapshot, StateError> {
        self.bound.ensure_current()?;
        let _guard = tokio::select! {
            biased;
            _ = self.bound.cancellation.cancelled() => return Err(cancelled_state()),
            _ = self.bound.operation.cancellation().cancelled() => return Err(cancelled_state()),
            guard = self.store.read() => guard,
        };
        self.bound.ensure_current()?;
        self.validate().await?;
        let snapshot = self.cell.load().await?;
        self.bound.ensure_current()?;
        if let Some(bytes) = &snapshot.payload {
            register_state_secrets(&self.bound.secrets, bytes);
        }
        Ok(snapshot)
    }

    async fn compare_exchange(
        &self,
        expected_revision: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, StateError> {
        self.bound.ensure_current()?;
        let fence = self
            .bound
            .operation
            .write_fence()
            .await
            .map_err(|_| cancelled_state())?;
        let _guard = tokio::select! {
            biased;
            _ = self.bound.cancellation.cancelled() => return Err(cancelled_state()),
            _ = self.bound.operation.cancellation().cancelled() => return Err(cancelled_state()),
            guard = self.store.read() => guard,
        };
        self.bound.ensure_current()?;
        self.validate().await?;
        if let Some(bytes) = &payload {
            register_state_secrets(&self.bound.secrets, bytes);
        }
        let exchanged = self
            .cell
            .compare_exchange(expected_revision, payload)
            .await?;
        self.bound.ensure_current()?;
        drop(fence);
        Ok(exchanged)
    }
}

impl ScopedHostServices {
    pub(super) async fn open_volatile_cell(
        &self,
        key: String,
    ) -> Result<Arc<dyn HostStateCell>, StateError> {
        self.ensure_current().map_err(|_| cancelled_state())?;
        if key.len() > 128 {
            return Err(StateError::new("invalid", "atomic state key is invalid"));
        }
        let StateScope::Provider {
            store,
            vendor_id,
            provider_id,
            data_epoch,
            ..
        } = &self.state
        else {
            return Err(StateError::new(
                "unsupported",
                "atomic state requires an admitted provider",
            ));
        };
        let volatile = store
            .volatile_store()
            .ok_or_else(|| StateError::new("unsupported", "volatile state is unavailable"))?;
        let guard = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => return Err(cancelled_state()),
            _ = self.operation.cancellation().cancelled() => return Err(cancelled_state()),
            guard = volatile.read() => guard,
        };
        self.ensure_current().map_err(|_| cancelled_state())?;
        let provider_fence = store
            .open_atomic_provider_fence(vendor_id, provider_id, *data_epoch)
            .await
            .map_err(atomic_state_error)?;
        self.ensure_current().map_err(|_| cancelled_state())?;
        let cell = guard.open(
            vendor_id,
            provider_id,
            *data_epoch,
            provider_fence,
            key.clone(),
        )?;
        drop(guard);
        Ok(Arc::new(VolatileHostCell {
            bound: AtomicHostCell {
                store: store.clone(),
                vendor_id: vendor_id.clone(),
                provider_id: provider_id.clone(),
                data_epoch: *data_epoch,
                provider_fence,
                history_boundary_candidate: None,
                principal: None,
                admission_id: None,
                lease_id: None,
                key,
                operation: self.operation.clone(),
                cancellation: self.cancellation.clone(),
                secrets: self.secrets.clone(),
            },
            store: volatile,
            cell,
        }))
    }

    pub(super) async fn open_atomic_cell(
        &self,
        scope: AtomicStateScope,
        key: String,
    ) -> Result<Arc<dyn HostStateCell>, StateError> {
        self.ensure_current().map_err(|_| cancelled_state())?;
        if key.len() > 128 {
            return Err(StateError::new("invalid", "atomic state key is invalid"));
        }
        let StateScope::Provider {
            store,
            vendor_id,
            provider_id,
            data_epoch,
            ..
        } = &self.state
        else {
            return Err(StateError::new(
                "unsupported",
                "atomic state requires an admitted provider",
            ));
        };
        let provider_fence = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => return Err(cancelled_state()),
            _ = self.operation.cancellation().cancelled() => return Err(cancelled_state()),
            result = store.open_atomic_provider_fence(vendor_id, provider_id, *data_epoch) => result.map_err(atomic_state_error)?,
        };
        self.ensure_current().map_err(|_| cancelled_state())?;
        let (principal, admission_id, lease_id) = match scope {
            AtomicStateScope::Provider => (None, None, None),
            AtomicStateScope::Conversation => {
                let context = self.atomic_state.as_ref().ok_or_else(|| {
                    StateError::new(
                        "unsupported",
                        "atomic state requires an admitted conversation",
                    )
                })?;
                (
                    Some(context.principal.clone()),
                    Some(context.admission_id.to_string()),
                    Some(context.atomic_lease_id.to_string()),
                )
            }
        };
        Ok(Arc::new(AtomicHostCell {
            store: store.clone(),
            vendor_id: vendor_id.clone(),
            provider_id: provider_id.clone(),
            data_epoch: *data_epoch,
            provider_fence,
            history_boundary_candidate: self.history_boundary_candidate.clone(),
            principal,
            admission_id,
            lease_id,
            key,
            operation: self.operation.clone(),
            cancellation: self.cancellation.clone(),
            secrets: self.secrets.clone(),
        }))
    }
}

fn cancelled_state() -> StateError {
    StateError::new("cancelled", "vendor operation is no longer current")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn state_journal_rejects_foreign_operation_and_unadmitted_scope() {
        use crate::plugin::lifecycle::VendorOperationTracker;
        use crate::storage::{DynStorage, MemoryStorage};

        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let tracker = VendorOperationTracker::default();
        let cell = AtomicHostCell {
            store: storage.vendor_plugins().clone(),
            vendor_id: "vendor".into(),
            provider_id: "provider".into(),
            data_epoch: 1,
            provider_fence: 0,
            history_boundary_candidate: None,
            principal: Some("principal".into()),
            admission_id: Some("admission".into()),
            lease_id: Some("lease".into()),
            key: "journal".into(),
            operation: tracker.begin("vendor").unwrap(),
            cancellation: CancellationToken::new(),
            secrets: ProtectedSecrets::default(),
        };
        let mut foreign = cell.clone();
        foreign.operation = tracker.begin("vendor").unwrap();
        let entry = foreign.journal_entry(&foreign.journal_scope().unwrap(), "node".into());
        let error = cell
            .journal_append("entry".into(), Some(entry.clone()))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "invalid");
        let error = cell
            .journal_compare_exchange(0, None, Some(entry))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "invalid");

        let mut provider_cell = cell;
        provider_cell.principal = None;
        provider_cell.admission_id = None;
        provider_cell.lease_id = None;
        let error = provider_cell.journal_load().await.err().unwrap();
        assert_eq!(error.code, "unsupported");
    }
}
