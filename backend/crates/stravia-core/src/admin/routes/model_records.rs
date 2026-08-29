use super::*;
use std::collections::{BTreeMap, BTreeSet};

type ModelTokenLimits = (Option<u64>, Option<u64>);

impl AdminService {
    // ── Models ──

    pub async fn list_models(&self) -> anyhow::Result<Vec<Model>> {
        let mut models = self.gw.storage.models().list().await?;
        if let Some(store) = self.gw.storage.model_backends() {
            for model in &mut models {
                model.targets = store.list_backends_by_model(&model.id).await?;
            }
        }
        for model in &mut models {
            model.refresh_supported_thinking_levels();
        }
        self.refresh_model_token_limits(&mut models).await?;
        Ok(models)
    }

    pub async fn create_model(&self, input: CreateModel) -> anyhow::Result<Model> {
        super::RouteModule::new(self).create(input).await
    }

    pub(super) async fn create_model_record(&self, input: CreateModel) -> anyhow::Result<Model> {
        let route_id = normalize_name(&input.name, "model ID sent by clients")?;
        self.ensure_route_id_unique(None, &route_id).await?;
        let balance = normalize_model_balance(input.balance.as_deref())?;
        let backends = normalize_create_model_backends(&input)?;
        ensure_model_backends_valid(&backends)?;
        let primary_backend = backends
            .first()
            .ok_or_else(|| anyhow::anyhow!("at least one model backend is required"))?;

        let model = self
            .gw
            .storage
            .models()
            .create(CreateModel {
                name: route_id,
                balance: Some(balance),
                target_provider: primary_backend.provider_id.clone(),
                target_model: primary_backend.model.clone(),
                targets: vec![],
            })
            .await?;
        if let Some(store) = self.gw.storage.model_backends() {
            store.set_backends(&model.id, &backends).await?;
        }
        self.reload_model_cache().await?;
        self.bump_config_epoch().await?;
        self.get_model_by_id(&model.id).await
    }

    pub async fn update_model(&self, id: &str, input: UpdateModel) -> anyhow::Result<Model> {
        super::RouteModule::new(self).change(id, input).await
    }

    pub(super) async fn update_model_record(
        &self,
        id: &str,
        input: UpdateModel,
    ) -> anyhow::Result<Model> {
        let current = self.get_model_by_id(id).await?;

        let route_id = normalize_name(
            &input.name.clone().unwrap_or_else(|| current.name.clone()),
            "model ID sent by clients",
        )?;
        self.ensure_route_id_unique(Some(id), &route_id).await?;
        let balance = normalize_model_balance(input.balance.as_deref().or(Some(&current.balance)))?;
        let backends = normalize_update_model_backends(&current, &input)?;
        ensure_model_backends_valid(&backends)?;
        let primary_backend = backends
            .first()
            .ok_or_else(|| anyhow::anyhow!("at least one model backend is required"))?;
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);

        self.gw
            .storage
            .models()
            .update(
                id,
                UpdateModel {
                    name: Some(route_id),
                    balance: Some(balance),
                    target_provider: Some(primary_backend.provider_id.clone()),
                    target_model: Some(primary_backend.model.clone()),
                    targets: None,
                    is_enabled: Some(is_enabled),
                },
            )
            .await?;
        if let Some(store) = self.gw.storage.model_backends() {
            store.set_backends(id, &backends).await?;
        }
        self.reload_model_cache().await?;
        self.bump_config_epoch().await?;
        self.get_model_by_id(id).await
    }

    pub async fn delete_model(&self, id: &str) -> anyhow::Result<()> {
        super::RouteModule::new(self).delete(id).await
    }

    pub(super) async fn delete_model_record(&self, id: &str) -> anyhow::Result<()> {
        if let Some(store) = self.gw.storage.model_backends() {
            store.delete_backends_by_model(id).await?;
        }
        self.gw.storage.models().delete(id).await?;
        self.reload_model_cache().await?;
        self.bump_config_epoch().await?;
        Ok(())
    }
    async fn ensure_route_id_unique(
        &self,
        exclude_id: Option<&str>,
        route_id: &str,
    ) -> anyhow::Result<()> {
        if self
            .gw
            .storage
            .models()
            .exists_by_name(route_id, exclude_id)
            .await?
        {
            return Err(coded_error(
                "ROUTE_ID_CONFLICT",
                &format!("The model ID clients send already exists: {route_id}"),
                serde_json::json!({ "route_id": route_id }),
            ));
        }
        Ok(())
    }
    async fn get_model_by_id(&self, id: &str) -> anyhow::Result<Model> {
        let mut model = self
            .gw
            .storage
            .models()
            .get(id)
            .await?
            .context("model not found")?;
        if let Some(store) = self.gw.storage.model_backends() {
            model.targets = store.list_backends_by_model(&model.id).await?;
        }
        model.refresh_supported_thinking_levels();
        self.refresh_model_token_limits(std::slice::from_mut(&mut model))
            .await?;
        Ok(model)
    }

    async fn refresh_model_token_limits(&self, models: &mut [Model]) -> anyhow::Result<()> {
        let provider_ids = models
            .iter()
            .flat_map(|model| {
                model
                    .targets
                    .iter()
                    .map(|target| target.provider_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let mut limits_by_provider = BTreeMap::<String, BTreeMap<String, ModelTokenLimits>>::new();

        for provider_id in provider_ids {
            let provider_limits = self
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

        for model in models {
            model.context_window =
                common_target_limit(&model.targets, &limits_by_provider, |limits| limits.0);
            model.output_max_tokens =
                common_target_limit(&model.targets, &limits_by_provider, |limits| limits.1);
        }
        Ok(())
    }

    pub(crate) async fn reload_model_cache(&self) -> anyhow::Result<()> {
        self.gw
            .model_cache
            .write()
            .await
            .reload(
                self.gw.storage.snapshots(),
                self.gw.storage.model_backends(),
            )
            .await
    }
}

fn common_target_limit(
    targets: &[ModelBackend],
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
