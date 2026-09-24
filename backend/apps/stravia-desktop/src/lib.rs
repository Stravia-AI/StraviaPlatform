mod commands;
mod desktop_gateway_runtime;
mod desktop_icons;
mod product_update;
mod startup;
mod window_geometry;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use desktop_gateway_runtime::{
    DesktopGatewayRuntime, PortSwitchPublisher, SystemPortOwnerResolver, autostart_root_argument,
    desktop_preference_store, desktop_root_override, desktop_runtime_dir, launched_in_background,
};
use startup::{StartupController, StartupDiagnostics, StartupStage, safe_error_details};
use stravia_core::{
    Gateway, admin::identity::AdminAuth, config::GatewayConfig, data_paths::DataPaths,
};
use stravia_server::{AdminMode, HttpAppConfig, build_http_app, desktop_origins};
use tauri::{
    Manager,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};
use window_geometry::WindowGeometry;

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

    let early_diagnostics = StartupDiagnostics::initialize(Some(
        std::env::temp_dir()
            .join("stravia-desktop")
            .join("early-diagnostics"),
    ))
    .diagnostics;
    let early_previous_failure = early_diagnostics.previous_failure();
    // 单实例插件可能正常终止第二次启动；只有真正的构建失败才保留早期失败标记。
    early_diagnostics.mark_clean();
    let active_diagnostics = Arc::new(parking_lot::Mutex::new(Arc::clone(&early_diagnostics)));
    let setup_diagnostics = Arc::clone(&active_diagnostics);
    let root_override = desktop_root_override();
    let restart_env = restart_environment(&root_override);
    let builder = tauri::Builder::default().manage(restart_env);
    #[cfg(feature = "desktop-e2e")]
    let builder = builder
        .plugin(tauri_plugin_wdio::init())
        .plugin(tauri_plugin_wdio_webdriver::init());

    let application = builder
        .on_window_event(|window, event| {
            if let Some(geometry) = window.try_state::<WindowGeometry>() {
                geometry.record_event(window, event);
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if let Some(geometry) = window.try_state::<WindowGeometry>()
                    && let Err(error) = geometry.save(window)
                {
                    tracing::warn!(%error, "failed to save desktop window geometry");
                }
                api.prevent_close();
                let should_hide = window
                    .try_state::<Arc<StartupController>>()
                    .is_some_and(|startup| startup.close_should_hide());
                if should_hide {
                    if let Err(error) = window.hide() {
                        tracing::debug!(%error, "failed to hide main window");
                    }
                } else {
                    window.app_handle().exit(0);
                }
            }
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::default().build())
        .setup(move |app| {
            let diagnostics_init = StartupDiagnostics::initialize(app.path().app_log_dir().ok());
            {
                let mut active = setup_diagnostics.lock();
                active.mark_clean();
                *active = Arc::clone(&diagnostics_init.diagnostics);
            }
            let startup = Arc::new(StartupController::new_with_previous_failure(
                Arc::clone(&diagnostics_init.diagnostics),
                early_previous_failure,
            ));
            if !app.manage(Arc::clone(&startup)) {
                fatal_startup(
                    &diagnostics_init.diagnostics,
                    "Stravia could not initialize desktop startup state. Restart the application.",
                );
            }

            match WindowGeometry::open(app.handle()) {
                Ok(geometry) => {
                    app.manage(geometry);
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to open desktop window geometry store");
                }
            }
            let preferred_shell_root = root_override
                .as_ref()
                .ok()
                .and_then(|root| desktop_runtime_dir(app.handle(), root.as_deref()).ok());
            let used_shell_fallback = match build_main_window(
                app.handle(),
                preferred_shell_root.as_deref(),
                !launched_in_background(),
            ) {
                Ok(used_fallback) => used_fallback,
                Err(_) => fatal_startup(
                    &diagnostics_init.diagnostics,
                    "Stravia could not create its desktop window. Check the installed desktop runtime and permissions for its WebView data directory, then restart.",
                ),
            };
            diagnostics_init.diagnostics.install_panic_hook();
            if let Some(details) = diagnostics_init.warning {
                startup.warning(app.handle(), "diagnostics", details);
            }
            if used_shell_fallback {
                startup.warning(
                    app.handle(),
                    "diagnostics",
                    "The primary recovery WebView directory was unavailable; temporary recovery storage is in use.",
                );
            }

            let app_handle = app.handle().clone();
            let startup_task = Arc::clone(&startup);
            tauri::async_runtime::spawn(async move {
                initialize_desktop(app_handle, root_override, startup_task).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            startup::get_desktop_startup_state,
            startup::restart_desktop,
            startup::exit_desktop,
            startup::open_desktop_logs,
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
        .build(tauri::generate_context!());

    let application = match application {
        Ok(application) => application,
        Err(error) => {
            let diagnostics = Arc::clone(&active_diagnostics.lock());
            let details = safe_desktop_build_error(&error);
            fatal_startup(&diagnostics, details);
        }
    };

    application.run(|app, event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen {
            has_visible_windows,
            ..
        } = &event
            && !*has_visible_windows
        {
            show_main_window(app);
        }

        if let tauri::RunEvent::ExitRequested { .. } = &event
            && let (Some(geometry), Some(window)) = (
                app.try_state::<WindowGeometry>(),
                app.get_webview_window("main"),
            )
            && let Err(error) = geometry.save(&window.as_ref().window())
        {
            tracing::warn!(%error, "failed to save desktop window geometry");
        }

        if let tauri::RunEvent::ExitRequested { api, code, .. } = &event
            && let Some(startup) = app.try_state::<Arc<StartupController>>()
            && !startup.shutdown_complete()
        {
            if *code == Some(tauri::RESTART_EXIT_CODE) {
                if startup.begin_shutdown() {
                    tauri::async_runtime::block_on(shutdown_desktop(app, startup.inner()));
                    startup.finish_shutdown();
                } else {
                    tauri::async_runtime::block_on(startup.wait_for_shutdown());
                }
            } else {
                api.prevent_exit();
                if startup.begin_shutdown() {
                    let app = app.clone();
                    let startup = Arc::clone(startup.inner());
                    let exit_code = code.unwrap_or(0);
                    tauri::async_runtime::spawn(async move {
                        shutdown_desktop(&app, &startup).await;
                        startup.finish_shutdown();
                        app.exit(exit_code);
                    });
                }
            }
        }
    });
}

pub(crate) async fn shutdown_desktop(app: &tauri::AppHandle, startup: &StartupController) {
    startup.wait_for_initialization().await;
    if let Some(runtime) = app.try_state::<Arc<DesktopGatewayRuntime>>()
        && runtime.shutdown().await.is_err()
    {
        tracing::warn!("desktop HTTP runtime did not stop cleanly");
        startup.diagnostics().record(
            "WARN",
            "http",
            "desktop HTTP runtime did not stop cleanly during shutdown",
        );
    }
    if let Some(session) = app.try_state::<commands::NativeAdminSession>()
        && session.revoke().await.is_err()
    {
        tracing::warn!("native administrator session revocation failed during shutdown");
        startup.diagnostics().record(
            "WARN",
            "session",
            "native administrator session revocation failed during shutdown",
        );
    }
    if let Some(gateway) = app.try_state::<Gateway>() {
        gateway.shutdown().await;
    }
    startup.mark_clean_shutdown();
}

struct InitializationDone(Arc<StartupController>);

impl Drop for InitializationDone {
    fn drop(&mut self) {
        self.0.finish_initialization();
    }
}

fn safe_desktop_build_error(error: &tauri::Error) -> &'static str {
    let category = error.to_string().to_ascii_lowercase();
    if category.contains("webview") || category.contains("webview2") {
        "The desktop WebView could not be initialized."
    } else if category.contains("plugin") {
        "A required desktop integration could not be initialized."
    } else if category.contains("config") {
        "The desktop application configuration is invalid."
    } else {
        "The desktop application could not be initialized."
    }
}

fn restart_environment(root_override: &anyhow::Result<Option<PathBuf>>) -> tauri::Env {
    let mut environment = tauri::Env::default();
    let mut args = environment.args_os.into_iter();
    let mut pinned = args.next().into_iter().collect::<Vec<_>>();
    while let Some(argument) = args.next() {
        if argument == "--background" {
            continue;
        }
        if root_override.as_ref().is_ok_and(Option::is_some) && argument == "--data-dir" {
            args.next();
            continue;
        }
        if root_override.as_ref().is_ok_and(Option::is_some)
            && argument
                .to_str()
                .is_some_and(|argument| argument.starts_with("--data-dir="))
        {
            continue;
        }
        pinned.push(argument);
    }
    if let Ok(Some(root)) = root_override {
        pinned.push("--data-dir".into());
        pinned.push(root.as_os_str().to_owned());
    }
    environment.args_os = pinned;
    environment
}

async fn initialize_desktop(
    app: tauri::AppHandle,
    root_override: anyhow::Result<Option<PathBuf>>,
    startup: Arc<StartupController>,
) {
    let _done = InitializationDone(Arc::clone(&startup));
    let mut stage = StartupStage::DataDirectory;
    let mut gateway = None;
    let mut session = None;
    let mut runtime = None;
    let mut installed = false;

    let result: anyhow::Result<()> = async {
        startup.stage(&app, stage);
        let root_override = root_override?;
        let data_dir = desktop_runtime_dir(&app, root_override.as_deref())?;
        let paths = DataPaths::new(&data_dir);
        paths.prepare()?;
        let lock = paths.lock()?;
        if !app.manage(lock) || !app.manage(data_dir.clone()) {
            anyhow::bail!("desktop data state was already installed");
        }
        if startup.is_shutting_down() {
            return Ok(());
        }

        match autostart_root_argument(&data_dir) {
            Ok(autostart_root) => {
                match app.plugin(
                    tauri_plugin_autostart::Builder::new()
                        .args(["--data-dir", autostart_root.as_str(), "--background"])
                        .build(),
                ) {
                    Ok(()) => {
                        #[cfg(not(feature = "desktop-e2e"))]
                        if let Some(autostart) =
                            app.try_state::<tauri_plugin_autostart::AutoLaunchManager>()
                        {
                            match autostart.is_enabled() {
                                Ok(true) => {
                                    if autostart.enable().is_err() {
                                        startup.warning(
                                            &app,
                                            "autostart",
                                            "Launch at login could not be refreshed; Stravia will continue without changing it.",
                                        );
                                    }
                                }
                                Ok(false) => {}
                                Err(_) => startup.warning(
                                    &app,
                                    "autostart",
                                    "Launch at login status is unavailable; Stravia will continue normally.",
                                ),
                            }
                        }
                    }
                    Err(_) => startup.warning(
                        &app,
                        "autostart",
                        "Launch at login integration is unavailable; Stravia will continue normally.",
                    ),
                }
            }
            Err(_) => startup.warning(
                &app,
                "autostart",
                "Launch at login cannot use the selected data directory; Stravia will continue normally.",
            ),
        }
        if startup.is_shutting_down() {
            return Ok(());
        }

        stage = StartupStage::Gateway;
        startup.stage(&app, stage);
        gateway = Some(
            Gateway::new(GatewayConfig {
                data_dir: data_dir.clone(),
                product_update_download_supported: true,
                catalog_base_url: Some(
                    stravia_core::provider_catalog::CATALOG_BASE_URL.to_owned(),
                ),
                catalog_background_refresh: true,
                ..Default::default()
            })
            .await?,
        );
        if startup.is_shutting_down() {
            return Ok(());
        }
        let gateway_ref = gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway initialization state is missing"))?;
        #[cfg(feature = "desktop-e2e")]
        gateway_ref.storage.settings().set(
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
        ).await?;

        stage = StartupStage::Session;
        startup.stage(&app, stage);
        let admin_auth = AdminAuth::new(gateway_ref.storage.clone());
        session = Some(commands::NativeAdminSession::initialize(admin_auth.clone()).await?);
        if startup.is_shutting_down() {
            return Ok(());
        }

        stage = StartupStage::Http;
        startup.stage(&app, stage);
        let cors_origins = desktop_origins();
        let app_router = build_http_app(
            Gateway::clone(gateway_ref),
            HttpAppConfig {
                admin_auth,
                admin_mode: AdminMode::Desktop,
                admin_entry: Default::default(),
                desktop_cors_origins: cors_origins.clone(),
                proxy_cors_origins: cors_origins,
                serve_embedded_webui: false,
            },
        );
        let port_store = desktop_preference_store(&app, &data_dir)?;
        let silent_start = launched_in_background()
            && port_store
                .load()
                .map(|preferences| preferences.silent_start)
                .unwrap_or(false);
        if !silent_start {
            show_main_window(&app);
        }
        if startup.is_shutting_down() {
            return Ok(());
        }
        runtime = Some(
            DesktopGatewayRuntime::start(
                app_router,
                Arc::clone(&port_store),
                Arc::new(SystemPortOwnerResolver),
            )
            .await?,
        );
        if startup.is_shutting_down() {
            return Ok(());
        }
        let runtime_ref = runtime
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("HTTP runtime initialization state is missing"))?;
        let server_port = runtime_ref.current_port();
        if gateway_ref
            .storage
            .settings()
            .get("artifact_settings")
            .await?
            .is_none()
        {
            let settings = stravia_core::agent::artifact::ArtifactSettings {
                client_base_url: format!("http://127.0.0.1:{server_port}"),
                ..Default::default()
            };
            gateway_ref
                .admin()
                .set_setting("artifact_settings", &serde_json::to_string(&settings)?)
                .await?;
        }
        if startup.is_shutting_down() {
            return Ok(());
        }

        stage = StartupStage::Desktop;
        startup.stage(&app, stage);
        let locale = gateway_ref
            .storage
            .settings()
            .get("ui_locale")
            .await
            .ok()
            .flatten();
        if startup.is_shutting_down() {
            return Ok(());
        }
        anyhow::ensure!(
            app.try_state::<Arc<dyn desktop_gateway_runtime::DesktopPreferenceStore>>()
                .is_none()
                && app.try_state::<Gateway>().is_none()
                && app.try_state::<commands::NativeAdminSession>().is_none()
                && app.try_state::<Arc<DesktopGatewayRuntime>>().is_none()
                && app
                    .try_state::<product_update::DesktopUpdateState>()
                    .is_none(),
            "desktop business state was already installed"
        );
        let native_session = session
            .take()
            .ok_or_else(|| anyhow::anyhow!("native session state is missing"))?;
        let port_installed = app.manage(Arc::clone(&port_store));
        let gateway_installed = app.manage(Gateway::clone(gateway_ref));
        let runtime_installed = app.manage(Arc::clone(runtime_ref));
        let update_installed = app.manage(product_update::DesktopUpdateState::default());
        let session_installed = app.manage(native_session);
        anyhow::ensure!(
            port_installed
                && gateway_installed
                && runtime_installed
                && update_installed
                && session_installed,
            "desktop business state installation raced with another initializer"
        );
        installed = true;
        if startup.is_shutting_down() {
            return Ok(());
        }

        match setup_tray(&app, locale.as_deref()) {
            Ok(tray) => {
                if app.manage(tray) {
                    startup.set_tray_available(true);
                } else {
                    startup.warning(
                        &app,
                        "tray",
                        "The system tray is unavailable; closing the window will exit Stravia.",
                    );
                    show_main_window(&app);
                }
            }
            Err(_) => {
                startup.warning(
                    &app,
                    "tray",
                    "The system tray is unavailable; closing the window will exit Stravia.",
                );
                show_main_window(&app);
            }
        }
        if desktop_icons::setup(&app).is_err() {
            startup.warning(
                &app,
                "icons",
                "Some desktop icons could not be applied; Stravia will continue normally.",
            );
        }
        runtime_ref.set_switch_publisher(Arc::new(TauriPortSwitchPublisher { app: app.clone() }));
        startup.ready(&app);
        Ok(())
    }
    .await;

    if !installed {
        cleanup_partial_startup(
            runtime.as_ref(),
            session.as_ref(),
            gateway.as_ref(),
            &startup,
        )
        .await;
    }
    if let Err(error) = result {
        let details = safe_error_details(stage, &error);
        tracing::error!(code = stage.code(), "desktop startup failed");
        startup.fail(&app, stage, details);
        show_main_window(&app);
    } else if startup.is_shutting_down() && !installed {
        startup.mark_clean_shutdown();
    }
}

async fn cleanup_partial_startup(
    runtime: Option<&Arc<DesktopGatewayRuntime>>,
    session: Option<&commands::NativeAdminSession>,
    gateway: Option<&Gateway>,
    startup: &StartupController,
) {
    if let Some(runtime) = runtime
        && runtime.shutdown().await.is_err()
    {
        tracing::warn!("partial desktop HTTP runtime did not stop cleanly");
        startup.diagnostics().record(
            "WARN",
            "http",
            "partial desktop HTTP runtime did not stop cleanly",
        );
    }
    if let Some(session) = session
        && session.revoke().await.is_err()
    {
        tracing::warn!("partial native administrator session revocation failed");
        startup.diagnostics().record(
            "WARN",
            "session",
            "partial native administrator session revocation failed",
        );
    }
    if let Some(gateway) = gateway {
        gateway.shutdown().await;
    }
}

fn setup_tray(
    app: &tauri::AppHandle,
    locale: Option<&str>,
) -> Result<DesktopTray, Box<dyn std::error::Error>> {
    // 托盘标签沿用业务初始化读取到的持久化界面语言。
    let locale = locale.map_or(TrayLocale::EnUs, TrayLocale::parse);

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

fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(target_os = "windows")]
fn shell_profile_key(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;

    fnv1a(
        path.as_os_str()
            .encode_wide()
            .flat_map(|unit| unit.to_le_bytes()),
    )
}

#[cfg(unix)]
fn shell_profile_key(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;

    fnv1a(path.as_os_str().as_bytes().iter().copied())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn shell_profile_key(path: &Path) -> u64 {
    fnv1a(path.to_string_lossy().bytes())
}

fn usable_existing_webview_profile(data_dir: &Path) -> Option<PathBuf> {
    let profile = DataPaths::new(data_dir).desktop_webview();
    if !profile.is_dir() {
        return None;
    }
    let probe = profile.join(format!(".startup-write-probe-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .ok()?;
    drop(file);
    if std::fs::remove_file(&probe).is_err() {
        eprintln!("Stravia could not verify its existing WebView profile safely.");
        return None;
    }
    Some(profile)
}

fn independent_webview_profile(
    app: &tauri::AppHandle,
    data_dir: Option<&Path>,
    profile_name: &str,
) -> Result<(PathBuf, bool), std::io::Error> {
    let candidates = [
        app.path().app_local_data_dir().ok(),
        app.path().app_config_dir().ok(),
    ];
    for root in candidates.into_iter().flatten() {
        if data_dir.is_some_and(|data_dir| root.starts_with(data_dir)) {
            continue;
        }
        let profile = root.join("recovery-webview").join(profile_name);
        if std::fs::create_dir_all(&profile).is_ok() {
            return Ok((profile, false));
        }
    }
    let temporary = std::env::temp_dir()
        .join("stravia-desktop-shell")
        .join(profile_name);
    std::fs::create_dir_all(&temporary)?;
    Ok((temporary, true))
}

/// Build the recovery-capable WebView before business data is opened. An
/// existing writable managed profile is preserved. Otherwise an independently
/// isolated shell profile keeps recovery available without creating or changing
/// the selected business root before its layout has been validated.
fn build_main_window(
    app: &tauri::AppHandle,
    preferred_data_dir: Option<&Path>,
    visible: bool,
) -> Result<bool, anyhow::Error> {
    let mut window_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or_else(|| anyhow::anyhow!("main WebView configuration is missing"))?
        .clone();
    if let Some(geometry) = app.try_state::<WindowGeometry>()
        && let Err(error) = geometry.configure(app, &mut window_config)
    {
        tracing::warn!(%error, "failed to configure desktop window geometry");
    }
    let profile_key = shell_profile_key(preferred_data_dir.unwrap_or_else(|| Path::new("default")));
    let profile_name = format!("{profile_key:016x}");
    let (webview_dir, used_fallback) =
        if let Some(profile) = preferred_data_dir.and_then(usable_existing_webview_profile) {
            (profile, false)
        } else {
            independent_webview_profile(app, preferred_data_dir, &profile_name)?
        };
    // 原生窗口必须带着位置创建；Windows 可能在 WebView 初始化时重置创建后的坐标。
    let window = tauri::WebviewWindowBuilder::from_config(app, &window_config)?
        .data_directory(webview_dir)
        .visible(visible)
        .focused(visible)
        .build()?;
    if app.try_state::<WindowGeometry>().is_none()
        && let Err(error) = window.center()
    {
        tracing::warn!(%error, "failed to center default desktop window");
    }
    if visible {
        window.show()?;
        if let Some(geometry) = app.try_state::<WindowGeometry>() {
            if let Err(error) = geometry.position_after_show(&window.as_ref().window()) {
                tracing::warn!(%error, "failed to position main window");
            }
            geometry.start_tracking(&window.as_ref().window());
        }
        if let Err(error) = window.set_focus() {
            tracing::debug!(%error, "failed to focus main window");
        }
    }
    // Wry 的创建消息可能只记录底层 WebView 错误，仍返回逻辑句柄。
    // 查询原生窗口确保恢复界面确实存在；隐藏启动的 false 也是成功结果。
    window.is_visible()?;
    Ok(used_fallback)
}

fn fatal_startup(diagnostics: &StartupDiagnostics, details: &str) -> ! {
    diagnostics.mark_incomplete();
    diagnostics.record("ERROR", "desktop", details);
    let message = diagnostics.path().map_or_else(
        || format!("{details}\n\nNo writable desktop startup log is available."),
        |path| format!("{details}\n\nStartup log: {}", path.display()),
    );
    show_native_fatal_error(&message);
    std::process::exit(1);
}

fn show_main_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        show_native_fatal_error(
            "Stravia could not open its recovery window. Review the desktop startup log and restart the application.",
        );
        app.exit(1);
        return;
    };
    if let Err(error) = window.show() {
        tracing::debug!(%error, "failed to show main window");
        return;
    }
    if let Some(geometry) = app.try_state::<WindowGeometry>() {
        if let Err(error) = geometry.position_after_show(&window.as_ref().window()) {
            tracing::warn!(%error, "failed to position main window");
        }
        geometry.start_tracking(&window.as_ref().window());
    }
    if let Err(error) = window.set_focus() {
        tracing::debug!(%error, "failed to focus main window");
    }
}

#[cfg(target_os = "windows")]
fn show_native_fatal_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let title = "Stravia\0".encode_utf16().collect::<Vec<_>>();
    let body = format!("{message}\0").encode_utf16().collect::<Vec<_>>();
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
    if result == 0 {
        tracing::error!(
            os_error = std::io::Error::last_os_error().raw_os_error(),
            "native startup error dialog could not be shown"
        );
        eprintln!("{message}");
    }
}

#[cfg(target_os = "linux")]
fn show_native_fatal_error(message: &str) {
    use std::ffi::{CString, c_char, c_void};

    #[link(name = "gtk-3")]
    unsafe extern "C" {
        fn gtk_init_check(argc: *mut i32, argv: *mut *mut *mut c_char) -> i32;
        fn gtk_message_dialog_new(
            parent: *mut c_void,
            flags: i32,
            message_type: i32,
            buttons: i32,
            message_format: *const c_char,
            ...
        ) -> *mut c_void;
        fn gtk_window_set_title(window: *mut c_void, title: *const c_char);
        fn gtk_dialog_run(dialog: *mut c_void) -> i32;
        fn gtk_widget_destroy(widget: *mut c_void);
    }

    let title = c"Stravia";
    let format = c"%s";
    let Ok(body) = CString::new(message.replace('\0', " ")) else {
        eprintln!("Stravia could not create its desktop window.");
        return;
    };
    let shown = unsafe {
        if gtk_init_check(std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
            false
        } else {
            // GtkMessageType::Error = 3 and GtkButtonsType::Close = 2.
            let dialog = gtk_message_dialog_new(
                std::ptr::null_mut(),
                1,
                3,
                2,
                format.as_ptr(),
                body.as_ptr(),
            );
            if dialog.is_null() {
                false
            } else {
                gtk_window_set_title(dialog, title.as_ptr());
                gtk_dialog_run(dialog);
                gtk_widget_destroy(dialog);
                true
            }
        }
    };
    if !shown {
        eprintln!("Stravia could not create its desktop window.");
    }
}

#[cfg(target_os = "macos")]
fn show_native_fatal_error(message: &str) {
    use std::process::Command;

    let script = "on run argv\ndisplay dialog (item 1 of argv) with title \"Stravia\" buttons {\"OK\"} default button \"OK\" with icon stop\nend run";
    if !Command::new("/usr/bin/osascript")
        .args(["-e", script, message])
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("Stravia could not create its desktop window.");
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn show_native_fatal_error(_message: &str) {
    eprintln!("Stravia could not create its desktop window.");
}
