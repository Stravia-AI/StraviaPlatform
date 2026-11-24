use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::{Extension, Router};
use tokio::sync::{Mutex, watch};
use tower_http::cors::{AllowOrigin, CorsLayer};

#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
use axum::body::Body;
#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
use axum::http::Uri;
#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
use axum::response::{IntoResponse, Response};
#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
use rust_embed::RustEmbed;

use stravia_core::Gateway;
use stravia_core::admin::identity::{AdminAuth, SingleAdminAuthProvider, SingleAdminAuthorizer};

mod admin_entry;
mod admin_routes;
pub use admin_entry::AdminEntryPolicy;
pub mod extensions;
pub use extensions::{
    CapabilitiesResponse, CapabilitiesSnapshot, CapabilityProvider, CeCapabilityProvider, Edition,
    HttpAppExtensions, LicenseStatus, LicenseTier, PostPrepareHook,
};
mod http_auth;
mod oauth_callback;
mod setup;
mod startup;

pub use startup::StartupHttpApp;

pub use setup::{
    DatabaseConfig, PreparedServerApp, ServerStartupConfig, gateway_config, prepare_server_app,
    read_database_config, recover_admin,
};

pub const DEFAULT_PORT: u16 = 23471;

#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
#[derive(RustEmbed)]
#[folder = "../../../frontend/stravia-webui/dist/"]
struct WebUiAssets;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminMode {
    Server,
    Desktop,
}

#[derive(Clone)]
pub struct HttpAppConfig {
    pub admin_auth: AdminAuth,
    pub admin_mode: AdminMode,
    pub admin_entry: AdminEntryPolicy,
    pub desktop_cors_origins: Vec<String>,
    pub proxy_cors_origins: Vec<String>,
    pub serve_embedded_webui: bool,
    pub extensions: HttpAppExtensions,
}

pub fn standalone_local_origins(port: u16) -> Vec<String> {
    vec![
        "tauri://localhost".to_string(),
        "http://tauri.localhost".to_string(),
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ]
}

pub fn desktop_origins() -> Vec<String> {
    vec![
        "tauri://localhost".to_string(),
        "http://tauri.localhost".to_string(),
        #[cfg(debug_assertions)]
        "http://localhost:5173".to_string(),
    ]
}

pub fn build_http_app(gateway: Gateway, config: HttpAppConfig) -> Router {
    let auth_provider = config
        .extensions
        .admin_auth_provider
        .unwrap_or_else(|| Arc::new(SingleAdminAuthProvider::new(config.admin_auth)));
    let authorizer = config
        .extensions
        .admin_authorizer
        .unwrap_or_else(|| Arc::new(SingleAdminAuthorizer));
    let capability_provider = config
        .extensions
        .capability_provider
        .unwrap_or_else(|| Arc::new(CeCapabilityProvider));
    let mut admin_router = admin_routes::create_router(
        gateway.clone(),
        http_auth::AdminHttpState {
            auth_provider,
            authorizer,
            mode: config.admin_mode,
        },
        capability_provider,
        config.extensions.extra_admin_routes,
        config.extensions.extra_public_routes,
    );
    if config.admin_mode == AdminMode::Desktop {
        admin_router = admin_router.layer(build_admin_cors_layer(&config.desktop_cors_origins));
    }
    let proxy_router = stravia_core::proxy::server::create_router(gateway)
        .layer(Extension(
            stravia_core::proxy::server::AllowedWebSocketOrigins::new(
                config.proxy_cors_origins.clone(),
            ),
        ))
        .layer(build_proxy_cors_layer(&config.proxy_cors_origins));
    let app = admin_router.merge(proxy_router);

    #[cfg(all(feature = "embed-webui", not(debug_assertions)))]
    {
        if config.serve_embedded_webui {
            let app = app.fallback(serve_embedded_webui_or_not_found);
            return if config.admin_mode == AdminMode::Server {
                config.admin_entry.protect(app)
            } else {
                app
            };
        }
    }

    let app = app.fallback(api_not_found);
    if config.admin_mode == AdminMode::Server {
        config.admin_entry.protect(app)
    } else {
        app
    }
}

struct ServerState {
    local_addr: SocketAddr,
    shutdown_tx: watch::Sender<()>,
    task: Mutex<Option<tokio::task::JoinHandle<anyhow::Result<()>>>>,
}

#[derive(Clone)]
pub struct ServerHandle(Arc<ServerState>);

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.0.local_addr
    }

    pub fn shutdown(&self) {
        let _ = self.0.shutdown_tx.send(());
    }

    async fn wait_for_stop(&self) -> anyhow::Result<()> {
        let task = self.0.task.lock().await.take();
        if let Some(task) = task {
            task.await.context("HTTP server task panicked")??;
        }
        Ok(())
    }
}

