use async_trait::async_trait;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent,
};
use stravia_vendor_sdk::{
    AllowanceRequest, AuthRequest, ConfigValidationRequest, DiscoverRequest, ErrorKind,
    MediaImageRequest, ModelMetadata, Operation, OperationInput, OperationOutput, ProviderSnapshot,
    SearchRequest,
};
use tokio::sync::{Mutex, mpsc};

use crate::Gateway;
use crate::db::models::{OAuthCredential, Provider};
use crate::interaction_observation::ProtectedSecrets;
use crate::provider_models::ProviderModelRecord;

use super::lifecycle::{VendorOperation, VendorPublicationFence};
use super::network::VendorNetwork;
use super::permissions::resolve_permissions;
use super::store::{PluginStorageError, PluginStore};

#[derive(Debug, Clone)]
pub(crate) enum VendorRequest {
    Infer(AiRequest),
    Compact(AiRequest),
    Search(SearchRequest),
    MediaImage(MediaImageRequest),
    Auth(AuthRequest),
    Discover(DiscoverRequest),
    Allowance(AllowanceRequest),
    ConfigValidation(ConfigValidationRequest),
}

impl VendorRequest {
    pub(crate) fn into_input(self, provider: ProviderSnapshot) -> OperationInput {
        match self {
            Self::Infer(request) => OperationInput::Infer { provider, request },
            Self::Compact(request) => OperationInput::Compact { provider, request },
            Self::Search(request) => OperationInput::Search { provider, request },
            Self::MediaImage(request) => OperationInput::MediaImage { provider, request },
            Self::Auth(request) => OperationInput::Auth { provider, request },
            Self::Discover(request) => OperationInput::Discover { provider, request },
            Self::Allowance(request) => OperationInput::Allowance { provider, request },
            Self::ConfigValidation(request) => {
                OperationInput::ConfigValidation { provider, request }
            }
        }
    }

    fn operation(&self) -> Operation {
        match self {
            Self::Infer(_) => Operation::Infer,
            Self::Compact(_) => Operation::Compact,
            Self::Search(_) => Operation::Search,
            Self::MediaImage(_) => Operation::MediaImage,
            Self::Auth(_) => Operation::Auth,
            Self::Discover(_) => Operation::Discover,
            Self::Allowance(_) => Operation::Allowance,
            Self::ConfigValidation(_) => Operation::ConfigValidation,
        }
    }
}

pub(crate) struct VendorEvent {
    pub(crate) event: RuntimeEvent,
    pub(crate) publication: VendorPublicationFence,
}

pub(crate) struct VendorExecution {
    pub(crate) output: OperationOutput,
    pub(crate) publication: VendorPublicationFence,
    /// Exact protocol hint pinned from the acquired component snapshot.
    pub(crate) protocol: String,
}

pub(crate) struct VendorCallContext {
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Deadline,
    pub(crate) events: Option<mpsc::Sender<VendorEvent>>,
    pub(crate) observer: Option<crate::interaction_observation::RunObserver>,
    pub(crate) model_turn_id: Option<String>,
    pub(crate) attempt_id: Option<String>,
    pub(crate) websocket_affinity: Option<String>,
    pub(crate) response_continuation_available: Arc<AtomicBool>,
    pub(crate) client_headers: Vec<(String, String)>,
    pub(crate) metadata: BTreeMap<String, Value>,
}

