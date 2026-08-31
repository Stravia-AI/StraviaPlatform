use serde::{Deserialize, Serialize};

use crate::agent::{AgentCompletion, AgentTurnId, ArtifactId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaArtifactInput {
    pub artifact_id: ArtifactId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaUnderstandingInput {
    pub prompt: String,
    #[serde(default)]
    pub artifacts: Vec<MediaArtifactInput>,
    #[serde(default)]
    pub previous_turn_id: Option<AgentTurnId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaArtifactReference {
    pub artifact_id: ArtifactId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaReport {
    pub answer: String,
    pub artifacts: Vec<MediaArtifactReference>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaUnderstandingResult {
    pub turn_id: AgentTurnId,
    pub completion: AgentCompletion,
    pub report: MediaReport,
}
