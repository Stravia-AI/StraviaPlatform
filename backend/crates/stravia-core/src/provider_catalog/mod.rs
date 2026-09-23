use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Digest;
use tokio::sync::{Mutex, RwLock};

use crate::provider_models::{ProviderModelMetadata, model_id_match_key};

mod types;
pub use types::*;

mod parse;
mod persist;
mod source;
#[cfg(test)]
mod tests;

use parse::*;
use persist::*;
pub(crate) use source::icon_content_type;
pub use source::{CatalogSource, HttpCatalogSource};

pub const CATALOG_BASE_URL: &str = "https://models.stravia.cn";
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_VERSION_BYTES: usize = 64 * 1024;
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOGO_BYTES: usize = 512 * 1024;
const LOGO_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_DIRECTORY: &str = "catalog";
const GENERATIONS_DIRECTORY: &str = "generations";
const SCOPES_DIRECTORY: &str = "scopes";
const ACTIVE_MANIFEST_FILE: &str = "active.json";
const LOGO_DIRECTORY: &str = "logos";
const FAVICON_DIRECTORY: &str = "favicons";
const BUILTIN_PROVIDERS: &str = include_str!("../../assets/providers.stravia.json");
const BUILTIN_CANONICAL_MODELS: &str = include_str!("../../assets/canonical-models.stravia.json");
const BOOTSTRAP_REVISION: &str = "bootstrap";
const BOOTSTRAP_GENERATED_AT: &str = "built-in";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogManifest {
    /// Revision of the guest-owned provider index (`providers.json`).
    providers: CatalogVersion,
    /// Revision of the host-owned Canonical Model index (`models.json`).
    canonical_models: CatalogVersion,
}

/// Pre-split manifest written by the single-generation layout; both documents
/// lived in `generations/<revision>/`.
#[derive(Debug, Deserialize)]
struct LegacyCatalogManifest {
    revision: String,
    generated_at: String,
}

#[derive(Debug, Clone)]
struct CatalogSnapshot {
    /// Canonical Model index version (`models.json`).
    version: CatalogVersion,
    /// Provider index version (`providers.json`), owned by the base guest.
    providers_version: CatalogVersion,
    providers: Vec<CatalogProvider>,
    providers_raw: Value,
    canonical_models: BTreeMap<String, Value>,
    canonical_summaries: Vec<CanonicalModelSummary>,
}

#[derive(Clone)]
pub struct ProviderCatalog {
    data_dir: PathBuf,
    source: Arc<dyn CatalogSource>,
    snapshot: Arc<RwLock<CatalogSnapshot>>,
    /// Serializes manifest rewrites and snapshot swaps across both documents.
    persist_lock: Arc<Mutex<()>>,
}

impl ProviderCatalog {
    pub fn new(
        data_dir: impl AsRef<Path>,
        catalog_base_url: Option<String>,
    ) -> anyhow::Result<Self> {
        Self::with_source(
            data_dir,
            Arc::new(HttpCatalogSource::new(catalog_base_url)?),
        )
    }

