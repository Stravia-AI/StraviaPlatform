use std::{
    io,
    sync::{
        Arc, Condvar, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};

use axum::{Router, routing::get};
use tokio::sync::{Notify, oneshot};

use super::{
    BindScope, BindingFailureKind, DEFAULT_PORT, DesktopGatewayRuntime, DesktopPortMode,
    DesktopPreferenceStore, DesktopPreferences, OwnerLookupStatus, PortOperationErrorCode,
    PortOwner, PortOwnerResolver, PortPreferenceLoad, PortSwitchPublisher, TestBind,
    args_include_background, e2e_data_dir, select_root_override, start_http_server,
};

#[test]
fn desktop_e2e_ignores_cli_and_environment_roots() {
    let isolated = select_root_override(true, [], None).unwrap().unwrap();
    let selected = select_root_override(
        true,
        ["--data-dir".into(), "production-root".into()],
        Some("production-env".into()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(selected, isolated);
    assert_ne!(
        selected,
        std::env::current_dir().unwrap().join("production-root")
    );
}

#[test]
fn explicit_desktop_root_overrides_environment_and_preserves_unicode() {
    let selected = select_root_override(
        false,
        ["--data-dir".into(), "中文 空格".into()],
        Some("environment-root".into()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(selected, std::env::current_dir().unwrap().join("中文 空格"));
    let environment = select_root_override(false, [], Some("environment-root".into()))
        .unwrap()
        .unwrap();
    assert_eq!(
        environment,
        std::env::current_dir().unwrap().join("environment-root")
    );
}

#[test]
fn malformed_desktop_override_does_not_fall_back_to_environment() {
    assert!(
        select_root_override(
            false,
            ["--data-dir".into()],
            Some("environment-root".into()),
        )
        .is_err()
    );
    assert!(select_root_override(false, ["--data-dir=".into()], None).is_err());
    assert!(select_root_override(false, [], Some("".into())).is_err());
}

#[test]
fn desktop_e2e_run_root_must_be_a_dedicated_temporary_directory() {
    let root = tempfile::Builder::new()
        .prefix("stravia-desktop-e2e-")
        .tempdir()
        .unwrap();
    assert_eq!(
        e2e_data_dir(root.path()).unwrap(),
        root.path().canonicalize().unwrap().join("data")
    );
    assert!(e2e_data_dir(std::path::Path::new("stravia-desktop-e2e-relative")).is_err());
    let wrong_name = tempfile::tempdir().unwrap();
    assert!(e2e_data_dir(wrong_name.path()).is_err());
    let nested = root.path().join("stravia-desktop-e2e-nested");
    std::fs::create_dir(&nested).unwrap();
    assert!(e2e_data_dir(&nested).is_err());
}

#[cfg(windows)]
#[test]
fn windows_autostart_quotes_unicode_roots_and_trailing_separators() {
    assert_eq!(
        super::autostart_root_argument(std::path::Path::new("C:\\中文 空格\\")).unwrap(),
        "\"C:\\中文 空格\\\\\"",
    );
}

struct TestStore {
    load: Result<DesktopPreferences, String>,
    saved: AtomicU16,
    saved_external: AtomicBool,
    fail_save: bool,
}

impl TestStore {
    fn new(load: PortPreferenceLoad) -> Self {
        Self::with_preferences(DesktopPreferences {
            fixed_port: load,
            ..Default::default()
        })
    }

    fn with_preferences(preferences: DesktopPreferences) -> Self {
        Self {
            load: Ok(preferences),
            saved: AtomicU16::new(0),
            saved_external: AtomicBool::new(false),
            fail_save: false,
        }
    }

    fn failing_read(message: &str) -> Self {
        Self {
            load: Err(message.to_string()),
            saved: AtomicU16::new(0),
            saved_external: AtomicBool::new(false),
            fail_save: false,
        }
    }

    fn failing_save(load: PortPreferenceLoad) -> Self {
        Self {
            load: Ok(DesktopPreferences {
                fixed_port: load,
                ..Default::default()
            }),
            saved: AtomicU16::new(0),
            saved_external: AtomicBool::new(false),
            fail_save: true,
        }
    }
}

impl DesktopPreferenceStore for TestStore {
    fn load(&self) -> Result<DesktopPreferences, String> {
        self.load.clone()
    }

    fn save_fixed_port(&self, port: u16) -> Result<(), String> {
        if self.fail_save {
            return Err("store is read-only".to_string());
        }
        self.saved.store(port, Ordering::Relaxed);
        Ok(())
    }

    fn save_external_access(&self, enabled: bool) -> Result<(), String> {
        if self.fail_save {
            return Err("store is read-only".to_string());
        }
        self.saved_external.store(enabled, Ordering::Relaxed);
        Ok(())
    }

    fn save_silent_start(&self, _enabled: bool) -> Result<(), String> {
        if self.fail_save {
            return Err("store is read-only".to_string());
        }
        Ok(())
    }
}

struct StaticOwners(Vec<PortOwner>);

impl PortOwnerResolver for StaticOwners {
    fn resolve(&self, _scope: BindScope, _port: u16) -> Result<Vec<PortOwner>, String> {
        Ok(self.0.clone())
    }
}

struct FailingOwners;

impl PortOwnerResolver for FailingOwners {
    fn resolve(&self, _scope: BindScope, _port: u16) -> Result<Vec<PortOwner>, String> {
        Err("owner table unavailable".to_string())
    }
}

struct BlockingOwners {
    started: AtomicBool,
    released: StdMutex<bool>,
    release: Condvar,
}

impl BlockingOwners {
    fn new() -> Self {
        Self {
            started: AtomicBool::new(false),
            released: StdMutex::new(false),
            release: Condvar::new(),
        }
    }

    fn release(&self) {
        match self.released.lock() {
            Ok(mut released) => {
                *released = true;
                self.release.notify_all();
            }
            Err(poisoned) => {
                *poisoned.into_inner() = true;
                self.release.notify_all();
            }
        }
    }
}

impl PortOwnerResolver for BlockingOwners {
    fn resolve(&self, _scope: BindScope, _port: u16) -> Result<Vec<PortOwner>, String> {
        self.started.store(true, Ordering::Release);
        let released = match self.released.lock() {
            Ok(released) => released,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _released = self
            .release
            .wait_while(released, |released| !*released)
            .map_err(|error| error.to_string())?;
        Ok(vec![PortOwner {
            name: "stale-owner".to_string(),
            pid: 91,
        }])
    }
}

struct AssertPublishedBeforeDrain {
    old_port: u16,
    called: AtomicBool,
}

impl PortSwitchPublisher for AssertPublishedBeforeDrain {
    fn publish(&self, _port: u16) -> Result<(), String> {
        std::net::TcpStream::connect(("127.0.0.1", self.old_port))
            .map_err(|error| format!("old listener closed before switch publication: {error}"))?;
        self.called.store(true, Ordering::Release);
        Ok(())
    }
}

fn no_owners() -> Arc<dyn PortOwnerResolver> {
    Arc::new(StaticOwners(vec![]))
}

fn test_app() -> Router {
    Router::new().route("/health", get(|| async { "ok" }))
}

fn unused_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve test port");
    listener.local_addr().unwrap().port()
}

async fn assert_reachable(port: u16) {
    // A rebound socket can briefly RST connections handed to the draining
    // listener, so poll instead of asserting on a single attempt.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(response) = reqwest::get(format!("http://127.0.0.1:{port}/health")).await
                && response.text().await.unwrap_or_default() == "ok"
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("listener should accept requests");
}

async fn wait_until_closed(port: u16) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("old listener should close");
}

#[tokio::test]
async fn absent_or_invalid_preference_requests_the_default_fixed_loopback_port() {
    for preference in [PortPreferenceLoad::Missing, PortPreferenceLoad::Invalid] {
        let requested_port = Arc::new(AtomicU16::new(0));
        let captured_port = requested_port.clone();
        let bind_override: Arc<TestBind> = Arc::new(move |scope, port, app| {
            captured_port.store(port, Ordering::Relaxed);
            Box::pin(async move { start_http_server((scope.host(), 0), app).await })
        });
        let runtime = DesktopGatewayRuntime::start_with_bind_override(
            test_app(),
            Arc::new(TestStore::new(preference)),
            no_owners(),
            bind_override,
        )
        .await
        .expect("runtime should start");

        let state = runtime.snapshot().await;
        assert_eq!(requested_port.load(Ordering::Relaxed), DEFAULT_PORT);
        assert_eq!(state.mode, DesktopPortMode::Fixed);
        assert_eq!(state.fixed_port, Some(DEFAULT_PORT));
        assert_reachable(state.current_port).await;

        runtime.shutdown().await.expect("runtime should stop");
    }
}

#[tokio::test]
async fn unavailable_default_port_uses_a_random_fallback() {
    let bind_override: Arc<TestBind> = Arc::new(|scope, port, app| {
        Box::pin(async move {
            if port == DEFAULT_PORT {
                Err(io::Error::new(io::ErrorKind::AddrInUse, "default port is occupied").into())
            } else {
                start_http_server((scope.host(), port), app).await
            }
        })
    });
    let runtime = DesktopGatewayRuntime::start_with_bind_override(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Missing)),
        no_owners(),
        bind_override,
    )
    .await
    .expect("runtime should use a random fallback");

    let state = runtime.snapshot().await;
    assert_eq!(state.mode, DesktopPortMode::Fallback);
    assert_eq!(state.fixed_port, Some(DEFAULT_PORT));
    assert_ne!(state.current_port, DEFAULT_PORT);
    assert_eq!(
        state.binding_failure.as_ref().map(|failure| failure.kind),
        Some(BindingFailureKind::AddrInUse)
    );
    assert_reachable(state.current_port).await;

    runtime.shutdown().await.expect("runtime should stop");
}

#[tokio::test]
async fn valid_preference_starts_on_the_fixed_loopback_port() {
    let port = unused_port();
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(port))),
        no_owners(),
    )
    .await
    .expect("runtime should bind the fixed port");

    let state = runtime.snapshot().await;
    assert_eq!(state.mode, DesktopPortMode::Fixed);
    assert_eq!(state.fixed_port, Some(port));
    assert_eq!(state.current_port, port);

    runtime.shutdown().await.expect("runtime should stop");
}

