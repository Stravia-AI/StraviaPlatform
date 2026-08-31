pub mod admin;
mod admission;
pub mod agent;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub(crate) mod generation_chain;
pub mod history_marker;
pub mod hook;
pub mod logging;
pub mod mcp;
pub(crate) mod media;
mod migrations;
pub(crate) mod model_turn;
pub mod plugin;
pub mod protocol;
pub mod provider;
pub mod provider_catalog;
pub mod provider_models;
pub mod proxy;
pub mod router;
pub mod storage;
pub mod thinking;
pub mod turn_chain;
pub(crate) mod web_access;
pub mod web_search;
#[cfg(debug_assertions)]
mod wire_capture;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use sqlx::{Pool, Postgres, SqlitePool};
use tokio::sync::mpsc;

use crate::auth::types::AuthSession;
use crate::hook::{Hook, HookRuntime, PlatformTool, PlatformToolRegistry};
use crate::mcp::{McpTool, McpToolRegistry};
use crate::router::health::HealthRegistry;
use config::{GatewayConfig, SqlStorageConfig, StorageBackendKind};
use logging::LogEntry;
use storage::sql::config::SqlBackendConfig;
use storage::{DynStorage, PostgresStorage, SqliteStorage};

const STORE_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
type StorageRuntime = (
    RuntimeStorageKind,
    DynStorage,
    Option<SqlitePool>,
    Option<Pool<Postgres>>,
);

struct GatewayLifecycle {
    cancellation: proxy::context::CancellationToken,
    owners: AtomicUsize,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl GatewayLifecycle {
    fn new() -> Self {
        Self {
            cancellation: proxy::context::CancellationToken::new(),
            owners: AtomicUsize::new(1),
            tasks: Mutex::new(Vec::new()),
        }
    }

    fn add_owner(&self) {
        self.owners.fetch_add(1, Ordering::AcqRel);
    }

    fn release_owner(&self) -> bool {
        self.owners.fetch_sub(1, Ordering::AcqRel) == 1
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(task);
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.push(handle);
    }

    fn abort_tasks(&self) {
        self.cancellation.cancel();
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for task in tasks.iter() {
            task.abort();
        }
    }

    async fn shutdown(&self) {
        self.cancellation.cancel();
        let tasks = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *tasks)
        };
        for task in tasks {
            let _ = task.await;
        }
    }
}

impl Drop for GatewayLifecycle {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let tasks = self
            .tasks
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}

#[derive(Clone, Debug)]
pub struct CapabilityCacheEntry {
    pub capabilities: Vec<String>,
    pub cached_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeStorageKind {
    Memory,
    Sqlite,
    Postgres,
}

pub struct Gateway {
    pub config: GatewayConfig,
    pub storage: DynStorage,
    pub storage_kind: RuntimeStorageKind,
    pub http_client: reqwest::Client,
    responses_websocket_client: reqwest::Client,
    pub provider_catalog: provider_catalog::ProviderCatalog,
    pub(crate) provider_allowance_state: admin::provider_allowance::ProviderAllowanceState,
    proxy_client_cache: Arc<tokio::sync::RwLock<Option<ProxyClientCache>>>,
    responses_websockets: proxy::client::ResponsesWebSocketRegistry,
    pub(crate) principal_admission: Arc<admission::PrincipalAdmission>,
    pub model_cache: Arc<tokio::sync::RwLock<router::RouteCache>>,
    pub health_registry: Arc<HealthRegistry>,
    pub(crate) cache_affinity: router::cache_affinity::CacheAffinity,
    pub ollama_capability_cache: Arc<tokio::sync::RwLock<HashMap<String, CapabilityCacheEntry>>>,
    pub log_tx: mpsc::Sender<LogEntry>,
    #[cfg(debug_assertions)]
    pub(crate) wire_capture: Option<wire_capture::WireCapture>,
    pub(crate) auth_sessions: Arc<tokio::sync::RwLock<HashMap<String, AuthSession>>>,
    pub(crate) agent_definitions: agent::AgentDefinitionRegistry,
    pub(crate) artifact_store: Option<Arc<dyn agent::ArtifactStore>>,
    pub(crate) media_derivatives: Option<Arc<media::MediaDerivativeStore>>,
    media_understanding: Arc<tokio::sync::RwLock<Option<media::MediaUnderstandingService>>>,
    media_run_snapshots: media::MediaRunSnapshotStore,
    hook_runtime: HookRuntime,
    pub(crate) mcp_registry: McpToolRegistry,
    pub(crate) history_markers: Arc<dyn history_marker::HistoryMarkerStore>,
    pub(crate) turn_chains: Arc<dyn turn_chain::TurnChainStore>,
    pub(crate) generation_chains: generation_chain::GenerationChain,
    pub(crate) model_turn: Arc<dyn model_turn::ModelTurnExecutor>,
    pub(crate) web_access_run_snapshots: web_access::WebAccessRunSnapshotStore,
    web_search_runner_state: Arc<tokio::sync::RwLock<Option<web_search::WebSearchRunner>>>,
    web_search_config_lock: Arc<tokio::sync::Mutex<()>>,
    _sqlite_pool: Option<SqlitePool>,
    _postgres_pool: Option<Pool<Postgres>>,
    history_marker_execution_gate: Arc<tokio::sync::RwLock<()>>,
    lifecycle: Arc<GatewayLifecycle>,
    lifecycle_owner: bool,
}

pub(crate) struct HistoryMarkerExecutionJob {
    pub(crate) marker_reference: String,
    pub(crate) owner_id: String,
    pub(crate) execution_deadline_unix_ms: i64,
    pub(crate) execution: hook::DetachedPlatformExecution,
}

pub(crate) struct StartedHistoryMarkerExecution {
    marker_reference: String,
    raw_result: tokio::sync::oneshot::Receiver<RawHistoryMarkerExecution>,
    transformed_result: tokio::sync::oneshot::Sender<hook::PlatformToolResult>,
}

#[derive(Clone)]
struct RawHistoryMarkerExecution {
    call: protocol::ir::ToolCall,
    result: hook::PlatformToolResult,
}

impl Gateway {
    fn clone_with_owner(&self, lifecycle_owner: bool) -> Self {
        if lifecycle_owner {
            self.lifecycle.add_owner();
        }
        Self {
            config: self.config.clone(),
            storage: Arc::clone(&self.storage),
            storage_kind: self.storage_kind,
            http_client: self.http_client.clone(),
            responses_websocket_client: self.responses_websocket_client.clone(),
            provider_catalog: self.provider_catalog.clone(),
            provider_allowance_state: self.provider_allowance_state.clone(),
            proxy_client_cache: Arc::clone(&self.proxy_client_cache),
            principal_admission: Arc::clone(&self.principal_admission),
            responses_websockets: self.responses_websockets.clone(),
            model_cache: Arc::clone(&self.model_cache),
            health_registry: Arc::clone(&self.health_registry),
            cache_affinity: self.cache_affinity.clone(),
            ollama_capability_cache: Arc::clone(&self.ollama_capability_cache),
            log_tx: self.log_tx.clone(),
            #[cfg(debug_assertions)]
            wire_capture: self.wire_capture.clone(),
            auth_sessions: Arc::clone(&self.auth_sessions),
            agent_definitions: self.agent_definitions.clone(),
            artifact_store: self.artifact_store.clone(),
            media_derivatives: self.media_derivatives.clone(),
            media_understanding: Arc::clone(&self.media_understanding),
            media_run_snapshots: self.media_run_snapshots.clone(),
            hook_runtime: self.hook_runtime.clone(),
            mcp_registry: self.mcp_registry.clone(),
            history_markers: Arc::clone(&self.history_markers),
            turn_chains: Arc::clone(&self.turn_chains),
            generation_chains: self.generation_chains.clone(),
            model_turn: Arc::clone(&self.model_turn),
            web_access_run_snapshots: self.web_access_run_snapshots.clone(),
            web_search_runner_state: Arc::clone(&self.web_search_runner_state),
            web_search_config_lock: Arc::clone(&self.web_search_config_lock),
            _sqlite_pool: self._sqlite_pool.clone(),
            _postgres_pool: self._postgres_pool.clone(),
            history_marker_execution_gate: Arc::clone(&self.history_marker_execution_gate),
            lifecycle: Arc::clone(&self.lifecycle),
            lifecycle_owner,
        }
    }

