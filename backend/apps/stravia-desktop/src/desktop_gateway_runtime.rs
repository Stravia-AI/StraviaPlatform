use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use axum::Router;
use listeners::{Protocol, SocketState};
use parking_lot::{Mutex as SyncMutex, RwLock as SyncRwLock};
use serde::Serialize;
#[cfg(test)]
use std::{future::Future, pin::Pin};
use stravia_server::{DEFAULT_PORT, RunningHttpServer, ServerHandle, start_http_server};
use tauri::Manager;
use tauri_plugin_store::{Store, StoreExt};
use tokio::sync::{Mutex, RwLock};

const FIXED_PORT_KEY: &str = "fixed_port";
const EXTERNAL_ACCESS_KEY: &str = "external_access";
const SILENT_START_KEY: &str = "silent_start";
const BACKGROUND_ARG: &str = "--background";
const DEVELOPMENT_RUNTIME_DIR: &str = ".stravia-dev";
const E2E_RUNTIME_DIR: &str = ".stravia-desktop-e2e";
const MIN_FIXED_PORT: u16 = 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const REBIND_ATTEMPTS: u32 = 40;
const REBIND_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortPreferenceLoad {
    Missing,
    Invalid,
    Fixed(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DesktopPreferences {
    pub fixed_port: PortPreferenceLoad,
    pub external_access: bool,
    pub silent_start: bool,
}

impl Default for DesktopPreferences {
    fn default() -> Self {
        Self {
            fixed_port: PortPreferenceLoad::Missing,
            external_access: false,
            silent_start: false,
        }
    }
}

/// The binding scope of the embedded HTTP listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindScope {
    Loopback,
    External,
}

impl BindScope {
    fn of(external: bool) -> Self {
        if external {
            Self::External
        } else {
            Self::Loopback
        }
    }

    fn host(self) -> &'static str {
        match self {
            Self::Loopback => "127.0.0.1",
            Self::External => "0.0.0.0",
        }
    }
}

/// The autostart entry always carries this flag so a launch can be told apart
/// from a manual start; whether it actually hides the window is decided by the
/// `silent_start` preference at startup.
fn args_include_background(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    args.into_iter().any(|arg| arg == BACKGROUND_ARG)
}

pub(crate) fn launched_in_background() -> bool {
    args_include_background(std::env::args_os().skip(1))
}

