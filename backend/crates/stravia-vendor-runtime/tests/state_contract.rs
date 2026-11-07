//! Exercises the SDK's state helpers through an actual Wasm component and host services.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, LogLevel, OperationScope,
    RuntimeError, RuntimeEvent, VendorRuntime,
    host::{
        HostJournalEntry, HostStateCell, JournalEntryData, JournalSnapshot, StateError, StateScope,
        StateSnapshot,
    },
};
use stravia_vendor_sdk::{
    AiRequest, ErrorKind, OperationInput, OperationOutput, ProviderSnapshot,
    net::{ConnectRequest, Options, Request},
};
use tokio::sync::Notify;

#[derive(Default)]
struct StoredCell {
    revision: u64,
    payload: Option<Vec<u8>>,
    head: Option<Arc<dyn HostJournalEntry>>,
}

#[derive(Default)]
struct State {
    cells: Mutex<BTreeMap<(u8, String), StoredCell>>,
    opened: AtomicUsize,
    dropped: AtomicUsize,
    entries_created: AtomicUsize,
    entries_dropped: AtomicUsize,
    waiting: Notify,
}

struct Entry {
    state: Arc<State>,
    text: String,
    previous: Option<Arc<dyn HostJournalEntry>>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.state.entries_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl HostJournalEntry for Entry {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn read(&self) -> Result<JournalEntryData, StateError> {
        if self.text == "read-error" {
            return Err(StateError::new("read-failed", "safe failure"));
        }
        Ok(JournalEntryData {
            text: self.text.clone(),
            previous: self.previous.clone(),
        })
    }
}

struct Cell {
    state: Arc<State>,
    key: (u8, String),
}

impl Drop for Cell {
    fn drop(&mut self) {
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl HostStateCell for Cell {
    async fn load(&self) -> Result<StateSnapshot, StateError> {
        if self.key.1 == "storage-error" {
            return Err(StateError::new("storage", "atomic state storage failed"));
        }
        if self.key.1 == "load-error" {
            return Err(StateError::new("load-failed", "safe failure"));
        }
        if self.key.1 == "cancel" {
            self.state.waiting.notify_one();
            return std::future::pending().await;
        }
        let cells = self.state.cells.lock().unwrap();
        Ok(match cells.get(&self.key) {
            Some(cell) => StateSnapshot {
                revision: cell.revision,
                payload: cell.payload.clone(),
            },
            None => StateSnapshot {
                revision: 0,
                payload: None,
            },
        })
    }

    async fn compare_exchange(
        &self,
        expected: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<bool, StateError> {
        if self.key.1 == "cas-error" {
            return Err(StateError::new("cas-failed", "safe failure"));
        }
        let mut cells = self.state.cells.lock().unwrap();
        let entry = cells.entry(self.key.clone()).or_default();
        if entry.revision != expected {
            return Ok(false);
        }
        entry.revision += 1;
        entry.payload = payload;
        entry.head = None;
        Ok(true)
    }

    async fn journal_load(&self) -> Result<JournalSnapshot, StateError> {
        if self.key.1 == "journal-load-error" {
            return Err(StateError::new("load-failed", "safe failure"));
        }
        let cells = self.state.cells.lock().unwrap();
        Ok(match cells.get(&self.key) {
            Some(cell) => JournalSnapshot {
                revision: cell.revision,
                payload: cell.payload.clone(),
                head: cell.head.clone(),
            },
            None => JournalSnapshot {
                revision: 0,
                payload: None,
                head: None,
            },
        })
    }

    async fn journal_append(
        &self,
        text: String,
        previous: Option<Arc<dyn HostJournalEntry>>,
    ) -> Result<Arc<dyn HostJournalEntry>, StateError> {
        if self.key.1 == "journal-append-error" {
            return Err(StateError::new("append-failed", "safe failure"));
        }
        if self.key.0 != 1 {
            return Err(StateError::new("unsupported", "safe failure"));
        }
        self.state.entries_created.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Entry {
            state: self.state.clone(),
            text,
            previous,
        }))
    }

    async fn journal_compare_exchange(
        &self,
        expected: u64,
        payload: Option<Vec<u8>>,
        head: Option<Arc<dyn HostJournalEntry>>,
    ) -> Result<bool, StateError> {
        if self.key.1 == "journal-cas-error" {
            return Err(StateError::new("cas-failed", "safe failure"));
        }
        let mut cells = self.state.cells.lock().unwrap();
        let cell = cells.entry(self.key.clone()).or_default();
        if cell.revision != expected {
            return Ok(false);
        }
        cell.revision += 1;
        cell.payload = payload;
        cell.head = head;
        Ok(true)
    }
}

struct Socket {
    fresh: bool,
}
#[async_trait]
impl HostWebSocket for Socket {
    fn handshake_status(&self) -> u16 {
        101
    }
    fn handshake_headers(&self) -> Vec<stravia_vendor_sdk::net::Field> {
        if self.fresh {
            vec![]
        } else {
            vec![stravia_vendor_sdk::net::Field {
                name: "x-old-handshake".into(),
                bytes: b"old".to_vec(),
            }]
        }
    }
    fn is_fresh_handshake(&self) -> bool {
        self.fresh
    }
    async fn send(&self, _: stravia_vendor_runtime::WebSocketMessage) -> Result<(), HostFailure> {
        panic!("unexpected send")
    }
    async fn next(&self) -> Result<Option<stravia_vendor_runtime::WebSocketMessage>, HostFailure> {
        panic!("unexpected next")
    }
    async fn close(&self, _: Option<String>) -> Result<(), HostFailure> {
        panic!("unexpected close")
    }
}

struct Services(Arc<State>);
#[async_trait]
impl HostServices for Services {
    fn http_start(&self, _: Request, _: Options) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        Err(HostFailure::new(ErrorKind::Unsupported, "unexpected HTTP"))
    }
    async fn ws_connect(
        &self,
        request: ConnectRequest,
        _: Options,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        let fresh = match request.url.as_str() {
            "wss://fresh.invalid" => true,
            "wss://reused.invalid" => false,
            _ => panic!("unexpected WebSocket URL"),
        };
        Ok(Arc::new(Socket { fresh }))
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(None)
    }
    async fn write_private_state(&self, _: Vec<u8>) -> Result<(), HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Unsupported,
            "unexpected private state",
        ))
    }
    async fn open_state(
        &self,
        scope: StateScope,
        key: String,
    ) -> Result<Arc<dyn HostStateCell>, StateError> {
        if key == "open-error" {
            return Err(StateError::new("open-failed", "safe failure"));
        }
        self.0.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Cell {
            state: self.0.clone(),
            key: (
                match scope {
                    StateScope::Provider => 0,
                    StateScope::Conversation => 1,
                },
                key,
            ),
        }))
    }
    async fn open_volatile_state(&self, key: String) -> Result<Arc<dyn HostStateCell>, StateError> {
        if key == "open-error" {
            return Err(StateError::new("open-failed", "safe failure"));
        }
        self.0.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Cell {
            state: self.0.clone(),
            key: (2, key),
        }))
    }
    async fn emit_event(&self, _: RuntimeEvent) -> Result<(), HostFailure> {
        Ok(())
    }
    fn log(&self, _: LogLevel, _: &str) {}
    fn generation_is_current(&self, generation: u64) -> bool {
        generation == 0
    }
}

