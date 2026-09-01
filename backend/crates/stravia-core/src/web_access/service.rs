use super::engine::*;
use super::*;

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct WebAccessAvailability {
    pub search: bool,
    pub fetch: bool,
}

#[derive(Clone)]
pub(super) struct WebAccessRunSnapshot {
    api_key_id: String,
    engine: WebAccessEngine,
}

#[derive(Clone, Default)]
pub(crate) struct WebAccessRunSnapshotStore {
    snapshots: Arc<std::sync::Mutex<HashMap<String, WebAccessRunSnapshot>>>,
}

impl WebAccessRunSnapshotStore {
    fn insert(&self, run_id: String, snapshot: WebAccessRunSnapshot) {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id, snapshot);
    }

    fn get(&self, run_id: &str) -> Option<WebAccessRunSnapshot> {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .cloned()
    }

    fn remove(&self, run_id: &str) {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(run_id);
    }
}

#[derive(Clone)]
pub struct WebAccessService {
    gateway: crate::Gateway,
}

impl WebAccessService {
    pub(crate) fn new(gateway: crate::Gateway) -> Self {
        Self { gateway }
    }

    pub async fn settings(&self) -> anyhow::Result<WebAccessSettings> {
        let Some(store) = self.gateway.storage.web_providers() else {
            return Ok(WebAccessSettings::default());
        };
        store.load_settings().await
    }

    pub(crate) async fn capture_run_snapshot(
        &self,
        run_id: &str,
        api_key_id: &str,
    ) -> anyhow::Result<WebAccessAvailability> {
        match tokio::time::timeout(
            WEB_ACCESS_DEADLINE,
            self.capture_run_snapshot_inner(run_id, api_key_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.release_run_snapshot(run_id);
                Err(anyhow::anyhow!("Web Access snapshot deadline exceeded"))
            }
        }
    }

    async fn capture_run_snapshot_inner(
        &self,
        run_id: &str,
        api_key_id: &str,
    ) -> anyhow::Result<WebAccessAvailability> {
        let (settings, providers, permissions) = self
            .runtime_config(api_key_id)
            .await
            .map_err(anyhow::Error::new)?;
        if !settings.enabled || !permissions.api_key_enabled {
            self.release_run_snapshot(run_id);
            return Ok(WebAccessAvailability::default());
        }
        let engine = self
            .engine(&settings, &providers)
            .await
            .map_err(anyhow::Error::new)?;
        let availability = WebAccessAvailability {
            search: engine.has_search_providers(),
            fetch: engine.has_fetch_providers(),
        };
        if availability.search || availability.fetch {
            self.gateway.web_access_run_snapshots.insert(
                run_id.to_string(),
                WebAccessRunSnapshot {
                    api_key_id: api_key_id.to_string(),
                    engine,
                },
            );
        } else {
            self.release_run_snapshot(run_id);
        }
        Ok(availability)
    }

    pub(crate) fn release_run_snapshot(&self, run_id: &str) {
        self.gateway.web_access_run_snapshots.remove(run_id);
    }

    pub(super) async fn search_in_run(
        &self,
        run_id: &str,
        api_key_id: &str,
        request: SearchRequest,
    ) -> Result<SearchResponse, WebAccessError> {
        let snapshot = self.run_snapshot(run_id, api_key_id)?;
        snapshot.engine.search(request).await
    }

    pub(super) async fn fetch_in_run(
        &self,
        run_id: &str,
        api_key_id: &str,
        request: FetchRequest,
    ) -> Result<FetchResponse, WebAccessError> {
        let snapshot = self.run_snapshot(run_id, api_key_id)?;
        snapshot.engine.fetch(request).await
    }

    pub(super) fn run_snapshot(
        &self,
        run_id: &str,
        api_key_id: &str,
    ) -> Result<WebAccessRunSnapshot, WebAccessError> {
        self.gateway
            .web_access_run_snapshots
            .get(run_id)
            .filter(|snapshot| snapshot.api_key_id == api_key_id)
            .ok_or_else(|| {
                WebAccessError::from_code(
                    WebAccessErrorCode::Unavailable,
                    "Web Access runtime snapshot is unavailable",
                )
            })
    }

    pub(crate) async fn test_provider(&self, provider: WebProvider) -> Result<(), WebAccessError> {
        tokio::time::timeout(WEB_ACCESS_DEADLINE, self.test_provider_inner(provider))
            .await
            .map_err(|_| {
                WebAccessError::from_code(
                    WebAccessErrorCode::Timeout,
                    "Web Provider connectivity test deadline exceeded",
                )
            })?
    }

