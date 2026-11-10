//! Signed, administrator-trusted vendor plugin sources and marketplace.
pub mod client;
mod install;
#[cfg(test)]
mod tests;
mod view;

pub use install::{PreviewInstallRequest, Progress};
pub use view::{MarketplaceEntry, MarketplaceRelease, MarketplaceView};

use crate::{
    gateway::Gateway,
    plugin::{
        PluginStore,
        distribution::{self, CatalogRelease},
    },
};
use client::{MarketClient, Repository};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;

const CATALOG: &str = "stravia-plugin-catalog.json";
const SIGNATURE: &str = "stravia-plugin-catalog.json.minisig";
const PUBLIC_KEY: &str = "stravia-plugin-catalog.pub";
const OFFICIAL_ID: u64 = 1350654373;
const PREVIEW_TTL: i64 = 15 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketError {
    pub code: String,
    pub message: String,
}
impl MarketError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for MarketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for MarketError {}

/// Preserve actionable marketplace trust failures from the atomic install transaction.
pub fn confirm_error(error: &anyhow::Error) -> Option<MarketError> {
    use crate::plugin::store::PluginStorageError;
    match error.downcast_ref::<PluginStorageError>()? {
        PluginStorageError::KeyChanged => Some(MarketError::new(
            "key_changed",
            "Plugin source signing key has changed; re-trust is required",
        )),
        PluginStorageError::VersionConflict => Some(MarketError::new(
            "version_conflict",
            "This plugin version has conflicting content",
        )),
        _ => None,
    }
}
fn storage_error(error: impl std::fmt::Display) -> MarketError {
    MarketError::new("network", error.to_string())
}
fn refresh_storage_error(error: anyhow::Error) -> MarketError {
    if matches!(
        error.downcast_ref::<crate::plugin::store::PluginStorageError>(),
        Some(crate::plugin::store::PluginStorageError::Changed)
    ) {
        MarketError::new("source_changed", "Plugin source changed during refresh")
    } else {
        storage_error(error)
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginSourceSummary {
    pub id: String,
    pub repository_id: u64,
    pub repository: String,
    pub url: String,
    pub official: bool,
    pub public_key: String,
    pub fingerprint: String,
    pub trust_revision: i64,
    pub enabled: bool,
    pub include_prerelease: bool,
    pub mirrored: Option<bool>,
    pub checked_at: Option<i64>,
    pub succeeded_at: Option<i64>,
    pub error: Option<MarketError>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedCatalog {
    pub source_id: String,
    pub release_id: u64,
    pub release_tag: String,
    pub release_url: String,
    pub prerelease: bool,
    pub catalog: Vec<u8>,
    pub signature: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourcePreview {
    pub preview_id: String,
    pub id: String,
    pub repository_id: u64,
    pub repository: String,
    pub url: String,
    pub public_key: String,
    pub fingerprint: String,
    pub mirrored: bool,
    pub releases: Vec<CatalogRelease>,
    pub previous_fingerprint: Option<String>,
}
struct PendingSource {
    session: String,
    expires: i64,
    preview: SourcePreview,
    target: Option<String>,
    trust_snapshot: Option<(i64, Option<String>)>,
}

pub struct PluginMarketplace {
    store: Arc<PluginStore>,
    client: MarketClient,
    preparations: Arc<Semaphore>,
    install_sessions: Mutex<HashMap<String, (String, i64)>>,
    previews: Mutex<HashMap<String, PendingSource>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}
impl PluginMarketplace {
    pub fn new(store: Arc<PluginStore>, client: MarketClient) -> Self {
        Self {
            store,
            client,
            preparations: Arc::new(Semaphore::new(2)),
            install_sessions: Mutex::new(HashMap::new()),
            previews: Mutex::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
        }
    }
    pub async fn initialize_official(&self) -> Result<(), MarketError> {
        let id = OFFICIAL_ID.to_string();
        let existing = self.store.get_source(&id).await.map_err(storage_error)?;
        let expected = existing.clone();
        let key = option_env!("STRAVIA_PLUGIN_CATALOG_PUBLIC_KEY")
            .unwrap_or("")
            .trim();
        let fingerprint = if key.is_empty() {
            String::new()
        } else {
            distribution::public_key_fingerprint(key)
                .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?
        };
        let mut source = existing.unwrap_or(PluginSourceSummary {
            id,
            repository_id: OFFICIAL_ID,
            repository: "Stravia-AI/StraviaPlatform".into(),
            url: "https://github.com/Stravia-AI/StraviaPlatform".into(),
            official: true,
            public_key: String::new(),
            fingerprint: String::new(),
            trust_revision: 1,
            enabled: true,
            include_prerelease: false,
            mirrored: None,
            checked_at: None,
            succeeded_at: None,
            error: None,
        });
        if expected.is_some() && source.fingerprint != fingerprint {
            source.trust_revision += 1;
            if !fingerprint.is_empty()
                && source
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "key_changed")
            {
                source.error = None;
            }
        }
        source.official = true;
        source.public_key = key.to_string();
        source.fingerprint = fingerprint;
        if key.is_empty() {
            source.enabled = false;
            source.error = Some(MarketError::new(
                "source_not_configured",
                "Official plugin catalog public key is not configured",
            ));
        } else if source
            .error
            .as_ref()
            .is_some_and(|e| e.code == "source_not_configured")
        {
            source.error = None;
            source.enabled = true;
        }
        self.store
            .put_source(&source, expected.as_ref())
            .await
            .map_err(storage_error)
    }
    pub async fn list_sources(&self) -> Result<Vec<PluginSourceSummary>, MarketError> {
        self.store.list_sources().await.map_err(storage_error)
    }
    async fn fetch_preview(
        &self,
        url: &str,
        expected: Option<&str>,
        previous: Option<String>,
    ) -> Result<SourcePreview, MarketError> {
        let repo = self.client.resolve_repository(url).await?;
        let mut found = None;
        for page in 1..=10 {
            let releases = self.client.releases(&repo, page).await?;
            let last = releases.len() < 100;
            for release in releases.into_iter().filter(|r| !r.draft) {
                if !release.assets.iter().any(|a| a.name == CATALOG) {
                    continue;
                }
                let key = self
                    .client
                    .asset(&repo, &release, PUBLIC_KEY, 4096, None)
                    .await?;
                let key = String::from_utf8(key)
                    .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                let fingerprint = distribution::public_key_fingerprint(&key)
                    .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                if found.is_none()
                    && expected.is_some_and(|s| !s.trim().eq_ignore_ascii_case(&fingerprint))
                {
                    return Err(MarketError::new(
                        "signature_invalid",
                        "Public key fingerprint does not match the supplied fingerprint",
                    ));
                }
                if found
                    .as_ref()
                    .is_some_and(|p: &SourcePreview| p.fingerprint != fingerprint)
                {
                    continue;
                }
                let bytes = self
                    .client
                    .asset(&repo, &release, CATALOG, 256 * 1024, None)
                    .await?;
                let sig = self
                    .client
                    .asset(&repo, &release, SIGNATURE, 4096, None)
                    .await?;
                let sig = String::from_utf8(sig)
                    .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                let catalog = distribution::verify_catalog(&bytes, &sig, &key)
                    .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                if catalog.repository_id != repo.id || catalog.release_tag != release.tag {
                    return Err(MarketError::new(
                        "source_identity_changed",
                        "Catalog repository or release does not match",
                    ));
                }
                if let Some(existing) = &mut found {
                    let existing: &mut SourcePreview = existing;
                    if existing.fingerprint != fingerprint {
                        continue;
                    }
                    if existing.releases.len() + catalog.releases.len() > 1000 {
                        return Err(MarketError::new(
                            "resource_limit",
                            "Source preview contains too many releases",
                        ));
                    }
                    existing.releases.extend(catalog.releases);
                } else {
                    found = Some(SourcePreview {
                        preview_id: uuid::Uuid::new_v4().to_string(),
                        id: repo.id.to_string(),
                        repository_id: repo.id,
                        repository: repo.name.clone(),
                        url: repo.url.clone(),
                        public_key: key,
                        fingerprint,
                        mirrored: repo.mirrored,
                        releases: catalog.releases,
                        previous_fingerprint: previous.clone(),
                    });
                }
            }
            if last {
                break;
            }
        }
        found.ok_or_else(|| {
            MarketError::new(
                "not_cataloged",
                format!(
                    "This repository has not published a plugin catalog: {}/releases",
                    repo.url
                ),
            )
        })
    }
    fn save_preview(
        &self,
        session: &str,
        target: Option<String>,
        trust_snapshot: Option<(i64, Option<String>)>,
        preview: SourcePreview,
    ) -> Result<SourcePreview, MarketError> {
        let mut pending = self.previews.lock();
        pending.retain(|_, p| p.expires > now());
        if pending.len() >= 16
            || serde_json::to_vec(&preview).map_err(storage_error)?.len() > 1024 * 1024
        {
            return Err(MarketError::new(
                "resource_limit",
                "Source preview capacity exceeded",
            ));
        }
        pending.insert(
            preview.preview_id.clone(),
            PendingSource {
                session: session.into(),
                expires: now() + PREVIEW_TTL,
                preview: preview.clone(),
                target,
                trust_snapshot,
            },
        );
        Ok(preview)
    }
    pub async fn source_preview(
        &self,
        session: &str,
        url: &str,
        fingerprint: Option<&str>,
    ) -> Result<SourcePreview, MarketError> {
        let preview = self.fetch_preview(url, fingerprint, None).await?;
        if self
            .store
            .get_source(&preview.id)
            .await
            .map_err(storage_error)?
            .is_some()
        {
            return Err(MarketError::new(
                "source_identity_changed",
                "Source already exists",
            ));
        }
        self.save_preview(session, None, None, preview)
    }
    pub async fn trust_preview(
        &self,
        session: &str,
        id: &str,
    ) -> Result<SourcePreview, MarketError> {
        let source = self
            .store
            .get_source(id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| MarketError::new("source_identity_changed", "Source not found"))?;
        if source.official {
            return Err(MarketError::new(
                "source_identity_changed",
                "Official source key can only change with the host",
            ));
        }
        let repo = self
            .client
            .resolve_existing_repository(&source.url, source.repository_id)
            .await?;
        let preview = self
            .fetch_preview(&repo.url, None, Some(source.fingerprint.clone()))
            .await?;
        if preview.repository_id != source.repository_id {
            return Err(MarketError::new(
                "source_identity_changed",
                "Repository identity changed",
            ));
        }
        let blocked = source
            .error
            .as_ref()
            .filter(|error| error.code == "key_changed")
            .map(|error| error.message.clone());
        self.save_preview(
            session,
            Some(id.into()),
            Some((source.trust_revision, blocked)),
            preview,
        )
    }
    async fn confirm_source(
        &self,
        session: &str,
        id: &str,
        target: Option<&str>,
    ) -> Result<PluginSourceSummary, MarketError> {
        let pending = {
            let mut previews = self.previews.lock();
            let p = previews
                .get(id)
                .ok_or_else(|| MarketError::new("preview_expired", "Source preview expired"))?;
            if p.session != session || p.expires <= now() || p.target.as_deref() != target {
                return Err(MarketError::new(
                    "preview_expired",
                    "Source preview expired or belongs to another session",
                ));
            }
            previews.remove(id).expect("preview exists")
        };
        let p = pending.preview;
        let verified = self
            .fetch_preview(&p.url, Some(&p.fingerprint), None)
            .await?;
        if verified.repository_id != p.repository_id {
            return Err(MarketError::new(
                "source_identity_changed",
                "Repository identity changed",
            ));
        }
        let existing = self.store.get_source(&p.id).await.map_err(storage_error)?;
        let expected = existing.clone();
        let source = if target.is_some() {
            let mut source = existing
                .ok_or_else(|| MarketError::new("source_identity_changed", "Source was removed"))?;
            let blocked = source
                .error
                .as_ref()
                .filter(|error| error.code == "key_changed")
                .map(|error| error.message.clone());
            if source.official
                || Some(source.fingerprint.as_str()) != p.previous_fingerprint.as_deref()
                || pending.trust_snapshot != Some((source.trust_revision, blocked))
            {
                return Err(MarketError::new(
                    "source_identity_changed",
                    "Source trust changed since preview",
                ));
            }
            source.public_key = p.public_key;
            source.fingerprint = p.fingerprint;
            source.trust_revision += 1;
            source.succeeded_at = None;
            source.error = None;
            source
        } else {
            if existing.is_some() {
                return Err(MarketError::new(
                    "source_identity_changed",
                    "Source already exists",
                ));
            }
            PluginSourceSummary {
                id: p.id,
                repository_id: p.repository_id,
                repository: p.repository,
                url: p.url,
                official: false,
                public_key: p.public_key,
                fingerprint: p.fingerprint,
                trust_revision: 1,
                enabled: true,
                include_prerelease: false,
                mirrored: Some(p.mirrored),
                checked_at: None,
                succeeded_at: None,
                error: None,
            }
        };
        self.store
            .put_source(&source, expected.as_ref())
            .await
            .map_err(storage_error)?;
        if target.is_some() && source.enabled {
            if let Err(error) = self.refresh_source(source.clone()).await {
                tracing::warn!(source_id = %source.id, %error, "Failed to refresh re-trusted plugin source");
            }
            return self
                .store
                .get_source(&source.id)
                .await
                .map_err(storage_error)?
                .ok_or_else(|| MarketError::new("source_identity_changed", "Source was removed"));
        }
        Ok(source)
    }
    pub async fn source_confirm(
        &self,
        session: &str,
        preview_id: &str,
    ) -> Result<PluginSourceSummary, MarketError> {
        self.confirm_source(session, preview_id, None).await
    }
    pub async fn trust_confirm(
        &self,
        session: &str,
        id: &str,
        preview_id: &str,
    ) -> Result<PluginSourceSummary, MarketError> {
        self.confirm_source(session, preview_id, Some(id)).await
    }
    pub async fn patch_source(
        &self,
        id: &str,
        enabled: Option<bool>,
        include_prerelease: Option<bool>,
    ) -> Result<PluginSourceSummary, MarketError> {
        let mut source = self
            .store
            .get_source(id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| MarketError::new("source_identity_changed", "Source not found"))?;
        let expected = source.clone();
        if enabled == Some(true) && source.public_key.is_empty() {
            return Err(MarketError::new(
                "source_not_configured",
                "Official public key is not configured",
            ));
        }
        if let Some(v) = enabled {
            source.enabled = v;
        }
        if let Some(v) = include_prerelease {
            source.include_prerelease = v;
        }
        self.store
            .put_source(&source, Some(&expected))
            .await
            .map_err(storage_error)?;
        Ok(source)
    }
    pub async fn delete_source(&self, id: &str) -> Result<(), MarketError> {
        let source = self
            .store
            .get_source(id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| MarketError::new("source_identity_changed", "Source not found"))?;
        if source.official {
            return Err(MarketError::new(
                "source_identity_changed",
                "Official source cannot be removed",
            ));
        }
        self.store
            .delete_source(id, &source)
            .await
            .map_err(storage_error)
    }
    pub async fn refresh(
        &self,
        gw: &Gateway,
        id: Option<&str>,
    ) -> Result<MarketplaceView, MarketError> {
        let sources = self.list_sources().await?;
        if let Some(id) = id {
            let source = sources
                .iter()
                .find(|s| s.id == id)
                .ok_or_else(|| MarketError::new("source_identity_changed", "Source not found"))?;
            if !source.enabled {
                return Err(source
                    .error
                    .clone()
                    .unwrap_or_else(|| MarketError::new("source_disabled", "Source is disabled")));
            }
        }
        let mut failure = None;
        for source in sources
            .into_iter()
            .filter(|s| s.enabled && id.is_none_or(|id| id == s.id))
        {
            let source_id = source.id.clone();
            if let Err(error) = self.refresh_source(source).await {
                if error.code != "source_changed" {
                    tracing::warn!(%source_id, %error, "Failed to refresh plugin source");
                    failure.get_or_insert(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        self.view(gw).await
    }
    pub async fn refresh_due(&self, _gw: &Gateway) -> Result<(), MarketError> {
        let mut failure = None;
        for source in self
            .list_sources()
            .await?
            .into_iter()
            .filter(|s| s.enabled && s.succeeded_at.is_none_or(|t| now() - t >= 86400))
        {
            let source_id = source.id.clone();
            if let Err(error) = self.refresh_source(source).await {
                if error.code != "source_changed" {
                    tracing::warn!(%source_id, %error, "Failed to refresh plugin source");
                    failure.get_or_insert(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }
    async fn refresh_source(&self, source: PluginSourceSummary) -> Result<(), MarketError> {
        let lock = {
            self.refresh_locks
                .lock()
                .entry(source.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let token = self
            .store
            .begin_source_check(&source)
            .await
            .map_err(refresh_storage_error)?;
        let checked = now();
        let result = self.fetch_catalogs(&source).await;
        match result {
            Ok((repo, catalogs, incomplete)) => {
                if let Err(e) = self
                    .store
                    .replace_catalogs(
                        &source.id,
                        &source,
                        token,
                        checked,
                        &catalogs,
                        incomplete,
                        repo.mirrored,
                    )
                    .await
                {
                    let error = refresh_storage_error(e);
                    if error.code == "source_changed" {
                        return Err(error);
                    }
                    self.store
                        .record_source_check(
                            &source.id,
                            source.trust_revision,
                            token,
                            checked,
                            Some(repo.mirrored),
                            Some(&error),
                        )
                        .await
                        .map_err(refresh_storage_error)?;
                    return Err(error);
                } else {
                    if let Some(current) = self
                        .store
                        .get_source(&source.id)
                        .await
                        .map_err(refresh_storage_error)?
                    {
                        if current.trust_revision == source.trust_revision
                            && (current.repository != repo.name || current.url != repo.url)
                        {
                            let mut renamed = current.clone();
                            renamed.repository = repo.name;
                            renamed.url = repo.url;
                            self.store
                                .put_source(&renamed, Some(&current))
                                .await
                                .map_err(refresh_storage_error)?;
                        }
                    }
                }
            }
            Err(error) => {
                if error.code == "source_changed" {
                    return Err(error);
                }
                tracing::warn!(source_id = %source.id, %error, "Plugin source refresh failed");
                self.store
                    .record_source_check(
                        &source.id,
                        source.trust_revision,
                        token,
                        checked,
                        None,
                        Some(&error),
                    )
                    .await
                    .map_err(refresh_storage_error)?;
            }
        }
        Ok(())
    }
    async fn fetch_catalogs(
        &self,
        source: &PluginSourceSummary,
    ) -> Result<(Repository, Vec<CachedCatalog>, bool), MarketError> {
        let repo = self
            .client
            .resolve_existing_repository(&source.url, source.repository_id)
            .await?;
        if repo.id != source.repository_id {
            return Err(MarketError::new(
                "source_identity_changed",
                "Repository identity changed",
            ));
        }
        let mut catalogs = Vec::new();
        let mut incomplete = false;
        let mut checked_key = false;
        for page in 1..=10 {
            let releases = self.client.releases(&repo, page).await?;
            let count = releases.len();
            for release in releases
                .into_iter()
                .filter(|r| !r.draft && (source.include_prerelease || !r.prerelease))
            {
                if !release.assets.iter().any(|a| a.name == CATALOG) {
                    continue;
                }
                let key = self
                    .client
                    .asset(&repo, &release, PUBLIC_KEY, 4096, None)
                    .await?;
                let key = String::from_utf8(key)
                    .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                let fingerprint = distribution::public_key_fingerprint(&key)
                    .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                if !checked_key && fingerprint != source.fingerprint {
                    self.store
                        .mark_source_key_changed(
                            &source.id,
                            source.trust_revision,
                            &source.fingerprint,
                            &fingerprint,
                        )
                        .await
                        .map_err(refresh_storage_error)?;
                    return Err(MarketError::new(
                        "key_changed",
                        format!(
                            "Publisher key changed: {} -> {}",
                            source.fingerprint, fingerprint
                        ),
                    ));
                }
                checked_key = true;
                if fingerprint != source.fingerprint {
                    continue;
                }
                let bytes = self
                    .client
                    .asset(&repo, &release, CATALOG, 256 * 1024, None)
                    .await?;
                let sig = String::from_utf8(
                    self.client
                        .asset(&repo, &release, SIGNATURE, 4096, None)
                        .await?,
                )
                .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                let catalog = distribution::verify_catalog(&bytes, &sig, &source.public_key)
                    .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
                if catalog.repository_id != source.repository_id
                    || catalog.release_tag != release.tag
                {
                    return Err(MarketError::new(
                        "source_identity_changed",
                        "Catalog identity mismatch",
                    ));
                }
                for item in &catalog.releases {
                    self.store
                        .record_seen_digest(
                            &source.id,
                            &item.vendor_id,
                            &item.version,
                            &item.component.sha256,
                        )
                        .await
                        .map_err(storage_error)?;
                }
                if catalogs.len() < 100 {
                    catalogs.push(CachedCatalog {
                        source_id: source.id.clone(),
                        release_id: release.id,
                        release_tag: release.tag,
                        release_url: release.url,
                        prerelease: release.prerelease,
                        catalog: bytes,
                        signature: sig,
                    });
                } else {
                    incomplete = true;
                }
            }
            if count < 100 {
                break;
            }
            if page == 10 {
                incomplete = true;
            }
        }
        if !checked_key {
            return Err(MarketError::new(
                "not_cataloged",
                format!("No plugin catalogs published: {}/releases", repo.url),
            ));
        }
        Ok((repo, catalogs, incomplete))
    }
}
