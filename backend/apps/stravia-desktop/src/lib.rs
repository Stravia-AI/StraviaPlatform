mod commands;
mod desktop_gateway_runtime;
mod desktop_icons;
mod product_update;

use std::sync::Arc;

use desktop_gateway_runtime::{
    DesktopGatewayRuntime, PortSwitchPublisher, SystemPortOwnerResolver, autostart_root_argument,
    desktop_preference_store, desktop_root_override, desktop_runtime_dir, launched_in_background,
};
use stravia_core::{
    Gateway, admin::identity::AdminAuth, config::GatewayConfig, data_paths::DataPaths,
};
use stravia_server::{AdminMode, HttpAppConfig, build_http_app, desktop_origins};
use tauri::{
    Manager,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};

// 托盘语言标签与 WebUI messages/{locale}.json 的语言列表保持一致。
#[derive(Clone, Copy)]
enum TrayLocale {
    EnUs,
    ZhCn,
}

impl TrayLocale {
    fn parse(value: &str) -> Self {
        if value.to_ascii_lowercase().starts_with("zh") {
            Self::ZhCn
        } else {
            Self::EnUs
        }
    }

    fn show_dashboard(self) -> &'static str {
        match self {
            Self::EnUs => "Show Dashboard",
            Self::ZhCn => "打开控制台",
        }
    }

    fn quit_stravia(self) -> &'static str {
        match self {
            Self::EnUs => "Quit Stravia",
            Self::ZhCn => "退出 Stravia",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::EnUs => "Stravia Agent infra",
            Self::ZhCn => "Stravia 智能体基础设施",
        }
    }
}

pub(crate) struct DesktopTray {
    tray: TrayIcon,
    show: MenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
}

impl DesktopTray {
    pub(crate) fn set_locale(&self, locale: &str) -> tauri::Result<()> {
        let locale = TrayLocale::parse(locale);
        self.show.set_text(locale.show_dashboard())?;
        self.quit.set_text(locale.quit_stravia())?;
        self.tray.set_tooltip(Some(locale.tooltip()))?;
        Ok(())
    }
}

struct TauriPortSwitchPublisher {
    app: tauri::AppHandle,
}

impl PortSwitchPublisher for TauriPortSwitchPublisher {
    fn publish(&self, _port: u16) -> Result<(), String> {
        // A silent-started app may have no window to reload.
        if let Some(window) = self.app.get_webview_window("main") {
            window
                .eval("window.location.reload()")
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter("stravia=debug,tower_http=debug")
        .init();

    let root_override = desktop_root_override();
    let mut restart_env = tauri::Env::default();
    if let Ok(Some(root)) = &root_override {
        // Tauri process/updater restart uses this managed argument vector. Append an
        // absolute root after removing the original override, including relative paths.
        let mut args = restart_env.args_os.into_iter();
        let mut pinned = args.next().into_iter().collect::<Vec<_>>();
        while let Some(arg) = args.next() {
            if arg == "--data-dir" {
                args.next();
            } else if !arg
                .to_str()
                .is_some_and(|arg| arg.starts_with("--data-dir="))
            {
                pinned.push(arg);
            }
        }
        pinned.push("--data-dir".into());
        pinned.push(root.as_os_str().to_owned());
        restart_env.args_os = pinned;
    }
    let builder = tauri::Builder::default().manage(restart_env);
    #[cfg(feature = "desktop-e2e")]
    let builder = builder
        .plugin(tauri_plugin_wdio::init())
        .plugin(tauri_plugin_wdio_webdriver::init());

    builder
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(error) = window.hide() {
                    tracing::debug!(%error, "failed to hide main window");
                }
            }
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::default().build())
        .setup(move |app| {
            let prepared = (|| {
                let root_override = root_override?;
                let data_dir = desktop_runtime_dir(app, root_override.as_deref())?;
                let paths = DataPaths::new(&data_dir);
                paths.prepare()?;
                let lock = paths.lock()?;
                Ok::<_, anyhow::Error>((data_dir, lock))
            })();
            let (data_dir, lock) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    tracing::error!(%error, "desktop data directory startup failed");
                    eprintln!("Stravia could not open its data directory: {error:#}");
                    return Err(error.into());
                }
            };
            app.manage(lock);
            app.manage(data_dir.clone());
            let autostart_root = autostart_root_argument(&data_dir)?;
            app.handle().plugin(
                tauri_plugin_autostart::Builder::new()
                    // --background 标记自启来源；是否隐藏窗口由 silent_start 偏好决定。
                    .args(["--data-dir", autostart_root.as_str(), "--background"])
                    .build(),
            )?;
            #[cfg(not(feature = "desktop-e2e"))]
            {
                let autostart = app.state::<tauri_plugin_autostart::AutoLaunchManager>();
                if autostart.is_enabled()? {
                    autostart.enable()?;
                }
            }
            let gateway = tauri::async_runtime::block_on(Gateway::new(GatewayConfig {
                data_dir: data_dir.clone(),
                product_update_download_supported: true,
                ..Default::default()
            }))?;
            #[cfg(feature = "desktop-e2e")]
            tauri::async_runtime::block_on(gateway.storage.settings().set(
                "product_update_state",
                &serde_json::json!({
                    "last_success_at": "2026-09-05T00:00:00Z",
                    "last_failure": null,
                    "available_update": {
                        "version": "9.9.9",
                        "published_at": "2026-09-04T00:00:00Z",
                        "release_url": "https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v9.9.9",
                        "manifest_url": "https://github.com/Stravia-AI/StraviaPlatform/releases/download/v9.9.9/stravia-updater.json",
                        "download_available": true,
                        "download_error": null
                    }
                })
                .to_string(),
            ))?;

