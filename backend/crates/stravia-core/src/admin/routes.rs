use super::*;
use crate::provider_models::{
    CreateManualProviderModel, NewProviderModelRecord, ProviderModelDetail, ProviderModelMutation,
    ProviderModelPresence, ProviderModelSelectionPolicy, ProviderModelSourceKind,
    ProviderModelSyncSummary, normalize_model_id,
};
use crate::thinking::{ThinkingLevel, ThinkingMappingSource, generate_thinking_level_map};

mod model_records;
mod provider_model_records;
use provider_model_records::PreparedProviderModel;

#[derive(Debug, Clone, Deserialize)]
pub struct BindRouteInput {
    pub route_id: Option<String>,
    pub provider_id: String,
    pub provider_model_id: String,
    pub weight: Option<i32>,
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnbindRouteInput {
    pub route_id: String,
    pub provider_id: String,
    pub provider_model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteBind {
    OneClick {
        provider_id: String,
        provider_model_id: String,
    },
    At {
        route_id: String,
        provider_id: String,
        provider_model_id: String,
        weight: i32,
        priority: i32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteUnbind {
    pub route_id: String,
    pub provider_id: String,
    pub provider_model_id: String,
}

pub(crate) struct RouteModule<'a> {
    admin: &'a AdminService,
}

impl<'a> RouteModule<'a> {
    pub(crate) fn new(admin: &'a AdminService) -> Self {
        Self { admin }
    }

    pub(crate) async fn add_provider_model(
        &self,
        provider_id: &str,
        provider_model_id: &str,
        input: CreateManualProviderModel,
    ) -> anyhow::Result<ProviderModelDetail> {
        self.admin
            .create_manual_provider_model_record(provider_id, provider_model_id, input)
            .await
    }

    pub(crate) async fn prepare_provider_model(
        &self,
        provider_id: &str,
        provider_model_id: &str,
        canonical_model_id: Option<&str>,
    ) -> anyhow::Result<PreparedProviderModel> {
        self.admin
            .prepare_provider_model_record(provider_id, provider_model_id, canonical_model_id)
            .await
    }

    pub(crate) async fn sync(&self, provider_id: &str) -> anyhow::Result<ProviderModelSyncSummary> {
        self.admin.sync_provider_models_record(provider_id).await
    }

    pub(crate) async fn discover_provider_model_ids(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        let provider = self.admin.get_provider(provider_id).await?;
        if uses_catalog_inventory(&provider) {
            return self
                .admin
                .preset_catalog_models_for_provider(&provider)
                .await?
                .map(|catalog| catalog.models.into_iter().map(|model| model.id).collect())
                .ok_or_else(|| anyhow::anyhow!("Catalog Provider identity is missing"));
        }
        let runtime = self.admin.resolve_provider_runtime(&provider).await?;
        let credential = runtime.access_token.clone();
        if let Some(static_list) = runtime.binding.static_models_override.as_deref() {
            let models = static_list
                .iter()
                .map(|model| model.trim().to_string())
                .filter(|model| !model.is_empty())
                .collect::<Vec<_>>();
            if !models.is_empty() {
                return Ok(models);
            }
        }
        let preset_static_models = preset_static_models(&provider);
        if !preset_static_models.is_empty() {
            return Ok(preset_static_models);
        }
        let endpoint = runtime
            .binding
            .models_source_override
            .clone()
            .or_else(|| resolve_models_endpoint(&provider))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Model Discovery URL is empty"))?;

        let mut headers = if runtime.binding.disable_default_auth {
            HeaderMap::new()
        } else {
            build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?
        };
        headers.extend(runtime_binding_headers(&runtime.binding)?);
        let mut request = self
            .admin
            .gw
            .http_client
            .get(&endpoint)
            .headers(headers)
            .timeout(Duration::from_secs(10));
        if provider.protocol == "gemini" && !runtime.binding.disable_default_auth {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?;
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            request = self
                .admin
                .gw
                .http_client
                .get(format!("{endpoint}{separator}key={credential}"))
                .headers(headers)
                .timeout(Duration::from_secs(10));
        }

        let response = request
            .send()
            .await
            .map_err(|error| anyhow::anyhow!(format_connectivity_error(&error)))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            let preview = body.chars().take(200).collect::<String>();
            anyhow::bail!("HTTP {status}: {preview}");
        }
        let json: Value = response.json().await.unwrap_or_default();
        let models =
            extract_models_from_response(&provider.protocol, provider.vendor.as_deref(), &json);
        if models.is_empty() {
            anyhow::bail!("Model list format is invalid or empty");
        }
        Ok(models)
    }

    pub(crate) async fn create(&self, mut input: CreateModel) -> anyhow::Result<Model> {
        let mut targets = normalize_create_model_backends(&input)?;
        self.ensure_new_targets_available(&[], &targets).await?;
        self.prepare_thinking_maps(&[], &mut targets).await?;
        self.ensure_thinking_controls_representable(&targets)
            .await?;
        input.targets = targets;
        self.admin.create_model_record(input).await
    }

    pub(crate) async fn change(
        &self,
        route_storage_id: &str,
        mut input: UpdateModel,
    ) -> anyhow::Result<Model> {
        let current = self
            .admin
            .list_models()
            .await?
            .into_iter()
            .find(|route| route.id == route_storage_id)
            .ok_or_else(|| anyhow::anyhow!("Route not found: {route_storage_id}"))?;
        let mut targets = normalize_update_model_backends(&current, &input)?;
        self.ensure_new_targets_available(&current.targets, &targets)
            .await?;
        self.prepare_thinking_maps(&current.targets, &mut targets)
            .await?;
        self.ensure_thinking_controls_representable(&targets)
            .await?;
        input.targets = Some(
            targets
                .iter()
                .map(|target| UpsertModelBackend {
                    id: None,
                    provider_id: target.provider_id.clone(),
                    model: target.model.clone(),
                    weight: target.weight,
                    priority: target.priority,
                    thinking_level_map: target.thinking_level_map.clone(),
                })
                .collect(),
        );
        self.admin
            .update_model_record(route_storage_id, input)
            .await
    }

    async fn prepare_thinking_maps(
        &self,
        existing: &[ModelBackend],
        proposed: &mut [CreateModelBackend],
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

    async fn ensure_thinking_controls_representable(
        &self,
        targets: &[CreateModelBackend],
    ) -> anyhow::Result<()> {
        for target in targets {
            let provider = self.admin.get_provider(target.provider_id.trim()).await?;
            for row in &target.thinking_level_map {
                let representable = match provider.protocol.as_str() {
                    "open-responses" => match &row.control {
                        crate::thinking::TargetThinkingControl::Effort { .. } => true,
                        crate::thinking::TargetThinkingControl::Disabled
                        | crate::thinking::TargetThinkingControl::Hidden => true,
                        _ => false,
                    },
                    "anthropic" | "anthropic-messages" | "anthropic-msgs" => {
                        !matches!(
                            row.control,
                            crate::thinking::TargetThinkingControl::Effort { .. }
                        ) || row.source == ThinkingMappingSource::Overridden
                    }
                    "gemini" | "google-gemini" | "google-genai" => true,
                    _ => matches!(
                        row.control,
                        crate::thinking::TargetThinkingControl::Effort { .. }
                            | crate::thinking::TargetThinkingControl::Disabled
                            | crate::thinking::TargetThinkingControl::Hidden
                    ),
                };
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
        route_storage_id: &str,
        target_id: &str,
        level: ThinkingLevel,
    ) -> anyhow::Result<Model> {
        self.replace_generated_thinking_rows(route_storage_id, target_id, Some(level))
            .await
    }

    pub(crate) async fn regenerate_thinking_map(
        &self,
        route_storage_id: &str,
        target_id: &str,
    ) -> anyhow::Result<Model> {
        self.replace_generated_thinking_rows(route_storage_id, target_id, None)
            .await
    }

    async fn replace_generated_thinking_rows(
        &self,
        route_storage_id: &str,
        target_id: &str,
        only_level: Option<ThinkingLevel>,
    ) -> anyhow::Result<Model> {
        let route = self
            .admin
            .list_models()
            .await?
            .into_iter()
            .find(|route| route.id == route_storage_id)
            .ok_or_else(|| anyhow::anyhow!("Route not found: {route_storage_id}"))?;
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
        self.admin
            .update_model_record(
                route_storage_id,
                UpdateModel {
                    targets: Some(targets),
                    ..UpdateModel::default()
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
            changes.push((route.id, targets));
        }
        if apply {
            for (route_id, targets) in changes {
                self.admin
                    .update_model_record(
                        &route_id,
                        UpdateModel {
                            targets: Some(targets),
                            ..UpdateModel::default()
                        },
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn ensure_new_targets_available(
        &self,
        existing: &[ModelBackend],
        proposed: &[CreateModelBackend],
    ) -> anyhow::Result<()> {
        for target in proposed {
            let provider_id = target.provider_id.trim();
            let provider_model_id = normalize_model_id(&target.model)?;
            if existing.iter().any(|current| {
                current.provider_id == provider_id && current.model == provider_model_id
            }) {
                continue;
            }
            let Some(provider_model) = self
                .admin
                .gw
                .storage
                .provider_models()
                .get(provider_id, &provider_model_id)
                .await?
            else {
                return Err(coded_error(
                    "PROVIDER_MODEL_NOT_FOUND",
                    "Provider Model not found",
                    serde_json::json!({
                        "provider_id": provider_id,
                        "model_id": provider_model_id,
                    }),
                ));
            };
            if !provider_model.effective_available() {
                return Err(coded_error(
                    "PROVIDER_MODEL_UNAVAILABLE",
                    "Provider Model is not available for a new Target",
                    serde_json::json!({
                        "provider_id": provider_id,
                        "model_id": provider_model_id,
                    }),
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn copy_provider_targets(
        &self,
        original_provider_id: &str,
        copied_provider_id: &str,
    ) -> anyhow::Result<()> {
        let routes = self.admin.list_models().await?;
        for route in routes.into_iter().filter(|route| {
            route
                .targets
                .iter()
                .any(|target| target.provider_id == original_provider_id)
        }) {
            let copied_targets = route
                .targets
                .iter()
                .filter(|target| target.provider_id == original_provider_id)
                .cloned()
                .collect::<Vec<_>>();

            for target in &copied_targets {
                self.copy_provider_model(original_provider_id, copied_provider_id, &target.model)
                    .await?;
            }

            let mut targets = route
                .targets
                .iter()
                .map(|target| UpsertModelBackend {
                    id: Some(target.id.clone()),
                    provider_id: target.provider_id.clone(),
                    model: target.model.clone(),
                    weight: Some(target.weight),
                    priority: Some(target.priority),
                    thinking_level_map: target.thinking_level_map.0.clone(),
                })
                .collect::<Vec<_>>();
            targets.extend(copied_targets.into_iter().map(|target| UpsertModelBackend {
                id: None,
                provider_id: copied_provider_id.to_string(),
                model: target.model,
                weight: Some(target.weight),
                priority: Some(target.priority),
                thinking_level_map: Vec::new(),
            }));

            self.change(
                &route.id,
                UpdateModel {
                    targets: Some(targets),
                    ..UpdateModel::default()
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn copy_provider_model(
        &self,
        original_provider_id: &str,
        copied_provider_id: &str,
        provider_model_id: &str,
    ) -> anyhow::Result<()> {
        let store = self.admin.gw.storage.provider_models();
        if store
            .get(copied_provider_id, provider_model_id)
            .await?
            .is_some()
        {
            return Ok(());
        }

        let input = match store.get(original_provider_id, provider_model_id).await? {
            Some(original) => NewProviderModelRecord {
                provider_id: copied_provider_id.to_string(),
                model_id: original.model_id,
                source_kind: original.source_kind,
                metadata_source_provider_id: original.metadata_source_provider_id,
                presence: original.presence,
                selection_policy: original.selection_policy,
                metadata: original.metadata,
            },
            None => NewProviderModelRecord {
                provider_id: copied_provider_id.to_string(),
                model_id: provider_model_id.to_string(),
                source_kind: ProviderModelSourceKind::Manual,
                metadata_source_provider_id: None,
                presence: ProviderModelPresence::Present,
                selection_policy: ProviderModelSelectionPolicy::Auto,
                metadata: serde_json::from_value(serde_json::json!({
                    "id": provider_model_id,
                    "name": provider_model_id,
                }))?,
            },
        };

        match store.create(input).await? {
            ProviderModelMutation::Applied(_) | ProviderModelMutation::Conflict => Ok(()),
            ProviderModelMutation::NotFound => {
                anyhow::bail!("copied Provider Model unexpectedly disappeared")
            }
        }
    }

    pub(crate) async fn delete(&self, route_storage_id: &str) -> anyhow::Result<()> {
        self.admin.delete_model_record(route_storage_id).await
    }

    pub(crate) async fn bind(&self, input: RouteBind) -> anyhow::Result<Model> {
        let (route_id, provider_id, provider_model_id, weight, priority) = match input {
            RouteBind::OneClick {
                provider_id,
                provider_model_id,
            } => {
                let route_id = provider_model_id.clone();
                (route_id, provider_id, provider_model_id, 100, 1)
            }
            RouteBind::At {
                route_id,
                provider_id,
                provider_model_id,
                weight,
                priority,
            } => (
                route_id,
                provider_id,
                provider_model_id,
                weight.max(0),
                priority.max(1),
            ),
        };
        let route_id = normalize_name(&route_id, "model ID sent by clients")?;
        let provider_model_id = normalize_model_id(&provider_model_id)?;
        let existing = self
            .admin
            .list_models()
            .await?
            .into_iter()
            .find(|route| route.name.eq_ignore_ascii_case(&route_id));
        if let Some(existing) = existing.as_ref()
            && existing.targets.iter().any(|target| {
                target.provider_id == provider_id && target.model == provider_model_id
            })
        {
            return Ok(existing.clone());
        }

        self.admin.get_provider(&provider_id).await?;
        let provider_model = self
            .admin
            .gw
            .storage
            .provider_models()
            .get(&provider_id, &provider_model_id)
            .await?
            .ok_or_else(|| {
                coded_error(
                    "PROVIDER_MODEL_NOT_FOUND",
                    "Provider Model not found",
                    serde_json::json!({
                        "provider_id": provider_id,
                        "model_id": provider_model_id,
                    }),
                )
            })?;
        if !provider_model.effective_available() {
            return Err(coded_error(
                "PROVIDER_MODEL_UNAVAILABLE",
                "Provider Model is not available for a new Target",
                serde_json::json!({
                    "provider_id": provider_id,
                    "model_id": provider_model_id,
                }),
            ));
        }

        let Some(existing) = existing else {
            return self
                .create(CreateModel {
                    name: route_id,
                    balance: Some("weighted".into()),
                    target_provider: provider_id.clone(),
                    target_model: provider_model_id.clone(),
                    targets: vec![CreateModelBackend {
                        provider_id,
                        model: provider_model_id,
                        weight: Some(weight),
                        priority: Some(priority),
                        thinking_level_map: Vec::new(),
                    }],
                })
                .await;
        };
        if existing
            .targets
            .iter()
            .any(|target| target.provider_id == provider_id && target.model == provider_model_id)
        {
            return Ok(existing);
        }

        let mut targets = existing
            .targets
            .iter()
            .map(|target| UpsertModelBackend {
                id: Some(target.id.clone()),
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                weight: Some(target.weight),
                priority: Some(target.priority),
                thinking_level_map: target.thinking_level_map.0.clone(),
            })
            .collect::<Vec<_>>();
        targets.push(UpsertModelBackend {
            id: None,
            provider_id,
            model: provider_model_id,
            weight: Some(weight),
            priority: Some(priority),
            thinking_level_map: Vec::new(),
        });
        self.change(
            &existing.id,
            UpdateModel {
                targets: Some(targets),
                ..UpdateModel::default()
            },
        )
        .await
    }

    pub(crate) async fn unbind(&self, input: RouteUnbind) -> anyhow::Result<Option<Model>> {
        let route_id = normalize_name(&input.route_id, "model ID sent by clients")?;
        let provider_model_id = normalize_model_id(&input.provider_model_id)?;
        let route = self
            .admin
            .list_models()
            .await?
            .into_iter()
            .find(|route| route.name.eq_ignore_ascii_case(&route_id))
            .ok_or_else(|| anyhow::anyhow!("Route not found: {route_id}"))?;
        let targets = route
            .targets
            .iter()
            .filter(|target| {
                target.provider_id != input.provider_id || target.model != provider_model_id
            })
            .map(|target| UpsertModelBackend {
                id: Some(target.id.clone()),
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                weight: Some(target.weight),
                priority: Some(target.priority),
                thinking_level_map: target.thinking_level_map.0.clone(),
            })
            .collect::<Vec<_>>();
        if targets.len() == route.targets.len() {
            return Ok(Some(route));
        }
        if targets.is_empty() {
            self.delete(&route.id).await?;
            return Ok(None);
        }
        self.change(
            &route.id,
            UpdateModel {
                targets: Some(targets),
                ..UpdateModel::default()
            },
        )
        .await
        .map(Some)
    }
}

impl AdminService {
    pub async fn bind_route(&self, input: BindRouteInput) -> anyhow::Result<Model> {
        let bind = match input.route_id {
            Some(route_id) => RouteBind::At {
                route_id,
                provider_id: input.provider_id,
                provider_model_id: input.provider_model_id,
                weight: input.weight.unwrap_or(100),
                priority: input.priority.unwrap_or(1),
            },
            None => RouteBind::OneClick {
                provider_id: input.provider_id,
                provider_model_id: input.provider_model_id,
            },
        };
        RouteModule::new(self).bind(bind).await
    }

    pub async fn unbind_route(&self, input: UnbindRouteInput) -> anyhow::Result<Option<Model>> {
        RouteModule::new(self)
            .unbind(RouteUnbind {
                route_id: input.route_id,
                provider_id: input.provider_id,
                provider_model_id: input.provider_model_id,
            })
            .await
    }

    pub async fn reset_target_thinking_mapping(
        &self,
        route_id: &str,
        target_id: &str,
        level: ThinkingLevel,
    ) -> anyhow::Result<Model> {
        RouteModule::new(self)
            .reset_thinking_mapping(route_id, target_id, level)
            .await
    }

    pub async fn regenerate_target_thinking_map(
        &self,
        route_id: &str,
        target_id: &str,
    ) -> anyhow::Result<Model> {
        RouteModule::new(self)
            .regenerate_thinking_map(route_id, target_id)
            .await
    }
}

fn route_targets_for_update(route: &Model) -> Vec<UpsertModelBackend> {
    route
        .targets
        .iter()
        .map(|target| UpsertModelBackend {
            id: Some(target.id.clone()),
            provider_id: target.provider_id.clone(),
            model: target.model.clone(),
            weight: Some(target.weight),
            priority: Some(target.priority),
            thinking_level_map: target.thinking_level_map.0.clone(),
        })
        .collect()
}

fn create_backend_from_upsert(target: &UpsertModelBackend) -> CreateModelBackend {
    CreateModelBackend {
        provider_id: target.provider_id.clone(),
        model: target.model.clone(),
        weight: target.weight,
        priority: target.priority,
        thinking_level_map: target.thinking_level_map.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::GatewayConfig;
    use crate::db::models::{CreateProvider, ProviderCredentialInput, ProviderSourceInput};
    use crate::provider_models::{
        CreateManualProviderModel, ProviderModelSelectionPolicy, UpdateProviderModelSelection,
    };
    use crate::thinking::mapping_control;

    async fn route_fixture_with_protocol(
        protocol: &str,
    ) -> anyhow::Result<(tempfile::TempDir, Gateway, Provider)> {
        let data_dir = tempfile::tempdir()?;
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        })
        .await?;
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some("Route Test Provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: None,
                    protocol: protocol.into(),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                use_proxy: false,
            })
            .await?;
        gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                "upstream-model",
                CreateManualProviderModel {
                    metadata: json!({
                        "id": "upstream-model",
                        "name": "Upstream Model",
                        "limit": {
                            "context": 200000,
                            "output": 32000
                        }
                    }),
                },
            )
            .await?;
        Ok((data_dir, gateway, provider))
    }

    async fn route_fixture() -> anyhow::Result<(tempfile::TempDir, Gateway, Provider)> {
        route_fixture_with_protocol("openai").await
    }

    #[tokio::test]
    async fn one_click_bind_is_idempotent_and_uses_upstream_id_as_route_id() -> anyhow::Result<()> {
        let (_data_dir, gateway, provider) = route_fixture().await?;
        let admin = gateway.admin();
        let routes = RouteModule::new(&admin);
        let input = RouteBind::OneClick {
            provider_id: provider.id.clone(),
            provider_model_id: "upstream-model".into(),
        };

        let first = routes.bind(input.clone()).await?;
        let detail = admin
            .get_provider_model(&provider.id, "upstream-model")
            .await?;
        admin
            .update_provider_model_selection(
                &provider.id,
                "upstream-model",
                UpdateProviderModelSelection {
                    policy: ProviderModelSelectionPolicy::ForceDisabled,
                    revision: detail.revision,
                },
            )
            .await?;
        let second = routes.bind(input).await?;

        assert_eq!(first.id, second.id);
        assert_eq!(first.name, "upstream-model");
        assert_eq!(second.targets.len(), 1);
        assert_eq!(second.targets[0].provider_id, provider.id);
        assert_eq!(second.targets[0].model, "upstream-model");
        assert_eq!(second.context_window, Some(200_000));
        assert_eq!(second.output_max_tokens, Some(32_000));
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_provider_model_cannot_be_bound_as_a_new_target() -> anyhow::Result<()> {
        let (_data_dir, gateway, provider) = route_fixture().await?;
        let admin = gateway.admin();
        let detail = admin
            .get_provider_model(&provider.id, "upstream-model")
            .await?;
        admin
            .update_provider_model_selection(
                &provider.id,
                "upstream-model",
                UpdateProviderModelSelection {
                    policy: ProviderModelSelectionPolicy::ForceDisabled,
                    revision: detail.revision,
                },
            )
            .await?;

        let error = RouteModule::new(&admin)
            .bind(RouteBind::OneClick {
                provider_id: provider.id.clone(),
                provider_model_id: "upstream-model".into(),
            })
            .await
            .expect_err("unavailable Provider Model must be rejected");

        assert!(error.to_string().contains("not available"));
        let change_error = admin
            .create_model(CreateModel {
                name: "manual-route".into(),
                balance: None,
                target_provider: provider.id.clone(),
                target_model: "upstream-model".into(),
                targets: vec![CreateModelBackend {
                    provider_id: provider.id,
                    model: "upstream-model".into(),
                    weight: Some(100),
                    priority: Some(1),
                    thinking_level_map: Vec::new(),
                }],
            })
            .await
            .expect_err("Route change must enforce Effective Availability");
        assert!(change_error.to_string().contains("not available"));
        assert!(admin.list_models().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn missing_provider_model_cannot_be_added_as_a_new_target() -> anyhow::Result<()> {
        let (_data_dir, gateway, provider) = route_fixture().await?;
        let admin = gateway.admin();

        let error = admin
            .create_model(CreateModel {
                name: "missing-model-route".into(),
                balance: None,
                target_provider: provider.id,
                target_model: "missing-model".into(),
                targets: vec![],
            })
            .await
            .expect_err("a Target requires a Provider Model snapshot");

        assert!(error.to_string().contains("PROVIDER_MODEL_NOT_FOUND"));
        assert!(admin.list_models().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn unbinding_the_last_target_deletes_the_route() -> anyhow::Result<()> {
        let (_data_dir, gateway, provider) = route_fixture().await?;
        let admin = gateway.admin();
        let routes = RouteModule::new(&admin);
        routes
            .bind(RouteBind::OneClick {
                provider_id: provider.id.clone(),
                provider_model_id: "upstream-model".into(),
            })
            .await?;

        let remaining = routes
            .unbind(RouteUnbind {
                route_id: "upstream-model".into(),
                provider_id: provider.id,
                provider_model_id: "upstream-model".into(),
            })
            .await?;

        assert!(remaining.is_none());
        assert!(admin.list_models().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn route_generates_seven_rows_seeds_levels_and_resets_one_override() -> anyhow::Result<()>
    {
        let (_data_dir, gateway, provider) = route_fixture().await?;
        let admin = gateway.admin();
        admin
            .create_manual_provider_model(
                &provider.id,
                "effort-model",
                CreateManualProviderModel {
                    metadata: json!({
                        "id": "effort-model",
                        "reasoning_options": [{
                            "type": "effort",
                            "values": ["none", "low", "high", "max"]
                        }]
                    }),
                },
            )
            .await?;
        let route = admin
            .create_model(CreateModel {
                name: "thinking-route".into(),
                balance: None,
                target_provider: provider.id.clone(),
                target_model: "effort-model".into(),
                targets: Vec::new(),
            })
            .await?;

        assert_eq!(route.targets[0].thinking_level_map.len(), 7);
        assert_eq!(
            route.supported_thinking_levels.0,
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::High,
                ThinkingLevel::Max
            ]
        );
        assert_eq!(
            mapping_control(&route.targets[0].thinking_level_map, ThinkingLevel::Minimal),
            Some(&crate::thinking::TargetThinkingControl::Hidden)
        );

        let mut targets = route_targets_for_update(&route);
        let low = targets[0]
            .thinking_level_map
            .iter_mut()
            .find(|row| row.level == ThinkingLevel::Low)
            .expect("low row");
        low.control = crate::thinking::TargetThinkingControl::Hidden;
        let updated = admin
            .update_model(
                &route.id,
                UpdateModel {
                    targets: Some(targets),
                    ..UpdateModel::default()
                },
            )
            .await?;
        assert_eq!(
            updated.supported_thinking_levels.0,
            vec![ThinkingLevel::Off, ThinkingLevel::High, ThinkingLevel::Max]
        );
        let low = updated.targets[0]
            .thinking_level_map
            .iter()
            .find(|row| row.level == ThinkingLevel::Low)
            .expect("low row");
        assert_eq!(low.source, ThinkingMappingSource::Overridden);

        let reset = admin
            .reset_target_thinking_mapping(&route.id, &updated.targets[0].id, ThinkingLevel::Low)
            .await?;
        let low = reset.targets[0]
            .thinking_level_map
            .iter()
            .find(|row| row.level == ThinkingLevel::Low)
            .expect("low row");
        assert_eq!(low.source, ThinkingMappingSource::Generated);
        assert_eq!(
            low.control,
            crate::thinking::TargetThinkingControl::Effort {
                value: "low".into()
            }
        );
        assert_eq!(
            reset.supported_thinking_levels.0,
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::High,
                ThinkingLevel::Max
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn open_responses_accepts_max_effort_map() -> anyhow::Result<()> {
        let (_data_dir, gateway, provider) = route_fixture_with_protocol("open-responses").await?;
        let admin = gateway.admin();
        admin
            .create_manual_provider_model(
                &provider.id,
                "max-effort-model",
                CreateManualProviderModel {
                    metadata: json!({
                        "id": "max-effort-model",
                        "reasoning_options": [{
                            "type": "effort",
                            "values": ["none", "max"]
                        }]
                    }),
                },
            )
            .await?;

        let route = admin
            .create_model(CreateModel {
                name: "max-effort-route".into(),
                balance: None,
                target_provider: provider.id,
                target_model: "max-effort-model".into(),
                targets: Vec::new(),
            })
            .await?;

        assert_eq!(
            mapping_control(&route.targets[0].thinking_level_map, ThinkingLevel::Max),
            Some(&crate::thinking::TargetThinkingControl::Effort {
                value: "max".into()
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn gemini_accepts_generated_effort_maps() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        })
        .await?;
        let admin = gateway.admin();
        let provider = admin
            .create_provider(CreateProvider {
                name: Some("Gemini Route Test Provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: None,
                    protocol: "google-gemini".into(),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                use_proxy: false,
            })
            .await?;
        admin
            .create_manual_provider_model(
                &provider.id,
                "gemini-effort-model",
                CreateManualProviderModel {
                    metadata: json!({
                        "id": "gemini-effort-model",
                        "reasoning_options": [{
                            "type": "effort",
                            "values": ["low", "high"]
                        }]
                    }),
                },
            )
            .await?;

        let route = admin
            .create_model(CreateModel {
                name: "gemini-thinking-route".into(),
                balance: None,
                target_provider: provider.id,
                target_model: "gemini-effort-model".into(),
                targets: Vec::new(),
            })
            .await?;

        assert_eq!(
            route.supported_thinking_levels.0,
            vec![ThinkingLevel::Low, ThinkingLevel::High]
        );
        Ok(())
    }

    #[tokio::test]
    async fn supported_levels_are_the_intersection_of_all_targets() -> anyhow::Result<()> {
        let (_data_dir, gateway, provider) = route_fixture().await?;
        let admin = gateway.admin();
        for (model, values, context, output) in [
            (
                "wide-effort-model",
                vec!["none", "low", "high", "max"],
                200_000,
                64_000,
            ),
            ("narrow-effort-model", vec!["low", "high"], 128_000, 32_000),
        ] {
            admin
                .create_manual_provider_model(
                    &provider.id,
                    model,
                    CreateManualProviderModel {
                        metadata: json!({
                            "id": model,
                            "reasoning_options": [{
                                "type": "effort",
                                "values": values
                            }],
                            "limit": {
                                "context": context,
                                "output": output
                            }
                        }),
                    },
                )
                .await?;
        }

        let route = admin
            .create_model(CreateModel {
                name: "intersection-route".into(),
                balance: None,
                target_provider: provider.id.clone(),
                target_model: "wide-effort-model".into(),
                targets: vec![
                    CreateModelBackend {
                        provider_id: provider.id.clone(),
                        model: "wide-effort-model".into(),
                        weight: Some(100),
                        priority: Some(1),
                        thinking_level_map: Vec::new(),
                    },
                    CreateModelBackend {
                        provider_id: provider.id,
                        model: "narrow-effort-model".into(),
                        weight: Some(100),
                        priority: Some(1),
                        thinking_level_map: Vec::new(),
                    },
                ],
            })
            .await?;

        assert_eq!(
            route.supported_thinking_levels.0,
            vec![ThinkingLevel::Low, ThinkingLevel::High]
        );
        assert_eq!(route.context_window, Some(128_000));
        assert_eq!(route.output_max_tokens, Some(32_000));
        Ok(())
    }

    #[tokio::test]
    async fn regenerate_updates_derived_supported_levels() -> anyhow::Result<()> {
        let (_data_dir, gateway, _provider) = route_fixture().await?;
        let admin = gateway.admin();
        let provider = admin
            .create_provider(CreateProvider {
                name: Some("Toggle Provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: None,
                    protocol: "anthropic".into(),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                use_proxy: false,
            })
            .await?;
        admin
            .create_manual_provider_model(
                &provider.id,
                "toggle-model",
                CreateManualProviderModel {
                    metadata: json!({
                        "id": "toggle-model",
                        "reasoning_options": [{"type": "toggle"}]
                    }),
                },
            )
            .await?;
        let route = admin
            .create_model(CreateModel {
                name: "toggle-route".into(),
                balance: None,
                target_provider: provider.id,
                target_model: "toggle-model".into(),
                targets: Vec::new(),
            })
            .await?;
        let mut targets = route_targets_for_update(&route);
        let high = targets[0]
            .thinking_level_map
            .iter_mut()
            .find(|row| row.level == ThinkingLevel::High)
            .expect("high row");
        high.control = crate::thinking::TargetThinkingControl::Effort {
            value: "high".into(),
        };
        let updated = admin
            .update_model(
                &route.id,
                UpdateModel {
                    targets: Some(targets),
                    ..UpdateModel::default()
                },
            )
            .await?;
        assert_eq!(
            updated.supported_thinking_levels.0,
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Medium,
                ThinkingLevel::High
            ]
        );

        let regenerated = admin
            .regenerate_target_thinking_map(&route.id, &updated.targets[0].id)
            .await?;
        assert_eq!(
            regenerated.supported_thinking_levels.0,
            vec![ThinkingLevel::Off, ThinkingLevel::Medium]
        );
        Ok(())
    }

    #[tokio::test]
    async fn refresh_regenerates_only_generated_rows() -> anyhow::Result<()> {
        let (_data_dir, gateway, provider) = route_fixture().await?;
        let admin = gateway.admin();
        let route = admin
            .create_model(CreateModel {
                name: "refresh-route".into(),
                balance: None,
                target_provider: provider.id.clone(),
                target_model: "upstream-model".into(),
                targets: Vec::new(),
            })
            .await?;
        let mut targets = route_targets_for_update(&route);
        let high = targets[0]
            .thinking_level_map
            .iter_mut()
            .find(|row| row.level == ThinkingLevel::High)
            .expect("high row");
        high.control = crate::thinking::TargetThinkingControl::Effort {
            value: "custom-high".into(),
        };
        let route = admin
            .update_model(
                &route.id,
                UpdateModel {
                    targets: Some(targets),
                    ..UpdateModel::default()
                },
            )
            .await?;
        let refreshed_metadata: crate::provider_models::ProviderModelMetadata =
            serde_json::from_value(json!({
                "id": "upstream-model",
                "reasoning_options": [{
                    "type": "effort",
                    "values": ["none", "low", "max"]
                }]
            }))?;
        RouteModule::new(&admin)
            .refresh_generated_thinking_maps(
                &provider.id,
                "upstream-model",
                &refreshed_metadata,
                true,
            )
            .await?;

        let refreshed = admin
            .list_models()
            .await?
            .into_iter()
            .find(|candidate| candidate.id == route.id)
            .expect("refreshed Route");
        let medium = refreshed.targets[0]
            .thinking_level_map
            .iter()
            .find(|row| row.level == ThinkingLevel::Medium)
            .expect("medium row");
        assert_eq!(medium.source, ThinkingMappingSource::Generated);
        assert_eq!(
            medium.control,
            crate::thinking::TargetThinkingControl::Hidden
        );
        let high = refreshed.targets[0]
            .thinking_level_map
            .iter()
            .find(|row| row.level == ThinkingLevel::High)
            .expect("high row");
        assert_eq!(high.source, ThinkingMappingSource::Overridden);
        assert_eq!(
            high.control,
            crate::thinking::TargetThinkingControl::Effort {
                value: "custom-high".into()
            }
        );
        Ok(())
    }
}
