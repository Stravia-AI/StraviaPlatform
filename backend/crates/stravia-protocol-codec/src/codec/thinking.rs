//! Shared egress-only thinking budget ladders and model-generation rules.
//!
//! Budget token ladders and legacy protocol downgrades belong strictly to
//! egress encoders and vendor plugins, never to the canonical request IR.

use std::borrow::Cow;

use serde_json::Value;
use stravia_runtime_contract::thinking::ThinkingLevel;

/// Minimum legal manual thinking budget for Anthropic / Bedrock Claude models.
pub const MIN_THINKING_BUDGET: u32 = 1024;
/// Buffer reserved above manual thinking budget when raising an explicit
/// `max_tokens` limit or fitting a budget inside an automatic limit.
pub const ANTHROPIC_OUTPUT_BUFFER: u32 = 4000;

/// Ephemeral egress-only metadata key marking that `request.generation.max_tokens`
/// was filled automatically from discovered model metadata rather than supplied
/// explicitly by the client.
pub const ANTHROPIC_AUTO_OUTPUT_LIMIT_EGRESS_KEY: &str = "__stravia_anthropic_auto_output_limit";
/// Ephemeral egress-only metadata key carrying discovered Anthropic `/v1/models`
/// capability metadata (`capabilities` JSON object) into the shared encoder.
pub const ANTHROPIC_CAPABILITIES_EGRESS_KEY: &str = "__stravia_anthropic_capabilities";
/// Ephemeral egress-only metadata key carrying declared `reasoning_efforts`
/// from the Provider Model specification into the shared encoder.
pub const ANTHROPIC_EFFORTS_EGRESS_KEY: &str = "__stravia_anthropic_efforts";

/// Unified static egress token budget ladder (egress layer only).
pub fn static_budget_for_level(level: ThinkingLevel) -> Option<u32> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(1024),
        ThinkingLevel::Low => Some(2048),
        ThinkingLevel::Medium => Some(8192),
        ThinkingLevel::High => Some(16384),
        ThinkingLevel::Xhigh => Some(24576),
        ThinkingLevel::Max => Some(32768),
    }
}

/// Resolve a positive static egress budget from an upstream effort string
/// (falling back to the canonical Thinking Level and finally `MIN_THINKING_BUDGET`).
pub fn static_budget_for_effort(effort: Option<&str>, fallback: Option<ThinkingLevel>) -> u32 {
    effort
        .and_then(|value| ThinkingLevel::from_wire(value).ok())
        .or(fallback)
        .and_then(static_budget_for_level)
        .unwrap_or(MIN_THINKING_BUDGET)
}

/// Raise an explicit client `max_tokens` to keep `ANTHROPIC_OUTPUT_BUFFER`
/// tokens above a manual thinking budget.
pub fn raise_explicit_max_tokens(current: u32, budget: u32) -> u32 {
    current.max(budget.saturating_add(ANTHROPIC_OUTPUT_BUFFER))
}

/// Fit a manual thinking budget below an automatically supplied `max_tokens`
/// cap without raising the cap. Returns `None` when the cap cannot fit even
/// `MIN_THINKING_BUDGET` (`budget < limit` and `budget >= 1024`).
pub fn fit_budget_within_auto_limit(limit: u32, budget: u32) -> Option<u32> {
    if limit <= MIN_THINKING_BUDGET {
        return None;
    }
    Some(
        budget
            .min(limit.saturating_sub(ANTHROPIC_OUTPUT_BUFFER))
            .max(MIN_THINKING_BUDGET)
            .min(limit - 1),
    )
}

// ── Shared Anthropic model thinking profile ──────────────────────────────────

pub type AnthropicRevision = [u8; 3];

