use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_sdk::{
    AiRequest, CANONICAL_FORMAT_VERSION, ErrorKind, Operation, OperationOutput, ProviderSnapshot,
    VendorDescriptor,
};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings;
use crate::bindings::stravia::net::{http as net_http, types as net_types, websocket as net_ws};
use crate::bindings::stravia::state::{
    journal as wit_journal, store as wit_state, volatile as wit_volatile,
};
use crate::bindings::stravia::vendor::{host as wit_host, types as wit_types};
use crate::error::{LoadError, RuntimeError};
use crate::host::{
    HostFailure, HostServices, HttpResponseResource, JournalEntryResource, LogLevel,
    NetOptionsResource, RuntimeEvent, StateCellResource, StateError, StateScope, WebSocketMessage,
    WebSocketResource,
};
use stravia_vendor_sdk::net::{ConnectRequest, Request};
use stravia_vendor_sdk::state::MAX_PAYLOAD_BYTES;

mod admission;
mod compilation_cache;
mod residency;

use admission::AdmissionRecords;
use compilation_cache::CompilationCache;
pub use residency::{ComponentBytes, ComponentSource, SourceError};
use residency::{IDLE_UNLOAD, Residency, Resident};

#[derive(Clone)]
pub struct LoadedPlugin {
    inner: Arc<LoadedVersion>,
}

struct LoadedVersion {
    /// Compiled component and import-resolved blueprint. Shared by
    /// `with_descriptor`, never by different loads. May be released after an
    /// idle period and restored on demand; descriptor and identity never are.
    residency: Arc<Residency>,
    descriptor: VendorDescriptor,
    identity: String,
}

impl LoadedPlugin {
    fn from_parts(
        residency: Arc<Residency>,
        descriptor: VendorDescriptor,
        identity: String,
    ) -> Self {
        Self {
            inner: Arc::new(LoadedVersion {
                residency,
                descriptor,
                identity,
            }),
        }
    }

    pub fn descriptor(&self) -> &VendorDescriptor {
        &self.inner.descriptor
    }

    /// Content identity of the exact component bytes. This deliberately does
    /// not expose the component's source path or any provider secret.
    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    /// Returns a handle to the same compiled component whose reported
    /// descriptor is `descriptor`. Used to pin an effective profile set
    /// produced by `sync-catalog` without recompiling or repinning the
    /// component itself.
    pub fn with_descriptor(&self, descriptor: VendorDescriptor) -> LoadedPlugin {
        Self::from_parts(
            Arc::clone(&self.inner.residency),
            descriptor,
            self.inner.identity.clone(),
        )
    }
}

pub struct OperationScope {
    pub services: Arc<dyn HostServices>,
    pub cancellation: CancellationToken,
    pub deadline: Deadline,
    pub generation: u64,
}

impl OperationScope {
    pub fn new(
        services: Arc<dyn HostServices>,
        cancellation: CancellationToken,
        deadline: Deadline,
        generation: u64,
    ) -> Self {
        Self {
            services,
            cancellation,
            deadline,
            generation,
        }
    }
}

#[derive(Clone)]
pub struct VendorRuntime {
    engine: Engine,
    compiled: Arc<Mutex<VecDeque<CompiledEntry>>>,
    /// 绑定了来源的插件空闲多久后释放编译产物。仅在有持久编译缓存时设置：
    /// 恢复是缓存反序列化；没有缓存的恢复是完整编译，代价高于保留内存。
    idle_unload: Option<Duration>,
    /// 已通过准入校验的组件记录，让启动可以跳过对这些组件的实例化校验。
    admissions: Option<Arc<AdmissionRecords>>,
    cache: Option<Arc<CompilationCache>>,
    /// 启用 Wasmtime 池化分配器时用于排队控制并发槽位，避免突发并发触发池满报错。
    pool_slots: Option<Arc<Semaphore>>,
    /// 仅在存在活动 Store 时以固定间隔递增 epoch，空闲零唤醒；随最后一个引用退出并回收线程。
    epoch_ticker: Arc<EpochTicker>,
}

const POOL_CONCURRENCY_LIMIT: u32 = 64;
const POOL_CORE_INSTANCES_PER_COMPONENT: u32 = 64;
const POOL_MEMORIES_PER_COMPONENT: u32 = 4;
const POOL_TABLES_PER_COMPONENT: u32 = 16;
// Preserve the full wasm32 address space; pooling must not impose a new
// per-request memory ceiling when canonical payloads contain large media.
const POOL_MAX_MEMORY_BYTES: u64 = 1 << 32;
const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);

struct EpochTickerState {
    active: usize,
    closed: bool,
}

struct EpochTicker {
    state: Arc<(Mutex<EpochTickerState>, Condvar)>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl EpochTicker {
    fn start(engine: Engine) -> Result<Self, LoadError> {
        let state = Arc::new((
            Mutex::new(EpochTickerState {
                active: 0,
                closed: false,
            }),
            Condvar::new(),
        ));
        let shared = Arc::clone(&state);
        let handle = std::thread::Builder::new()
            .name("stravia-wasm-epoch".into())
            .spawn(move || {
                let (lock, condvar) = &*shared;
                let mut guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
                loop {
                    while !guard.closed && guard.active == 0 {
                        guard = condvar
                            .wait(guard)
                            .unwrap_or_else(|poison| poison.into_inner());
                    }
                    if guard.closed {
                        break;
                    }
                    let (next_guard, _) = condvar
                        .wait_timeout(guard, EPOCH_TICK_INTERVAL)
                        .unwrap_or_else(|poison| poison.into_inner());
                    guard = next_guard;
                    if guard.closed {
                        break;
                    }
                    if guard.active > 0 {
                        engine.increment_epoch();
                    }
                }
            })
            .map_err(|error| {
                LoadError::InvalidComponent(format!("failed to start wasm epoch ticker: {error}"))
            })?;
        Ok(Self {
            state,
            thread: Mutex::new(Some(handle)),
        })
    }

    fn lease(self: &Arc<Self>) -> EpochLease {
        let (lock, condvar) = &*self.state;
        let mut guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.active += 1;
        if guard.active == 1 {
            condvar.notify_one();
        }
        EpochLease {
            ticker: Arc::clone(self),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        {
            let (lock, condvar) = &*self.state;
            let mut guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
            guard.closed = true;
            condvar.notify_all();
        }
        if let Some(handle) = self
            .thread
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

struct EpochLease {
    ticker: Arc<EpochTicker>,
}

impl Drop for EpochLease {
    fn drop(&mut self) {
        let (lock, _) = &*self.ticker.state;
        let mut guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.active = guard.active.saturating_sub(1);
    }
}

/// 持有池化并发许可与 epoch 计时租约。调用方须在 `Store` 之前声明此守卫，
/// 使 `Store` 先析构归还池槽位后再释放并发许可。
struct ExecutionSlot {
    _pool_permit: Option<OwnedSemaphorePermit>,
    _epoch_lease: EpochLease,
}

fn base_engine_config() -> Config {
    let mut engine_config = Config::new();
    engine_config.wasm_component_model(true);
    // Owned host calls release lowered payloads before awaiting the guest.
    // This scheduler does not expand the guest's component feature surface.
    engine_config.concurrency_support(true);
    engine_config.wasm_component_model_async(false);
    engine_config.wasm_component_model_more_async_builtins(false);
    engine_config.wasm_component_model_async_stackful(false);
    engine_config.wasm_component_model_threading(false);
    engine_config.epoch_interruption(true);
    engine_config
}

fn build_engine() -> Result<(Engine, Option<Arc<Semaphore>>), LoadError> {
    if cfg!(target_pointer_width = "64") {
        let mut pooled_config = base_engine_config();
        pooled_config.memory_reservation(POOL_MAX_MEMORY_BYTES);
        let mut pooling = PoolingAllocationConfig::new();
        pooling
            .total_component_instances(POOL_CONCURRENCY_LIMIT)
            .total_core_instances(POOL_CONCURRENCY_LIMIT * POOL_CORE_INSTANCES_PER_COMPONENT)
            .max_core_instances_per_component(POOL_CORE_INSTANCES_PER_COMPONENT)
            .total_memories(POOL_CONCURRENCY_LIMIT * POOL_MEMORIES_PER_COMPONENT)
            .max_memories_per_component(POOL_MEMORIES_PER_COMPONENT)
            .max_memories_per_module(2)
            .total_tables(POOL_CONCURRENCY_LIMIT * POOL_TABLES_PER_COMPONENT)
            .max_tables_per_component(POOL_TABLES_PER_COMPONENT)
            .max_tables_per_module(4)
            .table_elements(65_536)
            .total_stacks(POOL_CONCURRENCY_LIMIT)
            .max_memory_size(POOL_MAX_MEMORY_BYTES as usize)
            .max_unused_warm_slots(16)
            .linear_memory_keep_resident(0)
            .table_keep_resident(0)
            .async_stack_keep_resident(0);
        pooled_config.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling));
        match Engine::new(&pooled_config) {
            Ok(engine) => {
                return Ok((
                    engine,
                    Some(Arc::new(Semaphore::new(POOL_CONCURRENCY_LIMIT as usize))),
                ));
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "wasm pooling allocator unavailable; falling back to on-demand allocation"
                );
            }
        }
    }
    let config = base_engine_config();
    let engine =
        Engine::new(&config).map_err(|error| LoadError::InvalidComponent(error.to_string()))?;
    Ok((engine, None))
}

struct CompiledEntry {
    digest: [u8; 32],
    component: Arc<AsyncMutex<Weak<Component>>>,
}

const MAX_COMPILED_ENTRIES: usize = 16;
const MAX_COMPILE_THREADS: usize = 4;

fn compile_thread_count(env_override: Option<&str>, available_parallelism: usize) -> usize {
    if let Some(threads) = env_override
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&count| count > 0)
    {
        return threads;
    }
    available_parallelism.clamp(1, MAX_COMPILE_THREADS)
}

/// 在临时 Rayon 线程池中运行 Wasmtime 并行编译，避免初始化永不退出的全局 Rayon 池
/// 导致高核数主机在冷编译后长期滞留数百 MiB 至 GiB 级分配器线程堆。
pub(super) fn compile_in_ephemeral_pool<T, F>(compile: F) -> T
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let threads = compile_thread_count(
        std::env::var("RAYON_NUM_THREADS").ok().as_deref(),
        available,
    );
    match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("stravia-wasm-compile-{index}"))
        .build()
    {
        Ok(pool) => pool.install(compile),
        Err(error) => {
            tracing::warn!(%error, "ephemeral wasm compile pool unavailable; compiling on calling thread");
            compile()
        }
    }
}

