use super::*;
use crate::plugin::network::VendorNetworkConfig;

const CLIENT_INSTALLATION_ID_KEY: &str = "client_installation_id";

async fn load_client_installation_id(
    settings: &dyn storage::SettingsStore,
) -> anyhow::Result<Arc<str>> {
    let stored = match settings
        .get(CLIENT_INSTALLATION_ID_KEY)
        .await
        .context("failed to load client installation identity")?
    {
        Some(stored) => stored,
        None => {
            let candidate = uuid::Uuid::new_v4().to_string();
            settings
                .get_or_insert(CLIENT_INSTALLATION_ID_KEY, &candidate)
                .await
                .context("failed to create client installation identity")?
        }
    };
    let parsed = uuid::Uuid::parse_str(&stored).context("invalid client installation identity")?;
    anyhow::ensure!(
        parsed.to_string() == stored && !parsed.is_nil(),
        "invalid client installation identity"
    );
    Ok(Arc::from(stored))
}

const FIRST_ALLOWANCE_SAMPLE_DELAY: Duration = Duration::from_secs(35 * 60);

struct SuspendedAllowanceProviders {
    ids: std::collections::HashSet<String>,
    refreshable: Vec<(String, Option<i64>)>,
}

async fn suspended_allowance_providers(
    gw: &Gateway,
) -> anyhow::Result<SuspendedAllowanceProviders> {
    let ids = gw
        .storage
        .providers()
        .allowance_suspended_provider_ids()
        .await?;
    let mut refreshable = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(provider) = gw.storage.providers().get(id).await? else {
            continue;
        };
        if !provider.is_enabled || provider.credential_invalid() {
            continue;
        }
        let reset_at = gw
            .storage
            .providers()
            .allowance_suspension(id)
            .await?
            .and_then(|state| state.earliest_reset_at);
        refreshable.push((id.clone(), reset_at));
    }
    Ok(SuspendedAllowanceProviders { ids, refreshable })
}

async fn run_allowance_suspension_lifecycle(gw: Gateway, startup: SuspendedAllowanceProviders) {
    let cancellation = gw.lifecycle.cancellation.clone();
    let admin = gw.admin();
    let mut fired = std::collections::HashMap::<String, i64>::new();
    for (id, reset_at) in startup.refreshable {
        if let Some(reset_at) = reset_at
            && reset_at <= chrono::Utc::now().timestamp_millis()
        {
            fired.insert(id.clone(), reset_at);
        }
        tokio::select! {
            _ = cancellation.cancelled() => return,
            result = admin.refresh_provider_allowance(&id) => {
                if let Err(error) = result { tracing::warn!(provider_id = %id, error = ?error, "startup allowance refresh failed"); }
            }
        }
    }
    loop {
        let suspended = match suspended_allowance_providers(&gw).await {
            Ok(suspended) => suspended,
            Err(error) => {
                tracing::warn!(error = ?error, "allowance suspension schedule load failed");
                // Resume on state changes or the existing sampling cadence;
                // never spin on a failing database or shorten upstream polling.
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = gw.provider_allowance_state.changed() => {},
                    _ = tokio::time::sleep(crate::admin::provider_allowance::SAMPLE_INTERVAL) => {},
                }
                continue;
            }
        };
        fired.retain(|id, _| suspended.ids.contains(id));
        let mut next = None::<(String, i64)>;
        for (id, reset_at) in suspended.refreshable {
            let Some(reset_at) = reset_at else { continue };
            if fired.get(&id) == Some(&reset_at) {
                continue;
            }
            if next.as_ref().is_none_or(|(_, time)| reset_at < *time) {
                next = Some((id, reset_at));
            }
        }
        let timer = async {
            if let Some((_, reset_at)) = &next {
                tokio::time::sleep(Duration::from_millis(
                    reset_at
                        .saturating_sub(chrono::Utc::now().timestamp_millis())
                        .max(0) as u64,
                ))
                .await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = cancellation.cancelled() => return,
            _ = gw.provider_allowance_state.changed() => continue,
            _ = timer => {}
        }
        if let Some((id, reset_at)) = next {
            fired.insert(id.clone(), reset_at);
            tokio::select! {
                _ = cancellation.cancelled() => return,
                result = admin.refresh_provider_allowance(&id) => {
                    if let Err(error) = result { tracing::warn!(provider_id = %id, error = ?error, "reset-time allowance refresh failed"); }
                }
            }
        }
    }
}

async fn run_provider_allowance_sampler<F, Fut>(
    cancellation: stravia_runtime_contract::CancellationToken,
    first_tick: tokio::time::Instant,
    period: Duration,
    mut sample: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut interval = tokio::time::interval_at(first_tick, period);
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

async fn open_runtime_cache(
    config: &GatewayConfig,
    storage_kind: RuntimeStorageKind,
) -> anyhow::Result<crate::runtime_cache::RuntimeCache> {
    match storage_kind {
        RuntimeStorageKind::Memory | RuntimeStorageKind::Sqlite => Ok(
            crate::runtime_cache::RuntimeCache::tinyufo(config.cache.capacity_bytes),
        ),
        RuntimeStorageKind::Postgres => {
            let url = config
                .cache
                .redis_url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| anyhow::anyhow!("PostgreSQL requires cache.redis_url"))?;
            crate::runtime_cache::RuntimeCache::redis(url, config.cache.capacity_bytes).await
        }
    }
}

