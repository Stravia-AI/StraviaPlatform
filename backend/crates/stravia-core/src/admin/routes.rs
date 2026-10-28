use super::*;
use crate::provider_models::{
    NewProviderModelRecord, ProviderModelMutation, ProviderModelPresence,
    ProviderModelSelectionPolicy, ProviderModelSourceKind, model_id_match_key, normalize_model_id,
};
use crate::thinking::ThinkingMappingSource;
use crate::thinking::generate_thinking_level_map;
use stravia_runtime_contract::thinking::ThinkingLevel;

mod model_discovery;
mod model_records;
mod provider_model_records;
mod thinking_map;
use model_discovery::RouteModelDiscoveryError;
pub use model_records::RouteTargetStatus;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindRouteInput {
    pub route_id: Option<String>,
    pub provider_id: String,
    pub provider_model_id: String,
    pub priority: Option<i32>,
    pub first_token_timeout_ms: Option<i64>,
    pub target_retry_budget: Option<i32>,
    pub target_cooldown_ms: Option<i64>,
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
        priority: i32,
        first_token_timeout_ms: i64,
        target_retry_budget: i32,
        target_cooldown_ms: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteUnbind {
    pub route_id: String,
    pub provider_id: String,
    pub provider_model_id: String,
}

pub(crate) struct RouteModule<'a> {
    gw: &'a Gateway,
}

impl<'a> RouteModule<'a> {
    pub(crate) fn new(gw: &'a Gateway) -> Self {
        Self { gw }
    }