impl VendorRuntime {
    /// 不使用持久编译缓存。Engine、编译结果仍由调用方共享。
    pub fn new() -> Result<Self, LoadError> {
        Self::build(None)
    }

    /// 使用不可变的文件映射编译缓存。Wasmtime 提供引擎兼容身份，组件以完整
    /// 内容摘要区分；损坏产物从源字节重建。目录必须是可写绝对路径；Unix 上
    /// 权限收紧为 0700，真实路径的祖先不得受不可信用户控制。不要与旧版
    /// Wasmtime 压缩缓存共用目录，旧清理器不认识此格式。
    pub fn with_cache(directory: impl Into<PathBuf>) -> Result<Self, LoadError> {
        Self::build(Some(directory.into()))
    }

    fn build(cache_directory: Option<PathBuf>) -> Result<Self, LoadError> {
        let idle_unload = cache_directory.is_some().then_some(IDLE_UNLOAD);
        let (engine, pool_slots) = build_engine()?;
        let epoch_ticker = Arc::new(EpochTicker::start(engine.clone())?);
        let cache = cache_directory
            .map(|directory| CompilationCache::new(&directory, &engine).map(Arc::new))
            .transpose()?;
        Ok(Self {
            engine,
            compiled: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_COMPILED_ENTRIES))),
            idle_unload,
            admissions: None,
            cache,
            pool_slots,
            epoch_ticker,
        })
    }

    async fn acquire_execution_slot(
        &self,
        cancellation: &CancellationToken,
        deadline: &Deadline,
    ) -> Result<ExecutionSlot, RuntimeError> {
        let permit = match &self.pool_slots {
            Some(semaphore) => {
                let acquire = Arc::clone(semaphore).acquire_owned();
                let permit = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(RuntimeError::Cancelled),
                    () = deadline.wait() => return Err(RuntimeError::DeadlineExceeded),
                    result = acquire => result.map_err(|_| RuntimeError::Cancelled)?,
                };
                Some(permit)
            }
            None => None,
        };
        Ok(ExecutionSlot {
            _pool_permit: permit,
            _epoch_lease: self.epoch_ticker.lease(),
        })
    }

    async fn compiled_component<Compile, Compiling>(
        &self,
        digest: &[u8; 32],
        compile: Compile,
    ) -> Result<Arc<Component>, LoadError>
    where
        Compile: FnOnce() -> Compiling,
        Compiling: Future<Output = Result<Arc<Component>, LoadError>>,
    {
        let slot = {
            let mut entries = self
                .compiled
                .lock()
                .map_err(|_| LoadError::InvalidComponent("component cache lock poisoned".into()))?;
            if let Some(index) = entries.iter().position(|entry| entry.digest == *digest) {
                let entry = entries.remove(index).expect("located compiled entry");
                let slot = Arc::clone(&entry.component);
                entries.push_back(entry);
                Some(slot)
            } else {
                if entries.len() == MAX_COMPILED_ENTRIES {
                    // Do not evict an in-flight identity's single-flight slot.
                    if let Some(index) = entries
                        .iter()
                        .position(|entry| Arc::strong_count(&entry.component) == 1)
                    {
                        entries.remove(index);
                    }
                }
                if entries.len() < MAX_COMPILED_ENTRIES {
                    let slot = Arc::new(AsyncMutex::new(Weak::new()));
                    entries.push_back(CompiledEntry {
                        digest: *digest,
                        component: Arc::clone(&slot),
                    });
                    Some(slot)
                } else {
                    None
                }
            }
        };
        let Some(slot) = slot else {
            return compile().await;
        };
        // Only callers compiling the same identity wait here. The Engine-local
        // metadata lock is released before compilation; other bytes run independently.
        let mut cached = slot.lock().await;
        if let Some(component) = cached.upgrade() {
            return Ok(component);
        }
        let component = compile().await?;
        *cached = Arc::downgrade(&component);
        Ok(component)
    }

    fn compile_component(
        &self,
        bytes: &[u8],
        digest: &[u8; 32],
    ) -> Result<Arc<Component>, LoadError> {
        match &self.cache {
            Some(cache) => cache.load(&self.engine, bytes, digest),
            None => compile_in_ephemeral_pool(|| Component::from_binary(&self.engine, bytes))
                .map_err(|error| LoadError::InvalidComponent(error.to_string())),
        }
        .map(Arc::new)
    }

    /// Compile (or reuse a live Engine-local compilation), import-check,
    /// resolve imports once, freshly instantiate, and validate a component
    /// without mutating any installed version. Only immutable compilation is
    /// shared across loads; the prepared blueprint and every descriptor
    /// validation are rebuilt on each load. Active operations pin this exact
    /// blueprint.
    pub async fn load(&self, bytes: &[u8]) -> Result<LoadedPlugin, LoadError> {
        let digest = stravia_runtime_contract::protocol::ir::canonical::hash_bytes(bytes);
        let identity = stravia_runtime_contract::identifier::encode_digest(&digest);
        let component = self
            .compiled_component(&digest, || async { self.compile_component(bytes, &digest) })
            .await?;
        self.validate_imports(&component)?;
        let prepared = Self::prepare(&component)?;
        let descriptor = self.admit_descriptor(&prepared).await?;
        Ok(LoadedPlugin::from_parts(
            Residency::pinned(
                self.clone(),
                digest,
                descriptor.clone(),
                Resident::new(component, prepared),
            ),
            descriptor,
            identity,
        ))
    }

    /// 组件当场报告的 descriptor，须通过结构与 canonical 格式版本校验。
    async fn admit_descriptor(
        &self,
        prepared: &bindings::VendorPre<StoreState>,
    ) -> Result<VendorDescriptor, LoadError> {
        let descriptor = self.read_descriptor(prepared).await?;
        descriptor
            .validate()
            .map_err(|error| LoadError::DescriptorInvalid(error.to_string()))?;
        if descriptor.canonical_format_version != CANONICAL_FORMAT_VERSION {
            return Err(LoadError::CanonicalFormat {
                actual: descriptor.canonical_format_version,
                expected: CANONICAL_FORMAT_VERSION,
            });
        }
        Ok(descriptor)
    }

    /// Ask the pinned guest component to choose the egress protocol for one
    /// model request. This pure phase runs in a store where transport, private
    /// state, and event imports are unavailable.
    pub async fn select_protocol(
        &self,
        plugin: &LoadedPlugin,
        operation: Operation,
        provider: &ProviderSnapshot,
        request: &AiRequest,
        cancellation: CancellationToken,
        deadline: Deadline,
    ) -> Result<String, RuntimeError> {
        if !matches!(operation, Operation::Infer | Operation::Compact) {
            return Err(RuntimeError::from_guest(
                ErrorKind::Unsupported,
                "vendor operation is not supported".into(),
                None,
            ));
        }
        admit_operation(
            plugin.descriptor(),
            &provider.provider_id,
            &provider.channel,
            &provider.channel,
            operation,
        )?;
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if deadline.is_exceeded() {
            return Err(RuntimeError::DeadlineExceeded);
        }
        if let Some(protocol) = static_selected_protocol(
            plugin.descriptor(),
            &provider.provider_id,
            &provider.channel,
            &provider.protocol,
        ) {
            return Ok(protocol);
        }

        let (sdk_provider, sdk_request) =
            stravia_vendor_sdk::guest::encode_protocol_selection(provider, request)
                .map_err(|_| RuntimeError::InvalidOutput)?;
        let wit_operation = convert_operation(operation.into());
        let wit_provider = convert_provider(sdk_provider);
        let wit_request = convert_payload(sdk_request);

        let resident = residency::lease(plugin, &cancellation, &deadline).await?;
        let _slot = resident
            .runtime()
            .acquire_execution_slot(&cancellation, &deadline)
            .await?;
        let scope = OperationScope {
            services: Arc::new(DenyServices),
            cancellation,
            deadline,
            generation: 0,
        };
        let engine = resident.prepared.engine();
        let mut store = Self::new_store(engine, scope, None, false);
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline.clone();
        let instantiate = resident.prepared.instantiate_async(&mut store);
        let instance = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            () = deadline.wait() => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = instantiate => result.map_err(|error| classify_trap(&error))?,
        };

        let call = instance.call_select_protocol(
            &mut store,
            wit_operation,
            &provider.channel,
            &wit_provider,
            &wit_request,
        );
        let raw = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            () = deadline.wait() => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = call => result,
        };
        let raw = match raw {
            Ok(value) => value,
            Err(error) => return Err(classify_trap(&error)),
        };
        let protocol = match raw {
            Ok(protocol) => protocol,
            Err(failure) => {
                return Err(RuntimeError::from_guest(
                    convert_error_kind(failure.kind),
                    failure.message,
                    failure.upstream_status,
                ));
            }
        };
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if deadline.is_exceeded() {
            return Err(RuntimeError::DeadlineExceeded);
        }
        Ok(protocol)
    }

    pub async fn execute(
        &self,
        plugin: &LoadedPlugin,
        channel: &str,
        sdk_operation: stravia_vendor_sdk::wit::types::OperationKind,
        sdk_input: stravia_vendor_sdk::wit::types::OperationInput,
        scope: OperationScope,
    ) -> Result<OperationOutput, RuntimeError> {
        let operation = Operation::from(sdk_operation);
        admit_operation(
            plugin.descriptor(),
            &sdk_input.provider.provider_id,
            &sdk_input.provider.channel,
            channel,
            operation,
        )?;
        if scope.cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if scope.deadline.is_exceeded() {
            return Err(RuntimeError::DeadlineExceeded);
        }
        if !scope.services.generation_is_current(scope.generation) {
            return Err(RuntimeError::Cancelled);
        }

        let wit_operation = convert_operation(operation.into());
        let wit_input = convert_input(sdk_input);
        let resident = residency::lease(plugin, &scope.cancellation, &scope.deadline).await?;
        let _slot = resident
            .runtime()
            .acquire_execution_slot(&scope.cancellation, &scope.deadline)
            .await?;
        let engine = resident.prepared.engine();
        let mut store = Self::new_store(engine, scope, Some(operation), false);
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline.clone();
        let instantiate = resident.prepared.instantiate_async(&mut store);
        let instance = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            () = deadline.wait() => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = instantiate => result.map_err(|error| classify_trap(&error))?,
        };

        let call = store.run_concurrent(async |accessor| {
            instance
                .call_execute(accessor, wit_operation, channel.to_owned(), wit_input)
                .await
        });
        let raw = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            () = deadline.wait() => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = call => result.and_then(|result| result),
        };
        let raw = match raw {
            Ok(value) => value,
            Err(error) => {
                store
                    .data()
                    .services
                    .log(LogLevel::Error, &error.to_string());
                return Err(classify_trap(&error));
            }
        };

        let bytes = match raw {
            Ok(bytes) => bytes,
            Err(failure) => {
                store.data().services.log(LogLevel::Warn, &failure.message);
                return Err(RuntimeError::from_guest(
                    convert_error_kind(failure.kind),
                    failure.message,
                    failure.upstream_status,
                ));
            }
        };
        if matches!(operation, Operation::Infer | Operation::Compact)
            && store.data().upstream_starts != 1
        {
            return Err(RuntimeError::InvalidOutput);
        }
        // 本地校验或静态目录不能冒充一次完成的上游连接检查。
        if operation == Operation::ConnectionCheck && !store.data().received_check_response {
            return Err(RuntimeError::InvalidOutput);
        }
        OperationOutput::decode_for_host(operation, &bytes).map_err(|_| RuntimeError::InvalidOutput)
    }

    /// Run the vendor-scoped `sync-catalog` export. The store admits only
    /// `http-start` (the services implementation still enforces the catalog
    /// origin allowlist) and `log`; connection state, event emission, and
    /// WebSocket imports stay unavailable. The input/output payloads are the
    /// serialized `CatalogSyncRequest`/`CatalogSyncOutcome` contract bodies.
    pub async fn sync_catalog(
        &self,
        plugin: &LoadedPlugin,
        input: Vec<u8>,
        scope: OperationScope,
    ) -> Result<Vec<u8>, RuntimeError> {
        if scope.cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if scope.deadline.is_exceeded() {
            return Err(RuntimeError::DeadlineExceeded);
        }
        let wit_input = wit_types::CanonicalPayload {
            format: CANONICAL_FORMAT_VERSION,
            body: input,
        };
        let resident = residency::lease(plugin, &scope.cancellation, &scope.deadline).await?;
        let _slot = resident
            .runtime()
            .acquire_execution_slot(&scope.cancellation, &scope.deadline)
            .await?;
        let engine = resident.prepared.engine();
        let mut store = Self::new_store(engine, scope, None, true);
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline.clone();
        let instantiate = resident.prepared.instantiate_async(&mut store);
        let instance = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            () = deadline.wait() => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = instantiate => result.map_err(|error| classify_trap(&error))?,
        };

        let call = instance.call_sync_catalog(&mut store, &wit_input);
        let raw = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            () = deadline.wait() => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = call => result,
        };
        let raw = match raw {
            Ok(value) => value,
            Err(error) => return Err(classify_trap(&error)),
        };
        let payload = match raw {
            Ok(payload) => payload,
            Err(failure) => {
                return Err(RuntimeError::from_guest(
                    convert_error_kind(failure.kind),
                    failure.message,
                    failure.upstream_status,
                ));
            }
        };
        if payload.format != CANONICAL_FORMAT_VERSION {
            return Err(RuntimeError::InvalidOutput);
        }
        if cancel.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if deadline.is_exceeded() {
            return Err(RuntimeError::DeadlineExceeded);
        }
        Ok(payload.body)
    }

    fn validate_imports(&self, component: &Component) -> Result<(), LoadError> {
        for (name, _) in component.component_type().imports(&self.engine) {
            if let Some((interface, version)) = name.rsplit_once('@') {
                if matches!(
                    interface,
                    "stravia:vendor/host"
                        | "stravia:vendor/types"
                        | "stravia:net/types"
                        | "stravia:net/http"
                        | "stravia:net/websocket"
                        | "stravia:state/store"
                        | "stravia:state/volatile"
                        | "stravia:state/journal"
                ) {
                    let (available_minor, available_abi) =
                        if interface.starts_with("stravia:vendor/") {
                            (0, "1.0.0")
                        } else {
                            (1, "1.1.0")
                        };
                    // Compilation validates SemVer; only stable releases are
                    // eligible. Build metadata does not affect compatibility.
                    let release = version
                        .split_once('+')
                        .map_or(version, |(release, _)| release);
                    let mut parts = release.split('.');
                    if let (Some(major), Some(minor), Some(patch), None) =
                        (parts.next(), parts.next(), parts.next(), parts.next())
                        && let (Ok(major), Ok(minor), Ok(_)) = (
                            major.parse::<u64>(),
                            minor.parse::<u64>(),
                            patch.parse::<u64>(),
                        )
                    {
                        if major != 1 {
                            return Err(LoadError::IncompatibleAbi {
                                interface: interface.to_owned(),
                                required: version.to_owned(),
                                available: available_abi,
                            });
                        }
                        if minor > available_minor {
                            return Err(LoadError::RequiresNewerHost {
                                interface: interface.to_owned(),
                                required: version.to_owned(),
                                available: available_abi,
                            });
                        }
                        continue;
                    }
                }
                // A stable, versioned first-party interface that this host does
                // not recognize is a missing capability, not an invalid guest.
                // It remains rejected and is never registered with the linker.
                let release = version
                    .split_once('+')
                    .map_or(version, |(release, _)| release);
                if interface.starts_with("stravia:")
                    && !release.contains('-')
                    && release.split('.').count() == 3
                    && release.split('.').all(|part| {
                        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
                    })
                {
                    return Err(LoadError::RequiresNewerHost {
                        interface: interface.to_owned(),
                        required: version.to_owned(),
                        available: "unavailable",
                    });
                }
            }
            let allowed = name.rsplit_once('@').is_some_and(|(interface, version)| {
                // Component compilation already validates SemVer. Ignore build
                // metadata and admit only stable 0.2.x; the linker still checks
                // required functions and resource types. Interface permissions
                // remain independent of patch-version compatibility.
                let release = version
                    .split_once('+')
                    .map_or(version, |(release, _)| release);
                release.strip_prefix("0.2.").is_some_and(|patch| {
                    !patch.is_empty() && patch.bytes().all(|byte| byte.is_ascii_digit())
                }) && matches!(
                    interface,
                    "wasi:io/poll"
                        | "wasi:io/error"
                        | "wasi:io/streams"
                        | "wasi:clocks/monotonic-clock"
                        | "wasi:clocks/wall-clock"
                        | "wasi:random/random"
                        | "wasi:random/insecure"
                        | "wasi:random/insecure-seed"
                        | "wasi:cli/stdin"
                        | "wasi:cli/stdout"
                        | "wasi:cli/stderr"
                        | "wasi:cli/environment"
                        | "wasi:cli/exit"
                        | "wasi:cli/terminal-input"
                        | "wasi:cli/terminal-output"
                        | "wasi:cli/terminal-stdin"
                        | "wasi:cli/terminal-stdout"
                        | "wasi:cli/terminal-stderr"
                )
            });
            if !allowed {
                return Err(LoadError::ForbiddenImport(name.to_owned()));
            }
        }
        Ok(())
    }

    /// Resolve imports and required exports once against the component's own
    /// Engine. Every later instantiation in this load (descriptor and each
    /// operation) reuses this blueprint; stores and host state stay fresh.
    fn prepare(component: &Component) -> Result<bindings::VendorPre<StoreState>, LoadError> {
        let linker = Self::new_linker(component.engine())
            .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        let instance_pre = linker
            .instantiate_pre(component)
            .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        bindings::VendorPre::new(instance_pre)
            .map_err(|error| LoadError::DescriptorExecution(error.to_string()))
    }

    async fn read_descriptor(
        &self,
        prepared: &bindings::VendorPre<StoreState>,
    ) -> Result<VendorDescriptor, LoadError> {
        let scope = OperationScope {
            services: Arc::new(DenyServices),
            cancellation: CancellationToken::new(),
            deadline: Deadline::from_now(Duration::from_secs(2)),
            generation: 0,
        };
        let _slot = self
            .acquire_execution_slot(&scope.cancellation, &scope.deadline)
            .await
            .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        let engine = prepared.engine();
        let mut store = Self::new_store(engine, scope, None, false);
        let deadline = store.data().deadline.clone();
        let instantiate = prepared.instantiate_async(&mut store);
        let instance = tokio::select! {
            result = instantiate => result,
            () = deadline.wait() => {
                return Err(LoadError::DescriptorExecution("descriptor deadline elapsed".into()));
            }
        }
        .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        let descriptor_call = instance.call_descriptor(&mut store);
        let descriptor = tokio::select! {
            result = descriptor_call => result,
            () = deadline.wait() => {
                return Err(LoadError::DescriptorExecution("descriptor deadline elapsed".into()));
            }
        }
        .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        serde_json::from_str(&descriptor).map_err(LoadError::DescriptorJson)
    }

    fn new_linker(engine: &Engine) -> wasmtime::Result<Linker<StoreState>> {
        let mut linker = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wit_host::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        net_types::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        net_http::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        net_ws::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        wit_state::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        wit_volatile::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        wit_journal::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        Ok(linker)
    }

    fn new_store(
        engine: &Engine,
        scope: OperationScope,
        operation: Option<Operation>,
        catalog_sync: bool,
    ) -> Store<StoreState> {
        let mut wasi = WasiCtx::builder();
        wasi.max_random_size(64 * 1024);
        // 编译结果与 prepared blueprint 可以跨实例共享；Store 必须绑定该
        // 组件所属的 Engine，而不是调用方 runtime 的 Engine。
        let mut store = Store::new(
            engine,
            StoreState {
                wasi: wasi.build(),
                table: ResourceTable::new(),
                services: scope.services,
                cancellation: scope.cancellation,
                deadline: scope.deadline,
                generation: scope.generation,
                operation,
                catalog_sync,
                upstream_starts: 0,
                received_check_response: false,
            },
        );
        // Epoch interruption preempts CPU-bound guest loops for cancellation
        // and deadlines without per-instruction fuel instrumentation overhead.
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);
        store
    }
}