async fn open_storage_runtime(config: &GatewayConfig) -> anyhow::Result<StorageRuntime> {
    crate::startup_progress::report("storage_connect", "Connecting to storage", 0, None);
    let root = crate::data_paths::resolve_data_dir(&config.data_dir)?;
    let paths = crate::data_paths::DataPaths::new(&root);
    paths.prepare()?;
    let diagnostics = paths.diagnostics();
    let (storage_kind, storage, sqlite_pool, postgres_pool): StorageRuntime =
        match config.storage.backend {
            StorageBackendKind::Sqlite => {
                let pool = db::init_pool(&config.data_dir).await?;
                migrations::migrate_sqlite(&pool, Some(&diagnostics)).await?;
                let sqlite_storage = SqliteStorage::from_pool(pool.clone());
                (
                    RuntimeStorageKind::Sqlite,
                    Arc::new(sqlite_storage),
                    Some(pool),
                    None,
                )
            }
            StorageBackendKind::Postgres => {
                let backend_config = to_sql_backend_config(&config.storage.postgres, "postgres")?;
                let postgres_storage = PostgresStorage::connect(backend_config).await?;
                let pool = postgres_storage.pool().clone();
                migrations::migrate_postgres(&pool, Some(&diagnostics)).await?;
                (
                    RuntimeStorageKind::Postgres,
                    Arc::new(postgres_storage),
                    None,
                    Some(pool),
                )
            }
        };

    crate::startup_progress::report("storage_connect", "Checking storage health", 0, None);
    let health = storage.bootstrap().health().await?;
    if !health.can_connect {
        anyhow::bail!("selected storage backend is not reachable");
    }

    Ok((storage_kind, storage, sqlite_pool, postgres_pool))
}

impl PreparedGateway {
    /// 借用已初始化的存储，供宿主在启动后台任务前检查或创建管理员。
    pub fn storage(&self) -> &DynStorage {
        &self.runtime.1
    }

    /// 使用保留的配置与连接池启动 Gateway；初始化错误原样返回。
    /// 不重新连接、迁移数据库，也不把持久存储当作内存存储装配。
    pub async fn start(self) -> anyhow::Result<Gateway> {
        let (kind, storage, sqlite_pool, postgres_pool) = self.runtime;
        Gateway::from_storage_with_kind(
            self.config,
            storage,
            kind,
            sqlite_pool,
            postgres_pool,
            None,
        )
        .await
    }
}

impl Gateway {
    /// 检查配置所需的运行时缓存，不执行 SQL 迁移或启动后台任务。
    /// SQLite 使用进程内缓存；PostgreSQL 的 Redis 地址或连接无效时返回脱敏错误。
    pub async fn check_runtime_cache(config: &GatewayConfig) -> anyhow::Result<()> {
        if config.storage.backend == StorageBackendKind::Postgres {
            open_runtime_cache(config, RuntimeStorageKind::Postgres).await?;
        }
        Ok(())
    }

    /// 准备配置指定的存储，执行迁移并检查连接，不启动 Gateway 后台任务。
    /// SQLite 会按需创建数据目录和数据库；连接、迁移或健康检查失败时返回错误。
    /// 返回值保留配置与连接池，可通过 `PreparedGateway::start` 继续启动。
    pub async fn prepare(mut config: GatewayConfig) -> anyhow::Result<PreparedGateway> {
        config.data_dir = crate::data_paths::resolve_data_dir(&config.data_dir)?;
        let runtime = open_storage_runtime(&config).await?;
        Ok(PreparedGateway { config, runtime })
    }

    pub fn builder(config: GatewayConfig) -> GatewayBuilder {
        GatewayBuilder::new(config)
    }

    /// 请求后台任务协作停止，不排空 HTTP 请求或关闭观测写入与缓存。
    /// 宿主停止接收请求后调用，排空请求后仍须等待 `shutdown` 完成清理。
    pub fn request_shutdown(&self) {
        self.lifecycle.cancellation.cancel();
    }

    #[cfg(test)]
    pub(crate) async fn stop_background_tasks_for_test(&self) {
        // 实调用验证必须先排空空实例的维护任务，再导入真实凭据；不关闭请求使用的观测与缓存。
        self.lifecycle.shutdown().await;
    }

    pub async fn shutdown(&self) {
        self.lifecycle.shutdown().await;
        self.observation.shutdown().await;
        self.runtime_cache.shutdown().await;
    }

    pub async fn new(config: GatewayConfig) -> anyhow::Result<Self> {
        Self::prepare(config).await?.start().await
    }

    pub async fn from_storage(config: GatewayConfig, storage: DynStorage) -> anyhow::Result<Self> {
        Self::from_storage_with_kind(
            config,
            storage,
            RuntimeStorageKind::Memory,
            None,
            None,
            None,
        )
        .await
    }

    #[cfg(test)]
    pub(super) async fn from_storage_with_vendor_network(
        config: GatewayConfig,
        storage: DynStorage,
        network: VendorNetworkConfig,
    ) -> anyhow::Result<Self> {
        Self::from_storage_with_kind(
            config,
            storage,
            RuntimeStorageKind::Memory,
            None,
            None,
            Some(network),
        )
        .await
    }

