use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteOwnedBuf, SqlitePoolOptions};
use tokio::sync::OnceCell;

/// Restore a real migrated schema into a new, independently writable database.
/// Migration/upgrade/restart tests deliberately keep their own initialization paths.
pub(crate) async fn migrated_sqlite_pool() -> anyhow::Result<SqlitePool> {
    static TEMPLATE: OnceCell<Vec<u8>> = OnceCell::const_new();
    let template = TEMPLATE
        .get_or_try_init(|| async {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await?;
            crate::migrations::migrate_sqlite(&pool, None).await?;
            let bytes = {
                let mut connection = pool.acquire().await?;
                connection.serialize(None).await?.to_vec()
            };
            pool.close().await;
            Ok::<_, anyhow::Error>(bytes)
        })
        .await?;
    // 与 Gateway 存储池相同：租约外只读，单元测试也能暴露门外写入。
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .require_write_lease()
        .connect("sqlite::memory:")
        .await?;
    #[expect(
        clippy::disallowed_methods,
        reason = "sqlite3_deserialize 不能在事务内执行"
    )]
    let mut connection = pool.acquire_write().await?;
    connection
        .deserialize(None, SqliteOwnedBuf::try_from(template.as_slice())?, false)
        .await?;
    connection.finish().await?;
    Ok(pool)
}

/// 测试用外部 Web Provider 工厂：通过 `GatewayBuilder::web_provider_factory` 注入真实注册缝。
pub(crate) struct StubWebProviderFactory {
    descriptor: stravia_web_access::WebProviderKindDescriptor,
    available: std::sync::Arc<std::sync::atomic::AtomicBool>,
    unavailable_reason: Option<String>,
    adapters: parking_lot::Mutex<
        std::collections::HashMap<String, std::sync::Arc<dyn stravia_web_access::WebProviderAdapter>>,
    >,
    recorded_outbounds:
        parking_lot::Mutex<Vec<(String, stravia_web_access::OutboundProxyMode)>>,
}

impl StubWebProviderFactory {
    pub(crate) fn new(kind: impl Into<String>, search: bool, fetch: bool) -> Self {
        Self {
            descriptor: stravia_web_access::WebProviderKindDescriptor {
                kind: kind.into(),
                capabilities: stravia_web_access::WebProviderCapabilities { search, fetch },
                requires_api_key: true,
                singleton: false,
                allow_delete: true,
            },
            available: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            unavailable_reason: None,
            adapters: parking_lot::Mutex::new(std::collections::HashMap::new()),
            recorded_outbounds: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn with_requires_api_key(mut self, requires: bool) -> Self {
        self.descriptor.requires_api_key = requires;
        self
    }

    pub(crate) fn with_singleton(mut self, singleton: bool) -> Self {
        self.descriptor.singleton = singleton;
        self
    }

    pub(crate) fn with_allow_delete(mut self, allow_delete: bool) -> Self {
        self.descriptor.allow_delete = allow_delete;
        self
    }

    pub(crate) fn with_availability(
        mut self,
        available: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reason: impl Into<String>,
    ) -> Self {
        self.available = available;
        self.unavailable_reason = Some(reason.into());
        self
    }

    pub(crate) fn set_adapter(
        &self,
        provider_id: impl Into<String>,
        adapter: std::sync::Arc<dyn stravia_web_access::WebProviderAdapter>,
    ) {
        self.adapters.lock().insert(provider_id.into(), adapter);
    }

    pub(crate) fn recorded_outbounds(&self) -> Vec<(String, stravia_web_access::OutboundProxyMode)> {
        self.recorded_outbounds.lock().clone()
    }
}

struct NoopTestAdapter {
    id: String,
    capabilities: stravia_web_access::WebProviderCapabilities,
}

#[async_trait::async_trait]
impl stravia_web_access::WebProviderAdapter for NoopTestAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        self.capabilities.search
    }

    fn supports_fetch(&self) -> bool {
        self.capabilities.fetch
    }

    async fn search(
        &self,
        request: &stravia_web_access::SearchRequest,
    ) -> Result<
        stravia_web_access::AdapterSuccess<stravia_web_access::SearchResponse>,
        stravia_web_access::ProviderFailure,
    > {
        Ok(stravia_web_access::AdapterSuccess::new(
            stravia_web_access::SearchResponse {
                mode: stravia_web_access::SearchMode::Index,
                query: request.query.clone(),
                results: vec![],
                answer: None,
                citations: None,
            },
            None,
        ))
    }

    async fn fetch(
        &self,
        request: &stravia_web_access::FetchRequest,
    ) -> Result<
        stravia_web_access::AdapterSuccess<Vec<stravia_web_access::FetchResult>>,
        stravia_web_access::ProviderFailure,
    > {
        Ok(stravia_web_access::AdapterSuccess::new(
            request
                .urls
                .iter()
                .map(|url| stravia_web_access::FetchResult {
                    url: url.clone(),
                    status: stravia_web_access::FetchStatus::Success,
                    title: None,
                    content: Some("ok".into()),
                    format: Some("markdown".into()),
                    truncated: false,
                    limitations: Vec::new(),
                    error: None,
                })
                .collect(),
            None,
        ))
    }
}

#[async_trait::async_trait]
impl stravia_web_access::WebProviderFactory for StubWebProviderFactory {
    fn descriptor(&self) -> stravia_web_access::WebProviderKindDescriptor {
        self.descriptor.clone()
    }

    async fn is_available(&self) -> bool {
        self.available.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn unavailable_reason(&self) -> (&'static str, String) {
        (
            "WEB_PROVIDER_UNAVAILABLE",
            self.unavailable_reason
                .clone()
                .unwrap_or_else(|| "stub provider unavailable".into()),
        )
    }

    fn build(
        &self,
        config: &stravia_web_access::WebProviderConfig,
        outbound: stravia_web_access::OutboundProxyMode,
    ) -> Result<
        std::sync::Arc<dyn stravia_web_access::WebProviderAdapter>,
        stravia_web_access::ProviderFailure,
    > {
        self.recorded_outbounds
            .lock()
            .push((config.id.clone(), outbound));
        if let Some(adapter) = self.adapters.lock().get(&config.id).cloned() {
            return Ok(adapter);
        }
        Ok(std::sync::Arc::new(NoopTestAdapter {
            id: config.id.clone(),
            capabilities: self.descriptor.capabilities,
        }))
    }
}
