use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use crate::fetch::{FetchErrorCode, FetchedPage};
use crate::search::config::{Config, EngineConfig};
use crate::search::engines::{AllowedDomain, Engine, ProgressUpdateData};
use crate::{
    AdapterSuccess, FetchRequest, FetchResult, FetchStatus, ProviderFailure, SearchMode,
    SearchRequest, SearchResponse, SearchResult, WebAccessErrorCode, WebProviderAdapter,
};
use crate::{LocalWeb, OutboundProxyMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalSearchEngineSetting {
    pub enabled: bool,
}

pub type LocalSearchEngineSettings = BTreeMap<String, LocalSearchEngineSetting>;
const LOCAL_SEARCH_ENGINE_IDS: [&str; 7] = [
    "google",
    "bing",
    "brave",
    "baidu",
    "360",
    "sogou_weixin",
    "google_scholar",
];

#[async_trait::async_trait]
pub trait LocalAdapterRuntime: Send + Sync {
    async fn search(
        &self,
        request: &SearchRequest,
        config: Arc<Config>,
    ) -> Result<SearchResponse, ProviderFailure>;

    async fn fetch(&self, url: &str) -> Result<FetchedPage, ProviderFailure>;
}

pub fn build_local_adapter(
    id: String,
    outbound: OutboundProxyMode,
    engines: LocalSearchEngineSettings,
) -> Result<Arc<dyn WebProviderAdapter>, ProviderFailure> {
    let runtime = LocalWeb::new(outbound).map_err(|error| {
        ProviderFailure::new(WebAccessErrorCode::Unavailable, error.to_string())
    })?;
    build_local_adapter_with_runtime(id, engines, Arc::new(runtime))
}

pub fn build_local_adapter_with_runtime(
    id: String,
    engines: LocalSearchEngineSettings,
    runtime: Arc<dyn LocalAdapterRuntime>,
) -> Result<Arc<dyn WebProviderAdapter>, ProviderFailure> {
    Ok(Arc::new(LocalAdapter {
        id,
        config: Arc::new(local_search_config(&engines)?),
        runtime,
    }))
}

struct LocalAdapter {
    id: String,
    config: Arc<Config>,
    runtime: Arc<dyn LocalAdapterRuntime>,
}

#[async_trait::async_trait]
impl WebProviderAdapter for LocalAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        true
    }

    fn supports_fetch(&self) -> bool {
        true
    }

    async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        self.runtime
            .search(request, Arc::clone(&self.config))
            .await
            .map(|result| AdapterSuccess::new(result, None))
    }

    async fn fetch(
        &self,
        request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        let mut results = Vec::with_capacity(request.urls.len());
        for url in &request.urls {
            match self.runtime.fetch(url).await {
                Ok(page) => results.push(FetchResult {
                    url: url.clone(),
                    status: FetchStatus::Success,
                    content: Some(page.markdown),
                    format: Some("markdown".into()),
                    title: page.title,
                    truncated: page.truncated,
                    limitations: page.limitations,
                    error: None,
                }),
                Err(failure) => {
                    results.push(crate::failed_fetch_result(url.clone(), failure.code, None))
                }
            }
        }
        Ok(AdapterSuccess::new(results, None))
    }
}

fn local_search_config(engines: &LocalSearchEngineSettings) -> Result<Config, ProviderFailure> {
    let mut config = Config::default();
    let configs = Arc::make_mut(&mut config.engines);
    for (id, setting) in engines {
        if !LOCAL_SEARCH_ENGINE_IDS.contains(&id.as_str()) {
            return Err(ProviderFailure::new(
                WebAccessErrorCode::Unavailable,
                format!("unknown Local Search Engine: {id}"),
            ));
        }
        let engine = Engine::from_str(id).map_err(|_| {
            ProviderFailure::new(
                WebAccessErrorCode::Unavailable,
                format!("unknown Local Search Engine: {id}"),
            )
        })?;
        let existing = configs.get(engine).clone();
        configs.map.insert(
            engine,
            EngineConfig {
                enabled: setting.enabled,
                ..existing
            },
        );
    }
    if !engines.values().any(|setting| setting.enabled) {
        return Err(ProviderFailure::new(
            WebAccessErrorCode::Unavailable,
            "at least one Local Search Engine must be enabled",
        ));
    }
    Ok(config)
}