    fn background_clone(&self) -> Self {
        self.clone_with_owner(false)
    }

    async fn execute_history_marker_job(
        job: HistoryMarkerExecutionJob,
    ) -> RawHistoryMarkerExecution {
        let call = job.execution.call().call.clone();
        let remaining_ms = job
            .execution_deadline_unix_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis());
        let result = if remaining_ms <= 0 {
            hook::PlatformToolResult {
                tool_id: hook::ToolId::new("deadline"),
                call_id: call.id.clone(),
                content: serde_json::Value::String(
                    "Platform tool execution reached its registered deadline.".into(),
                ),
                is_error: true,
                metadata: serde_json::Map::new(),
            }
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_millis(remaining_ms as u64),
                job.execution.execute(),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => hook::PlatformToolResult {
                    tool_id: hook::ToolId::new("deadline"),
                    call_id: call.id.clone(),
                    content: serde_json::Value::String(
                        "Platform tool execution reached its registered deadline.".into(),
                    ),
                    is_error: true,
                    metadata: serde_json::Map::new(),
                },
            }
        };
        RawHistoryMarkerExecution { call, result }
    }

    async fn persist_history_marker_result(
        store: &dyn history_marker::HistoryMarkerStore,
        principal: &hook::Principal,
        marker_reference: &str,
        owner_id: &str,
        raw: RawHistoryMarkerExecution,
        result: hook::PlatformToolResult,
    ) {
        let state = if result.is_error {
            history_marker::PlatformExecutionState::Failed
        } else {
            history_marker::PlatformExecutionState::Completed
        };
        if let Err(error) = store
            .finish_execution(
                principal,
                marker_reference,
                owner_id,
                state,
                history_marker::HiddenHistorySegment::Platform {
                    call: raw.call,
                    result: result.content_block(),
                },
            )
            .await
        {
            tracing::error!(
                marker_reference,
                error = %error,
                "failed to persist Platform Tool terminal result"
            );
        }
    }

    pub(crate) fn start_history_marker_executions(
        &self,
        principal: hook::Principal,
        jobs: Vec<HistoryMarkerExecutionJob>,
    ) -> Vec<StartedHistoryMarkerExecution> {
        jobs.into_iter()
            .map(|job| {
                let marker_reference = job.marker_reference.clone();
                let owner_id = job.owner_id.clone();
                let store = Arc::clone(&self.history_markers);
                let principal = principal.clone();
                let task_marker_reference = marker_reference.clone();
                let execution_gate = Arc::clone(&self.history_marker_execution_gate);
                let parallel_safe = job.execution.parallel_safe();
                let (raw_tx, raw_result) = tokio::sync::oneshot::channel();
                let (transformed_result, transformed_rx) = tokio::sync::oneshot::channel();
                self.lifecycle.spawn(async move {
                    let raw = if parallel_safe {
                        let _permit = execution_gate.read().await;
                        Self::execute_history_marker_job(job).await
                    } else {
                        let _permit = execution_gate.write().await;
                        Self::execute_history_marker_job(job).await
                    };
                    if raw_tx.send(raw.clone()).is_err() {
                        let result = raw.result.clone();
                        Self::persist_history_marker_result(
                            store.as_ref(),
                            &principal,
                            &task_marker_reference,
                            &owner_id,
                            raw,
                            result,
                        )
                        .await;
                        return;
                    }
                    let result = transformed_rx.await.unwrap_or_else(|_| raw.result.clone());
                    Self::persist_history_marker_result(
                        store.as_ref(),
                        &principal,
                        &task_marker_reference,
                        &owner_id,
                        raw,
                        result,
                    )
                    .await;
                });
                StartedHistoryMarkerExecution {
                    marker_reference,
                    raw_result,
                    transformed_result,
                }
            })
            .collect()
    }

    async fn finish_history_marker_executions(
        executions: Vec<StartedHistoryMarkerExecution>,
        run: &mut hook::InferenceRun,
    ) {
        for execution in executions {
            let Ok(mut raw) = execution.raw_result.await else {
                continue;
            };
            let hook_failure = match run.on_tool_result(&mut raw.result).await {
                Ok(hook::HookControl::Continue) => None,
                Ok(
                    hook::HookControl::Respond(_)
                    | hook::HookControl::Reject(_)
                    | hook::HookControl::StreamAbort { .. },
                ) => Some("ToolResult Hook attempted response control".to_owned()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = hook_failure {
                tracing::error!(
                    marker_reference = execution.marker_reference,
                    error,
                    "ToolResult Hook failed for background Platform Tool"
                );
                raw.result.content = serde_json::Value::String(
                    "Platform tool result processing failed before persistence.".into(),
                );
                raw.result.is_error = true;
            }
            let _ = execution.transformed_result.send(raw.result);
        }
    }

    pub(crate) async fn run_history_marker_executions(
        &self,
        principal: hook::Principal,
        jobs: Vec<HistoryMarkerExecutionJob>,
        run: &mut hook::InferenceRun,
    ) {
        let executions = self.start_history_marker_executions(principal, jobs);
        Self::finish_history_marker_executions(executions, run).await;
    }

    pub(crate) async fn run_started_history_marker_executions(
        &self,
        executions: Vec<StartedHistoryMarkerExecution>,
        run: &mut hook::InferenceRun,
    ) {
        Self::finish_history_marker_executions(executions, run).await;
    }

    pub(crate) fn spawn_started_history_marker_executions(
        &self,
        executions: Vec<StartedHistoryMarkerExecution>,
        mut run: hook::InferenceRun,
    ) {
        self.lifecycle.spawn(async move {
            Self::finish_history_marker_executions(executions, &mut run).await;
        });
    }

    fn clone_for_live(&self) -> Self {
        let mut cloned = self.clone_with_owner(false);
        cloned.model_turn = model_turn::unreachable_executor();
        cloned
    }

    fn install_model_turn(&mut self) {
        self.model_turn = Arc::new(model_turn::LiveModelTurnExecutor::new(
            self.clone_for_live(),
            self.generation_chains.continuation_lookup(),
        ));
    }
}

impl Clone for Gateway {
    fn clone(&self) -> Self {
        self.clone_with_owner(self.lifecycle_owner)
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if self.lifecycle_owner && self.lifecycle.release_owner() {
            self.lifecycle.abort_tasks();
        }
    }
}

#[derive(Clone)]
struct ProxyClientCache {
    cache_key: String,
    client: reqwest::Client,
}

pub struct GatewayBuilder {
    config: GatewayConfig,
    storage: Option<DynStorage>,
    hooks: Vec<Arc<dyn Hook>>,
    tools: Vec<Arc<dyn PlatformTool>>,
    mcp_tools: Vec<Arc<dyn McpTool>>,
    agent_definitions: Vec<agent::AgentDefinitionSpec>,
    generation_chain_ttl: Duration,
}

impl GatewayBuilder {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            storage: None,
            hooks: Vec::new(),
            tools: Vec::new(),
            mcp_tools: Vec::new(),
            agent_definitions: Vec::new(),
            generation_chain_ttl: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }

    pub fn storage(mut self, storage: DynStorage) -> Self {
        self.storage = Some(storage);
        self
    }

    pub fn hook(mut self, hook: Arc<dyn Hook>) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn platform_tool(mut self, tool: Arc<dyn PlatformTool>) -> Self {
        self.tools.push(tool);
        self
    }
    pub fn mcp_tool(mut self, tool: Arc<dyn McpTool>) -> Self {
        self.mcp_tools.push(tool);
        self
    }

    pub fn agent_definition(mut self, definition: agent::AgentDefinitionSpec) -> Self {
        self.agent_definitions.push(definition);
        self
    }

    pub fn generation_chain_ttl(mut self, ttl: Duration) -> Self {
        self.generation_chain_ttl = ttl;
        self
    }

    pub async fn build(self) -> anyhow::Result<(Gateway, mpsc::Receiver<LogEntry>)> {
        let Self {
            config,
            storage,
            hooks,
            tools,
            mcp_tools,
            agent_definitions,
            generation_chain_ttl,
        } = self;
        let (mut gateway, log_rx) = if let Some(storage) = storage {
            Gateway::from_storage(config, storage).await?
        } else {
            Gateway::new(config).await?
        };
        gateway.generation_chains = generation_chain::GenerationChain::from_turn_chain(
            Arc::clone(&gateway.turn_chains),
            generation_chain_ttl,
            gateway.artifact_store.clone(),
        )
        .with_history_markers(Arc::clone(&gateway.history_markers));
        gateway.install_model_turn();
        configure_gateway_extensions(&mut gateway, hooks, tools, mcp_tools, agent_definitions)
            .await?;
        Ok((gateway, log_rx))
    }
}

