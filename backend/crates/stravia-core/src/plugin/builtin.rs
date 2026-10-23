use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use stravia_vendor_runtime::{LoadedPlugin, VendorRuntime};
use tokio::sync::OnceCell;

include!(concat!(env!("OUT_DIR"), "/vendor_components.rs"));

pub(super) struct BundledPlugin {
    pub(super) component: &'static [u8],
    pub(super) loaded: LoadedPlugin,
}

pub(super) struct BundledPlugins {
    runtime: VendorRuntime,
    pub(super) plugins: BTreeMap<String, BundledPlugin>,
}

impl BundledPlugins {
    /// 内置组件只编译一次；本地插件的编译运行时仍按实例持有缓存目录。
    pub(super) async fn load(
        cache_directory: Option<&Path>,
    ) -> anyhow::Result<(Arc<Self>, VendorRuntime)> {
        static COMPILED: OnceCell<Arc<BundledPlugins>> = OnceCell::const_new();
        let mut initialized = false;
        let bundled = COMPILED
            .get_or_try_init(|| async {
                initialized = true;
                Self::compile(cache_directory).await
            })
            .await
            .cloned()?;
        let runtime = if initialized {
            bundled.runtime.clone()
        } else {
            match cache_directory {
                Some(directory) => VendorRuntime::with_cache(directory)?,
                None => Self::uncached_runtime().await?,
            }
        };
        Ok((bundled, runtime))
    }

    async fn uncached_runtime() -> anyhow::Result<VendorRuntime> {
        static RUNTIME: OnceCell<VendorRuntime> = OnceCell::const_new();
        Ok(RUNTIME
            .get_or_try_init(|| async { VendorRuntime::new() })
            .await?
            .clone())
    }

    async fn compile(cache_directory: Option<&Path>) -> anyhow::Result<Arc<Self>> {
        let runtime = match cache_directory {
            Some(directory) => VendorRuntime::with_cache(directory)?,
            None => Self::uncached_runtime().await?,
        };
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
    }
}
