use serde::{Deserialize, Serialize};

use crate::agent::{AgentDefinitionConfig, AgentDefinitionId};
use crate::media::MEDIA_DEFINITION_ID;
use crate::thinking::ThinkingLevel;

use super::AdminService;

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

impl AdminService {
    pub async fn get_media_understanding_config(
        &self,
    ) -> Result<MediaUnderstandingConfigView, MediaUnderstandingConfigError> {
        let record = self.media_definition().await?;
        let eligible_models = self.list_eligible_media_models().await?;
        let state = if !record.config.enabled {
            MediaUnderstandingState::Disabled
        } else if self.gw.media_derivatives.is_none() {
            MediaUnderstandingState::Unavailable
        } else if record.config.model_id.as_ref().is_some_and(|id| {
            eligible_models.iter().any(|model| {
                &model.id == id
                    && record
                        .config
                        .thinking_level
                        .is_some_and(|level| model.supported_thinking_levels.contains(&level))
            })
        }) {
            MediaUnderstandingState::Available
        } else {
            MediaUnderstandingState::Unavailable
        };
        Ok(MediaUnderstandingConfigView {
            enabled: record.config.enabled,
            model_id: record.config.model_id,
            thinking_level: record.config.thinking_level,
            state,
            eligible_models,
        })
    }

    pub async fn update_media_understanding_config(
        &self,
        update: MediaUnderstandingConfigUpdate,
    ) -> Result<MediaUnderstandingConfigView, MediaUnderstandingConfigError> {
        if update.enabled && self.gw.media_derivatives.is_none() {
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
        self.gw
            .agent_definitions
            .patch_config(
                &AgentDefinitionId::new(MEDIA_DEFINITION_ID),
                AgentDefinitionConfig {
                    enabled: update.enabled,
                    model_id: update.model_id,
                    thinking_level: update.thinking_level,
                },
            )
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
        for model in self.list_models().await.map_err(|_| config_unavailable())? {
            if !crate::media::model_is_image_capable(&self.gw, &model).await {
                continue;
            }
            let display_name = model.effective_display_name().to_string();
            eligible.push(EligibleMediaModel {
                id: model.id,
                model_id: model.model_id,
                display_name,
                supported_thinking_levels: model.supported_thinking_levels.0,
            });
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
    ) -> Result<crate::agent::AgentDefinitionRecord, MediaUnderstandingConfigError> {
        self.gw
            .agent_definitions
            .get_current(&AgentDefinitionId::new(MEDIA_DEFINITION_ID))
            .await
            .map_err(|_| config_unavailable())
    }
}

fn config_unavailable() -> MediaUnderstandingConfigError {
    MediaUnderstandingConfigError::new(
        "MEDIA_UNDERSTANDING_CONFIG_UNAVAILABLE",
        "Media Understanding configuration is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn media_config_defaults_to_disabled_with_read_only_contract() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .build()
        .await
        .expect("Gateway");

        let config = gateway
            .admin()
            .get_media_understanding_config()
            .await
            .expect("Media Understanding config");

        assert_eq!(config.state, MediaUnderstandingState::Disabled);
        assert!(!config.enabled);
        assert!(config.model_id.is_none());
        assert!(config.thinking_level.is_none());
        assert!(config.eligible_models.is_empty());
    }
    #[tokio::test]
    async fn enabling_media_rejects_a_gateway_without_runtime_storage() {
        let storage = std::sync::Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let (gateway, _logs) =
            crate::Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
                .await
                .expect("Gateway");

        let error = gateway
            .admin()
            .update_media_understanding_config(MediaUnderstandingConfigUpdate {
                enabled: true,
                model_id: None,
                thinking_level: None,
            })
            .await
            .expect_err("Media runtime storage should be required");

        assert_eq!(error.code, "MEDIA_UNDERSTANDING_CONFIG_UNAVAILABLE");
    }

    #[tokio::test]
    async fn enabling_media_requires_and_persists_an_explicit_image_model() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .build()
        .await
        .expect("Gateway");
        let admin = gateway.admin();
        let provider = admin
            .gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "Vision Provider".into(),
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: "https://example.com/v1".into(),
                preset_key: None,
                channel: None,
                models_source: None,
                static_models: None,
                api_key: "sk-test".into(),
                adapter_credentials: r#"{"apiKey":"sk-test"}"#.into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
            })
            .await
            .expect("Provider");
        admin
            .create_manual_provider_model(
                &provider.id,
                "vision",
                crate::provider_models::CreateManualProviderModel {
                    metadata: serde_json::json!({
                        "id": "vision",
                        "modalities": { "input": ["text", "image"], "output": ["text"] }
                    }),
                },
            )
            .await
            .expect("Provider Model");
        admin
            .create_manual_provider_model(
                &provider.id,
                "text",
                crate::provider_models::CreateManualProviderModel {
                    metadata: serde_json::json!({
                        "id": "text",
                        "modalities": { "input": ["text"], "output": ["text"] }
                    }),
                },
            )
            .await
            .expect("text-only Provider Model");
        let model = admin
            .create_model(crate::db::models::CreateRoute {
                model_id: "Visual Route".into(),
                display_name: None,
                balance: Some("traffic_equalization".into()),
                target_provider: provider.id.clone(),
                target_model: "vision".into(),
                targets: vec![],
            })
            .await
            .expect("Model");
        let mixed_model = admin
            .create_model(crate::db::models::CreateRoute {
                model_id: "Mixed Route".into(),
                display_name: None,
                balance: Some("traffic_equalization".into()),
                target_provider: String::new(),
                target_model: String::new(),
                targets: vec![
                    crate::db::models::CreateTarget {
                        provider_id: provider.id.clone(),
                        model: "vision".into(),
                        priority: Some(1),
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    },
                    crate::db::models::CreateTarget {
                        provider_id: provider.id.clone(),
                        model: "text".into(),
                        priority: Some(2),
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    },
                ],
            })
            .await
            .expect("mixed Model");

        let before_update = admin
            .get_media_understanding_config()
            .await
            .expect("Media config");
        let eligible = before_update
            .eligible_models
            .iter()
            .find(|candidate| candidate.id == model.id)
            .expect("all-image Model should be eligible");
        assert!(
            eligible
                .supported_thinking_levels
                .contains(&ThinkingLevel::Medium)
        );
        assert!(
            !before_update
                .eligible_models
                .iter()
                .any(|candidate| candidate.id == mixed_model.id)
        );

        let unsupported_error = admin
            .update_media_understanding_config(MediaUnderstandingConfigUpdate {
                enabled: true,
                model_id: Some(model.id.clone()),
                thinking_level: Some(ThinkingLevel::Max),
            })
            .await
            .expect_err("hidden Thinking Level should be rejected");
        assert_eq!(
            unsupported_error.code,
            "MEDIA_UNDERSTANDING_THINKING_LEVEL_UNAVAILABLE"
        );

        let updated = admin
            .update_media_understanding_config(MediaUnderstandingConfigUpdate {
                enabled: true,
                model_id: Some(model.id.clone()),
                thinking_level: Some(ThinkingLevel::Medium),
            })
            .await
            .expect("eligible Media config");

        assert_eq!(updated.state, MediaUnderstandingState::Available);
        assert_eq!(updated.model_id.as_deref(), Some(model.id.as_str()));
        assert_eq!(updated.thinking_level, Some(ThinkingLevel::Medium));
        let persisted = admin
            .media_definition()
            .await
            .expect("Media Definition")
            .config;
        assert_eq!(
            (persisted.model_id, persisted.thinking_level),
            (Some(model.id), Some(ThinkingLevel::Medium))
        );
    }
}
