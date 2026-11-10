use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use stravia_vendor_runtime::host::{StateError, StateSnapshot};
use stravia_vendor_sdk::state::MAX_PAYLOAD_BYTES;

type ScopeKey = (String, String, i64, i64);
type ScopeMap = HashMap<ScopeKey, Arc<Mutex<Scope>>>;

#[derive(Default)]
struct Scope {
    revoked: bool,
    revision: u64,
    cells: HashMap<String, StateSnapshot>,
    cookies: Arc<crate::plugin::network::cookies::CookieJar>,
}

#[derive(Default)]
struct RegistryScopes {
    scopes: ScopeMap,
    watermarks: HashMap<(String, String), (i64, i64)>,
}

#[derive(Default)]
struct Registry {
    gate: Arc<RwLock<()>>,
    scopes: Mutex<RegistryScopes>,
}

/// Process-local state, shared by all Gateway instances for one installation.
#[derive(Clone)]
pub(crate) struct VolatileStore(Arc<Registry>);

pub(crate) struct VolatileReadGuard {
    registry: Arc<Registry>,
    _gate: OwnedRwLockReadGuard<()>,
}

pub(crate) struct VolatileLifecycleGuard {
    registry: Arc<Registry>,
    _gate: OwnedRwLockWriteGuard<()>,
}

#[derive(Clone)]
pub(crate) struct VolatileCell {
    scope: Arc<Mutex<Scope>>,
    key: String,
}

impl VolatileStore {
    pub(crate) fn for_installation(id: &str) -> Self {
        static REGISTRIES: LazyLock<Mutex<HashMap<String, Arc<Registry>>>> =
            LazyLock::new(Default::default);
        let mut registries = REGISTRIES.lock().expect("volatile registry poisoned");
        Self(registries.entry(id.to_owned()).or_default().clone())
    }

    pub(crate) async fn read(&self) -> VolatileReadGuard {
        VolatileReadGuard {
            registry: self.0.clone(),
            _gate: self.0.gate.clone().read_owned().await,
        }
    }

    pub(crate) async fn lock(&self) -> VolatileLifecycleGuard {
        VolatileLifecycleGuard {
            registry: self.0.clone(),
            _gate: self.0.gate.clone().write_owned().await,
        }
    }
}

impl VolatileReadGuard {
    fn scope(
        &self,
        vendor: &str,
        provider: &str,
        epoch: i64,
        fence: i64,
    ) -> Result<Arc<Mutex<Scope>>, StateError> {
        let mut registry = self
            .registry
            .scopes
            .lock()
            .expect("volatile registry poisoned");
        let provider_key = (vendor.to_owned(), provider.to_owned());
        let generation = (fence, epoch);
        if registry
            .watermarks
            .get(&provider_key)
            .is_some_and(|current| generation < *current)
        {
            return Err(StateError::new(
                "cancelled",
                "vendor operation is no longer current",
            ));
        }
        if registry
            .watermarks
            .get(&provider_key)
            .is_none_or(|current| generation > *current)
        {
            let obsolete: Vec<_> = registry
                .scopes
                .keys()
                .filter(|(v, p, _, _)| v == vendor && p == provider)
                .cloned()
                .collect();
            for key in obsolete {
                if let Some(scope) = registry.scopes.remove(&key) {
                    let mut scope = scope.lock().expect("volatile scope poisoned");
                    scope.revoked = true;
                    scope.cells.clear();
                    scope.cookies.clear();
                }
            }
            registry.watermarks.insert(provider_key, generation);
        }
        Ok(registry
            .scopes
            .entry((vendor.to_owned(), provider.to_owned(), epoch, fence))
            .or_default()
            .clone())
    }

    pub(crate) fn cookies(
        &self,
        vendor: &str,
        provider: &str,
        epoch: i64,
        fence: i64,
    ) -> Result<Arc<crate::plugin::network::cookies::CookieJar>, StateError> {
        let scope = self.scope(vendor, provider, epoch, fence)?;
        let cookies = scope
            .lock()
            .expect("volatile scope poisoned")
            .cookies
            .clone();
        Ok(cookies)
    }

    pub(crate) fn open(
        &self,
        vendor: &str,
        provider: &str,
        epoch: i64,
        fence: i64,
        key: String,
    ) -> Result<VolatileCell, StateError> {
        Ok(VolatileCell {
            scope: self.scope(vendor, provider, epoch, fence)?,
            key,
        })
    }
}

impl VolatileLifecycleGuard {
    async fn invalidate(&self, predicate: impl Fn(&ScopeKey) -> bool) {
        let removed = {
            let mut scopes = self
                .registry
                .scopes
                .lock()
                .expect("volatile registry poisoned");
            let keys: Vec<_> = scopes
                .scopes
                .keys()
                .filter(|key| predicate(key))
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|key| scopes.scopes.remove(&key))
                .collect::<Vec<_>>()
        };
        for scope in removed {
            let mut scope = scope.lock().expect("volatile scope poisoned");
            scope.revoked = true;
            scope.cells.clear();
            scope.cookies.clear();
        }
    }

    pub(crate) async fn invalidate_provider(&self, vendor: &str, provider: &str) {
        self.invalidate(|(v, p, _, _)| v == vendor && p == provider)
            .await;
    }

    pub(crate) async fn invalidate_provider_id(&self, provider: &str) {
        self.invalidate(|(_, p, _, _)| p == provider).await;
    }

    pub(crate) async fn invalidate_vendor(&self, vendor: &str) {
        self.invalidate(|(v, _, _, _)| v == vendor).await;
    }
}

impl VolatileCell {
    pub(crate) async fn load(&self) -> Result<StateSnapshot, StateError> {
        let scope = self.scope.lock().expect("volatile scope poisoned");
        if scope.revoked {
            return Err(StateError::new(
                "cancelled",
                "vendor operation is no longer current",
            ));
        }
        Ok(scope
            .cells
            .get(&self.key)
            .cloned()
            .unwrap_or(StateSnapshot {
                revision: 0,
                payload: None,
            }))
    }

    pub(crate) async fn compare_exchange(
        &self,
        expected: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, StateError> {
        if payload
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_PAYLOAD_BYTES)
        {
            return Err(StateError::new(
                "resource-exhausted",
                "atomic state limit exceeded",
            ));
        }
        let mut scope = self.scope.lock().expect("volatile scope poisoned");
        if scope.revoked {
            return Err(StateError::new(
                "cancelled",
                "vendor operation is no longer current",
            ));
        }
        let revision = scope.cells.get(&self.key).map_or(0, |cell| cell.revision);
        if revision != expected {
            return Ok(false);
        }
        if !scope.cells.contains_key(&self.key) && scope.cells.len() >= 16 {
            return Err(StateError::new(
                "resource-exhausted",
                "atomic state limit exceeded",
            ));
        }
        scope.revision = scope
            .revision
            .checked_add(1)
            .ok_or_else(|| StateError::new("storage", "atomic state storage failed"))?;
        let revision = scope.revision;
        scope
            .cells
            .insert(self.key.clone(), StateSnapshot { revision, payload });
        Ok(true)
    }
}
