use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use stravia_vendor_sdk::state::MAX_PAYLOAD_BYTES;

use tokio::sync::RwLock;

use crate::db::models::Provider;
use crate::provider_models::{ProviderModelMetadata, ProviderModelPresence, ProviderModelRecord};
use crate::storage::memory::MemoryOAuthCredentialStore;

#[path = "atomic_journal.rs"]
pub(crate) mod atomic_journal;
#[path = "atomic_journal_gc.rs"]
mod atomic_journal_gc;
#[cfg(test)]
#[path = "atomic_journal_tests.rs"]
mod atomic_journal_tests;

/// Storage-backed vendor plugin packages and their provider-scoped state.
#[derive(Clone)]
pub struct PluginStore {
    backend: PluginStoreBackend,
    config_projection: Option<Arc<crate::runtime_config::Projection>>,
    volatile_store: Arc<std::sync::RwLock<Option<super::volatile_state::VolatileStore>>>,
}

#[derive(Clone)]
enum PluginStoreBackend {
    Memory(MemoryPluginStore),
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

#[derive(Clone)]
struct MemoryPluginStore {
    providers: Arc<RwLock<Vec<Provider>>>,
    provider_models: Arc<RwLock<Vec<ProviderModelRecord>>>,
    oauth_credentials: Arc<MemoryOAuthCredentialStore>,
    data: Arc<RwLock<MemoryPluginData>>,
    history_pool: Arc<std::sync::RwLock<Option<sqlx::SqlitePool>>>,
    fallback_epoch: Arc<std::sync::atomic::AtomicI64>,
}

type AtomicLineageCells = HashMap<(String, String), BTreeSet<(String, String, String)>>;
type BoundaryFactCandidates =
    HashMap<(String, String, String, i64, String, uuid::Uuid), BTreeSet<String>>;
type AtomicLoadRow = (i64, Option<i64>, Option<Vec<u8>>, Option<String>);

#[derive(Default)]
struct MemoryPluginData {
    installed: BTreeMap<String, InstalledPlugin>,
    private_state: HashMap<String, MemoryPrivateState>,
    recovery: BTreeSet<(String, String)>,
    atomic_revision: i64,
    atomic_provider_fences: HashMap<String, i64>,
    atomic_cells: HashMap<(String, String, String, String, String), AtomicStateSnapshot>,
    atomic_lineage_cells: AtomicLineageCells,
    atomic_lineages: HashMap<(String, String), i64>,
    atomic_lineage_epochs: HashMap<(String, String), i64>,
    atomic_leases: HashMap<(String, String, String), i64>,
    atomic_checkpoints: HashMap<(String, String), MemoryAtomicCheckpoint>,
    journal_nodes: HashMap<String, atomic_journal::MemoryJournalNode>,
    journal_pins: HashMap<(String, String, String, String), u64>,
    boundary_facts: HashMap<String, atomic_journal::MemoryBoundaryFact>,
    boundary_fact_candidates: BoundaryFactCandidates,
    atomic_lineage_boundary_heads: HashMap<(String, String), String>,
}

struct MemoryAtomicCheckpoint {
    admission_id: String,
    lease_id: String,
    created_at_ms: i64,
    boundary_head: Option<String>,
    cells: Vec<((String, String, String), AtomicStateSnapshot)>,
}

pub(crate) struct AtomicStateKey<'a> {
    pub vendor_id: &'a str,
    pub provider_id: &'a str,
    pub data_epoch: i64,
    pub principal: Option<&'a str>,
    pub admission: Option<&'a str>,
    pub lease: Option<&'a str>,
    pub key: &'a str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AtomicStateSnapshot {
    pub revision: u64,
    pub payload: Option<Vec<u8>>,
    pub journal_head: Option<String>,
}

#[derive(Clone)]
struct MemoryPrivateState {
    vendor_id: String,
    format_version: String,
    payload: Vec<u8>,
}

