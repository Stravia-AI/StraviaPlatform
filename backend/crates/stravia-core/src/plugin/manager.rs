use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::RwLock;
use semver::Version;
use sha2::{Digest, Sha256};
use stravia_vendor_runtime::{LoadedPlugin, VendorRuntime};
use stravia_vendor_sdk::{ProviderDescriptor, VendorDescriptor, VendorKind};
use tokio::sync::Mutex;

use crate::Gateway;
use crate::db::models::Provider;

use super::artifacts::{MAX_COMPONENT_BYTES, PluginArtifacts};
use super::builtin::BundledPlugins;
use super::lifecycle::{VendorOperation, VendorOperationTracker, WritePermit};
use super::permissions::resolve_permissions;
use super::store::{
    DataReset, InstalledPlugin, PluginStore, ProviderReset, has_plugin_model_metadata,
};
use super::types::*;

const MAX_PREVIEWS: usize = 8;
const PREVIEW_LIFETIME: Duration = Duration::from_secs(15 * 60);

struct Entry {
    record: InstalledPlugin,
    loaded: Option<LoadedPlugin>,
    /// Catalog profiles the advertised set dropped. They stay inside the
    /// effective descriptor so saved connections keep executing, but are
    /// hidden from every creation/listing surface.
    retired_profiles: BTreeSet<String>,
}

// 数据兼容性只依赖持久化身份和格式代际，不依赖某一版表单展示契约能否解码。
#[derive(serde::Deserialize)]
struct RecordedDataManifest {
    providers: Vec<RecordedDataProfile>,
}

#[derive(serde::Deserialize)]
struct RecordedDataProfile {
    provider_id: String,
    data_compat: stravia_vendor_sdk::DataCompatibility,
    config_fields: Vec<RecordedDataField>,
}

#[derive(serde::Deserialize)]
struct RecordedDataField {
    key: String,
    #[serde(default)]
    secret: bool,
}

struct Pending {
    preview: PluginPreview,
    record: InstalledPlugin,
    loaded: LoadedPlugin,
    expected_revision: Option<i64>,
    provider_fingerprint: Vec<u8>,
    data_fingerprint: Vec<u8>,
    resolution_fingerprint: Vec<u8>,
    scope_ids: BTreeSet<String>,
    resets: Vec<ProviderReset>,
    data_incompatible: bool,
    created: Instant,
}

#[derive(serde::Serialize)]
struct ProviderState {
    provider: Provider,
    oauth: Option<crate::db::models::OAuthCredential>,
    models: Vec<crate::provider_models::ProviderModelRecord>,
    private_state: Option<(String, Vec<u8>)>,
}

pub(crate) struct VendorPlugins {
    pub(crate) runtime: VendorRuntime,
    pub(crate) store: PluginStore,
    operations: VendorOperationTracker,
    entries: RwLock<HashMap<String, Entry>>,
    bundled: Arc<BundledPlugins>,
    artifacts: Option<PluginArtifacts>,
    pending: Mutex<HashMap<String, Pending>>,
    updates: Mutex<()>,
}

impl VendorPlugins {
    pub(crate) async fn open(
        store: PluginStore,
        plugin_directory: PathBuf,
    ) -> anyhow::Result<Arc<Self>> {
        let bundled = BundledPlugins::load().await?;
        let runtime = bundled.runtime.clone();
        let artifacts = store
            .is_persistent()
            .then(|| PluginArtifacts::new(plugin_directory));
        let mut entries = HashMap::new();
        for mut record in store.list().await? {
            let component = match bundled
                .plugins
                .get(&record.vendor_id)
                .filter(|_| record.source == PluginSource::Builtin.as_str())
            {
                Some(bundle) => {
                    let digest = stravia_runtime_contract::protocol::ir::canonical::hash_hex(
                        &Sha256::digest(bundle.component).into(),
                    );
                    if digest == record.digest {
                        Ok(Bytes::from_static(bundle.component))
                    } else {
                        // 记录标为 builtin，但安装的是旧发行版内嵌字节或同版本
                        // 不同构建；artifact 按 digest 校验完整性，回退读取即可，
                        // 不能因与当前内嵌字节不同就判为不可用。
                        match &artifacts {
                            Some(artifacts) => artifacts.read(&record.digest).await,
                            None => Ok(record.component.clone()),
                        }
                    }
                }
                None => match &artifacts {
                    Some(artifacts) => artifacts.read(&record.digest).await,
                    None => Ok(record.component.clone()),
                },
            };
            let package = match component {
                Ok(component) => match bundled.plugins.get(&record.vendor_id) {
                    Some(bundle) if bundle.component == component.as_ref() => {
                        Ok(bundle.loaded.clone())
                    }
                    _ => runtime.load(&component).await.map_err(anyhow::Error::from),
                },
                Err(error) => {
                    tracing::warn!(vendor_id = %record.vendor_id, %error, "installed plugin artifact is unavailable");
                    Err(error)
                }
            };
            let loaded = match package {
                Ok(package)
                    if package.descriptor().vendor_id == record.vendor_id
                        && package.descriptor().version.to_string() == record.version =>
                {
                    Some(package)
                }
                _ => None,
            };
            if let Some(package) = &loaded {
                record.descriptor = serde_json::to_string(package.descriptor())?;
            }
            record.component = Bytes::new();
            entries.insert(
                record.vendor_id.clone(),
                Entry {
                    record,
                    loaded,
                    retired_profiles: BTreeSet::new(),
                },
            );
        }
        Ok(Arc::new(Self {
            runtime,
            store,
            operations: VendorOperationTracker::default(),
            entries: RwLock::new(entries),
            bundled,
            artifacts,
            pending: Mutex::new(HashMap::new()),
            updates: Mutex::new(()),
        }))
    }

