use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use stravia_core::config::GatewayConfig;
use stravia_core::data_paths::{DataPaths, resolve_data_dir};
use stravia_server::{
    AdminEntryPolicy, DEFAULT_PORT, ServerStartupConfig, StartupHttpApp, recover_admin,
    standalone_local_origins, start_http_server,
};

// 请求与后台写入跨线程交接大型历史树；及时复用释放页，避免逐线程 arena 驻留。
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

// mimalloc v2/v3 的 `mi_option_purge_delay` 枚举值均为 15（v3 默认 1000ms）。
// 空闲 worker 不再触发惰性回收时会长期滞留已释放页；在多线程启动前将默认延迟设为 0，
// 同时保留用户通过 `MIMALLOC_PURGE_DELAY` 显式覆盖的能力。
const MI_OPTION_PURGE_DELAY: std::ffi::c_int = 15;
const DEFAULT_MAX_WORKER_THREADS: usize = 8;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

unsafe extern "C" {
    fn mi_option_set_default(option: std::ffi::c_int, value: std::ffi::c_long);
    #[cfg(test)]
    fn mi_option_get(option: std::ffi::c_int) -> std::ffi::c_long;
}

fn configure_allocator() {
    // SAFETY: 在 `main` 单线程入口、尚未启动 Tokio 线程前调用，且 `libmimalloc-sys` 已随
    // `MiMalloc` 静态链接；`mi_option_set_default` 仅在未设置环境变量时更新默认值。
    unsafe {
        mi_option_set_default(MI_OPTION_PURGE_DELAY, 0);
    }
}

fn resolve_worker_threads(env_override: Option<&str>, available_parallelism: usize) -> usize {
    if let Some(threads) = env_override
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&count| count > 0)
    {
        return threads;
    }
    available_parallelism.clamp(1, DEFAULT_MAX_WORKER_THREADS)
}

fn build_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let workers = resolve_worker_threads(
        std::env::var("TOKIO_WORKER_THREADS").ok().as_deref(),
        available,
    );
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?)
}

#[derive(Parser)]
#[command(name = "stravia-server", version, about = "Stravia Agent infra")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(
        long,
        default_value = "127.0.0.1",
        env = "STRAVIA_HOST",
        help_heading = "Server",
        global = true
    )]
    host: String,

    #[arg(
        long,
        default_value_t = DEFAULT_PORT,
        env = "STRAVIA_PORT",
        help_heading = "Server",
        global = true
    )]
    port: u16,

    #[arg(
        long = "admin-origin",
        env = "STRAVIA_ADMIN_ORIGINS",
        action = clap::ArgAction::Append,
        value_delimiter = ',',
        help = "Allowed management HTTP(S) origin (repeatable; omitted means unrestricted)",
        help_heading = "Server",
        global = true
    )]
    admin_origins: Vec<String>,

    #[arg(
        long = "trusted-proxy",
        env = "STRAVIA_TRUSTED_PROXIES",
        action = clap::ArgAction::Append,
        value_delimiter = ',',
        help = "Trusted immediate TCP proxy IP or CIDR (repeatable; none trusted by default)",
        help_heading = "Server",
        global = true
    )]
    trusted_proxies: Vec<String>,

    #[arg(
        long,
        default_value = "info",
        env = "STRAVIA_LOG_LEVEL",
        value_parser = ["error", "warn", "info", "debug", "trace"],
        help_heading = "Server",
        global = true
    )]
    log_level: String,

    #[arg(
        long = "proxy-cors-origin",
        action = clap::ArgAction::Append,
        help = "Allowed CORS origin for proxy API (repeatable, use '*' for any)",
        help_heading = "Advanced",
        global = true
    )]
    proxy_cors_origins: Vec<String>,

    #[arg(
        long,
        default_value_t = default_data_dir(),
        env = "STRAVIA_DATA_DIR",
        help = "Root for configuration, SQLite database, artifacts, diagnostics, logs, cache and host state",
        help_heading = "Storage",
        global = true
    )]
    data_dir: String,

    #[arg(
        long,
        help = "Server configuration file (defaults to <data-dir>/server.toml)",
        help_heading = "Storage",
        global = true
    )]
    config: Option<String>,

    #[arg(
        long = "updater-url",
        env = "STRAVIA_UPDATER_URL",
        help = "Public HTTPS update distribution URL (defaults to the built-in production service)",
        help_heading = "Server",
        global = true
    )]
    updater_url: Option<String>,

    #[arg(
        long,
        default_value_t = 3,
        env = "STRAVIA_CONFIG_POLL_INTERVAL",
        help = "Seconds between config epoch polls (0 = disabled); does not coordinate multiple replicas",
        help_heading = "Advanced",
        global = true
    )]
    config_poll_interval: u64,

    // 性能与 HTTP 测试必须走真实目录装配，但不能访问生产目录。
    #[cfg(feature = "test-harness")]
    #[arg(long, hide = true)]
    test_catalog_base_url: Option<String>,

    #[cfg(feature = "test-harness")]
    #[arg(long, hide = true)]
    test_plugin_marketplace_url: Option<String>,

    #[cfg(feature = "test-harness")]
    #[arg(long, env = "STRAVIA_TEST_REDIS_URL", hide = true)]
    test_redis_url: Option<String>,

    #[cfg(feature = "test-harness")]
    #[arg(long, hide = true, value_parser = test_loopback_address)]
    test_handoff_barrier: Option<std::net::SocketAddr>,
}

