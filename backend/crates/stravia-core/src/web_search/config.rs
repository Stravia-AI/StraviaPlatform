#[cfg(test)]
use std::sync::Arc;

use async_trait::async_trait;
#[cfg(test)]
use tokio::sync::RwLock;

use super::{ResolvedWebSearchBackend, WebSearchBackendDraft, WebSearchConfig, WebSearchError};

pub const WEB_SEARCH_CONFIG_KEY: &str = "web_search_config";
pub const MIN_SEARCH_TURNS: u32 = 2;
pub const MAX_SEARCH_TURNS: u32 = 20;
pub const MIN_SEARCH_SECONDS: u64 = 60;
pub const MAX_SEARCH_SECONDS: u64 = 900;

#[async_trait]
pub(crate) trait WebSearchConfigStore: Send + Sync {
    async fn load(&self) -> Result<WebSearchConfig, WebSearchError>;
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct MemoryWebSearchConfigStore {
    config: Arc<RwLock<WebSearchConfig>>,
}

#[cfg(test)]
impl MemoryWebSearchConfigStore {
    pub(crate) fn new(config: WebSearchConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
        }
    }

    #[cfg(test)]
    pub(crate) async fn replace(&self, config: WebSearchConfig) {
        *self.config.write().await = config;
    }
}

#[cfg(test)]
#[async_trait]
impl WebSearchConfigStore for MemoryWebSearchConfigStore {
    async fn load(&self) -> Result<WebSearchConfig, WebSearchError> {
        Ok(self.config.read().await.clone())
    }
}

#[derive(Clone)]
pub(crate) struct SettingsWebSearchConfigStore {
    storage: crate::storage::DynStorage,
}

impl SettingsWebSearchConfigStore {
    pub(crate) fn new(storage: crate::storage::DynStorage) -> Self {
        Self { storage }
    }

    pub(crate) async fn save(&self, config: &WebSearchConfig) -> Result<(), WebSearchError> {
        let value = serde_json::to_string(config).map_err(|_| {
            WebSearchError::new(
                "config_unavailable",
                "Web Search configuration could not be encoded",
            )
        })?;
        self.storage
            .settings()
            .set(WEB_SEARCH_CONFIG_KEY, &value)
            .await
            .map_err(|_| {
                WebSearchError::new(
                    "config_unavailable",
                    "Web Search configuration could not be saved",
                )
            })
    }
}

#[async_trait]
impl WebSearchConfigStore for SettingsWebSearchConfigStore {
    async fn load(&self) -> Result<WebSearchConfig, WebSearchError> {
        let Some(value) = self
            .storage
            .settings()
            .get(WEB_SEARCH_CONFIG_KEY)
            .await
            .map_err(|_| {
                WebSearchError::new(
                    "config_unavailable",
                    "Web Search configuration is unavailable",
                )
            })?
        else {
            return Ok(WebSearchConfig::default());
        };
        serde_json::from_str(&value).map_err(|_| {
            WebSearchError::new(
                "config_unavailable",
                "Web Search configuration is unavailable",
            )
        })
    }
}

pub(crate) fn resolve_enabled_config(
    config: &WebSearchConfig,
) -> Result<ResolvedWebSearchBackend, WebSearchError> {
    if !config.enabled {
        return Err(WebSearchError::new("disabled", "Web Search is disabled"));
    }
    match config.backend.as_ref() {
        Some(WebSearchBackendDraft::Local {
            model_id: Some(model_id),
        }) if !model_id.trim().is_empty() => {
            if !(MIN_SEARCH_TURNS..=MAX_SEARCH_TURNS).contains(&config.max_turns)
                || !(MIN_SEARCH_SECONDS..=MAX_SEARCH_SECONDS).contains(&config.total_time_seconds)
            {
                return Err(WebSearchError::new(
                    "invalid_config",
                    "Web Search limits are outside the supported range",
                ));
            }
            Ok(ResolvedWebSearchBackend::Local {
                model_id: model_id.clone(),
            })
        }
        Some(WebSearchBackendDraft::Codex {
            provider_id: Some(provider_id),
            upstream_model: Some(upstream_model),
        }) if !provider_id.trim().is_empty() && !upstream_model.trim().is_empty() => {
            Ok(ResolvedWebSearchBackend::Codex {
                provider_id: provider_id.clone(),
                upstream_model: upstream_model.clone(),
            })
        }
        _ => Err(WebSearchError::new(
            "invalid_config",
            "Enabled Web Search requires a complete backend binding",
        )),
    }
}
