use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::get,
};
use futures::stream;
use serde::Serialize;
use stravia_core::startup_progress::{StartupProgress, observe_startup};
use tokio::sync::{RwLock, watch};
use tower::ServiceExt;

use crate::{PreparedServerApp, ServerStartupConfig, prepare_server_app};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum StartupStatus {
    Starting,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct StartupSnapshot {
    status: StartupStatus,
    progress: Option<StartupProgress>,
}

struct Runtime {
    current: RwLock<Router>,
    snapshot: watch::Sender<StartupSnapshot>,
}

#[derive(Clone)]
struct BootState {
    snapshot: watch::Sender<StartupSnapshot>,
    serve_embedded_webui: bool,
}

/// 在数据库初始化前提供受现有入口策略保护的启动页和只读进度。
/// 业务路由在准备完成后原子切换；失败时不开放任何管理或代理操作。
pub struct StartupHttpApp {
    runtime: Arc<Runtime>,
    progress_routes: Router,
    config: Option<ServerStartupConfig>,
    prepared: Option<PreparedServerApp>,
}

impl StartupHttpApp {
    pub fn new(config: ServerStartupConfig) -> Self {
        let (snapshot, _) = watch::channel(StartupSnapshot {
            status: StartupStatus::Starting,
            progress: None,
        });
        let boot = config.admin_entry.protect(
            Router::new()
                .route("/healthz", get(unavailable))
                .route("/readyz", get(unavailable))
                .fallback(boot_request)
                .with_state(BootState {
                    snapshot: snapshot.clone(),
                    serve_embedded_webui: config.serve_embedded_webui,
                }),
        );
        let runtime = Arc::new(Runtime {
            current: RwLock::new(boot),
            snapshot,
        });
        let progress_routes = config.admin_entry.protect(
            Router::new()
                .route("/api/v1/startup", get(startup_snapshot))
                .route("/api/v1/startup/events", get(events))
                .with_state(Arc::clone(&runtime)),
        );
        Self {
            runtime,
            progress_routes,
            config: Some(config),
            prepared: None,
        }
    }

    pub fn router(&self) -> Router {
        self.progress_routes.clone().merge(
            Router::new()
                .fallback(dispatch)
                .with_state(Arc::clone(&self.runtime)),
        )
    }

    /// 在调用方任务内执行初始化，保证 migration 与启动阶段共用观察范围。
    /// 返回首次设置令牌（如有）；错误原样交给宿主，网页只接收失败状态。
    pub async fn prepare(&mut self) -> anyhow::Result<Option<String>> {
        let config = self
            .config
            .take()
            .ok_or_else(|| anyhow::anyhow!("startup preparation already attempted"))?;
        let runtime = Arc::clone(&self.runtime);
        let result = observe_startup(
            move |progress| {
                runtime.snapshot.send_replace(StartupSnapshot {
                    status: StartupStatus::Starting,
                    progress: Some(progress),
                });
            },
            prepare_server_app(config),
        )
        .await;
        match result {
            Ok(mut prepared) => {
                *self.runtime.current.write().await = prepared.app.clone();
                let setup_token = prepared.setup_token.take();
                self.prepared = Some(prepared);
                self.runtime.snapshot.send_replace(StartupSnapshot {
                    status: StartupStatus::Ready,
                    progress: None,
                });
                Ok(setup_token)
            }
            Err(error) => {
                let progress = self.runtime.snapshot.borrow().progress;
                self.runtime.snapshot.send_replace(StartupSnapshot {
                    status: StartupStatus::Failed,
                    progress,
                });
                Err(error)
            }
        }
    }

    /// HTTP 监听器排空后由宿主显式等待关闭；准备失败时无需业务清理。
    pub async fn shutdown(self) {
        if let Some(prepared) = self.prepared {
            prepared.shutdown().await;
        }
    }
}

async fn startup_snapshot(State(runtime): State<Arc<Runtime>>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(*runtime.snapshot.borrow()),
    )
}

async fn events(State(runtime): State<Arc<Runtime>>) -> impl IntoResponse {
    let receiver = runtime.snapshot.subscribe();
    let stream = stream::unfold(
        (receiver, true, false),
        |(mut receiver, first, finished)| async move {
            if finished || (!first && receiver.changed().await.is_err()) {
                return None;
            }
            let snapshot = *receiver.borrow_and_update();
            let finished = snapshot.status != StartupStatus::Starting;
            let event = Event::default()
                .event("startup")
                .json_data(snapshot)
                .expect("startup snapshot contains only serializable fields");
            Some((Ok::<_, Infallible>(event), (receiver, false, finished)))
        },
    );
    (
        [(header::CACHE_CONTROL, "no-store")],
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
}

async fn dispatch(State(runtime): State<Arc<Runtime>>, request: Request) -> Response {
    let router = runtime.current.read().await.clone();
    router
        .oneshot(request)
        .await
        .unwrap_or_else(|never| match never {})
}

async fn unavailable(State(boot): State<BootState>) -> impl IntoResponse {
    let status = boot.snapshot.borrow().status;
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "status": status })),
    )
}

