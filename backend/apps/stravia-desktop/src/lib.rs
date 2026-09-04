mod commands;
mod desktop_gateway_runtime;

use std::sync::Arc;

use desktop_gateway_runtime::{
    DesktopGatewayRuntime, PortSwitchPublisher, SystemPortOwnerResolver, desktop_port_store,
    desktop_runtime_dir,
};
use stravia_core::{Gateway, config::GatewayConfig, logging};
use stravia_server::{HttpAppConfig, build_http_app, desktop_origins};
use tauri::{
    Manager,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};

pub(crate) struct DesktopTray {
    tray: TrayIcon,
    copy_url: MenuItem<tauri::Wry>,
}

impl DesktopTray {
    pub(crate) fn sync_port(&self, port: u16) -> tauri::Result<()> {
        self.tray
            .set_tooltip(Some(format!("Stravia AI Gateway — :{port}")))?;
        self.copy_url
            .set_text(format!("Copy Proxy URL (:{port})"))?;
        Ok(())
    }
}

struct TauriPortSwitchPublisher {
    app: tauri::AppHandle,
}

impl PortSwitchPublisher for TauriPortSwitchPublisher {
    fn publish(&self, port: u16) -> Result<(), String> {
        let tray = self
            .app
            .try_state::<DesktopTray>()
            .ok_or_else(|| "desktop tray state is unavailable".to_string())?;
        tray.sync_port(port).map_err(|error| error.to_string())?;
        let window = self
            .app
            .get_webview_window("main")
            .ok_or_else(|| "main WebView is unavailable".to_string())?;
        window
            .eval("window.location.reload()")
            .map_err(|error| error.to_string())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter("stravia=debug,tower_http=debug")
        .init();

    let builder = tauri::Builder::default();
    #[cfg(feature = "desktop-e2e")]
    let builder = builder
        .plugin(tauri_plugin_wdio::init())
        .plugin(tauri_plugin_wdio_webdriver::init());

    builder
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_store::Builder::default().build())
        .setup(|app| {
            let data_dir = desktop_runtime_dir(app);
            let (gateway, log_rx) = tauri::async_runtime::block_on(Gateway::new(GatewayConfig {
                data_dir: data_dir.clone(),
                #[cfg(debug_assertions)]
                wire_capture_dir: std::env::var_os("STRAVIA_WIRE_CAPTURE_DIR")
                    .map(std::path::PathBuf::from),
                ..Default::default()
            }))?;

            let cors_origins = desktop_origins();
            let app_router = build_http_app(
                gateway.clone(),
                HttpAppConfig {
                    admin_token: None,
                    admin_cors_origins: cors_origins.clone(),
                    proxy_cors_origins: cors_origins,
                    serve_embedded_webui: false,
                },
            );
            let port_store = desktop_port_store(app, &data_dir)?;
            let runtime = tauri::async_runtime::block_on(DesktopGatewayRuntime::start(
                app_router,
                port_store,
                Arc::new(SystemPortOwnerResolver),
            ))?;
            let server_port = runtime.current_port();

            let storage_for_logs = gateway.storage.clone();
            tauri::async_runtime::spawn(async move {
                logging::run_collector(log_rx, storage_for_logs).await;
            });

            app.manage(gateway);
            app.manage(runtime.clone());
            app.manage(setup_tray(app, server_port)?);
            runtime.set_switch_publisher(Arc::new(TauriPortSwitchPublisher {
                app: app.handle().clone(),
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_server_port,
            commands::get_desktop_port_state,
            commands::set_desktop_fixed_port,
            commands::recheck_desktop_fixed_port,
            commands::list_provider_allowances,
            commands::refresh_provider_allowances,
            commands::refresh_provider_allowance,
        ])
        .build(tauri::generate_context!())
        .expect("error while running Stravia application")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = &event
            {
                if !*has_visible_windows {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }

            if let tauri::RunEvent::ExitRequested { .. } = &event
                && let Some(runtime) = app.try_state::<Arc<DesktopGatewayRuntime>>()
            {
                runtime.request_shutdown();
            }

            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

fn setup_tray(
    app: &tauri::App,
    server_port: u16,
) -> Result<DesktopTray, Box<dyn std::error::Error>> {
    let show = MenuItem::with_id(app, "show", "Show Dashboard", true, None::<&str>)?;
    let copy_url = MenuItem::with_id(
        app,
        "copy_url",
        format!("Copy Proxy URL (:{server_port})"),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit Stravia", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &copy_url, &quit])?;

    let tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip(format!("Stravia AI Gateway — :{server_port}"))
        .menu(&menu)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "copy_url" => {
                if let (Some(window), Some(runtime)) = (
                    app.get_webview_window("main"),
                    app.try_state::<Arc<DesktopGatewayRuntime>>(),
                ) {
                    let port = runtime.current_port();
                    let _ = window.eval(format!(
                        "navigator.clipboard.writeText('http://127.0.0.1:{port}')"
                    ));
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(DesktopTray { tray, copy_url })
}