#[tokio::test]
async fn occupied_fixed_port_falls_back_and_identifies_owners_in_the_background() {
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupy fixed port");
    let port = occupied.local_addr().unwrap().port();
    let owner = PortOwner {
        name: "holder.exe".to_string(),
        pid: 41,
    };
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(port))),
        Arc::new(StaticOwners(vec![owner.clone()])),
    )
    .await
    .expect("runtime should fall back");

    let initial = runtime.snapshot().await;
    assert_eq!(initial.mode, DesktopPortMode::Fallback);
    assert_eq!(initial.fixed_port, Some(port));
    assert_ne!(initial.current_port, port);
    assert_eq!(initial.owner_lookup, OwnerLookupStatus::Identifying);
    let identified = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = runtime.snapshot().await;
            if state.owner_lookup != OwnerLookupStatus::Identifying {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner lookup should finish");
    assert_eq!(identified.owner_lookup, OwnerLookupStatus::Found);
    assert_eq!(identified.owners, vec![owner]);
    assert_reachable(initial.current_port).await;

    runtime.shutdown().await.expect("runtime should stop");
}

#[tokio::test]
async fn unreadable_preference_starts_in_a_recoverable_config_error_state() {
    let unreadable = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::failing_read("permission denied")),
        no_owners(),
    )
    .await
    .unwrap();
    let state = unreadable.snapshot().await;
    assert_eq!(state.mode, DesktopPortMode::ConfigError);
    assert_eq!(state.config_error.as_deref(), Some("permission denied"));
    unreadable.shutdown().await.unwrap();
}

