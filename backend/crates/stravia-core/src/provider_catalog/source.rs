use super::*;

#[async_trait]
pub trait CatalogSource: Send + Sync {
    async fn fetch_version(&self) -> anyhow::Result<CatalogVersion>;
    async fn fetch_providers(&self) -> anyhow::Result<Vec<u8>>;
    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>>;
    async fn fetch_provider_scope(&self, provider_id: &str) -> anyhow::Result<Vec<u8>>;
    async fn fetch_logo(&self, provider_id: &str) -> anyhow::Result<Vec<u8>>;
}

#[derive(Clone)]
pub struct HttpCatalogSource {
    client: reqwest::Client,
}

impl HttpCatalogSource {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client })
    }

    async fn fetch_json(
        &self,
        url: String,
        limit: usize,
        resource: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("fetch {resource}"))?;
        if response.status().is_redirection() {
            bail!("{resource} redirect is not allowed");
        }
        let response = response
            .error_for_status()
            .with_context(|| format!("{resource} status"))?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("application/json") {
            bail!("{resource} is not JSON");
        }
        read_limited(response.bytes_stream(), limit).await
    }
}

#[async_trait]
impl CatalogSource for HttpCatalogSource {
    async fn fetch_version(&self) -> anyhow::Result<CatalogVersion> {
        let body = self
            .fetch_json(
                format!("{CATALOG_BASE_URL}/version.json"),
                MAX_VERSION_BYTES,
                "catalog version",
            )
            .await?;
        parse_version(&body)
    }

    async fn fetch_providers(&self) -> anyhow::Result<Vec<u8>> {
        self.fetch_json(
            format!("{CATALOG_BASE_URL}/providers.json"),
            MAX_INDEX_BYTES,
            "provider index",
        )
        .await
    }

    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>> {
        self.fetch_json(
            format!("{CATALOG_BASE_URL}/models.json"),
            MAX_INDEX_BYTES,
            "Canonical Model index",
        )
        .await
    }

    async fn fetch_provider_scope(&self, provider_id: &str) -> anyhow::Result<Vec<u8>> {
        validate_provider_id(provider_id)?;
        self.fetch_json(
            format!("{CATALOG_BASE_URL}/providers/{provider_id}/models.json"),
            MAX_SCOPE_BYTES,
            "Provider Catalog scope",
        )
        .await
    }

    async fn fetch_logo(&self, provider_id: &str) -> anyhow::Result<Vec<u8>> {
        validate_provider_id(provider_id)?;
        let response = self
            .client
            .get(format!("{CATALOG_BASE_URL}/logos/{provider_id}.svg"))
            .send()
            .await
            .context("fetch provider logo")?;
        if response.status().is_redirection() {
            bail!("provider logo redirect is not allowed");
        }
        let response = response
            .error_for_status()
            .context("provider logo status")?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("image/svg+xml") {
            bail!("provider logo is not SVG");
        }
        read_limited(response.bytes_stream(), MAX_LOGO_BYTES).await
    }
}

async fn read_limited(
    mut stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > limit {
            bail!("response exceeds {limit} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
