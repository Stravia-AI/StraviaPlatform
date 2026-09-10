use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use stravia_core::config::GatewayConfig;
use stravia_server::{
    DEFAULT_PORT, ServerStartupConfig, prepare_server_app, recover_admin, standalone_local_origins,
    start_http_server,
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
        long,
        env = "STRAVIA_PUBLIC_ORIGIN",
        help = "Canonical public origin used for browser security; file access addresses are managed in settings",
        help_heading = "Server",
        global = true
    )]
    public_origin: Option<String>,

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
        long = "admin-cors-origin",
        action = clap::ArgAction::Append,
        help = "Allowed CORS origin for admin API (repeatable; wildcard is not accepted)",
        help_heading = "Advanced",
        global = true
    )]
    admin_cors_origins: Vec<String>,

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
    mut gateway: GatewayConfig,
) -> anyhow::Result<()> {
    let admin_origin = canonical_admin_origin(args)?;
    gateway.public_origin = Some(admin_origin.clone());
    if args
        .admin_cors_origins
        .iter()
        .any(|origin| origin.trim() == "*")
    {
        bail!("wildcard admin CORS origin is not allowed with cookie authentication");
    }
    let local_origins = standalone_local_origins(args.port);
    let mut admin_cors_origins = args.admin_cors_origins.clone();
    if !admin_cors_origins
        .iter()
        .any(|origin| origin.trim() == admin_origin.as_str())
    {
        admin_cors_origins.push(admin_origin.clone());
    }
    let proxy_cors_origins = if args.proxy_cors_origins.is_empty() {
        local_origins
    } else {
        args.proxy_cors_origins.clone()
    };

    let prepared = prepare_server_app(ServerStartupConfig {
        config_path,
        gateway,
        admin_origin,
        admin_cors_origins,
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
        public_origin: args
            .public_origin
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        config_poll_interval: Duration::from_secs(args.config_poll_interval),
        ..Default::default()
    }
}

fn canonical_admin_origin(args: &Args) -> anyhow::Result<String> {
    let origin = match args
        .public_origin
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            let url =
                url::Url::parse(value).context("--public-origin must be a valid HTTP(S) origin")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/"
            {
                bail!(
                    "--public-origin must contain only an HTTP(S) scheme, host, and optional port"
                );
            }
            url.origin().ascii_serialization()
        }
        None if is_loopback_host(&args.host) => {
            format!("http://{}:{}", display_origin_host(&args.host), args.port)
        }
        None => bail!("--public-origin is required when --host is not loopback"),
    };
    if !is_loopback_host(&args.host) && !origin.starts_with("https://") {
        bail!("--public-origin must use HTTPS when --host is not loopback");
    }
    Ok(origin)
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

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
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
