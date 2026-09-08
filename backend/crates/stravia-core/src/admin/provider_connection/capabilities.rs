use super::*;

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
            let constructed = construct_models_request(&provider, &runtime, &endpoint)?;
            let request = self
                .gw
                .http_client_for_provider(provider.use_proxy)
                .await?
                .get(constructed.url)
                .headers(constructed.headers);

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
        let constructed = construct_models_request(provider, &runtime, url)?;
        let request = self
            .gw
            .http_client_for_provider(provider.use_proxy)
            .await?
            .get(constructed.url)
            .headers(constructed.headers)
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

#[cfg(test)]
mod tests {
    use crate::db::models::{CreateProvider, ProviderCredentialInput, ProviderSourceInput};
    use axum::{Router, http::StatusCode, routing::get};

    #[tokio::test]
    async fn model_query_falls_back_after_upstream_failure_but_not_proxy_setup_failure()
    -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/models",
                    get(move |headers: axum::http::HeaderMap| {
                        let sent = sent.clone();
                        async move {
                            sent.send(headers).unwrap();
                            StatusCode::BAD_GATEWAY
                        }
                    }),
                ),
            )
            .await
        });
        let data_dir = tempfile::tempdir()?;
        let gateway = crate::Gateway::new(crate::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..crate::GatewayConfig::default()
        })
        .await?;
        let admin = gateway.admin();
        admin.set_setting("proxy_enabled", "true").await?;
        admin.set_setting("proxy_url", "").await?;
        let input = |use_proxy| CreateProvider {
            name: Some(format!("Inventory {use_proxy}")),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: format!("http://{address}"),
                models_source: Some(format!("http://{address}/models")),
                static_models: Some("fallback-model".into()),
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "synthetic-key".into(),
            },
            use_proxy,
        };
        let direct = admin.create_provider(input(false)).await?;
        let proxied = admin.create_provider(input(true)).await?;
        assert_eq!(
            admin.get_provider_models(&direct.id).await?,
            ["fallback-model"]
        );
        let headers = received.try_recv()?;
        assert_eq!(headers["authorization"], "Bearer synthetic-key");
        let error = admin
            .get_provider_models(&proxied.id)
            .await
            .expect_err("invalid proxy must not become a static-list success");
        assert!(error.to_string().contains("proxy_url"));
        assert!(matches!(
            received.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        server.abort();
        Ok(())
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
