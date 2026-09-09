use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::provider_models::{ProviderModelMetadata, ReasoningOption};

use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMappingSource {
    Generated,
    Overridden,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingLevelMapping {
    pub level: ThinkingLevel,
    pub control: TargetThinkingControl,
    pub source: ThinkingMappingSource,
}

impl ThinkingLevelMapping {
    fn generated(level: ThinkingLevel, control: TargetThinkingControl) -> Self {
        Self {
            level,
            control,
            source: ThinkingMappingSource::Generated,
        }
    }
}

pub fn generate_thinking_level_map(metadata: &ProviderModelMetadata) -> Vec<ThinkingLevelMapping> {
    let options = metadata.reasoning_options.as_deref().unwrap_or_default();
    if let Some(values) = options.iter().find_map(|option| match option {
        ReasoningOption::Effort { values } => Some(values),
        _ => None,
    }) {
        return effort_map(values);
    }
    if let Some((min, max)) = options.iter().find_map(|option| match option {
        ReasoningOption::BudgetTokens { min, max } => Some((*min, *max)),
        _ => None,
    }) {
        return budget_map(min, max);
    }
    if options
        .iter()
        .any(|option| matches!(option, ReasoningOption::Toggle))
    {
        return ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                let control = match level {
                    ThinkingLevel::Off => TargetThinkingControl::Disabled,
                    ThinkingLevel::Medium => TargetThinkingControl::Enabled,
                    _ => TargetThinkingControl::Hidden,
                };
                ThinkingLevelMapping::generated(level, control)
            })
            .collect();
    }
    default_map()
}