    async fn from_storage_with_kind(
        mut config: GatewayConfig,
        storage: DynStorage,
        storage_kind: RuntimeStorageKind,
        sqlite_pool: Option<SqlitePool>,
        postgres_pool: Option<Pool<Postgres>>,
        vendor_network: Option<VendorNetworkConfig>,
    ) -> anyhow::Result<Self> {
        // 注入持久存储时沿用同一连接池，避免历史与插件状态落入两个数据库。
        let sqlite_pool = sqlite_pool.or_else(|| storage.vendor_plugins().sqlite_pool().cloned());
        let postgres_pool =
            postgres_pool.or_else(|| storage.vendor_plugins().postgres_pool().cloned());
        let storage_kind = if postgres_pool.is_some() {
            RuntimeStorageKind::Postgres
        } else if sqlite_pool.is_some() {
            RuntimeStorageKind::Sqlite
        } else {
            storage_kind
        };
        crate::startup_progress::report("gateway_initialize", "Initializing gateway", 0, None);
        config.data_dir = crate::data_paths::resolve_data_dir(&config.data_dir)?;
        let runtime_cache = open_runtime_cache(&config, storage_kind).await?;
        let paths = crate::data_paths::DataPaths::new(&config.data_dir);
        paths.prepare()?;
        let history_sqlite_pool = if sqlite_pool.is_none() && postgres_pool.is_none() {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .require_write_lease()
                .connect("sqlite::memory:")
                .await?;
            migrations::migrate_sqlite(&pool, None).await?;
            Some(pool)
        } else {
            sqlite_pool.clone()
        };
        if sqlite_pool.is_none() && postgres_pool.is_none() {
            storage
                .vendor_plugins()
                .attach_atomic_history_pool(
                    history_sqlite_pool
                        .as_ref()
                        .expect("memory history pool")
                        .clone(),
                    storage.prompt_admissions().as_ref(),
                )
                .await?;
        }
        crate::startup_progress::report(
            "gateway_initialize",
            "Initializing gateway services",
            0,
            None,
        );
        // 直连路径不继承环境或系统代理；显式代理使用独立客户端。
        let http_client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        // Resolve networking before creating model-turn or background clones so
        // every consumer inherits the same trust, DNS and pool identities.
        let VendorNetworkConfig {
            shapes: vendor_shapes,
            resolver: vendor_resolver,
        } = match vendor_network {
            Some(network) => network,
            None => VendorNetworkConfig {
                shapes: Arc::new(
                    crate::plugin::network::shape::ShapeRegistry::builtin()
                        .context("failed to initialize vendor egress shapes")?,
                ),
                resolver: Arc::new(crate::plugin::network::transport::SystemResolver),
            },
        };
        let vendor_transport = Arc::new(crate::plugin::network::transport::VendorTransport::new());

        crate::startup_progress::report(
            "gateway_initialize",
            "Loading gateway configuration",
            0,
            None,
        );
        let client_installation_id = load_client_installation_id(storage.settings()).await?;
        let storage = if storage_kind == RuntimeStorageKind::Postgres {
            storage
        } else {
            crate::runtime_config::RuntimeConfig::attach(storage).await?
        };
        storage.vendor_plugins().attach_volatile_store(
            crate::plugin::volatile_state::VolatileStore::for_installation(&client_installation_id),
        );
        let model_cache = Arc::new(tokio::sync::RwLock::new(
            router::RouteCache::load(storage.routes()).await?,
        ));
        let rpm_setting = storage.settings().get(crate::rpm::SETTINGS_KEY).await?;
        let rpm_config = crate::rpm::RpmConfig::from_setting(rpm_setting.as_deref())?;
        let rpm_admission = crate::rpm::TargetAdmission::default();
        rpm_admission.configure(rpm_config);
        let provider_catalog = provider_catalog::ProviderCatalog::new(
            paths.catalog_root(),
            config.catalog_base_url.clone(),
        )?;
        let retention_setting = match storage.settings().get("log_retention_days").await {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!("history retention setting unavailable; using the default");
                None
            }
        };
        let retention =
            crate::history_retention::HistoryRetention::from_setting(retention_setting.as_deref());
        let turn_chains = if let Some(pool) = history_sqlite_pool.as_ref() {
            turn_chain::SqlTurnChainStore::sqlite(pool.clone(), retention.clone())
        } else {
            turn_chain::SqlTurnChainStore::postgres(
                postgres_pool
                    .as_ref()
                    .expect("Gateway requires a SQL history store")
                    .clone(),
                retention.clone(),
            )
        };
        let history_content = turn_chains.content_gc();
        let turn_chains: Arc<dyn stravia_runtime_contract::turn_chain::TurnChainStore> =
            Arc::new(turn_chains);
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
        history_markers.recover_public_executions().await?;
        let agent_definitions = if let Some(pool) = sqlite_pool.as_ref() {
            agent::AgentDefinitionRegistry::sqlite(pool.clone())
        } else if let Some(pool) = postgres_pool.as_ref() {
            agent::AgentDefinitionRegistry::postgres(pool.clone())
        } else {
            agent::AgentDefinitionRegistry::default()
        };
        let mappings: Arc<dyn stravia_credential_protection::store::MappingStore> =
            if let Some(pool) = history_sqlite_pool.as_ref() {
                Arc::new(
                    stravia_credential_protection::store::SqlMappingStore::sqlite(pool.clone()),
                )
            } else {
                Arc::new(
                    stravia_credential_protection::store::SqlMappingStore::postgres(
                        postgres_pool
                            .as_ref()
                            .expect("Gateway requires a SQL mapping store")
                            .clone(),
                    ),
                )
            };
        let custom_rules = Arc::new(if let Some(pool) = history_sqlite_pool.as_ref() {
            stravia_credential_protection::SqlCustomRuleStore::sqlite(pool.clone())
        } else {
            stravia_credential_protection::SqlCustomRuleStore::postgres(
                postgres_pool
                    .as_ref()
                    .expect("Gateway requires a SQL custom rule store")
                    .clone(),
            )
        });
        let redaction = crate::reversible_redaction::ReversibleRedaction::new(
            Arc::clone(&storage),
            mappings,
            custom_rules,
        );
        let upload_grants = Arc::new(
            agent::upload_grant::UploadGrantIssuer::load(
                history_sqlite_pool.as_ref(),
                postgres_pool.as_ref(),
            )
            .await?,
        );

