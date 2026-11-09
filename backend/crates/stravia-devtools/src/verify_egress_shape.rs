use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde_json::Value;

use crate::capture_client::hello;

/// Verify a component's egress ClientHello against a sanitized capture, offline.
#[derive(Args)]
pub struct VerifyEgressShapeArgs {
    /// Packaged vendor WebAssembly component.
    #[arg(long)]
    component: PathBuf,
    /// Plugin-owned shape selector (plugin:NAME).
    #[arg(long)]
    shape: String,
    /// Sanitized capture fixture JSON.
    #[arg(long)]
    fixture: PathBuf,
    /// DNS server name for the synthesized ClientHello.
    #[arg(long)]
    server_name: String,
    /// Select the shape's WebSocket TLS configuration.
    #[arg(long)]
    websocket: bool,
}

pub async fn run(args: VerifyEgressShapeArgs) -> Result<()> {
    let component =
        std::fs::read(&args.component).map_err(|_| anyhow::anyhow!("cannot read component"))?;
    let fixture =
        std::fs::read(&args.fixture).map_err(|_| anyhow::anyhow!("cannot read fixture"))?;
    let sample: Value = serde_json::from_slice(&fixture)
        .map_err(|_| anyhow::anyhow!("fixture is not valid JSON"))?;
    let expected = sample
        .get("clienthello")
        .and_then(Value::as_object)
        .context("fixture has no ClientHello object")?;
    let wire = stravia_core::plugin::synthesize_egress_client_hello(
        &component,
        &args.shape,
        &args.server_name,
        args.websocket,
    )
    .await?;
    let (_, actual) = hello::read_for_server_name(&mut wire.as_slice(), &args.server_name)
        .await
        .context("decoding synthesized ClientHello")?;
    for required in [
        "cipher_suites",
        "extensions",
        "extension_details",
        "supported_groups",
        "signature_algorithms",
        "key_share_groups",
        "alpn",
        "ja3",
        "ja4",
    ] {
        ensure!(
            expected.contains_key(required),
            "fixture missing required field {required}"
        );
    }
    for field in [
        "legacy_version",
        "cipher_suites",
        "extensions",
        "supported_groups",
        "ec_point_formats",
        "signature_algorithms",
        "signature_algorithms_cert",
        "certificate_compression",
        "supported_versions",
        "key_share_groups",
        "key_shares",
        "psk_modes",
        "padding_length",
        "sni_present",
        "alpn",
        "ja3",
        "ja3_hash",
        "ja4",
    ] {
        if let Some(value) = expected.get(field) {
            ensure!(
                actual.get(field) == Some(value),
                "ClientHello field mismatch: {field}"
            );
        }
    }
    let details = expected["extension_details"]
        .as_array()
        .context("fixture extension_details must be an array")?;
    let observed = actual["extension_details"]
        .as_array()
        .context("missing generated extension details")?;
    ensure!(
        details.len() == observed.len(),
        "ClientHello extension detail count mismatch"
    );
    for (index, (expected, actual)) in details.iter().zip(observed).enumerate() {
        for field in ["type", "length"] {
            ensure!(
                expected.get(field).is_some() && expected.get(field) == actual.get(field),
                "ClientHello extension {index} {field} mismatch"
            );
        }
        if let Some(body) = expected.get("body").filter(|body| !body.is_null()) {
            ensure!(
                actual.get("body") == Some(body),
                "ClientHello extension {index} body mismatch"
            );
        }
        if let Some(decoded) = expected.get("decoded").filter(|decoded| !decoded.is_null())
            && !contains_facts(&actual["decoded"], decoded)
        {
            bail!("ClientHello extension {index} decoded fields mismatch");
        }
    }
    println!("{{\"status\":\"ok\"}}");
    Ok(())
}

// Captures can omit fields that the current decoder knows; only compare recorded facts.
fn contains_facts(actual: &Value, expected: &Value) -> bool {
    match expected {
        Value::Object(fields) => actual.as_object().is_some_and(|actual| {
            fields.iter().all(|(key, value)| {
                actual
                    .get(key)
                    .is_some_and(|actual| contains_facts(actual, value))
            })
        }),
        _ => actual == expected,
    }
}
