use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;

use stravia_runtime_contract::CancellationToken;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::agent::{AgentDefinitionId, AgentEvent, AgentInput};

use super::store::MediaDerivativeStore;
use super::types::MediaArtifactReference;
use super::{
    MEDIA_DEFINITION_ID, MediaInputPreprocessor, MediaPreprocessError, MediaReport,
    MediaUnderstandingInput, MediaUnderstandingResult,
};

pub const MAX_MEDIA_PROMPT_BYTES: usize = 64 * 1024;
/// Staging retention requested for derivative Artifacts; the store always
/// extends it to at least the source Artifact's remaining retention.
pub const DERIVATIVE_STAGING_RETENTION: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, serde::Serialize, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct MediaUnderstandingError {
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
pub struct MediaUnderstandingService {
    runner: std::sync::Arc<dyn crate::host::MediaAgentHost>,
    preprocessor: MediaInputPreprocessor,
}

impl MediaUnderstandingService {
    pub fn new(
        runner: std::sync::Arc<dyn crate::host::MediaAgentHost>,
        derivatives: std::sync::Arc<MediaDerivativeStore>,
    ) -> Self {
        Self {
            runner,
            preprocessor: MediaInputPreprocessor::new(derivatives, DERIVATIVE_STAGING_RETENTION),
        }
    }

    pub async fn model_id(&self) -> Option<String> {
        self.runner
            .definition_model_with_thinking_level(&AgentDefinitionId::new(MEDIA_DEFINITION_ID))
            .await
            .map(|(model_id, _)| model_id)
    }
    pub async fn prepare_sources(
        &self,
        principal: &Principal,
        source_ids: &[stravia_runtime_contract::artifact::ArtifactId],
        cancellation: &CancellationToken,
        deadline: std::time::Instant,
    ) -> Result<(), MediaUnderstandingError> {
        self.preprocessor
            .preprocess_until(principal, source_ids, cancellation, deadline)
            .await
            .map(|_| ())
            .map_err(safe_preprocess_error)
    }

    pub async fn execute_until(
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
            source_bytes = prepared
                .iter()
                .map(|media| media.source().size)
                .sum::<u64>(),
            derivative_bytes = prepared
                .iter()
                .map(|media| media.derivative().size)
                .sum::<u64>(),
            "Media preprocessing completed"
        );
        let (media, appended, mut turn_limitations) =
            media_attachments(&prepared, &ancestor_derivatives);
        let prompt = serde_json::json!({
            "task": input.prompt,
            "media": media,
            "report_contract": {
                "marker_format": "[stravia://artifacts/<artifact-id>]",
                "source_artifact_paths_only": true,
            }
        })
        .to_string();
        let mut events = self.runner.run(AgentInput {
            principal,
            definition_id,
            parent_turn_id: input.previous_turn_id,
            prompt,
            artifacts: appended,
            cancellation,
        });
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::Completed(result) | AgentEvent::Partial(result) => {
                    let mut report: MediaReport =
                        serde_json::from_value(result.output).map_err(|_| {
                            MediaUnderstandingError::new(
                                "media_report_invalid",
                                "Media Understanding returned an invalid Report",
                            )
                        })?;
                    // Extraction/attach-budget limitations are platform facts
                    // the model cannot claim — merge them after Report
                    // validation so they never count against the model's
                    // serialized budget.
                    merge_limitations(
                        &mut report.limitations,
                        std::mem::take(&mut turn_limitations),
                    );
                    return Ok(MediaUnderstandingResult {
                        turn_id: result.turn_id,
                        completion: result.completion,
                        artifacts: source_ids
                            .into_iter()
                            .map(|artifact_id| MediaArtifactReference { artifact_id })
                            .collect(),
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

/// Per-document excerpt budget inside the Media prompt; the extracted
/// Markdown is truncated at a char boundary when it exceeds this share.
const MAX_DOCUMENT_PROMPT_BYTES: usize = 128 * 1024;
/// Total prompt budget for extracted document text across one Turn.
const MAX_TURN_DOCUMENT_PROMPT_BYTES: usize = 256 * 1024;

/// Tracks images physically appended to a Turn: deduplicated across sources
/// and ancestors, bounded by the existing attachment count/byte budgets.
struct Attachments {
    seen: HashSet<stravia_runtime_contract::artifact::ArtifactId>,
    appended: Vec<stravia_runtime_contract::artifact::ArtifactId>,
    appended_bytes: u64,
    limitations: Vec<String>,
}

impl Attachments {
    fn new(ancestors: &[stravia_runtime_contract::artifact::ArtifactId]) -> Self {
        Self {
            seen: ancestors.iter().cloned().collect(),
            appended: Vec::new(),
            appended_bytes: 0,
            limitations: Vec::new(),
        }
    }

    fn attach(&mut self, id: &stravia_runtime_contract::artifact::ArtifactId, size: u64) {
        if !self.seen.insert(id.clone()) {
            return;
        }
        if self.appended.len() >= super::MAX_MEDIA_ARTIFACTS
            || self.appended_bytes.saturating_add(size)
                > super::preprocessor::MAX_TURN_DERIVATIVE_BYTES as u64
        {
            self.limitations.push(
                "An image was declared but not attached because the Turn attachment budget was exhausted"
                    .to_owned(),
            );
            return;
        }
        self.appended.push(id.clone());
        self.appended_bytes += size;
    }
}

fn merge_limitations(report: &mut Vec<String>, extra: Vec<String>) {
    for limitation in extra {
        if !report.contains(&limitation) {
            report.push(limitation);
        }
    }
}

/// Keeps every declared Source citable while deduplicating only the images
/// physically appended to the Turn. Two Sources may share one normalized JPEG
/// (or reuse an ancestor's); each still gets its own prompt declaration, but
/// the shared JPEG is attached at most once per Turn and never re-attached
/// when it is already in the parent context.
///
/// Document Sources additionally declare their extracted Markdown as entry
/// `text` and declare each embedded image Artifact as a following `"image"`
/// entry; only normalized (JPEG) embedded images are attached. Per-entry
/// extraction limitations are surfaced to the model inside the entry, while
/// Turn-level attach-budget limitations merge into the final Report.
fn media_attachments(
    prepared: &[super::preprocessor::PreparedMedia],
    ancestor_derivatives: &[stravia_runtime_contract::artifact::ArtifactId],
) -> (
    Vec<serde_json::Value>,
    Vec<stravia_runtime_contract::artifact::ArtifactId>,
    Vec<String>,
) {
    use super::preprocessor::PreparedMedia;
    let mut media = Vec::with_capacity(prepared.len());
    let mut attachments = Attachments::new(ancestor_derivatives);
    let mut document_text_budget = MAX_TURN_DOCUMENT_PROMPT_BYTES;
    let mut ordinal = 0_usize;
    for item in prepared {
        ordinal += 1;
        match item {
            PreparedMedia::Image(image) => {
                media.push(serde_json::json!({
                    "path": image.source.id.reference(),
                    "ordinal": ordinal,
                    "kind": "image",
                }));
                attachments.attach(&image.derivative.id, image.derivative.size);
            }
            PreparedMedia::Document(document) => {
                let budget = MAX_DOCUMENT_PROMPT_BYTES.min(document_text_budget);
                let (text, text_truncated) =
                    crate::documents::truncate_utf8(&document.markdown, budget);
                document_text_budget = document_text_budget.saturating_sub(text.len());
                let mut entry_limitations = document.limitations.clone();
                if text_truncated {
                    entry_limitations
                        .push("Document text was truncated to fit the prompt budget".to_owned());
                }
                let mut entry = serde_json::json!({
                    "path": document.source.id.reference(),
                    "ordinal": ordinal,
                    "kind": "document",
                    "format": document.format,
                    "text": text,
                });
                if !entry_limitations.is_empty() {
                    entry["limitations"] = serde_json::json!(entry_limitations);
                }
                media.push(entry);
                for image in &document.images {
                    ordinal += 1;
                    media.push(serde_json::json!({
                        "path": image.artifact_id.reference(),
                        "ordinal": ordinal,
                        "kind": "image",
                    }));
                    if image.normalizable {
                        attachments.attach(&image.artifact_id, image.size);
                    }
                }
            }
        }
    }
    (media, attachments.appended, attachments.limitations)
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

pub fn safe_preprocess_error(error: MediaPreprocessError) -> MediaUnderstandingError {
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
        MediaPreprocessError::DocumentInvalid => "media_document_invalid",
        MediaPreprocessError::DerivativeTooLarge => "media_derivative_too_large",
        MediaPreprocessError::DerivativeAggregateTooLarge => "media_derivatives_too_large",
        MediaPreprocessError::Unavailable => "media_artifact_unavailable",
        MediaPreprocessError::Cancelled => "cancelled",
        MediaPreprocessError::DeadlineExceeded => "deadline_exceeded",
        MediaPreprocessError::Storage => "media_storage_failed",
    };
    MediaUnderstandingError::new(code, error.to_string())
}

fn safe_agent_error(
    error: stravia_runtime_contract::agent::AgentRunError,
) -> MediaUnderstandingError {
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
    use stravia_runtime_contract::agent::AgentTurnId;
    use stravia_runtime_contract::artifact::ArtifactId;
    use stravia_runtime_contract::artifact::ArtifactRef;

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
    fn shared_derivatives_keep_declarations_and_deduplicate_appended_images() {
        let prepared_media = |source: &str, derivative: &str| {
            super::super::preprocessor::PreparedMedia::Image(
                super::super::preprocessor::PreparedImage {
                    source: ArtifactRef {
                        id: ArtifactId::new(source),
                        mime_type: "image/png".into(),
                        size: 64,
                    },
                    derivative: ArtifactRef {
                        id: ArtifactId::new(derivative),
                        mime_type: "image/jpeg".into(),
                        size: 32,
                    },
                    derivative_bytes: bytes::Bytes::from_static(&[]),
                },
            )
        };

        // In one Turn, two fresh Sources normalize to the same JPEG: both stay
        // citable, but the shared image is appended once.
        let (media, appended, _) = media_attachments(
            &[
                prepared_media("source-a", "shared"),
                prepared_media("source-c", "shared"),
            ],
            &[],
        );
        assert_eq!(
            media
                .iter()
                .map(|entry| entry["path"].as_str())
                .collect::<Vec<_>>(),
            [
                Some("stravia://artifacts/source-a"),
                Some("stravia://artifacts/source-c"),
            ]
        );
        assert_eq!(
            media
                .iter()
                .map(|entry| entry["ordinal"].as_u64())
                .collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        assert_eq!(appended, vec![ArtifactId::new("shared")]);

        // In a continuation, a Source whose JPEG is already in the parent
        // context keeps its declaration while nothing is re-attached, and a
        // sibling sharing that same JPEG adds no further image either.
        let (media, appended, _) = media_attachments(
            &[
                prepared_media("source-a", "shared"),
                prepared_media("source-b", "fresh"),
                prepared_media("source-c", "shared"),
            ],
            &[ArtifactId::new("shared")],
        );
        assert_eq!(
            media
                .iter()
                .map(|entry| entry["path"].as_str())
                .collect::<Vec<_>>(),
            [
                Some("stravia://artifacts/source-a"),
                Some("stravia://artifacts/source-b"),
                Some("stravia://artifacts/source-c"),
            ]
        );
        assert_eq!(appended, vec![ArtifactId::new("fresh")]);
    }

    fn prepared_document(
        source: &str,
        markdown: String,
        images: Vec<(&str, bool)>,
    ) -> super::super::preprocessor::PreparedMedia {
        super::super::preprocessor::PreparedMedia::Document(
            super::super::preprocessor::PreparedDocument {
                source: ArtifactRef {
                    id: ArtifactId::new(source),
                    mime_type:
                        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                            .into(),
                    size: 100,
                },
                manifest_artifact: ArtifactRef {
                    id: ArtifactId::new(format!("manifest-{source}")),
                    mime_type: crate::documents::DOCUMENT_MANIFEST_MIME.into(),
                    size: 10,
                },
                format: office_oxide::DocumentFormat::Docx,
                title: Some("Report".into()),
                markdown,
                images: images
                    .into_iter()
                    .enumerate()
                    .map(|(index, (id, normalizable))| {
                        super::super::preprocessor::PreparedDocumentImage {
                            artifact_id: ArtifactId::new(id),
                            alt: None,
                            ordinal: index as u32 + 1,
                            normalizable,
                            size: 16,
                        }
                    })
                    .collect(),
                truncated: false,
                limitations: vec![],
            },
        )
    }

    #[test]
    fn document_entries_declare_text_and_only_normalized_images_attach() {
        let (media, appended, limitations) = media_attachments(
            &[prepared_document(
                "doc-a",
                "# Report\n\n![p](stravia://artifacts/emb-1)".into(),
                vec![("emb-1", true), ("emb-2", false)],
            )],
            &[],
        );
        assert_eq!(
            media
                .iter()
                .map(|entry| entry["kind"].as_str())
                .collect::<Vec<_>>(),
            [Some("document"), Some("image"), Some("image")]
        );
        assert_eq!(media[0]["path"], "stravia://artifacts/doc-a");
        assert_eq!(media[0]["format"], "docx");
        assert_eq!(
            media[0]["text"],
            "# Report\n\n![p](stravia://artifacts/emb-1)"
        );
        // Embedded images follow their document in order; only the normalized
        // one is physically attached.
        assert_eq!(media[1]["path"], "stravia://artifacts/emb-1");
        assert_eq!(media[1]["ordinal"], 2);
        assert_eq!(media[2]["path"], "stravia://artifacts/emb-2");
        assert_eq!(appended, vec![ArtifactId::new("emb-1")]);
        assert!(limitations.is_empty());
    }

    #[test]
    fn document_text_truncates_at_the_prompt_budget() {
        let (media, _, _) = media_attachments(
            &[prepared_document(
                "doc-big",
                "x".repeat(MAX_DOCUMENT_PROMPT_BYTES + 512),
                Vec::new(),
            )],
            &[],
        );
        let text = media[0]["text"].as_str().unwrap();
        assert!(text.len() <= MAX_DOCUMENT_PROMPT_BYTES);
        assert!(
            media[0]["limitations"].to_string().contains("truncated"),
            "entry limitations: {}",
            media[0]["limitations"]
        );
    }

    #[test]
    fn media_model_capability_changes_remain_unavailable_errors() {
        let normalized = safe_agent_error(stravia_runtime_contract::agent::AgentRunError::new(
            "media_understanding_unavailable",
            "changed",
        ));
        assert_eq!(normalized.code, "media_understanding_unavailable");
        assert_eq!(normalized.message, "Media Understanding is unavailable");
    }
}