impl VendorCallContext {
    pub(crate) fn new(cancellation: CancellationToken, deadline: Deadline) -> Self {
        Self {
            cancellation,
            deadline,
            events: None,
            observer: None,
            model_turn_id: None,
            attempt_id: None,
            websocket_affinity: None,
            response_continuation_available: Arc::new(AtomicBool::new(false)),
            client_headers: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct VendorSessionScope {
    pub(crate) vendor_id: String,
    pub(crate) plugin: LoadedPlugin,
    pub(crate) data_epoch: i64,
    pub(crate) provider: ProviderSnapshot,
    pub(crate) private_state: Arc<Mutex<Option<Vec<u8>>>>,
    origins: BTreeSet<String>,
}

#[derive(Clone)]
pub(crate) struct PreparedVendorExecution {
    plugin: LoadedPlugin,
    operation: Arc<VendorOperation>,
    data_epoch: i64,
    vendor_id: String,
    provider_id: String,
    provider: ProviderSnapshot,
    oauth_connection_id: Option<String>,
    use_proxy: bool,
    origins: BTreeSet<String>,
    kind: Operation,
}

impl PreparedVendorExecution {
    pub(crate) fn protocol(&self) -> &str {
        &self.provider.protocol
    }

    pub(crate) fn vendor_id(&self) -> &str {
        &self.vendor_id
    }

    pub(crate) fn descriptor(&self) -> &stravia_vendor_sdk::ProviderDescriptor {
        self.plugin
            .descriptor()
            .provider(&self.provider.provider_id)
            .expect("prepared execution retains its admitted provider profile")
    }

    pub(crate) fn provider(&self) -> &ProviderSnapshot {
        &self.provider
    }

    pub(crate) fn oauth_connection_id(&self) -> Option<&str> {
        self.oauth_connection_id.as_deref()
    }

    pub(crate) fn use_proxy(&self) -> bool {
        self.use_proxy
    }

    pub(crate) async fn cancelled(&self) {
        self.operation.cancellation().cancelled().await;
    }

    pub(crate) async fn write_fence(
        &self,
    ) -> anyhow::Result<tokio::sync::OwnedRwLockReadGuard<()>> {
        self.operation.write_fence().await
    }
}

struct ConnectionSnapshot {
    provider: Provider,
    oauth: Option<OAuthCredential>,
    model: Option<ProviderModelRecord>,
}

impl Gateway {
    /// Captures the currently loaded component and data epoch for an
    /// unpersisted connection candidate without retaining an operation lease.
    /// Execution reacquires admission and validates this epoch before use.
    pub(crate) fn create_vendor_session_scope(
        &self,
        vendor_id: &str,
        mut provider: ProviderSnapshot,
    ) -> anyhow::Result<VendorSessionScope> {
        let (plugin, operation, data_epoch) = self.vendor_plugins.acquire(vendor_id)?;
        anyhow::ensure!(
            provider.provider_id == vendor_id,
            "vendor session provider identity does not match"
        );
        let descriptor = plugin
            .descriptor()
            .provider(vendor_id)
            .ok_or_else(|| anyhow::anyhow!("loaded vendor plugin does not support the provider"))?;
        provider.protocol = pinned_protocol(descriptor, &provider.channel, &provider.protocol)?;
        let origins = resolve_permissions(
            descriptor,
            Some(&provider.base_url),
            None,
            None,
            &provider.options,
            &provider.credentials,
        )?
        .into_iter()
        .map(|grant| grant.origin)
        .collect();
        drop(operation);
        Ok(VendorSessionScope {
            vendor_id: vendor_id.to_owned(),
            plugin,
            data_epoch,
            provider,
            private_state: Arc::new(Mutex::new(None)),
            origins,
        })
    }

    pub(crate) async fn execute_vendor(
        &self,
        provider_id: &str,
        model: Option<&str>,
        request: VendorRequest,
        context: VendorCallContext,
    ) -> anyhow::Result<VendorExecution> {
        let mut prepared = self
            .prepare_vendor_execution(provider_id, model, request.operation(), &context)
            .await?;
        if let VendorRequest::Infer(request) | VendorRequest::Compact(request) = &request {
            self.select_vendor_protocol(&mut prepared, request, &context)
                .await?;
        }
        self.execute_prepared_vendor(prepared, request, context)
            .await
    }

    /// 在任何回放转换前固定实际协议；选择与执行持有同一个组件及操作租约。
    pub(crate) async fn select_vendor_protocol(
        &self,
        prepared: &mut PreparedVendorExecution,
        request: &AiRequest,
        context: &VendorCallContext,
    ) -> anyhow::Result<()> {
        prepared.operation.ensure_current()?;
        if let Some(observer) = &context.observer {
            register_snapshot_secrets(&observer.protected_secrets(), &prepared.provider);
        }
        let protocol = {
            let selection = self.vendor_plugins.runtime.select_protocol(
                &prepared.plugin,
                prepared.kind,
                &prepared.provider,
                request,
                context.cancellation.clone(),
                context.deadline.clone(),
            );
            tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return Err(stravia_vendor_runtime::RuntimeError::Cancelled.into());
                }
                _ = prepared.operation.cancellation().cancelled() => {
                    return Err(stravia_vendor_runtime::RuntimeError::Cancelled.into());
                }
                result = selection => result?,
            }
        };
        prepared.operation.ensure_current()?;
        prepared
            .provider
            .operation_metadata
            .insert("egress_protocol".into(), Value::String(protocol.clone()));
        prepared.provider.protocol = protocol;
        Ok(())
    }

    pub(crate) async fn prepare_vendor_execution(
        &self,
        provider_id: &str,
        model: Option<&str>,
        kind: Operation,
        context: &VendorCallContext,
    ) -> anyhow::Result<PreparedVendorExecution> {
        // This first read establishes only the vendor admission key. Its secret
        // fields are deliberately dropped before acquiring the version lease.
        let vendor_id = {
            let provider = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => return Err(RuntimeError::Cancelled.into()),
                () = context.deadline.wait() => return Err(RuntimeError::DeadlineExceeded.into()),
                result = self.storage.providers().get(provider_id) => result?,
            }
            .ok_or_else(|| anyhow::anyhow!("provider was not found"))?;
            provider
                .vendor
                .as_deref()
                .map(str::trim)
                .filter(|vendor| !vendor.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("provider is not backed by a vendor plugin"))?
        };

        let (plugin, operation, data_epoch) = self.vendor_plugins.acquire(&vendor_id)?;
        self.prepare_vendor_execution_with_admission(
            &vendor_id,
            plugin,
            operation,
            data_epoch,
            provider_id,
            model,
            kind,
            context,
        )
        .await
    }

