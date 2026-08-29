//! Unified vendor registry.
//!
//! Two `inventory` collections are used:
//!
//! * [`VendorRegistration`] — full vendors that implement [`Vendor`].
//!   One per vendor module (no duplicate registration needed).
//! * [`ExtensionRegistration`] — channel-scoped or vendor-scoped extensions
//!   that implement only [`VendorExtension`] (e.g. `claude-code`, `codex`,
//!   and protocol-default vendor fallbacks).
//!
//! The combined [`VendorRegistry`] exposes two resolution strategies:
//! * `get_vendor(vendor_id)` — vendor-id lookup for the proxy pipeline.
//! * `resolve(provider, protocol_id)` — three-tier Channel → Vendor → ProtocolDefault
//!   resolution used by admin/auth paths.

use std::sync::{Arc, OnceLock};

use crate::db::models::Provider;
use crate::protocol::ids::{Protocol, ProtocolEndpoint};
use crate::provider::metadata::VendorMetadata;
use crate::provider::vendor::Vendor;
use crate::provider::vendor_ext::{VendorAsExt, VendorExtension};

// ── VendorScope ───────────────────────────────────────────────────────────────

/// Selector for extension/vendor matching. Resolution order:
/// `Channel` → `Vendor` → `ProtocolDefault`.
#[derive(Debug, Clone, Copy)]
pub enum VendorScope {
    Vendor {
        vendor_id: &'static str,
    },
    Channel {
        vendor_id: &'static str,
        channel_id: &'static str,
    },
}

/// Returns the canonical default vendor for a given protocol suite.
///
/// Used in tier-3 resolution to apply the right family-level extension when a
/// provider has no explicit `vendor` field but its protocol implies a default vendor.
pub fn protocol_default_vendor(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAICompatible | Protocol::OpenResponses => "openai",
        Protocol::AnthropicMessages => "anthropic",
        Protocol::GoogleGemini => "google",
        Protocol::BedrockConverse => "amazon-bedrock",
        Protocol::CohereChat => "cohere",
        Protocol::WatsonxTextChat => "watsonx",
        Protocol::GatewayLanguageModel => "gateway",
    }
}

// ── VendorRegistration ────────────────────────────────────────────────────────

/// `inventory` registration record for a full [`Vendor`] implementation.
pub struct VendorRegistration {
    pub make: fn() -> Box<dyn Vendor>,
}

/// Metadata-only provider registration for endpoint families that do not implement inference.
pub struct VendorMetadataRegistration {
    pub metadata: &'static VendorMetadata,
    pub route_operations: &'static [&'static str],
}

inventory::collect!(VendorMetadataRegistration);

inventory::collect!(VendorRegistration);

// ── ExtensionRegistration ─────────────────────────────────────────────────────

/// `inventory` registration record for a channel-scoped or vendor-scoped
/// [`VendorExtension`] that does NOT implement the full [`Vendor`] trait
/// (e.g. `AnthropicClaudeCodeChannel`, `OpenAIVendorExt`).
pub struct ExtensionRegistration {
    pub make: fn() -> Box<dyn VendorExtension>,
}

inventory::collect!(ExtensionRegistration);

// ── VendorRegistry ────────────────────────────────────────────────────────────

/// Process-wide vendor registry.  Initialized once on first access.
pub struct VendorRegistry {
    /// Full vendor implementations (vendor-scoped).
    vendors: Vec<Arc<dyn Vendor>>,
    /// Pre-built `VendorAsExt` wrappers for each vendor entry, so
    /// `resolve()` can return `Arc<dyn VendorExtension>` without allocating.
    vendor_as_ext: Vec<Arc<dyn VendorExtension>>,
    /// Extension-only entries (channel-scoped or vendor-scoped).
    extensions: Vec<Arc<dyn VendorExtension>>,
    metadata_only: Vec<&'static VendorMetadataRegistration>,
}