async fn boot_request(State(boot): State<BootState>, request: Request) -> Response {
    let path = request.uri().path();
    // 启动期保留协议命名空间的 503，不让 SPA fallback 冒充 API 响应。
    if path == "/api"
        || path.starts_with("/api/")
        || path == "/v1"
        || path.starts_with("/v1/")
        || path == "/v1beta"
        || path.starts_with("/v1beta/")
        || path == "/mcp"
        || path.starts_with("/mcp/")
    {
        return unavailable(State(boot)).await.into_response();
    }
    #[cfg(all(feature = "embed-webui", not(debug_assertions)))]
    if boot.serve_embedded_webui {
        return crate::serve_embedded_webui_or_not_found(request.uri().clone()).await;
    }
    let _ = boot.serve_embedded_webui;
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AdminEntryPolicy;
    use anyhow::Context;
    use stravia_core::{config::GatewayConfig, data_paths::DataPaths};

    async fn first_event(response: &mut reqwest::Response) -> anyhow::Result<String> {
        let mut bytes = Vec::new();
        while !bytes.windows(2).any(|window| window == b"\n\n") {
            bytes.extend_from_slice(
                &response
                    .chunk()
                    .await?
                    .context("missing startup snapshot")?,
            );
        }
        Ok(String::from_utf8(bytes)?)
    }

    fn configured_sqlite(root: &std::path::Path, policy: AdminEntryPolicy) -> ServerStartupConfig {
        ServerStartupConfig {
            config_path: root.join("server.toml"),
            gateway: GatewayConfig {
                data_dir: root.to_owned(),
                ..Default::default()
            },
            admin_entry: policy,
            proxy_cors_origins: Vec::new(),
            serve_embedded_webui: false,
        }
    }

    #[tokio::test]
    async fn listener_precedes_migration_and_preserves_entry_policy_through_cutover()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        DataPaths::new(root.path()).prepare()?;
        std::fs::write(
            root.path().join("server.toml"),
            "[database]\nbackend = \"sqlite\"\n",
        )?;
        let policy = AdminEntryPolicy::new(&["http://allowed.test".to_owned()], &[])?;
        let mut startup = StartupHttpApp::new(configured_sqlite(root.path(), policy));
        let server = crate::start_http_server(("127.0.0.1", 0), startup.router()).await?;
        let base = format!("http://{}", server.local_addr());
        let client = reqwest::Client::new();

        for path in ["/api/v1/startup", "/api/v1/startup/events"] {
            let denied = client.get(format!("{base}{path}")).send().await?;
            assert_eq!(denied.status(), StatusCode::FORBIDDEN);
            let spoofed = client
                .get(format!("{base}{path}"))
                .header("x-forwarded-host", "allowed.test")
                .header("x-forwarded-proto", "http")
                .send()
                .await?;
            assert_eq!(spoofed.status(), StatusCode::FORBIDDEN);
        }
        let snapshot = client
            .get(format!("{base}/api/v1/startup"))
            .header("host", "allowed.test")
            .send()
            .await?;
        assert_eq!(snapshot.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            snapshot.json::<serde_json::Value>().await?["status"],
            "starting"
        );
        for path in [
            "/api/v1/auth/state",
            "/api/v1/setup/claim",
            "/v1/responses",
            "/mcp",
        ] {
            let response = client
                .post(format!("{base}{path}"))
                .header("host", "allowed.test")
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(
            client.get(format!("{base}/readyz")).send().await?.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            client.get(format!("{base}/healthz")).send().await?.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut events = client
            .get(format!("{base}/api/v1/startup/events"))
            .header("host", "allowed.test")
            .send()
            .await?;
        assert_eq!(events.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert!(
            first_event(&mut events)
                .await?
                .contains("\"status\":\"starting\"")
        );

        let token = startup.prepare().await?;
        assert!(
            token.is_some(),
            "a migrated database without an administrator must enter setup"
        );
        let events = events.text().await?;
        assert!(events.contains("\"status\":\"ready\""));
        let snapshot = client
            .get(format!("{base}/api/v1/startup"))
            .header("host", "allowed.test")
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;
        assert_eq!(
            snapshot,
            serde_json::json!({"status":"ready", "progress":null})
        );
        let auth = client
            .get(format!("{base}/api/v1/auth/state"))
            .header("host", "allowed.test")
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;
        assert_eq!(auth["mode"], "setup");
        assert_eq!(
            client.get(format!("{base}/healthz")).send().await?.status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{base}/api/v1/startup"))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .get(format!("{base}/api/v1/auth/state"))
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
        server.shutdown().await?;
        startup.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn failed_database_startup_stays_unavailable_without_exposing_error_details()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let paths = DataPaths::new(root.path());
        paths.prepare()?;
        std::fs::create_dir_all(paths.database_dir())?;
        std::fs::write(
            paths.database(),
            b"preserve-invalid-database: sensitive fixture",
        )?;
        std::fs::write(
            root.path().join("server.toml"),
            "[database]\nbackend = \"sqlite\"\n",
        )?;
        let policy = AdminEntryPolicy::default();
        let mut startup = StartupHttpApp::new(configured_sqlite(root.path(), policy));
        let server = crate::start_http_server(("127.0.0.1", 0), startup.router()).await?;
        let base = format!("http://{}", server.local_addr());
        let events = reqwest::get(format!("{base}/api/v1/startup/events")).await?;
        assert!(startup.prepare().await.is_err());
        let events = events.text().await?;
        assert!(events.contains("\"status\":\"failed\""));
        assert!(!events.contains("sensitive fixture"));
        assert!(!events.contains(&root.path().to_string_lossy().to_string()));
        for path in [
            "/api/v1/auth/state",
            "/api/v1/setup/claim",
            "/v1/responses",
            "/readyz",
        ] {
            assert_eq!(
                reqwest::get(format!("{base}{path}")).await?.status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        assert_eq!(
            std::fs::read(paths.database())?,
            b"preserve-invalid-database: sensitive fixture"
        );
        server.shutdown().await?;
        startup.shutdown().await;
        Ok(())
    }
}