    async fn test_provider_inner(&self, provider: WebProvider) -> Result<(), WebAccessError> {
        let adapter = self.adapter(&provider).await?;
        if adapter.supports_search() {
            adapter
                .search(&SearchRequest {
                    query: "Stravia connectivity test".into(),
                    max_results: 1,
                    allowed_domains: vec![],
                    blocked_domains: vec![],
                })
                .await
                .map_err(|failure| WebAccessError::from_code(failure.code, failure.message))?;
        }
        if adapter.supports_fetch() {
            let test_url = "https://example.com/";
            let response = adapter
                .fetch(&FetchRequest {
                    urls: vec![test_url.into()],
                    max_characters: 1_000,
                })
                .await
                .map_err(|failure| WebAccessError::from_code(failure.code, failure.message))?;
            let valid = response.result.len() == 1
                && response.result[0].url == test_url
                && response.result[0].status == FetchStatus::Success;
            if !valid {
                return Err(WebAccessError::from_code(
                    WebAccessErrorCode::Unavailable,
                    "Web Provider Fetch connectivity test returned an invalid result",
                ));
            }
        }
        Ok(())
    }

    async fn runtime_config(&self, api_key_id: &str) -> Result<RuntimeConfig, WebAccessError> {
        let Some(store) = self.gateway.storage.web_providers() else {
            return Ok((
                WebAccessSettings::default(),
                std::collections::HashMap::new(),
                WebAccessApiKeyPermissions::default(),
            ));
        };
        let config = store.load_runtime_config(api_key_id).await.map_err(|_| {
            WebAccessError::from_code(
                WebAccessErrorCode::Unavailable,
                "Web Access configuration is unavailable",
            )
        })?;
        Ok((
            config.settings,
            config
                .web_providers
                .into_iter()
                .map(|provider| (provider.id.clone(), provider))
                .collect(),
            config.api_key_permissions,
        ))
    }
    async fn engine(
        &self,
        settings: &WebAccessSettings,
        records: &std::collections::HashMap<String, WebProvider>,
    ) -> Result<WebAccessEngine, WebAccessError> {
        let search = self
            .ordered_adapters(&settings.search_provider_ids, records)
            .await;
        let fetch = self
            .ordered_adapters(&settings.fetch_provider_ids, records)
            .await;
        Ok(WebAccessEngine::new(search, fetch))
    }
    async fn ordered_adapters(
        &self,
        ids: &[String],
        records: &std::collections::HashMap<String, WebProvider>,
    ) -> Vec<Arc<dyn WebProviderAdapter>> {
        let mut adapters = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(provider) = records.get(id) else {
                continue;
            };
            match self.adapter(provider).await {
                Ok(adapter) => adapters.push(adapter),
                Err(error) => adapters.push(Arc::new(UnavailableAdapter {
                    id: provider.id.clone(),
                    search: provider
                        .capabilities()
                        .is_some_and(|capability| capability.search),
                    fetch: provider
                        .capabilities()
                        .is_some_and(|capability| capability.fetch),
                    error,
                })),
            }
        }
        adapters
    }

    async fn adapter(
        &self,
        provider: &WebProvider,
    ) -> Result<Arc<dyn WebProviderAdapter>, WebAccessError> {
        use providers::AdapterConfig;
        let config = match provider.kind.as_str() {
            "exa" => AdapterConfig::Exa {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            "brave" => AdapterConfig::Brave {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            "tavily" => AdapterConfig::Tavily {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            "zhipu" => AdapterConfig::Zhipu {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            _ => {
                return Err(WebAccessError::from_code(
                    WebAccessErrorCode::Unsupported,
                    "unsupported Web Provider kind",
                ));
            }
        };
        Ok(providers::build_adapter(
            self.gateway.http_client.clone(),
            config,
        ))
    }
}

fn required_secret(provider: &WebProvider) -> Result<String, WebAccessError> {
    provider
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(unavailable_configuration)
}

fn unavailable_configuration() -> WebAccessError {
    WebAccessError::from_code(
        WebAccessErrorCode::Unavailable,
        "Web Provider configuration is unavailable",
    )
}

struct UnavailableAdapter {
    id: String,
    search: bool,
    fetch: bool,
    error: WebAccessError,
}

#[async_trait::async_trait]
impl WebProviderAdapter for UnavailableAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        self.search
    }

    fn supports_fetch(&self) -> bool {
        self.fetch
    }

    async fn search(
        &self,
        _request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        Err(ProviderFailure::new(
            self.error.code,
            self.error.message.clone(),
        ))
    }

    async fn fetch(
        &self,
        _request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        Err(ProviderFailure::new(
            self.error.code,
            self.error.message.clone(),
        ))
    }
}
