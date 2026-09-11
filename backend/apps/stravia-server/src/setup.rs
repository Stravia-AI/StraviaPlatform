use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::admin_entry::RequestOrigin;
use anyhow::{Context, bail};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use stravia_core::Gateway;
use stravia_core::admin::identity::{AdminAuth, AuthError};
use stravia_core::config::{
    GatewayConfig, GatewayStorageConfig, SqlStorageConfig, StorageBackendKind,
};
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt;
use uuid::Uuid;

use crate::http_auth::{auth_error, cookie, validate_web_request};
use crate::{AdminMode, HttpAppConfig, build_http_app};

const SETUP_COOKIE: &str = "stravia_setup";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum DatabaseConfig {
    Sqlite {
        path: PathBuf,
    },
    Postgres {
        url: String,
        #[serde(default = "default_max_connections")]
        max_connections: u32,
        #[serde(default = "default_min_connections")]
        min_connections: u32,
        #[serde(default)]
        idle_timeout_seconds: Option<u64>,
    },
}

#[derive(Serialize, Deserialize)]
struct ServerFileConfig {
    database: DatabaseConfig,
}

#[derive(Clone)]
pub struct ServerStartupConfig {
    pub config_path: PathBuf,
    pub gateway: GatewayConfig,
    pub admin_entry: crate::AdminEntryPolicy,
    pub proxy_cors_origins: Vec<String>,
    pub serve_embedded_webui: bool,
}

pub struct PreparedServerApp {
    pub app: Router,
    pub setup_token: Option<String>,
}

struct SetupRuntime {
    current: Arc<RwLock<Router>>,
    startup: ServerStartupConfig,
    setup_token: Mutex<Option<String>>,
    setup_session: Mutex<Option<String>>,
    completion: Mutex<()>,
}

#[derive(Deserialize)]
struct ClaimInput {
    token: String,
}

#[derive(Deserialize)]
struct TestInput {
    database: DatabaseConfig,
}

#[derive(Deserialize)]
struct CompleteInput {
    database: DatabaseConfig,
    username: String,
    password: String,
    client_base_url: String,
}

#[derive(Serialize)]
struct SetupStateResponse {
    mode: &'static str,
    authenticated: bool,
    setup_authorized: bool,
    username: Option<String>,
}

pub async fn prepare_server_app(startup: ServerStartupConfig) -> anyhow::Result<PreparedServerApp> {
    match read_database_config(&startup.config_path)? {
        Some(database) => {
            let gateway_config = gateway_config(&startup.gateway, &database)?;
            let storage = Gateway::open_storage(&gateway_config).await.map_err(|_| {
                anyhow::anyhow!(
                    "configured database is unavailable or incompatible ({})",
                    startup.config_path.display()
                )
            })?;
            let auth = AdminAuth::new(storage);
            if auth.has_admin().await.map_err(auth_to_anyhow)? {
                let gateway = Gateway::new(gateway_config).await.map_err(|_| {
                    anyhow::anyhow!(
                        "configured Gateway could not start ({})",
                        startup.config_path.display()
                    )
                })?;
                let auth = AdminAuth::new(gateway.storage.clone());
                let app = normal_app(gateway, auth, &startup);
                return Ok(PreparedServerApp {
                    app,
                    setup_token: None,
                });
            }
        }
        None => {}
    }

    let setup_token = Uuid::new_v4().simple().to_string();
    let current = Arc::new(RwLock::new(Router::new()));
    let runtime = Arc::new(SetupRuntime {
        current: current.clone(),
        startup,
        setup_token: Mutex::new(Some(setup_token.clone())),
        setup_session: Mutex::new(None),
        completion: Mutex::new(()),
    });
    let setup = setup_router(runtime.clone());
    *current.write().await = setup;
    let app = Router::new().fallback(dispatch_current).with_state(runtime);
    Ok(PreparedServerApp {
        app,
        setup_token: Some(setup_token),
    })
}

