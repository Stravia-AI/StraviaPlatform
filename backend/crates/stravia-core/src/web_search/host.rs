use crate::agent::AgentRunner;
use async_trait::async_trait;
use futures::Stream;
use std::{future::Future, pin::Pin, sync::Arc};
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
        self.0
            .authorize_capability_injections(principal, crate::policy::PolicyCapability::WebSearch)
            .await
            .ok()
            .map(|access| SearchAccess {
                transparent_injection_enabled: access
                    .contains(crate::db::models::TransparentInjection::WebSearch),
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
        available: model.effective_available()
            && model
                .metadata
                .supports_route_purpose(crate::db::models::RoutePurpose::Inference),
        model_id: model.model_id,
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
        if route.purpose != Some(crate::db::models::RoutePurpose::WebSearch) {
            return Err(external_error(
                "route_unavailable",
                "External Search Route is incompatible",
            ));
        }
        let mut context = crate::plugin::VendorCallContext::new(
            cancellation.clone(),
            stravia_runtime_contract::Deadline::fixed(deadline),
        );
        context.observer = crate::interaction_observation::scope::current();
        let execution = self
            .0
            .execute_vendor_route(
                principal,
                &route,
                crate::plugin::VendorRequest::Search(request),
                context,
            )
            .await
            .map_err(external_execution_error)?;
        let guard = execution
            .publication
            .write_fence()
            .await
            .map_err(|_| external_publication_error(&cancellation, deadline))?;
        let stravia_vendor_sdk::OperationOutput::Search(mut response) = execution.output else {
            return Err(external_error(
                "invalid_report",
                "External Search Vendor returned an invalid result",
            ));
        };
        resolve_grounding_sources(&mut response, |url| {
            let proxy = execution.proxy.clone();
            async move { stravia_web_access::fetch::resolve_final_url(&url, proxy).await }
        })
        .await?;
        Ok(ExternalSearchExecution {
            response,
            publication: SearchPublicationGuard::new(Box::new(guard)),
            provider_id: execution.provider_id,
            upstream_model: execution.upstream_model,
            target_id: execution.target_id,
        })
    }
}

