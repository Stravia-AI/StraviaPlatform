use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use stravia_runtime_contract::protocol::ir::canonical::{hash_bytes, hash_hex};
use stravia_vendor_runtime::{
    ComponentBytes, ComponentSource, LoadedPlugin, SourceError, VendorRuntime,
};
use tokio::sync::OnceCell;

include!(concat!(env!("OUT_DIR"), "/vendor_components.rs"));

/// 内嵌组件的字节随二进制常驻，释放编译产物后直接取回，不经过磁盘。
struct EmbeddedSource(&'static [u8]);

#[async_trait]
impl ComponentSource for EmbeddedSource {
    async fn read(&self) -> Result<ComponentBytes, SourceError> {
        Ok(Box::new(self.0))
    }
}

/// 持久实例的编译产物目录。两者都只是可重建的派生数据，删除后下次启动重新生成。
#[derive(Clone)]
pub(crate) struct CompileCache {
    /// Wasmtime 官方编译缓存。
    pub(crate) wasmtime: PathBuf,
    /// 已通过完整校验的组件记录，让启动跳过对这些组件的实例化。须与 Wasmtime
    /// 缓存分开：后者的清理会删除不认识的文件。
    pub(crate) admissions: PathBuf,
}

impl CompileCache {
    fn runtime(&self) -> anyhow::Result<VendorRuntime> {
        Ok(VendorRuntime::with_cache(&self.wasmtime)?.with_admission_records(&self.admissions))
    }
}

pub(super) struct BundledPlugin {
    pub(super) component: &'static [u8],
    /// 内嵌字节的完整 SHA-256（小写十六进制），与安装记录的 digest 同一格式。
    pub(super) digest: String,
    pub(super) loaded: LoadedPlugin,
}

pub(super) struct BundledPlugins {
    runtime: VendorRuntime,
    pub(super) plugins: BTreeMap<String, BundledPlugin>,
}

impl BundledPlugins {
    /// 内置组件只编译一次；本地插件的编译运行时仍按实例持有缓存目录。
    pub(super) async fn load(
        cache: Option<&CompileCache>,
    ) -> anyhow::Result<(Arc<Self>, VendorRuntime)> {
        static COMPILED: OnceCell<Arc<BundledPlugins>> = OnceCell::const_new();
        let mut initialized = false;
        let bundled = COMPILED
            .get_or_try_init(|| async {
                initialized = true;
                Self::compile(cache).await
            })
            .await
            .cloned()?;
        let runtime = if initialized {
            bundled.runtime.clone()
        } else {
            match cache {
                Some(cache) => cache.runtime()?,
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

    async fn compile(cache: Option<&CompileCache>) -> anyhow::Result<Arc<Self>> {
        let runtime = match cache {
            Some(cache) => cache.runtime()?,
            None => Self::uncached_runtime().await?,
        };
        let mut plugins = BTreeMap::new();
        for &(vendor_id, component) in COMPONENTS {
            let digest = hash_hex(&hash_bytes(component));
            let source: Arc<dyn ComponentSource> = Arc::new(EmbeddedSource(component));
            // 此前由本二进制完整校验过的组件无需再实例化：用记录得到 descriptor，
            // 首次使用时才装入并核对。
            let loaded = match runtime.restore(&digest, Arc::clone(&source)).await {
                Some(loaded) => loaded,
                None => {
                    let loaded = runtime.load(component).await.map_err(|error| {
                        tracing::error!(vendor_id, error = ?error, "bundled component load failed");
                        anyhow::anyhow!("bundled vendor component is invalid: {vendor_id}")
                    })?;
                    record_admission(&runtime, &loaded).await;
                    // 已装入的编译产物沿用，空闲 60 分钟后释放，再从持久编译缓存恢复。
                    loaded.with_source(source)
                }
            };
            anyhow::ensure!(
                loaded.descriptor().vendor_id == vendor_id,
                "bundled plugin identity does not match: {vendor_id}"
            );
            plugins.insert(
                vendor_id.to_owned(),
                BundledPlugin {
                    component,
                    digest,
                    loaded,
                },
            );
        }
        Ok(Arc::new(Self { runtime, plugins }))
    }
}

/// 写入失败只意味着下次启动要重新校验，不影响当前插件，因此只告警。
pub(super) async fn record_admission(runtime: &VendorRuntime, plugin: &LoadedPlugin) {
    if let Err(error) = runtime.record_admission(plugin).await {
        tracing::warn!(%error, "failed to record plugin admission; it will be revalidated at next start");
    }
}
