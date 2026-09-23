use std::collections::BTreeMap;
use std::sync::Arc;

use stravia_vendor_runtime::{LoadedPlugin, VendorRuntime};
use tokio::sync::OnceCell;

include!(concat!(env!("OUT_DIR"), "/vendor_components.rs"));

pub(super) struct BundledPlugin {
    pub(super) component: &'static [u8],
    pub(super) loaded: LoadedPlugin,
}

pub(super) struct BundledPlugins {
    pub(super) runtime: VendorRuntime,
    pub(super) plugins: BTreeMap<String, BundledPlugin>,
}

impl BundledPlugins {
    pub(super) async fn load() -> anyhow::Result<Arc<Self>> {
        // 只共享不可变编译结果；每次操作仍创建独立的 Store 与连接作用域。
        static COMPILED: OnceCell<Arc<BundledPlugins>> = OnceCell::const_new();
        COMPILED
            .get_or_try_init(|| async {
                let runtime = VendorRuntime::new()?;
                let mut plugins = BTreeMap::new();
                for &(vendor_id, component) in COMPONENTS {
                    let loaded = runtime.load(component).await.map_err(|error| {
                        tracing::error!(vendor_id, error = ?error, "bundled component load failed");
                        anyhow::anyhow!("bundled vendor component is invalid: {vendor_id}")
                    })?;
                    anyhow::ensure!(
                        loaded.descriptor().vendor_id == vendor_id,
                        "bundled plugin identity does not match: {vendor_id}"
                    );
                    plugins.insert(vendor_id.to_owned(), BundledPlugin { component, loaded });
                }
                Ok(Arc::new(Self { runtime, plugins }))
            })
            .await
            .cloned()
    }
}
