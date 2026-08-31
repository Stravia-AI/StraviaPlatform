use super::*;
use rust_decimal::prelude::ToPrimitive;

mod interface;
pub(crate) use interface::{
    ProviderConnection, ProviderConnectivityTest, ProviderReconnect, ProviderReconnectCallback,
    ProviderReconnectResult, ProviderReconnectStart, ProviderSave,
};

impl AdminService {
    // ── Providers ──

    pub fn preview_provider_base_url(
        &self,
        vendor_id: &str,
        credentials: std::collections::BTreeMap<String, String>,
        configured_base_url: Option<&str>,
    ) -> anyhow::Result<String> {
        ProviderConnection::new(self).preview_base_url(vendor_id, credentials, configured_base_url)
    }

    pub async fn list_providers(&self) -> anyhow::Result<Vec<Provider>> {
        ProviderConnection::new(self).list().await
    }

    pub async fn get_provider(&self, id: &str) -> anyhow::Result<Provider> {
        ProviderConnection::new(self).get(id).await
    }
    pub async fn provider_requires_oauth_session(
        &self,
        input: &CreateProvider,
    ) -> anyhow::Result<bool> {
        ProviderConnection::new(self)
            .requires_oauth_session(input)
            .await
    }

    pub async fn create_provider(&self, input: CreateProvider) -> anyhow::Result<Provider> {
        let save = match input.source {
            ProviderSourceInput::Catalog { .. } => {
                crate::admin::provider_connection::ProviderSave::Catalog {
                    input,
                    authorization_id: None,
                }
            }
            ProviderSourceInput::Custom { .. } => {
                crate::admin::provider_connection::ProviderSave::Custom(input)
            }
        };
        crate::admin::provider_connection::ProviderConnection::new(self)
            .save(save)
            .await
    }

    pub(super) async fn create_provider_from_input(
        &self,
        input: CreateProvider,
        allow_oauth: bool,
    ) -> anyhow::Result<Provider> {
        let record = self.resolve_create_provider(input, allow_oauth).await?;
        self.ensure_provider_name_unique(None, &record.name).await?;
        self.gw.storage.providers().create(record).await
    }

