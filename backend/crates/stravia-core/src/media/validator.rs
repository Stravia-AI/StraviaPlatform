use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{
    AgentCompletion, AgentOutputValidationContext, AgentOutputValidator, AgentRunError, ArtifactId,
};
use crate::protocol::ir::{AiItem, ContentBlock, MediaSource, MessageContent};

use super::store::MediaDerivativeStore;
use super::types::MediaReport;

pub(crate) const MAX_MEDIA_ANSWER_BYTES: usize = 64 * 1024;
pub(crate) const MAX_MEDIA_REPORT_BYTES: usize = 128 * 1024;

pub(crate) fn validate_media_report(
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

pub(crate) struct MediaReportValidator {
    store: Arc<MediaDerivativeStore>,
}

impl MediaReportValidator {
    pub fn new(store: Arc<MediaDerivativeStore>) -> Self {
        Self { store }
    }

    async fn evidence(
        &self,
        principal: &crate::hook::Principal,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::types::MediaArtifactReference;
    use std::time::Duration;

    use crate::agent::{AgentDefinitionId, AgentTurnId, LocalArtifactStore};
    use crate::hook::Principal;
    use crate::protocol::ir::Role;
    use bytes::Bytes;

    fn id(value: &str) -> ArtifactId {
        ArtifactId::new(value)
    }

    fn jpeg() -> Bytes {
        use image::ImageEncoder;

        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .write_image(&[127, 127, 127], 1, 1, image::ExtendedColorType::Rgb8)
            .expect("encode JPEG");
        Bytes::from(bytes)
    }

    fn report(answer: String, artifacts: &[&str], limitations: &[&str]) -> MediaReport {
        MediaReport {
            answer,
            artifacts: artifacts
                .iter()
                .map(|artifact_id| MediaArtifactReference {
                    artifact_id: id(artifact_id),
                })
                .collect(),
            limitations: limitations
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    #[test]
    fn report_allows_repeated_citations_and_requires_marker_list_evidence_bijection() {
        let evidence = HashSet::from([id("artifact_a"), id("artifact_b")]);
        let valid = report(
            "Compare [artifact:artifact_a] with [artifact:artifact_b].".into(),
            &["artifact_a", "artifact_b"],
            &[],
        );
        assert!(validate_media_report(valid, &evidence, AgentCompletion::Completed).is_ok());
        let repeated = report(
            "First [artifact:artifact_a], then again [artifact:artifact_a].".into(),
            &["artifact_a"],
            &[],
        );
        assert!(validate_media_report(repeated, &evidence, AgentCompletion::Completed).is_ok());

        for invalid in [
            report(
                "Only [artifact:artifact_a].".into(),
                &["artifact_a", "artifact_b"],
                &[],
            ),
            report(
                "Forged [artifact:artifact_foreign].".into(),
                &["artifact_foreign"],
                &[],
            ),
            report("Broken [artifact:artifact_a".into(), &["artifact_a"], &[]),
        ] {
            assert!(validate_media_report(invalid, &evidence, AgentCompletion::Completed).is_err());
        }
    }

    #[test]
    fn partial_and_size_limits_are_enforced_without_reference_count_limit() {
        let evidence = HashSet::from([id("artifact_a")]);
        let partial = report(
            "Observed [artifact:artifact_a].".into(),
            &["artifact_a"],
            &[],
        );
        assert!(validate_media_report(partial, &evidence, AgentCompletion::Partial).is_err());

        let marker = "[artifact:artifact_a]";
        let at_limit = report(
            format!(
                "{marker}{}",
                "x".repeat(MAX_MEDIA_ANSWER_BYTES - marker.len())
            ),
            &["artifact_a"],
            &[],
        );
        assert!(validate_media_report(at_limit, &evidence, AgentCompletion::Completed).is_ok());
        let over_limit = report(
            format!(
                "{marker}{}",
                "x".repeat(MAX_MEDIA_ANSWER_BYTES - marker.len() + 1)
            ),
            &["artifact_a"],
            &[],
        );
        assert!(validate_media_report(over_limit, &evidence, AgentCompletion::Completed).is_err());

        let ids = (0..1000)
            .map(|index| format!("artifact_{index}"))
            .collect::<Vec<_>>();
        let answer = ids
            .iter()
            .map(|value| format!("[artifact:{value}]"))
            .collect::<Vec<_>>()
            .join(" ");
        let many = MediaReport {
            answer,
            artifacts: ids
                .iter()
                .map(|value| MediaArtifactReference {
                    artifact_id: id(value),
                })
                .collect(),
            limitations: vec![],
        };
        let evidence = ids.iter().map(|value| id(value)).collect();
        assert!(validate_media_report(many, &evidence, AgentCompletion::Completed).is_ok());
    }

    #[tokio::test]
    async fn agent_validator_uses_only_mapped_derivative_blocks_as_evidence() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let artifacts = Arc::new(LocalArtifactStore::sqlite(
            pool.clone(),
            data_dir.path().join("artifacts"),
        ));
        let store = Arc::new(MediaDerivativeStore::sqlite(pool, artifacts));
        let principal = Principal::new("owner");
        let source = store
            .create_source(
                &principal,
                "image/png",
                Bytes::from_static(b"source"),
                Duration::from_secs(60),
            )
            .await
            .expect("source");
        let media = store
            .get_or_create_derivative(&principal, &source.id, jpeg(), Duration::from_secs(60))
            .await
            .expect("derivative");
        let transcript = vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "[artifact:artifact_forged]".into(),
                    cache_control: None,
                },
                ContentBlock::Image {
                    source: MediaSource::FileId {
                        file_id: format!("stravia-artifact:{}", media.derivative.id.as_str()),
                        detail: None,
                    },
                    detail: None,
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }];
        let context = AgentOutputValidationContext {
            principal: principal.clone(),
            turn_id: AgentTurnId::agent(),
            definition_id: AgentDefinitionId::new("media-understanding"),
            definition_revision: 1,
            completion: AgentCompletion::Completed,
        };
        let validator = MediaReportValidator::new(store);
        let valid = report(
            format!("Observed [artifact:{}].", source.id.as_str()),
            &[source.id.as_str()],
            &[],
        );
        validator
            .validate(&context, &transcript, serde_json::to_value(valid).unwrap())
            .await
            .expect("validated source evidence");

        let derivative = report(
            format!("Observed [artifact:{}].", media.derivative.id.as_str()),
            &[media.derivative.id.as_str()],
            &[],
        );
        assert!(
            validator
                .validate(
                    &context,
                    &transcript,
                    serde_json::to_value(derivative).unwrap(),
                )
                .await
                .is_err()
        );
    }
}
