use serde::{Deserialize, Serialize};

use stravia_runtime_contract::agent::{AgentCompletion, AgentTurnId};
use stravia_runtime_contract::artifact::ArtifactId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaArtifactInput {
    #[serde(
        rename = "path",
        with = "stravia_runtime_contract::artifact::serde_path"
    )]
    pub artifact_id: ArtifactId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaUnderstandingInput {
    pub prompt: String,
    #[serde(default)]
    pub artifacts: Vec<MediaArtifactInput>,
    #[serde(
        default,
        rename = "previous_path",
        with = "stravia_runtime_contract::turn_chain::serde_option_path"
    )]
    pub previous_turn_id: Option<AgentTurnId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaArtifactReference {
    #[serde(
        rename = "path",
        with = "stravia_runtime_contract::artifact::serde_path"
    )]
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
    #[serde(
        rename = "path",
        with = "stravia_runtime_contract::turn_chain::serde_path"
    )]
    pub turn_id: AgentTurnId,
    pub completion: AgentCompletion,
    pub artifacts: Vec<MediaArtifactReference>,
    pub report: MediaReport,
}