/// Non-loopback IPv4 addresses other devices can dial. The default-route
/// source address comes first; the rest follow in stable sorted order.
pub(crate) fn lan_ipv4_addresses() -> Vec<String> {
    fn usable(ip: &std::net::Ipv4Addr) -> bool {
        !(ip.is_loopback()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast()
            || ip.is_multicast()
            || ip.is_documentation())
    }
    let primary = local_ip_address::local_ip().ok().and_then(|ip| match ip {
        std::net::IpAddr::V4(ip) if usable(&ip) => Some(ip),
        _ => None,
    });
    let mut addresses: Vec<std::net::Ipv4Addr> = local_ip_address::list_afinet_netifas()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .filter_map(|(_name, ip)| match ip {
                    std::net::IpAddr::V4(ip) if usable(&ip) => Some(ip),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    addresses.sort_unstable();
    addresses.dedup();
    if let Some(primary) = primary {
        addresses.retain(|address| *address != primary);
        addresses.insert(0, primary);
    }
    addresses
        .into_iter()
        .map(|address| address.to_string())
        .collect()
}

pub(crate) trait DesktopPreferenceStore: Send + Sync {
    fn load(&self) -> Result<DesktopPreferences, String>;
    fn save_fixed_port(&self, port: u16) -> Result<(), String>;
    fn save_external_access(&self, enabled: bool) -> Result<(), String>;
    fn save_silent_start(&self, enabled: bool) -> Result<(), String>;
}

struct TauriDesktopPreferenceStore {
    store: Arc<Store<tauri::Wry>>,
}

impl TauriDesktopPreferenceStore {
    fn flag(&self, key: &str) -> bool {
        self.store
            .get(key)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn set_and_save(&self, key: &str, value: serde_json::Value) -> Result<(), String> {
        let previous = self.store.get(key);
        self.store.set(key, value);
        if let Err(error) = self.store.save() {
            match previous {
                Some(value) => self.store.set(key, value),
                None => {
                    self.store.delete(key);
                }
            }
            return Err(error.to_string());
        }
        Ok(())
    }
}

impl DesktopPreferenceStore for TauriDesktopPreferenceStore {
    fn load(&self) -> Result<DesktopPreferences, String> {
        match self.store.reload() {
            Ok(()) => {}
            Err(tauri_plugin_store::Error::Io(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(DesktopPreferences::default());
            }
            Err(tauri_plugin_store::Error::Deserialize(_) | tauri_plugin_store::Error::Json(_)) => {
                return Ok(DesktopPreferences {
                    fixed_port: PortPreferenceLoad::Invalid,
                    ..Default::default()
                });
            }
            Err(error) => return Err(error.to_string()),
        }

        let fixed_port = match self.store.get(FIXED_PORT_KEY) {
            None => PortPreferenceLoad::Missing,
            Some(value) => match value.as_u64().and_then(|value| u16::try_from(value).ok()) {
                Some(port) if port >= MIN_FIXED_PORT => PortPreferenceLoad::Fixed(port),
                _ => PortPreferenceLoad::Invalid,
            },
        };
        Ok(DesktopPreferences {
            fixed_port,
            external_access: self.flag(EXTERNAL_ACCESS_KEY),
            silent_start: self.flag(SILENT_START_KEY),
        })
    }

    fn save_fixed_port(&self, port: u16) -> Result<(), String> {
        self.set_and_save(FIXED_PORT_KEY, serde_json::json!(port))
    }

    fn save_external_access(&self, enabled: bool) -> Result<(), String> {
        self.set_and_save(EXTERNAL_ACCESS_KEY, serde_json::json!(enabled))
    }

    fn save_silent_start(&self, enabled: bool) -> Result<(), String> {
        self.set_and_save(SILENT_START_KEY, serde_json::json!(enabled))
    }
}

pub(crate) fn desktop_root_override() -> anyhow::Result<Option<PathBuf>> {
    if cfg!(feature = "desktop-e2e")
        && let Some(root) = std::env::var_os("STRAVIA_DESKTOP_E2E_RUN_ROOT")
    {
        return e2e_data_dir(Path::new(&root)).map(Some);
    }
    select_root_override(
        cfg!(feature = "desktop-e2e"),
        std::env::args_os().skip(1),
        std::env::var_os("STRAVIA_DATA_DIR"),
    )
}

fn e2e_data_dir(root: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(root.is_absolute(), "E2E run root must be absolute");
    anyhow::ensure!(
        root.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("stravia-desktop-e2e-")),
        "E2E run root must have the stravia-desktop-e2e- prefix",
    );
    let temporary = std::env::temp_dir().canonicalize()?;
    let parent = root
        .parent()
        .context("E2E run root must have a parent")?
        .canonicalize()?;
    anyhow::ensure!(
        parent == temporary,
        "E2E run root must be directly inside the system temporary directory"
    );
    let metadata = std::fs::symlink_metadata(root).context("E2E run root must already exist")?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "E2E run root must be a real directory"
    );
    let canonical = root.canonicalize()?;
    anyhow::ensure!(
        canonical.parent() == Some(temporary.as_path()),
        "E2E run root must not redirect outside the temporary directory"
    );
    let data = canonical.join("data");
    match std::fs::symlink_metadata(&data) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "E2E data root must be a real directory"
            );
            anyhow::ensure!(
                data.canonicalize()?.parent() == Some(canonical.as_path()),
                "E2E data root must not redirect outside the run directory"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(data)
}

fn select_root_override(
    desktop_e2e: bool,
    args: impl IntoIterator<Item = std::ffi::OsString>,
    environment: Option<std::ffi::OsString>,
) -> anyhow::Result<Option<PathBuf>> {
    // Fixture runs must never inherit a user-selected production root.
    if desktop_e2e {
        return stravia_core::data_paths::resolve_data_dir(
            &repository_root().join(E2E_RUNTIME_DIR),
        )
        .map(Some);
    }
    let mut selected = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--data-dir" {
            Some(args.next().context("--data-dir requires a path")?)
        } else {
            arg.to_str()
                .and_then(|arg| arg.strip_prefix("--data-dir="))
                .map(std::ffi::OsString::from)
        };
        if let Some(value) = value {
            anyhow::ensure!(selected.is_none(), "--data-dir may only be specified once");
            anyhow::ensure!(!value.is_empty(), "--data-dir requires a non-empty path");
            selected = Some(value);
        }
    }
    selected
        .or(environment)
        .map(|path| {
            anyhow::ensure!(
                !path.is_empty(),
                "STRAVIA_DATA_DIR requires a non-empty path"
            );
            stravia_core::data_paths::resolve_data_dir(Path::new(&path))
        })
        .transpose()
}