#[tokio::test]
async fn saving_a_new_port_binds_before_persisting_and_drains_the_old_listener() {
    let store = Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port())));
    let runtime = DesktopGatewayRuntime::start(test_app(), store.clone(), no_owners())
        .await
        .unwrap();
    let old_port = runtime.snapshot().await.current_port;
    let new_port = unused_port();

    let state = runtime
        .configure_fixed_port(u32::from(new_port))
        .await
        .unwrap();

    assert_eq!(state.mode, DesktopPortMode::Fixed);
    assert_eq!(state.current_port, new_port);
    assert_eq!(store.saved.load(Ordering::Relaxed), new_port);
    assert_reachable(new_port).await;
    wait_until_closed(old_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn saving_the_current_port_persists_without_rebinding() {
    let store = Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port())));
    let runtime = DesktopGatewayRuntime::start(test_app(), store.clone(), no_owners())
        .await
        .unwrap();
    let current_port = runtime.snapshot().await.current_port;

    let state = runtime
        .configure_fixed_port(u32::from(current_port))
        .await
        .unwrap();

    assert_eq!(state.mode, DesktopPortMode::Fixed);
    assert_eq!(store.saved.load(Ordering::Relaxed), current_port);
    assert_reachable(current_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn occupied_candidate_reports_owners_without_changing_the_running_listener() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port()))),
        Arc::new(StaticOwners(vec![PortOwner {
            name: "busy-app".to_string(),
            pid: 73,
        }])),
    )
    .await
    .unwrap();
    let before = runtime.snapshot().await;
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();

    let error = runtime
        .configure_fixed_port(u32::from(occupied_port))
        .await
        .expect_err("occupied candidate should fail");

    assert_eq!(error.code, PortOperationErrorCode::BindFailed);
    assert_eq!(error.owner_lookup, OwnerLookupStatus::Identifying);
    let identified = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = runtime.snapshot().await;
            if state
                .candidate_error
                .as_ref()
                .is_some_and(|error| error.owner_lookup != OwnerLookupStatus::Identifying)
            {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("candidate owner lookup should finish");
    assert_eq!(identified.current_port, before.current_port);
    assert_eq!(identified.fixed_port, before.fixed_port);
    assert_eq!(identified.mode, before.mode);
    assert_eq!(identified.candidate_port, Some(occupied_port));
    let candidate_error = identified.candidate_error.unwrap();
    assert_eq!(candidate_error.owner_lookup, OwnerLookupStatus::Found);
    assert_eq!(candidate_error.owners[0].name, "busy-app");
    assert_reachable(before.current_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn recheck_switches_fallback_to_the_saved_port_after_it_is_released() {
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let fixed_port = occupied.local_addr().unwrap().port();
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(fixed_port))),
        no_owners(),
    )
    .await
    .unwrap();
    let fallback_port = runtime.snapshot().await.current_port;
    drop(occupied);

    let state = runtime.recheck_fixed_port().await.unwrap();

    assert_eq!(state.mode, DesktopPortMode::Fixed);
    assert_eq!(state.current_port, fixed_port);
    wait_until_closed(fallback_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn store_write_failure_stops_the_candidate_and_preserves_the_old_state() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::failing_save(PortPreferenceLoad::Fixed(
            unused_port(),
        ))),
        no_owners(),
    )
    .await
    .unwrap();
    let before = runtime.snapshot().await;
    let candidate_port = unused_port();

    let error = runtime
        .configure_fixed_port(u32::from(candidate_port))
        .await
        .expect_err("store write should fail");

    assert_eq!(error.code, PortOperationErrorCode::StoreWriteFailed);
    assert_eq!(runtime.snapshot().await, before);
    assert_reachable(before.current_port).await;
    wait_until_closed(candidate_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_port_is_rejected_without_changing_the_listener() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port()))),
        no_owners(),
    )
    .await
    .unwrap();
    let before = runtime.snapshot().await;

    let error = runtime.configure_fixed_port(1023).await.unwrap_err();
    assert_eq!(error.code, PortOperationErrorCode::InvalidPort);
    let error = runtime.configure_fixed_port(65_536).await.unwrap_err();
    assert_eq!(error.code, PortOperationErrorCode::InvalidPort);
    assert_eq!(runtime.snapshot().await, before);

    runtime.shutdown().await.unwrap();
}

