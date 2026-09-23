use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use stravia_web_access::{resolve_browser_executable, validate_browser_executable};
use tokio::sync::Mutex;

use super::AdminService;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSettings {
    pub configured_path: Option<String>,
    pub resolved_path: Option<String>,
    pub source: BrowserSource,
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserSource {
    Manual,
    Environment,
    Automatic,
}

#[derive(Deserialize)]
pub struct BrowserSettingsUpdate {
    pub path: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Preference {
    browser_path: Option<PathBuf>,
}

pub(crate) struct BrowserPreferences {
    store_path: PathBuf,
    pub(crate) load_error: Arc<Mutex<Option<String>>>,
}

impl BrowserPreferences {
    pub(crate) fn load(runtime_dir: &Path) -> (Self, Option<PathBuf>) {
        let store_path = runtime_dir.join("web-access-browser.json");
        let loaded = (|| -> anyhow::Result<Preference> {
            match std::fs::read(&store_path) {
                Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let legacy = runtime_dir.join("desktop-browser.json");
                    let preference = match std::fs::read(&legacy) {
                        Ok(bytes) => serde_json::from_slice(&bytes)?,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return Ok(Preference::default());
                        }
                        Err(error) => return Err(error.into()),
                    };
                    std::fs::rename(&legacy, &store_path)?;
                    Ok(preference)
                }
                Err(error) => Err(error.into()),
            }
        })();
        let (path, error) = match loaded {
            Ok(preference) => (preference.browser_path, None),
            Err(error) => (
                None,
                Some(format!("failed to load browser settings: {error:#}")),
            ),
        };
        (
            Self {
                store_path,
                load_error: Arc::new(Mutex::new(error)),
            },
            path,
        )
    }
}

impl AdminService {
    /// Reports file availability without launching the browser.
    pub async fn get_web_access_browser(&self) -> BrowserSettings {
        let error = self.gw.browser_preferences.load_error.lock().await;
        let path = self
            .gw
            .browser_path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        browser_state(path.as_deref(), error.clone()).await
    }

    /// Validate, atomically persist and then activate. A failed write leaves the old choice active.
    pub async fn update_web_access_browser(
        &self,
        input: BrowserSettingsUpdate,
    ) -> anyhow::Result<BrowserSettings> {
        let mut error = self
            .gw
            .browser_preferences
            .load_error
            .clone()
            .lock_owned()
            .await;
        let path = input.path.map(PathBuf::from);
        if let Some(path) = &path {
            validate_browser_executable(path).await?;
        }
        let store_path = self.gw.browser_preferences.store_path.clone();
        let active: Arc<RwLock<Option<PathBuf>>> = self.gw.browser_path.clone();
        // Keep persistence and activation in one blocking task even if the request is cancelled.
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            persist(
                &store_path,
                &Preference {
                    browser_path: path.clone(),
                },
            )?;
            *active
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
            *error = None;
            Ok(())
        })
        .await??;
        Ok(self.get_web_access_browser().await)
    }
}

fn persist(path: &Path, preference: &Preference) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("browser settings directory is unavailable"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, preference)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