            let admin_auth = AdminAuth::new(gateway.storage.clone());
            let native_admin_session = tauri::async_runtime::block_on(
                commands::NativeAdminSession::initialize(admin_auth.clone()),
            )?;

            let cors_origins = desktop_origins();
            let app_router = build_http_app(
                gateway.clone(),
                HttpAppConfig {
                    admin_auth,
                    admin_mode: AdminMode::Desktop,
                    admin_entry: Default::default(),
                    desktop_cors_origins: cors_origins.clone(),
                    proxy_cors_origins: cors_origins,
                    serve_embedded_webui: false,
                },
            );
            let port_store = desktop_preference_store(app, &data_dir)?;
            let silent_start = launched_in_background()
                && port_store
                    .load()
                    .map(|preferences| preferences.silent_start)
                    .unwrap_or(false);
            app.manage(port_store.clone());
            let runtime = tauri::async_runtime::block_on(DesktopGatewayRuntime::start(
                app_router,
                port_store,
                Arc::new(SystemPortOwnerResolver),
            ))?;
            let server_port = runtime.current_port();
            // The native frontend is not an HTTP entry. Use its actual local transport
            // once, then preserve the administrator's complete saved address.
            tauri::async_runtime::block_on(async {
                if gateway.storage.settings().get("artifact_settings").await?.is_none() {
                    let settings = stravia_core::agent::artifact::ArtifactSettings {
                        client_base_url: format!("http://127.0.0.1:{server_port}"),
                        ..Default::default()
                    };
                    gateway.admin().set_setting("artifact_settings", &serde_json::to_string(&settings)?).await?;
                }
                Ok::<(), anyhow::Error>(())
            })?;

