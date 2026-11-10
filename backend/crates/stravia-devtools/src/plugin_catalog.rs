use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::Args as ClapArgs;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use stravia_core::plugin::distribution::{
    Artifact, Catalog, CatalogRelease, Compatibility, ComponentDigest, MAX_COMPONENT_BYTES,
    decode_component, encode_component, parse_catalog,
};
use stravia_vendor_runtime::VendorRuntime;

/// Generate compressed components and a v2 catalog. Signing is a separate operation.
#[derive(ClapArgs)]
pub struct Args {
    #[arg(long)]
    repository_id: u64,
    #[arg(long)]
    release_tag: String,
    #[arg(long)]
    host_version_req: String,
    /// RFC3339 timestamp used for generation and publication.
    #[arg(long)]
    published_at: String,
    /// JSON array of {file, vendor_id, version, display_name} declarations.
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    file: PathBuf,
    vendor_id: String,
    version: String,
    display_name: String,
}

pub async fn run(args: Args) -> Result<()> {
    let inputs: Vec<Input> = serde_json::from_slice(&std::fs::read(&args.input)?)?;
    ensure!(!inputs.is_empty(), "at least one component is required");
    let runtime = VendorRuntime::new()?;
    let mut releases = Vec::with_capacity(inputs.len());
    let mut assets = Vec::with_capacity(inputs.len());
    for input in inputs {
        let bytes = std::fs::read(&input.file)
            .with_context(|| format!("reading {}", input.file.display()))?;
        ensure!(
            bytes.len() <= MAX_COMPONENT_BYTES,
            "component exceeds size limit"
        );
        let plugin = runtime
            .load(&bytes)
            .await
            .with_context(|| format!("admitting {}", input.file.display()))?;
        let descriptor = plugin.descriptor();
        ensure!(descriptor.vendor_id != "base", "base cannot be published");
        ensure!(
            descriptor.vendor_id == input.vendor_id
                && descriptor.version.to_string() == input.version
                && descriptor.display_name == input.display_name,
            "declared identity or display name does not match descriptor: {}",
            input.file.display()
        );
        let compressed = encode_component(&bytes)?;
        let component = ComponentDigest {
            size: bytes.len() as u64,
            sha256: digest(&bytes),
        };
        ensure!(
            decode_component(&compressed, Some(&component))? == bytes,
            "compressed component failed round-trip"
        );
        let filename = format!(
            "stravia-vendor-{}-v{}.wasm.zst",
            descriptor.vendor_id, descriptor.version
        );
        releases.push(CatalogRelease {
            vendor_id: descriptor.vendor_id.clone(),
            version: descriptor.version.to_string(),
            display_name: descriptor.display_name.clone(),
            description: descriptor.description.clone(),
            authors: descriptor.authors.clone(),
            icon_svg: descriptor
                .providers
                .first()
                .and_then(|provider| provider.icon_svg.clone()),
            published_at: args.published_at.clone(),
            compatibility: Compatibility {
                host_version_req: args.host_version_req.clone(),
                wit_version: "0.6.0".into(),
                canonical_format_version: descriptor.canonical_format_version,
            },
            artifact: Artifact {
                filename: filename.clone(),
                compression: "zstd".into(),
                media_type: "application/zstd".into(),
                size: compressed.len() as u64,
                sha256: digest(&compressed),
            },
            component,
        });
        assets.push((filename, compressed));
    }
    let catalog = Catalog {
        schema_version: 2,
        repository_id: args.repository_id,
        release_tag: args.release_tag,
        generated_at: args.published_at,
        releases,
    };
    let json = serde_json::to_vec_pretty(&catalog)?;
    parse_catalog(&json).context("generated catalog failed validation")?;
    std::fs::create_dir_all(&args.output)?;
    for (filename, bytes) in assets {
        std::fs::write(args.output.join(filename), bytes)?;
    }
    std::fs::write(args.output.join("stravia-plugin-catalog.json"), json)?;
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hash = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut hash, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hash
}
