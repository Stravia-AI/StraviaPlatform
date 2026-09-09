use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use stravia_runtime_contract::agent::{
    AgentCompletion, AgentOutputValidationContext, AgentOutputValidator, AgentRunError,
};
use stravia_runtime_contract::protocol::ir::{AiItem, ContentBlock, MediaSource, MessageContent};

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

fn answer_markers(answer: &str) -> Result<Vec<&str>, String> {
    let mut markers = Vec::new();
    let mut rest = answer;
    while let Some(start) = rest.find("[artifact:") {
        let marker = &rest[start + "[artifact:".len()..];
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
        let mut evidence = HashSet::new();
        let mut retained = HashSet::new();
        for message in transcript {
            let MessageContent::Blocks(blocks) = &message.content else {
                continue;
            };
            for block in blocks {
                let ContentBlock::Image {
                    source: MediaSource::FileId { file_id, .. },
                    ..
                } = block
                else {
                    continue;
                };
                let Some(derivative_id) = file_id.strip_prefix("stravia-artifact:") else {
                    return Err(AgentRunError::new(
                        "media_report_invalid",
                        "Media transcript evidence is invalid",
                    ));
                };
                let derivative_id = ArtifactId::new(derivative_id);
                let source_id = self
                    .store
                    .source_for_derivative(principal, &derivative_id)
                    .await
                    .map_err(|_| {
                        AgentRunError::new(
                            "media_report_invalid",
                            "Media transcript evidence is unavailable",
                        )
                    })?
                    .ok_or_else(|| {
                        AgentRunError::new(
                            "media_report_invalid",
                            "Media transcript evidence is invalid",
                        )
                    })?;
                retained.insert(derivative_id);
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