    async fn resolve_create_provider(
        &self,
        input: CreateProvider,
        allow_oauth: bool,
    ) -> anyhow::Result<CreateProviderRecord> {
        match input.source {
            ProviderSourceInput::Catalog {
                provider_id,
                channel_id,
                fingerprint,
                base_url_override,
            } => {
                let (provider, channel) = self
                    .gw
                    .provider_catalog
                    .resolve_channel(&provider_id, &channel_id, &fingerprint)
                    .await?;
                let name = normalize_name(
                    input.name.as_deref().unwrap_or(&provider.name),
                    "provider name",
                )?;
                let (credentials, auth_mode) = match (channel.auth_mode, input.credential) {
                    (
                        crate::provider_catalog::CatalogAuthMode::OptionalApiKey,
                        ProviderCredentialInput::ApiKey { value },
                    ) => (
                        std::collections::BTreeMap::from([("apiKey".to_string(), value)]),
                        "apikey".to_string(),
                    ),
                    (
                        crate::provider_catalog::CatalogAuthMode::OptionalApiKey,
                        ProviderCredentialInput::Fields { values },
                    ) => (
                        validate_adapter_credentials(&provider.vendor_id, values)?,
                        "apikey".to_string(),
                    ),
                    (
                        crate::provider_catalog::CatalogAuthMode::OptionalApiKey,
                        ProviderCredentialInput::None,
                    ) => (std::collections::BTreeMap::new(), "apikey".to_string()),
                    (
                        crate::provider_catalog::CatalogAuthMode::SetupToken,
                        ProviderCredentialInput::SetupToken { value },
                    ) => (
                        std::collections::BTreeMap::from([("apiKey".to_string(), value)]),
                        "apikey".to_string(),
                    ),
                    (
                        crate::provider_catalog::CatalogAuthMode::OAuth,
                        ProviderCredentialInput::None,
                    ) if allow_oauth => (std::collections::BTreeMap::new(), "oauth".to_string()),
                    (
                        crate::provider_catalog::CatalogAuthMode::OAuth,
                        ProviderCredentialInput::None,
                    ) => anyhow::bail!(
                        r#"{{"code":"AUTH_SESSION_REQUIRED","message":"OAuth providers must be created from a completed OAuth session"}}"#
                    ),
                    _ => anyhow::bail!(
                        "credential type is not allowed for catalog channel {provider_id}/{channel_id}"
                    ),
                };
                let adapter_credentials = serde_json::to_string(&credentials)?;
                let api_key = credentials.get("apiKey").cloned().unwrap_or_default();
                let selected_base_url = base_url_override
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(&channel.base_url);
                let assembled_base_url = assemble_vendor_base_url(
                    &provider.vendor_id,
                    &credentials,
                    Some(selected_base_url),
                )?;
                let base_url = validate_provider_base_url(&assembled_base_url)?;
                Ok(CreateProviderRecord {
                    name,
                    vendor: Some(provider.vendor_id),
                    protocol: channel.protocol,
                    base_url,
                    preset_key: Some(provider.id),
                    channel: Some(channel.id),
                    models_source: Some("catalog".to_string()),
                    static_models: None,
                    api_key,
                    adapter_credentials,
                    auth_mode,
                    use_proxy: input.use_proxy,
                })
            }
            ProviderSourceInput::Custom {
                vendor,
                protocol,
                base_url,
                models_source,
                static_models,
            } => {
                let name =
                    normalize_name(input.name.as_deref().unwrap_or_default(), "provider name")?;
                let vendor = normalize_vendor(vendor.as_deref());
                let credentials = match input.credential {
                    ProviderCredentialInput::ApiKey { value } => {
                        std::collections::BTreeMap::from([("apiKey".to_string(), value)])
                    }
                    ProviderCredentialInput::Fields { values } => {
                        validate_adapter_credentials(vendor.as_deref().unwrap_or("custom"), values)?
                    }
                    ProviderCredentialInput::None => std::collections::BTreeMap::new(),
                    ProviderCredentialInput::SetupToken { .. } => {
                        anyhow::bail!("setup token is only valid for a catalog channel")
                    }
                };
                let adapter_credentials = serde_json::to_string(&credentials)?;
                let api_key = credentials.get("apiKey").cloned().unwrap_or_default();
                Ok(CreateProviderRecord {
                    name,
                    vendor,
                    protocol,
                    base_url: validate_provider_base_url(&base_url)?,
                    preset_key: None,
                    channel: None,
                    models_source,
                    static_models,
                    api_key,
                    adapter_credentials,
                    auth_mode: "apikey".to_string(),
                    use_proxy: input.use_proxy,
                })
            }
        }
    }

    pub async fn copy_provider(&self, id: &str) -> anyhow::Result<Provider> {
        self.copy_provider_with_options(id, CopyProviderOptions::default())
            .await
    }

    pub async fn copy_provider_with_options(
        &self,
        id: &str,
        options: CopyProviderOptions,
    ) -> anyhow::Result<Provider> {
        ProviderConnection::new(self).copy(id, options).await
    }