struct StoreState {
    wasi: WasiCtx,
    table: ResourceTable,
    services: Arc<dyn HostServices>,
    cancellation: CancellationToken,
    deadline: Deadline,
    generation: u64,
    operation: Option<Operation>,
    /// `sync-catalog` runs without an operation kind: `http-start` stays
    /// admitted under the services' own origin checks while every
    /// connection-scoped import remains rejected.
    catalog_sync: bool,
    upstream_starts: usize,
    received_check_response: bool,
}

impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl StoreState {
    fn active(&self) -> Result<(), HostFailure> {
        if self.cancellation.is_cancelled() || !self.services.generation_is_current(self.generation)
        {
            return Err(HostFailure::new(
                ErrorKind::Cancelled,
                "operation is no longer active",
            ));
        }
        if self.deadline.is_exceeded() {
            return Err(HostFailure::new(
                ErrorKind::DeadlineExceeded,
                "operation deadline elapsed",
            ));
        }
        // Every guest↔host boundary call is activity: renew the shared deadline
        // so a streaming operation stays alive while data keeps flowing.
        self.deadline.renew();
        Ok(())
    }

    fn operation_active(&self) -> Result<Operation, HostFailure> {
        self.active()?;
        self.operation.ok_or_else(|| {
            HostFailure::new(
                ErrorKind::Trapped,
                "host operation services are unavailable during a pure guest call",
            )
        })
    }

    fn network_active(&self) -> Result<(), HostFailure> {
        self.active()?;
        if self.operation.is_none() && !self.catalog_sync {
            return Err(HostFailure::new(
                ErrorKind::Trapped,
                "host operation services are unavailable during a pure guest call",
            ));
        }
        Ok(())
    }

    fn start_http(
        &mut self,
        request: net_http::Request,
        options: Resource<NetOptionsResource>,
    ) -> Result<Result<Resource<HttpResponseResource>, net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_net_failure(error)));
        }
        let options = self.table.get(&options)?.0.clone();
        let mut headers = stravia_vendor_sdk::net::Fields::new();
        for field in request.headers {
            headers.append(field.name, field.bytes);
        }
        let request = stravia_vendor_sdk::net::Request {
            method: request.method,
            url: request.url,
            headers,
            body: request.body,
        };
        match self.services.http_start(request, options) {
            Ok(response) => self
                .table
                .push(HttpResponseResource(response))
                .map(Ok)
                .map_err(Into::into),
            Err(error) => Ok(Err(to_net_failure(error))),
        }
    }

    async fn connect_ws(
        &mut self,
        request: net_ws::ConnectRequest,
        options: Resource<NetOptionsResource>,
    ) -> Result<Result<Resource<WebSocketResource>, net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_net_failure(error)));
        }
        let options = self.table.get(&options)?.0.clone();
        let mut headers = stravia_vendor_sdk::net::Fields::new();
        for field in request.headers {
            headers.append(field.name, field.bytes);
        }
        let request = stravia_vendor_sdk::net::ConnectRequest {
            url: request.url,
            headers,
        };
        match self.services.ws_connect(request, options).await {
            Ok(socket) => self
                .table
                .push(WebSocketResource(socket))
                .map(Ok)
                .map_err(Into::into),
            Err(error) => Ok(Err(to_net_failure(error))),
        }
    }
}

