use std::collections::HashSet;

use stravia_media::{
    types::{MediaArtifactReference, MediaReport},
    validator::validate_media_report,
};
use stravia_runtime_contract::{agent::AgentCompletion, artifact::ArtifactId};

#[test]
fn media_report_accepts_trusted_ancestor_and_rejects_untrusted_input() {
    let input_id = ArtifactId::new("a".repeat(55));
    let ancestor_id = ArtifactId::new("b".repeat(55));
    let input_path = format!("stravia://artifacts/{}", input_id.as_str());
    let ancestor_path = format!("stravia://artifacts/{}", ancestor_id.as_str());
    let answer = format!("The prior image shows this [{ancestor_path}]");

    let report = MediaReport {
        answer: answer.clone(),
        artifacts: vec![MediaArtifactReference {
            artifact_id: ancestor_id.clone(),
        }],
        limitations: Vec::new(),
    };
    let validated = validate_media_report(
        report,
        &HashSet::from([ancestor_id.clone()]),
        AgentCompletion::Completed,
    )
    .expect("ancestor evidence is valid when it is in the trusted evidence set");
    assert_eq!(validated.artifacts[0].artifact_id, ancestor_id);
    assert!(validated.answer.contains(&ancestor_path));
    assert_ne!(input_id, validated.artifacts[0].artifact_id);

    assert!(
        validate_media_report(
            MediaReport {
                answer: format!("This cites [{input_path}]"),
                artifacts: vec![MediaArtifactReference {
                    artifact_id: input_id,
                }],
                limitations: Vec::new(),
            },
            &HashSet::from([ancestor_id]),
            AgentCompletion::Completed,
        )
        .is_err()
    );
}
