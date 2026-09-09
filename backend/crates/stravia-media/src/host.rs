use crate::{MediaDerivativeStore, MediaRunSnapshotStore, MediaUnderstandingService};
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use stravia_runtime_contract::agent::{
    AgentDefinitionId, AgentEventStream, AgentInput, AgentRunError, AgentTurnId,
};
use stravia_runtime_contract::artifact::{ArtifactError, ArtifactId, ArtifactReader, ArtifactRef};
use stravia_runtime_contract::{Principal, thinking::ThinkingLevel};

#[async_trait]
pub trait MediaAgentHost: Send + Sync {
    async fn definition_model_with_thinking_level(
        &self,
        id: &AgentDefinitionId,
    ) -> Option<(String, ThinkingLevel)>;
    async fn parent_artifact_ids(
        &self,
        principal: &Principal,
        parent: &AgentTurnId,
        definition: &AgentDefinitionId,
    ) -> Result<Vec<ArtifactId>, AgentRunError>;
    fn run(&self, input: AgentInput) -> AgentEventStream;
}

#[async_trait]
pub trait MediaArtifactHost: Send + Sync {
    async fn create_ready_bytes(
        &self,
        principal: &Principal,
        mime_type: &str,
        bytes: Bytes,
        retention: Duration,
    ) -> Result<ArtifactRef, ArtifactError>;
    async fn open(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<ArtifactReader, ArtifactError>;
    async fn delete_ready(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<(), ArtifactError>;
    async fn extend_retention(
        &self,
        principal: &Principal,
        ids: &[ArtifactId],
        retention: Duration,
    ) -> Result<(), ArtifactError>;
}

#[derive(Clone)]
pub struct MediaTarget {
    pub provider_id: String,
    pub model: String,
    pub input_modalities: Vec<String>,
    pub tool_call: Option<bool>,
}
#[derive(Clone)]
pub struct MediaRoute {
    pub id: String,
    pub is_enabled: bool,
    pub targets: Vec<MediaTarget>,
}
#[derive(Debug)]
pub struct MediaAuthorizationError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

#[async_trait]
pub trait MediaHost: Send + Sync {
    async fn service(&self) -> Option<MediaUnderstandingService>;
    async fn match_route(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Option<MediaRoute>, MediaAuthorizationError>;
    async fn active_route(&self, id: &str) -> Option<MediaRoute>;
    async fn authorize_capability(&self, principal: &Principal) -> bool;
    async fn transparent_injection_enabled(&self, principal: &Principal) -> bool;
}

#[derive(Clone)]
pub struct MediaRuntime {
    pub host: Arc<dyn MediaHost>,
    pub media_derivatives: Option<Arc<MediaDerivativeStore>>,
    pub media_run_snapshots: MediaRunSnapshotStore,
}
