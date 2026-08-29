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

use crate::provider_models::ProviderModelMetadata;

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

    pub async fn providers(&self) -> CatalogProviderList {
        let snapshot = self.snapshot.read().await;
        CatalogProviderList {
            revision: snapshot.version.revision.clone(),
            generated_at: snapshot.version.generated_at.clone(),
            providers: snapshot.providers.clone(),
        }
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
            .filter(|source| {
                provider_id != "openai" || channel_id != "codex" || {
                    let id = source
                        .metadata
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    codex_subscription_model(id)
                }
            })
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
        let provider = self
            .snapshot
            .read()
            .await
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
            .ok_or_else(|| anyhow!("catalog provider not found: {provider_id}"))?;
        let source = self.model_source(provider_id, model_id).await?;
        parse_catalog_model(provider_id, &provider.protocol, &source.metadata)
    }

    pub async fn resolve_channel(
        &self,
        provider_id: &str,
        channel_id: &str,
        fingerprint: &str,
    ) -> anyhow::Result<(CatalogProvider, CatalogChannel)> {
        let snapshot = self.snapshot.read().await;
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
            .ok_or_else(|| anyhow!("catalog provider not found: {provider_id}"))?;
        let channel = provider
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .cloned()
            .ok_or_else(|| anyhow!("catalog channel not found: {provider_id}/{channel_id}"))?;
        if channel.fingerprint != fingerprint {
            bail!("catalog channel changed; refresh and select it again");
        }
        Ok((provider, channel))
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
        let provider = self
            .snapshot
            .read()
            .await
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
            .ok_or_else(|| anyhow!("catalog provider not found: {provider_id}"))?;
        if !provider
            .channels
            .iter()
            .any(|channel| channel.id == channel_id)
        {
            bail!("catalog channel not found: {provider_id}/{channel_id}");
        }
        let scope = self.provider_scope(provider_id).await?;
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
