use std::collections::BTreeMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::hook::Principal;
use crate::proxy::context::CancellationToken;
use crate::turn_chain::TurnNodeId;

pub type SearchTurnId = TurnNodeId;

#[derive(Debug, Clone)]
pub struct WebSearchInput {
    pub principal: Principal,
    pub query: String,
    pub previous_turn_id: Option<SearchTurnId>,
    pub policy: Option<WebSearchRunPolicy>,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchRunPolicy {
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub blocked_domains: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchCompletion {
    Complete,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPartialCause {
    WorkingBudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchReport {
    pub answer: String,
    pub sources: Vec<SearchSource>,
    #[serde(default)]
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchSource {
    pub id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchEvidence {
    pub url: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchEvidenceSet {
    pub(crate) by_url: BTreeMap<String, Option<String>>,
}

impl SearchEvidenceSet {
    pub fn from_evidence(evidence: impl IntoIterator<Item = SearchEvidence>) -> Self {
        evidence.into_iter().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = SearchEvidence> + '_ {
        self.by_url.iter().map(|(url, title)| SearchEvidence {
            url: url.clone(),
            title: title.clone(),
        })
    }

    pub(crate) fn extend(&mut self, other: impl IntoIterator<Item = SearchEvidence>) {
        for item in other {
            if let Ok(url) = super::validator::normalize_public_url(&item.url) {
                self.by_url.entry(url).or_insert(item.title);
            }
        }
    }
}

impl FromIterator<SearchEvidence> for SearchEvidenceSet {
    fn from_iter<T: IntoIterator<Item = SearchEvidence>>(iter: T) -> Self {
        let mut evidence = Self::default();
        evidence.extend(iter);
        evidence
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub turn_id: SearchTurnId,
    pub completion: SearchCompletion,
    pub report: SearchReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchBackendKind {
    Local,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSearchBinding {
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSearchBinding {
    pub provider_id: String,
    pub upstream_model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebSearchBackendDraft {
    Local {
        model_id: Option<String>,
    },
    Codex {
        provider_id: Option<String>,
        upstream_model: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolvedWebSearchBackend {
    Local {
        model_id: String,
    },
    Codex {
        provider_id: String,
        upstream_model: String,
    },
}

impl ResolvedWebSearchBackend {
    pub fn kind(&self) -> WebSearchBackendKind {
        match self {
            Self::Local { .. } => WebSearchBackendKind::Local,
            Self::Codex { .. } => WebSearchBackendKind::Codex,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchConfig {
    pub revision: u64,
    pub enabled: bool,
    pub backend: Option<WebSearchBackendDraft>,
    pub max_turns: u32,
    pub total_time_seconds: u64,
    pub updated_at: String,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: false,
            backend: None,
            max_turns: 12,
            total_time_seconds: 600,
            updated_at: String::new(),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
#[error("{message}")]
pub struct WebSearchError {
    pub backend: Option<WebSearchBackendKind>,
    pub code: String,
    pub message: String,
}

impl WebSearchError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            backend: None,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn backend(
        backend: WebSearchBackendKind,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            backend: Some(backend),
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchPhase {
    Started,
    Searching,
    Synthesizing,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchEvent {
    RunStarted {
        turn_id: SearchTurnId,
    },
    Progress {
        call_id: String,
        phase: WebSearchPhase,
        ordinal: u32,
    },
    Completed(WebSearchResult),
    Partial(WebSearchResult),
    Failed(WebSearchError),
}

impl WebSearchEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed(_) | Self::Partial(_) | Self::Failed(_)
        )
    }
}
