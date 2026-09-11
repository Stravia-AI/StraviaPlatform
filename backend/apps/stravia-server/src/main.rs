use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use stravia_core::config::GatewayConfig;
use stravia_server::{
    AdminEntryPolicy, DEFAULT_PORT, ServerStartupConfig, prepare_server_app, recover_admin,
    standalone_local_origins, start_http_server,
};

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
        help = "Runtime data and artifact directory (not a database override)",
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
        long,
        default_value_t = 3,
        env = "STRAVIA_CONFIG_POLL_INTERVAL",
        help = "Seconds between config epoch polls (0 = disabled); does not coordinate multiple replicas",
        help_heading = "Advanced",
        global = true
    )]
    config_poll_interval: u64,
}

#[derive(Subcommand)]
enum Command {
    /// Interactively replace the sole administrator credentials and revoke all sessions.
    RecoverAdmin,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_dotenv()?;
    let args = Args::parse();
    let filter = format!("stravia={level},tower_http={level}", level = args.log_level);
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let data_dir = expanded_path(&args.data_dir);
    let config_path = args
        .config
        .as_deref()
        .map(expanded_path)
        .unwrap_or_else(|| data_dir.join("server.toml"));
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

    let prepared = prepare_server_app(ServerStartupConfig {
        config_path,
        gateway,
        admin_entry,
        proxy_cors_origins,
        serve_embedded_webui: true,
    })
    .await?;
    if let Some(token) = prepared.setup_token.as_deref() {
        println!("Stravia setup token: {token}");
        std::io::stdout().flush()?;
    }

    let server = start_http_server(listener_address(&args.host, args.port), prepared.app).await?;
    let address = server.local_addr();
    tracing::info!(%address, "Stravia Server listening");
    shutdown_signal().await;
    server.shutdown().await
}

fn base_gateway_config(args: &Args, data_dir: PathBuf) -> GatewayConfig {
    GatewayConfig {
        data_dir,
        config_poll_interval: Duration::from_secs(args.config_poll_interval),
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
    use super::{default_data_dir_for_build, repository_root};
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
}
