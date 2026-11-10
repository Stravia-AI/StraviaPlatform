use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{MarketError, PluginMarketplace, now};
use crate::gateway::Gateway;
use crate::plugin::PluginPreview;
use crate::plugin::distribution::{self, MAX_COMPONENT_BYTES};
use crate::plugin::types::PluginInstallOrigin;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewInstallRequest {
    pub source_id: String,
    pub release_id: u64,
    pub vendor_id: String,
    pub version: String,
    pub catalog_sha256: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub stage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

fn progress(callback: &Option<Arc<dyn Fn(Progress) + Send + Sync>>, stage: &'static str) {
    if let Some(callback) = callback {
        callback(Progress {
            stage,
            received: None,
            total: None,
        });
    }
}

fn digest(bytes: &[u8]) -> String {
    stravia_runtime_contract::protocol::ir::canonical::hash_hex(&Sha256::digest(bytes).into())
}

impl PluginMarketplace {
    pub async fn preview_install(
        &self,
        gw: &Gateway,
        session: &str,
        request: PreviewInstallRequest,
        callback: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
    ) -> Result<PluginPreview, MarketError> {
        let permit = Arc::clone(&self.preparations)
            .acquire_owned()
            .await
            .map_err(|_| {
                MarketError::new("resource_limit", "installation preparation is unavailable")
            })?;
        if request.vendor_id == "base" {
            return Err(MarketError::new(
                "incompatible",
                "the base plugin cannot be installed from the marketplace",
            ));
        }
        let source = self
            .store
            .get_source(&request.source_id)
            .await
            .map_err(|e| MarketError::new("network", e.to_string()))?
            .ok_or_else(|| MarketError::new("source_disabled", "plugin source no longer exists"))?;
        if !source.enabled {
            return Err(MarketError::new(
                "source_disabled",
                "plugin source is disabled",
            ));
        }
        if source
            .error
            .as_ref()
            .is_some_and(|error| error.code == "key_changed")
        {
            return Err(MarketError::new(
                "key_changed",
                "plugin source signing key has changed",
            ));
        }
        let catalogs = self
            .store
            .list_catalogs(&source.id)
            .await
            .map_err(|e| MarketError::new("network", e.to_string()))?;
        let mut selected = None;
        let mut seen_content: Option<(String, String)> = None;
        for cached in catalogs {
            let catalog = distribution::verify_catalog(
                &cached.catalog,
                &cached.signature,
                &source.public_key,
            )
            .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
            if catalog.repository_id != source.repository_id
                || catalog.release_tag != cached.release_tag
            {
                return Err(MarketError::new(
                    "source_identity_changed",
                    "cached catalog identity does not match source",
                ));
            }
            for release in catalog.releases {
                if release.vendor_id != request.vendor_id || release.version != request.version {
                    continue;
                }
                let content = (
                    release.artifact.sha256.clone(),
                    release.component.sha256.clone(),
                );
                if seen_content
                    .as_ref()
                    .is_some_and(|previous| previous != &content)
                {
                    return Err(MarketError::new(
                        "version_conflict",
                        "the same version has different content",
                    ));
                }
                seen_content = Some(content);
                if cached.release_id == request.release_id
                    && digest(&cached.catalog) == request.catalog_sha256
                {
                    if (cached.prerelease
                        || !semver::Version::parse(&release.version)
                            .map_err(|e| MarketError::new("artifact_corrupt", e.to_string()))?
                            .pre
                            .is_empty())
                        && !source.include_prerelease
                    {
                        return Err(MarketError::new(
                            "incompatible",
                            "prereleases are disabled for this source",
                        ));
                    }
                    selected = Some((
                        cached.release_tag.clone(),
                        cached.release_url.clone(),
                        release,
                    ));
                }
            }
        }
        let (tag, release_url, release) = selected.ok_or_else(|| {
            MarketError::new(
                "preview_expired",
                "the requested cached release is no longer available",
            )
        })?;
        if let Some(reason) = super::view::compatibility(&release) {
            return Err(MarketError::new("incompatible", reason));
        }
        if release.compatibility.wit_version != "0.6.0" {
            return Err(MarketError::new(
                "incompatible",
                "unsupported plugin WIT version",
            ));
        }
        if self
            .store
            .record_seen_digest(
                &source.id,
                &release.vendor_id,
                &release.version,
                &release.component.sha256,
            )
            .await
            .map_err(|e| MarketError::new("network", e.to_string()))?
        {
            return Err(MarketError::new(
                "version_conflict",
                "this version was previously observed with different content",
            ));
        }
        let repository = self
            .client
            .resolve_existing_repository(&source.url, source.repository_id)
            .await?;
        if repository.id != source.repository_id {
            return Err(MarketError::new(
                "source_identity_changed",
                "repository identity has changed",
            ));
        }
        let mut selected_release = None;
        let mut key_release = None;
        for page in 1..=10 {
            let releases = self.client.releases(&repository, page).await?;
            let count = releases.len();
            for item in releases {
                if !item.draft
                    && (!item.prerelease || source.include_prerelease)
                    && key_release.is_none()
                    && item.assets.iter().any(|asset| asset.name == super::CATALOG)
                {
                    key_release = Some(item.clone());
                }
                if item.id == request.release_id && item.tag == tag && !item.draft {
                    selected_release = Some(item);
                }
            }
            if (selected_release.is_some() && key_release.is_some()) || count < 100 {
                break;
            }
        }
        let remote_release = selected_release
            .ok_or_else(|| MarketError::new("preview_expired", "release is no longer available"))?;
        let key_release = key_release.ok_or_else(|| {
            MarketError::new("not_cataloged", "source signing key is unavailable")
        })?;
        let key = self
            .client
            .asset(&repository, &key_release, super::PUBLIC_KEY, 4096, None)
            .await?;
        let key = std::str::from_utf8(&key)
            .map_err(|_| MarketError::new("signature_invalid", "invalid source signing key"))?;
        let fingerprint = distribution::public_key_fingerprint(key)
            .map_err(|e| MarketError::new("signature_invalid", e.to_string()))?;
        if fingerprint != source.fingerprint {
            self.store
                .mark_source_key_changed(
                    &source.id,
                    source.trust_revision,
                    &source.fingerprint,
                    &fingerprint,
                )
                .await
                .map_err(|e| MarketError::new("network", e.to_string()))?;
            return Err(MarketError::new(
                "key_changed",
                "plugin source signing key has changed",
            ));
        }
        let artifact_size = usize::try_from(release.artifact.size)
            .map_err(|_| MarketError::new("resource_limit", "artifact is too large"))?;
        if artifact_size > MAX_COMPONENT_BYTES {
            return Err(MarketError::new("resource_limit", "artifact is too large"));
        }
        progress(&callback, "download");
        let download_progress = callback.as_ref().map(|callback| {
            let callback = Arc::clone(callback);
            Arc::new(move |received, total| {
                callback(Progress {
                    stage: "download",
                    received: Some(received),
                    total,
                })
            }) as Arc<dyn Fn(u64, Option<u64>) + Send + Sync>
        });
        let artifact = self
            .client
            .asset(
                &repository,
                &remote_release,
                &release.artifact.filename,
                artifact_size,
                download_progress,
            )
            .await?;
        progress(&callback, "verify");
        if artifact.len() != artifact_size || digest(&artifact) != release.artifact.sha256 {
            return Err(MarketError::new(
                "artifact_corrupt",
                "artifact size or digest does not match catalog",
            ));
        }
        progress(&callback, "decompress");
        let expected = release.component.clone();
        let (permit, component) = tokio::task::spawn_blocking(move || {
            (
                permit,
                distribution::decode_component(&artifact, Some(&expected)),
            )
        })
        .await
        .map_err(|e| MarketError::new("artifact_corrupt", e.to_string()))?;
        let component =
            component.map_err(|e| MarketError::new("artifact_corrupt", e.to_string()))?;
        let _permit = permit;
        progress(&callback, "admission");
        let origin = PluginInstallOrigin {
            source_id: source.id,
            repository_id: source.repository_id,
            repository: repository.name,
            public_key_fingerprint: source.fingerprint,
            trust_revision: source.trust_revision,
            release_id: request.release_id,
            release_tag: tag,
            release_url,
            catalog_sha256: request.catalog_sha256,
            artifact_sha256: release.artifact.sha256,
        };
        let preview = gw
            .vendor_plugins
            .preview_marketplace(
                gw,
                component.into(),
                origin,
                &release.vendor_id,
                &release.version,
                release.compatibility.canonical_format_version,
            )
            .await
            .map_err(|e| MarketError::new("artifact_corrupt", e.to_string()))?;
        let mut sessions = self.install_sessions.lock();
        let current = now();
        sessions.retain(|_, (_, expires)| *expires > current);
        if sessions.len() >= 1024 {
            return Err(MarketError::new(
                "resource_limit",
                "Too many pending installation previews",
            ));
        }
        // Keep bindings longer than the manager's 15-minute preview lifetime,
        // including the sub-second truncation of the wall-clock timestamp.
        sessions.insert(preview.id.clone(), (session.to_owned(), current + 16 * 60));
        Ok(preview)
    }

    pub fn authorize_install_preview(
        &self,
        session: &str,
        preview_id: &str,
    ) -> Result<(), MarketError> {
        let mut sessions = self.install_sessions.lock();
        let current = now();
        sessions.retain(|_, (_, expires)| *expires > current);
        match sessions.get(preview_id) {
            None => Ok(()), // Local and builtin previews are validated by the plugin manager.
            Some((owner, _)) if owner == session => Ok(()),
            Some(_) => Err(MarketError::new(
                "preview_expired",
                "installation preview belongs to another session",
            )),
        }
    }
}