impl net_types::HostOptions for StoreState {
    async fn new(&mut self) -> wasmtime::Result<Resource<NetOptionsResource>> {
        self.network_active()
            .map_err(|error| wasmtime::Error::msg(error.message))?;
        Ok(self
            .table
            .push(NetOptionsResource(stravia_vendor_sdk::net::Options::new()))?)
    }

    async fn set(
        &mut self,
        options: Resource<NetOptionsResource>,
        key: String,
        value: String,
    ) -> Result<Result<(), net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_net_failure(error)));
        }
        if let Err(error) = stravia_vendor_sdk::net::validate_host_option(&key, &value)
            .and_then(|()| self.services.validate_net_option(&key, &value))
        {
            return Ok(Err(net_types::NetError {
                code: "unsupported-option".into(),
                message: error.to_string(),
                status: None,
                retry_after_millis: None,
                model_error_kind: None,
                transport_failure: None,
            }));
        }
        // Host validation is stricter than the SDK's local grammar.
        if let Err(error) = self.table.get_mut(&options)?.0.set(&key, &value) {
            return Ok(Err(net_types::NetError {
                code: "unsupported-option".into(),
                message: error.to_string(),
                status: None,
                retry_after_millis: None,
                model_error_kind: None,
                transport_failure: None,
            }));
        }
        Ok(Ok(()))
    }

    async fn drop(&mut self, options: Resource<NetOptionsResource>) -> wasmtime::Result<()> {
        self.table.delete(options)?;
        Ok(())
    }
}

impl net_types::Host for StoreState {}

impl net_http::Host for StoreState {
    async fn start(
        &mut self,
        request: net_http::Request,
        options: Resource<NetOptionsResource>,
    ) -> Result<Result<Resource<HttpResponseResource>, net_types::NetError>, wasmtime::Error> {
        self.start_http(request, options)
    }
}

impl net_ws::Host for StoreState {
    async fn connect(
        &mut self,
        request: net_ws::ConnectRequest,
        options: Resource<NetOptionsResource>,
    ) -> Result<Result<Resource<WebSocketResource>, net_types::NetError>, wasmtime::Error> {
        self.connect_ws(request, options).await
    }
}

impl wit_host::Host for StoreState {
    async fn read_private_state(
        &mut self,
    ) -> Result<Result<Option<Vec<u8>>, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        match self.services.read_private_state().await {
            Ok(Some(bytes)) => Ok(Ok(Some(bytes))),
            Ok(None) => Ok(Ok(None)),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn write_private_state(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<Result<(), wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        if self.operation == Some(Operation::ConnectionCheck) {
            return Ok(Err(to_wit_failure(HostFailure::new(
                ErrorKind::Unsupported,
                "connection checks cannot write private state",
            ))));
        }
        match self.services.write_private_state(bytes).await {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn emit_started(
        &mut self,
    ) -> Result<Result<(), wit_types::PluginError>, wasmtime::Error> {
        let operation = match self.operation_active() {
            Ok(operation) => operation,
            Err(error) => return Ok(Err(to_wit_failure(error))),
        };
        if operation == Operation::ConnectionCheck {
            return Ok(Err(to_wit_failure(HostFailure::new(
                ErrorKind::Unsupported,
                "connection checks cannot start generation",
            ))));
        }
        if matches!(operation, Operation::Infer | Operation::Compact) && self.upstream_starts != 0 {
            return Ok(Err(to_wit_failure(HostFailure::new(
                ErrorKind::Trapped,
                "inference and compaction may start only one model request",
            ))));
        }
        match self
            .services
            .emit_event(RuntimeEvent::UpstreamStarted)
            .await
        {
            Ok(()) => {
                self.upstream_starts += 1;
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn emit_event(
        &mut self,
        event: wit_types::CanonicalEvent,
    ) -> Result<Result<(), wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        if self.operation == Some(Operation::ConnectionCheck) {
            return Ok(Err(to_wit_failure(HostFailure::new(
                ErrorKind::Unsupported,
                "connection checks cannot emit generation events",
            ))));
        }
        if let wit_types::CanonicalEvent::Failed(failure) = &event {
            self.services.log(LogLevel::Warn, &failure.message);
        }
        let event = match decode_event(event) {
            Ok(value) => value,
            Err(error) => return Ok(Err(to_wit_failure(error))),
        };
        match self.services.emit_event(event).await {
            Ok(()) => Ok(Ok(())),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn log(&mut self, level: wit_host::LogLevel, message: String) -> wasmtime::Result<()> {
        if self.active().is_ok() {
            self.services.log(
                match level {
                    wit_host::LogLevel::Debug => LogLevel::Debug,
                    wit_host::LogLevel::Info => LogLevel::Info,
                    wit_host::LogLevel::Warn => LogLevel::Warn,
                    wit_host::LogLevel::Error => LogLevel::Error,
                },
                &message,
            );
        }
        Ok(())
    }
}

fn state_activity_error(error: HostFailure) -> wit_state::Error {
    let code = match error.kind {
        ErrorKind::Cancelled => "cancelled",
        ErrorKind::DeadlineExceeded => "deadline-exceeded",
        _ => "unsupported",
    };
    to_state_error(StateError::new(code, error.message))
}

fn to_state_error(error: StateError) -> wit_state::Error {
    wit_state::Error {
        code: error.code,
        message: error.message,
    }
}

impl wit_state::Host for StoreState {
    async fn open(
        &mut self,
        scope: wit_state::Scope,
        key: String,
    ) -> Result<Result<Resource<StateCellResource>, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        if key.len() > 128 {
            return Ok(Err(to_state_error(StateError::new(
                "invalid",
                "state key exceeds 128 bytes",
            ))));
        }
        let scope = match scope {
            wit_state::Scope::Provider => StateScope::Provider,
            wit_state::Scope::Conversation => StateScope::Conversation,
        };
        match self.services.open_state(scope, key).await {
            Ok(cell) => self
                .table
                .push(StateCellResource(cell))
                .map(Ok)
                .map_err(Into::into),
            Err(error) => Ok(Err(to_state_error(error))),
        }
    }
}

impl wit_volatile::Host for StoreState {
    async fn open(
        &mut self,
        key: String,
    ) -> Result<Result<Resource<StateCellResource>, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        if key.len() > 128 {
            return Ok(Err(to_state_error(StateError::new(
                "invalid",
                "state key exceeds 128 bytes",
            ))));
        }
        match self.services.open_volatile_state(key).await {
            Ok(cell) => self
                .table
                .push(StateCellResource(cell))
                .map(Ok)
                .map_err(Into::into),
            Err(error) => Ok(Err(to_state_error(error))),
        }
    }
}

impl wit_journal::Host for StoreState {
    async fn load(
        &mut self,
        cell: Resource<StateCellResource>,
    ) -> Result<Result<wit_journal::Snapshot, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        let cell = self.table.get(&cell)?.0.clone();
        let snapshot = match cell.journal_load().await {
            Ok(snapshot) => snapshot,
            Err(error) => return Ok(Err(to_state_error(error))),
        };
        let head = snapshot
            .head
            .map(|entry| self.table.push(JournalEntryResource(entry)))
            .transpose()?;
        Ok(Ok(wit_journal::Snapshot {
            revision: snapshot.revision,
            payload: snapshot.payload,
            head,
        }))
    }

    async fn append(
        &mut self,
        cell: Resource<StateCellResource>,
        text: String,
        previous: Option<Resource<JournalEntryResource>>,
    ) -> Result<Result<Resource<JournalEntryResource>, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        if text.len() > 64 * 1024 * 1024 {
            return Ok(Err(to_state_error(StateError::new(
                "resource-exhausted",
                "journal document exceeds 64 MiB",
            ))));
        }
        let cell = self.table.get(&cell)?.0.clone();
        let previous = previous
            .map(|entry| self.table.get(&entry).map(|entry| entry.0.clone()))
            .transpose()?;
        match cell.journal_append(text, previous).await {
            Ok(entry) => self
                .table
                .push(JournalEntryResource(entry))
                .map(Ok)
                .map_err(Into::into),
            Err(error) => Ok(Err(to_state_error(error))),
        }
    }

    async fn compare_exchange(
        &mut self,
        cell: Resource<StateCellResource>,
        expected_revision: u64,
        payload: Option<Vec<u8>>,
        head: Option<Resource<JournalEntryResource>>,
    ) -> Result<Result<bool, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        if payload
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_PAYLOAD_BYTES)
        {
            return Ok(Err(to_state_error(StateError::new(
                "resource-exhausted",
                "state payload exceeds 1 MiB",
            ))));
        }
        let cell = self.table.get(&cell)?.0.clone();
        let head = head
            .map(|entry| self.table.get(&entry).map(|entry| entry.0.clone()))
            .transpose()?;
        Ok(cell
            .journal_compare_exchange(expected_revision, payload, head)
            .await
            .map_err(to_state_error))
    }
}

impl wit_journal::HostEntry for StoreState {
    async fn read(
        &mut self,
        entry: Resource<JournalEntryResource>,
    ) -> Result<Result<wit_journal::EntryData, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        let entry = self.table.get(&entry)?.0.clone();
        let data = match entry.read().await {
            Ok(data) => data,
            Err(error) => return Ok(Err(to_state_error(error))),
        };
        let previous = data
            .previous
            .map(|entry| self.table.push(JournalEntryResource(entry)))
            .transpose()?;
        Ok(Ok(wit_journal::EntryData {
            text: data.text,
            previous,
        }))
    }

