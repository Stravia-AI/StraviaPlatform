use super::*;
use crate::db::models::CreateProviderRecord;
use crate::storage::DynStorage;

pub(super) fn provider(name: &str, vendor: &str) -> CreateProviderRecord {
    CreateProviderRecord {
        name: name.into(),
        vendor: Some(vendor.into()),
        protocol: "openai-compatible".into(),
        base_url: "http://127.0.0.1:9".into(),
        preset_key: None,
        channel: Some("default".into()),
        models_source: None,
        static_models: None,
        api_key: String::new(),
        adapter_credentials: "{}".into(),
        vendor_options: "{}".into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
    }
}

pub(super) async fn install(storage: &DynStorage, vendor: &str) -> anyhow::Result<()> {
    storage
        .vendor_plugins()
        .install(
            &InstalledPlugin {
                vendor_id: vendor.into(),
                version: "test".into(),
                source: "local".into(),
                descriptor: "{}".into(),
                component: Bytes::new(),
                digest: "test".into(),
                revision: 1,
                data_epoch: 1,
                installed_at: 1,
            },
            None,
            &[],
        )
        .await?;
    Ok(())
}