async fn configure_gateway_extensions(
    gateway: &mut Gateway,
    mut hooks: Vec<Arc<dyn Hook>>,
    mut tools: Vec<Arc<dyn PlatformTool>>,
    mut mcp_tools: Vec<Arc<dyn McpTool>>,
    mut agent_definitions: Vec<agent::AgentDefinitionSpec>,
) -> anyhow::Result<()> {
    agent_definitions.push(web_search::local_search_definition());
    agent_definitions.push(media::media_definition());
    gateway
        .agent_definitions
        .synchronize(agent_definitions)
        .await?;
    let web_platform_tools = web_access::internal_platform_tools(gateway);
    let mut runner_tools: Vec<Arc<dyn agent::AgentTool>> = tools
        .iter()
        .map(|tool| {
            Arc::new(agent::PlatformToolAgentAdapter::new(Arc::clone(tool), 1))
                as Arc<dyn agent::AgentTool>
        })
        .collect();
    for tool in &web_platform_tools {
        runner_tools.push(Arc::new(agent::PlatformToolAgentAdapter::with_id(
            Arc::clone(tool),
            agent::VersionedToolId {
                id: tool.id().as_str().to_owned(),
                version: 1,
            },
        )));
    }
    for tool in &mcp_tools {
        runner_tools.push(Arc::new(agent::McpToolAgentAdapter::new(
            Arc::clone(tool),
            1,
        )));
    }
    let report_validator = Arc::new(web_search::SearchReportValidator);
    let local_search_evidence = Arc::new(web_search::LocalSearchEvidenceStore::default());
    let model = Arc::clone(&gateway.model_turn);
    let mut runner = agent::AgentRunner::new(
        gateway.agent_definitions.clone(),
        model,
        runner_tools,
        Arc::clone(&gateway.turn_chains),
    )?
    .with_hook_runtime(gateway.hook_runtime.clone())
    .with_tool_authorizer(Arc::new(GatewayAgentToolAuthorizer {
        storage: Arc::clone(&gateway.storage),
    }))
    .with_artifact_store(gateway.artifact_store.clone())
    .with_run_lifecycles(vec![Arc::new(WebAccessAgentRunLifecycle {
        service: gateway.web_access(),
    })])
    .with_output_validator(
        agent::AgentDefinitionId::new(web_search::LOCAL_SEARCH_DEFINITION_ID),
        web_search::LOCAL_SEARCH_DEFINITION_REVISION,
        Arc::new(web_search::LocalSearchOutputValidator::new(
            Arc::clone(&report_validator),
            Arc::clone(&local_search_evidence),
        )),
    )
    .with_capability_model_authorization(
        agent::AgentDefinitionId::new(web_search::LOCAL_SEARCH_DEFINITION_ID),
        web_search::LOCAL_SEARCH_DEFINITION_REVISION,
        agent::CapabilityModelAuthorization::WebSearch,
    )
    .with_capability_model_authorization(
        agent::AgentDefinitionId::new(media::MEDIA_DEFINITION_ID),
        media::MEDIA_DEFINITION_REVISION,
        agent::CapabilityModelAuthorization::MediaUnderstanding,
    );
    if let Some(store) = gateway.media_derivatives.as_ref() {
        runner = runner.with_output_validator(
            agent::AgentDefinitionId::new(media::MEDIA_DEFINITION_ID),
            media::MEDIA_DEFINITION_REVISION,
            Arc::new(media::MediaReportValidator::new(Arc::clone(store))),
        );
    }
    if let Some(store) = gateway.media_derivatives.as_ref() {
        *gateway.media_understanding.write().await = Some(media::MediaUnderstandingService::new(
            runner.clone(),
            Arc::clone(store),
        ));
    }
    let definitions = gateway.agent_definitions.list().await;
    for record in &definitions {
        runner.validate_definition_tools(&record.spec)?;
    }
    let public_definitions = gateway.agent_definitions.list_public().await;
    if !public_definitions.is_empty() {
        hooks.push(Arc::new(agent::AgentDefinitionHook::new(
            gateway.agent_definitions.clone(),
        )));
    }
    for record in public_definitions {
        tools.push(Arc::new(agent::AgentCallPlatformTool::new(
            record.spec.id.clone(),
            record.spec.slug.as_str(),
            record.spec.description.clone(),
            runner.clone(),
        )));
        mcp_tools.push(Arc::new(agent::AgentCallMcpTool::new(
            record.spec.id,
            record.spec.slug.as_str(),
            record.spec.description,
            runner.clone(),
            gateway.clone(),
        )));
    }
    let search_runner = web_search::WebSearchRunner::new(
        Arc::new(web_search::SettingsWebSearchConfigStore::new(Arc::clone(
            &gateway.storage,
        ))),
        Arc::clone(&gateway.turn_chains),
        Arc::new(web_search::LocalSearchBackend::new(
            runner,
            local_search_evidence,
        )),
        Arc::new(web_search::CodexAgenticSearchBackend::new(gateway.clone())),
        report_validator,
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(GatewayWebSearchAuthorizer {
            storage: Arc::clone(&gateway.storage),
        }),
    );
    *gateway.web_search_runner_state.write().await = Some(search_runner);
    gateway.hook_runtime = hook_runtime_with_web_search(gateway, hooks, tools)?;
    gateway.mcp_registry = mcp_registry_with_web_search(gateway, mcp_tools)?;
    Ok(())
}
struct GatewayWebSearchAuthorizer {
    storage: storage::DynStorage,
}