    /// Vendor 配置与状态写回的复合许可；持有期间该 Vendor 的插件更新无法完成排空。
    pub(crate) async fn write_permit(&self, vendor_id: &str) -> anyhow::Result<WritePermit> {
        self.operations.write_permit(vendor_id).await
    }

    /// 只串行化连接配置与包切换的管理写路径。
    pub(crate) async fn configuration_guard(
        &self,
        vendor_id: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        self.operations.configuration_guard(vendor_id).await
    }

    pub(crate) fn acquire(
        &self,
        vendor_id: &str,
    ) -> anyhow::Result<(LoadedPlugin, Arc<VendorOperation>, i64)> {
        // 准入先于版本读取；不兼容更新在 drain 完成前不能换入新版本。
        let operation = self.operations.begin(vendor_id)?;
        let entries = self.entries.read();
        let entry = resolved_entry(&entries, vendor_id)
            .ok_or_else(|| anyhow::anyhow!("vendor plugin is not installed"))?;
        let loaded = entry
            .loaded
            .clone()
            .ok_or_else(|| anyhow::anyhow!("vendor plugin is unavailable"))?;
        anyhow::ensure!(
            loaded.descriptor().provider(vendor_id).is_some(),
            "selected vendor plugin does not support this provider"
        );
        operation.ensure_current()?;
        Ok((loaded, operation, entry.record.data_epoch))
    }

    /// Acquire a plugin package without provider admission. Plugin-scoped
    /// exports like `sync-catalog` operate on the component itself, not on
    /// one admitted provider profile.
    pub(crate) fn acquire_package(
        &self,
        package_id: &str,
    ) -> anyhow::Result<(LoadedPlugin, Arc<VendorOperation>, i64)> {
        let operation = self.operations.begin(package_id)?;
        let entries = self.entries.read();
        let entry = entries
            .get(package_id)
            .ok_or_else(|| anyhow::anyhow!("vendor plugin is not installed"))?;
        let loaded = entry
            .loaded
            .clone()
            .ok_or_else(|| anyhow::anyhow!("vendor plugin is unavailable"))?;
        operation.ensure_current()?;
        Ok((loaded, operation, entry.record.data_epoch))
    }

