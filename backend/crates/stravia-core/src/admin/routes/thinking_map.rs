use super::*;

impl RouteModule<'_> {
    pub(super) async fn prepare_thinking_maps(
        &self,
        existing: &[Target],
        proposed: &mut [CreateTarget],
    ) -> anyhow::Result<()> {
        for target in proposed {
            let current = existing.iter().find(|current| {
                current.provider_id == target.provider_id.trim()
                    && current.model == target.model.trim()
            });
            if let Some(current) = current {
                if target.thinking_level_map.is_empty() {
                    if current.thinking_level_map.is_empty() {
                        let provider_model = self
                            .admin
                            .gw
                            .storage
                            .provider_models()
                            .get(target.provider_id.trim(), target.model.trim())
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
                        target.thinking_level_map =
                            generate_thinking_level_map(&provider_model.metadata);
                    } else {
                        target.thinking_level_map = current.thinking_level_map.0.clone();
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
                                    .unwrap_or_else(|| crate::thinking::ThinkingLevelMapping {
                                        level,
                                        control: crate::thinking::TargetThinkingControl::Hidden,
                                        source: ThinkingMappingSource::Generated,
                                    })
                            }
                        })
                        .collect();
                }
                continue;
            }

            let provider_model = self
                .admin
                .gw
                .storage
                .provider_models()
                .get(target.provider_id.trim(), target.model.trim())
                .await?
                .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
            let generated = generate_thinking_level_map(&provider_model.metadata);
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
            let provider = self.admin.get_provider(target.provider_id.trim()).await?;
            for row in &target.thinking_level_map {
                let registry = crate::protocol::registry::ProtocolRegistry::global();
                let representable = registry
                    .protocol_represents_target_thinking_control(&provider.protocol, &row.control)
                    || registry
                        .parse_protocol(&provider.protocol)
                        .is_some_and(|protocol| {
                            protocol == crate::protocol::ids::Protocol::OpenAICompatible
                            && matches!(
                                row.control,
                                crate::thinking::TargetThinkingControl::Enabled
                                    | crate::thinking::TargetThinkingControl::Disabled
                            )
                            && crate::provider::common::openai_compatible_thinking::supports_toggle(
                                &provider,
                                &target.model,
                            )
                        });
                if !representable {
                    return Err(coded_error(
                        "THINKING_CONTROL_UNREPRESENTABLE",
                        "Target protocol cannot write this Target Thinking Control",
                        serde_json::json!({
                            "provider_id": target.provider_id,
                            "model_id": target.model,
                            "level": row.level.as_str(),
                            "protocol": provider.protocol,
                        }),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn reset_thinking_mapping(
        &self,
        route_id: &str,
        target_id: &str,
        level: ThinkingLevel,
    ) -> anyhow::Result<Route> {
        self.replace_generated_thinking_rows(route_id, target_id, Some(level))
            .await
    }

    pub(crate) async fn regenerate_thinking_map(
        &self,
        route_id: &str,
        target_id: &str,
    ) -> anyhow::Result<Route> {
        self.replace_generated_thinking_rows(route_id, target_id, None)
            .await
    }

    pub(super) async fn replace_generated_thinking_rows(
        &self,
        route_id: &str,
        target_id: &str,
        only_level: Option<ThinkingLevel>,
    ) -> anyhow::Result<Route> {
        let route = self.get(route_id).await?;
        let target = route
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| anyhow::anyhow!("Target not found: {target_id}"))?;
        let provider_model = self
            .admin
            .gw
            .storage
            .provider_models()
            .get(&target.provider_id, &target.model)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Provider Model not found"))?;
        let generated = generate_thinking_level_map(&provider_model.metadata);
        let mut targets = route_targets_for_update(&route);
        let edited = targets
            .iter_mut()
            .find(|candidate| candidate.id.as_deref() == Some(target_id))
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
        let prepared = targets
            .iter()
            .map(create_backend_from_upsert)
            .collect::<Vec<_>>();
        self.ensure_thinking_controls_representable(&prepared)
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

    pub(crate) async fn refresh_generated_thinking_maps(
        &self,
        provider_id: &str,
        provider_model_id: &str,
        metadata: &crate::provider_models::ProviderModelMetadata,
        apply: bool,
    ) -> anyhow::Result<()> {
        let generated = generate_thinking_level_map(metadata);
        let mut changes = Vec::new();
        for route in self.admin.list_models().await? {
            if !route.targets.iter().any(|target| {
                target.provider_id == provider_id && target.model == provider_model_id
            }) {
                continue;
            }
            let mut targets = route_targets_for_update(&route);
            for target in targets.iter_mut().filter(|target| {
                target.provider_id == provider_id && target.model == provider_model_id
            }) {
                for row in &mut target.thinking_level_map {
                    if row.source == ThinkingMappingSource::Generated {
                        *row = generated
                            .iter()
                            .find(|generated_row| generated_row.level == row.level)
                            .cloned()
                            .expect("generated map contains every Thinking Level");
                    }
                }
            }
            let prepared = targets
                .iter()
                .map(create_backend_from_upsert)
                .collect::<Vec<_>>();
            self.ensure_thinking_controls_representable(&prepared)
                .await?;
            changes.push((route.name, targets));
        }
        if apply {
            for (route_id, targets) in changes {
                self.change_record(
                    &route_id,
                    UpdateRoute {
                        targets: Some(targets),
                        ..UpdateRoute::default()
                    },
                )
                .await?;
            }
        }
        Ok(())
    }
}
