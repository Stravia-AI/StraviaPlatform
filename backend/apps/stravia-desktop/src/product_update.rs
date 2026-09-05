use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use stravia_core::Gateway;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};

const UPDATE_PROGRESS_EVENT: &str = "stravia://product-update-progress";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DesktopUpdatePhase {
    Idle,
    Downloading,
    Downloaded,
    Installing,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DesktopUpdateSnapshot {
    pub phase: DesktopUpdatePhase,
    pub target_version: Option<String>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}

impl Default for DesktopUpdateSnapshot {
    fn default() -> Self {
        Self {
            phase: DesktopUpdatePhase::Idle,
            target_version: None,
            downloaded_bytes: 0,
            total_bytes: None,
            error: None,
        }
    }
}

#[derive(Clone, Serialize)]
struct DesktopUpdateProgress {
    target_version: String,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    finished: bool,
}

enum DownloadedUpdate {
    Verified {
        update: Update,
        bytes: Vec<u8>,
    },
    #[cfg(any(test, feature = "desktop-e2e"))]
    TestBridge,
}

#[derive(Default)]
struct DesktopUpdateInner {
    snapshot: DesktopUpdateSnapshot,
    downloaded: Option<DownloadedUpdate>,
}

#[derive(Default)]
pub struct DesktopUpdateState {
    inner: tokio::sync::Mutex<DesktopUpdateInner>,
    operation: tokio::sync::Mutex<()>,
}

impl DesktopUpdateState {
    async fn snapshot(&self) -> DesktopUpdateSnapshot {
        self.inner.lock().await.snapshot.clone()
    }

    async fn begin_download(
        &self,
        target_version: &str,
    ) -> Result<Option<DesktopUpdateSnapshot>, String> {
        let mut inner = self.inner.lock().await;
        if matches!(
            inner.snapshot.phase,
            DesktopUpdatePhase::Downloading | DesktopUpdatePhase::Installing
        ) {
            return Err("A Desktop update operation is already in progress".to_string());
        }
        if inner.snapshot.phase == DesktopUpdatePhase::Downloaded
            && inner.snapshot.target_version.as_deref() == Some(target_version)
            && inner.downloaded.is_some()
        {
            return Ok(Some(inner.snapshot.clone()));
        }
        inner.downloaded = None;
        inner.snapshot = DesktopUpdateSnapshot {
            phase: DesktopUpdatePhase::Downloading,
            target_version: Some(target_version.to_string()),
            downloaded_bytes: 0,
            total_bytes: None,
            error: None,
        };
        Ok(None)
    }

    async fn complete_download(
        &self,
        target_version: &str,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
        downloaded: DownloadedUpdate,
    ) -> DesktopUpdateSnapshot {
        let mut inner = self.inner.lock().await;
        inner.snapshot = DesktopUpdateSnapshot {
            phase: DesktopUpdatePhase::Downloaded,
            target_version: Some(target_version.to_string()),
            downloaded_bytes,
            total_bytes,
            error: None,
        };
        inner.downloaded = Some(downloaded);
        inner.snapshot.clone()
    }

    async fn fail(&self, target_version: Option<&str>, message: String) {
        let mut inner = self.inner.lock().await;
        inner.downloaded = None;
        inner.snapshot = DesktopUpdateSnapshot {
            phase: DesktopUpdatePhase::Error,
            target_version: target_version.map(ToOwned::to_owned),
            downloaded_bytes: 0,
            total_bytes: None,
            error: Some(message),
        };
    }
}

#[tauri::command]
pub async fn get_desktop_update_state(
    state: State<'_, DesktopUpdateState>,
) -> Result<DesktopUpdateSnapshot, String> {
    Ok(state.snapshot().await)
}

#[tauri::command]
pub async fn download_product_update(
    app: AppHandle,
    gateway: State<'_, Gateway>,
    state: State<'_, DesktopUpdateState>,
    version: String,
) -> Result<DesktopUpdateSnapshot, String> {
    let _operation = state.operation.lock().await;
    let status = gateway
        .admin()
        .get_update_status()
        .await
        .map_err(|error| error.to_string())?;
    let available = status
        .available_update
        .filter(|available| available.version.to_string() == version)
        .ok_or_else(|| "The requested version is not the current available update".to_string())?;
    if !status.download_supported || !available.download_available {
        return Err(available
            .download_error
            .unwrap_or_else(|| "Desktop download is unavailable for this update".to_string()));
    }
    if available.manifest_url.is_empty() {
        return Err("The selected Release has no updater manifest".to_string());
    }

    if let Some(downloaded) = state.begin_download(&version).await? {
        return Ok(downloaded);
    }
    #[cfg(feature = "desktop-e2e")]
    {
        let progress = DesktopUpdateProgress {
            target_version: version.clone(),
            downloaded_bytes: 64,
            total_bytes: Some(128),
            finished: false,
        };
        let _ = app.emit(UPDATE_PROGRESS_EVENT, progress);
        tokio::time::sleep(Duration::from_millis(750)).await;
        return Ok(state
            .complete_download(&version, 128, Some(128), DownloadedUpdate::TestBridge)
            .await);
    }

    #[cfg(not(feature = "desktop-e2e"))]
    let result = download_verified_update(&app, &gateway, &version, &available.manifest_url).await;
    #[cfg(not(feature = "desktop-e2e"))]
    match result {
        Ok((update, bytes, downloaded_bytes, total_bytes)) => Ok(state
            .complete_download(
                &version,
                downloaded_bytes,
                total_bytes,
                DownloadedUpdate::Verified { update, bytes },
            )
            .await),
        Err(message) => {
            state.fail(Some(&version), message.clone()).await;
            Err(message)
        }
    }
}

async fn download_verified_update(
    app: &AppHandle,
    gateway: &Gateway,
    version: &str,
    manifest_url: &str,
) -> Result<(Update, Vec<u8>, u64, Option<u64>), String> {
    let public_key = option_env!("STRAVIA_UPDATER_PUBLIC_KEY")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "This Desktop build has no embedded updater public key".to_string())?;
    let endpoint = url::Url::parse(manifest_url).map_err(|error| error.to_string())?;
    if endpoint.scheme() != "https" {
        return Err("Updater manifests must use HTTPS".to_string());
    }

    let mut builder = app
        .updater_builder()
        .pubkey(public_key)
        .endpoints(vec![endpoint])
        .map_err(|error| error.to_string())?
        .timeout(Duration::from_secs(300));
    let settings = gateway.storage.settings();
    let proxy_enabled = settings
        .get("proxy_enabled")
        .await
        .map_err(|error| error.to_string())?
        .as_deref()
        .is_some_and(parse_bool_setting);
    if proxy_enabled {
        let proxy_url = settings
            .get("proxy_url")
            .await
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        if proxy_url.trim().is_empty() {
            return Err("Outbound proxy is enabled but proxy_url is empty".to_string());
        }
        let proxy = url::Url::parse(proxy_url.trim()).map_err(|error| error.to_string())?;
        builder = builder.proxy(proxy);
    } else {
        builder = builder.no_proxy();
    }

    let updater = builder.build().map_err(|error| error.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "The selected updater manifest has no newer version".to_string())?;
    if update.version != version {
        return Err(format!(
            "Updater manifest returned version {}, expected {version}",
            update.version
        ));
    }

    let downloaded = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(u64::MAX));
    let downloaded_for_progress = Arc::clone(&downloaded);
    let total_for_progress = Arc::clone(&total);
    let app_for_progress = app.clone();
    let progress_version = version.to_string();
    let app_for_finish = app.clone();
    let finish_version = version.to_string();
    let downloaded_for_finish = Arc::clone(&downloaded);
    let total_for_finish = Arc::clone(&total);
    let bytes = update
        .download(
            move |chunk_length, content_length| {
                if let Some(content_length) = content_length {
                    total_for_progress.store(content_length, Ordering::Relaxed);
                }
                let downloaded_bytes = downloaded_for_progress
                    .fetch_add(chunk_length as u64, Ordering::Relaxed)
                    + chunk_length as u64;
                let total_bytes = atomic_optional(&total_for_progress);
                let _ = app_for_progress.emit(
                    UPDATE_PROGRESS_EVENT,
                    DesktopUpdateProgress {
                        target_version: progress_version.clone(),
                        downloaded_bytes,
                        total_bytes,
                        finished: false,
                    },
                );
            },
            move || {
                let _ = app_for_finish.emit(
                    UPDATE_PROGRESS_EVENT,
                    DesktopUpdateProgress {
                        target_version: finish_version,
                        downloaded_bytes: downloaded_for_finish.load(Ordering::Relaxed),
                        total_bytes: atomic_optional(&total_for_finish),
                        finished: true,
                    },
                );
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let downloaded_bytes = downloaded.load(Ordering::Relaxed);
    Ok((update, bytes, downloaded_bytes, atomic_optional(&total)))
}

#[tauri::command]
pub async fn install_product_update(
    app: AppHandle,
    state: State<'_, DesktopUpdateState>,
) -> Result<(), String> {
    let _operation = state.operation.lock().await;
    let mut inner = state.inner.lock().await;
    if inner.snapshot.target_version.is_none() {
        return Err("No downloaded Desktop update is available".to_string());
    }
    if inner.snapshot.phase != DesktopUpdatePhase::Downloaded {
        return Err("No downloaded Desktop update is ready to install".to_string());
    }
    inner.snapshot.phase = DesktopUpdatePhase::Installing;
    inner.snapshot.error = None;
    let result = match inner.downloaded.as_ref() {
        Some(DownloadedUpdate::Verified { update, bytes }) => update
            .clone()
            .restart_after_install(true)
            .install(bytes)
            .map_err(|error| error.to_string()),
        #[cfg(any(test, feature = "desktop-e2e"))]
        Some(DownloadedUpdate::TestBridge) => Ok(()),
        None => Err("Downloaded update bytes are unavailable".to_string()),
    };
    if let Err(message) = result {
        inner.snapshot.phase = DesktopUpdatePhase::Downloaded;
        inner.snapshot.error = Some(message.clone());
        return Err(message);
    }
    drop(inner);

    #[cfg(feature = "desktop-e2e")]
    {
        let _ = app;
        Ok(())
    }

    #[cfg(all(not(feature = "desktop-e2e"), target_os = "linux"))]
    {
        app.restart()
    }

    #[cfg(all(not(feature = "desktop-e2e"), not(target_os = "linux")))]
    {
        let _ = app;
        Ok(())
    }
}

fn atomic_optional(value: &AtomicU64) -> Option<u64> {
    match value.load(Ordering::Relaxed) {
        u64::MAX => None,
        value => Some(value),
    }
}

fn parse_bool_setting(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn download_state_is_single_flight_and_failure_has_one_retryable_target() {
        let state = DesktopUpdateState::default();
        assert!(state.begin_download("1.2.0").await.unwrap().is_none());
        assert_eq!(
            state.begin_download("1.2.0").await.unwrap_err(),
            "A Desktop update operation is already in progress"
        );

        state
            .fail(Some("1.2.0"), "network unavailable".to_string())
            .await;
        let failed = state.snapshot().await;
        assert_eq!(failed.phase, DesktopUpdatePhase::Error);
        assert_eq!(failed.target_version.as_deref(), Some("1.2.0"));
        assert_eq!(failed.error.as_deref(), Some("network unavailable"));

        assert!(state.begin_download("1.2.0").await.unwrap().is_none());
        assert_eq!(
            state.snapshot().await.phase,
            DesktopUpdatePhase::Downloading
        );
    }

    #[tokio::test]
    async fn repeated_download_reuses_the_verified_same_version() {
        let state = DesktopUpdateState::default();
        assert!(state.begin_download("1.2.0").await.unwrap().is_none());
        let downloaded = state
            .complete_download("1.2.0", 42, Some(42), DownloadedUpdate::TestBridge)
            .await;

        assert_eq!(
            state.begin_download("1.2.0").await.unwrap(),
            Some(downloaded)
        );
        assert_eq!(state.snapshot().await.phase, DesktopUpdatePhase::Downloaded);
    }

    #[test]
    fn unknown_download_length_stays_indeterminate() {
        let total = AtomicU64::new(u64::MAX);
        assert_eq!(atomic_optional(&total), None);
        total.store(42, Ordering::Relaxed);
        assert_eq!(atomic_optional(&total), Some(42));
    }
}
