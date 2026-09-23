use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use stravia_runtime_contract::CancellationToken;
use stravia_vendor_sdk::{
    AiRequest, CANONICAL_FORMAT_VERSION, ErrorKind, Operation, OperationInput, OperationOutput,
    ProviderSnapshot, VendorDescriptor,
};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings;
use crate::bindings::stravia::vendor::{host as wit_host, types as wit_types};
use crate::error::{LoadError, RuntimeError};
use crate::host::{
    HostFailure, HostServices, HttpRequest, HttpResponseResource, LogLevel, RuntimeEvent,
    WebSocketMessage, WebSocketResource,
};

#[derive(Clone)]
pub struct LoadedPlugin {
    inner: Arc<LoadedVersion>,
}

struct LoadedVersion {
    component: Component,
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
}

pub struct OperationScope {
    pub services: Arc<dyn HostServices>,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    pub generation: u64,
}

impl OperationScope {
    pub fn new(
        services: Arc<dyn HostServices>,
        cancellation: CancellationToken,
        deadline: Instant,
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
}

impl VendorRuntime {
    pub fn new() -> Result<Self, LoadError> {
        let mut engine_config = Config::new();
        engine_config.wasm_component_model(true);
        engine_config.consume_fuel(true);
        let engine = Engine::new(&engine_config)
            .map_err(|error| LoadError::InvalidComponent(error.to_string()))?;
        Ok(Self { engine })
    }