/// Anthropic effort levels in ascending order.
pub const ANTHROPIC_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
pub const ANTHROPIC_EFFORTS_WITHOUT_XHIGH: [&str; 4] = ["low", "medium", "high", "max"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnthropicModelFamily {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

const ANTHROPIC_FAMILIES: [(&str, AnthropicModelFamily); 5] = [
    ("opus", AnthropicModelFamily::Opus),
    ("sonnet", AnthropicModelFamily::Sonnet),
    ("haiku", AnthropicModelFamily::Haiku),
    ("fable", AnthropicModelFamily::Fable),
    ("mythos", AnthropicModelFamily::Mythos),
];

/// Thinking-relevant subset of Anthropic `/v1/models` capabilities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnthropicThinkingCapabilities {
    pub thinking: Option<bool>,
    pub adaptive: Option<bool>,
    pub enabled: Option<bool>,
    /// Declared supported effort strings; `Some(empty)` means effort is unsupported.
    pub efforts: Option<Vec<String>>,
}

impl AnthropicThinkingCapabilities {
    pub fn is_empty(&self) -> bool {
        self.thinking.is_none()
            && self.adaptive.is_none()
            && self.enabled.is_none()
            && self.efforts.is_none()
    }

    pub fn from_wire(capabilities: &Value, efforts: Option<Vec<String>>) -> Self {
        let supported = |path: &str| {
            capabilities
                .pointer(&format!("{path}/supported"))
                .and_then(Value::as_bool)
        };
        let efforts = match supported("/effort") {
            Some(false) => Some(Vec::new()),
            Some(true) => efforts.or_else(|| {
                let levels = ANTHROPIC_EFFORTS
                    .into_iter()
                    .filter(|level| supported(&format!("/effort/{level}")) == Some(true))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                (!levels.is_empty()).then_some(levels)
            }),
            None => efforts,
        };
        Self {
            thinking: supported("/thinking"),
            adaptive: supported("/thinking/types/adaptive"),
            enabled: supported("/thinking/types/enabled"),
            efforts,
        }
    }

    /// Combines optional `/v1/models` `capabilities` with catalog/manual `reasoning_efforts`,
    /// suppressing synthesized effort ladders on legacy or wire-declared manual-only models
    /// unless `/effort/supported` is explicitly `true`.
    pub fn from_wire_for_model(
        model: &str,
        raw: Option<&Value>,
        declared_efforts: Option<Vec<String>>,
    ) -> Self {
        let default_caps = Self::default();
        let static_profile = AnthropicModelProfile::new(model, &default_caps);
        let known_without_native_effort =
            static_profile.known().is_some() && static_profile.efforts().next().is_none();
        match raw {
            Some(capabilities) => {
                let supported = |path: &str| {
                    capabilities
                        .pointer(&format!("{path}/supported"))
                        .and_then(Value::as_bool)
                };
                let effort_supported = supported("/effort");
                let adaptive = supported("/thinking/types/adaptive");
                let enabled = supported("/thinking/types/enabled");
                let manual_only_wire =
                    adaptive == Some(false) || (enabled == Some(true) && adaptive != Some(true));
                let fallback_efforts = if effort_supported == Some(true) {
                    declared_efforts
                } else if manual_only_wire {
                    Some(Vec::new())
                } else if known_without_native_effort {
                    None
                } else {
                    declared_efforts
                };
                Self::from_wire(capabilities, fallback_efforts)
            }
            None => Self {
                efforts: if known_without_native_effort {
                    None
                } else {
                    declared_efforts
                },
                ..Self::default()
            },
        }
    }
}

/// Shared Anthropic thinking profile combining discovered capabilities with
/// static Claude generation rules.
#[derive(Clone, Debug)]
pub struct AnthropicModelProfile<'a> {
    known: Option<(AnthropicModelFamily, AnthropicRevision)>,
    caps: &'a AnthropicThinkingCapabilities,
}

impl<'a> AnthropicModelProfile<'a> {
    pub fn new(model: &str, caps: &'a AnthropicThinkingCapabilities) -> Self {
        Self::from_known(parse_known_anthropic_model(model), caps)
    }

    pub fn from_known(
        known: Option<(AnthropicModelFamily, AnthropicRevision)>,
        caps: &'a AnthropicThinkingCapabilities,
    ) -> Self {
        Self { known, caps }
    }

