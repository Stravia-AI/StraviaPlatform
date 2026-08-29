use super::*;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("Provider Catalog scope refresh failed for {provider_id}: {message}")]
    ScopeRefresh {
        provider_id: String,
        message: String,
    },
    #[error("Canonical Model was not found: {id}")]
    ModelNotFound { id: String },
    #[error("Provider Catalog Entry was not found: {provider_id}/{model_id}")]
    EntryNotFound {
        provider_id: String,
        model_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogVersion {
    pub revision: String,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogProviderList {
    pub revision: String,
    pub generated_at: String,
    pub providers: Vec<CatalogProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    pub documentation_url: Option<String>,
    pub npm: String,
    pub vendor_id: String,
    pub protocol: String,
    pub base_url: String,
    pub channels: Vec<CatalogChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogChannel {
    pub id: String,
    pub label: String,
    pub protocol: String,
    pub base_url: String,
    pub auth_mode: CatalogAuthMode,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogAuthMode {
    OptionalApiKey,
    #[serde(rename = "oauth")]
    OAuth,
    SetupToken,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanonicalModelList {
    pub revision: String,
    pub generated_at: String,
    pub models: Vec<CanonicalModelSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanonicalModelSummary {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogModelList {
    pub revision: String,
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogModelSource {
    pub provider_id: String,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogProviderScope {
    pub revision: String,
    pub provider_id: String,
    pub models: Vec<CatalogModelSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub status: Option<String>,
    pub release_date: Option<String>,
    pub capabilities: Option<CatalogCapabilities>,
    pub limits: Option<CatalogLimits>,
    pub cost: Option<CatalogCost>,
    pub reasoning_options: Option<CatalogReasoningOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CatalogCapabilities {
    pub tool_call: bool,
    pub reasoning: bool,
    pub attachment: bool,
    pub temperature: bool,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CatalogLimits {
    pub context: Option<u64>,
    pub output: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CatalogCost {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CatalogReasoningOptions {
    Effort { values: Vec<String> },
    Toggle,
    Budget { min: Option<u64>, max: Option<u64> },
}

impl CatalogReasoningOptions {
    pub fn effort_values(&self) -> Option<&[String]> {
        match self {
            Self::Effort { values } => Some(values),
            Self::Toggle | Self::Budget { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogRefreshSummary {
    pub revision: String,
    pub generated_at: String,
    pub provider_count: usize,
    pub model_count: usize,
    pub changed: bool,
}