    async fn drop(&mut self, entry: Resource<JournalEntryResource>) -> wasmtime::Result<()> {
        self.table.delete(entry)?;
        Ok(())
    }
}

impl wit_state::HostCell for StoreState {
    async fn load(
        &mut self,
        cell: Resource<StateCellResource>,
    ) -> Result<Result<wit_state::Snapshot, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        let cell = self.table.get(&cell)?.0.clone();
        Ok(cell
            .load()
            .await
            .map(|snapshot| wit_state::Snapshot {
                revision: snapshot.revision,
                payload: snapshot.payload,
            })
            .map_err(to_state_error))
    }

    async fn compare_exchange(
        &mut self,
        cell: Resource<StateCellResource>,
        expected_revision: u64,
        payload: Option<Vec<u8>>,
    ) -> Result<Result<bool, wit_state::Error>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(state_activity_error(error)));
        }
        if payload
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_PAYLOAD_BYTES)
        {
            return Ok(Err(to_state_error(StateError::new(
                "resource-exhausted",
                "state payload exceeds 1 MiB",
            ))));
        }
        let cell = self.table.get(&cell)?.0.clone();
        Ok(cell
            .compare_exchange(expected_revision, payload)
            .await
            .map_err(to_state_error))
    }

    async fn drop(&mut self, cell: Resource<StateCellResource>) -> wasmtime::Result<()> {
        self.table.delete(cell)?;
        Ok(())
    }
}

impl net_http::HostResponse for StoreState {
    async fn status(
        &mut self,
        response: Resource<HttpResponseResource>,
    ) -> Result<Result<u16, net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_net_failure(error)));
        }
        let response = self.table.get(&response)?.0.clone();
        match response.status().await {
            Ok(status) => {
                if self.operation == Some(Operation::ConnectionCheck)
                    && (200..300).contains(&status)
                {
                    self.received_check_response = true;
                }
                Ok(Ok(status))
            }
            Err(error) => Ok(Err(to_net_failure(error))),
        }
    }

    async fn headers(
        &mut self,
        response: Resource<HttpResponseResource>,
    ) -> Result<Result<Vec<net_types::Field>, net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_net_failure(error)));
        }
        let response = self.table.get(&response)?.0.clone();
        Ok(response
            .headers()
            .await
            .map(|fields| {
                fields
                    .into_iter()
                    .map(|field| net_types::Field {
                        name: field.name,
                        bytes: field.bytes,
                    })
                    .collect()
            })
            .map_err(to_net_failure))
    }

    async fn read_body(
        &mut self,
        response: Resource<HttpResponseResource>,
    ) -> Result<Result<Option<Vec<u8>>, net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_net_failure(error)));
        }
        let response = self.table.get(&response)?.0.clone();
        let result = response.read_body().await;
        if matches!(&result, Ok(Some(bytes)) if !bytes.is_empty()) {
            self.deadline.upstream_activity();
        }
        Ok(result.map_err(to_net_failure))
    }

    async fn drop(&mut self, response: Resource<HttpResponseResource>) -> wasmtime::Result<()> {
        self.table.delete(response)?;
        Ok(())
    }
}

impl net_ws::HostConnection for StoreState {
    async fn is_fresh_handshake(
        &mut self,
        socket: Resource<WebSocketResource>,
    ) -> Result<Result<bool, net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_net_failure(error)));
        }
        Ok(Ok(self.table.get(&socket)?.0.is_fresh_handshake()))
    }

    async fn handshake_status(
        &mut self,
        socket: Resource<WebSocketResource>,
    ) -> wasmtime::Result<u16> {
        self.operation_active()
            .map_err(|error| wasmtime::Error::msg(error.message))?;
        Ok(self.table.get(&socket)?.0.handshake_status())
    }

    async fn handshake_headers(
        &mut self,
        socket: Resource<WebSocketResource>,
    ) -> wasmtime::Result<Vec<net_types::Field>> {
        self.operation_active()
            .map_err(|error| wasmtime::Error::msg(error.message))?;
        Ok(self
            .table
            .get(&socket)?
            .0
            .handshake_headers()
            .into_iter()
            .map(|field| net_types::Field {
                name: field.name,
                bytes: field.bytes,
            })
            .collect())
    }

    async fn send(
        &mut self,
        socket: Resource<WebSocketResource>,
        message: net_ws::Message,
    ) -> Result<Result<(), net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_net_failure(error)));
        }
        let message = convert_ws_from_wit(message);
        let socket = self.table.get(&socket)?.0.clone();
        Ok(socket.send(message).await.map_err(to_net_failure))
    }

    async fn next(
        &mut self,
        socket: Resource<WebSocketResource>,
    ) -> Result<Result<Option<net_ws::Message>, net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_net_failure(error)));
        }
        let socket = self.table.get(&socket)?.0.clone();
        let result = socket.next().await;
        if matches!(&result, Ok(Some(_))) {
            self.deadline.upstream_activity();
        }
        Ok(result
            .map(|message| message.map(convert_ws_to_wit))
            .map_err(to_net_failure))
    }

    async fn close(
        &mut self,
        socket: Resource<WebSocketResource>,
        continuation_id: Option<String>,
    ) -> Result<Result<(), net_types::NetError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_net_failure(error)));
        }
        if continuation_id.is_some() && self.operation != Some(Operation::Infer) {
            return Ok(Err(to_net_failure(HostFailure::new(
                ErrorKind::Trapped,
                "response continuation can only be declared by inference",
            ))));
        }
        let socket = self.table.get(&socket)?.0.clone();
        Ok(socket.close(continuation_id).await.map_err(to_net_failure))
    }

    async fn drop(&mut self, socket: Resource<WebSocketResource>) -> wasmtime::Result<()> {
        self.table.delete(socket)?;
        Ok(())
    }
}

fn admit_operation(
    descriptor: &VendorDescriptor,
    provider_id: &str,
    snapshot_channel: &str,
    channel: &str,
    operation: Operation,
) -> Result<(), RuntimeError> {
    if snapshot_channel != channel {
        return Err(RuntimeError::InvalidOutput);
    }
    let Some(profile) = descriptor.provider(provider_id) else {
        return Err(RuntimeError::from_guest(
            ErrorKind::Unsupported,
            "vendor operation is not supported".into(),
            None,
        ));
    };
    let Some(channel) = profile
        .channels
        .iter()
        .find(|candidate| candidate.id == channel)
    else {
        return Err(RuntimeError::from_guest(
            ErrorKind::Unsupported,
            "vendor operation is not supported".into(),
            None,
        ));
    };
    if operation
        .required_capability()
        .is_some_and(|capability| !channel.capabilities.contains(&capability))
    {
        return Err(RuntimeError::from_guest(
            ErrorKind::Unsupported,
            "vendor operation is not supported".into(),
            None,
        ));
    }
    Ok(())
}

fn convert_operation(
    value: stravia_vendor_sdk::wit::types::OperationKind,
) -> wit_types::OperationKind {
    match value {
        stravia_vendor_sdk::wit::types::OperationKind::Infer => wit_types::OperationKind::Infer,
        stravia_vendor_sdk::wit::types::OperationKind::Compact => wit_types::OperationKind::Compact,
        stravia_vendor_sdk::wit::types::OperationKind::Search => wit_types::OperationKind::Search,
        stravia_vendor_sdk::wit::types::OperationKind::MediaImage => {
            wit_types::OperationKind::MediaImage
        }
        stravia_vendor_sdk::wit::types::OperationKind::Auth => wit_types::OperationKind::Auth,
        stravia_vendor_sdk::wit::types::OperationKind::Discover => {
            wit_types::OperationKind::Discover
        }
        stravia_vendor_sdk::wit::types::OperationKind::Allowance => {
            wit_types::OperationKind::Allowance
        }
        stravia_vendor_sdk::wit::types::OperationKind::ConfigValidation => {
            wit_types::OperationKind::ConfigValidation
        }
        stravia_vendor_sdk::wit::types::OperationKind::ConnectionCheck => {
            wit_types::OperationKind::ConnectionCheck
        }
    }
}

fn convert_input(
    value: stravia_vendor_sdk::wit::types::OperationInput,
) -> wit_types::OperationInput {
    wit_types::OperationInput {
        provider: convert_provider(value.provider),
        input: convert_payload(value.input),
    }
}

fn convert_provider(
    value: stravia_vendor_sdk::wit::types::ProviderSnapshot,
) -> wit_types::ProviderSnapshot {
    wit_types::ProviderSnapshot {
        provider_id: value.provider_id,
        channel: value.channel,
        base_url: value.base_url,
        protocol: value.protocol,
        options: value.options,
        secrets: value.secrets,
        model: value.model,
        model_metadata: value.model_metadata,
        client_headers: value.client_headers,
        operation_metadata: value.operation_metadata,
    }
}

fn convert_payload(
    value: stravia_vendor_sdk::wit::types::CanonicalPayload,
) -> wit_types::CanonicalPayload {
    wit_types::CanonicalPayload {
        format: value.format,
        body: value.body,
    }
}

