use crate::agent::AgentRunner;
use crate::agent::LocalArtifactStore;
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use stravia_media::MediaUnderstandingService;
use stravia_media::host::{
    MediaAgentHost, MediaArtifactHost, MediaAuthorizationError, MediaHost, MediaRoute,
    MediaRuntime, MediaTarget,
};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::agent::AgentDefinitionId;
use stravia_runtime_contract::agent::AgentEventStream;
use stravia_runtime_contract::agent::AgentInput;
use stravia_runtime_contract::agent::AgentRunError;
use stravia_runtime_contract::agent::AgentTurnId;
use stravia_runtime_contract::artifact::ArtifactError;
use stravia_runtime_contract::artifact::ArtifactId;
use stravia_runtime_contract::artifact::ArtifactReader;
use stravia_runtime_contract::artifact::ArtifactRef;
use stravia_runtime_contract::artifact::ArtifactStore;

mod mcp;
pub(crate) use mcp::tools as mcp_tools;

pub(crate) struct AgentHost(pub AgentRunner);
#[async_trait]
impl MediaAgentHost for AgentHost {
    async fn definition_model_with_thinking_level(
        &self,
        id: &AgentDefinitionId,
    ) -> Option<(String, stravia_runtime_contract::thinking::ThinkingLevel)> {
        self.0.definition_model_with_thinking_level(id).await
    }
    async fn parent_artifact_ids(
        &self,
        principal: &Principal,
        parent: &AgentTurnId,
        definition: &AgentDefinitionId,
    ) -> Result<Vec<ArtifactId>, AgentRunError> {
        self.0
            .parent_artifact_ids(principal, parent, definition)
            .await
    }
    fn run(&self, input: AgentInput) -> AgentEventStream {
        self.0.run(input)
    }
}

pub(crate) struct ArtifactHost(pub Arc<LocalArtifactStore>);
#[async_trait]
impl MediaArtifactHost for ArtifactHost {
    async fn create_ready_bytes(
        &self,
        principal: &Principal,
        mime_type: &str,
        bytes: Bytes,
        retention: Duration,
    ) -> Result<ArtifactRef, ArtifactError> {
        self.0
            .create_ready_bytes(principal, mime_type, bytes, retention)
            .await
    }
    async fn open(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<ArtifactReader, ArtifactError> {
        self.0.open(principal, id).await
    }
    async fn delete_ready(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<(), ArtifactError> {
        self.0.delete_ready(principal, id).await
    }
    async fn extend_retention(
        &self,
        principal: &Principal,
        ids: &[ArtifactId],
        retention: Duration,
    ) -> Result<(), ArtifactError> {
        self.0.extend_retention(principal, ids, retention).await
    }
}

struct GatewayHost(crate::Gateway);
#[async_trait]
impl MediaHost for GatewayHost {
    async fn service(&self) -> Option<MediaUnderstandingService> {
        self.0.media_understanding.read().await.clone()
    }
    async fn match_route(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Option<MediaRoute>, MediaAuthorizationError> {
        let route = self.0.model_cache.read().await.match_model(model).cloned();
        let Some(route) = route else {
            return Ok(None);
        };
        crate::proxy::security::Security::new(self.0.storage.auth())
            .authorize_principal_model(principal, &route)
            .await
            .map_err(|error| MediaAuthorizationError {
                status: error.http_status().as_u16(),
                code: error.stable_code().into(),
                message: error.message(),
            })?;
        Ok(Some(route_metadata(&self.0, &route).await))
    }
    async fn active_route(&self, id: &str) -> Option<MediaRoute> {
        let route = self
            .0
            .storage
            .routes()
            .list_active()
            .await
            .ok()?
            .into_iter()
            .find(|route| route.id == id)?;
        Some(route_metadata(&self.0, &route).await)
    }
    async fn authorize_capability(&self, principal: &Principal) -> bool {
        crate::proxy::security::Security::new(self.0.storage.auth())
            .authorize_principal_capability(principal)
            .await
            .is_ok()
    }
    async fn transparent_injection_enabled(&self, principal: &Principal) -> bool {
        crate::proxy::security::Security::new(self.0.storage.auth())
            .media_transparent_injection_enabled(principal)
            .await
            .unwrap_or(false)
    }
}

pub(crate) async fn route_metadata(
    gateway: &crate::Gateway,
    route: &crate::db::models::Route,
) -> MediaRoute {
    let mut targets = Vec::with_capacity(route.targets.len());
    for target in &route.targets {
        let actual_model = if target.model.is_empty() || target.model == "*" {
            route.model_id.as_str()
        } else {
            target.model.as_str()
        };
        let metadata = gateway
            .storage
            .provider_models()
            .get(&target.provider_id, actual_model)
            .await
            .ok()
            .flatten()
            .map(|record| record.metadata);
        targets.push(MediaTarget {
            provider_id: target.provider_id.clone(),
            model: target.model.clone(),
            input_modalities: metadata
                .as_ref()
                .and_then(|metadata| metadata.modalities.as_ref())
                .map(|modalities| modalities.input.clone())
                .unwrap_or_default(),
            tool_call: metadata.as_ref().and_then(|metadata| metadata.tool_call),
        });
    }
    MediaRoute {
        id: route.id.clone(),
        is_enabled: route.is_enabled,
        targets,
    }
}

pub(crate) fn runtime(gateway: &crate::Gateway) -> MediaRuntime {
    MediaRuntime {
        host: Arc::new(GatewayHost(gateway.clone())),
        media_derivatives: gateway.media_derivatives.clone(),
        media_run_snapshots: gateway.media_run_snapshots.clone(),
    }
}
pub(crate) fn planning_hook(
    gateway: &crate::Gateway,
) -> Arc<dyn stravia_runtime_contract::hook::Hook> {
    stravia_media::planning_hook(&runtime(gateway))
}
pub(crate) fn platform_tools(
    gateway: &crate::Gateway,
) -> Vec<Arc<dyn stravia_runtime_contract::hook::PlatformTool>> {
    stravia_media::platform_tools(&runtime(gateway))
}
pub(crate) async fn model_is_image_capable(
    gateway: &crate::Gateway,
    model: &crate::db::models::Route,
) -> bool {
    stravia_media::platform::model_is_image_capable(&route_metadata(gateway, model).await)
}
pub(crate) fn supports_image(metadata: &crate::provider_models::ProviderModelMetadata) -> bool {
    stravia_media::supports_image(
        metadata
            .modalities
            .as_ref()
            .map(|modalities| modalities.input.as_slice())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod preprocessor_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod validator_tests;
