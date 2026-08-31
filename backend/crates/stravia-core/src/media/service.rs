use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;

use crate::agent::{AgentDefinitionId, AgentEvent, AgentInput, AgentRunner};
use crate::hook::Principal;
use crate::proxy::context::CancellationToken;

use super::store::MediaDerivativeStore;
use super::{
    MEDIA_DEFINITION_ID, MediaInputPreprocessor, MediaPreprocessError, MediaReport,
    MediaUnderstandingInput, MediaUnderstandingResult,
};

pub(crate) const MAX_MEDIA_PROMPT_BYTES: usize = 64 * 1024;
const DERIVATIVE_STAGING_RETENTION: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, serde::Serialize, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub(crate) struct MediaUnderstandingError {
    pub code: String,
    pub message: String,
}

impl MediaUnderstandingError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct MediaUnderstandingService {
    runner: AgentRunner,
    preprocessor: MediaInputPreprocessor,
}

impl MediaUnderstandingService {
    pub(crate) fn new(
        runner: AgentRunner,
        derivatives: std::sync::Arc<MediaDerivativeStore>,
    ) -> Self {
        Self {
            runner,
            preprocessor: MediaInputPreprocessor::new(derivatives, DERIVATIVE_STAGING_RETENTION),
        }
    }

    pub(crate) async fn model_id(&self) -> Option<String> {
        self.runner
            .definition_model_with_thinking_level(&AgentDefinitionId::new(MEDIA_DEFINITION_ID))
            .await
            .map(|(model_id, _)| model_id)
    }
    pub(crate) async fn prepare_sources(
        &self,
        principal: &Principal,
        source_ids: &[crate::agent::ArtifactId],
        cancellation: &CancellationToken,
        deadline: std::time::Instant,
    ) -> Result<(), MediaUnderstandingError> {
        self.preprocessor
            .preprocess_until(principal, source_ids, cancellation, deadline)
            .await
            .map(|_| ())
            .map_err(safe_preprocess_error)
    }