fn web_search_authorization_error() -> web_search::WebSearchError {
    web_search::WebSearchError::new("authorization_failed", "Web Search authorization failed")
}

#[async_trait::async_trait]
impl web_search::SearchRunAuthorizer for GatewayWebSearchAuthorizer {
    async fn authorize(
        &self,
        principal: &hook::Principal,
        binding: &web_search::ResolvedWebSearchBackend,
    ) -> Result<(), web_search::WebSearchError> {
        proxy::security::Security::new(self.storage.auth())
            .authorize_principal_web_search(principal)
            .await
            .map_err(|_| web_search_authorization_error())?;
        match binding {
            web_search::ResolvedWebSearchBackend::Local { model_id } => {
                self.storage
                    .routes()
                    .list_active()
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .into_iter()
                    .find(|route| route.id == *model_id)
                    .ok_or_else(web_search_authorization_error)?;
                proxy::security::Security::new(self.storage.auth())
                    .authorize_principal_capability(principal)
                    .await
                    .map_err(|_| web_search_authorization_error())?;
            }
            web_search::ResolvedWebSearchBackend::Codex {
                provider_id,
                upstream_model,
            } => {
                let provider = self
                    .storage
                    .providers()
                    .get(provider_id)
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .filter(web_search::codex_provider_contract)
                    .ok_or_else(web_search_authorization_error)?;
                let model_available = self
                    .storage
                    .provider_models()
                    .get(provider_id, upstream_model)
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .is_some_and(|model| {
                        model.model_id == *upstream_model && model.effective_available()
                    });
                let credential_available = self
                    .storage
                    .oauth_credentials()
                    .get(&provider.id)
                    .await
                    .map_err(|_| web_search_authorization_error())?
                    .is_some();
                if !model_available || !credential_available {
                    return Err(web_search_authorization_error());
                }
            }
        }
        Ok(())
    }
}

