use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::Deserialize;
use stravia_runtime_contract::protocol::ir::canonical::{hash_bytes, hash_hex};

#[path = "../stravia-vendor-sdk/build-support/messages.rs"]
mod messages;

#[derive(Deserialize)]
struct ComponentRecord {
    vendor_id: String,
    version: String,
    file: String,
    sha256: String,
}

fn main() -> anyhow::Result<()> {
    messages::generate().expect("compile host language catalogs");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").context("missing CARGO_MANIFEST_DIR")?,
    );
    let root = manifest_dir
        .ancestors()
        .nth(3)
        .context("Core is outside the workspace layout")?;
    let directory = root.join("target/vendor-plugins");
    let manifest = directory.join("manifest.json");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let records: Vec<ComponentRecord> = serde_json::from_slice(&fs::read(&manifest).context(
        "builtin components are missing; run `task build:vendors` before invoking Cargo directly",
    )?)?;
    let mut seen = BTreeSet::new();
    let mut source = String::from("pub(crate) const COMPONENTS: &[(&str, &[u8])] = &[\n");
    // 即使旧构建清单仍含其他组件，程序也只随附基础包。
    for record in records
        .into_iter()
        .filter(|record| record.vendor_id == "base")
    {
        ensure!(
            seen.insert(record.vendor_id.clone()),
            "duplicate builtin vendor id"
        );
        ensure!(
            !record.vendor_id.is_empty()
                && record
                    .vendor_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
            "invalid builtin vendor id"
        );
        ensure!(
            !record.version.is_empty(),
            "builtin component version is missing"
        );
        ensure!(
            Path::new(&record.file)
                .file_name()
                .is_some_and(|name| name == record.file.as_str()),
            "builtin component filename must not contain a directory"
        );
        let path = directory.join(&record.file);
        println!("cargo:rerun-if-changed={}", path.display());
        let component = fs::read(&path)
            .with_context(|| format!("missing builtin component for {}", record.vendor_id))?;
        ensure!(
            hash_hex(&hash_bytes(&component)) == record.sha256,
            "builtin component digest mismatch for {}; rebuild with `task build:vendors`",
            record.vendor_id
        );
        source.push_str(&format!(
            "    ({:?}, include_bytes!({:?})),\n",
            record.vendor_id,
            path.to_string_lossy()
        ));
    }
    ensure!(seen.contains("base"), "builtin base component is missing");
    source.push_str("];\n");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").context("missing OUT_DIR")?);
    fs::write(output.join("vendor_components.rs"), source)?;
    Ok(())
}
