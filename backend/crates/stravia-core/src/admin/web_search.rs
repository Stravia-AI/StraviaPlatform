use serde::Serialize;

use crate::protocol::registry::ProtocolRegistry;
use crate::provider_models::ProviderModelRecord;
use crate::web_search::WebSearchBackendDraft;
use crate::web_search::{
    MAX_SEARCH_SECONDS, MAX_SEARCH_TURNS, MIN_SEARCH_SECONDS, MIN_SEARCH_TURNS,
    ResolvedWebSearchBackend, SettingsWebSearchConfigStore, WebSearchConfig, WebSearchConfigStore,
    codex_provider_contract, resolve_enabled_config,
};

use super::AdminService;

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct WebSearchConfigError {
    pub code: &'static str,
    pub message: String,
}

impl WebSearchConfigError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EligibleSearchModel {
    pub id: String,
    pub name: String,
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

impl AdminService {
    pub async fn get_web_search_config(&self) -> Result<WebSearchConfigView, WebSearchConfigError> {
        SettingsWebSearchConfigStore::new(self.gw.storage.clone())
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
        let models = self.list_models().await.map_err(|_| {
            WebSearchConfigError::new(
                "WEB_SEARCH_MODEL_INELIGIBLE",
                "Local Search Model eligibility is unavailable",
            )
        })?;
        let mut eligible = Vec::new();
        for model in models.into_iter().filter(|model| model.is_enabled) {
            if self.validate_local_targets(&model).await.is_ok() {
                eligible.push(EligibleSearchModel {
                    id: model.id,
                    name: model.name,
                });
            }
        }
        eligible.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(eligible)
    }

