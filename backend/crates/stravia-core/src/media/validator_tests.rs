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
    use stravia_runtime_contract::artifact::ArtifactStore;
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
            "Compare [sa:artifact_a] with [sa:artifact_b].".into(),
            &["artifact_a", "artifact_b"],
            &[],
        );
        assert!(validate_media_report(valid, &evidence, AgentCompletion::Completed).is_ok());
        let repeated = report(
            "First [sa:artifact_a], then again [sa:artifact_a].".into(),
            &["artifact_a"],
            &[],
        );
        assert!(validate_media_report(repeated, &evidence, AgentCompletion::Completed).is_ok());

        for invalid in [
            report(
                "Only [sa:artifact_a].".into(),
                &["artifact_a", "artifact_b"],
                &[],
            ),
            report(
                "Forged [sa:artifact_foreign].".into(),
                &["artifact_foreign"],
                &[],
            ),
            report("Broken [sa:artifact_a".into(), &["artifact_a"], &[]),
        ] {
            assert!(validate_media_report(invalid, &evidence, AgentCompletion::Completed).is_err());
        }
    }

    #[test]
    fn partial_and_size_limits_are_enforced_without_reference_count_limit() {
        let evidence = HashSet::from([id("artifact_a")]);
        let partial = report("Observed [sa:artifact_a].".into(), &["artifact_a"], &[]);
        assert!(validate_media_report(partial, &evidence, AgentCompletion::Partial).is_err());

        let marker = "[sa:artifact_a]";
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
            .map(|value| format!("[sa:{value}]"))
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

    fn prompt_declaring(source_ids: &[&ArtifactId]) -> String {
        serde_json::json!({
            "task": "describe",
            "media": source_ids.iter().enumerate().map(|(index, id)| serde_json::json!({
                "artifact_id": id.as_str(),
                "ordinal": index + 1,
            })).collect::<Vec<_>>(),
            "report_contract": {
                "marker_format": "[sa:<full ArtifactId>]",
                "source_artifact_ids_only": true,
            }
        })
        .to_string()
    }

    fn turn(content: Vec<ContentBlock>) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Blocks(content),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    fn prompt_block(source_ids: &[&ArtifactId]) -> ContentBlock {
        ContentBlock::Text {
            text: prompt_declaring(source_ids),
            cache_control: None,
        }
    }

    fn derivative_block(derivative_id: &ArtifactId) -> ContentBlock {
        ContentBlock::Image {
            source: MediaSource::FileId {
                file_id: format!("sa:{}", derivative_id.as_str()),
                detail: None,
            },
            detail: None,
            cache_control: None,
        }
    }

    fn validation_context(principal: Principal) -> AgentOutputValidationContext {
        AgentOutputValidationContext {
            principal,
            turn_id: AgentTurnId::agent(),
            definition_id: AgentDefinitionId::new("media-understanding"),
            definition_revision: 1,
            completion: AgentCompletion::Completed,
        }
    }

    #[tokio::test]
    async fn agent_validator_cites_only_sources_declared_by_the_media_prompt() {
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
            .get_or_create_derivative(
                &principal,
                &source.id,
                jpeg(),
                Duration::from_secs(60),
                "image/jpeg",
            )
            .await
            .expect("derivative");
        let transcript = vec![turn(vec![
            prompt_block(&[&source.id]),
            derivative_block(&media.derivative.id),
        ])];
        let context = validation_context(principal);
        let validator = MediaReportValidator::new(store);
        let valid = report(
            format!("Observed [sa:{}].", source.id.as_str()),
            &[source.id.as_str()],
            &[],
        );
        validator
            .validate(&context, &transcript, serde_json::to_value(valid).unwrap())
            .await
            .expect("validated source evidence");

        // The shown derivative itself is not a declared source.
        let derivative = report(
            format!("Observed [sa:{}].", media.derivative.id.as_str()),
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

    #[tokio::test]
    async fn shared_derivative_does_not_widen_evidence_to_undeclared_sources() {
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
            pool.clone(),
            Arc::new(super::super::ArtifactHost(artifacts)),
        ));
        let principal = Principal::new("owner");
        let declared = store
            .create_source(
                &principal,
                "image/png",
                Bytes::from_static(b"declared-original"),
                Duration::from_secs(60),
            )
            .await
            .expect("declared source");
        let undeclared = store
            .create_source(
                &principal,
                "image/png",
                Bytes::from_static(b"undeclared-original"),
                Duration::from_secs(60),
            )
            .await
            .expect("undeclared source");
        // Both originals normalize to the very same JPEG Artifact.
        let shared = jpeg();
        let declared_media = store
            .get_or_create_derivative(
                &principal,
                &declared.id,
                shared.clone(),
                Duration::from_secs(60),
                "image/jpeg",
            )
            .await
            .expect("declared mapping");
        let undeclared_media = store
            .get_or_create_derivative(
                &principal,
                &undeclared.id,
                shared,
                Duration::from_secs(60),
                "image/jpeg",
            )
            .await
            .expect("undeclared mapping");
        assert_eq!(declared_media.derivative.id, undeclared_media.derivative.id);
        let context = validation_context(principal.clone());
        let validator = MediaReportValidator::new(store);

        let root_turn = vec![turn(vec![
            prompt_block(&[&declared.id]),
            derivative_block(&declared_media.derivative.id),
        ])];
        let citing_declared = report(
            format!("Observed [sa:{}].", declared.id.as_str()),
            &[declared.id.as_str()],
            &[],
        );
        validator
            .validate(
                &context,
                &root_turn,
                serde_json::to_value(citing_declared).unwrap(),
            )
            .await
            .expect("declared source is evidence");
        let citing_undeclared = report(
            format!("Observed [sa:{}].", undeclared.id.as_str()),
            &[undeclared.id.as_str()],
            &[],
        );
        assert!(
            validator
                .validate(
                    &context,
                    &root_turn,
                    serde_json::to_value(&citing_undeclared).unwrap(),
                )
                .await
                .is_err(),
            "an undeclared source must not ride along on a shared JPEG"
        );

        // A continuation may declare the second source while its JPEG is
        // already in the parent context; the declaration restores citability.
        let mut continuation = root_turn.clone();
        continuation.push(turn(vec![prompt_block(&[&undeclared.id])]));
        validator
            .validate(
                &context,
                &continuation,
                serde_json::to_value(&citing_undeclared).unwrap(),
            )
            .await
            .expect("continuation declaration is evidence");
    }

    fn document_prompt(document: &ArtifactId, embedded: &[&ArtifactId], with_text: bool) -> String {
        let mut media = vec![serde_json::json!({
            "artifact_id": document.as_str(),
            "ordinal": 1,
            "kind": "document",
            "format": "docx",
        })];
        if with_text {
            media[0]["text"] = serde_json::json!("extracted markdown");
        }
        for (index, id) in embedded.iter().enumerate() {
            media.push(serde_json::json!({
                "artifact_id": id.as_str(),
                "ordinal": index + 2,
                "kind": "image",
            }));
        }
        serde_json::json!({
            "task": "describe",
            "media": media,
            "report_contract": {
                "marker_format": "[sa:<full ArtifactId>]",
                "source_artifact_ids_only": true,
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn document_evidence_requires_manifest_derivative_and_declared_text() {
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
            Arc::new(super::super::ArtifactHost(Arc::clone(&artifacts))),
        ));
        let principal = Principal::new("owner");
        let document = store
            .create_source(
                &principal,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                Bytes::from_static(b"document"),
                Duration::from_secs(60),
            )
            .await
            .expect("document source");
        let markdown = artifacts
            .ingest(
                &principal,
                "text/markdown",
                Some(3),
                stravia_runtime_contract::artifact::bytes_stream(Bytes::from_static(b"md!")),
                Duration::from_secs(60),
            )
            .await
            .expect("markdown artifact");
        let embedded = artifacts
            .ingest(
                &principal,
                "image/jpeg",
                None,
                stravia_runtime_contract::artifact::bytes_stream(jpeg()),
                Duration::from_secs(60),
            )
            .await
            .expect("embedded image");
        let manifest = serde_json::json!({
            "version": 1,
            "format": "docx",
            "markdown_artifact": format!("sa:{}", markdown.id.as_str()),
            "images": [{
                "artifact_id": embedded.id.as_str(),
                "ordinal": 1,
                "normalizable": true,
                "size": 3,
            }],
        });
        let media = store
            .get_or_create_derivative(
                &principal,
                &document.id,
                Bytes::from(serde_json::to_vec(&manifest).unwrap()),
                Duration::from_secs(60),
                stravia_media::documents::DOCUMENT_MANIFEST_MIME,
            )
            .await
            .expect("manifest derivative");
        let context = validation_context(principal);
        let validator = MediaReportValidator::new(store);

        // A document declared with text + a verified manifest is evidence;
        // embedded images attach under their own Artifact ids.
        let transcript = vec![turn(vec![
            ContentBlock::Text {
                text: document_prompt(&document.id, &[&embedded.id], true),
                cache_control: None,
            },
            derivative_block(&embedded.id),
        ])];
        let citing_document = report(
            format!("Read [sa:{}].", document.id.as_str()),
            &[document.id.as_str()],
            &[],
        );
        validator
            .validate(
                &context,
                &transcript,
                serde_json::to_value(citing_document).unwrap(),
            )
            .await
            .expect("document source is evidence");
        let citing_embedded = report(
            format!("Figure [sa:{}].", embedded.id.as_str()),
            &[embedded.id.as_str()],
            &[],
        );
        validator
            .validate(
                &context,
                &transcript,
                serde_json::to_value(citing_embedded).unwrap(),
            )
            .await
            .expect("embedded image is evidence");
        // The manifest derivative itself is never citable.
        let citing_manifest = report(
            format!("Forged [sa:{}].", media.derivative.id.as_str()),
            &[media.derivative.id.as_str()],
            &[],
        );
        assert!(
            validator
                .validate(
                    &context,
                    &transcript,
                    serde_json::to_value(citing_manifest).unwrap(),
                )
                .await
                .is_err(),
            "derivative ids are not citable"
        );

        // A document entry without extracted text is not evidence.
        let no_text = vec![turn(vec![ContentBlock::Text {
            text: document_prompt(&document.id, &[], false),
            cache_control: None,
        }])];
        assert!(
            validator
                .validate(
                    &context,
                    &no_text,
                    serde_json::to_value(report(
                        format!("Read [sa:{}].", document.id.as_str()),
                        &[document.id.as_str()],
                        &[],
                    ))
                    .unwrap(),
                )
                .await
                .is_err(),
            "documents without declared text carry no evidence"
        );
    }
}