    pub fn known(&self) -> Option<(AnthropicModelFamily, AnthropicRevision)> {
        self.known
    }

    pub fn is_recognized(&self) -> bool {
        self.known.is_some() || !self.caps.is_empty()
    }

    /// Anthropic stopped accepting assistant prefill starting with Claude 4.6.
    pub fn accepts_prefill(&self) -> bool {
        match self.known {
            Some((
                AnthropicModelFamily::Opus
                | AnthropicModelFamily::Sonnet
                | AnthropicModelFamily::Haiku,
                revision,
            )) => revision < [4, 6, 0],
            Some((AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos, _)) => false,
            None => true,
        }
    }

    /// Models that only accept `adaptive` and reject `enabled`.
    pub fn adaptive_only(&self) -> bool {
        if let (Some(adaptive), Some(enabled)) = (self.caps.adaptive, self.caps.enabled) {
            return adaptive && !enabled;
        }
        match self.known {
            Some((AnthropicModelFamily::Opus, revision)) => revision >= [4, 7, 0],
            Some((AnthropicModelFamily::Sonnet, revision)) => revision >= [5, 0, 0],
            Some((AnthropicModelFamily::Haiku, revision)) => revision >= [5, 5, 0],
            Some((AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos, _)) => true,
            None => false,
        }
    }

    /// Whether the model accepts `thinking: { type: "adaptive" }`.
    /// Unknown custom models default to the latest `adaptive` specification
    /// unless discovered capabilities explicitly disable adaptive thinking or
    /// declare manual `enabled` thinking without `adaptive`.
    pub fn supports_adaptive(&self) -> bool {
        if let Some(adaptive) = self.caps.adaptive {
            return adaptive;
        }
        if self.caps.enabled == Some(true) {
            return false;
        }
        match self.known {
            Some((AnthropicModelFamily::Opus | AnthropicModelFamily::Sonnet, revision)) => {
                revision >= [4, 6, 0]
            }
            Some((AnthropicModelFamily::Haiku, revision)) => revision >= [5, 5, 0],
            Some((AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos, _)) => true,
            None => true,
        }
    }

    /// Whether thinking is adaptive by default when omitted.
    pub fn adaptive_by_default(&self) -> bool {
        self.supports_adaptive()
            && match self.known {
                Some((AnthropicModelFamily::Opus | AnthropicModelFamily::Sonnet, revision)) => {
                    revision >= [5, 0, 0]
                }
                Some((AnthropicModelFamily::Haiku, revision)) => revision >= [5, 5, 0],
                Some((AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos, _)) => true,
                None => false,
            }
    }

    /// Always-on thinking models where `thinking: { type: "disabled" }` is 400.
    pub fn always_on(&self) -> bool {
        match self.known {
            Some((AnthropicModelFamily::Opus, revision)) => revision >= [5, 5, 0],
            Some((AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos, _)) => true,
            Some((AnthropicModelFamily::Sonnet | AnthropicModelFamily::Haiku, _)) | None => false,
        }
    }

    /// Sonnet >= 5.5 uses `thinking: { type: "between_tools" }` instead of `disabled`.
    pub fn between_tools(&self) -> bool {
        matches!(
            self.known,
            Some((AnthropicModelFamily::Sonnet, revision)) if revision >= [5, 5, 0]
        )
    }

    /// Opus 5 and Haiku >= 5.5 only accept `disabled` when effort <= high.
    pub fn disabled_requires_effort_cap(&self) -> bool {
        matches!(
            self.known,
            Some((AnthropicModelFamily::Opus, revision)) if ([5, 0, 0]..[5, 5, 0]).contains(&revision)
        ) || matches!(
            self.known,
            Some((AnthropicModelFamily::Haiku, revision)) if revision >= [5, 5, 0]
        )
    }