struct GatewayAgentToolAuthorizer {
    storage: storage::DynStorage,
}

fn agent_tool_authorization_error() -> agent::AgentRunError {
    agent::AgentRunError::new(
        "tool_authorization_failed",
        "Agent Tool authorization failed",
    )
}

#[async_trait::async_trait]
impl agent::AgentToolAuthorizer for GatewayAgentToolAuthorizer {
    async fn authorize(
        &self,
        principal: &hook::Principal,
        definition_id: &agent::AgentDefinitionId,
        model_id: &str,
    ) -> Result<(), agent::AgentRunError> {
        let model = self
            .storage
            .routes()
            .list_active()
            .await
            .map_err(|_| agent_tool_authorization_error())?
            .into_iter()
            .find(|route| route.id == model_id)
            .ok_or_else(agent_tool_authorization_error)?;
        let security = crate::proxy::security::Security::new(self.storage.auth());
        let capability_owned = definition_id.as_str() == web_search::LOCAL_SEARCH_DEFINITION_ID
            || definition_id.as_str() == media::MEDIA_DEFINITION_ID;
        if capability_owned {
            security.authorize_principal_capability(principal).await
        } else {
            security.authorize_principal_model(principal, &model).await
        }
        .map_err(|_| agent_tool_authorization_error())?;
        Ok(())
    }
}

struct WebAccessAgentRunLifecycle {
    service: web_access::WebAccessService,
}

struct WebAccessAgentRunGuard {
    service: web_access::WebAccessService,
    run_id: String,
}

impl agent::AgentRunGuard for WebAccessAgentRunGuard {}

impl Drop for WebAccessAgentRunGuard {
    fn drop(&mut self) {
        self.service.release_run_snapshot(&self.run_id);
    }
}

#[async_trait::async_trait]
impl agent::AgentRunLifecycle for WebAccessAgentRunLifecycle {
    async fn start(
        &self,
        principal: &hook::Principal,
        run_id: &agent::AgentTurnId,
    ) -> Result<Box<dyn agent::AgentRunGuard>, agent::AgentRunError> {
        self.service
            .capture_run_snapshot(run_id.as_str(), principal.api_key_id())
            .await
            .map_err(|error| {
                agent::AgentRunError::new("web_access_unavailable", error.to_string())
            })?;
        Ok(Box::new(WebAccessAgentRunGuard {
            service: self.service.clone(),
            run_id: run_id.as_str().to_owned(),
        }))
    }
}

