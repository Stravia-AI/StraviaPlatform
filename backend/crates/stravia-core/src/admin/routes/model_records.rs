use super::*;
use std::collections::{BTreeMap, BTreeSet};

type ModelTokenLimits = (Option<u64>, Option<u64>);

impl AdminService {
    pub async fn list_models(&self) -> anyhow::Result<Vec<Route>> {
        RouteModule::new(self).list().await
    }

    pub async fn get_model(&self, route_id: &str) -> anyhow::Result<Route> {
        RouteModule::new(self).get(route_id).await
    }

    pub async fn create_model(&self, input: CreateRoute) -> anyhow::Result<Route> {
        RouteModule::new(self).create(input).await
    }

    pub async fn update_model(&self, route_id: &str, input: UpdateRoute) -> anyhow::Result<Route> {
        RouteModule::new(self).change(route_id, input).await
    }

    pub async fn delete_model(&self, route_id: &str) -> anyhow::Result<()> {
        RouteModule::new(self).delete(route_id).await
    }
}

impl RouteModule<'_> {
    pub(crate) async fn list(&self) -> anyhow::Result<Vec<Route>> {
        let mut routes = self.admin.gw.storage.routes().list().await?;
        self.refresh_route_token_limits(&mut routes).await?;
        Ok(routes)
    }

    pub(crate) async fn get(&self, route_id: &str) -> anyhow::Result<Route> {
        let route_id = normalize_name(route_id, "model ID sent by clients")?;
        let mut route = self
            .admin
            .gw
            .storage
            .routes()
            .get(&route_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Route not found: {route_id}"))?;
        self.refresh_route_token_limits(std::slice::from_mut(&mut route))
            .await?;
        Ok(route)
    }
    pub(super) async fn create_record(&self, input: CreateRoute) -> anyhow::Result<Route> {
        let route_id = normalize_name(&input.name, "model ID sent by clients")?;
        let selection_strategy = normalize_model_balance(input.balance.as_deref())?;
        let targets = normalize_create_route_targets(&input)?;
        ensure_route_targets_valid(&targets)?;
        let route = self
            .admin
            .gw
            .storage
            .routes()
            .put(PutRoute {
                id: None,
                route_id,
                selection_strategy,
                is_enabled: true,
                targets,
            })
            .await?;
        self.after_write().await?;
        self.get(&route.name).await
    }

    pub(super) async fn change_record(
        &self,
        route_id: &str,
        input: UpdateRoute,
    ) -> anyhow::Result<Route> {
        let current = self.get(route_id).await?;
        let next_route_id = normalize_name(
            input.name.as_deref().unwrap_or(&current.name),
            "model ID sent by clients",
        )?;
        let selection_strategy =
            normalize_model_balance(input.balance.as_deref().or(Some(&current.balance)))?;
        let targets = normalize_update_route_targets(&current, &input)?;
        ensure_route_targets_valid(&targets)?;
        let route = self
            .admin
            .gw
            .storage
            .routes()
            .put(PutRoute {
                id: Some(current.id),
                route_id: next_route_id,
                selection_strategy,
                is_enabled: input.is_enabled.unwrap_or(current.is_enabled),
                targets,
            })
            .await?;
        self.after_write().await?;
        self.get(&route.name).await
    }

    pub(super) async fn delete_record(&self, route_id: &str) -> anyhow::Result<()> {
        let route_id = normalize_name(route_id, "model ID sent by clients")?;
        self.admin.gw.storage.routes().delete(&route_id).await?;
        self.after_write().await
    }

    async fn after_write(&self) -> anyhow::Result<()> {
        self.reload_cache().await?;
        self.admin.bump_config_epoch().await?;
        Ok(())
    }

    pub(crate) async fn reload_cache(&self) -> anyhow::Result<()> {
        self.admin
            .gw
            .model_cache
            .write()
            .await
            .reload(self.admin.gw.storage.routes())
            .await
    }

    pub(super) async fn refresh_route_token_limits(
        &self,
        routes: &mut [Route],
    ) -> anyhow::Result<()> {
        let provider_ids = routes
            .iter()
            .flat_map(|route| {
                route
                    .targets
                    .iter()
                    .map(|target| target.provider_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let mut limits_by_provider = BTreeMap::<String, BTreeMap<String, ModelTokenLimits>>::new();

        for provider_id in provider_ids {
            let provider_limits = self
                .admin
                .gw
                .storage
                .provider_models()
                .list_for_provider(&provider_id)
                .await?
                .into_iter()
                .map(|record| {
                    let limits = record.metadata.limit.unwrap_or_default();
                    (record.model_id, (limits.context, limits.output))
                })
                .collect();
            limits_by_provider.insert(provider_id, provider_limits);
        }

        for route in routes {
            route.context_window =
                common_target_limit(&route.targets, &limits_by_provider, |limits| limits.0);
            route.output_max_tokens =
                common_target_limit(&route.targets, &limits_by_provider, |limits| limits.1);
        }
        Ok(())
    }
}

fn common_target_limit(
    targets: &[Target],
    limits_by_provider: &BTreeMap<String, BTreeMap<String, ModelTokenLimits>>,
    select: impl Fn(&ModelTokenLimits) -> Option<u64>,
) -> Option<u64> {
    let mut limits = targets.iter().map(|target| {
        limits_by_provider
            .get(&target.provider_id)
            .and_then(|models| models.get(&target.model))
            .and_then(&select)
            .filter(|limit| *limit > 0)
    });
    let first = limits.next()??;
    limits.try_fold(first, |common, limit| Some(common.min(limit?)))
}