pub(crate) fn autostart_root_argument(root: &Path) -> anyhow::Result<String> {
    let root = root
        .to_str()
        .context("desktop data directory must be valid Unicode for autostart")?;
    #[cfg(target_os = "windows")]
    {
        // auto-launch joins arguments verbatim into a Windows command line.
        let mut quoted = String::from("\"");
        let mut slashes = 0;
        for character in root.chars() {
            if character == '\\' {
                slashes += 1;
                continue;
            }
            quoted.extend(std::iter::repeat_n(
                '\\',
                if character == '"' {
                    slashes * 2 + 1
                } else {
                    slashes
                },
            ));
            quoted.push(character);
            slashes = 0;
        }
        quoted.extend(std::iter::repeat_n('\\', slashes * 2));
        quoted.push('"');
        Ok(quoted)
    }
    #[cfg(target_os = "linux")]
    {
        // Desktop Entry Exec has both string escaping and quoted argument escaping.
        anyhow::ensure!(
            !root.contains(['\n', '\r']),
            "autostart data directory cannot contain line breaks"
        );
        let escaped = root
            .replace('\\', "\\\\\\\\")
            .replace('"', "\\\\\"")
            .replace('`', "\\\\`")
            .replace('$', "\\\\$")
            .replace('%', "%%");
        Ok(format!("\"{escaped}\""))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        // LaunchAgent stores each argument as a separate plist string.
        Ok(root.to_owned())
    }
}

pub(crate) fn desktop_runtime_dir(
    app: &tauri::App,
    root_override: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let root = if let Some(root) = root_override {
        root.to_path_buf()
    } else if cfg!(debug_assertions) {
        repository_root().join(DEVELOPMENT_RUNTIME_DIR)
    } else {
        app.path()
            .app_data_dir()
            .context("failed to resolve desktop app data directory")?
    };
    stravia_core::data_paths::resolve_data_dir(&root)
}

pub(crate) fn desktop_preference_store(
    app: &tauri::App,
    runtime_dir: &Path,
) -> anyhow::Result<Arc<dyn DesktopPreferenceStore>> {
    let path = stravia_core::data_paths::DataPaths::new(runtime_dir).desktop_port();
    std::fs::create_dir_all(path.parent().context("desktop port store has no parent")?)?;
    let store = app
        .store_builder(path)
        .disable_auto_save()
        .build()
        .context("failed to open desktop port store")?;
    Ok(Arc::new(TauriDesktopPreferenceStore { store }))
}

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("desktop crate must live under backend/apps")
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortOwner {
    pub name: String,
    pub pid: u32,
}

pub(crate) trait PortOwnerResolver: Send + Sync {
    fn resolve(&self, scope: BindScope, port: u16) -> Result<Vec<PortOwner>, String>;
}

pub(crate) struct SystemPortOwnerResolver;

