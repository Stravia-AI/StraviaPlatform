use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::Emitter;
use tauri_plugin_opener::OpenerExt;

pub(crate) const STARTUP_EVENT: &str = "desktop-startup-changed";
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StartupStatus {
    Starting,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupStage {
    DataDirectory,
    Gateway,
    Session,
    Http,
    Desktop,
}

impl StartupStage {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::DataDirectory => "data_directory",
            Self::Gateway => "gateway",
            Self::Session => "session",
            Self::Http => "http",
            Self::Desktop => "desktop",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct StartupError {
    pub code: String,
    pub details: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct StartupWarning {
    pub code: String,
    pub details: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopStartupSnapshot {
    pub status: StartupStatus,
    pub stage: String,
    pub error: Option<StartupError>,
    pub warnings: Vec<StartupWarning>,
    pub log_path: Option<String>,
    pub previous_failure: bool,
}

struct Lifecycle {
    snapshot: DesktopStartupSnapshot,
    shutting_down: bool,
    tray_available: bool,
}

pub(crate) struct StartupController {
    lifecycle: Mutex<Lifecycle>,
    diagnostics: Arc<StartupDiagnostics>,
    initialization_done: AtomicBool,
    initialized: tokio::sync::Notify,
    shutdown_cleanup_started: AtomicBool,
    shutdown_complete: AtomicBool,
    shutdown_finished: tokio::sync::Notify,
}

impl StartupController {
    pub(crate) fn new_with_previous_failure(
        diagnostics: Arc<StartupDiagnostics>,
        previous_failure: bool,
    ) -> Self {
        Self {
            lifecycle: Mutex::new(Lifecycle {
                snapshot: DesktopStartupSnapshot {
                    status: StartupStatus::Starting,
                    stage: StartupStage::DataDirectory.code().to_string(),
                    error: None,
                    warnings: Vec::new(),
                    log_path: diagnostics
                        .path()
                        .map(|path| path.to_string_lossy().into_owned()),
                    previous_failure: previous_failure || diagnostics.previous_failure(),
                },
                shutting_down: false,
                tray_available: false,
            }),
            diagnostics,
            initialization_done: AtomicBool::new(false),
            initialized: tokio::sync::Notify::new(),
            shutdown_cleanup_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
            shutdown_finished: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn snapshot(&self) -> DesktopStartupSnapshot {
        let mut snapshot = self.lifecycle.lock().snapshot.clone();
        snapshot.log_path = self
            .diagnostics
            .path()
            .map(|path| path.to_string_lossy().into_owned());
        snapshot
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.lifecycle.lock().shutting_down
    }

    pub(crate) fn stage(&self, app: &tauri::AppHandle, stage: StartupStage) -> bool {
        {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.shutting_down || lifecycle.snapshot.status != StartupStatus::Starting {
                return false;
            }
            lifecycle.snapshot.stage = stage.code().to_string();
        }
        self.diagnostics
            .record("INFO", stage.code(), "startup stage entered");
        publish(app, &self.snapshot());
        true
    }

    pub(crate) fn warning(
        &self,
        app: &tauri::AppHandle,
        code: &'static str,
        details: &'static str,
    ) {
        {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.shutting_down {
                return;
            }
            if !lifecycle
                .snapshot
                .warnings
                .iter()
                .any(|warning| warning.code == code)
            {
                lifecycle.snapshot.warnings.push(StartupWarning {
                    code: code.to_string(),
                    details: details.to_string(),
                });
            }
        }
        self.diagnostics.record("WARN", code, details);
        publish(app, &self.snapshot());
    }

    pub(crate) fn fail(&self, app: &tauri::AppHandle, stage: StartupStage, details: &'static str) {
        if self.transition_failure(stage, details).is_none() {
            return;
        }
        self.diagnostics.record("ERROR", stage.code(), details);
        publish(app, &self.snapshot());
    }

    fn transition_failure(
        &self,
        stage: StartupStage,
        details: &'static str,
    ) -> Option<DesktopStartupSnapshot> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.shutting_down {
            return None;
        }
        lifecycle.snapshot.status = StartupStatus::Failed;
        lifecycle.snapshot.stage = stage.code().to_string();
        lifecycle.snapshot.error = Some(StartupError {
            code: stage.code().to_string(),
            details: details.to_string(),
        });
        Some(lifecycle.snapshot.clone())
    }

    pub(crate) fn ready(&self, app: &tauri::AppHandle) -> bool {
        let snapshot = {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.shutting_down || lifecycle.snapshot.status != StartupStatus::Starting {
                return false;
            }
            lifecycle.snapshot.status = StartupStatus::Ready;
            lifecycle.snapshot.stage = StartupStage::Desktop.code().to_string();
            lifecycle.snapshot.error = None;
            self.diagnostics
                .record("INFO", StartupStage::Desktop.code(), "startup ready");
            let mut snapshot = lifecycle.snapshot.clone();
            snapshot.log_path = self
                .diagnostics
                .path()
                .map(|path| path.to_string_lossy().into_owned());
            publish(app, &snapshot);
            snapshot
        };
        snapshot.status == StartupStatus::Ready
    }

    pub(crate) fn set_tray_available(&self, available: bool) {
        self.lifecycle.lock().tray_available = available;
    }

    pub(crate) fn close_should_hide(&self) -> bool {
        let lifecycle = self.lifecycle.lock();
        lifecycle.snapshot.status == StartupStatus::Ready && lifecycle.tray_available
    }

    pub(crate) fn begin_shutdown(&self) -> bool {
        self.lifecycle.lock().shutting_down = true;
        self.shutdown_cleanup_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn shutdown_complete(&self) -> bool {
        self.shutdown_complete.load(Ordering::Acquire)
    }

    pub(crate) fn finish_shutdown(&self) {
        self.shutdown_complete.store(true, Ordering::Release);
        self.shutdown_finished.notify_waiters();
    }

    pub(crate) async fn wait_for_shutdown(&self) {
        while !self.shutdown_complete() {
            let notified = self.shutdown_finished.notified();
            if self.shutdown_complete() {
                break;
            }
            notified.await;
        }
    }

    pub(crate) fn finish_initialization(&self) {
        self.initialization_done.store(true, Ordering::Release);
        self.initialized.notify_waiters();
    }

    pub(crate) async fn wait_for_initialization(&self) {
        while !self.initialization_done.load(Ordering::Acquire) {
            let notified = self.initialized.notified();
            if self.initialization_done.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }

    pub(crate) fn mark_clean_shutdown(&self) {
        self.diagnostics.mark_clean();
    }

    pub(crate) fn diagnostics(&self) -> &Arc<StartupDiagnostics> {
        &self.diagnostics
    }
}

fn publish(app: &tauri::AppHandle, snapshot: &DesktopStartupSnapshot) {
    if let Err(error) = app.emit_to(
        tauri::EventTarget::webview_window("main"),
        STARTUP_EVENT,
        snapshot,
    ) {
        tracing::debug!(%error, "failed to publish desktop startup state");
    }
}

struct LogFile {
    file: File,
    path: PathBuf,
    backup: PathBuf,
    size: u64,
}

pub(crate) struct StartupDiagnostics {
    log: Mutex<Option<LogFile>>,
    path: Mutex<Option<PathBuf>>,
    marker: Option<PathBuf>,
    previous_failure: bool,
}

pub(crate) struct DiagnosticsInitialization {
    pub diagnostics: Arc<StartupDiagnostics>,
    pub warning: Option<&'static str>,
}

impl StartupDiagnostics {
    pub(crate) fn initialize(preferred_dir: Option<PathBuf>) -> DiagnosticsInitialization {
        let fallback_dir = std::env::temp_dir()
            .join("stravia-desktop")
            .join("diagnostics");
        let mut used_fallback = false;
        let mut opened = preferred_dir
            .as_deref()
            .and_then(|directory| open_log(directory).ok());
        if opened.is_none() {
            used_fallback = true;
            opened = open_log(&fallback_dir).ok();
        }

        let (log, path, marker, previous_failure, marker_failed) = match opened {
            Some(log) => {
                let path = log.path.clone();
                let marker = path.with_file_name("startup-incomplete");
                let previous_failure = marker.try_exists().unwrap_or(false);
                let marker_failed = fs::write(&marker, b"startup-incomplete\n").is_err();
                (
                    Some(log),
                    Some(path),
                    Some(marker),
                    previous_failure,
                    marker_failed,
                )
            }
            None => (None, None, None, false, false),
        };
        let diagnostics = Arc::new(Self {
            log: Mutex::new(log),
            path: Mutex::new(path),
            marker,
            previous_failure,
        });
        let build_identity = format!(
            "desktop diagnostics initialized version={} platform={}-{}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        );
        diagnostics.record("INFO", "diagnostics", &build_identity);
        DiagnosticsInitialization {
            warning: if diagnostics.path().is_none() {
                Some("Desktop diagnostics could not create a writable log file.")
            } else if marker_failed {
                Some("Desktop diagnostics could not record abnormal process termination.")
            } else if used_fallback {
                Some(if preferred_dir.is_some() {
                    "The primary diagnostic location was unavailable; a temporary log is in use."
                } else {
                    "The system diagnostic location was unavailable; a temporary log is in use."
                })
            } else {
                None
            },
            diagnostics,
        }
    }

    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.path.lock().clone()
    }

    pub(crate) fn previous_failure(&self) -> bool {
        self.previous_failure
    }

    pub(crate) fn record(&self, level: &str, code: &str, details: &str) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let line = format!("timestamp_ms={timestamp_ms} {level} code={code} details={details}\n");
        let mut guard = self.log.lock();
        let failure = {
            let Some(log) = guard.as_mut() else {
                return;
            };
            let mut failure = None;
            if log.size.saturating_add(line.len() as u64) > MAX_LOG_BYTES {
                if rotate_log(log).is_err() {
                    eprintln!(
                        "Stravia desktop diagnostics could not rotate the log; the current log will be reset."
                    );
                    if log.file.set_len(0).is_err() || log.file.seek(SeekFrom::Start(0)).is_err() {
                        failure = Some(
                            "Stravia desktop diagnostics became unavailable during log rotation.",
                        );
                    }
                }
                log.size = 0;
            }
            if failure.is_none() {
                if log.file.write_all(line.as_bytes()).is_err() || log.file.flush().is_err() {
                    failure = Some(
                        "Stravia desktop diagnostics became unavailable while writing the log.",
                    );
                } else {
                    log.size = log.size.saturating_add(line.len() as u64);
                }
            }
            failure
        };
        if let Some(message) = failure {
            guard.take();
            drop(guard);
            self.path.lock().take();
            eprintln!("{message}");
        }
    }

    pub(crate) fn mark_incomplete(&self) {
        if let Some(marker) = &self.marker
            && fs::write(marker, b"startup-incomplete\n").is_err()
        {
            tracing::warn!("failed to persist desktop startup failure marker");
        }
    }

    pub(crate) fn mark_clean(&self) {
        if let Some(marker) = &self.marker
            && let Err(error) = fs::remove_file(marker)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            self.record(
                "WARN",
                "diagnostics",
                "failed to clear the startup failure marker",
            );
        }
    }

    pub(crate) fn install_panic_hook(self: &Arc<Self>) {
        let diagnostics = Arc::clone(self);
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map_or("unknown", |location| location.file());
            let details = if location.contains("startup") {
                "fatal panic in desktop startup"
            } else {
                "fatal panic in desktop process"
            };
            diagnostics.record("ERROR", "panic", details);
            eprintln!("Stravia encountered a fatal error. See the desktop startup log.");
            std::process::abort();
        }));
    }
}

fn open_log(directory: &Path) -> std::io::Result<LogFile> {
    fs::create_dir_all(directory)?;
    let path = directory.join("desktop-startup.log");
    let backup = directory.join("desktop-startup.previous.log");
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES)
    {
        match fs::remove_file(&backup) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&path, &backup)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let size = file.metadata()?.len();
    Ok(LogFile {
        file,
        path,
        backup,
        size,
    })
}

fn rotate_log(log: &mut LogFile) -> std::io::Result<()> {
    match fs::remove_file(&log.backup) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    log.file.flush()?;
    fs::copy(&log.path, &log.backup)?;
    log.file.set_len(0)?;
    log.file.seek(SeekFrom::Start(0))?;
    log.size = 0;
    Ok(())
}

pub(crate) fn safe_error_details(stage: StartupStage, error: &anyhow::Error) -> &'static str {
    let categories = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    match stage {
        StartupStage::DataDirectory if categories.contains("legacy data layout") => {
            "The data directory uses an incompatible older layout that this version cannot upgrade. Move it aside or start with a fresh data directory."
        }
        StartupStage::DataDirectory
            if categories.contains("in use by another instance")
                || categories.contains("resource temporarily unavailable") =>
        {
            "The data directory is in use by another Stravia instance or migration."
        }
        StartupStage::DataDirectory
            if categories.contains("permission denied")
                || categories.contains("access is denied")
                || categories.contains("read-only") =>
        {
            "Stravia does not have permission to access the data directory."
        }
        StartupStage::Gateway
            if categories.contains("not a database")
                || categories.contains("database disk image is malformed")
                || categories.contains("database is malformed")
                || categories.contains("invalid database") =>
        {
            "The Stravia database is invalid or damaged. Stravia did not reset or repair it."
        }
        StartupStage::Gateway
            if categories.contains("permission denied")
                || categories.contains("access is denied") =>
        {
            "Stravia does not have permission to open its database."
        }
        StartupStage::DataDirectory => "Stravia could not prepare or lock its data directory.",
        StartupStage::Gateway => {
            "Stravia could not initialize the gateway. It was not automatically reset or repaired."
        }
        StartupStage::Session => "Stravia could not establish the local administrator session.",
        StartupStage::Http => "Stravia could not start the local HTTP listener.",
        StartupStage::Desktop => "Stravia could not finish desktop integration startup.",
    }
}

