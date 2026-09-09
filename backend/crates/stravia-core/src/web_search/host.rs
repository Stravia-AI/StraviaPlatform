use crate::agent::AgentRunner;
use async_trait::async_trait;
use futures::Stream;
use std::{pin::Pin, sync::Arc};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::agent::AgentEvent;
use stravia_runtime_contract::agent::AgentInput;
use stravia_runtime_contract::agent::AgentRunLimits;
use stravia_web_search::{
    SettingsWebSearchConfigStore, WebSearchConfig, WebSearchConfigStore, WebSearchError,
    WebSearchRunner, host::*,
};

pub(crate) struct SearchHost(pub(crate) crate::Gateway);
pub(crate) struct SearchSettings(pub(crate) crate::storage::DynStorage);
pub(crate) struct LocalAgentHost(pub(crate) AgentRunner);
impl LocalSearchHost for LocalAgentHost {
    fn run_ephemeral_resolved(
        &self,
        input: AgentInput,
        revision: u32,
        model_id: String,
        limits: AgentRunLimits,
    ) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
        self.0
            .run_ephemeral_resolved(input, revision, model_id, limits)
    }
}
#[async_trait]
impl SearchSettingsHost for SearchSettings {
    async fn get(&self, key: &str) -> Result<Option<String>, WebSearchError> {
        self.0.settings().get(key).await.map_err(|_| {
            WebSearchError::new(
                "config_unavailable",
                "Web Search configuration is unavailable",
            )
        })
    }
    async fn set(&self, key: &str, value: &str) -> Result<(), WebSearchError> {
        self.0.settings().set(key, value).await.map_err(|_| {
            WebSearchError::new(
                "config_unavailable",
                "Web Search configuration could not be saved",
            )
        })
    }
}
#[async_trait]
impl PublicSearchHost for SearchHost {
    async fn authorize(&self, principal: &Principal) -> Option<SearchAccess> {
        crate::proxy::security::Security::new(self.0.storage.auth())
            .authorize_principal_web_search(principal)
            .await
            .ok()
            .map(|access| SearchAccess {
                transparent_injection_enabled: access.transparent_injection_enabled,
            })
    }
    async fn runner_ready(&self) -> bool {
        self.0.web_search_runner_state.read().await.is_some()
    }
    async fn config(&self) -> Result<WebSearchConfig, WebSearchError> {
        SettingsWebSearchConfigStore::new(Arc::new(SearchSettings(self.0.storage.clone())))
            .load()
            .await
    }
    async fn runner(&self) -> Result<WebSearchRunner, WebSearchError> {
        self.0
            .web_search_runner()
            .await
            .map_err(|_| WebSearchError::new("web_search_unavailable", "Web Search is unavailable"))
    }
}
pub(crate) fn provider_snapshot(provider: &crate::db::models::Provider) -> SearchProvider {
    SearchProvider {
        id: provider.id.clone(),
        name: provider.name.clone(),
        is_enabled: provider.is_enabled,
        channel: provider.channel.clone(),
        auth_mode: provider.auth_mode.clone(),
        protocol: provider.protocol.clone(),
        function_calling: crate::protocol::registry::ProtocolRegistry::global()
            .protocol_supports_function_calling(&provider.protocol),
    }
}
fn model_snapshot(model: crate::provider_models::ProviderModelRecord) -> SearchModel {
    SearchModel {
        available: model.effective_available(),
        model_id: model.model_id,
        tool_call: model.metadata.tool_call,
    }
}
struct ProviderSession {
    gateway: crate::Gateway,
    provider: crate::db::models::Provider,
    snapshot: SearchProvider,
}
#[async_trait]
impl CodexHost for SearchHost {
    async fn provider(
        &self,
        provider_id: &str,
    ) -> Result<Option<Arc<dyn CodexSession>>, WebSearchError> {
        self.0
            .storage
            .providers()
            .get(provider_id)
            .await
            .map(|provider| {
                provider.map(|provider| {
                    Arc::new(ProviderSession {
                        snapshot: provider_snapshot(&provider),
                        provider,
                        gateway: self.0.clone(),
                    }) as Arc<dyn CodexSession>
                })
            })
            .map_err(|_| {
                WebSearchError::backend(
                    stravia_web_search::WebSearchBackendKind::Codex,
                    "provider_unavailable",
                    "Codex Provider is unavailable",
                )
            })
    }
}
#[async_trait]
impl CodexSession for ProviderSession {
    fn provider(&self) -> &SearchProvider {
        &self.snapshot
    }
    async fn model(&self, model_id: &str) -> Result<Option<SearchModel>, WebSearchError> {
        self.gateway
            .storage
            .provider_models()
            .get(&self.provider.id, model_id)
            .await
            .map(|model| model.map(model_snapshot))
            .map_err(|_| {
                WebSearchError::backend(
                    stravia_web_search::WebSearchBackendKind::Codex,
                    "model_unavailable",
                    "Codex model is unavailable",
                )
            })
    }
    async fn transport(&self) -> Result<CodexTransport, WebSearchError> {
        let credential = self
            .gateway
            .storage
            .oauth_credentials()
            .get(&self.provider.id)
            .await
            .map_err(|_| {
                WebSearchError::backend(
                    stravia_web_search::WebSearchBackendKind::Codex,
                    "oauth_unavailable",
                    "Codex OAuth credential is unavailable",
                )
            })?;
        let runtime = self
            .gateway
            .admin()
            .resolve_provider_runtime_from_snapshot(&self.provider, credential.as_ref())
            .await
            .map_err(|_| {
                WebSearchError::backend(
                    stravia_web_search::WebSearchBackendKind::Codex,
                    "oauth_unavailable",
                    "Codex OAuth credential is unavailable",
                )
            })?;
        let client = self
            .gateway
            .http_client_for_provider(self.provider.use_proxy)
            .await
            .map_err(|_| {
                WebSearchError::backend(
                    stravia_web_search::WebSearchBackendKind::Codex,
                    "transport_unavailable",
                    "Codex transport is unavailable",
                )
            })?;
        Ok(CodexTransport {
            client,
            access_token: runtime.access_token,
            extra_headers: runtime.binding.extra_headers,
            endpoint: runtime
                .binding
                .base_url_override
                .unwrap_or_else(|| self.provider.base_url.clone()),
        })
    }
}
#[async_trait]
impl SearchAdminHost for SearchHost {
    fn settings(&self) -> Arc<dyn SearchSettingsHost> {
        Arc::new(SearchSettings(self.0.storage.clone()))
    }
    fn config_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.0.web_search_config_lock.clone()
    }
    async fn models(&self) -> Result<Vec<SearchRoute>, ()> {
        self.0
            .admin()
            .list_models()
            .await
            .map(|models| {
                models
                    .into_iter()
                    .map(|model| {
                        let display_name = model.effective_display_name().to_owned();
                        SearchRoute {
                            id: model.id,
                            model_id: model.model_id,
                            display_name,
                            is_enabled: model.is_enabled,
                            targets: model
                                .targets
                                .into_iter()
                                .map(|target| SearchRouteTarget {
                                    provider_id: target.provider_id,
                                    model: target.model,
                                })
                                .collect(),
                        }
                    })
                    .collect()
            })
            .map_err(|_| ())
    }
    async fn providers(&self) -> Result<Vec<SearchProvider>, ()> {
        self.0
            .storage
            .providers()
            .list()
            .await
            .map(|providers| providers.iter().map(provider_snapshot).collect())
            .map_err(|_| ())
    }
    async fn provider(&self, id: &str) -> Result<Option<SearchProvider>, ()> {
        self.0
            .storage
            .providers()
            .get(id)
            .await
            .map(|provider| provider.as_ref().map(provider_snapshot))
            .map_err(|_| ())
    }
    async fn credential(&self, id: &str) -> Result<Option<SearchCredential>, ()> {
        self.0
            .storage
            .oauth_credentials()
            .get(id)
            .await
            .map(|credential| {
                credential.map(|credential| SearchCredential {
                    connected: credential.status == "connected",
                    has_access_token: !credential.access_token.trim().is_empty(),
                    expiry_valid: credential.expires_at.as_deref().is_none_or(|expires_at| {
                        crate::proxy::security::is_key_expired(expires_at) == Ok(false)
                    }),
                    has_refresh_token: credential
                        .refresh_token
                        .as_deref()
                        .is_some_and(|token| !token.trim().is_empty()),
                })
            })
            .map_err(|_| ())
    }
    async fn models_for_provider(&self, id: &str) -> Result<Vec<SearchModel>, ()> {
        self.0
            .storage
            .provider_models()
            .list_for_provider(id)
            .await
            .map(|models| models.into_iter().map(model_snapshot).collect())
            .map_err(|_| ())
    }
    async fn provider_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<Option<SearchModel>, ()> {
        self.0
            .storage
            .provider_models()
            .get(provider_id, model)
            .await
            .map(|model| model.map(model_snapshot))
            .map_err(|_| ())
    }
    async fn sources(&self) -> Result<Option<SearchSources>, ()> {
        let Some(store) = self.0.storage.web_providers() else {
            return Ok(None);
        };
        let settings = store.load_settings().await.map_err(|_| ())?;
        let providers = store
            .list()
            .await
            .map_err(|_| ())?
            .into_iter()
            .map(|provider| {
                let capabilities = provider.capabilities();
                SearchSourceProvider {
                    id: provider.id,
                    kind: provider.kind,
                    search: capabilities.as_ref().is_some_and(|value| value.search),
                    fetch: capabilities.as_ref().is_some_and(|value| value.fetch),
                }
            })
            .collect();
        Ok(Some(SearchSources {
            settings: SearchSourceSettings {
                search_provider_ids: settings.search_provider_ids,
                fetch_provider_ids: settings.fetch_provider_ids,
            },
            providers,
            local_browser_available: self.0.web_access().local_browser_available().await,
        }))
    }
}
