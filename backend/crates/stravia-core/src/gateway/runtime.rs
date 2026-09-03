use super::*;

async fn run_provider_allowance_sampler<F, Fut>(
    cancellation: proxy::context::CancellationToken,
    period: Duration,
    mut sample: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return,
            _ = interval.tick() => {}
        }
        let result = tokio::select! {
            _ = cancellation.cancelled() => return,
            result = sample() => result,
        };
        if let Err(error) = result {
            tracing::warn!(error = ?error, "provider allowance background sample failed");
        }
    }
}

impl Gateway {
    pub fn builder(config: GatewayConfig) -> GatewayBuilder {
        GatewayBuilder::new(config)
    }

    pub async fn shutdown(&self) {
        self.lifecycle.shutdown().await;
    }

    pub async fn new(config: GatewayConfig) -> anyhow::Result<(Self, mpsc::Receiver<LogEntry>)> {
        let (storage_kind, storage, sqlite_pool, postgres_pool): StorageRuntime =
            match config.storage.backend {
                StorageBackendKind::Sqlite => {
                    let pool = db::init_pool(&config.data_dir).await?;
                    migrations::migrate_sqlite(&pool).await?;
                    let sqlite_storage = SqliteStorage::from_pool(pool.clone());
                    (
                        RuntimeStorageKind::Sqlite,
                        Arc::new(sqlite_storage),
                        Some(pool),
                        None,
                    )
                }
                StorageBackendKind::Postgres => {
                    let backend_config =
                        to_sql_backend_config(&config.storage.postgres, "postgres")?;
                    let postgres_storage = PostgresStorage::connect(backend_config).await?;
                    let pool = postgres_storage.pool().clone();
                    migrations::migrate_postgres(&pool).await?;
                    (
                        RuntimeStorageKind::Postgres,
                        Arc::new(postgres_storage),
                        None,
                        Some(pool),
                    )
                }
            };

        let health = storage.bootstrap().health().await?;
        if !health.can_connect {
            anyhow::bail!("selected storage backend is not reachable");
        }

        Self::from_storage_with_kind(config, storage, storage_kind, sqlite_pool, postgres_pool)
            .await
    }

    pub async fn from_storage(
        config: GatewayConfig,
        storage: DynStorage,
    ) -> anyhow::Result<(Self, mpsc::Receiver<LogEntry>)> {
        Self::from_storage_with_kind(config, storage, RuntimeStorageKind::Memory, None, None).await
    }

