use super::*;
use std::collections::BTreeMap;

#[derive(PartialEq, Eq, Clone)]
struct EffectiveTargetProfile {
    first_token_timeout_ms: i64,
    target_retry_budget: i32,
    target_cooldown_ms: i64,
    controls: Vec<(
        ThinkingLevel,
        Option<stravia_runtime_contract::thinking::TargetThinkingControl>,
    )>,
}

fn effective_create_target_profile(target: &CreateTarget) -> EffectiveTargetProfile {
    EffectiveTargetProfile {
        first_token_timeout_ms: target
            .first_token_timeout_ms
            .unwrap_or(DEFAULT_FIRST_TOKEN_TIMEOUT_MS),
        target_retry_budget: target
            .target_retry_budget
            .unwrap_or(DEFAULT_TARGET_RETRY_BUDGET),
        target_cooldown_ms: target
            .target_cooldown_ms
            .unwrap_or(DEFAULT_TARGET_COOLDOWN_MS),
        controls: ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                let control = target
                    .thinking_level_map
                    .iter()
                    .find(|row| row.level == level)
                    .map(|row| row.control.clone());
                (level, control)
            })
            .collect(),
    }
}

fn effective_saved_target_profile(target: &TargetConfig) -> EffectiveTargetProfile {
    EffectiveTargetProfile {
        first_token_timeout_ms: target.first_token_timeout_ms,
        target_retry_budget: target.target_retry_budget,
        target_cooldown_ms: target.target_cooldown_ms,
        controls: ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                let control = target
                    .thinking_level_map
                    .iter()
                    .find(|row| row.level == level)
                    .map(|row| row.control.clone());
                (level, control)
            })
            .collect(),
    }
}