#[cfg(feature = "test-harness")]
fn test_loopback_address(value: &str) -> Result<std::net::SocketAddr, String> {
    let address: std::net::SocketAddr = value.parse().map_err(|error| format!("{error}"))?;
    if !address.ip().is_loopback() {
        return Err("test handoff barrier must use a loopback address".into());
    }
    Ok(address)
}

#[derive(Subcommand)]
enum Command {
    /// Interactively replace the sole administrator credentials and revoke all sessions.
    RecoverAdmin,
}

fn main() -> anyhow::Result<()> {
    configure_allocator();
    load_dotenv()?;
    let args = Args::parse();
    let runtime = build_runtime()?;
    runtime.block_on(async_main(args))
}

async fn async_main(args: Args) -> anyhow::Result<()> {
    let data_dir = resolve_data_dir(Path::new(&args.data_dir))?;
    let paths = DataPaths::new(&data_dir);
    paths.prepare()?;
    let _instance_lock = paths.lock()?;
    let filter = format!("stravia={level},tower_http={level}", level = args.log_level);
    // stdout 之外同步落到 <data-dir>/logs/，guard 持有后台写线程直至进程结束。
    let _log_guard =
        stravia_core::logging::init_runtime_logging(&filter, Some(paths.logs()), "stravia-server");
    let config_path = args
        .config
        .as_deref()
        .map(expanded_path)
        .unwrap_or_else(|| paths.server_config());
    let gateway = base_gateway_config(&args, data_dir);

    match args.command {
        Some(Command::RecoverAdmin) => recover_admin(&config_path, gateway).await,
        None => run_server(&args, config_path, gateway).await,
    }
}