    async fn from_storage_with_kind(
        config: GatewayConfig,
        storage: DynStorage,
        storage_kind: RuntimeStorageKind,
        sqlite_pool: Option<SqlitePool>,
        postgres_pool: Option<Pool<Postgres>>,
    ) -> anyhow::Result<(Self, mpsc::Receiver<LogEntry>)> {
        let history_sqlite_pool = if sqlite_pool.is_none() && postgres_pool.is_none() {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await?;
            migrations::migrate_sqlite(&pool).await?;
            Some(pool)
        } else {
            sqlite_pool.clone()
        };
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        let responses_websocket_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .http1_only()
            .build()?;

        let model_cache = Arc::new(tokio::sync::RwLock::new(
            router::RouteCache::load(storage.routes()).await?,
        ));
        let health_registry = Arc::new(HealthRegistry::new());
        let ollama_capability_cache = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let provider_catalog = provider_catalog::ProviderCatalog::new(&config.data_dir)?;
        #[cfg(debug_assertions)]
        let wire_capture = {
            let capture = config
                .wire_capture_dir
                .clone()
                .map(wire_capture::WireCapture::new)
                .transpose()?;
            if let Some(directory) = &config.wire_capture_dir {
                tracing::warn!(
                    directory = %directory.display(),
                    "wire capture enabled; redacted headers and full request/response bodies will be written to disk"
                );
            }
            capture
        };

        let (log_tx, log_rx) = mpsc::channel(1024);
        let turn_chains: Arc<dyn turn_chain::TurnChainStore> =
            if let Some(pool) = history_sqlite_pool.as_ref() {
                Arc::new(turn_chain::SqlTurnChainStore::sqlite(pool.clone()))
            } else {
                Arc::new(turn_chain::SqlTurnChainStore::postgres(
                    postgres_pool
                        .as_ref()
                        .expect("Gateway requires a SQL history store")
                        .clone(),
                ))
            };
        let history_markers: Arc<dyn history_marker::HistoryMarkerStore> =
            if let Some(pool) = history_sqlite_pool.as_ref() {
                Arc::new(history_marker::SqlHistoryMarkerStore::sqlite(pool.clone()))
            } else {
                Arc::new(history_marker::SqlHistoryMarkerStore::postgres(
                    postgres_pool
                        .as_ref()
                        .expect("Gateway requires a SQL history store")
                        .clone(),
                ))
            };
        let agent_definitions = if let Some(pool) = sqlite_pool.as_ref() {
            agent::AgentDefinitionRegistry::sqlite(pool.clone())
        } else if let Some(pool) = postgres_pool.as_ref() {
            agent::AgentDefinitionRegistry::postgres(pool.clone())
        } else {
            agent::AgentDefinitionRegistry::default()
        };

        let (artifact_store, media_derivatives): (
            Option<Arc<dyn agent::ArtifactStore>>,
            Option<Arc<media::MediaDerivativeStore>>,
        ) = if let Some(pool) = sqlite_pool.as_ref() {
            let local = Arc::new(agent::LocalArtifactStore::sqlite(
                pool.clone(),
                config.data_dir.join("artifacts"),
            ));
            let artifacts: Arc<dyn agent::ArtifactStore> = local.clone();
            (
                Some(artifacts),
                Some(Arc::new(media::MediaDerivativeStore::sqlite(
                    pool.clone(),
                    local,
                ))),
            )
        } else if let Some(pool) = postgres_pool.as_ref() {
            let local = Arc::new(agent::LocalArtifactStore::postgres(
                pool.clone(),
                config.data_dir.join("artifacts"),
            ));
            let artifacts: Arc<dyn agent::ArtifactStore> = local.clone();
            (
                Some(artifacts),
                Some(Arc::new(media::MediaDerivativeStore::postgres(
                    pool.clone(),
                    local,
                ))),
            )
        } else {
            (None, None)
        };
        let generation_chains = generation_chain::GenerationChain::from_turn_chain(
            Arc::clone(&turn_chains),
            Duration::from_secs(7 * 24 * 60 * 60),
            artifact_store.clone(),
        )
        .with_history_markers(Arc::clone(&history_markers));
        let allowance_samples = match storage_kind {
            RuntimeStorageKind::Memory => admin::provider_allowance::AllowanceSampleStore::memory(),
            RuntimeStorageKind::Sqlite => admin::provider_allowance::AllowanceSampleStore::sqlite(
                sqlite_pool
                    .as_ref()
                    .expect("SQLite Gateway requires a SQLite pool")
                    .clone(),
            ),
            RuntimeStorageKind::Postgres => {
                admin::provider_allowance::AllowanceSampleStore::postgres(
                    postgres_pool
                        .as_ref()
                        .expect("PostgreSQL Gateway requires a PostgreSQL pool")
                        .clone(),
                )
            }
        };
        allowance_samples
            .cleanup_at(chrono::Utc::now().timestamp_millis())
            .await?;
        let mut gw = Self {
            config,
            storage,
            storage_kind,
            http_client,
            responses_websocket_client,
            provider_catalog,
            provider_allowance_state: admin::provider_allowance::ProviderAllowanceState::default(),
            allowance_samples,
            proxy_client_cache: Arc::new(tokio::sync::RwLock::new(None)),
            responses_websockets: proxy::client::ResponsesWebSocketRegistry::default(),
            model_cache,
            health_registry,
            cache_affinity: router::cache_affinity::CacheAffinity::default(),
            route_policy_state: router::RoutePolicyState::default(),
            ollama_capability_cache,
            log_tx,
            #[cfg(debug_assertions)]
            wire_capture,
            auth_sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            agent_definitions,
            artifact_store,
            media_run_snapshots: media::MediaRunSnapshotStore::default(),
            media_derivatives,
            media_understanding: Arc::new(tokio::sync::RwLock::new(None)),
            hook_runtime: HookRuntime::default(),
            mcp_registry: McpToolRegistry::default(),
            history_markers,
            turn_chains,
            generation_chains,
            model_turn: model_turn::unreachable_executor(),
            web_access_run_snapshots: web_access::WebAccessRunSnapshotStore::default(),
            web_search_runner_state: Arc::new(tokio::sync::RwLock::new(None)),
            web_search_config_lock: Arc::new(tokio::sync::Mutex::new(())),
            _sqlite_pool: sqlite_pool,
            _postgres_pool: postgres_pool,
            history_marker_execution_gate: Arc::new(tokio::sync::RwLock::new(())),
            lifecycle: Arc::new(GatewayLifecycle::new()),
            principal_admission: Arc::new(admission::PrincipalAdmission::new()),
            lifecycle_owner: true,
        };
        gw.install_model_turn();
        configure_gateway_extensions(&mut gw, Vec::new(), Vec::new(), Vec::new(), Vec::new())
            .await?;
        {
            let catalog = gw.provider_catalog.clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let initial_refresh = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    result = catalog.refresh() => result,
                };
                if let Err(error) = initial_refresh {
                    tracing::warn!(error = ?error, "provider catalog startup refresh failed");
                }

                let mut interval = tokio::time::interval(provider_catalog::REFRESH_INTERVAL);
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = interval.tick() => {}
                }
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    let refresh = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = catalog.refresh() => result,
                    };
                    if let Err(error) = refresh {
                        tracing::warn!(error = ?error, "provider catalog refresh failed");
                    }
                }
            });
        }

        {
            let gw_refresh = gw.background_clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(120));
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    let refresh = {
                        let admin = gw_refresh.admin();
                        tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = admin.refresh_oauth_providers() => result,
                        }
                    };
                    if let Err(error) = refresh {
                        tracing::warn!("background oauth refresh skipped: {error}");
                    }
                    let cleanup = {
                        let admin = gw_refresh.admin();
                        tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = admin.cleanup_auth_sessions() => result,
                        }
                    };
                    if let Err(error) = cleanup {
                        tracing::warn!("auth session cleanup skipped: {error}");
                    }
                }
            });
        }

        {
            let gw_sample = gw.background_clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                run_provider_allowance_sampler(
                    cancellation,
                    admin::provider_allowance::SAMPLE_INTERVAL,
                    move || {
                        let admin = gw_sample.admin();
                        let samples = gw_sample.allowance_samples.clone();
                        async move {
                            let refresh =
                                admin.list_provider_allowances().await.map(|_snapshots| ());
                            samples
                                .cleanup_at(chrono::Utc::now().timestamp_millis())
                                .await?;
                            refresh
                        }
                    },
                )
                .await;
            });
        }

        if !gw.config.config_poll_interval.is_zero() {
            let gw_poll = gw.background_clone();
            let poll_interval = gw.config.config_poll_interval;
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let initial_epoch = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    result = gw_poll.storage.settings().get(admin::settings::CONFIG_EPOCH_KEY) => result,
                };
                let mut known_epoch: i64 = initial_epoch
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);

                let mut interval = tokio::time::interval(poll_interval);
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = interval.tick() => {}
                }
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    let current_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = gw_poll.storage.settings().get(admin::settings::CONFIG_EPOCH_KEY) => result,
                    };
                    let current: i64 = match current_result {
                        Ok(val) => val.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0),
                        Err(error) => {
                            tracing::warn!("config epoch poll failed: {error}");
                            continue;
                        }
                    };

                    if current > known_epoch {
                        known_epoch = current;
                        let reload = tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = async {
                                gw_poll
                                    .model_cache
                                    .write()
                                    .await
                                    .reload(gw_poll.storage.routes())
                                    .await
                            } => result,
                        };
                        if let Err(error) = reload {
                            tracing::warn!("config epoch reload failed: {error}");
                        } else {
                            tracing::debug!("model_cache reloaded (epoch={current})");
                        }
                    }
                }
            });
        }

        {
            let turn_chains = Arc::clone(&gw.turn_chains);
            let history_markers = Arc::clone(&gw.history_markers);
            let artifact_store = gw.artifact_store.clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let mut interval = tokio::time::interval(STORE_SWEEP_INTERVAL);
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }

                    let turn_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = turn_chains.sweep_expired() => result,
                    };
                    if let Err(error) = turn_result {
                        tracing::warn!(error = ?error, "turn chain ttl cleanup failed");
                    }

                    let marker_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = history_markers.cleanup_expired() => result,
                    };
                    if let Err(error) = marker_result {
                        tracing::warn!(error = ?error, "history marker ttl cleanup failed");
                    }

                    if let Some(artifact_store) = artifact_store.as_ref() {
                        let artifact_result = tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = artifact_store.sweep_expired() => result,
                        };
                        if let Err(error) = artifact_result {
                            tracing::warn!(error = ?error, "artifact ttl cleanup failed");
                        }
                    }
                }
            });
        }

        Ok((gw, log_rx))
    }

    pub fn admin(&self) -> admin::AdminService {
        admin::AdminService::new(self.clone())
    }

    pub fn web_access(&self) -> web_access::WebAccessService {
        web_access::WebAccessService::new(self.clone())
    }

    pub async fn web_search_runner(&self) -> anyhow::Result<web_search::WebSearchRunner> {
        self.web_search_runner_state
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Web Search Runner is unavailable"))
    }

    pub fn hook_runtime(&self) -> &HookRuntime {
        &self.hook_runtime
    }

    pub fn agent_definitions(&self) -> &agent::AgentDefinitionRegistry {
        &self.agent_definitions
    }

    pub fn artifact_store(&self) -> Option<&Arc<dyn agent::ArtifactStore>> {
        self.artifact_store.as_ref()
    }

    pub async fn http_client_for_provider(
        &self,
        use_proxy: bool,
    ) -> anyhow::Result<reqwest::Client> {
        self.client_for_provider(use_proxy, false).await
    }

    pub(crate) async fn responses_websocket_client_for_provider(
        &self,
        use_proxy: bool,
    ) -> anyhow::Result<reqwest::Client> {
        self.client_for_provider(use_proxy, true).await
    }

    async fn client_for_provider(
        &self,
        use_proxy: bool,
        require_http1: bool,
    ) -> anyhow::Result<reqwest::Client> {
        let default_client = if require_http1 {
            &self.responses_websocket_client
        } else {
            &self.http_client
        };
        if !use_proxy {
            return Ok(default_client.clone());
        }

        let enabled = self
            .storage
            .settings()
            .get("proxy_enabled")
            .await?
            .as_deref()
            .map(parse_bool_setting)
            .unwrap_or(false);
        if !enabled {
            return Ok(default_client.clone());
        }

        let proxy_url = self
            .storage
            .settings()
            .get("proxy_url")
            .await?
            .unwrap_or_default()
            .trim()
            .to_string();
        if proxy_url.is_empty() {
            anyhow::bail!("proxy_url is empty");
        }

        let force_http1 = require_http1
            || self
                .storage
                .settings()
                .get("proxy_force_http1")
                .await?
                .as_deref()
                .map(parse_bool_setting)
                .unwrap_or(false);

        let cache_key = format!("{proxy_url}|{force_http1}");
        if let Some(cached) = self.proxy_client_cache.read().await.clone()
            && cached.cache_key == cache_key
        {
            return Ok(cached.client);
        }

        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300));
        if force_http1 {
            builder = builder.http1_only();
        }
        let client = builder.proxy(reqwest::Proxy::all(&proxy_url)?).build()?;

        *self.proxy_client_cache.write().await = Some(ProxyClientCache {
            cache_key,
            client: client.clone(),
        });
        Ok(client)
    }

    pub async fn get_ollama_capabilities_cached(
        &self,
        provider_id: &str,
        model: &str,
        ttl: Duration,
    ) -> Option<Vec<String>> {
        let key = format!("{provider_id}:{model}");
        let cache = self.ollama_capability_cache.read().await;
        cache.get(&key).and_then(|entry| {
            if entry.cached_at.elapsed() < ttl {
                Some(entry.capabilities.clone())
            } else {
                None
            }
        })
    }

    pub async fn set_ollama_capabilities_cache(
        &self,
        provider_id: &str,
        model: &str,
        capabilities: Vec<String>,
    ) {
        let key = format!("{provider_id}:{model}");
        let mut cache = self.ollama_capability_cache.write().await;
        cache.insert(
            key,
            CapabilityCacheEntry {
                capabilities,
                cached_at: Instant::now(),
            },
        );
    }

    pub async fn clear_ollama_capability_cache_for_provider(&self, provider_id: &str) {
        let prefix = format!("{provider_id}:");
        let mut cache = self.ollama_capability_cache.write().await;
        cache.retain(|k, _| !k.starts_with(&prefix));
    }
}

fn parse_bool_setting(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn to_sql_backend_config(
    config: &SqlStorageConfig,
    backend: &str,
) -> anyhow::Result<SqlBackendConfig> {
    let url = config
        .configured_url()
        .with_context(|| format!("{backend} backend selected but storage url is empty"))?;
    Ok(SqlBackendConfig {
        url,
        max_connections: config.max_connections,
        min_connections: config.min_connections,
        idle_timeout: config.idle_timeout,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn provider_allowance_sampler_waits_thirty_minutes_and_stops_on_shutdown() {
        let cancellation = proxy::context::CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let task_calls = Arc::clone(&calls);
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(run_provider_allowance_sampler(
            task_cancellation,
            admin::provider_allowance::SAMPLE_INTERVAL,
            move || {
                task_calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            },
        ));
        tokio::task::yield_now().await;

        tokio::time::advance(admin::provider_allowance::SAMPLE_INTERVAL - Duration::from_millis(1))
            .await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(admin::provider_allowance::SAMPLE_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        cancellation.cancel();
        task.await.expect("sampler should stop after cancellation");
    }
}
