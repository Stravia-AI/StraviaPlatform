use std::sync::Arc;

use tauri::State;

use crate::desktop_gateway_runtime::{DesktopGatewayRuntime, DesktopPortState, PortOperationError};

#[tauri::command]
pub fn get_server_port(runtime: State<'_, Arc<DesktopGatewayRuntime>>) -> u16 {
    runtime.current_port()
}

#[tauri::command]
pub async fn get_desktop_port_state(
    runtime: State<'_, Arc<DesktopGatewayRuntime>>,
) -> Result<DesktopPortState, String> {
    Ok(runtime.snapshot().await)
}

#[tauri::command]
pub async fn set_desktop_fixed_port(
    port: u32,
    runtime: State<'_, Arc<DesktopGatewayRuntime>>,
) -> Result<DesktopPortState, PortOperationError> {
    runtime.configure_fixed_port(port).await
}

#[tauri::command]
pub async fn recheck_desktop_fixed_port(
    runtime: State<'_, Arc<DesktopGatewayRuntime>>,
) -> Result<DesktopPortState, PortOperationError> {
    runtime.recheck_fixed_port().await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use stravia_core::{
        Gateway,
        config::GatewayConfig,
        storage::{DynStorage, MemoryStorage},
    };
    use stravia_server::{HttpAppConfig, build_http_app, desktop_origins};

    use crate::desktop_gateway_runtime::{
        DesktopGatewayRuntime, PortOwner, PortOwnerResolver, PortPreferenceLoad,
        PortPreferenceStore,
    };

    struct MissingStore;

    impl PortPreferenceStore for MissingStore {
        fn load(&self) -> Result<PortPreferenceLoad, String> {
            Ok(PortPreferenceLoad::Missing)
        }

        fn save(&self, _port: u16) -> Result<(), String> {
            Ok(())
        }
    }

    struct NoOwners;

    impl PortOwnerResolver for NoOwners {
        fn resolve(&self, _port: u16) -> Result<Vec<PortOwner>, String> {
            Ok(vec![])
        }
    }

    async fn start_desktop_runtime() -> Arc<DesktopGatewayRuntime> {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let (gateway, _log_rx) = Gateway::from_storage(GatewayConfig::default(), storage)
            .await
            .expect("desktop gateway should initialize from memory storage");
        let cors_origins = desktop_origins();
        let app = build_http_app(
            gateway,
            HttpAppConfig {
                admin_token: None,
                admin_cors_origins: cors_origins.clone(),
                proxy_cors_origins: cors_origins,
                serve_embedded_webui: false,
            },
        );

        DesktopGatewayRuntime::start(app, Arc::new(MissingStore), Arc::new(NoOwners))
            .await
            .expect("desktop runtime should bind an OS-assigned port")
    }

    #[tokio::test]
    async fn desktop_discovery_returns_distinct_loopback_servers_with_http_management() {
        let first = start_desktop_runtime().await;
        let second = start_desktop_runtime().await;
        let first_port = first.current_port();
        let second_port = second.current_port();

        assert_ne!(first_port, second_port);

        let client = reqwest::Client::new();
        for port in [first_port, second_port] {
            let status = client
                .get(format!("http://127.0.0.1:{port}/api/v1/status"))
                .send()
                .await
                .expect("desktop status request");
            assert_eq!(status.status(), reqwest::StatusCode::OK);

            let models = client
                .get(format!("http://127.0.0.1:{port}/v1/models"))
                .send()
                .await
                .expect("desktop proxy models request");
            assert_eq!(models.status(), reqwest::StatusCode::OK);
        }

        let packaged_origin = client
            .request(
                reqwest::Method::OPTIONS,
                format!("http://127.0.0.1:{first_port}/api/v1/providers"),
            )
            .header(reqwest::header::ORIGIN, "tauri://localhost")
            .header("access-control-request-method", "POST")
            .send()
            .await
            .expect("desktop CORS preflight");
        assert_eq!(packaged_origin.status(), reqwest::StatusCode::OK);
        assert_eq!(
            packaged_origin
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("tauri://localhost")
        );

        #[cfg(debug_assertions)]
        {
            let development_origin = client
                .request(
                    reqwest::Method::OPTIONS,
                    format!("http://127.0.0.1:{first_port}/api/v1/providers"),
                )
                .header(reqwest::header::ORIGIN, "http://localhost:5173")
                .header("access-control-request-method", "POST")
                .send()
                .await
                .expect("desktop development CORS preflight");
            assert_eq!(development_origin.status(), reqwest::StatusCode::OK);
            assert_eq!(
                development_origin
                    .headers()
                    .get("access-control-allow-origin")
                    .and_then(|value| value.to_str().ok()),
                Some("http://localhost:5173")
            );
        }

        #[cfg(not(debug_assertions))]
        {
            let development_origin = client
                .request(
                    reqwest::Method::OPTIONS,
                    format!("http://127.0.0.1:{first_port}/api/v1/providers"),
                )
                .header(reqwest::header::ORIGIN, "http://localhost:5173")
                .header("access-control-request-method", "POST")
                .send()
                .await
                .expect("desktop development CORS preflight");
            assert_eq!(development_origin.status(), reqwest::StatusCode::OK);
            assert!(
                development_origin
                    .headers()
                    .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .is_none()
            );
        }

        first.shutdown().await.expect("first runtime should stop");
        second.shutdown().await.expect("second runtime should stop");
    }
}
