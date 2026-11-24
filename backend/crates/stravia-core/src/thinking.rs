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

/// Host-side authorization for a resolved Target Thinking Control.
/// When the Provider Model declares `reasoning_efforts`, it acts as a strict
/// allowlist for `Effort` values; otherwise any non-empty `Effort` is writable
/// if the target protocol natively represents `Effort`. `Hidden` is always writable.
pub(crate) fn control_is_writable(
    protocol: &str,
    metadata: &ProviderModelMetadata,
    control: &TargetThinkingControl,
) -> bool {
    match control {
        TargetThinkingControl::Hidden => true,
        TargetThinkingControl::Effort { value } => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return false;
            }
            match metadata.reasoning_efforts.as_deref() {
                Some(efforts) => efforts
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(trimmed)),
                None => {
                    stravia_runtime_contract::protocol::ids::Protocol::from_identifier(protocol)
                        .is_some_and(|protocol| {
                            protocol.represents_target_thinking_control(control)
                        })
                }
            }
        }
    }
}

/// 派发时模型声明是 Effort 白名单；缺少声明才回退到协议基线。
/// protocol 为 None 时仅执行选协议之前的模型能力过滤。
pub(crate) fn resolve_runtime_thinking_level<'a>(
    mappings: &'a [ThinkingLevelMapping],
    requested: ThinkingLevel,
    metadata: &ProviderModelMetadata,
    protocol: Option<&str>,
) -> Option<(ThinkingLevel, &'a TargetThinkingControl)> {
    let mut supported = ThinkingLevel::ALL;
    let mut count = 0;
    for level in ThinkingLevel::ALL {
        let Some(control) = mapping_control(mappings, level) else {
            continue;
        };
        let available = match control {
            TargetThinkingControl::Hidden => false,
            TargetThinkingControl::Effort { value } => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    false
                } else {
                    match metadata.reasoning_efforts.as_deref() {
                        Some(efforts) => efforts
                            .iter()
                            .any(|effort| effort.eq_ignore_ascii_case(trimmed)),
                        None => protocol.is_none_or(|protocol| {
                            stravia_runtime_contract::protocol::ids::Protocol::from_identifier(
                                protocol,
                            )
                            .is_some_and(|protocol| {
                                protocol.represents_target_thinking_control(control)
                            })
                        }),
                    }
                }
            }
        };
        if available {
            supported[count] = level;
            count += 1;
        }
    }
    let level = requested.clamp(&supported[..count])?;
    mapping_control(mappings, level).map(|control| (level, control))
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
    fn control_writability_enforces_declared_efforts_and_protocol_baseline() {
        let empty = ProviderModelMetadata::default();
        assert!(control_is_writable(
            "anthropic-messages",
            &empty,
            &TargetThinkingControl::Effort {
                value: "high".into()
            }
        ));
        assert!(!control_is_writable(
            "anthropic-messages",
            &empty,
            &TargetThinkingControl::Effort { value: " ".into() }
        ));
        assert!(!control_is_writable(
            "acme/private",
            &empty,
            &TargetThinkingControl::Effort {
                value: "high".into()
            }
        ));
        assert!(control_is_writable(
            "acme/private",
            &empty,
            &TargetThinkingControl::Hidden
        ));

        let strict = ProviderModelMetadata {
            reasoning_efforts: Some(vec!["none".into(), "high".into()]),
            ..Default::default()
        };
        assert!(control_is_writable(
            "acme/private",
            &strict,
            &TargetThinkingControl::Effort {
                value: "high".into()
            }
        ));
        assert!(!control_is_writable(
            "openai-compatible",
            &strict,
            &TargetThinkingControl::Effort {
                value: "medium".into()
            }
        ));
    }
}