#[derive(Clone, sqlx::FromRow)]
pub(crate) struct InstalledPlugin {
    pub vendor_id: String,
    pub version: String,
    pub source: String,
    pub descriptor: String,
    /// 仅在导入与内存存储中保留字节；SQL 只存摘要，随附包从程序内存读取，本地包从实例目录读取。
    #[sqlx(skip)]
    pub component: Bytes,
    pub digest: String,
    pub revision: i64,
    pub data_epoch: i64,
    pub installed_at: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct DataReset {
    pub provider_id: String,
    pub options: bool,
    pub credentials: bool,
    pub models: bool,
    pub private_state: bool,
}

pub(crate) struct ProviderReset {
    pub impact: DataReset,
    pub expected_vendor: String,
    /// 已按新声明分类；凭据重置不能删除原来混存的普通连接配置。
    pub retained_adapter_credentials: String,
    pub retained_options: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PluginStorageError {
    #[error("plugin or provider changed; review the update again")]
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

const ATOMIC_LOAD_SQL: &str = "WITH args AS (SELECT CAST($1 AS TEXT) AS provider_id, CAST($2 AS TEXT) AS vendor_id, CAST($3 AS TEXT) AS principal, CAST($4 AS TEXT) AS admission_id, CAST($5 AS TEXT) AS lease_id, CAST($6 AS TEXT) AS cell_key, CAST($7 AS BIGINT) AS now_ms, CAST($8 AS BIGINT) AS fence) SELECT v.data_epoch, c.revision, c.payload, c.journal_head FROM args a JOIN providers p ON p.id=a.provider_id AND p.vendor=a.vendor_id JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') LEFT JOIN vendor_atomic_cells c ON c.provider_id=a.provider_id AND c.vendor_id=a.vendor_id AND c.principal=a.principal AND c.admission_id=a.admission_id AND c.cell_key=a.cell_key WHERE (a.fence IS NULL OR EXISTS (SELECT 1 FROM vendor_atomic_provider_fences f WHERE f.provider_id=a.provider_id AND f.generation=a.fence)) AND (a.principal='' OR EXISTS (SELECT 1 FROM vendor_atomic_leases l WHERE l.principal=a.principal AND l.admission_id=a.admission_id AND l.lease_id=a.lease_id AND l.expires_at_ms>a.now_ms))";

fn atomic_load_result(
    row: Option<AtomicLoadRow>,
    data_epoch: i64,
) -> Result<(AtomicStateSnapshot, bool), PluginStorageError> {
    let (epoch, revision, payload, journal_head) = row.ok_or(PluginStorageError::StaleOperation)?;
    if epoch != data_epoch {
        return Err(PluginStorageError::StaleOperation);
    }
    Ok((
        AtomicStateSnapshot {
            revision: revision.unwrap_or(0) as u64,
            payload,
            journal_head,
        },
        false,
    ))
}

type BoundaryRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
);

const BOUNDARY_CANDIDATES_SQL: &str = "WITH RECURSIVE args AS (SELECT CAST($1 AS TEXT) AS vendor_id, CAST($2 AS TEXT) AS provider_id, CAST($3 AS TEXT) AS principal, CAST($4 AS TEXT) AS admission_id, CAST($5 AS TEXT) AS lease_id, CAST($6 AS BIGINT) AS now_ms), valid AS (SELECT g.boundary_head FROM args a JOIN providers p ON p.id=a.provider_id AND p.vendor=a.vendor_id JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') JOIN vendor_atomic_lineages g ON g.principal=a.principal AND g.admission_id=a.admission_id JOIN vendor_atomic_leases l ON l.principal=g.principal AND l.admission_id=g.admission_id AND l.lease_id=a.lease_id AND l.expires_at_ms>a.now_ms), chain AS (SELECT f.*, 0 AS depth FROM vendor_atomic_boundary_facts f JOIN valid v ON f.node_id=v.boundary_head JOIN args a ON f.principal=a.principal UNION ALL SELECT f.*, c.depth+1 FROM vendor_atomic_boundary_facts f JOIN chain c ON f.node_id=c.previous_id JOIN args a ON f.principal=a.principal) SELECT c.boundary_id,c.window_id,c.source_admission_id,c.source_item_gap,c.source_unit_gap,c.prefix_hash,c.controls_hash FROM valid v CROSS JOIN args a LEFT JOIN chain c ON c.vendor_id=a.vendor_id AND c.provider_id=a.provider_id AND c.provider_fence=(SELECT generation FROM vendor_atomic_provider_fences WHERE provider_id=a.provider_id) ORDER BY c.depth DESC";

fn decode_boundary_rows(
    rows: Vec<BoundaryRow>,
) -> Result<Vec<crate::generation_chain::HistoryBoundaryCandidate>, PluginStorageError> {
    if rows.is_empty() {
        return Err(PluginStorageError::StaleOperation);
    }
    rows.into_iter()
        .filter_map(|(id, window, admission, item, unit, prefix, controls)| {
            id.as_ref()?;
            Some((|| {
                Ok(crate::generation_chain::HistoryBoundaryCandidate {
                    id: uuid::Uuid::parse_str(id.as_deref().ok_or(PluginStorageError::Storage)?)
                        .map_err(|_| PluginStorageError::Storage)?,
                    window_id: uuid::Uuid::parse_str(
                        window.as_deref().ok_or(PluginStorageError::Storage)?,
                    )
                    .map_err(|_| PluginStorageError::Storage)?,
                    source_admission_id: uuid::Uuid::parse_str(
                        admission.as_deref().ok_or(PluginStorageError::Storage)?,
                    )
                    .map_err(|_| PluginStorageError::Storage)?,
                    source_item_gap: u64::try_from(item.ok_or(PluginStorageError::Storage)?)
                        .map_err(|_| PluginStorageError::Storage)?,
                    source_unit_gap: u64::try_from(unit.ok_or(PluginStorageError::Storage)?)
                        .map_err(|_| PluginStorageError::Storage)?,
                    source_prefix_hash: prefix
                        .ok_or(PluginStorageError::Storage)?
                        .try_into()
                        .map_err(|_| PluginStorageError::Storage)?,
                    source_controls_hash: controls
                        .ok_or(PluginStorageError::Storage)?
                        .try_into()
                        .map_err(|_| PluginStorageError::Storage)?,
                })
            })())
        })
        .collect()
}

fn storage_error(_: sqlx::Error) -> PluginStorageError {
    // 数据库错误可能包含秘密绑定值，不跨管理/插件接口传播原始错误。
    PluginStorageError::Storage
}

/// 仅这些字段由供应商插件发现流程拥有；规范、价格与人工选择均归 Core/用户所有。
pub(crate) fn has_plugin_model_metadata(metadata: &ProviderModelMetadata) -> bool {
    !metadata.extensions.is_empty()
        || metadata.provider.is_some()
        || metadata.experimental.is_some()
        || metadata.status.is_some()
}

fn clear_plugin_model_metadata(metadata: &mut ProviderModelMetadata) -> bool {
    if !has_plugin_model_metadata(metadata) {
        return false;
    }
    metadata.extensions.clear();
    metadata.provider = None;
    metadata.experimental = None;
    metadata.status = None;
    true
}

fn reset_memory_model(model: &mut ProviderModelRecord) -> bool {
    if !clear_plugin_model_metadata(&mut model.metadata) {
        return false;
    }
    if model.source_kind == crate::provider_models::ProviderModelSourceKind::Discovered {
        model.presence = ProviderModelPresence::Missing;
    }
    model.revision += 1;
    true
}

async fn install_memory(
    store: &MemoryPluginStore,
    next: &InstalledPlugin,
    expected_revision: Option<i64>,
    resets: &[ProviderReset],
) -> Result<(), PluginStorageError> {
    // All multi-store memory mutations take locks in this order. Provider deletion
    // follows the same order before it reaches plugin data.
    let mut providers = store.providers.write().await;
    let mut provider_models = store.provider_models.write().await;
    let mut oauth_credentials = store.oauth_credentials.credentials.write().await;
    let mut data = store.data.write().await;

    let revision_matches = match expected_revision {
        Some(expected) => data
            .installed
            .get(&next.vendor_id)
            .is_some_and(|installed| installed.revision == expected),
        None => !data.installed.contains_key(&next.vendor_id),
    };
    if !revision_matches {
        return Err(PluginStorageError::Changed);
    }
    if resets.iter().any(|reset| {
        providers
            .iter()
            .find(|provider| provider.id == reset.impact.provider_id)
            .and_then(|provider| provider.vendor.as_deref())
            != Some(reset.expected_vendor.as_str())
    }) {
        return Err(PluginStorageError::Changed);
    }

    for reset in resets {
        let impact = &reset.impact;
        let provider = providers
            .iter_mut()
            .find(|provider| provider.id == impact.provider_id)
            .expect("provider ownership was validated before memory plugin reset");
        if impact.options {
            provider.vendor_options.clone_from(&reset.retained_options);
        }
        if impact.options || impact.credentials {
            provider
                .adapter_credentials
                .clone_from(&reset.retained_adapter_credentials);
        }
        if impact.credentials {
            provider.api_key.clear();
            oauth_credentials.remove(&impact.provider_id);
        }
        if impact.models {
            for model in provider_models
                .iter_mut()
                .filter(|model| model.provider_id == impact.provider_id)
            {
                reset_memory_model(model);
            }
        }
        if impact.private_state
            && data
                .private_state
                .get(&impact.provider_id)
                .is_some_and(|state| state.vendor_id == reset.expected_vendor)
        {
            data.private_state.remove(&impact.provider_id);
        }
        if impact.private_state {
            data.atomic_provider_fences.remove(&impact.provider_id);
            let revoked_nodes: BTreeSet<_> = data
                .journal_nodes
                .iter()
                .filter(|(_, node)| {
                    node.vendor_id == reset.expected_vendor
                        && node.provider_id == impact.provider_id
                })
                .map(|(id, _)| id.clone())
                .collect();
            data.journal_pins
                .retain(|(_, _, _, node), _| !revoked_nodes.contains(node));
            data.atomic_cells.retain(|(vendor, provider, _, _, _), _| {
                vendor != &reset.expected_vendor || provider != &impact.provider_id
            });
            data.atomic_lineage_cells.retain(|_, cells| {
                cells.retain(|(vendor, provider, _)| {
                    vendor != &reset.expected_vendor || provider != &impact.provider_id
                });
                !cells.is_empty()
            });
            for cells in data.atomic_checkpoints.values_mut() {
                cells.cells.retain(|((vendor, provider, _), _)| {
                    vendor != &reset.expected_vendor || provider != &impact.provider_id
                });
            }
        }
        for (kind, reset_kind) in [
            ("options", impact.options),
            ("credentials", impact.credentials),
            ("models", impact.models),
        ] {
            if reset_kind {
                data.recovery
                    .insert((impact.provider_id.clone(), kind.to_string()));
            }
        }
    }
    data.installed.insert(next.vendor_id.clone(), next.clone());
    Ok(())
}

async fn validate_memory_atomic_mirror(
    store: &MemoryPluginStore,
    principal: &str,
    admission: &str,
    lease: &str,
) -> Result<(), PluginStorageError> {
    let pool = store
        .history_pool
        .read()
        .expect("history pool lock poisoned")
        .clone();
    if let Some(pool) = pool {
        let live: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vendor_atomic_lineages g JOIN vendor_atomic_leases l ON l.principal=g.principal AND l.admission_id=g.admission_id WHERE g.principal=$1 AND g.admission_id=$2 AND l.lease_id=$3 AND l.expires_at_ms>$4")
            .bind(principal).bind(admission).bind(lease).bind(chrono::Utc::now().timestamp_millis())
            .fetch_optional(&pool).await.map_err(storage_error)?;
        if live.is_none() {
            return Err(PluginStorageError::StaleOperation);
        }
    }
    Ok(())
}

struct MemoryAtomicMirror<'a> {
    principal: &'a str,
    admission: &'a str,
    lease: &'a str,
    expiry: i64,
    created_at: i64,
    history_epoch: i64,
    checkpoint: Option<&'a str>,
    parent: Option<(&'a str, &'a str, &'a str)>,
    fresh: bool,
    create_lineage: bool,
}

async fn mirror_memory_atomic(
    store: &MemoryPluginStore,
    operation: &MemoryAtomicMirror<'_>,
) -> Result<(), PluginStorageError> {
    let MemoryAtomicMirror {
        principal,
        admission,
        lease,
        expiry,
        created_at,
        history_epoch,
        checkpoint,
        parent,
        fresh,
        create_lineage,
    } = *operation;
    let pool = store
        .history_pool
        .read()
        .expect("history pool lock poisoned")
        .clone();
    let Some(pool) = pool else {
        return Ok(());
    };
    let mut connection = pool.acquire_write().await.map_err(storage_error)?;
    let mut tx = connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(storage_error)?;
    sqlx::query("UPDATE vendor_atomic_revision SET revision=revision WHERE id=1")
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
    if let Some((parent, parent_admission, parent_lease)) = parent {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vendor_atomic_checkpoints WHERE principal=$1 AND checkpoint_id=$2 AND admission_id=$3 AND lease_id=$4").bind(principal).bind(parent).bind(parent_admission).bind(parent_lease).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        if exists.is_none() {
            return Err(PluginStorageError::StaleOperation);
        }
    }
    if !create_lineage {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2 AND history_epoch=$3")
            .bind(principal).bind(admission).bind(history_epoch).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        if exists.is_none() {
            return Err(PluginStorageError::StaleOperation);
        }
    }
    if !fresh {
        let live: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vendor_atomic_leases WHERE principal=$1 AND admission_id=$2 AND lease_id=$3 AND expires_at_ms>$4")
            .bind(principal).bind(admission).bind(lease).bind(chrono::Utc::now().timestamp_millis()).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        if live.is_none() {
            return Err(PluginStorageError::StaleOperation);
        }
    }
    if create_lineage {
        let inserted = sqlx::query("INSERT INTO vendor_atomic_lineages(principal,admission_id,created_at_ms,history_epoch) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
            .bind(principal).bind(admission).bind(created_at).bind(history_epoch).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
        if inserted != 1 {
            return Err(PluginStorageError::StaleOperation);
        }
    }
    sqlx::query("INSERT INTO vendor_atomic_leases(principal,admission_id,lease_id,expires_at_ms) VALUES($1,$2,$3,$4) ON CONFLICT(principal,admission_id,lease_id) DO UPDATE SET expires_at_ms=excluded.expires_at_ms")
        .bind(principal).bind(admission).bind(lease).bind(expiry).execute(&mut *tx).await.map_err(storage_error)?;
    if let Some(checkpoint) = checkpoint {
        sqlx::query("INSERT INTO vendor_atomic_checkpoints(principal,checkpoint_id,admission_id,lease_id,created_at_ms) VALUES($1,$2,$3,$4,$5)")
            .bind(principal).bind(checkpoint).bind(admission).bind(lease).bind(chrono::Utc::now().timestamp_millis()).execute(&mut *tx).await.map_err(storage_error)?;
    }
    tx.commit().await.map_err(storage_error)?;
    connection.finish().await.map_err(storage_error)?;
    Ok(())
}

impl PluginStore {
    pub(crate) fn attach_volatile_store(&self, store: super::volatile_state::VolatileStore) {
        *self
            .volatile_store
            .write()
            .expect("volatile store lock poisoned") = Some(store);
    }

    pub(crate) fn volatile_store(&self) -> Option<super::volatile_state::VolatileStore> {
        self.volatile_store
            .read()
            .expect("volatile store lock poisoned")
            .clone()
    }

    pub(crate) fn with_config_projection(
        &self,
        projection: Arc<crate::runtime_config::Projection>,
    ) -> Self {
        Self {
            backend: self.backend.clone(),
            config_projection: Some(projection),
            volatile_store: self.volatile_store.clone(),
        }
    }

    pub(crate) fn is_persistent(&self) -> bool {
        !matches!(self.backend, PluginStoreBackend::Memory(_))
    }

    pub(crate) fn sqlite_pool(&self) -> Option<&sqlx::SqlitePool> {
        match &self.backend {
            PluginStoreBackend::Sqlite(pool) => Some(pool),
            _ => None,
        }
    }

    pub(crate) fn postgres_pool(&self) -> Option<&sqlx::PgPool> {
        match &self.backend {
            PluginStoreBackend::Postgres(pool) => Some(pool),
            _ => None,
        }
    }