fn ensure_main(webview: &tauri::WebviewWindow) -> Result<(), String> {
    if webview.label() == "main" {
        Ok(())
    } else {
        Err("desktop startup controls are available only to the main WebView".to_string())
    }
}

#[tauri::command]
pub(crate) fn get_desktop_startup_state(
    webview: tauri::WebviewWindow,
    startup: tauri::State<'_, Arc<StartupController>>,
) -> Result<DesktopStartupSnapshot, String> {
    ensure_main(&webview)?;
    Ok(startup.snapshot())
}

#[tauri::command]
pub(crate) async fn restart_desktop(
    webview: tauri::WebviewWindow,
    app: tauri::AppHandle,
    startup: tauri::State<'_, Arc<StartupController>>,
) -> Result<(), String> {
    ensure_main(&webview)?;
    if !startup.begin_shutdown() {
        return Ok(());
    }
    crate::shutdown_desktop(&app, startup.inner()).await;
    startup.finish_shutdown();
    app.request_restart();
    Ok(())
}

#[tauri::command]
pub(crate) fn exit_desktop(
    webview: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<(), String> {
    ensure_main(&webview)?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
pub(crate) fn open_desktop_logs(
    webview: tauri::WebviewWindow,
    app: tauri::AppHandle,
    startup: tauri::State<'_, Arc<StartupController>>,
) -> Result<(), String> {
    ensure_main(&webview)?;
    let path = startup
        .diagnostics()
        .path()
        .ok_or_else(|| "No desktop diagnostic log is available.".to_string())?;
    app.opener()
        .open_path(path.to_string_lossy().into_owned(), None::<String>)
        .map_err(|_| "The desktop diagnostic log could not be opened.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_is_terminal_and_shutdown_blocks_later_transitions() {
        let temp = tempfile::tempdir().expect("temporary diagnostics");
        let diagnostics =
            StartupDiagnostics::initialize(Some(temp.path().to_path_buf())).diagnostics;
        let controller = StartupController::new_with_previous_failure(diagnostics, false);
        let failed = controller
            .transition_failure(
                StartupStage::DataDirectory,
                "The data directory uses an older layout.",
            )
            .expect("failure transition");
        assert_eq!(failed.status, StartupStatus::Failed);
        assert_eq!(
            failed.error.as_ref().map(|error| error.code.as_str()),
            Some("data_directory")
        );

        assert!(controller.begin_shutdown());
        assert!(
            controller
                .transition_failure(StartupStage::Gateway, "must not replace the first failure")
                .is_none()
        );
        assert_eq!(controller.snapshot(), failed);
    }

    #[test]
    fn safe_details_never_copy_url_credentials_or_query_secrets() {
        let error = anyhow::anyhow!(
            "database failed at https://admin:password@example.test/db?token=secret-api-key"
        );
        let details = safe_error_details(StartupStage::Gateway, &error);
        assert!(!details.contains("password"));
        assert!(!details.contains("secret-api-key"));
        assert!(!details.contains("example.test"));
    }

    #[tokio::test]
    async fn shutdown_waits_until_partial_initialization_cleanup_finishes() {
        let temp = tempfile::tempdir().expect("temporary diagnostics");
        let diagnostics =
            StartupDiagnostics::initialize(Some(temp.path().to_path_buf())).diagnostics;
        let controller = Arc::new(StartupController::new_with_previous_failure(
            diagnostics,
            false,
        ));
        let waiter = {
            let controller = Arc::clone(&controller);
            tokio::spawn(async move { controller.wait_for_initialization().await })
        };
        assert!(controller.begin_shutdown());
        assert!(!waiter.is_finished());
        controller.finish_initialization();
        waiter.await.expect("cleanup waiter");
    }
}