/// Read the database configuration, returning `None` when the file is absent.
/// Relative SQLite paths are resolved from the configuration file's directory.
/// Unreadable or invalid configurations and unresolvable paths return an error.
pub fn read_database_config(path: &Path) -> anyhow::Result<Option<DatabaseConfig>> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read server configuration"),
    };
    let config: ServerFileConfig =
        toml::from_str(&source).map_err(|_| anyhow::anyhow!("server configuration is invalid"))?;
    resolve_database_config(path, config.database).map(Some)
}

fn resolve_database_config(
    config_path: &Path,
    mut database: DatabaseConfig,
) -> anyhow::Result<DatabaseConfig> {
    validate_database_config(&database)?;
    if let DatabaseConfig::Sqlite { path } = &mut database {
        *path = expand_path(path);
        if path.is_relative() {
            let config_path =
                std::path::absolute(config_path).context("resolve configuration file path")?;
            let directory = config_path
                .parent()
                .context("configuration file has no parent directory")?;
            *path = directory.join(&*path);
        }
    }
    Ok(database)
}

pub fn gateway_config(
    base: &GatewayConfig,
    database: &DatabaseConfig,
) -> anyhow::Result<GatewayConfig> {
    validate_database_config(database)?;
    let mut config = base.clone();
    match database {
        DatabaseConfig::Sqlite { path } => {
            let path = expand_path(path);
            let parent = path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            config.data_dir = parent.to_path_buf();
            config.storage = GatewayStorageConfig::default();
        }
        DatabaseConfig::Postgres {
            url,
            max_connections,
            min_connections,
            idle_timeout_seconds,
        } => {
            config.storage = GatewayStorageConfig {
                backend: StorageBackendKind::Postgres,
                postgres: SqlStorageConfig {
                    url: Some(url.trim().to_string()),
                    max_connections: *max_connections,
                    min_connections: *min_connections,
                    idle_timeout: idle_timeout_seconds.map(Duration::from_secs),
                },
            };
        }
    }
    Ok(config)
}

pub async fn recover_admin(config_path: &Path, base: GatewayConfig) -> anyhow::Result<()> {
    let database = read_database_config(config_path)?
        .context("server configuration does not exist; initial setup is required")?;
    let storage = Gateway::open_storage(&gateway_config(&base, &database)?)
        .await
        .context("open configured database")?;
    let auth = AdminAuth::new(storage);
    if !auth.has_admin().await.map_err(auth_to_anyhow)? {
        bail!("configured database has no administrator; complete initial setup instead");
    }

    let mut username = String::new();
    print!("New administrator username: ");
    std::io::stdout().flush()?;
    std::io::stdin().read_line(&mut username)?;
    let password = rpassword::prompt_password("New administrator password: ")?;
    let confirmation = rpassword::prompt_password("Confirm new administrator password: ")?;
    if password != confirmation {
        bail!("password confirmation does not match");
    }
    auth.recover_credentials(username.trim(), &password)
        .await
        .map_err(auth_to_anyhow)?;
    println!("Administrator credentials recovered; all sessions revoked.");
    Ok(())
}

fn setup_router(runtime: Arc<SetupRuntime>) -> Router {
    let policy = runtime.startup.admin_entry.clone();
    let serve_embedded_webui = runtime.startup.serve_embedded_webui;
    let router = Router::new()
        .route("/healthz", get(setup_health))
        .route("/readyz", get(setup_ready))
        .route("/api/v1/auth/state", get(setup_state))
        .route("/api/v1/setup/claim", post(claim_setup))
        .route("/api/v1/setup/test", post(test_database))
        .route("/api/v1/setup/complete", post(complete_setup))
        .with_state(runtime);

    #[cfg(all(feature = "embed-webui", not(debug_assertions)))]
    if serve_embedded_webui {
        return policy.protect(router.fallback(crate::serve_embedded_webui_or_not_found));
    }
    let _ = serve_embedded_webui;
    policy.protect(router.fallback(setup_not_found))
}

