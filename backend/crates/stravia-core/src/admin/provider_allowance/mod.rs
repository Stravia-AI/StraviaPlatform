use serde::{Deserialize, Serialize};

mod parsers;
mod samples;
mod service;

use parsers::{
    CommandCodeSubscription, ParsedAllowance, commandcode_billing_cycle, parse_commandcode_org_id,
    parse_commandcode_subscription, parse_commandcode_summary_cost, parse_minimax_fallback,
    parse_monitor_response,
};
pub(crate) use samples::AllowanceSampleStore;
#[cfg(test)]
use service::{
    AllowanceHttpRequest, AllowanceHttpResponse, AllowanceTransport, TransportFailure,
    fetch_monitor, get_provider_allowance_with_transport,
    list_provider_allowance_targets_with_transport, list_provider_allowances_with_transport,
    monitor_requests, refresh_provider_allowance_with_transport,
};
pub(crate) use service::{ProviderAllowanceState, SAMPLE_INTERVAL};

/// One eligible provider's entry in the non-blocking allowance list: identity
/// is always present, `snapshot` carries the cached state when one exists, and
/// `refreshing` marks that an upstream fetch is in flight for this provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderAllowanceTarget {
    pub provider_id: String,
    pub provider_name: String,
    pub catalog_provider_id: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ProviderAllowanceSnapshot>,
    pub refreshing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderAllowanceSnapshot {
    pub provider_id: String,
    pub provider_name: String,
    pub catalog_provider_id: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_label: Option<String>,
    pub status: ProviderAllowanceStatus,
    pub allowances: Vec<Allowance>,
    pub models: Vec<ModelAllowance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProviderAllowanceError>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAllowanceStatus {
    Fresh,
    Stale,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Allowance {
    pub key: String,
    pub label: String,
    pub kind: AllowanceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<AllowanceAmount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<AllowanceAmount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<AllowanceAmount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<AllowanceCondition>,
    pub forecast: ExhaustionForecast,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllowanceCondition {
    Normal,
    Tight,
    Exhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ExhaustionForecast {
    pub status: ExhaustionForecastStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_remaining_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exhausts_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustionForecastStatus {
    NoRisk,
    WillExhaust,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllowanceKind {
    QuotaWindow,
    RequestAllowance,
    Balance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllowanceAmount {
    pub value: f64,
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelAllowance {
    pub model: String,
    pub allowances: Vec<Allowance>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderAllowanceError {
    pub category: ProviderAllowanceErrorCategory,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAllowanceErrorCategory {
    Authentication,
    RateLimited,
    Timeout,
    UpstreamUnavailable,
    InvalidResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorKind {
    AnthropicClaudeCode,
    OpenAiCodex,
    GitHubCopilot,
    KimiForCoding,
    NanoGpt,
    ZaiCodingPlan,
    ZhipuAiCodingPlan,
    MiniMaxCodingPlan,
    MiniMaxCnCodingPlan,
    Wafer,
    OpenCodeGo,
    Crof,
    DeepSeek,
    NeuralWatt,
    XaiGrok,
    CommandCode,
}

fn monitor_for(preset_key: &str, channel: &str) -> Option<MonitorKind> {
    match (preset_key, channel) {
        ("anthropic", "claude-code") => Some(MonitorKind::AnthropicClaudeCode),
        ("openai", "codex") => Some(MonitorKind::OpenAiCodex),
        ("github-copilot", "default") => Some(MonitorKind::GitHubCopilot),
        ("kimi-for-coding", "default") => Some(MonitorKind::KimiForCoding),
        ("nano-gpt", "default") => Some(MonitorKind::NanoGpt),
        ("zai-coding-plan", "default") => Some(MonitorKind::ZaiCodingPlan),
        ("zhipuai-coding-plan", "default") => Some(MonitorKind::ZhipuAiCodingPlan),
        ("minimax-coding-plan", "default") => Some(MonitorKind::MiniMaxCodingPlan),
        ("minimax-cn-coding-plan", "default") => Some(MonitorKind::MiniMaxCnCodingPlan),
        ("wafer.ai", "default") => Some(MonitorKind::Wafer),
        ("opencode-go", "default") => Some(MonitorKind::OpenCodeGo),
        ("crof", "default") => Some(MonitorKind::Crof),
        ("deepseek", "default") => Some(MonitorKind::DeepSeek),
        ("neuralwatt", "default") => Some(MonitorKind::NeuralWatt),
        ("xai", "grok") => Some(MonitorKind::XaiGrok),
        ("commandcode", "default") => Some(MonitorKind::CommandCode),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
