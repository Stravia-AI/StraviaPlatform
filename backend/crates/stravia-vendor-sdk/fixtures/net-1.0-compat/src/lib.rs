// Bind directly to the released snapshot, independently of the current SDK.
wit_bindgen::generate!({
    path: "../../wit/released/stravia-vendor@1.0.0",
    world: "vendor",
    generate_all,
});

mod released_state {
    wit_bindgen::generate!({ path: "wit-state", world: "state-imports", generate_all });
}

use released_state::stravia::state::store;
use serde_json::{Value, json};
use stravia::net::{http, types as net_types, websocket};
use stravia::vendor::{host, types};

struct Compat;

impl Guest for Compat {
    fn descriptor() -> String {
        json!({
            "vendor_id": "fixture.net-1-0-compat", "version": "1.0.0",
            "display_name": "Released net compatibility", "description": null,
            "canonical_format_version": 1, "kind": "dedicated",
            "providers": [{
                "provider_id": "fixture.net-1-0-compat", "catalog_id": null,
                "display_name": "Released net compatibility", "description": null,
                "capabilities": ["infer"],
                "channels": [{"id": "default", "name": {"en-US": "Compatibility", "zh-CN": "兼容性"},
                    "description": null, "protocol": "fixture-compat", "capabilities": ["infer"]}],
                "config_fields": [{"key": "wsUrl", "label": {"en-US": "WebSocket URL", "zh-CN": "WebSocket 地址"},
                    "description": null, "kind": {"type": "string"}}],
                "network": {"field_origins": ["wsUrl"]}
            }]
        }).to_string()
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
        operation: types::OperationKind,
        _: String,
        input: types::OperationInput,
    ) -> Result<Vec<u8>, types::PluginError> {
        if !matches!(operation, types::OperationKind::Infer) {
            return Err(invalid("unsupported operation"));
        }
        let cell =
            store::open(store::Scope::Provider, "released").map_err(|e| invalid(e.message))?;
        let initial = cell.load().map_err(|e| invalid(e.message))?;
        if initial.revision != 0 || initial.payload.is_some() {
            return Err(invalid("initial released state"));
        }
        if !cell
            .compare_exchange(0, Some(b"released-state"))
            .map_err(|e| invalid(e.message))?
        {
            return Err(invalid("released state CAS"));
        }
        let current = cell.load().map_err(|e| invalid(e.message))?;
        if current.payload.as_deref() != Some(&b"released-state"[..]) || current.revision == 0 {
            return Err(invalid("released state load"));
        }
        if cell
            .compare_exchange(0, None)
            .map_err(|e| invalid(e.message))?
        {
            return Err(invalid("released state stale CAS"));
        }
        let config: Value = serde_json::from_slice(&input.provider.options).map_err(invalid)?;
        let url = config["wsUrl"]
            .as_str()
            .ok_or_else(|| invalid("missing wsUrl"))?;
        let opts = net_types::Options::new();
        opts.set("http.version", "http1").map_err(net_error)?;
        host::emit_started()?;
        let response = http::start(
            &http::Request {
                method: "POST".into(),
                url: format!("{}/compat", input.provider.base_url.trim_end_matches('/')),
                headers: vec![net_types::Field {
                    name: "x-compat".into(),
                    bytes: b"snapshot".to_vec(),
                }],
                body: b"released-http".to_vec(),
            },
            &opts,
        )
        .map_err(net_error)?;
        if response.status().map_err(net_error)? != 200 {
            return Err(invalid("HTTP status"));
        }
        if !response
            .headers()
            .map_err(net_error)?
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case("x-compat-response") && f.bytes == b"http-ok")
        {
            return Err(invalid("HTTP response header"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.read_body().map_err(net_error)? {
            body.extend(chunk);
        }
        if body != b"http-response" {
            return Err(invalid("HTTP response body"));
        }
        let socket = websocket::connect(
            &websocket::ConnectRequest {
                url: url.into(),
                headers: vec![],
            },
            &opts,
        )
        .map_err(net_error)?;
        if socket.handshake_status() != 101 {
            return Err(invalid("WebSocket handshake"));
        }
        socket
            .send(&websocket::Message::Text("released-ws".into()))
            .map_err(net_error)?;
        let result = match socket.next().map_err(net_error)? {
            Some(websocket::Message::Text(text)) => text.into_bytes(),
            _ => return Err(invalid("WebSocket response")),
        };
        socket.close(None).map_err(net_error)?;
        host::emit_event(&types::CanonicalEvent::Completed(types::CanonicalPayload {
            format: 1,
            body: result.clone(),
        }))?;
        Ok(result)
    }

    fn sync_catalog(
        _: types::CanonicalPayload,
    ) -> Result<types::CanonicalPayload, types::PluginError> {
        Err(invalid("unsupported"))
    }
}

fn invalid(error: impl std::fmt::Display) -> types::PluginError {
    types::PluginError {
        kind: types::ErrorKind::Invalid,
        message: error.to_string(),
        upstream_status: None,
    }
}
fn net_error(error: net_types::NetError) -> types::PluginError {
    invalid(error.message)
}
export!(Compat);