impl RouteModule<'_> {
    pub(super) async fn prepare_thinking_maps(
        &self,
        existing: &[TargetConfig],
        proposed: &mut [CreateTarget],
    ) -> anyhow::Result<()> {
        for target in proposed {
            let model = target.model.clone();
            let provider_model = self
                .gw
                .storage
                .provider_models()
                .find(target.provider_id.trim(), model.trim())
                .await?
                .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
            let provider = self.get_provider(target.provider_id.trim()).await?;
            let current = existing.iter().find(|current| {
                current.provider_id().as_str() == target.provider_id.trim()
                    && current.model().as_str() == model.as_str()
            });
            if let Some(current) = current {
                if target.thinking_level_map.is_empty() {
                    if current.thinking_level_map.is_empty() {
                        target.thinking_level_map =
                            generate_thinking_level_map(&provider_model.metadata);
                    } else {
                        target.thinking_level_map = current.thinking_level_map.clone();
                    }
                } else {
                    let submitted = std::mem::take(&mut target.thinking_level_map);
                    target.thinking_level_map = ThinkingLevel::ALL
                        .into_iter()
                        .map(|level| {
                            if let Some(mut row) =
                                submitted.iter().find(|row| row.level == level).cloned()
                            {
                                let unchanged = current
                                    .thinking_level_map
                                    .iter()
                                    .find(|old| old.level == level)
                                    .is_some_and(|old| old.control == row.control);
                                row.source = if unchanged {
                                    current
                                        .thinking_level_map
                                        .iter()
                                        .find(|old| old.level == level)
                                        .map(|old| old.source)
                                        .unwrap_or(ThinkingMappingSource::Overridden)
                                } else {
                                    ThinkingMappingSource::Overridden
                                };
                                row
                            } else {
                                current
                                    .thinking_level_map
                                    .iter()
                                    .find(|old| old.level == level)
                                    .cloned()
                                    .unwrap_or(crate::thinking::ThinkingLevelMapping {
                                        level,
                                        control: stravia_runtime_contract::thinking::TargetThinkingControl::Hidden,
                                        source: ThinkingMappingSource::Generated,
                                    })
                            }
                        })
                        .collect();
                }
                hide_unwritable_generated_controls(
                    &provider,
                    &provider_model.metadata,
                    &mut target.thinking_level_map,
                );
                continue;
            }

            let mut generated = generate_thinking_level_map(&provider_model.metadata);
            hide_unwritable_generated_controls(&provider, &provider_model.metadata, &mut generated);
            let submitted = std::mem::take(&mut target.thinking_level_map);
            target.thinking_level_map = ThinkingLevel::ALL
                .into_iter()
                .map(|level| {
                    let generated_row = generated
                        .iter()
                        .find(|row| row.level == level)
                        .cloned()
                        .expect("generated map contains every Thinking Level");
                    submitted
                        .iter()
                        .find(|row| row.level == level)
                        .cloned()
                        .map(|mut row| {
                            if !(row.source == ThinkingMappingSource::Generated
                                && row.control == generated_row.control)
                            {
                                row.source = ThinkingMappingSource::Overridden;
                            }
                            row
                        })
                        .unwrap_or(generated_row)
                })
                .collect();
        }
        Ok(())
    }

    /// 校验并解析同层同模型组内每个 Target 的 `config_mode`。
    /// 显式声明为 `shared` 的同组成员必须拥有一致的超时、重试预算、冷却与思考映射控制；
    /// 省略 `config_mode` 的旧调用方按存量与组内一致性安全推导，避免静默覆写或破坏兼容。
    pub(super) fn resolve_target_config_modes(
        &self,
        existing: &[TargetConfig],
        proposed: &mut [CreateTarget],
    ) -> anyhow::Result<()> {
        let mut groups: BTreeMap<(bool, i32, String), Vec<usize>> = BTreeMap::new();
        for (index, target) in proposed.iter().enumerate() {
            let key = (
                target.enabled,
                target.priority.unwrap_or(DEFAULT_TARGET_PRIORITY),
                target.model.trim().to_owned(),
            );
            groups.entry(key).or_default().push(index);
        }

        for ((enabled, priority, model), indices) in groups {
            let profiles: Vec<EffectiveTargetProfile> = indices
                .iter()
                .map(|&idx| effective_create_target_profile(&proposed[idx]))
                .collect();

            let explicit_shared: Vec<usize> = indices
                .iter()
                .enumerate()
                .filter_map(|(pos, _)| {
                    (proposed[indices[pos]].config_mode == Some(TargetConfigMode::Shared))
                        .then_some(pos)
                })
                .collect();

            if explicit_shared.len() > 1 {
                let anchor = &profiles[explicit_shared[0]];
                if explicit_shared.iter().any(|&pos| &profiles[pos] != anchor) {
                    return Err(coded_error(
                        "TARGET_GROUP_CONFIG_CONFLICT",
                        "Shared targets in the same model group must have identical timeout, retry budget, cooldown, and thinking controls",
                        serde_json::json!({
                            "model": model,
                            "enabled": enabled,
                            "priority": priority,
                        }),
                    ));
                }
            }

            let baseline = if let Some(&first_shared) = explicit_shared.first() {
                Some(profiles[first_shared].clone())
            } else {
                // 仅在全部省略 config_mode 且存在多数一致配置（或全组一致）时作为共享基线。
                let mut counts: Vec<(usize, usize)> = Vec::new();
                for pos in 0..profiles.len() {
                    if proposed[indices[pos]].config_mode == Some(TargetConfigMode::Custom) {
                        continue;
                    }
                    if let Some((_, count)) = counts
                        .iter_mut()
                        .find(|(rep, _)| profiles[*rep] == profiles[pos])
                    {
                        *count += 1;
                    } else {
                        counts.push((pos, 1));
                    }
                }
                if counts.len() == 1 {
                    Some(profiles[counts[0].0].clone())
                } else {
                    counts
                        .into_iter()
                        .filter(|(_, count)| *count > 1)
                        .max_by_key(|(_, count)| *count)
                        .map(|(rep, _)| profiles[rep].clone())
                }
            };

            for (pos, &idx) in indices.iter().enumerate() {
                if proposed[idx].config_mode.is_some() {
                    continue;
                }
                let previous = existing.iter().find(|current| {
                    current.provider_id().as_str() == proposed[idx].provider_id.trim()
                        && current.model().as_str() == proposed[idx].model.trim()
                });
                let resolved = if let Some(prev) = previous {
                    let unchanged = effective_saved_target_profile(prev) == profiles[pos];
                    if unchanged && prev.config_mode == TargetConfigMode::Custom {
                        TargetConfigMode::Custom
                    } else if baseline.as_ref().is_some_and(|base| base == &profiles[pos]) {
                        TargetConfigMode::Shared
                    } else {
                        TargetConfigMode::Custom
                    }
                } else if baseline.as_ref().is_some_and(|base| base == &profiles[pos]) {
                    TargetConfigMode::Shared
                } else {
                    TargetConfigMode::Custom
                };
                proposed[idx].config_mode = Some(resolved);
            }
        }
        Ok(())
    }

    pub(super) async fn ensure_thinking_controls_representable(
        &self,
        targets: &[CreateTarget],
    ) -> anyhow::Result<()> {
        for target in targets {
            let model = target.model.as_str();
            let provider = self.get_provider(target.provider_id.trim()).await?;
            let provider_model = self
                .gw
                .storage
                .provider_models()
                .find(target.provider_id.trim(), model.trim())
                .await?
                .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
            ensure_thinking_map_representable(
                &provider,
                model,
                &provider_model.metadata,
                &target.thinking_level_map,
            )?;
        }
        Ok(())
    }

    pub(crate) async fn reset_thinking_mapping(
        &self,
        route_id: &str,
        target_id: &str,
        level: ThinkingLevel,
    ) -> anyhow::Result<RouteConfig> {
        self.replace_generated_thinking_rows(route_id, target_id, Some(level))
            .await
    }

    pub(crate) async fn regenerate_thinking_map(
        &self,
        route_id: &str,
        target_id: &str,
    ) -> anyhow::Result<RouteConfig> {
        self.replace_generated_thinking_rows(route_id, target_id, None)
            .await
    }

    pub(crate) async fn reset_group_thinking_mapping(
        &self,
        route_id: &str,
        input: TargetGroupThinkingResetInput,
    ) -> anyhow::Result<RouteConfig> {
        self.replace_group_generated_thinking_rows(
            route_id,
            input.enabled,
            input.priority,
            &input.model,
            Some(input.level),
            input.include_custom,
        )
        .await
    }

    pub(crate) async fn regenerate_group_thinking_map(
        &self,
        route_id: &str,
        input: TargetGroupThinkingRegenerateInput,
    ) -> anyhow::Result<RouteConfig> {
        self.replace_group_generated_thinking_rows(
            route_id,
            input.enabled,
            input.priority,
            &input.model,
            None,
            input.include_custom,
        )
        .await
    }

    pub(super) async fn replace_generated_thinking_rows(
        &self,
        route_id: &str,
        target_id: &str,
        only_level: Option<ThinkingLevel>,
    ) -> anyhow::Result<RouteConfig> {
        let route = self.get(route_id).await?;
        let target = route
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| anyhow::anyhow!("Target not found: {target_id}"))?;
        let model = target.model().as_str();
        let provider_model = self
            .gw
            .storage
            .provider_models()
            .find(target.provider_id().as_str(), model)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
        let provider = self.get_provider(target.provider_id().as_str()).await?;
        let mut generated = generate_thinking_level_map(&provider_model.metadata);
        hide_unwritable_generated_controls(&provider, &provider_model.metadata, &mut generated);
        let mut targets = route_targets_for_update(&route);
        let edited_index = targets
            .iter()
            .position(|candidate| {
                candidate.provider_id == target.provider_id().as_str()
                    && candidate.model == target.model().as_str()
            })
            .expect("target was loaded from this Route");
        for row in &mut targets[edited_index].thinking_level_map {
            if only_level.is_none_or(|level| row.level == level) {
                *row = generated
                    .iter()
                    .find(|generated_row| generated_row.level == row.level)
                    .cloned()
                    .expect("generated map contains every Thinking Level");
            }
        }
        // 单目标重置/重新生成若导致其配置偏离同组其他共享成员，自动转为独立配置，保持共享组不变量。
        if targets[edited_index].config_mode == Some(TargetConfigMode::Shared) {
            let edited_profile = effective_create_target_profile(&targets[edited_index]);
            let group_key = (
                targets[edited_index].enabled,
                targets[edited_index]
                    .priority
                    .unwrap_or(DEFAULT_TARGET_PRIORITY),
                targets[edited_index].model.trim().to_owned(),
            );
            let conflicts_with_shared_peer = targets.iter().enumerate().any(|(idx, candidate)| {
                idx != edited_index
                    && candidate.config_mode == Some(TargetConfigMode::Shared)
                    && (
                        candidate.enabled,
                        candidate.priority.unwrap_or(DEFAULT_TARGET_PRIORITY),
                        candidate.model.trim(),
                    ) == (group_key.0, group_key.1, group_key.2.as_str())
                    && effective_create_target_profile(candidate) != edited_profile
            });
            if conflicts_with_shared_peer {
                targets[edited_index].config_mode = Some(TargetConfigMode::Custom);
            }
        }
        self.ensure_thinking_controls_representable(&targets)
            .await?;
        self.change_record(
            route_id,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await
    }

    async fn replace_group_generated_thinking_rows(
        &self,
        route_id: &str,
        enabled: bool,
        priority: i32,
        model: &str,
        only_level: Option<ThinkingLevel>,
        include_custom: bool,
    ) -> anyhow::Result<RouteConfig> {
        let _guard = self.gw.route_media_mutation.lock().await;
        let route = self.get(route_id).await?;
        let model = normalize_model_id(model)?;
        let mut targets = route_targets_for_update(&route);

        let matched_indices: Vec<usize> = route
            .targets
            .iter()
            .enumerate()
            .filter_map(|(idx, target)| {
                let in_group = target.enabled == enabled
                    && target.priority == priority
                    && target.model().as_str() == model;
                let selected =
                    include_custom || target.config_mode == TargetConfigMode::Shared;
                (in_group && selected).then_some(idx)
            })
            .collect();
        if matched_indices.is_empty() {
            return Err(coded_error(
                "TARGET_GROUP_NOT_FOUND",
                "Target group not found",
                serde_json::json!({
                    "route_id": route_id,
                    "enabled": enabled,
                    "priority": priority,
                    "model": model,
                }),
            ));
        }

        let mut member_specs = Vec::with_capacity(matched_indices.len());
        for &idx in &matched_indices {
            let target = &route.targets[idx];
            let provider = self.get_provider(target.provider_id().as_str()).await?;
            let provider_model = self
                .gw
                .storage
                .provider_models()
                .find(target.provider_id().as_str(), target.model().as_str())
                .await?
                .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
            let mut generated = generate_thinking_level_map(&provider_model.metadata);
            hide_unwritable_generated_controls(
                &provider,
                &provider_model.metadata,
                &mut generated,
            );
            member_specs.push((idx, provider, provider_model.metadata, generated));
        }

        for level in ThinkingLevel::ALL {
            if !only_level.is_none_or(|target_level| target_level == level) {
                continue;
            }
            // 选择所有受影响成员都能共同写入的生成控制；若任一成员无法写入该档位，回退为 Hidden，
            // 保证组内所有共享成员结果一致且不会在保存时触发不可表达错误。
            let chosen_control = member_specs
                .iter()
                .filter_map(|(_, _, _, generated)| {
                    generated
                        .iter()
                        .find(|row| row.level == level)
                        .map(|row| &row.control)
                })
                .find(|control| {
                    !matches!(
                        control,
                        stravia_runtime_contract::thinking::TargetThinkingControl::Hidden
                    ) && member_specs.iter().all(|(_, provider, metadata, _)| {
                        thinking_control_writable(provider, metadata, control)
                    })
                })
                .cloned()
                .unwrap_or(stravia_runtime_contract::thinking::TargetThinkingControl::Hidden);

            for (idx, _, _, generated) in &member_specs {
                let own_generated = generated
                    .iter()
                    .find(|row| row.level == level)
                    .expect("generated map contains every Thinking Level");
                let source = if own_generated.control == chosen_control {
                    ThinkingMappingSource::Generated
                } else {
                    ThinkingMappingSource::Overridden
                };
                if let Some(row) = targets[*idx]
                    .thinking_level_map
                    .iter_mut()
                    .find(|row| row.level == level)
                {
                    row.control = chosen_control.clone();
                    row.source = source;
                }
            }
        }

        self.ensure_thinking_controls_representable(&targets)
            .await?;
        self.resolve_target_config_modes(&route.targets, &mut targets)?;
        self.change_record(
            route_id,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await
    }
}

