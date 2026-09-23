//! Runtime Provider Catalog owned by the base vendor.
//!
//! The host persists the snapshot and scope bodies and schedules refreshes;
//! this module owns fetching, validation, and profile derivation through the
//! `sync-catalog` export. Catalog entries are upstream identities claiming an
//! SDK implementation — only entries mapping to an implemented protocol/auth
//! shape become Provider Profiles.

use std::collections::BTreeMap;

use serde::Deserialize;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    CatalogSyncOutcome, CatalogSyncRequest, ErrorKind, GuestHost, HttpRequest, PluginError,
    ProviderDescriptor, read_http_body,
};

const MAX_VERSION_BODY: usize = 64 * 1024;
const MAX_INDEX_BODY: usize = 16 * 1024 * 1024;
const MAX_SCOPE_BODY: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct RemoteVersion {
    revision: String,
    generated_at: String,
}

/// One `providers.json` entry. Extra upstream fields (`doc`, `env`) are
/// tolerated but ignored — profiles never use documentation URLs.
#[derive(Deserialize)]
pub(crate) struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub npm: String,
    #[serde(default)]
    pub api: Option<String>,
}

/// `sync-catalog` implementation: optionally fetch the provider scope, then
/// either derive profiles from the supplied/embedded data or refresh the
/// index and derive from the confirmed revision.
pub(crate) fn sync_catalog(
    host: &GuestHost,
    request: &CatalogSyncRequest,
) -> Result<CatalogSyncOutcome, PluginError> {
    let base_url = request.catalog_base_url.trim_end_matches('/');
    let scope_body = match request.scope_provider_id.as_deref() {
        Some(provider_id) => Some(fetch_scope(host, base_url, provider_id)?),
        None => None,
    };
    if !request.refresh_remote {
        let (providers, revision, generated_at) = match request.snapshot_body.as_deref() {
            Some(body) => (
                derive_profiles(body)?,
                request.snapshot_revision.clone().unwrap_or_default(),
                request.snapshot_generated_at.clone(),
            ),
            None => (
                bootstrap_profiles(),
                "bootstrap".to_owned(),
                Some("built-in".to_owned()),
            ),
        };
        return Ok(CatalogSyncOutcome {
            revision,
            generated_at,
            snapshot_body: None,
            scope_body,
            providers,
        });
    }

    let announced = fetch_version(host, base_url)?;
    let index_body = fetch_document(host, base_url, "providers.json", MAX_INDEX_BODY)?;
    // A second version read catches a publish racing the index fetch; the
    // profiles then come from the revision the service confirmed.
    let confirmed = fetch_version(host, base_url)?;
    let (version, index_body) = if confirmed.revision == announced.revision {
        (announced, index_body)
    } else {
        let index_body = fetch_document(host, base_url, "providers.json", MAX_INDEX_BODY)?;
        (confirmed, index_body)
    };
    let providers = derive_profiles(&index_body)?;
    Ok(CatalogSyncOutcome {
        revision: version.revision,
        generated_at: Some(version.generated_at),
        snapshot_body: Some(index_body),
        scope_body,
        providers,
    })
}

/// The complete Provider Profile set owned by this vendor for a catalog
/// snapshot: statically declared internal profiles plus the catalog-derived
/// set. The host swaps the whole set — a profile absent here disappears.
pub(crate) fn provider_set(catalog_profiles: Vec<ProviderDescriptor>) -> Vec<ProviderDescriptor> {
    let mut providers = catalog_profiles;
    providers.extend(crate::INTERNAL_PROVIDER_IDS.iter().map(|provider_id| {
        crate::provider_descriptor(provider_id)
            .unwrap_or_else(|| panic!("base provider descriptor `{provider_id}` must exist"))
    }));
    providers
}

/// Profiles derived from the embedded bootstrap catalog — identity-only
/// fallbacks used before the first successful remote sync.
fn bootstrap_profiles() -> Vec<ProviderDescriptor> {
    metadata_entries()
        .iter()
        .map(|profile| {
            crate::metadata::catalog_profile_descriptor(
                profile.id,
                profile.name,
                profile.npm,
                profile.api,
            )
            .unwrap_or_else(|| {
                panic!(
                    "bundled Catalog profile `{}` uses unsupported implementation `{}`",
                    profile.id, profile.npm
                )
            })
        })
        .collect()
}

fn metadata_entries() -> &'static [crate::metadata::BundledCatalogProfile] {
    crate::metadata::bundled_catalog_profiles()
}

/// Parses `providers.json` and derives the implementable subset. Malformed
/// entries fail the whole snapshot; unimplemented `npm` values are dropped.
pub(crate) fn derive_profiles(body: &str) -> Result<Vec<ProviderDescriptor>, PluginError> {
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_str(body).map_err(|_| {
        common::plugin_error(ErrorKind::Invalid, "provider index is not valid JSON")
    })?;
    let mut providers = Vec::new();
    for (key, value) in raw {
        let entry: CatalogEntry = serde_json::from_value(value).map_err(|_| {
            common::plugin_error(
                ErrorKind::Invalid,
                format!("provider index entry `{key}` is missing id/name/npm"),
            )
        })?;
        if entry.id != key || !valid_provider_id(&entry.id) {
            return Err(common::plugin_error(
                ErrorKind::Invalid,
                format!("provider index entry `{key}` has an invalid id"),
            ));
        }
        if entry.name.trim().is_empty() || entry.npm.trim().is_empty() {
            return Err(common::plugin_error(
                ErrorKind::Invalid,
                format!("provider index entry `{key}` has an empty name or npm"),
            ));
        }
        if let Some(descriptor) = crate::metadata::catalog_profile_descriptor(
            &entry.id,
            &entry.name,
            &entry.npm,
            entry.api.as_deref(),
        ) {
            providers.push(descriptor);
        }
    }
    if providers.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "provider index contains no supported providers",
        ));
    }
    Ok(providers)
}