fn effort_map(values: &[Option<String>]) -> Vec<ThinkingLevelMapping> {
    let controls = values
        .iter()
        .filter_map(|value| value.as_deref())
        .filter(|value| *value != "default")
        .filter_map(|value| {
            ThinkingLevel::from_wire(value)
                .ok()
                .map(|level| (level, value.to_string()))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    ThinkingLevel::ALL
        .into_iter()
        .map(|level| {
            let control = controls
                .get(&level)
                .cloned()
                .map(|value| TargetThinkingControl::Effort { value })
                .unwrap_or(TargetThinkingControl::Hidden);
            ThinkingLevelMapping::generated(level, control)
        })
        .collect()
}

fn budget_map(min: Option<i64>, max: Option<u64>) -> Vec<ThinkingLevelMapping> {
    let min = min.filter(|value| *value >= 0).unwrap_or(0) as u64;
    let max = max.unwrap_or(u32::MAX as u64).min(u32::MAX as u64).max(min);
    let mut seen = BTreeSet::new();
    let mut rows = Vec::with_capacity(ThinkingLevel::ALL.len());
    for level in ThinkingLevel::ALL {
        let control = match level {
            ThinkingLevel::Off => TargetThinkingControl::Disabled,
            ThinkingLevel::Minimal
            | ThinkingLevel::Low
            | ThinkingLevel::Medium
            | ThinkingLevel::High => {
                let default = match level {
                    ThinkingLevel::Minimal => 1024_u64,
                    ThinkingLevel::Low => 2048,
                    ThinkingLevel::Medium => 8192,
                    ThinkingLevel::High => 16384,
                    _ => unreachable!(),
                };
                let value = default.clamp(min, max) as u32;
                if seen.insert(value) {
                    TargetThinkingControl::Budget { value }
                } else {
                    TargetThinkingControl::Hidden
                }
            }
            ThinkingLevel::Xhigh | ThinkingLevel::Max => TargetThinkingControl::Hidden,
        };
        rows.push(ThinkingLevelMapping::generated(level, control));
    }
    rows
}

fn default_map() -> Vec<ThinkingLevelMapping> {
    ThinkingLevel::ALL
        .into_iter()
        .map(|level| {
            let control = match level {
                ThinkingLevel::Off => TargetThinkingControl::Effort {
                    value: "none".into(),
                },
                ThinkingLevel::Minimal
                | ThinkingLevel::Low
                | ThinkingLevel::Medium
                | ThinkingLevel::High => TargetThinkingControl::Effort {
                    value: level.as_str().into(),
                },
                ThinkingLevel::Xhigh | ThinkingLevel::Max => TargetThinkingControl::Hidden,
            };
            ThinkingLevelMapping::generated(level, control)
        })
        .collect()
}

pub fn mapping_control(
    mappings: &[ThinkingLevelMapping],
    level: ThinkingLevel,
) -> Option<&TargetThinkingControl> {
    mappings
        .iter()
        .find(|mapping| mapping.level == level)
        .map(|mapping| &mapping.control)
}

pub fn visible_levels(mappings: &[ThinkingLevelMapping]) -> Vec<ThinkingLevel> {
    ThinkingLevel::ALL
        .into_iter()
        .filter(|level| {
            mapping_control(mappings, *level).is_some_and(|control| !control.is_hidden())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(reasoning_options: serde_json::Value) -> ProviderModelMetadata {
        serde_json::from_value(serde_json::json!({
            "id": "test-model",
            "reasoning_options": reasoning_options,
        }))
        .expect("Provider Model metadata")
    }

    #[test]
    fn budget_ties_snap_to_the_higher_pi_rung() {
        assert_eq!(ThinkingLevel::from_budget(1536), ThinkingLevel::Low);
        assert_eq!(ThinkingLevel::from_budget(0), ThinkingLevel::Off);
        assert_eq!(ThinkingLevel::from_budget(30_000), ThinkingLevel::High);
    }

    #[test]
    fn clamp_searches_higher_before_lower() {
        assert_eq!(
            ThinkingLevel::Medium.clamp(&[ThinkingLevel::Low, ThinkingLevel::High]),
            Some(ThinkingLevel::High)
        );
        assert_eq!(ThinkingLevel::Medium.clamp(&[]), None);
    }

    #[test]
    fn effort_generation_has_priority_and_keeps_hidden_rows() {
        let map = generate_thinking_level_map(&metadata(serde_json::json!([
            {"type": "toggle"},
            {"type": "budget_tokens", "min": 2048, "max": 8192},
            {"type": "effort", "values": [null, "default", "none", "low", "high", "max"]}
        ])));
        assert_eq!(map.len(), 7);
        assert_eq!(
            mapping_control(&map, ThinkingLevel::Off),
            Some(&TargetThinkingControl::Effort {
                value: "none".into()
            })
        );
        assert_eq!(
            mapping_control(&map, ThinkingLevel::Minimal),
            Some(&TargetThinkingControl::Hidden)
        );
        assert_eq!(
            visible_levels(&map),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::High,
                ThinkingLevel::Max
            ]
        );
    }

    #[test]
    fn budget_generation_clamps_and_hides_duplicate_rows() {
        let map = generate_thinking_level_map(&metadata(serde_json::json!([
            {"type": "budget_tokens", "min": 4096, "max": 10000}
        ])));
        assert_eq!(
            mapping_control(&map, ThinkingLevel::Minimal),
            Some(&TargetThinkingControl::Budget { value: 4096 })
        );
        assert_eq!(
            mapping_control(&map, ThinkingLevel::Low),
            Some(&TargetThinkingControl::Hidden)
        );
        assert_eq!(
            mapping_control(&map, ThinkingLevel::Medium),
            Some(&TargetThinkingControl::Budget { value: 8192 })
        );
        assert_eq!(
            mapping_control(&map, ThinkingLevel::High),
            Some(&TargetThinkingControl::Budget { value: 10000 })
        );
    }

    #[test]
    fn toggle_and_empty_generation_follow_pi_defaults() {
        let toggle =
            generate_thinking_level_map(&metadata(serde_json::json!([{"type": "toggle"}])));
        assert_eq!(
            visible_levels(&toggle),
            vec![ThinkingLevel::Off, ThinkingLevel::Medium]
        );

        let default = generate_thinking_level_map(&metadata(serde_json::json!([])));
        assert_eq!(
            visible_levels(&default),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High
            ]
        );
    }
}