    pub(super) async fn copy_provider_record(
        &self,
        id: &str,
        options: CopyProviderOptions,
    ) -> anyhow::Result<Provider> {
        let original = self.get_provider(id).await?;
        let name = self.next_provider_copy_name(&original.name).await?;
        let credential = if original.effective_auth_mode() == "oauth" {
            ProviderCredentialInput::None
        } else {
            let values: std::collections::BTreeMap<String, String> =
                serde_json::from_str(&original.adapter_credentials).unwrap_or_default();
            if !values.is_empty() {
                ProviderCredentialInput::Fields { values }
            } else if !original.api_key.is_empty() {
                ProviderCredentialInput::ApiKey {
                    value: original.api_key.clone(),
                }
            } else {
                ProviderCredentialInput::None
            }
        };
        let copied = ProviderConnection::new(self)
            .save(ProviderSave::Custom(CreateProvider {
                name: Some(name),
                source: ProviderSourceInput::Custom {
                    vendor: original.vendor.clone(),
                    protocol: original.protocol.clone(),
                    base_url: original.base_url.clone(),
                    models_source: original.models_source.clone(),
                    static_models: original.static_models.clone(),
                },
                credential,
                use_proxy: original.use_proxy,
            }))
            .await?;
        let copied = self
            .update_provider(
                &copied.id,
                UpdateProvider {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await?;

        let copied = if original.effective_auth_mode() == "oauth" {
            match self
                .gw
                .storage
                .oauth_credentials()
                .get(&original.id)
                .await?
            {
                Some(credential) => {
                    let credential_input = upsert_credential_from_oauth(&credential);
                    let provisioned = async {
                        self.gw
                            .storage
                            .oauth_credentials()
                            .upsert(&copied.id, credential_input)
                            .await?;
                        let driver_key = credential.driver_key.clone();
                        let stored = stored_credential_from_oauth(&credential, &driver_key);
                        self.sync_provider_runtime_fields(&copied, &stored).await
                    }
                    .await;

                    match provisioned {
                        Ok(provider) => provider,
                        Err(error) => {
                            if let Err(cleanup_error) = self.delete_provider(&copied.id).await {
                                tracing::warn!(
                                    "failed to rollback copied oauth provider {} after provisioning error: {}",
                                    copied.id,
                                    cleanup_error
                                );
                            }
                            return Err(error.context("copy oauth provider"));
                        }
                    }
                }
                None => copied,
            }
        } else {
            copied
        };

        if options.append_targets {
            super::routes::RouteModule::new(self)
                .copy_provider_targets(&original.id, &copied.id)
                .await?;
        }

        Ok(copied)
    }

    pub async fn update_provider(
        &self,
        id: &str,
        input: UpdateProvider,
    ) -> anyhow::Result<Provider> {
        ProviderConnection::new(self)
            .save(ProviderSave::Update {
                provider_id: id.to_string(),
                input,
            })
            .await
    }

    pub(super) async fn update_provider_record(
        &self,
        id: &str,
        input: UpdateProvider,
    ) -> anyhow::Result<Provider> {
        let current = self.get_provider(id).await?;
        let is_catalog =
            current.models_source.as_deref() == Some("catalog") && current.preset_key.is_some();
        let changes_identity = input
            .preset_key
            .as_deref()
            .is_some_and(|value| Some(value) != current.preset_key.as_deref())
            || input
                .channel
                .as_deref()
                .is_some_and(|value| Some(value) != current.channel.as_deref())
            || input
                .auth_mode
                .as_deref()
                .is_some_and(|value| value != current.auth_mode)
            || is_catalog
                && input
                    .protocol
                    .as_deref()
                    .is_some_and(|value| value != current.protocol)
            || is_catalog
                && input
                    .vendor
                    .as_deref()
                    .is_some_and(|value| Some(value) != current.vendor.as_deref())
            || is_catalog
                && input
                    .models_source
                    .as_deref()
                    .is_some_and(|value| Some(value) != current.models_source.as_deref())
            || is_catalog && input.static_models.is_some();
        if changes_identity {
            anyhow::bail!(
                "provider source, channel, protocol, and authentication cannot be changed after creation"
            );
        }
        let current_base_url = current.base_url.clone();
        let models_source_input = input.models_source.map(|value| value.trim().to_string());

        let name = normalize_name(&input.name.unwrap_or(current.name), "provider name")?;
        self.ensure_provider_name_unique(Some(id), &name).await?;
        let vendor = if input.vendor.is_some() {
            normalize_vendor(input.vendor.as_deref())
        } else {
            normalize_vendor(current.vendor.as_deref())
        };
        let models_source = models_source_input
            .or_else(|| current.models_source.as_deref().map(ToString::to_string));
        let protocol = input.protocol.unwrap_or(current.protocol);
        let requested_base_url = input.base_url;
        let preset_key = input.preset_key.or(current.preset_key);
        let channel = input.channel.or(current.channel);
        let static_models = input.static_models.or(current.static_models);
        let api_key_input = input.api_key.clone();
        let credential_vendor = vendor.as_deref().unwrap_or("custom");
        let adapter_credentials = match input.adapter_credentials {
            Some(values) => Some(validate_adapter_credentials(credential_vendor, values)?),
            None => api_key_input
                .as_ref()
                .map(|api_key| {
                    let mut values = serde_json::from_str::<
                        std::collections::BTreeMap<String, String>,
                    >(&current.adapter_credentials)
                    .unwrap_or_default();
                    values.insert("apiKey".to_string(), api_key.clone());
                    validate_adapter_credentials(credential_vendor, values)
                })
                .transpose()?,
        };
        let api_key = api_key_input.unwrap_or(current.api_key);
        let auth_mode = input.auth_mode.unwrap_or(current.auth_mode);
        let api_key = if auth_mode == "oauth" {
            Some(String::new())
        } else {
            Some(api_key)
        };
        let adapter_credentials = if auth_mode == "oauth" {
            Some(std::collections::BTreeMap::new())
        } else {
            adapter_credentials
        };
        let current_credentials =
            serde_json::from_str::<std::collections::BTreeMap<String, String>>(
                &current.adapter_credentials,
            )
            .unwrap_or_default();
        let next_credentials = adapter_credentials.as_ref().unwrap_or(&current_credentials);
        let current_derived_base_url =
            assemble_vendor_base_url(credential_vendor, &current_credentials, None)
                .and_then(|base_url| validate_provider_base_url(&base_url))
                .ok();
        let current_is_derived = current_derived_base_url
            .as_deref()
            .is_some_and(|value| value == current.base_url);
        let configured_base_url = requested_base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|value| *value != current.base_url || !current_is_derived)
            .or_else(|| (!current_is_derived).then_some(current.base_url.as_str()));
        let base_url = validate_provider_base_url(&assemble_vendor_base_url(
            credential_vendor,
            next_credentials,
            configured_base_url,
        )?)?;
        let use_proxy = input.use_proxy.unwrap_or(current.use_proxy);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
        let base_url_changed = base_url != current_base_url;

        let provider = self
            .gw
            .storage
            .providers()
            .update(
                id,
                UpdateProvider {
                    name: Some(name),
                    vendor,
                    protocol: Some(protocol),
                    base_url: Some(base_url),
                    preset_key,
                    channel,
                    models_source,
                    static_models,
                    api_key,
                    adapter_credentials,
                    auth_mode: Some(auth_mode),
                    use_proxy: Some(use_proxy),
                    is_enabled: Some(is_enabled),
                },
            )
            .await?;

        if base_url_changed {
            self.gw.clear_ollama_capability_cache_for_provider(id).await;
        }

        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        crate::admin::provider_connection::ProviderConnection::new(self)
            .delete(id)
            .await
    }

    pub(super) async fn delete_provider_record(&self, id: &str) -> anyhow::Result<()> {
        // ProviderStore owns the backend transaction that removes this
        // Provider, prunes its Targets, and deletes Routes left empty.
        self.gw.storage.providers().delete(id).await?;
        super::routes::RouteModule::new(self).reload_cache().await?;
        self.bump_config_epoch().await?;
        self.gw.clear_ollama_capability_cache_for_provider(id).await;
        Ok(())
    }

    async fn ensure_provider_name_unique(
        &self,
        exclude_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()> {
        if self
            .gw
            .storage
            .providers()
            .exists_by_name(name, exclude_id)
            .await?
        {
            return Err(coded_error(
                "PROVIDER_NAME_CONFLICT",
                &format!("provider name already exists: {name}"),
                serde_json::json!({ "name": name }),
            ));
        }
        Ok(())
    }

    async fn next_provider_copy_name(&self, original_name: &str) -> anyhow::Result<String> {
        let base = format!("{}_Copy", normalize_name(original_name, "provider name")?);
        if !self
            .gw
            .storage
            .providers()
            .exists_by_name(&base, None)
            .await?
        {
            return Ok(base);
        }

        for index in 2.. {
            let candidate = format!("{base}{index}");
            if !self
                .gw
                .storage
                .providers()
                .exists_by_name(&candidate, None)
                .await?
            {
                return Ok(candidate);
            }
        }

        unreachable!("unbounded provider copy name search must return");
    }

    pub async fn test_provider(&self, id: &str) -> anyhow::Result<TestResult> {
        crate::admin::provider_connection::ProviderConnection::new(self)
            .test(
                crate::admin::provider_connection::ProviderConnectivityTest::Existing(
                    id.to_string(),
                ),
            )
            .await
    }

    pub(super) async fn test_provider_record(&self, id: &str) -> anyhow::Result<TestResult> {
        let provider = self.get_provider(id).await?;
        self.gw
            .clear_ollama_capability_cache_for_provider(&provider.id)
            .await;
        let start = Instant::now();
        let protocol = provider.protocol.trim();
        let vertex_runtime = if google_vertex::is_vertex_vendor(&provider) {
            Some(self.resolve_provider_runtime(&provider).await?)
        } else {
            None
        };
        let base_url_owned = vertex_runtime
            .as_ref()
            .and_then(|runtime| runtime.binding.base_url_override.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| provider.base_url.clone());
        let base_url = base_url_owned.trim();

        let result = if base_url.is_empty() {
            TestResult {
                success: false,
                latency_ms: 0,
                model: None,
                error: Some("Base URL is empty".to_string()),
            }
        } else {
            let mut failures: Vec<String> = Vec::new();
            if reqwest::Url::parse(base_url).is_err() {
                failures.push(format!("{protocol}: Base URL format is invalid"));
            } else {
                let mut request = self
                    .gw
                    .http_client
                    .get(base_url)
                    .timeout(Duration::from_secs(10));
                if let Some(runtime) = &vertex_runtime {
                    let mut headers = runtime_binding_headers(&runtime.binding)?;
                    if !runtime.binding.disable_default_auth {
                        headers.insert(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {}", runtime.access_token))?,
                        );
                    }
                    request = request.headers(headers);
                }
                if let Err(e) = request.send().await {
                    failures.push(format!("{protocol}: {}", format_connectivity_error(&e)));
                }
            }

            if failures.is_empty() {
                TestResult {
                    success: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    model: None,
                    error: None,
                }
            } else {
                TestResult {
                    success: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    model: None,
                    error: Some(format!(
                        "Connectivity check failed for provider endpoint: {}",
                        failures.join("; ")
                    )),
                }
            }
        };
        self.record_provider_test_result(&provider.id, &result)
            .await?;
        Ok(result)
    }

    pub(super) async fn test_provider_candidate_record(
        &self,
        input: CreateProvider,
    ) -> anyhow::Result<TestResult> {
        let record = self.resolve_create_provider(input, false).await?;
        let start = Instant::now();
        let result = match self
            .gw
            .http_client
            .get(&record.base_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(_) => TestResult {
                success: true,
                latency_ms: start.elapsed().as_millis() as u64,
                model: None,
                error: None,
            },
            Err(error) => TestResult {
                success: false,
                latency_ms: start.elapsed().as_millis() as u64,
                model: None,
                error: Some(format!(
                    "Connectivity check failed for provider endpoint: {}: {}",
                    record.protocol,
                    format_connectivity_error(&error)
                )),
            },
        };
        Ok(result)
    }

    async fn record_provider_test_result(
        &self,
        provider_id: &str,
        result: &TestResult,
    ) -> anyhow::Result<()> {
        self.gw
            .storage
            .providers()
            .record_test_result(
                provider_id,
                ProviderTestResult {
                    success: result.success,
                    tested_at: String::new(),
                },
            )
            .await
    }

    pub(super) async fn preset_catalog_models_for_provider(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<Option<crate::provider_catalog::CatalogModelList>> {
        let Some(provider_id) = provider.preset_key.as_deref() else {
            return Ok(None);
        };
        let channel_id = provider.channel.as_deref().unwrap_or("default");
        self.gw
            .provider_catalog
            .models(provider_id, channel_id)
            .await
            .map(Some)
    }

    async fn catalog_models_for_provider(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<Option<crate::provider_catalog::CatalogModelList>> {
        if !uses_catalog_inventory(provider) {
            return Ok(None);
        }
        self.preset_catalog_models_for_provider(provider).await
    }

    pub async fn test_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        Ok(super::routes::RouteModule::new(self)
            .discover_provider_model_ids(id)
            .await?)
    }
    pub async fn get_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let provider = self.get_provider(id).await?;
        if let Some(catalog) = self.catalog_models_for_provider(&provider).await? {
            return Ok(catalog.models.into_iter().map(|model| model.id).collect());
        }
        let runtime = self.resolve_provider_runtime(&provider).await?;
        let credential = runtime.access_token.clone();
        if let Some(static_list) = runtime.binding.static_models_override.as_deref() {
            let models: Vec<String> = static_list
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !models.is_empty() {
                return Ok(models);
            }
        }
        let preset_static_models = preset_static_models(&provider);
        if !preset_static_models.is_empty() {
            return Ok(preset_static_models);
        }

        if let Some(endpoint) = runtime
            .binding
            .models_source_override
            .clone()
            .or_else(|| resolve_models_endpoint(&provider))
        {
            let mut headers = if runtime.binding.disable_default_auth {
                HeaderMap::new()
            } else {
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?
            };
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            let mut request = self.gw.http_client.get(&endpoint).headers(headers);

            if provider.protocol == "gemini" && !runtime.binding.disable_default_auth {
                let separator = if endpoint.contains('?') { '&' } else { '?' };
                let mut headers = build_model_headers(
                    &provider.protocol,
                    provider.vendor.as_deref(),
                    &credential,
                )?;
                headers.extend(runtime_binding_headers(&runtime.binding)?);
                request = self
                    .gw
                    .http_client
                    .get(format!("{endpoint}{separator}key={}", credential))
                    .headers(headers);
            }

            if let Ok(resp) = request.send().await
                && resp.status().is_success()
            {
                let json: Value = resp.json().await.unwrap_or_default();
                let models = extract_models_from_response(
                    &provider.protocol,
                    provider.vendor.as_deref(),
                    &json,
                );
                if !models.is_empty() {
                    return Ok(models);
                }
            }
        }

        Ok(parse_static_models(provider.static_models.as_deref()))
    }

    pub async fn get_model_capabilities(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let provider = self.get_provider(provider_id).await?;
        let trimmed_model = model.trim();
        if trimmed_model.is_empty() {
            anyhow::bail!("model cannot be empty");
        }
        if let Some(model) = self
            .gw
            .storage
            .provider_models()
            .get(provider_id, trimmed_model)
            .await?
        {
            let metadata = model.metadata;
            let limits = metadata.limit.unwrap_or_default();
            let modalities = metadata.modalities.unwrap_or_default();
            let prices = metadata.cost.map(|cost| cost.prices).unwrap_or_default();
            return Ok(ModelCapabilities {
                provider: provider
                    .preset_key
                    .clone()
                    .unwrap_or_else(|| provider.vendor.clone().unwrap_or_default()),
                model_id: trimmed_model.to_string(),
                context_window: limits.context.unwrap_or(0),
                embedding_length: None,
                output_max_tokens: limits.output,
                tool_call: metadata.tool_call.unwrap_or(false),
                reasoning: metadata.reasoning.unwrap_or(false),
                input_modalities: modalities.input,
                output_modalities: modalities.output,
                input_cost: prices.input.and_then(|value| value.to_f64()),
                output_cost: prices.output.and_then(|value| value.to_f64()),
            });
        }
        self.resolve_provider_model_capabilities(&provider, trimmed_model)
            .await
    }

    async fn resolve_provider_model_capabilities(
        &self,
        provider: &Provider,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        if let Some(catalog) = self.catalog_models_for_provider(provider).await?
            && let Some(model) = catalog.models.into_iter().find(|entry| entry.id == model)
        {
            return Ok(catalog_model_capabilities(
                provider
                    .preset_key
                    .clone()
                    .unwrap_or_else(|| provider.vendor.clone().unwrap_or_default()),
                model,
            ));
        }
        match preset_capabilities_source(provider) {
            CapabilitiesSource::Catalog(catalog_provider_id) => {
                let model = self
                    .gw
                    .provider_catalog
                    .model(catalog_provider_id, model)
                    .await?;
                Ok(catalog_model_capabilities(
                    catalog_provider_id.to_string(),
                    model,
                ))
            }
            CapabilitiesSource::Http(url) => {
                if is_ollama_show_endpoint(url) {
                    self.query_ollama_show_capability(url, model).await
                } else {
                    self.query_http_capability(provider, url, model).await
                }
            }
            CapabilitiesSource::Auto => {
                anyhow::bail!("Provider Model metadata is not available for this provider")
            }
        }
    }

    async fn query_http_capability(
        &self,
        provider: &Provider,
        url: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let runtime = self.resolve_provider_runtime(provider).await?;
        let credential = runtime.access_token;
        let mut headers = if runtime.binding.disable_default_auth {
            HeaderMap::new()
        } else {
            build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?
        };
        headers.extend(runtime_binding_headers(&runtime.binding)?);
        let mut request = self
            .gw
            .http_client
            .get(url)
            .headers(headers)
            .timeout(Duration::from_secs(10));

        if provider.protocol == "gemini" && !runtime.binding.disable_default_auth {
            let separator = if url.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?;
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            request = self
                .gw
                .http_client
                .get(format!("{url}{separator}key={}", credential))
                .headers(headers)
                .timeout(Duration::from_secs(10));
        }

        let resp = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            anyhow::bail!("capability source returned status {}", resp.status());
        }
        let json: Value = resp.json().await.unwrap_or_default();
        if let Some(cap) = parse_http_capability(&json, model) {
            return Ok(cap);
        }
        anyhow::bail!("no matched model capabilities found from capability source")
    }

    async fn query_ollama_show_capability(
        &self,
        url: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let resp = self
            .gw
            .http_client
            .post(url)
            .json(&serde_json::json!({ "name": model }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            anyhow::bail!("ollama /api/show returned status {}", resp.status());
        }
        let json: Value = resp.json().await.unwrap_or_default();
        Ok(parse_ollama_capability(&json, model))
    }
}

fn catalog_model_capabilities(
    provider: String,
    model: crate::provider_catalog::CatalogModel,
) -> ModelCapabilities {
    let capabilities = model.capabilities.unwrap_or_default();
    let limits = model.limits.unwrap_or_default();
    let cost = model.cost.unwrap_or_default();
    ModelCapabilities {
        provider,
        model_id: model.id,
        context_window: limits.context.unwrap_or(0),
        embedding_length: None,
        output_max_tokens: limits.output,
        tool_call: capabilities.tool_call,
        reasoning: capabilities.reasoning,
        input_modalities: capabilities.input_modalities,
        output_modalities: capabilities.output_modalities,
        input_cost: cost.input,
        output_cost: cost.output,
    }
}

fn validate_adapter_credentials(
    vendor_id: &str,
    values: std::collections::BTreeMap<String, String>,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let vendor = crate::provider::VendorRegistry::global()
        .get_vendor(vendor_id)
        .ok_or_else(|| anyhow::anyhow!("Vendor `{vendor_id}` is not installed"))?;
    vendor.validate_credentials(values)
}

fn assemble_vendor_base_url(
    vendor_id: &str,
    credentials: &std::collections::BTreeMap<String, String>,
    configured_base_url: Option<&str>,
) -> anyhow::Result<String> {
    crate::provider::VendorRegistry::global()
        .get_vendor(vendor_id)
        .ok_or_else(|| anyhow::anyhow!("Vendor `{vendor_id}` is not installed"))?
        .assemble_base_url(credentials, configured_base_url)
}

fn validate_provider_base_url(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| anyhow::anyhow!("Provider Base URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "Provider Base URL must contain only an HTTP(S) origin and optional base path"
        );
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

#[cfg(test)]
mod image_base_url_tests {
    use super::validate_provider_base_url;

    #[test]
    fn image_provider_base_url_rejects_credential_query_and_fragment_smuggling() {
        for value in [
            "https://user:secret@example.com/v1",
            "https://example.com/v1?api_key=secret",
            "https://example.com/v1#images",
        ] {
            assert!(validate_provider_base_url(value).is_err(), "{value}");
        }
        assert_eq!(
            validate_provider_base_url("https://example.com/base/v1/").unwrap(),
            "https://example.com/base/v1"
        );
    }
}