async fn dispatch_current(State(runtime): State<Arc<SetupRuntime>>, request: Request) -> Response {
    let router = { runtime.current.read().await.clone() };
    router
        .oneshot(request)
        .await
        .unwrap_or_else(|never| match never {})
}

async fn setup_health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

async fn setup_ready() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "status": "setup" })),
    )
}

fn unavailable_router(runtime: &SetupRuntime) -> Router {
    let policy = runtime.startup.admin_entry.clone();
    let serve_embedded_webui = runtime.startup.serve_embedded_webui;
    let router = Router::new()
        .route("/healthz", get(setup_health))
        .route("/readyz", get(unavailable_ready))
        .route("/api/v1/auth/state", get(unavailable_state));

    #[cfg(all(feature = "embed-webui", not(debug_assertions)))]
    if serve_embedded_webui {
        return policy.protect(router.fallback(crate::serve_embedded_webui_or_not_found));
    }
    let _ = serve_embedded_webui;
    policy.protect(router.fallback(setup_not_found))
}

async fn unavailable_ready() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "status": "unavailable" })),
    )
}

async fn unavailable_state() -> impl IntoResponse {
    Json(SetupStateResponse {
        mode: "unavailable",
        authenticated: false,
        setup_authorized: false,
        username: None,
    })
}

async fn setup_state(
    State(runtime): State<Arc<SetupRuntime>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session = runtime.setup_session.lock().await;
    let authorized = cookie(&headers, SETUP_COOKIE)
        .zip(session.as_deref())
        .is_some_and(|(cookie, expected)| cookie == expected);
    Json(SetupStateResponse {
        mode: "setup",
        authenticated: false,
        setup_authorized: authorized,
        username: None,
    })
}

async fn claim_setup(
    State(runtime): State<Arc<SetupRuntime>>,
    Extension(origin): Extension<RequestOrigin>,
    headers: HeaderMap,
    Json(input): Json<ClaimInput>,
) -> Response {
    if let Err(response) = validate_web_request(Some(origin.as_str()), &headers, true) {
        return response;
    }
    let mut token = runtime.setup_token.lock().await;
    if token.as_deref() != Some(input.token.as_str()) {
        return auth_error(StatusCode::UNAUTHORIZED, "invalid_setup_token");
    }
    token.take();
    let session = Uuid::new_v4().simple().to_string();
    *runtime.setup_session.lock().await = Some(session.clone());
    let secure = origin.secure();
    let secure = if secure { "; Secure" } else { "" };
    let value =
        format!("{SETUP_COOKIE}={session}; Path=/api/v1; HttpOnly; SameSite=Strict{secure}");
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

async fn test_database(
    State(runtime): State<Arc<SetupRuntime>>,
    Extension(origin): Extension<RequestOrigin>,
    headers: HeaderMap,
    Json(input): Json<TestInput>,
) -> Response {
    if let Err(response) = authorize_setup(&runtime, &origin, &headers, true).await {
        return response;
    }
    let database = match resolve_database_config(&runtime.startup.config_path, input.database) {
        Ok(database) => database,
        Err(_) => return auth_error(StatusCode::BAD_REQUEST, "database_unavailable"),
    };
    match preflight_database(&database).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => auth_error(StatusCode::BAD_REQUEST, "database_unavailable"),
    }
}

