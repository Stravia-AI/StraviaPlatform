use serde::{Deserialize, Serialize};

use stravia_runtime_contract::agent::AgentDefinitionConfig;

use stravia_runtime_contract::thinking::ThinkingLevel;

use crate::types::MediaKind;
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

/// 单个模态（图片/音频）的配置与有效状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaUnderstandingKindView {
    pub enabled: bool,
    pub model_id: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub state: MediaUnderstandingState,
    pub eligible_models: Vec<EligibleMediaModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaUnderstandingConfigView {
    pub image: MediaUnderstandingKindView,
    pub audio: MediaUnderstandingKindView,
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
        let models = self.host.models().await.map_err(|_| config_unavailable())?;
        Ok(MediaUnderstandingConfigView {
            image: self.kind_view(MediaKind::Image, &models).await?,
            audio: self.kind_view(MediaKind::Audio, &models).await?,
        })
    }

    async fn kind_view(
        &self,
        kind: MediaKind,
        models: &[MediaAdminModel],
    ) -> Result<MediaUnderstandingKindView, MediaUnderstandingConfigError> {
        let record = self
            .host
            .config(kind)
            .await
            .map_err(|_| config_unavailable())?;
        let eligible_models = eligible_models(kind, models);
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
        Ok(MediaUnderstandingKindView {
            enabled: record.enabled,
            model_id: record.model_id,
            thinking_level: record.thinking_level,
            state,
            eligible_models,
        })
    }

    pub async fn update_media_understanding_config(
        &self,
        kind: MediaKind,
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
            let models = self.host.models().await.map_err(|_| config_unavailable())?;
            let eligible_models = eligible_models(kind, &models);
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
            .patch_config(
                kind,
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
}

/// 候选模型按模态过滤：必须启用、文本输出，且每个 Target 都接受该模态输入。
fn eligible_models(kind: MediaKind, models: &[MediaAdminModel]) -> Vec<EligibleMediaModel> {
    let mut eligible = models
        .iter()
        .filter(|model| crate::platform::model_is_eligible(&model.route, kind))
        .map(|model| model.view.clone())
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| {
        left.display_name
            .cmp(&right.display_name)
            .then(left.model_id.cmp(&right.model_id))
    });
    eligible
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
    async fn config(&self, kind: MediaKind) -> Result<AgentDefinitionConfig, ()>;
    async fn patch_config(&self, kind: MediaKind, config: AgentDefinitionConfig) -> Result<(), ()>;
    async fn models(&self) -> Result<Vec<MediaAdminModel>, ()>;
}
pub struct MediaAdmin {
    host: Arc<dyn MediaAdminHost>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DisabledHost;

    #[async_trait]
    impl MediaAdminHost for DisabledHost {
        fn storage_available(&self) -> bool {
            false
        }

        async fn config(&self, _kind: MediaKind) -> Result<AgentDefinitionConfig, ()> {
            Ok(AgentDefinitionConfig::default())
        }

        async fn patch_config(
            &self,
            _kind: MediaKind,
            _config: AgentDefinitionConfig,
        ) -> Result<(), ()> {
            panic!("unexpected config patch")
        }

        async fn models(&self) -> Result<Vec<MediaAdminModel>, ()> {
            Ok(Vec::new())
        }
    }

    struct UnavailableHost;

    #[async_trait]
    impl MediaAdminHost for UnavailableHost {
        fn storage_available(&self) -> bool {
            false
        }

        async fn config(&self, _kind: MediaKind) -> Result<AgentDefinitionConfig, ()> {
            panic!("unexpected config lookup")
        }

        async fn patch_config(
            &self,
            _kind: MediaKind,
            _config: AgentDefinitionConfig,
        ) -> Result<(), ()> {
            panic!("unexpected config patch")
        }

        async fn models(&self) -> Result<Vec<MediaAdminModel>, ()> {
            panic!("unexpected models lookup")
        }
    }

    #[tokio::test]
    async fn media_config_defaults_to_disabled() {
        let config = MediaAdmin::new(Arc::new(DisabledHost))
            .get_media_understanding_config()
            .await
            .expect("Media Understanding config");

        assert_eq!(config.image.state, MediaUnderstandingState::Disabled);
        assert_eq!(config.audio.state, MediaUnderstandingState::Disabled);
    }

    #[tokio::test]
    async fn enabling_media_requires_runtime_storage() {
        let error = MediaAdmin::new(Arc::new(UnavailableHost))
            .update_media_understanding_config(
                MediaKind::Audio,
                MediaUnderstandingConfigUpdate {
                    enabled: true,
                    model_id: None,
                    thinking_level: None,
                },
            )
            .await
            .expect_err("Media runtime storage should be required");

        assert_eq!(error.code, "MEDIA_UNDERSTANDING_CONFIG_UNAVAILABLE");
    }

    fn admin_model(id: &str, text_output: bool, input_modalities: &[&str]) -> MediaAdminModel {
        MediaAdminModel {
            route: crate::host::MediaRoute {
                id: id.into(),
                is_enabled: true,
                text_output,
                targets: vec![crate::host::MediaTarget {
                    provider_id: "provider".into(),
                    model: id.into(),
                    input_modalities: input_modalities.iter().map(|m| (*m).into()).collect(),
                }],
            },
            view: EligibleMediaModel {
                id: id.into(),
                model_id: id.into(),
                display_name: id.into(),
                supported_thinking_levels: vec![ThinkingLevel::Medium],
            },
        }
    }

    #[test]
    fn candidates_require_the_kinds_input_and_text_output() {
        let models = vec![
            admin_model("vision-chat", true, &["text", "image"]),
            admin_model("image-generator", false, &["text", "image"]),
            admin_model("audio-chat", true, &["text", "audio"]),
            admin_model("omni", true, &["text", "image", "audio"]),
            admin_model("text-only", true, &["text"]),
        ];
        let ids = |kind| {
            eligible_models(kind, &models)
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>()
        };

        assert_eq!(ids(MediaKind::Image), ["omni", "vision-chat"]);
        assert_eq!(ids(MediaKind::Audio), ["audio-chat", "omni"]);
    }
}
