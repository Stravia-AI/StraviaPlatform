use super::{
    PlatformLlmConfig, PlatformLlmConfigView, PlatformLlmError, PlatformLlmRouteSummary,
    PlatformLlmStatus, SETTINGS_KEY, UpdatePlatformLlmConfig,
};
use crate::Gateway;
use crate::db::models::{RouteConfig, RoutePurpose};

fn normalize_route_id(route_id: Option<String>) -> Option<String> {
    route_id.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

pub(crate) fn is_eligible_route(route: &RouteConfig) -> bool {
    route.is_enabled
        && route.purpose == Some(RoutePurpose::Inference)
        && route.targets.iter().any(|target| target.enabled)
}

fn route_summary(route: &RouteConfig) -> PlatformLlmRouteSummary {
    PlatformLlmRouteSummary {
        id: route.id.clone().into(),
        model_id: route.model_id.clone().into(),
        display_name: route.effective_display_name().to_string(),
    }
}

pub async fn load(gateway: &Gateway) -> Result<PlatformLlmConfig, PlatformLlmError> {
    let raw = gateway.storage.settings().get(SETTINGS_KEY).await?;
    let Some(raw) = raw else {
        return Ok(PlatformLlmConfig::default());
    };
    let mut config: PlatformLlmConfig = serde_json::from_str(&raw).map_err(|error| {
        PlatformLlmError::new(
            "platform_llm_config_invalid",
            format!("Invalid platform LLM configuration: {error}"),
        )
    })?;
    config.route_id = normalize_route_id(config.route_id);
    Ok(config)
}

pub async fn eligible_routes(
    gateway: &Gateway,
) -> Result<Vec<PlatformLlmRouteSummary>, PlatformLlmError> {
    let mut routes: Vec<PlatformLlmRouteSummary> = gateway
        .storage
        .routes()
        .list()
        .await?
        .into_iter()
        .filter(is_eligible_route)
        .map(|route| route_summary(&route))
        .collect();
    routes.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
            .then_with(|| left.model_id.cmp(&right.model_id))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(routes)
}

pub async fn view(
    gateway: &Gateway,
    config: PlatformLlmConfig,
) -> Result<PlatformLlmConfigView, PlatformLlmError> {
    let eligible = eligible_routes(gateway).await?;
    let route_id = normalize_route_id(config.route_id);
    let Some(bound_id) = route_id else {
        return Ok(PlatformLlmConfigView {
            route_id: None,
            status: PlatformLlmStatus::Unset,
            route: None,
            eligible_routes: eligible,
        });
    };
    if let Some(found) = eligible.iter().find(|candidate| candidate.id == bound_id) {
        return Ok(PlatformLlmConfigView {
            route_id: Some(bound_id),
            status: PlatformLlmStatus::Available,
            route: Some(found.clone()),
            eligible_routes: eligible,
        });
    }
    Ok(PlatformLlmConfigView {
        route_id: Some(bound_id),
        status: PlatformLlmStatus::Unavailable,
        route: None,
        eligible_routes: eligible,
    })
}

pub async fn save(
    gateway: &Gateway,
    update: UpdatePlatformLlmConfig,
) -> Result<PlatformLlmConfigView, PlatformLlmError> {
    let route_id = normalize_route_id(update.route_id);
    if let Some(id) = route_id.as_deref() {
        let eligible = eligible_routes(gateway).await?;
        if !eligible.iter().any(|candidate| candidate.id == id) {
            return Err(PlatformLlmError::new(
                "platform_llm_route_invalid",
                "Select an enabled inference Route with at least one enabled Target",
            ));
        }
    }
    let config = PlatformLlmConfig { route_id };
    let serialized = serde_json::to_string(&config).map_err(anyhow::Error::from)?;
    gateway
        .storage
        .settings()
        .set(SETTINGS_KEY, &serialized)
        .await?;
    crate::storage::bump_config_epoch(gateway.storage.settings()).await?;
    view(gateway, config).await
}

pub async fn resolve_active_route(gateway: &Gateway) -> Result<RouteConfig, PlatformLlmError> {
    let config = load(gateway).await?;
    let Some(route_id) = config.route_id else {
        return Err(PlatformLlmError::new(
            "platform_llm_not_configured",
            "No platform task model is configured",
        ));
    };
    let route = {
        let cache = gateway.model_cache.read().await;
        cache
            .models
            .iter()
            .find(|route| route.id.as_str() == route_id.as_str())
            .cloned()
    };
    let Some(route) = route else {
        return Err(PlatformLlmError::new(
            "platform_llm_route_unavailable",
            "Configured platform task model is unavailable",
        ));
    };
    if !is_eligible_route(&route) {
        return Err(PlatformLlmError::new(
            "platform_llm_route_unavailable",
            "Configured platform task model is unavailable",
        ));
    }
    Ok(route)
}
