fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            #[cfg(feature = "desktop-e2e")]
            "get_desktop_icon_theme",
            "get_admin_session",
            "get_server_port",
            "get_desktop_port_state",
            "set_desktop_fixed_port",
            "recheck_desktop_fixed_port",
            "plan_connect_client",
            "apply_connect_client",
            "list_provider_allowances",
            "refresh_provider_allowances",
            "refresh_provider_allowance",
            "get_desktop_update_state",
            "download_product_update",
            "install_product_update",
        ]),
    ))
    .expect("failed to build the Tauri application");
}