#[async_trait::async_trait]
impl LocalAdapterRuntime for LocalWeb {
    async fn search(
        &self,
        request: &SearchRequest,
        config: Arc<Config>,
    ) -> Result<SearchResponse, ProviderFailure> {
        let mut query = self.search_query(&request.query);
        query.allowed_domains = request
            .allowed_domains
            .iter()
            .map(|domain| {
                AllowedDomain::parse(domain).map_err(|error| {
                    ProviderFailure::new(WebAccessErrorCode::InvalidInput, error.to_string())
                })
            })
            .collect::<Result<_, _>>()?;
        query.config = config;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        LocalWeb::search(self, query, progress_tx)
            .await
            .map_err(|error| {
                ProviderFailure::new(WebAccessErrorCode::Unavailable, error.to_string())
            })?;
        let mut local_response = None;
        while let Ok(update) = progress_rx.try_recv() {
            if let ProgressUpdateData::Response(response) = update.data {
                local_response = Some(response);
            }
        }
        let local_response = local_response.ok_or_else(|| {
            ProviderFailure::new(
                WebAccessErrorCode::Unavailable,
                "Local Search returned no response",
            )
        })?;
        let results = local_response
            .search_results
            .into_iter()
            .take(request.max_results)
            .map(|result| SearchResult {
                url: result.result.url,
                title: Some(result.result.title),
                snippet: Some(result.result.description),
            })
            .collect();
        Ok(SearchResponse {
            mode: SearchMode::Index,
            query: request.query.clone(),
            results,
            answer: None,
            citations: None,
        })
    }

    async fn fetch(&self, url: &str) -> Result<FetchedPage, ProviderFailure> {
        LocalWeb::fetch(self, url).await.map_err(|error| {
            let code = match error.code() {
                FetchErrorCode::InvalidUrl => WebAccessErrorCode::InvalidInput,
                FetchErrorCode::UnsupportedMediaType => WebAccessErrorCode::Unsupported,
                FetchErrorCode::Unavailable | FetchErrorCode::ResponseTooLarge => {
                    WebAccessErrorCode::Unavailable
                }
            };
            ProviderFailure::new(code, error.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::ExtractionPath;
    use std::sync::Mutex;

    struct FakeLocalRuntime {
        observed_config: Mutex<Option<Arc<Config>>>,
    }

    #[async_trait::async_trait]
    impl LocalAdapterRuntime for FakeLocalRuntime {
        async fn search(
            &self,
            request: &SearchRequest,
            config: Arc<Config>,
        ) -> Result<SearchResponse, ProviderFailure> {
            *self.observed_config.lock().expect("observed config lock") = Some(config);
            Ok(SearchResponse {
                mode: SearchMode::Index,
                query: request.query.clone(),
                results: Vec::new(),
                answer: None,
                citations: None,
            })
        }

        async fn fetch(&self, url: &str) -> Result<FetchedPage, ProviderFailure> {
            Ok(FetchedPage {
                requested_url: url.into(),
                final_url: url.into(),
                title: Some("Example".into()),
                markdown: "body".into(),
                extraction_path: ExtractionPath::Static,
                limitations: vec!["Main content may be incomplete.".into()],
                truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn injected_local_runtime_preserves_empty_search_and_fetch_limitations() {
        let runtime = Arc::new(FakeLocalRuntime {
            observed_config: Mutex::new(None),
        });
        let engines = [
            ("google", true),
            ("bing", false),
            ("brave", false),
            ("baidu", false),
            ("360", false),
            ("sogou_weixin", false),
            ("google_scholar", false),
        ]
        .into_iter()
        .map(|(id, enabled)| (id.into(), LocalSearchEngineSetting { enabled }))
        .collect();
        let adapter =
            build_local_adapter_with_runtime("local".into(), engines, runtime.clone()).unwrap();

        let search = adapter
            .search(&SearchRequest {
                query: "quiet query".into(),
                max_results: 5,
                allowed_domains: Vec::new(),
                blocked_domains: Vec::new(),
            })
            .await
            .expect("empty Local Search is successful");
        assert!(search.result.results.is_empty());
        let config = runtime
            .observed_config
            .lock()
            .expect("observed config lock")
            .clone()
            .expect("observed config");
        assert!(config.engines.get(Engine::Google).enabled);
        assert!(!config.engines.get(Engine::Bing).enabled);

        let fetch = adapter
            .fetch(&FetchRequest {
                urls: vec!["https://example.com/".into()],
                max_characters: 8_000,
            })
            .await
            .expect("Local Fetch");
        assert_eq!(
            fetch.result[0].limitations,
            ["Main content may be incomplete."]
        );
    }
}
