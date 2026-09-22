use crate::{WebSearchConfig, WebSearchError, WebSearchRunner};
use async_trait::async_trait;
use futures::Stream;
use std::{pin::Pin, sync::Arc};
use stravia_runtime_contract::{
    Principal,
    agent::{AgentEvent, AgentInput, AgentRunLimits},
};

pub trait LocalSearchHost: Send + Sync {
    fn run_ephemeral_resolved(
        &self,
        input: AgentInput,
        revision: u32,
        model_id: String,
        limits: AgentRunLimits,
    ) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;
}

pub struct SearchAccess {
    pub transparent_injection_enabled: bool,
}

#[async_trait]
pub trait PublicSearchHost: Send + Sync {
    async fn authorize(&self, principal: &Principal) -> Option<SearchAccess>;
    async fn runner_ready(&self) -> bool;
    async fn config(&self) -> Result<WebSearchConfig, WebSearchError>;
    async fn runner(&self) -> Result<WebSearchRunner, WebSearchError>;
}

#[async_trait]
pub trait SearchSettingsHost: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, WebSearchError>;
    async fn set(&self, key: &str, value: &str) -> Result<(), WebSearchError>;
}

#[derive(Clone)]
pub struct SearchProvider {
    pub id: String,
    pub name: String,
    pub is_enabled: bool,
}
#[derive(Clone)]
pub struct SearchModel {
    pub model_id: String,
    pub available: bool,
    pub tool_call: Option<bool>,
}
impl SearchModel {
    pub fn effective_available(&self) -> bool {
        self.available
    }
}

/// Opaque host publication lease held across report validation and Turn commit.
pub struct SearchPublicationGuard {
    _guard: Box<dyn Send>,
}

impl SearchPublicationGuard {
    pub fn new(guard: Box<dyn Send>) -> Self {
        Self { _guard: guard }
    }
}

pub struct ExternalSearchExecution {
    pub response: stravia_vendor_sdk::SearchResponse,
    pub publication: SearchPublicationGuard,
    pub provider_id: String,
    pub upstream_model: Option<String>,
    pub target_id: String,
}

#[async_trait]
pub trait ExternalSearchHost: Send + Sync {
    async fn execute_external_search(
        &self,
        principal: &Principal,
        route_id: &str,
        request: stravia_vendor_sdk::SearchRequest,
        cancellation: stravia_runtime_contract::CancellationToken,
        deadline: std::time::Instant,
    ) -> Result<ExternalSearchExecution, WebSearchError>;
}

#[derive(Clone)]
pub struct SearchRouteTarget {
    pub provider_id: String,
    pub model: Option<String>,
    pub enabled: bool,
}
#[derive(Clone)]
pub struct SearchRoute {
    pub id: String,
    pub model_id: String,
    pub display_name: String,
    pub is_enabled: bool,
    pub targets: Vec<SearchRouteTarget>,
}
impl SearchRoute {
    pub fn effective_display_name(&self) -> &str {
        &self.display_name
    }
}
pub struct SearchSourceProvider {
    pub id: String,
    pub search: bool,
    pub fetch: bool,
}
pub struct SearchSourceSettings {
    pub search_provider_ids: Vec<String>,
    pub fetch_provider_ids: Vec<String>,
}
pub struct SearchSources {
    pub settings: SearchSourceSettings,
    pub providers: Vec<SearchSourceProvider>,
}
#[async_trait]
pub trait SearchAdminHost: Send + Sync {
    fn settings(&self) -> Arc<dyn SearchSettingsHost>;
    fn config_lock(&self) -> Arc<tokio::sync::Mutex<()>>;
    async fn models(&self) -> Result<Vec<SearchRoute>, ()>;
    async fn providers(&self) -> Result<Vec<SearchProvider>, ()>;
    async fn provider(&self, id: &str) -> Result<Option<SearchProvider>, ()>;
    async fn provider_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<Option<SearchModel>, ()>;
    async fn validate_external_route(&self, route_id: &str) -> Result<(), ()>;
    async fn sources(&self) -> Result<Option<SearchSources>, ()>;
}
