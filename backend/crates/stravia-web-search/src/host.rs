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
    pub channel: Option<String>,
    pub auth_mode: String,
    pub protocol: String,
    pub function_calling: bool,
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
pub struct CodexTransport {
    pub client: reqwest::Client,
    pub access_token: String,
    pub extra_headers: std::collections::HashMap<String, String>,
    pub endpoint: String,
}
#[async_trait]
pub trait CodexSession: Send + Sync {
    fn provider(&self) -> &SearchProvider;
    async fn model(&self, model_id: &str) -> Result<Option<SearchModel>, WebSearchError>;
    async fn transport(&self) -> Result<CodexTransport, WebSearchError>;
}
#[async_trait]
pub trait CodexHost: Send + Sync {
    async fn provider(
        &self,
        provider_id: &str,
    ) -> Result<Option<Arc<dyn CodexSession>>, WebSearchError>;
}

#[derive(Clone)]
pub struct SearchRouteTarget {
    pub provider_id: String,
    pub model: String,
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
pub struct SearchCredential {
    pub connected: bool,
    pub has_access_token: bool,
    pub expiry_valid: bool,
    pub has_refresh_token: bool,
}
pub struct SearchSourceProvider {
    pub id: String,
    pub kind: String,
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
    async fn credential(&self, id: &str) -> Result<Option<SearchCredential>, ()>;
    async fn models_for_provider(&self, id: &str) -> Result<Vec<SearchModel>, ()>;
    async fn provider_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<Option<SearchModel>, ()>;
    async fn sources(&self) -> Result<Option<SearchSources>, ()>;
}