    pub(crate) async fn execute_until(
        &self,
        principal: Principal,
        input: MediaUnderstandingInput,
        cancellation: CancellationToken,
        deadline: std::time::Instant,
    ) -> Result<MediaUnderstandingResult, MediaUnderstandingError> {
        let started = std::time::Instant::now();
        let mut execution =
            Box::pin(self.execute_inner(principal, input, cancellation.clone(), deadline));
        let outcome = tokio::select! {
            biased;
            result = &mut execution => Ok(result),
            _ = cancellation.cancelled() => Err((
                "cancelled",
                "Media Understanding cancelled",
            )),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                cancellation.cancel();
                Err((
                    "deadline_exceeded",
                    "Media Understanding deadline exceeded",
                ))
            }
        };
        let result = match outcome {
            Ok(result) => result,
            Err((code, message)) => {
                cancellation.cancel();
                match execution.await {
                    Ok(result) => Ok(result),
                    Err(_) => Err(MediaUnderstandingError::new(code, message)),
                }
            }
        };
        match &result {
            Ok(result) => tracing::info!(
                media_completion = ?result.completion,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Media Understanding completed"
            ),
            Err(error) => tracing::warn!(
                error_code = error.code,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Media Understanding failed"
            ),
        }
        result
    }

    async fn execute_inner(
        &self,
        principal: Principal,
        input: MediaUnderstandingInput,
        cancellation: CancellationToken,
        deadline: std::time::Instant,
    ) -> Result<MediaUnderstandingResult, MediaUnderstandingError> {
        validate_input(&input)?;
        let definition_id = AgentDefinitionId::new(MEDIA_DEFINITION_ID);
        let ancestor_derivatives = if let Some(parent) = input.previous_turn_id.as_ref() {
            self.runner
                .parent_artifact_ids(&principal, parent, &definition_id)
                .await
                .map_err(safe_agent_error)?
        } else {
            Vec::new()
        };
        let source_ids = input
            .artifacts
            .iter()
            .map(|artifact| artifact.artifact_id.clone())
            .collect::<Vec<_>>();
        let prepared = self
            .preprocessor
            .preprocess_until(&principal, &source_ids, &cancellation, deadline)
            .await
            .map_err(safe_preprocess_error)?;
        tracing::info!(
            source_bytes = prepared.iter().map(|media| media.source.size).sum::<u64>(),
            derivative_bytes = prepared
                .iter()
                .map(|media| media.derivative.size)
                .sum::<u64>(),
            "Media preprocessing completed"
        );
        let ancestor_set = ancestor_derivatives.iter().collect::<HashSet<_>>();
        if prepared
            .iter()
            .any(|media| ancestor_set.contains(&media.derivative.id))
        {
            return Err(MediaUnderstandingError::new(
                "duplicate_media_artifact",
                "Media Artifact is already present in the ancestor chain",
            ));
        }
        let prompt = serde_json::json!({
            "task": input.prompt,
            "media": prepared.iter().enumerate().map(|(index, media)| serde_json::json!({
                "artifact_id": media.source.id,
                "ordinal": index + 1,
            })).collect::<Vec<_>>(),
            "report_contract": {
                "marker_format": "[artifact:<full ArtifactId>]",
                "source_artifact_ids_only": true,
            }
        })
        .to_string();
        let mut events = self.runner.run(AgentInput {
            principal,
            definition_id,
            parent_turn_id: input.previous_turn_id,
            prompt,
            artifacts: prepared
                .into_iter()
                .map(|media| media.derivative.id)
                .collect(),
            cancellation,
        });
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::Completed(result) | AgentEvent::Partial(result) => {
                    let report: MediaReport =
                        serde_json::from_value(result.output).map_err(|_| {
                            MediaUnderstandingError::new(
                                "media_report_invalid",
                                "Media Understanding returned an invalid Report",
                            )
                        })?;
                    return Ok(MediaUnderstandingResult {
                        turn_id: result.turn_id,
                        completion: result.completion,
                        report,
                    });
                }
                AgentEvent::Failed { error } => return Err(safe_agent_error(error)),
                _ => {}
            }
        }
        Err(MediaUnderstandingError::new(
            "media_execution_failed",
            "Media Understanding ended without a terminal result",
        ))
    }
}

fn validate_input(input: &MediaUnderstandingInput) -> Result<(), MediaUnderstandingError> {
    if input.prompt.is_empty() || input.prompt.len() > MAX_MEDIA_PROMPT_BYTES {
        return Err(MediaUnderstandingError::new(
            "invalid_media_prompt",
            "Media prompt must contain between 1 and 65536 UTF-8 bytes",
        ));
    }
    if input.previous_turn_id.is_none() && input.artifacts.is_empty() {
        return Err(MediaUnderstandingError::new(
            "media_artifact_required",
            "A root Media Turn requires at least one Artifact",
        ));
    }
    if input.artifacts.len() > super::MAX_MEDIA_ARTIFACTS {
        return Err(MediaUnderstandingError::new(
            "too_many_media_artifacts",
            "A Media Turn accepts at most eight new Artifacts",
        ));
    }
    let mut seen = HashSet::with_capacity(input.artifacts.len());
    if input
        .artifacts
        .iter()
        .any(|artifact| !seen.insert(&artifact.artifact_id))
    {
        return Err(MediaUnderstandingError::new(
            "duplicate_media_artifact",
            "Duplicate Media Artifact",
        ));
    }
    Ok(())
}