    pub async fn list_compatible_codex_search_providers(
        &self,
    ) -> Result<Vec<CompatibleCodexProvider>, WebSearchConfigError> {
        let providers = self
            .gw
            .storage
            .providers()
            .list()
            .await
            .map_err(|_| invalid_codex_provider())?;
        let mut compatible = Vec::new();
        for provider in providers {
            if !codex_provider_contract(&provider) {
                continue;
            }
            let credential = self
                .gw
                .storage
                .oauth_credentials()
                .get(&provider.id)
                .await
                .map_err(|_| invalid_codex_provider())?;
            if !credential.as_ref().is_some_and(effective_oauth_credential) {
                continue;
            }
            let mut models = self
                .gw
                .storage
                .provider_models()
                .list_for_provider(&provider.id)
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
        let _guard = self.gw.web_search_config_lock.lock().await;
        if matches!(
            config.backend.as_ref(),
            Some(WebSearchBackendDraft::Local { .. })
        ) {
            validate_limits(&config)?;
        }
        if config.enabled {
            self.validate_enabled_web_search(&config).await?;
        }
        let store = SettingsWebSearchConfigStore::new(self.gw.storage.clone());
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
            .list_models()
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
        model: &crate::db::models::Model,
    ) -> Result<(), WebSearchConfigError> {
        for target in &model.targets {
            let Some(provider) = self
                .gw
                .storage
                .providers()
                .get(&target.provider_id)
                .await
                .map_err(|_| {
                    WebSearchConfigError::new(
                        "WEB_SEARCH_MODEL_INELIGIBLE",
                        "Local Search Model eligibility is unavailable",
                    )
                })?
            else {
                continue;
            };
            if !provider.is_enabled
                || !ProtocolRegistry::global()
                    .protocol_supports_function_calling(&provider.protocol)
            {
                continue;
            }
            let provider_model = self
                .gw
                .storage
                .provider_models()
                .get(&target.provider_id, &target.model)
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

    async fn validate_local_sources(&self) -> Result<(), WebSearchConfigError> {
        let Some(store) = self.gw.storage.web_providers() else {
            return Err(sources_unavailable());
        };
        let settings = store
            .load_settings()
            .await
            .map_err(|_| sources_unavailable())?;
        if !settings.enabled {
            return Err(sources_unavailable());
        }
        let providers = store.list().await.map_err(|_| sources_unavailable())?;
        let has_search = settings.search_provider_ids.iter().any(|id| {
            providers.iter().any(|provider| {
                provider.id == *id
                    && provider.kind != "codex"
                    && provider
                        .capabilities()
                        .is_some_and(|capabilities| capabilities.search)
            })
        });
        let has_fetch = settings.fetch_provider_ids.iter().any(|id| {
            providers.iter().any(|provider| {
                provider.id == *id
                    && provider.kind != "codex"
                    && provider
                        .capabilities()
                        .is_some_and(|capabilities| capabilities.fetch)
            })
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
            .gw
            .storage
            .providers()
            .get(provider_id)
            .await
            .map_err(|_| invalid_codex_provider())?
            .ok_or_else(invalid_codex_provider)?;
        if !codex_provider_contract(&provider) {
            return Err(invalid_codex_provider());
        }
        self.gw
            .storage
            .oauth_credentials()
            .get(provider_id)
            .await
            .map_err(|_| invalid_codex_provider())?
            .filter(effective_oauth_credential)
            .ok_or_else(invalid_codex_provider)?;
        let model = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, upstream_model)
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

fn effective_oauth_credential(credential: &crate::db::models::OAuthCredential) -> bool {
    if credential.status != "connected" || credential.access_token.trim().is_empty() {
        return false;
    }
    credential.expires_at.as_deref().is_none_or(|expires_at| {
        crate::proxy::security::is_key_expired(expires_at) == Ok(false)
            || credential
                .refresh_token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty())
    })
}

fn eligible_provider_model(model: &ProviderModelRecord) -> bool {
    model.effective_available() && model.metadata.tool_call == Some(true)
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
        "Local Search requires enabled Search and Fetch sources",
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn admin() -> (tempfile::TempDir, AdminService) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        (directory, gateway.admin())
    }

    #[tokio::test]
    async fn disabled_config_accepts_an_incomplete_binding_as_a_full_replacement() {
        let (_directory, admin) = admin().await;
        let current = admin.get_web_search_config().await.expect("current config");
        assert_eq!(
            current.limits,
            WebSearchLimits {
                min_turns: 2,
                max_turns: 20,
                min_total_time_seconds: 60,
                max_total_time_seconds: 900,
            }
        );

        let updated = admin
            .update_web_search_config(WebSearchConfig {
                revision: current.revision,
                enabled: false,
                backend: Some(WebSearchBackendDraft::Local { model_id: None }),
                max_turns: 6,
                total_time_seconds: 120,
                updated_at: current.updated_at.clone(),
            })
            .await
            .expect("disabled incomplete config");

        assert_eq!(updated.revision, current.revision + 1);
        assert_eq!(updated.max_turns, 6);
        assert_eq!(
            admin.get_web_search_config().await.expect("stored config"),
            updated
        );
    }

    #[tokio::test]
    async fn enabled_config_rejects_incomplete_binding_and_invalid_limits() {
        let (_directory, admin) = admin().await;
        let current = admin.get_web_search_config().await.expect("current config");
        let mut input = WebSearchConfig {
            revision: current.revision,
            enabled: true,
            backend: None,
            max_turns: 12,
            total_time_seconds: 600,
            updated_at: current.updated_at.clone(),
        };

        let error = admin
            .update_web_search_config(input.clone())
            .await
            .expect_err("incomplete binding");
        assert_eq!(error.code, "WEB_SEARCH_INVALID_CONFIG");

        input.enabled = false;
        input.backend = Some(WebSearchBackendDraft::Local { model_id: None });
        input.max_turns = 1;
        let error = admin
            .update_web_search_config(input)
            .await
            .expect_err("invalid limits");
        assert_eq!(error.code, "WEB_SEARCH_INVALID_CONFIG");
    }

    #[tokio::test]
    async fn codex_mode_preserves_local_limits_without_validating_them() {
        let (_directory, admin) = admin().await;
        let current = admin.get_web_search_config().await.expect("current config");

        let codex = admin
            .update_web_search_config(WebSearchConfig {
                revision: current.revision,
                enabled: false,
                backend: Some(WebSearchBackendDraft::Codex {
                    provider_id: None,
                    upstream_model: None,
                }),
                max_turns: 1,
                total_time_seconds: 1,
                updated_at: current.updated_at.clone(),
            })
            .await
            .expect("disabled Codex config");

        assert_eq!(codex.max_turns, 1);
        assert_eq!(codex.total_time_seconds, 1);

        let error = admin
            .update_web_search_config(WebSearchConfig {
                backend: Some(WebSearchBackendDraft::Local { model_id: None }),
                ..codex.config
            })
            .await
            .expect_err("Local mode must validate restored limits");
        assert_eq!(error.code, "WEB_SEARCH_INVALID_CONFIG");
    }

    #[tokio::test]
    async fn eligible_models_include_tool_capable_openai_compatible_routes() {
        let (_directory, admin) = admin().await;
        let provider = admin
            .gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "Tool-capable Provider".into(),
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
                "tool-model",
                crate::provider_models::CreateManualProviderModel {
                    metadata: serde_json::json!({
                        "id": "tool-model",
                        "tool_call": true,
                    }),
                },
            )
            .await
            .expect("Provider Model");
        let model = admin
            .create_model(crate::db::models::CreateModel {
                name: "Search Model".into(),
                balance: Some("weighted".into()),
                target_provider: provider.id.clone(),
                target_model: "tool-model".into(),
                targets: vec![],
            })
            .await
            .expect("Model route");

        let eligible = admin
            .list_eligible_web_search_models()
            .await
            .expect("eligible Models");

        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].id, model.id);
        assert_eq!(eligible[0].name, "Search Model");
    }

    #[tokio::test]
    async fn codex_binding_requires_an_effective_oauth_credential() {
        let (_directory, admin) = admin().await;
        let provider = admin
            .gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "Codex without OAuth".into(),
                vendor: Some("openai".into()),
                protocol: "open-responses".into(),
                base_url: "https://chatgpt.com/backend-api/codex/responses".into(),
                preset_key: Some("openai".into()),
                channel: Some("codex".into()),
                models_source: None,
                static_models: None,
                api_key: String::new(),
                adapter_credentials: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await
            .expect("Codex Provider");

        assert!(
            admin
                .list_compatible_codex_search_providers()
                .await
                .expect("compatible Providers")
                .is_empty()
        );

        let current = admin.get_web_search_config().await.expect("current config");
        let error = admin
            .update_web_search_config(WebSearchConfig {
                revision: current.revision,
                enabled: true,
                backend: Some(WebSearchBackendDraft::Codex {
                    provider_id: Some(provider.id),
                    upstream_model: Some("gpt-5".into()),
                }),
                max_turns: 12,
                total_time_seconds: 600,
                updated_at: current.updated_at.clone(),
            })
            .await
            .expect_err("missing OAuth credential");
        assert_eq!(error.code, "WEB_SEARCH_CODEX_PROVIDER_INVALID");
    }
}