pub struct RunningHttpServer {
    handle: ServerHandle,
}

impl RunningHttpServer {
    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.handle.local_addr()
    }

    pub fn detach(self) {}

    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.handle.shutdown();
        self.handle.wait_for_stop().await
    }

    pub async fn shutdown_with_timeout(self, timeout: std::time::Duration) -> anyhow::Result<()> {
        self.handle.shutdown();
        let task = self.handle.0.task.lock().await.take();
        if let Some(mut task) = task {
            match tokio::time::timeout(timeout, &mut task).await {
                Ok(result) => result.context("HTTP server task panicked")??,
                Err(_) => {
                    if !timeout.is_zero() {
                        tracing::warn!(
                            timeout_secs = timeout.as_secs(),
                            "HTTP server drain timed out; aborting remaining connections"
                        );
                    }
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        Ok(())
    }
}

pub async fn start_http_server(
    address: impl tokio::net::ToSocketAddrs,
    app: Router,
) -> anyhow::Result<RunningHttpServer> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    let local_addr = listener.local_addr()?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    let task = tokio::spawn(async move {
        let result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await
        .map_err(anyhow::Error::from);
        if let Err(error) = &result {
            tracing::error!(%error, "HTTP server stopped unexpectedly");
        }
        result
    });

    Ok(RunningHttpServer {
        handle: ServerHandle(Arc::new(ServerState {
            local_addr,
            shutdown_tx,
            task: Mutex::new(Some(task)),
        })),
    })
}

#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
async fn serve_embedded_webui_or_not_found(uri: Uri) -> Response {
    if is_reserved_api_namespace(uri.path()) {
        return api_not_found().await.into_response();
    }

    let path = uri.path().trim_start_matches('/');
    let file_path = if path.is_empty() { "index.html" } else { path };

    match WebUiAssets::get(file_path) {
        Some(content) => Response::builder()
            .header(header::CONTENT_TYPE, infer_mime(file_path))
            .body(Body::from(content.data.into_owned()))
            .expect("embedded WebUI response should be valid"),
        None => match WebUiAssets::get("index.html") {
            Some(content) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(content.data.into_owned()))
                .expect("embedded WebUI response should be valid"),
            None => api_not_found().await.into_response(),
        },
    }
}

async fn api_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
fn is_reserved_api_namespace(path: &str) -> bool {
    matches!(path, "/api" | "/v1" | "/v1beta" | "/mcp" | "/codex")
        || path.starts_with("/mcp/")
        || path.starts_with("/api/")
        || path.starts_with("/v1/")
        || path.starts_with("/v1beta/")
        || path.starts_with("/codex/")
}

#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
fn infer_mime(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".json") || path.ends_with(".map") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

fn build_admin_cors_layer(origins: &[String]) -> CorsLayer {
    let origins = origins
        .iter()
        .filter(|origin| origin.trim() != "*")
        .filter_map(|origin| HeaderValue::from_str(origin.trim()).ok())
        .collect::<Vec<_>>();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::HeaderName::from_static("x-api-key"),
            header::HeaderName::from_static("anthropic-version"),
            header::HeaderName::from_static("x-stravia-csrf"),
        ])
        .allow_credentials(true)
}

fn build_proxy_cors_layer(origins: &[String]) -> CorsLayer {
    build_cors_layer(
        origins,
        [Method::GET, Method::POST, Method::DELETE, Method::OPTIONS],
        [
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::HeaderName::from_static("x-api-key"),
            header::HeaderName::from_static("anthropic-version"),
            header::HeaderName::from_static("anthropic-beta"),
            header::HeaderName::from_static("openai-beta"),
            header::HeaderName::from_static("openai-organization"),
            header::HeaderName::from_static("openai-project"),
            header::HeaderName::from_static("idempotency-key"),
            header::HeaderName::from_static("x-request-id"),
            header::HeaderName::from_static("mcp-protocol-version"),
            header::HeaderName::from_static("mcp-session-id"),
            header::HeaderName::from_static("last-event-id"),
        ],
    )
    .expose_headers([header::HeaderName::from_static("mcp-session-id")])
}

fn build_cors_layer<const METHOD_COUNT: usize, const HEADER_COUNT: usize>(
    origins: &[String],
    methods: [Method; METHOD_COUNT],
    headers: [header::HeaderName; HEADER_COUNT],
) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(parse_allow_origin(origins))
        .allow_methods(methods)
        .allow_headers(headers)
}

