//! 空闲释放与按需恢复。依赖真实 base 组件，因此与其他真实组件回归一样为
//! 显式 opt-in：先执行 `task build:vendors`，再运行
//! `cargo test --locked -p stravia-vendor-runtime --lib residency -- --ignored`。

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use stravia_runtime_contract::protocol::ir::canonical::{hash_bytes, hash_hex};
use stravia_runtime_contract::protocol::ir::{EmbeddingInput, EmbeddingRequest};
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_sdk::{AiRequest, ErrorKind, Operation, ProviderSnapshot};

use super::admission::AdmissionRecords;
use super::compilation_cache_tests::{base_component, metrics};
use super::residency;
use super::{
    ComponentBytes, ComponentSource, LoadedPlugin, RuntimeError, SourceError, VendorRuntime,
};

const IDLE: Duration = Duration::from_secs(60 * 60);
const EXPECTED_PROTOCOL: &str = "openai-compatible/embeddings/v1";

struct CountingSource {
    bytes: Vec<u8>,
    reads: AtomicUsize,
}

impl CountingSource {
    fn new(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            bytes,
            reads: AtomicUsize::new(0),
        })
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ComponentSource for CountingSource {
    async fn read(&self) -> Result<ComponentBytes, SourceError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        // 让并发的首次使用有机会同时到达恢复入口。
        tokio::task::yield_now().await;
        Ok(Box::new(self.bytes.clone()))
    }
}

fn provider() -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "custom".into(),
        channel: "default".into(),
        base_url: "https://upstream.invalid/v1".into(),
        protocol: "openai-compatible".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: None,
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    }
}

fn request() -> AiRequest {
    let mut request = AiRequest::new("model", Vec::new());
    request.embedding = Some(EmbeddingRequest {
        input: EmbeddingInput::Text("input".into()),
        dimensions: None,
        encoding_format: None,
        user: None,
    });
    request
}

async fn select_protocol(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
) -> Result<String, RuntimeError> {
    runtime
        .select_protocol(
            plugin,
            Operation::Infer,
            &provider(),
            &request(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(60)),
        )
        .await
}