    /// Freezes another Provider connection and operation kind under the same
    /// admitted Vendor component, data epoch, and cancellation boundary.
    pub(crate) async fn prepare_vendor_execution_with_lease(
        &self,
        lease: &PreparedVendorExecution,
        provider_id: &str,
        model: Option<&str>,
        kind: Operation,
        context: &VendorCallContext,
    ) -> anyhow::Result<PreparedVendorExecution> {
        lease.operation.ensure_current()?;
        self.prepare_vendor_execution_with_admission(
            &lease.vendor_id,
            lease.plugin.clone(),
            lease.operation.clone(),
            lease.data_epoch,
            provider_id,
            model,
            kind,
            context,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_vendor_execution_with_admission(
        &self,
        vendor_id: &str,
        plugin: LoadedPlugin,
        operation: Arc<VendorOperation>,
        data_epoch: i64,
        provider_id: &str,
        model: Option<&str>,
        kind: Operation,
        context: &VendorCallContext,
    ) -> anyhow::Result<PreparedVendorExecution> {
        let cancellation = operation.cancellation().clone();
        let prepare = async {
            let connection = self
                .read_connection_after_admission(provider_id, vendor_id, model)
                .await?;
            anyhow::ensure!(connection.provider.is_enabled, "provider is disabled");
            ensure_recovery_complete(
                &self.vendor_plugins.store,
                provider_id,
                kind,
                model.filter(|_| {
                    connection.model.as_ref().is_none_or(|record| {
                        record.source_kind
                            != crate::provider_models::ProviderModelSourceKind::Manual
                    })
                }),
            )
            .await?;

            let descriptor = plugin.descriptor().provider(vendor_id).ok_or_else(|| {
                anyhow::anyhow!("loaded vendor plugin does not support the provider")
            })?;
            let effective_models_source =
                connection.provider.channel.as_deref().and_then(|channel| {
                    effective_discovery_source(
                        descriptor,
                        channel,
                        connection.provider.models_source.as_deref(),
                    )
                });
            let catalog_models = if kind == Operation::Discover
                && effective_models_source == Some("catalog")
                && connection
                    .provider
                    .static_models
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
            {
                let catalog_id = connection
                    .provider
                    .preset_key
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .or(descriptor.catalog_id.as_deref())
                    .unwrap_or(vendor_id);
                match self.provider_catalog.provider_scope(catalog_id).await {
                    Ok(scope) => Some(
                        scope
                            .models
                            .into_iter()
                            .map(|model| model.metadata)
                            .collect::<Vec<_>>(),
                    ),
                    Err(error)
                        if error
                            .downcast_ref::<crate::provider_catalog::CatalogError>()
                            .is_some_and(|error| {
                                matches!(
                                    error,
                                    crate::provider_catalog::CatalogError::ProviderNotFound { .. }
                                )
                            }) =>
                    {
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let oauth_connection_id = if connection.provider.auth_mode.trim() == "oauth" {
                connection
                    .oauth
                    .as_ref()
                    .map(|credential| credential.connection_id.clone())
            } else {
                None
            };
            let (mut provider, use_proxy, origins) =
                provider_snapshot(descriptor, connection, model, kind, context)?;
            if let Some(models) = catalog_models {
                provider
                    .operation_metadata
                    .insert("catalog_models".into(), Value::Array(models));
            }
            operation.ensure_current()?;
            Ok(PreparedVendorExecution {
                plugin,
                operation,
                data_epoch,
                vendor_id: vendor_id.to_owned(),
                provider_id: provider_id.to_owned(),
                provider,
                oauth_connection_id,
                use_proxy,
                origins,
                kind,
            })
        };
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => Err(RuntimeError::Cancelled.into()),
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled.into()),
            () = context.deadline.wait() => Err(RuntimeError::DeadlineExceeded.into()),
            result = prepare => result,
        }
    }

    pub(crate) async fn execute_prepared_vendor(
        &self,
        mut prepared: PreparedVendorExecution,
        request: VendorRequest,
        context: VendorCallContext,
    ) -> anyhow::Result<VendorExecution> {
        anyhow::ensure!(
            request.operation() == prepared.kind,
            "prepared vendor operation kind changed"
        );
        // 请求编排可在拿到实际协议后补充提示；连接及权限事实只来自冻结快照。
        for (key, value) in &context.metadata {
            if !matches!(
                key.as_str(),
                "host_platform"
                    | "egress_protocol"
                    | "egress_base_url"
                    | "models_source"
                    | "static_models"
                    | "catalog_models"
            ) {
                prepared
                    .provider
                    .operation_metadata
                    .insert(key.clone(), value.clone());
            }
        }
        if let Some(affinity) = &context.websocket_affinity {
            prepared
                .provider
                .operation_metadata
                .insert("transport_affinity".into(), Value::String(affinity.clone()));
        }
        self.execute_vendor_input(
            prepared.plugin,
            prepared.operation,
            prepared.data_epoch,
            Some(prepared.provider_id),
            None,
            prepared.use_proxy,
            prepared.origins,
            request.into_input(prepared.provider),
            context,
        )
        .await
    }

    /// Executes the two operations that may legitimately target an
    /// unpersisted connection snapshot. It deliberately does not create a
    /// general bypass around Provider-scoped execution.
    pub(crate) async fn execute_vendor_session(
        &self,
        session: &VendorSessionScope,
        request: VendorRequest,
        context: VendorCallContext,
    ) -> anyhow::Result<VendorExecution> {
        let operation_kind = request.operation();
        anyhow::ensure!(
            matches!(
                operation_kind,
                Operation::Auth | Operation::ConfigValidation
            ),
            "unpersisted vendor sessions only support authentication and configuration validation"
        );
        // Admission is intentionally taken from the current manager while an
        // auth component remains pinned to the session's original version.
        let (current_plugin, operation, current_epoch) =
            self.vendor_plugins.acquire(&session.vendor_id)?;
        anyhow::ensure!(
            current_epoch == session.data_epoch,
            "vendor session data is incompatible with the installed plugin"
        );
        anyhow::ensure!(
            session.provider.provider_id == session.vendor_id,
            "vendor session plugin identity does not match"
        );
        let descriptor = session
            .plugin
            .descriptor()
            .provider(&session.vendor_id)
            .ok_or_else(|| anyhow::anyhow!("vendor session profile is unavailable"))?;
        if operation_kind == Operation::ConfigValidation {
            anyhow::ensure!(
                current_plugin.identity() == session.plugin.identity(),
                "vendor plugin changed while preparing configuration validation"
            );
        }

        let mut provider = session.provider.clone();
        provider.protocol = pinned_protocol(descriptor, &provider.channel, &provider.protocol)?;
        provider.client_headers = filtered_client_headers(&context.client_headers);
        provider.operation_metadata.extend(context.metadata.clone());
        provider.operation_metadata.insert(
            "egress_protocol".into(),
            Value::String(provider.protocol.clone()),
        );
        apply_host_metadata(&mut provider.operation_metadata);
        let use_proxy = provider
            .operation_metadata
            .get("use_proxy")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        self.execute_vendor_input(
            session.plugin.clone(),
            operation,
            current_epoch,
            None,
            Some(session.private_state.clone()),
            use_proxy,
            session.origins.clone(),
            request.into_input(provider),
            context,
        )
        .await
    }

    async fn read_connection_after_admission(
        &self,
        provider_id: &str,
        vendor_id: &str,
        model: Option<&str>,
    ) -> anyhow::Result<ConnectionSnapshot> {
        let provider = self
            .storage
            .providers()
            .get(provider_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider was removed during vendor admission"))?;
        anyhow::ensure!(
            provider.vendor.as_deref() == Some(vendor_id),
            "provider vendor changed during vendor admission"
        );
        let oauth = self.storage.oauth_credentials().get(provider_id).await?;
        let model_record = match model {
            Some(model) => {
                self.storage
                    .provider_models()
                    .find(provider_id, model)
                    .await?
            }
            None => None,
        };

        // Storage backends do not expose a cross-store transaction through the
        // aggregate interface. Re-reading all participating generations keeps
        // one call from combining a changed Provider/OAuth/model connection.
        let current_provider = self
            .storage
            .providers()
            .get(provider_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider was removed during connection snapshot"))?;
        let current_oauth = self.storage.oauth_credentials().get(provider_id).await?;
        let current_model = match model {
            Some(model) => {
                self.storage
                    .provider_models()
                    .find(provider_id, model)
                    .await?
            }
            None => None,
        };
        anyhow::ensure!(
            same_provider_generation(&provider, &current_provider)
                && oauth == current_oauth
                && model_record == current_model,
            "provider connection changed while preparing the vendor operation"
        );
        anyhow::ensure!(
            current_provider.vendor.as_deref() == Some(vendor_id),
            "provider vendor changed while preparing the vendor operation"
        );
        if current_provider.auth_mode.trim() == "oauth" {
            anyhow::ensure!(
                current_oauth.is_some(),
                "provider OAuth credential is missing"
            );
        }
        Ok(ConnectionSnapshot {
            provider: current_provider,
            oauth: current_oauth,
            model: current_model,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_vendor_input(
        &self,
        plugin: LoadedPlugin,
        operation: Arc<VendorOperation>,
        data_epoch: i64,
        provider_id: Option<String>,
        session_state: Option<Arc<Mutex<Option<Vec<u8>>>>>,
        use_proxy: bool,
        origins: BTreeSet<String>,
        input: OperationInput,
        context: VendorCallContext,
    ) -> anyhow::Result<VendorExecution> {
        if context.deadline.is_exceeded() {
            return Err(RuntimeError::DeadlineExceeded.into());
        }
        if context.cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled.into());
        }
        operation.ensure_current()?;

        let descriptor = plugin
            .descriptor()
            .provider(&input.provider().provider_id)
            .ok_or_else(|| anyhow::anyhow!("vendor operation profile is unavailable"))?;
        let clients = self.vendor_client_snapshot(use_proxy).await?;
        if context.cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled.into());
        }
        if context.deadline.is_exceeded() {
            return Err(RuntimeError::DeadlineExceeded.into());
        }
        operation.ensure_current()?;
        let websocket_scope = websocket_scope_key(
            provider_id.as_deref(),
            plugin.identity(),
            &input.provider().credentials,
            &clients.websocket_reuse_identity,
        )?;
        let websocket_affinity = provider_id
            .as_ref()
            .and_then(|_| context.websocket_affinity.clone());
        let network = VendorNetwork::new(
            clients.http,
            clients.websocket,
            origins,
            operation.clone(),
            context.cancellation.clone(),
            input.provider().protocol.clone(),
        )
        .with_observer(context.observer.clone())
        .with_observation_scope(context.model_turn_id.clone(), context.attempt_id.clone())
        .with_response_continuation_available(context.response_continuation_available.clone())
        .with_websocket_pool(
            self.vendor_websocket_pool.clone(),
            websocket_scope,
            websocket_affinity,
        );
        let secrets = context
            .observer
            .as_ref()
            .map_or_else(ProtectedSecrets::default, |observer| {
                observer.protected_secrets()
            });
        register_snapshot_secrets(&secrets, input.provider());
        let state = match (provider_id, session_state) {
            (Some(provider_id), None) => StateScope::Provider {
                store: self.vendor_plugins.store.clone(),
                vendor_id: descriptor.provider_id.clone(),
                provider_id,
                data_epoch,
                format_version: descriptor.data_compat.private_state_format.to_string(),
            },
            (None, Some(state)) => StateScope::Session(state),
            _ => anyhow::bail!("vendor operation has an invalid private-state scope"),
        };
        let publication =
            operation.publication_fence(context.cancellation.clone(), context.deadline.clone());
        let services = Arc::new(ScopedHostServices {
            network,
            operation: operation.clone(),
            cancellation: context.cancellation.clone(),
            events: context.events,
            publication: publication.clone(),
            state,
            secrets,
            event_delivery_failed: AtomicBool::new(false),
        });
        let scope = OperationScope::new(
            services.clone(),
            context.cancellation.clone(),
            context.deadline.clone(),
            0,
        );
        let channel = input.provider().channel.clone();
        let protocol = input.provider().protocol.clone();
        let execution = self
            .vendor_plugins
            .runtime
            .execute(&plugin, &channel, input, scope);
        let output = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => {
                return Err(RuntimeError::Cancelled.into());
            }
            _ = operation.cancellation().cancelled() => {
                return Err(RuntimeError::Cancelled.into());
            }
            () = context.deadline.wait() => {
                return Err(RuntimeError::DeadlineExceeded.into());
            }
            result = execution => result.map_err(anyhow::Error::from)?,
        };
        anyhow::ensure!(
            !services.event_delivery_failed.load(Ordering::Acquire),
            "vendor model event could not be delivered"
        );

        // 返回值之后仍可能等待凭据持久化、报告校验或 Artifact 收存。
        // 提交许可随结果传递，不把消费方的背压变成不可取消的活跃任务。
        publication.ensure_current()?;
        Ok(VendorExecution {
            output,
            publication,
            protocol,
        })
    }
}

fn effective_discovery_source<'a>(
    descriptor: &'a stravia_vendor_sdk::ProviderDescriptor,
    channel: &str,
    saved: Option<&'a str>,
) -> Option<&'a str> {
    let declared = descriptor
        .channels
        .iter()
        .find(|candidate| candidate.id == channel)
        .and_then(|channel| channel.default_models_source)
        .map(|source| source.as_str());
    match saved.map(str::trim).filter(|source| !source.is_empty()) {
        Some(source) => Some(source),
        None => declared,
    }
}

fn provider_snapshot(
    descriptor: &stravia_vendor_sdk::ProviderDescriptor,
    connection: ConnectionSnapshot,
    model: Option<&str>,
    kind: Operation,
    context: &VendorCallContext,
) -> anyhow::Result<(ProviderSnapshot, bool, BTreeSet<String>)> {
    let use_proxy = connection.provider.use_proxy;
    let options = json_object(&connection.provider.vendor_options, "provider options")?;
    let saved_credentials = connection_credentials(&connection.provider)?;
    let channel = connection
        .provider
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|channel| !channel.is_empty())
        .ok_or_else(|| anyhow::anyhow!("provider channel is missing"))?
        .to_owned();
    let protocol = pinned_protocol(descriptor, &channel, &connection.provider.protocol)?;
    let mut base_url = connection.provider.base_url.clone();
    if let Some(field) = descriptor.network.base_url_field.as_deref() {
        base_url = configured_string(field, &options, &saved_credentials)
            .ok_or_else(|| anyhow::anyhow!("configured network address is missing"))?
            .to_owned();
    }
    anyhow::ensure!(!base_url.trim().is_empty(), "provider base URL is missing");
    let discovery_source = (kind == Operation::Discover)
        .then(|| {
            effective_discovery_source(
                descriptor,
                &channel,
                connection.provider.models_source.as_deref(),
            )
        })
        .flatten();
    // OAuth/Guest 返回值不能成为新的目的地址授权来源。
    let origins = resolve_permissions(
        descriptor,
        Some(&base_url),
        discovery_source,
        connection.provider.static_models.as_deref(),
        &options,
        &saved_credentials,
    )?
    .into_iter()
    .map(|grant| grant.origin)
    .collect();
    let mut credentials = saved_credentials;
    if let Some(oauth) = connection.oauth.as_ref() {
        extend_oauth_credentials(&mut credentials, oauth)?;
    }

