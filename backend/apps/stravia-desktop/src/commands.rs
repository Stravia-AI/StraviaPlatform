use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use stravia_core::Gateway;
use stravia_core::admin::provider_allowance::ProviderAllowanceSnapshot;
use stravia_core::connect_client_apply::{
    ConnectClientApplyError, ConnectClientApplyInput, ConnectClientApplyPlan,
    PlannedConnectClientFile, plan_connect_client_apply,
};
use tauri::State;

use crate::desktop_gateway_runtime::{DesktopGatewayRuntime, DesktopPortState, PortOperationError};

struct StagedConnectClientFile {
    target: PathBuf,
    temporary: Option<tempfile::TempPath>,
    backup: Option<PathBuf>,
}

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
pub fn plan_connect_client(
    input: ConnectClientApplyInput,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    load_connect_client_plan(&input)
}

#[tauri::command]
pub fn apply_connect_client(
    input: ConnectClientApplyInput,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let plan = load_connect_client_plan(&input)?;
    apply_planned_files(&plan.files)?;
    Ok(plan)
}

fn load_connect_client_plan(
    input: &ConnectClientApplyInput,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let environment = std::env::vars().collect::<BTreeMap<_, _>>();
    let unresolved = plan_connect_client_apply(input, &environment, &BTreeMap::new())?;
    let mut existing_files = BTreeMap::new();
    for file in &unresolved.files {
        validate_global_target(file)?;
        let path = PathBuf::from(&file.path);
        match fs::read(&path) {
            Ok(bytes) => {
                existing_files.insert(path, bytes);
            }
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
            Err(cause) => {
                return Err(io_error(
                    "read_error",
                    format!("Failed to read Connect Client Global Config: {cause}"),
                    &path,
                ));
            }
        }
    }
    plan_connect_client_apply(input, &environment, &existing_files)
}

fn validate_global_target(file: &PlannedConnectClientFile) -> Result<(), ConnectClientApplyError> {
    let root = PathBuf::from(&file.root);
    let target = PathBuf::from(&file.path);
    if !root.is_absolute() || !target.is_absolute() || !target.starts_with(&root) {
        return Err(io_error(
            "path_escape",
            "Connect Client Apply target escapes the resolved global directory",
            &target,
        ));
    }

    if root.exists() {
        let canonical_root = fs::canonicalize(&root).map_err(|cause| {
            io_error(
                "path_error",
                format!("Failed to resolve Connect Client global directory: {cause}"),
                &root,
            )
        })?;
        let existing = if target.exists() {
            Some(target.as_path())
        } else {
            target.parent().filter(|parent| parent.exists())
        };
        if let Some(existing) = existing {
            let canonical_target = fs::canonicalize(existing).map_err(|cause| {
                io_error(
                    "path_error",
                    format!("Failed to resolve Connect Client target: {cause}"),
                    existing,
                )
            })?;
            if !canonical_target.starts_with(&canonical_root) {
                return Err(io_error(
                    "path_escape",
                    "Connect Client Apply target escapes the resolved global directory",
                    &target,
                ));
            }
        }
    }
    Ok(())
}

fn apply_planned_files(files: &[PlannedConnectClientFile]) -> Result<(), ConnectClientApplyError> {
    let mut staged = Vec::with_capacity(files.len());
    for file in files {
        validate_global_target(file)?;
        let target = PathBuf::from(&file.path);
        let parent = target.parent().ok_or_else(|| {
            io_error(
                "path_error",
                "Connect Client Global Config has no parent directory",
                &target,
            )
        })?;
        fs::create_dir_all(parent).map_err(|cause| {
            io_error(
                "write_error",
                format!("Failed to create Connect Client global directory: {cause}"),
                parent,
            )
        })?;
        validate_global_target(file)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".stravia-stage-")
            .tempfile_in(parent)
            .map_err(|cause| {
                io_error(
                    "write_error",
                    format!("Failed to stage Connect Client Global Config: {cause}"),
                    &target,
                )
            })?;
        temporary.write_all(&file.bytes).map_err(|cause| {
            io_error(
                "write_error",
                format!("Failed to stage Connect Client Global Config: {cause}"),
                &target,
            )
        })?;
        temporary.flush().map_err(|cause| {
            io_error(
                "write_error",
                format!("Failed to flush Connect Client Global Config: {cause}"),
                &target,
            )
        })?;
        staged.push(StagedConnectClientFile {
            target,
            temporary: Some(temporary.into_temp_path()),
            backup: None,
        });
    }

    for index in 0..staged.len() {
        if !staged[index].target.exists() {
            continue;
        }
        let parent = staged[index]
            .target
            .parent()
            .expect("validated target has a parent");
        let backup_file = match tempfile::Builder::new()
            .prefix(".stravia-backup-")
            .tempfile_in(parent)
        {
            Ok(file) => file,
            Err(cause) => {
                rollback_backups(&staged[..index]);
                return Err(io_error(
                    "write_error",
                    format!("Failed to reserve Connect Client config backup: {cause}"),
                    &staged[index].target,
                ));
            }
        };
        let backup = backup_file.into_temp_path();
        let backup_path = backup.to_path_buf();
        if let Err(cause) = backup.close() {
            rollback_backups(&staged[..index]);
            return Err(io_error(
                "write_error",
                format!("Failed to prepare Connect Client config backup: {cause}"),
                &staged[index].target,
            ));
        }
        if let Err(cause) = fs::rename(&staged[index].target, &backup_path) {
            rollback_backups(&staged[..index]);
            return Err(io_error(
                "write_error",
                format!("Failed to back up Connect Client Global Config: {cause}"),
                &staged[index].target,
            ));
        }
        staged[index].backup = Some(backup_path);
    }

    for index in 0..staged.len() {
        let temporary = staged[index]
            .temporary
            .take()
            .expect("each staged file is persisted once");
        if let Err(cause) = temporary.persist_noclobber(&staged[index].target) {
            for committed in &staged[..index] {
                let _ = fs::remove_file(&committed.target);
            }
            rollback_backups(&staged);
            return Err(io_error(
                "write_error",
                format!(
                    "Failed to replace Connect Client Global Config: {}",
                    cause.error
                ),
                &staged[index].target,
            ));
        }
    }

    for file in staged {
        if let Some(backup) = file.backup {
            let _ = fs::remove_file(backup);
        }
    }
    Ok(())
}

fn rollback_backups(files: &[StagedConnectClientFile]) {
    for file in files.iter().rev() {
        if let Some(backup) = &file.backup {
            let _ = fs::rename(backup, &file.target);
        }
    }
}

fn io_error(
    code: &'static str,
    message: impl Into<String>,
    path: &Path,
) -> ConnectClientApplyError {
    ConnectClientApplyError {
        code,
        message: message.into(),
        path: Some(path.display().to_string()),
    }
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