    /// Route module 唯一的 Provider 读投影入口；与 ProviderConnection::get
    /// 共享「provider not found」错误契约。
    async fn get_provider(&self, provider_id: &str) -> anyhow::Result<Provider> {
        self.gw
            .storage
            .providers()
            .get(provider_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider not found: {provider_id}"))
    }

    pub(crate) async fn discover_provider_model_ids(
        &self,
        provider_id: &str,
    ) -> Result<Vec<String>, RouteModelDiscoveryError> {
        let discovered = model_discovery::discover_provider_models(self.gw, provider_id).await?;
        let _publication_guard = discovered.write_fence().await.map_err(|error| {
            RouteModelDiscoveryError::DiscoverySetup {
                provider_id: provider_id.to_owned(),
                source: error,
            }
        })?;
        Ok(discovered
            .models
            .into_iter()
            .map(|model| model.id)
            .collect())
    }

    pub(crate) async fn create(&self, input: CreateRoute) -> anyhow::Result<RouteConfig> {
        let _guard = self.gw.route_media_mutation.lock().await;
        self.create_locked(input).await
    }

    async fn create_locked(&self, mut input: CreateRoute) -> anyhow::Result<RouteConfig> {
        ensure_route_targets_valid(&input.targets)?;
        self.ensure_new_targets_available(&[], &input.targets)
            .await?;
        self.ensure_targets_match(input.output_modality, &[], &input.targets, true)
            .await?;
        self.prepare_thinking_maps(&[], &mut input.targets).await?;
        self.ensure_thinking_controls_representable(&input.targets)
            .await?;
        self.create_record(input).await
    }

    pub(crate) async fn change(
        &self,
        route_id: &str,
        input: UpdateRoute,
    ) -> anyhow::Result<RouteConfig> {
        let _guard = self.gw.route_media_mutation.lock().await;
        self.change_locked(route_id, input).await
    }

    async fn change_locked(
        &self,
        route_id: &str,
        mut input: UpdateRoute,
    ) -> anyhow::Result<RouteConfig> {
        let current = self.get(route_id).await?;
        if let Some(modality) = input.output_modality {
            if modality == crate::db::models::RouteOutputModality::Text
                && crate::media_generation::config::load(self.gw)
                    .await?
                    .image
                    .route_id
                    .as_deref()
                    == Some(current.model_id.as_str())
            {
                return Err(coded_error(
                    "ROUTE_IMAGE_GENERATION_BOUND",
                    "Unbind this Route in image generation configuration before selecting text output",
                    serde_json::json!({"route_id": route_id, "action": "unbind_image_generation"}),
                ));
            }
            let retained;
            let targets = match input.targets.as_deref() {
                Some(targets) => targets,
                None => {
                    retained = route_targets_for_update(&current);
                    &retained
                }
            };
            ensure_route_targets_valid(targets)?;
            self.ensure_new_targets_available(&current.targets, targets)
                .await?;
            self.ensure_targets_match(modality, &[], targets, true)
                .await?;
        } else if let Some(targets) = input.targets.as_deref() {
            let adds_target = targets.iter().any(|target| {
                !current.targets.iter().any(|existing| {
                    existing.provider_id().as_str() == target.provider_id.trim()
                        && existing.model().as_str() == target.model.trim()
                })
            });
            if let Some(modality) = current.output_modality {
                self.ensure_targets_match(modality, &current.targets, targets, false)
                    .await?;
            } else if adds_target {
                return Err(route_unconfigured_error(route_id));
            }
        }
        if let Some(targets) = input.targets.as_mut() {
            if input.output_modality.is_none() {
                ensure_route_targets_valid(targets)?;
                self.ensure_new_targets_available(&current.targets, targets)
                    .await?;
            }
            self.prepare_thinking_maps(&current.targets, targets)
                .await?;
            self.ensure_thinking_controls_representable(targets).await?;
        }
        self.change_record(route_id, input).await
    }

    async fn ensure_targets_match(
        &self,
        modality: crate::db::models::RouteOutputModality,
        existing: &[TargetConfig],
        proposed: &[CreateTarget],
        validate_all: bool,
    ) -> anyhow::Result<()> {
        for target in proposed {
            if !validate_all
                && existing.iter().any(|current| {
                    current.provider_id().as_str() == target.provider_id.trim()
                        && current.model().as_str() == target.model.trim()
                })
            {
                continue;
            }
            let provider_id = target.provider_id.trim();
            let model_id = normalize_model_id(&target.model)?;
            let record = self
                .gw
                .storage
                .provider_models()
                .find(provider_id, &model_id)
                .await?;
            if !record
                .is_some_and(|record| record.metadata.supports_route_output_modality(modality))
            {
                return Err(coded_error(
                    "ROUTE_OUTPUT_MODALITY_MISMATCH",
                    "Remove or replace the Target, or register its matching output modality",
                    serde_json::json!({
                        "provider_id": provider_id, "model_id": model_id,
                        "output_modality": modality, "action": "repair_target_output_modality"
                    }),
                ));
            }
        }
        Ok(())
    }

    async fn ensure_new_targets_available(
        &self,
        existing: &[TargetConfig],
        proposed: &[CreateTarget],
    ) -> anyhow::Result<()> {
        for target in proposed {
            let provider_id = target.provider_id.trim();
            let provider_model_id = normalize_model_id(&target.model)?;
            if existing.iter().any(|current| {
                current.provider_id().as_str() == provider_id
                    && same_target_model(current.model().as_str(), &provider_model_id)
            }) {
                continue;
            }
            let Some(provider_model) = self
                .gw
                .storage
                .provider_models()
                .find(provider_id, &provider_model_id)
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
        let routes = self.list().await?;
        for route in routes.into_iter().filter(|route| {
            route
                .targets
                .iter()
                .any(|target| target.provider_id().as_str() == original_provider_id)
        }) {
            let copied_targets = route
                .targets
                .iter()
                .filter(|target| target.provider_id().as_str() == original_provider_id)
                .cloned()
                .collect::<Vec<_>>();

            for target in &copied_targets {
                self.copy_provider_model(
                    original_provider_id,
                    copied_provider_id,
                    target.model().as_str(),
                )
                .await?;
            }

            let mut targets = route
                .targets
                .iter()
                .map(|target| CreateTarget {
                    provider_id: target.provider_id().clone().into(),
                    model: target.model().clone().into(),
                    enabled: target.enabled,
                    priority: Some(target.priority),
                    first_token_timeout_ms: Some(target.first_token_timeout_ms),
                    target_retry_budget: Some(target.target_retry_budget),
                    target_cooldown_ms: Some(target.target_cooldown_ms),
                    thinking_level_map: target.thinking_level_map.clone(),
                })
                .collect::<Vec<_>>();
            targets.extend(copied_targets.into_iter().map(|target| {
                let (_, model) = target.destination.into_parts();
                CreateTarget {
                    provider_id: copied_provider_id.to_string(),
                    model: model.into(),
                    enabled: target.enabled,
                    priority: Some(target.priority),
                    first_token_timeout_ms: Some(target.first_token_timeout_ms),
                    target_retry_budget: Some(target.target_retry_budget),
                    target_cooldown_ms: Some(target.target_cooldown_ms),
                    thinking_level_map: Vec::new(),
                }
            }));

            self.change(
                &route.model_id,
                UpdateRoute {
                    targets: Some(targets),
                    ..UpdateRoute::default()
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
        let store = self.gw.storage.provider_models();
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
                snapshot_state: original.snapshot_state,
                source_metadata: original.source_metadata,
                metadata: original.metadata,
            },
            None => NewProviderModelRecord {
                provider_id: copied_provider_id.to_string(),
                model_id: provider_model_id.to_string(),
                source_kind: ProviderModelSourceKind::Manual,
                metadata_source_provider_id: None,
                presence: ProviderModelPresence::Present,
                selection_policy: ProviderModelSelectionPolicy::Auto,
                snapshot_state: crate::provider_models::SnapshotState::Local,
                source_metadata: None,
                metadata: serde_json::from_value(serde_json::json!({
                    "id": provider_model_id,
                    "name": provider_model_id,
                }))?,
            },
        };

        match store.create(input, None).await? {
            ProviderModelMutation::Applied(_) | ProviderModelMutation::Conflict => Ok(()),
            ProviderModelMutation::NotFound => {
                anyhow::bail!("copied Provider Model unexpectedly disappeared")
            }
        }
    }

    pub(crate) async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        self.delete_record(route_id).await
    }

    pub(crate) async fn bind(&self, input: RouteBind) -> anyhow::Result<RouteConfig> {
        let _guard = self.gw.route_media_mutation.lock().await;
        let one_click = matches!(&input, RouteBind::OneClick { .. });
        let (
            route_id,
            provider_id,
            provider_model_id,
            priority,
            first_token_timeout_ms,
            target_retry_budget,
            target_cooldown_ms,
        ) = match input {
            RouteBind::OneClick {
                provider_id,
                provider_model_id,
            } => {
                let route_id = provider_model_id.clone();
                (
                    route_id,
                    provider_id,
                    provider_model_id,
                    DEFAULT_TARGET_PRIORITY,
                    DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
                    DEFAULT_TARGET_RETRY_BUDGET,
                    DEFAULT_TARGET_COOLDOWN_MS,
                )
            }
            RouteBind::At {
                route_id,
                provider_id,
                provider_model_id,
                priority,
                first_token_timeout_ms,
                target_retry_budget,
                target_cooldown_ms,
            } => {
                let target = CreateTarget {
                    provider_id: provider_id.clone(),
                    model: provider_model_id.clone(),
                    enabled: true,
                    priority: Some(priority),
                    first_token_timeout_ms: Some(first_token_timeout_ms),
                    target_retry_budget: Some(target_retry_budget),
                    target_cooldown_ms: Some(target_cooldown_ms),
                    thinking_level_map: Vec::new(),
                };
                ensure_route_targets_valid(std::slice::from_ref(&target))?;
                (
                    route_id,
                    provider_id,
                    provider_model_id,
                    priority,
                    first_token_timeout_ms,
                    target_retry_budget,
                    target_cooldown_ms,
                )
            }
        };
        let route_id = normalize_name(&route_id, "model ID sent by clients")?;
        let provider_model_id = normalize_model_id(&provider_model_id)?;
        let mut existing = self.gw.storage.routes().get(&route_id).await?;
        if existing.is_none() && !one_click {
            return Err(coded_error(
                "ROUTE_NOT_FOUND",
                "Create the Route with an explicit output modality before binding a Target",
                serde_json::json!({"route_id": route_id, "action": "create_route"}),
            ));
        }
        if let Some(route) = existing.as_mut() {
            self.refresh_route_target_projections(std::slice::from_mut(route))
                .await?;
        }
        self.get_provider(&provider_id).await?;
        let provider_model = self
            .gw
            .storage
            .provider_models()
            .find(&provider_id, &provider_model_id)
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
        let modality = match existing.as_ref() {
            Some(route) => route
                .output_modality
                .ok_or_else(|| route_unconfigured_error(&route_id))?,
            None => [
                crate::db::models::RouteOutputModality::Text,
                crate::db::models::RouteOutputModality::Image,
            ]
            .into_iter()
            .find(|modality| {
                provider_model
                    .metadata
                    .supports_route_output_modality(*modality)
            })
            .ok_or_else(|| {
                coded_error(
                    "ROUTE_OUTPUT_MODALITY_UNREGISTERED",
                    "Register text or image output before connecting this Provider Model",
                    serde_json::json!({"provider_id": provider_id, "model_id": provider_model_id,
                        "action": "register_output_modality"}),
                )
            })?,
        };
        if !provider_model
            .metadata
            .supports_route_output_modality(modality)
        {
            return Err(coded_error(
                "ROUTE_OUTPUT_MODALITY_MISMATCH",
                "Select a matching Route or manually reconfigure its output modality",
                serde_json::json!({"route_id": route_id, "provider_id": provider_id,
                    "model_id": provider_model_id, "output_modality": modality,
                    "action": "reconfigure_route"}),
            ));
        }
        if let Some(route) = existing.as_ref()
            && route.targets.iter().any(|target| {
                target.provider_id().as_str() == provider_id
                    && same_target_model(target.model().as_str(), &provider_model_id)
            })
        {
            return Ok(route.clone());
        }
        if !provider_model.effective_available() {
            return Err(coded_error(
                "PROVIDER_MODEL_UNAVAILABLE",
                "Provider Model is not available for a new Target",
                serde_json::json!({"provider_id": provider_id, "model_id": provider_model_id}),
            ));
        }

        let Some(existing) = existing else {
            return self
                .create_locked(CreateRoute {
                    model_id: route_id,
                    output_modality: modality,
                    display_name: provider_model.metadata.name,
                    balance: Some("traffic_equalization".into()),
                    default_thinking_level: None,
                    billing: None,
                    targets: vec![CreateTarget {
                        provider_id,
                        model: provider_model_id,
                        enabled: true,
                        priority: Some(priority),
                        first_token_timeout_ms: Some(first_token_timeout_ms),
                        target_retry_budget: Some(target_retry_budget),
                        target_cooldown_ms: Some(target_cooldown_ms),
                        thinking_level_map: Vec::new(),
                    }],
                })
                .await;
        };
        let mut targets = existing
            .targets
            .iter()
            .map(|target| CreateTarget {
                provider_id: target.provider_id().clone().into(),
                model: target.model().clone().into(),
                enabled: target.enabled,
                priority: Some(target.priority),
                first_token_timeout_ms: Some(target.first_token_timeout_ms),
                target_retry_budget: Some(target.target_retry_budget),
                target_cooldown_ms: Some(target.target_cooldown_ms),
                thinking_level_map: target.thinking_level_map.clone(),
            })
            .collect::<Vec<_>>();
        targets.push(CreateTarget {
            provider_id,
            model: provider_model_id,
            enabled: true,
            priority: Some(priority),
            first_token_timeout_ms: Some(first_token_timeout_ms),
            target_retry_budget: Some(target_retry_budget),
            target_cooldown_ms: Some(target_cooldown_ms),
            thinking_level_map: Vec::new(),
        });
        self.change_locked(
            &existing.model_id,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await
    }

    pub(crate) async fn unbind(&self, input: RouteUnbind) -> anyhow::Result<Option<RouteConfig>> {
        let route_id = normalize_name(&input.route_id, "model ID sent by clients")?;
        let provider_model_id = normalize_model_id(&input.provider_model_id)?;
        let route = self
            .gw
            .storage
            .routes()
            .get(&route_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Route not found: {route_id}"))?;
        let targets = route
            .targets
            .iter()
            .filter(|target| {
                target.provider_id().as_str() != input.provider_id
                    || target.model().as_str() != provider_model_id.as_str()
            })
            .map(|target| CreateTarget {
                provider_id: target.provider_id().clone().into(),
                model: target.model().clone().into(),
                enabled: target.enabled,
                priority: Some(target.priority),
                first_token_timeout_ms: Some(target.first_token_timeout_ms),
                target_retry_budget: Some(target.target_retry_budget),
                target_cooldown_ms: Some(target.target_cooldown_ms),
                thinking_level_map: target.thinking_level_map.clone(),
            })
            .collect::<Vec<_>>();
        if targets.len() == route.targets.len() {
            return Ok(Some(route));
        }
        if targets.is_empty() {
            self.delete(&route.model_id).await?;
            return Ok(None);
        }
        self.change(
            &route.model_id,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await
        .map(Some)
    }
}

impl AdminService {
    pub async fn bind_route(&self, input: BindRouteInput) -> anyhow::Result<RouteConfig> {
        let bind = match input.route_id {
            Some(route_id) => RouteBind::At {
                route_id,
                provider_id: input.provider_id,
                provider_model_id: input.provider_model_id,
                priority: input.priority.unwrap_or(DEFAULT_TARGET_PRIORITY),
                first_token_timeout_ms: input
                    .first_token_timeout_ms
                    .unwrap_or(DEFAULT_FIRST_TOKEN_TIMEOUT_MS),
                target_retry_budget: input
                    .target_retry_budget
                    .unwrap_or(DEFAULT_TARGET_RETRY_BUDGET),
                target_cooldown_ms: input
                    .target_cooldown_ms
                    .unwrap_or(DEFAULT_TARGET_COOLDOWN_MS),
            },
            None => RouteBind::OneClick {
                provider_id: input.provider_id,
                provider_model_id: input.provider_model_id,
            },
        };
        RouteModule::new(&self.gw).bind(bind).await
    }

    pub async fn unbind_route(
        &self,
        input: UnbindRouteInput,
    ) -> anyhow::Result<Option<RouteConfig>> {
        RouteModule::new(&self.gw)
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
    ) -> anyhow::Result<RouteConfig> {
        RouteModule::new(&self.gw)
            .reset_thinking_mapping(route_id, target_id, level)
            .await
    }

    pub async fn regenerate_target_thinking_map(
        &self,
        route_id: &str,
        target_id: &str,
    ) -> anyhow::Result<RouteConfig> {
        RouteModule::new(&self.gw)
            .regenerate_thinking_map(route_id, target_id)
            .await
    }
}

fn same_target_model(left: &str, right: &str) -> bool {
    model_id_match_key(left) == model_id_match_key(right)
}

fn route_unconfigured_error(route_id: &str) -> anyhow::Error {
    coded_error(
        "ROUTE_OUTPUT_MODALITY_UNCONFIGURED",
        "Explicitly select an output modality and repair all Targets before using this Route",
        serde_json::json!({"route_id": route_id, "action": "reconfigure_route"}),
    )
}

fn route_targets_for_update(route: &RouteConfig) -> Vec<CreateTarget> {
    route
        .targets
        .iter()
        .map(|target| CreateTarget {
            provider_id: target.provider_id().clone().into(),
            model: target.model().clone().into(),
            enabled: target.enabled,
            priority: Some(target.priority),
            first_token_timeout_ms: Some(target.first_token_timeout_ms),
            target_retry_budget: Some(target.target_retry_budget),
            target_cooldown_ms: Some(target.target_cooldown_ms),
            thinking_level_map: target.thinking_level_map.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
