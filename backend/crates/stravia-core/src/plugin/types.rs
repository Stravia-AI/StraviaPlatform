use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSource {
    Builtin,
    Local,
}

impl PluginSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginProvider {
    pub id: String,
    pub name: String,
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
    pub upstream_model: Option<String>,
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
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginSummary {
    pub vendor_id: String,
    pub name: String,
    pub version: String,
    pub source: PluginSource,
    pub status: String,
    pub error: Option<String>,
    pub builtin_version: Option<String>,
    pub capabilities: Vec<String>,
    pub affected_bindings: Vec<PluginBindingImpact>,
    pub pending_update: Option<PluginPreview>,
}

#[derive(Deserialize)]
pub struct ConfirmPluginUpdate {
    pub preview_id: String,
    #[serde(default)]
    pub allow_data_discard: bool,
}