#[test]
fn blank_process_names_are_not_reported_as_known_owners() {
    assert_eq!(super::known_port_owner("  ".to_string(), 17), None);
}

#[tokio::test]
async fn non_address_in_use_errors_fall_back_and_remain_manually_recheckable() {
    let fixed_port = unused_port();
    let bind_override: Arc<TestBind> = Arc::new(move |scope, port, app| {
        Box::pin(async move {
            if port == fixed_port {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "blocked by policy").into())
            } else {
                start_http_server((scope.host(), port), app).await
            }
        })
    });
    let runtime = DesktopGatewayRuntime::start_with_bind_override(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(fixed_port))),
        no_owners(),
        bind_override,
    )
    .await
    .expect("non-address bind failure should use a random fallback");

    let initial = runtime.snapshot().await;
    assert_eq!(initial.mode, DesktopPortMode::Fallback);
    assert_eq!(
        initial.binding_failure.as_ref().map(|failure| failure.kind),
        Some(BindingFailureKind::Other)
    );
    assert_eq!(initial.owner_lookup, OwnerLookupStatus::NotApplicable);

    let rechecked = runtime.recheck_fixed_port().await.unwrap();
    assert_eq!(rechecked.current_port, initial.current_port);
    assert_eq!(rechecked.mode, DesktopPortMode::Fallback);
    assert_eq!(
        rechecked
            .binding_failure
            .as_ref()
            .map(|failure| failure.kind),
        Some(BindingFailureKind::Other)
    );
    assert_reachable(initial.current_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_fails_when_a_fixed_bind_and_its_random_fallback_both_fail() {
    let fixed_port = unused_port();
    let bind_override: Arc<TestBind> = Arc::new(|_scope, _port, _app| {
        Box::pin(async {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "no listener allowed").into())
        })
    });

    let result = DesktopGatewayRuntime::start_with_bind_override(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(fixed_port))),
        no_owners(),
        bind_override,
    )
    .await;

    assert!(
        result.is_err(),
        "startup must fail without a fallback listener"
    );
}