pub(super) fn ensure_thinking_map_representable(
    provider: &crate::db::models::Provider,
    model_id: &str,
    metadata: &crate::provider_models::ProviderModelMetadata,
    map: &[crate::thinking::ThinkingLevelMapping],
) -> anyhow::Result<()> {
    let mut levels = Vec::new();
    let mut controls = Vec::new();
    for row in map {
        if thinking_control_writable(provider, metadata, &row.control) {
            continue;
        }
        levels.push(row.level.as_str());
        let kind = row.control.kind();
        if !controls.contains(&kind) {
            controls.push(kind);
        }
    }
    if levels.is_empty() {
        return Ok(());
    }
    Err(coded_error(
        "THINKING_CONTROL_UNREPRESENTABLE",
        "Target protocol cannot write this Target Thinking Control",
        serde_json::json!({
            "provider_id": provider.id,
            "model_id": model_id,
            "level": levels[0],
            "levels": levels,
            "control": controls[0],
            "controls": controls,
            "supported_controls": writable_thinking_control_kinds(
                provider,
                metadata,
            ),
            "protocol": provider.protocol,
        }),
    ))
}

/// Generated rows never block a Route: when the catalog declares a Thinking
/// Control the Provider cannot write, the level falls back to Hidden instead of
/// guessing a wire shape. Rows the user explicitly submits stay fail-closed in
/// `ensure_thinking_controls_representable`.
pub(super) fn hide_unwritable_generated_controls(
    provider: &crate::db::models::Provider,
    metadata: &crate::provider_models::ProviderModelMetadata,
    map: &mut [crate::thinking::ThinkingLevelMapping],
) {
    for row in map.iter_mut() {
        if row.source == ThinkingMappingSource::Generated
            && !thinking_control_writable(provider, metadata, &row.control)
        {
            row.control = stravia_runtime_contract::thinking::TargetThinkingControl::Hidden;
        }
    }
}

fn thinking_control_writable(
    provider: &crate::db::models::Provider,
    metadata: &crate::provider_models::ProviderModelMetadata,
    control: &stravia_runtime_contract::thinking::TargetThinkingControl,
) -> bool {
    crate::thinking::control_is_writable(&provider.protocol, metadata, control)
}

fn writable_thinking_control_kinds(
    provider: &crate::db::models::Provider,
    metadata: &crate::provider_models::ProviderModelMetadata,
) -> Vec<&'static str> {
    use stravia_runtime_contract::thinking::TargetThinkingControl;

    let mut kinds = Vec::with_capacity(2);
    let effort_writable = match metadata.reasoning_efforts.as_deref() {
        Some(efforts) => efforts.iter().any(|value| !value.trim().is_empty()),
        None => {
            stravia_runtime_contract::protocol::ids::Protocol::from_identifier(&provider.protocol)
                .is_some_and(|protocol| {
                    protocol.represents_target_thinking_control(&TargetThinkingControl::Effort {
                        value: "high".into(),
                    })
                })
        }
    };
    if effort_writable {
        kinds.push("effort");
    }
    kinds.push("hidden");
    kinds
}