async fn complete_setup(
    State(runtime): State<Arc<SetupRuntime>>,
    Extension(origin): Extension<RequestOrigin>,
    headers: HeaderMap,
    Json(input): Json<CompleteInput>,
) -> Response {
    if let Err(response) = authorize_setup(&runtime, &origin, &headers, true).await {
        return response;
    }
    let _completion = runtime.completion.lock().await;
    let mut setup_session = runtime.setup_session.lock().await;
    if cookie(&headers, SETUP_COOKIE)
        .zip(setup_session.as_deref())
        .is_none_or(|(cookie, expected)| cookie != expected)
    {
        return auth_error(StatusCode::CONFLICT, "setup_complete");
    }
    let artifact_settings = stravia_core::agent::artifact::ArtifactSettings {
        client_base_url: input.client_base_url,
        ..Default::default()
    };
    if let Err(error) = artifact_settings.validate_for_save() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    let database = match resolve_database_config(&runtime.startup.config_path, input.database) {
        Ok(database) => database,
        Err(_) => return auth_error(StatusCode::BAD_REQUEST, "database_unavailable"),
    };
    if let Err(error) = preflight_database(&database).await {
        tracing::warn!(error = %redacted_database_error(&error), "database preflight failed");
        return auth_error(StatusCode::BAD_REQUEST, "database_unavailable");
    }
    if let Err(error) = save_database_config(&runtime.startup.config_path, &database) {
        tracing::warn!(error = %error, "server configuration save failed");
        return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "config_save_failed");
    }

    let gateway_config = match gateway_config(&runtime.startup.gateway, &database) {
        Ok(config) => config,
        Err(_) => return auth_error(StatusCode::BAD_REQUEST, "invalid_database_config"),
    };
    let storage = match Gateway::open_storage(&gateway_config).await {
        Ok(storage) => storage,
        Err(error) => {
            tracing::warn!(error = %redacted_database_error(&error), "database migration failed");
            return auth_error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
        }
    };
    let artifact_json = match serde_json::to_string(&artifact_settings) {
        Ok(value) => value,
        Err(_) => return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "config_save_failed"),
    };
    let auth = AdminAuth::new(storage.clone());
    let has_admin = match auth.has_admin().await {
        Ok(value) => value,
        Err(error) => return map_setup_auth_error(error),
    };
    if !has_admin {
        if storage
            .settings()
            .set("artifact_settings", &artifact_json)
            .await
            .is_err()
        {
            return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "config_save_failed");
        }
        if let Err(error) = auth.create_admin(&input.username, &input.password).await {
            if matches!(&error, AuthError::Conflict) {
                match auth.has_admin().await {
                    Ok(true) => {}
                    Ok(false) => return map_setup_auth_error(error),
                    Err(check_error) => return map_setup_auth_error(check_error),
                }
            } else {
                return map_setup_auth_error(error);
            }
        }
    }

    // Once an administrator exists, setup authorization is permanently gone even if
    // the full Gateway cannot start. Concurrent requests observe unavailable mode.
    *setup_session = None;
    drop(setup_session);
    *runtime.setup_token.lock().await = None;
    *runtime.current.write().await = unavailable_router(&runtime);

    let gateway = match Gateway::new(gateway_config).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %redacted_database_error(&error), "gateway initialization failed");
            return clear_setup_cookie(
                &origin,
                auth_error(StatusCode::SERVICE_UNAVAILABLE, "gateway_unavailable"),
            );
        }
    };
    let auth = AdminAuth::new(gateway.storage.clone());
    let normal = normal_app(gateway, auth, &runtime.startup);
    *runtime.current.write().await = normal;
    clear_setup_cookie(
        &origin,
        Json(serde_json::json!({ "mode": "server" })).into_response(),
    )
}

fn clear_setup_cookie(origin: &RequestOrigin, mut response: Response) -> Response {
    let secure = if origin.secure() { "; Secure" } else { "" };
    let value =
        format!("{SETUP_COOKIE}=; Path=/api/v1; HttpOnly; SameSite=Strict; Max-Age=0{secure}");
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

async fn authorize_setup(
    runtime: &SetupRuntime,
    origin: &RequestOrigin,
    headers: &HeaderMap,
    json: bool,
) -> Result<(), Response> {
    validate_web_request(Some(origin.as_str()), headers, json)?;
    let session = runtime.setup_session.lock().await;
    if cookie(headers, SETUP_COOKIE)
        .zip(session.as_deref())
        .is_some_and(|(a, b)| a == b)
    {
        Ok(())
    } else {
        Err(auth_error(StatusCode::UNAUTHORIZED, "setup_unauthorized"))
    }
}

async fn preflight_database(database: &DatabaseConfig) -> anyhow::Result<()> {
    validate_database_config(database)?;
    match database {
        DatabaseConfig::Sqlite { path } => {
            let path = expand_path(path);
            let parent = path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent).context("create SQLite directory")?;
            tempfile::NamedTempFile::new_in(parent).context("SQLite directory is not writable")?;
            let options = SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await
                .context("connect SQLite database")?;
            pool.close().await;
        }
        DatabaseConfig::Postgres {
            url,
            max_connections,
            min_connections,
            idle_timeout_seconds,
        } => {
            let pool = PgPoolOptions::new()
                .max_connections(*max_connections)
                .min_connections(*min_connections)
                .idle_timeout(idle_timeout_seconds.map(Duration::from_secs))
                .connect(url)
                .await
                .map_err(|_| anyhow::anyhow!("connect PostgreSQL database failed"))?;
            pool.close().await;
        }
    }
    Ok(())
}