fn parse_allow_origin(origins: &[String]) -> AllowOrigin {
    if origins.iter().any(|origin| origin.trim() == "*") {
        return AllowOrigin::any();
    }

    let values = origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin.trim()).ok())
        .collect::<Vec<_>>();

    if values.is_empty() {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(values)
    }
}

#[cfg(test)]
mod tests {
    use std::{future::pending, sync::Arc, time::Duration};

    use axum::{Router, routing::get};
    use tokio::sync::{Mutex, oneshot};

    use super::start_http_server;

    #[tokio::test]
    async fn shutdown_timeout_stops_waiting_for_an_in_flight_request() {
        let (started_tx, started_rx) = oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let app = Router::new().route(
            "/slow",
            get(move || {
                let started_tx = started_tx.clone();
                async move {
                    if let Some(started_tx) = started_tx.lock().await.take() {
                        let _ = started_tx.send(());
                    }
                    pending::<()>().await
                }
            }),
        );
        let server = start_http_server(("127.0.0.1", 0), app)
            .await
            .expect("server should start");
        let address = server.local_addr();
        let request = tokio::spawn(reqwest::get(format!("http://{address}/slow")));
        started_rx.await.expect("slow request should start");

        tokio::time::timeout(
            Duration::from_secs(1),
            server.shutdown_with_timeout(Duration::from_millis(20)),
        )
        .await
        .expect("forced shutdown should finish")
        .expect("forced shutdown should not fail");

        tokio::net::TcpListener::bind(address)
            .await
            .expect("shutdown should release the listener");
        request.abort();
        request
            .await
            .expect_err("request task should be cancelled after the server stops waiting");
    }

    struct StubCapabilityProvider {
        snapshot: crate::CapabilitiesSnapshot,
    }

    impl crate::CapabilityProvider for StubCapabilityProvider {
        fn capabilities(&self) -> crate::CapabilitiesSnapshot {
            self.snapshot.clone()
        }
    }

    struct DenyingAuthorizer {
        denied_prefix: &'static str,
    }

    #[async_trait::async_trait]
    impl stravia_core::admin::identity::AdminAuthorizer for DenyingAuthorizer {
        async fn authorize(
            &self,
            _principal: &stravia_core::admin::identity::AdminPrincipal,
            _action: stravia_core::admin::identity::AdminAction,
            resource: &stravia_core::admin::identity::AdminResourceRef<'_>,
        ) -> Result<(), stravia_core::admin::identity::AuthorizationDenied> {
            if resource
                .id
                .is_some_and(|path| path.starts_with(self.denied_prefix))
            {
                return Err(
                    stravia_core::admin::identity::AuthorizationDenied::forbidden(
                        "ee_role_denied",
                        "Not permitted by commercial authorizer",
                    ),
                );
            }
            Ok(())
        }
    }

    async fn desktop_gateway_and_token(
        extensions: crate::HttpAppExtensions,
    ) -> (Router, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let gateway = stravia_core::GatewayBuilder::new(stravia_core::config::GatewayConfig {
            data_dir: dir.path().to_path_buf(),
            catalog_background_refresh: false,
            catalog_base_url: None,
            ..Default::default()
        })
        .build()
        .await
        .expect("build gateway");
        let admin_auth = stravia_core::admin::identity::AdminAuth::new(gateway.storage.clone());
        admin_auth
            .ensure_native_admin()
            .await
            .expect("ensure native admin");
        let tokens = admin_auth.login_native().await.expect("login native");
        let app = crate::build_http_app(
            gateway,
            crate::HttpAppConfig {
                admin_auth,
                admin_mode: crate::AdminMode::Desktop,
                admin_entry: Default::default(),
                desktop_cors_origins: Vec::new(),
                proxy_cors_origins: Vec::new(),
                serve_embedded_webui: false,
                extensions,
            },
        );
        (app, tokens.access_token, dir)
    }

