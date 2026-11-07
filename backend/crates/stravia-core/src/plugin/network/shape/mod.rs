//! Validated, built-in egress shapes. A shape does not contain request fields or credentials.

mod h2;
mod kx;
#[cfg(test)]
pub(crate) mod tests;
mod tls;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};

use super::transport::H2Settings;

#[derive(Debug, thiserror::Error)]
#[error("{field}: {message}")]
pub(crate) struct ShapeError {
    field: String,
    message: String,
}

impl ShapeError {
    fn field(field: &str, message: impl fmt::Display) -> Self {
        Self {
            field: field.to_owned(),
            message: message.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EgressShape {
    pub name: String,
    pub revision: u64,
    #[serde(default)]
    pub source: Option<ShapeSource>,
    pub tls: TlsShape,
    #[serde(default)]
    pub http1: Http1Shape,
    #[serde(default)]
    pub http2: Http2Shape,
    #[serde(default)]
    pub ws: WsShape,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShapeSource {
    pub client: String,
    pub runtime: String,
    pub captured: String,
    pub ja4: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TlsShape {
    pub versions: Vec<String>,
    pub cipher_suites: Vec<String>,
    pub advertise_only_cipher_suites: Vec<String>,
    pub groups: Vec<String>,
    pub key_shares: Vec<String>,
    pub signature_algorithms: Vec<String>,
    #[serde(deserialize_with = "deserialize_extension_order")]
    pub extension_order: Option<Vec<u16>>,
    pub omit_extensions: Vec<u16>,
    pub raw_extensions: Vec<RawExtensionShape>,
    pub padding: Option<String>,
    pub renegotiation: Option<String>,
    pub ec_point_formats: Option<Vec<u8>>,
    pub cert_compression: Vec<String>,
    #[serde(default = "default_true")]
    pub session_tickets: bool,
    pub alpn: AlpnShape,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawExtensionShape {
    #[serde(rename = "type")]
    pub typ: u16,
    #[serde(deserialize_with = "deserialize_bytes")]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AlpnShape {
    pub http: Vec<String>,
    pub ws: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Http1Shape {
    pub slot_positions: SlotPositions,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SlotPositions {
    pub host: Option<String>,
    pub content_length: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Http2Shape {
    pub settings: Vec<(String, u32)>,
    pub early_frames: Vec<EarlyFrameShape>,
    pub pseudo_order: Vec<String>,
    pub content_length: String,
    pub keepalive: KeepaliveShape,
}

impl Default for Http2Shape {
    fn default() -> Self {
        Self {
            settings: Vec::new(),
            early_frames: Vec::new(),
            pseudo_order: Vec::new(),
            content_length: "auto".into(),
            keepalive: KeepaliveShape::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum EarlyFrameShape {
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    Priority {
        stream_id: u32,
        dependency_id: u32,
        weight: u8,
        exclusive: bool,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct KeepaliveShape {
    pub interval_ms: Option<u64>,
    pub timeout_ms: u64,
    pub while_idle: bool,
}

impl Default for KeepaliveShape {
    fn default() -> Self {
        Self {
            interval_ms: Some(15_000),
            timeout_ms: 10_000,
            while_idle: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WsShape {
    pub over_h2: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExtensionOrderInput {
    Random(String),
    List(Vec<WireU16>),
}
#[derive(Deserialize)]
#[serde(untagged)]
enum WireU16 {
    Number(u16),
    Hex(String),
}

fn deserialize_extension_order<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<u16>>, D::Error> {
    use serde::de::Error as _;
    match Option::<ExtensionOrderInput>::deserialize(deserializer)? {
        None => Ok(None),
        Some(ExtensionOrderInput::Random(s)) if s == "random" => Ok(None),
        Some(ExtensionOrderInput::Random(_)) => Err(D::Error::custom(
            "expected random or an ordered extension list",
        )),
        Some(ExtensionOrderInput::List(values)) => values
            .into_iter()
            .map(|v| match v {
                WireU16::Number(n) => Ok(n),
                WireU16::Hex(s) => u16::from_str_radix(
                    s.strip_prefix("0x")
                        .ok_or_else(|| D::Error::custom("extension type must be 0x-prefixed"))?,
                    16,
                )
                .map_err(D::Error::custom),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
    }
}

fn deserialize_bytes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    use serde::de::Error as _;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Bytes {
        Hex(String),
        Array(Vec<u8>),
    }
    match Bytes::deserialize(deserializer)? {
        Bytes::Array(bytes) => Ok(bytes),
        Bytes::Hex(s) => {
            let hex = s.strip_prefix("0x").unwrap_or(&s);
            if !hex.len().is_multiple_of(2) || !hex.is_ascii() {
                return Err(D::Error::custom("expected an even-length hex string"));
            }
            hex.as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    let pair = std::str::from_utf8(pair).map_err(D::Error::custom)?;
                    u8::from_str_radix(pair, 16).map_err(D::Error::custom)
                })
                .collect()
        }
    }
}

/// Fully validated shape with reusable, proxy-isolated TLS session stores.
pub(crate) struct ResolvedShape {
    pub name: String,
    pub revision: u64,
    pub http1: Http1Shape,
    pub http2: H2Settings,
    pub http2_content_length: String,
    pub ws: WsShape,
    http_tls: Arc<tls::ResolvedTls>,
    ws_tls: Arc<tls::ResolvedTls>,
}

pub(crate) use tls::SessionPolicy;

impl ResolvedShape {
    pub fn session_policy(&self, websocket: bool) -> SessionPolicy {
        if websocket {
            self.ws_tls.session_policy()
        } else {
            self.http_tls.session_policy()
        }
    }

    pub fn tls_for(&self, proxy_identity: &str, websocket: bool) -> Arc<rustls::ClientConfig> {
        if websocket {
            self.ws_tls.tls_for(proxy_identity, true)
        } else {
            self.http_tls.tls_for(proxy_identity, false)
        }
    }
}

pub(crate) struct ShapeRegistry {
    shapes: HashMap<String, ResolvedShape>,
    proxy_tls: Arc<rustls::ClientConfig>,
}

const BUILTIN_SHAPES: &[&str] = &[
    include_str!("data/default.json"),
    include_str!("data/claude-code.json"),
    include_str!("data/bun.json"),
    include_str!("data/bun-1.3.14.json"),
    include_str!("data/antigravity.json"),
    include_str!("data/codex-http.json"),
    include_str!("data/codex-ws.json"),
    include_str!("data/command-code.json"),
    include_str!("data/devin.json"),
    include_str!("data/grok.json"),
];

impl ShapeRegistry {
    pub fn builtin() -> Result<Self, ShapeError> {
        Self::from_json_sources(BUILTIN_SHAPES)
    }

    fn from_json_sources(sources: &[&str]) -> Result<Self, ShapeError> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new(provider)
            .map_err(|error| ShapeError::field("verifier", error))?;
        Self::from_json_sources_with_verifier(sources, Arc::new(verifier))
    }

    #[cfg(test)]
    pub(crate) fn builtin_with_verifier(
        verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    ) -> Result<Self, ShapeError> {
        Self::from_json_sources_with_verifier(BUILTIN_SHAPES, verifier)
    }

    fn from_json_sources_with_verifier(
        sources: &[&str],
        verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    ) -> Result<Self, ShapeError> {
        let mut shapes = HashMap::new();
        for source in sources {
            let shape: EgressShape =
                serde_json::from_str(source).map_err(|e| ShapeError::field("shape", e))?;
            if shape.name.is_empty()
                || !shape
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(ShapeError::field(
                    "name",
                    "must contain lowercase ASCII letters, digits or hyphens",
                ));
            }
            if shape.revision == 0 {
                return Err(ShapeError::field("revision", "must be positive"));
            }
            if shapes.contains_key(&shape.name) {
                return Err(ShapeError::field(
                    "name",
                    format!("duplicate shape {}", shape.name),
                ));
            }
            if let Some(source) = &shape.source {
                for (field, text) in [
                    ("source.client", &source.client),
                    ("source.runtime", &source.runtime),
                    ("source.captured", &source.captured),
                    ("source.ja4", &source.ja4),
                ] {
                    if text.trim().is_empty() {
                        return Err(ShapeError::field(
                            field,
                            "capture provenance must not be empty",
                        ));
                    }
                }
            }
            for (field, position) in [
                (
                    "http1.slot_positions.host",
                    &shape.http1.slot_positions.host,
                ),
                (
                    "http1.slot_positions.content_length",
                    &shape.http1.slot_positions.content_length,
                ),
            ] {
                if position
                    .as_ref()
                    .is_some_and(|p| p != "first" && p != "last")
                {
                    return Err(ShapeError::field(field, "expected first or last"));
                }
            }
            let http2 = h2::resolve(&shape.http2)?;
            let http_tls = Arc::new(tls::ResolvedTls::build(&shape.tls, Arc::clone(&verifier))?);
            let ws_tls = http_tls.clone();
            let resolved = ResolvedShape {
                name: shape.name.clone(),
                revision: shape.revision,
                http1: shape.http1,
                http2,
                http2_content_length: shape.http2.content_length,
                ws: shape.ws,
                http_tls,
                ws_tls,
            };
            shapes.insert(shape.name, resolved);
        }
        let default = shapes
            .get("default")
            .ok_or_else(|| ShapeError::field("name", "default shape is required"))?;
        let mut proxy_tls = (*default.tls_for("proxy-template", false)).clone();
        proxy_tls.client_hello_shape = None;
        proxy_tls.alpn_protocols.clear();
        proxy_tls.resumption = rustls::client::Resumption::in_memory_sessions(256);
        Ok(Self {
            shapes,
            proxy_tls: Arc::new(proxy_tls),
        })
    }

    pub fn get(&self, name: &str) -> Option<&ResolvedShape> {
        self.shapes.get(name)
    }

    /// Platform-verified TLS for a proxy, isolated from origin TLS sessions.
    pub fn proxy_tls(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.proxy_tls)
    }
}
