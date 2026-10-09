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
    GatewayCacheConfig, GatewayConfig, GatewayStorageConfig, SqlStorageConfig, StorageBackendKind,
};
use stravia_core::data_paths::DataPaths;
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt;
use uuid::Uuid;

use crate::http_auth::{auth_error, cookie, validate_web_request};
use crate::{AdminMode, HttpAppConfig, build_http_app};

const SETUP_COOKIE: &str = "stravia_setup";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum DatabaseConfig {
    // Empty struct variants enforce deny_unknown_fields; unit variants ignore extra keys.
    Sqlite {},
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

#[derive(Default, Serialize, Deserialize)]
struct ServerFileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    database: Option<DatabaseConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache: Option<ServerCacheConfig>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerCacheConfig {
    #[serde(default = "default_cache_capacity_mb")]
    capacity_mb: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    redis_url: Option<String>,
}

impl ServerCacheConfig {
    fn gateway_cache(&self) -> anyhow::Result<GatewayCacheConfig> {
        let capacity_bytes = usize::try_from(self.capacity_mb)
            .ok()
            .and_then(|capacity| capacity.checked_mul(1024 * 1024))
            .context("cache.capacity_mb exceeds this platform's supported capacity")?;
        Ok(GatewayCacheConfig {
            capacity_bytes,
            redis_url: self.redis_url.clone(),
        })
    }
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
    shutdown: Arc<Mutex<Option<Gateway>>>,
}

impl PreparedServerApp {
    /// 宿主先排空 HTTP 请求，再等待业务任务、观测写入和缓存命名空间清理。
    pub async fn shutdown(self) {
        if let Some(gateway) = self.shutdown.lock().await.take() {
            gateway.shutdown().await;
        }
    }
}

struct SetupRuntime {
    current: Arc<RwLock<Router>>,
    shutdown: Arc<Mutex<Option<Gateway>>>,
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

pub async fn prepare_server_app(
    mut startup: ServerStartupConfig,
) -> anyhow::Result<PreparedServerApp> {
    let file = read_server_config(&startup.config_path)?;
    if let Some(cache) = file.as_ref().and_then(|config| config.cache.as_ref()) {
        startup.gateway.cache = cache.gateway_cache()?;
    }
    if let Some(database) = file.and_then(|config| config.database) {
        let gateway_config = gateway_config(&startup.gateway, &database)?;
        let storage = Gateway::open_storage(&gateway_config)
            .await
            .map_err(|error| {
                error.context(format!(
                    "configured database is unavailable or incompatible ({})",
                    startup.config_path.display()
                ))
            })?;
        let auth = AdminAuth::new(storage);
        if auth.has_admin().await.map_err(auth_to_anyhow)? {
            let gateway = Gateway::new(gateway_config).await.map_err(|error| {
                error.context(format!(
                    "configured Gateway could not start ({})",
                    startup.config_path.display()
                ))
            })?;
            let auth = AdminAuth::new(gateway.storage.clone());
            let app = normal_app(gateway.clone(), auth, &startup);
            return Ok(PreparedServerApp {
                app,
                setup_token: None,
                shutdown: Arc::new(Mutex::new(Some(gateway))),
            });
        }
    }

    let setup_token = Uuid::new_v4().simple().to_string();
    let current = Arc::new(RwLock::new(Router::new()));
    let shutdown = Arc::new(Mutex::new(None));
    let runtime = Arc::new(SetupRuntime {
        current: current.clone(),
        shutdown: Arc::clone(&shutdown),
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
        shutdown,
    })
}

/// Read the database configuration, returning `None` before a database is selected.
/// SQLite always uses the data root; legacy path overrides require explicit migration.
/// Unreadable or invalid configurations return an error.
pub fn read_database_config(path: &Path) -> anyhow::Result<Option<DatabaseConfig>> {
    Ok(read_server_config(path)?.and_then(|config| config.database))
}

fn read_server_config(path: &Path) -> anyhow::Result<Option<ServerFileConfig>> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read server configuration"),
    };
    let config: ServerFileConfig = toml::from_str(&source).map_err(|_| {
        anyhow::anyhow!(
            "server configuration is invalid; legacy SQLite path configurations require stravia-tools migrate-data"
        )
    })?;
    if let Some(database) = config.database.as_ref() {
        validate_database_config(database)?;
    }
    if let Some(cache) = config.cache.as_ref() {
        cache.gateway_cache()?;
    }
    Ok(Some(config))
}

