use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use reqwest::Url;
use semver::Version;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::AdminService;
use crate::outbound_proxy::{OutboundProxySettings, update_uses_proxy};
use crate::storage::DynStorage;

const UPDATE_STATE_KEY: &str = "product_update_state";
const SKIPPED_VERSION_KEY: &str = "product_update_skipped_version";
const UPDATE_SOURCE_ENV: &str = "STRAVIA_UPDATER_URL";
const DEFAULT_UPDATE_SOURCE_URL: &str = "https://updater.stravia.cn";
const MANIFEST_ASSET_NAME: &str = "stravia-updater.json";
const SUCCESS_CACHE_TTL: chrono::Duration = chrono::Duration::hours(24);
const FAILURE_RETRY_DELAY: chrono::Duration = chrono::Duration::hours(1);
const MAX_RELEASE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_MANIFEST_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_RELEASE_PAGES: usize = 10;
const RELEASE_PAGE_SIZE: usize = 100;
const REQUIRED_PLATFORMS: [&str; 4] = [
    "linux-aarch64",
    "linux-x86_64",
    "windows-aarch64",
    "windows-x86_64",
];

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateCheckMode {
    Automatic,
    Manual,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateCheckStatus {
    Idle,
    UpToDate,
    Available,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: Version,
    pub published_at: DateTime<Utc>,
    pub release_url: String,
    pub manifest_url: String,
    pub download_available: bool,
    pub download_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateFailure {
    pub code: String,
    pub message: String,
    pub attempted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateStatus {
    pub current_version: Version,
    pub check_status: UpdateCheckStatus,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_failure: Option<UpdateFailure>,
    pub available_update: Option<AvailableUpdate>,
    pub skipped: bool,
    pub download_supported: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedUpdateState {
    last_success_at: Option<DateTime<Utc>>,
    last_failure: Option<UpdateFailure>,
    available_update: Option<AvailableUpdate>,
}

#[derive(Debug, Clone)]
struct RemoteRelease {
    version: Version,
    prerelease: bool,
    published_at: DateTime<Utc>,
    release_url: String,
    manifest_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseEntry {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
    html_url: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct UpdateManifest {
    version: Version,
    #[allow(dead_code)]
    pub_date: Option<DateTime<Utc>>,
    release_notes_url: String,
    platforms: UniquePlatforms,
}

#[derive(Debug, Clone)]
struct ManifestPlatform {
    url: String,
    signature: String,
}

impl<'de> Deserialize<'de> for ManifestPlatform {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            url: String,
            signature: String,
        }
        let fields = Fields::deserialize(deserializer)?;
        Ok(Self {
            url: fields.url,
            signature: fields.signature,
        })
    }
}

#[derive(Debug, Clone)]
struct UniquePlatforms(BTreeMap<String, ManifestPlatform>);

impl<'de> Deserialize<'de> for UniquePlatforms {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniquePlatformsVisitor;

        impl<'de> Visitor<'de> for UniquePlatformsVisitor {
            type Value = UniquePlatforms;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map with unique updater platform entries")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((platform, value)) = map.next_entry::<String, ManifestPlatform>()? {
                    if values.insert(platform.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate updater platform entry: {platform}"
                        )));
                    }
                }
                Ok(UniquePlatforms(values))
            }
        }

        deserializer.deserialize_map(UniquePlatformsVisitor)
    }
}

#[derive(Debug, Clone)]
struct SourceError {
    code: &'static str,
    message: String,
}

impl SourceError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[async_trait]
trait ReleaseSource: Send + Sync {
    async fn releases(&self) -> Result<Vec<RemoteRelease>, SourceError>;
    async fn manifest(&self, url: &str) -> Result<UpdateManifest, SourceError>;
    fn validate_manifest_source(&self, url: &str) -> Result<(), SourceError>;
}

trait UpdateClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

struct SystemClock;

impl UpdateClock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug)]
struct UpdaterBaseUrl(Url);

