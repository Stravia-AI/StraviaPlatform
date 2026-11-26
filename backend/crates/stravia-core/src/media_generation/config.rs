use serde::{Deserialize, Serialize};

use super::GenerationError;
use crate::{Gateway, db::models::RouteConfig};

pub(crate) const SETTINGS_KEY: &str = "media_generation_config";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaGenerationConfig {
    pub enabled: bool,
    pub image: ImageGenerationConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageGenerationConfig {
    pub route_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaGenerationValidation {
    pub valid: bool,
    pub code: Option<&'static str>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaGenerationConfigView {
    pub config: MediaGenerationConfig,
    pub validation: MediaGenerationValidation,
}

#[derive(Clone, Debug, Serialize)]
pub struct EligibleGenerationRoute {
    pub id: String,
    pub name: Option<String>,
}

pub(crate) async fn load(gateway: &Gateway) -> Result<MediaGenerationConfig, GenerationError> {
    gateway
        .storage
        .settings()
        .get(SETTINGS_KEY)
        .await?
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map(|config| config.unwrap_or_default())
        .map_err(|_| {
            GenerationError::new(
                "media_generation_config_invalid",
                "Media generation configuration could not be read",
            )
        })
}

pub(crate) async fn validate_route(
    gateway: &Gateway,
    route_id: &str,
) -> Result<RouteConfig, GenerationError> {
    let route = gateway
        .storage
        .routes()
        .get(route_id)
        .await?
        .ok_or_else(|| {
            GenerationError::new(
                "media_generation_route_missing",
                "Select an existing image generation Route",
            )
        })?;
    validate_targets(gateway, &route).await?;
    Ok(route)
}

async fn validate_targets(gateway: &Gateway, route: &RouteConfig) -> Result<(), GenerationError> {
    match route.purpose {
        Some(crate::db::models::RoutePurpose::ImageGeneration) => {}
        Some(
            crate::db::models::RoutePurpose::Inference | crate::db::models::RoutePurpose::WebSearch,
        ) => {
            return Err(GenerationError::new(
                "media_generation_route_purpose_mismatch",
                "Select a Route configured for image output",
            ));
        }
        None => {
            return Err(GenerationError::new(
                "route_purpose_unconfigured",
                "Select and save image output to reconfigure this Route",
            ));
        }
    }
    if !route.is_enabled {
        return Err(GenerationError::new(
            "media_generation_route_disabled",
            "Enable the selected image generation Route",
        ));
    }
    if !route.targets.iter().any(|target| target.enabled) {
        return Err(GenerationError::new(
            "media_generation_targets_missing",
            "The selected Route needs an enabled Target",
        ));
    }
    let mut matching = false;
    for target in route.targets.iter().filter(|target| target.enabled) {
        if gateway
            .storage
            .provider_models()
            .find(target.provider_id().as_str(), target.model().as_str())
            .await?
            .is_some_and(|model| {
                model
                    .metadata
                    .supports_route_purpose(crate::db::models::RoutePurpose::ImageGeneration)
            })
        {
            matching = true;
            break;
        }
    }
    if !matching {
        return Err(GenerationError::new(
            "media_generation_targets_purpose_mismatch",
            "The selected Route has no enabled image-output Target; repair the registered model output",
        ));
    }
    gateway
        .validate_vendor_route_capability(route, stravia_vendor_sdk::Capability::MediaImage)
        .await
        .map_err(|_| {
            GenerationError::new(
                "media_generation_target_incompatible",
                "Every enabled Target must select an image-capable Provider Model",
            )
        })
}

async fn validate(
    gateway: &Gateway,
    config: &MediaGenerationConfig,
) -> Result<RouteConfig, GenerationError> {
    let route_id = config
        .image
        .route_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            GenerationError::new(
                "media_generation_route_missing",
                "Select an image generation Route",
            )
        })?;
    validate_route(gateway, route_id).await
}

pub(crate) async fn view(
    gateway: &Gateway,
    config: MediaGenerationConfig,
) -> Result<MediaGenerationConfigView, GenerationError> {
    let validation = match validate(gateway, &config).await {
        Ok(_) => MediaGenerationValidation {
            valid: true,
            code: None,
            message: None,
        },
        Err(error) if error.code == "media_generation_unavailable" => return Err(error),
        Err(error) => MediaGenerationValidation {
            valid: false,
            code: Some(error.code),
            message: Some(error.message),
        },
    };
    Ok(MediaGenerationConfigView { config, validation })
}

pub(crate) async fn save(
    gateway: &Gateway,
    config: MediaGenerationConfig,
) -> Result<MediaGenerationConfigView, GenerationError> {
    let _mutation = gateway.route_media_mutation.lock().await;
    // 配置失效不能阻止关闭能力；保留原绑定供管理员修复，不接受新无效绑定。
    let disabling = !config.enabled && load(gateway).await?.image.route_id == config.image.route_id;
    if !disabling && (config.enabled || config.image.route_id.is_some()) {
        validate(gateway, &config).await?;
    }
    let value = serde_json::to_string(&config).map_err(anyhow::Error::from)?;
    gateway.storage.settings().set(SETTINGS_KEY, &value).await?;
    view(gateway, config).await
}

pub(crate) async fn eligible_routes(
    gateway: &Gateway,
) -> Result<Vec<EligibleGenerationRoute>, GenerationError> {
    let mut result = Vec::new();
    for route in gateway.storage.routes().list().await? {
        match validate_targets(gateway, &route).await {
            Ok(()) => result.push(EligibleGenerationRoute {
                id: route.model_id.into(),
                name: route.display_name,
            }),
            Err(error) if error.code == "media_generation_unavailable" => return Err(error),
            Err(_) => {}
        }
    }
    Ok(result)
}

pub(crate) async fn validated_route(gateway: &Gateway) -> Result<RouteConfig, GenerationError> {
    let config = load(gateway).await?;
    if !config.enabled {
        return Err(GenerationError::new(
            "media_generation_disabled",
            "Media generation is disabled",
        ));
    }
    validate(gateway, &config).await
}