async fn run_server(
    args: &Args,
    config_path: PathBuf,
    gateway: GatewayConfig,
) -> anyhow::Result<()> {
    if std::env::var_os("STRAVIA_PUBLIC_ORIGIN").is_some() {
        anyhow::bail!(
            "STRAVIA_PUBLIC_ORIGIN was removed; migrate the entry restriction to STRAVIA_ADMIN_ORIGINS"
        );
    }
    let admin_entry = AdminEntryPolicy::new(&args.admin_origins, &args.trusted_proxies)?;
    if admin_entry.unrestricted() {
        tracing::warn!(
            "Management entry origins are unrestricted; configure --admin-origin and network isolation to restrict access"
        );
    }
    if admin_entry.allows_http() {
        tracing::warn!(
            "Management policy permits HTTP; plaintext entry exposes credentials, sessions and management operations. CSRF and SameSite do not replace TLS"
        );
    }
    let local_origins = standalone_local_origins(args.port);
    let proxy_cors_origins = if args.proxy_cors_origins.is_empty() {
        local_origins
    } else {
        args.proxy_cors_origins.clone()
    };

    let mut startup = StartupHttpApp::new(ServerStartupConfig {
        config_path,
        gateway,
        admin_entry,
        proxy_cors_origins,
        serve_embedded_webui: true,
        extensions: Default::default(),
    });
    let server =
        start_http_server(listener_address(&args.host, args.port), startup.router()).await?;
    let address = server.local_addr();
    tracing::info!(%address, "Stravia startup listener opened");
    let result = async {
        let setup_token = startup.prepare().await?;
        if let Some(token) = setup_token.as_deref() {
            println!("Stravia setup token: {token}");
            std::io::stdout().flush()?;
        }

        tracing::info!(%address, "Stravia Server listening");
        shutdown_signal().await;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    // 后台同步与长连接监听同一关闭信号；先停接新连接并取消后台与订阅，再限时排空，
    // 确保即便存在未结束的长流请求也能进入 Gateway 持久化清理。
    server.handle().shutdown();
    startup.request_shutdown().await;
    let stopped = if result.is_err() {
        server.shutdown_with_timeout(Duration::ZERO).await
    } else {
        server.shutdown_with_timeout(SHUTDOWN_DRAIN_TIMEOUT).await
    };
    startup.shutdown().await;
    match result {
        Ok(()) => stopped,
        Err(error) => {
            if let Err(shutdown_error) = stopped {
                tracing::warn!(%shutdown_error, "failed to stop the startup listener");
            }
            Err(error)
        }
    }
}

fn base_gateway_config(args: &Args, data_dir: PathBuf) -> GatewayConfig {
    #[cfg(feature = "test-harness")]
    let catalog_base_url = args.test_catalog_base_url.clone();
    #[cfg(not(feature = "test-harness"))]
    let catalog_base_url = Some(stravia_core::provider_catalog::CATALOG_BASE_URL.to_owned());
    #[cfg(feature = "test-harness")]
    let cache = stravia_core::config::GatewayCacheConfig {
        redis_url: args.test_redis_url.clone(),
        ..Default::default()
    };
    #[cfg(not(feature = "test-harness"))]
    let cache = stravia_core::config::GatewayCacheConfig::default();
    GatewayConfig {
        data_dir,
        cache,
        update_source_url: args.updater_url.clone(),
        #[cfg(feature = "test-harness")]
        test_plugin_marketplace_url: args.test_plugin_marketplace_url.clone(),
        config_poll_interval: Duration::from_secs(args.config_poll_interval),
        catalog_background_refresh: catalog_base_url.is_some(),
        catalog_base_url,
        #[cfg(feature = "test-harness")]
        test_handoff_barrier: args.test_handoff_barrier,
        ..Default::default()
    }
}

fn load_dotenv() -> anyhow::Result<()> {
    match dotenvy::from_path(".env") {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "failed to listen for shutdown signal");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::warn!(%error, "failed to listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutdown signal received");
}

fn display_origin_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn listener_address(host: &str, port: u16) -> String {
    format!("{}:{port}", display_origin_host(host))
}

fn expanded_path(value: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(value).as_ref())
}

fn default_data_dir() -> String {
    default_data_dir_for_build(cfg!(debug_assertions))
        .to_string_lossy()
        .into_owned()
}

fn default_data_dir_for_build(development: bool) -> PathBuf {
    if development {
        repository_root().join(".stravia-dev")
    } else {
        PathBuf::from("~/.stravia")
    }
}

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("server crate must live under backend/apps")
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MAX_WORKER_THREADS, MI_OPTION_PURGE_DELAY, configure_allocator,
        default_data_dir_for_build, mi_option_get, repository_root, resolve_worker_threads,
    };
    use std::path::PathBuf;

    #[test]
    fn development_default_data_dir_matches_desktop_runtime_directory() {
        assert_eq!(
            default_data_dir_for_build(true),
            repository_root().join(".stravia-dev")
        );
    }

    #[test]
    fn release_default_data_dir_remains_the_user_home_directory() {
        assert_eq!(
            default_data_dir_for_build(false),
            PathBuf::from("~/.stravia")
        );
    }

    #[test]
    fn resolve_worker_threads_caps_default_and_respects_override() {
        assert_eq!(resolve_worker_threads(None, 64), DEFAULT_MAX_WORKER_THREADS);
        assert_eq!(resolve_worker_threads(None, 4), 4);
        assert_eq!(resolve_worker_threads(None, 0), 1);
        assert_eq!(resolve_worker_threads(Some("16"), 64), 16);
        assert_eq!(
            resolve_worker_threads(Some("0"), 64),
            DEFAULT_MAX_WORKER_THREADS
        );
        assert_eq!(
            resolve_worker_threads(Some("bad"), 64),
            DEFAULT_MAX_WORKER_THREADS
        );
    }

    #[test]
    fn configure_allocator_sets_immediate_purge_delay_by_default() {
        configure_allocator();
        if std::env::var_os("MIMALLOC_PURGE_DELAY").is_none() {
            let delay = unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) };
            assert_eq!(delay, 0);
        }
    }
}