    let mut operation_metadata = context.metadata.clone();
    for key in ["models_source", "static_models", "catalog_models"] {
        operation_metadata.remove(key);
    }
    if kind == Operation::Discover {
        if let Some(source) = discovery_source {
            operation_metadata.insert("models_source".into(), Value::String(source.to_owned()));
        }
        if let Some(models) = connection
            .provider
            .static_models
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            operation_metadata.insert(
                "static_models".into(),
                Value::Array(
                    models
                        .lines()
                        .flat_map(|line| line.split(','))
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(|value| Value::String(value.to_owned()))
                        .collect(),
                ),
            );
        }
    }
    operation_metadata.insert("egress_protocol".into(), Value::String(protocol.clone()));
    operation_metadata.insert("egress_base_url".into(), Value::String(base_url.clone()));
    apply_host_metadata(&mut operation_metadata);
    Ok((
        ProviderSnapshot {
            provider_id: descriptor.provider_id.clone(),
            channel,
            base_url,
            protocol,
            options,
            credentials,
            model: model.map(str::to_owned),
            model_metadata: connection.model.as_ref().map(model_metadata).transpose()?,
            client_headers: filtered_client_headers(&context.client_headers),
            operation_metadata,
        },
        use_proxy,
        origins,
    ))
}

fn pinned_protocol(
    descriptor: &stravia_vendor_sdk::ProviderDescriptor,
    channel: &str,
    saved_hint: &str,
) -> anyhow::Result<String> {
    let channel = descriptor
        .channels
        .iter()
        .find(|candidate| candidate.id == channel)
        .ok_or_else(|| anyhow::anyhow!("vendor channel is not available in the acquired plugin"))?;
    // 管理员已选的 wire hint 是连接配置，不能被一次插件更新的默认值覆盖。
    // 缺省值取自本次实际组件；不解析或猜测第三方协议。
    Ok(if saved_hint.trim().is_empty() {
        channel.protocol.clone().unwrap_or_default()
    } else {
        saved_hint.to_owned()
    })
}

