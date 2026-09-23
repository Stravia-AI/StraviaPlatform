use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, types::Json};

use crate::thinking::ThinkingLevelMapping;
use crate::thinking::mapping_control;
use stravia_runtime_contract::thinking::ThinkingLevel;

pub fn default_provider_auth_mode() -> String {
    "apikey".to_string()
}

pub fn is_valid_provider_auth_mode(value: &str) -> bool {
    matches!(value.trim(), "apikey" | "oauth")
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
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    #[serde(skip_serializing)]
    pub api_key: String,
    #[serde(default = "empty_adapter_credentials", skip_serializing)]
    pub adapter_credentials: String,
    /// Non-secret vendor behavior options (JSON object). Unlike
    /// `adapter_credentials` this round-trips to clients so the UI can show
    /// the current state (e.g. the command-code zdr switch). Storage keeps a
    /// JSON string; the API surface is a real object.
    #[serde(default = "empty_vendor_options", with = "vendor_options_json")]
    pub vendor_options: String,
    #[serde(default = "default_provider_auth_mode")]
    pub auth_mode: String,
    #[serde(default)]
    pub use_proxy: bool,
    pub last_test_success: Option<bool>,
    pub last_test_at: Option<String>,
    pub is_enabled: bool,
    /// ADR-0073 凭据失效：`ok` / `invalid`。上游明确拒绝当前凭据组合后由
    /// 条件写置为 `invalid`，只有新凭据证据（凭据变更、OAuth 重绑或刷新
    /// 成功、测试成功）才恢复 `ok`。与 `is_enabled` 正交。
    #[serde(default = "default_credential_status")]
    pub credential_status: String,
    pub credential_invalid_at: Option<String>,
    /// 行写入代际，用于凭据失效的条件写竞态保护；不进入 API 序列化。
    #[serde(default, skip_serializing)]
    pub revision: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub fn default_credential_status() -> String {
    "ok".to_string()
}

/// ADR-0073：发起请求时锁定的凭据代际。标记失效时若代际已变化（管理员
/// 改了凭据、OAuth 已刷新/重绑），说明拒绝证据属于旧凭据，放弃写入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCredentialVersion {
    pub provider_revision: i64,
    /// 请求带 OAuth 凭据时锁定 `provider_oauth_credentials.status_version`；
    /// 无 OAuth 连接时为 `None`，标记时要求该 Provider 仍无 OAuth 行。
    pub oauth_status_version: Option<i32>,
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
    pub model_id: String,
    pub display_name: Option<String>,
    /// 客户端未表达任何推理意图时应用的 Canonical Thinking Level；
    /// `None` 表示不加控制，由上游模型自行决定。
    #[serde(default)]
    pub default_thinking_level: Option<String>,
    pub balance: String,
    pub target_provider: String,
    /// Model of the highest-priority enabled Target, or `None` for Provider-only.
    pub target_model: Option<String>,
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
    pub supports_image_input: bool,
    #[serde(default)]
    #[sqlx(skip)]
    pub targets: Vec<Target>,
}

impl Route {
    pub fn effective_display_name(&self) -> &str {
        self.display_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.model_id)
    }

    pub fn refresh_supported_thinking_levels(&mut self) {
        self.supported_thinking_levels = sqlx::types::Json(
            ThinkingLevel::ALL
                .into_iter()
                .filter(|level| {
                    self.targets.iter().any(|target| target.enabled)
                        && self
                            .targets
                            .iter()
                            .filter(|target| target.enabled)
                            .all(|target| {
                                mapping_control(&target.thinking_level_map, *level)
                                    .is_some_and(|control| !control.is_hidden())
                            })
                })
                .collect(),
        );
    }
}

pub const DEFAULT_TARGET_PRIORITY: i32 = 0;
pub const DEFAULT_FIRST_TOKEN_TIMEOUT_MS: i64 = 60_000;
pub const DEFAULT_TARGET_RETRY_BUDGET: i32 = 5;
pub const DEFAULT_TARGET_COOLDOWN_MS: i64 = 120_000;

const fn default_target_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Target {
    pub id: String,
    pub model_id: String,
    pub provider_id: String,
    /// `None` is a Provider-only full-search Target. `Some` is always non-empty.
    pub model: Option<String>,
    #[serde(default = "default_target_enabled")]
    pub enabled: bool,
    pub priority: i32,
    pub first_token_timeout_ms: i64,
    pub target_retry_budget: i32,
    pub target_cooldown_ms: i64,
    pub created_at: String,
    #[serde(default)]
    pub thinking_level_map: sqlx::types::Json<Vec<ThinkingLevelMapping>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum RouteSelectionStrategy {
    /// Selects the Target carrying the least weighted traffic in the last 24 hours.
    #[default]
    TrafficEqualization,
    /// Selects the fastest reliable Target when enough recent samples exist.
    LatencyPreference,
}

impl RouteSelectionStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TrafficEqualization => "traffic_equalization",
            Self::LatencyPreference => "latency_preference",
        }
    }
}