    pub fn with_source(
        data_dir: impl AsRef<Path>,
        source: Arc<dyn CatalogSource>,
    ) -> anyhow::Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let snapshot = match load_active_generation(&data_dir) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => bootstrap_snapshot()?,
            Err(error) => {
                tracing::warn!(error = %error, "ignoring invalid Provider Catalog generation");
                bootstrap_snapshot()?
            }
        };
        Ok(Self::from_snapshot(data_dir, source, snapshot))
    }

    fn from_snapshot(
        data_dir: PathBuf,
        source: Arc<dyn CatalogSource>,
        snapshot: CatalogSnapshot,
    ) -> Self {
        Self {
            data_dir,
            source,
            snapshot: Arc::new(RwLock::new(snapshot)),
            persist_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Same catalog state behind a different remote source. The shared
    /// snapshot and persist lock keep `VendorCatalogSync`'s clone observing
    /// the same generations; tests stub the host-owned fetches without
    /// replacing the catalog instance.
    pub fn with_source_override(&self, source: Arc<dyn CatalogSource>) -> Self {
        Self {
            data_dir: self.data_dir.clone(),
            source,
            snapshot: Arc::clone(&self.snapshot),
            persist_lock: Arc::clone(&self.persist_lock),
        }
    }

    /// Catalog profiles the confirmed snapshot dropped, kept admitted for
    /// existing connections. A corrupt retired set only loses tombstones —
    /// profiles still fetch fresh on the next overlay — so a bad file degrades
    /// to empty rather than blocking startup.
    pub(crate) async fn retired_profiles(&self) -> Vec<stravia_vendor_sdk::ProviderDescriptor> {
        persist::load_retired_profiles(&self.data_dir).unwrap_or_else(|error| {
            tracing::warn!(error = %error, "ignoring invalid retired Provider Catalog profiles");
            Vec::new()
        })
    }

    pub(crate) async fn persist_retired_profiles(
        &self,
        profiles: &[stravia_vendor_sdk::ProviderDescriptor],
    ) -> anyhow::Result<()> {
        let _guard = self.persist_lock.lock().await;
        persist::persist_retired_profiles(&self.data_dir, profiles)
    }

    /// Upstream catalog membership is decided by the raw index keys, not the
    /// parsed subset — the guest may implement entries this instance cannot
    /// brand.
    pub(crate) async fn contains_provider(&self, provider_id: &str) -> bool {
        self.snapshot
            .read()
            .await
            .providers_raw
            .get(provider_id)
            .is_some()
    }

    /// Render the catalog against the provider profiles actually available in
    /// this instance. A profile's `provider_id` is the selectable connection
    /// identity; `catalog_id` only links it to upstream branding and model
    /// metadata. Refreshing the remote catalog never mutates saved Providers.
    pub async fn providers(
        &self,
        descriptors: &[stravia_vendor_sdk::ProviderDescriptor],
    ) -> CatalogProviderList {
        let snapshot = self.snapshot.read().await;
        let catalog_by_id = snapshot
            .providers
            .iter()
            .map(|catalog| (catalog.id.as_str(), catalog))
            .collect::<BTreeMap<_, _>>();
        let mut providers = descriptors
            .iter()
            .map(|descriptor| {
                descriptor
                    .catalog_id
                    .as_deref()
                    .and_then(|catalog_id| catalog_by_id.get(catalog_id).copied())
                    .map_or_else(
                        || provider_from_descriptor(descriptor),
                        |catalog| bind_catalog_provider(catalog, descriptor),
                    )
            })
            .collect::<Vec<_>>();
        providers.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        CatalogProviderList {
            revision: snapshot.providers_version.revision.clone(),
            generated_at: snapshot.providers_version.generated_at.clone(),
            providers,
        }
    }

    pub async fn resolve_channel(
        &self,
        provider_id: &str,
        channel_id: &str,
        fingerprint: &str,
        descriptors: &[stravia_vendor_sdk::ProviderDescriptor],
    ) -> anyhow::Result<(CatalogProvider, CatalogChannel)> {
        let providers = self.providers(descriptors).await;
        let provider = providers
            .providers
            .into_iter()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| CatalogError::ProviderNotFound {
                provider_id: provider_id.to_owned(),
            })?;
        let channel = provider
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .cloned()
            .ok_or_else(|| CatalogError::ChannelNotFound {
                provider_id: provider_id.to_owned(),
                channel_id: channel_id.to_owned(),
            })?;
        if channel.fingerprint != fingerprint {
            return Err(CatalogError::ChannelChanged {
                provider_id: provider_id.to_owned(),
                channel_id: channel_id.to_owned(),
            }
            .into());
        }
        Ok((provider, channel))
    }

    pub async fn canonical_models(&self) -> CanonicalModelList {
        let snapshot = self.snapshot.read().await;
        CanonicalModelList {
            revision: snapshot.version.revision.clone(),
            generated_at: snapshot.version.generated_at.clone(),
            models: snapshot.canonical_summaries.clone(),
        }
    }

    pub async fn canonical_model(&self, id: &str) -> anyhow::Result<Value> {
        validate_canonical_model_id(id)?;
        let snapshot = self.snapshot.read().await;
        snapshot
            .canonical_models
            .get(id)
            .cloned()
            .ok_or_else(|| CatalogError::ModelNotFound { id: id.to_string() }.into())
    }

    pub async fn canonical_model_matching_upstream_id(&self, model_id: &str) -> Option<Value> {
        let snapshot = self.snapshot.read().await;
        canonical_template_for_upstream_id(&snapshot.canonical_models, model_id)
    }

    /// Last-good provider index body plus its version, fed back to the guest
    /// as the fallback snapshot on the next `sync-catalog` call.
    pub(crate) async fn provider_index_snapshot(&self) -> (String, CatalogVersion) {
        let snapshot = self.snapshot.read().await;
        (
            serde_json::to_string(&snapshot.providers_raw)
                .expect("provider index snapshot serializes"),
            snapshot.providers_version.clone(),
        )
    }

    pub(crate) async fn provider_count(&self) -> usize {
        self.snapshot.read().await.providers.len()
    }

    /// Disk-cached provider scope under the active index revision. `None`
    /// means the caller must refresh through the guest-owned sync path;
    /// `ProviderNotFound` means the id is absent from the upstream index.
    pub(crate) async fn cached_provider_scope(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<CatalogProviderScope>> {
        validate_provider_id(provider_id)?;
        let snapshot = self.snapshot.read().await;
        ensure_catalog_provider(&snapshot, provider_id)?;
        let revision = snapshot.providers_version.revision.clone();
        drop(snapshot);
        load_verified_scope(&self.data_dir, &revision, provider_id)
    }

    /// Validate and persist a guest-fetched scope body under the active index
    /// revision. The body was fetched by `sync-catalog` in the revision the
    /// snapshot already confirms; a provider absent from the index cannot
    /// acquire a scope.
    pub(crate) async fn install_provider_scope(
        &self,
        provider_id: &str,
        body: &[u8],
    ) -> anyhow::Result<CatalogProviderScope> {
        let snapshot = self.snapshot.read().await;
        ensure_catalog_provider(&snapshot, provider_id)?;
        let revision = snapshot.providers_version.revision.clone();
        drop(snapshot);
        let scope = parse_scope(body, &revision, provider_id)?;
        persist_scope(&self.data_dir, &scope)?;
        Ok(scope)
    }

    /// Persist a guest-validated provider index body as the active generation
    /// and swap the in-memory branding/branding snapshot. The manifest only
    /// advances after the body parses, so failures keep the last-good index.
    /// Returns whether the active providers revision changed.
    pub(crate) async fn install_provider_index(
        &self,
        body: &str,
        revision: String,
        generated_at: String,
    ) -> anyhow::Result<bool> {
        let version = CatalogVersion {
            revision,
            generated_at,
        };
        validate_version(&version)?;
        let providers_raw: Value =
            serde_json::from_str(body).context("decode provider index JSON")?;
        let providers = parse_providers(&providers_raw)?;
        let _guard = self.persist_lock.lock().await;
        let mut snapshot = self.snapshot.write().await;
        let changed = snapshot.providers_version.revision != version.revision;
        persist_provider_generation(&self.data_dir, &providers_raw, &version)?;
        snapshot.providers_version = version;
        snapshot.providers = providers;
        snapshot.providers_raw = providers_raw;
        Ok(changed)
    }

    /// Host-owned Canonical Model index refresh. Provider index data and the
    /// guest-owned profile set are untouched; a failed fetch or parse keeps
    /// the last-good generation.
    pub async fn refresh_canonical(&self) -> anyhow::Result<CatalogRefreshSummary> {
        let version = self.source.fetch_version().await?;
        validate_version(&version)?;
        {
            let snapshot = self.snapshot.read().await;
            if version.revision == snapshot.version.revision {
                return Ok(self.summary(false).await);
            }
        }

        let canonical_models_body = self.source.fetch_canonical_models().await?;
        let canonical_models = parse_canonical_models(&canonical_models_body)?;
        let confirmed = self.source.fetch_version().await?;
        validate_version(&confirmed)?;
        if confirmed.revision != version.revision {
            bail!("catalog revision changed while downloading global indexes; retry the refresh");
        }

        let _guard = self.persist_lock.lock().await;
        let mut snapshot = self.snapshot.write().await;
        persist_canonical_generation(&self.data_dir, &canonical_models, &version)?;
        snapshot.canonical_summaries = canonical_summaries(&canonical_models);
        snapshot.canonical_models = canonical_models;
        snapshot.version = version;
        Ok(self.summary_locked(&snapshot, true))
    }

    pub async fn models(
        &self,
        provider_id: &str,
        channel_id: &str,
        scope: CatalogProviderScope,
    ) -> anyhow::Result<CatalogModelList> {
        let provider = self.resolve_provider(provider_id, channel_id).await?;
        let mut models = scope
            .models
            .into_iter()
            .map(|source| parse_catalog_model(provider_id, &provider.protocol, &source.metadata))
            .collect::<anyhow::Result<Vec<_>>>()?;
        models.sort_by(model_sort_order);
        Ok(CatalogModelList {
            revision: scope.revision,
            models,
        })
    }

    pub fn model_source(
        &self,
        provider_id: &str,
        model_id: &str,
        scope: CatalogProviderScope,
    ) -> anyhow::Result<CatalogModelSource> {
        let model_id = model_id.trim();
        scope
            .models
            .into_iter()
            .find(|source| source.metadata.get("id").and_then(Value::as_str) == Some(model_id))
            .ok_or_else(|| {
                CatalogError::EntryNotFound {
                    provider_id: provider_id.to_string(),
                    model_id: model_id.to_string(),
                }
                .into()
            })
    }

    /// Catalog logo for one stable provider identity (`catalog_id` or
    /// `provider_id`, never a connection UUID). The id does not have to be a
    /// current catalog member — dedicated profiles map onto catalog assets the
    /// index may not list.
    pub async fn logo(&self, provider_id: &str) -> anyhow::Result<Vec<u8>> {
        validate_provider_id(provider_id)?;
        let path = logo_path(&self.data_dir, provider_id);
        if file_is_fresh(&path, LOGO_TTL) {
            return std::fs::read(path).context("read provider logo cache");
        }
        match self.source.fetch_logo(provider_id).await {
            Ok(body) => match cache_logo_body(&path, &body) {
                Ok(()) => Ok(body),
                Err(error) => match std::fs::read(&path) {
                    Ok(stale) => {
                        tracing::debug!(provider_id, error = %error, "serving stale provider logo after cache refresh failure");
                        Ok(stale)
                    }
                    Err(_) => Err(error),
                },
            },
            Err(error) => match std::fs::read(&path) {
                Ok(stale) => {
                    tracing::debug!(provider_id, error = %error, "serving stale provider logo after fetch failure");
                    Ok(stale)
                }
                Err(_) => Err(error),
            },
        }
    }

    /// Website icon for one already-selected origin (`{origin}/favicon.ico`
    /// only — no HTML discovery). Shares the logo cache TTL and
    /// stale-if-error behavior.
    pub async fn favicon(&self, origin: &str) -> anyhow::Result<Vec<u8>> {
        let path = favicon_path(&self.data_dir, origin)?;
        if file_is_fresh(&path, LOGO_TTL) {
            return std::fs::read(path).context("read provider favicon cache");
        }
        match self.source.fetch_favicon(origin).await {
            Ok(body) => match atomic_write(&path, &body) {
                Ok(()) => Ok(body),
                Err(error) => match std::fs::read(&path) {
                    Ok(stale) => {
                        tracing::debug!(origin, error = %error, "serving stale provider favicon after cache write failure");
                        Ok(stale)
                    }
                    Err(_) => Err(error),
                },
            },
            Err(error) => match std::fs::read(&path) {
                Ok(stale) => {
                    tracing::debug!(origin, error = %error, "serving stale provider favicon after fetch failure");
                    Ok(stale)
                }
                Err(_) => Err(error),
            },
        }
    }

    async fn resolve_provider(
        &self,
        provider_id: &str,
        channel_id: &str,
    ) -> anyhow::Result<CatalogProvider> {
        let provider = self
            .snapshot
            .read()
            .await
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
            .ok_or_else(|| CatalogError::ProviderNotFound {
                provider_id: provider_id.to_string(),
            })?;
        if !provider
            .channels
            .iter()
            .any(|channel| channel.id == channel_id)
        {
            return Err(CatalogError::ChannelNotFound {
                provider_id: provider_id.to_string(),
                channel_id: channel_id.to_string(),
            }
            .into());
        }
        Ok(provider)
    }

    async fn summary(&self, changed: bool) -> CatalogRefreshSummary {
        let snapshot = self.snapshot.read().await;
        self.summary_locked(&snapshot, changed)
    }

    fn summary_locked(&self, snapshot: &CatalogSnapshot, changed: bool) -> CatalogRefreshSummary {
        CatalogRefreshSummary {
            revision: snapshot.version.revision.clone(),
            generated_at: snapshot.version.generated_at.clone(),
            provider_count: snapshot.providers.len(),
            model_count: snapshot.canonical_models.len(),
            changed,
        }
    }
}

fn descriptor_auth_mode(
    channel: &stravia_vendor_sdk::ChannelDescriptor,
) -> Option<CatalogAuthMode> {
    channel.auth.as_ref().map(|auth| match auth.flow {
        stravia_vendor_sdk::AuthFlow::AuthorizationCode
        | stravia_vendor_sdk::AuthFlow::DeviceCode => CatalogAuthMode::OAuth,
        stravia_vendor_sdk::AuthFlow::Manual => CatalogAuthMode::SetupToken,
    })
}

fn bind_catalog_provider(
    catalog: &CatalogProvider,
    descriptor: &stravia_vendor_sdk::ProviderDescriptor,
) -> CatalogProvider {
    let channels = descriptor
        .channels
        .iter()
        .map(|definition| {
            let catalog_channel = catalog
                .channels
                .iter()
                .find(|channel| channel.id == definition.id);
            let protocol = definition
                .protocol
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    catalog_channel
                        .map(|channel| channel.protocol.as_str())
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let base_url = definition
                .default_base_url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    catalog_channel
                        .map(|channel| channel.base_url.as_str())
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let auth_mode =
                descriptor_auth_mode(definition).unwrap_or(CatalogAuthMode::OptionalApiKey);
            channel(
                &descriptor.provider_id,
                &definition.id,
                definition.name.english_text(),
                &protocol,
                &base_url,
                auth_mode,
            )
        })
        .collect::<Vec<_>>();
    let protocol = channels
        .first()
        .map(|channel| channel.protocol.clone())
        .unwrap_or_else(|| catalog.protocol.clone());
    let base_url = channels
        .first()
        .map(|channel| channel.base_url.clone())
        .unwrap_or_else(|| catalog.base_url.clone());
    CatalogProvider {
        id: descriptor.provider_id.clone(),
        catalog_id: Some(catalog.id.clone()),
        name: descriptor.display_name.clone(),
        documentation_url: catalog.documentation_url.clone(),
        npm: catalog.npm.clone(),
        protocol,
        base_url,
        channels,
    }
}

fn provider_from_descriptor(
    descriptor: &stravia_vendor_sdk::ProviderDescriptor,
) -> CatalogProvider {
    let channels = descriptor
        .channels
        .iter()
        .map(|definition| {
            channel(
                &descriptor.provider_id,
                &definition.id,
                definition.name.english_text(),
                definition.protocol.as_deref().unwrap_or_default(),
                definition.default_base_url.as_deref().unwrap_or_default(),
                descriptor_auth_mode(definition).unwrap_or(CatalogAuthMode::OptionalApiKey),
            )
        })
        .collect::<Vec<_>>();
    CatalogProvider {
        id: descriptor.provider_id.clone(),
        catalog_id: descriptor.catalog_id.clone(),
        name: descriptor.display_name.clone(),
        documentation_url: None,
        npm: String::new(),
        protocol: channels
            .first()
            .map(|channel| channel.protocol.clone())
            .unwrap_or_default(),
        base_url: channels
            .first()
            .map(|channel| channel.base_url.clone())
            .unwrap_or_default(),
        channels,
    }
}

fn canonical_summaries(canonical_models: &BTreeMap<String, Value>) -> Vec<CanonicalModelSummary> {
    let mut summaries: Vec<_> = canonical_models
        .iter()
        .map(|(id, metadata)| CanonicalModelSummary {
            id: id.clone(),
            name: metadata
                .get("name")
                .and_then(Value::as_str)
                .expect("validated Canonical Model name")
                .to_string(),
        })
        .collect();
    summaries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    summaries
}

/// 上游模型 ID 优先精确匹配 Canonical ID；否则按「`/` 最右段 + 忽略大小写」
/// 归一后匹配，且仅在归一键唯一时采用该模板（与 ProviderModelStore::find 同语义）。
///
/// 上游清单 ID 的命名空间与大小写不必与 Canonical ID 一致
/// （`MiniMaxAI/MiniMax-M2.7` vs `minimax/MiniMax-M2.7`），
/// 因此宽松回退与 b95e5a4 的读取侧匹配共用 model_id_match_key。
fn canonical_template_for_upstream_id(
    canonical_models: &BTreeMap<String, Value>,
    model_id: &str,
) -> Option<Value> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    if let Some(value) = canonical_models.get(model_id) {
        return Some(value.clone());
    }
    let needle = model_id_match_key(model_id);
    let mut matched = None;
    for (id, value) in canonical_models {
        if model_id_match_key(id) != needle {
            continue;
        }
        if matched.is_some() {
            return None;
        }
        matched = Some(value.clone());
    }
    matched
}

/// Persist a fetched logo after content validation. Validation and write
/// failures are refresh failures for the caller's stale-if-error fallback,
/// not reasons to drop a good cached icon.
fn cache_logo_body(path: &Path, body: &[u8]) -> anyhow::Result<()> {
    validate_svg(body)?;
    atomic_write(path, body)
}