impl VendorRegistry {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<VendorRegistry> = OnceLock::new();
        INSTANCE.get_or_init(Self::build)
    }

    fn build() -> Self {
        let mut vendors: Vec<Arc<dyn Vendor>> = Vec::new();
        for reg in inventory::iter::<VendorRegistration> {
            vendors.push(Arc::from((reg.make)()));
        }

        let vendor_as_ext: Vec<Arc<dyn VendorExtension>> = vendors
            .iter()
            .map(|v| Arc::new(VendorAsExt(v.clone())) as Arc<dyn VendorExtension>)
            .collect();

        let mut extensions: Vec<Arc<dyn VendorExtension>> = Vec::new();
        for reg in inventory::iter::<ExtensionRegistration> {
            extensions.push(Arc::from((reg.make)()));
        }
        let metadata_only = inventory::iter::<VendorMetadataRegistration>
            .into_iter()
            .collect();

        Self {
            vendors,
            vendor_as_ext,
            extensions,
            metadata_only,
        }
    }

    /// Look up a full [`Vendor`] by `vendor_id` (case-insensitive).
    /// Used by the proxy pipeline's target iteration loop.
    pub fn get_vendor(&self, vendor_id: &str) -> Option<&Arc<dyn Vendor>> {
        self.vendors
            .iter()
            .find(|v| v.vendor_id().eq_ignore_ascii_case(vendor_id))
    }

    pub fn supports_route_operation(&self, vendor_id: &str, operation: &str) -> bool {
        self.metadata_only.iter().any(|registration| {
            registration.metadata.id.eq_ignore_ascii_case(vendor_id)
                && registration.route_operations.contains(&operation)
        })
    }

    /// Three-tier resolution: Channel → Vendor → ProtocolDefault.
    ///
    /// Returns `Arc<dyn VendorExtension>` for use by admin and auth paths
    /// that only need sync hooks (`auth_headers`, `build_url`, `metadata`).
    ///
    /// Tier 3 uses the protocol's canonical default vendor (`protocol_default_vendor`)
    /// to look up a vendor-scoped extension that serves as a fallback when the
    /// provider has no explicit `vendor` field.
    pub fn resolve(
        &self,
        provider: &Provider,
        protocol_id: ProtocolEndpoint,
    ) -> Option<&Arc<dyn VendorExtension>> {
        let vendor_id = provider
            .vendor
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let channel_id = provider
            .channel
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());

        // 1. Channel-scoped (extension-only entries only, since vendor-level
        //    implementations don't carry channel scope).
        if let (Some(v), Some(c)) = (vendor_id, channel_id) {
            for ext in &self.extensions {
                if let VendorScope::Channel {
                    vendor_id: vk,
                    channel_id: ck,
                } = ext.scope()
                    && vk.eq_ignore_ascii_case(v)
                    && ck.eq_ignore_ascii_case(c)
                {
                    return Some(ext);
                }
            }
        }

        // 2. Vendor-scoped (prefer full Vendor entries, then extension-only).
        if let Some(v) = vendor_id {
            for (i, vendor) in self.vendors.iter().enumerate() {
                if let VendorScope::Vendor { vendor_id: vk } = vendor.scope()
                    && vk.eq_ignore_ascii_case(v)
                {
                    return Some(&self.vendor_as_ext[i]);
                }
            }
            for ext in &self.extensions {
                if let VendorScope::Vendor { vendor_id: vk } = ext.scope()
                    && vk.eq_ignore_ascii_case(v)
                {
                    return Some(ext);
                }
            }
        }

        // 3. Protocol-default vendor fallback.
        //
        // For providers with no explicit vendor, use the default vendor for the
        // protocol suite (e.g. OpenAI-compatible → "openai", Anthropic → "anthropic").
        let default_vendor = protocol_default_vendor(protocol_id.protocol);
        for (i, vendor) in self.vendors.iter().enumerate() {
            if let VendorScope::Vendor { vendor_id: vk } = vendor.scope()
                && vk.eq_ignore_ascii_case(default_vendor)
            {
                return Some(&self.vendor_as_ext[i]);
            }
        }
        for ext in &self.extensions {
            if let VendorScope::Vendor { vendor_id: vk } = ext.scope()
                && vk.eq_ignore_ascii_case(default_vendor)
            {
                return Some(ext);
            }
        }

        None
    }

    pub fn list_metadata(&self) -> Vec<&'static VendorMetadata> {
        let mut out: Vec<&'static VendorMetadata> =
            self.vendors.iter().filter_map(|v| v.metadata()).collect();
        for ext in &self.extensions {
            if let Some(metadata) = ext.metadata() {
                out.push(metadata);
            }
        }
        out.extend(
            self.metadata_only
                .iter()
                .map(|registration| registration.metadata),
        );
        out.sort_by_key(|m| m.id);
        out.dedup_by_key(|m| m.id);
        out
    }

    pub fn metadata(&self, vendor_id: &str) -> Option<&'static VendorMetadata> {
        self.list_metadata()
            .into_iter()
            .find(|m| m.id.eq_ignore_ascii_case(vendor_id))
    }
}
