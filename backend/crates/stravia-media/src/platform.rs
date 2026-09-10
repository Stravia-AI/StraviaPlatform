use std::sync::Arc;
use stravia_runtime_contract::Principal;

use async_trait::async_trait;
use serde_json::Value;

use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::hook::{
    PlatformTool, PlatformToolError, PlatformToolOutput, ToolExecutionContext, ToolId,
};
use stravia_runtime_contract::protocol::ir::ContentBlock;

use super::{MediaUnderstandingInput, MediaUnderstandingService};

pub const MEDIA_TOOL_ID: &str = "media-understanding";
pub const MEDIA_TOOL_DESCRIPTION: &str = "Understand static JPEG, PNG, or WebP Artifacts using OCR, description, comparison, or visual reasoning.";
pub const MEDIA_TOOL_NAME: &str = "StraviaRead";

pub fn model_is_image_capable(model: &crate::host::MediaRoute) -> bool {
    model.is_enabled
        && !model.targets.is_empty()
        && model
            .targets
            .iter()
            .all(|target| supports_image(&target.input_modalities))
}

pub fn supports_image(input_modalities: &[String]) -> bool {
    input_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"))
}

pub fn tools(gateway: &crate::host::MediaRuntime) -> Vec<Arc<dyn PlatformTool>> {
    vec![Arc::new(MediaUnderstandingPlatformTool {
        gateway: gateway.clone(),
    })]
}

pub fn input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "minLength": 1,
                "description": "The OCR, description, comparison, or visual reasoning task. UTF-8 encoding must not exceed 64 KiB."
            },
            "artifacts": {
                "type": "array",
                "maxItems": 8,
                "default": [],
                "description": "Static JPEG, PNG, or WebP source Artifacts in stable order. Retained ancestor sources are reused when continuing previous_turn_id; duplicate IDs within one call are rejected.",
                "items": {
                    "type": "object",
                    "properties": {
                        "artifact_id": { "type": "string", "minLength": 1 }
                    },
                    "required": ["artifact_id"],
                    "additionalProperties": false
                }
            },
            "previous_turn_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 128,
                "description": "An explicit prior Media Understanding Turn to continue or branch from."
            }
        },
        "required": ["prompt"],
        "anyOf": [
            {
                "required": ["previous_turn_id"]
            },
            {
                "properties": {
                    "artifacts": {
                        "minItems": 1
                    }
                },
                "required": ["artifacts"]
            }
        ],
        "additionalProperties": false
    })
}

pub fn output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "turn_id": { "type": "string" },
            "completion": { "type": "string", "enum": ["complete", "partial"] },
            "report": super::definition::media_report_schema()
        },
        "required": ["turn_id", "completion", "report"],
        "additionalProperties": false
    })
}

pub async fn is_available(gateway: &crate::host::MediaRuntime, principal: &Principal) -> bool {
    let service = gateway.host.service().await;
    let Some(service) = service else {
        return false;
    };
    let Some(model_id) = service.model_id().await else {
        return false;
    };
    let model = gateway.host.active_route(&model_id).await;
    let Some(model) = model else {
        return false;
    };
    if !model_is_image_capable(&model) {
        return false;
    }
    gateway.host.authorize_capability(principal).await
}

pub async fn execute_until(
    gateway: &crate::host::MediaRuntime,
    arguments: Value,
    principal: Principal,
    cancellation: CancellationToken,
    deadline: std::time::Instant,
) -> Result<Value, Value> {
    if !is_available(gateway, &principal).await {
        return Err(unavailable_error());
    }
    let input: MediaUnderstandingInput = serde_json::from_value(arguments).map_err(|_| {
        serde_json::json!({
            "error": {
                "code": "invalid_input",
                "message": "Invalid Media Understanding arguments"
            }
        })
    })?;
    let service: MediaUnderstandingService =
        gateway.host.service().await.ok_or_else(unavailable_error)?;
    let result = service
        .execute_until(principal, input, cancellation, deadline)
        .await
        .map_err(|error| serde_json::json!({ "error": error }))?;
    serde_json::to_value(result).map_err(|_| {
        serde_json::json!({
            "error": {
                "code": "result_encoding_failed",
                "message": "Media Understanding result could not be encoded"
            }
        })
    })
}

async fn execute_platform(
    gateway: &crate::host::MediaRuntime,
    arguments: Value,
    context: &ToolExecutionContext,
) -> Result<Value, Value> {
    execute_until(
        gateway,
        arguments,
        context.principal.clone(),
        context.cancellation.clone(),
        std::time::Instant::now() + crate::MEDIA_TOTAL_WALL_TIME,
    )
    .await
}

fn unavailable_error() -> Value {
    serde_json::json!({
        "error": {
            "code": "media_understanding_unavailable",
            "message": "Media Understanding is unavailable"
        }
    })
}

struct MediaUnderstandingPlatformTool {
    gateway: crate::host::MediaRuntime,
}

#[async_trait]
impl PlatformTool for MediaUnderstandingPlatformTool {
    fn read_domain(&self) -> Option<stravia_runtime_contract::hook::StraviaReadDomain> {
        Some(stravia_runtime_contract::hook::StraviaReadDomain::Media)
    }
    fn id(&self) -> ToolId {
        ToolId::new(MEDIA_TOOL_ID)
    }

    fn external_name(&self) -> &str {
        MEDIA_TOOL_NAME
    }

    fn description(&self) -> Option<&str> {
        Some(MEDIA_TOOL_DESCRIPTION)
    }

    fn parameters(&self) -> Value {
        input_schema()
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        execute_platform(&self.gateway, arguments, &context)
            .await
            .map_err(|error| PlatformToolError::new(error.to_string()))
    }

    async fn execute_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        match execute_platform(&self.gateway, arguments, &context).await {
            Ok(result) => {
                let mut metadata = serde_json::Map::new();
                metadata.insert("stravia_media".into(), result.clone());
                Ok(PlatformToolOutput {
                    content: vec![ContentBlock::Unknown { raw: result }],
                    is_error: false,
                    metadata,
                })
            }
            Err(error) => Ok(PlatformToolOutput {
                content: vec![ContentBlock::Unknown { raw: error }],
                is_error: true,
                metadata: serde_json::Map::new(),
            }),
        }
    }
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
