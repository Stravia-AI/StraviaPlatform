use super::*;

impl RouteModule<'_> {
    pub(super) async fn prepare_thinking_maps(
        &self,
        existing: &[TargetConfig],
        proposed: &mut [CreateTarget],
    ) -> anyhow::Result<()> {
        for target in proposed {
            let Some(model) = target.model.clone() else {
                anyhow::ensure!(
                    target.thinking_level_map.is_empty(),
                    "Provider-only Targets cannot define a Thinking Level Map"
                );
                continue;
            };
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
                    && current.model().map(|model| model.as_str()) == Some(model.as_str())
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
                    self.gw,
                    &provider,
                    &provider_model.metadata,
                    &mut target.thinking_level_map,
                );
                continue;
            }

            let mut generated = generate_thinking_level_map(&provider_model.metadata);
            hide_unwritable_generated_controls(
                self.gw,
                &provider,
                &provider_model.metadata,
                &mut generated,
            );
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
            let Some(model) = target.model.as_deref() else {
                anyhow::ensure!(
                    target.thinking_level_map.is_empty(),
                    "Provider-only Targets cannot define a Thinking Level Map"
                );
                continue;
            };
            let provider = self.get_provider(target.provider_id.trim()).await?;
            let provider_model = self
                .gw
                .storage
                .provider_models()
                .find(target.provider_id.trim(), model.trim())
                .await?
                .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
            ensure_thinking_map_representable(
                self.gw,
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
        let model = target
            .model()
            .map(|model| model.as_str())
            .ok_or_else(|| anyhow::anyhow!("Provider-only Target has no Thinking Level Map"))?;
        let provider_model = self
            .gw
            .storage
            .provider_models()
            .find(target.provider_id().as_str(), model)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
        let provider = self.get_provider(target.provider_id().as_str()).await?;
        let mut generated = generate_thinking_level_map(&provider_model.metadata);
        hide_unwritable_generated_controls(
            self.gw,
            &provider,
            &provider_model.metadata,
            &mut generated,
        );
        let mut targets = route_targets_for_update(&route);
        let edited = targets
            .iter_mut()
            .find(|candidate| {
                candidate.provider_id == target.provider_id().as_str()
                    && candidate.model.as_deref() == target.model().map(|model| model.as_str())
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
    gw: &Gateway,
    provider: &crate::db::models::Provider,
    model_id: &str,
    metadata: &crate::provider_models::ProviderModelMetadata,
    map: &[crate::thinking::ThinkingLevelMapping],
) -> anyhow::Result<()> {
    let toggle_declared = thinking_toggle_declared(gw, provider, metadata);
    let mut levels = Vec::new();
    let mut controls = Vec::new();
    for row in map {
        if thinking_control_writable(provider, metadata, toggle_declared, &row.control) {
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
                toggle_declared,
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
    gw: &Gateway,
    provider: &crate::db::models::Provider,
    metadata: &crate::provider_models::ProviderModelMetadata,
    map: &mut [crate::thinking::ThinkingLevelMapping],
) {
    let toggle_declared = thinking_toggle_declared(gw, provider, metadata);
    for row in map.iter_mut() {
        if row.source == ThinkingMappingSource::Generated
            && !thinking_control_writable(provider, metadata, toggle_declared, &row.control)
        {
            row.control = stravia_runtime_contract::thinking::TargetThinkingControl::Hidden;
        }
    }
}

fn thinking_control_writable(
    provider: &crate::db::models::Provider,
    metadata: &crate::provider_models::ProviderModelMetadata,
    toggle_declared: bool,
    control: &stravia_runtime_contract::thinking::TargetThinkingControl,
) -> bool {
    crate::thinking::control_is_writable(&provider.protocol, metadata, toggle_declared, control)
}

fn thinking_toggle_declared(
    gw: &Gateway,
    provider: &crate::db::models::Provider,
    metadata: &crate::provider_models::ProviderModelMetadata,
) -> bool {
    let model_declares = metadata
        .extensions
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|capabilities| {
            capabilities.iter().any(|capability| {
                capability.as_str() == Some(stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE)
            })
        });

    let Some(vendor_id) = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let Some(channel_id) = provider
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    gw.vendor_plugins
        .descriptor(vendor_id)
        .ok()
        .and_then(|descriptor| {
            descriptor
                .channels
                .into_iter()
                .find(|channel| channel.id == channel_id)
        })
        .is_some_and(|channel| {
            model_declares
                || channel
                    .model_capabilities
                    .contains(stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE)
        })
}

fn writable_thinking_control_kinds(
    provider: &crate::db::models::Provider,
    metadata: &crate::provider_models::ProviderModelMetadata,
    toggle_declared: bool,
) -> Vec<&'static str> {
    use stravia_runtime_contract::thinking::TargetThinkingControl;

    const KINDS: [(&str, TargetThinkingControl); 5] = [
        (
            "effort",
            TargetThinkingControl::Effort {
                value: String::new(),
            },
        ),
        ("budget", TargetThinkingControl::Budget { value: 0 }),
        ("enabled", TargetThinkingControl::Enabled),
        ("disabled", TargetThinkingControl::Disabled),
        ("hidden", TargetThinkingControl::Hidden),
    ];
    KINDS
        .into_iter()
        .filter_map(|(kind, control)| {
            thinking_control_writable(provider, metadata, toggle_declared, &control).then_some(kind)
        })
        .collect()
}