    pub(crate) fn memory(
        providers: Arc<RwLock<Vec<Provider>>>,
        provider_models: Arc<RwLock<Vec<ProviderModelRecord>>>,
        oauth_credentials: Arc<MemoryOAuthCredentialStore>,
        fallback_epoch: Arc<std::sync::atomic::AtomicI64>,
    ) -> Self {
        Self {
            backend: PluginStoreBackend::Memory(MemoryPluginStore {
                providers,
                provider_models,
                oauth_credentials,
                data: Arc::new(RwLock::new(MemoryPluginData::default())),
                history_pool: Arc::new(std::sync::RwLock::new(None)),
                fallback_epoch,
            }),
            config_projection: None,
            volatile_store: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    pub(crate) async fn remove_memory_provider_data(&self, provider_id: &str) {
        let PluginStoreBackend::Memory(store) = &self.backend else {
            unreachable!("memory provider cleanup requires the memory plugin backend");
        };
        let mut data = store.data.write().await;
        data.private_state.remove(provider_id);
        data.atomic_provider_fences.remove(provider_id);
        data.atomic_cells
            .retain(|(_, provider, _, _, _), _| provider != provider_id);
        data.atomic_lineage_cells.retain(|_, cells| {
            cells.retain(|(_, provider, _)| provider != provider_id);
            !cells.is_empty()
        });
        for cells in data.atomic_checkpoints.values_mut() {
            cells
                .cells
                .retain(|((_, provider, _), _)| provider != provider_id);
        }
        data.recovery
            .retain(|(candidate, _)| candidate != provider_id);
    }

    pub(crate) fn has_history_pool(&self) -> bool {
        match &self.backend {
            PluginStoreBackend::Memory(store) => store
                .history_pool
                .read()
                .expect("history pool lock poisoned")
                .is_some(),
            _ => false,
        }
    }

    pub(crate) async fn history_epoch(&self) -> Result<i64, PluginStorageError> {
        let PluginStoreBackend::Memory(store) = &self.backend else {
            return Err(PluginStorageError::Storage);
        };
        let pool = store
            .history_pool
            .read()
            .expect("history pool lock poisoned")
            .clone();
        match pool {
            Some(pool) => sqlx::query_scalar("SELECT epoch FROM prompt_admission_epoch WHERE id=1")
                .fetch_one(&pool)
                .await
                .map_err(storage_error),
            None => Ok(store
                .fallback_epoch
                .load(std::sync::atomic::Ordering::Acquire)),
        }
    }

    pub(crate) async fn attach_atomic_history_pool(
        &self,
        pool: sqlx::SqlitePool,
        admissions: &dyn crate::storage::PromptAdmissionStore,
    ) -> Result<(), PluginStorageError> {
        let PluginStoreBackend::Memory(store) = &self.backend else {
            return Err(PluginStorageError::Storage);
        };
        let history_lock = admissions
            .memory_history_lock()
            .ok_or(PluginStorageError::Storage)?;
        let _history_guard = history_lock.write().await;
        if store
            .history_pool
            .read()
            .expect("history pool lock poisoned")
            .is_some()
        {
            return Err(PluginStorageError::Changed);
        }
        admissions
            .clear()
            .await
            .map_err(|_| PluginStorageError::Storage)?;
        let mut data = store.data.write().await;
        let fallback = store
            .fallback_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let mut connection = pool.acquire_write().await.map_err(storage_error)?;
        let mut tx = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage_error)?;
        let current: i64 =
            sqlx::query_scalar("SELECT epoch FROM prompt_admission_epoch WHERE id=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
        let next = current
            .max(fallback)
            .checked_add(1)
            .ok_or(PluginStorageError::Storage)?;
        sqlx::query("UPDATE prompt_admission_epoch SET epoch=$1 WHERE id=1")
            .bind(next)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        sqlx::query("DELETE FROM vendor_atomic_leases")
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        data.atomic_leases.clear();
        tx.commit().await.map_err(storage_error)?;
        drop(connection);
        *store
            .history_pool
            .write()
            .expect("history pool lock poisoned") = Some(pool);
        Ok(())
    }

    pub(crate) fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self {
            backend: PluginStoreBackend::Sqlite(pool),
            config_projection: None,
            volatile_store: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    pub(crate) fn postgres(pool: sqlx::PgPool) -> Self {
        Self {
            backend: PluginStoreBackend::Postgres(pool),
            config_projection: None,
            volatile_store: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    pub(crate) async fn list(&self) -> Result<Vec<InstalledPlugin>, PluginStorageError> {
        const SQL: &str = "SELECT vendor_id, version, source, descriptor, digest, revision, data_epoch, installed_at FROM vendor_plugins ORDER BY vendor_id";
        match &self.backend {
            PluginStoreBackend::Memory(store) => Ok(store
                .data
                .read()
                .await
                .installed
                .values()
                .cloned()
                .collect()),
            PluginStoreBackend::Sqlite(pool) => sqlx::query_as(SQL)
                .fetch_all(pool)
                .await
                .map_err(storage_error),
            PluginStoreBackend::Postgres(pool) => sqlx::query_as(SQL)
                .fetch_all(pool)
                .await
                .map_err(storage_error),
        }
    }

    pub(crate) async fn get(
        &self,
        vendor_id: &str,
    ) -> Result<Option<InstalledPlugin>, PluginStorageError> {
        const SQL: &str = "SELECT vendor_id, version, source, descriptor, digest, revision, data_epoch, installed_at FROM vendor_plugins WHERE vendor_id = $1";
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                Ok(store.data.read().await.installed.get(vendor_id).cloned())
            }
            PluginStoreBackend::Sqlite(pool) => sqlx::query_as(SQL)
                .bind(vendor_id)
                .fetch_optional(pool)
                .await
                .map_err(storage_error),
            PluginStoreBackend::Postgres(pool) => sqlx::query_as(SQL)
                .bind(vendor_id)
                .fetch_optional(pool)
                .await
                .map_err(storage_error),
        }
    }

    /// 仅删除安装元数据；连接、凭据、模型、恢复标记与插件私有状态均留待重装兼容性检查。
    pub(crate) async fn uninstall(
        &self,
        vendor_id: &str,
        expected_revision: i64,
    ) -> Result<(), PluginStorageError> {
        let volatile = self.volatile_store();
        let guard = match &volatile {
            Some(store) => Some(store.lock().await),
            None => None,
        };
        let result = self.uninstall_inner(vendor_id, expected_revision).await;
        if result.is_ok() {
            if let Some(guard) = guard {
                guard.invalidate_vendor(vendor_id).await;
            }
            if self.sweep_atomic_journal().await.is_err() {
                tracing::warn!("atomic journal cleanup deferred");
            }
        }
        result
    }

    async fn uninstall_inner(
        &self,
        vendor_id: &str,
        expected_revision: i64,
    ) -> Result<(), PluginStorageError> {
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                if !data
                    .installed
                    .get(vendor_id)
                    .is_some_and(|installed| installed.revision == expected_revision)
                {
                    return Err(PluginStorageError::Changed);
                }
                data.installed.remove(vendor_id);
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => {
                let mut connection = pool.acquire_write().await.map_err(storage_error)?;
                let changed =
                    sqlx::query("DELETE FROM vendor_plugins WHERE vendor_id=$1 AND revision=$2")
                        .bind(vendor_id)
                        .bind(expected_revision)
                        .execute(&mut *connection)
                        .await
                        .map_err(storage_error)?
                        .rows_affected();
                connection.finish().await.map_err(storage_error)?;
                if changed != 1 {
                    return Err(PluginStorageError::Changed);
                }
                Ok(())
            }
            PluginStoreBackend::Postgres(pool) => {
                let mut tx = pool.begin().await.map_err(storage_error)?;
                sqlx::query("SELECT pg_advisory_xact_lock(7342179085017261)")
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                let changed =
                    sqlx::query("DELETE FROM vendor_plugins WHERE vendor_id=$1 AND revision=$2")
                        .bind(vendor_id)
                        .bind(expected_revision)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage_error)?
                        .rows_affected();
                if changed != 1 {
                    return Err(PluginStorageError::Changed);
                }
                tx.commit().await.map_err(storage_error)?;
                Ok(())
            }
        }
    }

    /// 包替换与预览确定的数据重置原子提交；调用者先持有供应商静默凭证。
    pub(crate) async fn install(
        &self,
        next: &InstalledPlugin,
        expected_revision: Option<i64>,
        resets: &[ProviderReset],
    ) -> Result<(), PluginStorageError> {
        let volatile = self.volatile_store();
        let guard = match &volatile {
            Some(store) => Some(store.lock().await),
            None => None,
        };
        let previous_epoch = if guard.is_some() {
            self.get(&next.vendor_id)
                .await?
                .map(|plugin| plugin.data_epoch)
        } else {
            None
        };
        let mutation = self
            .config_projection
            .as_ref()
            .map(|projection| projection.begin());
        let result = self.install_inner(next, expected_revision, resets).await;
        if result.is_ok()
            && let Some(guard) = &guard
        {
            if previous_epoch != Some(next.data_epoch) {
                guard.invalidate_vendor(&next.vendor_id).await;
            }
            for reset in resets.iter().filter(|reset| reset.impact.private_state) {
                guard
                    .invalidate_provider(&reset.expected_vendor, &reset.impact.provider_id)
                    .await;
            }
        }
        if let Some(mutation) = mutation {
            mutation.finish();
        }
        drop(guard);
        if result.is_ok()
            && resets.iter().any(|reset| reset.impact.private_state)
            && self.sweep_atomic_journal().await.is_err()
        {
            tracing::warn!("atomic journal cleanup deferred");
        }
        result
    }

    async fn install_inner(
        &self,
        next: &InstalledPlugin,
        expected_revision: Option<i64>,
        resets: &[ProviderReset],
    ) -> Result<(), PluginStorageError> {
        macro_rules! install {
    ($pool:expr, $begin:literal, $lock:literal) => {{
        let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
        let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
        sqlx::query($lock).execute(&mut *tx).await.map_err(storage_error)?;
        let changed = if let Some(expected) = expected_revision {
            sqlx::query(
                "UPDATE vendor_plugins SET version=$2, source=$3, digest=$4, revision=$5, data_epoch=$6, installed_at=$7, descriptor=$9 WHERE vendor_id=$1 AND revision=$8",
            )
            .bind(&next.vendor_id)
            .bind(&next.version)
            .bind(&next.source)
            .bind(&next.digest)
            .bind(next.revision)
            .bind(next.data_epoch)
            .bind(next.installed_at)
            .bind(expected)
            .bind(&next.descriptor)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?
            .rows_affected()
        } else {
            sqlx::query(
                "INSERT INTO vendor_plugins (vendor_id,version,source,digest,revision,data_epoch,installed_at,descriptor) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(vendor_id) DO NOTHING",
            )
            .bind(&next.vendor_id)
            .bind(&next.version)
            .bind(&next.source)
            .bind(&next.digest)
            .bind(next.revision)
            .bind(next.data_epoch)
            .bind(next.installed_at)
            .bind(&next.descriptor)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?
            .rows_affected()
        };
        if changed != 1 {
            return Err(PluginStorageError::Changed);
        }
        for reset in resets {
            let impact = &reset.impact;
            let owner: Option<String> =
                sqlx::query_scalar("SELECT vendor FROM providers WHERE id=$1")
                    .bind(&impact.provider_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage_error)?
                    .flatten();
            if owner.as_deref() != Some(reset.expected_vendor.as_str()) {
                return Err(PluginStorageError::Changed);
            }
            if impact.options {
                sqlx::query("UPDATE providers SET vendor_options=$2 WHERE id=$1")
                    .bind(&impact.provider_id)
                    .bind(&reset.retained_options)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            if impact.options || impact.credentials {
                sqlx::query("UPDATE providers SET adapter_credentials=$2 WHERE id=$1")
                    .bind(&impact.provider_id)
                    .bind(&reset.retained_adapter_credentials)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            if impact.credentials {
                sqlx::query("UPDATE providers SET api_key='' WHERE id=$1")
                    .bind(&impact.provider_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                sqlx::query("DELETE FROM provider_oauth_credentials WHERE provider_id=$1")
                    .bind(&impact.provider_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            if impact.models {
                let models: Vec<(
                    String,
                    String,
                    String,
                    sqlx::types::Json<ProviderModelMetadata>,
                )> = sqlx::query_as(
                    "SELECT model_id, source_kind, presence, metadata_json FROM provider_models WHERE provider_id=$1",
                )
                    .bind(&impact.provider_id)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                for (model_id, source_kind, presence, mut metadata) in models {
                    if !clear_plugin_model_metadata(&mut metadata.0) {
                        continue;
                    }
                    let presence = if source_kind == "discovered" {
                        "missing"
                    } else {
                        presence.as_str()
                    };
                    sqlx::query(
                        "UPDATE provider_models SET metadata_json=$3, lifecycle_status=NULL, presence=$4, revision=revision+1 WHERE provider_id=$1 AND model_id=$2",
                    )
                    .bind(&impact.provider_id)
                    .bind(model_id)
                    .bind(metadata)
                    .bind(presence)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                }
            }
            if impact.private_state {
                sqlx::query("DELETE FROM vendor_atomic_provider_fences WHERE provider_id=$1").bind(&impact.provider_id).execute(&mut *tx).await.map_err(storage_error)?;
                sqlx::query("DELETE FROM vendor_atomic_journal_pins WHERE node_id IN (SELECT node_id FROM vendor_atomic_journal_nodes WHERE vendor_id=$1 AND provider_id=$2)")
                    .bind(&reset.expected_vendor).bind(&impact.provider_id).execute(&mut *tx).await.map_err(storage_error)?;
                for sql in ["DELETE FROM vendor_atomic_cells WHERE provider_id=$1 AND vendor_id=$2", "DELETE FROM vendor_atomic_checkpoint_cells WHERE provider_id=$1 AND vendor_id=$2"] {
                    sqlx::query(sql).bind(&impact.provider_id).bind(&reset.expected_vendor).execute(&mut *tx).await.map_err(storage_error)?;
                }
                sqlx::query("DELETE FROM vendor_private_state WHERE provider_id=$1 AND vendor_id=$2")
                    .bind(&impact.provider_id)
                    .bind(&reset.expected_vendor)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            for (kind, reset) in [
                ("options", impact.options),
                ("credentials", impact.credentials),
                ("models", impact.models),
            ] {
                if reset {
                    sqlx::query("INSERT INTO vendor_data_recovery (provider_id,data_kind) VALUES ($1,$2) ON CONFLICT(provider_id,data_kind) DO NOTHING")
                        .bind(&impact.provider_id)
                        .bind(kind)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage_error)?;
                }
            }
        }
        tx.commit().await.map_err(storage_error)?;
        connection.finish().await.map_err(storage_error)?;
        Ok(())
    }};
            }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                install_memory(store, next, expected_revision, resets).await
            }
            PluginStoreBackend::Sqlite(pool) => install!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1"
            ),
            PluginStoreBackend::Postgres(pool) => install!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock(7342179085017261)"
            ),
        }
    }

    pub(crate) async fn open_atomic_provider_fence(
        &self,
        vendor: &str,
        provider: &str,
        epoch: i64,
    ) -> Result<i64, PluginStorageError> {
        const EXISTING: &str = "SELECT v.data_epoch, f.generation FROM providers p JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') LEFT JOIN vendor_atomic_provider_fences f ON f.provider_id=p.id WHERE p.id=$1 AND p.vendor=$2";
        macro_rules! open {
            ($pool:expr, $begin:literal, $gate:literal, $revision:literal, $pg:expr) => {{
                // The snapshot validates owner, epoch and fence together. A later reset
                // revokes this generation when the caller actually uses it.
                let existing: Option<(i64, Option<i64>)> = sqlx::query_as(EXISTING)
                    .bind(provider).bind(vendor).fetch_optional($pool).await.map_err(storage_error)?;
                match existing {
                    Some((current, Some(fence))) if current == epoch => return Ok(fence),
                    Some((current, None)) if current == epoch => {},
                    _ => return Err(PluginStorageError::StaleOperation),
                }
                // Serialize creation with reset/delete and recheck after taking locks.
                let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
                sqlx::query($gate).execute(&mut *tx).await.map_err(storage_error)?;
                if $pg {
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('provider:' || $1),hashtext($2))").bind(vendor).bind(provider).execute(&mut *tx).await.map_err(storage_error)?;
                }
                let valid: Option<i64> = sqlx::query_scalar("SELECT v.data_epoch FROM providers p JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') WHERE p.id=$1 AND p.vendor=$2")
                    .bind(provider).bind(vendor).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                if valid != Some(epoch) { return Err(PluginStorageError::StaleOperation); }
                let existing: Option<i64> = sqlx::query_scalar("SELECT generation FROM vendor_atomic_provider_fences WHERE provider_id=$1").bind(provider).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                let fence = if let Some(fence) = existing { fence } else {
                    let fence: i64 = sqlx::query_scalar($revision).fetch_one(&mut *tx).await.map_err(storage_error)?;
                    sqlx::query("INSERT INTO vendor_atomic_provider_fences(provider_id,generation) VALUES($1,$2)").bind(provider).bind(fence).execute(&mut *tx).await.map_err(storage_error)?;
                    fence
                };
                tx.commit().await.map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)?;
                Ok(fence)
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                let owner = providers
                    .iter()
                    .any(|p| p.id == provider && p.vendor.as_deref() == Some(vendor));
                if !owner {
                    return Err(PluginStorageError::StaleOperation);
                }
                {
                    let data = store.data.read().await;
                    let valid = data
                        .installed
                        .get(vendor)
                        .or_else(|| data.installed.get("base"))
                        .is_some_and(|v| v.data_epoch == epoch);
                    if !valid {
                        return Err(PluginStorageError::StaleOperation);
                    }
                    if let Some(fence) = data.atomic_provider_fences.get(provider) {
                        return Ok(*fence);
                    }
                }
                let mut data = store.data.write().await;
                let valid = data
                    .installed
                    .get(vendor)
                    .or_else(|| data.installed.get("base"))
                    .is_some_and(|v| v.data_epoch == epoch);
                if !valid {
                    return Err(PluginStorageError::StaleOperation);
                }
                if let Some(fence) = data.atomic_provider_fences.get(provider) {
                    return Ok(*fence);
                }
                data.atomic_revision = data
                    .atomic_revision
                    .checked_add(1)
                    .ok_or(PluginStorageError::Storage)?;
                let fence = data.atomic_revision;
                data.atomic_provider_fences
                    .insert(provider.to_owned(), fence);
                Ok(fence)
            }
            PluginStoreBackend::Sqlite(pool) => open!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                "UPDATE vendor_atomic_revision SET revision=revision+1 WHERE id=1 RETURNING revision",
                false
            ),
            PluginStoreBackend::Postgres(pool) => open!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock_shared(7342179085017261)",
                "SELECT nextval('vendor_atomic_revision_seq')",
                true
            ),
        }
    }

