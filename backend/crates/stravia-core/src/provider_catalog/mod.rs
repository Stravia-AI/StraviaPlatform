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

use parse::*;
use persist::*;
pub use source::{CatalogSource, HttpCatalogSource};

pub const CATALOG_BASE_URL: &str = "https://models.stravia.cn";
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_VERSION_BYTES: usize = 64 * 1024;
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCOPE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOGO_BYTES: usize = 512 * 1024;
const LOGO_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_DIRECTORY: &str = "catalog";
const GENERATIONS_DIRECTORY: &str = "generations";
const SCOPES_DIRECTORY: &str = "scopes";
const ACTIVE_MANIFEST_FILE: &str = "active.json";
const LOGO_DIRECTORY: &str = "logos";
const BUILTIN_PROVIDERS: &str = include_str!("../../assets/providers.stravia.json");
const BUILTIN_CANONICAL_MODELS: &str = include_str!("../../assets/canonical-models.stravia.json");
const BOOTSTRAP_REVISION: &str = "bootstrap";
const BOOTSTRAP_GENERATED_AT: &str = "built-in";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogManifest {
    revision: String,
    generated_at: String,
}

#[derive(Debug, Clone)]
struct CatalogSnapshot {
    version: CatalogVersion,
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
    refresh_lock: Arc<Mutex<()>>,
    scope_refresh_lock: Arc<Mutex<()>>,
    generation: Arc<AtomicU64>,
}

