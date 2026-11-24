pub mod config;
pub mod service;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use std::time::Duration;
use stravia_runtime_contract::thinking::ThinkingLevel;

pub const SETTINGS_KEY: &str = "platform_llm_config";

/// Persisted platform-internal LLM configuration stored under `settings.platform_llm_config`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformLlmConfig {
    /// Internal logical Route record ID (`models.id`). `None` means disabled / unset.
    #[serde(default)]
    pub route_id: Option<String>,
}

/// Availability status of the configured platform-internal LLM binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformLlmStatus {
    Unset,
    Available,
    Unavailable,
}

/// Lightweight logical Route summary returned for display and selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformLlmRouteSummary {
    pub id: String,
    pub model_id: String,
    pub display_name: String,
}

/// Admin view combining the saved binding, resolved availability status, and eligible candidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformLlmConfigView {
    pub route_id: Option<String>,
    pub status: PlatformLlmStatus,
    pub route: Option<PlatformLlmRouteSummary>,
    pub eligible_routes: Vec<PlatformLlmRouteSummary>,
}

/// Admin update payload for selecting or clearing the platform-internal LLM Route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePlatformLlmConfig {
    #[serde(default)]
    pub route_id: Option<String>,
}

/// Input for generating a short conversation title using the configured platform-internal LLM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTitleInput {
    pub api_key_id: String,
    pub user_message: String,
    #[serde(default)]
    pub assistant_message: Option<String>,
    /// Optional UI locale (e.g. `zh-CN`, `en-US`) so the generated title matches the interface language.
    #[serde(default)]
    pub locale: Option<String>,
    /// Optional per-task override; `None` follows the bound Route's default thinking level.
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
}

/// Generated conversation title result. `None` means the model deferred naming
/// (e.g. greeting-only turn or output rejected by normalization) so a later turn may retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTitleResult {
    pub title: Option<String>,
}

/// Specification for a single platform-internal text generation task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformLlmTaskSpec {
    pub instructions: String,
    pub input: String,
    pub max_tokens: Option<u32>,
    /// Optional per-task override; `None` leaves `AiRequest.reasoning.level` unset so
    /// the bound Route's `default_thinking_level` applies.
    pub thinking_level: Option<ThinkingLevel>,
    pub timeout: Duration,
}

/// Extensible catalog of platform-internal LLM tasks that share the configured platform model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformLlmTask {
    ConversationTitle {
        user_message: String,
        assistant_message: Option<String>,
        locale: Option<String>,
        thinking_level: Option<ThinkingLevel>,
    },
}

/// Typed error for platform-internal LLM configuration and task execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformLlmError {
    pub code: &'static str,
    pub message: String,
}

impl PlatformLlmError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PlatformLlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PlatformLlmError {}

impl From<anyhow::Error> for PlatformLlmError {
    fn from(error: anyhow::Error) -> Self {
        Self::new("platform_llm_storage_unavailable", error.to_string())
    }
}
