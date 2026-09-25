use super::*;

impl AdminService {
    pub async fn test_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        Ok(super::routes::RouteModule::new(&self.gw)
            .discover_provider_model_ids(id)
            .await?)
    }

    pub async fn get_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        self.test_provider_models(id).await
    }

    /// The loaded descriptor and saved channel authorize inference; typed
    /// Provider Model metadata supplies the model-specific limits, modalities,
    /// and features. Catalog entries never infer runtime capabilities.
    pub async fn get_model_capabilities(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let provider = self.get_provider(provider_id).await?;
        let vendor_id = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let channel_id = provider
            .channel
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider channel is missing"))?;
        let descriptor = self.gw.vendor_plugins.descriptor(vendor_id)?;
        let channel = descriptor
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .ok_or_else(|| {
                anyhow::anyhow!("Vendor `{vendor_id}` does not declare channel `{channel_id}`")
            })?;
        anyhow::ensure!(
            channel
                .capabilities
                .contains(&stravia_vendor_sdk::Capability::Infer),
            "provider channel does not support inference"
        );
        let model_id = model.trim();
        if model_id.is_empty() {
            anyhow::bail!("model cannot be empty");
        }
        let record = self
            .gw
            .storage
            .provider_models()
            .find(provider_id, model_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Provider Model metadata is unavailable; synchronize or add the model first"
                )
            })?;
        let metadata = record.metadata;
        let limits = metadata.limit.unwrap_or_default();
        let modalities = metadata.modalities.unwrap_or_default();
        let prices = metadata.cost.map(|cost| cost.prices).unwrap_or_default();
        Ok(ModelCapabilities {
            provider: provider
                .vendor
                .clone()
                .unwrap_or_else(|| provider.preset_key.clone().unwrap_or_default()),
            model_id: model_id.to_owned(),
            context_window: limits.context.unwrap_or(0),
            embedding_length: None,
            output_max_tokens: limits.output,
            tool_call: metadata.tool_call.unwrap_or(false),
            reasoning: metadata.reasoning.unwrap_or(false),
            input_modalities: modalities.input,
            output_modalities: modalities.output,
            input_cost: prices.input.and_then(|value| value.to_f64()),
            output_cost: prices.output.and_then(|value| value.to_f64()),
        })
    }
}