            app.manage(gateway);
            app.manage(native_admin_session);
            app.manage(runtime.clone());
            app.manage(product_update::DesktopUpdateState::default());
            // dragDropEnabled:false 在窗口配置里：Windows 上 Tauri 默认 drop handler 会换掉 WebView2 的 HTML5 DnD。
            if !silent_start {
                build_main_window(app.handle())?;
            }
            app.manage(setup_tray(app)?);
            desktop_icons::setup(app.handle())?;
            runtime.set_switch_publisher(Arc::new(TauriPortSwitchPublisher {
                app: app.handle().clone(),
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_admin_session,
            commands::get_server_port,
            commands::get_desktop_port_state,
            commands::set_desktop_fixed_port,
            commands::recheck_desktop_fixed_port,
            commands::set_desktop_external_access,
            commands::get_desktop_client_settings,
            commands::set_desktop_launch_at_login,
            commands::set_desktop_silent_start,
            commands::plan_connect_client,
            commands::apply_connect_client,
            commands::set_desktop_locale,
            product_update::get_desktop_update_state,
            product_update::download_product_update,
            product_update::install_product_update,
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
                    show_main_window(app);
                }
            }

            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                if let Some(session) = app.try_state::<commands::NativeAdminSession>()
                    && tauri::async_runtime::block_on(session.revoke()).is_err()
                {
                    api.prevent_exit();
                    tracing::error!("exit cancelled: failed to revoke the native admin session");
                    return;
                }
                if let Some(runtime) = app.try_state::<Arc<DesktopGatewayRuntime>>() {
                    runtime.request_shutdown();
                }
                if let Some(gateway) = app.try_state::<Gateway>() {
                    tauri::async_runtime::block_on(gateway.shutdown());
                }
            }

            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

fn setup_tray(app: &tauri::App) -> Result<DesktopTray, Box<dyn std::error::Error>> {
    // 托盘先于 WebView 就绪；读取上次持久化的界面语言，避免先英文再切换。
    let locale = app
        .try_state::<Gateway>()
        .and_then(|gateway| {
            tauri::async_runtime::block_on(gateway.storage.settings().get("ui_locale"))
                .ok()
                .flatten()
        })
        .as_deref()
        .map_or(TrayLocale::EnUs, TrayLocale::parse);

    let show = MenuItem::with_id(app, "show", locale.show_dashboard(), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", locale.quit_stravia(), true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    #[cfg(target_os = "windows")]
    remove_menu_check_gutter(&menu);

    let tray = TrayIconBuilder::new()
        .icon(desktop_icons::tray_image())
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip(locale.tooltip())
        .menu(&menu)
        // 左键只唤出主窗口；默认行为会连菜单一起弹出。
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                show_main_window(app);
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(DesktopTray { tray, show, quit })
}

// Windows 弹出菜单默认在文字左侧预留勾选/图标列；菜单项没有图标，去掉这段空隙。
#[cfg(target_os = "windows")]
fn remove_menu_check_gutter(menu: &Menu<tauri::Wry>) {
    use tauri::menu::ContextMenu;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetMenuInfo, MENUINFO, MIM_STYLE, MNS_CHECKORBMP, MNS_NOCHECK, SetMenuInfo,
    };

    let Ok(hmenu) = menu.hpopupmenu() else {
        return;
    };
    unsafe {
        let mut info: MENUINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MENUINFO>() as u32;
        info.fMask = MIM_STYLE;
        if GetMenuInfo(hmenu as _, &mut info) == 0 {
            return;
        }
        info.dwStyle = (info.dwStyle & !MNS_CHECKORBMP) | MNS_NOCHECK;
        SetMenuInfo(hmenu as _, &info);
    }
}

/// Build the main WebView from its declared config; the WebView data directory
/// lives under the managed data root.
fn build_main_window(app: &tauri::AppHandle) -> Result<(), anyhow::Error> {
    let window_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or_else(|| anyhow::anyhow!("main WebView configuration is missing"))?
        .clone();
    let data_dir = app
        .try_state::<std::path::PathBuf>()
        .ok_or_else(|| anyhow::anyhow!("desktop data directory state is missing"))?;
    let webview_dir = DataPaths::new(data_dir.inner()).desktop_webview();
    std::fs::create_dir_all(&webview_dir)?;
    tauri::WebviewWindowBuilder::from_config(app, &window_config)?
        .data_directory(webview_dir)
        .build()?;
    Ok(())
}

/// Show the dashboard window, creating it first when the app launched without
/// one (silent start leaves only the tray).
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window.show() {
            tracing::debug!(%error, "failed to show main window");
        }
        if let Err(error) = window.set_focus() {
            tracing::debug!(%error, "failed to focus main window");
        }
        return;
    }
    if let Err(error) = build_main_window(app) {
        tracing::error!(%error, "failed to open the main window");
    }
}