fn hook_runtime_with_web_search(
    gateway: &Gateway,
    mut hooks: Vec<Arc<dyn Hook>>,
    mut tools: Vec<Arc<dyn PlatformTool>>,
) -> anyhow::Result<HookRuntime> {
    let (builtin_hooks, builtin_tools) = web_search::builtin_extensions(gateway);
    hooks.extend(builtin_hooks);
    tools.extend(builtin_tools);
    tools.extend(media::platform_tools(gateway));
    hooks.push(media::planning_hook(gateway));

    let mut hook_ids = std::collections::HashSet::new();
    for hook in &hooks {
        let descriptor = hook.descriptor();
        if descriptor.id.as_str().trim().is_empty() {
            anyhow::bail!("hook id cannot be empty");
        }
        if !hook_ids.insert(descriptor.id.as_str().to_string()) {
            anyhow::bail!("duplicate hook id: {}", descriptor.id);
        }
    }
    let tool_registry = PlatformToolRegistry::new(tools).map_err(anyhow::Error::new)?;
    Ok(HookRuntime::with_tools(hooks, tool_registry))
}
fn mcp_registry_with_web_search(
    gateway: &Gateway,
    mut tools: Vec<Arc<dyn McpTool>>,
) -> anyhow::Result<McpToolRegistry> {
    tools.extend(web_search::mcp_tools(gateway));
    tools.extend(media::mcp_tools(gateway));
    McpToolRegistry::new(tools)
}

impl Gateway {
    pub fn builder(config: GatewayConfig) -> GatewayBuilder {
        GatewayBuilder::new(config)
    }

    pub async fn shutdown(&self) {
        self.lifecycle.shutdown().await;
    }

    pub async fn new(config: GatewayConfig) -> anyhow::Result<(Self, mpsc::Receiver<LogEntry>)> {
        let (storage_kind, storage, sqlite_pool, postgres_pool): StorageRuntime =
            match config.storage.backend {
                StorageBackendKind::Sqlite => {
                    let pool = db::init_pool(&config.data_dir).await?;
                    migrations::migrate_sqlite(&pool).await?;
                    let sqlite_storage = SqliteStorage::from_pool(pool.clone());
                    (
                        RuntimeStorageKind::Sqlite,
                        Arc::new(sqlite_storage),
                        Some(pool),
                        None,
                    )
                }
                StorageBackendKind::Postgres => {
                    let backend_config =
                        to_sql_backend_config(&config.storage.postgres, "postgres")?;
                    let postgres_storage = PostgresStorage::connect(backend_config).await?;
                    let pool = postgres_storage.pool().clone();
                    migrations::migrate_postgres(&pool).await?;
                    (
                        RuntimeStorageKind::Postgres,
                        Arc::new(postgres_storage),
                        None,
                        Some(pool),
                    )
                }
            };

        let health = storage.bootstrap().health().await?;
        if !health.can_connect {
            anyhow::bail!("selected storage backend is not reachable");
        }

        Self::from_storage_with_kind(config, storage, storage_kind, sqlite_pool, postgres_pool)
            .await
    }

    pub async fn from_storage(
        config: GatewayConfig,
        storage: DynStorage,
    ) -> anyhow::Result<(Self, mpsc::Receiver<LogEntry>)> {
        Self::from_storage_with_kind(config, storage, RuntimeStorageKind::Memory, None, None).await
    }

