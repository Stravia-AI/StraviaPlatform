use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, types::Json};

use crate::provider::AuthMode;
use crate::provider::VendorRegistry;
use crate::thinking::{ThinkingLevel, ThinkingLevelMapping, mapping_control};

pub fn default_provider_auth_mode() -> String {
    "apikey".to_string()
}

pub fn is_valid_provider_auth_mode(value: &str) -> bool {
    matches!(value.trim(), "apikey" | "oauth")
}

fn auth_mode_to_legacy(mode: AuthMode) -> &'static str {
    // Legacy DB / WebUI vocabulary only knows "apikey" / "oauth"; the
    // newer `setuptoken` mode degrades to "apikey" for storage purposes
    // (the OAuth driver layer knows the real flow via vendor metadata).
    match mode {
        AuthMode::ApiKey => "apikey",
        AuthMode::OAuth => "oauth",
        AuthMode::SetupToken => "apikey",
    }
}

/// Resolve the authentication mode for a Provider's `(vendor, preset_key,
/// channel_id)` identity. Catalog identity remains in `preset_key`, while
/// credential semantics belong to the npm-keyed Vendor.
pub fn resolve_preset_channel_auth_mode(
    vendor_id: Option<&str>,
    preset_key: Option<&str>,
    channel_id: Option<&str>,
) -> Option<String> {
    let preset_key = preset_key?.trim();
    if preset_key.is_empty() {
        return None;
    }
    let requested_channel = channel_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default");
    let registry = VendorRegistry::global();
    let metadata = vendor_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|vendor_id| registry.metadata(vendor_id))
        .or_else(|| registry.metadata(preset_key))?;
    let channel = metadata
        .channels
        .iter()
        .find(|c| c.id.eq_ignore_ascii_case(requested_channel))
        .or_else(|| metadata.channels.iter().find(|c| c.id == "default"))?;
    Some(auth_mode_to_legacy(channel.auth_mode).to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    pub protocol: String,
    pub base_url: String,
    pub preset_key: Option<String>,
    pub channel: Option<String>,
    #[serde(alias = "modelsEndpoint")]
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    #[serde(skip_serializing)]
    pub api_key: String,
    #[serde(default = "empty_adapter_credentials", skip_serializing)]
    pub adapter_credentials: String,
    #[serde(default = "default_provider_auth_mode")]
    pub auth_mode: String,
    #[serde(default)]
    pub use_proxy: bool,
    pub last_test_success: Option<bool>,
    pub last_test_at: Option<String>,
    pub is_enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct OAuthCredential {
    pub provider_id: String,
    pub connection_id: String,
    pub driver_key: String,
    pub scheme: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: String,
    pub meta: String,
    pub status: String,
    pub status_version: i32,
    pub last_error: Option<String>,
    pub last_refresh_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpsertOAuthCredential {
    pub driver_key: String,
    pub scheme: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: Option<String>,
    pub meta: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Route {
    pub id: String,
    pub name: String,
    pub balance: String,
    pub target_provider: String,
    pub target_model: String,
    pub is_enabled: bool,
    pub created_at: String,
    #[serde(default)]
    #[sqlx(skip)]
    pub supported_thinking_levels: sqlx::types::Json<Vec<ThinkingLevel>>,
    #[serde(default)]
    #[sqlx(skip)]
    pub context_window: Option<u64>,
    #[serde(default)]
    #[sqlx(skip)]
    pub output_max_tokens: Option<u64>,
    #[serde(default)]
    #[sqlx(skip)]
    pub targets: Vec<Target>,
}

impl Route {
    pub fn refresh_supported_thinking_levels(&mut self) {
        self.supported_thinking_levels = sqlx::types::Json(
            ThinkingLevel::ALL
                .into_iter()
                .filter(|level| {
                    !self.targets.is_empty()
                        && self.targets.iter().all(|target| {
                            mapping_control(&target.thinking_level_map, *level)
                                .is_some_and(|control| !control.is_hidden())
                        })
                })
                .collect(),
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Target {
    pub id: String,
    pub model_id: String,
    pub provider_id: String,
    pub model: String,
    pub weight: i32,
    pub priority: i32,
    pub created_at: String,
    #[serde(default)]
    pub thinking_level_map: sqlx::types::Json<Vec<ThinkingLevelMapping>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum RouteSelectionStrategy {
    /// Weighted reservoir sampling — targets with higher weight are preferred.
    #[default]
    Weighted,
    /// Priority groups — lower priority number tried first; random within group.
    Priority,
    /// Cooldown-aware round-robin — deprioritises recently-used targets.
    Cooldown,
    /// Latency-ordered — targets sorted by ascending EMA response latency.
    Latency,
}

impl RouteSelectionStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::Priority => "priority",
            Self::Cooldown => "cooldown",
            Self::Latency => "latency",
        }
    }
}

impl std::str::FromStr for RouteSelectionStrategy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "weighted" => Ok(Self::Weighted),
            "priority" => Ok(Self::Priority),
            "cooldown" => Ok(Self::Cooldown),
            "latency" => Ok(Self::Latency),
            other => anyhow::bail!("unsupported model balance: {other}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKey {
    pub id: String,
    #[serde(rename = "key")]
    pub token: String,
    pub name: String,
    pub concurrency_limit: Option<i32>,
    pub is_enabled: bool,
    #[serde(default)]
    pub mcp_access_enabled: bool,
    #[serde(default)]
    pub transparent_injection_enabled: bool,
    #[serde(default)]
    pub inject_media_understanding: bool,
    #[serde(default)]
    pub inject_web_search: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyWithBindings {
    pub id: String,
    #[serde(rename = "key")]
    pub token: String,
    pub name: String,
    pub concurrency_limit: Option<i32>,
    pub is_enabled: bool,
    #[serde(default)]
    pub mcp_access_enabled: bool,
    #[serde(default)]
    pub transparent_injection_enabled: bool,
    #[serde(default)]
    pub inject_media_understanding: bool,
    #[serde(default)]
    pub inject_web_search: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(alias = "route_ids")]
    pub model_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct RequestLog {
    pub id: String,
    /// Unix 毫秒时间戳
    pub created_at: i64,
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,

    pub client_protocol: Option<String>,
    pub upstream_protocol: Option<String>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    #[serde(alias = "route_id")]
    pub model_id: Option<String>,
    #[serde(alias = "route_name")]
    pub model_name: Option<String>,
    pub upstream_url: Option<String>,
    pub client_model: Option<String>,
    pub upstream_model: Option<String>,

    pub method: Option<String>,
    pub path: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_request_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_response_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_response_body: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_response_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_response_body: Option<String>,

    pub upstream_status_code: Option<i32>,
    pub client_status_code: Option<i32>,

    pub latency_total_ms: Option<i64>,
    pub latency_upstream_ms: Option<i64>,
    pub input_tokens: i32,
    pub output_tokens: i32,
    #[serde(default)]
    pub cache_read_tokens: i32,
    #[serde(default)]
    pub cache_write_tokens: i32,
    pub thinking_level: Option<String>,

    pub is_stream: bool,
    pub stream_chunks_count: i32,
    pub stream_first_chunk_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProvider {
    #[serde(default)]
    pub name: Option<String>,
    pub source: ProviderSourceInput,
    #[serde(default)]
    pub credential: ProviderCredentialInput,
    #[serde(default)]
    pub use_proxy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderSourceInput {
    Catalog {
        provider_id: String,
        channel_id: String,
        fingerprint: String,
        #[serde(default)]
        base_url_override: Option<String>,
    },
    Custom {
        vendor: Option<String>,
        protocol: String,
        base_url: String,
        #[serde(default, alias = "modelsSource")]
        models_source: Option<String>,
        #[serde(default)]
        static_models: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderCredentialInput {
    ApiKey {
        value: String,
    },
    SetupToken {
        value: String,
    },
    Fields {
        values: BTreeMap<String, String>,
    },
    #[default]
    None,
}

#[derive(Debug, Clone)]
pub struct CreateProviderRecord {
    pub name: String,
    pub vendor: Option<String>,
    pub protocol: String,
    pub base_url: String,
    pub preset_key: Option<String>,
    pub channel: Option<String>,
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    pub api_key: String,
    pub adapter_credentials: String,
    pub auth_mode: String,
    pub use_proxy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProvider {
    pub name: Option<String>,
    pub vendor: Option<String>,
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    pub preset_key: Option<String>,
    pub channel: Option<String>,
    #[serde(alias = "modelsSource")]
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    pub api_key: Option<String>,
    pub adapter_credentials: Option<BTreeMap<String, String>>,
    pub auth_mode: Option<String>,
    pub use_proxy: Option<bool>,
    pub is_enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateRoute {
    #[serde(alias = "virtual_model", alias = "vmodel")]
    pub name: Option<String>,
    #[serde(rename = "balance", alias = "strategy")]
    pub balance: Option<String>,
    pub target_provider: Option<String>,
    pub target_model: Option<String>,
    #[serde(default)]
    pub targets: Option<Vec<UpsertTarget>>,
    pub is_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRoute {
    #[serde(alias = "virtual_model", alias = "vmodel")]
    pub name: String,
    #[serde(rename = "balance", alias = "strategy")]
    pub balance: Option<String>,
    pub target_provider: String,
    pub target_model: String,
    #[serde(default)]
    pub targets: Vec<CreateTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTarget {
    pub provider_id: String,
    pub model: String,
    pub weight: Option<i32>,
    pub priority: Option<i32>,
    #[serde(default)]
    pub thinking_level_map: Vec<ThinkingLevelMapping>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertTarget {
    pub id: Option<String>,
    pub provider_id: String,
    pub model: String,
    pub weight: Option<i32>,
    pub priority: Option<i32>,
    #[serde(default)]
    pub thinking_level_map: Vec<ThinkingLevelMapping>,
}

#[derive(Debug, Clone)]
pub struct PutRoute {
    pub id: Option<String>,
    pub route_id: String,
    pub selection_strategy: String,
    pub is_enabled: bool,
    pub targets: Vec<CreateTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApiKey {
    #[serde(default)]
    pub key: Option<String>,
    pub name: String,
    #[serde(default)]
    pub concurrency_limit: Option<i32>,
    pub expires_at: Option<String>,
    #[serde(default)]
    pub mcp_access_enabled: bool,
    #[serde(default)]
    pub transparent_injection_enabled: bool,
    #[serde(default)]
    pub inject_media_understanding: bool,
    #[serde(default)]
    pub inject_web_search: bool,
    #[serde(default, alias = "route_ids")]
    pub model_ids: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateApiKey {
    pub key: Option<String>,
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub concurrency_limit: Option<Option<i32>>,
    pub is_enabled: Option<bool>,
    pub mcp_access_enabled: Option<bool>,
    pub transparent_injection_enabled: Option<bool>,
    pub inject_media_understanding: Option<bool>,
    pub inject_web_search: Option<bool>,
    pub expires_at: Option<String>,
    #[serde(alias = "route_ids")]
    pub model_ids: Option<Vec<String>>,
}

#[cfg(test)]
mod api_key_tests {
    use super::{CreateApiKey, UpdateApiKey};

    #[test]
    fn api_key_update_distinguishes_omitted_and_null_concurrency_limit() {
        let omitted: UpdateApiKey =
            serde_json::from_value(serde_json::json!({})).expect("omitted concurrency limit");
        assert_eq!(omitted.concurrency_limit, None);

        let cleared: UpdateApiKey = serde_json::from_value(serde_json::json!({
            "concurrency_limit": null
        }))
        .expect("null concurrency limit");
        assert_eq!(cleared.concurrency_limit, Some(None));

        let set: UpdateApiKey = serde_json::from_value(serde_json::json!({
            "concurrency_limit": 2
        }))
        .expect("numeric concurrency limit");
        assert_eq!(set.concurrency_limit, Some(Some(2)));
    }
    #[test]
    fn api_key_dtos_reject_legacy_quota_fields() {
        for field in ["rpm", "rpd", "tpm", "tpd"] {
            let mut create = serde_json::json!({ "name": "legacy" });
            create[field] = serde_json::json!(1);
            assert!(
                serde_json::from_value::<CreateApiKey>(create).is_err(),
                "CreateApiKey accepted legacy field {field}"
            );

            let mut update = serde_json::json!({});
            update[field] = serde_json::json!(1);
            assert!(
                serde_json::from_value::<UpdateApiKey>(update).is_err(),
                "UpdateApiKey accepted legacy field {field}"
            );
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct WebProvider {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub use_proxy: bool,
    #[serde(skip_serializing)]
    pub local_engines: Option<Json<LocalSearchEngineConfigs>>,
    pub last_test_success: Option<bool>,
    pub last_test_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWebProvider {
    pub name: String,
    pub kind: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub use_proxy: bool,
    pub local_engines: Option<LocalSearchEngineConfigs>,
}

fn deserialize_double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateWebProvider {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub api_key: Option<Option<String>>,
    pub use_proxy: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub local_engines: Option<Option<LocalSearchEngineConfigs>>,
}

pub type LocalSearchEngineConfigs = BTreeMap<String, LocalSearchEngineConfig>;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalSearchEngineConfig {
    pub enabled: bool,
    #[serde(default)]
    pub private_settings: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocalSearchEngineView {
    pub enabled: bool,
}

impl std::fmt::Debug for LocalSearchEngineConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalSearchEngineConfig")
            .field("enabled", &self.enabled)
            .field(
                "private_settings_configured",
                &self
                    .private_settings
                    .as_ref()
                    .is_some_and(|settings| !settings.is_empty()),
            )
            .finish()
    }
}

pub fn default_local_search_engines() -> LocalSearchEngineConfigs {
    [
        ("google", true),
        ("bing", true),
        ("brave", true),
        ("baidu", true),
        ("360", false),
        ("sogou_weixin", false),
        ("google_scholar", false),
    ]
    .into_iter()
    .map(|(id, enabled)| {
        (
            id.to_string(),
            LocalSearchEngineConfig {
                enabled,
                private_settings: Some(BTreeMap::new()),
            },
        )
    })
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebProviderCapabilities {
    pub search: bool,
    pub fetch: bool,
}

impl WebProvider {
    pub fn capabilities(&self) -> Option<WebProviderCapabilities> {
        match self.kind.as_str() {
            "local" | "exa" | "zhipu" => Some(WebProviderCapabilities {
                search: true,
                fetch: true,
            }),
            _ => None,
        }
    }

    pub fn local_engine_views(&self) -> Option<BTreeMap<String, LocalSearchEngineView>> {
        self.local_engines.as_deref().map(|engines| {
            engines
                .iter()
                .map(|(id, config)| {
                    (
                        id.clone(),
                        LocalSearchEngineView {
                            enabled: config.enabled,
                        },
                    )
                })
                .collect()
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebAccessSettings {
    pub enabled: bool,
    pub search_provider_ids: Vec<String>,
    pub fetch_provider_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LogQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status_min: Option<i32>,
    pub status_max: Option<i32>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogPage {
    pub items: Vec<RequestLog>,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, FromRow)]
pub struct StatsOverview {
    pub total_requests: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
    pub avg_duration_ms: f64,
    pub avg_first_token_ms: Option<f64>,
    pub error_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct StatsHourly {
    pub hour: String,
    pub request_count: i64,
    pub error_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cache_write_tokens: i64,
    pub avg_duration_ms: f64,
    pub avg_first_token_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ModelStats {
    pub model: String,
    pub request_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub avg_duration_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProviderStats {
    pub provider: String,
    pub request_count: i64,
    pub error_count: i64,
    pub avg_duration_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKeyStats {
    pub api_key_id: String,
    pub api_key_name: String,
    pub request_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub success: bool,
    pub latency_ms: u64,
    pub model: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub provider: String,
    pub model_id: String,
    pub context_window: u64,
    pub embedding_length: Option<u64>,
    pub output_max_tokens: Option<u64>,
    pub tool_call: bool,
    pub reasoning: bool,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub input_cost: Option<f64>,
    pub output_cost: Option<f64>,
}

impl Provider {
    pub fn adapter_credential(&self, key: &str) -> Option<String> {
        serde_json::from_str::<BTreeMap<String, String>>(&self.adapter_credentials)
            .ok()?
            .remove(key)
            .filter(|value| !value.trim().is_empty())
    }

    pub fn effective_api_key(&self) -> String {
        self.adapter_credential("apiKey")
            .unwrap_or_else(|| self.api_key.trim().to_string())
    }

    pub fn effective_auth_mode(&self) -> String {
        resolve_preset_channel_auth_mode(
            self.vendor.as_deref(),
            self.preset_key.as_deref(),
            self.channel.as_deref(),
        )
        .unwrap_or_else(|| {
            let mode = self.auth_mode.trim();
            if mode.is_empty() {
                default_provider_auth_mode()
            } else {
                mode.to_string()
            }
        })
    }

    pub fn effective_models_source(&self) -> Option<&str> {
        self.models_source
            .as_deref()
            .filter(|v| !v.trim().is_empty())
    }
}

fn empty_adapter_credentials() -> String {
    "{}".to_string()
}

impl CreateProviderRecord {
    pub fn effective_models_source(&self) -> Option<&str> {
        self.models_source
            .as_deref()
            .filter(|v| !v.trim().is_empty())
    }
}

impl UpdateProvider {
    pub fn effective_models_source(&self) -> Option<&str> {
        self.models_source
            .as_deref()
            .filter(|v| !v.trim().is_empty())
    }
}
