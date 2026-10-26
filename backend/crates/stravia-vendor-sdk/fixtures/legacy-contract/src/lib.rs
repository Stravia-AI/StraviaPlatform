use std::io::{Read as _, Write as _};

use serde_json::{Value, json};

// 使用当前 WIT 的手动编码入口，保护原始 HTTP/WS 字节契约。
wit_bindgen::generate!({
    path: "../../wit",
    world: "vendor",
    generate_all,
});

use stravia::vendor::{host, types};

struct LegacyContract;

impl Guest for LegacyContract {
    fn descriptor() -> String {
        json!({
            "vendor_id": "fixture.legacy-abi",
            "version": "1.0.0",
            "display_name": "Legacy ABI Fixture",
            "description": null,
            "canonical_format_version": 1,
            "kind": "dedicated",
            "providers": [{
                "provider_id": "fixture.legacy-abi",
                "catalog_id": null,
                "display_name": "Legacy ABI Fixture",
                "description": null,
                "capabilities": ["infer"],
                "channels": [{
                    "id": "default",
                    "name": {"en-US": "Legacy ABI", "zh-CN": "旧版 ABI"},
                    "description": null,
                    "protocol": "fixture-lifecycle",
                    "capabilities": ["infer"]
                }],
                "config_fields": [
                    {"key": "mode", "label": {"en-US": "Mode", "zh-CN": "模式"},
                     "description": null, "kind": {"type": "string"}},
                    {"key": "auxUrl", "label": {"en-US": "WebSocket URL", "zh-CN": "WebSocket 地址"},
                     "description": null, "kind": {"type": "string"}},
                    {"key": "apiKey", "label": {"en-US": "API key", "zh-CN": "API 密钥"},
                     "description": null, "kind": {"type": "string"}, "secret": true}
                ],
                "network": {"field_origins": ["auxUrl"]}
            }]
        })
        .to_string()
    }

    fn select_protocol(
        _operation: types::OperationKind,
        _channel: String,
        provider: types::ProviderSnapshot,
        _request: types::CanonicalPayload,
    ) -> Result<String, types::PluginError> {
        Ok(provider.protocol)
    }

    fn execute(
        operation: types::OperationKind,
        _channel: String,
        input: types::OperationInput,
    ) -> Result<Vec<u8>, types::PluginError> {
        if !matches!(operation, types::OperationKind::Infer) {
            return Err(failure(
                types::ErrorKind::Unsupported,
                "unsupported legacy operation",
            ));
        }
        let options: Value = serde_json::from_slice(&input.provider.options).map_err(invalid)?;
        let secrets: Value = serde_json::from_slice(&input.provider.secrets).map_err(invalid)?;
        let mode = options["mode"].as_str().unwrap_or("manual");
        let mut headers = vec![(
            "authorization".into(),
            format!("Bearer {}", secrets["apiKey"].as_str().unwrap_or_default()),
        )];
        host::emit_started()?;
        let body = if mode == "websocket" {
            headers.push(("connection".into(), "keep-alive".into()));
            headers.push(("accept-encoding".into(), "gzip".into()));
            let socket = host::ws_connect(&types::WsRequest {
                url: options["auxUrl"]
                    .as_str()
                    .ok_or_else(|| invalid("missing legacy WebSocket URL"))?
                    .into(),
                headers,
                protocols: vec!["legacy-v1".into()],
                continuation_id: None,
                reuse_max_age_millis: Some(0),
            })?;
            socket.send(&types::WsMessage::Text(
                String::from_utf8(input.input.body).map_err(invalid)?,
            ))?;
            let body = loop {
                match socket.next()? {
                    Some(types::WsMessage::Text(text)) => break text.into_bytes(),
                    Some(types::WsMessage::Ping(_) | types::WsMessage::Pong(_)) => {}
                    _ => return Err(invalid("legacy WebSocket ended without a response")),
                }
            };
            socket.close(None)?;
            body
        } else {
            let mut body = input.input.body;
            if mode == "manual" {
                headers.push(("accept-encoding".into(), "gzip".into()));
            } else if mode == "preencoded" {
                headers.push(("content-encoding".into(), "gzip".into()));
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&body).map_err(invalid)?;
                body = encoder.finish().map_err(invalid)?;
            }
            let response = host::http_start(&types::HttpRequest {
                method: "POST".into(),
                url: format!("{}/infer", input.provider.base_url.trim_end_matches('/')),
                headers,
                body,
            })?;
            if response.status()? != 200 {
                return Err(invalid("legacy HTTP failed"));
            }
            let response_headers = response.headers()?;
            let mut body = Vec::new();
            while let Some(chunk) = response.read_body()? {
                if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
                    return Err(failure(
                        types::ErrorKind::ResourceExhausted,
                        "legacy body limit",
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            if mode == "raw" {
                if !body.starts_with(&[0x1f, 0x8b])
                    || !response_headers.iter().any(|(name, value)| {
                        name.eq_ignore_ascii_case("content-encoding") && value == "gzip"
                    })
                    || !response_headers.iter().any(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            && value == &body.len().to_string()
                    })
                {
                    return Err(invalid(
                        "legacy raw response or encoding headers were changed",
                    ));
                }
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(body.as_slice())
                    .read_to_end(&mut decoded)
                    .map_err(invalid)?;
                decoded
            } else {
                if mode == "manual"
                    && response_headers.iter().any(|(name, _)| {
                        name.eq_ignore_ascii_case("content-encoding")
                            || name.eq_ignore_ascii_case("content-length")
                    })
                {
                    return Err(invalid(
                        "legacy negotiated response headers were not decoded",
                    ));
                }
                body
            }
        };
        let event = types::CanonicalEvent::Completed(types::CanonicalPayload { format: 1, body });
        host::emit_event(&event)?;
        let types::CanonicalEvent::Completed(payload) = event else {
            unreachable!()
        };
        Ok(payload.body)
    }

    fn sync_catalog(
        _input: types::CanonicalPayload,
    ) -> Result<types::CanonicalPayload, types::PluginError> {
        Err(failure(
            types::ErrorKind::Unsupported,
            "no legacy runtime catalog",
        ))
    }
}

fn invalid(error: impl std::fmt::Display) -> types::PluginError {
    failure(types::ErrorKind::Invalid, error.to_string())
}

fn failure(kind: types::ErrorKind, message: impl Into<String>) -> types::PluginError {
    types::PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

export!(LegacyContract);
