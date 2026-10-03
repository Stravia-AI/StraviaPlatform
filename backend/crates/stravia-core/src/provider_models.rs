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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceStamp {
    ProviderCatalog { provider_id: String },
    Canonical { model_id: String },
    Discovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SnapshotState {
    Unregistered,
    Imported { source: SourceStamp },
    Edited { source: Option<SourceStamp> },
}

impl SnapshotState {
    pub fn source(&self) -> Option<&SourceStamp> {
        match self {
            Self::Unregistered => None,
            Self::Imported { source } => Some(source),
            Self::Edited { source } => source.as_ref(),
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
    pub open_weights: Option<bool>,
    pub reasoning_efforts: Option<Vec<String>>,
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    pub modalities: Option<ModelModalities>,
    pub limit: Option<ModelLimit>,
    pub cost: Option<ModelCost>,
    pub status: Option<String>,
    pub experimental: Option<Value>,
    pub provider: Option<Value>,
    #[serde(
        flatten,
        deserialize_with = "deserialize_metadata_extensions",
        serialize_with = "serialize_metadata_extensions"
    )]
    pub extensions: BTreeMap<String, Value>,
}

fn legacy_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "attachment"
            | "reasoning"
            | "tool_call"
            | "structured_output"
            | "temperature"
            | "interleaved"
            | "reasoning_options"
            | "reasoning_levels"
            | "thinking_toggle"
    )
}

fn deserialize_metadata_extensions<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Value>, D::Error> {
    let mut values = BTreeMap::<String, Value>::deserialize(deserializer)?;
    values.retain(|key, _| !legacy_metadata_key(key));
    Ok(values)
}

