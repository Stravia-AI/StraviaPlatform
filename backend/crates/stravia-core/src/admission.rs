use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::GatewayError;
use crate::hook::Principal;

/// Gateway-local coordinator for Principal Concurrency Limit admission.
///
/// Every root request is recorded, including Principals without a configured
/// limit, so lowering a limit affects only later admission without cancelling
/// work already in progress.
pub(crate) struct PrincipalAdmission {
    state: Mutex<AdmissionState>,
    released: tokio::sync::Notify,
}

struct AdmissionState {
    active_by_principal: HashMap<String, usize>,
    limit_by_principal: HashMap<String, Option<i32>>,
}

impl PrincipalAdmission {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(AdmissionState {
                active_by_principal: HashMap::new(),
                limit_by_principal: HashMap::new(),
            }),
            released: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn set_limit(&self, principal_id: &str, concurrency_limit: Option<i32>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .limit_by_principal
            .insert(principal_id.to_owned(), concurrency_limit);
    }

    pub(crate) fn remove_principal(&self, principal_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.limit_by_principal.remove(principal_id);
    }

    pub(crate) fn acquire(
        self: &Arc<Self>,
        principal: &Principal,
        concurrency_limit: Option<i32>,
    ) -> Result<PrincipalAdmissionLease, GatewayError> {
        let principal_id = principal.api_key_id().to_owned();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let limit = *state
            .limit_by_principal
            .entry(principal_id.clone())
            .or_insert(concurrency_limit);
        let limit = normalize_limit(limit)?;
        let active = state
            .active_by_principal
            .entry(principal_id.clone())
            .or_default();
        if limit.is_some_and(|limit| *active >= limit) {
            return Err(GatewayError::ConcurrencyLimitExceeded);
        }
        *active += 1;

        Ok(PrincipalAdmissionLease {
            coordinator: Arc::clone(self),
            principal_id,
        })
    }

    pub(crate) async fn acquire_wait(
        self: &Arc<Self>,
        principal: &Principal,
        concurrency_limit: Option<i32>,
    ) -> Result<PrincipalAdmissionLease, GatewayError> {
        loop {
            let released = self.released.notified();
            match self.acquire(principal, concurrency_limit) {
                Ok(lease) => return Ok(lease),
                Err(GatewayError::ConcurrencyLimitExceeded) => released.await,
                Err(error) => return Err(error),
            }
        }
    }

    fn release(&self, principal_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(active) = state.active_by_principal.get_mut(principal_id) else {
            return;
        };
        debug_assert!(*active > 0, "admission lease released exactly once");
        *active -= 1;
        if *active == 0 {
            state.active_by_principal.remove(principal_id);
        }
        drop(state);
        self.released.notify_waiters();
    }
}

fn normalize_limit(value: Option<i32>) -> Result<Option<usize>, GatewayError> {
    value
        .map(|value| {
            usize::try_from(value).map_err(|_| {
                GatewayError::internal(anyhow::anyhow!(
                    "stored Principal Concurrency Limit must be positive"
                ))
            })
        })
        .transpose()
        .and_then(|limit| {
            if limit == Some(0) {
                Err(GatewayError::internal(anyhow::anyhow!(
                    "stored Principal Concurrency Limit must be positive"
                )))
            } else {
                Ok(limit)
            }
        })
}

pub(crate) struct PrincipalAdmissionLease {
    coordinator: Arc<PrincipalAdmission>,
    principal_id: String,
}

impl Drop for PrincipalAdmissionLease {
    fn drop(&mut self) {
        self.coordinator.release(&self.principal_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::PrincipalAdmission;
    use crate::error::GatewayError;
    use crate::hook::Principal;

    #[test]
    fn configured_limit_wins_over_stale_authenticated_snapshot() {
        let admission = Arc::new(PrincipalAdmission::new());
        let principal = Principal::new("key");
        let first = admission
            .acquire(&principal, None)
            .expect("unlimited authenticated snapshot");

        admission.set_limit(principal.api_key_id(), Some(1));
        assert!(matches!(
            admission.acquire(&principal, None),
            Err(GatewayError::ConcurrencyLimitExceeded)
        ));

        drop(first);
        admission
            .acquire(&principal, None)
            .expect("slot released for next root request");
    }

    #[tokio::test]
    async fn waiting_admission_acquires_after_inherited_slot_release() {
        let admission = Arc::new(PrincipalAdmission::new());
        let principal = Principal::new("key");
        let inherited = admission
            .acquire(&principal, Some(1))
            .expect("originating request slot");
        let waiting = tokio::spawn({
            let admission = Arc::clone(&admission);
            let principal = principal.clone();
            async move { admission.acquire_wait(&principal, Some(1)).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        drop(inherited);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("waiter notified after inherited slot release")
            .expect("waiter task")
            .expect("waiter admission");
    }
}
