use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::Principal;

use serde_json::Value;

use super::{
    MediaKind, MediaUnderstandingError, MediaUnderstandingInput, MediaUnderstandingResult,
    MediaUnderstandingService,
};

/// 目标是否能原生接收图片输入；只描述输入，不代表适合承担理解。
pub fn model_is_image_capable(model: &crate::host::MediaRoute) -> bool {
    model.is_enabled
        && !model.targets.is_empty()
        && model
            .targets
            .iter()
            .all(|target| supports_image(&target.input_modalities))
}

pub fn supports_image(input_modalities: &[String]) -> bool {
    supports_modality(input_modalities, "image")
}

pub fn supports_audio(input_modalities: &[String]) -> bool {
    supports_modality(input_modalities, "audio")
}

fn supports_modality(input_modalities: &[String], modality: &str) -> bool {
    input_modalities
        .iter()
        .any(|declared| declared.eq_ignore_ascii_case(modality))
}

/// 能否绑定为某类理解模型：已启用，Route 声明文本输出，且每个 Target 都接受该模态的输入。
pub fn model_is_eligible(model: &crate::host::MediaRoute, kind: MediaKind) -> bool {
    model.is_enabled
        && model.text_output
        && !model.targets.is_empty()
        && model.targets.iter().all(|target| match kind {
            MediaKind::Image => supports_image(&target.input_modalities),
            MediaKind::Audio => supports_audio(&target.input_modalities),
        })
}

pub fn output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "pattern": "^stravia://turns/[a-z]{28}$" },
            "completion": { "type": "string", "enum": ["complete", "partial"] },
            "artifacts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "path": { "type": "string", "pattern": "^stravia://artifacts/[a-z]{55}$" } },
                    "required": ["path"],
                    "additionalProperties": false
                }
            },
            "report": super::definition::media_report_schema(),
            "pagination": stravia_web_access::read_path::pagination_schema()
        },
        "required": ["path", "completion", "artifacts", "report"],
        "additionalProperties": false
    })
}

pub async fn is_available(
    gateway: &crate::host::MediaRuntime,
    principal: &Principal,
    kind: MediaKind,
) -> bool {
    let service = gateway.host.service().await;
    let Some(service) = service else {
        return false;
    };
    let Some(model_id) = service.model_id(kind).await else {
        return false;
    };
    let model = gateway.host.active_route(&model_id).await;
    let Some(model) = model else {
        return false;
    };
    if !model_is_eligible(&model, kind) {
        return false;
    }
    gateway.host.authorize_capability(principal).await
}

/// 任一模态可用即视为多模态理解入口可见；具体调用再按所需模态单独判定。
pub async fn is_any_available(gateway: &crate::host::MediaRuntime, principal: &Principal) -> bool {
    for kind in MediaKind::ALL {
        if is_available(gateway, principal, kind).await {
            return true;
        }
    }
    false
}

pub async fn execute_input(
    gateway: &crate::host::MediaRuntime,
    input: MediaUnderstandingInput,
    principal: Principal,
    cancellation: CancellationToken,
    deadline: std::time::Instant,
) -> Result<MediaUnderstandingResult, MediaUnderstandingError> {
    if !is_any_available(gateway, &principal).await {
        return Err(unavailable_error());
    }
    let service: MediaUnderstandingService =
        gateway.host.service().await.ok_or_else(unavailable_error)?;
    let kind = service.resolve_kind(&principal, &input).await?;
    if !is_available(gateway, &principal, kind).await {
        return Err(unavailable_error());
    }
    service
        .execute_until(principal, kind, input, cancellation, deadline)
        .await
}

fn unavailable_error() -> MediaUnderstandingError {
    MediaUnderstandingError::new(
        "media_understanding_unavailable",
        "Media Understanding is unavailable",
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_modality_metadata_is_not_treated_as_image_support() {
        assert!(!supports_image(&[]));
        assert!(!supports_image(&["text".into()]));
    }
}