    async fn from_storage_with_kind(
        config: GatewayConfig,
        storage: DynStorage,
        storage_kind: RuntimeStorageKind,
        sqlite_pool: Option<SqlitePool>,
        postgres_pool: Option<Pool<Postgres>>,
    ) -> anyhow::Result<(Self, mpsc::Receiver<LogEntry>)> {
        let history_sqlite_pool = if sqlite_pool.is_none() && postgres_pool.is_none() {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await?;
            migrations::migrate_sqlite(&pool).await?;
            Some(pool)
        } else {
            sqlite_pool.clone()
        };
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        let responses_websocket_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .http1_only()
            .build()?;

        let model_cache = Arc::new(tokio::sync::RwLock::new(
            router::RouteCache::load(storage.routes()).await?,
        ));
        let health_registry = Arc::new(HealthRegistry::new());
        let ollama_capability_cache = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let provider_catalog = provider_catalog::ProviderCatalog::new(&config.data_dir)?;
        #[cfg(debug_assertions)]
        let wire_capture = {
            let capture = config
                .wire_capture_dir
                .clone()
                .map(wire_capture::WireCapture::new)
                .transpose()?;
            if let Some(directory) = &config.wire_capture_dir {
                tracing::warn!(
                    directory = %directory.display(),
                    "wire capture enabled; redacted headers and full request/response bodies will be written to disk"
                );
            }
            capture
        };

        let (log_tx, log_rx) = mpsc::channel(1024);
        let turn_chains: Arc<dyn turn_chain::TurnChainStore> =
            if let Some(pool) = history_sqlite_pool.as_ref() {
                Arc::new(turn_chain::SqlTurnChainStore::sqlite(pool.clone()))
            } else {
                Arc::new(turn_chain::SqlTurnChainStore::postgres(
                    postgres_pool
                        .as_ref()
                        .expect("Gateway requires a SQL history store")
                        .clone(),
                ))
            };
        let history_markers: Arc<dyn history_marker::HistoryMarkerStore> =
            if let Some(pool) = history_sqlite_pool.as_ref() {
                Arc::new(history_marker::SqlHistoryMarkerStore::sqlite(pool.clone()))
            } else {
                Arc::new(history_marker::SqlHistoryMarkerStore::postgres(
                    postgres_pool
                        .as_ref()
                        .expect("Gateway requires a SQL history store")
                        .clone(),
                ))
            };
        let agent_definitions = if let Some(pool) = sqlite_pool.as_ref() {
            agent::AgentDefinitionRegistry::sqlite(pool.clone())
        } else if let Some(pool) = postgres_pool.as_ref() {
            agent::AgentDefinitionRegistry::postgres(pool.clone())
        } else {
            agent::AgentDefinitionRegistry::default()
        };

        let (artifact_store, media_derivatives): (
            Option<Arc<dyn agent::ArtifactStore>>,
            Option<Arc<media::MediaDerivativeStore>>,
        ) = if let Some(pool) = sqlite_pool.as_ref() {
            let local = Arc::new(agent::LocalArtifactStore::sqlite(
                pool.clone(),
                config.data_dir.join("artifacts"),
            ));
            let artifacts: Arc<dyn agent::ArtifactStore> = local.clone();
            (
                Some(artifacts),
                Some(Arc::new(media::MediaDerivativeStore::sqlite(
                    pool.clone(),
                    local,
                ))),
            )
        } else if let Some(pool) = postgres_pool.as_ref() {
            let local = Arc::new(agent::LocalArtifactStore::postgres(
                pool.clone(),
                config.data_dir.join("artifacts"),
            ));
            let artifacts: Arc<dyn agent::ArtifactStore> = local.clone();
            (
                Some(artifacts),
                Some(Arc::new(media::MediaDerivativeStore::postgres(
                    pool.clone(),
                    local,
                ))),
            )
        } else {
            (None, None)
        };
        let generation_chains = generation_chain::GenerationChain::from_turn_chain(
            Arc::clone(&turn_chains),
            Duration::from_secs(7 * 24 * 60 * 60),
            artifact_store.clone(),
        )
        .with_history_markers(Arc::clone(&history_markers));
        let mut gw = Self {
            config,
            storage,
            storage_kind,
            http_client,
            responses_websocket_client,
            provider_catalog,
            provider_allowance_state: admin::provider_allowance::ProviderAllowanceState::default(),
            proxy_client_cache: Arc::new(tokio::sync::RwLock::new(None)),
            responses_websockets: proxy::client::ResponsesWebSocketRegistry::default(),
            model_cache,
            health_registry,
            cache_affinity: router::cache_affinity::CacheAffinity::default(),
            ollama_capability_cache,
            log_tx,
            #[cfg(debug_assertions)]
            wire_capture,
            auth_sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            agent_definitions,
            artifact_store,
            media_run_snapshots: media::MediaRunSnapshotStore::default(),
            media_derivatives,
            media_understanding: Arc::new(tokio::sync::RwLock::new(None)),
            hook_runtime: HookRuntime::default(),
            mcp_registry: McpToolRegistry::default(),
            history_markers,
            turn_chains,
            generation_chains,
            model_turn: model_turn::unreachable_executor(),
            web_access_run_snapshots: web_access::WebAccessRunSnapshotStore::default(),
            web_search_runner_state: Arc::new(tokio::sync::RwLock::new(None)),
            web_search_config_lock: Arc::new(tokio::sync::Mutex::new(())),
            _sqlite_pool: sqlite_pool,
            _postgres_pool: postgres_pool,
            history_marker_execution_gate: Arc::new(tokio::sync::RwLock::new(())),
            lifecycle: Arc::new(GatewayLifecycle::new()),
            principal_admission: Arc::new(admission::PrincipalAdmission::new()),
            lifecycle_owner: true,
        };
        gw.install_model_turn();
        configure_gateway_extensions(&mut gw, Vec::new(), Vec::new(), Vec::new(), Vec::new())
            .await?;
        {
            let catalog = gw.provider_catalog.clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let initial_refresh = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    result = catalog.refresh() => result,
                };
                if let Err(error) = initial_refresh {
                    tracing::warn!(error = ?error, "provider catalog startup refresh failed");
                }

                let mut interval = tokio::time::interval(provider_catalog::REFRESH_INTERVAL);
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = interval.tick() => {}
                }
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    let refresh = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = catalog.refresh() => result,
                    };
                    if let Err(error) = refresh {
                        tracing::warn!(error = ?error, "provider catalog refresh failed");
                    }
                }
            });
        }

        {
            let gw_refresh = gw.background_clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(120));
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    let refresh = {
                        let admin = gw_refresh.admin();
                        tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = admin.refresh_oauth_providers() => result,
                        }
                    };
                    if let Err(error) = refresh {
                        tracing::warn!("background oauth refresh skipped: {error}");
                    }
                    let cleanup = {
                        let admin = gw_refresh.admin();
                        tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = admin.cleanup_auth_sessions() => result,
                        }
                    };
                    if let Err(error) = cleanup {
                        tracing::warn!("auth session cleanup skipped: {error}");
                    }
                }
            });
        }

        if !gw.config.config_poll_interval.is_zero() {
            let gw_poll = gw.background_clone();
            let poll_interval = gw.config.config_poll_interval;
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let initial_epoch = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    result = gw_poll.storage.settings().get(admin::settings::CONFIG_EPOCH_KEY) => result,
                };
                let mut known_epoch: i64 = initial_epoch
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);

                let mut interval = tokio::time::interval(poll_interval);
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = interval.tick() => {}
                }
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    let current_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = gw_poll.storage.settings().get(admin::settings::CONFIG_EPOCH_KEY) => result,
                    };
                    let current: i64 = match current_result {
                        Ok(val) => val.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
                        Err(error) => {
                            tracing::warn!("config epoch poll failed: {error}");
                            continue;
                        }
                    };

                    if current > known_epoch {
                        known_epoch = current;
                        let reload = tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = async {
                                gw_poll
                                    .model_cache
                                    .write()
                                    .await
                                    .reload(gw_poll.storage.routes())
                                    .await
                            } => result,
                        };
                        if let Err(error) = reload {
                            tracing::warn!("config epoch reload failed: {error}");
                        } else {
                            tracing::debug!("model_cache reloaded (epoch={current})");
                        }
                    }
                }
            });
        }

        {
            let turn_chains = Arc::clone(&gw.turn_chains);
            let history_markers = Arc::clone(&gw.history_markers);
            let artifact_store = gw.artifact_store.clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let mut interval = tokio::time::interval(STORE_SWEEP_INTERVAL);
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }

                    let turn_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = turn_chains.sweep_expired() => result,
                    };
                    if let Err(error) = turn_result {
                        tracing::warn!(error = ?error, "turn chain ttl cleanup failed");
                    }

                    let marker_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = history_markers.cleanup_expired() => result,
                    };
                    if let Err(error) = marker_result {
                        tracing::warn!(error = ?error, "history marker ttl cleanup failed");
                    }

                    if let Some(artifact_store) = artifact_store.as_ref() {
                        let artifact_result = tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = artifact_store.sweep_expired() => result,
                        };
                        if let Err(error) = artifact_result {
                            tracing::warn!(error = ?error, "artifact ttl cleanup failed");
                        }
                    }
                }
            });
        }

        Ok((gw, log_rx))
    }

    pub fn admin(&self) -> admin::AdminService {
        admin::AdminService::new(self.clone())
    }

    pub fn web_access(&self) -> web_access::WebAccessService {
        web_access::WebAccessService::new(self.clone())
    }

    pub async fn web_search_runner(&self) -> anyhow::Result<web_search::WebSearchRunner> {
        self.web_search_runner_state
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Web Search Runner is unavailable"))
    }

    pub fn hook_runtime(&self) -> &HookRuntime {
        &self.hook_runtime
    }

    pub fn agent_definitions(&self) -> &agent::AgentDefinitionRegistry {
        &self.agent_definitions
    }

    pub fn artifact_store(&self) -> Option<&Arc<dyn agent::ArtifactStore>> {
        self.artifact_store.as_ref()
    }

    pub async fn http_client_for_provider(
        &self,
        use_proxy: bool,
    ) -> anyhow::Result<reqwest::Client> {
        self.client_for_provider(use_proxy, false).await
    }

    pub(crate) async fn responses_websocket_client_for_provider(
        &self,
        use_proxy: bool,
    ) -> anyhow::Result<reqwest::Client> {
        self.client_for_provider(use_proxy, true).await
    }

    async fn client_for_provider(
        &self,
        use_proxy: bool,
        require_http1: bool,
    ) -> anyhow::Result<reqwest::Client> {
        let default_client = if require_http1 {
            &self.responses_websocket_client
        } else {
            &self.http_client
        };
        if !use_proxy {
            return Ok(default_client.clone());
        }

        let enabled = self
            .storage
            .settings()
            .get("proxy_enabled")
            .await?
            .as_deref()
            .map(parse_bool_setting)
            .unwrap_or(false);
        if !enabled {
            return Ok(default_client.clone());
        }

        let proxy_url = self
            .storage
            .settings()
            .get("proxy_url")
            .await?
            .unwrap_or_default()
            .trim()
            .to_string();
        if proxy_url.is_empty() {
            anyhow::bail!("proxy_url is empty");
        }

        let force_http1 = require_http1
            || self
                .storage
                .settings()
                .get("proxy_force_http1")
                .await?
                .as_deref()
                .map(parse_bool_setting)
                .unwrap_or(false);

        let cache_key = format!("{proxy_url}|{force_http1}");
        if let Some(cached) = self.proxy_client_cache.read().await.clone()
            && cached.cache_key == cache_key
        {
            return Ok(cached.client);
        }

        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300));
        if force_http1 {
            builder = builder.http1_only();
        }
        let client = builder.proxy(reqwest::Proxy::all(&proxy_url)?).build()?;

        *self.proxy_client_cache.write().await = Some(ProxyClientCache {
            cache_key,
            client: client.clone(),
        });
        Ok(client)
    }

    pub async fn get_ollama_capabilities_cached(
        &self,
        provider_id: &str,
        model: &str,
        ttl: Duration,
    ) -> Option<Vec<String>> {
        let key = format!("{provider_id}:{model}");
        let cache = self.ollama_capability_cache.read().await;
        cache.get(&key).and_then(|entry| {
            if entry.cached_at.elapsed() < ttl {
                Some(entry.capabilities.clone())
            } else {
                None
            }
        })
    }

    pub async fn set_ollama_capabilities_cache(
        &self,
        provider_id: &str,
        model: &str,
        capabilities: Vec<String>,
    ) {
        let key = format!("{provider_id}:{model}");
        let mut cache = self.ollama_capability_cache.write().await;
        cache.insert(
            key,
            CapabilityCacheEntry {
                capabilities,
                cached_at: Instant::now(),
            },
        );
    }

    pub async fn clear_ollama_capability_cache_for_provider(&self, provider_id: &str) {
        let prefix = format!("{provider_id}:");
        let mut cache = self.ollama_capability_cache.write().await;
        cache.retain(|k, _| !k.starts_with(&prefix));
    }
}

fn parse_bool_setting(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn to_sql_backend_config(
    config: &SqlStorageConfig,
    backend: &str,
) -> anyhow::Result<SqlBackendConfig> {
    let url = config
        .configured_url()
        .with_context(|| format!("{backend} backend selected but storage url is empty"))?;
    Ok(SqlBackendConfig {
        url,
        max_connections: config.max_connections,
        min_connections: config.min_connections,
        idle_timeout: config.idle_timeout,
        ..Default::default()
    })
}
