use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSource {
    Builtin,
    Local,
    Marketplace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstallOrigin {
    pub source_id: String,
    pub repository_id: u64,
    pub repository: String,
    pub public_key_fingerprint: String,
    pub trust_revision: i64,
    pub release_id: u64,
    pub release_tag: String,
    pub release_url: String,
    pub catalog_sha256: String,
    pub artifact_sha256: String,
}

impl PluginSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Local => "local",
            Self::Marketplace => "marketplace",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginProvider {
    pub id: String,
    pub name: String,
}

/// The installed Vendor Component that supplied a provider profile. Read from
/// the loaded component manifest, never from the profile itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderPluginSource {
    pub id: String,
    pub name: String,
}

/// Management view of one provider profile. The JSON is the SDK
/// profile's fields followed by `plugin`; `descriptor.provider_id` remains the
/// execution identity, while `display_name` may be canonicalized for the
/// picker (see `plugin::manager::provider_metadata`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderMetadata {
    #[serde(flatten)]
    pub descriptor: stravia_vendor_sdk::ProviderDescriptor,
    pub plugin: ProviderPluginSource,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginNetworkPermission {
    pub origin: String,
    pub provider_id: Option<String>,
    pub configuration_field: Option<String>,
    pub added: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginDataDiscard {
    pub provider: PluginProvider,
    pub kinds: Vec<String>,
    pub recovery_actions: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginBindingImpact {
    pub route_id: String,
    pub provider_id: String,
    pub upstream_model: String,
    pub capability: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginPreview {
    pub id: String,
    pub vendor_id: String,
    pub name: String,
    pub author: Option<String>,
    pub previous_version: Option<String>,
    pub new_version: String,
    pub target_source: PluginSource,
    pub origin: Option<PluginInstallOrigin>,
    pub previous_origin: Option<PluginInstallOrigin>,
    pub is_downgrade: bool,
    pub inherits_credentials: bool,
    pub affected_providers: Vec<PluginProvider>,
    pub network_permissions: Vec<PluginNetworkPermission>,
    pub removed_network_permissions: Vec<String>,
    pub discarded_data: Vec<PluginDataDiscard>,
    pub affected_bindings: Vec<PluginBindingImpact>,
    pub cancels_active_operations: bool,
    pub active_operations: usize,
    pub affected_auth_sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_svg: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginSummary {
    pub vendor_id: String,
    pub name: String,
    pub version: String,
    pub source: PluginSource,
    pub origin: Option<PluginInstallOrigin>,
    pub digest: String,
    pub installed_at: i64,
    pub status: String,
    pub error: Option<String>,
    pub builtin_version: Option<String>,
    pub capabilities: Vec<String>,
    pub affected_bindings: Vec<PluginBindingImpact>,
    pub pending_update: Option<PluginPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_svg: Option<String>,
}

#[derive(Deserialize)]
pub struct ConfirmPluginUpdate {
    pub preview_id: String,
    #[serde(default)]
    pub allow_data_discard: bool,
}