    pub(crate) async fn validate_atomic_provider_fence(
        &self,
        vendor: &str,
        provider: &str,
        epoch: i64,
        fence: i64,
    ) -> Result<(), PluginStorageError> {
        const SQL: &str = "SELECT v.data_epoch FROM providers p JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') JOIN vendor_atomic_provider_fences f ON f.provider_id=p.id WHERE p.id=$1 AND p.vendor=$2 AND f.generation=$3";
        let valid = match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                let data = store.data.read().await;
                providers
                    .iter()
                    .any(|p| p.id == provider && p.vendor.as_deref() == Some(vendor))
                    && data
                        .installed
                        .get(vendor)
                        .or_else(|| data.installed.get("base"))
                        .is_some_and(|v| v.data_epoch == epoch)
                    && data.atomic_provider_fences.get(provider) == Some(&fence)
            }
            PluginStoreBackend::Sqlite(pool) => {
                sqlx::query_scalar::<_, i64>(SQL)
                    .bind(provider)
                    .bind(vendor)
                    .bind(fence)
                    .fetch_optional(pool)
                    .await
                    .map_err(storage_error)?
                    == Some(epoch)
            }
            PluginStoreBackend::Postgres(pool) => {
                sqlx::query_scalar::<_, i64>(SQL)
                    .bind(provider)
                    .bind(vendor)
                    .bind(fence)
                    .fetch_optional(pool)
                    .await
                    .map_err(storage_error)?
                    == Some(epoch)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(PluginStorageError::StaleOperation)
        }
    }

