use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use stravia_runtime_contract::agent::{
    AgentCompletion, AgentOutputValidationContext, AgentOutputValidator, AgentRunError,
};
use stravia_runtime_contract::protocol::ir::{
    AiItem, ContentBlock, MediaSource, MessageContent, Role,
};

use super::store::MediaDerivativeStore;
use super::types::MediaReport;
use stravia_runtime_contract::artifact::ArtifactId;

pub const MAX_MEDIA_ANSWER_BYTES: usize = 64 * 1024;
pub const MAX_MEDIA_REPORT_BYTES: usize = 128 * 1024;

pub fn validate_media_report(
    report: MediaReport,
    evidence: &HashSet<ArtifactId>,
    completion: AgentCompletion,
) -> Result<MediaReport, String> {
    if report.answer.is_empty() || report.answer.len() > MAX_MEDIA_ANSWER_BYTES {
        return Err("Media Report answer exceeds its byte limit".into());
    }
    if report
        .limitations
        .iter()
        .any(|limitation| limitation.trim().is_empty())
    {
        return Err("Media Report limitation is invalid".into());
    }
    if completion == AgentCompletion::Partial && report.limitations.is_empty() {
        return Err("A partial Media Report must explain its limitation".into());
    }

    let marker_ids = answer_markers(&report.answer)?;
    let mut marker_set = HashSet::with_capacity(marker_ids.len());
    for marker in marker_ids {
        let id = ArtifactId::new(marker);
        marker_set.insert(id);
    }
    let mut listed = HashSet::with_capacity(report.artifacts.len());
    for artifact in &report.artifacts {
        if !listed.insert(artifact.artifact_id.clone()) {
            return Err("Media Report contains a duplicate Artifact reference".into());
        }
    }
    if marker_set != listed {
        return Err("Media Report markers and Artifact references do not match".into());
    }
    if !listed.is_subset(evidence) {
        return Err("Media Report references unavailable evidence".into());
    }
    let canonical =
        serde_json::to_vec(&report).map_err(|_| "Media Report serialization failed".to_owned())?;
    if canonical.len() > MAX_MEDIA_REPORT_BYTES {
        return Err("Media Report exceeds its byte limit".into());
    }
    Ok(report)
}

/// Only Media service-authored User prompts declare citable sources.
fn declared_sources(text: &str, declared: &mut HashSet<ArtifactId>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(media) = value.get("media").and_then(|media| media.as_array()) else {
        return;
    };
    declared.extend(
        media
            .iter()
            .filter_map(|entry| entry.get("artifact_id"))
            .filter_map(|id| id.as_str())
            .map(ArtifactId::new),
    );
}

fn answer_markers(answer: &str) -> Result<Vec<&str>, String> {
    let mut markers = Vec::new();
    let mut rest = answer;
    while let Some(start) = rest.find("[sa:") {
        let marker = &rest[start + "[sa:".len()..];
        let end = marker
            .find(']')
            .ok_or_else(|| "Media Report contains a malformed Artifact marker".to_owned())?;
        let artifact_id = &marker[..end];
        if artifact_id.is_empty()
            || artifact_id.len() > 128
            || !artifact_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("Media Report contains an invalid Artifact ID".into());
        }
        markers.push(artifact_id);
        rest = &marker[end + 1..];
    }
    Ok(markers)
}

pub struct MediaReportValidator {
    store: Arc<MediaDerivativeStore>,
}

impl MediaReportValidator {
    pub fn new(store: Arc<MediaDerivativeStore>) -> Self {
        Self { store }
    }

    async fn evidence(
        &self,
        principal: &stravia_runtime_contract::Principal,
        transcript: &[AiItem],
    ) -> Result<(HashSet<ArtifactId>, Vec<ArtifactId>), AgentRunError> {
        let mut shown = HashSet::new();
        let mut declared = HashSet::new();
        for message in transcript {
            // Declarations are request-authored: only the Turn prompts built by
            // the Media service carry the declared source list.
            if message.role == Role::User {
                match &message.content {
                    MessageContent::Blocks(blocks) => {
                        for block in blocks {
                            if let ContentBlock::Text { text, .. } = block {
                                declared_sources(text, &mut declared);
                            }
                        }
                    }
                    MessageContent::Text(text) => declared_sources(text, &mut declared),
                }
            }
            let MessageContent::Blocks(blocks) = &message.content else {
                continue;
            };
            for block in blocks {
                let ContentBlock::Image { source, .. } = block else {
                    continue;
                };
                let derivative_id = match source {
                    MediaSource::Url(reference) => ArtifactId::from_reference(reference).ok(),
                    MediaSource::FileId { file_id, .. } => ArtifactId::from_reference(file_id).ok(),
                    _ => None,
                }
                .ok_or_else(|| {
                    AgentRunError::new(
                        "media_report_invalid",
                        "Media transcript evidence is invalid",
                    )
                })?;
                shown.insert(derivative_id);
            }
        }
        // Forward verification: a source is evidence only when it was actually
        // declared in a Turn prompt and its mapped derivative is really present
        // in the transcript. A shared JPEG never widens the citable set to
        // sources the requests did not declare.
        let mut evidence = HashSet::new();
        let mut retained = HashSet::new();
        for source_id in declared {
            let Some(media) = self
                .store
                .find_derivative(principal, &source_id)
                .await
                .map_err(|_| {
                    AgentRunError::new(
                        "media_report_invalid",
                        "Media transcript evidence is unavailable",
                    )
                })?
            else {
                continue;
            };
            if shown.contains(&media.derivative.id) {
                retained.insert(media.derivative.id);
                retained.insert(source_id.clone());
                evidence.insert(source_id);
            }
        }
        Ok((evidence, retained.into_iter().collect()))
    }
}

#[async_trait]
impl AgentOutputValidator for MediaReportValidator {
    async fn validate(
        &self,
        context: &AgentOutputValidationContext,
        transcript: &[AiItem],
        output: Value,
    ) -> Result<Value, AgentRunError> {
        let report: MediaReport = serde_json::from_value(output).map_err(|_| {
            AgentRunError::new("media_report_invalid", "Media Report shape is invalid")
        })?;
        let (evidence, _) = self.evidence(&context.principal, transcript).await?;
        let report = validate_media_report(report, &evidence, context.completion)
            .map_err(|message| AgentRunError::new("media_report_invalid", message))?;
        serde_json::to_value(report).map_err(|_| {
            AgentRunError::new("media_report_invalid", "Media Report serialization failed")
        })
    }

    async fn before_commit(
        &self,
        context: &AgentOutputValidationContext,
        transcript: &[AiItem],
        _output: &Value,
    ) -> Result<(), AgentRunError> {
        let (_, retained) = self.evidence(&context.principal, transcript).await?;
        self.store
            .promote(
                &context.principal,
                &retained,
                Duration::from_secs(7 * 24 * 60 * 60),
            )
            .await
            .map_err(|_| {
                AgentRunError::new(
                    "media_store_failed",
                    "Media Artifact retention could not be extended",
                )
            })
    }
}