    /// Compile, import-check, instantiate, and validate a component without
    /// mutating any installed version. The returned `LoadedPlugin` owns an Arc
    /// to this exact compiled component, so active operations remain pinned.
    pub async fn load(&self, bytes: &[u8]) -> Result<LoadedPlugin, LoadError> {
        let identity = stravia_runtime_contract::identifier::encode_digest(
            &stravia_runtime_contract::protocol::ir::canonical::hash_bytes(bytes),
        );
        let component = Component::from_binary(&self.engine, bytes)
            .map_err(|error| LoadError::InvalidComponent(error.to_string()))?;
        self.validate_imports(&component)?;
        let descriptor = self.read_descriptor(&component).await?;
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
        deadline: Instant,
    ) -> Result<String, RuntimeError> {
        if !matches!(operation, Operation::Infer | Operation::Compact) {
            return Err(RuntimeError::from_guest(
                ErrorKind::Unsupported,
                "vendor operation is not supported".into(),
                None,
            ));
        }
        admit_operation(plugin.descriptor(), provider, &provider.channel, operation)?;
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if Instant::now() >= deadline {
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
        let mut store = self.new_store(scope, None);
        let linker = self.new_linker().map_err(|_| RuntimeError::Trapped)?;
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline;
        let instantiate =
            bindings::Vendor::instantiate_async(&mut store, &plugin.inner.component, &linker);
        let instance = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
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
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
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
        if Instant::now() >= deadline {
            return Err(RuntimeError::DeadlineExceeded);
        }
        Ok(protocol)
    }

    pub async fn execute(
        &self,
        plugin: &LoadedPlugin,
        channel: &str,
        input: OperationInput,
        scope: OperationScope,
    ) -> Result<OperationOutput, RuntimeError> {
        let operation = input.operation();
        admit_operation(plugin.descriptor(), input.provider(), channel, operation)?;
        if scope.cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if Instant::now() >= scope.deadline {
            return Err(RuntimeError::DeadlineExceeded);
        }
        if !scope.services.generation_is_current(scope.generation) {
            return Err(RuntimeError::Cancelled);
        }

        let (sdk_operation, sdk_input) = input
            .encode_for_host()
            .map_err(|_| RuntimeError::InvalidOutput)?;
        drop(input);
        let wit_operation = convert_operation(sdk_operation);
        let wit_input = convert_input(sdk_input);
        let mut store = self.new_store(scope, Some(operation));
        let linker = self.new_linker().map_err(|_| RuntimeError::Trapped)?;
        let cancel = store.data().cancellation.clone();
        let deadline = store.data().deadline;
        let instantiate =
            bindings::Vendor::instantiate_async(&mut store, &plugin.inner.component, &linker);
        let instance = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = instantiate => result.map_err(|error| classify_trap(&error))?,
        };

        let call = instance.call_execute(&mut store, wit_operation, channel, &wit_input);
        let raw = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RuntimeError::Cancelled),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(RuntimeError::DeadlineExceeded);
            }
            result = call => result,
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
        OperationOutput::decode_for_host(operation, &bytes).map_err(|_| RuntimeError::InvalidOutput)
    }

    fn validate_imports(&self, component: &Component) -> Result<(), LoadError> {
        for (name, _) in component.component_type().imports(&self.engine) {
            if !matches!(
                name,
                "stravia:vendor/host@0.2.0"
                    | "stravia:vendor/types@0.2.0"
                    // Rust 1.98's wasm32-wasip2 standard library is pinned to
                    // WASIp2 0.2.9. These exact interfaces provide closed
                    // stdio, empty environment, clocks, and entropy through
                    // the restricted WasiCtx below. Filesystem and socket
                    // packages are intentionally absent.
                    | "wasi:io/poll@0.2.9"
                    | "wasi:io/error@0.2.9"
                    | "wasi:io/streams@0.2.9"
                    | "wasi:clocks/monotonic-clock@0.2.9"
                    | "wasi:clocks/wall-clock@0.2.9"
                    | "wasi:random/random@0.2.9"
                    | "wasi:random/insecure@0.2.9"
                    | "wasi:random/insecure-seed@0.2.9"
                    | "wasi:cli/stdin@0.2.9"
                    | "wasi:cli/stdout@0.2.9"
                    | "wasi:cli/stderr@0.2.9"
                    | "wasi:cli/environment@0.2.9"
                    | "wasi:cli/exit@0.2.9"
                    | "wasi:cli/terminal-input@0.2.9"
                    | "wasi:cli/terminal-output@0.2.9"
                    | "wasi:cli/terminal-stdin@0.2.9"
                    | "wasi:cli/terminal-stdout@0.2.9"
                    | "wasi:cli/terminal-stderr@0.2.9"
            ) {
                return Err(LoadError::ForbiddenImport(name.to_owned()));
            }
        }
        Ok(())
    }

    async fn read_descriptor(&self, component: &Component) -> Result<VendorDescriptor, LoadError> {
        let scope = OperationScope {
            services: Arc::new(DenyServices),
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(2),
            generation: 0,
        };
        let mut store = self.new_store(scope, None);
        let linker = self
            .new_linker()
            .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        let deadline = store.data().deadline;
        let instantiate = bindings::Vendor::instantiate_async(&mut store, component, &linker);
        let instance = tokio::select! {
            result = instantiate => result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(LoadError::DescriptorExecution("descriptor deadline elapsed".into()));
            }
        }
        .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        let descriptor_call = instance.call_descriptor(&mut store);
        let descriptor = tokio::select! {
            result = descriptor_call => result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(LoadError::DescriptorExecution("descriptor deadline elapsed".into()));
            }
        }
        .map_err(|error| LoadError::DescriptorExecution(error.to_string()))?;
        serde_json::from_str(&descriptor).map_err(LoadError::DescriptorJson)
    }

    fn new_linker(&self) -> wasmtime::Result<Linker<StoreState>> {
        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wit_host::add_to_linker::<StoreState, HasSelf<StoreState>>(&mut linker, |state| state)?;
        Ok(linker)
    }

    fn new_store(&self, scope: OperationScope, operation: Option<Operation>) -> Store<StoreState> {
        let mut wasi = WasiCtx::builder();
        wasi.max_random_size(64 * 1024);
        let mut store = Store::new(
            &self.engine,
            StoreState {
                wasi: wasi.build(),
                table: ResourceTable::new(),
                services: scope.services,
                cancellation: scope.cancellation,
                deadline: scope.deadline,
                generation: scope.generation,
                operation,
                upstream_starts: 0,
            },
        );
        // Fuel stays enabled only so the async yield interval can preempt
        // CPU-bound guest work for cancellation and deadlines; the budget
        // itself is unlimited.
        store
            .set_fuel(u64::MAX)
            .expect("fuel consumption is enabled for vendor runtime");
        store
            .fuel_async_yield_interval(Some(10_000))
            .expect("async support and fuel consumption are enabled for vendor runtime");
        store
    }
}

