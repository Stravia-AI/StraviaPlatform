use serde::{Deserialize, Serialize};

mod samples;
mod service;

pub(crate) use samples::AllowanceSampleStore;
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