fn save_database_config(path: &Path, database: &DatabaseConfig) -> anyhow::Result<()> {
    validate_database_config(database)?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).context("create configuration directory")?;
    let source = toml::to_string_pretty(&ServerFileConfig {
        database: database.clone(),
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .context("configuration directory is not writable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(source.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("atomically replace server configuration")?;
    Ok(())
}

fn validate_database_config(database: &DatabaseConfig) -> anyhow::Result<()> {
    match database {
        DatabaseConfig::Sqlite { path } => {
            if path.file_name().and_then(|value| value.to_str()) != Some("gateway.db") {
                bail!("SQLite database path must end in gateway.db");
            }
        }
        DatabaseConfig::Postgres {
            url,
            max_connections,
            min_connections,
            ..
        } => {
            let valid_url = url::Url::parse(url.trim()).ok().is_some_and(|url| {
                matches!(url.scheme(), "postgres" | "postgresql") && url.host_str().is_some()
            });
            if !valid_url {
                bail!("PostgreSQL URL is invalid");
            }
            if *max_connections == 0 || min_connections > max_connections {
                bail!("PostgreSQL pool sizes are invalid");
            }
        }
    }
    Ok(())
}

fn normal_app(gateway: Gateway, auth: AdminAuth, startup: &ServerStartupConfig) -> Router {
    build_http_app(
        gateway,
        HttpAppConfig {
            admin_auth: auth,
            admin_mode: AdminMode::Server,
            admin_entry: startup.admin_entry.clone(),
            desktop_cors_origins: Vec::new(),
            proxy_cors_origins: startup.proxy_cors_origins.clone(),
            serve_embedded_webui: startup.serve_embedded_webui,
        },
    )
}

fn map_setup_auth_error(error: AuthError) -> Response {
    match error {
        AuthError::Conflict => auth_error(StatusCode::CONFLICT, "admin_exists"),
        AuthError::InvalidInput(_) => auth_error(StatusCode::BAD_REQUEST, "invalid_credentials"),
        AuthError::InvalidCredentials | AuthError::Unauthorized => {
            auth_error(StatusCode::UNAUTHORIZED, "unauthorized")
        }
        AuthError::Storage(_) => auth_error(StatusCode::SERVICE_UNAVAILABLE, "auth_unavailable"),
    }
}

fn auth_to_anyhow(error: AuthError) -> anyhow::Error {
    match error {
        AuthError::InvalidCredentials => anyhow::anyhow!("invalid administrator credentials"),
        AuthError::Unauthorized => anyhow::anyhow!("administrator authorization failed"),
        AuthError::Conflict => anyhow::anyhow!("administrator already exists"),
        AuthError::InvalidInput(message) => anyhow::anyhow!(message),
        AuthError::Storage(error) => error,
    }
}

fn redacted_database_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("postgres://") || message.contains("postgresql://") {
        "database operation failed".to_string()
    } else {
        message
    }
}

fn expand_path(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).as_ref())
}

fn default_max_connections() -> u32 {
    10
}

fn default_min_connections() -> u32 {
    1
}

async fn setup_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}
