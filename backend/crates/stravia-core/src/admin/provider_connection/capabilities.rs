use super::*;

fn provider_get_request(
    gateway: &crate::Gateway,
    provider: &Provider,
    runtime: &crate::admin::ResolvedProviderRuntime,
    endpoint: &str,
) -> anyhow::Result<reqwest::RequestBuilder> {
    let mut headers = if runtime.binding.disable_default_auth {
        HeaderMap::new()
    } else {
        build_model_headers(
            &provider.protocol,
            provider.vendor.as_deref(),
            &runtime.access_token,
        )?
    };
    headers.extend(runtime_binding_headers(&runtime.binding)?);

    let mut endpoint = reqwest::Url::parse(endpoint)?;
    if provider.protocol == "gemini" && !runtime.binding.disable_default_auth {
        endpoint
            .query_pairs_mut()
            .append_pair("key", &runtime.access_token);
    }
    Ok(gateway.http_client.get(endpoint).headers(headers))
}

impl AdminService {
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
            let request = provider_get_request(&self.gw, &provider, &runtime, &endpoint)?;

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
        let request = provider_get_request(&self.gw, provider, &runtime, url)?
            .timeout(Duration::from_secs(10));

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
