use super::AdminService;
use async_trait::async_trait;
use std::sync::Arc;
use stravia_media::MEDIA_DEFINITION_ID;
use stravia_media::admin::{
    EligibleMediaModel, MediaUnderstandingConfigError, MediaUnderstandingConfigUpdate,
    MediaUnderstandingConfigView,
};
use stravia_media::admin::{MediaAdmin, MediaAdminHost, MediaAdminModel};
use stravia_runtime_contract::agent::AgentDefinitionConfig;
use stravia_runtime_contract::agent::AgentDefinitionId;

struct AdminHost(AdminService);
#[async_trait]
impl MediaAdminHost for AdminHost {
    fn storage_available(&self) -> bool {
        self.0.gw.media_derivatives.is_some()
    }
    async fn config(&self) -> Result<AgentDefinitionConfig, ()> {
        self.0
            .gw
            .agent_definitions
            .get_current(&AgentDefinitionId::new(MEDIA_DEFINITION_ID))
            .await
            .map(|record| record.config)
            .map_err(|_| ())
    }
    async fn patch_config(&self, config: AgentDefinitionConfig) -> Result<(), ()> {
        self.0
            .gw
            .agent_definitions
            .patch_config(&AgentDefinitionId::new(MEDIA_DEFINITION_ID), config)
            .await
            .map(|_| ())
            .map_err(|_| ())
    }
    async fn models(&self) -> Result<Vec<MediaAdminModel>, ()> {
        let mut result = Vec::new();
        for model in self.0.list_models().await.map_err(|_| ())? {
            let route = crate::media::route_metadata(&self.0.gw, &model).await;
            let display_name = model.effective_display_name().to_string();
            result.push(MediaAdminModel {
                route,
                view: EligibleMediaModel {
                    id: model.id,
                    model_id: model.model_id,
                    display_name,
                    supported_thinking_levels: model.supported_thinking_levels.0,
                },
            });
        }
        Ok(result)
    }
}
impl AdminService {
    pub async fn get_media_understanding_config(
        &self,
    ) -> Result<MediaUnderstandingConfigView, MediaUnderstandingConfigError> {
        MediaAdmin::new(Arc::new(AdminHost(self.clone())))
            .get_media_understanding_config()
            .await
    }
    pub async fn update_media_understanding_config(
        &self,
        update: MediaUnderstandingConfigUpdate,
    ) -> Result<MediaUnderstandingConfigView, MediaUnderstandingConfigError> {
        MediaAdmin::new(Arc::new(AdminHost(self.clone())))
            .update_media_understanding_config(update)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_media::admin::MediaUnderstandingState;
    use stravia_runtime_contract::thinking::ThinkingLevel;

    #[tokio::test]
    async fn media_config_defaults_to_disabled_with_read_only_contract() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
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
        let directory = tempfile::tempdir().expect("temporary directory");
        let storage = std::sync::Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let gateway = crate::Gateway::from_storage(
            crate::config::GatewayConfig {
                data_dir: directory.path().to_path_buf(),
                ..Default::default()
            },
            storage,
        )
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
        let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
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
                        enabled: true,
                        priority: Some(1),
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    },
                    crate::db::models::CreateTarget {
                        provider_id: provider.id.clone(),
                        model: "text".into(),
                        enabled: true,
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
            .get_media_understanding_config()
            .await
            .expect("persisted Media configuration");
        assert_eq!(
            (persisted.model_id, persisted.thinking_level),
            (Some(model.id), Some(ThinkingLevel::Medium))
        );
    }
}