#[tokio::test]
async fn owner_lookup_failure_becomes_unknown_without_changing_the_fallback() {
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let fixed_port = occupied.local_addr().unwrap().port();
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(fixed_port))),
        Arc::new(FailingOwners),
    )
    .await
    .unwrap();
    let fallback_port = runtime.snapshot().await.current_port;

    let state = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = runtime.snapshot().await;
            if state.owner_lookup != OwnerLookupStatus::Identifying {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed owner lookup should settle");

    assert_eq!(state.owner_lookup, OwnerLookupStatus::Unknown);
    assert!(state.owners.is_empty());
    assert_eq!(state.current_port, fallback_port);
    assert_reachable(fallback_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn switch_publication_happens_before_the_old_listener_starts_draining() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port()))),
        no_owners(),
    )
    .await
    .unwrap();
    let old_port = runtime.current_port();
    let publisher = Arc::new(AssertPublishedBeforeDrain {
        old_port,
        called: AtomicBool::new(false),
    });
    runtime.set_switch_publisher(publisher.clone());
    let new_port = unused_port();

    runtime
        .configure_fixed_port(u32::from(new_port))
        .await
        .unwrap();

    assert!(publisher.called.load(Ordering::Acquire));
    wait_until_closed(old_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_in_flight_request_finishes_while_the_old_listener_drains() {
    let (started_tx, started_rx) = oneshot::channel();
    let started_tx = Arc::new(StdMutex::new(Some(started_tx)));
    let release = Arc::new(Notify::new());
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/slow",
            get({
                let started_tx = started_tx.clone();
                let release = release.clone();
                move || {
                    let started_tx = started_tx.clone();
                    let release = release.clone();
                    async move {
                        let sender = match started_tx.lock() {
                            Ok(mut sender) => sender.take(),
                            Err(poisoned) => poisoned.into_inner().take(),
                        };
                        if let Some(sender) = sender {
                            let _ = sender.send(());
                        }
                        release.notified().await;
                        "done"
                    }
                }
            }),
        );
    let runtime = DesktopGatewayRuntime::start(
        app,
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port()))),
        no_owners(),
    )
    .await
    .unwrap();
    let old_port = runtime.current_port();
    let request = tokio::spawn(async move {
        reqwest::get(format!("http://127.0.0.1:{old_port}/slow"))
            .await
            .expect("in-flight request should receive a response")
            .text()
            .await
            .unwrap()
    });
    started_rx.await.expect("slow request should start");

    let new_port = unused_port();
    runtime
        .configure_fixed_port(u32::from(new_port))
        .await
        .unwrap();
    assert!(
        reqwest::get(format!("http://127.0.0.1:{old_port}/health"))
            .await
            .is_err(),
        "old listener must reject new requests after publication"
    );
    runtime.request_shutdown();
    wait_until_closed(new_port).await;
    release.notify_one();
    assert_eq!(request.await.unwrap(), "done");
    wait_until_closed(old_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_port_changes_publish_only_one_current_listener() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port()))),
        no_owners(),
    )
    .await
    .unwrap();
    let first_port = unused_port();
    let second_port = unused_port();

    let first = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .configure_fixed_port(u32::from(first_port))
                .await
                .unwrap()
        })
    };
    let second = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .configure_fixed_port(u32::from(second_port))
                .await
                .unwrap()
        })
    };
    first.await.unwrap();
    second.await.unwrap();

    let state = runtime.snapshot().await;
    assert!(state.current_port == first_port || state.current_port == second_port);
    let superseded = if state.current_port == first_port {
        second_port
    } else {
        first_port
    };
    assert_reachable(state.current_port).await;
    wait_until_closed(superseded).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_owner_results_cannot_overwrite_a_new_fixed_state() {
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let fixed_port = occupied.local_addr().unwrap().port();
    let owners = Arc::new(BlockingOwners::new());
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::new(PortPreferenceLoad::Fixed(fixed_port))),
        owners.clone(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !owners.started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner lookup should start");

    let new_port = unused_port();
    runtime
        .configure_fixed_port(u32::from(new_port))
        .await
        .unwrap();
    owners.release();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = runtime.snapshot().await;
    assert_eq!(state.mode, DesktopPortMode::Fixed);
    assert_eq!(state.current_port, new_port);
    assert!(state.owners.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn recheck_without_a_fixed_target_is_rejected() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::failing_read("preference unavailable")),
        no_owners(),
    )
    .await
    .unwrap();

    let error = runtime.recheck_fixed_port().await.unwrap_err();

    assert_eq!(error.code, PortOperationErrorCode::NoFixedPort);
    runtime.shutdown().await.unwrap();
}

