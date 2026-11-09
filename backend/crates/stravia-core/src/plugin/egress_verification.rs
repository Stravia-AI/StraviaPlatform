//! Offline synthesis of a component's production egress ClientHello.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use rustls::{ClientConnection, pki_types::ServerName};
use stravia_vendor_runtime::VendorRuntime;

use super::network::shape::ShapeRegistry;

/// Load a component and synthesize its selected ClientHello without opening a socket.
/// The returned wire bytes contain ephemeral TLS material and must not be logged.
pub async fn synthesize_egress_client_hello(
    component: &[u8],
    shape: &str,
    server_name: &str,
    websocket: bool,
) -> Result<Vec<u8>> {
    ensure!(
        shape.starts_with("plugin:"),
        "shape must use the plugin: namespace"
    );
    let runtime = VendorRuntime::new().context("initializing component runtime")?;
    // Guest-provided error text is deliberately not propagated.
    let loaded = runtime
        .load(component)
        .await
        .map_err(|_| anyhow::anyhow!("component loading or descriptor validation failed"))?;
    let registry = Arc::new(ShapeRegistry::builtin().context("initializing builtin shapes")?);
    let registry = registry
        .with_plugin(loaded.identity(), &loaded.descriptor().egress_shapes)
        .map_err(|_| anyhow::anyhow!("component egress shape compilation failed"))?;
    let resolved = registry
        .get(shape)
        .context("selected plugin shape not found")?;
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|_| anyhow::anyhow!("invalid server name"))?;
    let mut connection = ClientConnection::new(resolved.tls_for("direct", websocket), name)
        .map_err(|_| anyhow::anyhow!("constructing TLS ClientHello failed"))?;
    let mut wire = Vec::new();
    connection
        .write_tls(&mut wire)
        .context("serializing TLS ClientHello")?;
    Ok(wire)
}