pub(crate) fn safe_preprocess_error(error: MediaPreprocessError) -> MediaUnderstandingError {
    let code = match error {
        MediaPreprocessError::SourceTooLarge => "media_source_too_large",
        MediaPreprocessError::TooManyArtifacts => "too_many_media_artifacts",
        MediaPreprocessError::DuplicateArtifact => "duplicate_media_artifact",
        MediaPreprocessError::UnsupportedType => "media_type_unsupported",
        MediaPreprocessError::MimeMismatch => "media_type_mismatch",
        MediaPreprocessError::SourceAggregateTooLarge => "media_sources_too_large",
        MediaPreprocessError::AnimatedWebp => "animated_media_unsupported",
        MediaPreprocessError::DimensionsTooLarge => "media_dimensions_too_large",
        MediaPreprocessError::TooManyPixels => "media_pixels_too_large",
        MediaPreprocessError::Decode => "media_decode_failed",
        MediaPreprocessError::DerivativeTooLarge => "media_derivative_too_large",
        MediaPreprocessError::DerivativeAggregateTooLarge => "media_derivatives_too_large",
        MediaPreprocessError::Unavailable => "media_artifact_unavailable",
        MediaPreprocessError::Cancelled => "cancelled",
        MediaPreprocessError::DeadlineExceeded => "deadline_exceeded",
        MediaPreprocessError::Storage => "media_storage_failed",
    };
    MediaUnderstandingError::new(code, error.to_string())
}

fn safe_agent_error(error: crate::agent::AgentRunError) -> MediaUnderstandingError {
    match error.code.as_str() {
        "parent_turn_unavailable" | "parent_turn_invalid" | "parent_turn_definition_mismatch" => {
            MediaUnderstandingError::new(
                "media_turn_unavailable",
                "Media Understanding Turn is unavailable",
            )
        }
        "cancelled" => MediaUnderstandingError::new("cancelled", "Media Understanding cancelled"),
        "deadline_exceeded" => MediaUnderstandingError::new(
            "deadline_exceeded",
            "Media Understanding deadline exceeded",
        ),
        "model_unavailable"
        | "definition_unavailable"
        | "definition_disabled"
        | "media_understanding_unavailable" => MediaUnderstandingError::new(
            "media_understanding_unavailable",
            "Media Understanding is unavailable",
        ),
        "media_report_invalid" => MediaUnderstandingError::new(
            "media_report_invalid",
            "Media Understanding could not produce a verified Report",
        ),
        "tool_authorization_failed" | "model_not_allowed" | "forbidden" => {
            MediaUnderstandingError::new(
                "media_authorization_failed",
                "Media Understanding authorization failed",
            )
        }
        _ => MediaUnderstandingError::new(
            "media_execution_failed",
            "Media Understanding execution failed",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::MediaArtifactInput;
    use super::*;
    use crate::agent::{AgentTurnId, ArtifactId};

    fn input(
        prompt: String,
        artifacts: Vec<&str>,
        parent: Option<&str>,
    ) -> MediaUnderstandingInput {
        MediaUnderstandingInput {
            prompt,
            artifacts: artifacts
                .into_iter()
                .map(|id| MediaArtifactInput {
                    artifact_id: ArtifactId::new(id),
                })
                .collect(),
            previous_turn_id: parent.map(AgentTurnId::new),
        }
    }

    #[test]
    fn root_requires_media_and_continuation_may_omit_new_media() {
        assert_eq!(
            validate_input(&input("describe".into(), Vec::new(), None))
                .unwrap_err()
                .code,
            "media_artifact_required"
        );
        assert!(
            validate_input(&input("follow up".into(), Vec::new(), Some("aturn_parent"))).is_ok()
        );
    }

    #[test]
    fn prompt_limit_counts_utf8_bytes_and_duplicates_are_rejected() {
        assert_eq!(
            validate_input(&input("界".repeat(21_846), vec!["source"], None))
                .unwrap_err()
                .code,
            "invalid_media_prompt"
        );
        assert_eq!(
            validate_input(&input("compare".into(), vec!["source", "source"], None))
                .unwrap_err()
                .code,
            "duplicate_media_artifact"
        );
    }

    #[test]
    fn media_model_capability_changes_remain_unavailable_errors() {
        let normalized = safe_agent_error(crate::agent::AgentRunError::new(
            "media_understanding_unavailable",
            "changed",
        ));
        assert_eq!(normalized.code, "media_understanding_unavailable");
        assert_eq!(normalized.message, "Media Understanding is unavailable");
    }
}