fn convert_error_kind(value: wit_types::ErrorKind) -> ErrorKind {
    match value {
        wit_types::ErrorKind::Unsupported => ErrorKind::Unsupported,
        wit_types::ErrorKind::Invalid => ErrorKind::Invalid,
        wit_types::ErrorKind::InvalidResponse => ErrorKind::InvalidResponse,
        wit_types::ErrorKind::Auth => ErrorKind::Auth,
        wit_types::ErrorKind::ContinuationNotFound => ErrorKind::ContinuationNotFound,
        wit_types::ErrorKind::ContinuationUnavailable => ErrorKind::ContinuationUnavailable,
        wit_types::ErrorKind::ProtectedReasoningRejected => ErrorKind::ProtectedReasoningRejected,
        wit_types::ErrorKind::Upstream(failure) => {
            ErrorKind::Upstream(stravia_vendor_sdk::UpstreamFailure {
                model_error_kind: failure.model_error_kind.map(convert_model_error_kind),
                retry_after_milliseconds: failure.retry_after_milliseconds,
                transport_failure: failure.transport_failure,
            })
        }
        wit_types::ErrorKind::Trapped => ErrorKind::Trapped,
        wit_types::ErrorKind::Cancelled => ErrorKind::Cancelled,
        wit_types::ErrorKind::DeadlineExceeded => ErrorKind::DeadlineExceeded,
        wit_types::ErrorKind::ResourceExhausted => ErrorKind::ResourceExhausted,
        wit_types::ErrorKind::ProviderNotFound => ErrorKind::ProviderNotFound,
    }
}

fn convert_model_error_kind(
    value: wit_types::ModelErrorKind,
) -> stravia_vendor_sdk::ModelErrorKind {
    use stravia_vendor_sdk::ModelErrorKind;
    match value {
        wit_types::ModelErrorKind::AuthenticationError => ModelErrorKind::AuthenticationError,
        wit_types::ModelErrorKind::AuthorizationError => ModelErrorKind::AuthorizationError,
        wit_types::ModelErrorKind::NotFoundError => ModelErrorKind::NotFoundError,
        wit_types::ModelErrorKind::RateLimitError => ModelErrorKind::RateLimitError,
        wit_types::ModelErrorKind::QuotaExceeded => ModelErrorKind::QuotaExceeded,
        wit_types::ModelErrorKind::InvalidRequest => ModelErrorKind::InvalidRequest,
        wit_types::ModelErrorKind::ServerError => ModelErrorKind::ServerError,
        wit_types::ModelErrorKind::ServiceUnavailable => ModelErrorKind::ServiceUnavailable,
        wit_types::ModelErrorKind::Timeout => ModelErrorKind::Timeout,
        wit_types::ModelErrorKind::ContentFiltered => ModelErrorKind::ContentFiltered,
        wit_types::ModelErrorKind::ContextLengthExceeded => ModelErrorKind::ContextLengthExceeded,
        wit_types::ModelErrorKind::ModelNotAvailable => ModelErrorKind::ModelNotAvailable,
        wit_types::ModelErrorKind::StreamMidError => ModelErrorKind::StreamMidError,
        wit_types::ModelErrorKind::UnexpectedEof => ModelErrorKind::UnexpectedEof,
        wit_types::ModelErrorKind::Unknown => ModelErrorKind::Unknown,
        wit_types::ModelErrorKind::Other(value) => ModelErrorKind::Other(value),
    }
}

fn to_net_failure(failure: HostFailure) -> net_types::NetError {
    use stravia_vendor_sdk::ModelErrorKind as M;
    let (code, retry_after_millis, model_error_kind, transport_failure) = match failure.kind {
        ErrorKind::Unsupported => ("unsupported", None, None, None),
        ErrorKind::Invalid => ("invalid-request", None, None, None),
        ErrorKind::InvalidResponse => ("invalid-response", None, None, None),
        ErrorKind::Auth => ("auth", None, None, None),
        ErrorKind::ContinuationNotFound => ("continuation-not-found", None, None, None),
        ErrorKind::ContinuationUnavailable => ("continuation-unavailable", None, None, None),
        ErrorKind::ProtectedReasoningRejected => ("protected-reasoning-rejected", None, None, None),
        ErrorKind::Trapped => ("trapped", None, None, None),
        ErrorKind::Cancelled => ("cancelled", None, None, None),
        ErrorKind::DeadlineExceeded => ("deadline-exceeded", None, None, None),
        ErrorKind::ResourceExhausted => ("resource-exhausted", None, None, None),
        ErrorKind::ProviderNotFound => ("provider-not-found", None, None, None),
        ErrorKind::Upstream(facts) => {
            let model = facts.model_error_kind.map(|kind| match kind {
                M::AuthenticationError => "authentication-error".into(),
                M::AuthorizationError => "authorization-error".into(),
                M::NotFoundError => "not-found-error".into(),
                M::RateLimitError => "rate-limit-error".into(),
                M::QuotaExceeded => "quota-exceeded".into(),
                M::InvalidRequest => "invalid-request".into(),
                M::ServerError => "server-error".into(),
                M::ServiceUnavailable => "service-unavailable".into(),
                M::Timeout => "timeout".into(),
                M::ContentFiltered => "content-filtered".into(),
                M::ContextLengthExceeded => "context-length-exceeded".into(),
                M::ModelNotAvailable => "model-not-available".into(),
                M::StreamMidError => "stream-mid-error".into(),
                M::UnexpectedEof => "unexpected-eof".into(),
                M::Unknown => "unknown".into(),
                M::Other(value) => value,
            });
            (
                "upstream",
                facts.retry_after_milliseconds,
                model,
                facts.transport_failure,
            )
        }
    };
    net_types::NetError {
        code: code.into(),
        message: failure.message,
        status: failure.upstream_status,
        retry_after_millis,
        model_error_kind,
        transport_failure,
    }
}

fn to_wit_failure(value: HostFailure) -> wit_types::PluginError {
    let kind = match value.kind {
        ErrorKind::Unsupported => wit_types::ErrorKind::Unsupported,
        ErrorKind::Invalid => wit_types::ErrorKind::Invalid,
        ErrorKind::InvalidResponse => wit_types::ErrorKind::InvalidResponse,
        ErrorKind::Auth => wit_types::ErrorKind::Auth,
        ErrorKind::ContinuationNotFound => wit_types::ErrorKind::ContinuationNotFound,
        ErrorKind::ContinuationUnavailable => wit_types::ErrorKind::ContinuationUnavailable,
        ErrorKind::ProtectedReasoningRejected => wit_types::ErrorKind::ProtectedReasoningRejected,
        ErrorKind::Upstream(failure) => {
            wit_types::ErrorKind::Upstream(wit_types::UpstreamFailure {
                model_error_kind: failure.model_error_kind.map(to_wit_model_error_kind),
                retry_after_milliseconds: failure.retry_after_milliseconds,
                transport_failure: failure.transport_failure,
            })
        }
        ErrorKind::Trapped => wit_types::ErrorKind::Trapped,
        ErrorKind::Cancelled => wit_types::ErrorKind::Cancelled,
        ErrorKind::DeadlineExceeded => wit_types::ErrorKind::DeadlineExceeded,
        ErrorKind::ResourceExhausted => wit_types::ErrorKind::ResourceExhausted,
        ErrorKind::ProviderNotFound => wit_types::ErrorKind::ProviderNotFound,
    };
    wit_types::PluginError {
        kind,
        message: value.message,
        upstream_status: value.upstream_status,
    }
}

fn to_wit_model_error_kind(value: stravia_vendor_sdk::ModelErrorKind) -> wit_types::ModelErrorKind {
    use stravia_vendor_sdk::ModelErrorKind;
    match value {
        ModelErrorKind::AuthenticationError => wit_types::ModelErrorKind::AuthenticationError,
        ModelErrorKind::AuthorizationError => wit_types::ModelErrorKind::AuthorizationError,
        ModelErrorKind::NotFoundError => wit_types::ModelErrorKind::NotFoundError,
        ModelErrorKind::RateLimitError => wit_types::ModelErrorKind::RateLimitError,
        ModelErrorKind::QuotaExceeded => wit_types::ModelErrorKind::QuotaExceeded,
        ModelErrorKind::InvalidRequest => wit_types::ModelErrorKind::InvalidRequest,
        ModelErrorKind::ServerError => wit_types::ModelErrorKind::ServerError,
        ModelErrorKind::ServiceUnavailable => wit_types::ModelErrorKind::ServiceUnavailable,
        ModelErrorKind::Timeout => wit_types::ModelErrorKind::Timeout,
        ModelErrorKind::ContentFiltered => wit_types::ModelErrorKind::ContentFiltered,
        ModelErrorKind::ContextLengthExceeded => wit_types::ModelErrorKind::ContextLengthExceeded,
        ModelErrorKind::ModelNotAvailable => wit_types::ModelErrorKind::ModelNotAvailable,
        ModelErrorKind::StreamMidError => wit_types::ModelErrorKind::StreamMidError,
        ModelErrorKind::UnexpectedEof => wit_types::ModelErrorKind::UnexpectedEof,
        ModelErrorKind::Unknown => wit_types::ModelErrorKind::Unknown,
        ModelErrorKind::Other(value) => wit_types::ModelErrorKind::Other(value),
    }
}

fn decode_event(value: wit_types::CanonicalEvent) -> Result<RuntimeEvent, HostFailure> {
    match value {
        wit_types::CanonicalEvent::Delta(payload) => {
            ensure_format(payload.format)?;
            let delta = serde_json::from_slice(&payload.body).map_err(|_| {
                HostFailure::new(ErrorKind::Trapped, "invalid canonical stream delta")
            })?;
            Ok(RuntimeEvent::Delta(delta))
        }
        wit_types::CanonicalEvent::Completed(payload) => {
            ensure_format(payload.format)?;
            serde_json::from_slice::<stravia_runtime_contract::protocol::ir::AiResponse>(
                &payload.body,
            )
            .map_err(|_| HostFailure::new(ErrorKind::Trapped, "invalid canonical response"))?;
            Ok(RuntimeEvent::Completed)
        }
        wit_types::CanonicalEvent::Compacted(payload) => {
            ensure_format(payload.format)?;
            serde_json::from_slice::<
                stravia_runtime_contract::protocol::ir::NativeCompactionResponse,
            >(&payload.body)
            .map_err(|_| {
                HostFailure::new(ErrorKind::Trapped, "invalid canonical compaction response")
            })?;
            Ok(RuntimeEvent::Compacted)
        }
        wit_types::CanonicalEvent::Failed(failure) => Ok(RuntimeEvent::Failed {
            kind: convert_error_kind(failure.kind),
            message: failure.message,
            upstream_status: failure.upstream_status,
        }),
    }
}