impl ProviderCatalog {
    pub fn new(data_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::with_source(data_dir, Arc::new(HttpCatalogSource::new()?))
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
            refresh_lock: Arc::new(Mutex::new(())),
            scope_refresh_lock: Arc::new(Mutex::new(())),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn contains_provider(&self, provider_id: &str) -> bool {
        self.snapshot
            .read()
            .await
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
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
            revision: snapshot.version.revision.clone(),
            generated_at: snapshot.version.generated_at.clone(),
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

    pub async fn provider_scope(&self, provider_id: &str) -> anyhow::Result<CatalogProviderScope> {
        validate_provider_id(provider_id)?;
        let snapshot = self.snapshot.read().await;
        ensure_catalog_provider(&snapshot, provider_id)?;
        let revision = snapshot.version.revision.clone();
        drop(snapshot);

        if let Some(scope) = load_verified_scope(&self.data_dir, &revision, provider_id)? {
            return Ok(scope);
        }

        let _guard = self.scope_refresh_lock.lock().await;
        let snapshot = self.snapshot.read().await;
        ensure_catalog_provider(&snapshot, provider_id)?;
        let revision = snapshot.version.revision.clone();
        if let Some(scope) = load_verified_scope(&self.data_dir, &revision, provider_id)? {
            return Ok(scope);
        }
        drop(snapshot);

        // Scoped URLs expose the latest catalog, not the revision of our cached indexes.
        self.refresh()
            .await
            .map_err(|error| CatalogError::ScopeRefresh {
                provider_id: provider_id.to_string(),
                message: error.to_string(),
            })?;
        let snapshot = self.snapshot.read().await;
        ensure_catalog_provider(&snapshot, provider_id)?;
        let revision = snapshot.version.revision.clone();
        drop(snapshot);

        let body = self
            .source
            .fetch_provider_scope(provider_id)
            .await
            .map_err(|error| CatalogError::ScopeRefresh {
                provider_id: provider_id.to_string(),
                message: error.to_string(),
            })?;
        let scope = parse_scope(&body, &revision, provider_id).map_err(|error| {
            CatalogError::ScopeRefresh {
                provider_id: provider_id.to_string(),
                message: error.to_string(),
            }
        })?;
        let observed =
            self.source
                .fetch_version()
                .await
                .map_err(|error| CatalogError::ScopeRefresh {
                    provider_id: provider_id.to_string(),
                    message: error.to_string(),
                })?;
        validate_version(&observed).map_err(|error| CatalogError::ScopeRefresh {
            provider_id: provider_id.to_string(),
            message: error.to_string(),
        })?;
        let active_revision = self.snapshot.read().await.version.revision.clone();
        if observed.revision != revision || active_revision != revision {
            return Err(CatalogError::ScopeRefresh {
                provider_id: provider_id.to_string(),
                message: "catalog revision changed while loading the Provider Catalog scope; retry the operation"
                    .to_string(),
            }
            .into());
        }
        persist_scope(&self.data_dir, &scope)?;
        Ok(scope)
    }

    pub async fn models(
        &self,
        provider_id: &str,
        channel_id: &str,
    ) -> anyhow::Result<CatalogModelList> {
        let (provider, scope) = self.resolve_provider_scope(provider_id, channel_id).await?;
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

    pub async fn model_sources(
        &self,
        provider_id: &str,
        channel_id: &str,
    ) -> anyhow::Result<Vec<CatalogModelSource>> {
        let (_, scope) = self.resolve_provider_scope(provider_id, channel_id).await?;
        Ok(scope.models)
    }

    pub async fn model_source(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<CatalogModelSource> {
        let model_id = model_id.trim();
        let scope = self.provider_scope(provider_id).await?;
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

    pub async fn model(&self, provider_id: &str, model_id: &str) -> anyhow::Result<CatalogModel> {
        let source = self.model_source(provider_id, model_id).await?;
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
        parse_catalog_model(provider_id, &provider.protocol, &source.metadata)
    }

    pub async fn refresh(&self) -> anyhow::Result<CatalogRefreshSummary> {
        let observed_generation = self.generation.load(Ordering::Acquire);
        let _guard = self.refresh_lock.lock().await;
        if self.generation.load(Ordering::Acquire) != observed_generation {
            return Ok(self.summary(false).await);
        }

        let version = self.source.fetch_version().await?;
        validate_version(&version)?;
        if version.revision == self.snapshot.read().await.version.revision {
            return Ok(self.summary(false).await);
        }

        let providers_body = self.source.fetch_providers().await?;
        let canonical_models_body = self.source.fetch_canonical_models().await?;
        let candidate = parse_snapshot(&providers_body, &canonical_models_body, version.clone())?;
        let confirmed = self.source.fetch_version().await?;
        validate_version(&confirmed)?;
        if confirmed.revision != version.revision {
            bail!("catalog revision changed while downloading global indexes; retry the refresh");
        }

        persist_generation(&self.data_dir, &candidate)?;
        *self.snapshot.write().await = candidate;
        self.generation.fetch_add(1, Ordering::Release);
        Ok(self.summary(true).await)
    }

    pub async fn refresh_forever(self) {
        if let Err(error) = self.refresh().await {
            tracing::warn!(error = ?error, "Provider Catalog startup refresh failed");
        }
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = self.refresh().await {
                tracing::warn!(error = ?error, "Provider Catalog refresh failed");
            }
        }
    }

    pub async fn logo(&self, provider_id: &str) -> anyhow::Result<Vec<u8>> {
        validate_provider_id(provider_id)?;
        let snapshot = self.snapshot.read().await;
        ensure_catalog_provider(&snapshot, provider_id)?;
        drop(snapshot);
        let path = logo_path(&self.data_dir, provider_id);
        if file_is_fresh(&path, LOGO_TTL) {
            return std::fs::read(path).context("read provider logo cache");
        }
        match self.source.fetch_logo(provider_id).await {
            Ok(body) => {
                validate_svg(&body)?;
                atomic_write(&path, &body)?;
                Ok(body)
            }
            Err(error) => match std::fs::read(&path) {
                Ok(body) => Ok(body),
                Err(_) => Err(error),
            },
        }
    }

    async fn resolve_provider_scope(
        &self,
        provider_id: &str,
        channel_id: &str,
    ) -> anyhow::Result<(CatalogProvider, CatalogProviderScope)> {
        let scope = self.provider_scope(provider_id).await?;
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
        Ok((provider, scope))
    }

    async fn summary(&self, changed: bool) -> CatalogRefreshSummary {
        let snapshot = self.snapshot.read().await;
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
                &definition.name,
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
                &definition.name,
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
