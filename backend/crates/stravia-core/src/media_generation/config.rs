use serde::{Deserialize, Serialize};

use super::GenerationError;
use crate::{Gateway, db::models::Route};

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
) -> Result<(), GenerationError> {
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
    validate_targets(gateway, &route).await
}

async fn validate_targets(gateway: &Gateway, route: &Route) -> Result<(), GenerationError> {
    if !route.is_enabled {
        return Err(GenerationError::new(
            "media_generation_route_disabled",
            "Enable the selected image generation Route",
        ));
    }
    if !route.targets.iter().any(|target| target.enabled) {
        return Err(GenerationError::new(
            "media_generation_targets_missing",
            "The selected Route needs an enabled Codex Target",
        ));
    }
    for target in &route.targets {
        if !target.enabled {
            continue;
        }
        let provider = gateway
            .storage
            .providers()
            .get(&target.provider_id)
            .await?
            .ok_or_else(|| {
                GenerationError::new(
                    "media_generation_provider_missing",
                    "A Target's model service no longer exists",
                )
            })?;
        if !crate::provider::openai::codex::media_generation::eligible(&provider, &target.model) {
            return Err(GenerationError::new(
                "media_generation_target_incompatible",
                format!(
                    "Target {} requires a supported Codex model and enabled Codex OAuth service",
                    target.id
                ),
            ));
        }
        let credential = gateway
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?;
        if !credential.is_some_and(|credential| {
            credential.status == "connected"
                && !credential.access_token.trim().is_empty()
                && (credential.expires_at.as_deref().is_none_or(|expiry| {
                    crate::proxy::security::is_key_expired(expiry) == Ok(false)
                }) || credential
                    .refresh_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty()))
        }) {
            return Err(GenerationError::new(
                "media_generation_oauth_unavailable",
                format!("Reconnect the Codex account for {}", provider.name),
            ));
        }
    }
    Ok(())
}

async fn validate(
    gateway: &Gateway,
    config: &MediaGenerationConfig,
) -> Result<(), GenerationError> {
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
        Ok(()) => MediaGenerationValidation {
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
                id: route.model_id,
                name: route.display_name,
            }),
            Err(error) if error.code == "media_generation_unavailable" => return Err(error),
            Err(_) => {}
        }
    }
    Ok(result)
}

pub(crate) async fn validated_route(gateway: &Gateway) -> Result<String, GenerationError> {
    let config = load(gateway).await?;
    if !config.enabled {
        return Err(GenerationError::new(
            "media_generation_disabled",
            "Media generation is disabled",
        ));
    }
    validate(gateway, &config).await?;
    Ok(config.image.route_id.expect("validated image binding"))
}