struct StoreState {
    wasi: WasiCtx,
    table: ResourceTable,
    services: Arc<dyn HostServices>,
    cancellation: CancellationToken,
    deadline: Instant,
    generation: u64,
    operation: Option<Operation>,
    upstream_starts: usize,
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
        if Instant::now() >= self.deadline {
            return Err(HostFailure::new(
                ErrorKind::DeadlineExceeded,
                "operation deadline elapsed",
            ));
        }
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
}

impl wit_host::Host for StoreState {
    async fn http_start(
        &mut self,
        request: wit_types::HttpRequest,
    ) -> Result<Result<Resource<HttpResponseResource>, wit_types::PluginError>, wasmtime::Error>
    {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        let request = HttpRequest {
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: request.body,
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

    async fn ws_connect(
        &mut self,
        request: wit_types::WsRequest,
    ) -> Result<Result<Resource<WebSocketResource>, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        match self
            .services
            .ws_connect(request.url, request.headers, request.protocols)
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
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        let response = self.table.get(&response)?.0.clone();
        Ok(response.status().await.map_err(to_wit_failure))
    }

    async fn headers(
        &mut self,
        response: Resource<HttpResponseResource>,
    ) -> Result<Result<Vec<(String, String)>, wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
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
        if let Err(error) = self.operation_active() {
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
        response_continuation: bool,
    ) -> Result<Result<(), wit_types::PluginError>, wasmtime::Error> {
        if let Err(error) = self.operation_active() {
            return Ok(Err(to_wit_failure(error)));
        }
        if response_continuation && self.operation != Some(Operation::Infer) {
            return Ok(Err(to_wit_failure(HostFailure::new(
                ErrorKind::Trapped,
                "response continuation can only be declared by inference",
            ))));
        }
        let socket = self.table.get(&socket)?.0.clone();
        Ok(socket
            .close(response_continuation)
            .await
            .map_err(to_wit_failure))
    }

    async fn drop(&mut self, socket: Resource<WebSocketResource>) -> wasmtime::Result<()> {
        self.table.delete(socket)?;
        Ok(())
    }
}

fn admit_operation(
    descriptor: &VendorDescriptor,
    provider: &ProviderSnapshot,
    channel: &str,
    operation: Operation,
) -> Result<(), RuntimeError> {
    if provider.channel != channel {
        return Err(RuntimeError::InvalidOutput);
    }
    let Some(profile) = descriptor.provider(&provider.provider_id) else {
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
        wit_types::ErrorKind::Auth => ErrorKind::Auth,
        wit_types::ErrorKind::ContinuationNotFound => ErrorKind::ContinuationNotFound,
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
        ErrorKind::Auth => wit_types::ErrorKind::Auth,
        ErrorKind::ContinuationNotFound => wit_types::ErrorKind::ContinuationNotFound,
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
        _url: String,
        _headers: Vec<(String, String)>,
        _protocols: Vec<String>,
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
            display_name: provider_id.into(),
            description: None,
            channels: vec![ChannelDescriptor {
                id: "default".into(),
                name: "Default".into(),
                description: None,
                auth: None,
                protocol: Some("test".into()),
                default_base_url: None,
                default_models_source: None,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            }],
            capabilities,
            config_fields: Vec::new(),
            network: NetworkDeclaration::default(),
            data_compat: DataCompatibility::default(),
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
        let error = admit_operation(
            &descriptor(),
            &snapshot("alpha"),
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
        let error = admit_operation(&descriptor(), &snapshot("alpha"), "other", Operation::Infer)
            .expect_err("execute channel must match the signed snapshot");
        assert!(matches!(error, RuntimeError::InvalidOutput));
    }
}
