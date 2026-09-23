use super::*;
use crate::provider_models::{
    CreateManualProviderModel, NewProviderModelRecord, ProviderModelDetail, ProviderModelMutation,
    ProviderModelPresence, ProviderModelSelectionPolicy, ProviderModelSourceKind,
    ProviderModelSyncSummary, model_id_match_key, normalize_model_id,
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
use provider_model_records::PreparedProviderModel;

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
    ) -> Result<Vec<String>, RouteModelDiscoveryError> {
        let discovered = model_discovery::discover_provider_models(self.admin, provider_id).await?;
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

    pub(crate) async fn create(&self, mut input: CreateRoute) -> anyhow::Result<RouteConfig> {
        ensure_route_targets_valid(&input.targets)?;
        self.ensure_new_targets_available(&[], &input.targets)
            .await?;
        self.prepare_thinking_maps(&[], &mut input.targets).await?;
        self.ensure_thinking_controls_representable(&input.targets)
            .await?;
        self.create_record(input).await
    }

    pub(crate) async fn change(
        &self,
        route_id: &str,
        mut input: UpdateRoute,
    ) -> anyhow::Result<RouteConfig> {
        let current = self.get(route_id).await?;
        if let Some(targets) = input.targets.as_mut() {
            ensure_route_targets_valid(targets)?;
            self.ensure_new_targets_available(&current.targets, targets)
                .await?;
            self.prepare_thinking_maps(&current.targets, targets)
                .await?;
            self.ensure_thinking_controls_representable(targets).await?;
        }
        self.change_record(route_id, input).await
    }

    async fn ensure_new_targets_available(
        &self,
        existing: &[TargetConfig],
        proposed: &[CreateTarget],
    ) -> anyhow::Result<()> {
        for target in proposed {
            let provider_id = target.provider_id.trim();
            let provider_model_id = target
                .model
                .as_deref()
                .map(normalize_model_id)
                .transpose()?;
            if existing.iter().any(|current| {
                current.provider_id().as_str() == provider_id
                    && same_target_model(
                        current.model().map(|model| model.as_str()),
                        provider_model_id.as_deref(),
                    )
            }) {
                continue;
            }
            if provider_model_id.is_none() {
                self.ensure_provider_only_search_target(provider_id).await?;
                continue;
            }
            let provider_model_id = provider_model_id.expect("checked model Target");
            let Some(provider_model) = self
                .admin
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

    async fn ensure_provider_only_search_target(&self, provider_id: &str) -> anyhow::Result<()> {
        let provider = self.admin.get_provider(provider_id).await?;
        let vendor_id = provider
            .vendor
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                coded_error(
                    "PROVIDER_ONLY_TARGET_UNAVAILABLE",
                    "Provider-only Targets require an installed search Vendor",
                    serde_json::json!({ "provider_id": provider_id }),
                )
            })?;
        let descriptor = self
            .admin
            .gw
            .vendor_plugins
            .descriptor(vendor_id)
            .map_err(|_| {
                coded_error(
                    "PROVIDER_ONLY_TARGET_UNAVAILABLE",
                    "Provider-only Targets require an installed search Vendor",
                    serde_json::json!({ "provider_id": provider_id, "vendor_id": vendor_id }),
                )
            })?;
        let channel_id = provider.channel.as_deref().unwrap_or("default");
        let channel = descriptor
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .ok_or_else(|| {
                coded_error(
                    "PROVIDER_ONLY_TARGET_UNAVAILABLE",
                    "Provider channel is not available in the installed Vendor",
                    serde_json::json!({
                        "provider_id": provider_id,
                        "vendor_id": vendor_id,
                        "channel": channel_id,
                    }),
                )
            })?;
        if !channel
            .capabilities
            .contains(&stravia_vendor_sdk::Capability::Search)
            || channel.search_model_required
        {
            return Err(coded_error(
                "PROVIDER_ONLY_TARGET_UNAVAILABLE",
                "Provider channel does not support model-free search",
                serde_json::json!({
                    "provider_id": provider_id,
                    "vendor_id": vendor_id,
                    "channel": channel_id,
                }),
            ));
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
                .any(|target| target.provider_id().as_str() == original_provider_id)
        }) {
            let copied_targets = route
                .targets
                .iter()
                .filter(|target| target.provider_id().as_str() == original_provider_id)
                .cloned()
                .collect::<Vec<_>>();

            for target in &copied_targets {
                if let Some(model) = target.model().map(|model| model.as_str()) {
                    self.copy_provider_model(original_provider_id, copied_provider_id, model)
                        .await?;
                }
            }

            let mut targets = route
                .targets
                .iter()
                .map(|target| CreateTarget {
                    provider_id: target.provider_id().clone().into(),
                    model: target.model().cloned().map(Into::into),
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
                    model: model.map(Into::into),
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
                snapshot_state: original.snapshot_state,
                metadata: original.metadata,
            },
            None => NewProviderModelRecord {
                provider_id: copied_provider_id.to_string(),
                model_id: provider_model_id.to_string(),
                source_kind: ProviderModelSourceKind::Manual,
                metadata_source_provider_id: None,
                presence: ProviderModelPresence::Present,
                selection_policy: ProviderModelSelectionPolicy::Auto,
                snapshot_state: crate::provider_models::SnapshotState::Edited { source: None },
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

    pub(crate) async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        self.delete_record(route_id).await
    }

    pub(crate) async fn bind(&self, input: RouteBind) -> anyhow::Result<RouteConfig> {
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
                    model: Some(provider_model_id.clone()),
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
        let mut existing = self.admin.gw.storage.routes().get(&route_id).await?;
        if let Some(route) = existing.as_mut() {
            self.refresh_route_client_capabilities(std::slice::from_mut(route))
                .await?;
        }
        if let Some(existing) = existing.as_ref()
            && existing.targets.iter().any(|target| {
                target.provider_id().as_str() == provider_id
                    && target
                        .model()
                        .map(|model| model.as_str())
                        .is_some_and(|model| {
                            model_id_match_key(model) == model_id_match_key(&provider_model_id)
                        })
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
                .create(CreateRoute {
                    model_id: route_id,
                    display_name: provider_model.metadata.name,
                    balance: Some("traffic_equalization".into()),
                    default_thinking_level: None,
                    targets: vec![CreateTarget {
                        provider_id,
                        model: Some(provider_model_id),
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
        if existing.targets.iter().any(|target| {
            target.provider_id().as_str() == provider_id
                && target.model().map(|model| model.as_str()) == Some(provider_model_id.as_str())
        }) {
            return Ok(existing);
        }

        let mut targets = existing
            .targets
            .iter()
            .map(|target| CreateTarget {
                provider_id: target.provider_id().clone().into(),
                model: target.model().cloned().map(Into::into),
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
            model: Some(provider_model_id),
            enabled: true,
            priority: Some(priority),
            first_token_timeout_ms: Some(first_token_timeout_ms),
            target_retry_budget: Some(target_retry_budget),
            target_cooldown_ms: Some(target_cooldown_ms),
            thinking_level_map: Vec::new(),
        });
        self.change(
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
            .admin
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
                    || target.model().map(|model| model.as_str())
                        != Some(provider_model_id.as_str())
            })
            .map(|target| CreateTarget {
                provider_id: target.provider_id().clone().into(),
                model: target.model().cloned().map(Into::into),
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
        RouteModule::new(self).bind(bind).await
    }

    pub async fn unbind_route(
        &self,
        input: UnbindRouteInput,
    ) -> anyhow::Result<Option<RouteConfig>> {
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
    ) -> anyhow::Result<RouteConfig> {
        RouteModule::new(self)
            .reset_thinking_mapping(route_id, target_id, level)
            .await
    }

    pub async fn regenerate_target_thinking_map(
        &self,
        route_id: &str,
        target_id: &str,
    ) -> anyhow::Result<RouteConfig> {
        RouteModule::new(self)
            .regenerate_thinking_map(route_id, target_id)
            .await
    }
}

fn same_target_model(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => model_id_match_key(left) == model_id_match_key(right),
        (None, None) => true,
        _ => false,
    }
}

fn route_targets_for_update(route: &RouteConfig) -> Vec<CreateTarget> {
    route
        .targets
        .iter()
        .map(|target| CreateTarget {
            provider_id: target.provider_id().clone().into(),
            model: target.model().cloned().map(Into::into),
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
