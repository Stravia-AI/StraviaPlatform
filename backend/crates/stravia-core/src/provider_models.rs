use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::LazyLock;

pub mod pinning_migration;

use crate::db::models::RouteOutputModality;
use crate::pricing::{CustomCurrency, PeakPricing, PriceField};
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

/// 可逐字段固定的 Provider Model 规格字段；标识与编辑器控件一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProviderModelField {
    #[serde(rename = "name")]
    Name,
    #[serde(rename = "description")]
    Description,
    #[serde(rename = "limit.context")]
    LimitContext,
    #[serde(rename = "modalities.input")]
    ModalitiesInput,
    #[serde(rename = "modalities.output")]
    ModalitiesOutput,
    #[serde(rename = "reasoning_efforts")]
    ReasoningEfforts,
    #[serde(rename = "cost.input")]
    CostInput,
    #[serde(rename = "cost.output")]
    CostOutput,
    #[serde(rename = "cost.cache_read")]
    CostCacheRead,
    #[serde(rename = "cost.cache_write")]
    CostCacheWrite,
    #[serde(rename = "cost.tiers")]
    CostTiers,
}

impl ProviderModelField {
    pub const ALL: [Self; 11] = [
        Self::Name,
        Self::Description,
        Self::LimitContext,
        Self::ModalitiesInput,
        Self::ModalitiesOutput,
        Self::ReasoningEfforts,
        Self::CostInput,
        Self::CostOutput,
        Self::CostCacheRead,
        Self::CostCacheWrite,
        Self::CostTiers,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Description => "description",
            Self::LimitContext => "limit.context",
            Self::ModalitiesInput => "modalities.input",
            Self::ModalitiesOutput => "modalities.output",
            Self::ReasoningEfforts => "reasoning_efforts",
            Self::CostInput => "cost.input",
            Self::CostOutput => "cost.output",
            Self::CostCacheRead => "cost.cache_read",
            Self::CostCacheWrite => "cost.cache_write",
            Self::CostTiers => "cost.tiers",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|field| field.as_str() == value.trim())
    }

    fn cost_key(self) -> Option<&'static str> {
        match self {
            Self::CostInput => Some("input"),
            Self::CostOutput => Some("output"),
            Self::CostCacheRead => Some("cache_read"),
            Self::CostCacheWrite => Some("cache_write"),
            _ => None,
        }
    }
}

impl std::fmt::Display for ProviderModelField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotState {
    /// 没有生效规格，也没有同步来源值。
    Unregistered,
    /// 跟随同步来源的规格；`pinned` 中的人工修改字段不再随同步变化。
    Imported {
        source: SourceStamp,
        pinned: BTreeSet<ProviderModelField>,
    },
    /// 没有同步来源的手动规格；JSON 沿用 `{"type":"edited","source":null}`。
    Local,
}

impl SnapshotState {
    pub fn source(&self) -> Option<&SourceStamp> {
        match self {
            Self::Unregistered | Self::Local => None,
            Self::Imported { source, .. } => Some(source),
        }
    }

    pub fn pinned(&self) -> &BTreeSet<ProviderModelField> {
        static EMPTY: LazyLock<BTreeSet<ProviderModelField>> = LazyLock::new(BTreeSet::new);
        match self {
            Self::Imported { pinned, .. } => pinned,
            Self::Unregistered | Self::Local => &EMPTY,
        }
    }

    pub fn pinned_fields(&self) -> Vec<ProviderModelField> {
        self.pinned().iter().copied().collect()
    }

    pub fn is_imported(&self) -> bool {
        matches!(self, Self::Imported { .. })
    }
}