fn ensure_format(format: u32) -> Result<(), HostFailure> {
    if format == CANONICAL_FORMAT_VERSION {
        Ok(())
    } else {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "unsupported canonical format",
        ))
    }
}

/// 创建并验证编译缓存目录。只接受绝对路径，拒绝末级符号链接/非目录，
/// Unix 上收紧到仅属主可访问，并用写入探针尽早暴露只读或空间错误。
fn prepare_cache_directory(directory: &Path) -> Result<PathBuf, LoadError> {
    if directory.as_os_str().is_empty() || !directory.is_absolute() {
        return Err(LoadError::CompilationCache(format!(
            "cache directory must be an absolute path: {}",
            directory.display()
        )));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(directory)
        .map_err(|error| cache_directory_error(directory, "create", error))?;
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| cache_directory_error(directory, "inspect", error))?;
    if !metadata.file_type().is_dir() {
        return Err(LoadError::CompilationCache(format!(
            "cache path is not a directory: {}",
            directory.display()
        )));
    }
    // 引擎只使用真实路径，父目录中的可信符号链接不会成为后续缓存访问的跳板。
    let resolved = std::fs::canonicalize(directory)
        .map_err(|error| cache_directory_error(directory, "resolve", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::metadata(&resolved)
            .map_err(|error| cache_directory_error(directory, "inspect resolved", error))?;
        // SAFETY: geteuid has no preconditions and touches no memory.
        let current_user = unsafe { libc::geteuid() };
        if metadata.uid() != current_user {
            return Err(LoadError::CompilationCache(format!(
                "cache directory {} is not owned by the current user",
                directory.display()
            )));
        }
        // 已允许其他用户写入的缓存可能包含伪造机器码，收紧权限也不能恢复其可信性。
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(LoadError::CompilationCache(format!(
                "cache directory {} is writable by another user",
                directory.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&resolved, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| cache_directory_error(directory, "restrict", error))?;
        }
        // 仅保护末级目录不足以保护原生编译产物：可写父目录仍允许替换整个路径。
        // /tmp 等 sticky 目录允许共享写入，但只有可信属主才能控制其中的既有子目录。
        for ancestor in resolved.ancestors().skip(1) {
            let metadata = std::fs::metadata(ancestor)
                .map_err(|error| cache_directory_error(ancestor, "inspect ancestor of", error))?;
            let mode = metadata.permissions().mode();
            if (metadata.uid() != current_user && metadata.uid() != 0)
                || (mode & 0o022 != 0 && mode & 0o1000 == 0)
            {
                return Err(LoadError::CompilationCache(format!(
                    "cache ancestor is controlled by another user: {}",
                    ancestor.display()
                )));
            }
        }
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let probe = resolved.join(format!(
        ".stravia-cache-write-probe-{}-{nonce}",
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&probe)
        .map_err(|error| cache_directory_error(directory, "write", error))?;
    let written = file.write_all(b"stravia compilation cache probe");
    drop(file);
    let removed = std::fs::remove_file(&probe);
    written.map_err(|error| cache_directory_error(directory, "write", error))?;
    removed.map_err(|error| cache_directory_error(directory, "remove write probe from", error))?;
    Ok(resolved)
}

fn cache_directory_error(directory: &Path, action: &str, error: std::io::Error) -> LoadError {
    LoadError::CompilationCache(format!(
        "{action} cache directory {}: {error}",
        directory.display()
    ))
}

fn static_selected_protocol(
    descriptor: &VendorDescriptor,
    provider_id: &str,
    channel_id: &str,
    configured_protocol: &str,
) -> Option<String> {
    let channel = descriptor
        .providers
        .iter()
        .find(|profile| profile.provider_id == provider_id)?
        .channels
        .iter()
        .find(|candidate| candidate.id == channel_id)?;
    match channel.static_protocol.as_ref()? {
        stravia_vendor_sdk::StaticProtocol::Configured => {
            (!configured_protocol.is_empty()).then(|| configured_protocol.to_owned())
        }
        stravia_vendor_sdk::StaticProtocol::Fixed(protocol) => {
            (!protocol.is_empty()).then(|| protocol.clone())
        }
    }
}

fn classify_trap(error: &wasmtime::Error) -> RuntimeError {
    if error
        .downcast_ref::<wasmtime::Trap>()
        .is_some_and(|trap| matches!(trap, wasmtime::Trap::OutOfFuel))
    {
        RuntimeError::ResourceExhausted
    } else {
        RuntimeError::Trapped
    }
}

fn convert_ws_from_wit(value: net_ws::Message) -> WebSocketMessage {
    match value {
        net_ws::Message::Text(value) => WebSocketMessage::Text(value),
        net_ws::Message::Binary(value) => WebSocketMessage::Binary(value),
        net_ws::Message::Ping(value) => WebSocketMessage::Ping(value),
        net_ws::Message::Pong(value) => WebSocketMessage::Pong(value),
        net_ws::Message::Close(value) => WebSocketMessage::Close(value),
    }
}

fn convert_ws_to_wit(value: WebSocketMessage) -> net_ws::Message {
    match value {
        WebSocketMessage::Text(value) => net_ws::Message::Text(value),
        WebSocketMessage::Binary(value) => net_ws::Message::Binary(value),
        WebSocketMessage::Ping(value) => net_ws::Message::Ping(value),
        WebSocketMessage::Pong(value) => net_ws::Message::Pong(value),
        WebSocketMessage::Close(value) => net_ws::Message::Close(value),
    }
}

struct DenyServices;

#[async_trait]
impl HostServices for DenyServices {
    fn http_start(
        &self,
        _request: Request,
        _options: stravia_vendor_sdk::net::Options,
    ) -> Result<Arc<dyn crate::host::HostHttpResponse>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "host services unavailable during pure guest call",
        ))
    }

    async fn ws_connect(
        &self,
        _request: ConnectRequest,
        _options: stravia_vendor_sdk::net::Options,
    ) -> Result<Arc<dyn crate::host::HostWebSocket>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "host services unavailable during pure guest call",
        ))
    }

    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "host services unavailable during pure guest call",
        ))
    }

    async fn write_private_state(&self, _bytes: Vec<u8>) -> Result<(), HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "host services unavailable during pure guest call",
        ))
    }

    async fn emit_event(&self, _event: RuntimeEvent) -> Result<(), HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "host services unavailable during pure guest call",
        ))
    }

    fn log(&self, _level: LogLevel, _message: &str) {}

    fn generation_is_current(&self, _generation: u64) -> bool {
        true
    }
}

#[cfg(test)]
mod net_error_tests {
    use super::*;

    #[test]
    fn invalid_network_request_has_documented_code() {
        let error = to_net_failure(HostFailure::new(ErrorKind::Invalid, "invalid header"));
        assert_eq!(error.code, "invalid-request");
        assert_eq!(error.message, "invalid header");
        assert!(error.status.is_none());
        assert!(error.retry_after_millis.is_none());
        assert!(error.model_error_kind.is_none());
        assert!(error.transport_failure.is_none());
        assert_eq!(
            to_net_failure(HostFailure::new(ErrorKind::Unsupported, "option")).code,
            "unsupported"
        );
        assert_eq!(
            to_net_failure(HostFailure::new(ErrorKind::Trapped, "trap")).code,
            "trapped"
        );
    }
}

#[cfg(test)]
#[path = "runtime/state_tests.rs"]
mod state_tests;

#[cfg(test)]
#[path = "compilation_cache_tests.rs"]
mod compilation_cache_tests;

#[cfg(test)]
#[path = "residency_tests.rs"]
mod residency_tests;

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::time::Duration;

    fn state(deadline: Deadline) -> StoreState {
        StoreState {
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            services: Arc::new(DenyServices),
            cancellation: CancellationToken::new(),
            deadline,
            generation: 0,
            operation: None,
            upstream_starts: 0,
            received_check_response: false,
            catalog_sync: false,
        }
    }

    #[tokio::test]
    async fn host_boundary_activity_renews_shared_deadline() {
        let deadline = Deadline::from_now(Duration::from_secs(60));
        let state = state(deadline.clone());
        // 不依赖线程在毫秒级 sleep 后准时获得调度；先建立仍有效但更早的共享截止时间。
        deadline.reset(std::time::Instant::now() + Duration::from_secs(30));
        let original = deadline.at();
        state.active().expect("boundary call before expiry");
        // The renewal pushed the shared instant out, so a clone held by an
        // outer layer observes the extension too.
        assert!(deadline.at() > original);
        assert!(!deadline.is_exceeded());
    }

    #[tokio::test]
    async fn silent_operation_expires_at_deadline() {
        let deadline = Deadline::from_now(Duration::from_millis(30));
        let state = state(deadline);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let error = state.active().expect_err("silence must expire");
        assert!(matches!(error.kind, ErrorKind::DeadlineExceeded));
    }

    #[tokio::test]
    async fn cancelled_operation_fails_without_renewing() {
        let deadline = Deadline::from_now(Duration::from_secs(60));
        let state = state(deadline.clone());
        state.cancellation.cancel();
        let at = deadline.at();
        let error = state.active().expect_err("cancelled operation");
        assert!(matches!(error.kind, ErrorKind::Cancelled));
        // Cancellation returns before the renewal step, so the shared instant
        // stays untouched.
        assert_eq!(deadline.at(), at);
    }
}

