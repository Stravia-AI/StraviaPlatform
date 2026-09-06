#[cfg(debug_assertions)]
use crate::wire_capture;
use crate::{
    admin, admission, agent, config, db, generation_chain, history_marker, hook, logging, media,
    migrations, model_turn, protocol, provider_catalog, proxy, router, storage, turn_chain,
    web_access, web_search,
};

mod builder;
mod extensions;
mod history_marker_executions;
mod lifecycle;
mod runtime;

pub use builder::GatewayBuilder;
use extensions::configure_gateway_extensions;
pub(crate) use history_marker_executions::{
    HistoryMarkerExecutionJob, StartedHistoryMarkerExecution,
};
use lifecycle::GatewayLifecycle;

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
    pub(crate) allowance_samples: admin::provider_allowance::AllowanceSampleStore,
    proxy_client_cache: Arc<tokio::sync::RwLock<Option<ProxyClientCache>>>,
    pub(crate) responses_websockets: proxy::client::ResponsesWebSocketRegistry,
    pub(crate) principal_admission: Arc<admission::PrincipalAdmission>,
    pub model_cache: Arc<tokio::sync::RwLock<router::RouteCache>>,
    pub health_registry: Arc<HealthRegistry>,
    pub(crate) cache_affinity: router::cache_affinity::CacheAffinity,
    pub(crate) route_policy_state: router::RoutePolicyState,
    pub ollama_capability_cache: Arc<tokio::sync::RwLock<HashMap<String, CapabilityCacheEntry>>>,
    pub log_tx: mpsc::Sender<LogEntry>,
    #[cfg(debug_assertions)]
    pub(crate) wire_capture: Option<wire_capture::WireCapture>,
    pub(crate) auth_sessions: Arc<tokio::sync::RwLock<HashMap<String, AuthSession>>>,
    pub(crate) agent_definitions: agent::AgentDefinitionRegistry,
    pub(crate) artifact_store: Option<Arc<dyn agent::ArtifactStore>>,
    pub(crate) media_derivatives: Option<Arc<media::MediaDerivativeStore>>,
    pub(crate) media_understanding:
        Arc<tokio::sync::RwLock<Option<media::MediaUnderstandingService>>>,
    pub(crate) media_run_snapshots: media::MediaRunSnapshotStore,
    hook_runtime: HookRuntime,
    pub(crate) mcp_registry: McpToolRegistry,
    pub(crate) history_markers: Arc<dyn history_marker::HistoryMarkerStore>,
    pub(crate) turn_chains: Arc<dyn turn_chain::TurnChainStore>,
    pub(crate) generation_chains: generation_chain::GenerationChain,
    pub(crate) model_turn: Arc<dyn model_turn::ModelTurnExecutor>,
    pub(crate) web_access_run_snapshots: web_access::WebAccessRunSnapshotStore,
    pub(crate) web_search_runner_state:
        Arc<tokio::sync::RwLock<Option<web_search::WebSearchRunner>>>,
    pub(crate) web_search_config_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) update_service: Arc<admin::updates::UpdateService>,
    pub(crate) _sqlite_pool: Option<SqlitePool>,
    pub(crate) _postgres_pool: Option<Pool<Postgres>>,
    history_marker_execution_gate: Arc<tokio::sync::RwLock<()>>,
    pub(crate) lifecycle: Arc<GatewayLifecycle>,
    lifecycle_owner: bool,
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
            allowance_samples: self.allowance_samples.clone(),
            proxy_client_cache: Arc::clone(&self.proxy_client_cache),
            principal_admission: Arc::clone(&self.principal_admission),
            responses_websockets: self.responses_websockets.clone(),
            model_cache: Arc::clone(&self.model_cache),
            health_registry: Arc::clone(&self.health_registry),
            cache_affinity: self.cache_affinity.clone(),
            route_policy_state: self.route_policy_state.clone(),
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
            update_service: Arc::clone(&self.update_service),
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