impl std::str::FromStr for RouteSelectionStrategy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "traffic_equalization" => Ok(Self::TrafficEqualization),
            "latency_preference" => Ok(Self::LatencyPreference),
            other => anyhow::bail!("unsupported Route Scheduling Strategy: {other}"),
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
    #[serde(default)]
    pub inject_media_generation: bool,
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
    #[serde(default)]
    pub inject_media_generation: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub model_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProvider {
    #[serde(default)]
    pub name: Option<String>,
    pub source: ProviderSourceInput,
    #[serde(default)]
    pub credential: ProviderCredentialInput,
    #[serde(default)]
    pub vendor_options: serde_json::Map<String, serde_json::Value>,
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
        vendor: String,
        channel: String,
        #[serde(default)]
        protocol: Option<String>,
        base_url: String,
        #[serde(default)]
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
        values: BTreeMap<String, serde_json::Value>,
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
    pub vendor_options: String,
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
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    pub api_key: Option<String>,
    pub adapter_credentials: Option<BTreeMap<String, serde_json::Value>>,
    /// Validated against the vendor's declared non-secret `config_fields`;
    /// `None` keeps the stored value, while an empty object reapplies defaults.
    #[serde(default)]
    pub vendor_options: Option<serde_json::Map<String, serde_json::Value>>,
    pub auth_mode: Option<String>,
    pub use_proxy: Option<bool>,
    pub is_enabled: Option<bool>,
    /// Internal flag, never serialized: admin updates resolve into a
    /// full-field rewrite, so the store cannot distinguish "the caller
    /// wrote credential fields" from "fields were backfilled with current
    /// values". Set when the original input only carries display/intent
    /// fields, to keep an existing credential-invalid marker.
    #[serde(skip)]
    pub preserve_credential_status: bool,
}

