use async_trait::async_trait;
use thiserror::Error;

use super::*;

#[derive(Debug, Error)]
pub(crate) enum RouteModelDiscoveryError {
    #[error("Provider Model discovery setup failed for Provider {provider_id}: {source}")]
    DiscoverySetup {
        provider_id: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("Catalog Provider identity is missing for Provider {provider_id}")]
    CatalogIdentityMissing { provider_id: String },
    #[error("Model Discovery URL is empty for Provider {provider_id}")]
    DiscoveryUrlEmpty { provider_id: String },
    #[error("Provider Model discovery failed for Provider {provider_id}: {message}")]
    DiscoveryRequestFailed {
        provider_id: String,
        message: String,
    },
    #[error("Provider Model discovery returned HTTP {status} for Provider {provider_id}")]
    DiscoveryHttpStatus { provider_id: String, status: u16 },
    #[error(
        "Provider Model discovery returned an invalid or empty list for Provider {provider_id}"
    )]
    InvalidDiscoveryResponse { provider_id: String },
}

impl RouteModelDiscoveryError {
    fn setup(provider_id: &str, source: anyhow::Error) -> Self {
        Self::DiscoverySetup {
            provider_id: provider_id.to_string(),
            source,
        }
    }
}

#[async_trait]
pub(super) trait ProviderModelDiscovery: Send + Sync {
    async fn discover(
        &self,
        admin: &AdminService,
        provider_id: &str,
    ) -> Result<Vec<String>, RouteModelDiscoveryError>;
}

pub(super) struct HttpProviderModelDiscovery;

#[async_trait]
impl ProviderModelDiscovery for HttpProviderModelDiscovery {
    async fn discover(
        &self,
        admin: &AdminService,
        provider_id: &str,
    ) -> Result<Vec<String>, RouteModelDiscoveryError> {
        let provider = admin
            .get_provider(provider_id)
            .await
            .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?;
        if uses_catalog_inventory(&provider) {
            return admin
                .preset_catalog_models_for_provider(&provider)
                .await
                .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?
                .map(|catalog| catalog.models.into_iter().map(|model| model.id).collect())
                .ok_or_else(|| RouteModelDiscoveryError::CatalogIdentityMissing {
                    provider_id: provider_id.to_string(),
                });
        }
        let runtime = admin
            .resolve_provider_runtime(&provider)
            .await
            .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?;
        let credential = runtime.access_token.clone();
        if let Some(static_list) = runtime.binding.static_models_override.as_deref() {
            let models = static_list
                .iter()
                .map(|model| model.trim().to_string())
                .filter(|model| !model.is_empty())
                .collect::<Vec<_>>();
            if !models.is_empty() {
                return Ok(models);
            }
        }
        let preset_static_models = preset_static_models(&provider);
        if !preset_static_models.is_empty() {
            return Ok(preset_static_models);
        }
        let endpoint = runtime
            .binding
            .models_source_override
            .clone()
            .or_else(|| resolve_models_endpoint(&provider))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| RouteModelDiscoveryError::DiscoveryUrlEmpty {
                provider_id: provider_id.to_string(),
            })?;

        let mut headers = if runtime.binding.disable_default_auth {
            HeaderMap::new()
        } else {
            build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)
                .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?
        };
        headers.extend(
            runtime_binding_headers(&runtime.binding)
                .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?,
        );
        let mut request = admin
            .gw
            .http_client
            .get(&endpoint)
            .headers(headers)
            .timeout(Duration::from_secs(10));
        if provider.protocol == "gemini" && !runtime.binding.disable_default_auth {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)
                    .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?;
            headers.extend(
                runtime_binding_headers(&runtime.binding)
                    .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?,
            );
            request = admin
                .gw
                .http_client
                .get(format!("{endpoint}{separator}key={credential}"))
                .headers(headers)
                .timeout(Duration::from_secs(10));
        }

        let response = request.send().await.map_err(|error| {
            RouteModelDiscoveryError::DiscoveryRequestFailed {
                provider_id: provider_id.to_string(),
                message: format_connectivity_error(&error),
            }
        })?;
        if !response.status().is_success() {
            return Err(RouteModelDiscoveryError::DiscoveryHttpStatus {
                provider_id: provider_id.to_string(),
                status: response.status().as_u16(),
            });
        }
        let json: Value = response.json().await.unwrap_or_default();
        let models =
            extract_models_from_response(&provider.protocol, provider.vendor.as_deref(), &json);
        if models.is_empty() {
            return Err(RouteModelDiscoveryError::InvalidDiscoveryResponse {
                provider_id: provider_id.to_string(),
            });
        }
        Ok(models)
    }
}
