use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageBackendKind {
    #[default]
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone)]
pub struct SqlStorageConfig {
    pub url: Option<String>,
    pub max_connections: u32,
    pub min_connections: u32,
    pub idle_timeout: Option<Duration>,
}

impl SqlStorageConfig {
    pub fn configured_url(&self) -> Option<String> {
        self.url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    }
}

impl Default for SqlStorageConfig {
    fn default() -> Self {
        Self {
            url: None,
            max_connections: 10,
            min_connections: 1,
            idle_timeout: Some(Duration::from_secs(300)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayStorageConfig {
    pub backend: StorageBackendKind,
    pub postgres: SqlStorageConfig,
}

impl Default for GatewayStorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackendKind::Sqlite,
            postgres: SqlStorageConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub data_dir: PathBuf,
    pub auth_key: Option<String>,
    /// Canonical externally reachable origin used for signed Artifact URLs.
    /// Request forwarding headers are never trusted for this value.
    pub public_origin: Option<String>,
    pub storage: GatewayStorageConfig,
    /// Whether this process has a trusted Desktop updater bridge.
    pub product_update_download_supported: bool,
    /// How often to poll the shared DB for a config epoch change and reload
    /// `model_cache` when a change is detected. Set to `Duration::ZERO` to
    /// disable (default for desktop / single-process deployments).
    pub config_poll_interval: Duration,
    #[cfg(debug_assertions)]
    /// Debug-only directory for redacted-header, full-body protocol wire captures.
    pub wire_capture_dir: Option<PathBuf>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            auth_key: None,
            public_origin: None,
            storage: GatewayStorageConfig::default(),
            product_update_download_supported: false,
            config_poll_interval: Duration::ZERO,
            #[cfg(debug_assertions)]
            wire_capture_dir: None,
        }
    }
}

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".stravia")
}
