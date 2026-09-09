use serde::{Deserialize, Serialize};

use stravia_runtime_contract::agent::AgentDefinitionConfig;

use stravia_runtime_contract::thinking::ThinkingLevel;

use async_trait::async_trait;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaUnderstandingConfigUpdate {
    pub enabled: bool,
    pub model_id: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaUnderstandingState {
    Disabled,
    Unavailable,
    Available,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EligibleMediaModel {
    pub id: String,
    pub model_id: String,
    pub display_name: String,
    pub supported_thinking_levels: Vec<ThinkingLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaUnderstandingConfigView {
    pub enabled: bool,
    pub model_id: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub state: MediaUnderstandingState,
    pub eligible_models: Vec<EligibleMediaModel>,
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct MediaUnderstandingConfigError {
    pub code: &'static str,
    pub message: String,
}

impl MediaUnderstandingConfigError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl MediaAdmin {
    pub fn new(host: Arc<dyn MediaAdminHost>) -> Self {
        Self { host }
    }
    pub async fn get_media_understanding_config(
        &self,
    ) -> Result<MediaUnderstandingConfigView, MediaUnderstandingConfigError> {
        let record = self.media_definition().await?;
        let eligible_models = self.list_eligible_media_models().await?;
        let state = if !record.enabled {
            MediaUnderstandingState::Disabled
        } else if !self.host.storage_available() {
            MediaUnderstandingState::Unavailable
        } else if record.model_id.as_ref().is_some_and(|id| {
            eligible_models.iter().any(|model| {
                &model.id == id
                    && record
                        .thinking_level
                        .is_some_and(|level| model.supported_thinking_levels.contains(&level))
            })
        }) {
            MediaUnderstandingState::Available
        } else {
            MediaUnderstandingState::Unavailable
        };
        Ok(MediaUnderstandingConfigView {
            enabled: record.enabled,
            model_id: record.model_id,
            thinking_level: record.thinking_level,
            state,
            eligible_models,
        })
    }

    pub async fn update_media_understanding_config(
        &self,
        update: MediaUnderstandingConfigUpdate,
    ) -> Result<MediaUnderstandingConfigView, MediaUnderstandingConfigError> {
        if update.enabled && !self.host.storage_available() {
            return Err(MediaUnderstandingConfigError::new(
                "MEDIA_UNDERSTANDING_CONFIG_UNAVAILABLE",
                "Media Understanding runtime storage is unavailable",
            ));
        }
        if update.enabled {
            let model_id = update.model_id.as_deref().ok_or_else(|| {
                MediaUnderstandingConfigError::new(
                    "MEDIA_UNDERSTANDING_MODEL_REQUIRED",
                    "Media Understanding requires a logical Model",
                )
            })?;
            let eligible_models = self.list_eligible_media_models().await?;
            let model = eligible_models
                .iter()
                .find(|model| model.id == model_id)
                .ok_or_else(|| {
                    MediaUnderstandingConfigError::new(
                        "MEDIA_UNDERSTANDING_MODEL_UNAVAILABLE",
                        "The selected Model is unavailable for Media Understanding",
                    )
                })?;
            let thinking_level = update.thinking_level.ok_or_else(|| {
                MediaUnderstandingConfigError::new(
                    "MEDIA_UNDERSTANDING_THINKING_LEVEL_REQUIRED",
                    "Media Understanding requires a Thinking Level",
                )
            })?;
            if !model.supported_thinking_levels.contains(&thinking_level) {
                return Err(MediaUnderstandingConfigError::new(
                    "MEDIA_UNDERSTANDING_THINKING_LEVEL_UNAVAILABLE",
                    "The selected Thinking Level is unavailable on the selected Model",
                ));
            }
        }
        self.host
            .patch_config(AgentDefinitionConfig {
                enabled: update.enabled,
                model_id: update.model_id,
                thinking_level: update.thinking_level,
            })
            .await
            .map_err(|_| {
                MediaUnderstandingConfigError::new(
                    "MEDIA_UNDERSTANDING_CONFIG_UNAVAILABLE",
                    "Media Understanding configuration could not be saved",
                )
            })?;
        self.get_media_understanding_config().await
    }

    async fn list_eligible_media_models(
        &self,
    ) -> Result<Vec<EligibleMediaModel>, MediaUnderstandingConfigError> {
        let mut eligible = Vec::new();
        for model in self.host.models().await.map_err(|_| config_unavailable())? {
            if crate::platform::model_is_image_capable(&model.route) {
                eligible.push(model.view);
            }
        }
        eligible.sort_by(|left, right| {
            left.display_name
                .cmp(&right.display_name)
                .then(left.model_id.cmp(&right.model_id))
        });
        Ok(eligible)
    }

    async fn media_definition(
        &self,
    ) -> Result<AgentDefinitionConfig, MediaUnderstandingConfigError> {
        self.host.config().await.map_err(|_| config_unavailable())
    }
}

fn config_unavailable() -> MediaUnderstandingConfigError {
    MediaUnderstandingConfigError::new(
        "MEDIA_UNDERSTANDING_CONFIG_UNAVAILABLE",
        "Media Understanding configuration is unavailable",
    )
}

pub struct MediaAdminModel {
    pub route: crate::host::MediaRoute,
    pub view: EligibleMediaModel,
}
#[async_trait]
pub trait MediaAdminHost: Send + Sync {
    fn storage_available(&self) -> bool;
    async fn config(&self) -> Result<AgentDefinitionConfig, ()>;
    async fn patch_config(&self, config: AgentDefinitionConfig) -> Result<(), ()>;
    async fn models(&self) -> Result<Vec<MediaAdminModel>, ()>;
}
pub struct MediaAdmin {
    host: Arc<dyn MediaAdminHost>,
}