fn valid_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn fetch_scope(host: &GuestHost, base_url: &str, provider_id: &str) -> Result<String, PluginError> {
    if !valid_provider_id(provider_id) {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "catalog scope provider id is invalid",
        ));
    }
    let path = format!("providers/{provider_id}/models.json");
    let response = request(host, base_url, &path)?;
    let status = response.status()?;
    if status == 404 {
        return Err(PluginError {
            kind: ErrorKind::ProviderNotFound,
            message: format!("provider `{provider_id}` has no catalog scope"),
            upstream_status: Some(status),
        });
    }
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_SCOPE_BODY)?;
    ensure_success(status, &headers, &body, "provider scope")?;
    String::from_utf8(body)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "provider scope is not valid UTF-8"))
}

fn fetch_version(host: &GuestHost, base_url: &str) -> Result<RemoteVersion, PluginError> {
    let body = fetch_document(host, base_url, "version.json", MAX_VERSION_BODY)?;
    let version: RemoteVersion = serde_json::from_str(&body).map_err(|_| {
        common::plugin_error(ErrorKind::Invalid, "catalog version is not valid JSON")
    })?;
    if version.revision.is_empty()
        || version.revision.len() > 256
        || !version
            .revision
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "catalog revision must be a non-empty safe path segment",
        ));
    }
    if version.generated_at.trim().is_empty() || version.generated_at.len() > 256 {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "catalog generated_at must be a non-empty string",
        ));
    }
    Ok(version)
}

fn fetch_document(
    host: &GuestHost,
    base_url: &str,
    path: &str,
    limit: usize,
) -> Result<String, PluginError> {
    let response = request(host, base_url, path)?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, limit)?;
    ensure_success(status, &headers, &body, path)?;
    String::from_utf8(body)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, format!("{path} is not valid UTF-8")))
}

fn request(
    host: &GuestHost,
    base_url: &str,
    path: &str,
) -> Result<stravia_vendor_sdk::HttpResponse, PluginError> {
    host.http_start(HttpRequest {
        method: "GET".to_owned(),
        url: format!("{base_url}/{path}"),
        headers: vec![("accept".to_owned(), "application/json".to_owned())],
        body: Vec::new(),
    })
}

fn ensure_success(
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    document: &str,
) -> Result<(), PluginError> {
    if !(200..300).contains(&status) {
        let mut error = common::upstream_error(status, headers, body);
        error.message = format!("catalog {document} fetch failed: {}", error.message);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_profiles_filters_unsupported_implementations() {
        let body = r#"{
            "made-up": {"id": "made-up", "name": "Made Up", "npm": "@fake/sdk", "api": "https://x"},
            "remote-openai": {"id": "remote-openai", "name": "Remote OpenAI", "npm": "@ai-sdk/openai-compatible", "api": "https://api.remote.test/v1"}
        }"#;
        let providers = derive_profiles(body).expect("index parses");
        assert_eq!(providers.len(), 1);
        let profile = &providers[0];
        assert_eq!(profile.provider_id, "remote-openai");
        assert_eq!(profile.catalog_id.as_deref(), Some("remote-openai"));
        assert_eq!(
            profile.implementation.as_deref(),
            Some("@ai-sdk/openai-compatible")
        );
        assert_eq!(
            profile.channels[0].default_base_url.as_deref(),
            Some("https://api.remote.test/v1")
        );
    }

    #[test]
    fn derive_profiles_rejects_mismatched_ids() {
        let body = r#"{"a": {"id": "b", "name": "B", "npm": "@ai-sdk/openai"}}"#;
        assert!(derive_profiles(body).is_err());
    }

    #[test]
    fn derive_profiles_rejects_index_without_supported_providers() {
        let body = r#"{"x": {"id": "x", "name": "X", "npm": "@fake/sdk"}}"#;
        assert!(derive_profiles(body).is_err());
    }

    #[test]
    fn bootstrap_profiles_cover_the_embedded_catalog() {
        let profiles = bootstrap_profiles();
        assert_eq!(
            profiles.len(),
            crate::metadata::bundled_catalog_profiles().len()
        );
        assert!(profiles.iter().all(|profile| profile.catalog_id.is_some()));
        assert!(
            profiles
                .iter()
                .all(|profile| profile.implementation.is_some())
        );
    }

    #[test]
    fn provider_set_contains_internals_plus_catalog() {
        let set = provider_set(
            derive_profiles(
                r#"{"x": {"id": "x", "name": "X", "npm": "@ai-sdk/openai-compatible"}}"#,
            )
            .expect("index parses"),
        );
        assert!(set.iter().any(|p| p.provider_id == "custom"));
        assert!(set.iter().any(|p| p.provider_id == "x"));
        assert!(!set.iter().any(|p| p.provider_id == "openai"));
    }
}
