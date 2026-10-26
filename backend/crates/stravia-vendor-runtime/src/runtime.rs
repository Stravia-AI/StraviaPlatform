use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_sdk::{
    AiRequest, CANONICAL_FORMAT_VERSION, ErrorKind, Operation, OperationOutput, ProviderSnapshot,
    VendorDescriptor,
};
use tokio::sync::Mutex as AsyncMutex;
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Cache, CacheConfig, Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings;
use crate::bindings::stravia::vendor::{host as wit_host, types as wit_types};
use crate::error::{LoadError, RuntimeError};
use crate::host::{
    HostFailure, HostServices, HttpRequest, HttpResponseResource, LogLevel, RuntimeEvent,
    WebSocketMessage, WebSocketResource, WsRequest,
};

#[derive(Clone)]
pub struct LoadedPlugin {
    inner: Arc<LoadedVersion>,
}

struct LoadedVersion {
    /// Strong reference that keeps the runtime's Engine-local `Weak<Component>`
    /// compilation entry upgradeable for as long as this plugin is pinned.
    /// Instantiation uses `prepared`, which is bound to this component's
    /// Engine, so a plugin driven by another runtime never switches to the
    /// caller's Engine.
    component: Arc<Component>,
    /// Import-resolved, export-indexed blueprint bound to `component`'s Engine.
    /// Shared by `with_descriptor`, never by different loads.
    prepared: bindings::VendorPre<StoreState>,
    descriptor: VendorDescriptor,
    identity: String,
}

impl LoadedPlugin {
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
        LoadedPlugin {
            inner: Arc::new(LoadedVersion {
                component: self.inner.component.clone(),
                prepared: self.inner.prepared.clone(),
                descriptor,
                identity: self.inner.identity.clone(),
            }),
        }
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
    #[cfg(test)]
    cache: Option<Cache>,
}

struct CompiledEntry {
    digest: [u8; 32],
    component: Arc<AsyncMutex<Weak<Component>>>,
}

const MAX_COMPILED_ENTRIES: usize = 16;

impl VendorRuntime {
    /// 不使用持久编译缓存。Engine、编译结果仍由调用方共享。
    pub fn new() -> Result<Self, LoadError> {
        Self::build(None)
    }

    /// 使用 Wasmtime 官方持久编译缓存（版本、目标 CPU、编译特性与组件内容
    /// 的失效由 Wasmtime 负责）。目录必须为绝对路径；目录及其上层按需创建，
    /// Unix 上权限收紧为 0700，真实路径的祖先不得受不可信用户控制，且必须可写。
    pub fn with_cache(directory: impl Into<PathBuf>) -> Result<Self, LoadError> {
        Self::build(Some(directory.into()))
    }

