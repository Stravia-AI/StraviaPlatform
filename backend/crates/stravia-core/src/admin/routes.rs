use super::*;
use crate::provider_models::{
    CreateManualProviderModel, NewProviderModelRecord, ProviderModelDetail, ProviderModelMutation,
    ProviderModelPresence, ProviderModelSelectionPolicy, ProviderModelSourceKind,
    ProviderModelSyncSummary, normalize_model_id,
};
use crate::thinking::{ThinkingLevel, ThinkingMappingSource, generate_thinking_level_map};

mod model_discovery;
mod model_records;
mod provider_model_records;
mod thinking_map;
use model_discovery::{
    HttpProviderModelDiscovery, ProviderModelDiscovery, RouteModelDiscoveryError,
};
use provider_model_records::PreparedProviderModel;

static HTTP_PROVIDER_MODEL_DISCOVERY: HttpProviderModelDiscovery = HttpProviderModelDiscovery;

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
    model_discovery: &'a dyn ProviderModelDiscovery,
}

impl<'a> RouteModule<'a> {
    pub(crate) fn new(admin: &'a AdminService) -> Self {
        Self::with_model_discovery(admin, &HTTP_PROVIDER_MODEL_DISCOVERY)
    }

    fn with_model_discovery(
        admin: &'a AdminService,
        model_discovery: &'a dyn ProviderModelDiscovery,
    ) -> Self {
        Self {
            admin,
            model_discovery,
        }
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
        self.model_discovery.discover(self.admin, provider_id).await
    }

    pub(crate) async fn create(&self, mut input: CreateRoute) -> anyhow::Result<Route> {
        let mut targets = normalize_create_route_targets(&input)?;
        self.ensure_new_targets_available(&[], &targets).await?;
        self.prepare_thinking_maps(&[], &mut targets).await?;
        self.ensure_thinking_controls_representable(&targets)
            .await?;
        input.targets = targets;
        self.create_record(input).await
    }

    pub(crate) async fn change(
        &self,
        route_id: &str,
        mut input: UpdateRoute,
    ) -> anyhow::Result<Route> {
        let current = self.get(route_id).await?;
        let mut targets = normalize_update_route_targets(&current, &input)?;
        self.ensure_new_targets_available(&current.targets, &targets)
            .await?;
        self.prepare_thinking_maps(&current.targets, &mut targets)
            .await?;
        self.ensure_thinking_controls_representable(&targets)
            .await?;
        input.targets = Some(
            targets
                .iter()
                .map(|target| UpsertTarget {
                    id: None,
                    provider_id: target.provider_id.clone(),
                    model: target.model.clone(),
                    weight: target.weight,
                    priority: target.priority,
                    thinking_level_map: target.thinking_level_map.clone(),
                })
                .collect(),
        );
        self.change_record(route_id, input).await
    }

    async fn ensure_new_targets_available(
        &self,
        existing: &[Target],
        proposed: &[CreateTarget],
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
                .map(|target| UpsertTarget {
                    id: Some(target.id.clone()),
                    provider_id: target.provider_id.clone(),
                    model: target.model.clone(),
                    weight: Some(target.weight),
                    priority: Some(target.priority),
                    thinking_level_map: target.thinking_level_map.0.clone(),
                })
                .collect::<Vec<_>>();
            targets.extend(copied_targets.into_iter().map(|target| UpsertTarget {
                id: None,
                provider_id: copied_provider_id.to_string(),
                model: target.model,
                weight: Some(target.weight),
                priority: Some(target.priority),
                thinking_level_map: Vec::new(),
            }));

            self.change(
                &route.name,
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

    pub(crate) async fn delete(&self, route_id: &str) -> anyhow::Result<()> {
        self.delete_record(route_id).await
    }

    pub(crate) async fn bind(&self, input: RouteBind) -> anyhow::Result<Route> {
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
        let mut existing = self.admin.gw.storage.routes().get(&route_id).await?;
        if let Some(route) = existing.as_mut() {
            self.refresh_route_token_limits(std::slice::from_mut(route))
                .await?;
        }
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
                .create(CreateRoute {
                    name: route_id,
                    balance: Some("weighted".into()),
                    target_provider: provider_id.clone(),
                    target_model: provider_model_id.clone(),
                    targets: vec![CreateTarget {
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
            .map(|target| UpsertTarget {
                id: Some(target.id.clone()),
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                weight: Some(target.weight),
                priority: Some(target.priority),
                thinking_level_map: target.thinking_level_map.0.clone(),
            })
            .collect::<Vec<_>>();
        targets.push(UpsertTarget {
            id: None,
            provider_id,
            model: provider_model_id,
            weight: Some(weight),
            priority: Some(priority),
            thinking_level_map: Vec::new(),
        });
        self.change(
            &existing.name,
            UpdateRoute {
                targets: Some(targets),
                ..UpdateRoute::default()
            },
        )
        .await
    }

    pub(crate) async fn unbind(&self, input: RouteUnbind) -> anyhow::Result<Option<Route>> {
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
                target.provider_id != input.provider_id || target.model != provider_model_id
            })
            .map(|target| UpsertTarget {
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
            self.delete(&route.name).await?;
            return Ok(None);
        }
        self.change(
            &route.name,
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
    pub async fn bind_route(&self, input: BindRouteInput) -> anyhow::Result<Route> {
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

    pub async fn unbind_route(&self, input: UnbindRouteInput) -> anyhow::Result<Option<Route>> {
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
    ) -> anyhow::Result<Route> {
        RouteModule::new(self)
            .reset_thinking_mapping(route_id, target_id, level)
            .await
    }

    pub async fn regenerate_target_thinking_map(
        &self,
        route_id: &str,
        target_id: &str,
    ) -> anyhow::Result<Route> {
        RouteModule::new(self)
            .regenerate_thinking_map(route_id, target_id)
            .await
    }
}

fn route_targets_for_update(route: &Route) -> Vec<UpsertTarget> {
    route
        .targets
        .iter()
        .map(|target| UpsertTarget {
            id: Some(target.id.clone()),
            provider_id: target.provider_id.clone(),
            model: target.model.clone(),
            weight: Some(target.weight),
            priority: Some(target.priority),
            thinking_level_map: target.thinking_level_map.0.clone(),
        })
        .collect()
}

fn create_backend_from_upsert(target: &UpsertTarget) -> CreateTarget {
    CreateTarget {
        provider_id: target.provider_id.clone(),
        model: target.model.clone(),
        weight: target.weight,
        priority: target.priority,
        thinking_level_map: target.thinking_level_map.clone(),
    }
}

#[cfg(test)]
mod tests;
