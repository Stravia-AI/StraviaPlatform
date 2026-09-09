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

    pub fn from_budget(budget: u32) -> Self {
        if budget == 0 {
            return Self::Off;
        }
        const RUNGS: [(u32, ThinkingLevel); 4] = [
            (1024, ThinkingLevel::Minimal),
            (2048, ThinkingLevel::Low),
            (8192, ThinkingLevel::Medium),
            (16384, ThinkingLevel::High),
        ];
        RUNGS
            .into_iter()
            .min_by_key(|(rung, _)| (budget.abs_diff(*rung), std::cmp::Reverse(*rung)))
            .map(|(_, level)| level)
            .unwrap_or(Self::High)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetThinkingControl {
    Effort { value: String },
    Budget { value: u32 },
    Enabled,
    Disabled,
    Hidden,
}

impl TargetThinkingControl {
    pub fn is_hidden(&self) -> bool {
        matches!(self, Self::Hidden)
    }
}
