//! Read-only inventory of built-in protocol and provider extensions.

use std::sync::OnceLock;

use crate::protocol::registry::ProtocolRegistry;
use crate::provider::VendorRegistry;

/// The kind of built-in extension represented by a manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityKind {
    /// A provider vendor preset/adapter.
    ProviderVendor,
    /// A protocol endpoint handler.
    ProtocolEndpoint,
}

impl CapabilityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityKind::ProviderVendor => "provider_vendor",
            CapabilityKind::ProtocolEndpoint => "protocol_endpoint",
        }
    }
}

/// A read-only description of one loaded extension.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// Stable identifier of the extension (hook name / vendor id / protocol id).
    pub id: String,
    /// Which capability slot this extension occupies.
    pub capability: CapabilityKind,
}

/// Aggregated, read-only view over Stravia's compile-time provider and protocol
/// registries.
pub struct PluginKernel {
    vendors: &'static VendorRegistry,
    protocols: &'static ProtocolRegistry,
}

impl PluginKernel {
    /// Process-wide kernel singleton.
    pub fn global() -> &'static PluginKernel {
        static KERNEL: OnceLock<PluginKernel> = OnceLock::new();
        KERNEL.get_or_init(|| PluginKernel {
            vendors: VendorRegistry::global(),
            protocols: ProtocolRegistry::global(),
        })
    }

    /// Enumerate every loaded extension across all registries.
    pub fn manifests(&self) -> Vec<PluginManifest> {
        let mut out = Vec::new();

        for vendor in self.vendors.list_metadata() {
            out.push(PluginManifest {
                id: vendor.id.to_string(),
                capability: CapabilityKind::ProviderVendor,
            });
        }
        for endpoint in self.protocols.endpoints() {
            out.push(PluginManifest {
                id: endpoint.to_string(),
                capability: CapabilityKind::ProtocolEndpoint,
            });
        }

        out
    }

    /// Manifests filtered to a single capability kind.
    pub fn manifests_of(&self, kind: CapabilityKind) -> Vec<PluginManifest> {
        self.manifests()
            .into_iter()
            .filter(|m| m.capability == kind)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_aggregates_builtin_extensions() {
        let kernel = PluginKernel::global();
        let manifests = kernel.manifests();

        // Built-in protocol endpoints and vendor presets are always registered,
        // so the aggregated view must never be empty.
        assert!(
            !manifests.is_empty(),
            "expected built-in extensions to be registered"
        );
        assert!(
            !kernel
                .manifests_of(CapabilityKind::ProtocolEndpoint)
                .is_empty(),
            "expected at least one protocol endpoint"
        );
        assert!(
            !kernel
                .manifests_of(CapabilityKind::ProviderVendor)
                .is_empty(),
            "expected at least one provider vendor"
        );
    }
}
