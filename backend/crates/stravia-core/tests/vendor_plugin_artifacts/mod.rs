use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;
use stravia_core::Gateway;
use stravia_core::plugin::{ConfirmPluginUpdate, PluginSummary};

#[derive(Deserialize)]
struct DistributionRecord {
    vendor_id: String,
    file: String,
}

fn distribution_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-plugins-all")
}

pub async fn install_distributed_vendor_plugin(
    gateway: &Gateway,
    vendor_id: &str,
) -> anyhow::Result<PluginSummary> {
    let directory = distribution_directory();
    let manifest_path = directory.join("manifest.json");
    let records: Vec<DistributionRecord> =
        serde_json::from_slice(&std::fs::read(&manifest_path).with_context(|| {
            format!(
                "missing Vendor distribution manifest {}; run `task build:vendors:all`",
                manifest_path.display()
            )
        })?)?;
    let record = records
        .into_iter()
        .find(|record| record.vendor_id == vendor_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "vendor {vendor_id} is missing from distribution manifest {}; run `task build:vendors:all`",
                manifest_path.display()
            )
        })?;
    let artifact_path = directory.join(record.file);
    let component = std::fs::read(&artifact_path).with_context(|| {
        format!(
            "missing Vendor distribution artifact {}; run `task build:vendors:all`",
            artifact_path.display()
        )
    })?;
    let preview = gateway.admin().preview_vendor_plugin(component).await?;
    anyhow::ensure!(
        preview.vendor_id == vendor_id,
        "distribution artifact for {vendor_id} previews as {}",
        preview.vendor_id
    );
    gateway
        .admin()
        .confirm_vendor_plugin(ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await
}