    #[tokio::test]
    async fn capabilities_requires_admin_auth_and_defaults_to_ce() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode, header};
        use tower::ServiceExt;

        let (app, token, _dir) =
            desktop_gateway_and_token(crate::HttpAppExtensions::default()).await;

        let anon = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

        let authed = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/capabilities")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authed.status(), StatusCode::OK);
        let body = to_bytes(authed.into_body(), 1024 * 64).await.unwrap();
        let payload: crate::CapabilitiesResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.edition, crate::Edition::Ce);
        assert_eq!(payload.license_tier, None);
        assert_eq!(payload.license_status, None);
        assert!(payload.capabilities.is_empty());
        assert_eq!(payload.degraded_reason, None);
        assert_eq!(payload.auth_mode, "single_admin");
    }

    #[tokio::test]
    async fn extensions_mount_commercial_capabilities_extra_routes_and_authorizer() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode, header};
        use tower::ServiceExt;

        let extensions = crate::HttpAppExtensions::default()
            .with_capability_provider(Arc::new(StubCapabilityProvider {
                snapshot: crate::CapabilitiesSnapshot {
                    edition: crate::Edition::Commercial,
                    license_tier: Some(crate::LicenseTier::Enterprise),
                    license_status: Some(crate::LicenseStatus::Valid),
                    capabilities: vec![
                        "multi_admin".to_string(),
                        "license_status".to_string(),
                        "multi_admin".to_string(),
                    ],
                    degraded_reason: None,
                },
            }))
            .with_admin_authorizer(Arc::new(DenyingAuthorizer {
                denied_prefix: "/ee/restricted",
            }))
            .with_extra_admin_routes(
                Router::new()
                    .route("/ee/license", get(|| async { "ok-license" }))
                    .route("/ee/restricted", get(|| async { "secret" })),
            )
            .with_extra_public_routes(
                Router::new().route("/ee/public-ping", get(|| async { "pong" })),
            );

        let (app, token, _dir) = desktop_gateway_and_token(extensions).await;

        // Capabilities sorted & deduplicated
        let caps_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/capabilities")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(caps_res.status(), StatusCode::OK);
        let caps: crate::CapabilitiesResponse =
            serde_json::from_slice(&to_bytes(caps_res.into_body(), 1024 * 64).await.unwrap())
                .unwrap();
        assert_eq!(caps.edition, crate::Edition::Commercial);
        assert_eq!(caps.license_tier, Some(crate::LicenseTier::Enterprise));
        assert_eq!(caps.license_status, Some(crate::LicenseStatus::Valid));
        assert_eq!(caps.capabilities, vec!["license_status", "multi_admin"]);

        // Extra admin route requires auth
        let unauth_extra = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ee/license")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth_extra.status(), StatusCode::UNAUTHORIZED);

        // Allowed extra admin route returns 200
        let ok_extra = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ee/license")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_extra.status(), StatusCode::OK);

        // Authorizer denial returns 403 fail-closed
        let denied_extra = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ee/restricted")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_extra.status(), StatusCode::FORBIDDEN);
        let denied_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(denied_extra.into_body(), 1024 * 64).await.unwrap())
                .unwrap();
        assert_eq!(denied_json["error"]["code"], "ee_role_denied");

        // Extra public route works without auth
        let public_res = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ee/public-ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(public_res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ce_excludes_external_and_http_mcp_management_routes_and_scoped_endpoints() {
        use axum::body::Body;
        use axum::http::{Method, Request, StatusCode, header};
        use tower::ServiceExt;

        let (app, token, _dir) =
            desktop_gateway_and_token(crate::HttpAppExtensions::default()).await;

        // Commercial MCP management endpoints do not exist in CE
        for path in [
            "/api/v1/mcp/overview",
            "/api/v1/mcp/servers",
            "/api/v1/mcp/groups",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "path {path} must be 404 in CE"
            );
        }

        // Create an API key with built-in MCP enabled to verify /mcp works while scoped subpaths 404
        let create_key = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/api-keys")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-stravia-csrf", "1")
                    .body(Body::from(
                        r#"{"name":"ce-mcp-key","mcp_access_enabled":true,"transparent_injection_enabled":false,"transparent_injections":[],"model_ids":[]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_key.status(), StatusCode::OK);
        let key_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(create_key.into_body(), 1024 * 64)
                .await
                .unwrap(),
        )
        .unwrap();
        let api_key = key_json["data"]["key"].as_str().expect("api key token");

        for scoped_path in [
            "/mcp/category/external",
            "/mcp/category/http",
            "/mcp/server/demo",
            "/mcp/group/demo",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(scoped_path)
                        .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::ACCEPT, "application/json, text/event-stream")
                        .body(Body::from(
                            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "scoped MCP path {scoped_path} must be 404 in CE"
            );
        }

        // Built-in /mcp endpoint stays operational in CE
        let init_res = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header(header::HOST, "127.0.0.1")
                    .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
                    ))
                    .unwrap(),
                )
            .await
            .unwrap();
        let init_status = init_res.status();
        let init_body = axum::body::to_bytes(init_res.into_body(), 1024 * 64)
            .await
            .unwrap();
        assert_eq!(
            init_status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&init_body)
        );
    }
}