async fn browser_state(path: Option<&Path>, load_error: Option<String>) -> BrowserSettings {
    let source = if path.is_some() {
        BrowserSource::Manual
    } else if std::env::var_os("STRAVIA_CHROME_PATH").is_some() {
        BrowserSource::Environment
    } else {
        BrowserSource::Automatic
    };
    // An unreadable preference is not permission to silently select another browser.
    let (resolved_path, error) = if let Some(error) = load_error {
        (None, Some(error))
    } else {
        match resolve_browser_executable(path).await {
            Ok(path) => (Some(path.to_string_lossy().into_owned()), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        }
    };
    BrowserSettings {
        configured_path: path.map(|path| path.to_string_lossy().into_owned()),
        available: resolved_path.is_some(),
        resolved_path,
        source,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gateway, config::GatewayConfig};

    async fn gateway(path: &Path) -> Gateway {
        Gateway::new(GatewayConfig {
            data_dir: path.to_owned(),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn browser_preferences_save_reset_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = gateway(directory.path()).await;
        let clone = gateway.clone();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let saved = gateway
            .admin()
            .update_web_access_browser(BrowserSettingsUpdate {
                path: Some(executable.clone()),
            })
            .await
            .unwrap();
        assert_eq!(saved.configured_path.as_deref(), Some(executable.as_str()));
        assert_eq!(saved.resolved_path.as_deref(), Some(executable.as_str()));
        assert_eq!(saved.source, BrowserSource::Manual);
        assert!(saved.available);
        assert!(clone.web_access().local_browser_available().await);
        assert_eq!(
            clone.admin().get_web_access_browser().await.resolved_path,
            saved.resolved_path
        );
        let restarted = self::gateway(directory.path()).await;
        assert_eq!(
            restarted
                .admin()
                .get_web_access_browser()
                .await
                .configured_path,
            saved.configured_path
        );
        gateway
            .admin()
            .update_web_access_browser(BrowserSettingsUpdate { path: None })
            .await
            .unwrap();
        assert!(
            clone
                .admin()
                .get_web_access_browser()
                .await
                .configured_path
                .is_none()
        );
        let restarted = self::gateway(directory.path()).await;
        let automatic = restarted.admin().get_web_access_browser().await;
        assert!(automatic.configured_path.is_none());
        let detected = resolve_browser_executable(None)
            .await
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        assert_eq!(automatic.resolved_path, detected);
        assert_eq!(automatic.available, detected.is_some());
        assert_eq!(
            automatic.source,
            if std::env::var_os("STRAVIA_CHROME_PATH").is_some() {
                BrowserSource::Environment
            } else {
                BrowserSource::Automatic
            }
        );
    }

    #[tokio::test]
    async fn invalid_and_failed_persistence_never_activate_selection() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = gateway(directory.path()).await;
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        gateway
            .admin()
            .update_web_access_browser(BrowserSettingsUpdate {
                path: Some(executable.clone()),
            })
            .await
            .unwrap();
        let store = directory.path().join("web-access-browser.json");
        let original = std::fs::read(&store).unwrap();
        let missing = directory
            .path()
            .join("missing.exe")
            .to_string_lossy()
            .into_owned();
        assert!(
            gateway
                .admin()
                .update_web_access_browser(BrowserSettingsUpdate {
                    path: Some(missing)
                })
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&store).unwrap(), original);
        assert_eq!(
            gateway
                .admin()
                .get_web_access_browser()
                .await
                .configured_path,
            Some(executable.clone())
        );
        std::fs::remove_file(&store).unwrap();
        std::fs::create_dir(&store).unwrap();
        assert!(
            gateway
                .admin()
                .update_web_access_browser(BrowserSettingsUpdate { path: None })
                .await
                .is_err()
        );
        assert_eq!(
            gateway
                .clone()
                .admin()
                .get_web_access_browser()
                .await
                .configured_path,
            Some(executable)
        );
        assert!(gateway.web_access().local_browser_available().await);
    }

    #[tokio::test]
    async fn desktop_preference_migrates_once_and_invalid_saved_path_stays_gated() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.exe");
        let legacy = directory.path().join("desktop-browser.json");
        std::fs::write(
            &legacy,
            serde_json::to_vec(&Preference {
                browser_path: Some(missing.clone()),
            })
            .unwrap(),
        )
        .unwrap();
        let gateway = gateway(directory.path()).await;
        assert!(!legacy.exists());
        let state = gateway.admin().get_web_access_browser().await;
        assert_eq!(
            state.configured_path,
            Some(missing.to_string_lossy().into_owned())
        );
        assert!(!state.available);
        assert!(state.error.is_some());
        assert!(!gateway.web_access().local_browser_available().await);
        gateway
            .admin()
            .update_web_access_browser(BrowserSettingsUpdate { path: None })
            .await
            .unwrap();
        assert!(
            self::gateway(directory.path())
                .await
                .admin()
                .get_web_access_browser()
                .await
                .configured_path
                .is_none()
        );
    }

    #[tokio::test]
    async fn malformed_preferences_do_not_fall_back_to_automatic_detection() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("web-access-browser.json"), b"{").unwrap();
        let gateway = gateway(directory.path()).await;
        let state = gateway.admin().get_web_access_browser().await;
        assert!(!state.available);
        assert!(state.resolved_path.is_none());
        assert!(
            state
                .error
                .unwrap()
                .contains("failed to load browser settings")
        );
        assert!(!gateway.web_access().local_browser_available().await);
        let reset = gateway
            .admin()
            .update_web_access_browser(BrowserSettingsUpdate { path: None })
            .await
            .unwrap();
        assert!(reset.configured_path.is_none());
        assert!(
            !reset
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("failed to load browser settings")
        );
    }
}