fn input(mode: &str) -> OperationInput {
    OperationInput::Infer {
        provider: ProviderSnapshot {
            provider_id: "fixture.state-contract".into(),
            channel: "default".into(),
            base_url: "https://example.invalid".into(),
            protocol: "fixture-state".into(),
            options: BTreeMap::from([("mode".into(), serde_json::json!(mode))]),
            credentials: BTreeMap::new(),
            model: None,
            model_metadata: None,
            client_headers: vec![],
            operation_metadata: BTreeMap::new(),
        },
        request: AiRequest::new("fixture-model", vec![]),
    }
}

#[tokio::test]
async fn sdk_state_roundtrip_failures_and_resource_cleanup() {
    let bytes = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target/vendor-test-fixtures/state-contract.wasm"),
    )
    .expect("build vendor test fixtures first");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&bytes).await.expect("load state fixture");
    let state = Arc::new(State::default());
    for mode in [
        "flow",
        "open-error",
        "load-error",
        "cas-error",
        "volatile-flow",
        "volatile-reopen",
        "volatile-invalid",
        "volatile-open-error",
        "volatile-load-error",
        "volatile-cas-error",
        "ws-freshness",
        "journal-flow",
        "journal-load-error",
        "journal-append-error",
        "journal-read-error",
        "journal-cas-error",
        "journal-unsupported",
    ] {
        let (operation, input) = input(mode).encode_for_host().unwrap();
        let scope = OperationScope::new(
            Arc::new(Services(state.clone())),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(30)),
            0,
        );
        let output = runtime
            .execute(&plugin, "default", operation, input, scope)
            .await
            .expect("execute state fixture");
        let OperationOutput::Infer(response) = output else {
            panic!("expected inference response")
        };
        assert_eq!(response.id, mode);
        assert_eq!(
            state.opened.load(Ordering::SeqCst),
            state.dropped.load(Ordering::SeqCst),
            "leaked cell after {mode}"
        );
        assert_eq!(
            state.entries_created.load(Ordering::SeqCst),
            state.entries_dropped.load(Ordering::SeqCst),
            "leaked journal entry after {mode}"
        );
    }

    for (mode, expected_message) in [
        ("classify-invalid", "state key exceeds 128 bytes"),
        ("classify-unsupported", "safe failure"),
        ("classify-storage", "atomic state storage failed"),
        ("classify-conflict", "atomic state revision conflict"),
    ] {
        let (operation, input) = input(mode).encode_for_host().unwrap();
        let scope = OperationScope::new(
            Arc::new(Services(state.clone())),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(30)),
            0,
        );
        let error = runtime
            .execute(&plugin, "default", operation, input, scope)
            .await
            .unwrap_err();
        let RuntimeError::Plugin {
            kind,
            diagnostic_message,
            ..
        } = error
        else {
            panic!("expected guest state failure for {mode}: {error:?}");
        };
        assert_eq!(diagnostic_message, expected_message);
        assert!(
            match mode {
                "classify-invalid" => matches!(kind, ErrorKind::Invalid),
                "classify-unsupported" => matches!(kind, ErrorKind::Unsupported),
                "classify-storage" | "classify-conflict" => matches!(kind, ErrorKind::Trapped),
                _ => unreachable!(),
            },
            "unexpected classification for {mode}: {kind:?}"
        );
        assert_eq!(
            state.opened.load(Ordering::SeqCst),
            state.dropped.load(Ordering::SeqCst)
        );
    }

    for mode in ["cancel", "volatile-cancel"] {
        let cancellation = CancellationToken::new();
        let scope = OperationScope::new(
            Arc::new(Services(state.clone())),
            cancellation.clone(),
            Deadline::from_now(Duration::from_secs(30)),
            0,
        );
        let (operation, input) = input(mode).encode_for_host().unwrap();
        let execute = runtime.execute(&plugin, "default", operation, input, scope);
        tokio::pin!(execute);
        tokio::select! {
            _ = state.waiting.notified() => {},
            result = &mut execute => panic!("operation ended before pending load: {result:?}"),
        }
        cancellation.cancel();
        assert!(matches!(execute.await, Err(RuntimeError::Cancelled)));
        assert_eq!(
            state.opened.load(Ordering::SeqCst),
            state.dropped.load(Ordering::SeqCst),
            "leaked cancelled cell: {mode}"
        );
    }
}