fn connection_credentials(provider: &Provider) -> anyhow::Result<BTreeMap<String, Value>> {
    let mut credentials = json_object(&provider.adapter_credentials, "adapter credentials")?;
    if provider.auth_mode != "oauth" && !provider.api_key.trim().is_empty() {
        credentials
            .entry("apiKey".to_owned())
            .or_insert_with(|| Value::String(provider.api_key.trim().to_owned()));
    }
    Ok(credentials)
}

fn extend_oauth_credentials(
    credentials: &mut BTreeMap<String, Value>,
    oauth: &OAuthCredential,
) -> anyhow::Result<()> {
    let meta = serde_json::from_str::<Value>(&oauth.meta)
        .map_err(|_| anyhow::anyhow!("stored OAuth metadata is invalid"))?;
    let raw = meta.get("raw").cloned().unwrap_or_else(|| meta.clone());
    if let Value::Object(values) = &meta {
        for (key, value) in values {
            credentials
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    credentials.insert("raw".into(), raw);
    credentials.insert(
        "access_token".into(),
        Value::String(oauth.access_token.clone()),
    );
    insert_optional(credentials, "refresh_token", oauth.refresh_token.as_deref());
    insert_optional(credentials, "expires_at", oauth.expires_at.as_deref());
    insert_optional(credentials, "resource_url", oauth.resource_url.as_deref());
    insert_optional(credentials, "subject_id", oauth.subject_id.as_deref());
    credentials.insert("driver_key".into(), Value::String(oauth.driver_key.clone()));
    credentials.insert("scheme".into(), Value::String(oauth.scheme.clone()));
    let scopes = serde_json::from_str::<Value>(&oauth.scopes).unwrap_or_else(|_| {
        Value::Array(
            oauth
                .scopes
                .split_whitespace()
                .map(|scope| Value::String(scope.to_owned()))
                .collect(),
        )
    });
    credentials.insert("scopes".into(), scopes);
    Ok(())
}

fn insert_optional(map: &mut BTreeMap<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        map.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn json_object(value: &str, label: &str) -> anyhow::Result<BTreeMap<String, Value>> {
    let value: Value =
        serde_json::from_str(value).map_err(|_| anyhow::anyhow!("{label} are not valid JSON"))?;
    let Value::Object(values) = value else {
        anyhow::bail!("{label} must be a JSON object");
    };
    Ok(values.into_iter().collect())
}

fn model_metadata(record: &ProviderModelRecord) -> anyhow::Result<ModelMetadata> {
    let value = serde_json::to_value(&record.metadata)?;
    let Value::Object(values) = value else {
        anyhow::bail!("stored Provider Model metadata is invalid");
    };
    let extensions: BTreeMap<String, Value> = values.into_iter().collect();
    let selector = extensions
        .get("selector")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut capabilities = extensions
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let image_input = if record
        .metadata
        .modalities
        .as_ref()
        .is_some_and(|modalities| stravia_media::platform::supports_image(&modalities.input))
    {
        Some(true)
    } else {
        record.metadata.attachment
    };
    for (name, supported) in [
        ("image_input", image_input),
        ("reasoning", record.metadata.reasoning),
        ("tools", record.metadata.tool_call),
        ("structured_output", record.metadata.structured_output),
    ] {
        match supported {
            Some(true) => {
                capabilities.insert(name.to_owned());
            }
            Some(false) => {
                capabilities.remove(name);
            }
            None => {}
        }
    }
    Ok(ModelMetadata {
        id: Some(record.model_id.clone()),
        family: record.metadata.family.clone(),
        selector,
        capabilities: capabilities.into_iter().collect(),
        extensions,
    })
}

fn configured_string<'a>(
    field: &str,
    options: &'a BTreeMap<String, Value>,
    credentials: &'a BTreeMap<String, Value>,
) -> Option<&'a str> {
    options
        .get(field)
        .and_then(Value::as_str)
        .or_else(|| credentials.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn websocket_scope_key(
    provider_id: Option<&str>,
    plugin_identity: &str,
    credentials: &BTreeMap<String, Value>,
    proxy_reuse_identity: &str,
) -> anyhow::Result<String> {
    let encoded = serde_json::to_vec(credentials)?;
    let credential_hash = stravia_runtime_contract::protocol::ir::canonical::hash_hex(
        &stravia_runtime_contract::protocol::ir::canonical::hash_bytes(&encoded),
    );
    match provider_id {
        Some(provider_id) => Ok(serde_json::to_string(&(
            "provider",
            provider_id,
            plugin_identity,
            credential_hash,
            proxy_reuse_identity,
        ))?),
        None => Ok(serde_json::to_string(&(
            "session",
            plugin_identity,
            credential_hash,
            proxy_reuse_identity,
        ))?),
    }
}

fn filtered_client_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.to_ascii_lowercase();
            (matches!(
                name.as_str(),
                "accept"
                    | "accept-language"
                    | "idempotency-key"
                    | "openai-beta"
                    | "session-id"
                    | "session_id"
                    | "conversation_id"
                    | "thread-id"
                    | "x-client-request-id"
                    | "x-codex-beta-features"
                    | "x-codex-installation-id"
                    | "x-codex-turn-metadata"
                    | "x-codex-turn-state"
                    | "x-codex-window-id"
            ) && !value.contains(['\r', '\n']))
            .then(|| (name, value.clone()))
        })
        .collect()
}