    /// Models that allow forced `tool_choice` (`any`/`tool`).
    pub fn forced_tool_use(&self) -> bool {
        !match self.known {
            Some((AnthropicModelFamily::Opus | AnthropicModelFamily::Sonnet, revision)) => {
                revision >= [5, 5, 0]
            }
            Some((AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos, revision)) => {
                revision >= [5, 1, 0]
            }
            Some((AnthropicModelFamily::Haiku, _)) | None => false,
        }
    }

    /// Supported native effort strings when known or discovered.
    pub fn efforts(&self) -> impl Iterator<Item = &str> {
        let ladder: &[&str] = if self.caps.efforts.is_some() {
            &[]
        } else {
            match self.known {
                None => &[],
                Some((AnthropicModelFamily::Haiku, revision)) if revision < [5, 5, 0] => &[],
                Some((AnthropicModelFamily::Sonnet, revision)) if revision < [4, 6, 0] => &[],
                Some((AnthropicModelFamily::Opus, revision)) if revision < [4, 5, 0] => &[],
                Some((AnthropicModelFamily::Opus, revision)) if revision < [4, 6, 0] => {
                    &ANTHROPIC_EFFORTS[..3]
                }
                Some((AnthropicModelFamily::Opus, revision)) if revision < [4, 7, 0] => {
                    &ANTHROPIC_EFFORTS_WITHOUT_XHIGH
                }
                Some((AnthropicModelFamily::Sonnet, revision)) if revision < [5, 0, 0] => {
                    &ANTHROPIC_EFFORTS_WITHOUT_XHIGH
                }
                Some(_) => &ANTHROPIC_EFFORTS,
            }
        };
        ladder
            .iter()
            .copied()
            .chain(self.caps.efforts.iter().flatten().map(String::as_str))
    }

    /// Canonical `reasoning_efforts` list published during discovery for this
    /// model, including `"none"` for models that can turn thinking off and the
    /// full canonical ladder for legacy manual-budget models.
    pub fn discovery_efforts(&self) -> Vec<String> {
        if self.caps.thinking == Some(false) {
            return Vec::new();
        }
        let native_efforts: Vec<&str> = self.efforts().collect();
        let allow_off = !self.always_on() && !self.between_tools();
        let mut efforts = Vec::new();
        if !native_efforts.is_empty() {
            if allow_off
                && !native_efforts
                    .iter()
                    .any(|effort| stravia_runtime_contract::thinking::effort_is_off(effort))
            {
                efforts.push("none".to_owned());
            }
            efforts.extend(native_efforts.into_iter().map(str::to_owned));
        } else if self.caps.thinking == Some(true) || self.caps.enabled == Some(true) {
            if allow_off {
                efforts.push("none".to_owned());
            }
            efforts.extend(
                ["minimal", "low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .map(str::to_owned),
            );
        }
        efforts
    }

    /// Whether the profile has a constrained effort list (either known model
    /// or discovered capabilities).
    pub fn has_known_effort_constraints(&self) -> bool {
        self.known.is_some() || self.caps.efforts.is_some()
    }

    /// Pick the highest supported effort not exceeding `requested`, or the lowest
    /// supported effort if all are higher. Returns `None` when the model supports
    /// no effort parameter.
    pub fn clamp_effort(&self, requested: &str) -> Option<&str> {
        let rank = anthropic_effort_rank(requested);
        let mut lower: Option<(usize, &str)> = None;
        let mut lowest: Option<(usize, &str)> = None;
        for candidate in self.efforts() {
            if candidate == requested {
                return Some(candidate);
            }
            let (Some(rank), Some(value)) = (rank, anthropic_effort_rank(candidate)) else {
                continue;
            };
            if lowest.is_none_or(|(current, _)| value < current) {
                lowest = Some((value, candidate));
            }
            if value <= rank && lower.is_none_or(|(current, _)| value > current) {
                lower = Some((value, candidate));
            }
        }
        lower.or(lowest).map(|(_, candidate)| candidate)
    }

    /// Normalize a requested effort string against this profile's constraints.
    /// For unknown custom models without discovered effort constraints,
    /// preserves the requested value verbatim (mapping canonical `"minimal"` to `"low"`).
    pub fn resolve_effort<'b>(&'b self, requested: &'b str) -> Option<&'b str> {
        if self.has_known_effort_constraints() {
            let normalized = if self
                .caps
                .efforts
                .as_ref()
                .is_some_and(|efforts| efforts.iter().any(|candidate| candidate == requested))
            {
                requested
            } else {
                parse_anthropic_effort(requested)?
            };
            self.clamp_effort(normalized)
        } else if requested.eq_ignore_ascii_case("minimal") {
            Some("low")
        } else {
            Some(requested)
        }
    }
}

/// Parse known Claude family and revision from an Anthropic, Bedrock, or Vertex model ID.
pub fn parse_known_anthropic_model(
    model: &str,
) -> Option<(AnthropicModelFamily, AnthropicRevision)> {
    let model = if model.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(model.to_ascii_lowercase())
    } else {
        Cow::Borrowed(model)
    };
    let (name, family) = ANTHROPIC_FAMILIES
        .into_iter()
        .find(|(name, _)| model.contains(name))?;
    let index = model.find(name)?;
    let tail = &model[index + name.len()..];
    let tail = tail.strip_prefix(['-', '.']).unwrap_or(tail);
    match (family, revision_prefix(tail)) {
        (AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos, None) => None,
        (family, revision) => Some((family, revision.unwrap_or([0; 3]))),
    }
}