#[cfg(test)]
mod profile_admission_tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use stravia_vendor_sdk::{
        ChannelDescriptor, DataCompatibility, NetworkDeclaration, ProviderDescriptor, VendorKind,
    };

    fn profile(
        provider_id: &str,
        capability: stravia_vendor_sdk::Capability,
    ) -> ProviderDescriptor {
        let capabilities = BTreeSet::from([capability]);
        ProviderDescriptor {
            provider_id: provider_id.into(),
            catalog_id: None,
            icon_svg: None,
            display_name: provider_id.into(),
            description: None,
            channels: vec![ChannelDescriptor {
                id: "default".into(),
                name: stravia_vendor_sdk::LocalizedText::english("Default"),
                description: None,
                auth: None,
                protocol: Some("test".into()),
                protocols: Vec::new(),
                static_protocol: None,
                default_base_url: None,
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                client_header_names: BTreeSet::new(),
                model_capabilities: BTreeSet::new(),
            }],
            capabilities,
            config_groups: Vec::new(),
            config_fields: Vec::new(),
            network: NetworkDeclaration::default(),
            data_compat: DataCompatibility::default(),
            website: None,
            implementation: None,
        }
    }

    fn descriptor() -> VendorDescriptor {
        VendorDescriptor {
            egress_shapes: Vec::new(),
            vendor_id: "base".into(),
            version: "1.0.0".parse().expect("valid test version"),
            display_name: "Base".into(),
            description: None,
            authors: Vec::new(),
            canonical_format_version: CANONICAL_FORMAT_VERSION,
            kind: VendorKind::Fallback,
            providers: vec![
                profile("alpha", stravia_vendor_sdk::Capability::Infer),
                profile("beta", stravia_vendor_sdk::Capability::Search),
            ],
        }
    }

    fn snapshot(provider_id: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: provider_id.into(),
            channel: "default".into(),
            base_url: "https://example.invalid".into(),
            protocol: "test".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: None,
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn admission_does_not_union_capabilities_across_profiles() {
        let provider = snapshot("alpha");
        let error = admit_operation(
            &descriptor(),
            &provider.provider_id,
            &provider.channel,
            "default",
            Operation::Search,
        )
        .expect_err("alpha must not inherit beta search support");
        assert!(matches!(
            error,
            RuntimeError::Plugin {
                kind: ErrorKind::Unsupported,
                ..
            }
        ));
    }

    #[test]
    fn admission_allows_connection_check_without_capability() {
        admit_operation(
            &descriptor(),
            "alpha",
            "default",
            "default",
            Operation::ConnectionCheck,
        )
        .expect("required connection check must not need an optional capability");
    }

    #[test]
    fn connection_check_admission_rejects_unknown_profile() {
        let error = admit_operation(
            &descriptor(),
            "gamma",
            "default",
            "default",
            Operation::ConnectionCheck,
        )
        .expect_err("required checks must not inherit another profile");
        assert!(matches!(
            error,
            RuntimeError::Plugin {
                kind: ErrorKind::Unsupported,
                ..
            }
        ));
    }

    #[test]
    fn connection_check_admission_rejects_unknown_channel() {
        let error = admit_operation(
            &descriptor(),
            "alpha",
            "other",
            "other",
            Operation::ConnectionCheck,
        )
        .expect_err("required checks must not inherit another channel");
        assert!(matches!(
            error,
            RuntimeError::Plugin {
                kind: ErrorKind::Unsupported,
                ..
            }
        ));
    }

    #[test]
    fn connection_check_admission_rejects_snapshot_channel_mismatch() {
        let error = admit_operation(
            &descriptor(),
            "alpha",
            "default",
            "other",
            Operation::ConnectionCheck,
        )
        .expect_err("required checks must use the snapshot channel");
        assert!(matches!(error, RuntimeError::InvalidOutput));
    }

    #[test]
    fn admission_rejects_snapshot_channel_mismatch() {
        let provider = snapshot("alpha");
        let error = admit_operation(
            &descriptor(),
            &provider.provider_id,
            &provider.channel,
            "other",
            Operation::Infer,
        )
        .expect_err("execute channel must match the signed snapshot");
        assert!(matches!(error, RuntimeError::InvalidOutput));
    }

    #[test]
    fn compile_thread_count_caps_high_core_hosts_and_respects_override() {
        assert_eq!(compile_thread_count(None, 64), MAX_COMPILE_THREADS);
        assert_eq!(compile_thread_count(None, 2), 2);
        assert_eq!(compile_thread_count(None, 0), 1);
        assert_eq!(compile_thread_count(Some("12"), 64), 12);
        assert_eq!(compile_thread_count(Some(" 3 "), 64), 3);
        assert_eq!(compile_thread_count(Some("0"), 64), MAX_COMPILE_THREADS);
        assert_eq!(
            compile_thread_count(Some("invalid"), 64),
            MAX_COMPILE_THREADS
        );
    }

    #[test]
    fn ephemeral_compile_pool_runs_on_named_worker_threads() {
        let worker_name = compile_in_ephemeral_pool(|| {
            std::thread::current()
                .name()
                .map(str::to_owned)
                .unwrap_or_default()
        });
        assert!(
            worker_name.starts_with("stravia-wasm-compile-"),
            "expected ephemeral compile worker thread, got {worker_name:?}"
        );
    }

    #[test]
    fn static_selected_protocol_resolves_configured_fixed_and_dynamic_modes() {
        let mut manifest = descriptor();
        assert_eq!(
            static_selected_protocol(&manifest, "alpha", "default", "openai-compatible"),
            None
        );

        manifest.providers[0].channels[0].static_protocol =
            Some(stravia_vendor_sdk::StaticProtocol::Configured);
        assert_eq!(
            static_selected_protocol(&manifest, "alpha", "default", "openai-compatible"),
            Some("openai-compatible".into())
        );
        assert_eq!(
            static_selected_protocol(&manifest, "alpha", "default", ""),
            None
        );

        manifest.providers[0].channels[0].static_protocol = Some(
            stravia_vendor_sdk::StaticProtocol::Fixed("open-responses/responses/2026-04-24".into()),
        );
        assert_eq!(
            static_selected_protocol(&manifest, "alpha", "default", "ignored-hint"),
            Some("open-responses/responses/2026-04-24".into())
        );
    }

    #[tokio::test]
    async fn pooling_preserves_memory_growth_beyond_256_mib() {
        let runtime = VendorRuntime::new().expect("runtime");
        let bytes = wat::parse_str(
            r#"(component
                (core module $m
                    (memory 1)
                    (func (export "grow") (result i32)
                        i32.const 4096
                        memory.grow))
                (core instance $i (instantiate $m))
                (func (export "grow") (result u32)
                    (canon lift (core func $i "grow"))))"#,
        )
        .expect("valid growth component");
        let component = Component::new(&runtime.engine, &bytes).expect("compile growth component");
        let pre = Linker::<StoreState>::new(&runtime.engine)
            .instantiate_pre(&component)
            .expect("instantiate_pre");
        let cancel = CancellationToken::new();
        let deadline = Deadline::from_now(Duration::from_secs(30));
        let _slot = runtime
            .acquire_execution_slot(&cancel, &deadline)
            .await
            .expect("slot");
        let scope = OperationScope::new(Arc::new(DenyServices), cancel, deadline, 0);
        let mut store = VendorRuntime::new_store(&runtime.engine, scope, None, false);
        let instance = pre
            .instantiate_async(&mut store)
            .await
            .expect("instantiate");
        let grow = instance
            .get_typed_func::<(), (u32,)>(&mut store, "grow")
            .expect("grow export");
        let (previous_pages,) = grow.call_async(&mut store, ()).await.expect("grow call");
        assert_eq!(
            previous_pages, 1,
            "memory.grow must not acquire a new 256 MiB ceiling"
        );
    }

    #[tokio::test]
    async fn pooling_capacity_covers_every_admitted_execution_slot() {
        let runtime = VendorRuntime::new().expect("runtime");
        // Each permitted component uses its full per-component allowance. All
        // instances stay live so aggregate pool exhaustion cannot hide in reuse.
        let mut source = String::from(
            "(component (core module $resources (memory 0) \
             (table 0 funcref) (table 0 funcref) \
             (table 0 funcref) (table 0 funcref)) (core module $empty)",
        );
        for _ in 0..4 {
            source.push_str("(core instance (instantiate $resources))");
        }
        for _ in 4..64 {
            source.push_str("(core instance (instantiate $empty))");
        }
        source.push(')');
        let bytes = wat::parse_str(&source).expect("valid resource component");
        let component =
            Component::new(&runtime.engine, &bytes).expect("compile resource component");
        let pre = Linker::<StoreState>::new(&runtime.engine)
            .instantiate_pre(&component)
            .expect("instantiate_pre");
        let cancel = CancellationToken::new();
        let deadline = Deadline::from_now(Duration::from_secs(30));
        let mut live = Vec::with_capacity(POOL_CONCURRENCY_LIMIT as usize);
        for index in 0..POOL_CONCURRENCY_LIMIT {
            let slot = runtime
                .acquire_execution_slot(&cancel, &deadline)
                .await
                .expect("slot");
            let scope =
                OperationScope::new(Arc::new(DenyServices), cancel.clone(), deadline.clone(), 0);
            let mut store = VendorRuntime::new_store(&runtime.engine, scope, None, false);
            pre.instantiate_async(&mut store)
                .await
                .unwrap_or_else(|error| {
                    panic!("admitted execution slot {index} exhausted the pool: {error:#}")
                });
            live.push((store, slot));
        }
    }

    #[tokio::test]
    async fn epoch_interruption_preempts_cpu_bound_guest_on_deadline() {
        let runtime = VendorRuntime::new().expect("runtime");
        if cfg!(target_pointer_width = "64") {
            assert!(
                runtime.pool_slots.is_some(),
                "64-bit host should initialize pooling allocator"
            );
        }

        // Pure core loop inside a component: never calls host imports, so only
        // epoch interruption can yield control back to the deadline select.
        let bytes = wat::parse_str(
            r#"
            (component
                (core module $m
                    (func (export "spin") (loop $l (br $l)))
                )
                (core instance $i (instantiate $m))
                (func (export "spin") (canon lift (core func $i "spin")))
            )
            "#,
        )
        .expect("valid spin component");
        let component = Component::new(&runtime.engine, &bytes).expect("compile spin component");
        let linker = Linker::<StoreState>::new(&runtime.engine);
        let pre = linker.instantiate_pre(&component).expect("instantiate_pre");

        let deadline = Deadline::from_now(Duration::from_millis(80));
        let cancel = CancellationToken::new();
        let _slot = runtime
            .acquire_execution_slot(&cancel, &deadline)
            .await
            .expect("slot");
        let scope = OperationScope {
            services: Arc::new(DenyServices),
            cancellation: cancel,
            deadline: deadline.clone(),
            generation: 0,
        };
        let mut store = VendorRuntime::new_store(&runtime.engine, scope, None, false);
        let instance = pre
            .instantiate_async(&mut store)
            .await
            .expect("instantiate");
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .expect("spin export");

        let start = std::time::Instant::now();
        let timed_out = tokio::select! {
            biased;
            () = deadline.wait() => true,
            res = spin.call_async(&mut store, ()) => {
                panic!("infinite loop unexpectedly returned: {res:?}");
            }
        };
        assert!(timed_out);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "epoch preemption took too long: {:?}",
            start.elapsed()
        );
    }
}