    pub(crate) async fn history_boundary_candidates(
        &self,
        vendor: &str,
        provider: &str,
        principal: &str,
        admission: &str,
        lease: &str,
    ) -> Result<Vec<crate::generation_chain::HistoryBoundaryCandidate>, PluginStorageError> {
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                let data = store.data.read().await;
                if !providers
                    .iter()
                    .any(|p| p.id == provider && p.vendor.as_deref() == Some(vendor))
                    || data
                        .installed
                        .get(vendor)
                        .or_else(|| data.installed.get("base"))
                        .is_none()
                    || !data
                        .atomic_leases
                        .get(&(principal.to_owned(), admission.to_owned(), lease.to_owned()))
                        .is_some_and(|expiry| *expiry > chrono::Utc::now().timestamp_millis())
                {
                    return Err(PluginStorageError::StaleOperation);
                }
                validate_memory_atomic_mirror(store, principal, admission, lease).await?;
                let mut cursor = data
                    .atomic_lineage_boundary_heads
                    .get(&(principal.to_owned(), admission.to_owned()))
                    .map(String::as_str);
                let mut candidates = Vec::new();
                while let Some(id) = cursor {
                    let fact = data
                        .boundary_facts
                        .get(id)
                        .ok_or(PluginStorageError::Storage)?;
                    if fact.principal != principal {
                        return Err(PluginStorageError::Storage);
                    }
                    if fact.vendor_id == vendor
                        && fact.provider_id == provider
                        && data.atomic_provider_fences.get(provider) == Some(&fact.provider_fence)
                    {
                        candidates.push(fact.candidate.clone());
                    }
                    cursor = fact.previous.as_deref();
                }
                candidates.reverse();
                Ok(candidates)
            }
            PluginStoreBackend::Sqlite(pool) => {
                let rows: Vec<BoundaryRow> = sqlx::query_as(BOUNDARY_CANDIDATES_SQL)
                    .bind(vendor)
                    .bind(provider)
                    .bind(principal)
                    .bind(admission)
                    .bind(lease)
                    .bind(chrono::Utc::now().timestamp_millis())
                    .fetch_all(pool)
                    .await
                    .map_err(storage_error)?;
                decode_boundary_rows(rows)
            }
            PluginStoreBackend::Postgres(pool) => {
                let rows: Vec<BoundaryRow> = sqlx::query_as(BOUNDARY_CANDIDATES_SQL)
                    .bind(vendor)
                    .bind(provider)
                    .bind(principal)
                    .bind(admission)
                    .bind(lease)
                    .bind(chrono::Utc::now().timestamp_millis())
                    .fetch_all(pool)
                    .await
                    .map_err(storage_error)?;
                decode_boundary_rows(rows)
            }
        }
    }

    pub(crate) async fn has_atomic_state(
        &self,
        vendor_id: &str,
        provider_id: &str,
    ) -> Result<bool, PluginStorageError> {
        const SQL: &str = "SELECT EXISTS(SELECT CAST(1 AS BIGINT) FROM vendor_atomic_cells WHERE vendor_id=$1 AND provider_id=$2 UNION ALL SELECT CAST(1 AS BIGINT) FROM vendor_atomic_checkpoint_cells WHERE vendor_id=$1 AND provider_id=$2)";
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let data = store.data.read().await;
                Ok(data
                    .atomic_cells
                    .keys()
                    .any(|(v, p, _, _, _)| v == vendor_id && p == provider_id)
                    || data.atomic_checkpoints.values().any(|checkpoint| {
                        checkpoint
                            .cells
                            .iter()
                            .any(|((v, p, _), _)| v == vendor_id && p == provider_id)
                    }))
            }
            PluginStoreBackend::Sqlite(pool) => sqlx::query_scalar(SQL)
                .bind(vendor_id)
                .bind(provider_id)
                .fetch_one(pool)
                .await
                .map_err(storage_error),
            PluginStoreBackend::Postgres(pool) => sqlx::query_scalar(SQL)
                .bind(vendor_id)
                .bind(provider_id)
                .fetch_one(pool)
                .await
                .map_err(storage_error),
        }
    }

    #[cfg(test)]
    pub(crate) async fn acquire_atomic_state_lease(
        &self,
        principal: &str,
        admission: &str,
        lease: &str,
    ) -> Result<(), PluginStorageError> {
        self.atomic_state_lease(principal, admission, lease, false, None)
            .await
    }

    pub(crate) async fn acquire_atomic_state_lease_at_epoch(
        &self,
        principal: &str,
        admission: &str,
        lease: &str,
        epoch: i64,
    ) -> Result<(), PluginStorageError> {
        self.atomic_state_lease(principal, admission, lease, false, Some(epoch))
            .await
    }

    pub(crate) async fn renew_atomic_state_lease(
        &self,
        principal: &str,
        admission: &str,
        lease: &str,
    ) -> Result<(), PluginStorageError> {
        self.atomic_state_lease(principal, admission, lease, true, None)
            .await
    }

    async fn atomic_state_lease(
        &self,
        principal: &str,
        admission: &str,
        lease: &str,
        renew: bool,
        expected_epoch: Option<i64>,
    ) -> Result<(), PluginStorageError> {
        macro_rules! lease_sql {
            ($pool:expr, $begin:literal, $lock:literal, $pg:expr) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
                sqlx::query($lock).execute(&mut *tx).await.map_err(storage_error)?;
                if let Some(expected) = expected_epoch {
                    let epoch_sql = if $pg { "SELECT epoch FROM prompt_admission_epoch WHERE id=1 FOR SHARE" } else { "SELECT epoch FROM prompt_admission_epoch WHERE id=1" };
                    let current: i64 = sqlx::query_scalar(epoch_sql).fetch_one(&mut *tx).await.map_err(storage_error)?;
                    if current != expected { return Err(PluginStorageError::StaleOperation); }
                }
                if $pg {
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('lineage:' || $1),hashtext($2))").bind(principal).bind(admission).execute(&mut *tx).await.map_err(storage_error)?;
                }
                let now = chrono::Utc::now().timestamp_millis();
                if !renew {
                    let exists: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2").bind(principal).bind(admission).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                    if exists.is_none() { return Err(PluginStorageError::MissingLineage); }
                    if let Some(expected) = expected_epoch {
                        let lineage_epoch: Option<i64> = sqlx::query_scalar("SELECT history_epoch FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2").bind(principal).bind(admission).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                        if lineage_epoch != Some(expected) { return Err(PluginStorageError::StaleOperation); }
                    }
                }
                let changed = if renew {
                    sqlx::query("UPDATE vendor_atomic_leases SET expires_at_ms=$4 WHERE principal=$1 AND admission_id=$2 AND lease_id=$3 AND expires_at_ms>$5")
                        .bind(principal).bind(admission).bind(lease).bind(now + 120_000).bind(now).execute(&mut *tx).await.map_err(storage_error)?.rows_affected()
                } else {
                    sqlx::query("INSERT INTO vendor_atomic_leases(principal,admission_id,lease_id,expires_at_ms) SELECT principal,admission_id,$3,$4 FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2 ON CONFLICT DO NOTHING")
                        .bind(principal).bind(admission).bind(lease).bind(now + 120_000).execute(&mut *tx).await.map_err(storage_error)?.rows_affected()
                };
                if changed != 1 { return Err(PluginStorageError::StaleOperation); }
                tx.commit().await.map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)?;
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                let now = chrono::Utc::now().timestamp_millis();
                if !data
                    .atomic_lineages
                    .contains_key(&(principal.to_owned(), admission.to_owned()))
                {
                    return Err(if renew {
                        PluginStorageError::StaleOperation
                    } else {
                        PluginStorageError::MissingLineage
                    });
                }
                if let Some(epoch) = expected_epoch
                    && (self.history_epoch().await? != epoch
                        || data
                            .atomic_lineage_epochs
                            .get(&(principal.to_owned(), admission.to_owned()))
                            != Some(&epoch))
                {
                    return Err(PluginStorageError::StaleOperation);
                }
                let key = (principal.to_owned(), admission.to_owned(), lease.to_owned());
                let expiry = now + 120_000;
                if renew {
                    let previous = *data
                        .atomic_leases
                        .get(&key)
                        .ok_or(PluginStorageError::StaleOperation)?;
                    if previous <= now {
                        return Err(PluginStorageError::StaleOperation);
                    }
                    data.atomic_leases.insert(key.clone(), expiry);
                    let created =
                        data.atomic_lineages[&(principal.to_owned(), admission.to_owned())];
                    if let Err(error) = mirror_memory_atomic(
                        store,
                        &MemoryAtomicMirror {
                            principal,
                            admission,
                            lease,
                            expiry,
                            created_at: created,
                            history_epoch: data.atomic_lineage_epochs
                                [&(principal.to_owned(), admission.to_owned())],
                            checkpoint: None,
                            parent: None,
                            fresh: false,
                            create_lineage: false,
                        },
                    )
                    .await
                    {
                        data.atomic_leases.insert(key, previous);
                        return Err(error);
                    }
                } else {
                    if data.atomic_leases.contains_key(&key) {
                        return Err(PluginStorageError::StaleOperation);
                    }
                    data.atomic_leases.insert(key.clone(), expiry);
                    let created =
                        data.atomic_lineages[&(principal.to_owned(), admission.to_owned())];
                    if let Err(error) = mirror_memory_atomic(
                        store,
                        &MemoryAtomicMirror {
                            principal,
                            admission,
                            lease,
                            expiry,
                            created_at: created,
                            history_epoch: data.atomic_lineage_epochs
                                [&(principal.to_owned(), admission.to_owned())],
                            checkpoint: None,
                            parent: None,
                            fresh: true,
                            create_lineage: false,
                        },
                    )
                    .await
                    {
                        data.atomic_leases.remove(&key);
                        return Err(error);
                    }
                }
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => lease_sql!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                false
            ),
            PluginStoreBackend::Postgres(pool) => lease_sql!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock_shared(7342179085017261)",
                true
            ),
        }
    }

    #[cfg(test)]
    pub(crate) async fn fork_atomic_state(
        &self,
        principal: &str,
        admission: &str,
        parent_checkpoint: Option<&str>,
        lease: &str,
    ) -> Result<(), PluginStorageError> {
        self.fork_atomic_state_inner(principal, admission, parent_checkpoint, lease, None)
            .await
    }

    pub(crate) async fn fork_atomic_state_at_epoch(
        &self,
        principal: &str,
        admission: &str,
        parent_checkpoint: Option<&str>,
        lease: &str,
        epoch: i64,
    ) -> Result<(), PluginStorageError> {
        self.fork_atomic_state_inner(principal, admission, parent_checkpoint, lease, Some(epoch))
            .await
    }

    async fn fork_atomic_state_inner(
        &self,
        principal: &str,
        admission: &str,
        parent_checkpoint: Option<&str>,
        lease: &str,
        expected_epoch: Option<i64>,
    ) -> Result<(), PluginStorageError> {
        macro_rules! fork {
            ($pool:expr, $begin:literal, $lock:literal, $pg:expr) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
                sqlx::query($lock).execute(&mut *tx).await.map_err(storage_error)?;
                let epoch_sql = if $pg { "SELECT epoch FROM prompt_admission_epoch WHERE id=1 FOR SHARE" } else { "SELECT epoch FROM prompt_admission_epoch WHERE id=1" };
                let current_epoch: i64 = sqlx::query_scalar(epoch_sql).fetch_one(&mut *tx).await.map_err(storage_error)?;
                if expected_epoch.is_some_and(|epoch| epoch != current_epoch) { return Err(PluginStorageError::StaleOperation); }
                if $pg {
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('lineage:' || $1),hashtext($2))").bind(principal).bind(admission).execute(&mut *tx).await.map_err(storage_error)?;
                }
                let exists: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2").bind(principal).bind(admission).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                if exists.is_none() {
                    let mut boundary_head: Option<String> = None;
                    if let Some(parent) = parent_checkpoint {
                        let found: Option<(Option<String>,)> = sqlx::query_as("SELECT boundary_head FROM vendor_atomic_checkpoints WHERE principal=$1 AND checkpoint_id=$2").bind(principal).bind(parent).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                        boundary_head = found.ok_or(PluginStorageError::StaleOperation)?.0;
                        sqlx::query("INSERT INTO vendor_atomic_cells(vendor_id,provider_id,principal,admission_id,cell_key,revision,payload,journal_head) SELECT vendor_id,provider_id,principal,$3,cell_key,revision,payload,journal_head FROM vendor_atomic_checkpoint_cells WHERE principal=$1 AND checkpoint_id=$2")
                            .bind(principal).bind(parent).bind(admission).execute(&mut *tx).await.map_err(storage_error)?;
                    }
                    sqlx::query("INSERT INTO vendor_atomic_lineages(principal,admission_id,created_at_ms,history_epoch,boundary_head) VALUES($1,$2,$3,$4,$5)").bind(principal).bind(admission).bind(chrono::Utc::now().timestamp_millis()).bind(current_epoch).bind(boundary_head).execute(&mut *tx).await.map_err(storage_error)?;
                } else if expected_epoch.is_some() {
                    let lineage_epoch: i64 = sqlx::query_scalar("SELECT history_epoch FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2").bind(principal).bind(admission).fetch_one(&mut *tx).await.map_err(storage_error)?;
                    if lineage_epoch != current_epoch { return Err(PluginStorageError::StaleOperation); }
                }
                let inserted = sqlx::query("INSERT INTO vendor_atomic_leases(principal,admission_id,lease_id,expires_at_ms) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
                    .bind(principal).bind(admission).bind(lease).bind(chrono::Utc::now().timestamp_millis() + 120_000).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
                if inserted != 1 { return Err(PluginStorageError::StaleOperation); }
                tx.commit().await.map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)?;
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                let current_epoch = self.history_epoch().await?;
                if expected_epoch.is_some_and(|epoch| epoch != current_epoch) {
                    return Err(PluginStorageError::StaleOperation);
                }
                let lineage = (principal.to_owned(), admission.to_owned());
                if expected_epoch.is_some()
                    && data.atomic_lineages.contains_key(&lineage)
                    && data.atomic_lineage_epochs.get(&lineage) != Some(&current_epoch)
                {
                    return Err(PluginStorageError::StaleOperation);
                }
                let parent_identity = parent_checkpoint
                    .map(|parent| {
                        data.atomic_checkpoints
                            .get(&(principal.to_owned(), parent.to_owned()))
                            .map(|checkpoint| {
                                (
                                    parent.to_owned(),
                                    checkpoint.admission_id.clone(),
                                    checkpoint.lease_id.clone(),
                                )
                            })
                            .ok_or(PluginStorageError::StaleOperation)
                    })
                    .transpose()?;
                let new_lineage = !data.atomic_lineages.contains_key(&lineage);
                if new_lineage {
                    if let Some(parent) = parent_checkpoint {
                        let checkpoint = data
                            .atomic_checkpoints
                            .get(&(principal.to_owned(), parent.to_owned()))
                            .ok_or(PluginStorageError::StaleOperation)?;
                        let cells = checkpoint.cells.clone();
                        let boundary_head = checkpoint.boundary_head.clone();
                        if let Some(head) = boundary_head {
                            data.atomic_lineage_boundary_heads
                                .insert(lineage.clone(), head);
                        }
                        for ((vendor, provider, key), snapshot) in cells {
                            data.atomic_lineage_cells
                                .entry(lineage.clone())
                                .or_default()
                                .insert((vendor.clone(), provider.clone(), key.clone()));
                            data.atomic_cells.insert(
                                (
                                    vendor,
                                    provider,
                                    principal.to_owned(),
                                    admission.to_owned(),
                                    key,
                                ),
                                snapshot,
                            );
                        }
                    }
                    data.atomic_lineages
                        .insert(lineage.clone(), chrono::Utc::now().timestamp_millis());
                    data.atomic_lineage_epochs
                        .insert(lineage.clone(), current_epoch);
                }
                let lease_key = (principal.to_owned(), admission.to_owned(), lease.to_owned());
                if data.atomic_leases.contains_key(&lease_key) {
                    return Err(PluginStorageError::StaleOperation);
                }
                let expiry = chrono::Utc::now().timestamp_millis() + 120_000;
                data.atomic_leases.insert(lease_key.clone(), expiry);
                let created = data.atomic_lineages[&(principal.to_owned(), admission.to_owned())];
                if let Err(error) = mirror_memory_atomic(
                    store,
                    &MemoryAtomicMirror {
                        principal,
                        admission,
                        lease,
                        expiry,
                        created_at: created,
                        history_epoch: current_epoch,
                        checkpoint: None,
                        parent: parent_identity.as_ref().map(|(id, admission, lease)| {
                            (id.as_str(), admission.as_str(), lease.as_str())
                        }),
                        fresh: true,
                        create_lineage: new_lineage,
                    },
                )
                .await
                {
                    data.atomic_leases.remove(&lease_key);
                    if new_lineage {
                        data.atomic_lineages.remove(&lineage);
                        data.atomic_lineage_epochs.remove(&lineage);
                        data.atomic_lineage_boundary_heads.remove(&lineage);
                        data.atomic_lineage_cells.remove(&lineage);
                        data.atomic_cells
                            .retain(|(_, _, p, a, _), _| p != principal || a != admission);
                    }
                    return Err(error);
                }
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => fork!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                false
            ),
            PluginStoreBackend::Postgres(pool) => fork!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock_shared(7342179085017261)",
                true
            ),
        }
    }

    pub(crate) async fn checkpoint_atomic_state(
        &self,
        principal: &str,
        admission: &str,
        checkpoint: &str,
        lease: &str,
    ) -> Result<(), PluginStorageError> {
        macro_rules! checkpoint {
            ($pool:expr, $begin:literal, $lock:literal, $pg:expr) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
                sqlx::query($lock).execute(&mut *tx).await.map_err(storage_error)?;
                if $pg {
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('lineage:' || $1),hashtext($2))").bind(principal).bind(admission).execute(&mut *tx).await.map_err(storage_error)?;
                }
                let exists: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_leases WHERE principal=$1 AND admission_id=$2 AND lease_id=$3 AND expires_at_ms>$4").bind(principal).bind(admission).bind(lease).bind(chrono::Utc::now().timestamp_millis()).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                if exists.is_none() { return Err(PluginStorageError::StaleOperation); }
                let inserted = sqlx::query("INSERT INTO vendor_atomic_checkpoints(principal,checkpoint_id,admission_id,lease_id,created_at_ms,boundary_head) VALUES($1,$2,$3,$4,$5,(SELECT boundary_head FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$3)) ON CONFLICT(principal,checkpoint_id) DO NOTHING")
                    .bind(principal).bind(checkpoint).bind(admission).bind(lease).bind(chrono::Utc::now().timestamp_millis()).execute(&mut *tx).await.map_err(storage_error)?.rows_affected();
                if inserted != 1 { return Err(PluginStorageError::Changed); }
                sqlx::query("INSERT INTO vendor_atomic_checkpoint_cells(principal,checkpoint_id,vendor_id,provider_id,cell_key,revision,payload,journal_head) SELECT principal,$3,vendor_id,provider_id,cell_key,revision,payload,journal_head FROM vendor_atomic_cells WHERE principal=$1 AND admission_id=$2")
                    .bind(principal).bind(admission).bind(checkpoint).execute(&mut *tx).await.map_err(storage_error)?;
                tx.commit().await.map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)?;
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                if !data
                    .atomic_leases
                    .get(&(principal.to_owned(), admission.to_owned(), lease.to_owned()))
                    .is_some_and(|expiry| *expiry > chrono::Utc::now().timestamp_millis())
                {
                    return Err(PluginStorageError::StaleOperation);
                }
                let id = (principal.to_owned(), checkpoint.to_owned());
                if data.atomic_checkpoints.contains_key(&id) {
                    return Err(PluginStorageError::Changed);
                }
                let cells = data
                    .atomic_lineage_cells
                    .get(&(principal.to_owned(), admission.to_owned()))
                    .into_iter()
                    .flatten()
                    .map(|(vendor, provider, key)| {
                        let snapshot = data
                            .atomic_cells
                            .get(&(
                                vendor.clone(),
                                provider.clone(),
                                principal.to_owned(),
                                admission.to_owned(),
                                key.clone(),
                            ))
                            .expect("lineage cell index is consistent");
                        (
                            (vendor.clone(), provider.clone(), key.clone()),
                            snapshot.clone(),
                        )
                    })
                    .collect();
                let boundary_head = data
                    .atomic_lineage_boundary_heads
                    .get(&(principal.to_owned(), admission.to_owned()))
                    .cloned();
                data.atomic_checkpoints.insert(
                    id.clone(),
                    MemoryAtomicCheckpoint {
                        admission_id: admission.to_owned(),
                        lease_id: lease.to_owned(),
                        created_at_ms: chrono::Utc::now().timestamp_millis(),
                        boundary_head,
                        cells,
                    },
                );
                let expiry = data.atomic_leases
                    [&(principal.to_owned(), admission.to_owned(), lease.to_owned())];
                let created = data.atomic_lineages[&(principal.to_owned(), admission.to_owned())];
                if let Err(error) = mirror_memory_atomic(
                    store,
                    &MemoryAtomicMirror {
                        principal,
                        admission,
                        lease,
                        expiry,
                        created_at: created,
                        history_epoch: data.atomic_lineage_epochs
                            [&(principal.to_owned(), admission.to_owned())],
                        checkpoint: Some(checkpoint),
                        parent: None,
                        fresh: false,
                        create_lineage: false,
                    },
                )
                .await
                {
                    data.atomic_checkpoints.remove(&id);
                    return Err(error);
                }
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => checkpoint!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                false
            ),
            PluginStoreBackend::Postgres(pool) => checkpoint!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock_shared(7342179085017261)",
                true
            ),
        }
    }

    #[cfg(test)]
    pub(crate) async fn load_atomic_state(
        &self,
        key: &AtomicStateKey<'_>,
    ) -> Result<AtomicStateSnapshot, PluginStorageError> {
        self.atomic_state(key, None, None)
            .await
            .map(|(snapshot, _)| snapshot)
    }

    #[cfg(test)]
    pub(crate) async fn compare_exchange_atomic_state(
        &self,
        key: &AtomicStateKey<'_>,
        expected_revision: u64,
        payload: Option<&[u8]>,
    ) -> Result<bool, PluginStorageError> {
        self.atomic_state(key, Some((expected_revision, payload)), None)
            .await
            .map(|(_, changed)| changed)
    }

    pub(crate) async fn load_atomic_state_fenced(
        &self,
        key: &AtomicStateKey<'_>,
        provider_fence: i64,
    ) -> Result<AtomicStateSnapshot, PluginStorageError> {
        self.atomic_state(key, None, Some(provider_fence))
            .await
            .map(|(snapshot, _)| snapshot)
    }

    pub(crate) async fn compare_exchange_atomic_state_fenced(
        &self,
        key: &AtomicStateKey<'_>,
        expected_revision: u64,
        payload: Option<&[u8]>,
        provider_fence: i64,
    ) -> Result<bool, PluginStorageError> {
        self.atomic_state(
            key,
            Some((expected_revision, payload)),
            Some(provider_fence),
        )
        .await
        .map(|(_, changed)| changed)
    }

    async fn atomic_state(
        &self,
        identity: &AtomicStateKey<'_>,
        update: Option<(u64, Option<&[u8]>)>,
        provider_fence: Option<i64>,
    ) -> Result<(AtomicStateSnapshot, bool), PluginStorageError> {
        let AtomicStateKey {
            vendor_id,
            provider_id,
            data_epoch,
            principal,
            admission,
            lease,
            key,
        } = *identity;
        if key.len() > 128
            || update
                .is_some_and(|(_, payload)| payload.is_some_and(|p| p.len() > MAX_PAYLOAD_BYTES))
        {
            return Err(PluginStorageError::Limit);
        }
        let (principal, admission, lease) = match (principal, admission, lease) {
            (None, None, None) => ("", "", ""),
            (Some(p), Some(a), Some(l)) if !p.is_empty() && !a.is_empty() && !l.is_empty() => {
                (p, a, l)
            }
            _ => return Err(PluginStorageError::StaleOperation),
        };
        if update.is_none() {
            match &self.backend {
                PluginStoreBackend::Memory(store) => {
                    let providers = store.providers.read().await;
                    let data = store.data.read().await;
                    let owner = providers
                        .iter()
                        .find(|p| p.id == provider_id)
                        .and_then(|p| p.vendor.as_deref())
                        == Some(vendor_id);
                    let epoch = data
                        .installed
                        .get(vendor_id)
                        .or_else(|| data.installed.get("base"))
                        .is_some_and(|v| v.data_epoch == data_epoch);
                    if !owner
                        || !epoch
                        || (!principal.is_empty()
                            && !data
                                .atomic_leases
                                .get(&(
                                    principal.to_owned(),
                                    admission.to_owned(),
                                    lease.to_owned(),
                                ))
                                .is_some_and(|expiry| {
                                    *expiry > chrono::Utc::now().timestamp_millis()
                                }))
                    {
                        return Err(PluginStorageError::StaleOperation);
                    }
                    if provider_fence.is_some_and(|fence| {
                        data.atomic_provider_fences.get(provider_id) != Some(&fence)
                    }) {
                        return Err(PluginStorageError::StaleOperation);
                    }
                    if !principal.is_empty() {
                        validate_memory_atomic_mirror(store, principal, admission, lease).await?;
                    }
                    let cell = (
                        vendor_id.to_owned(),
                        provider_id.to_owned(),
                        principal.to_owned(),
                        admission.to_owned(),
                        key.to_owned(),
                    );
                    return Ok((
                        data.atomic_cells.get(&cell).cloned().unwrap_or_default(),
                        false,
                    ));
                }
                PluginStoreBackend::Sqlite(pool) => {
                    let row: Option<AtomicLoadRow> = sqlx::query_as(ATOMIC_LOAD_SQL)
                        .bind(provider_id)
                        .bind(vendor_id)
                        .bind(principal)
                        .bind(admission)
                        .bind(lease)
                        .bind(key)
                        .bind(chrono::Utc::now().timestamp_millis())
                        .bind(provider_fence)
                        .fetch_optional(pool)
                        .await
                        .map_err(storage_error)?;
                    return atomic_load_result(row, data_epoch);
                }
                PluginStoreBackend::Postgres(pool) => {
                    let row: Option<AtomicLoadRow> = sqlx::query_as(ATOMIC_LOAD_SQL)
                        .bind(provider_id)
                        .bind(vendor_id)
                        .bind(principal)
                        .bind(admission)
                        .bind(lease)
                        .bind(key)
                        .bind(chrono::Utc::now().timestamp_millis())
                        .bind(provider_fence)
                        .fetch_optional(pool)
                        .await
                        .map_err(storage_error)?;
                    return atomic_load_result(row, data_epoch);
                }
            }
        }
        macro_rules! sql {
            ($pool:expr, $begin:literal, $lock:literal, $owner:literal, $revision:literal, $pg:expr) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
                sqlx::query($lock).execute(&mut *tx).await.map_err(storage_error)?;
                if $pg {
                    let (namespace, first, second) = if principal.is_empty() { ("provider:", vendor_id, provider_id) } else { ("lineage:", principal, admission) };
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1 || $2),hashtext($3))").bind(namespace).bind(first).bind(second).execute(&mut *tx).await.map_err(storage_error)?;
                }
                let valid: Option<i64> = sqlx::query_scalar($owner)
                    .bind(provider_id).bind(vendor_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                if valid != Some(data_epoch) { return Err(PluginStorageError::StaleOperation); }
                if let Some(fence) = provider_fence {
                    let current: Option<i64> = sqlx::query_scalar("SELECT generation FROM vendor_atomic_provider_fences WHERE provider_id=$1").bind(provider_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                    if current != Some(fence) { return Err(PluginStorageError::StaleOperation); }
                }
                if !principal.is_empty() {
                    let exists: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_leases WHERE principal=$1 AND admission_id=$2 AND lease_id=$3 AND expires_at_ms>$4")
                        .bind(principal).bind(admission).bind(lease).bind(chrono::Utc::now().timestamp_millis()).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                    if exists.is_none() { return Err(PluginStorageError::StaleOperation); }
                }
                let row: Option<(i64, Option<Vec<u8>>, Option<String>)> = sqlx::query_as("SELECT revision,payload,journal_head FROM vendor_atomic_cells WHERE vendor_id=$1 AND provider_id=$2 AND principal=$3 AND admission_id=$4 AND cell_key=$5")
                    .bind(vendor_id).bind(provider_id).bind(principal).bind(admission).bind(key).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                let snapshot = row.map(|(revision,payload,journal_head)| AtomicStateSnapshot { revision: revision as u64, payload, journal_head }).unwrap_or_default();
                let mut changed = false;
                if let Some((expected, payload)) = update {
                    if snapshot.revision == expected {
                        if snapshot.revision == 0 {
                            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vendor_atomic_cells WHERE vendor_id=$1 AND provider_id=$2 AND principal=$3 AND admission_id=$4")
                                .bind(vendor_id).bind(provider_id).bind(principal).bind(admission).fetch_one(&mut *tx).await.map_err(storage_error)?;
                            if count >= 16 { return Err(PluginStorageError::Limit); }
                        }
                        let revision: i64 = sqlx::query_scalar($revision).fetch_one(&mut *tx).await.map_err(storage_error)?;
                        sqlx::query("INSERT INTO vendor_atomic_cells(vendor_id,provider_id,principal,admission_id,cell_key,revision,payload) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(vendor_id,provider_id,principal,admission_id,cell_key) DO UPDATE SET revision=excluded.revision,payload=excluded.payload,journal_head=NULL")
                            .bind(vendor_id).bind(provider_id).bind(principal).bind(admission).bind(key).bind(revision).bind(payload).execute(&mut *tx).await.map_err(storage_error)?;
                        changed = true;
                    }
                }
                tx.commit().await.map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)?;
                Ok((snapshot, changed))
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                let mut data = store.data.write().await;
                let owner = providers
                    .iter()
                    .find(|p| p.id == provider_id)
                    .and_then(|p| p.vendor.as_deref())
                    == Some(vendor_id);
                let epoch = data
                    .installed
                    .get(vendor_id)
                    .or_else(|| data.installed.get("base"))
                    .is_some_and(|v| v.data_epoch == data_epoch);
                if !owner
                    || !epoch
                    || (!principal.is_empty()
                        && !data
                            .atomic_leases
                            .get(&(principal.to_owned(), admission.to_owned(), lease.to_owned()))
                            .is_some_and(|expiry| *expiry > chrono::Utc::now().timestamp_millis()))
                {
                    return Err(PluginStorageError::StaleOperation);
                }
                if provider_fence.is_some_and(|fence| {
                    data.atomic_provider_fences.get(provider_id) != Some(&fence)
                }) {
                    return Err(PluginStorageError::StaleOperation);
                }
                if !principal.is_empty() {
                    validate_memory_atomic_mirror(store, principal, admission, lease).await?;
                }
                let cell = (
                    vendor_id.to_owned(),
                    provider_id.to_owned(),
                    principal.to_owned(),
                    admission.to_owned(),
                    key.to_owned(),
                );
                let snapshot = data.atomic_cells.get(&cell).cloned().unwrap_or_default();
                let mut changed = false;
                if let Some((expected, payload)) = update
                    && snapshot.revision == expected
                {
                    if snapshot.revision == 0
                        && data
                            .atomic_cells
                            .keys()
                            .filter(|(v, p, pr, a, _)| {
                                v == vendor_id
                                    && p == provider_id
                                    && pr == principal
                                    && a == admission
                            })
                            .count()
                            >= 16
                    {
                        return Err(PluginStorageError::Limit);
                    }
                    data.atomic_revision = data
                        .atomic_revision
                        .checked_add(1)
                        .ok_or(PluginStorageError::Storage)?;
                    let revision = data.atomic_revision as u64;
                    if !principal.is_empty() {
                        data.atomic_lineage_cells
                            .entry((principal.to_owned(), admission.to_owned()))
                            .or_default()
                            .insert((vendor_id.to_owned(), provider_id.to_owned(), key.to_owned()));
                    }
                    data.atomic_cells.insert(
                        cell,
                        AtomicStateSnapshot {
                            revision,
                            payload: payload.map(<[u8]>::to_vec),
                            journal_head: None,
                        },
                    );
                    changed = true;
                }
                Ok((snapshot, changed))
            }
            PluginStoreBackend::Sqlite(pool) => sql!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                "SELECT v.data_epoch FROM providers p JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') WHERE p.id=$1 AND p.vendor=$2",
                "UPDATE vendor_atomic_revision SET revision=revision+1 WHERE id=1 RETURNING revision",
                false
            ),
            PluginStoreBackend::Postgres(pool) => sql!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock_shared(7342179085017261)",
                "SELECT v.data_epoch FROM providers p JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') WHERE p.id=$1 AND p.vendor=$2 FOR SHARE OF p,v",
                "SELECT nextval('vendor_atomic_revision_seq')",
                true
            ),
        }
    }

    pub(crate) async fn read_private_state(
        &self,
        vendor_id: &str,
        provider_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>, PluginStorageError> {
        const SQL: &str = "SELECT s.format_version,s.payload FROM vendor_private_state s JOIN providers p ON p.id=s.provider_id WHERE s.provider_id=$1 AND s.vendor_id=$2 AND p.vendor=$2";
        macro_rules! read {
            ($pool:expr) => {
                sqlx::query_as(SQL)
                    .bind(provider_id)
                    .bind(vendor_id)
                    .fetch_optional($pool)
                    .await
            };
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                if providers
                    .iter()
                    .find(|provider| provider.id == provider_id)
                    .and_then(|provider| provider.vendor.as_deref())
                    != Some(vendor_id)
                {
                    return Ok(None);
                }
                let data = store.data.read().await;
                Ok(data.private_state.get(provider_id).and_then(|state| {
                    (state.vendor_id == vendor_id)
                        .then(|| (state.format_version.clone(), state.payload.clone()))
                }))
            }
            PluginStoreBackend::Sqlite(pool) => read!(pool).map_err(storage_error),
            PluginStoreBackend::Postgres(pool) => read!(pool).map_err(storage_error),
        }
    }

    pub(crate) async fn write_private_state(
        &self,
        vendor_id: &str,
        provider_id: &str,
        data_epoch: i64,
        format_version: &str,
        payload: &[u8],
    ) -> Result<(), PluginStorageError> {
        const SQL: &str = "INSERT INTO vendor_private_state (provider_id,vendor_id,format_version,payload,updated_at)
    SELECT p.id,p.vendor,$4,$5,$6 FROM providers p
    JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT d.vendor_id FROM vendor_plugins d WHERE d.vendor_id=p.vendor),'base')
    WHERE p.id=$1 AND p.vendor=$2 AND v.data_epoch=$3
    ON CONFLICT(provider_id) DO UPDATE SET vendor_id=excluded.vendor_id,format_version=excluded.format_version,payload=excluded.payload,updated_at=excluded.updated_at";
        macro_rules! write {
            ($pool:expr, $begin:literal, $lock:literal) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
                // PostgreSQL 的 INSERT ... SELECT 可持有旧 MVCC 快照；
                // 与包切换锁住同一行，确保旧写入在重置之前提交或被拒绝。
                let current: Option<i64> = sqlx::query_scalar($lock)
                    .bind(vendor_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                if current != Some(data_epoch) {
                    return Err(PluginStorageError::StaleOperation);
                }
                let changed = sqlx::query(SQL)
                    .bind(provider_id)
                    .bind(vendor_id)
                    .bind(data_epoch)
                    .bind(format_version)
                    .bind(payload)
                    .bind(chrono::Utc::now().timestamp_millis())
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?
                    .rows_affected();
                if changed != 1 {
                    return Err(PluginStorageError::StaleOperation);
                }
                tx.commit().await.map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)?;
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                let mut data = store.data.write().await;
                let owner_matches = providers
                    .iter()
                    .find(|provider| provider.id == provider_id)
                    .and_then(|provider| provider.vendor.as_deref())
                    == Some(vendor_id);
                let epoch_matches = data
                    .installed
                    .get(vendor_id)
                    .or_else(|| data.installed.get("base"))
                    .is_some_and(|installed| installed.data_epoch == data_epoch);
                if !owner_matches || !epoch_matches {
                    return Err(PluginStorageError::StaleOperation);
                }
                data.private_state.insert(
                    provider_id.to_string(),
                    MemoryPrivateState {
                        vendor_id: vendor_id.to_string(),
                        format_version: format_version.to_string(),
                        payload: payload.to_vec(),
                    },
                );
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => write!(
                pool,
                "BEGIN IMMEDIATE",
                "SELECT data_epoch FROM vendor_plugins WHERE vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=$1),'base')"
            ),
            PluginStoreBackend::Postgres(pool) => write!(
                pool,
                "BEGIN",
                "SELECT data_epoch FROM vendor_plugins WHERE vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=$1),'base') FOR SHARE"
            ),
        }
    }

    pub(crate) async fn recovery(
        &self,
        provider_id: &str,
    ) -> Result<Vec<String>, PluginStorageError> {
        const SQL: &str =
            "SELECT data_kind FROM vendor_data_recovery WHERE provider_id=$1 ORDER BY data_kind";
        match &self.backend {
            PluginStoreBackend::Memory(store) => Ok(store
                .data
                .read()
                .await
                .recovery
                .iter()
                .filter(|(candidate, _)| candidate == provider_id)
                .map(|(_, kind)| kind.clone())
                .collect()),
            PluginStoreBackend::Sqlite(pool) => sqlx::query_scalar(SQL)
                .bind(provider_id)
                .fetch_all(pool)
                .await
                .map_err(storage_error),
            PluginStoreBackend::Postgres(pool) => sqlx::query_scalar(SQL)
                .bind(provider_id)
                .fetch_all(pool)
                .await
                .map_err(storage_error),
        }
    }

    pub(crate) async fn recovered(
        &self,
        provider_id: &str,
        kind: &str,
    ) -> Result<(), PluginStorageError> {
        const SQL: &str = "DELETE FROM vendor_data_recovery WHERE provider_id=$1 AND data_kind=$2";
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                store
                    .data
                    .write()
                    .await
                    .recovery
                    .retain(|(candidate, data_kind)| candidate != provider_id || data_kind != kind);
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => {
                let mut connection = pool.acquire_write().await.map_err(storage_error)?;
                sqlx::query(SQL)
                    .bind(provider_id)
                    .bind(kind)
                    .execute(&mut *connection)
                    .await
                    .map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)
            }
            PluginStoreBackend::Postgres(pool) => sqlx::query(SQL)
                .bind(provider_id)
                .bind(kind)
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(storage_error),
        }
    }
}

