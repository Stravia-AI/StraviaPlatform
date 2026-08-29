use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use stravia_core::{
    Gateway,
    config::{GatewayConfig, GatewayStorageConfig, SqlStorageConfig, StorageBackendKind},
    logging,
};
use stravia_server::{HttpAppConfig, build_http_app, standalone_local_origins, start_http_server};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "stravia-server", version, about = "Stravia AI Gateway")]
struct Args {
    // ── Server ────────────────────────────────────────────────────────────────
    #[arg(
        long,
        default_value = "127.0.0.1",
        env = "STRAVIA_HOST",
        help_heading = "Server"
    )]
    host: String,

    #[arg(
        long,
        default_value_t = stravia_server::DEFAULT_PORT,
        env = "STRAVIA_PORT",
        help_heading = "Server"
    )]
    port: u16,

    #[arg(
        long,
        env = "STRAVIA_PUBLIC_ORIGIN",
        help = "Canonical public origin for signed Artifact URLs (for example https://gateway.example.com)",
        help_heading = "Server"
    )]
    public_origin: Option<String>,

    #[arg(
        long,
        env = "STRAVIA_ADMIN_TOKEN",
        help = "Bearer token for admin API authentication",
        help_heading = "Server"
    )]
    admin_token: Option<String>,

    #[arg(
        long,
        default_value = "info",
        env = "STRAVIA_LOG_LEVEL",
        value_parser = ["error", "warn", "info", "debug", "trace"],
        help_heading = "Server"
    )]
    log_level: String,

    // ── Advanced (CORS) ───────────────────────────────────────────────────────
    #[arg(
        long = "admin-cors-origin",
        action = clap::ArgAction::Append,
        help = "Allowed CORS origin for admin API (repeatable, use '*' for any)",
        help_heading = "Advanced"
    )]
    admin_cors_origins: Vec<String>,

    #[arg(
        long = "proxy-cors-origin",
        action = clap::ArgAction::Append,
        help = "Allowed CORS origin for proxy API (repeatable, use '*' for any)",
        help_heading = "Advanced"
    )]
    proxy_cors_origins: Vec<String>,

    // ── Storage ───────────────────────────────────────────────────────────────
    #[arg(
        long,
        default_value_t = default_data_dir(),
        env = "STRAVIA_DATA_DIR",
        help_heading = "Storage"
    )]
    data_dir: String,

    #[arg(long, value_parser = ["sqlite", "postgres"], default_value = "sqlite",
          env = "STRAVIA_STORAGE_BACKEND", help_heading = "Storage")]
    storage_backend: String,

    #[arg(
        long,
        env = "STRAVIA_POSTGRES_DSN",
        help = "PostgreSQL connection string (required when --storage-backend=postgres)",
        help_heading = "Storage"
    )]
    postgres_dsn: Option<String>,

    #[arg(
        long,
        default_value_t = 10,
        help = "Postgres: max connection pool size",
        help_heading = "Storage"
    )]
    postgres_max_connections: u32,

    #[arg(
        long,
        default_value_t = 1,
        help = "Postgres: min connection pool size",
        help_heading = "Storage"
    )]
    postgres_min_connections: u32,

    #[arg(
        long,
        help = "Postgres: idle connection timeout (seconds)",
        help_heading = "Storage"
    )]
    postgres_idle_timeout: Option<u64>,

    // ── Advanced ──────────────────────────────────────────────────────────────
    #[arg(
        long,
        default_value_t = 3,
        env = "STRAVIA_CONFIG_POLL_INTERVAL",
        help = "Seconds between config epoch polls (0 = disabled); does not coordinate multiple replicas",
        help_heading = "Advanced"
    )]
    config_poll_interval: u64,

    #[cfg(debug_assertions)]
    #[arg(
        long,
        env = "STRAVIA_WIRE_CAPTURE_DIR",
        help = "Diagnostic JSONL directory for client/upstream wire payloads; headers are redacted but bodies may contain sensitive content",
        help_heading = "Advanced"
    )]
    wire_capture_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_dotenv()?;
    let args = Args::parse();

    let filter = format!("stravia={level},tower_http={level}", level = args.log_level);
    tracing_subscriber::fmt().with_env_filter(filter).init();

    run_full(&args).await
}

fn load_dotenv() -> anyhow::Result<()> {
    match dotenvy::from_path(".env") {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn run_full(args: &Args) -> anyhow::Result<()> {
    let data_dir = shellexpand::tilde(&args.data_dir).to_string();
    let admin_token = args
        .admin_token
        .clone()
        .filter(|token| !token.trim().is_empty());

    if !is_loopback_host(&args.host) && admin_token.is_none() {
        anyhow::bail!(
            "--admin-token is required when --host is not loopback (localhost/127.0.0.1/::1)"
        );
    }

    let default_origins = standalone_local_origins(args.port);
    let admin_cors_origins = if args.admin_cors_origins.is_empty() {
        default_origins.clone()
    } else {
        args.admin_cors_origins.clone()
    };
    let proxy_cors_origins = if args.proxy_cors_origins.is_empty() {
        default_origins
    } else {
        args.proxy_cors_origins.clone()
    };

    let config = GatewayConfig {
        data_dir: PathBuf::from(data_dir),
        public_origin: args
            .public_origin
            .as_deref()
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(ToOwned::to_owned),
        storage: build_storage_config(args)?,
        config_poll_interval: Duration::from_secs(args.config_poll_interval),
        #[cfg(debug_assertions)]
        wire_capture_dir: args.wire_capture_dir.clone(),
        ..Default::default()
    };

    let (gateway, log_rx) = Gateway::new(config).await?;
    let storage_for_logs = gateway.storage.clone();
    tokio::spawn(async move {
        logging::run_collector(log_rx, storage_for_logs).await;
    });

    let app = build_http_app(
        gateway,
        HttpAppConfig {
            admin_token: admin_token.clone(),
            admin_cors_origins,
            proxy_cors_origins,
            serve_embedded_webui: true,
        },
    );
    let server = start_http_server(listener_address(&args.host, args.port), app).await?;
    let address = server.local_addr();
    tracing::info!(%address, "Stravia Server listening");

    if admin_token.is_none() {
        tracing::warn!("admin API auth disabled: set --admin-token for production");
    }

    shutdown_signal().await;
    server.shutdown().await
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

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

fn build_storage_config(args: &Args) -> anyhow::Result<GatewayStorageConfig> {
    let backend = parse_storage_backend(&args.storage_backend)?;

    let postgres_url = if matches!(backend, StorageBackendKind::Postgres) {
        let dsn = args
            .postgres_dsn
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--postgres-dsn (or env STRAVIA_POSTGRES_DSN) is required \
                     when --storage-backend=postgres"
                )
            })?;
        Some(dsn.to_string())
    } else {
        None
    };

    let postgres = SqlStorageConfig {
        url: postgres_url,
        max_connections: args.postgres_max_connections,
        min_connections: args.postgres_min_connections,
        idle_timeout: args.postgres_idle_timeout.map(Duration::from_secs),
    };

    Ok(GatewayStorageConfig { backend, postgres })
}

fn parse_storage_backend(value: &str) -> anyhow::Result<StorageBackendKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sqlite" => Ok(StorageBackendKind::Sqlite),
        "postgres" => Ok(StorageBackendKind::Postgres),
        other => anyhow::bail!("unsupported storage backend: {other}"),
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn listener_address(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
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
