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

mod admin_routes;
mod oauth_callback;

pub const DEFAULT_PORT: u16 = 23471;

#[cfg(all(feature = "embed-webui", not(debug_assertions)))]
#[derive(RustEmbed)]
#[folder = "../../../frontend/stravia-webui/dist/"]
struct WebUiAssets;

#[derive(Clone, Debug)]
pub struct HttpAppConfig {
    pub admin_token: Option<String>,
    pub admin_cors_origins: Vec<String>,
    pub proxy_cors_origins: Vec<String>,
    pub serve_embedded_webui: bool,
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
    let admin_router = admin_routes::create_router(gateway.clone(), config.admin_token)
        .layer(build_admin_cors_layer(&config.admin_cors_origins));
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
            return app.fallback(serve_embedded_webui_or_not_found);
        }
    }

    app.fallback(api_not_found)
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
        let result = axum::serve(listener, app)
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
    matches!(path, "/api" | "/v1" | "/v1beta")
        || path.starts_with("/api/")
        || path.starts_with("/v1/")
        || path.starts_with("/v1beta/")
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
    build_cors_layer(
        origins,
        [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ],
        [
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::HeaderName::from_static("x-api-key"),
            header::HeaderName::from_static("anthropic-version"),
        ],
    )
}

fn build_proxy_cors_layer(origins: &[String]) -> CorsLayer {
    build_cors_layer(
        origins,
        [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ],
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
            header::HeaderName::from_static("x-upload-token"),
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
}
