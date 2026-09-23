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
    }
}
fn model_snapshot(model: crate::provider_models::ProviderModelRecord) -> SearchModel {
    SearchModel {
        available: model.effective_available(),
        model_id: model.model_id,
        tool_call: model.metadata.tool_call,
    }
}
#[async_trait]
impl ExternalSearchHost for SearchHost {
    async fn execute_external_search(
        &self,
        principal: &Principal,
        route_id: &str,
        request: stravia_vendor_sdk::SearchRequest,
        cancellation: stravia_runtime_contract::CancellationToken,
        deadline: std::time::Instant,
    ) -> Result<ExternalSearchExecution, WebSearchError> {
        let route = self
            .0
            .storage
            .routes()
            .get(route_id)
            .await
            .map_err(|_| {
                external_error("route_unavailable", "External Search Route is unavailable")
            })?
            .ok_or_else(|| {
                external_error("route_unavailable", "External Search Route is unavailable")
            })?;
        let execution = self
            .0
            .execute_vendor_route(
                principal,
                &route,
                crate::plugin::VendorRequest::Search(request),
                crate::plugin::VendorCallContext::new(
                    cancellation.clone(),
                    stravia_runtime_contract::Deadline::fixed(deadline),
                ),
            )
            .await
            .map_err(external_execution_error)?;
        let guard = execution
            .publication
            .write_fence()
            .await
            .map_err(|_| external_publication_error(&cancellation, deadline))?;
        let stravia_vendor_sdk::OperationOutput::Search(response) = execution.output else {
            return Err(external_error(
                "invalid_report",
                "External Search Vendor returned an invalid result",
            ));
        };
        Ok(ExternalSearchExecution {
            response,
            publication: SearchPublicationGuard::new(Box::new(guard)),
            provider_id: execution.provider_id,
            upstream_model: execution.upstream_model,
            target_id: execution.target_id,
        })
    }
}

fn external_execution_error(error: anyhow::Error) -> WebSearchError {
    match error.downcast_ref::<stravia_vendor_runtime::RuntimeError>() {
        Some(stravia_vendor_runtime::RuntimeError::Cancelled)
        | Some(stravia_vendor_runtime::RuntimeError::Plugin {
            kind: stravia_vendor_sdk::ErrorKind::Cancelled,
            ..
        }) => external_error("cancelled", "External Search was cancelled"),
        Some(stravia_vendor_runtime::RuntimeError::DeadlineExceeded)
        | Some(stravia_vendor_runtime::RuntimeError::Plugin {
            kind: stravia_vendor_sdk::ErrorKind::DeadlineExceeded,
            ..
        }) => external_error("deadline_exceeded", "External Search deadline exceeded"),
        _ => external_error("upstream_failed", "External Search execution failed"),
    }
}

fn external_publication_error(
    cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: std::time::Instant,
) -> WebSearchError {
    if std::time::Instant::now() >= deadline {
        external_error("deadline_exceeded", "External Search deadline exceeded")
    } else if cancellation.is_cancelled() {
        external_error("cancelled", "External Search was cancelled")
    } else {
        external_error(
            "cancelled",
            "External Search result can no longer be published",
        )
    }
}

fn external_error(code: &'static str, message: &'static str) -> WebSearchError {
    WebSearchError::backend(
        stravia_web_search::WebSearchBackendKind::External,
        code,
        message,
    )
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
                            id: model.id.into(),
                            model_id: model.model_id.into(),
                            display_name,
                            is_enabled: model.is_enabled,
                            targets: model
                                .targets
                                .into_iter()
                                .map(|target| {
                                    let (provider_id, model) = target.destination.into_parts();
                                    SearchRouteTarget {
                                        provider_id: provider_id.into(),
                                        model: model.map(Into::into),
                                        enabled: target.enabled,
                                    }
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
    async fn provider_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<Option<SearchModel>, ()> {
        self.0
            .storage
            .provider_models()
            .find(provider_id, model)
            .await
            .map(|model| model.map(model_snapshot))
            .map_err(|_| ())
    }
    async fn validate_external_route(&self, route_id: &str) -> Result<(), ()> {
        let route = self
            .0
            .storage
            .routes()
            .get(route_id)
            .await
            .map_err(|_| ())?
            .ok_or(())?;
        self.0
            .validate_vendor_route_capability(&route, stravia_vendor_sdk::Capability::Search)
            .await
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
        }))
    }
}
