use serde::Serialize;

use crate::host::{SearchAdminHost, SearchModel as ProviderModelRecord};
use std::sync::Arc;

use crate::WebSearchBackendDraft;
use crate::{
    MAX_SEARCH_SECONDS, MAX_SEARCH_TURNS, MIN_SEARCH_SECONDS, MIN_SEARCH_TURNS,
    ResolvedWebSearchBackend, SettingsWebSearchConfigStore, WebSearchConfig, WebSearchConfigStore,
    codex_provider_contract, resolve_enabled_config,
};

pub struct SearchAdmin {
    host: Arc<dyn SearchAdminHost>,
}
impl SearchAdmin {
    pub fn new(host: Arc<dyn SearchAdminHost>) -> Self {
        Self { host }
    }
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct WebSearchConfigError {
    pub code: &'static str,
    pub message: String,
}

impl WebSearchConfigError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EligibleSearchModel {
    pub id: String,
    pub model_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompatibleCodexModel {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompatibleCodexProvider {
    pub id: String,
    pub name: String,
    pub models: Vec<CompatibleCodexModel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WebSearchLimits {
    pub min_turns: u32,
    pub max_turns: u32,
    pub min_total_time_seconds: u64,
    pub max_total_time_seconds: u64,
}

impl Default for WebSearchLimits {
    fn default() -> Self {
        Self {
            min_turns: MIN_SEARCH_TURNS,
            max_turns: MAX_SEARCH_TURNS,
            min_total_time_seconds: MIN_SEARCH_SECONDS,
            max_total_time_seconds: MAX_SEARCH_SECONDS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WebSearchConfigView {
    #[serde(flatten)]
    pub config: WebSearchConfig,
    pub limits: WebSearchLimits,
}

impl std::ops::Deref for WebSearchConfigView {
    type Target = WebSearchConfig;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl From<WebSearchConfig> for WebSearchConfigView {
    fn from(config: WebSearchConfig) -> Self {
        Self {
            config,
            limits: WebSearchLimits::default(),
        }
    }
}

impl SearchAdmin {
    pub async fn get_web_search_config(&self) -> Result<WebSearchConfigView, WebSearchConfigError> {
        SettingsWebSearchConfigStore::new(self.host.settings())
            .load()
            .await
            .map(WebSearchConfigView::from)
            .map_err(|_| {
                WebSearchConfigError::new(
                    "WEB_SEARCH_CONFIG_UNAVAILABLE",
                    "Web Search configuration is unavailable",
                )
            })
    }

    pub async fn list_eligible_web_search_models(
        &self,
    ) -> Result<Vec<EligibleSearchModel>, WebSearchConfigError> {
        let models = self.host.models().await.map_err(|_| {
            WebSearchConfigError::new(
                "WEB_SEARCH_MODEL_INELIGIBLE",
                "Local Search Model eligibility is unavailable",
            )
        })?;
        let mut eligible = Vec::new();
        for model in models.into_iter().filter(|model| model.is_enabled) {
            if self.validate_local_targets(&model).await.is_ok() {
                let display_name = model.effective_display_name().to_string();
                eligible.push(EligibleSearchModel {
                    id: model.id,
                    model_id: model.model_id,
                    display_name,
                });
            }
        }
        eligible.sort_by(|left, right| {
            left.display_name
                .cmp(&right.display_name)
                .then(left.model_id.cmp(&right.model_id))
        });
        Ok(eligible)
    }

    pub async fn list_compatible_codex_search_providers(
        &self,
    ) -> Result<Vec<CompatibleCodexProvider>, WebSearchConfigError> {
        let providers = self
            .host
            .providers()
            .await
            .map_err(|_| invalid_codex_provider())?;
        let mut compatible = Vec::new();
        for provider in providers {
            if !codex_provider_contract(&provider) {
                continue;
            }
            let credential = self
                .host
                .credential(&provider.id)
                .await
                .map_err(|_| invalid_codex_provider())?;
            if !credential.as_ref().is_some_and(effective_oauth_credential) {
                continue;
            }
            let mut models = self
                .host
                .models_for_provider(&provider.id)
                .await
                .map_err(|_| missing_codex_model())?
                .into_iter()
                .filter(ProviderModelRecord::effective_available)
                .map(|model| CompatibleCodexModel { id: model.model_id })
                .collect::<Vec<_>>();
            models.sort_by(|left, right| left.id.cmp(&right.id));
            compatible.push(CompatibleCodexProvider {
                id: provider.id,
                name: provider.name,
                models,
            });
        }
        compatible.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(compatible)
    }

    pub async fn update_web_search_config(
        &self,
        mut config: WebSearchConfig,
    ) -> Result<WebSearchConfigView, WebSearchConfigError> {
        let lock = self.host.config_lock();
        let _guard = lock.lock().await;
        if matches!(
            config.backend.as_ref(),
            Some(WebSearchBackendDraft::Local { .. })
        ) {
            validate_limits(&config)?;
        }
        if config.enabled {
            self.validate_enabled_web_search(&config).await?;
        }
        let store = SettingsWebSearchConfigStore::new(self.host.settings());
        let current = store.load().await.map_err(|_| {
            WebSearchConfigError::new(
                "WEB_SEARCH_CONFIG_UNAVAILABLE",
                "Web Search configuration is unavailable",
            )
        })?;
        config.revision = current.revision.saturating_add(1);
        config.updated_at = chrono::Utc::now().to_rfc3339();
        store.save(&config).await.map_err(|_| {
            WebSearchConfigError::new(
                "WEB_SEARCH_CONFIG_UNAVAILABLE",
                "Web Search configuration could not be saved",
            )
        })?;
        Ok(config.into())
    }

    async fn validate_enabled_web_search(
        &self,
        config: &WebSearchConfig,
    ) -> Result<(), WebSearchConfigError> {
        match resolve_enabled_config(config).map_err(|error| {
            WebSearchConfigError::new("WEB_SEARCH_INVALID_CONFIG", error.message)
        })? {
            ResolvedWebSearchBackend::Local { model_id } => {
                self.validate_local_binding(&model_id).await?;
                self.validate_local_sources().await
            }
            ResolvedWebSearchBackend::Codex {
                provider_id,
                upstream_model,
            } => {
                self.validate_codex_binding(&provider_id, &upstream_model)
                    .await
            }
        }
    }

    async fn validate_local_binding(&self, model_id: &str) -> Result<(), WebSearchConfigError> {
        let model = self
            .host
            .models()
            .await
            .map_err(|_| {
                WebSearchConfigError::new(
                    "WEB_SEARCH_MODEL_INELIGIBLE",
                    "Local Search Model eligibility is unavailable",
                )
            })?
            .into_iter()
            .find(|model| model.id == model_id && model.is_enabled)
            .ok_or_else(|| {
                WebSearchConfigError::new(
                    "WEB_SEARCH_MODEL_INELIGIBLE",
                    "Local Search Model is unavailable",
                )
            })?;
        self.validate_local_targets(&model).await
    }

    async fn validate_local_targets(
        &self,
        model: &crate::host::SearchRoute,
    ) -> Result<(), WebSearchConfigError> {
        for target in &model.targets {
            let Some(provider) = self.host.provider(&target.provider_id).await.map_err(|_| {
                WebSearchConfigError::new(
                    "WEB_SEARCH_MODEL_INELIGIBLE",
                    "Local Search Model eligibility is unavailable",
                )
            })?
            else {
                continue;
            };
            if !provider.is_enabled || !provider.function_calling {
                continue;
            }
            let provider_model = self
                .host
                .provider_model(&target.provider_id, &target.model)
                .await
                .map_err(|_| {
                    WebSearchConfigError::new(
                        "WEB_SEARCH_MODEL_INELIGIBLE",
                        "Local Search Model eligibility is unavailable",
                    )
                })?;
            if provider_model.as_ref().is_some_and(eligible_provider_model) {
                return Ok(());
            }
        }
        Err(WebSearchConfigError::new(
            "WEB_SEARCH_MODEL_INELIGIBLE",
            "Local Search Model has no eligible Target",
        ))
    }

    pub async fn validate_local_sources(&self) -> Result<(), WebSearchConfigError> {
        let sources = self
            .host
            .sources()
            .await
            .map_err(|_| sources_unavailable())?
            .ok_or_else(sources_unavailable)?;
        let settings = sources.settings;
        let providers = sources.providers;
        let has_search = settings.search_provider_ids.iter().any(|id| {
            providers
                .iter()
                .any(|provider| provider.id == *id && provider.kind != "codex" && provider.search)
        });
        let has_fetch = settings.fetch_provider_ids.iter().any(|id| {
            providers
                .iter()
                .any(|provider| provider.id == *id && provider.kind != "codex" && provider.fetch)
        });
        if has_search && has_fetch {
            Ok(())
        } else {
            Err(sources_unavailable())
        }
    }

    async fn validate_codex_binding(
        &self,
        provider_id: &str,
        upstream_model: &str,
    ) -> Result<(), WebSearchConfigError> {
        let provider = self
            .host
            .provider(provider_id)
            .await
            .map_err(|_| invalid_codex_provider())?
            .ok_or_else(invalid_codex_provider)?;
        if !codex_provider_contract(&provider) {
            return Err(invalid_codex_provider());
        }
        self.host
            .credential(provider_id)
            .await
            .map_err(|_| invalid_codex_provider())?
            .filter(effective_oauth_credential)
            .ok_or_else(invalid_codex_provider)?;
        let model = self
            .host
            .provider_model(provider_id, upstream_model)
            .await
            .map_err(|_| missing_codex_model())?
            .filter(|model| model.effective_available())
            .ok_or_else(missing_codex_model)?;
        if model.model_id != upstream_model {
            return Err(missing_codex_model());
        }
        Ok(())
    }
}

fn effective_oauth_credential(credential: &crate::host::SearchCredential) -> bool {
    credential.connected
        && credential.has_access_token
        && (credential.expiry_valid || credential.has_refresh_token)
}

fn eligible_provider_model(model: &ProviderModelRecord) -> bool {
    model.effective_available() && model.tool_call == Some(true)
}

fn validate_limits(config: &WebSearchConfig) -> Result<(), WebSearchConfigError> {
    if !(MIN_SEARCH_TURNS..=MAX_SEARCH_TURNS).contains(&config.max_turns)
        || !(MIN_SEARCH_SECONDS..=MAX_SEARCH_SECONDS).contains(&config.total_time_seconds)
    {
        return Err(WebSearchConfigError::new(
            "WEB_SEARCH_INVALID_CONFIG",
            format!(
                "Web Search limits must be {MIN_SEARCH_TURNS}..={MAX_SEARCH_TURNS} turns and {MIN_SEARCH_SECONDS}..={MAX_SEARCH_SECONDS} seconds"
            ),
        ));
    }
    Ok(())
}

fn sources_unavailable() -> WebSearchConfigError {
    WebSearchConfigError::new(
        "WEB_SEARCH_SOURCES_UNAVAILABLE",
        "Local Search requires available Search and Fetch sources",
    )
}

fn invalid_codex_provider() -> WebSearchConfigError {
    WebSearchConfigError::new(
        "WEB_SEARCH_CODEX_PROVIDER_INVALID",
        "Codex Search requires an enabled Codex OAuth Responses Provider",
    )
}

fn missing_codex_model() -> WebSearchConfigError {
    WebSearchConfigError::new(
        "WEB_SEARCH_CODEX_MODEL_NOT_FOUND",
        "Configured Codex upstream model is unavailable",
    )
}
