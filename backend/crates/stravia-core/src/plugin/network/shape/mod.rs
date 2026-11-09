//! Validated builtin and component-owned egress shapes, without request fields or credentials.

mod h2;
mod kx;
#[cfg(test)]
pub(crate) mod tests;
mod tls;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use stravia_vendor_sdk::net::shape::{
    EarlyFrameShape, EgressShape, Http1Shape, Http2Shape, TlsShape, WsShape,
};

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

/// Fully validated shape with reusable, proxy-isolated TLS session stores.
pub(crate) struct ResolvedShape {
    pub identity: Arc<str>,
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
    shapes: HashMap<String, Arc<ResolvedShape>>,
    proxy_tls: Arc<rustls::ClientConfig>,
    verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
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
            let name = shape.name.clone();
            let identity = format!("builtin:{}:{}", name, shape.revision);
            let resolved = Self::compile(&shape, identity, &verifier)?;
            if shapes.insert(name.clone(), Arc::new(resolved)).is_some() {
                return Err(ShapeError::field("name", format!("duplicate shape {name}")));
            }
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
            verifier,
        })
    }

    fn compile(
        shape: &EgressShape,
        identity: String,
        verifier: &Arc<dyn rustls::client::danger::ServerCertVerifier>,
    ) -> Result<ResolvedShape, ShapeError> {
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
        let http_tls = Arc::new(tls::ResolvedTls::build(&shape.tls, Arc::clone(verifier))?);
        Ok(ResolvedShape {
            identity: Arc::from(identity),
            http1: shape.http1.clone(),
            http2,
            http2_content_length: shape.http2.content_length.clone(),
            ws: shape.ws.clone(),
            ws_tls: Arc::clone(&http_tls),
            http_tls,
        })
    }

    /// Compile a component's definitions while retaining the base verifier and builtins.
    pub fn with_plugin(
        self: &Arc<Self>,
        component_identity: &str,
        declarations: &[EgressShape],
    ) -> Result<Arc<Self>, ShapeError> {
        if declarations.is_empty() && self.shapes.keys().all(|name| !name.starts_with("plugin:")) {
            return Ok(Arc::clone(self));
        }
        let mut shapes: HashMap<_, _> = self
            .shapes
            .iter()
            .filter(|(name, _)| !name.starts_with("plugin:"))
            .map(|(name, shape)| (name.clone(), Arc::clone(shape)))
            .collect();
        for shape in declarations {
            let name = format!("plugin:{}", shape.name);
            let identity = format!(
                "plugin:{}:{}:{}:{}",
                component_identity.len(),
                component_identity,
                shape.name,
                shape.revision
            );
            let resolved = Self::compile(shape, identity, &self.verifier)?;
            if shapes.insert(name.clone(), Arc::new(resolved)).is_some() {
                return Err(ShapeError::field("name", format!("duplicate shape {name}")));
            }
        }
        Ok(Arc::new(Self {
            shapes,
            proxy_tls: Arc::clone(&self.proxy_tls),
            verifier: Arc::clone(&self.verifier),
        }))
    }

    pub fn get(&self, name: &str) -> Option<&ResolvedShape> {
        self.shapes.get(name).map(Arc::as_ref)
    }

    /// Platform-verified TLS for a proxy, isolated from origin TLS sessions.
    pub fn proxy_tls(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.proxy_tls)
    }
}
