use serde::{Deserialize, Serialize};

use stravia_runtime_contract::agent::{AgentCompletion, AgentTurnId};
use stravia_runtime_contract::artifact::ArtifactId;

/// 理解能力的输入模态。每种模态绑定独立的内部 Agent Definition、模型与思考等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// 静态图片与 Office 文档（文档内嵌图片同样走图片模型）。
    Image,
    Audio,
}

impl MediaKind {
    pub const ALL: [MediaKind; 2] = [MediaKind::Image, MediaKind::Audio];

    pub fn definition_id(self) -> &'static str {
        match self {
            Self::Image => crate::definition::MEDIA_DEFINITION_ID,
            Self::Audio => crate::definition::AUDIO_DEFINITION_ID,
        }
    }

    pub fn from_definition_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.definition_id() == id)
    }

    /// 依据 Artifact MIME 归类；音频以外一律交给图片/文档路径，由其预处理给出明确的不支持错误。
    pub fn of_mime(mime_type: &str) -> Self {
        let base = mime_type.split(';').next().unwrap_or_default().trim();
        if base
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("audio/"))
        {
            Self::Audio
        } else {
            Self::Image
        }
    }
}

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
