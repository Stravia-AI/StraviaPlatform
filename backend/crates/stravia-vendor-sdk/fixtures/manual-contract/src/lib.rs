use std::io::{Read as _, Write as _};

use serde_json::{Value, json};

// 使用当前 WIT 的手动编码入口，保护原始 HTTP/WS 字节契约。
wit_bindgen::generate!({
    path: "../../wit",
    world: "vendor",
    generate_all,
});

use stravia::net::{http, types as net_types, websocket};
use stravia::vendor::{host, types};

struct ManualContract;

impl Guest for ManualContract {
    fn descriptor() -> String {
        json!({
            "vendor_id": "fixture.manual-entrypoints",
            "version": "1.0.0",
            "display_name": "Manual ABI Fixture",
            "description": null,
            "canonical_format_version": 1,
            "kind": "dedicated",
            "providers": [{
                "provider_id": "fixture.manual-entrypoints",
                "catalog_id": null,
                "display_name": "Manual ABI Fixture",
                "description": null,
                "capabilities": ["infer"],
                "channels": [{
                    "id": "default",
                    "name": {"en-US": "Manual ABI", "zh-CN": "手动入口"},
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
                "unsupported manual operation",
            ));
        }
        let options: Value = serde_json::from_slice(&input.provider.options).map_err(invalid)?;
        let secrets: Value = serde_json::from_slice(&input.provider.secrets).map_err(invalid)?;
        let mode = options["mode"].as_str().unwrap_or("manual");
        let mut headers = vec![net_types::Field {
            name: "authorization".into(),
            bytes: format!("Bearer {}", secrets["apiKey"].as_str().unwrap_or_default())
                .into_bytes(),
        }];
        let net_options = net_types::Options::new();
        net_options.set("shape", "default").map_err(net_error)?;
        host::emit_started()?;
        let body = if mode == "websocket" {
            headers.push(field("connection", ""));
            headers.push(field("accept-encoding", "gzip"));
            headers.push(field("sec-websocket-protocol", "manual-v1"));
            net_options
                .set("ws.reuse-max-age-millis", "0")
                .map_err(net_error)?;
            let socket = websocket::connect(
                &websocket::ConnectRequest {
                    url: options["auxUrl"]
                        .as_str()
                        .ok_or_else(|| invalid("missing manual WebSocket URL"))?
                        .into(),
                    headers,
                },
                &net_options,
            )
            .map_err(net_error)?;
            socket
                .send(&websocket::Message::Text(
                    String::from_utf8(input.input.body).map_err(invalid)?,
                ))
                .map_err(net_error)?;
            let body = loop {
                match socket.next().map_err(net_error)? {
                    Some(websocket::Message::Text(text)) => break text.into_bytes(),
                    Some(websocket::Message::Ping(_) | websocket::Message::Pong(_)) => {}
                    _ => return Err(invalid("manual WebSocket ended without a response")),
                }
            };
            socket.close(None).map_err(net_error)?;
            body
        } else {
            let mut body = input.input.body;
            if mode == "manual" {
                headers.push(field("accept-encoding", "gzip"));
                net_options
                    .set("http.response-decoding", "gzip")
                    .map_err(net_error)?;
            } else if mode == "ordered-headers" {
                headers.push(field("X-Wire-Duplicate", "first"));
                headers.push(field("x-Wire-Between", "middle"));
                headers.push(net_types::Field {
                    name: "x-wire-duplicate".into(),
                    bytes: vec![0x80, 0xff],
                });
            } else if mode == "preencoded" {
                headers.push(field("content-encoding", "gzip"));
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&body).map_err(invalid)?;
                body = encoder.finish().map_err(invalid)?;
            }
            let response = http::start(
                &http::Request {
                    method: "POST".into(),
                    url: format!("{}/infer", input.provider.base_url.trim_end_matches('/')),
                    headers,
                    body,
                },
                &net_options,
            )
            .map_err(net_error)?;
            if response.status().map_err(net_error)? != 200 {
                return Err(invalid("manual HTTP failed"));
            }
            let response_headers = response.headers().map_err(net_error)?;
            let mut body = Vec::new();
            while let Some(chunk) = response.read_body().map_err(net_error)? {
                if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
                    return Err(failure(
                        types::ErrorKind::ResourceExhausted,
                        "manual body limit",
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            if mode == "raw" {
                if !body.starts_with(&[0x1f, 0x8b])
                    || !response_headers.iter().any(|field| {
                        field.name.eq_ignore_ascii_case("content-encoding")
                            && field.bytes == b"gzip"
                    })
                    || !response_headers.iter().any(|field| {
                        field.name.eq_ignore_ascii_case("content-length")
                            && field.bytes == body.len().to_string().as_bytes()
                    })
                {
                    return Err(invalid(
                        "manual raw response or encoding headers were changed",
                    ));
                }
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(body.as_slice())
                    .read_to_end(&mut decoded)
                    .map_err(invalid)?;
                decoded
            } else {
                if mode == "manual"
                    && response_headers.iter().any(|field| {
                        field.name.eq_ignore_ascii_case("content-encoding")
                            || field.name.eq_ignore_ascii_case("content-length")
                    })
                {
                    return Err(invalid(
                        "manual negotiated response headers were not decoded",
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
            "no manual runtime catalog",
        ))
    }
}

fn field(name: &str, value: &str) -> net_types::Field {
    net_types::Field {
        name: name.into(),
        bytes: value.as_bytes().to_vec(),
    }
}

fn net_error(error: net_types::NetError) -> types::PluginError {
    failure(types::ErrorKind::Invalid, error.message)
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

export!(ManualContract);