/// 暂停的时钟只在空闲等待期间使用：恢复含阻塞线程的编译，自动推进会误触发
/// 其他定时器，所以调用方在执行真实操作前先 `resume`。
async fn elapse(duration: Duration) {
    tokio::time::advance(duration).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// 让插件空闲满期并确认已释放，然后恢复真实时钟。
async fn idle_out(plugin: &LoadedPlugin) {
    tokio::task::yield_now().await;
    tokio::time::pause();
    elapse(IDLE + Duration::from_secs(60)).await;
    assert!(!plugin.is_resident(), "idle plugin kept its compiled code");
    tokio::time::resume();
}

struct Fixture {
    _workspace: tempfile::TempDir,
    runtime: VendorRuntime,
    source: Arc<CountingSource>,
    plugin: LoadedPlugin,
}

/// 带持久缓存、绑定来源的 base 插件。装入的编译产物沿用，空闲计时已开始。
async fn fixture() -> Result<Fixture, Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let runtime = VendorRuntime::with_cache(workspace.path().join("cache/wasmtime"))?;
    let bytes = base_component();
    let source = CountingSource::new(bytes.clone());
    let plugin = runtime
        .load(&bytes)
        .await?
        .with_source(Arc::clone(&source) as Arc<dyn ComponentSource>);
    // 让空闲计时任务登记其睡眠，之后才能冻结时钟。
    tokio::task::yield_now().await;
    Ok(Fixture {
        _workspace: workspace,
        runtime,
        source,
        plugin,
    })
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn idle_plugin_is_released_and_restored_from_the_persistent_cache()
-> Result<(), Box<dyn Error>> {
    let Fixture {
        _workspace,
        runtime,
        source,
        plugin,
    } = fixture().await?;
    // 装入的产物沿用，使用它不需要读取来源。
    assert!(plugin.is_resident());
    assert_eq!(select_protocol(&runtime, &plugin).await?, EXPECTED_PROTOCOL);
    assert_eq!(source.reads(), 0);

    tokio::time::pause();
    elapse(IDLE - Duration::from_secs(60)).await;
    assert!(
        plugin.is_resident(),
        "released before the idle period ended"
    );
    elapse(Duration::from_secs(120)).await;
    assert!(!plugin.is_resident(), "idle plugin kept its compiled code");
    // 元数据不依赖编译产物。
    assert_eq!(plugin.descriptor().vendor_id, "base");
    tokio::time::resume();

    // 释放后的首次使用从持久缓存反序列化，不重新编译。
    let (hits, misses) = metrics(&runtime);
    assert_eq!(select_protocol(&runtime, &plugin).await?, EXPECTED_PROTOCOL);
    assert!(plugin.is_resident());
    assert_eq!(source.reads(), 1);
    let (hits_after, misses_after) = metrics(&runtime);
    assert!(
        hits_after > hits,
        "restore must deserialize the cached artifact: hits {hits}->{hits_after}, misses {misses}->{misses_after}"
    );
    assert_eq!(misses_after, misses, "restore must not recompile");

    // 恢复后重新计时，可再次释放。
    idle_out(&plugin).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn leased_plugin_is_kept_and_its_idle_period_restarts_when_returned()
-> Result<(), Box<dyn Error>> {
    let Fixture {
        _workspace, plugin, ..
    } = fixture().await?;
    let lease = residency::lease(
        &plugin,
        &CancellationToken::new(),
        &Deadline::from_now(Duration::from_secs(60)),
    )
    .await
    .map_err(|error| error.to_string())?;

    assert!(plugin.is_resident());
    tokio::task::yield_now().await;
    tokio::time::pause();
    elapse(3 * IDLE).await;
    assert!(plugin.is_resident(), "released while an operation held it");

    drop(lease);
    elapse(IDLE - Duration::from_secs(60)).await;
    assert!(plugin.is_resident(), "idle period must restart on return");
    elapse(Duration::from_secs(120)).await;
    assert!(!plugin.is_resident());
    Ok(())
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn concurrent_first_use_restores_once() -> Result<(), Box<dyn Error>> {
    let Fixture {
        _workspace,
        runtime,
        source,
        plugin,
    } = fixture().await?;
    idle_out(&plugin).await;

    let (first, second, third) = tokio::join!(
        select_protocol(&runtime, &plugin),
        select_protocol(&runtime, &plugin),
        select_protocol(&runtime, &plugin),
    );
    for protocol in [first?, second?, third?] {
        assert_eq!(protocol, EXPECTED_PROTOCOL);
    }
    assert_eq!(source.reads(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn a_source_cannot_swap_in_different_code_for_an_admitted_plugin()
-> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let runtime = VendorRuntime::with_cache(workspace.path().join("cache/wasmtime"))?;
    let plugin = runtime
        .load(&base_component())
        .await?
        .with_source(CountingSource::new(b"not the admitted component".to_vec()));
    idle_out(&plugin).await;

    let error = select_protocol(&runtime, &plugin)
        .await
        .expect_err("mismatched bytes must not run");
    assert!(
        matches!(
            error,
            RuntimeError::Plugin {
                kind: ErrorKind::Trapped,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(!plugin.is_resident());
    Ok(())
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn plugins_without_a_persistent_cache_stay_resident() -> Result<(), Box<dyn Error>> {
    // 没有缓存时恢复意味着完整重新编译，宁可保留内存。
    let runtime = VendorRuntime::new()?;
    let bytes = base_component();
    let plugin = runtime
        .load(&bytes)
        .await?
        .with_source(CountingSource::new(bytes));
    assert!(plugin.is_resident());

    tokio::time::pause();
    elapse(3 * IDLE).await;
    assert!(plugin.is_resident());
    Ok(())
}

fn digest_hex(bytes: &[u8]) -> String {
    hash_hex(&hash_bytes(bytes))
}

/// 模拟重启：新的 Engine 与空的内存状态，只共享磁盘上的编译缓存和准入记录。
fn restarted(workspace: &tempfile::TempDir) -> Result<VendorRuntime, Box<dyn Error>> {
    Ok(
        VendorRuntime::with_cache(workspace.path().join("cache/wasmtime"))?
            .with_admission_records(workspace.path().join("cache/admissions")),
    )
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn admitted_plugin_is_restored_without_loading_and_verified_on_first_use()
-> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let bytes = base_component();
    let first = restarted(&workspace)?;
    let loaded = first.load(&bytes).await?;
    first.record_admission(&loaded).await?;
    let descriptor = loaded.descriptor().clone();
    drop(loaded);
    drop(first);

    let runtime = restarted(&workspace)?;
    let source = CountingSource::new(bytes.clone());
    let plugin = runtime
        .restore(
            &digest_hex(&bytes),
            Arc::clone(&source) as Arc<dyn ComponentSource>,
        )
        .await
        .expect("a recorded admission restores");
    // 启动路径：有 descriptor 与身份，但没有读取字节，也没有编译。
    assert_eq!(plugin.descriptor(), &descriptor);
    assert_eq!(source.reads(), 0);
    assert!(!plugin.is_resident());
    assert_eq!(metrics(&runtime), (0, 0));

    // 首次使用才装入：走持久缓存，不重新编译，并核对了组件的实际 descriptor。
    assert_eq!(select_protocol(&runtime, &plugin).await?, EXPECTED_PROTOCOL);
    assert_eq!(source.reads(), 1);
    assert!(plugin.is_resident());
    let (hits, misses) = metrics(&runtime);
    assert!(hits > 0 && misses == 0, "hits {hits}, misses {misses}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn missing_foreign_or_unusable_records_do_not_restore() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let bytes = base_component();
    let digest = hash_bytes(&bytes);
    let hex = digest_hex(&bytes);
    let source = || CountingSource::new(bytes.clone()) as Arc<dyn ComponentSource>;

    let runtime = restarted(&workspace)?;
    assert!(runtime.restore(&hex, source()).await.is_none(), "no record");
    let loaded = runtime.load(&bytes).await?;

    // 另一个宿主构建写下的记录不适用于当前构建。
    let records = workspace.path().join("cache/admissions");
    AdmissionRecords::for_host(records.clone(), "another-build")
        .write(&digest, loaded.descriptor())
        .map_err(|error| error.to_string())?;
    assert!(runtime.restore(&hex, source()).await.is_none());

    // 损坏的记录视为没有记录。
    std::fs::write(records.join(format!("{hex}.json")), b"{ truncated")?;
    assert!(runtime.restore(&hex, source()).await.is_none());

    // 没有持久编译缓存时推迟装入没有收益，不使用记录。
    let uncached = VendorRuntime::new()?.with_admission_records(records);
    uncached.record_admission(&loaded).await?;
    assert!(uncached.restore(&hex, source()).await.is_none());
    Ok(())
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn a_record_that_disagrees_with_the_component_fails_first_use_and_is_dropped()
-> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let bytes = base_component();
    let hex = digest_hex(&bytes);
    let runtime = restarted(&workspace)?;
    let loaded = runtime.load(&bytes).await?;

    // 记录里的 descriptor 与组件实际报告的不同（例如记录被改动）。
    let mut forged = serde_json::to_value(loaded.descriptor())?;
    forged["version"] = serde_json::json!("99.0.0");
    let forged: stravia_vendor_sdk::VendorDescriptor = serde_json::from_value(forged)?;
    AdmissionRecords::new(workspace.path().join("cache/admissions"))
        .expect("host build is identifiable")
        .write(&hash_bytes(&bytes), &forged)
        .map_err(|error| error.to_string())?;

    let source = CountingSource::new(bytes);
    let plugin = runtime
        .restore(&hex, source as Arc<dyn ComponentSource>)
        .await
        .expect("the forged record is structurally valid");
    let error = select_protocol(&runtime, &plugin)
        .await
        .expect_err("a record must not override what the component reports");
    assert!(
        matches!(
            error,
            RuntimeError::Plugin {
                kind: ErrorKind::Trapped,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(!plugin.is_resident());

    // 记录已作废：下次启动会完整校验，而不是再次信任它。
    assert!(
        runtime
            .restore(
                &hex,
                CountingSource::new(Vec::new()) as Arc<dyn ComponentSource>
            )
            .await
            .is_none()
    );
    Ok(())
}