impl UpdaterBaseUrl {
    fn parse(value: Option<&str>, allow_http: bool) -> Result<Self, SourceError> {
        let value = value.unwrap_or(DEFAULT_UPDATE_SOURCE_URL).trim();
        let mut url = Url::parse(value).map_err(|error| {
            SourceError::new(
                "UPDATE_SOURCE_INVALID",
                format!("{UPDATE_SOURCE_ENV} is not a valid URL: {error}"),
            )
        })?;
        let scheme_allowed = url.scheme() == "https" || (allow_http && url.scheme() == "http");
        if !scheme_allowed || url.host_str().is_none() {
            return Err(SourceError::new(
                "UPDATE_SOURCE_INVALID",
                format!("{UPDATE_SOURCE_ENV} must be an absolute HTTPS URL"),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(SourceError::new(
                "UPDATE_SOURCE_INVALID",
                format!("{UPDATE_SOURCE_ENV} must not contain credentials"),
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(SourceError::new(
                "UPDATE_SOURCE_INVALID",
                format!("{UPDATE_SOURCE_ENV} must not contain a query or fragment"),
            ));
        }
        if !url.path().ends_with('/') {
            url.path_segments_mut()
                .map_err(|_| {
                    SourceError::new(
                        "UPDATE_SOURCE_INVALID",
                        format!("{UPDATE_SOURCE_ENV} must be a hierarchical URL"),
                    )
                })?
                .push("");
        }
        Ok(Self(url))
    }

    fn releases_url(&self, page: usize) -> Result<Url, SourceError> {
        let mut url = self.0.clone();
        url.path_segments_mut()
            .map_err(|_| {
                SourceError::new(
                    "UPDATE_SOURCE_INVALID",
                    format!("{UPDATE_SOURCE_ENV} must be a hierarchical URL"),
                )
            })?
            .pop_if_empty()
            .extend(["api", "v1", "releases"]);
        url.query_pairs_mut()
            .append_pair("per_page", &RELEASE_PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        Ok(url)
    }

    fn validate_manifest_source(&self, value: &str) -> Result<(), SourceError> {
        let matches = Url::parse(value).is_ok_and(|url| {
            url.query().is_none()
                && url.fragment().is_none()
                && url
                    .as_str()
                    .strip_prefix(self.0.as_str())
                    .and_then(|path| path.strip_prefix("api/v1/releases/"))
                    .and_then(|path| path.strip_suffix("/manifest.json"))
                    .is_some_and(|tag| !tag.is_empty() && !tag.contains('/'))
        });
        if matches {
            Ok(())
        } else {
            Err(SourceError::new(
                "UPDATE_MANIFEST_SOURCE_MISMATCH",
                "Check for updates again to refresh download metadata for the configured update source",
            ))
        }
    }
}

struct UpdaterReleaseSource {
    storage: DynStorage,
    base_url: Option<String>,
    #[cfg(test)]
    allow_http: bool,
}

impl UpdaterReleaseSource {
    fn new(storage: DynStorage, base_url: Option<String>) -> Self {
        Self {
            storage,
            base_url,
            #[cfg(test)]
            allow_http: false,
        }
    }

    #[cfg(test)]
    fn allows_http(&self) -> bool {
        self.allow_http
    }

    #[cfg(not(test))]
    fn allows_http(&self) -> bool {
        false
    }

    async fn client(&self) -> Result<reqwest::Client, SourceError> {
        self.client_with_timeout(Duration::from_secs(15)).await
    }

    async fn client_with_timeout(&self, timeout: Duration) -> Result<reqwest::Client, SourceError> {
        let settings = self.storage.settings();
        let use_proxy = update_uses_proxy(settings)
            .await
            .map_err(|error| SourceError::new("UPDATE_SETTINGS_UNAVAILABLE", error.to_string()))?;

        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(format!("Stravia/{}", env!("CARGO_PKG_VERSION")));
        if use_proxy {
            let config = OutboundProxySettings::load(settings)
                .await
                .map_err(|error| {
                    SourceError::new("UPDATE_SETTINGS_UNAVAILABLE", error.to_string())
                })?;
            let proxy = config
                .reqwest_proxy()
                .map_err(|error| SourceError::new("UPDATE_PROXY_INVALID", error.to_string()))?;
            builder = builder.proxy(proxy);
        } else {
            builder = builder.no_proxy();
        }
        builder
            .build()
            .map_err(|error| SourceError::new("UPDATE_CLIENT_FAILED", error.to_string()))
    }

    async fn get_limited(
        &self,
        client: &reqwest::Client,
        url: &str,
        limit: usize,
    ) -> Result<Vec<u8>, SourceError> {
        if !self.allows_http() {
            require_https(url, "request URL")?;
        }
        let response = client.get(url).send().await.map_err(|error| {
            SourceError::new("UPDATE_REQUEST_FAILED", format_connectivity_error(&error))
        })?;
        if !self.allows_http() {
            require_https(response.url().as_str(), "redirected request URL")?;
        }
        if !response.status().is_success() {
            return Err(SourceError::new(
                "UPDATE_UPSTREAM_FAILED",
                format!("Update source returned HTTP {}", response.status()),
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(SourceError::new(
                "UPDATE_RESPONSE_TOO_LARGE",
                format!("Update response exceeds {limit} bytes"),
            ));
        }

        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                SourceError::new("UPDATE_REQUEST_FAILED", format_connectivity_error(&error))
            })?;
            if body.len().saturating_add(chunk.len()) > limit {
                return Err(SourceError::new(
                    "UPDATE_RESPONSE_TOO_LARGE",
                    format!("Update response exceeds {limit} bytes"),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[async_trait]
impl ReleaseSource for UpdaterReleaseSource {
    async fn releases(&self) -> Result<Vec<RemoteRelease>, SourceError> {
        let base_url = UpdaterBaseUrl::parse(self.base_url.as_deref(), self.allows_http())?;
        let client = self.client().await?;
        let mut releases = Vec::new();
        for page in 1..=MAX_RELEASE_PAGES {
            let url = base_url.releases_url(page)?;
            let bytes = self
                .get_limited(&client, url.as_str(), MAX_RELEASE_RESPONSE_BYTES)
                .await?;
            let page_releases: Vec<ReleaseEntry> = serde_json::from_slice(&bytes)
                .map_err(|error| SourceError::new("UPDATE_RESPONSE_INVALID", error.to_string()))?;
            let page_len = page_releases.len();
            releases.extend(page_releases.into_iter().filter_map(validate_release));
            if page_len < RELEASE_PAGE_SIZE {
                return Ok(releases);
            }
        }
        Err(SourceError::new(
            "UPDATE_RESPONSE_TOO_LARGE",
            "The update source returned more release pages than Stravia will inspect",
        ))
    }

    async fn manifest(&self, url: &str) -> Result<UpdateManifest, SourceError> {
        self.validate_manifest_source(url)?;
        let client = self.client().await?;
        let bytes = self
            .get_limited(&client, url, MAX_MANIFEST_RESPONSE_BYTES)
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| SourceError::new("UPDATE_MANIFEST_INVALID", error.to_string()))
    }

    fn validate_manifest_source(&self, url: &str) -> Result<(), SourceError> {
        UpdaterBaseUrl::parse(self.base_url.as_deref(), self.allows_http())?
            .validate_manifest_source(url)
    }
}

pub(crate) struct UpdateService {
    storage: DynStorage,
    source: Arc<dyn ReleaseSource>,
    clock: Arc<dyn UpdateClock>,
    current_version: Version,
    download_supported: bool,
    checks_enabled: bool,
    check_lock: tokio::sync::Mutex<()>,
    last_completed_at: tokio::sync::Mutex<Option<Instant>>,
}

impl UpdateService {
    pub(crate) fn updater(
        storage: DynStorage,
        download_supported: bool,
        base_url: Option<String>,
    ) -> anyhow::Result<Self> {
        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .context("CARGO_PKG_VERSION must be valid SemVer")?;
        Ok(Self::new(
            Arc::clone(&storage),
            Arc::new(UpdaterReleaseSource::new(storage, base_url)),
            Arc::new(SystemClock),
            current_version,
            download_supported,
            !cfg!(debug_assertions),
        ))
    }

    fn new(
        storage: DynStorage,
        source: Arc<dyn ReleaseSource>,
        clock: Arc<dyn UpdateClock>,
        current_version: Version,
        download_supported: bool,
        checks_enabled: bool,
    ) -> Self {
        Self {
            storage,
            source,
            clock,
            current_version,
            download_supported,
            checks_enabled,
            check_lock: tokio::sync::Mutex::new(()),
            last_completed_at: tokio::sync::Mutex::new(None),
        }
    }

    pub(crate) async fn status(&self) -> anyhow::Result<UpdateStatus> {
        let state = self.load_state().await?;
        self.view(state).await
    }

    pub(crate) async fn check(&self, mode: UpdateCheckMode) -> anyhow::Result<UpdateStatus> {
        let queued_at = Instant::now();
        let _check = self.check_lock.lock().await;
        let joined_existing_check = self
            .last_completed_at
            .lock()
            .await
            .is_some_and(|completed_at| completed_at >= queued_at);
        if joined_existing_check {
            return self.status().await;
        }

        let result = self.check_exclusive(mode).await;
        *self.last_completed_at.lock().await = Some(Instant::now());
        result
    }

    async fn check_exclusive(&self, mode: UpdateCheckMode) -> anyhow::Result<UpdateStatus> {
        let now = self.clock.now();
        let mut state = self.load_state().await?;
        let cached_source_matches = state
            .available_update
            .as_ref()
            .filter(|update| !update.manifest_url.is_empty())
            .is_none_or(|update| {
                self.source
                    .validate_manifest_source(&update.manifest_url)
                    .is_ok()
            });
        if mode == UpdateCheckMode::Automatic && cached_source_matches {
            if state
                .last_success_at
                .is_some_and(|checked_at| now.signed_duration_since(checked_at) < SUCCESS_CACHE_TTL)
            {
                return self.view(state).await;
            }
            if state.last_failure.as_ref().is_some_and(|failure| {
                now.signed_duration_since(failure.attempted_at) < FAILURE_RETRY_DELAY
            }) {
                return self.view(state).await;
            }
        }
        if !self.checks_enabled {
            state.last_failure = Some(UpdateFailure {
                code: "UPDATE_CHECK_DISABLED".to_string(),
                message: "Production update checks are disabled in debug and test builds"
                    .to_string(),
                attempted_at: now,
            });
            self.save_state(&state).await?;
            return self.view(state).await;
        }

        match self.discover().await {
            Ok(available_update) => {
                state.last_success_at = Some(now);
                state.last_failure = None;
                state.available_update = available_update;
            }
            Err(error) => {
                state.last_failure = Some(UpdateFailure {
                    code: error.code.to_string(),
                    message: error.message,
                    attempted_at: now,
                });
            }
        }
        self.save_state(&state).await?;
        self.view(state).await
    }

    async fn discover(&self) -> Result<Option<AvailableUpdate>, SourceError> {
        let releases = self.source.releases().await?;
        let Some(release) = select_release(&self.current_version, releases) else {
            return Ok(None);
        };
        let Some(manifest_url) = release.manifest_url.clone() else {
            return Ok(Some(AvailableUpdate {
                version: release.version,
                published_at: release.published_at,
                release_url: release.release_url,
                manifest_url: String::new(),
                download_available: false,
                download_error: Some("Update manifest is missing from this Release".to_string()),
            }));
        };

        let manifest_result = self
            .source
            .manifest(&manifest_url)
            .await
            .and_then(|manifest| validate_manifest(&release, manifest));
        let (download_available, download_error) = match manifest_result {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error.message)),
        };
        Ok(Some(AvailableUpdate {
            version: release.version,
            published_at: release.published_at,
            release_url: release.release_url,
            manifest_url,
            download_available,
            download_error,
        }))
    }

    pub(crate) async fn set_skipped_version(
        &self,
        version: Option<&str>,
    ) -> anyhow::Result<UpdateStatus> {
        let value = match version {
            Some(version) => Version::parse(version.trim())?.to_string(),
            None => String::new(),
        };
        self.storage
            .settings()
            .set(SKIPPED_VERSION_KEY, &value)
            .await?;
        self.status().await
    }

    async fn load_state(&self) -> anyhow::Result<PersistedUpdateState> {
        let Some(raw) = self.storage.settings().get(UPDATE_STATE_KEY).await? else {
            return Ok(PersistedUpdateState::default());
        };
        if raw.trim().is_empty() {
            return Ok(PersistedUpdateState::default());
        }
        serde_json::from_str(&raw).context("stored product update state is invalid")
    }

    async fn save_state(&self, state: &PersistedUpdateState) -> anyhow::Result<()> {
        let value = serde_json::to_string(state)?;
        self.storage.settings().set(UPDATE_STATE_KEY, &value).await
    }

    async fn view(&self, state: PersistedUpdateState) -> anyhow::Result<UpdateStatus> {
        // 更新缓存跨安装版本保留；历史候选不能继续作为已安装版本的可用更新。
        let available_update = state
            .available_update
            .filter(|update| update.version > self.current_version)
            .map(|mut update| {
                if !update.manifest_url.is_empty()
                    && let Err(error) = self.source.validate_manifest_source(&update.manifest_url)
                {
                    // 历史检查结果仍可展示，但切换来源后不能绕过新的分发入口。
                    update.manifest_url.clear();
                    update.download_available = false;
                    update.download_error = Some(error.message);
                }
                update
            });
        let skipped_version = self
            .storage
            .settings()
            .get(SKIPPED_VERSION_KEY)
            .await?
            .and_then(|value| Version::parse(value.trim()).ok());
        let skipped = available_update
            .as_ref()
            .is_some_and(|update| skipped_version.as_ref() == Some(&update.version));
        let check_status = if state.last_failure.is_some() {
            UpdateCheckStatus::Error
        } else if available_update.is_some() {
            UpdateCheckStatus::Available
        } else if state.last_success_at.is_some() {
            UpdateCheckStatus::UpToDate
        } else {
            UpdateCheckStatus::Idle
        };
        Ok(UpdateStatus {
            current_version: self.current_version.clone(),
            check_status,
            last_success_at: state.last_success_at,
            last_failure: state.last_failure,
            available_update,
            skipped,
            download_supported: self.download_supported,
        })
    }
}

impl AdminService {
    pub async fn get_update_status(&self) -> anyhow::Result<UpdateStatus> {
        self.gw.update_service.status().await
    }

    pub async fn check_for_updates(&self, mode: UpdateCheckMode) -> anyhow::Result<UpdateStatus> {
        self.gw.update_service.check(mode).await
    }

    pub async fn set_skipped_update_version(
        &self,
        version: Option<&str>,
    ) -> anyhow::Result<UpdateStatus> {
        self.gw.update_service.set_skipped_version(version).await
    }
}

fn select_release(
    current: &Version,
    releases: impl IntoIterator<Item = RemoteRelease>,
) -> Option<RemoteRelease> {
    let stable_channel = current.pre.is_empty();
    releases
        .into_iter()
        .filter(|release| release.version > *current && (!stable_channel || !release.prerelease))
        .max_by(|left, right| left.version.cmp(&right.version))
}

fn validate_release(release: ReleaseEntry) -> Option<RemoteRelease> {
    if release.draft || require_https(&release.html_url, "Release URL").is_err() {
        return None;
    }
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .ok()?;
    if release.prerelease != !version.pre.is_empty() {
        return None;
    }
    let published_at = release.published_at?;
    let manifest_urls = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == MANIFEST_ASSET_NAME)
        .map(|asset| asset.browser_download_url)
        .collect::<Vec<_>>();
    let manifest_url = match manifest_urls.as_slice() {
        [] => None,
        [url] if require_https(url, "manifest URL").is_ok() => Some(url.clone()),
        _ => return None,
    };
    Some(RemoteRelease {
        version,
        prerelease: release.prerelease,
        published_at,
        release_url: release.html_url,
        manifest_url,
    })
}

fn validate_manifest(release: &RemoteRelease, manifest: UpdateManifest) -> Result<(), SourceError> {
    if manifest.version != release.version {
        return Err(SourceError::new(
            "UPDATE_MANIFEST_VERSION_MISMATCH",
            format!(
                "Update manifest version {} does not match Release {}",
                manifest.version, release.version
            ),
        ));
    }
    require_https(&manifest.release_notes_url, "Release notes URL")?;
    if manifest.release_notes_url != release.release_url {
        return Err(SourceError::new(
            "UPDATE_MANIFEST_RELEASE_MISMATCH",
            "Update manifest Release notes URL does not match the selected Release",
        ));
    }

    let keys = manifest
        .platforms
        .0
        .keys()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    for required in REQUIRED_PLATFORMS {
        if !keys.contains(required) {
            return Err(SourceError::new(
                "UPDATE_MANIFEST_PLATFORM_MISSING",
                format!("Update manifest is missing platform {required}"),
            ));
        }
    }
    if keys.len() != REQUIRED_PLATFORMS.len() {
        return Err(SourceError::new(
            "UPDATE_MANIFEST_PLATFORM_INVALID",
            "Update manifest contains an unsupported platform",
        ));
    }
    for (platform, asset) in manifest.platforms.0 {
        require_https(&asset.url, &format!("asset URL for {platform}"))?;
        if asset.signature.trim().is_empty() {
            return Err(SourceError::new(
                "UPDATE_MANIFEST_SIGNATURE_MISSING",
                format!("Update manifest signature is missing for {platform}"),
            ));
        }
    }
    Ok(())
}

fn require_https(url: &str, label: &str) -> Result<(), SourceError> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        SourceError::new("UPDATE_URL_INVALID", format!("Invalid {label}: {error}"))
    })?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(SourceError::new(
            "UPDATE_URL_INVALID",
            format!("{label} must use HTTPS"),
        ));
    }
    Ok(())
}

fn format_connectivity_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "Update source request timed out".to_string()
    } else if error.is_connect() {
        "Unable to connect to the update source".to_string()
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use crate::storage::MemoryStorage;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;

    const RELEASE_URL: &str = "https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v1.2.0";
    const MANIFEST_URL: &str = "https://updates.example.com/api/v1/releases/v1.2.0/manifest.json";

    struct FakeClock(AtomicI64);

    impl FakeClock {
        fn new(now: &str) -> Self {
            Self(AtomicI64::new(
                DateTime::parse_from_rfc3339(now)
                    .unwrap()
                    .timestamp_millis(),
            ))
        }

        fn advance(&self, duration: chrono::Duration) {
            self.0
                .fetch_add(duration.num_milliseconds(), Ordering::SeqCst);
        }
    }

    impl UpdateClock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            DateTime::from_timestamp_millis(self.0.load(Ordering::SeqCst))
                .expect("fake clock timestamp is valid")
        }
    }

    enum ReleaseReply {
        Releases(Vec<RemoteRelease>),
        Error(&'static str),
    }

    struct FakeSource {
        replies: tokio::sync::Mutex<VecDeque<ReleaseReply>>,
        calls: AtomicUsize,
        manifest_calls: tokio::sync::Mutex<Vec<String>>,
        delay: Duration,
    }

    impl FakeSource {
        fn new(replies: impl IntoIterator<Item = ReleaseReply>) -> Self {
            Self {
                replies: tokio::sync::Mutex::new(replies.into_iter().collect()),
                calls: AtomicUsize::new(0),
                manifest_calls: tokio::sync::Mutex::new(Vec::new()),
                delay: Duration::ZERO,
            }
        }

        fn delayed(replies: impl IntoIterator<Item = ReleaseReply>) -> Self {
            Self {
                replies: tokio::sync::Mutex::new(replies.into_iter().collect()),
                calls: AtomicUsize::new(0),
                manifest_calls: tokio::sync::Mutex::new(Vec::new()),
                delay: Duration::from_millis(30),
            }
        }
    }

    #[async_trait]
    impl ReleaseSource for FakeSource {
        async fn releases(&self) -> Result<Vec<RemoteRelease>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            match self.replies.lock().await.pop_front().expect("fake reply") {
                ReleaseReply::Releases(releases) => Ok(releases),
                ReleaseReply::Error(message) => {
                    Err(SourceError::new("UPDATE_REQUEST_FAILED", message))
                }
            }
        }

        async fn manifest(&self, url: &str) -> Result<UpdateManifest, SourceError> {
            self.manifest_calls.lock().await.push(url.to_string());
            Ok(valid_manifest("1.2.0", RELEASE_URL))
        }

        fn validate_manifest_source(&self, url: &str) -> Result<(), SourceError> {
            UpdaterBaseUrl::parse(Some("https://updates.example.com"), false)?
                .validate_manifest_source(url)
        }
    }

    fn release(version: &str, prerelease: bool) -> RemoteRelease {
        RemoteRelease {
            version: Version::parse(version).unwrap(),
            prerelease,
            published_at: DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            release_url: RELEASE_URL.to_string(),
            manifest_url: Some(MANIFEST_URL.to_string()),
        }
    }

    fn release_entry_json(tag: &str, manifest_url: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "published_at": "2026-09-01T00:00:00Z",
            "html_url": format!("https://github.com/Stravia-AI/StraviaPlatform/releases/tag/{tag}"),
            "assets": [{
                "name": MANIFEST_ASSET_NAME,
                "browser_download_url": manifest_url,
            }],
        })
    }

    fn valid_manifest(version: &str, release_url: &str) -> UpdateManifest {
        UpdateManifest {
            version: Version::parse(version).unwrap(),
            pub_date: Some(Utc::now()),
            release_notes_url: release_url.to_string(),
            platforms: UniquePlatforms(
                REQUIRED_PLATFORMS
                    .into_iter()
                    .map(|platform| {
                        (
                            platform.to_string(),
                            ManifestPlatform {
                                url: format!("https://example.invalid/{platform}"),
                                signature: "signed".to_string(),
                            },
                        )
                    })
                    .collect(),
            ),
        }
    }

    async fn local_http_source(router: Router) -> (UpdaterReleaseSource, reqwest::Client, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let source = UpdaterReleaseSource {
            storage,
            base_url: None,
            allow_http: true,
        };
        let client = source.client().await.unwrap();
        (source, client, format!("http://{address}"))
    }

    fn service(source: Arc<FakeSource>, clock: Arc<FakeClock>, current: &str) -> UpdateService {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        UpdateService::new(
            storage,
            source,
            clock,
            Version::parse(current).unwrap(),
            true,
            true,
        )
    }

    #[tokio::test]
    async fn persisted_github_manifest_is_routed_through_the_configured_update_source() {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        storage
            .settings()
            .set(
                UPDATE_STATE_KEY,
                &serde_json::json!({
                    "last_success_at": "2026-09-01T00:00:00Z",
                    "last_failure": null,
                    "available_update": {
                        "version": "1.2.0",
                        "published_at": "2026-09-01T00:00:00Z",
                        "release_url": RELEASE_URL,
                        "manifest_url": "https://github.com/Stravia-AI/StraviaPlatform/releases/download/v1.2.0/stravia-updater.json",
                        "download_available": true,
                        "download_error": null
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();
        let source = Arc::new(UpdaterReleaseSource::new(
            Arc::clone(&storage),
            Some("https://updates.example.com".to_string()),
        ));
        let service = UpdateService::new(
            Arc::clone(&storage),
            source,
            Arc::new(FakeClock::new("2026-09-01T00:01:00Z")),
            Version::parse("1.0.0").unwrap(),
            true,
            true,
        );

        let cached = service.status().await.unwrap().available_update.unwrap();
        assert!(!cached.download_available);
        assert!(cached.manifest_url.is_empty());

        let refreshed = UpdateService::new(
            storage,
            Arc::new(FakeSource::new([ReleaseReply::Releases(vec![release(
                "1.2.0", false,
            )])])),
            Arc::new(FakeClock::new("2026-09-01T00:01:00Z")),
            Version::parse("1.0.0").unwrap(),
            true,
            true,
        );
        let update = refreshed
            .check(UpdateCheckMode::Automatic)
            .await
            .unwrap()
            .available_update
            .unwrap();
        assert_eq!(update.manifest_url, MANIFEST_URL);
        assert!(update.download_available);
    }

    #[tokio::test]
    async fn cached_download_is_available_only_for_its_current_configured_source() {
        let existing = service(
            Arc::new(FakeSource::new([ReleaseReply::Releases(vec![release(
                "1.2.0", false,
            )])])),
            Arc::new(FakeClock::new("2026-09-01T00:00:00Z")),
            "1.0.0",
        );
        existing.check(UpdateCheckMode::Manual).await.unwrap();

        for (base, expected_download) in [
            (None, false),
            (Some("https://another-updater.example.com"), false),
            (Some("https://updates.example.com"), true),
        ] {
            let current = UpdateService::new(
                Arc::clone(&existing.storage),
                Arc::new(UpdaterReleaseSource::new(
                    Arc::clone(&existing.storage),
                    base.map(str::to_string),
                )),
                Arc::new(FakeClock::new("2026-09-01T00:01:00Z")),
                Version::parse("1.0.0").unwrap(),
                true,
                true,
            );
            let update = current.status().await.unwrap().available_update.unwrap();
            assert_eq!(update.download_available, expected_download, "{base:?}");
            assert_eq!(
                update.manifest_url,
                if expected_download { MANIFEST_URL } else { "" },
                "{base:?}"
            );
        }
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn debug_builds_report_update_checks_as_disabled() {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let service = UpdateService::updater(storage, false, None).unwrap();

        let status = service.check(UpdateCheckMode::Manual).await.unwrap();
        assert_eq!(status.check_status, UpdateCheckStatus::Error);
        assert_eq!(
            status.last_failure.map(|failure| failure.code),
            Some("UPDATE_CHECK_DISABLED".to_string())
        );
    }

    #[test]
    fn release_filter_rejects_drafts_invalid_tags_and_non_https_urls() {
        let release = |tag_name: &str, draft: bool, html_url: &str| ReleaseEntry {
            tag_name: tag_name.to_string(),
            draft,
            prerelease: false,
            published_at: DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                .ok()
                .map(|value| value.with_timezone(&Utc)),
            html_url: html_url.to_string(),
            assets: vec![],
        };

        assert!(validate_release(release("1.2.0", true, RELEASE_URL)).is_none());
        assert!(validate_release(release("not-semver", false, RELEASE_URL)).is_none());
        assert!(validate_release(release("1.2.0", false, "http://example.invalid")).is_none());
        let with_manifest = |manifest_url: &str| ReleaseEntry {
            tag_name: "1.2.0".to_string(),
            draft: false,
            prerelease: false,
            published_at: Some(
                DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            html_url: RELEASE_URL.to_string(),
            assets: vec![ReleaseAsset {
                name: MANIFEST_ASSET_NAME.to_string(),
                browser_download_url: manifest_url.to_string(),
            }],
        };
        assert!(validate_release(with_manifest("http://example.invalid/manifest.json")).is_none());
        assert_eq!(
            validate_release(with_manifest(MANIFEST_URL))
                .unwrap()
                .manifest_url
                .as_deref(),
            Some(MANIFEST_URL)
        );
    }

    #[test]
    fn update_source_base_url_requires_https_and_builds_paginated_release_urls() {
        let base =
            UpdaterBaseUrl::parse(Some("https://updates.example.com/distribution"), false).unwrap();
        assert_eq!(
            base.releases_url(3).unwrap().as_str(),
            "https://updates.example.com/distribution/api/v1/releases?per_page=100&page=3"
        );

        let trailing =
            UpdaterBaseUrl::parse(Some("https://updates.example.com/distribution/"), false)
                .unwrap();
        assert_eq!(
            trailing.releases_url(1).unwrap().as_str(),
            "https://updates.example.com/distribution/api/v1/releases?per_page=100&page=1"
        );

        let root = UpdaterBaseUrl::parse(Some("https://updates.example.com/"), false).unwrap();
        assert_eq!(
            root.releases_url(1).unwrap().as_str(),
            "https://updates.example.com/api/v1/releases?per_page=100&page=1"
        );

        for invalid in [
            "",
            "  ",
            "http://updates.example.com",
            "updates.example.com",
            "https://user:secret@updates.example.com",
            "https://updates.example.com?token=1",
            "https://updates.example.com#fragment",
        ] {
            assert_eq!(
                UpdaterBaseUrl::parse(Some(invalid), false)
                    .unwrap_err()
                    .code,
                "UPDATE_SOURCE_INVALID",
                "{invalid}"
            );
        }
        assert!(UpdaterBaseUrl::parse(Some("http://127.0.0.1:8080"), true).is_ok());
    }

    #[tokio::test]
    async fn invalid_explicit_update_source_is_reported_as_a_check_failure() {
        for configured in [
            "",
            "  ",
            "http://updates.example.com",
            "updates.example.com",
        ] {
            let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
            let source = Arc::new(UpdaterReleaseSource::new(
                Arc::clone(&storage),
                Some(configured.to_string()),
            ));
            let service = UpdateService::new(
                storage,
                source,
                Arc::new(FakeClock::new("2026-09-05T00:00:00Z")),
                Version::parse("1.0.0").unwrap(),
                true,
                true,
            );

            let status = service.check(UpdateCheckMode::Manual).await.unwrap();
            assert_eq!(status.check_status, UpdateCheckStatus::Error);
            assert_eq!(
                status
                    .last_failure
                    .as_ref()
                    .map(|failure| failure.code.as_str()),
                Some("UPDATE_SOURCE_INVALID")
            );
            assert!(status.available_update.is_none());
        }
    }

    #[tokio::test]
    async fn discovery_fetches_the_service_manifest_and_keeps_the_github_release_url() {
        let available = release("1.2.0", false);
        let source = Arc::new(FakeSource::new([ReleaseReply::Releases(vec![available])]));
        let clock = Arc::new(FakeClock::new("2026-09-05T00:00:00Z"));
        let service = service(Arc::clone(&source), clock, "1.0.0");

        let status = service.check(UpdateCheckMode::Manual).await.unwrap();
        let update = status.available_update.expect("available update");
        assert_eq!(update.release_url, RELEASE_URL);
        assert_eq!(update.manifest_url, MANIFEST_URL);
        assert!(update.download_available);
        let manifest_calls = source.manifest_calls.lock().await;
        assert_eq!(manifest_calls.len(), 1);
        assert_eq!(manifest_calls[0], MANIFEST_URL);
    }

    #[tokio::test]
    async fn update_source_reads_paginated_releases_from_configured_base() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}/distribution");
        let manifest_url =
            "https://updates.example.com/distribution/api/v1/releases/v2.0.0/manifest.json";
        let first_page = (1..=RELEASE_PAGE_SIZE)
            .map(|index| {
                release_entry_json(
                    &format!("v1.{index}.0"),
                    &format!(
                        "https://updates.example.com/distribution/api/v1/releases/v1.{index}.0/manifest.json"
                    ),
                )
            })
            .collect::<Vec<_>>();
        let pages = Arc::new(vec![
            first_page,
            vec![release_entry_json("v2.0.0", manifest_url)],
        ]);
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let observed_requests = Arc::clone(&requests);
        let served_pages = Arc::clone(&pages);
        let router = Router::new().fallback(move |uri: axum::http::Uri| {
            let pages = Arc::clone(&served_pages);
            let requests = Arc::clone(&observed_requests);
            async move {
                requests.lock().await.push(match uri.query() {
                    Some(query) => format!("{}?{query}", uri.path()),
                    None => uri.path().to_string(),
                });
                let page = uri
                    .query()
                    .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("page=")))
                    .and_then(|page| page.parse::<usize>().ok())
                    .unwrap_or(1);
                let body = pages
                    .get(page.saturating_sub(1))
                    .cloned()
                    .unwrap_or_default();
                axum::Json(body).into_response()
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let mut source = UpdaterReleaseSource::new(storage, Some(base));
        source.allow_http = true;
        let releases = source.releases().await.unwrap();

        assert_eq!(releases.len(), RELEASE_PAGE_SIZE + 1);
        let selected = select_release(&Version::parse("0.5.3").unwrap(), releases).unwrap();
        assert_eq!(selected.version, Version::parse("2.0.0").unwrap());
        assert_eq!(
            selected.release_url,
            "https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v2.0.0"
        );
        assert_eq!(selected.manifest_url.as_deref(), Some(manifest_url));
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0],
            "/distribution/api/v1/releases?per_page=100&page=1"
        );
        assert_eq!(
            requests[1],
            "/distribution/api/v1/releases?per_page=100&page=2"
        );
    }

    #[tokio::test]
    async fn update_source_rejects_more_than_ten_release_pages() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let page = Arc::new(
            (1..=RELEASE_PAGE_SIZE)
                .map(|index| {
                    release_entry_json(
                        &format!("v1.{index}.0"),
                        "https://updates.example.com/api/v1/releases/v1/manifest.json",
                    )
                })
                .collect::<Vec<_>>(),
        );
        let requests = Arc::new(AtomicUsize::new(0));
        let served_page = Arc::clone(&page);
        let observed_requests = Arc::clone(&requests);
        let router = Router::new().fallback(move || {
            let page = Arc::clone(&served_page);
            let requests = Arc::clone(&observed_requests);
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                axum::Json(page.as_ref().clone()).into_response()
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let mut source = UpdaterReleaseSource::new(storage, Some(format!("http://{address}")));
        source.allow_http = true;
        let error = source.releases().await.unwrap_err();

        assert_eq!(error.code, "UPDATE_RESPONSE_TOO_LARGE");
        assert_eq!(requests.load(Ordering::SeqCst), MAX_RELEASE_PAGES);
    }

    #[tokio::test]
    async fn update_source_reports_non_success_oversize_and_timeout() {
        let (source, client, url) = local_http_source(
            Router::new().route("/failure", get(|| async { StatusCode::BAD_GATEWAY })),
        )
        .await;
        let failure = source
            .get_limited(&client, &format!("{url}/failure"), 32)
            .await
            .unwrap_err();
        assert_eq!(failure.code, "UPDATE_UPSTREAM_FAILED");

        let (source, client, url) =
            local_http_source(Router::new().route("/large", get(|| async { "too large" }))).await;
        let oversize = source
            .get_limited(&client, &format!("{url}/large"), 4)
            .await
            .unwrap_err();
        assert_eq!(oversize.code, "UPDATE_RESPONSE_TOO_LARGE");

        let (source, _, url) = local_http_source(Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                "late"
            }),
        ))
        .await;
        let client = source
            .client_with_timeout(Duration::from_millis(10))
            .await
            .unwrap();
        let timeout = source
            .get_limited(&client, &format!("{url}/slow"), 32)
            .await
            .unwrap_err();
        assert_eq!(timeout.code, "UPDATE_REQUEST_FAILED");
    }

    #[tokio::test]
    async fn update_proxy_flag_is_independent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_url = format!("http://{}/probe", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/probe", get(|| async { "origin" })),
            )
            .await
            .unwrap();
        });

        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let source = UpdaterReleaseSource {
            storage: Arc::clone(&storage),
            base_url: None,
            allow_http: true,
        };
        let set_proxy_config = async |url: &str| {
            storage
                .settings()
                .set(
                    crate::outbound_proxy::SETTINGS_KEY,
                    &serde_json::to_string(&OutboundProxySettings {
                        url: url.to_string(),
                        ..Default::default()
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        };

        let unused_proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unused_proxy_url = format!("http://{}", unused_proxy_listener.local_addr().unwrap());
        drop(unused_proxy_listener);

        set_proxy_config(&unused_proxy_url).await;
        let client = source.client().await.unwrap();
        let body = source.get_limited(&client, &origin_url, 32).await.unwrap();
        assert_eq!(body, b"origin");

        set_proxy_config("not a proxy URL").await;
        let client = source.client().await.unwrap();
        let body = source.get_limited(&client, &origin_url, 32).await.unwrap();
        assert_eq!(body, b"origin");

        storage
            .settings()
            .set(crate::outbound_proxy::UPDATE_USE_PROXY_KEY, "true")
            .await
            .unwrap();
        let error = source.client().await.unwrap_err();
        assert_eq!(error.code, "UPDATE_PROXY_INVALID");
    }

    #[tokio::test]
    async fn update_proxy_bypass_is_evaluated_on_redirects() {
        let origin_requests = Arc::new(AtomicUsize::new(0));
        let origin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin_listener.local_addr().unwrap().port();
        let observed_origin_requests = Arc::clone(&origin_requests);
        let origin = Router::new().route(
            "/done",
            get(move || {
                let requests = Arc::clone(&observed_origin_requests);
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    "origin"
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(origin_listener, origin).await.unwrap();
        });

        let proxy_requests = Arc::new(AtomicUsize::new(0));
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", proxy_listener.local_addr().unwrap());
        let redirect_url = format!("http://127.0.0.1:{origin_port}/done");
        let observed_proxy_requests = Arc::clone(&proxy_requests);
        let proxy = Router::new().fallback(move || {
            let requests = Arc::clone(&observed_proxy_requests);
            let redirect_url = redirect_url.clone();
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                axum::response::Redirect::temporary(&redirect_url)
            }
        });
        tokio::spawn(async move {
            axum::serve(proxy_listener, proxy).await.unwrap();
        });

        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        storage
            .settings()
            .set(
                crate::outbound_proxy::SETTINGS_KEY,
                &serde_json::to_string(&OutboundProxySettings {
                    url: proxy_url,
                    bypass: "127.0.0.1".to_string(),
                    force_http1: false,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        storage
            .settings()
            .set(crate::outbound_proxy::UPDATE_USE_PROXY_KEY, "true")
            .await
            .unwrap();
        let source = UpdaterReleaseSource {
            storage,
            base_url: None,
            allow_http: true,
        };
        let client = source.client().await.unwrap();

        let body = source
            .get_limited(
                &client,
                &format!("http://localhost:{origin_port}/start"),
                1024,
            )
            .await
            .unwrap();
        assert_eq!(body, b"origin");
        assert_eq!(proxy_requests.load(Ordering::SeqCst), 1);
        assert_eq!(origin_requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stable_install_selects_highest_newer_stable_release() {
        let selected = select_release(
            &Version::parse("1.0.0").unwrap(),
            [
                release("1.2.0", false),
                release("1.3.0-rc.1", true),
                release("1.1.0", false),
            ],
        )
        .expect("a stable update should be available");
        assert_eq!(selected.version, Version::parse("1.2.0").unwrap());
    }

    #[test]
    fn prerelease_install_selects_highest_newer_release_across_channels() {
        let selected = select_release(
            &Version::parse("1.3.0-beta.1").unwrap(),
            [
                release("1.3.0-rc.1", true),
                release("1.3.0", false),
                release("1.2.9", false),
            ],
        )
        .expect("a later release should be available");
        assert_eq!(selected.version, Version::parse("1.3.0").unwrap());
    }

    #[tokio::test]
    async fn installed_update_is_not_offered_from_persisted_cache() {
        let source = Arc::new(FakeSource::new([
            ReleaseReply::Releases(vec![release("1.2.0", false)]),
            ReleaseReply::Error("offline"),
        ]));
        let clock = Arc::new(FakeClock::new("2026-09-05T00:00:00Z"));
        let old = service(Arc::clone(&source), Arc::clone(&clock), "1.0.0");
        old.check(UpdateCheckMode::Manual).await.unwrap();
        old.set_skipped_version(Some("1.2.0")).await.unwrap();
        let installed = UpdateService::new(
            Arc::clone(&old.storage),
            source,
            clock,
            Version::parse("1.2.0").unwrap(),
            true,
            true,
        );

        for status in [
            installed.status().await.unwrap(),
            installed.check(UpdateCheckMode::Automatic).await.unwrap(),
        ] {
            assert!(status.available_update.is_none());
            assert_eq!(status.check_status, UpdateCheckStatus::UpToDate);
            assert!(!status.skipped);
        }
        let failed = installed.check(UpdateCheckMode::Manual).await.unwrap();
        assert_eq!(failed.check_status, UpdateCheckStatus::Error);
        assert!(failed.available_update.is_none());
        assert!(!failed.skipped);
    }

    #[tokio::test]
    async fn automatic_check_caches_success_for_twenty_four_hours_and_manual_bypasses_it() {
        let source = Arc::new(FakeSource::new([
            ReleaseReply::Releases(vec![release("1.2.0", false)]),
            ReleaseReply::Releases(vec![release("1.2.0", false)]),
        ]));
        let clock = Arc::new(FakeClock::new("2026-09-05T00:00:00Z"));
        let service = service(Arc::clone(&source), Arc::clone(&clock), "1.0.0");

        service.check(UpdateCheckMode::Automatic).await.unwrap();
        clock.advance(chrono::Duration::hours(23));
        service.check(UpdateCheckMode::Automatic).await.unwrap();
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);

        service.check(UpdateCheckMode::Manual).await.unwrap();
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn automatic_failure_is_limited_for_one_hour_and_preserves_cached_success() {
        let source = Arc::new(FakeSource::new([
            ReleaseReply::Releases(vec![release("1.2.0", false)]),
            ReleaseReply::Error("offline"),
            ReleaseReply::Releases(vec![release("1.2.0", false)]),
        ]));
        let clock = Arc::new(FakeClock::new("2026-09-05T00:00:00Z"));
        let service = service(Arc::clone(&source), Arc::clone(&clock), "1.0.0");

        service.check(UpdateCheckMode::Automatic).await.unwrap();
        clock.advance(chrono::Duration::hours(25));
        let failed = service.check(UpdateCheckMode::Automatic).await.unwrap();
        assert_eq!(failed.check_status, UpdateCheckStatus::Error);
        assert_eq!(
            failed.available_update.unwrap().version,
            Version::parse("1.2.0").unwrap()
        );

        clock.advance(chrono::Duration::minutes(59));
        service.check(UpdateCheckMode::Automatic).await.unwrap();
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
        clock.advance(chrono::Duration::minutes(1));
        service.check(UpdateCheckMode::Automatic).await.unwrap();
        assert_eq!(source.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn skip_only_suppresses_the_exact_available_version() {
        let source = Arc::new(FakeSource::new([
            ReleaseReply::Releases(vec![release("1.2.0", false)]),
            ReleaseReply::Releases(vec![release("1.3.0", false)]),
        ]));
        let clock = Arc::new(FakeClock::new("2026-09-05T00:00:00Z"));
        let service = service(source, Arc::clone(&clock), "1.0.0");

        service.check(UpdateCheckMode::Manual).await.unwrap();
        let skipped = service.set_skipped_version(Some("1.2.0")).await.unwrap();
        assert!(skipped.skipped);

        clock.advance(chrono::Duration::minutes(1));
        let newer = service.check(UpdateCheckMode::Manual).await.unwrap();
        assert_eq!(
            newer.available_update.unwrap().version,
            Version::parse("1.3.0").unwrap()
        );
        assert!(!newer.skipped);
    }

    #[tokio::test]
    async fn concurrent_manual_checks_share_one_release_request() {
        let source = Arc::new(FakeSource::delayed([ReleaseReply::Releases(vec![
            release("1.2.0", false),
        ])]));
        let clock = Arc::new(FakeClock::new("2026-09-05T00:00:00Z"));
        let service = Arc::new(service(Arc::clone(&source), clock, "1.0.0"));

        let (left, right) = tokio::join!(
            service.check(UpdateCheckMode::Manual),
            service.check(UpdateCheckMode::Manual)
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manifest_rejects_duplicate_platform_entries() {
        let json = r#"{
          "version":"1.2.0",
          "release_notes_url":"https://example.invalid/release",
          "platforms":{
            "linux-x86_64":{"url":"https://example.invalid/a","signature":"a"},
            "linux-x86_64":{"url":"https://example.invalid/b","signature":"b"}
          }
        }"#;
        assert!(serde_json::from_str::<UpdateManifest>(json).is_err());
    }

    #[test]
    fn manifest_rejects_non_https_assets_and_incomplete_platforms() {
        let release = release("1.2.0", false);
        let mut manifest = valid_manifest("1.2.0", RELEASE_URL);
        manifest.platforms.0.get_mut("linux-x86_64").unwrap().url =
            "http://example.invalid/update".to_string();
        assert_eq!(
            validate_manifest(&release, manifest).unwrap_err().code,
            "UPDATE_URL_INVALID"
        );

        let mut incomplete = valid_manifest("1.2.0", RELEASE_URL);
        incomplete.platforms.0.remove("windows-aarch64");
        assert_eq!(
            validate_manifest(&release, incomplete).unwrap_err().code,
            "UPDATE_MANIFEST_PLATFORM_MISSING"
        );
    }
}
