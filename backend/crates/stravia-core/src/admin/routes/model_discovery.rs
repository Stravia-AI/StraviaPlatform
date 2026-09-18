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
                .map(|catalog| {
                    retain_discovered_model_ids(
                        &provider,
                        catalog.models.into_iter().map(|model| model.id).collect(),
                    )
                })
                .ok_or_else(|| RouteModelDiscoveryError::CatalogIdentityMissing {
                    provider_id: provider_id.to_string(),
                });
        }
        let runtime = admin
            .resolve_provider_runtime(&provider)
            .await
            .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?;
        if provider.vendor.as_deref() == Some("devin")
            && let Ok(entries) = discover_devin_catalog(admin, &provider, &runtime).await
            && !entries.is_empty()
        {
            // Union with the preset selector list: the catalog omits
            // `swe-1-6-slow` on some accounts even though free tier can still
            // run it (upstream #258), so the curated list doubles as the
            // safety net for selectors the catalog does not advertise.
            let merged = static_model_union(
                &runtime,
                &provider,
                entries.iter().map(|entry| entry.selector.clone()).collect(),
            );
            return Ok(retain_discovered_model_ids(&provider, merged));
        }
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

        let constructed = construct_models_request(&provider, &runtime, &endpoint)
            .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?;
        let client = admin
            .gw
            .http_client_for_provider(provider.use_proxy)
            .await
            .map_err(|error| RouteModelDiscoveryError::setup(provider_id, error))?;
        let request = client
            .get(constructed.url)
            .headers(constructed.headers)
            .timeout(Duration::from_secs(10));

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

/// Devin's model inventory is not an HTTP JSON endpoint — it is the unary
/// Connect-RPC `ApiServerService/GetCliModelConfigs` on the api-server host.
/// The request carries only `ClientMetadata` (raw `application/proto`, doubled
/// Basic auth), the same shape the seat-management monitor already uses.
/// Errors propagate to the caller, which falls back to the preset selector
/// list — discovery must never fail a sync just because the catalog probe did.
pub(super) async fn discover_devin_catalog(
    admin: &AdminService,
    provider: &Provider,
    runtime: &ResolvedProviderRuntime,
) -> anyhow::Result<Vec<crate::protocol::codec::devin_connect::DevinModelConfig>> {
    let token = runtime.access_token.trim();
    if token.is_empty() {
        anyhow::bail!("devin session token is empty");
    }
    let base_url = runtime
        .binding
        .base_url_override
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let value = provider.base_url.trim();
            (!value.is_empty()).then_some(value)
        })
        .unwrap_or("https://server.codeium.com")
        .trim_end_matches('/');
    let url = format!("{base_url}/exa.api_server_pb.ApiServerService/GetCliModelConfigs");

    let mut headers = HeaderMap::new();
    for (name, value) in &runtime.binding.extra_headers {
        if let (Ok(name), Ok(value)) = (
            name.parse::<reqwest::header::HeaderName>(),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers.insert(reqwest::header::ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("connect-protocol-version"),
        HeaderValue::from_static("1"),
    );

    let client = admin
        .gw
        .http_client_for_provider(provider.use_proxy)
        .await?;
    let response = client
        .post(url)
        .headers(headers)
        .body(crate::protocol::codec::devin_connect::encode_client_metadata_request(token))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("GetCliModelConfigs returned HTTP {}", response.status());
    }
    let body = response.bytes().await?;
    Ok(crate::protocol::codec::devin_connect::decode_cli_model_configs(&body))
}

/// Merge live catalog selectors with the preset/binding static list, keeping
/// discovery order first and appending static entries the catalog missed.
pub(super) fn static_model_union(
    runtime: &ResolvedProviderRuntime,
    provider: &Provider,
    mut models: Vec<String>,
) -> Vec<String> {
    let statics = runtime
        .binding
        .static_models_override
        .clone()
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| preset_static_models(provider));
    for entry in statics {
        let entry = entry.trim();
        if !entry.is_empty() && !models.iter().any(|model| model == entry) {
            models.push(entry.to_string());
        }
    }
    models
}