pub fn gateway_config(
    base: &GatewayConfig,
    database: &DatabaseConfig,
) -> anyhow::Result<GatewayConfig> {
    validate_database_config(database)?;
    let mut config = base.clone();
    match database {
        DatabaseConfig::Sqlite {} => {
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
        return *response;
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
        return *response;
    }
    let database = input.database;
    let config = match gateway_config(&runtime.startup.gateway, &database) {
        Ok(config) => config,
        Err(_) => return auth_error(StatusCode::BAD_REQUEST, "invalid_database_config"),
    };
    if Gateway::check_runtime_cache(&config).await.is_err() {
        return auth_error(StatusCode::BAD_REQUEST, "cache_unavailable");
    }
    match preflight_database(&runtime.startup.gateway.data_dir, &database).await {
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
        return *response;
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
    let database = input.database;
    let gateway_config = match gateway_config(&runtime.startup.gateway, &database) {
        Ok(config) => config,
        Err(_) => return auth_error(StatusCode::BAD_REQUEST, "invalid_database_config"),
    };
    // 缓存连接失败不能发生在创建管理员、撤销 setup 授权之后。
    if let Err(error) = Gateway::check_runtime_cache(&gateway_config).await {
        tracing::warn!(error = %error, "runtime cache preflight failed");
        return auth_error(StatusCode::BAD_REQUEST, "cache_unavailable");
    }
    if let Err(error) = preflight_database(&runtime.startup.gateway.data_dir, &database).await {
        tracing::warn!(error = %redacted_database_error(&error), "database preflight failed");
        return auth_error(StatusCode::BAD_REQUEST, "database_unavailable");
    }
    if let Err(error) = save_database_config(&runtime.startup.config_path, &database) {
        tracing::warn!(error = %error, "server configuration save failed");
        return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "config_save_failed");
    }

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
    let normal = normal_app(gateway.clone(), auth, &runtime.startup);
    // 保留首次设置创建的 Gateway；路由状态切换不能丢失宿主的关闭入口。
    *runtime.shutdown.lock().await = Some(gateway);
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
) -> Result<(), Box<Response>> {
    validate_web_request(Some(origin.as_str()), headers, json)?;
    let session = runtime.setup_session.lock().await;
    if cookie(headers, SETUP_COOKIE)
        .zip(session.as_deref())
        .is_some_and(|(a, b)| a == b)
    {
        Ok(())
    } else {
        Err(Box::new(auth_error(
            StatusCode::UNAUTHORIZED,
            "setup_unauthorized",
        )))
    }
}

async fn preflight_database(data_dir: &Path, database: &DatabaseConfig) -> anyhow::Result<()> {
    validate_database_config(database)?;
    match database {
        DatabaseConfig::Sqlite {} => {
            let paths = DataPaths::new(data_dir);
            let parent = paths.database_dir();
            std::fs::create_dir_all(&parent).context("create SQLite directory")?;
            tempfile::NamedTempFile::new_in(&parent).context("SQLite directory is not writable")?;
            let options = SqliteConnectOptions::new()
                .filename(paths.database())
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
    // 初始配置可以只有 cache；选择数据库时不能丢掉 Redis 地址和共享预算。
    let mut config = read_server_config(path)?.unwrap_or_default();
    config.database = Some(database.clone());
    let source = toml::to_string_pretty(&config)?;
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
        DatabaseConfig::Sqlite {} => {}
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

fn default_max_connections() -> u32 {
    10
}

fn default_min_connections() -> u32 {
    1
}

fn default_cache_capacity_mb() -> u32 {
    16
}

async fn setup_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};

    fn startup(directory: &Path) -> ServerStartupConfig {
        ServerStartupConfig {
            config_path: directory.join("server.toml"),
            gateway: GatewayConfig {
                data_dir: directory.join("data"),
                ..Default::default()
            },
            admin_entry: crate::AdminEntryPolicy::default(),
            proxy_cors_origins: Vec::new(),
            serve_embedded_webui: false,
        }
    }

    fn post(path: &str, input: Value, session: &str) -> anyhow::Result<Request> {
        Ok(Request::post(path)
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "http://localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-stravia-csrf", "1")
            .header(header::COOKIE, session)
            .body(Body::from(serde_json::to_vec(&input)?))?)
    }

    async fn response_json(response: Response) -> anyhow::Result<Value> {
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    async fn claim(prepared: &PreparedServerApp) -> anyhow::Result<String> {
        let response = prepared
            .app
            .clone()
            .oneshot(post(
                "/api/v1/setup/claim",
                json!({ "token": prepared.setup_token.as_ref().expect("setup token") }),
                "",
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        Ok(response.headers()[header::SET_COOKIE]
            .to_str()?
            .split(';')
            .next()
            .expect("setup cookie")
            .to_string())
    }

    fn completion(database: Value) -> Value {
        json!({
            "database": database,
            "username": "setup-admin",
            "password": "setup-regression-password",
            "client_base_url": "http://localhost:8080"
        })
    }

    async fn state(app: &Router, session: &str) -> anyhow::Result<Value> {
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/auth/state")
                    .header(header::HOST, "localhost")
                    .header(header::COOKIE, session)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        response_json(response).await
    }

    #[tokio::test]
    async fn graceful_cleanup_stops_observation_after_setup_and_configured_restart()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let config = startup(directory.path());
        let prepared = prepare_server_app(config.clone()).await?;
        let session = claim(&prepared).await?;
        let response = prepared
            .app
            .clone()
            .oneshot(post(
                "/api/v1/setup/complete",
                completion(json!({ "backend": "sqlite" })),
                &session,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);

        // 保持真实 Gateway 的观察句柄存活，确保关闭来自宿主而非最后一个引用 Drop。
        let gateway = prepared
            .shutdown
            .lock()
            .await
            .as_ref()
            .context("setup must retain its Gateway for shutdown")?
            .clone();
        gateway.admin().observation_flush().await?;
        gateway
            .storage
            .settings()
            .set("shutdown_regression", "setup-persisted")
            .await?;
        let server = crate::start_http_server(("127.0.0.1", 0), prepared.app.clone()).await?;
        server.shutdown().await?;
        prepared.shutdown().await;
        assert!(
            gateway.admin().observation_flush().await.is_err(),
            "awaited cleanup must stop the observation writer even with live handles"
        );
        drop(gateway);

        let prepared = prepare_server_app(config).await?;
        assert!(prepared.setup_token.is_none());
        let gateway = prepared
            .shutdown
            .lock()
            .await
            .as_ref()
            .context("configured startup must retain its Gateway for shutdown")?
            .clone();
        assert_eq!(
            gateway
                .storage
                .settings()
                .get("shutdown_regression")
                .await?,
            Some("setup-persisted".to_owned())
        );
        gateway.admin().observation_flush().await?;
        let server = crate::start_http_server(("127.0.0.1", 0), prepared.app.clone()).await?;
        server.shutdown().await?;
        prepared.shutdown().await;
        assert!(
            gateway.admin().observation_flush().await.is_err(),
            "configured startup must await the same real writer cleanup"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cache_only_config_enters_setup_and_completion_preserves_cache() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let startup = startup(directory.path());
        let config_path = startup.config_path.clone();
        let redis_url = "redis://cache-user:cache-secret@cache.invalid:6379/5";
        std::fs::write(
            &config_path,
            format!("[cache]\ncapacity_mb = 7\nredis_url = \"{redis_url}\"\n"),
        )?;
        assert!(read_database_config(&config_path)?.is_none());

        let prepared = prepare_server_app(startup).await?;
        assert_eq!(
            state(&prepared.app, "").await?,
            json!({
                "mode": "setup", "authenticated": false,
                "setup_authorized": false, "username": null
            })
        );
        let session = claim(&prepared).await?;
        let response = prepared
            .app
            .clone()
            .oneshot(post(
                "/api/v1/setup/complete",
                completion(json!({ "backend": "sqlite" })),
                &session,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()?
                .contains("Max-Age=0")
        );
        assert_eq!(response_json(response).await?, json!({ "mode": "server" }));

        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
        assert_eq!(saved["database"]["backend"].as_str(), Some("sqlite"));
        assert_eq!(saved["cache"]["capacity_mb"].as_integer(), Some(7));
        assert_eq!(saved["cache"]["redis_url"].as_str(), Some(redis_url));
        assert!(matches!(
            read_database_config(&config_path)?,
            Some(DatabaseConfig::Sqlite {})
        ));
        assert_eq!(
            state(&prepared.app, &session).await?,
            json!({
                "mode": "server", "authenticated": false,
                "setup_authorized": false, "username": null
            })
        );
        prepared.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn postgres_without_redis_rejects_preflight_and_keeps_setup_reusable()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let startup = startup(directory.path());
        let config_path = startup.config_path.clone();
        let data_dir = startup.gateway.data_dir.clone();
        let cache_only = "[cache]\ncapacity_mb = 9\n";
        std::fs::write(&config_path, cache_only)?;
        let prepared = prepare_server_app(startup).await?;
        let session = claim(&prepared).await?;
        let postgres = json!({
            "backend": "postgres",
            "url": "postgres://database-user:database-secret@database.invalid:5432/setup",
            "max_connections": 1,
            "min_connections": 0
        });

        for (path, input) in [
            (
                "/api/v1/setup/test",
                json!({ "database": postgres.clone() }),
            ),
            ("/api/v1/setup/complete", completion(postgres)),
        ] {
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                prepared.app.clone().oneshot(post(path, input, &session)?),
            )
            .await??;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(!response.headers().contains_key(header::SET_COOKIE));
            assert_eq!(
                response_json(response).await?,
                json!({ "error": "cache_unavailable", "code": "cache_unavailable" })
            );
            assert_eq!(std::fs::read_to_string(&config_path)?, cache_only);
            assert!(
                !data_dir.exists(),
                "preflight must not open storage or create an admin"
            );
            assert_eq!(
                state(&prepared.app, &session).await?,
                json!({
                    "mode": "setup", "authenticated": false,
                    "setup_authorized": true, "username": null
                })
            );
        }

        let response = prepared
            .app
            .clone()
            .oneshot(post(
                "/api/v1/setup/complete",
                completion(json!({ "backend": "sqlite" })),
                &session,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await?, json!({ "mode": "server" }));
        let response = prepared
            .app
            .clone()
            .oneshot(post(
                "/api/v1/auth/login",
                json!({
                    "username": "setup-admin",
                    "password": "setup-regression-password"
                }),
                "",
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await?["username"], "setup-admin");
        prepared.shutdown().await;
        Ok(())
    }
}