impl UpdateProvider {
    /// ADR-0073 黑名单清除：除纯展示/意图字段（name、is_enabled、
    /// models_source、static_models）外，任何字段写入都视为凭据证据场景
    /// 变化（换平台、协议、代理都可能使旧的拒绝证据失效），清除失效标记。
    /// `credential_status` 不在本结构体中，管理 API 无法直接写该字段。
    pub fn resets_credential_status(&self) -> bool {
        self.vendor.is_some()
            || self.protocol.is_some()
            || self.base_url.is_some()
            || self.preset_key.is_some()
            || self.channel.is_some()
            || self.api_key.is_some()
            || self.adapter_credentials.is_some()
            || self.vendor_options.is_some()
            || self.auth_mode.is_some()
            || self.use_proxy.is_some()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRoute {
    pub model_id: Option<String>,
    pub display_name: Option<String>,
    pub balance: Option<String>,
    pub target_provider: Option<String>,
    /// Omitted preserves the projection; explicit `null` selects Provider-only.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub target_model: Option<Option<String>>,
    #[serde(default)]
    pub targets: Option<Vec<UpsertTarget>>,
    pub is_enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub default_thinking_level: Option<Option<ThinkingLevel>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRoute {
    pub model_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub balance: Option<String>,
    pub target_provider: String,
    pub target_model: Option<String>,
    #[serde(default)]
    pub targets: Vec<CreateTarget>,
    #[serde(default)]
    pub default_thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTarget {
    pub provider_id: String,
    /// `None` is a Provider-only full-search Target. `Some` is always non-empty.
    pub model: Option<String>,
    #[serde(default = "default_target_enabled")]
    pub enabled: bool,
    pub priority: Option<i32>,
    pub first_token_timeout_ms: Option<i64>,
    pub target_retry_budget: Option<i32>,
    pub target_cooldown_ms: Option<i64>,
    #[serde(default)]
    pub thinking_level_map: Vec<ThinkingLevelMapping>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertTarget {
    pub id: Option<String>,
    pub provider_id: String,
    /// `None` is a Provider-only full-search Target. `Some` is always non-empty.
    pub model: Option<String>,
    #[serde(default = "default_target_enabled")]
    pub enabled: bool,
    pub priority: Option<i32>,
    pub first_token_timeout_ms: Option<i64>,
    pub target_retry_budget: Option<i32>,
    pub target_cooldown_ms: Option<i64>,
    #[serde(default)]
    pub thinking_level_map: Vec<ThinkingLevelMapping>,
}

#[derive(Debug, Clone)]
pub struct PutRoute {
    pub id: Option<String>,
    pub model_id: String,
    pub display_name: Option<String>,
    pub selection_strategy: String,
    pub is_enabled: bool,
    pub targets: Vec<CreateTarget>,
    pub default_thinking_level: Option<ThinkingLevel>,
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
    #[serde(default)]
    pub inject_media_generation: bool,
    #[serde(default)]
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
    pub inject_media_generation: Option<bool>,
    pub expires_at: Option<String>,
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
    pub search_provider_ids: Vec<String>,
    pub fetch_provider_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, FromRow)]
pub struct StatsOverview {
    pub total_requests: i64,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub total_cache_read_tokens: Option<i64>,
    pub total_cache_write_tokens: Option<i64>,
    pub total_reasoning_tokens: Option<i64>,
    pub avg_duration_ms: Option<f64>,
    pub avg_first_token_ms: Option<f64>,
    pub error_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct StatsSeries {
    /// Epoch milliseconds of the bucket start, aligned to the requested bucket
    /// size and caller timezone offset.
    pub bucket_start: i64,
    pub request_count: i64,
    pub error_count: i64,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub total_cache_read_tokens: Option<i64>,
    pub total_cache_write_tokens: Option<i64>,
    pub total_reasoning_tokens: Option<i64>,
    pub avg_duration_ms: Option<f64>,
    pub avg_first_token_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ModelStats {
    pub model: String,
    pub request_count: i64,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub total_reasoning_tokens: Option<i64>,
    pub avg_duration_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProviderStats {
    pub provider: String,
    pub request_count: i64,
    pub error_count: i64,
    pub avg_duration_ms: Option<f64>,
    /// 已完成 attempt 的输出 Token 总速（tok/s）：Σoutput / Σ净生成耗时。
    /// 组内任一已完成 attempt 未报告输出或耗时，或总生成耗时为零时为 null。
    pub avg_output_tps: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKeyStats {
    pub api_key_id: String,
    pub api_key_name: String,
    pub request_count: i64,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
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
        let mut values =
            serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&self.adapter_credentials)
                .ok()?;
        let value = values.remove(key)?;
        let value = value.as_str()?.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }

    /// Parsed `vendor_options` object; malformed JSON yields an empty map
    /// rather than failing the request — options only tweak behavior.
    pub fn vendor_options(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(&self.vendor_options)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }

    pub fn effective_api_key(&self) -> String {
        self.adapter_credential("apiKey")
            .unwrap_or_else(|| self.api_key.trim().to_string())
    }

    /// ADR-0073：上游已确认拒绝当前凭据组合。失效 Provider 的 Target
    /// 在调度快照装配时被排除，直到出现新凭据证据。
    pub fn credential_invalid(&self) -> bool {
        self.credential_status == "invalid"
    }

    pub fn effective_auth_mode(&self) -> String {
        let mode = self.auth_mode.trim();
        if mode.is_empty() {
            default_provider_auth_mode()
        } else {
            mode.to_string()
        }
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

fn empty_vendor_options() -> String {
    "{}".to_string()
}

/// Serializes the stored JSON string as a real object and accepts either an
/// object (canonical) or a JSON-encoded string (lenient) on the way in.
mod vendor_options_json {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;

    pub fn serialize<S: Serializer>(value: &str, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::from_str::<Value>(value)
            .unwrap_or_else(|_| Value::Object(Default::default()))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Object(map) => serde_json::to_string(&Value::Object(map))
                .map_err(|error| D::Error::custom(format!("invalid vendor options: {error}"))),
            Value::String(text) => {
                // Lenient: a pre-existing string payload must still parse.
                serde_json::from_str::<Value>(&text).map_err(|error| {
                    D::Error::custom(format!("invalid vendor options: {error}"))
                })?;
                Ok(text)
            }
            Value::Null => Ok("{}".to_string()),
            other => Err(D::Error::custom(format!(
                "vendor options must be an object, got {}",
                ValueTypeLabel(&other)
            ))),
        }
    }

    struct ValueTypeLabel<'a>(&'a Value);

    impl std::fmt::Display for ValueTypeLabel<'_> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let label = match self.0 {
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::String(_) => "a string",
                Value::Array(_) => "an array",
                _ => "an unrecognized value",
            };
            formatter.write_str(label)
        }
    }
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
