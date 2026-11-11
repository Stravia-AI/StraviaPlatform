use std::collections::{BTreeMap, HashMap};

use semver::{Version, VersionReq};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{MarketError, PluginMarketplace, PluginSourceSummary};
use crate::{
    gateway::Gateway,
    plugin::distribution::{CatalogRelease, verify_catalog},
};

#[derive(Clone, Debug, Serialize)]
pub struct MarketplaceView {
    pub sources: Vec<PluginSourceSummary>,
    pub entries: Vec<MarketplaceEntry>,
    pub update_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct MarketplaceEntry {
    pub source_id: String,
    pub vendor_id: String,
    pub releases: Vec<MarketplaceRelease>,
    pub incomplete: bool,
    pub conflict: bool,
    pub latest: Option<MarketplaceRelease>,
    pub update: Option<MarketplaceRelease>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MarketplaceRelease {
    #[serde(flatten)]
    pub catalog: CatalogRelease,
    pub release_id: u64,
    pub release_tag: String,
    pub release_url: String,
    pub catalog_sha256: String,
    pub compatible: bool,
    pub incompatibility: Option<String>,
    pub prerelease: bool,
    pub conflict: bool,
}

fn storage_error(error: impl std::fmt::Display) -> MarketError {
    MarketError::new("network", error.to_string())
}

pub(super) fn compatibility(release: &CatalogRelease) -> Option<String> {
    let host = Version::parse(env!("CARGO_PKG_VERSION"));
    let requirement = VersionReq::parse(&release.compatibility.host_version_req);
    if !matches!((host, requirement), (Ok(host), Ok(req)) if req.matches(&host)) {
        return Some(format!(
            "Requires host {}",
            release.compatibility.host_version_req
        ));
    }
    if release.compatibility.wit_version != "0.6.0" {
        return Some(format!(
            "Requires WIT {}",
            release.compatibility.wit_version
        ));
    }
    if release.compatibility.canonical_format_version != 1 {
        return Some(format!(
            "Requires canonical format {}",
            release.compatibility.canonical_format_version
        ));
    }
    None
}

impl PluginMarketplace {
    pub async fn view(&self, gw: &Gateway) -> Result<MarketplaceView, MarketError> {
        let mut sources = self.store.list_sources().await.map_err(storage_error)?;
        let installed = gw.vendor_plugins.list(gw).await.map_err(storage_error)?;
        let installed: HashMap<_, _> = installed
            .into_iter()
            .map(|p| (p.vendor_id.clone(), p))
            .collect();
        let mut entries = Vec::new();
        let mut update_count = 0;

        for source in &mut sources {
            let incomplete = self
                .store
                .source_incomplete(&source.id)
                .await
                .map_err(storage_error)?;
            let catalogs = self
                .store
                .list_catalogs(&source.id)
                .await
                .map_err(storage_error)?;
            let ledger = self
                .store
                .list_seen_digests(&source.id)
                .await
                .map_err(storage_error)?;
            let mut by_vendor: BTreeMap<String, MarketplaceEntry> = BTreeMap::new();
            let mut seen_artifacts: HashMap<(String, String), String> = HashMap::new();
            let mut seen_components: HashMap<(String, String), String> = HashMap::new();
            let mut conflicts = std::collections::HashSet::new();
            for cached in catalogs {
                let catalog =
                    match verify_catalog(&cached.catalog, &cached.signature, &source.public_key) {
                        Ok(catalog)
                            if catalog.repository_id == source.repository_id
                                && catalog.release_tag == cached.release_tag =>
                        {
                            catalog
                        }
                        _ => {
                            if !source
                                .error
                                .as_ref()
                                .is_some_and(|error| error.code == "key_changed")
                            {
                                source.error = Some(MarketError::new(
                                    "signature_invalid",
                                    "Cached catalog signature or identity is invalid",
                                ));
                            }
                            continue;
                        }
                    };
                let digest = stravia_runtime_contract::protocol::ir::canonical::hash_hex(
                    &Sha256::digest(&cached.catalog).into(),
                );
                for release in catalog.releases {
                    if release.vendor_id == "base" {
                        continue;
                    }
                    let conflict = ledger
                        .get(&(release.vendor_id.clone(), release.version.clone()))
                        .is_some_and(|(digest, conflict)| {
                            *conflict || digest != &release.component.sha256
                        });
                    let artifact_conflict = match seen_artifacts
                        .entry((release.vendor_id.clone(), release.version.clone()))
                    {
                        std::collections::hash_map::Entry::Occupied(previous) => {
                            previous.get() != &release.artifact.sha256
                        }
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert(release.artifact.sha256.clone());
                            false
                        }
                    };
                    let component_conflict = match seen_components
                        .entry((release.vendor_id.clone(), release.version.clone()))
                    {
                        std::collections::hash_map::Entry::Occupied(previous) => {
                            previous.get() != &release.component.sha256
                        }
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert(release.component.sha256.clone());
                            false
                        }
                    };
                    if conflict || artifact_conflict || component_conflict {
                        conflicts.insert((release.vendor_id.clone(), release.version.clone()));
                    }
                    let version = match Version::parse(&release.version) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let prerelease = cached.prerelease || !version.pre.is_empty();
                    let entry = by_vendor
                        .entry(release.vendor_id.clone())
                        .or_insert_with(|| MarketplaceEntry {
                            source_id: source.id.clone(),
                            vendor_id: release.vendor_id.clone(),
                            releases: Vec::new(),
                            incomplete,
                            conflict: false,
                            latest: None,
                            update: None,
                        });
                    entry.conflict |= conflict || artifact_conflict || component_conflict;
                    if prerelease && !source.include_prerelease {
                        continue;
                    }
                    let incompatibility = compatibility(&release);
                    entry.releases.push(MarketplaceRelease {
                        catalog: release,
                        release_id: cached.release_id,
                        release_tag: cached.release_tag.clone(),
                        release_url: cached.release_url.clone(),
                        catalog_sha256: digest.clone(),
                        compatible: incompatibility.is_none(),
                        incompatibility,
                        prerelease,
                        conflict: false,
                    });
                }
            }
            for mut entry in by_vendor.into_values() {
                for release in &mut entry.releases {
                    release.conflict = conflicts
                        .contains(&(entry.vendor_id.clone(), release.catalog.version.clone()));
                }
                entry.releases.sort_by(|a, b| {
                    Version::parse(&b.catalog.version)
                        .ok()
                        .cmp(&Version::parse(&a.catalog.version).ok())
                        .then_with(|| b.release_id.cmp(&a.release_id))
                });
                entry.releases.dedup_by(|a, b| {
                    a.catalog.version == b.catalog.version
                        && a.catalog.component.sha256 == b.catalog.component.sha256
                        && a.catalog.artifact.sha256 == b.catalog.artifact.sha256
                });
                entry.latest = entry
                    .releases
                    .iter()
                    .find(|r| r.compatible && !r.conflict)
                    .cloned();
                if source.enabled
                    && !matches!(
                        source.error.as_ref().map(|e| e.code.as_str()),
                        Some("key_changed" | "signature_invalid" | "source_identity_changed")
                    )
                    && let Some(plugin) = installed.get(&entry.vendor_id)
                    && plugin.origin.as_ref().is_some_and(|origin| {
                        origin.source_id == source.id
                            && origin.repository_id == source.repository_id
                    })
                    && let Ok(version) = Version::parse(&plugin.version)
                {
                    entry.update = entry
                        .releases
                        .iter()
                        .find(|r| {
                            r.compatible
                                && !r.conflict
                                && Version::parse(&r.catalog.version).is_ok_and(|v| v > version)
                        })
                        .cloned();
                }
                if entry.update.is_some() {
                    update_count += 1;
                }
                if !entry.releases.is_empty() {
                    entries.push(entry);
                }
            }
        }
        Ok(MarketplaceView {
            sources,
            entries,
            update_count,
        })
    }
}
