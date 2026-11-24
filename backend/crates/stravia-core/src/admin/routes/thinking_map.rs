use super::*;

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
                    submitted
                        .iter()
                        .find(|row| row.level == level)
                        .cloned()
                        .map(|mut row| {
                            row.source = ThinkingMappingSource::Overridden;
                            row
                        })
                        .or_else(|| generated.iter().find(|row| row.level == level).cloned())
                        .expect("generated map contains every Thinking Level")
                })
                .collect();
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
        let edited = targets
            .iter_mut()
            .find(|candidate| {
                candidate.provider_id == target.provider_id().as_str()
                    && candidate.model == target.model().as_str()
            })
            .expect("target was loaded from this Route");
        for row in &mut edited.thinking_level_map {
            if only_level.is_none_or(|level| row.level == level) {
                *row = generated
                    .iter()
                    .find(|generated_row| generated_row.level == row.level)
                    .cloned()
                    .expect("generated map contains every Thinking Level");
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