impl Serialize for SnapshotState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            Self::Unregistered => map.serialize_entry("type", "unregistered")?,
            Self::Imported { source, pinned } => {
                map.serialize_entry("type", "imported")?;
                map.serialize_entry("source", source)?;
                map.serialize_entry("pinned", pinned)?;
            }
            Self::Local => {
                map.serialize_entry("type", "edited")?;
                map.serialize_entry("source", &Option::<SourceStamp>::None)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SnapshotState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Raw {
            Unregistered,
            Imported {
                source: SourceStamp,
                #[serde(default)]
                pinned: BTreeSet<ProviderModelField>,
            },
            Edited {
                source: Option<SourceStamp>,
            },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Unregistered => Ok(Self::Unregistered),
            Raw::Imported { source, pinned } => Ok(Self::Imported { source, pinned }),
            Raw::Edited { source: None } => Ok(Self::Local),
            Raw::Edited { source: Some(_) } => Err(serde::de::Error::custom(
                "Provider Model snapshot state 'edited' cannot carry a source; run the pinning migration",
            )),
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
        let mut metadata = Self::from_value(model_id, value)?;
        // 目录导入与模型发现得到的价格一律去掉只来自管理写入的本地字段。
        crate::pricing::strip_source_local_cost_fields(&mut metadata.cost);
        Ok(metadata)
    }

    /// Unknown specifications remain unknown until an authoritative source supplies them.
    pub fn bare(model_id: &str) -> Self {
        Self {
            id: Some(model_id.to_string()),
            name: Some(model_id.to_string()),
            ..Self::default()
        }
    }

    /// 未登记模态按文本输入输出使用；不写回快照，避免默认值被视为上游规格。
    pub fn effective_modalities(&self) -> &ModelModalities {
        static TEXT_MODALITIES: LazyLock<ModelModalities> =
            LazyLock::new(ModelModalities::text_only);
        self.modalities.as_ref().unwrap_or(&TEXT_MODALITIES)
    }

    /// Route eligibility uses the persisted declaration, never display defaults.
    pub fn supports_route_output_modality(&self, modality: RouteOutputModality) -> bool {
        self.modalities.as_ref().is_some_and(|modalities| {
            modalities
                .output
                .iter()
                .any(|output| output == modality.as_str())
        })
    }

    pub fn has_specification(&self) -> bool {
        let declared_limit = self
            .limit
            .as_ref()
            .is_some_and(|limit| limit.context.is_some());
        let declared_modalities = self.modalities.is_some();
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

impl ModelModalities {
    pub fn text_only() -> Self {
        Self {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelLimit {
    pub context: Option<u64>,
}

/// 价格以十进制字符串对外序列化；读取仍接受来源目录中的 JSON 数字。
mod decimal_string_option {
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use serde::de::{self, Visitor};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_str(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Decimal>, D::Error> {
        struct OptionalDecimalVisitor;

        impl<'de> Visitor<'de> for OptionalDecimalVisitor {
            type Value = Option<Decimal>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an optional decimal number or decimal string")
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D2: Deserializer<'de>>(
                self,
                deserializer: D2,
            ) -> Result<Self::Value, D2::Error> {
                <Decimal as Deserialize>::deserialize(deserializer).map(Some)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Decimal::from_str(value).map(Some).map_err(E::custom)
            }
        }

        deserializer.deserialize_option(OptionalDecimalVisitor)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PriceComponents {
    #[serde(
        with = "decimal_string_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub input: Option<Decimal>,
    #[serde(
        with = "decimal_string_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<Decimal>,
    #[serde(
        with = "decimal_string_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_read: Option<Decimal>,
    #[serde(
        with = "decimal_string_option",
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
    /// 以自定义币种填写的价格字段；只来自管理写入，目录来源恒为空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_currency_fields: Vec<PriceField>,
    /// 至多一条峰谷规则；只来自管理写入，目录来源恒为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak: Option<PeakPricing>,
}

impl ModelCost {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
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
            tier.prices.validate()?;
        }
        let mut custom_fields = std::collections::BTreeSet::new();
        for field in &self.custom_currency_fields {
            if !custom_fields.insert(*field) {
                anyhow::bail!("duplicate custom currency price field");
            }
        }
        if let Some(peak) = &self.peak {
            peak.validate()?;
        }
        Ok(())
    }

    /// 自定义币种价格字段只在实例已设定自定义币种时合法。
    pub fn validate_currency(&self, currency: Option<&CustomCurrency>) -> anyhow::Result<()> {
        if currency.is_none() && !self.custom_currency_fields.is_empty() {
            anyhow::bail!("custom currency price fields require a configured custom currency");
        }
        Ok(())
    }

    /// 基础价格换算成美元；自定义币种价格在未设定币种时不可用。
    pub fn base_price_in_usd(
        &self,
        field: PriceField,
        currency: Option<&CustomCurrency>,
    ) -> Option<Decimal> {
        let value = match field {
            PriceField::Input => self.prices.input,
            PriceField::Output => self.prices.output,
            PriceField::CacheRead => self.prices.cache_read,
            PriceField::CacheWrite => self.prices.cache_write,
            PriceField::Tiers => return None,
        };
        crate::pricing::usd_price(
            value?,
            self.custom_currency_fields.contains(&field),
            currency,
        )
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
    /// 最近一次成功同步得到的来源规格；不参与生效规格的读取合并。
    pub source_metadata: Option<ProviderModelMetadata>,
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
    pub fn route_output_modalities(&self) -> Vec<RouteOutputModality> {
        if !self.effective_available() {
            return Vec::new();
        }
        [RouteOutputModality::Text, RouteOutputModality::Image]
            .into_iter()
            .filter(|modality| self.metadata.supports_route_output_modality(*modality))
            .collect()
    }

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

fn ensure_json_object<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> &'a mut Map<String, Value> {
    if !object.get(key).is_some_and(Value::is_object) {
        object.insert(key.to_owned(), Value::Object(Map::new()));
    }
    object
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("object inserted above")
}

fn prune_empty_object(object: &mut Map<String, Value>, key: &str) {
    if object
        .get(key)
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        object.remove(key);
    }
}

fn copy_json_key(target: &mut Map<String, Value>, source: &Map<String, Value>, key: &str) {
    match source.get(key) {
        Some(value) => {
            target.insert(key.to_owned(), value.clone());
        }
        None => {
            target.remove(key);
        }
    }
}

fn copy_nested_key(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    parent: &str,
    key: &str,
) {
    let value = source
        .get(parent)
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .cloned();
    match value {
        Some(value) => {
            ensure_json_object(target, parent).insert(key.to_owned(), value);
        }
        None => {
            if let Some(object) = target.get_mut(parent).and_then(Value::as_object_mut) {
                object.remove(key);
            }
            prune_empty_object(target, parent);
        }
    }
}

fn custom_currency_marker(cost: Option<&Map<String, Value>>, field: &str) -> bool {
    cost.and_then(|cost| cost.get("custom_currency_fields"))
        .and_then(Value::as_array)
        .is_some_and(|fields| fields.iter().any(|value| value.as_str() == Some(field)))
}

fn set_custom_currency_marker(cost: &mut Map<String, Value>, field: &str, custom: bool) {
    let mut markers = cost
        .get("custom_currency_fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    if custom {
        markers.insert(field.to_owned());
    } else {
        markers.remove(field);
    }
    if markers.is_empty() {
        cost.remove("custom_currency_fields");
    } else {
        cost.insert(
            "custom_currency_fields".to_owned(),
            Value::Array(markers.into_iter().map(Value::String).collect()),
        );
    }
}

fn cost_is_empty(cost: &Map<String, Value>) -> bool {
    let prices_empty = ["input", "output", "cache_read", "cache_write"]
        .iter()
        .all(|key| cost.get(*key).is_none_or(Value::is_null));
    let tiers_empty = cost.get("context_over_200k").is_none_or(Value::is_null)
        && cost
            .get("tiers")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
    let peak_empty = cost.get("peak").is_none_or(Value::is_null);
    let markers_empty = cost
        .get("custom_currency_fields")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    prices_empty && tiers_empty && peak_empty && markers_empty
}

fn prune_cost(object: &mut Map<String, Value>) {
    if object
        .get("cost")
        .and_then(Value::as_object)
        .is_some_and(cost_is_empty)
    {
        object.remove("cost");
    }
}

fn copy_pinned_field(
    effective: &mut Map<String, Value>,
    current: &Map<String, Value>,
    field: ProviderModelField,
) {
    match field {
        ProviderModelField::Name => copy_json_key(effective, current, "name"),
        ProviderModelField::Description => copy_json_key(effective, current, "description"),
        ProviderModelField::LimitContext => copy_nested_key(effective, current, "limit", "context"),
        ProviderModelField::ModalitiesInput => {
            copy_nested_key(effective, current, "modalities", "input")
        }
        ProviderModelField::ModalitiesOutput => {
            copy_nested_key(effective, current, "modalities", "output")
        }
        ProviderModelField::ReasoningEfforts => {
            copy_json_key(effective, current, "reasoning_efforts")
        }
        ProviderModelField::CostInput
        | ProviderModelField::CostOutput
        | ProviderModelField::CostCacheRead
        | ProviderModelField::CostCacheWrite => {
            copy_nested_key(
                effective,
                current,
                "cost",
                field.cost_key().expect("cost price field"),
            );
        }
        ProviderModelField::CostTiers => {
            for key in ["context_over_200k", "tiers"] {
                copy_nested_key(effective, current, "cost", key);
            }
        }
    }
}

fn local_cost_marker(
    effective: &mut Map<String, Value>,
    current: &Map<String, Value>,
    field: ProviderModelField,
) {
    let current_cost = current.get("cost").and_then(Value::as_object);
    let marker = field.cost_key().unwrap_or("tiers");
    if custom_currency_marker(current_cost, marker) {
        set_custom_currency_marker(ensure_json_object(effective, "cost"), marker, true);
    }
}

/// 同步合并：来源规格决定未固定字段，已固定字段沿用生效规格。
///
/// 峰谷定价与币种标记是纯本地设置，来源永远不携带；峰谷始终保留，币种标记
/// 只随仍被固定的价格字段保留。来源缺少已固定字段时该字段变为未登记。
pub fn merge_source_specification(
    current: &ProviderModelMetadata,
    source: &ProviderModelMetadata,
    pinned: &BTreeSet<ProviderModelField>,
) -> anyhow::Result<ProviderModelMetadata> {
    let current_value = current.to_value()?;
    let source_value = source.to_value()?;
    let current_object = current_value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata must be an object"))?;
    let source_object = source_value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata must be an object"))?;
    let mut effective = source_object.clone();
    if let Some(peak) = current_object
        .get("cost")
        .and_then(Value::as_object)
        .and_then(|cost| cost.get("peak"))
        .filter(|peak| !peak.is_null())
    {
        ensure_json_object(&mut effective, "cost").insert("peak".to_owned(), peak.clone());
    }
    for field in pinned {
        copy_pinned_field(&mut effective, current_object, *field);
        if matches!(
            field,
            ProviderModelField::CostInput
                | ProviderModelField::CostOutput
                | ProviderModelField::CostCacheRead
                | ProviderModelField::CostCacheWrite
                | ProviderModelField::CostTiers
        ) {
            local_cost_marker(&mut effective, current_object, *field);
        }
    }
    if let Some(cost) = effective.get_mut("cost").and_then(Value::as_object_mut) {
        // Rebuild the marker set from pinned fields only; unpinned prices follow
        // the always-USD source.
        let markers = pinned
            .iter()
            .filter_map(|field| match field {
                ProviderModelField::CostInput
                | ProviderModelField::CostOutput
                | ProviderModelField::CostCacheRead
                | ProviderModelField::CostCacheWrite => field.cost_key(),
                ProviderModelField::CostTiers => Some("tiers"),
                _ => None,
            })
            .filter(|marker| {
                custom_currency_marker(
                    current_object.get("cost").and_then(Value::as_object),
                    marker,
                )
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if markers.is_empty() {
            cost.remove("custom_currency_fields");
        } else {
            cost.insert(
                "custom_currency_fields".to_owned(),
                Value::Array(markers.into_iter().map(Value::String).collect()),
            );
        }
    }
    prune_cost(&mut effective);
    let model_id = source
        .id
        .as_deref()
        .or(current.id.as_deref())
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata is missing id"))?;
    ProviderModelMetadata::from_value(model_id, Value::Object(effective))
}

/// 恢复同步：把来源值写回指定字段并取消固定；来源缺少该字段时恢复为未登记。
pub fn restore_source_fields(
    current: &ProviderModelMetadata,
    source: &ProviderModelMetadata,
    fields: &[ProviderModelField],
) -> anyhow::Result<ProviderModelMetadata> {
    let current_value = current.to_value()?;
    let source_value = source.to_value()?;
    let source_object = source_value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata must be an object"))?;
    let mut effective = current_value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata must be an object"))?;
    for field in fields {
        copy_pinned_field(&mut effective, source_object, *field);
        let marker = match field {
            ProviderModelField::CostInput
            | ProviderModelField::CostOutput
            | ProviderModelField::CostCacheRead
            | ProviderModelField::CostCacheWrite => field.cost_key(),
            ProviderModelField::CostTiers => Some("tiers"),
            _ => None,
        };
        if let Some(marker) = marker
            && let Some(cost) = effective.get_mut("cost").and_then(Value::as_object_mut)
        {
            // Source prices are always USD.
            set_custom_currency_marker(cost, marker, false);
        }
    }
    prune_cost(&mut effective);
    let model_id = current
        .id
        .as_deref()
        .or(source.id.as_deref())
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata is missing id"))?;
    ProviderModelMetadata::from_value(model_id, Value::Object(effective))
}

/// 解析变更集键为可固定字段；`cost.peak` 是纯本地字段，不参与固定。
pub fn change_fields(
    changes: &BTreeMap<String, Value>,
) -> anyhow::Result<BTreeSet<ProviderModelField>> {
    let mut fields = BTreeSet::new();
    for key in changes.keys() {
        if key == "cost.peak" {
            continue;
        }
        let field = ProviderModelField::parse(key)
            .ok_or_else(|| anyhow::anyhow!("unsupported Provider Model field: {key}"))?;
        fields.insert(field);
    }
    Ok(fields)
}

fn decimal_from_json(value: &Value, field: &str) -> anyhow::Result<Decimal> {
    let text = match value {
        Value::String(text) => text.trim(),
        _ => anyhow::bail!("Provider Model {field} must be a decimal string"),
    };
    let decimal =
        Decimal::from_str(text).with_context(|| format!("invalid Provider Model {field}"))?;
    anyhow::ensure!(
        !decimal.is_sign_negative(),
        "Provider Model {field} must be non-negative"
    );
    Ok(decimal)
}

fn price_currency(value: &Value, field: &str) -> anyhow::Result<bool> {
    match value.as_object().and_then(|object| object.get("currency")) {
        Some(Value::String(currency)) if currency == "usd" => Ok(false),
        Some(Value::String(currency)) if currency == "custom" => Ok(true),
        Some(Value::String(_)) => {
            anyhow::bail!("Provider Model {field} currency must be usd or custom")
        }
        _ => anyhow::bail!("Provider Model {field} currency is required"),
    }
}

fn price_amount(value: &Value, field: &str) -> anyhow::Result<Decimal> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Provider Model {field} must be an object"))?;
    for key in object.keys() {
        anyhow::ensure!(
            key == "amount" || key == "currency",
            "unsupported Provider Model {field} property: {key}"
        );
    }
    let amount = object
        .get("amount")
        .ok_or_else(|| anyhow::anyhow!("Provider Model {field} amount is required"))?;
    decimal_from_json(amount, &format!("{field} amount"))
}

fn parse_price_components(value: &Value, field: &str) -> anyhow::Result<Map<String, Value>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Provider Model {field} must be an object"))?;
    let mut prices = Map::new();
    for (key, item) in object {
        anyhow::ensure!(
            matches!(
                key.as_str(),
                "input" | "output" | "cache_read" | "cache_write"
            ),
            "unsupported Provider Model {field} property: {key}"
        );
        if item.is_null() {
            continue;
        }
        let decimal = decimal_from_json(item, &format!("{field}.{key}"))?;
        prices.insert(key.clone(), Value::String(decimal.to_string()));
    }
    Ok(prices)
}

fn apply_tiers_change(effective: &mut Map<String, Value>, value: &Value) -> anyhow::Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Provider Model cost.tiers must be an object"))?;
    for key in object.keys() {
        anyhow::ensure!(
            matches!(key.as_str(), "currency" | "context_over_200k" | "tiers"),
            "unsupported Provider Model cost.tiers property: {key}"
        );
    }
    let custom = price_currency(value, "cost.tiers")?;
    let mut replacement = Map::new();
    if let Some(extended) = object
        .get("context_over_200k")
        .filter(|value| !value.is_null())
    {
        let prices = parse_price_components(extended, "cost.tiers.context_over_200k")?;
        if !prices.is_empty() {
            replacement.insert("context_over_200k".to_owned(), Value::Object(prices));
        }
    }
    if let Some(tiers) = object.get("tiers").filter(|value| !value.is_null()) {
        let tiers = tiers
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Provider Model cost.tiers.tiers must be an array"))?;
        let mut parsed = Vec::new();
        for tier in tiers {
            let tier = tier
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Provider Model cost tier must be an object"))?;
            for key in tier.keys() {
                anyhow::ensure!(
                    key == "threshold" || key == "prices",
                    "unsupported Provider Model cost tier property: {key}"
                );
            }
            let threshold = tier
                .get("threshold")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("Provider Model cost tier threshold is required"))?;
            let prices = parse_price_components(
                tier.get("prices").ok_or_else(|| {
                    anyhow::anyhow!("Provider Model cost tier prices are required")
                })?,
                "cost tier prices",
            )?;
            let mut entry = Map::new();
            entry.insert(
                "tier".to_owned(),
                serde_json::json!({ "type": "context", "size": threshold }),
            );
            entry.extend(prices);
            parsed.push(Value::Object(entry));
        }
        if !parsed.is_empty() {
            replacement.insert("tiers".to_owned(), Value::Array(parsed));
        }
    }
    let cost = ensure_json_object(effective, "cost");
    cost.remove("context_over_200k");
    cost.remove("tiers");
    cost.extend(replacement);
    set_custom_currency_marker(cost, "tiers", custom);
    Ok(())
}

/// 把管理写入的字段变更集应用到现有生效规格；出现的字段即被固定。
pub fn apply_provider_model_changes(
    current: &ProviderModelMetadata,
    changes: &BTreeMap<String, Value>,
) -> anyhow::Result<ProviderModelMetadata> {
    let current_value = current.to_value()?;
    let mut effective = current_value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata must be an object"))?;
    for (key, value) in changes {
        match key.as_str() {
            "name" | "description" => {
                if value.is_null() {
                    effective.remove(key);
                } else if let Some(text) = value.as_str() {
                    effective.insert(key.clone(), Value::String(text.to_owned()));
                } else {
                    anyhow::bail!("Provider Model {key} must be a string or null");
                }
            }
            "limit.context" => {
                if value.is_null() {
                    if let Some(limit) = effective.get_mut("limit").and_then(Value::as_object_mut) {
                        limit.remove("context");
                    }
                    prune_empty_object(&mut effective, "limit");
                } else {
                    let context = value.as_u64().ok_or_else(|| {
                        anyhow::anyhow!(
                            "Provider Model limit.context must be a non-negative integer"
                        )
                    })?;
                    ensure_json_object(&mut effective, "limit")
                        .insert("context".to_owned(), Value::from(context));
                }
            }
            "modalities.input" | "modalities.output" => {
                let direction = key.rsplit('.').next().expect("modality direction");
                if value.is_null() {
                    if let Some(modalities) = effective
                        .get_mut("modalities")
                        .and_then(Value::as_object_mut)
                    {
                        modalities.remove(direction);
                    }
                    prune_empty_object(&mut effective, "modalities");
                } else {
                    let values = value.as_array().ok_or_else(|| {
                        anyhow::anyhow!("Provider Model {key} must be an array or null")
                    })?;
                    for item in values {
                        anyhow::ensure!(
                            item.is_string(),
                            "Provider Model {key} values must be strings"
                        );
                    }
                    ensure_json_object(&mut effective, "modalities")
                        .insert(direction.to_owned(), value.clone());
                }
            }
            "reasoning_efforts" => {
                if value.is_null() {
                    effective.remove("reasoning_efforts");
                } else {
                    let values = value.as_array().ok_or_else(|| {
                        anyhow::anyhow!("Provider Model reasoning_efforts must be an array or null")
                    })?;
                    for item in values {
                        anyhow::ensure!(
                            item.is_string(),
                            "Provider Model reasoning_efforts values must be strings"
                        );
                    }
                    effective.insert("reasoning_efforts".to_owned(), value.clone());
                }
            }
            "cost.input" | "cost.output" | "cost.cache_read" | "cost.cache_write" => {
                let price_key = key.rsplit('.').next().expect("price field");
                if value.is_null() {
                    if let Some(cost) = effective.get_mut("cost").and_then(Value::as_object_mut) {
                        cost.remove(price_key);
                        set_custom_currency_marker(cost, price_key, false);
                    }
                } else {
                    let amount = price_amount(value, key)?;
                    let custom = price_currency(value, key)?;
                    let cost = ensure_json_object(&mut effective, "cost");
                    cost.insert(price_key.to_owned(), Value::String(amount.to_string()));
                    set_custom_currency_marker(cost, price_key, custom);
                }
            }
            "cost.tiers" => {
                if value.is_null() {
                    if let Some(cost) = effective.get_mut("cost").and_then(Value::as_object_mut) {
                        cost.remove("context_over_200k");
                        cost.remove("tiers");
                        set_custom_currency_marker(cost, "tiers", false);
                    }
                } else {
                    apply_tiers_change(&mut effective, value)?;
                }
            }
            "cost.peak" => {
                if value.is_null() {
                    if let Some(cost) = effective.get_mut("cost").and_then(Value::as_object_mut) {
                        cost.remove("peak");
                    }
                } else {
                    ensure_json_object(&mut effective, "cost")
                        .insert("peak".to_owned(), value.clone());
                }
            }
            other => anyhow::bail!("unsupported Provider Model field: {other}"),
        }
    }
    prune_cost(&mut effective);
    let model_id = current
        .id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Provider Model metadata is missing id"))?;
    ProviderModelMetadata::from_value(model_id, Value::Object(effective))
}

#[derive(Debug, Clone)]
pub struct RestoreProviderModel {
    pub metadata: ProviderModelMetadata,
    pub snapshot_state: SnapshotState,
    pub expected_revision: i64,
    pub generated_thinking_level_map: Option<Vec<crate::thinking::ThinkingLevelMapping>>,
}

/// 在原子写入内，依据新规格校验最新 Target 的合并结果；失败则整笔回滚。
pub type ProviderModelThinkingMapValidator<'a> = dyn Fn(&str, &ProviderModelMetadata, &[crate::thinking::ThinkingLevelMapping]) -> anyhow::Result<()>
    + Send
    + Sync
    + 'a;

#[derive(Debug, Clone)]
pub enum ProviderModelRestore {
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
    pub source_metadata: Option<ProviderModelMetadata>,
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
    /// 本次同步得到的来源规格；`None` 表示保留现有来源值。
    pub source_metadata: Option<ProviderModelMetadata>,
    /// 推理强度列表变化时在同一事务内重算关联 Target 的 Generated Mapping。
    pub generated_thinking_level_map: Option<Vec<crate::thinking::ThinkingLevelMapping>>,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderModelReconciliation {
    pub inserts: Vec<NewProviderModelRecord>,
    pub updates: Vec<ProviderModelPresenceUpdate>,
    pub sync_summary: Option<ProviderModelSyncSummary>,
    pub change_record: Option<ModelChangeRecord>,
    pub expected_provider: Option<crate::db::models::Provider>,
    pub expected_revisions: Vec<(String, i64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelSyncSummary {
    pub added: usize,
    pub missing: usize,
    pub restored: usize,
    pub deprecated: usize,
    pub checked_at: String,
    pub source: ModelChangeSource,
    pub record_id: Option<String>,
    pub deprecation_cleared: usize,
    pub specification_updated: usize,
    pub outcome: ModelChangeOutcome,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelChangeSource {
    AutomaticSync,
    ManualSync,
    ManualChange,
}

impl ModelChangeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutomaticSync => "automatic_sync",
            Self::ManualSync => "manual_sync",
            Self::ManualChange => "manual_change",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelChangeType {
    Added,
    Missing,
    Restored,
    Deprecated,
    DeprecationCleared,
    SpecificationUpdated,
    ManualAdded,
    SelectionPolicyChanged,
    SyncFailed,
}

impl ModelChangeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Missing => "missing",
            Self::Restored => "restored",
            Self::Deprecated => "deprecated",
            Self::DeprecationCleared => "deprecation_cleared",
            Self::SpecificationUpdated => "specification_updated",
            Self::ManualAdded => "manual_added",
            Self::SelectionPolicyChanged => "selection_policy_changed",
            Self::SyncFailed => "sync_failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelChangeOutcome {
    Success,
    PartialSuccess,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpecificationDiff {
    pub field: String,
    pub before: Value,
    pub after: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelChangeItem {
    pub model_id: String,
    pub name: String,
    pub types: Vec<ModelChangeType>,
    pub specification: Vec<ModelSpecificationDiff>,
    pub before_presence: Option<ProviderModelPresence>,
    pub after_presence: Option<ProviderModelPresence>,
    pub before_deprecated: Option<bool>,
    pub after_deprecated: Option<bool>,
    pub before_policy: Option<ProviderModelSelectionPolicy>,
    pub after_policy: Option<ProviderModelSelectionPolicy>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelChangeRecord {
    pub id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub provider_deleted: bool,
    pub source: ModelChangeSource,
    pub checked_at: String,
    pub outcome: ModelChangeOutcome,
    pub initial_sync: bool,
    pub changes: Vec<ModelChangeItem>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelChangeQuery {
    pub provider_id: Option<String>,
    #[serde(rename = "type")]
    pub change_type: Option<ModelChangeType>,
    pub source: Option<ModelChangeSource>,
    pub q: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelChangePage {
    pub records: Vec<ModelChangeRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModelSyncState {
    pub automatic_sync: bool,
    pub in_progress: bool,
    pub pause_reason: Option<String>,
    pub last_check: Option<ProviderModelSyncSummary>,
    pub has_successful_sync: bool,
}

impl Default for ProviderModelSyncState {
    fn default() -> Self {
        Self {
            automatic_sync: true,
            in_progress: false,
            pause_reason: None,
            last_check: None,
            has_successful_sync: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProviderModelSyncState {
    pub automatic_sync: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderModelSyncFailure {
    pub summary: ProviderModelSyncSummary,
}

impl std::fmt::Display for ProviderModelSyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            self.summary
                .error
                .as_deref()
                .unwrap_or("Model synchronization failed"),
        )
    }
}
impl std::error::Error for ProviderModelSyncFailure {}

pub(crate) fn same_discovery_provider(
    left: &crate::db::models::Provider,
    right: &crate::db::models::Provider,
) -> bool {
    left.id == right.id
        && left.vendor == right.vendor
        && left.protocol == right.protocol
        && left.base_url == right.base_url
        && left.preset_key == right.preset_key
        && left.channel == right.channel
        && left.models_source == right.models_source
        && left.static_models == right.static_models
        && left.api_key == right.api_key
        && left.adapter_credentials == right.adapter_credentials
        && left.vendor_options == right.vendor_options
        && left.auth_mode == right.auth_mode
        && left.use_proxy == right.use_proxy
        && left.is_enabled == right.is_enabled
        && left.updated_at == right.updated_at
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelSummary {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub route_output_modalities: Vec<RouteOutputModality>,
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
            route_output_modalities: record.route_output_modalities(),
            source_kind: record.source_kind,
            snapshot_state: record.snapshot_state.clone(),
            selection_policy: record.selection_policy,
            specification: ModelSpecification {
                reasoning_efforts: record.metadata.reasoning_efforts.clone(),
                limit: record.metadata.limit.clone(),
                modalities: Some(record.metadata.effective_modalities().clone()),
            },
            revision: record.revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelDetail {
    pub id: String,
    pub available: bool,
    pub route_output_modalities: Vec<RouteOutputModality>,
    pub source_kind: ProviderModelSourceKind,
    pub snapshot_state: SnapshotState,
    pub pinned_fields: Vec<ProviderModelField>,
    pub restorable_fields: Vec<ProviderModelField>,
    pub selection_policy: ProviderModelSelectionPolicy,
    pub metadata: ProviderModelMetadata,
    pub thinking_level_map: Vec<crate::thinking::ThinkingLevelMapping>,
    pub extensions: Value,
    pub revision: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl From<ProviderModelRecord> for ProviderModelDetail {
    fn from(mut record: ProviderModelRecord) -> Self {
        let extensions = record.metadata.extension_value();
        let available = record.effective_available();
        let route_output_modalities = record.route_output_modalities();
        let thinking_level_map = crate::thinking::generate_thinking_level_map(&record.metadata);
        let pinned_fields = record.snapshot_state.pinned_fields();
        let restorable_fields =
            if record.snapshot_state.is_imported() && record.source_metadata.is_some() {
                pinned_fields.clone()
            } else {
                Vec::new()
            };
        record
            .metadata
            .modalities
            .get_or_insert_with(ModelModalities::text_only);
        Self {
            id: record.model_id,
            available,
            route_output_modalities,
            source_kind: record.source_kind,
            snapshot_state: record.snapshot_state,
            pinned_fields,
            restorable_fields,
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
    pub revision: i64,
    #[serde(default)]
    pub changes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RestoreProviderModelSync {
    pub revision: i64,
    #[serde(default)]
    pub fields: Vec<String>,
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
    use std::collections::BTreeSet;

    use serde_json::json;

    use crate::provider_models::{ProviderModelField, SnapshotState, SourceStamp};

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

        assert!(matches!(
            serde_json::from_value::<SnapshotState>(json!({"type":"edited","source":null}))
                .unwrap(),
            SnapshotState::Local
        ));
        assert!(
            serde_json::from_value::<SnapshotState>(json!({
                "type":"edited",
                "source":{"type":"canonical","model_id":"template/model"}
            }))
            .is_err()
        );
        let imported = SnapshotState::Imported {
            source: SourceStamp::Canonical {
                model_id: "template/model".into(),
            },
            pinned: BTreeSet::from([ProviderModelField::Name]),
        };
        assert_eq!(
            serde_json::to_value(&imported).unwrap(),
            json!({
                "type":"imported",
                "source":{"type":"canonical","model_id":"template/model"},
                "pinned":["name"]
            })
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
