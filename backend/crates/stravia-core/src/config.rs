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

#[derive(Clone)]
pub struct GatewayCacheConfig {
    /// 全部可丢弃派生值共享的逻辑字节预算，不是进程 RSS 上限。
    pub capacity_bytes: usize,
    /// PostgreSQL 必须配置 Redis；SQLite 不使用此连接。
    pub redis_url: Option<String>,
}

impl std::fmt::Debug for GatewayCacheConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayCacheConfig")
            .field("capacity_bytes", &self.capacity_bytes)
            .field("redis_configured", &self.redis_url.is_some())
            .finish()
    }
}

impl Default for GatewayCacheConfig {
    fn default() -> Self {
        Self {
            capacity_bytes: 16 * 1024 * 1024,
            redis_url: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub data_dir: PathBuf,
    pub storage: GatewayStorageConfig,
    pub cache: GatewayCacheConfig,
    /// Whether this process has a trusted Desktop updater bridge.
    pub product_update_download_supported: bool,
    /// How often to poll the shared DB for a config epoch change and reload
    /// `model_cache` when a change is detected. Set to `Duration::ZERO` to
    /// disable (default for desktop / single-process deployments).
    pub config_poll_interval: Duration,
    /// Provider Catalog origin the guest fetches through host HTTP. `None`
    /// disables remote catalog access entirely — a gateway that never
    /// configured an origin fails closed instead of reaching the production
    /// service. Tests point it at an in-process fixture so the real guest
    /// still fetches across the production call boundary.
    pub catalog_base_url: Option<String>,
    /// Fetch the remote Provider Catalog in the background. Disabled by
    /// default so tests never reach the network; production deployments opt
    /// in explicitly.
    pub catalog_background_refresh: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            storage: GatewayStorageConfig::default(),
            cache: GatewayCacheConfig::default(),
            product_update_download_supported: false,
            config_poll_interval: Duration::ZERO,
            catalog_base_url: None,
            catalog_background_refresh: false,
        }
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("~/.stravia")
}