fn apply_host_metadata(metadata: &mut BTreeMap<String, Value>) {
    metadata.insert(
        "host_platform".into(),
        Value::String(std::env::consts::OS.into()),
    );
}

async fn ensure_recovery_complete(
    store: &PluginStore,
    provider_id: &str,
    operation: Operation,
    model: Option<&str>,
) -> anyhow::Result<()> {
    let missing = store.recovery(provider_id).await?;
    if missing.iter().any(|kind| kind == "options") && operation != Operation::ConfigValidation {
        anyhow::bail!("provider plugin options must be reconfigured");
    }
    if missing.iter().any(|kind| kind == "credentials")
        && !matches!(operation, Operation::Auth | Operation::ConfigValidation)
    {
        anyhow::bail!("provider plugin credentials must be reconfigured");
    }
    if model.is_some()
        && missing.iter().any(|kind| kind == "models")
        && !matches!(
            operation,
            Operation::Discover | Operation::ConfigValidation | Operation::Auth
        )
    {
        anyhow::bail!("provider model metadata must be rediscovered");
    }
    Ok(())
}

fn same_provider_generation(left: &Provider, right: &Provider) -> bool {
    left.id == right.id
        && left.vendor == right.vendor
        && left.protocol == right.protocol
        && left.base_url == right.base_url
        && left.channel == right.channel
        && left.preset_key == right.preset_key
        && left.models_source == right.models_source
        && left.static_models == right.static_models
        && left.api_key == right.api_key
        && left.adapter_credentials == right.adapter_credentials
        && left.vendor_options == right.vendor_options
        && left.auth_mode == right.auth_mode
        && left.use_proxy == right.use_proxy
        && left.is_enabled == right.is_enabled
        && left.updated_at == right.updated_at
}