async fn resolve_grounding_sources<F, Fut>(
    response: &mut stravia_vendor_sdk::SearchResponse,
    mut resolve: F,
) -> Result<(), WebSearchError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<url::Url, stravia_web_access::fetch::FetchError>>,
{
    // Match the report validator's limits before making any network requests.
    if response.sources.len() > stravia_web_search::MAX_SEARCH_REPORT_SOURCES
        || response
            .sources
            .iter()
            .any(|source| source.url.len() > stravia_web_search::MAX_SEARCH_SOURCE_URL_BYTES)
    {
        return Err(external_error(
            "invalid_report",
            "Search Source count or URL exceeds the configured bounds",
        ));
    }
    let mut resolved = std::collections::HashMap::<String, String>::new();
    for source in &mut response.sources {
        let Ok(url) = url::Url::parse(&source.url) else {
            continue;
        };
        if url.scheme() != "https"
            || url.host_str() != Some("vertexaisearch.cloud.google.com")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || !url
                .path()
                .strip_prefix("/grounding-api-redirect/")
                .is_some_and(|tail| !tail.is_empty())
        {
            continue;
        }
        let final_url = if let Some(value) = resolved.get(&source.url) {
            value.clone()
        } else {
            let value = resolve(source.url.clone())
                .await
                .map_err(|error| {
                    use stravia_web_access::fetch::FetchErrorCode;
                    match error.code() {
                        FetchErrorCode::InvalidUrl => external_error(
                            "invalid_source_url",
                            "External Search source redirect is not a public URL",
                        ),
                        _ if error.to_string() == "HTTP redirect omitted Location" => {
                            external_error(
                                "source_resolution_failed",
                                "External Search source redirect omitted Location",
                            )
                        }
                        _ if error.to_string() == "HTTP redirect limit exceeded" => external_error(
                            "source_resolution_failed",
                            "External Search source redirect limit exceeded",
                        ),
                        _ => external_error(
                            "source_resolution_failed",
                            "External Search source redirect network request failed",
                        ),
                    }
                })?
                .to_string();
            resolved.insert(source.url.clone(), value.clone());
            value
        };
        source.url = final_url;
    }
    Ok(())
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
        self.0.route_media_mutation.clone()
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
                            is_inference: model.purpose
                                == Some(crate::db::models::RoutePurpose::Inference),
                            is_web_search: model.purpose
                                == Some(crate::db::models::RoutePurpose::WebSearch),
                            targets: model
                                .targets
                                .into_iter()
                                .map(|target| {
                                    let (provider_id, model) = target.destination.into_parts();
                                    SearchRouteTarget {
                                        provider_id: provider_id.into(),
                                        model: model.into(),
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
        if route.purpose != Some(crate::db::models::RoutePurpose::WebSearch) {
            return Err(());
        }
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
        let records = store.list().await.map_err(|_| ())?;
        let registry = self.0.web_provider_registry();
        let mut providers = Vec::with_capacity(records.len());
        for provider in records {
            let (search, fetch) = match registry.get(&provider.kind) {
                Some(factory) if factory.is_available().await => {
                    let capabilities = factory.descriptor().capabilities;
                    (capabilities.search, capabilities.fetch)
                }
                _ => (false, false),
            };
            providers.push(SearchSourceProvider {
                id: provider.id,
                search,
                fetch,
            });
        }
        Ok(Some(SearchSources {
            settings: SearchSourceSettings {
                search_provider_ids: settings.search_provider_ids,
                fetch_provider_ids: settings.fetch_provider_ids,
            },
            providers,
        }))
    }
}

#[cfg(test)]
mod grounding_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn response(urls: &[&str]) -> stravia_vendor_sdk::SearchResponse {
        stravia_vendor_sdk::SearchResponse {
            answer: "Claim [sc:first]".into(),
            sources: urls
                .iter()
                .enumerate()
                .map(|(index, url)| stravia_vendor_sdk::SearchSource {
                    id: if index == 0 {
                        "first".into()
                    } else {
                        format!("source-{index}")
                    },
                    url: (*url).into(),
                    title: None,
                    snippet: None,
                    published_at: None,
                })
                .collect(),
            limitations: Vec::new(),
            usage: None,
        }
    }

    #[tokio::test]
    async fn only_exact_grounding_sources_resolve_once_preserving_citations() {
        let grounding = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/opaque";
        let mut result = response(&[
            "https://publisher.example/article",
            grounding,
            grounding,
            "https://other.google.com/grounding-api-redirect/x",
            "https://vertexaisearch.cloud.google.com:444/grounding-api-redirect/x",
            "https://vertexaisearch.cloud.google.com/grounding-api-redirect/",
        ]);
        let calls = AtomicUsize::new(0);
        resolve_grounding_sources(&mut result, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Ok(url::Url::parse("https://publisher.example/final").unwrap()) }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(result.sources[0].url, "https://publisher.example/article");
        assert_eq!(result.sources[1].url, "https://publisher.example/final");
        assert_eq!(result.sources[2].url, result.sources[1].url);
        assert_eq!(result.sources[0].id, "first");
        assert_eq!(result.sources[1].id, "source-1");
        assert_eq!(result.answer, "Claim [sc:first]");
    }

    #[tokio::test]
    async fn ordinary_sources_never_trigger_resolution() {
        let mut result = response(&[
            "https://example.com/article",
            "https://google.com/grounding-api-redirect/a",
        ]);
        let calls = AtomicUsize::new(0);
        resolve_grounding_sources(&mut result, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Ok(url::Url::parse("https://example.com/").unwrap()) }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn oversized_response_is_rejected_before_any_resolution() {
        let grounding = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/opaque";
        let oversized = "x".repeat(8193);
        let mut result = response(&[grounding, &oversized]);
        let calls = AtomicUsize::new(0);
        let error = resolve_grounding_sources(&mut result, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Ok(url::Url::parse("https://publisher.example/").unwrap()) }
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_report");
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn validator_checks_resolved_destination_against_allowed_domains() {
        let mut result =
            response(&["https://vertexaisearch.cloud.google.com/grounding-api-redirect/opaque"]);
        resolve_grounding_sources(&mut result, |_| async {
            Ok(url::Url::parse("https://8.8.8.8/article").unwrap())
        })
        .await
        .unwrap();
        let turn = stravia_web_search::SearchTurnId::new("abcdefghijklmnopqrstuvwxyzab");
        let path = format!("{}/sources/1", turn.reference());
        let report = stravia_web_search::SearchReport {
            answer: format!("Claim [{path}]"),
            sources: vec![stravia_web_search::SearchSource {
                path,
                url: result.sources[0].url.clone(),
                title: None,
            }],
            limitations: Vec::new(),
        };
        let validator = stravia_web_search::SearchReportValidator;
        let rejected = validator
            .validate(
                &turn,
                stravia_web_search::SearchCompletion::Complete,
                None,
                report.clone(),
                None,
                &["vertexaisearch.cloud.google.com".into()],
            )
            .await
            .unwrap_err();
        assert_eq!(rejected.code, "source_outside_allowed_domains");
        let accepted = validator
            .validate(
                &turn,
                stravia_web_search::SearchCompletion::Complete,
                None,
                report,
                None,
                &["8.8.8.8".into()],
            )
            .await
            .unwrap();
        assert_eq!(accepted.sources[0].url, "https://8.8.8.8/article");
        assert_eq!(result.sources[0].id, "first");
    }

    #[tokio::test]
    async fn resolution_failure_is_user_visible_without_source_query() {
        let mut result = response(&[
            "https://vertexaisearch.cloud.google.com/grounding-api-redirect/opaque?secret=value",
        ]);
        let error = resolve_grounding_sources(&mut result, |_| async {
            stravia_web_access::fetch::resolve_final_url(
                "http://127.0.0.1/",
                stravia_web_access::OutboundProxyMode::Direct,
            )
            .await
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_source_url");
        assert!(!error.message.contains("secret"));
    }
}
