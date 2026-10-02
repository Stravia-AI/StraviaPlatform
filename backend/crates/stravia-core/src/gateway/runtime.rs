use super::*;

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

async fn run_allowance_suspension_lifecycle(gw: Gateway) {
    let cancellation = gw.lifecycle.cancellation.clone();
    let admin = gw.admin();
    let mut fired = std::collections::HashMap::<String, i64>::new();
    let mut startup = true;
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
        if startup {
            for (id, reset_at) in suspended.refreshable {
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
            startup = false;
            continue;
        }
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

impl Gateway {
    /// 打开配置指定的存储，执行迁移并检查连接，不启动 Gateway 后台任务。
    /// SQLite 会按需创建数据目录和数据库；连接、迁移或健康检查失败时返回错误。
    pub async fn open_storage(config: &GatewayConfig) -> anyhow::Result<DynStorage> {
        let (_, storage, _, _) = open_storage_runtime(config).await?;
        Ok(storage)
    }

    pub fn builder(config: GatewayConfig) -> GatewayBuilder {
        GatewayBuilder::new(config)
    }

    pub async fn shutdown(&self) {
        self.lifecycle.shutdown().await;
        self.observation.shutdown().await;
    }

    pub async fn new(mut config: GatewayConfig) -> anyhow::Result<Self> {
        config.data_dir = crate::data_paths::resolve_data_dir(&config.data_dir)?;
        let (storage_kind, storage, sqlite_pool, postgres_pool) =
            open_storage_runtime(&config).await?;
        Self::from_storage_with_kind(config, storage, storage_kind, sqlite_pool, postgres_pool)
            .await
    }

    pub async fn from_storage(config: GatewayConfig, storage: DynStorage) -> anyhow::Result<Self> {
        Self::from_storage_with_kind(config, storage, RuntimeStorageKind::Memory, None, None).await
    }

    async fn from_storage_with_kind(
        mut config: GatewayConfig,
        storage: DynStorage,
        storage_kind: RuntimeStorageKind,
        sqlite_pool: Option<SqlitePool>,
        postgres_pool: Option<Pool<Postgres>>,
    ) -> anyhow::Result<Self> {
        crate::startup_progress::report("gateway_initialize", "Initializing gateway", 0, None);
        config.data_dir = crate::data_paths::resolve_data_dir(&config.data_dir)?;
        let paths = crate::data_paths::DataPaths::new(&config.data_dir);
        paths.prepare()?;
        let history_sqlite_pool = if sqlite_pool.is_none() && postgres_pool.is_none() {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await?;
            migrations::migrate_sqlite(&pool, None).await?;
            Some(pool)
        } else {
            sqlite_pool.clone()
        };
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
        // 供应商操作的共享可续期 deadline 管理整个操作；reqwest 总超时
        // 不随流式活动续期，会截断仍持续输出的长响应。
        let vendor_http_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let vendor_websocket_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .http1_only()
            .build()?;

        crate::startup_progress::report(
            "gateway_initialize",
            "Loading gateway configuration",
            0,
            None,
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
        let retention_days = match storage.settings().get("log_retention_days").await {
            Ok(value) => value
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(7),
            Err(_) => {
                tracing::warn!("observation retention setting unavailable; using seven days");
                7
            }
        };
        let (turn_chains, sqlite_write_gate) = if let Some(pool) = history_sqlite_pool.as_ref() {
            let gate = Arc::new(tokio::sync::Mutex::new(()));
            (
                turn_chain::SqlTurnChainStore::sqlite(pool.clone(), Arc::clone(&gate)),
                Some(gate),
            )
        } else {
            (
                turn_chain::SqlTurnChainStore::postgres(
                    postgres_pool
                        .as_ref()
                        .expect("Gateway requires a SQL history store")
                        .clone(),
                ),
                None,
            )
        };
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
            Duration::from_secs(7 * 24 * 60 * 60),
            artifact_store.clone(),
        )
        .with_history_markers(Arc::clone(&history_markers))
        .with_redaction_mappings(Arc::clone(&redaction.mappings))
        .with_compaction(compaction.clone());
        generation_chains.rebuild_prefixes().await?;
        let observation = interaction_observation::InteractionObservation::new(
            history_sqlite_pool.clone(),
            postgres_pool.clone(),
            paths.diagnostics(),
            retention_days,
            !matches!(storage_kind, RuntimeStorageKind::Memory),
            generation_chains.clone(),
            sqlite_write_gate,
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
        let update_service = Arc::new(admin::updates::UpdateService::github(
            Arc::clone(&storage),
            config.product_update_download_supported,
        )?);
        let vendor_plugins = crate::plugin::manager::VendorPlugins::open(
            storage.vendor_plugins().clone(),
            crate::data_paths::DataPaths::new(&config.data_dir).plugins(),
        )
        .await?;
        let catalog_base_url = config.catalog_base_url.clone();
        let (browser_preferences, browser_path) =
            admin::browser::BrowserPreferences::load(&config.data_dir);
        let mut gw = Self {
            config,
            storage,
            storage_kind,
            http_client,
            vendor_http_client: vendor_http_client.clone(),
            vendor_websocket_client: vendor_websocket_client.clone(),
            vendor_plugins: vendor_plugins.clone(),
            vendor_websocket_pool: Arc::new(crate::plugin::network::VendorWebSocketPool::default()),
            provider_catalog: provider_catalog.clone(),
            catalog_sync: crate::plugin::catalog_sync::VendorCatalogSync::new(
                vendor_plugins.clone(),
                provider_catalog,
                catalog_base_url,
                vendor_http_client.clone(),
                vendor_websocket_client.clone(),
            ),
            provider_allowance_state: admin::provider_allowance::ProviderAllowanceState::default(),
            allowance_samples,
            vendor_client_cache: Arc::new(tokio::sync::RwLock::new([None, None])),
            model_cache,
            cache_affinity: router::cache_affinity::CacheAffinity::default(),
            route_policy_state: router::RoutePolicyState::default(),
            observation,
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
            update_service,
            _sqlite_pool: sqlite_pool,
            _postgres_pool: postgres_pool,
            history_marker_execution_gate: Arc::new(tokio::sync::RwLock::new(())),
            lifecycle: Arc::new(GatewayLifecycle::new()),
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

        {
            let gw_suspension = gw.background_clone();
            gw.lifecycle.spawn(async move {
                run_allowance_suspension_lifecycle(gw_suspension).await;
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
            let turn_chains = Arc::clone(&gw.turn_chains);
            let compaction = gw.compaction.clone();
            let history_markers = Arc::clone(&gw.history_markers);
            let mappings = Arc::clone(&gw.redaction.mappings);
            let artifact_store = gw.artifact_store.clone();
            let observation = gw.observation.clone();
            let cancellation = gw.lifecycle.cancellation.clone();
            gw.lifecycle.spawn(async move {
                let mut interval = tokio::time::interval(STORE_SWEEP_INTERVAL);
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = interval.tick() => {}
                    }

                    let observation_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = observation.sweep_retention() => result,
                    };
                    if observation_result.is_err() {
                        tracing::warn!("observation retention cleanup incomplete");
                    }

                    let compaction_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = compaction.cleanup_expired() => result,
                    };
                    if compaction_result.is_err() {
                        tracing::warn!("native compaction retention cleanup failed");
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
                    let mapping_result = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = mappings.cleanup_expired() => result,
                    };
                    if mapping_result.is_err() {
                        tracing::warn!("reversible redaction retention cleanup failed");
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
    ) {
        match self
            .storage
            .providers()
            .mark_credential_invalid(provider_id, expected)
            .await
        {
            Ok(true) => tracing::warn!(
                provider_id,
                "provider credentials marked invalid after upstream authentication rejection"
            ),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(provider_id, %error, "failed to record provider credential invalidation")
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

    pub(crate) async fn vendor_client_snapshot(
        &self,
        use_proxy: bool,
    ) -> anyhow::Result<VendorClientSnapshot> {
        let proxy = self.effective_vendor_proxy(use_proxy).await?;
        let http = self.client_for_vendor(&proxy, false).await?;
        let websocket = self.client_for_vendor(&proxy, true).await?;
        Ok(VendorClientSnapshot {
            http,
            websocket,
            websocket_reuse_identity: proxy.reuse_identity()?,
        })
    }

    async fn effective_vendor_proxy(
        &self,
        use_proxy: bool,
    ) -> anyhow::Result<EffectiveVendorProxy> {
        if !use_proxy {
            return Ok(EffectiveVendorProxy::Direct { use_proxy: false });
        }
        let settings = self.storage.settings();
        let enabled = settings
            .get("proxy_enabled")
            .await?
            .as_deref()
            .map(parse_bool_setting)
            .unwrap_or(false);
        if !enabled {
            return Ok(EffectiveVendorProxy::Direct { use_proxy: true });
        }
        let proxy_url = settings
            .get("proxy_url")
            .await?
            .unwrap_or_default()
            .trim()
            .to_string();
        if proxy_url.is_empty() {
            anyhow::bail!("proxy_url is empty");
        }
        let force_http1 = settings
            .get("proxy_force_http1")
            .await?
            .as_deref()
            .map(parse_bool_setting)
            .unwrap_or(false);
        Ok(EffectiveVendorProxy::Explicit {
            proxy_url,
            force_http1,
        })
    }

    async fn client_for_vendor(
        &self,
        proxy: &EffectiveVendorProxy,
        require_http1: bool,
    ) -> anyhow::Result<reqwest::Client> {
        let default_client = if require_http1 {
            &self.vendor_websocket_client
        } else {
            &self.vendor_http_client
        };
        let EffectiveVendorProxy::Explicit {
            proxy_url,
            force_http1,
        } = proxy
        else {
            return Ok(default_client.clone());
        };
        let force_http1 = require_http1 || *force_http1;
        let cache_key = format!("{proxy_url}|{force_http1}");
        let slot = usize::from(force_http1);
        let mut cache = self.vendor_client_cache.write().await;
        if let Some(cached) = &cache[slot]
            && cached.cache_key == cache_key
        {
            return Ok(cached.client.clone());
        }

        let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        if force_http1 {
            builder = builder.http1_only();
        }
        let client = builder.proxy(reqwest::Proxy::all(proxy_url)?).build()?;

        cache[slot] = Some(VendorClientCache {
            cache_key,
            client: client.clone(),
        });
        Ok(client)
    }
}

enum EffectiveVendorProxy {
    Direct {
        use_proxy: bool,
    },
    Explicit {
        proxy_url: String,
        force_http1: bool,
    },
}

impl EffectiveVendorProxy {
    fn reuse_identity(&self) -> anyhow::Result<String> {
        let encoded = match self {
            Self::Direct { use_proxy } => serde_json::to_vec(&("direct", use_proxy))?,
            Self::Explicit {
                proxy_url,
                force_http1,
            } => serde_json::to_vec(&("explicit", proxy_url, force_http1))?,
        };
        Ok(stravia_runtime_contract::protocol::ir::canonical::hash_hex(
            &stravia_runtime_contract::protocol::ir::canonical::hash_bytes(&encoded),
        ))
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
                let direct = gateway
                    .client_for_vendor(&EffectiveVendorProxy::Direct { use_proxy: false }, http1)
                    .await?;
                assert_eq!(
                    direct.get(&origin).send().await?.text().await?,
                    "direct origin"
                );
                let selected = gateway
                    .client_for_vendor(
                        &EffectiveVendorProxy::Explicit {
                            proxy_url: explicit.clone(),
                            force_http1: http1,
                        },
                        http1,
                    )
                    .await?;
                assert_eq!(
                    selected.get(&origin).send().await?.text().await?,
                    "explicit proxy"
                );
            }
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
                            force_http1: http1,
                        }
                    } else {
                        EffectiveVendorProxy::Direct { use_proxy: false }
                    };
                    let client = gateway.client_for_vendor(&proxy, http1).await?;
                    let request_url = if proxied {
                        "http://127.0.0.1:9/stream"
                    } else {
                        &origin
                    };
                    sender.send(Bytes::from_static(b"first")).await?;
                    let mut response = client.get(request_url).send().await?;
                    assert_eq!(response.chunk().await?.unwrap(), "first");
                    let streamed = async {
                        for part in [b"second".as_slice(), b"third", b"after-300s"] {
                            // 只推进 reqwest 的时钟；读取真实 socket 时恢复时钟，
                            // 避免 Tokio 空闲自动跳时把网络调度误判成超时。
                            tokio::time::pause();
                            tokio::time::advance(Duration::from_secs(101)).await;
                            tokio::time::resume();
                            sender.send(Bytes::copy_from_slice(part)).await?;
                            assert_eq!(response.chunk().await?.unwrap().as_ref(), part);
                        }
                        drop(sender);
                        assert!(response.chunk().await?.is_none());
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
                vec![
                    ("proxy_enabled".into(), "true".into()),
                    ("proxy_url".into(), proxy_url),
                ],
            )),
        )
        .await?;
        let result = async {
            for _ in 0..2 {
                let clients = gateway.vendor_client_snapshot(true).await?;
                let body = clients
                    .http
                    .get("http://127.0.0.1:9/vendor")
                    .send()
                    .await?
                    .text()
                    .await?;
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

    #[tokio::test(start_paused = true)]
    async fn provider_allowance_sampler_waits_thirty_minutes_and_stops_on_shutdown() {
        let cancellation = stravia_runtime_contract::CancellationToken::new();
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
