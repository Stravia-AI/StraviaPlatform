use serde::{Deserialize, Serialize};

use crate::provider_models::ProviderModelMetadata;

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

pub(crate) fn refresh_generated_thinking_level_map(
    current: &mut [ThinkingLevelMapping],
    generated: &[ThinkingLevelMapping],
) -> anyhow::Result<bool> {
    let mut changed = false;
    for row in current {
        if row.source != ThinkingMappingSource::Generated {
            continue;
        }
        let replacement = generated
            .iter()
            .find(|candidate| candidate.level == row.level)
            .ok_or_else(|| anyhow::anyhow!("generated Thinking Level Map is incomplete"))?;
        anyhow::ensure!(
            replacement.source == ThinkingMappingSource::Generated,
            "replacement Thinking Level Mapping must be generated"
        );
        if row != replacement {
            row.clone_from(replacement);
            changed = true;
        }
    }
    Ok(changed)
}

pub fn generate_thinking_level_map(metadata: &ProviderModelMetadata) -> Vec<ThinkingLevelMapping> {
    effort_map(metadata.reasoning_efforts.as_deref().unwrap_or_default())
}

fn effort_map(values: &[String]) -> Vec<ThinkingLevelMapping> {
    let mut supported = [false; ThinkingLevel::ALL.len()];
    for value in values {
        let index = match value.as_str() {
            "none" => 0,
            "minimal" => 1,
            "low" => 2,
            "medium" => 3,
            "high" => 4,
            "xhigh" => 5,
            "max" => 6,
            _ => continue,
        };
        supported[index] = true;
    }
    ThinkingLevel::ALL
        .into_iter()
        .zip(supported)
        .map(|(level, supported)| {
            let control = if supported {
                TargetThinkingControl::Effort {
                    value: if level == ThinkingLevel::Off {
                        "none"
                    } else {
                        level.as_str()
                    }
                    .to_owned(),
                }
            } else {
                TargetThinkingControl::Hidden
            };
            ThinkingLevelMapping::generated(level, control)
        })
        .collect()
}

/// Host-side authorization for a resolved Target Thinking Control. Standard
/// protocol semantics and explicit model/plugin metadata are both valid
/// evidence; the host does not require a proprietary guest codec to be present.
pub(crate) fn control_is_writable(
    protocol: &str,
    metadata: &ProviderModelMetadata,
    toggle_declared: bool,
    control: &TargetThinkingControl,
) -> bool {
    if control.is_hidden() {
        return true;
    }
    stravia_runtime_contract::protocol::ids::Protocol::from_identifier(protocol)
        .is_some_and(|protocol| protocol.represents_target_thinking_control(control))
        || model_declares_control(metadata, toggle_declared, control)
}

fn model_declares_control(
    metadata: &ProviderModelMetadata,
    toggle_declared: bool,
    control: &TargetThinkingControl,
) -> bool {
    match control {
        TargetThinkingControl::Hidden => true,
        TargetThinkingControl::Enabled | TargetThinkingControl::Disabled => toggle_declared,
        TargetThinkingControl::Effort { value } => metadata
            .reasoning_efforts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(value)),
        TargetThinkingControl::Budget { .. } => false,
    }
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

    #[test]
    fn generated_maps_use_only_known_explicit_efforts() {
        let metadata = ProviderModelMetadata {
            reasoning_efforts: Some(vec!["none".into(), "high".into(), "custom".into()]),
            ..Default::default()
        };
        let map = generate_thinking_level_map(&metadata);
        assert_eq!(
            visible_levels(&map),
            vec![ThinkingLevel::Off, ThinkingLevel::High]
        );
        assert_eq!(
            mapping_control(&map, ThinkingLevel::High),
            Some(&TargetThinkingControl::Effort {
                value: "high".into()
            })
        );
        assert!(
            visible_levels(&generate_thinking_level_map(
                &ProviderModelMetadata::default()
            ))
            .is_empty()
        );
    }

    #[test]
    fn explicit_target_protocol_controls_remain_writable() {
        let metadata = ProviderModelMetadata::default();
        assert!(control_is_writable(
            "anthropic",
            &metadata,
            true,
            &TargetThinkingControl::Enabled
        ));
        assert!(control_is_writable(
            "acme/private",
            &metadata,
            true,
            &TargetThinkingControl::Disabled
        ));
        assert!(!control_is_writable(
            "acme/private",
            &metadata,
            false,
            &TargetThinkingControl::Budget { value: 4096 }
        ));
    }
}
