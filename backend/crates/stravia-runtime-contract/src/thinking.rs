use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn from_wire(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => anyhow::bail!("unsupported Thinking Level: {value}"),
        }
    }

    pub fn clamp(self, supported: &[Self]) -> Option<Self> {
        let supported = supported.iter().copied().collect::<BTreeSet<_>>();
        if supported.contains(&self) {
            return Some(self);
        }
        Self::ALL
            .into_iter()
            .filter(|level| *level > self)
            .find(|level| supported.contains(level))
            .or_else(|| {
                Self::ALL
                    .into_iter()
                    .rev()
                    .filter(|level| *level < self)
                    .find(|level| supported.contains(level))
            })
    }
}

/// Whether an upstream effort string represents turning thinking off.
pub fn effort_is_off(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.eq_ignore_ascii_case("none") || trimmed.eq_ignore_ascii_case("off")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetThinkingControl {
    Effort { value: String },
    Hidden,
}

impl TargetThinkingControl {
    pub fn is_hidden(&self) -> bool {
        matches!(self, Self::Hidden)
    }

    pub fn is_off(&self) -> bool {
        matches!(self, Self::Effort { value } if effort_is_off(value))
    }

    pub fn effort_value(&self) -> Option<&str> {
        match self {
            Self::Effort { value } => Some(value.as_str()),
            Self::Hidden => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Effort { .. } => "effort",
            Self::Hidden => "hidden",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_off_check_ignores_whitespace_and_ascii_case() {
        for raw in ["none", "NONE", " off ", "Off", "\tNone\n"] {
            assert!(effort_is_off(raw), "{raw:?}");
            assert!(
                TargetThinkingControl::Effort { value: raw.into() }.is_off(),
                "{raw:?}"
            );
        }
        for raw in ["low", "high", "minimal", "", "disable"] {
            assert!(!effort_is_off(raw), "{raw:?}");
            assert!(
                !TargetThinkingControl::Effort { value: raw.into() }.is_off(),
                "{raw:?}"
            );
        }
        assert!(!TargetThinkingControl::Hidden.is_off());
    }

    #[test]
    fn public_target_control_rejects_removed_legacy_variants() {
        for legacy in [
            r#"{"type":"budget","value":1024}"#,
            r#"{"type":"enabled"}"#,
            r#"{"type":"disabled"}"#,
        ] {
            assert!(
                serde_json::from_str::<TargetThinkingControl>(legacy).is_err(),
                "public serde must reject legacy control {legacy}"
            );
        }
        let effort: TargetThinkingControl =
            serde_json::from_str(r#"{"type":"effort","value":"high"}"#).unwrap();
        assert_eq!(
            effort,
            TargetThinkingControl::Effort {
                value: "high".into()
            }
        );
        let hidden: TargetThinkingControl = serde_json::from_str(r#"{"type":"hidden"}"#).unwrap();
        assert_eq!(hidden, TargetThinkingControl::Hidden);
    }

    #[test]
    fn protocol_represents_target_thinking_control_covers_effort_and_hidden() {
        use crate::protocol::ids::Protocol;
        let effort = TargetThinkingControl::Effort {
            value: "high".into(),
        };
        for protocol in [
            Protocol::OpenResponses,
            Protocol::OpenAICompatible,
            Protocol::AnthropicMessages,
            Protocol::GoogleGemini,
            Protocol::BedrockConverse,
            Protocol::CohereChat,
            Protocol::GatewayLanguageModel,
        ] {
            assert!(
                protocol.represents_target_thinking_control(&TargetThinkingControl::Hidden),
                "{protocol:?} must allow hidden"
            );
            assert!(
                protocol.represents_target_thinking_control(&effort),
                "{protocol:?} must allow effort"
            );
        }
        assert!(
            Protocol::WatsonxTextChat
                .represents_target_thinking_control(&TargetThinkingControl::Hidden)
        );
        assert!(!Protocol::WatsonxTextChat.represents_target_thinking_control(&effort));
    }
}
