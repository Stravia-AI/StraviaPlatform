//! Real SDK state calls exercised through the component boundary.
use serde_json::{Value, json};
use stravia_vendor_sdk::{
    AiResponse,
    state::{self, Error, Scope},
    wit::{self, host, types},
};

struct Fixture;

impl wit::Guest for Fixture {
    fn descriptor() -> String {
        json!({
            "vendor_id": "fixture.state-contract", "version": "1.0.0",
            "display_name": "State contract", "description": null,
            "canonical_format_version": 1, "kind": "dedicated",
            "providers": [{
                "provider_id": "fixture.state-contract", "catalog_id": null,
                "display_name": "State contract", "description": null,
                "capabilities": ["infer"],
                "channels": [{"id": "default", "name": {"en-US": "State", "zh-CN": "状态"},
                    "description": null, "protocol": "fixture-state", "capabilities": ["infer"]}]
            }]
        })
        .to_string()
    }

    fn select_protocol(
        _: types::OperationKind,
        _: String,
        provider: types::ProviderSnapshot,
        _: types::CanonicalPayload,
    ) -> Result<String, types::PluginError> {
        Ok(provider.protocol)
    }

    fn execute(
        _: types::OperationKind,
        _: String,
        input: types::OperationInput,
    ) -> Result<Vec<u8>, types::PluginError> {
        let options: Value = serde_json::from_slice(&input.provider.options).map_err(invalid)?;
        let mode = options["mode"]
            .as_str()
            .ok_or_else(|| invalid("missing mode"))?;
        host::emit_started()?;
        match mode {
            "classify-invalid" => {
                state::open(Scope::Provider, &"x".repeat(129))?;
            }
            "classify-unsupported" => {
                let cell = state::open(Scope::Provider, "journal")?;
                state::journal::append(&cell, "entry", None)?;
            }
            "classify-storage" => {
                let cell = state::open(Scope::Provider, "storage-error")?;
                cell.load()?;
            }
            "classify-conflict" => {
                let cell = state::open(Scope::Provider, "conflict")?;
                cell.compare_exchange(0, Some(b"first"))?;
                cell.compare_exchange(0, Some(b"stale"))?;
            }
            "journal-flow" => {
                use state::journal;
                let cell = state::open(Scope::Conversation, "journal").map_err(invalid)?;
                let initial = journal::load(&cell).map_err(invalid)?;
                assert_eq!(
                    (initial.revision, initial.payload, initial.head.is_none()),
                    (0, None, true)
                );
                let first = journal::append(&cell, "first α", None).map_err(invalid)?;
                let second = journal::append(&cell, "second", Some(&first)).map_err(invalid)?;
                journal::compare_exchange(&cell, 0, Some(b"payload"), Some(&second))
                    .map_err(invalid)?;
                assert_eq!(
                    journal::compare_exchange(&cell, 0, Some(b"stale"), Some(&first)),
                    Err(Error::Conflict)
                );
                let current = journal::load(&cell).map_err(invalid)?;
                assert_ne!(current.revision, 0);
                assert_eq!(current.payload.as_deref(), Some(&b"payload"[..]));
                let data = current.head.unwrap().read().map_err(invalid)?;
                assert_eq!(data.text, "second");
                let data = data.previous.unwrap().read().map_err(invalid)?;
                assert_eq!(data.text, "first α");
                assert!(data.previous.is_none());
                // A legacy CAS must atomically clear the journal head.
                cell.compare_exchange(current.revision, Some(b"legacy"))
                    .map_err(invalid)?;
                let cleared = journal::load(&cell).map_err(invalid)?;
                assert_eq!(cleared.payload.as_deref(), Some(&b"legacy"[..]));
                assert!(cleared.head.is_none());
                assert_ne!(cleared.revision, current.revision);
            }
            "journal-load-error" => {
                let cell = state::open(Scope::Conversation, mode).map_err(invalid)?;
                assert_host_error(state::journal::load(&cell).err(), "load-failed");
            }
            "journal-append-error" => {
                let cell = state::open(Scope::Conversation, mode).map_err(invalid)?;
                assert_host_error(
                    state::journal::append(&cell, "private", None).err(),
                    "append-failed",
                );
            }
            "journal-read-error" => {
                let cell = state::open(Scope::Conversation, mode).map_err(invalid)?;
                let entry = state::journal::append(&cell, "read-error", None).map_err(invalid)?;
                assert_host_error(entry.read().err(), "read-failed");
            }
            "journal-cas-error" => {
                let cell = state::open(Scope::Conversation, mode).map_err(invalid)?;
                assert_host_error(
                    state::journal::compare_exchange(&cell, 0, Some(b"private"), None).err(),
                    "cas-failed",
                );
            }
            "journal-unsupported" => {
                for cell in [
                    state::open(Scope::Provider, "journal"),
                    state::open_volatile("journal"),
                ] {
                    let cell = cell.map_err(invalid)?;
                    assert_host_error(
                        state::journal::append(&cell, "private", None).err(),
                        "unsupported",
                    );
                }
            }
            "flow" => {
                let cell = state::open(Scope::Provider, "counter").map_err(invalid)?;
                let initial = cell.load().map_err(invalid)?;
                assert_eq!((initial.revision, initial.payload), (0, None));
                cell.compare_exchange(0, Some(b"one")).map_err(invalid)?;
                let current = cell.load().map_err(invalid)?;
                assert_eq!(current.payload.as_deref(), Some(&b"one"[..]));
                assert_ne!(current.revision, 0);
                assert_eq!(
                    cell.compare_exchange(0, Some(b"stale")),
                    Err(Error::Conflict)
                );
                assert_eq!(cell.load().map_err(invalid)?, current);
                cell.delete(current.revision).map_err(invalid)?;
                let deleted = cell.load().map_err(invalid)?;
                assert_eq!(deleted.payload, None);
                assert_ne!(deleted.revision, current.revision);
                assert_eq!(
                    cell.compare_exchange(current.revision, Some(b"aba")),
                    Err(Error::Conflict)
                );
                let other = state::open(Scope::Conversation, "counter").map_err(invalid)?;
                assert_eq!(other.load().map_err(invalid)?.revision, 0);
            }
            "volatile-flow" => {
                let cell = state::open_volatile("counter").map_err(invalid)?;
                assert_eq!(
                    (
                        cell.load().map_err(invalid)?.revision,
                        cell.load().map_err(invalid)?.payload
                    ),
                    (0, None)
                );
                cell.compare_exchange(0, Some(b"volatile"))
                    .map_err(invalid)?;
                let current = cell.load().map_err(invalid)?;
                assert_eq!(current.payload.as_deref(), Some(&b"volatile"[..]));
                assert_eq!(cell.compare_exchange(0, None), Err(Error::Conflict));
                assert_eq!(
                    state::open(Scope::Provider, "counter")
                        .map_err(invalid)?
                        .load()
                        .map_err(invalid)?
                        .payload,
                    None
                );
            }
            "volatile-reopen" => {
                let cell = state::open_volatile("counter").map_err(invalid)?;
                let current = cell.load().map_err(invalid)?;
                assert_eq!(current.payload.as_deref(), Some(&b"volatile"[..]));
                cell.delete(current.revision).map_err(invalid)?;
                let deleted = cell.load().map_err(invalid)?;
                assert!(deleted.payload.is_none());
                assert_ne!(deleted.revision, current.revision);
                assert_eq!(
                    cell.compare_exchange(current.revision, Some(b"stale")),
                    Err(Error::Conflict)
                );
            }
            "volatile-invalid" => {
                assert!(
                    matches!(state::open_volatile(&"x".repeat(129)), Err(Error::Host { code, .. }) if code == "invalid")
                );
                let cell = state::open_volatile("limit").map_err(invalid)?;
                let payload = vec![7; state::MAX_PAYLOAD_BYTES];
                cell.compare_exchange(0, Some(&payload)).map_err(invalid)?;
                let saved = cell.load().map_err(invalid)?;
                assert!(
                    matches!(cell.compare_exchange(saved.revision, Some(&vec![0; state::MAX_PAYLOAD_BYTES + 1])), Err(Error::Host { code, .. }) if code == "resource-exhausted")
                );
                assert_eq!(cell.load().map_err(invalid)?, saved);
            }
            "volatile-open-error" => {
                assert_host_error(state::open_volatile("open-error").err(), "open-failed")
            }
            "volatile-load-error" => {
                let cell = state::open_volatile("load-error").map_err(invalid)?;
                assert_host_error(cell.load().err(), "load-failed");
            }
            "volatile-cas-error" => {
                let cell = state::open_volatile("cas-error").map_err(invalid)?;
                assert_host_error(
                    cell.compare_exchange(0, Some(b"private")).err(),
                    "cas-failed",
                );
            }
            "ws-freshness" => {
                let host = stravia_vendor_sdk::GuestHost::new();
                for (url, fresh, has_headers) in [
                    ("wss://fresh.invalid", true, false),
                    ("wss://reused.invalid", false, true),
                ] {
                    let socket = host.ws_connect(
                        stravia_vendor_sdk::net::ConnectRequest::new(url),
                        &stravia_vendor_sdk::net::Options::new(),
                    )?;
                    assert_eq!(socket.is_fresh_handshake()?, fresh);
                    assert_eq!(
                        socket.handshake_headers().iter().next().is_some(),
                        has_headers
                    );
                    assert_eq!(socket.is_fresh_handshake()?, fresh);
                }
            }
            "open-error" => assert_host_error(
                state::open(Scope::Provider, "open-error").err(),
                "open-failed",
            ),
            "load-error" => {
                let cell = state::open(Scope::Provider, "load-error").map_err(invalid)?;
                assert_host_error(cell.load().err(), "load-failed");
            }
            "cas-error" => {
                let cell = state::open(Scope::Provider, "cas-error").map_err(invalid)?;
                assert_host_error(
                    cell.compare_exchange(0, Some(b"private")).err(),
                    "cas-failed",
                );
            }
            "cancel" | "volatile-cancel" => {
                let cell = if mode == "cancel" {
                    state::open(Scope::Provider, "cancel")
                } else {
                    state::open_volatile("cancel")
                }
                .map_err(invalid)?;
                let _ = cell.load();
                return Err(invalid("cancelled load returned"));
            }
            _ => return Err(invalid("unknown mode")),
        }
        let body = serde_json::to_vec(&AiResponse::new(mode, "fixture-model")).map_err(invalid)?;
        host::emit_event(&types::CanonicalEvent::Completed(types::CanonicalPayload {
            format: 1,
            body: body.clone(),
        }))?;
        Ok(body)
    }

    fn sync_catalog(
        _: types::CanonicalPayload,
    ) -> Result<types::CanonicalPayload, types::PluginError> {
        Err(invalid("unsupported"))
    }
}

fn assert_host_error(error: Option<Error>, code: &str) {
    assert_eq!(
        error,
        Some(Error::Host {
            code: code.into(),
            message: "safe failure".into()
        })
    );
}

fn invalid(error: impl std::fmt::Display) -> types::PluginError {
    types::PluginError {
        kind: types::ErrorKind::Invalid,
        message: error.to_string(),
        upstream_status: None,
    }
}

stravia_vendor_sdk::bindings::export!(Fixture with_types_in stravia_vendor_sdk::bindings);
