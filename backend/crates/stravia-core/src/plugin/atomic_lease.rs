//! Operation-scoped ownership of a durable conversation-state lease.
//!
//! The guard keeps the lease alive; it is not itself authority to access state.
//! Every state operation and checkpoint must still be fenced by the lease ID in
//! storage. Dropping the last clone stops renewal and lets the lease expire.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use stravia_runtime_contract::CancellationToken;
use uuid::Uuid;

use super::store::{PluginStorageError, PluginStore};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Safe, payload-free classification of the first lease operation failure.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub(crate) enum AtomicLeaseFailure {
    #[error("plugin or provider changed")]
    Changed,
    #[error("plugin operation is no longer current")]
    StaleOperation,
    #[error("plugin state lineage is unavailable")]
    MissingLineage,
    #[error("plugin state limit exceeded")]
    Limit,
    #[error("plugin storage operation failed")]
    Storage,
}

impl From<&PluginStorageError> for AtomicLeaseFailure {
    fn from(error: &PluginStorageError) -> Self {
        match error {
            PluginStorageError::Changed => Self::Changed,
            PluginStorageError::StaleOperation => Self::StaleOperation,
            PluginStorageError::MissingLineage => Self::MissingLineage,
            PluginStorageError::Limit => Self::Limit,
            PluginStorageError::Storage => Self::Storage,
        }
    }
}

impl From<AtomicLeaseFailure> for PluginStorageError {
    fn from(failure: AtomicLeaseFailure) -> Self {
        match failure {
            AtomicLeaseFailure::Changed => Self::Changed,
            AtomicLeaseFailure::StaleOperation => Self::StaleOperation,
            AtomicLeaseFailure::MissingLineage => Self::MissingLineage,
            AtomicLeaseFailure::Limit => Self::Limit,
            AtomicLeaseFailure::Storage => Self::Storage,
        }
    }
}

fn record_failure(
    failure: &Mutex<Option<AtomicLeaseFailure>>,
    cancellation: &CancellationToken,
    error: &PluginStorageError,
) {
    let mut saved = failure.lock();
    if saved.is_none() {
        let classified = AtomicLeaseFailure::from(error);
        tracing::warn!(failure = %classified, "atomic state lease failed");
        *saved = Some(classified);
    }
    drop(saved);
    cancellation.cancel();
}

#[derive(Clone)]
pub(crate) struct AtomicLeaseGuard {
    inner: Arc<AtomicLeaseInner>,
}

struct AtomicLeaseInner {
    store: PluginStore,
    principal: String,
    admission: String,
    lease_id: Uuid,
    failure: Arc<Mutex<Option<AtomicLeaseFailure>>>,
    cancellation: CancellationToken,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for AtomicLeaseGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtomicLeaseGuard")
            .field("lease_id", &self.inner.lease_id)
            .field("failed", &self.is_failed())
            .finish_non_exhaustive()
    }
}

impl Drop for AtomicLeaseInner {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

impl AtomicLeaseGuard {
    /// Fork only if the principal's history epoch is still the one observed
    /// before admission began. Clear and fork are serialized by the backend.
    pub(crate) async fn fork_at_epoch(
        store: PluginStore,
        principal: String,
        admission_id: Uuid,
        parent_checkpoint_id: Option<Uuid>,
        epoch: i64,
    ) -> Result<Self, PluginStorageError> {
        let lease_id = Uuid::new_v4();
        let admission = admission_id.to_string();
        let parent = parent_checkpoint_id.map(|id| id.to_string());
        store
            .fork_atomic_state_at_epoch(
                &principal,
                &admission,
                parent.as_deref(),
                &lease_id.to_string(),
                epoch,
            )
            .await?;
        Ok(Self::start(store, principal, admission, lease_id))
    }

    /// Reopen a lineage only while the captured history epoch remains current.
    pub(crate) async fn acquire_at_epoch(
        store: PluginStore,
        principal: String,
        admission_id: Uuid,
        epoch: i64,
    ) -> Result<Self, PluginStorageError> {
        let lease_id = Uuid::new_v4();
        let admission = admission_id.to_string();
        store
            .acquire_atomic_state_lease_at_epoch(
                &principal,
                &admission,
                &lease_id.to_string(),
                epoch,
            )
            .await?;
        Ok(Self::start(store, principal, admission, lease_id))
    }

    fn start(store: PluginStore, principal: String, admission: String, lease_id: Uuid) -> Self {
        let failure = Arc::new(Mutex::new(None));
        let cancellation = CancellationToken::new();
        let heartbeat_store = store.clone();
        let heartbeat_principal = principal.clone();
        let heartbeat_admission = admission.clone();
        let heartbeat_lease = lease_id.to_string();
        let heartbeat_failure = Arc::clone(&failure);
        let heartbeat_cancellation = cancellation.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                if let Err(error) = heartbeat_store
                    .renew_atomic_state_lease(
                        &heartbeat_principal,
                        &heartbeat_admission,
                        &heartbeat_lease,
                    )
                    .await
                {
                    record_failure(&heartbeat_failure, &heartbeat_cancellation, &error);
                    break;
                }
            }
        });
        Self {
            inner: Arc::new(AtomicLeaseInner {
                store,
                principal,
                admission,
                lease_id,
                failure,
                cancellation,
                heartbeat,
            }),
        }
    }

    pub(crate) fn lease_id(&self) -> Uuid {
        self.inner.lease_id
    }

    /// Signals when renewal or checkpointing fails. Link the operation's own
    /// cancellation token with this token to interrupt in-flight work.
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.inner.cancellation.clone()
    }

    /// The first renewal or checkpoint failure, recorded before cancellation.
    pub(crate) fn failure(&self) -> Option<AtomicLeaseFailure> {
        *self.inner.failure.lock()
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failure().is_some()
    }

    /// The backend checks the durable lease in the same transaction as the
    /// snapshot, even if the last heartbeat succeeded locally.
    pub(crate) async fn checkpoint(&self, checkpoint_id: Uuid) -> Result<(), PluginStorageError> {
        if let Some(failure) = self.failure() {
            return Err(failure.into());
        }
        let result = self
            .inner
            .store
            .checkpoint_atomic_state(
                &self.inner.principal,
                &self.inner.admission,
                &checkpoint_id.to_string(),
                &self.inner.lease_id.to_string(),
            )
            .await;
        if let Err(error) = &result {
            record_failure(&self.inner.failure, &self.inner.cancellation, error);
        }
        result
    }
}
