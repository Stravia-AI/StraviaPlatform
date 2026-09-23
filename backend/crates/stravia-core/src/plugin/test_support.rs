//! 测试辅助：把 `task build:vendors:all` 产出的专属 Vendor 组件经真实
//! preview + confirm 路径装入测试 Gateway。base 只为 Catalog profile 提供
//! 兜底实现，openai-codex、xai-grok、command-code、devin 等专属 Vendor 必须
//! 显式安装；安装须先于绑定该 Vendor 的 Provider，否则预览会把既有连接数据
//! 判定为不兼容并要求确认丢弃。

use std::path::PathBuf;

use anyhow::Context as _;
use serde::Deserialize;

use crate::Gateway;
use crate::plugin::ConfirmPluginUpdate;

#[derive(Deserialize)]
struct DistributionRecord {
    vendor_id: String,
    file: String,
}

fn distribution_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-plugins-all")
}

pub(crate) async fn install_distributed_vendor(
    gateway: &Gateway,
    vendor_id: &str,
) -> anyhow::Result<()> {
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
                "vendor `{vendor_id}` is missing from distribution manifest {}; run `task build:vendors:all`",
                manifest_path.display()
            )
        })?;
    let artifact_path = directory.join(&record.file);
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
        .await?;
    Ok(())
}
