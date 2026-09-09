use std::collections::HashSet;
use std::sync::Arc;

use stravia_runtime_contract::agent::AgentCompletion;
use stravia_runtime_contract::agent::AgentOutputValidationContext;
use stravia_runtime_contract::agent::AgentOutputValidator;
use stravia_runtime_contract::artifact::ArtifactId;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;

use stravia_media::store::MediaDerivativeStore;
use stravia_media::types::MediaReport;

use stravia_media::validator::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use stravia_media::types::MediaArtifactReference;

    use crate::agent::LocalArtifactStore;
    use bytes::Bytes;
    use stravia_runtime_contract::Principal;
    use stravia_runtime_contract::agent::AgentDefinitionId;
    use stravia_runtime_contract::agent::AgentTurnId;
    use stravia_runtime_contract::protocol::ir::Role;

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
        let store = Arc::new(MediaDerivativeStore::sqlite(
            pool,
            Arc::new(super::super::ArtifactHost(artifacts)),
        ));
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