        let (artifact_store, media_derivatives): (
            Option<Arc<dyn stravia_runtime_contract::artifact::ArtifactStore>>,
            Option<Arc<stravia_media::MediaDerivativeStore>>,
        ) = if let Some(pool) = sqlite_pool.as_ref() {
            let local = Arc::new(agent::LocalArtifactStore::sqlite(
                pool.clone(),
                paths.artifacts(),
            ));
            let artifacts: Arc<dyn stravia_runtime_contract::artifact::ArtifactStore> =
                local.clone();
            (
                Some(artifacts),
                Some(Arc::new(stravia_media::MediaDerivativeStore::sqlite(
                    pool.clone(),
                    Arc::new(media::ArtifactHost(local)),
                ))),
            )
        } else if let Some(pool) = postgres_pool.as_ref() {
            let local = Arc::new(agent::LocalArtifactStore::postgres(
                pool.clone(),
                paths.artifacts(),
            ));
            let artifacts: Arc<dyn stravia_runtime_contract::artifact::ArtifactStore> =
                local.clone();
            (
                Some(artifacts),
                Some(Arc::new(stravia_media::MediaDerivativeStore::postgres(
                    pool.clone(),
                    Arc::new(media::ArtifactHost(local)),
                ))),
            )
        } else {
            (None, None)
        };
        if let Some(store) = artifact_store.as_ref() {
            let settings = storage
                .settings()
                .get("artifact_settings")
                .await?
                .map(|value| {
                    serde_json::from_str::<stravia_runtime_contract::artifact::ArtifactSettings>(
                        &value,
                    )
                })
                .transpose()?
                .unwrap_or_default();
            store.configure(&settings).await?;
        }
        let compaction = if let Some(pool) = history_sqlite_pool.as_ref() {
            crate::compaction::Compaction::sqlite(pool.clone())
        } else {
            crate::compaction::Compaction::postgres(
                postgres_pool
                    .as_ref()
                    .expect("Gateway requires a SQL history store")
                    .clone(),
            )
        };
        let generation_chains = generation_chain::GenerationChain::from_turn_chain(
            Arc::clone(&turn_chains),
            artifact_store.clone(),
            runtime_cache.clone(),
            retention.clone(),
        )
        .with_history_markers(Arc::clone(&history_markers))
        .with_redaction_references()
        .with_prompt_admissions(storage.prompt_admissions())
        .with_atomic_state(storage.vendor_plugins().clone())
        .with_compaction(compaction.clone());
        generation_chains.rebuild_prefixes().await?;
        let observation = interaction_observation::InteractionObservation::new(
            history_sqlite_pool.clone(),
            postgres_pool.clone(),
            paths.diagnostics(),
            retention.clone(),
            !matches!(storage_kind, RuntimeStorageKind::Memory),
            generation_chains.clone(),
        )
        .await;
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
        let update_service = Arc::new(admin::updates::UpdateService::updater(
            Arc::clone(&storage),
            config.product_update_download_supported,
            config.update_source_url.clone(),
        )?);
        let vendor_plugins = crate::plugin::manager::VendorPlugins::open(
            storage.vendor_plugins().clone(),
            paths.plugins(),
            Some(crate::plugin::CompileCache {
                wasmtime: paths.wasmtime_cache(),
                admissions: paths.plugin_admissions(),
            }),
            Arc::clone(&vendor_shapes),
        )
        .await?;
        #[cfg(feature = "test-support")]
        let marketplace_client = if let Some(origin) = &config.test_plugin_marketplace_url {
            crate::plugin::marketplace::client::MarketClient::with_test_origins(
                Arc::clone(&storage),
                origin.clone(),
                origin.clone(),
                origin.clone(),
            )
            .map_err(|error| anyhow::anyhow!("invalid marketplace test origin: {error:?}"))?
        } else {
            crate::plugin::marketplace::client::MarketClient::new(
                Arc::clone(&storage),
                config.update_source_url.clone(),
            )
        };
        #[cfg(not(feature = "test-support"))]
        let marketplace_client = crate::plugin::marketplace::client::MarketClient::new(
            Arc::clone(&storage),
            config.update_source_url.clone(),
        );
        let marketplace = Arc::new(crate::plugin::marketplace::PluginMarketplace::new(
            Arc::new(storage.vendor_plugins().clone()),
            marketplace_client,
        ));
        if let Err(error) = marketplace.initialize_official().await {
            tracing::warn!(error = ?error, "plugin marketplace official source initialization failed");
        }
        let catalog_base_url = config.catalog_base_url.clone();
        let (browser_preferences, browser_path) =
            admin::browser::BrowserPreferences::load(&config.data_dir);
        let custom_currency = storage.money().custom_currency().await?;
        let mut gw = Self {
            config,
            storage,
            storage_kind,
            client_installation_id,
            runtime_cache: runtime_cache.clone(),
            http_client,
            vendor_transport: Arc::clone(&vendor_transport),
            vendor_shapes: Arc::clone(&vendor_shapes),
            vendor_resolver: Arc::clone(&vendor_resolver),
            marketplace,
            vendor_plugins: vendor_plugins.clone(),
            vendor_websocket_pool: Arc::new(crate::plugin::network::VendorWebSocketPool::default()),
            provider_catalog: provider_catalog.clone(),
            catalog_sync: crate::plugin::catalog_sync::VendorCatalogSync::new(
                vendor_plugins.clone(),
                provider_catalog,
                catalog_base_url,
                vendor_transport,
                vendor_resolver,
            ),
            provider_allowance_state: admin::provider_allowance::ProviderAllowanceState::new(
                runtime_cache.clone(),
            ),
            allowance_samples,
            model_cache,
            cache_affinity: router::cache_affinity::CacheAffinity::new(runtime_cache.clone()),
            route_policy_state: router::RoutePolicyState::new(runtime_cache),
            custom_currency: Arc::new(parking_lot::RwLock::new(custom_currency.map(Arc::new))),
            observation,
            retention: retention.clone(),
            history_content,
            auth_sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            agent_definitions,
            artifact_store,
            upload_grants,
            media_run_snapshots: stravia_media::MediaRunSnapshotStore::default(),
            media_derivatives,
            media_understanding: Arc::new(tokio::sync::RwLock::new(None)),
            hook_runtime: HookRuntime::default(),
            mcp_registry: McpToolRegistry::default(),
            history_markers,
            reasoning_rejections: history_marker::ReasoningRejections::default(),
            redaction,
            turn_chains,
            generation_chains,
            compaction,
            model_turn: model_turn::unreachable_executor(),
            web_access_run_snapshots: web_access::WebAccessRunSnapshotStore::default(),
            browser_path: Arc::new(std::sync::RwLock::new(browser_path)),
            browser_preferences: Arc::new(browser_preferences),
            web_search_runner_state: Arc::new(tokio::sync::RwLock::new(None)),
            web_search_config_lock: Arc::new(tokio::sync::Mutex::new(())),
            route_media_mutation: Arc::new(tokio::sync::Mutex::new(())),
            update_service,
            _sqlite_pool: sqlite_pool,
            _postgres_pool: postgres_pool,
            history_marker_execution_gate: Arc::new(tokio::sync::RwLock::new(())),
            lifecycle: Arc::new(GatewayLifecycle::new()),
            provider_model_sync: Arc::new(
                super::provider_model_sync::ProviderModelSyncRuntime::default(),
            ),
            oauth_refresh: Arc::new(crate::admin::OAuthRefreshRuntime::default()),
            principal_admission: Arc::new(admission::PrincipalAdmission::new()),
            rpm_admission,
            lifecycle_owner: true,
        };
        crate::startup_progress::report(
            "gateway_initialize",
            "Preparing provider integrations",
            0,
            None,
        );
        #[cfg(debug_assertions)]
        if let Some(manifest) = std::env::var_os("STRAVIA_DEV_VENDOR_MANIFEST") {
            crate::plugin::development::reconcile(&gw, std::path::Path::new(&manifest)).await?;
        } else {
            gw.vendor_plugins.reconcile_bundled(&gw).await?;
        }
        #[cfg(not(debug_assertions))]
        gw.vendor_plugins.reconcile_bundled(&gw).await?;
        crate::startup_progress::report("gateway_initialize", "Loading provider catalog", 0, None);
        if let Err(error) = gw.catalog_sync.bootstrap().await {
            tracing::warn!(error = ?error, "provider catalog bootstrap sync failed");
        }
        gw.install_model_turn();
        crate::startup_progress::report(
            "gateway_initialize",
            "Configuring gateway extensions",
            0,
            None,
        );
        configure_gateway_extensions(&mut gw, Vec::new(), Vec::new(), Vec::new(), Vec::new())
            .await?;
        // 两类周期任务共用起点，避免各自首次轮询的延迟改变错峰相位。
        let maintenance_started_at = tokio::time::Instant::now();
        gw.start_provider_model_sync(maintenance_started_at).await?;
        if gw.config.catalog_background_refresh {
            let catalog_sync = gw.catalog_sync.clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let initial_refresh = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    result = catalog_sync.refresh() => result,
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
                        result = catalog_sync.refresh() => result,
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
                let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    let refresh = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = gw_refresh.marketplace.refresh_due(&gw_refresh) => result,
                    };
                    if let Err(error) = refresh {
                        tracing::warn!(error = ?error, "plugin marketplace refresh failed");
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
                    maintenance_started_at + FIRST_ALLOWANCE_SAMPLE_DELAY,
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

        {
            // 在发布 Gateway 前冻结启动恢复集合，后台首次轮询不能纳入后来新增的暂停。
            let startup = suspended_allowance_providers(&gw).await?;
            let gw_suspension = gw.background_clone();
            gw.lifecycle.spawn(async move {
                run_allowance_suspension_lifecycle(gw_suspension, startup).await;
            });
        }

        if !gw.config.config_poll_interval.is_zero() {
            let gw_poll = gw.background_clone();
            let poll_interval = gw.config.config_poll_interval;
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let initial_epoch = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    result = gw_poll.storage.settings().get(crate::storage::CONFIG_EPOCH_KEY) => result,
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
                        result = gw_poll.storage.settings().get(crate::storage::CONFIG_EPOCH_KEY) => result,
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
                        // Provider Model prices change inside the same epoch
                        // bump: drop the shared pricing cache as soon as a new
                        // epoch is seen, so a failed route reload cannot leave
                        // stale prices behind once `known_epoch` advanced.
                        gw_poll.route_policy_state.clear_pricing();
                        let reload = tokio::select! {
                            _ = cancellation.cancelled() => return,
                            result = async {
                                let _reload = crate::rpm::CONFIG_WRITE.lock().await;
                                let value = gw_poll.storage.settings().get(crate::rpm::SETTINGS_KEY).await?;
                                let config = crate::rpm::RpmConfig::from_setting(value.as_deref())?;
                                let custom_currency =
                                    gw_poll.storage.money().custom_currency().await?;
                                gw_poll.apply_custom_currency(custom_currency);
                                gw_poll
                                    .model_cache
                                    .write()
                                    .await
                                    .reload(gw_poll.storage.routes())
                                    .await?;
                                gw_poll.rpm_admission.configure(config);
                                if let Some(keys) = gw_poll.storage.api_keys() {
                                    for key in keys.list().await? {
                                        gw_poll.principal_admission.set_limit(&key.id, key.rpm_limit);
                                    }
                                }
                                Ok::<(), anyhow::Error>(())
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
            let maintenance = gw.history_maintenance();
            let artifact_store = gw.artifact_store.clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let mut interval = tokio::time::interval(STORE_SWEEP_INTERVAL);
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }

                    let now = chrono::Utc::now().timestamp_millis();
                    let sweep = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = maintenance.sweep_history(now) => result,
                    };
                    if let Err(error) = sweep {
                        tracing::warn!(error = ?error, "history retention sweep failed");
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

        Ok(gw)
    }

    pub fn admin(&self) -> admin::AdminService {
        admin::AdminService::new(self.clone())
    }

    pub(crate) fn retention(&self) -> &crate::history_retention::HistoryRetention {
        &self.retention
    }

    pub(crate) fn history_maintenance(&self) -> crate::history_retention::HistoryMaintenance {
        crate::history_retention::HistoryMaintenance {
            retention: self.retention.clone(),
            observation: self.observation.clone(),
            turn_chain: Arc::clone(&self.turn_chains),
            prompt_admissions: self.storage.prompt_admissions(),
            atomic_state: self.storage.vendor_plugins().clone(),
            history_markers: Arc::clone(&self.history_markers),
            mappings: Arc::clone(&self.redaction.mappings),
            compaction: self.compaction.clone(),
            content: self.history_content.clone(),
        }
    }

    /// 设置保存后立即执行一轮扫描，让调小 N 立即生效。
    pub(crate) async fn sweep_history(&self) -> anyhow::Result<u64> {
        self.history_maintenance()
            .sweep_history(chrono::Utc::now().timestamp_millis())
            .await
    }

    /// Replace the in-memory currency snapshot after the management write has
    /// committed. Pricing snapshots already captured by in-flight attempts are
    /// not rewritten.
    pub(crate) fn apply_custom_currency(&self, currency: Option<crate::pricing::CustomCurrency>) {
        *self.custom_currency.write() = currency.map(Arc::new);
    }

    pub(crate) fn custom_currency_snapshot(&self) -> Option<Arc<crate::pricing::CustomCurrency>> {
        self.custom_currency.read().clone()
    }

    /// Root API Key spend admission. Reads one cumulative row and never writes.
    /// A configured limit fails closed when the row cannot be read; an
    /// unlimited Key skips the read entirely.
    pub(crate) async fn check_api_key_spend_limit(
        &self,
        api_key_id: &str,
        spend_limit_nanos: Option<i64>,
    ) -> Result<(), crate::error::GatewayError> {
        let Some(limit) = spend_limit_nanos else {
            return Ok(());
        };
        let spend = self
            .storage
            .money()
            .api_key_spend(api_key_id)
            .await
            .map_err(|error| {
                tracing::warn!(
                    api_key_id,
                    %error,
                    "API key spend could not be read; rejecting the root request"
                );
                crate::error::GatewayError::internal(anyhow::anyhow!(
                    "API key spend could not be read"
                ))
            })?;
        let spent = crate::pricing::display_nanos(
            spend.spent_custom_nanos,
            spend.spent_usd_unconverted_nanos,
            self.custom_currency_snapshot().as_deref(),
        )
        .ok_or_else(|| {
            tracing::warn!(
                api_key_id,
                "API key spend display amount overflowed; rejecting the root request"
            );
            crate::error::GatewayError::internal(anyhow::anyhow!(
                "API key spend could not be represented"
            ))
        })?;
        if spent >= limit {
            return Err(crate::error::GatewayError::ApiKeySpendLimitReached);
        }
        Ok(())
    }

    /// Change the browser path for this Gateway and its clones; active runs retain their snapshot.
    /// Does not persist or validate the path. Admin updates use the persistent API instead.
    pub fn set_browser_path(&self, path: Option<std::path::PathBuf>) {
        *self
            .browser_path
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
    }

    /// ADR-0073 收口：把一条上游凭据拒绝证据按条件写落库。代际已变（凭据
    /// 已被新证据替换）时不写；存储失败只记 warn，不改变原请求结果。
    pub(crate) async fn mark_provider_credential_invalid(
        &self,
        provider_id: &str,
        expected: crate::db::models::ProviderCredentialVersion,
    ) -> bool {
        match self
            .storage
            .providers()
            .mark_credential_invalid(provider_id, expected)
            .await
        {
            Ok(true) => {
                tracing::warn!(
                    provider_id,
                    "provider credentials marked invalid after upstream authentication rejection"
                );
                true
            }
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(provider_id, %error, "failed to record provider credential invalidation");
                false
            }
        }
    }

    /// ADR-0073 收口：新凭据证据出现时清除失效标记（OAuth 刷新成功等不改
    /// Provider 行的路径）；写失败同样只记 warn。
    pub(crate) async fn clear_provider_credential_invalid(&self, provider_id: &str) {
        if let Err(error) = self
            .storage
            .providers()
            .clear_credential_invalid(provider_id)
            .await
        {
            tracing::warn!(provider_id, %error, "failed to clear provider credential invalidation");
        }
    }

    pub fn web_access(&self) -> web_access::WebAccessService {
        web_access::WebAccessService::new(self.clone())
    }

    pub async fn web_search_runner(&self) -> anyhow::Result<stravia_web_search::WebSearchRunner> {
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

    pub fn artifact_store(
        &self,
    ) -> Option<&Arc<dyn stravia_runtime_contract::artifact::ArtifactStore>> {
        self.artifact_store.as_ref()
    }

    pub(crate) async fn vendor_proxy_snapshot(
        &self,
        use_proxy: bool,
    ) -> anyhow::Result<EffectiveVendorProxy> {
        if !use_proxy {
            return Ok(EffectiveVendorProxy::Direct);
        }
        let config =
            crate::outbound_proxy::OutboundProxySettings::load(self.storage.settings()).await?;
        config.validate()?;
        anyhow::ensure!(!config.url.trim().is_empty(), "outbound proxy URL is empty");
        Ok(EffectiveVendorProxy::Explicit {
            proxy_url: config.url,
            bypass: config.bypass,
            force_http1: config.force_http1,
        })
    }
}

pub(crate) enum EffectiveVendorProxy {
    Direct,
    Explicit {
        proxy_url: String,
        bypass: String,
        force_http1: bool,
    },
}

impl EffectiveVendorProxy {
    pub(crate) fn into_settings(self) -> crate::outbound_proxy::OutboundProxySettings {
        match self {
            Self::Direct => crate::outbound_proxy::OutboundProxySettings::default(),
            Self::Explicit {
                proxy_url,
                bypass,
                force_http1,
            } => crate::outbound_proxy::OutboundProxySettings {
                url: proxy_url,
                bypass,
                force_http1,
            },
        }
    }

    pub(crate) fn reuse_identity(&self) -> anyhow::Result<String> {
        let encoded = match self {
            Self::Direct => serde_json::to_vec(&"direct")?,
            Self::Explicit {
                proxy_url,
                bypass,
                force_http1,
            } => serde_json::to_vec(&("explicit", proxy_url, bypass, force_http1))?,
        };
        Ok(stravia_runtime_contract::protocol::ir::canonical::hash_hex(
            &stravia_runtime_contract::protocol::ir::canonical::hash_bytes(&encoded),
        ))
    }
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

    #[tokio::test]
    async fn installation_identity_survives_concurrent_initialization_and_restart()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("identity.sqlite"))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone())
            .await?;
        migrations::migrate_sqlite(&pool, None).await?;
        let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool.clone()));
        let first_dir = tempfile::tempdir()?;
        let second_dir = tempfile::tempdir()?;
        let first_config = GatewayConfig {
            data_dir: first_dir.path().to_path_buf(),
            ..Default::default()
        };
        let second_config = GatewayConfig {
            data_dir: second_dir.path().to_path_buf(),
            ..Default::default()
        };
        let (first, second) = tokio::join!(
            Gateway::from_storage(first_config.clone(), Arc::clone(&storage)),
            Gateway::from_storage(second_config, Arc::clone(&storage))
        );
        let first = first?;
        let second = second?;
        let identity = Arc::clone(&first.client_installation_id);
        assert_eq!(identity, second.client_installation_id);
        assert_eq!(
            uuid::Uuid::parse_str(&identity)?.to_string(),
            identity.as_ref()
        );
        first.shutdown().await;
        second.shutdown().await;
        drop(first);
        drop(second);
        drop(storage);
        pool.close().await;

        let reopened = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(options)
            .await?;
        let reopened_storage: DynStorage = Arc::new(SqliteStorage::from_pool(reopened.clone()));
        let restarted =
            Gateway::from_storage(first_config.clone(), Arc::clone(&reopened_storage)).await?;
        assert_eq!(identity, restarted.client_installation_id);
        restarted.shutdown().await;
        drop(restarted);

        let isolated_storage: DynStorage = Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let isolated = Gateway::from_storage(first_config.clone(), isolated_storage).await?;
        assert_ne!(identity, isolated.client_installation_id);
        isolated.shutdown().await;
        drop(isolated);

        reopened_storage
            .settings()
            .set(CLIENT_INSTALLATION_ID_KEY, "corrupt")
            .await?;
        let error = match Gateway::from_storage(first_config, Arc::clone(&reopened_storage)).await {
            Ok(_) => panic!("corrupt identity was accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("invalid client installation identity"));
        assert_eq!(
            reopened_storage
                .settings()
                .get(CLIENT_INSTALLATION_ID_KEY)
                .await?
                .as_deref(),
            Some("corrupt")
        );
        Ok(())
    }

    fn vendor_request(
        gateway: &Gateway,
        proxy: EffectiveVendorProxy,
        http1: bool,
        url: &str,
    ) -> anyhow::Result<Arc<dyn stravia_vendor_runtime::HostHttpResponse>> {
        use crate::plugin::{
            VendorOperationTracker,
            network::VendorNetwork,
            permissions::{NetworkOrigin, NetworkPolicy},
        };
        use stravia_vendor_sdk::net::{HttpVersion, Options, Request};

        let operation = VendorOperationTracker::default().begin("gateway-transport-test")?;
        let origin = url::Url::parse(url)?.origin().ascii_serialization();
        let network = VendorNetwork::new(
            Arc::clone(&gateway.vendor_transport),
            VendorNetworkConfig {
                shapes: Arc::clone(&gateway.vendor_shapes),
                resolver: Arc::clone(&gateway.vendor_resolver),
            },
            proxy.into_settings(),
            NetworkPolicy::compile([NetworkOrigin::Exact(origin)])?,
            operation,
            stravia_runtime_contract::CancellationToken::new(),
            "openai".into(),
        );
        let mut options = Options::new();
        options.http_version(if http1 {
            HttpVersion::Http1
        } else {
            HttpVersion::Auto
        })?;
        network
            .http_start(Request::new("GET", url), options)
            .map_err(|failure| anyhow::anyhow!(failure.message))
    }

    async fn vendor_text(
        gateway: &Gateway,
        proxy: EffectiveVendorProxy,
        http1: bool,
        url: &str,
    ) -> anyhow::Result<String> {
        let response = vendor_request(gateway, proxy, http1, url)?;
        assert_eq!(
            response
                .status()
                .await
                .map_err(|failure| anyhow::anyhow!(failure.message))?,
            200
        );
        let mut body = Vec::new();
        while let Some(chunk) = response
            .read_body()
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))?
        {
            body.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8(body)?)
    }

    #[tokio::test]
    async fn direct_and_explicit_vendor_clients_ignore_ambient_proxy() -> anyhow::Result<()> {
        const CHILD_ENV: &str = "STRAVIA_TEST_DIRECT_PROXY_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            // 环境代理在子进程中固定，避免修改全局环境污染并行测试。
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let proxy_url = format!("http://{}", listener.local_addr()?);
            let app = axum::Router::new().fallback(|| async { "ambient proxy" });
            let server = tokio::spawn(async move { axum::serve(listener, app).await });
            let test_name = std::thread::current()
                .name()
                .context("test thread has no name")?
                .to_owned();
            let mut command = tokio::process::Command::new(std::env::current_exe()?);
            command
                .args(["--exact", &test_name, "--nocapture"])
                .env(CHILD_ENV, "1")
                .env("NO_PROXY", "")
                .env("no_proxy", "");
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                command.env(key, &proxy_url);
            }
            let output = command.output().await;
            server.abort();
            let output = output?;
            anyhow::ensure!(
                output.status.success(),
                "isolated proxy regression failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }

        let origin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", origin_listener.local_addr()?);
        let origin_server = tokio::spawn(async move {
            axum::serve(
                origin_listener,
                axum::Router::new().fallback(|| async { "direct origin" }),
            )
            .await
        });
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let explicit = format!("http://{}", proxy_listener.local_addr()?);
        let explicit_server = tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                axum::Router::new().fallback(|| async { "explicit proxy" }),
            )
            .await
        });
        let directory = tempfile::tempdir()?;
        let gateway = Gateway::from_storage(
            GatewayConfig {
                data_dir: directory.path().to_path_buf(),
                catalog_base_url: None,
                ..Default::default()
            },
            Arc::new(crate::storage::MemoryStorage::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        )
        .await?;
        let result = async {
            for http1 in [false, true] {
                assert_eq!(
                    vendor_text(&gateway, EffectiveVendorProxy::Direct, http1, &origin).await?,
                    "direct origin"
                );
                let selected = EffectiveVendorProxy::Explicit {
                            proxy_url: explicit.clone(),
                            bypass: String::new(),
                            force_http1: http1,
                        };
                assert_eq!(
                    vendor_text(&gateway, selected, http1, &origin).await?,
                    "explicit proxy"
                );
                let bypassed = EffectiveVendorProxy::Explicit {
                    proxy_url: explicit.clone(), bypass: " NONMATCH.INVALID 127.0.0.1\tOTHER.INVALID; ".into(), force_http1: http1,
                };
                assert_eq!(vendor_text(&gateway, bypassed, http1, &origin).await?, "direct origin");
                let switched = EffectiveVendorProxy::Explicit {
                    proxy_url: explicit.clone(), bypass: String::new(), force_http1: http1,
                };
                assert_eq!(vendor_text(&gateway, switched, http1, &origin).await?, "explicit proxy");
            }
            let admin = gateway.admin();
            let saved = serde_json::to_string(&crate::outbound_proxy::OutboundProxySettings {
                url: explicit.clone(), bypass: "127.0.0.1".into(), force_http1: false,
            })?;
            admin.set_setting(crate::outbound_proxy::SETTINGS_KEY, &saved).await?;
            let invalid = serde_json::json!({"url":"ftp://user:secret@127.0.0.1:1","bypass":"","force_http1":true}).to_string();
            let error = admin.set_setting(crate::outbound_proxy::SETTINGS_KEY, &invalid).await.unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert_eq!(admin.get_setting(crate::outbound_proxy::SETTINGS_KEY).await?, Some(saved));
            assert_eq!(vendor_text(&gateway, gateway.vendor_proxy_snapshot(true).await?, false, &origin).await?, "direct origin");
            admin.set_setting(crate::outbound_proxy::SETTINGS_KEY, r#"{"url":"","bypass":"","force_http1":false}"#).await?;
            assert!(gateway.vendor_proxy_snapshot(true).await.is_err());
            assert_eq!(vendor_text(&gateway, gateway.vendor_proxy_snapshot(false).await?, false, &origin).await?, "direct origin");
            anyhow::Ok(())
        }
        .await;
        gateway.shutdown().await;
        origin_server.abort();
        explicit_server.abort();
        result
    }

    #[tokio::test]
    async fn vendor_stream_continues_past_three_hundred_seconds() -> anyhow::Result<()> {
        use axum::body::Body;
        use bytes::Bytes;
        use futures::StreamExt;

        let directory = tempfile::tempdir()?;
        let gateway = Gateway::from_storage(
            GatewayConfig {
                data_dir: directory.path().to_path_buf(),
                ..Default::default()
            },
            Arc::new(crate::storage::MemoryStorage::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        )
        .await?;
        let result = async {
            for proxied in [false, true] {
                for http1 in [false, true] {
                    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
                    let receiver = Arc::new(tokio::sync::Mutex::new(Some(receiver)));
                    let app = axum::Router::new().fallback(move || {
                        let receiver = Arc::clone(&receiver);
                        async move {
                            let stream = tokio_stream::wrappers::ReceiverStream::new(
                                receiver.lock().await.take().expect("one request"),
                            );
                            Body::from_stream(stream.map(Ok::<_, std::convert::Infallible>))
                        }
                    });
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                    let origin = format!("http://{}", listener.local_addr()?);
                    let server = tokio::spawn(async move { axum::serve(listener, app).await });
                    let proxy = if proxied {
                        EffectiveVendorProxy::Explicit {
                            proxy_url: origin.clone(),
                            bypass: String::new(),
                            force_http1: http1,
                        }
                    } else {
                        EffectiveVendorProxy::Direct
                    };
                    let request_url = if proxied {
                        "http://127.0.0.1:9/stream"
                    } else {
                        &origin
                    };
                    sender.send(Bytes::from_static(b"first")).await?;
                    let response = vendor_request(&gateway, proxy, http1, request_url)?;
                    assert_eq!(response.status().await.unwrap(), 200);
                    assert_eq!(response.read_body().await.unwrap().unwrap(), b"first");
                    let streamed = async {
                        for part in [b"second".as_slice(), b"third", b"after-300s"] {
                            // 只推进传输层时钟；读取真实 socket 时恢复时钟，
                            // 避免 Tokio 空闲自动跳时把网络调度误判成超时。
                            tokio::time::pause();
                            tokio::time::advance(Duration::from_secs(101)).await;
                            tokio::time::resume();
                            sender.send(Bytes::copy_from_slice(part)).await?;
                            assert_eq!(
                                response.read_body().await.unwrap().unwrap().as_slice(),
                                part
                            );
                        }
                        drop(sender);
                        assert!(response.read_body().await.unwrap().is_none());
                        anyhow::Ok(())
                    }
                    .await;
                    server.abort();
                    streamed?;
                }
            }
            anyhow::Ok(())
        }
        .await;
        gateway.shutdown().await;
        result
    }

    #[tokio::test]
    async fn storage_health_is_reachable_for_sqlite_gateway() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let gateway = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await?;
        let health = gateway.storage.bootstrap().health().await?;
        assert!(
            health.can_connect,
            "SQLite health check should report can_connect"
        );
        assert!(
            health.schema_compatible,
            "SQLite health check should report schema_compatible after migration"
        );
        gateway.shutdown().await;
        gateway
            ._sqlite_pool
            .as_ref()
            .expect("Gateway SQLite pool")
            .close()
            .await;
        drop(gateway);
        data_dir.close()?;
        Ok(())
    }

    #[tokio::test]
    async fn proxied_http_requests_reuse_connections_across_vendor_snapshots() -> anyhow::Result<()>
    {
        use axum::extract::{ConnectInfo, State};
        use std::collections::BTreeSet;
        use std::net::SocketAddr;

        let connections = Arc::new(tokio::sync::Mutex::new(BTreeSet::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let proxy_url = format!("http://{}", listener.local_addr()?);
        let app = axum::Router::new()
            .fallback(
                |ConnectInfo(peer): ConnectInfo<SocketAddr>,
                 State(connections): State<
                    Arc<tokio::sync::Mutex<BTreeSet<SocketAddr>>>,
                >| async move {
                    connections.lock().await.insert(peer);
                    "proxy response"
                },
            )
            .with_state(Arc::clone(&connections));
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        let directory = tempfile::tempdir()?;
        let gateway = Gateway::from_storage(
            GatewayConfig {
                data_dir: directory.path().to_path_buf(),
                ..Default::default()
            },
            Arc::new(crate::storage::MemoryStorage::new(
                Vec::new(),
                Vec::new(),
                vec![(
                    crate::outbound_proxy::SETTINGS_KEY.into(),
                    serde_json::to_string(&crate::outbound_proxy::OutboundProxySettings {
                        url: proxy_url,
                        ..Default::default()
                    })?,
                )],
            )),
        )
        .await?;
        let result = async {
            for _ in 0..2 {
                let proxy = gateway.vendor_proxy_snapshot(true).await?;
                let body = vendor_text(&gateway, proxy, false, "http://127.0.0.1:9/vendor").await?;
                assert_eq!(body, "proxy response");
            }
            assert_eq!(connections.lock().await.len(), 1);
            anyhow::Ok(())
        }
        .await;
        gateway.shutdown().await;
        server.abort();
        result
    }
}