impl PortOwnerResolver for SystemPortOwnerResolver {
    fn resolve(&self, scope: BindScope, port: u16) -> Result<Vec<PortOwner>, String> {
        let listeners = listeners::get_all().map_err(|error| error.to_string())?;
        let current_pid = std::process::id();
        Ok(listeners
            .into_iter()
            .filter(|listener| {
                listener.protocol == Protocol::TCP
                    && listener.state == SocketState::Listen
                    && listener.socket.port() == port
                    && listener.process.pid != current_pid
                    && matches!(listener.socket.ip(), std::net::IpAddr::V4(ip) if ip.is_loopback() || ip.is_unspecified() || scope == BindScope::External)
            })
            .filter_map(|listener| known_port_owner(listener.process.name, listener.process.pid))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }
}

fn known_port_owner(name: String, pid: u32) -> Option<PortOwner> {
    if name.trim().is_empty() {
        None
    } else {
        Some(PortOwner { name, pid })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DesktopPortMode {
    Fixed,
    Fallback,
    ConfigError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BindingFailureKind {
    AddrInUse,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BindingFailure {
    pub kind: BindingFailureKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OwnerLookupStatus {
    NotApplicable,
    Identifying,
    Found,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopPortState {
    pub current_port: u16,
    pub fixed_port: Option<u16>,
    pub mode: DesktopPortMode,
    /// Whether the listener is bound to all interfaces (0.0.0.0) instead of
    /// loopback only. Reflects the actual socket, not just the saved flag.
    pub external_access: bool,
    pub binding_failure: Option<BindingFailure>,
    pub owner_lookup: OwnerLookupStatus,
    pub owners: Vec<PortOwner>,
    pub config_error: Option<String>,
    pub candidate_port: Option<u16>,
    pub candidate_error: Option<PortOperationError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PortOperationErrorCode {
    InvalidPort,
    BindFailed,
    StoreWriteFailed,
    NoFixedPort,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortOperationError {
    pub code: PortOperationErrorCode,
    pub message: String,
    pub binding_failure: Option<BindingFailure>,
    pub owner_lookup: OwnerLookupStatus,
    pub owners: Vec<PortOwner>,
}

impl PortOperationError {
    fn invalid_port() -> Self {
        Self {
            code: PortOperationErrorCode::InvalidPort,
            message: format!("Port must be a whole number between {MIN_FIXED_PORT} and 65535."),
            binding_failure: None,
            owner_lookup: OwnerLookupStatus::NotApplicable,
            owners: vec![],
        }
    }

    fn store_write(message: String) -> Self {
        Self {
            code: PortOperationErrorCode::StoreWriteFailed,
            message,
            binding_failure: None,
            owner_lookup: OwnerLookupStatus::NotApplicable,
            owners: vec![],
        }
    }

    fn no_fixed_port() -> Self {
        Self {
            code: PortOperationErrorCode::NoFixedPort,
            message: "No fixed desktop port is configured.".to_string(),
            binding_failure: None,
            owner_lookup: OwnerLookupStatus::NotApplicable,
            owners: vec![],
        }
    }
}

pub(crate) trait PortSwitchPublisher: Send + Sync {
    fn publish(&self, port: u16) -> Result<(), String>;
}

#[derive(Clone, Copy)]
enum OwnerLookupTarget {
    Fallback,
    Candidate,
}

#[derive(Default)]
struct HttpServerBinder {
    #[cfg(test)]
    bind_override: Option<Arc<TestBind>>,
}

#[cfg(test)]
type TestBind = dyn Fn(
        BindScope,
        u16,
        Router,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RunningHttpServer>> + Send>>
    + Send
    + Sync;

impl HttpServerBinder {
    async fn bind(
        &self,
        scope: BindScope,
        port: u16,
        app: Router,
    ) -> anyhow::Result<RunningHttpServer> {
        #[cfg(test)]
        if let Some(bind_override) = &self.bind_override {
            return bind_override(scope, port, app).await;
        }
        start_http_server((scope.host(), port), app).await
    }
}

struct RuntimeInner {
    server: Option<RunningHttpServer>,
    state: DesktopPortState,
    generation: u64,
}

pub(crate) struct DesktopGatewayRuntime {
    app: Router,
    store: Arc<dyn DesktopPreferenceStore>,
    owners: Arc<dyn PortOwnerResolver>,
    operation: Mutex<()>,
    inner: RwLock<RuntimeInner>,
    current_handle: SyncRwLock<ServerHandle>,
    draining_handles: SyncMutex<Vec<ServerHandle>>,
    binder: HttpServerBinder,
    switch_publisher: SyncRwLock<Option<Arc<dyn PortSwitchPublisher>>>,
}
impl DesktopGatewayRuntime {
    pub(crate) async fn start(
        app: Router,
        store: Arc<dyn DesktopPreferenceStore>,
        owners: Arc<dyn PortOwnerResolver>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::start_inner(app, store, owners, HttpServerBinder::default()).await
    }

    #[cfg(test)]
    async fn start_with_bind_override(
        app: Router,
        store: Arc<dyn DesktopPreferenceStore>,
        owners: Arc<dyn PortOwnerResolver>,
        bind_override: Arc<TestBind>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::start_inner(
            app,
            store,
            owners,
            HttpServerBinder {
                bind_override: Some(bind_override),
            },
        )
        .await
    }

    async fn start_inner(
        app: Router,
        store: Arc<dyn DesktopPreferenceStore>,
        owners: Arc<dyn PortOwnerResolver>,
        binder: HttpServerBinder,
    ) -> anyhow::Result<Arc<Self>> {
        let preference = store.load();
        // 读不出偏好时按最保守的回环绑定，绝不把服务意外暴露到局域网。
        let scope = BindScope::of(
            preference
                .as_ref()
                .map(|prefs| prefs.external_access)
                .unwrap_or(false),
        );
        let external_access = scope == BindScope::External;
        let mut lookup_port = None;
        let (server, state, generation) = match preference.map(|prefs| prefs.fixed_port) {
            Ok(PortPreferenceLoad::Fixed(port)) => {
                match binder.bind(scope, port, app.clone()).await {
                    Ok(server) => {
                        let current_port = server.local_addr().port();
                        (server, fixed_state(current_port, port, external_access), 0)
                    }
                    Err(error) => {
                        let failure = binding_failure(&error);
                        tracing::warn!(port, %error, "fixed desktop port unavailable; using a random port");
                        let server = binder.bind(scope, 0, app.clone()).await?;
                        let current_port = server.local_addr().port();
                        if failure.kind == BindingFailureKind::AddrInUse {
                            lookup_port = Some(port);
                        }
                        (
                            server,
                            fallback_state(current_port, port, failure, external_access),
                            1,
                        )
                    }
                }
            }
            Ok(PortPreferenceLoad::Missing | PortPreferenceLoad::Invalid) => {
                match binder.bind(scope, DEFAULT_PORT, app.clone()).await {
                    Ok(server) => {
                        let current_port = server.local_addr().port();
                        (
                            server,
                            fixed_state(current_port, DEFAULT_PORT, external_access),
                            0,
                        )
                    }
                    Err(error) => {
                        let failure = binding_failure(&error);
                        tracing::warn!(
                            port = DEFAULT_PORT,
                            %error,
                            "default desktop port unavailable; using a random port"
                        );
                        let server = binder.bind(scope, 0, app.clone()).await?;
                        let current_port = server.local_addr().port();
                        if failure.kind == BindingFailureKind::AddrInUse {
                            lookup_port = Some(DEFAULT_PORT);
                        }
                        (
                            server,
                            fallback_state(current_port, DEFAULT_PORT, failure, external_access),
                            1,
                        )
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "failed to read desktop port preference");
                let server = binder.bind(BindScope::Loopback, 0, app.clone()).await?;
                let current_port = server.local_addr().port();
                (server, config_error_state(current_port, error), 0)
            }
        };
        let current_handle = server.handle();
        let runtime = Arc::new(Self {
            app,
            store,
            owners,
            operation: Mutex::new(()),
            inner: RwLock::new(RuntimeInner {
                server: Some(server),
                state,
                generation,
            }),
            current_handle: SyncRwLock::new(current_handle),
            draining_handles: SyncMutex::new(vec![]),
            binder,
            switch_publisher: SyncRwLock::new(None),
        });
        if let Some(port) = lookup_port {
            runtime.spawn_owner_lookup(scope, port, generation, OwnerLookupTarget::Fallback);
        }
        Ok(runtime)
    }

    pub(crate) async fn snapshot(&self) -> DesktopPortState {
        self.inner.read().await.state.clone()
    }

    pub(crate) fn current_port(&self) -> u16 {
        self.current_local_addr().port()
    }

    pub(crate) fn current_local_addr(&self) -> std::net::SocketAddr {
        self.current_handle.read().local_addr()
    }

    pub(crate) fn set_switch_publisher(&self, publisher: Arc<dyn PortSwitchPublisher>) {
        *self.switch_publisher.write() = Some(publisher);
    }

    pub(crate) async fn configure_fixed_port(
        self: &Arc<Self>,
        port: u32,
    ) -> Result<DesktopPortState, PortOperationError> {
        let Ok(port) = u16::try_from(port) else {
            return Err(PortOperationError::invalid_port());
        };
        if port < MIN_FIXED_PORT {
            return Err(PortOperationError::invalid_port());
        }
        let operation = self.operation.lock().await;
        let (current_port, external_access) = {
            let inner = self.inner.read().await;
            (inner.state.current_port, inner.state.external_access)
        };
        if port == current_port {
            self.store
                .save_fixed_port(port)
                .map_err(PortOperationError::store_write)?;
            let mut inner = self.inner.write().await;
            inner.generation = inner.generation.wrapping_add(1);
            inner.state = fixed_state(current_port, port, external_access);
            return Ok(inner.state.clone());
        }
        let scope = BindScope::of(external_access);
        let candidate = match self.binder.bind(scope, port, self.app.clone()).await {
            Ok(server) => server,
            Err(error) => {
                let operation_error = self.candidate_bind_error(port, error).await;
                drop(operation);
                return Err(operation_error);
            }
        };
        if let Err(error) = self.store.save_fixed_port(port) {
            if let Err(shutdown_error) = candidate.shutdown().await {
                tracing::warn!(%shutdown_error, "failed to stop rejected desktop port candidate");
            }
            return Err(PortOperationError::store_write(error));
        }

        Ok(self.install_candidate(candidate, port).await)
    }

    /// Switch the listener between loopback-only and all interfaces on the
    /// same port. The old socket must be released before the new bind succeeds,
    /// so unlike a port change there is a short window without a listener.
    pub(crate) async fn set_external_access(
        self: &Arc<Self>,
        enabled: bool,
    ) -> Result<DesktopPortState, PortOperationError> {
        let _operation = self.operation.lock().await;
        let (port, previous) = {
            let inner = self.inner.read().await;
            (inner.state.current_port, inner.state.external_access)
        };
        if enabled == previous {
            self.store
                .save_external_access(enabled)
                .map_err(PortOperationError::store_write)?;
            return Ok(self.inner.read().await.state.clone());
        }
        self.store
            .save_external_access(enabled)
            .map_err(PortOperationError::store_write)?;

        let target = BindScope::of(enabled);
        let revert = BindScope::of(previous);
        let old_server = self.inner.write().await.server.take();
        let Some(old_server) = old_server else {
            let _ = self.store.save_external_access(previous);
            return Err(PortOperationError {
                code: PortOperationErrorCode::BindFailed,
                message: "no running listener to rebind".to_string(),
                binding_failure: None,
                owner_lookup: OwnerLookupStatus::NotApplicable,
                owners: vec![],
            });
        };
        old_server.handle().shutdown();

        match self.bind_after_release(target, port).await {
            Ok(candidate) => {
                {
                    let mut inner = self.inner.write().await;
                    inner.state.external_access = enabled;
                }
                let state = self.install_rebound(candidate).await;
                self.drain_server(old_server);
                Ok(state)
            }
            Err(error) => {
                let failure = binding_failure(&error);
                let _ = self.store.save_external_access(previous);
                let restored = match self.bind_after_release(revert, port).await {
                    Ok(server) => server,
                    Err(rollback_error) => {
                        tracing::error!(
                            %rollback_error,
                            "failed to restore the previous desktop listener; using a random port"
                        );
                        self.bind_after_release(revert, 0).await.map_err(|error| {
                            PortOperationError {
                                code: PortOperationErrorCode::BindFailed,
                                message: error.to_string(),
                                binding_failure: Some(binding_failure(&error)),
                                owner_lookup: OwnerLookupStatus::NotApplicable,
                                owners: vec![],
                            }
                        })?
                    }
                };
                let restored_port = restored.local_addr().port();
                {
                    let mut inner = self.inner.write().await;
                    if restored_port != port {
                        inner.state = fallback_state(
                            restored_port,
                            inner.state.fixed_port.unwrap_or(port),
                            binding_failure(&anyhow::anyhow!(
                                "listener moved after a failed scope switch"
                            )),
                            previous,
                        );
                    }
                    inner.state.external_access = previous;
                }
                let _state = self.install_rebound(restored).await;
                self.drain_server(old_server);
                Err(PortOperationError {
                    code: PortOperationErrorCode::BindFailed,
                    message: failure.message.clone(),
                    binding_failure: Some(failure),
                    owner_lookup: OwnerLookupStatus::NotApplicable,
                    owners: vec![],
                })
            }
        }
    }

    /// The just-shutdown listener frees its socket asynchronously; retry the
    /// bind briefly until the port is released instead of failing spuriously.
    async fn bind_after_release(
        &self,
        scope: BindScope,
        port: u16,
    ) -> anyhow::Result<RunningHttpServer> {
        let mut last_error = None;
        for _ in 0..REBIND_ATTEMPTS {
            match self.binder.bind(scope, port, self.app.clone()).await {
                Err(error) if binding_failure(&error).kind == BindingFailureKind::AddrInUse => {
                    last_error = Some(error);
                    tokio::time::sleep(REBIND_INTERVAL).await;
                }
                result => return result,
            }
        }
        Err(last_error.expect("at least one bind attempt ran"))
    }

    /// Swap in a rebound listener without publishing: the port did not change,
    /// so the tray tooltip and the WebView origin stay valid.
    async fn install_rebound(&self, candidate: RunningHttpServer) -> DesktopPortState {
        let candidate_handle = candidate.handle();
        *self.current_handle.write() = candidate_handle;
        let mut inner = self.inner.write().await;
        let _ = inner.server.replace(candidate);
        inner.generation = inner.generation.wrapping_add(1);
        inner.state.clone()
    }

    pub(crate) async fn recheck_fixed_port(
        self: &Arc<Self>,
    ) -> Result<DesktopPortState, PortOperationError> {
        let operation = self.operation.lock().await;
        let (port, current_port, external_access) = {
            let inner = self.inner.read().await;
            let Some(port) = inner.state.fixed_port else {
                return Err(PortOperationError::no_fixed_port());
            };
            (port, inner.state.current_port, inner.state.external_access)
        };
        let scope = BindScope::of(external_access);
        if port == current_port {
            let mut inner = self.inner.write().await;
            inner.state = fixed_state(current_port, port, external_access);
            return Ok(inner.state.clone());
        }

        match self.binder.bind(scope, port, self.app.clone()).await {
            Ok(candidate) => Ok(self.install_candidate(candidate, port).await),
            Err(error) => {
                let failure = binding_failure(&error);
                let lookup = failure.kind == BindingFailureKind::AddrInUse;
                let (state, generation) = {
                    let mut inner = self.inner.write().await;
                    inner.generation = inner.generation.wrapping_add(1);
                    inner.state = fallback_state(current_port, port, failure, external_access);
                    (inner.state.clone(), inner.generation)
                };
                drop(operation);
                if lookup {
                    self.spawn_owner_lookup(scope, port, generation, OwnerLookupTarget::Fallback);
                }
                Ok(state)
            }
        }
    }

    pub(crate) fn request_shutdown(&self) {
        self.current_handle.read().shutdown();
        let handles = self.draining_handles.lock();
        for handle in handles.iter() {
            handle.shutdown();
        }
    }

    #[cfg(test)]
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        let _operation = self.operation.lock().await;
        self.request_shutdown();
        let server = self.inner.write().await.server.take();
        if let Some(server) = server {
            server.shutdown_with_timeout(DRAIN_TIMEOUT).await?;
        }
        Ok(())
    }

    async fn candidate_bind_error(
        self: &Arc<Self>,
        port: u16,
        error: anyhow::Error,
    ) -> PortOperationError {
        let failure = binding_failure(&error);
        let should_lookup = failure.kind == BindingFailureKind::AddrInUse;
        let operation_error = PortOperationError {
            code: PortOperationErrorCode::BindFailed,
            message: failure.message.clone(),
            binding_failure: Some(failure),
            owner_lookup: if should_lookup {
                OwnerLookupStatus::Identifying
            } else {
                OwnerLookupStatus::NotApplicable
            },
            owners: vec![],
        };
        let generation = {
            let mut inner = self.inner.write().await;
            inner.generation = inner.generation.wrapping_add(1);
            inner.state.candidate_port = Some(port);
            inner.state.candidate_error = Some(operation_error.clone());
            inner.generation
        };
        if should_lookup {
            let scope = {
                let inner = self.inner.read().await;
                BindScope::of(inner.state.external_access)
            };
            self.spawn_owner_lookup(scope, port, generation, OwnerLookupTarget::Candidate);
        }
        operation_error
    }

    fn spawn_owner_lookup(
        self: &Arc<Self>,
        scope: BindScope,
        port: u16,
        generation: u64,
        target: OwnerLookupTarget,
    ) {
        let runtime = Arc::downgrade(self);
        let resolver = self.owners.clone();
        tauri::async_runtime::spawn(async move {
            let owners =
                match tokio::task::spawn_blocking(move || resolver.resolve(scope, port)).await {
                    Ok(Ok(owners)) => owners,
                    Ok(Err(error)) => {
                        tracing::warn!(port, %error, "failed to identify desktop port owner");
                        vec![]
                    }
                    Err(error) => {
                        tracing::warn!(port, %error, "desktop port owner lookup task failed");
                        vec![]
                    }
                };
            let Some(runtime) = runtime.upgrade() else {
                return;
            };
            let mut inner = runtime.inner.write().await;
            if inner.generation != generation {
                return;
            }
            let status = if owners.is_empty() {
                OwnerLookupStatus::Unknown
            } else {
                OwnerLookupStatus::Found
            };
            match target {
                OwnerLookupTarget::Fallback
                    if inner.state.mode == DesktopPortMode::Fallback
                        && inner.state.fixed_port == Some(port) =>
                {
                    inner.state.owner_lookup = status;
                    inner.state.owners = owners;
                }
                OwnerLookupTarget::Candidate if inner.state.candidate_port == Some(port) => {
                    if let Some(error) = inner.state.candidate_error.as_mut() {
                        error.owner_lookup = status;
                        error.owners = owners;
                    }
                }
                _ => {}
            }
        });
    }

    async fn install_candidate(
        self: &Arc<Self>,
        candidate: RunningHttpServer,
        port: u16,
    ) -> DesktopPortState {
        let current_port = candidate.local_addr().port();
        let candidate_handle = candidate.handle();
        *self.current_handle.write() = candidate_handle;
        let (old_server, state) = {
            let mut inner = self.inner.write().await;
            let old_server = inner.server.replace(candidate);
            inner.generation = inner.generation.wrapping_add(1);
            inner.state = fixed_state(current_port, port, inner.state.external_access);
            (old_server, inner.state.clone())
        };
        if let Some(old_server) = old_server {
            self.drain_server(old_server);
        }
        let publisher = self.switch_publisher.read().clone();
        if let Some(publisher) = publisher
            && let Err(error) = publisher.publish(current_port)
        {
            tracing::warn!(%error, "failed to publish desktop port switch");
        }
        state
    }

    fn drain_server(&self, server: RunningHttpServer) {
        let handle = server.handle();
        handle.shutdown();
        self.draining_handles.lock().push(handle);
        tauri::async_runtime::spawn(async move {
            if let Err(error) = server.shutdown_with_timeout(DRAIN_TIMEOUT).await {
                tracing::warn!(%error, "desktop listener failed while draining");
            }
        });
    }
}

fn binding_failure(error: &anyhow::Error) -> BindingFailure {
    let kind = error
        .downcast_ref::<io::Error>()
        .map(io::Error::kind)
        .unwrap_or(io::ErrorKind::Other);
    BindingFailure {
        kind: if kind == io::ErrorKind::AddrInUse {
            BindingFailureKind::AddrInUse
        } else {
            BindingFailureKind::Other
        },
        message: error.to_string(),
    }
}

fn fixed_state(current_port: u16, fixed_port: u16, external_access: bool) -> DesktopPortState {
    DesktopPortState {
        current_port,
        fixed_port: Some(fixed_port),
        mode: DesktopPortMode::Fixed,
        external_access,
        binding_failure: None,
        owner_lookup: OwnerLookupStatus::NotApplicable,
        owners: vec![],
        config_error: None,
        candidate_port: None,
        candidate_error: None,
    }
}

fn fallback_state(
    current_port: u16,
    fixed_port: u16,
    failure: BindingFailure,
    external_access: bool,
) -> DesktopPortState {
    let owner_lookup = if failure.kind == BindingFailureKind::AddrInUse {
        OwnerLookupStatus::Identifying
    } else {
        OwnerLookupStatus::NotApplicable
    };
    DesktopPortState {
        current_port,
        fixed_port: Some(fixed_port),
        mode: DesktopPortMode::Fallback,
        external_access,
        binding_failure: Some(failure),
        owner_lookup,
        owners: vec![],
        config_error: None,
        candidate_port: None,
        candidate_error: None,
    }
}

fn config_error_state(current_port: u16, error: String) -> DesktopPortState {
    DesktopPortState {
        current_port,
        fixed_port: None,
        mode: DesktopPortMode::ConfigError,
        external_access: false,
        binding_failure: None,
        owner_lookup: OwnerLookupStatus::NotApplicable,
        owners: vec![],
        config_error: Some(error),
        candidate_port: None,
        candidate_error: None,
    }
}

#[cfg(test)]
mod tests;
