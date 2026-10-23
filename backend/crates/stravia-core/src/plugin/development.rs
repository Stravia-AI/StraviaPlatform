use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::Gateway;

use super::{ConfirmPluginUpdate, PluginSource};

#[derive(Deserialize)]
struct BuildRecord {
    vendor_id: String,
    file: String,
    sha256: String,
}

pub(crate) async fn reconcile(gw: &Gateway, manifest: &Path) -> anyhow::Result<()> {
    let directory = manifest
        .parent()
        .context("development manifest has no parent")?;
    let mut records: Vec<BuildRecord> =
        serde_json::from_slice(&tokio::fs::read(manifest).await.with_context(|| {
            format!("read development plugin manifest {}", manifest.display())
        })?)?;
    // 先建立专属归属，避免 base 更新把专属连接误判成自己的旧数据。
    records.sort_by_key(|record| record.vendor_id == "base");
    let installed = gw.vendor_plugins.store.list().await?;
    let bundled = gw.vendor_plugins.bundled();
    for record in records {
        if installed
            .iter()
            .any(|old| old.vendor_id == record.vendor_id && old.digest == record.sha256)
            && gw.vendor_plugins.acquire_package(&record.vendor_id).is_ok()
        {
            continue;
        }
        let path = directory.join(&record.file);
        let component = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read development plugin {}", path.display()))?;
        let digest = stravia_runtime_contract::protocol::ir::canonical::hash_hex(
            &Sha256::digest(&component).into(),
        );
        anyhow::ensure!(
            digest == record.sha256,
            "development plugin digest does not match manifest: {}",
            path.display()
        );
        // base 保持内嵌来源，避免开发启动把它永久变成本地替代包。
        let source = if bundled
            .plugins
            .get(&record.vendor_id)
            .is_some_and(|bundle| bundle.component == component.as_slice())
        {
            PluginSource::Builtin
        } else {
            PluginSource::Local
        };
        let preview = gw
            .vendor_plugins
            .preview(gw, component.into(), source, Some(&record.vendor_id))
            .await
            .with_context(|| format!("preview development plugin {}", record.vendor_id))?;
        if !preview.discarded_data.is_empty() {
            tracing::warn!(
                vendor_id = %record.vendor_id,
                artifact = %path.display(),
                discarded_data = ?preview.discarded_data,
                "development plugin update skipped; import the artifact in Vendor Plugins and explicitly confirm data discard"
            );
            continue;
        }
        gw.vendor_plugins
            .confirm(
                gw,
                ConfirmPluginUpdate {
                    preview_id: preview.id,
                    allow_data_discard: false,
                },
            )
            .await
            .with_context(|| format!("install development plugin {}", record.vendor_id))?;
        tracing::info!(vendor_id = %record.vendor_id, "installed current development plugin");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use crate::db::models::{CreateProvider, ProviderCredentialInput, ProviderSourceInput};

    async fn manifest(directory: &Path, artifact: &str) -> anyhow::Result<std::path::PathBuf> {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target/vendor-test-fixtures")
            .join(artifact);
        let component = tokio::fs::read(source).await?;
        let sha256 = stravia_runtime_contract::protocol::ir::canonical::hash_hex(
            &Sha256::digest(&component).into(),
        );
        tokio::fs::write(directory.join("plugin.wasm"), component).await?;
        let path = directory.join("manifest.json");
        tokio::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!([{
                "vendor_id": "fixture.lifecycle", "file": "plugin.wasm", "sha256": sha256,
            }]))?,
        )
        .await?;
        Ok(path)
    }

    #[tokio::test]
    async fn updates_changed_components_without_reinstalling_identical_bytes() -> anyhow::Result<()>
    {
        let data = tempfile::tempdir()?;
        let build = tempfile::tempdir()?;
        let gw = Gateway::new(GatewayConfig {
            data_dir: data.path().to_owned(),
            ..Default::default()
        })
        .await?;
        let path = manifest(build.path(), "lifecycle-v1.wasm").await?;
        reconcile(&gw, &path).await?;
        let installed = gw.vendor_plugins.store.list().await?;
        let old = installed
            .iter()
            .find(|p| p.vendor_id == "fixture.lifecycle")
            .unwrap();
        reconcile(&gw, &path).await?;
        let unchanged = gw.vendor_plugins.store.list().await?;
        let unchanged = unchanged
            .iter()
            .find(|p| p.vendor_id == old.vendor_id)
            .unwrap();
        assert_eq!(unchanged.revision, old.revision);

        // custom section 改变构建字节但不改变版本，覆盖源码更新后未升版的开发场景。
        let mut component = tokio::fs::read(build.path().join("plugin.wasm")).await?;
        component.extend_from_slice(&[0, 2, 1, b'x']);
        let digest = stravia_runtime_contract::protocol::ir::canonical::hash_hex(
            &Sha256::digest(&component).into(),
        );
        tokio::fs::write(build.path().join("plugin.wasm"), component).await?;
        tokio::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!([{
                "vendor_id": "fixture.lifecycle", "file": "plugin.wasm", "sha256": digest,
            }]))?,
        )
        .await?;
        reconcile(&gw, &path).await?;
        let changed = gw.vendor_plugins.store.list().await?;
        let changed = changed
            .iter()
            .find(|p| p.vendor_id == old.vendor_id)
            .unwrap();
        assert_eq!(changed.version, old.version);
        assert_eq!(changed.digest, digest);
        assert_ne!(changed.digest, old.digest);
        gw.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn incompatible_update_preserves_installed_plugin_and_private_data() -> anyhow::Result<()>
    {
        let data = tempfile::tempdir()?;
        let build = tempfile::tempdir()?;
        let gw = Gateway::new(GatewayConfig {
            data_dir: data.path().to_owned(),
            ..Default::default()
        })
        .await?;
        let path = manifest(build.path(), "lifecycle-v2.wasm").await?;
        reconcile(&gw, &path).await?;
        let provider = gw
            .admin()
            .create_provider(CreateProvider {
                name: Some("Development compatibility".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "fixture.lifecycle".into(),
                    channel: "default".into(),
                    protocol: Some("fixture-lifecycle".into()),
                    base_url: "http://127.0.0.1:1".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "local-fixture-key".into(),
                },
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await?;
        let installed = gw.vendor_plugins.store.list().await?;
        let old = installed
            .iter()
            .find(|p| p.vendor_id == "fixture.lifecycle")
            .unwrap();
        gw.vendor_plugins
            .store
            .write_private_state(
                &old.vendor_id,
                &provider.id,
                old.data_epoch,
                "1",
                b"retained development state",
            )
            .await?;

        manifest(build.path(), "lifecycle-v3.wasm").await?;
        reconcile(&gw, &path).await?;
        let plugins = gw.admin().list_vendor_plugins().await?;
        assert_eq!(
            plugins
                .iter()
                .find(|p| p.vendor_id == old.vendor_id)
                .unwrap()
                .version,
            "2.0.0"
        );
        assert_eq!(
            gw.vendor_plugins
                .store
                .read_private_state(&old.vendor_id, &provider.id)
                .await?,
            Some(("1".into(), b"retained development state".to_vec())),
        );
        gw.shutdown().await;
        Ok(())
    }
}
