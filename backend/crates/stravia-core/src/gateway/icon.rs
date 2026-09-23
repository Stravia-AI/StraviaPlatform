//! Provider icon resolution: catalog logo first, then a website favicon.
//!
//! The request key is either a stable provider identity (`catalog_id` or
//! `provider_id`, chosen by the caller) or a saved provider connection UUID —
//! the UUID only identifies which connection's `base_url` may serve as the
//! website fallback and is never sent upstream as a catalog id. The website
//! origin is the plugin-declared `website` when present, otherwise the saved
//! connection's `base_url` origin; once a website is selected its failure is
//! final. All remote bodies share the catalog disk cache, TTL, and
//! stale-if-error behavior in `ProviderCatalog`.

use anyhow::Context;

use super::Gateway;

/// Resolved icon body plus its `Content-Type`.
pub struct ProviderIcon {
    pub body: Vec<u8>,
    pub content_type: &'static str,
}

impl Gateway {
    /// Resolve the icon for `key`: a provider descriptor/catalog identity or a
    /// saved provider connection id. Fails when every source misses, letting
    /// the caller render its built-in fallback.
    pub async fn provider_icon(&self, key: &str) -> anyhow::Result<ProviderIcon> {
        if let Some(provider) = self.storage.providers().get(key).await? {
            return self.connection_icon(&provider).await;
        }
        self.identity_icon(key).await
    }

    /// Saved connection: catalog logo from the profile's stable identity, then
    /// the declared website or — only without a website — this connection's
    /// `base_url` origin.
    async fn connection_icon(
        &self,
        provider: &crate::db::models::Provider,
    ) -> anyhow::Result<ProviderIcon> {
        let descriptor = provider
            .vendor
            .as_deref()
            .and_then(|vendor| self.vendor_plugins.descriptor(vendor).ok());
        let logo_id = descriptor
            .as_ref()
            .map(|descriptor| {
                descriptor
                    .catalog_id
                    .clone()
                    .unwrap_or_else(|| descriptor.provider_id.clone())
            })
            .or_else(|| provider.preset_key.clone())
            .or_else(|| provider.vendor.clone());
        if let Some(logo_id) = logo_id
            && let Ok(body) = self.provider_catalog.logo(&logo_id).await
        {
            return Ok(ProviderIcon {
                body,
                content_type: "image/svg+xml",
            });
        }
        let origin = descriptor
            .as_ref()
            .and_then(|descriptor| descriptor.website.as_deref())
            .and_then(website_origin)
            .or_else(|| website_origin(&provider.base_url));
        match origin {
            Some(origin) => self.favicon_icon(&origin).await,
            None => Err(anyhow::anyhow!("provider icon is unavailable")),
        }
    }

    /// Descriptor/catalog identity: catalog logo under that exact id (no
    /// second-id guessing), then the descriptor's declared website favicon.
    async fn identity_icon(&self, key: &str) -> anyhow::Result<ProviderIcon> {
        if let Ok(body) = self.provider_catalog.logo(key).await {
            return Ok(ProviderIcon {
                body,
                content_type: "image/svg+xml",
            });
        }
        let descriptors = self.vendor_plugins.descriptors();
        let descriptor = descriptors
            .iter()
            .find(|descriptor| descriptor.provider_id == key)
            .or_else(|| {
                descriptors
                    .iter()
                    .find(|descriptor| descriptor.catalog_id.as_deref() == Some(key))
            });
        let origin = descriptor
            .and_then(|descriptor| descriptor.website.as_deref())
            .and_then(website_origin);
        match origin {
            Some(origin) => self.favicon_icon(&origin).await,
            None => Err(anyhow::anyhow!("provider icon is unavailable")),
        }
    }

    async fn favicon_icon(&self, origin: &str) -> anyhow::Result<ProviderIcon> {
        let body = self.provider_catalog.favicon(origin).await?;
        let content_type = crate::provider_catalog::icon_content_type(&body)
            .context("cached provider favicon is not a supported image")?;
        Ok(ProviderIcon { body, content_type })
    }
}

/// Extract the fetchable `http(s)` origin of a declared website or connection
/// base URL. Anything else is not a valid icon source.
fn website_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.origin().ascii_serialization())
}