enum StateScope {
    Provider {
        store: PluginStore,
        vendor_id: String,
        provider_id: String,
        data_epoch: i64,
        format_version: String,
    },
    Session(Arc<Mutex<Option<Vec<u8>>>>),
}

struct ScopedHostServices {
    network: VendorNetwork,
    operation: Arc<VendorOperation>,
    cancellation: CancellationToken,
    events: Option<mpsc::Sender<VendorEvent>>,
    publication: VendorPublicationFence,
    state: StateScope,
    secrets: ProtectedSecrets,
    event_delivery_failed: AtomicBool,
}

impl ScopedHostServices {
    fn ensure_current(&self) -> Result<(), HostFailure> {
        if self.cancellation.is_cancelled() {
            return Err(cancelled_failure());
        }
        self.operation
            .ensure_current()
            .map_err(|_| cancelled_failure())
    }

    fn register_state_secrets(&self, bytes: &[u8]) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            self.secrets.register([text]);
        }
        if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
            register_value_secrets(&self.secrets, &value);
        }
    }
}

#[async_trait]
impl HostServices for ScopedHostServices {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        self.ensure_current()?;
        self.network.http_start(request)
    }

    async fn ws_connect(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        protocols: Vec<String>,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        self.ensure_current()?;
        self.network.ws_connect(url, headers, protocols).await
    }

    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        self.ensure_current()?;
        let result = match &self.state {
            StateScope::Provider {
                store,
                vendor_id,
                provider_id,
                format_version,
                ..
            } => {
                let stored = tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => return Err(cancelled_failure()),
                    _ = self.operation.cancellation().cancelled() => return Err(cancelled_failure()),
                    result = store.read_private_state(vendor_id, provider_id) => result,
                };
                match stored {
                    Ok(Some((stored_format, bytes))) if stored_format == *format_version => {
                        Some(bytes)
                    }
                    Ok(Some(_)) => {
                        return Err(HostFailure::new(
                            ErrorKind::Trapped,
                            "vendor private state has an incompatible format",
                        ));
                    }
                    Ok(None) => None,
                    Err(_) => return Err(storage_failure()),
                }
            }
            StateScope::Session(state) => {
                let guard = tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => return Err(cancelled_failure()),
                    _ = self.operation.cancellation().cancelled() => return Err(cancelled_failure()),
                    guard = state.lock() => guard,
                };
                guard.clone()
            }
        };
        self.ensure_current()?;
        if let Some(bytes) = &result {
            self.register_state_secrets(bytes);
        }
        Ok(result)
    }

    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        self.ensure_current()?;
        let fence = self
            .operation
            .write_fence()
            .await
            .map_err(|_| cancelled_failure())?;
        self.ensure_current()?;
        self.register_state_secrets(&bytes);
        match &self.state {
            StateScope::Provider {
                store,
                vendor_id,
                provider_id,
                data_epoch,
                format_version,
            } => {
                tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => return Err(cancelled_failure()),
                    _ = self.operation.cancellation().cancelled() => return Err(cancelled_failure()),
                    result = store.write_private_state(
                        vendor_id,
                        provider_id,
                        *data_epoch,
                        format_version,
                        &bytes,
                    ) => result.map_err(plugin_storage_failure)?,
                }
            }
            StateScope::Session(state) => {
                let mut guard = tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => return Err(cancelled_failure()),
                    _ = self.operation.cancellation().cancelled() => return Err(cancelled_failure()),
                    guard = state.lock() => guard,
                };
                self.ensure_current()?;
                *guard = Some(bytes);
            }
        }
        self.ensure_current()?;
        drop(fence);
        Ok(())
    }

    async fn emit_event(&self, mut event: RuntimeEvent) -> Result<(), HostFailure> {
        self.ensure_current()?;
        let Some(events) = &self.events else {
            self.event_delivery_failed.store(true, Ordering::Release);
            return Err(HostFailure::new(
                ErrorKind::Trapped,
                "model events are not accepted for this operation",
            ));
        };
        // 字面秘密与通用凭据结构都要保护，包括 URL 编码后不再匹配原值的回显。
        match &mut event {
            RuntimeEvent::Delta(
                stravia_runtime_contract::protocol::ir::AiStreamDelta::StreamError { error },
            ) => {
                self.secrets.text(&mut error.message);
                error.message = crate::interaction_observation::redact_text(&error.message);
                if let Some(raw) = &mut error.raw {
                    self.secrets.value(raw);
                    crate::interaction_observation::redact_value(raw);
                }
            }
            RuntimeEvent::Failed { message, .. } => {
                self.secrets.text(message);
                *message = crate::interaction_observation::redact_text(message);
            }
            _ => {}
        }
        let fence = self
            .operation
            .write_fence()
            .await
            .map_err(|_| cancelled_failure())?;
        self.ensure_current()?;
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => return Err(cancelled_failure()),
            _ = self.operation.cancellation().cancelled() => return Err(cancelled_failure()),
            result = events.send(VendorEvent {
                event,
                publication: self.publication.clone(),
            }) => {
                if result.is_err() {
                    self.event_delivery_failed.store(true, Ordering::Release);
                    return Err(cancelled_failure());
                }
            }
        }
        self.ensure_current()?;
        drop(fence);
        Ok(())
    }

    fn log(&self, level: LogLevel, message: &str) {
        let mut message = message.to_owned();
        self.secrets.text(&mut message);
        let message = crate::interaction_observation::redact_text(&message);
        match level {
            LogLevel::Debug => {
                tracing::debug!(target: "stravia::vendor", message = %message, "vendor plugin log")
            }
            LogLevel::Info => {
                tracing::info!(target: "stravia::vendor", message = %message, "vendor plugin log")
            }
            LogLevel::Warn => {
                tracing::warn!(target: "stravia::vendor", message = %message, "vendor plugin log")
            }
            LogLevel::Error => {
                tracing::error!(target: "stravia::vendor", message = %message, "vendor plugin log")
            }
        }
    }

    fn generation_is_current(&self, _generation: u64) -> bool {
        self.ensure_current().is_ok()
    }
}

fn register_snapshot_secrets(secrets: &ProtectedSecrets, provider: &ProviderSnapshot) {
    for value in provider.credentials.values() {
        register_value_secrets(secrets, value);
    }
    secrets.register(
        provider
            .client_headers
            .iter()
            .map(|(_, value)| value.as_str()),
    );
}

fn register_value_secrets(secrets: &ProtectedSecrets, value: &Value) {
    match value {
        Value::String(value) => secrets.register([value.as_str()]),
        Value::Array(values) => {
            for value in values {
                register_value_secrets(secrets, value);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                register_value_secrets(secrets, value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn cancelled_failure() -> HostFailure {
    HostFailure::new(ErrorKind::Cancelled, "vendor operation was cancelled")
}

fn storage_failure() -> HostFailure {
    HostFailure::new(ErrorKind::Trapped, "vendor private state storage failed")
}

fn plugin_storage_failure(error: PluginStorageError) -> HostFailure {
    match error {
        PluginStorageError::StaleOperation | PluginStorageError::Changed => cancelled_failure(),
        PluginStorageError::Storage => storage_failure(),
    }
}