#[test]
fn background_argument_marks_autostart_launches_only() {
    assert!(args_include_background([
        "--data-dir".into(),
        "root".into(),
        "--background".into()
    ]));
    assert!(!args_include_background([
        "--data-dir".into(),
        "root".into()
    ]));
    assert!(!args_include_background([]));
    assert!(!args_include_background(["--backgrounded".into()]));
}

#[tokio::test]
async fn external_access_preference_binds_all_interfaces_on_startup() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::with_preferences(DesktopPreferences {
            fixed_port: PortPreferenceLoad::Fixed(unused_port()),
            external_access: true,
            silent_start: false,
        })),
        no_owners(),
    )
    .await
    .unwrap();

    let state = runtime.snapshot().await;
    assert!(state.external_access);
    assert!(runtime.current_local_addr().ip().is_unspecified());
    assert_reachable(state.current_port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn enabling_external_access_rebinds_the_same_port_on_all_interfaces() {
    let store = Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port())));
    let runtime = DesktopGatewayRuntime::start(test_app(), store.clone(), no_owners())
        .await
        .unwrap();
    let port = runtime.current_port();
    assert!(runtime.current_local_addr().ip().is_loopback());

    let state = runtime.set_external_access(true).await.unwrap();

    assert!(state.external_access);
    assert_eq!(state.current_port, port);
    assert_eq!(store.saved_external.load(Ordering::Relaxed), true);
    assert!(runtime.current_local_addr().ip().is_unspecified());
    assert_reachable(port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn disabling_external_access_returns_to_loopback() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::with_preferences(DesktopPreferences {
            fixed_port: PortPreferenceLoad::Fixed(unused_port()),
            external_access: true,
            silent_start: false,
        })),
        no_owners(),
    )
    .await
    .unwrap();
    let port = runtime.current_port();

    let state = runtime.set_external_access(false).await.unwrap();

    assert!(!state.external_access);
    assert_eq!(state.current_port, port);
    assert!(runtime.current_local_addr().ip().is_loopback());
    assert_reachable(port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_external_bind_restores_the_loopback_listener() {
    let store = Arc::new(TestStore::new(PortPreferenceLoad::Fixed(unused_port())));
    let bind_override: Arc<TestBind> = Arc::new(|scope, port, app| {
        Box::pin(async move {
            if scope == BindScope::External {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "wildcard blocked").into())
            } else {
                start_http_server((scope.host(), port), app).await
            }
        })
    });
    let runtime = DesktopGatewayRuntime::start_with_bind_override(
        test_app(),
        store.clone(),
        no_owners(),
        bind_override,
    )
    .await
    .unwrap();
    let port = runtime.current_port();

    let error = runtime.set_external_access(true).await.unwrap_err();

    assert_eq!(error.code, PortOperationErrorCode::BindFailed);
    let state = runtime.snapshot().await;
    assert!(!state.external_access);
    assert_eq!(state.current_port, port);
    assert!(!store.saved_external.load(Ordering::Relaxed));
    assert!(runtime.current_local_addr().ip().is_loopback());
    assert_reachable(port).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn external_access_survives_a_later_port_change() {
    let runtime = DesktopGatewayRuntime::start(
        test_app(),
        Arc::new(TestStore::with_preferences(DesktopPreferences {
            fixed_port: PortPreferenceLoad::Fixed(unused_port()),
            external_access: true,
            silent_start: false,
        })),
        no_owners(),
    )
    .await
    .unwrap();
    let new_port = unused_port();

    let state = runtime
        .configure_fixed_port(u32::from(new_port))
        .await
        .unwrap();

    assert!(state.external_access);
    assert_eq!(state.current_port, new_port);
    assert!(runtime.current_local_addr().ip().is_unspecified());
    assert_reachable(new_port).await;
    runtime.shutdown().await.unwrap();
}
