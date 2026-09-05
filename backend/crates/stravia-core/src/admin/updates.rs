use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use semver::Version;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::AdminService;
use crate::storage::DynStorage;

const UPDATE_STATE_KEY: &str = "product_update_state";
const SKIPPED_VERSION_KEY: &str = "product_update_skipped_version";
const RELEASES_URL: &str = "https://api.github.com/repos/Stravia-AI/StraviaPlatform/releases";
const MANIFEST_ASSET_NAME: &str = "stravia-updater.json";
const SUCCESS_CACHE_TTL: chrono::Duration = chrono::Duration::hours(24);
const FAILURE_RETRY_DELAY: chrono::Duration = chrono::Duration::hours(1);
const MAX_RELEASE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_MANIFEST_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_RELEASE_PAGES: usize = 10;
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
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
    html_url: String,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
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

struct GitHubReleaseSource {
    storage: DynStorage,
    #[cfg(test)]
    allow_http: bool,
}

impl GitHubReleaseSource {
    fn new(storage: DynStorage) -> Self {
        Self {
            storage,
            #[cfg(test)]
            allow_http: false,
        }
    }

    async fn client(&self) -> Result<reqwest::Client, SourceError> {
        self.client_with_timeout(Duration::from_secs(15)).await
    }

    async fn client_with_timeout(&self, timeout: Duration) -> Result<reqwest::Client, SourceError> {
        let settings = self.storage.settings();
        let proxy_enabled = settings
            .get("proxy_enabled")
            .await
            .map_err(|error| SourceError::new("UPDATE_SETTINGS_UNAVAILABLE", error.to_string()))?
            .as_deref()
            .is_some_and(parse_bool_setting);

        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(format!("Stravia/{}", env!("CARGO_PKG_VERSION")));
        if proxy_enabled {
            let proxy_url = settings
                .get("proxy_url")
                .await
                .map_err(|error| {
                    SourceError::new("UPDATE_SETTINGS_UNAVAILABLE", error.to_string())
                })?
                .unwrap_or_default();
            if proxy_url.trim().is_empty() {
                return Err(SourceError::new(
                    "UPDATE_PROXY_INVALID",
                    "Outbound proxy is enabled but proxy_url is empty",
                ));
            }
            let proxy = reqwest::Proxy::all(proxy_url.trim())
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
        #[cfg(not(test))]
        require_https(url, "request URL")?;
        #[cfg(test)]
        if !self.allow_http {
            require_https(url, "request URL")?;
        }
        let response = client.get(url).send().await.map_err(|error| {
            SourceError::new("UPDATE_REQUEST_FAILED", format_connectivity_error(&error))
        })?;
        #[cfg(not(test))]
        require_https(response.url().as_str(), "redirected request URL")?;
        #[cfg(test)]
        if !self.allow_http {
            require_https(response.url().as_str(), "redirected request URL")?;
        }
        if !response.status().is_success() {
            return Err(SourceError::new(
                "UPDATE_UPSTREAM_FAILED",
                format!("GitHub returned HTTP {}", response.status()),
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
impl ReleaseSource for GitHubReleaseSource {
    async fn releases(&self) -> Result<Vec<RemoteRelease>, SourceError> {
        let client = self.client().await?;
        let mut releases = Vec::new();
        for page in 1..=MAX_RELEASE_PAGES {
            let url = format!("{RELEASES_URL}?per_page=100&page={page}");
            let bytes = self
                .get_limited(&client, &url, MAX_RELEASE_RESPONSE_BYTES)
                .await?;
            let page_releases: Vec<GitHubRelease> = serde_json::from_slice(&bytes)
                .map_err(|error| SourceError::new("UPDATE_RESPONSE_INVALID", error.to_string()))?;
            let page_len = page_releases.len();
            releases.extend(
                page_releases
                    .into_iter()
                    .filter_map(validate_github_release),
            );
            if page_len < 100 {
                return Ok(releases);
            }
        }
        Err(SourceError::new(
            "UPDATE_RESPONSE_TOO_LARGE",
            "GitHub returned more release pages than Stravia will inspect",
        ))
    }

    async fn manifest(&self, url: &str) -> Result<UpdateManifest, SourceError> {
        let client = self.client().await?;
        let bytes = self
            .get_limited(&client, url, MAX_MANIFEST_RESPONSE_BYTES)
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| SourceError::new("UPDATE_MANIFEST_INVALID", error.to_string()))
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
    pub(crate) fn github(storage: DynStorage, download_supported: bool) -> anyhow::Result<Self> {
        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .context("CARGO_PKG_VERSION must be valid SemVer")?;
        Ok(Self::new(
            Arc::clone(&storage),
            Arc::new(GitHubReleaseSource::new(storage)),
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
        if mode == UpdateCheckMode::Automatic {
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
        let skipped_version = self
            .storage
            .settings()
            .get(SKIPPED_VERSION_KEY)
            .await?
            .and_then(|value| Version::parse(value.trim()).ok());
        let skipped = state
            .available_update
            .as_ref()
            .is_some_and(|update| skipped_version.as_ref() == Some(&update.version));
        let check_status = if state.last_failure.is_some() {
            UpdateCheckStatus::Error
        } else if state.available_update.is_some() {
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
            available_update: state.available_update,
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

fn validate_github_release(release: GitHubRelease) -> Option<RemoteRelease> {
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

fn parse_bool_setting(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn format_connectivity_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "GitHub update request timed out".to_string()
    } else if error.is_connect() {
        "Unable to connect to GitHub Releases".to_string()
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
    use axum::routing::get;

    use super::*;

    const RELEASE_URL: &str = "https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v1.2.0";
    const MANIFEST_URL: &str = "https://github.com/Stravia-AI/StraviaPlatform/releases/download/v1.2.0/stravia-updater.json";

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
        delay: Duration,
    }

    impl FakeSource {
        fn new(replies: impl IntoIterator<Item = ReleaseReply>) -> Self {
            Self {
                replies: tokio::sync::Mutex::new(replies.into_iter().collect()),
                calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
            }
        }

        fn delayed(replies: impl IntoIterator<Item = ReleaseReply>) -> Self {
            Self {
                replies: tokio::sync::Mutex::new(replies.into_iter().collect()),
                calls: AtomicUsize::new(0),
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

        async fn manifest(&self, _url: &str) -> Result<UpdateManifest, SourceError> {
            Ok(valid_manifest("1.2.0", RELEASE_URL))
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

    async fn local_http_source(router: Router) -> (GitHubReleaseSource, reqwest::Client, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let source = GitHubReleaseSource {
            storage,
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

    #[test]
    fn debug_builds_disable_the_production_release_source_by_default() {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let service = UpdateService::github(storage, false).unwrap();

        assert_eq!(service.checks_enabled, !cfg!(debug_assertions));
    }

    #[test]
    fn github_release_filter_rejects_drafts_invalid_tags_and_non_https_urls() {
        let release = |tag_name: &str, draft: bool, html_url: &str| GitHubRelease {
            tag_name: tag_name.to_string(),
            draft,
            prerelease: false,
            published_at: DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                .ok()
                .map(|value| value.with_timezone(&Utc)),
            html_url: html_url.to_string(),
            assets: vec![],
        };

        assert!(validate_github_release(release("1.2.0", true, RELEASE_URL)).is_none());
        assert!(validate_github_release(release("not-semver", false, RELEASE_URL)).is_none());
        assert!(
            validate_github_release(release("1.2.0", false, "http://example.invalid")).is_none()
        );
    }

    #[tokio::test]
    async fn http_source_reports_non_success_oversize_and_timeout_without_github() {
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
        assert_eq!(timeout.message, "GitHub update request timed out");
    }

    #[tokio::test]
    async fn proxy_setting_is_enforced_only_when_enabled() {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        storage
            .settings()
            .set("proxy_url", "not a proxy URL")
            .await
            .unwrap();
        let source = GitHubReleaseSource {
            storage: Arc::clone(&storage),
            allow_http: false,
        };
        source.client().await.unwrap();

        storage
            .settings()
            .set("proxy_enabled", "true")
            .await
            .unwrap();
        let error = source.client().await.unwrap_err();
        assert_eq!(error.code, "UPDATE_PROXY_INVALID");
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
        let error = serde_json::from_str::<UpdateManifest>(json).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate updater platform entry")
        );
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