pub fn supports_anthropic_message_threads(model: &str) -> bool {
    model == "claude-mythos-preview"
        || parse_known_anthropic_model(model).is_some_and(|(_, revision)| revision >= [4, 5, 0])
}

pub fn anthropic_effort_rank(effort: &str) -> Option<usize> {
    ANTHROPIC_EFFORTS
        .iter()
        .position(|candidate| *candidate == effort)
}

/// Normalize a wire effort string to the Anthropic effort vocabulary.
/// `"minimal"` maps to `"low"`; `"none"`/`"off"`/`"adaptive"` return `None`.
pub fn parse_anthropic_effort(value: &str) -> Option<&'static str> {
    let value = if value == "minimal" { "low" } else { value };
    ANTHROPIC_EFFORTS
        .into_iter()
        .find(|effort| *effort == value)
}

pub fn cap_anthropic_effort_at_high(effort: &'static str) -> &'static str {
    if anthropic_effort_rank(effort) > anthropic_effort_rank("high") {
        "high"
    } else {
        effort
    }
}

fn revision_prefix(tail: &str) -> Option<AnthropicRevision> {
    let bytes = tail.as_bytes();
    let mut revision = [0; 3];
    let mut index = 0;
    for (count, slot) in revision.iter_mut().enumerate() {
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let component = if bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
            None
        } else {
            tail[start..index].parse::<u8>().ok()
        };
        let Some(component) = component else {
            return (count > 0).then_some(revision);
        };
        *slot = component;
        let continues = matches!(bytes.get(index), Some(b'.' | b'-'))
            && bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
        if !continues {
            break;
        }
        index += 1;
    }
    Some(revision)
}

// ── Shared Google Gemini model thinking classifier ───────────────────────────

/// Egress thinking family for Google Gemini models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeminiThinkingModel {
    /// Gemini 3+ and unknown custom Gemini models use `thinkingLevel`.
    Level { can_disable: bool },
    /// Gemini 2.5 family uses `thinkingBudget` with family-specific caps and
    /// safe off-downgrade floor (`0` for Flash/Flash-Lite, `128` for Pro).
    Budget { max_budget: u32, off_budget: u32 },
}

pub fn classify_gemini_thinking_model(model: &str) -> GeminiThinkingModel {
    let lower = model.trim().to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(lower.as_str());

    if name.contains("gemini-2.5") || name.contains("gemini-2-5") {
        if has_token(name, "pro") && !has_token(name, "flash") {
            return GeminiThinkingModel::Budget {
                max_budget: 32_768,
                off_budget: 128,
            };
        }
        return GeminiThinkingModel::Budget {
            max_budget: 24_576,
            off_budget: 0,
        };
    }

    let is_gemini_3_pro = (name.contains("gemini-3-") || name.contains("gemini-3."))
        && has_token(name, "pro")
        && !has_token(name, "flash");
    GeminiThinkingModel::Level {
        can_disable: !is_gemini_3_pro,
    }
}