    fn build(cache_directory: Option<PathBuf>) -> Result<Self, LoadError> {
        let mut engine_config = Config::new();
        engine_config.wasm_component_model(true);
        // Owned host calls release lowered payloads before awaiting the guest.
        // This scheduler does not expand the guest's component feature surface.
        engine_config.concurrency_support(true);
        engine_config.wasm_component_model_async(false);
        engine_config.wasm_component_model_more_async_builtins(false);
        engine_config.wasm_component_model_async_stackful(false);
        engine_config.wasm_component_model_threading(false);
        engine_config.consume_fuel(true);
        let cache = match cache_directory {
            Some(directory) => {
                let directory = prepare_cache_directory(&directory)?;
                let mut cache_config = CacheConfig::new();
                cache_config.with_directory(directory);
                let cache = Cache::new(cache_config).map_err(|error| {
                    LoadError::CompilationCache(format!(
                        "initialize persistent compilation cache: {error}"
                    ))
                })?;
                Some(cache)
            }
            None => None,
        };
        #[cfg(test)]
        let measured_cache = cache.clone();
        engine_config.cache(cache);
        let engine = Engine::new(&engine_config)
            .map_err(|error| LoadError::InvalidComponent(error.to_string()))?;
        Ok(Self {
            engine,
            compiled: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_COMPILED_ENTRIES))),
            #[cfg(test)]
            cache: measured_cache,
        })
    }

    async fn compiled_component(
        &self,
        digest: &[u8; 32],
        bytes: &[u8],
    ) -> Result<Arc<Component>, LoadError> {
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
            return self.compile_component(bytes);
        };
        // Only callers compiling the same identity wait here. The Engine-local
        // metadata lock is released before compilation; other bytes run independently.
        let mut cached = slot.lock().await;
        if let Some(component) = cached.upgrade() {
            return Ok(component);
        }
        let component = self.compile_component(bytes)?;
        *cached = Arc::downgrade(&component);
        Ok(component)
    }

    fn compile_component(&self, bytes: &[u8]) -> Result<Arc<Component>, LoadError> {
        Component::from_binary(&self.engine, bytes)
            .map(Arc::new)
            .map_err(|error| LoadError::InvalidComponent(error.to_string()))
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
        let component = self.compiled_component(&digest, bytes).await?;
        self.validate_imports(&component)?;
        let prepared = Self::prepare(&component)?;
        let descriptor = self.read_descriptor(&prepared).await?;
        descriptor
            .validate()
            .map_err(|error| LoadError::DescriptorInvalid(error.to_string()))?;
        if descriptor.canonical_format_version != CANONICAL_FORMAT_VERSION {
            return Err(LoadError::CanonicalFormat {
                actual: descriptor.canonical_format_version,
                expected: CANONICAL_FORMAT_VERSION,
            });
        }
        Ok(LoadedPlugin {
            inner: Arc::new(LoadedVersion {
                component,
                prepared,
                descriptor,
                identity,
            }),
        })
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

        let (sdk_provider, sdk_request) =
            stravia_vendor_sdk::guest::encode_protocol_selection(provider, request)
                .map_err(|_| RuntimeError::InvalidOutput)?;
        let wit_operation = convert_operation(operation.into());
        let wit_provider = convert_provider(sdk_provider);
        let wit_request = convert_payload(sdk_request);

        let scope = OperationScope {
            services: Arc::new(DenyServices),
            cancellation,
            deadline,
            generation: 0,
        };
        let engine = plugin.inner.prepared.engine();
        let mut store = Self::new_store(engine, scope, None, false);
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline.clone();
        let instantiate = plugin.inner.prepared.instantiate_async(&mut store);
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
        let engine = plugin.inner.prepared.engine();
        let mut store = Self::new_store(engine, scope, Some(operation), false);
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline.clone();
        let instantiate = plugin.inner.prepared.instantiate_async(&mut store);
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
        let engine = plugin.inner.prepared.engine();
        let mut store = Self::new_store(engine, scope, None, true);
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline.clone();
        let instantiate = plugin.inner.prepared.instantiate_async(&mut store);
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
            let allowed = matches!(
                name,
                "stravia:vendor/host@0.6.0" | "stravia:vendor/types@0.6.0"
            ) || name.rsplit_once('@').is_some_and(|(interface, version)| {
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
        // Fuel stays enabled only so the async yield interval can preempt
        // CPU-bound guest work for cancellation and deadlines; the budget
        // itself is unlimited.
        store
            .set_fuel(u64::MAX)
            .expect("fuel consumption is enabled for vendor runtime");
        // 大载荷的 JSON 编解码不应为每个短循环切换 fiber；仍定期让出执行，
        // 使纯 CPU guest 的取消与 deadline 不依赖它调用宿主服务。
        store
            .fuel_async_yield_interval(Some(1_000_000))
            .expect("async support and fuel consumption are enabled for vendor runtime");
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
        request: wit_types::HttpRequest,
        compression: Option<wit_types::HttpCompression>,
    ) -> Result<Result<Resource<HttpResponseResource>, wit_types::PluginError>, wasmtime::Error>
    {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        let request = HttpRequest {
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: request.body,
            compression: compression.map(|compression| stravia_vendor_sdk::HttpCompression {
                request: compression.request.map(convert_http_content_coding),
                response: compression
                    .response
                    .into_iter()
                    .map(convert_http_content_coding)
                    .collect(),
            }),
        };
        match self.services.http_start(request) {
            Ok(response) => self
                .table
                .push(HttpResponseResource(response))
                .map(Ok)
                .map_err(Into::into),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn connect_ws(
        &mut self,
        request: wit_types::WsRequest,
        compression: Option<wit_types::WsCompression>,
    ) -> Result<Result<Resource<WebSocketResource>, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        match self
            .services
            .ws_connect(WsRequest {
                url: request.url,
                headers: request.headers,
                protocols: request.protocols,
                continuation_id: request.continuation_id,
                reuse_max_age_millis: request.reuse_max_age_millis,
                compression: compression.map(convert_ws_compression),
            })
            .await
        {
            Ok(socket) => self
                .table
                .push(WebSocketResource(socket))
                .map(Ok)
                .map_err(Into::into),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }
}

impl wit_host::Host for StoreState {
    async fn http_start(
        &mut self,
        request: wit_types::HttpRequest,
    ) -> Result<Result<Resource<HttpResponseResource>, wit_types::PluginError>, wasmtime::Error>
    {
        self.start_http(request, None)
    }

    async fn http_start_with_compression(
        &mut self,
        request: wit_types::HttpRequest,
        compression: wit_types::HttpCompression,
    ) -> Result<Result<Resource<HttpResponseResource>, wit_types::PluginError>, wasmtime::Error>
    {
        self.start_http(request, Some(compression))
    }

    async fn ws_connect(
        &mut self,
        request: wit_types::WsRequest,
    ) -> Result<Result<Resource<WebSocketResource>, wit_types::PluginError>, wasmtime::Error> {
        self.connect_ws(request, None).await
    }

    async fn ws_connect_with_compression(
        &mut self,
        request: wit_types::WsRequest,
        compression: wit_types::WsCompression,
    ) -> Result<Result<Resource<WebSocketResource>, wit_types::PluginError>, wasmtime::Error> {
        self.connect_ws(request, Some(compression)).await
    }

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

impl wit_host::HostHttpResponse for StoreState {
    async fn status(
        &mut self,
        response: Resource<HttpResponseResource>,
    ) -> Result<Result<u16, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_wit_failure(error)));
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
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn headers(
        &mut self,
        response: Resource<HttpResponseResource>,
    ) -> Result<Result<Vec<(String, String)>, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        let response = self.table.get(&response)?.0.clone();
        match response.headers().await {
            Ok(headers) => Ok(Ok(headers)),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn read_body(
        &mut self,
        response: Resource<HttpResponseResource>,
    ) -> Result<Result<Option<Vec<u8>>, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.network_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        let response = self.table.get(&response)?.0.clone();
        match response.read_body().await {
            Ok(Some(chunk)) => Ok(Ok(Some(chunk))),
            Ok(None) => Ok(Ok(None)),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn drop(&mut self, response: Resource<HttpResponseResource>) -> wasmtime::Result<()> {
        self.table.delete(response)?;
        Ok(())
    }
}

impl wit_host::HostWsConnection for StoreState {
    async fn send(
        &mut self,
        socket: Resource<WebSocketResource>,
        message: wit_types::WsMessage,
    ) -> Result<Result<(), wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        let message = convert_ws_from_wit(message);
        let socket = self.table.get(&socket)?.0.clone();
        Ok(socket.send(message).await.map_err(to_wit_failure))
    }

    async fn next(
        &mut self,
        socket: Resource<WebSocketResource>,
    ) -> Result<Result<Option<wit_types::WsMessage>, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        let socket = self.table.get(&socket)?.0.clone();
        match socket.next().await {
            Ok(Some(message)) => Ok(Ok(Some(convert_ws_to_wit(message)))),
            Ok(None) => Ok(Ok(None)),
            Err(error) => Ok(Err(to_wit_failure(error))),
        }
    }

    async fn close(
        &mut self,
        socket: Resource<WebSocketResource>,
        continuation_id: Option<String>,
    ) -> Result<Result<(), wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        if continuation_id.is_some() && self.operation != Some(Operation::Infer) {
            return Ok(Err(to_wit_failure(HostFailure::new(
                ErrorKind::Trapped,
                "response continuation can only be declared by inference",
            ))));
        }
        let socket = self.table.get(&socket)?.0.clone();
        Ok(socket.close(continuation_id).await.map_err(to_wit_failure))
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
    if !channel
        .capabilities
        .contains(&operation.required_capability())
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
                transport_failure: failure.transport_failure.map(convert_transport_failure),
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
    }
}

fn convert_http_content_coding(
    value: wit_types::HttpContentCoding,
) -> stravia_vendor_sdk::HttpContentCoding {
    use stravia_vendor_sdk::HttpContentCoding;
    match value {
        wit_types::HttpContentCoding::Gzip => HttpContentCoding::Gzip,
        wit_types::HttpContentCoding::Deflate => HttpContentCoding::Deflate,
        wit_types::HttpContentCoding::Br => HttpContentCoding::Br,
        wit_types::HttpContentCoding::Zstd => HttpContentCoding::Zstd,
    }
}

fn convert_ws_compression(value: wit_types::WsCompression) -> stravia_vendor_sdk::WsCompression {
    use stravia_vendor_sdk::{CompressionNegotiation, WsCompression};
    match value {
        wit_types::WsCompression::Disabled => WsCompression::Disabled,
        wit_types::WsCompression::PermessageDeflate(negotiation) => {
            WsCompression::PermessageDeflate(match negotiation {
                wit_types::CompressionNegotiation::Preferred => CompressionNegotiation::Preferred,
                wit_types::CompressionNegotiation::Required => CompressionNegotiation::Required,
            })
        }
    }
}

fn convert_transport_failure(
    value: wit_types::TransportFailure,
) -> stravia_vendor_sdk::TransportFailure {
    match value {
        wit_types::TransportFailure::Websocket => stravia_vendor_sdk::TransportFailure::Websocket,
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
                transport_failure: failure.transport_failure.map(to_wit_transport_failure),
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
    }
}

fn to_wit_transport_failure(
    value: stravia_vendor_sdk::TransportFailure,
) -> wit_types::TransportFailure {
    match value {
        stravia_vendor_sdk::TransportFailure::Websocket => wit_types::TransportFailure::Websocket,
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

/// 创建并验证官方编译缓存目录。只接受绝对路径，拒绝末级符号链接/非目录，
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

fn convert_ws_from_wit(value: wit_types::WsMessage) -> WebSocketMessage {
    match value {
        wit_types::WsMessage::Text(value) => WebSocketMessage::Text(value),
        wit_types::WsMessage::Binary(value) => WebSocketMessage::Binary(value),
        wit_types::WsMessage::Ping(value) => WebSocketMessage::Ping(value),
        wit_types::WsMessage::Pong(value) => WebSocketMessage::Pong(value),
        wit_types::WsMessage::Close(value) => WebSocketMessage::Close(value),
    }
}

fn convert_ws_to_wit(value: WebSocketMessage) -> wit_types::WsMessage {
    match value {
        WebSocketMessage::Text(value) => wit_types::WsMessage::Text(value),
        WebSocketMessage::Binary(value) => wit_types::WsMessage::Binary(value),
        WebSocketMessage::Ping(value) => wit_types::WsMessage::Ping(value),
        WebSocketMessage::Pong(value) => wit_types::WsMessage::Pong(value),
        WebSocketMessage::Close(value) => wit_types::WsMessage::Close(value),
    }
}

struct DenyServices;

#[async_trait]
impl HostServices for DenyServices {
    fn http_start(
        &self,
        _request: HttpRequest,
    ) -> Result<Arc<dyn crate::host::HostHttpResponse>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Trapped,
            "host services unavailable during pure guest call",
        ))
    }

    async fn ws_connect(
        &self,
        _request: WsRequest,
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
#[path = "compilation_cache_tests.rs"]
mod compilation_cache_tests;

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
}
