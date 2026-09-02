use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::hook::{
    PlatformTool, PlatformToolError, PlatformToolOutput, Principal, ToolExecutionContext, ToolId,
};
use crate::protocol::ir::ContentBlock;
use crate::proxy::context::CancellationToken;
use crate::proxy::security::Security;

use super::{MediaUnderstandingInput, MediaUnderstandingService};

pub(crate) const MEDIA_TOOL_ID: &str = "media-understanding";
const MEDIA_TOOL_DESCRIPTION: &str = "Understand static JPEG, PNG, or WebP Artifacts using OCR, description, comparison, or visual reasoning.";
pub(crate) const MEDIA_TOOL_NAME: &str = "understand_media";

pub(crate) async fn model_is_image_capable(
    gateway: &crate::Gateway,
    model: &crate::db::models::Route,
) -> bool {
    if !model.is_enabled {
        return false;
    }
    let targets = model.targets.clone();
    if targets.is_empty() {
        return false;
    }
    for target in targets {
        let actual_model = if target.model.is_empty() || target.model == "*" {
            model.model_id.as_str()
        } else {
            target.model.as_str()
        };
        let metadata = gateway
            .storage
            .provider_models()
            .get(&target.provider_id, actual_model)
            .await
            .ok()
            .flatten()
            .map(|record| record.metadata);
        if !metadata.as_ref().is_some_and(supports_image) {
            return false;
        }
    }
    true
}

pub(crate) fn supports_image(metadata: &crate::provider_models::ProviderModelMetadata) -> bool {
    metadata.modalities.as_ref().is_some_and(|modalities| {
        modalities
            .input
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("image"))
    })
}

pub(crate) fn tools(gateway: &crate::Gateway) -> Vec<Arc<dyn PlatformTool>> {
    vec![Arc::new(MediaUnderstandingPlatformTool {
        gateway: gateway.clone(),
    })]
}

pub(crate) fn input_schema() -> Value {
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
                "description": "New static JPEG, PNG, or WebP source Artifacts in stable order. Use [] when continuing previous_turn_id without new media. Never repeat Artifact IDs from previous turns.",
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

pub(crate) fn output_schema() -> Value {
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

pub(crate) async fn is_available(gateway: &crate::Gateway, principal: &Principal) -> bool {
    let service = gateway.media_understanding.read().await.clone();
    let Some(service) = service else {
        return false;
    };
    let Some(model_id) = service.model_id().await else {
        return false;
    };
    let model = gateway
        .storage
        .routes()
        .list_active()
        .await
        .ok()
        .and_then(|routes| routes.into_iter().find(|route| route.id == model_id));
    let Some(model) = model else {
        return false;
    };
    if !model_is_image_capable(gateway, &model).await {
        return false;
    }
    Security::new(gateway.storage.auth())
        .authorize_principal_capability(principal)
        .await
        .is_ok()
}

pub(crate) async fn execute_until(
    gateway: &crate::Gateway,
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
                "message": "Invalid understand_media arguments"
            }
        })
    })?;
    let service: MediaUnderstandingService = gateway
        .media_understanding
        .read()
        .await
        .clone()
        .ok_or_else(unavailable_error)?;
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
    gateway: &crate::Gateway,
    arguments: Value,
    context: &ToolExecutionContext,
) -> Result<Value, Value> {
    let input: MediaUnderstandingInput =
        serde_json::from_value(arguments.clone()).map_err(|_| {
            serde_json::json!({
                "error": {
                    "code": "invalid_input",
                    "message": "Invalid understand_media arguments"
                }
            })
        })?;
    if let Some(previous_turn_id) = input.previous_turn_id.as_ref()
        && !gateway.media_run_snapshots.permits_turn(
            &context.run_id,
            &context.principal,
            previous_turn_id,
        )
    {
        return Err(serde_json::json!({
            "error": {
                "code": "media_turn_unavailable",
                "message": "Previous Media Turn is unavailable to this Inference Run"
            }
        }));
    }
    let artifact_ids = input
        .artifacts
        .iter()
        .map(|artifact| artifact.artifact_id.clone())
        .collect::<Vec<_>>();
    if !artifact_ids.is_empty()
        && !gateway
            .media_run_snapshots
            .permits(&context.run_id, &context.principal, &artifact_ids)
    {
        return Err(serde_json::json!({
            "error": {
                "code": "media_artifact_unavailable",
                "message": "Media Artifact is unavailable to this Inference Run; continue a previous Media Turn with artifacts: [] when no new media was attached"
            }
        }));
    }
    let deadline = gateway
        .media_run_snapshots
        .deadline(&context.run_id, &context.principal)
        .ok_or_else(|| {
            serde_json::json!({
                "error": {
                    "code": "media_artifact_unavailable",
                    "message": "Media Artifact is unavailable to this Inference Run"
                }
            })
        })?;
    let result = execute_until(
        gateway,
        arguments,
        context.principal.clone(),
        context.cancellation.clone(),
        deadline,
    )
    .await?;
    if let Some(turn_id) = result.get("turn_id").and_then(Value::as_str) {
        gateway.media_run_snapshots.allow_turn(
            &context.run_id,
            &context.principal,
            crate::agent::AgentTurnId::new(turn_id),
            deadline,
        );
    }
    Ok(result)
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
    gateway: crate::Gateway,
}

#[async_trait]
impl PlatformTool for MediaUnderstandingPlatformTool {
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
    fn media_tool_schema_is_strict_and_media_specific() {
        let schema = input_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["artifacts"]["maxItems"], 8);
        assert_eq!(
            schema["properties"]["artifacts"]["default"],
            serde_json::json!([])
        );
        assert_eq!(
            schema["properties"]["artifacts"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(schema["required"], serde_json::json!(["prompt"]));
        assert_eq!(
            schema["anyOf"][0]["required"],
            serde_json::json!(["previous_turn_id"])
        );
        assert_eq!(schema["anyOf"][1]["properties"]["artifacts"]["minItems"], 1);
        let continuation: MediaUnderstandingInput = serde_json::from_value(serde_json::json!({
            "prompt": "Continue the prior analysis",
            "previous_turn_id": "aturn_parent"
        }))
        .expect("continuation input may omit new Artifacts");
        assert!(continuation.artifacts.is_empty());
        assert!(MEDIA_TOOL_DESCRIPTION.contains("JPEG"));
    }

    #[test]
    fn missing_modality_metadata_is_not_treated_as_image_support() {
        assert!(!supports_image(
            &crate::provider_models::ProviderModelMetadata::default()
        ));
        let text_only = crate::provider_models::ProviderModelMetadata::from_value(
            "text-only",
            serde_json::json!({
                "id": "text-only",
                "modalities": { "input": ["text"], "output": ["text"] }
            }),
        )
        .expect("text-only Provider Model metadata");
        assert!(!supports_image(&text_only));
    }
}