fn serialize_metadata_extensions<S: serde::Serializer>(
    values: &BTreeMap<String, Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(None)?;
    for (key, value) in values {
        if !legacy_metadata_key(key) {
            map.serialize_entry(key, value)?;
        }
    }
    map.end()
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
        let efforts = crate::provider_catalog::source_reasoning_efforts(object)?;
        if let Some(efforts) = efforts {
            object.insert("reasoning_efforts".into(), serde_json::to_value(efforts)?);
        }
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

    /// Unknown specifications remain unknown until an authoritative source supplies them.
    pub fn bare(model_id: &str) -> Self {
        Self {
            id: Some(model_id.to_string()),
            name: Some(model_id.to_string()),
            ..Self::default()
        }
    }

    pub fn has_specification(&self) -> bool {
        let declared_limit = self
            .limit
            .as_ref()
            .is_some_and(|limit| limit.context.is_some());
        let declared_modalities = self.modalities.as_ref().is_some_and(|modalities| {
            !modalities.input.is_empty() || !modalities.output.is_empty()
        });
        declared_limit
            || declared_modalities
            || self.open_weights.is_some()
            || self.reasoning_efforts.is_some()
            || self.cost.is_some()
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        serde_json::to_value(self).context("encode Provider Model metadata")
    }

    pub fn extension_value(&self) -> Value {
        Value::Object(Map::from_iter(
            self.extensions
                .iter()
                .filter(|(key, _)| !legacy_metadata_key(key))
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
        if let Some(efforts) = &self.reasoning_efforts {
            validate_string_values("reasoning_efforts", efforts)?;
            if efforts.iter().any(|value| {
                let value = value.trim();
                value.eq_ignore_ascii_case("default") || value.eq_ignore_ascii_case("null")
            }) {
                anyhow::bail!("reasoning_efforts cannot contain default or null");
            }
        }
        if let Some(cost) = &self.cost {
            cost.validate()?;
        }
        Ok(())
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
    pub cache_read: Option<Decimal>,
    #[serde(
        with = "rust_decimal::serde::arbitrary_precision_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write: Option<Decimal>,
}

impl PriceComponents {
    fn validate(&self) -> anyhow::Result<()> {
        for value in [self.input, self.output, self.cache_read, self.cache_write] {
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
    pub snapshot_state: SnapshotState,
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
pub struct ReimportProviderModel {
    pub metadata: ProviderModelMetadata,
    pub source_provider_id: String,
    pub expected_revision: i64,
    pub generated_thinking_level_map: Vec<crate::thinking::ThinkingLevelMapping>,
}

/// 在原子写入内，依据新规格校验最新 Target 的合并结果；失败则整笔回滚。
pub type ReimportThinkingMapValidator<'a> = dyn Fn(&ProviderModelMetadata, &[crate::thinking::ThinkingLevelMapping]) -> anyhow::Result<()>
    + Send
    + Sync
    + 'a;

#[derive(Debug, Clone)]
pub enum ProviderModelReimport {
    Applied {
        model: Box<ProviderModelRecord>,
        active_routes: Vec<crate::db::models::RouteConfig>,
    },
    NotFound,
    Conflict,
}

#[derive(Debug, Clone)]
pub struct NewProviderModelRecord {
    pub provider_id: String,
    pub model_id: String,
    pub source_kind: ProviderModelSourceKind,
    pub snapshot_state: SnapshotState,
    pub metadata_source_provider_id: Option<String>,
    pub presence: ProviderModelPresence,
    pub selection_policy: ProviderModelSelectionPolicy,
    pub metadata: ProviderModelMetadata,
}

#[derive(Debug, Clone)]
pub struct ProviderModelPresenceUpdate {
    pub model_id: String,
    pub expected_revision: i64,
    pub snapshot_state: Option<SnapshotState>,
    pub metadata_source_provider_id: Option<String>,
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
    pub snapshot_state: SnapshotState,
    pub selection_policy: ProviderModelSelectionPolicy,
    pub specification: ModelSpecification,
    pub revision: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelSpecification {
    pub reasoning_efforts: Option<Vec<String>>,
    pub limit: Option<ModelLimit>,
    pub modalities: Option<ModelModalities>,
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
            snapshot_state: record.snapshot_state.clone(),
            selection_policy: record.selection_policy,
            specification: ModelSpecification {
                reasoning_efforts: record.metadata.reasoning_efforts.clone(),
                limit: record.metadata.limit.clone(),
                modalities: record.metadata.modalities.clone(),
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
    pub snapshot_state: SnapshotState,
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
            snapshot_state: record.snapshot_state,
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
    #[serde(default)]
    pub template_id: Option<String>,
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
#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::provider_models::{SnapshotState, SourceStamp};

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
    fn unknown_snapshot_does_not_invent_specifications() {
        let bare = ProviderModelMetadata::bare("glm-5.1");
        assert_eq!(bare.id.as_deref(), Some("glm-5.1"));
        assert!(!bare.has_specification());
        assert!(bare.limit.is_none());
        assert!(bare.modalities.is_none());
        assert!(bare.reasoning_efforts.is_none());

        let state = SnapshotState::Edited {
            source: Some(SourceStamp::Canonical {
                model_id: "template/model".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            json!({"type":"edited","source":{"type":"canonical","model_id":"template/model"}})
        );
        assert_eq!(
            serde_json::from_value::<SnapshotState>(json!({"type":"edited","source":null}))
                .unwrap()
                .source(),
            None
        );
    }

    #[test]
    fn legacy_keys_never_escape_metadata_extensions() {
        let metadata: ProviderModelMetadata = serde_json::from_value(json!({
            "reasoning": true, "tool_call": true, "interleaved": true,
            "reasoning_options": [{"type":"effort", "values":["low"]}],
            "limit": {"context": 8192, "input": 4096, "output": 1024},
            "native_protocol_fact": {"enabled":true}
        }))
        .unwrap();
        let encoded = metadata.to_value().unwrap();
        for key in ["reasoning", "tool_call", "interleaved", "reasoning_options"] {
            assert!(encoded.get(key).is_none());
            assert!(metadata.extension_value().get(key).is_none());
        }
        assert_eq!(encoded["limit"], json!({"context":8192}));
        assert_eq!(encoded["native_protocol_fact"], json!({"enabled":true}));
        let source = ProviderModelMetadata::from_source_value("model", json!({"reasoning_options":[{"type":"toggle"}, {"type":"effort","values":[null,"default","low","custom"]}]})).unwrap();
        assert_eq!(source.reasoning_efforts.unwrap(), ["low", "custom"]);
    }

    #[test]
    fn nullable_effort_spec_is_unknown_but_null_effort_is_invalid() {
        let metadata = ProviderModelMetadata::from_source_value(
            "model",
            json!({"limit":{"context":8192}, "reasoning_efforts":null}),
        )
        .unwrap();
        assert_eq!(metadata.limit.unwrap().context, Some(8192));
        assert!(metadata.reasoning_efforts.is_none());
        assert!(
            ProviderModelMetadata::from_source_value("model", json!({"reasoning_efforts":[null]}),)
                .is_err()
        );
    }

    #[test]
    fn efforts_reject_default_null_and_empty_strings() {
        for effort in ["default", "null", "", "  "] {
            assert!(
                ProviderModelMetadata::from_value(
                    "test-model",
                    json!({"reasoning_efforts": [effort]})
                )
                .is_err()
            );
        }
        let metadata = ProviderModelMetadata::from_value(
            "test-model",
            json!({"reasoning_efforts": ["custom", "none", "max"]}),
        )
        .unwrap();
        assert_eq!(
            metadata.reasoning_efforts.unwrap(),
            ["custom", "none", "max"]
        );
    }
}
