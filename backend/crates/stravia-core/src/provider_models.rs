use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::Context;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelSourceKind {
    Discovered,
    Manual,
}

impl ProviderModelSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Manual => "manual",
        }
    }
}

impl FromStr for ProviderModelSourceKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "discovered" => Ok(Self::Discovered),
            "manual" => Ok(Self::Manual),
            _ => anyhow::bail!("invalid Provider Model source kind: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelPresence {
    Present,
    Missing,
}

impl ProviderModelPresence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
        }
    }
}

impl FromStr for ProviderModelPresence {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "present" => Ok(Self::Present),
            "missing" => Ok(Self::Missing),
            _ => anyhow::bail!("invalid Provider Model presence: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelSelectionPolicy {
    Auto,
    ForceEnabled,
    ForceDisabled,
}

impl ProviderModelSelectionPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ForceEnabled => "force_enabled",
            Self::ForceDisabled => "force_disabled",
        }
    }
}

impl FromStr for ProviderModelSelectionPolicy {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "force_enabled" => Ok(Self::ForceEnabled),
            "force_disabled" => Ok(Self::ForceDisabled),
            _ => anyhow::bail!("invalid Provider Model selection policy: {value}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderModelMetadata {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub family: Option<String>,
    pub attachment: Option<bool>,
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    pub open_weights: Option<bool>,
    pub reasoning_options: Option<Vec<ReasoningOption>>,
    pub interleaved: Option<Interleaved>,
    pub structured_output: Option<bool>,
    pub temperature: Option<bool>,
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    pub modalities: Option<ModelModalities>,
    pub limit: Option<ModelLimit>,
    pub cost: Option<ModelCost>,
    pub status: Option<String>,
    pub experimental: Option<Value>,
    pub provider: Option<Value>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl ProviderModelMetadata {
    pub fn from_value(model_id: &str, value: Value) -> anyhow::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Provider Model metadata must be an object"))?;
        if let Some(id) = object.get("id").and_then(Value::as_str)
            && id != model_id
        {
            anyhow::bail!("Provider Model metadata id must match model ID");
        }
        let mut metadata: Self =
            serde_json::from_value(value).context("decode Provider Model metadata")?;
        metadata.id = Some(model_id.to_string());
        metadata.validate()?;
        Ok(metadata)
    }
    pub fn from_source_value(model_id: &str, mut value: Value) -> anyhow::Result<Self> {
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Provider Model metadata must be an object"))?;
        for field in [
            "name",
            "description",
            "family",
            "knowledge",
            "release_date",
            "last_updated",
            "status",
        ] {
            if let Some(Value::String(text)) = object.get_mut(field) {
                let trimmed = text.trim();
                if trimmed.len() != text.len() {
                    *text = trimmed.to_string();
                }
            }
        }
        Self::from_value(model_id, value)
    }

    /// 未匹配 Provider Catalog 与 Canonical 模板时的占位快照：按最常见的文本
    /// 对话模型登记保守默认，避免规格完全空白。占位不等于已登记（见
    /// lacks_registered_specification），后续同步命中真实元数据仍会整体替换。
    pub fn bare(model_id: &str) -> Self {
        Self {
            id: Some(model_id.to_string()),
            name: Some(model_id.to_string()),
            reasoning: Some(true),
            tool_call: Some(true),
            modalities: Some(unmatched_default_modalities()),
            limit: Some(unmatched_default_limit()),
            ..Self::default()
        }
    }

    /// 规格芯片依赖这些字段。全空或仅含 bare() 占位默认值时视为「未登记」，
    /// 后续同步可按模型 ID 补模板。
    pub fn lacks_registered_specification(&self) -> bool {
        if self.structured_output.is_some()
            || self.attachment.is_some()
            || self.temperature.is_some()
        {
            return false;
        }
        if self.limit.is_none()
            && self.modalities.is_none()
            && self.reasoning.is_none()
            && self.tool_call.is_none()
        {
            return true;
        }
        self.limit == Some(unmatched_default_limit())
            && self.modalities == Some(unmatched_default_modalities())
            && self.reasoning == Some(true)
            && self.tool_call == Some(true)
    }

    /// 与历史 bare() 写法完全一致（id/name 均为模型 ID、其余全空）的快照；
    /// 同步时可整体升级为占位默认或模板数据而不覆盖人工修改。
    pub(crate) fn is_identity_only(&self) -> bool {
        *self
            == Self {
                id: self.id.clone(),
                name: self.id.clone(),
                ..Self::default()
            }
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        serde_json::to_value(self).context("encode Provider Model metadata")
    }

    pub fn extension_value(&self) -> Value {
        Value::Object(Map::from_iter(
            self.extensions
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        ))
    }

    pub fn lifecycle_status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn cost_rules(&self) -> Vec<ProviderModelCostRule> {
        let Some(cost) = &self.cost else {
            return Vec::new();
        };
        let mut rules = Vec::new();
        if let Some(prices) = &cost.context_over_200k {
            rules.push(ProviderModelCostRule {
                rule_index: 0,
                kind: ProviderModelCostRuleKind::ContextOver200k,
                threshold_tokens: 200_000,
                prices: prices.clone(),
            });
        }
        let offset = rules.len();
        for (index, tier) in cost.tiers.iter().enumerate() {
            rules.push(ProviderModelCostRule {
                rule_index: (offset + index) as i64,
                kind: ProviderModelCostRuleKind::Tier,
                threshold_tokens: tier.tier.size,
                prices: tier.prices(),
            });
        }
        rules
    }

    fn validate(&self) -> anyhow::Result<()> {
        for (field, value) in [
            ("name", self.name.as_deref()),
            ("description", self.description.as_deref()),
            ("family", self.family.as_deref()),
            ("knowledge", self.knowledge.as_deref()),
            ("release_date", self.release_date.as_deref()),
            ("last_updated", self.last_updated.as_deref()),
        ] {
            if value.is_some_and(|value| value.len() > 4096 || value.chars().any(char::is_control))
            {
                anyhow::bail!("invalid Provider Model {field}");
            }
        }
        if let Some(modalities) = &self.modalities {
            validate_string_values("modalities.input", &modalities.input)?;
            validate_string_values("modalities.output", &modalities.output)?;
        }
        if let Some(options) = &self.reasoning_options {
            let mut seen_types = 0_u8;
            for option in options {
                let (type_name, type_bit) = option.discriminator();
                if seen_types & type_bit != 0 {
                    anyhow::bail!("duplicate Provider Model reasoning option type `{type_name}`");
                }
                seen_types |= type_bit;
                option.validate()?;
            }
        }
        if let Some(cost) = &self.cost {
            cost.validate()?;
        }
        Ok(())
    }
}

/// 未匹配占位规格：最常见的文本对话模型假设，发现后仍可人工改正或由同步用真实元数据替换。
const UNMATCHED_DEFAULT_CONTEXT_TOKENS: u64 = 256 * 1024;

fn unmatched_default_limit() -> ModelLimit {
    ModelLimit {
        context: Some(UNMATCHED_DEFAULT_CONTEXT_TOKENS),
        ..ModelLimit::default()
    }
}

fn unmatched_default_modalities() -> ModelModalities {
    ModelModalities {
        input: vec!["text".to_string()],
        output: vec!["text".to_string()],
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelModalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelLimit {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Interleaved {
    Enabled(bool),
    Field { field: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Toggle,
    Effort { values: Vec<Option<String>> },
    BudgetTokens { min: Option<i64>, max: Option<u64> },
}

impl ReasoningOption {
    fn discriminator(&self) -> (&'static str, u8) {
        match self {
            Self::Toggle => ("toggle", 1),
            Self::Effort { .. } => ("effort", 2),
            Self::BudgetTokens { .. } => ("budget_tokens", 4),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Toggle => Ok(()),
            Self::Effort { values } => {
                validate_optional_string_values("reasoning_options.values", values)
            }
            Self::BudgetTokens { min, max } => {
                if min.is_some_and(|min| min < -1)
                    || matches!((min, max), (Some(min), Some(max)) if *min >= 0 && *min as u64 > *max)
                {
                    anyhow::bail!("invalid reasoning token budget");
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PriceComponents {
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub input: Option<Decimal>,
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<Decimal>,
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning: Option<Decimal>,
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_read: Option<Decimal>,
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write: Option<Decimal>,
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub input_audio: Option<Decimal>,
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_audio: Option<Decimal>,
}

impl PriceComponents {
    fn validate(&self) -> anyhow::Result<()> {
        for value in [
            self.input,
            self.output,
            self.reasoning,
            self.cache_read,
            self.cache_write,
            self.input_audio,
            self.output_audio,
        ] {
            if value.is_some_and(|value| value.is_sign_negative()) {
                anyhow::bail!("Provider Model costs must be non-negative");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCost {
    #[serde(flatten)]
    pub prices: PriceComponents,
    pub context_over_200k: Option<PriceComponents>,
    pub tiers: Vec<ModelCostTier>,
}

impl ModelCost {
    fn validate(&self) -> anyhow::Result<()> {
        self.prices.validate()?;
        if let Some(prices) = &self.context_over_200k {
            prices.validate()?;
        }
        let mut thresholds = std::collections::BTreeSet::new();
        for tier in &self.tiers {
            if tier.tier.kind != "context" {
                anyhow::bail!("unsupported Provider Model cost tier type");
            }
            if !thresholds.insert(tier.tier.size) {
                anyhow::bail!("duplicate Provider Model cost tier threshold");
            }
            tier.prices().validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCostTier {
    pub tier: ModelCostTierThreshold,
    #[serde(flatten)]
    pub prices: PriceComponents,
}

impl ModelCostTier {
    fn prices(&self) -> PriceComponents {
        self.prices.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCostTierThreshold {
    #[serde(rename = "type")]
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelCostRuleKind {
    ContextOver200k,
    Tier,
}

impl ProviderModelCostRuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextOver200k => "context_over_200k",
            Self::Tier => "tier",
        }
    }
}

impl FromStr for ProviderModelCostRuleKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "context_over_200k" => Ok(Self::ContextOver200k),
            "tier" => Ok(Self::Tier),
            _ => anyhow::bail!("invalid Provider Model cost rule kind: {value}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelCostRule {
    pub rule_index: i64,
    pub kind: ProviderModelCostRuleKind,
    pub threshold_tokens: u64,
    pub prices: PriceComponents,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelRecord {
    pub provider_id: String,
    pub model_id: String,
    pub source_kind: ProviderModelSourceKind,
    pub metadata_source_provider_id: Option<String>,
    pub presence: ProviderModelPresence,
    pub selection_policy: ProviderModelSelectionPolicy,
    pub metadata: ProviderModelMetadata,
    pub revision: i64,
    pub created_at: String,
    pub updated_at: String,
    pub cost_rules: Vec<ProviderModelCostRule>,
}

impl ProviderModelRecord {
    pub fn effective_available(&self) -> bool {
        match self.selection_policy {
            ProviderModelSelectionPolicy::ForceEnabled => true,
            ProviderModelSelectionPolicy::ForceDisabled => false,
            ProviderModelSelectionPolicy::Auto => {
                (self.source_kind == ProviderModelSourceKind::Manual
                    || self.presence == ProviderModelPresence::Present)
                    && self.metadata.lifecycle_status() != Some("deprecated")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderModelMutation {
    Applied(Box<ProviderModelRecord>),
    NotFound,
    Conflict,
}

#[derive(Debug, Clone)]
pub struct NewProviderModelRecord {
    pub provider_id: String,
    pub model_id: String,
    pub source_kind: ProviderModelSourceKind,
    pub metadata_source_provider_id: Option<String>,
    pub presence: ProviderModelPresence,
    pub selection_policy: ProviderModelSelectionPolicy,
    pub metadata: ProviderModelMetadata,
}

#[derive(Debug, Clone)]
pub struct ProviderModelPresenceUpdate {
    pub model_id: String,
    pub presence: ProviderModelPresence,
    pub lifecycle_status: Option<String>,
    pub metadata: Option<ProviderModelMetadata>,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderModelReconciliation {
    pub inserts: Vec<NewProviderModelRecord>,
    pub updates: Vec<ProviderModelPresenceUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelSyncSummary {
    pub added: usize,
    pub missing: usize,
    pub restored: usize,
    pub deprecated: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelSummary {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub source_kind: ProviderModelSourceKind,
    pub selection_policy: ProviderModelSelectionPolicy,
    pub specification: ModelSpecification,
    pub revision: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelSpecification {
    pub limit: Option<ModelLimit>,
    pub modalities: Option<ModelModalities>,
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    pub structured_output: Option<bool>,
    pub attachment: Option<bool>,
    pub temperature: Option<bool>,
}

impl From<&ProviderModelRecord> for ProviderModelSummary {
    fn from(record: &ProviderModelRecord) -> Self {
        Self {
            id: record.model_id.clone(),
            name: record
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| record.model_id.clone()),
            available: record.effective_available(),
            source_kind: record.source_kind,
            selection_policy: record.selection_policy,
            specification: ModelSpecification {
                limit: record.metadata.limit.clone(),
                modalities: record.metadata.modalities.clone(),
                reasoning: record.metadata.reasoning,
                tool_call: record.metadata.tool_call,
                structured_output: record.metadata.structured_output,
                attachment: record.metadata.attachment,
                temperature: record.metadata.temperature,
            },
            revision: record.revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelDetail {
    pub id: String,
    pub available: bool,
    pub source_kind: ProviderModelSourceKind,
    pub can_reimport: bool,
    pub selection_policy: ProviderModelSelectionPolicy,
    pub metadata: ProviderModelMetadata,
    pub thinking_level_map: Vec<crate::thinking::ThinkingLevelMapping>,
    pub extensions: Value,
    pub revision: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl From<ProviderModelRecord> for ProviderModelDetail {
    fn from(record: ProviderModelRecord) -> Self {
        let extensions = record.metadata.extension_value();
        let available = record.effective_available();
        let thinking_level_map = crate::thinking::generate_thinking_level_map(&record.metadata);
        Self {
            id: record.model_id,
            available,
            source_kind: record.source_kind,
            can_reimport: record.source_kind == ProviderModelSourceKind::Discovered
                && record.metadata_source_provider_id.is_some(),
            selection_policy: record.selection_policy,
            metadata: record.metadata,
            thinking_level_map,
            extensions,
            revision: record.revision,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateManualProviderModel {
    pub metadata: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateProviderModel {
    pub metadata: Value,
    pub revision: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateProviderModelSelection {
    pub policy: ProviderModelSelectionPolicy,
    pub revision: i64,
}

pub fn normalize_model_id(model_id: &str) -> anyhow::Result<String> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        anyhow::bail!("model ID cannot be empty");
    }
    if model_id.len() > 512 || model_id.chars().any(char::is_control) {
        anyhow::bail!("model ID is invalid");
    }
    Ok(model_id.to_string())
}

/// 读取侧匹配 `/v1/models` 清单 ID 使用的键：取 `/` 分隔的最右段并忽略大小写。
///
/// 清单 ID 可能带命名空间前缀或大小写差异（`zhipuai/glm-4.6` vs `glm-4.6`、
/// `GLM-4.6`），而路由 Target 的 model 保留用户输入，因此用归一化键比较提高命中率。
pub fn model_id_match_key(model_id: &str) -> String {
    model_id
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn validate_string_values(field: &str, values: &[String]) -> anyhow::Result<()> {
    if values.len() > 128 {
        anyhow::bail!("too many Provider Model {field} values");
    }
    for value in values {
        if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            anyhow::bail!("invalid Provider Model {field} value");
        }
    }
    Ok(())
}
fn validate_optional_string_values(field: &str, values: &[Option<String>]) -> anyhow::Result<()> {
    if values.len() > 128 {
        anyhow::bail!("too many Provider Model {field} values");
    }
    for value in values.iter().flatten() {
        if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            anyhow::bail!("invalid Provider Model {field} value");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ProviderModelMetadata, model_id_match_key};

    #[test]
    fn match_key_takes_rightmost_segment_case_insensitively() {
        assert_eq!(model_id_match_key("glm-4.6"), "glm-4.6");
        assert_eq!(model_id_match_key("zhipuai/glm-4.6"), "glm-4.6");
        assert_eq!(model_id_match_key("GLM-4.6"), "glm-4.6");
        assert_eq!(model_id_match_key("Zhipu/GLM-4.6"), "glm-4.6");
        assert_eq!(model_id_match_key("  glm-4.6  "), "glm-4.6");
        assert_eq!(model_id_match_key(""), "");
    }

    #[test]
    fn lacks_registered_specification_matches_bare_metadata() {
        let bare = ProviderModelMetadata::bare("glm-5.1");
        assert_eq!(bare.reasoning, Some(true));
        assert_eq!(bare.tool_call, Some(true));
        assert_eq!(
            bare.limit.as_ref().and_then(|limit| limit.context),
            Some(256 * 1024)
        );
        assert_eq!(
            bare.modalities
                .as_ref()
                .map(|modalities| modalities.input.as_slice()),
            Some(["text".to_string()].as_slice())
        );
        assert_eq!(
            bare.modalities
                .as_ref()
                .map(|modalities| modalities.output.as_slice()),
            Some(["text".to_string()].as_slice())
        );
        // 占位默认不算已登记,后续同步仍可补模板。
        assert!(bare.lacks_registered_specification());
        assert!(!bare.is_identity_only());

        // 占位之外的任何规格修改都视为已登记。
        let mut touched = ProviderModelMetadata::bare("glm-5.1");
        touched.temperature = Some(false);
        assert!(!touched.lacks_registered_specification());

        let specified = ProviderModelMetadata::from_value(
            "glm-5.1",
            json!({
                "name": "GLM-5.1",
                "tool_call": true,
                "limit": { "context": 200000, "output": 131072 }
            }),
        )
        .expect("specified metadata");
        assert!(!specified.lacks_registered_specification());

        // 历史空快照:规格全空时仍视为未登记,且允许整体升级。
        let mut legacy = ProviderModelMetadata::default();
        legacy.id = Some("glm-5.1".to_string());
        legacy.name = Some("glm-5.1".to_string());
        assert!(legacy.lacks_registered_specification());
        assert!(legacy.is_identity_only());
        legacy.name = Some("Renamed".to_string());
        assert!(legacy.lacks_registered_specification());
        assert!(!legacy.is_identity_only());
    }

    #[test]
    fn reasoning_option_types_are_unique() {
        let metadata = ProviderModelMetadata::from_value(
            "test-model",
            json!({
                "reasoning_options": [
                    {"type": "toggle"},
                    {"type": "effort", "values": ["low", "high"]},
                    {"type": "budget_tokens", "min": 1024, "max": 32768}
                ]
            }),
        )
        .expect("distinct reasoning option types should be accepted");
        assert_eq!(metadata.reasoning_options.unwrap().len(), 3);

        let error = ProviderModelMetadata::from_value(
            "test-model",
            json!({
                "reasoning_options": [
                    {"type": "effort", "values": ["low"]},
                    {"type": "effort", "values": ["high"]}
                ]
            }),
        )
        .expect_err("duplicate reasoning option types should be rejected");
        assert!(
            error
                .to_string()
                .contains("duplicate Provider Model reasoning option type `effort`")
        );
    }
}
