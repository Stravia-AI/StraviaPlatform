use std::sync::Arc;

use stravia_core::Gateway;
use stravia_core::admin::provider_allowance::ProviderAllowanceSnapshot;
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

#[tauri::command]
pub async fn list_provider_allowances(
    gateway: State<'_, Gateway>,
) -> Result<Vec<ProviderAllowanceSnapshot>, String> {
    list_provider_allowances_for_gateway(&gateway).await
}

#[tauri::command]
pub async fn refresh_provider_allowances(
    gateway: State<'_, Gateway>,
) -> Result<Vec<ProviderAllowanceSnapshot>, String> {
    refresh_provider_allowances_for_gateway(&gateway).await
}

#[tauri::command]
pub async fn refresh_provider_allowance(
    provider_id: String,
    gateway: State<'_, Gateway>,
) -> Result<Option<ProviderAllowanceSnapshot>, String> {
    refresh_provider_allowance_for_gateway(&gateway, &provider_id).await
}

async fn list_provider_allowances_for_gateway(
    gateway: &Gateway,
) -> Result<Vec<ProviderAllowanceSnapshot>, String> {
    gateway
        .admin()
        .list_provider_allowances()
        .await
        .map_err(|_| "failed to load provider allowances".to_string())
}

async fn refresh_provider_allowances_for_gateway(
    gateway: &Gateway,
) -> Result<Vec<ProviderAllowanceSnapshot>, String> {
    gateway
        .admin()
        .refresh_provider_allowances()
        .await
        .map_err(|_| "failed to refresh provider allowances".to_string())
}

async fn refresh_provider_allowance_for_gateway(
    gateway: &Gateway,
    provider_id: &str,
) -> Result<Option<ProviderAllowanceSnapshot>, String> {
    gateway
        .admin()
        .refresh_provider_allowance(provider_id)
        .await
        .map_err(|_| "failed to refresh provider allowance".to_string())
}

#[cfg(feature = "desktop-e2e")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderWebviewSmokeResult {
    ready: bool,
    final_url: String,
    contains_smoke_marker: bool,
}

#[cfg(feature = "desktop-e2e")]
#[tauri::command]
pub async fn render_webview_smoke(
    factory: State<'_, Arc<crate::webview_renderer::TauriPageRendererFactory>>,
) -> Result<RenderWebviewSmokeResult, String> {
    use stravia_web_access::renderer::{
        PageRendererConfig, PageRendererFactory, RenderRequest, RenderRequestPolicy,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const HTML: &str = "<!doctype html><html><body><script>document.body.innerHTML = '<h1 id=\"rendered\">Native WebView Rendered</h1>';</script></body></html>";
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let fixture = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await?;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                    HTML.len()
                )
                .as_bytes(),
            )
            .await?;
        Ok::<_, std::io::Error>(())
    });
    let renderer = factory.build(PageRendererConfig::direct())?;
    let fixture_url = format!("http://{address}/");
    let page = renderer
        .render(RenderRequest {
            url: &fixture_url,
            preflight_url: None,
            ready_selector: "#rendered",
            timeout: std::time::Duration::from_secs(15),
            request_policy: RenderRequestPolicy::Unrestricted,
        })
        .await
        .map_err(|error| error.to_string())?;
    fixture
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    Ok(RenderWebviewSmokeResult {
        ready: page.ready,
        final_url: page.url,
        contains_smoke_marker: page.html.contains("Native WebView Rendered"),
    })
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

    use super::{
        list_provider_allowances_for_gateway, refresh_provider_allowance_for_gateway,
        refresh_provider_allowances_for_gateway,
    };
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
    async fn allowance_commands_preserve_the_core_result_shape() {
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let (gateway, _log_rx) = Gateway::from_storage(GatewayConfig::default(), storage)
            .await
            .expect("desktop gateway should initialize from memory storage");

        assert_eq!(
            list_provider_allowances_for_gateway(&gateway)
                .await
                .expect("list allowances"),
            gateway
                .admin()
                .list_provider_allowances()
                .await
                .expect("core list allowances")
        );
        assert_eq!(
            refresh_provider_allowances_for_gateway(&gateway)
                .await
                .expect("refresh allowances"),
            gateway
                .admin()
                .refresh_provider_allowances()
                .await
                .expect("core refresh allowances")
        );
        assert_eq!(
            refresh_provider_allowance_for_gateway(&gateway, "missing")
                .await
                .expect("refresh missing provider"),
            None
        );
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