#[path = "atomic_retention.rs"]
mod atomic_retention;

#[cfg(test)]
#[path = "atomic_test_fixtures.rs"]
mod atomic_test_fixtures;

#[cfg(test)]
#[path = "atomic_state_tests.rs"]
mod atomic_state_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_models::{
        ModelCost, ModelLimit, PriceComponents, ProviderModelCostRule, ProviderModelCostRuleKind,
        ProviderModelSelectionPolicy, ProviderModelSourceKind, SnapshotState, SourceStamp,
    };
    use rust_decimal::Decimal;
    use serde_json::json;

    #[tokio::test]
    async fn plugin_transaction_invalidates_runtime_configuration() -> anyhow::Result<()> {
        use crate::db::models::CreateProviderRecord;
        use crate::provider_models::NewProviderModelRecord;
        use crate::storage::{DynStorage, SqliteStorage};

        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let source: DynStorage = Arc::new(SqliteStorage::from_pool(pool));
        let config = crate::runtime_config::RuntimeConfig::attach(source).await?;
        let provider = config
            .providers()
            .create(CreateProviderRecord {
                name: "runtime provider".into(),
                vendor: Some("custom".into()),
                protocol: "openai-compatible".into(),
                base_url: "http://127.0.0.1:9".into(),
                preset_key: None,
                channel: Some("default".into()),
                models_source: None,
                static_models: None,
                api_key: "local-test-credential".into(),
                adapter_credentials: "{}".into(),
                vendor_options: "{\"old\":true}".into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
            })
            .await?;
        let mut metadata = ProviderModelMetadata::bare("model");
        metadata
            .extensions
            .insert("plugin-field".into(), json!(true));
        config
            .provider_models()
            .create(
                NewProviderModelRecord {
                    provider_id: provider.id.clone(),
                    model_id: "model".into(),
                    source_kind: ProviderModelSourceKind::Discovered,
                    snapshot_state: SnapshotState::Unregistered,
                    source_metadata: None,
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Present,
                    selection_policy: ProviderModelSelectionPolicy::Auto,
                    metadata,
                },
                None,
            )
            .await?;
        assert_eq!(
            config.providers().get(&provider.id).await?.unwrap().api_key,
            "local-test-credential"
        );
        assert_eq!(
            config
                .provider_models()
                .find(&provider.id, "MODEL")
                .await?
                .unwrap()
                .presence,
            ProviderModelPresence::Present
        );
        config
            .vendor_plugins()
            .install(
                &InstalledPlugin {
                    vendor_id: "custom".into(),
                    version: "test".into(),
                    source: "local".into(),
                    descriptor: "{}".into(),
                    component: Bytes::new(),
                    digest: "test".into(),
                    revision: 1,
                    data_epoch: 1,
                    installed_at: 1,
                },
                None,
                &[ProviderReset {
                    impact: DataReset {
                        provider_id: provider.id.clone(),
                        options: true,
                        credentials: true,
                        models: true,
                        private_state: false,
                    },
                    expected_vendor: "custom".into(),
                    retained_adapter_credentials: "{}".into(),
                    retained_options: "{}".into(),
                }],
            )
            .await?;
        let current = config.providers().get(&provider.id).await?.unwrap();
        assert!(current.api_key.is_empty());
        assert_eq!(current.vendor_options, "{}");
        let model = config
            .provider_models()
            .find(&provider.id, "model")
            .await?
            .unwrap();
        assert_eq!(model.presence, ProviderModelPresence::Missing);
        assert!(!model.metadata.extensions.contains_key("plugin-field"));
        Ok(())
    }

    fn model(source_kind: ProviderModelSourceKind) -> ProviderModelRecord {
        let metadata = ProviderModelMetadata {
            id: Some("model-a".to_owned()),
            name: Some("人工名称".to_owned()),
            description: Some("人工描述".to_owned()),
            family: Some("family-a".to_owned()),
            reasoning_efforts: Some(vec!["low".to_owned(), "high".to_owned()]),
            limit: Some(ModelLimit {
                context: Some(128_000),
            }),
            cost: Some(ModelCost {
                prices: PriceComponents {
                    input: Some(Decimal::new(125, 2)),
                    output: Some(Decimal::new(250, 2)),
                    ..PriceComponents::default()
                },
                ..ModelCost::default()
            }),
            status: Some("deprecated".to_owned()),
            experimental: Some(json!({"vendor_flag": true})),
            provider: Some(json!({"upstream": "model-a"})),
            extensions: BTreeMap::from([("opaque".to_owned(), json!({"rank": 7}))]),
            ..ProviderModelMetadata::default()
        };
        ProviderModelRecord {
            provider_id: "provider-a".to_owned(),
            model_id: "model-a".to_owned(),
            source_kind,
            snapshot_state: SnapshotState::Imported {
                source: SourceStamp::ProviderCatalog {
                    provider_id: "catalog-a".to_owned(),
                },
                pinned: BTreeSet::new(),
            },
            source_metadata: None,
            metadata_source_provider_id: Some("catalog-a".to_owned()),
            presence: ProviderModelPresence::Present,
            selection_policy: ProviderModelSelectionPolicy::ForceEnabled,
            metadata,
            revision: 4,
            created_at: "created".to_owned(),
            updated_at: "updated".to_owned(),
            cost_rules: vec![ProviderModelCostRule {
                rule_index: 0,
                kind: ProviderModelCostRuleKind::Tier,
                threshold_tokens: 128_000,
                prices: PriceComponents::default(),
            }],
        }
    }

    #[test]
    fn plugin_model_metadata_detection_covers_each_owned_field() {
        let canonical = ProviderModelMetadata {
            id: Some("model-a".to_owned()),
            name: Some("人工名称".to_owned()),
            cost: Some(ModelCost::default()),
            ..ProviderModelMetadata::default()
        };
        assert!(!has_plugin_model_metadata(&canonical));

        for metadata in [
            ProviderModelMetadata {
                extensions: BTreeMap::from([("opaque".to_owned(), json!(true))]),
                ..canonical.clone()
            },
            ProviderModelMetadata {
                provider: Some(json!({})),
                ..canonical.clone()
            },
            ProviderModelMetadata {
                experimental: Some(json!({})),
                ..canonical.clone()
            },
            ProviderModelMetadata {
                status: Some("deprecated".to_owned()),
                ..canonical.clone()
            },
        ] {
            assert!(has_plugin_model_metadata(&metadata));
        }
    }

    #[test]
    fn plugin_model_reset_preserves_core_owned_discovered_fields() {
        let mut actual = model(ProviderModelSourceKind::Discovered);
        let mut expected = actual.clone();
        expected.metadata.status = None;
        expected.metadata.experimental = None;
        expected.metadata.provider = None;
        expected.metadata.extensions.clear();
        expected.presence = ProviderModelPresence::Missing;
        expected.revision += 1;

        assert!(reset_memory_model(&mut actual));
        assert_eq!(actual, expected);
    }

    #[test]
    fn plugin_model_reset_preserves_manual_presence_and_skips_canonical_only_records() {
        let mut manual = model(ProviderModelSourceKind::Manual);
        let mut expected_manual = manual.clone();
        expected_manual.metadata.status = None;
        expected_manual.metadata.experimental = None;
        expected_manual.metadata.provider = None;
        expected_manual.metadata.extensions.clear();
        expected_manual.revision += 1;

        assert!(reset_memory_model(&mut manual));
        assert_eq!(manual, expected_manual);

        let mut canonical_only = expected_manual;
        let unchanged = canonical_only.clone();
        assert!(!has_plugin_model_metadata(&canonical_only.metadata));
        assert!(!reset_memory_model(&mut canonical_only));
        assert_eq!(canonical_only, unchanged);
    }
}