fn has_token(model: &str, token: &str) -> bool {
    model
        .split(['-', '.', '_', ':'])
        .any(|segment| segment == token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_budget_ladder_matches_egress_specification() {
        assert_eq!(static_budget_for_level(ThinkingLevel::Off), None);
        assert_eq!(static_budget_for_level(ThinkingLevel::Minimal), Some(1024));
        assert_eq!(static_budget_for_level(ThinkingLevel::Low), Some(2048));
        assert_eq!(static_budget_for_level(ThinkingLevel::Medium), Some(8192));
        assert_eq!(static_budget_for_level(ThinkingLevel::High), Some(16384));
        assert_eq!(static_budget_for_level(ThinkingLevel::Xhigh), Some(24576));
        assert_eq!(static_budget_for_level(ThinkingLevel::Max), Some(32768));
    }

    #[test]
    fn gemini_classifier_distinguishes_2_5_budget_and_3_plus_level_families() {
        assert_eq!(
            classify_gemini_thinking_model("gemini-2.5-pro"),
            GeminiThinkingModel::Budget {
                max_budget: 32_768,
                off_budget: 128
            }
        );
        assert_eq!(
            classify_gemini_thinking_model("models/gemini-2.5-pro-preview-06-05"),
            GeminiThinkingModel::Budget {
                max_budget: 32_768,
                off_budget: 128
            }
        );
        assert_eq!(
            classify_gemini_thinking_model("gemini-2.5-flash"),
            GeminiThinkingModel::Budget {
                max_budget: 24_576,
                off_budget: 0
            }
        );
        assert_eq!(
            classify_gemini_thinking_model("gemini-2.5-flash-lite-preview"),
            GeminiThinkingModel::Budget {
                max_budget: 24_576,
                off_budget: 0
            }
        );
        assert_eq!(
            classify_gemini_thinking_model("gemini-3-pro"),
            GeminiThinkingModel::Level { can_disable: false }
        );
        assert_eq!(
            classify_gemini_thinking_model("gemini-3.1-pro-preview"),
            GeminiThinkingModel::Level { can_disable: false }
        );
        assert_eq!(
            classify_gemini_thinking_model("gemini-3-flash"),
            GeminiThinkingModel::Level { can_disable: true }
        );
        assert_eq!(
            classify_gemini_thinking_model("custom-gemini-model"),
            GeminiThinkingModel::Level { can_disable: true }
        );
    }

    #[test]
    fn anthropic_profile_handles_api_bedrock_and_unknown_models() {
        let empty = AnthropicThinkingCapabilities::default();
        assert!(!AnthropicModelProfile::new("claude-sonnet-4-5", &empty).supports_adaptive());
        assert!(
            !AnthropicModelProfile::new("anthropic.claude-3-7-sonnet-20250219-v1:0", &empty)
                .supports_adaptive()
        );
        assert!(
            !AnthropicModelProfile::new("claude-haiku-4-5-20251001", &empty).supports_adaptive()
        );
        assert!(AnthropicModelProfile::new("claude-sonnet-4-6", &empty).supports_adaptive());
        assert!(
            AnthropicModelProfile::new("us.anthropic.claude-opus-4-6-v1", &empty)
                .supports_adaptive()
        );
        assert!(AnthropicModelProfile::new("claude-haiku-5-5", &empty).supports_adaptive());
        assert!(AnthropicModelProfile::new("custom-anthropic-proxy", &empty).supports_adaptive());
        assert!(AnthropicModelProfile::new("claude-opus-5-5", &empty).always_on());
        assert!(AnthropicModelProfile::new("claude-fable-5-2", &empty).always_on());
        assert!(AnthropicModelProfile::new("claude-sonnet-5-5", &empty).between_tools());
    }
}
