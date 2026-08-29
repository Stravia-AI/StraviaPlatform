use serde::{Deserialize, Serialize};

use crate::agent::{AgentDefinitionConfig, AgentDefinitionId};
use crate::media::MEDIA_DEFINITION_ID;

use super::AdminService;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaUnderstandingConfigUpdate {
    pub enabled: bool,
    pub model_id: Option<String>,
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
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaUnderstandingConfigView {
    pub enabled: bool,
    pub model_id: Option<String>,
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
        } else if record
            .config
            .model_id
            .as_ref()
            .is_some_and(|id| eligible_models.iter().any(|model| &model.id == id))
        {
            MediaUnderstandingState::Available
        } else {
            MediaUnderstandingState::Unavailable
        };
        Ok(MediaUnderstandingConfigView {
            enabled: record.config.enabled,
            model_id: record.config.model_id,
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
            if !self
                .list_eligible_media_models()
                .await?
                .iter()
                .any(|model| model.id == model_id)
            {
                return Err(MediaUnderstandingConfigError::new(
                    "MEDIA_UNDERSTANDING_MODEL_UNAVAILABLE",
                    "The selected Model is unavailable for Media Understanding",
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
            eligible.push(EligibleMediaModel {
                id: model.id,
                name: model.name,
            });
        }
        eligible.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
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
        let model = admin
            .create_model(crate::db::models::CreateModel {
                name: "Visual Route".into(),
                balance: Some("weighted".into()),
                target_provider: provider.id,
                target_model: "vision".into(),
                targets: vec![],
            })
            .await
            .expect("Model");

        let updated = admin
            .update_media_understanding_config(MediaUnderstandingConfigUpdate {
                enabled: true,
                model_id: Some(model.id.clone()),
            })
            .await
            .expect("eligible Media config");

        assert_eq!(updated.state, MediaUnderstandingState::Available);
        assert_eq!(updated.model_id.as_deref(), Some(model.id.as_str()));
        assert_eq!(
            admin
                .media_definition()
                .await
                .expect("Media Definition")
                .config
                .model_id,
            Some(model.id)
        );
    }
}
