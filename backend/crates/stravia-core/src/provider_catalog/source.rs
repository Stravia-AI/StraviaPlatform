use super::*;

/// Host-owned catalog documents: `version.json` (canonical revision checks),
/// `models.json`, and provider logos. The provider index and provider scopes
/// are fetched by the base guest through `sync-catalog`, not through this
/// source.
#[async_trait]
pub trait CatalogSource: Send + Sync {
    async fn fetch_version(&self) -> anyhow::Result<CatalogVersion>;
    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>>;
    async fn fetch_logo(&self, provider_id: &str) -> anyhow::Result<Vec<u8>>;
    /// Download `{origin}/favicon.ico` for the website icon fallback. The
    /// request carries no provider credentials, follows no redirects, and only
    /// accepts bodies that decode as a known image format.
    async fn fetch_favicon(&self, origin: &str) -> anyhow::Result<Vec<u8>>;
}

#[derive(Clone)]
pub struct HttpCatalogSource {
    client: reqwest::Client,
    /// `None` disables every remote document fetch — a gateway that never
    /// configured a catalog origin must fail closed rather than silently
    /// reaching the production service.
    base_url: Option<String>,
}

impl HttpCatalogSource {
    pub fn new(base_url: Option<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let base_url = base_url.map(|value| value.trim_end_matches('/').to_owned());
        Ok(Self { client, base_url })
    }

    fn catalog_url(&self, path: &str) -> anyhow::Result<String> {
        let base_url = self
            .base_url
            .as_deref()
            .context("provider catalog remote origin is not configured")?;
        Ok(format!("{base_url}/{path}"))
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
                self.catalog_url("version.json")?,
                MAX_VERSION_BYTES,
                "catalog version",
            )
            .await?;
        parse_version(&body)
    }

    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>> {
        self.fetch_json(
            self.catalog_url("models.json")?,
            MAX_INDEX_BYTES,
            "Canonical Model index",
        )
        .await
    }

    async fn fetch_logo(&self, provider_id: &str) -> anyhow::Result<Vec<u8>> {
        validate_provider_id(provider_id)?;
        let response = self
            .client
            .get(self.catalog_url(&format!("logos/{provider_id}.svg"))?)
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

    async fn fetch_favicon(&self, origin: &str) -> anyhow::Result<Vec<u8>> {
        let origin = url::Url::parse(origin)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .map(|url| url.origin().ascii_serialization())
            .context("provider website origin is invalid")?;
        let response = self
            .client
            .get(format!("{origin}/favicon.ico"))
            .send()
            .await
            .context("fetch provider website favicon")?;
        if response.status().is_redirection() {
            bail!("provider website favicon redirect is not allowed");
        }
        let response = response
            .error_for_status()
            .context("provider website favicon status")?;
        let body = read_limited(response.bytes_stream(), MAX_LOGO_BYTES).await?;
        if icon_content_type(&body).is_none() {
            bail!("provider website favicon is not a supported image");
        }
        Ok(body)
    }
}

/// Sniff the image format of a cached or freshly fetched icon body. Both the
/// fetch validation and the response `Content-Type` derive from this so cached
/// bytes need no sidecar metadata.
pub(crate) fn icon_content_type(body: &[u8]) -> Option<&'static str> {
    if body.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some("image/x-icon");
    }
    if body.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        return Some("image/png");
    }
    if body.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if body.starts_with(&[0xff, 0xd8]) {
        return Some("image/jpeg");
    }
    if body.starts_with(b"RIFF") && body.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    let head = body
        .get(..256)
        .unwrap_or(body)
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if head
        .windows(4)
        .any(|window| window == b"<svg" || window == b"<?xm")
    {
        return Some("image/svg+xml");
    }
    None
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
