use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use tokio::sync::RwLock;

use crate::db::models::Provider;
use crate::provider_models::{ProviderModelMetadata, ProviderModelPresence, ProviderModelRecord};
use crate::storage::memory::MemoryOAuthCredentialStore;

pub(crate) const MAX_PRIVATE_STATE_BYTES: usize = 256 * 1024;

/// Storage-backed vendor plugin packages and their provider-scoped state.
#[derive(Clone)]
pub struct PluginStore {
    backend: PluginStoreBackend,
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
}

#[derive(Default)]
struct MemoryPluginData {
    installed: BTreeMap<String, InstalledPlugin>,
    private_state: HashMap<String, MemoryPrivateState>,
    recovery: BTreeSet<(String, String)>,
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
    #[error("vendor private state exceeds its size limit")]
    StateTooLarge,
    #[error("plugin storage operation failed")]
    Storage,
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

impl PluginStore {
    pub(crate) fn is_persistent(&self) -> bool {
        !matches!(self.backend, PluginStoreBackend::Memory(_))
    }

    pub(crate) fn memory(
        providers: Arc<RwLock<Vec<Provider>>>,
        provider_models: Arc<RwLock<Vec<ProviderModelRecord>>>,
        oauth_credentials: Arc<MemoryOAuthCredentialStore>,
    ) -> Self {
        Self {
            backend: PluginStoreBackend::Memory(MemoryPluginStore {
                providers,
                provider_models,
                oauth_credentials,
                data: Arc::new(RwLock::new(MemoryPluginData::default())),
            }),
        }
    }

    pub(crate) async fn remove_memory_provider_data(&self, provider_id: &str) {
        let PluginStoreBackend::Memory(store) = &self.backend else {
            unreachable!("memory provider cleanup requires the memory plugin backend");
        };
        let mut data = store.data.write().await;
        data.private_state.remove(provider_id);
        data.recovery
            .retain(|(candidate, _)| candidate != provider_id);
    }

    pub(crate) fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self {
            backend: PluginStoreBackend::Sqlite(pool),
        }
    }

    pub(crate) fn postgres(pool: sqlx::PgPool) -> Self {
        Self {
            backend: PluginStoreBackend::Postgres(pool),
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

    /// 包替换与经确认的数据重置原子提交；调用者先持有供应商静默凭证。
    pub(crate) async fn install(
        &self,
        next: &InstalledPlugin,
        expected_revision: Option<i64>,
        resets: &[ProviderReset],
    ) -> Result<(), PluginStorageError> {
        macro_rules! install {
            ($pool:expr, $begin:literal) => {{
                let mut connection = $pool.acquire().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
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
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                install_memory(store, next, expected_revision, resets).await
            }
            PluginStoreBackend::Sqlite(pool) => install!(pool, "BEGIN IMMEDIATE"),
            PluginStoreBackend::Postgres(pool) => install!(pool, "BEGIN"),
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
        if payload.len() > MAX_PRIVATE_STATE_BYTES {
            return Err(PluginStorageError::StateTooLarge);
        }
        const SQL: &str = "INSERT INTO vendor_private_state (provider_id,vendor_id,format_version,payload,updated_at)
            SELECT p.id,p.vendor,$4,$5,$6 FROM providers p
            JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT d.vendor_id FROM vendor_plugins d WHERE d.vendor_id=p.vendor),'base')
            WHERE p.id=$1 AND p.vendor=$2 AND v.data_epoch=$3
            ON CONFLICT(provider_id) DO UPDATE SET vendor_id=excluded.vendor_id,format_version=excluded.format_version,payload=excluded.payload,updated_at=excluded.updated_at";
        macro_rules! write {
            ($pool:expr, $begin:literal, $lock:literal) => {{
                let mut connection = $pool.acquire().await.map_err(storage_error)?;
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
            PluginStoreBackend::Sqlite(pool) => sqlx::query(SQL)
                .bind(provider_id)
                .bind(kind)
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(storage_error),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_models::{
        ModelCost, ModelLimit, PriceComponents, ProviderModelCostRule, ProviderModelCostRuleKind,
        ProviderModelSelectionPolicy, ProviderModelSourceKind,
    };
    use rust_decimal::Decimal;
    use serde_json::json;

    fn model(source_kind: ProviderModelSourceKind) -> ProviderModelRecord {
        let metadata = ProviderModelMetadata {
            id: Some("model-a".to_owned()),
            name: Some("人工名称".to_owned()),
            description: Some("人工描述".to_owned()),
            family: Some("family-a".to_owned()),
            reasoning: Some(true),
            limit: Some(ModelLimit {
                context: Some(128_000),
                input: Some(120_000),
                output: Some(8_000),
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