    pub(crate) fn descriptor(&self, vendor_id: &str) -> anyhow::Result<ProviderDescriptor> {
        resolved_entry(&self.entries.read(), vendor_id)
            .and_then(|entry| entry.loaded.as_ref())
            .and_then(|plugin| plugin.descriptor().provider(vendor_id))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("vendor plugin is unavailable"))
    }

    /// Replace the base vendor's reported provider set with the profiles the
    /// guest derived from the latest catalog snapshot. The swap only changes
    /// the effective descriptor view — the compiled component, install record
    /// version, and dedicated-vendor takeover rules are untouched.
    ///
    /// A profile absent from `providers` is retired, not removed: it stays
    /// admitted for `acquire`/descriptor resolution so saved connections keep
    /// executing, while `descriptors()`/creation paths hide it. `seeded` carries
    /// the retired set restored from disk at bootstrap. Returns the retired
    /// profiles for the host to persist.
    pub(crate) fn apply_catalog_overlay(
        &self,
        providers: Vec<ProviderDescriptor>,
        seeded: Vec<ProviderDescriptor>,
    ) -> anyhow::Result<Vec<ProviderDescriptor>> {
        let mut entries = self.entries.write();
        let entry = entries
            .get_mut("base")
            .ok_or_else(|| anyhow::anyhow!("base vendor plugin is not installed"))?;
        let loaded = entry
            .loaded
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("base vendor plugin is unavailable"))?;
        let fresh: BTreeSet<String> = providers
            .iter()
            .map(|profile| profile.provider_id.clone())
            .collect();
        let mut descriptor = loaded.descriptor().clone();
        // Every profile the effective set once carried but the fresh set drops
        // is retained as retired; a profile returning to the advertised set is
        // active again because it arrives through `providers`.
        let mut retired: BTreeMap<String, ProviderDescriptor> = descriptor
            .providers
            .iter()
            .filter(|profile| !fresh.contains(&profile.provider_id))
            .map(|profile| (profile.provider_id.clone(), profile.clone()))
            .collect();
        for profile in seeded {
            if !fresh.contains(&profile.provider_id) {
                retired
                    .entry(profile.provider_id.clone())
                    .or_insert(profile);
            }
        }
        entry.retired_profiles = retired.keys().cloned().collect();
        descriptor.providers = providers;
        descriptor.providers.extend(retired.values().cloned());
        entry.loaded = Some(loaded.with_descriptor(descriptor.clone()));
        entry.record.descriptor = serde_json::to_string(&descriptor)?;
        Ok(retired.into_values().collect())
    }

    /// Whether `vendor_id` resolves to a retired catalog profile — still
    /// admitted for existing connections but unavailable for new ones. A
    /// dedicated package installed for the same id takes over entirely and is
    /// never shadowed by the base retired set.
    pub(crate) fn is_retired_profile(&self, vendor_id: &str) -> bool {
        let entries = self.entries.read();
        if entries.contains_key(vendor_id) {
            return false;
        }
        entries
            .get("base")
            .is_some_and(|entry| entry.retired_profiles.contains(vendor_id))
    }

    pub(crate) fn descriptors(&self) -> Vec<ProviderDescriptor> {
        let entries = self.entries.read();
        let mut descriptors = Vec::new();
        for (package_id, entry) in entries.iter() {
            let Some(loaded) = &entry.loaded else {
                continue;
            };
            descriptors.extend(
                loaded
                    .descriptor()
                    .providers
                    .iter()
                    .filter(|profile| {
                        package_id != "base"
                            || (!entries.contains_key(&profile.provider_id)
                                && !entry.retired_profiles.contains(&profile.provider_id))
                    })
                    .cloned(),
            );
        }
        descriptors.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
        descriptors
    }

    fn resolution_fingerprint(&self, scope_ids: &BTreeSet<String>) -> anyhow::Result<Vec<u8>> {
        let entries = self.entries.read();
        let mut digest = Sha256::new();
        for id in scope_ids {
            digest.update(serde_json::to_vec(&(
                id,
                resolved_entry(&entries, id).map(|entry| {
                    (
                        &entry.record.vendor_id,
                        entry.record.revision,
                        entry.record.data_epoch,
                    )
                }),
            ))?);
        }
        Ok(digest.finalize().to_vec())
    }

    fn scope_ids(
        &self,
        descriptor: &VendorDescriptor,
        previous: Option<&VendorDescriptor>,
    ) -> BTreeSet<String> {
        let entries = self.entries.read();
        descriptor
            .providers
            .iter()
            .chain(
                previous
                    .into_iter()
                    .flat_map(|manifest| &manifest.providers),
            )
            .map(|profile| &profile.provider_id)
            .filter(|id| {
                descriptor.kind != VendorKind::Fallback || !entries.contains_key(id.as_str())
            })
            .cloned()
            .collect()
    }

    pub(crate) async fn preview(
        &self,
        gw: &Gateway,
        bytes: Bytes,
        source: PluginSource,
        expected_vendor: Option<&str>,
    ) -> anyhow::Result<PluginPreview> {
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_COMPONENT_BYTES,
            "plugin component must contain between 1 byte and 64 MiB"
        );
        let loaded = match (source, expected_vendor) {
            (PluginSource::Builtin, Some(vendor_id)) => self
                .bundled
                .plugins
                .get(vendor_id)
                .ok_or_else(|| anyhow::anyhow!("bundled vendor component is unavailable"))?
                .loaded
                .clone(),
            _ => self
                .runtime
                .load(&bytes)
                .await
                .map_err(|_| anyhow::anyhow!("invalid or incompatible vendor component"))?,
        };
        let descriptor = loaded.descriptor();
        if let Some(expected) = expected_vendor {
            anyhow::ensure!(
                descriptor.vendor_id == expected,
                "bundled plugin identity does not match"
            );
        }
        let _update = self.updates.lock().await;
        let old = self
            .entries
            .read()
            .get(&descriptor.vendor_id)
            .map(|entry| entry.record.clone());
        let old_descriptor = old.as_ref().and_then(recorded_descriptor);
        let mut scope_ids = self.scope_ids(descriptor, old_descriptor.as_ref());
        let recorded_data = {
            let entries = self.entries.read();
            let mut profiles = BTreeMap::new();
            for entry in entries.values() {
                let manifest = match serde_json::from_str::<RecordedDataManifest>(
                    &entry.record.descriptor,
                ) {
                    Ok(manifest) => manifest,
                    Err(_) => {
                        tracing::warn!(vendor_id = %entry.record.vendor_id, "stored vendor data compatibility metadata is invalid");
                        continue;
                    }
                };
                for profile in manifest.providers {
                    let owner = resolved_entry(&entries, &profile.provider_id);
                    if owner.is_some_and(|owner| owner.record.vendor_id == entry.record.vendor_id) {
                        if entry.record.vendor_id == descriptor.vendor_id {
                            scope_ids.insert(profile.provider_id.clone());
                        }
                        profiles.insert(profile.provider_id.clone(), profile);
                    }
                }
            }
            profiles
        };
        let resolution_fingerprint = self.resolution_fingerprint(&scope_ids)?;
        let (previous_profiles, previous_owners, previous_epoch) = {
            let entries = self.entries.read();
            let mut profiles = BTreeMap::new();
            let mut owners = BTreeSet::new();
            let mut epoch = old.as_ref().map_or(0, |record| record.data_epoch);
            let unavailable: HashMap<_, _> = entries
                .values()
                .filter(|entry| entry.loaded.is_none())
                .map(|entry| {
                    (
                        entry.record.vendor_id.as_str(),
                        recorded_descriptor(&entry.record),
                    )
                })
                .collect();
            for id in &scope_ids {
                if let Some(entry) = resolved_entry(&entries, id) {
                    owners.insert(id.clone());
                    if old.is_none() {
                        epoch = epoch.max(entry.record.data_epoch);
                    }
                    let manifest =
                        entry
                            .loaded
                            .as_ref()
                            .map(LoadedPlugin::descriptor)
                            .or_else(|| {
                                unavailable
                                    .get(entry.record.vendor_id.as_str())
                                    .and_then(Option::as_ref)
                            });
                    if let Some(profile) = manifest.and_then(|manifest| manifest.provider(id)) {
                        profiles.insert(id.clone(), profile.clone());
                    }
                }
            }
            (profiles, owners, epoch)
        };
        let states = provider_states(gw, &scope_ids).await?;
        let providers: Vec<_> = states.iter().map(|state| state.provider.clone()).collect();
        let data_incompatible = scope_ids.iter().any(|id| {
            previous_owners.contains(id)
                && descriptor.provider(id).is_some_and(|next| {
                    recorded_data.get(id).map_or_else(
                        || {
                            providers
                                .iter()
                                .any(|provider| provider.vendor.as_ref() == Some(id))
                        },
                        |previous| previous.data_compat != next.data_compat,
                    )
                })
        });
        let mut resets = Vec::new();
        let mut discarded_data = Vec::new();
        for state in &states {
            let provider = &state.provider;
            let vendor_id = provider
                .vendor
                .as_deref()
                .expect("scoped provider has a vendor");
            if !previous_owners.contains(vendor_id) {
                continue;
            }
            let Some(next) = descriptor.provider(vendor_id) else {
                // 删除 profile 使绑定不可用，不构成删除其持久化数据的授权。
                continue;
            };
            let previous = recorded_data.get(vendor_id);
            let mut retained: BTreeMap<String, serde_json::Value> =
                serde_json::from_str(&provider.adapter_credentials)?;
            let secret_keys: BTreeSet<_> = previous
                .into_iter()
                .flat_map(|previous| &previous.config_fields)
                .filter(|field| field.secret)
                .map(|field| field.key.as_str())
                .collect();
            let has_options = !serde_json::from_str::<BTreeMap<String, serde_json::Value>>(
                &provider.vendor_options,
            )?
            .is_empty()
                || retained
                    .keys()
                    .any(|key| !secret_keys.contains(key.as_str()));
            let has_credentials = !provider.api_key.is_empty()
                || state.oauth.is_some()
                || retained
                    .keys()
                    .any(|key| previous.is_none() || secret_keys.contains(key.as_str()));
            let impact = DataReset {
                provider_id: provider.id.clone(),
                options: has_options
                    && previous.is_none_or(|previous| {
                        previous.data_compat.config_fields_format
                            != next.data_compat.config_fields_format
                    }),
                credentials: has_credentials
                    && previous.is_none_or(|previous| {
                        previous.data_compat.credentials_format
                            != next.data_compat.credentials_format
                    }),
                models: state
                    .models
                    .iter()
                    .any(|model| has_plugin_model_metadata(&model.metadata))
                    && previous.is_none_or(|previous| {
                        previous.data_compat.model_metadata_format
                            != next.data_compat.model_metadata_format
                    }),
                private_state: state.private_state.is_some()
                    && previous.is_none_or(|previous| {
                        previous.data_compat.private_state_format
                            != next.data_compat.private_state_format
                    }),
            };
            let kinds: Vec<String> = [
                ("options", impact.options),
                ("credentials", impact.credentials),
                ("models", impact.models),
                ("private_state", impact.private_state),
            ]
            .into_iter()
            .filter(|(_, changed)| *changed)
            .map(|(name, _)| name.to_owned())
            .collect();
            if kinds.is_empty() {
                continue;
            }
            retained.retain(|key, _| {
                if previous.is_none() {
                    return !impact.options && !impact.credentials;
                }
                if secret_keys.contains(key.as_str()) {
                    !impact.credentials
                } else {
                    !impact.options
                }
            });
            discarded_data.push(PluginDataDiscard {
                provider: provider_view(provider),
                recovery_actions: kinds
                    .iter()
                    .filter(|kind| kind.as_str() != "private_state")
                    .cloned()
                    .collect(),
                kinds,
            });
            resets.push(ProviderReset {
                expected_vendor: vendor_id.to_owned(),
                retained_adapter_credentials: serde_json::to_string(&retained)?,
                retained_options: if impact.options {
                    "{}".to_owned()
                } else {
                    provider.vendor_options.clone()
                },
                impact,
            });
        }
        let mut projected_providers = providers.clone();
        for reset in &resets {
            if let Some(provider) = projected_providers
                .iter_mut()
                .find(|provider| provider.id == reset.impact.provider_id)
            {
                provider
                    .adapter_credentials
                    .clone_from(&reset.retained_adapter_credentials);
                provider.vendor_options.clone_from(&reset.retained_options);
                if reset.impact.credentials {
                    provider.api_key.clear();
                }
            }
        }
        let next_profiles: BTreeMap<_, _> = descriptor
            .providers
            .iter()
            .filter(|profile| scope_ids.contains(&profile.provider_id))
            .map(|profile| (profile.provider_id.clone(), profile.clone()))
            .collect();
        let permissions = declared_permissions(&next_profiles, &projected_providers)?;
        let previous_permissions = declared_permissions(&previous_profiles, &providers)?;
        let previous_origins: BTreeSet<_> =
            previous_permissions.iter().map(permission_key).collect();
        let next_origins: BTreeSet<_> = permissions.iter().map(permission_key).collect();
        let old_version = old
            .as_ref()
            .and_then(|record| Version::parse(&record.version).ok());
        let preview = PluginPreview {
            id: stravia_runtime_contract::identifier::new_id(),
            vendor_id: descriptor.vendor_id.clone(),
            name: descriptor.display_name.clone(),
            author: (!descriptor.authors.is_empty()).then(|| descriptor.authors.join(", ")),
            previous_version: old.as_ref().map(|record| record.version.clone()),
            new_version: descriptor.version.to_string(),
            target_source: source,
            is_downgrade: old_version.is_some_and(|version| descriptor.version < version),
            inherits_credentials: states.iter().any(|state| {
                state.oauth.is_some()
                    || !state.provider.api_key.is_empty()
                    || state.provider.adapter_credentials != "{}"
            }),
            affected_providers: providers.iter().map(provider_view).collect(),
            network_permissions: permissions
                .into_iter()
                .map(|mut permission| {
                    permission.added = !previous_origins.contains(&permission_key(&permission));
                    permission
                })
                .collect(),
            removed_network_permissions: previous_permissions
                .into_iter()
                .filter(|permission| !next_origins.contains(&permission_key(permission)))
                .map(|permission| permission.origin)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            discarded_data,
            affected_bindings: binding_impacts(
                gw,
                &next_profiles,
                Some(&previous_profiles),
                &providers,
                true,
            )
            .await?,
            cancels_active_operations: data_incompatible,
            active_operations: scope_ids
                .iter()
                .map(|id| self.operations.active_count(id))
                .sum(),
            affected_auth_sessions: {
                let mut sessions = 0;
                for id in &scope_ids {
                    sessions += gw.admin().affected_vendor_auth_sessions(id).await;
                }
                sessions
            },
        };
        let record = InstalledPlugin {
            vendor_id: descriptor.vendor_id.clone(),
            version: descriptor.version.to_string(),
            source: source.as_str().to_owned(),
            descriptor: serde_json::to_string(descriptor)?,
            digest: stravia_runtime_contract::protocol::ir::canonical::hash_hex(
                &Sha256::digest(&bytes).into(),
            ),
            component: bytes,
            revision: old.as_ref().map_or(Ok(1), |record| {
                record
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("plugin revisions exhausted"))
            })?,
            data_epoch: previous_epoch
                .checked_add(i64::from(data_incompatible))
                .ok_or_else(|| anyhow::anyhow!("plugin data epochs exhausted"))?,
            installed_at: chrono::Utc::now().timestamp_millis(),
        };
        let mut pending = self.pending.lock().await;
        pending.retain(|_, pending| {
            if pending.preview.target_source == PluginSource::Builtin {
                source != PluginSource::Builtin || pending.preview.vendor_id != descriptor.vendor_id
            } else {
                pending.created.elapsed() < PREVIEW_LIFETIME
            }
        });
        anyhow::ensure!(
            source == PluginSource::Builtin
                || pending
                    .values()
                    .filter(|pending| pending.preview.target_source == PluginSource::Local)
                    .count()
                    < MAX_PREVIEWS,
            "too many pending plugin updates; complete or let an existing preview expire"
        );
        pending.insert(
            preview.id.clone(),
            Pending {
                preview: preview.clone(),
                expected_revision: old.map(|record| record.revision),
                provider_fingerprint: provider_fingerprint(&states, false)?,
                data_fingerprint: provider_fingerprint(&states, true)?,
                resolution_fingerprint,
                scope_ids,
                record,
                loaded,
                resets,
                data_incompatible,
                created: Instant::now(),
            },
        );
        Ok(preview)
    }

    pub(crate) async fn confirm(
        &self,
        gw: &Gateway,
        input: ConfirmPluginUpdate,
    ) -> anyhow::Result<PluginSummary> {
        let _update = self.updates.lock().await;
        let pending = {
            let mut previews = self.pending.lock().await;
            let pending = previews
                .get(&input.preview_id)
                .ok_or_else(|| anyhow::anyhow!("plugin update preview is no longer available"))?;
            anyhow::ensure!(
                pending.preview.target_source == PluginSource::Builtin
                    || pending.created.elapsed() < PREVIEW_LIFETIME,
                "plugin update preview expired"
            );
            anyhow::ensure!(
                pending.resets.is_empty() || input.allow_data_discard,
                "explicit confirmation is required to discard incompatible plugin data"
            );
            previews
                .remove(&input.preview_id)
                .ok_or_else(|| anyhow::anyhow!("plugin update preview is no longer available"))?
        };
        let vendor_id = &pending.record.vendor_id;
        anyhow::ensure!(
            self.entries
                .read()
                .get(vendor_id)
                .map(|entry| entry.record.revision)
                == pending.expected_revision,
            "installed plugin changed; review the update again"
        );
        anyhow::ensure!(
            self.resolution_fingerprint(&pending.scope_ids)? == pending.resolution_fingerprint,
            "provider plugin ownership changed; review the update again"
        );
        anyhow::ensure!(
            provider_fingerprint(&provider_states(gw, &pending.scope_ids).await?, false)?
                == pending.provider_fingerprint,
            "provider configuration or plugin data changed; review the update again"
        );
        if pending.preview.target_source == PluginSource::Local
            && let Some(artifacts) = &self.artifacts
        {
            artifacts
                .store(&pending.record.digest, pending.record.component.clone())
                .await?;
        }
        let quiescent = if pending.data_incompatible {
            let quiescent = futures::future::try_join_all(
                pending
                    .scope_ids
                    .iter()
                    .map(|id| self.operations.cancel_and_drain(id)),
            )
            .await?;
            for id in &pending.scope_ids {
                gw.admin().cancel_vendor_sessions(id).await;
            }
            quiescent
        } else {
            Vec::new()
        };
        // 不兼容切换先排空，避免与持发布许可、正在准备连接写入的旧认证流程互锁。
        let mut configuration = Vec::with_capacity(pending.scope_ids.len());
        for id in &pending.scope_ids {
            configuration.push(self.operations.configuration_guard(id).await);
        }
        let current = provider_states(gw, &pending.scope_ids).await?;
        anyhow::ensure!(
            provider_fingerprint(&current, false)? == pending.provider_fingerprint,
            "provider configuration or plugin data changed; review the update again"
        );
        anyhow::ensure!(
            !pending.data_incompatible
                || provider_fingerprint(&current, true)? == pending.data_fingerprint,
            "plugin data changed; review the update again"
        );
        self.store
            .install(&pending.record, pending.expected_revision, &pending.resets)
            .await?;
        let vendor_id = vendor_id.clone();
        let mut record = pending.record;
        record.component = Bytes::new();
        self.entries.write().insert(
            vendor_id,
            Entry {
                record,
                loaded: Some(pending.loaded),
                retired_profiles: BTreeSet::new(),
            },
        );
        for quiescent in quiescent {
            quiescent.resume();
        }
        drop(configuration);
        self.summary(gw, &pending.preview.vendor_id).await
    }

    pub(crate) async fn uninstall(&self, gw: &Gateway, vendor_id: &str) -> anyhow::Result<()> {
        let vendor_id = vendor_id.trim();
        anyhow::ensure!(!vendor_id.is_empty(), "vendor plugin id is required");
        anyhow::ensure!(
            vendor_id != "base",
            "the embedded base plugin cannot be uninstalled"
        );

        let _update = self.updates.lock().await;
        let revision = self
            .entries
            .read()
            .get(vendor_id)
            .map(|entry| entry.record.revision)
            .ok_or_else(|| anyhow::anyhow!("vendor plugin is not installed"))?;
        // 非 base 包只拥有与 Vendor ID 同名的操作域；卸载异常插件不应依赖描述符解析或加载。
        let quiescent = self.operations.cancel_and_drain(vendor_id).await?;
        gw.admin().cancel_vendor_sessions(vendor_id).await;
        let configuration = self.operations.configuration_guard(vendor_id).await;
        self.store.uninstall(vendor_id, revision).await?;
        self.entries.write().remove(vendor_id);
        // 清除依赖旧归属的预览，防止重装后 revision 重用使旧确认再次生效；其他插件的预览保留。
        self.pending.lock().await.retain(|_, pending| {
            pending.preview.vendor_id != vendor_id && !pending.scope_ids.contains(vendor_id)
        });
        quiescent.resume();
        drop(configuration);
        Ok(())
    }

    pub(crate) async fn restore(
        &self,
        gw: &Gateway,
        vendor_id: &str,
    ) -> anyhow::Result<PluginPreview> {
        let bundle = self
            .bundled
            .plugins
            .get(vendor_id)
            .ok_or_else(|| anyhow::anyhow!("no bundled version exists for this vendor"))?;
        self.preview(
            gw,
            Bytes::from_static(bundle.component),
            PluginSource::Builtin,
            Some(vendor_id),
        )
        .await
    }

    pub(crate) async fn reconcile_bundled(&self, gw: &Gateway) -> anyhow::Result<()> {
        // 首次迁移先建立专属归属，不把尚未安装的专属接入误认成 base 的旧数据格式。
        let mut bundles: Vec<_> = self.bundled.plugins.iter().collect();
        bundles.sort_by_key(|(_, bundle)| bundle.loaded.descriptor().kind == VendorKind::Fallback);
        for (vendor_id, bundle) in bundles {
            let old = self
                .entries
                .read()
                .get(vendor_id)
                .map(|entry| entry.record.clone());
            if old
                .as_ref()
                .is_some_and(|record| record.source != "builtin")
            {
                continue;
            }
            if let Some(old) = &old
                && Version::parse(&old.version)
                    .is_ok_and(|version| bundle.loaded.descriptor().version <= version)
            {
                continue;
            }
            let preview = self.restore(gw, vendor_id).await?;
            let auto_reset = vendor_id.as_str() == "base";
            if auto_reset || preview.discarded_data.is_empty() {
                let discarded_data = preview.discarded_data;
                self.confirm(
                    gw,
                    ConfirmPluginUpdate {
                        preview_id: preview.id,
                        allow_data_discard: auto_reset,
                    },
                )
                .await?;
                for discard in discarded_data {
                    tracing::warn!(
                        vendor_id = %vendor_id,
                        provider_id = %discard.provider.id,
                        kinds = ?discard.kinds,
                        "bundled plugin upgrade reset incompatible provider data"
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn list(&self, gw: &Gateway) -> anyhow::Result<Vec<PluginSummary>> {
        let mut ids: Vec<_> = self.entries.read().keys().cloned().collect();
        ids.sort();
        let mut summaries = Vec::with_capacity(ids.len());
        for id in ids {
            summaries.push(self.summary(gw, &id).await?);
        }
        Ok(summaries)
    }

    async fn summary(&self, gw: &Gateway, vendor_id: &str) -> anyhow::Result<PluginSummary> {
        let (record, loaded) = {
            let entries = self.entries.read();
            let entry = entries
                .get(vendor_id)
                .ok_or_else(|| anyhow::anyhow!("vendor plugin is not installed"))?;
            (entry.record.clone(), entry.loaded.clone())
        };
        let descriptor = recorded_descriptor(&record);
        let mut scope_ids = match &descriptor {
            Some(descriptor) => self.scope_ids(descriptor, None),
            None if vendor_id == "base" => BTreeSet::new(),
            None => BTreeSet::from([vendor_id.to_owned()]),
        };
        let providers = if vendor_id == "base" {
            let mut providers = gw.storage.providers().list().await?;
            let entries = self.entries.read();
            scope_ids.extend(
                providers
                    .iter()
                    .filter_map(|provider| provider.vendor.as_ref())
                    .filter(|id| !entries.contains_key(*id))
                    .cloned(),
            );
            providers.retain(|provider| {
                provider
                    .vendor
                    .as_ref()
                    .is_some_and(|id| scope_ids.contains(id))
            });
            providers.sort_by(|left, right| left.id.cmp(&right.id));
            providers
        } else {
            vendor_providers(gw, &scope_ids).await?
        };
        let profiles: BTreeMap<_, _> = descriptor
            .as_ref()
            .into_iter()
            .flat_map(|manifest| &manifest.providers)
            .filter(|profile| scope_ids.contains(&profile.provider_id))
            .map(|profile| (profile.provider_id.clone(), profile.clone()))
            .collect();
        let pending_update = self
            .pending
            .lock()
            .await
            .values()
            .find(|pending| {
                pending.preview.vendor_id == vendor_id
                    && pending.preview.target_source == PluginSource::Builtin
            })
            .map(|pending| pending.preview.clone());
        let builtin_version = self
            .bundled
            .plugins
            .get(vendor_id)
            .map(|bundle| bundle.loaded.descriptor().version.to_string());
        Ok(PluginSummary {
            vendor_id: vendor_id.to_owned(),
            name: descriptor.as_ref().map_or_else(
                || vendor_id.to_owned(),
                |descriptor| descriptor.display_name.clone(),
            ),
            version: record.version,
            source: match record.source.as_str() {
                "builtin" => PluginSource::Builtin,
                "local" => PluginSource::Local,
                _ => anyhow::bail!("invalid recorded plugin source"),
            },
            status: if loaded.is_none() {
                "unavailable"
            } else if pending_update.is_some() {
                "pending_update"
            } else {
                "ready"
            }
            .to_owned(),
            error: loaded
                .is_none()
                .then(|| "Installed component cannot run on this host".to_owned()),
            builtin_version,
            capabilities: profiles
                .values()
                .flat_map(|profile| &profile.capabilities)
                .map(|capability| capability.as_str().to_owned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            affected_bindings: binding_impacts(gw, &profiles, None, &providers, loaded.is_some())
                .await?,
            pending_update,
        })
    }
}

/// 安装记录本身建立接管关系；加载失败也不得绕过记录去调用基础插件。
fn resolved_entry<'a>(entries: &'a HashMap<String, Entry>, provider_id: &str) -> Option<&'a Entry> {
    entries.get(provider_id).or_else(|| entries.get("base"))
}

fn provider_view(provider: &Provider) -> PluginProvider {
    PluginProvider {
        id: provider.id.clone(),
        name: provider.name.clone(),
    }
}

async fn vendor_providers(
    gw: &Gateway,
    scope_ids: &BTreeSet<String>,
) -> anyhow::Result<Vec<Provider>> {
    let mut providers: Vec<_> = gw
        .storage
        .providers()
        .list()
        .await?
        .into_iter()
        .filter(|provider| {
            provider
                .vendor
                .as_ref()
                .is_some_and(|id| scope_ids.contains(id))
        })
        .collect();
    providers.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(providers)
}

fn recorded_descriptor(record: &InstalledPlugin) -> Option<VendorDescriptor> {
    match serde_json::from_str(&record.descriptor) {
        Ok(descriptor) => Some(descriptor),
        Err(_) => {
            tracing::warn!(vendor_id = %record.vendor_id, "stored vendor descriptor is invalid");
            None
        }
    }
}

async fn provider_states(
    gw: &Gateway,
    scope_ids: &BTreeSet<String>,
) -> anyhow::Result<Vec<ProviderState>> {
    let providers = vendor_providers(gw, scope_ids).await?;
    let mut states = Vec::with_capacity(providers.len());
    for provider in providers {
        let oauth = gw.storage.oauth_credentials().get(&provider.id).await?;
        let mut models = gw
            .storage
            .provider_models()
            .list_for_provider(&provider.id)
            .await?;
        models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
        let private_state = gw
            .vendor_plugins
            .store
            .read_private_state(
                provider
                    .vendor
                    .as_deref()
                    .expect("scoped provider has a vendor"),
                &provider.id,
            )
            .await?;
        states.push(ProviderState {
            provider,
            oauth,
            models,
            private_state,
        });
    }
    Ok(states)
}

fn provider_fingerprint(states: &[ProviderState], include_data: bool) -> anyhow::Result<Vec<u8>> {
    let mut digest = Sha256::new();
    for state in states {
        digest.update(serde_json::to_vec(&(
            &state.provider,
            &state.provider.api_key,
            &state.provider.adapter_credentials,
        ))?);
        if include_data {
            // 确认授权的是数据类别，不是某次 token、发现结果或私有计数器的字节快照。
            // 新出现的数据仍须重新预览，已披露类别的正常写入不会使更新永远无法完成。
            digest.update(serde_json::to_vec(&(
                state.oauth.is_some(),
                state
                    .models
                    .iter()
                    .any(|model| has_plugin_model_metadata(&model.metadata)),
                state.private_state.is_some(),
            ))?);
        }
    }
    Ok(digest.finalize().to_vec())
}

fn permission_key(permission: &PluginNetworkPermission) -> (String, Option<String>) {
    (permission.origin.clone(), permission.provider_id.clone())
}

pub(crate) fn declared_permissions(
    profiles: &BTreeMap<String, ProviderDescriptor>,
    providers: &[Provider],
) -> anyhow::Result<Vec<PluginNetworkPermission>> {
    let empty = BTreeMap::new();
    let mut permissions = Vec::new();
    let mut static_origins = BTreeSet::new();
    for profile in profiles.values() {
        permissions.extend(
            resolve_permissions(profile, None, None, None, &empty, &empty)?
                .into_iter()
                .filter(|grant| static_origins.insert(grant.origin.clone()))
                .map(|grant| PluginNetworkPermission {
                    origin: grant.origin,
                    provider_id: None,
                    configuration_field: grant.configuration_field,
                    added: false,
                }),
        );
    }
    for provider in providers {
        let Some(descriptor) = provider.vendor.as_ref().and_then(|id| profiles.get(id)) else {
            continue;
        };
        let options: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&provider.vendor_options)?;
        let mut credentials: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&provider.adapter_credentials)?;
        if provider.auth_mode != "oauth" && !provider.api_key.trim().is_empty() {
            credentials
                .entry("apiKey".into())
                .or_insert_with(|| serde_json::Value::String(provider.api_key.trim().to_owned()));
        }
        permissions.extend(
            resolve_permissions(
                descriptor,
                Some(&provider.base_url),
                provider.models_source.as_deref(),
                provider.static_models.as_deref(),
                &options,
                &credentials,
            )?
            .into_iter()
            .filter(|grant| grant.connection_scoped)
            .map(|grant| PluginNetworkPermission {
                origin: grant.origin,
                provider_id: Some(provider.id.clone()),
                configuration_field: grant.configuration_field,
                added: false,
            }),
        );
    }
    Ok(permissions)
}

async fn binding_impacts(
    gw: &Gateway,
    profiles: &BTreeMap<String, ProviderDescriptor>,
    previous: Option<&BTreeMap<String, ProviderDescriptor>>,
    providers: &[Provider],
    available: bool,
) -> anyhow::Result<Vec<PluginBindingImpact>> {
    use stravia_vendor_sdk::Capability;

    if providers.is_empty() {
        return Ok(Vec::new());
    }
    let mut bound_capabilities: HashMap<String, BTreeSet<Capability>> = HashMap::new();
    if let Some(raw) = gw
        .storage
        .settings()
        .get(stravia_web_search::WEB_SEARCH_CONFIG_KEY)
        .await?
    {
        let config: stravia_web_search::WebSearchConfig = serde_json::from_str(&raw)?;
        match config.backend {
            Some(stravia_web_search::WebSearchBackendDraft::External { route_id: Some(id) }) => {
                bound_capabilities
                    .entry(id)
                    .or_default()
                    .insert(Capability::Search);
            }
            Some(stravia_web_search::WebSearchBackendDraft::Local { model_id: Some(id) }) => {
                bound_capabilities
                    .entry(id)
                    .or_default()
                    .insert(Capability::Infer);
            }
            _ => {}
        }
    }
    if let Some(id) = crate::media_generation::config::load(gw)
        .await?
        .image
        .route_id
    {
        bound_capabilities
            .entry(id)
            .or_default()
            .insert(Capability::MediaImage);
    }
    let providers: HashMap<_, _> = providers
        .iter()
        .map(|provider| (provider.id.as_str(), provider))
        .collect();
    let mut impacts = Vec::new();
    for route in gw.storage.routes().list().await? {
        for target in route.targets {
            let Some(provider) = providers.get(target.provider_id().as_str()) else {
                continue;
            };
            let descriptor = provider.vendor.as_ref().and_then(|id| profiles.get(id));
            let previous_descriptor = provider
                .vendor
                .as_ref()
                .and_then(|id| previous.unwrap_or(profiles).get(id));
            let channel = descriptor.and_then(|descriptor| {
                descriptor
                    .channels
                    .iter()
                    .find(|channel| Some(channel.id.as_str()) == provider.channel.as_deref())
            });
            let previous_channel = previous_descriptor.and_then(|descriptor| {
                descriptor
                    .channels
                    .iter()
                    .find(|channel| Some(channel.id.as_str()) == provider.channel.as_deref())
            });
            let mut required = bound_capabilities
                .get(route.model_id.as_str())
                .cloned()
                .unwrap_or_default();
            if target.model().is_none() {
                required.insert(Capability::Search);
            } else if required.is_empty()
                || previous_channel
                    .is_some_and(|channel| channel.capabilities.contains(&Capability::Infer))
            {
                required.insert(Capability::Infer);
            }
            for capability in required {
                let supported = available
                    && channel.is_some_and(|channel| {
                        channel.capabilities.contains(&capability)
                            && match capability {
                                Capability::Infer => target.model().is_some(),
                                Capability::Search => {
                                    !channel.search_model_required
                                        || target.model().is_some_and(|model| {
                                            !model.trim().is_empty() && model.as_str() != "*"
                                        })
                                }
                                Capability::MediaImage => target.model().is_some_and(|model| {
                                    !model.trim().is_empty() && model.as_str() != "*"
                                }),
                                _ => true,
                            }
                    });
                if !supported {
                    impacts.push(PluginBindingImpact {
                        route_id: route.model_id.clone().into(),
                        provider_id: provider.id.clone(),
                        upstream_model: target.model().cloned().map(Into::into),
                        capability: capability.as_str().to_owned(),
                    });
                }
            }
        }
    }
    Ok(impacts)
}
