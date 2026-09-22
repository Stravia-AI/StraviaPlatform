use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use super::{
    BackendOutput, SearchBackend, SearchBackendInput, SearchCompletion, SearchEvidence,
    SearchEvidenceSet, SearchReport, SearchSource, WebSearchBackendKind, WebSearchError,
};
use crate::host::ExternalSearchHost;

#[derive(Clone)]
pub struct ExternalSearchBackend {
    host: Arc<dyn ExternalSearchHost>,
}

impl ExternalSearchBackend {
    pub fn new(host: Arc<dyn ExternalSearchHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl SearchBackend for ExternalSearchBackend {
    fn kind(&self) -> WebSearchBackendKind {
        WebSearchBackendKind::External
    }

    async fn run(&self, input: SearchBackendInput) -> Result<BackendOutput, WebSearchError> {
        let super::ResolvedWebSearchBackend::External { route_id } = &input.binding else {
            return Err(external_error(
                "invalid_binding",
                "External Search Route binding is invalid",
            ));
        };
        if !input.ancestors.is_empty() {
            return Err(external_error(
                "continuation_unsupported",
                "External Search does not support continuing a previous report",
            ));
        }

        let execution = self
            .host
            .execute_external_search(
                &input.principal,
                route_id,
                stravia_vendor_sdk::SearchRequest {
                    query: input.query,
                    continuation: None,
                    allowed_domains: input.policy.allowed_domains,
                    language: None,
                    max_sources: Some(20),
                },
                input.cancellation,
                input.deadline,
            )
            .await?;
        let (report, evidence) = normalize_response(&input.turn_id, &execution.response)?;

        Ok(BackendOutput {
            completion: SearchCompletion::Complete,
            partial_cause: None,
            report,
            evidence,
            usage: execution.response.usage,
            model_turns: 0,
            tool_calls: 0,
            publication: Some(execution.publication),
            provider_id: Some(execution.provider_id),
            upstream_model: execution.upstream_model,
            target_id: Some(execution.target_id),
        })
    }
}

fn normalize_response(
    turn_id: &super::SearchTurnId,
    response: &stravia_vendor_sdk::SearchResponse,
) -> Result<(SearchReport, SearchEvidenceSet), WebSearchError> {
    if response.sources.is_empty() {
        return Err(external_error(
            "invalid_report",
            "External Search returned no cited sources",
        ));
    }

    let mut ids = HashSet::new();
    let mut replacements = HashMap::new();
    let mut sources = Vec::with_capacity(response.sources.len());
    let mut evidence = Vec::with_capacity(response.sources.len());
    for (index, source) in response.sources.iter().enumerate() {
        let source_id = source.id.trim();
        if source_id.is_empty() || !ids.insert(source_id.to_owned()) {
            return Err(external_error(
                "invalid_report",
                "External Search returned invalid source identities",
            ));
        }
        let path = format!("{}/sources/{}", turn_id.reference(), index + 1);
        replacements.insert(format!("[sc:{source_id}]"), format!("[{path}]"));
        sources.push(SearchSource {
            path,
            url: source.url.clone(),
            title: source.title.clone(),
        });
        evidence.push(SearchEvidence {
            url: source.url.clone(),
            title: source.title.clone(),
        });
    }

    let mut answer = response.answer.clone();
    for (marker, replacement) in replacements {
        answer = answer.replace(&marker, &replacement);
    }

    Ok((
        SearchReport {
            answer,
            sources,
            limitations: response.limitations.clone(),
        },
        SearchEvidenceSet::from_evidence(evidence),
    ))
}

fn external_error(code: &'static str, message: &'static str) -> WebSearchError {
    WebSearchError::backend(WebSearchBackendKind::External, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_source_ids_are_projected_as_turn_scoped_paths() {
        let response = stravia_vendor_sdk::SearchResponse {
            answer: "Verified claim [sc:upstream-source]".into(),
            sources: vec![stravia_vendor_sdk::SearchSource {
                id: "upstream-source".into(),
                url: "https://8.8.8.8/source".into(),
                title: Some("Source".into()),
                snippet: None,
                published_at: None,
            }],
            limitations: Vec::new(),
            usage: None,
        };

        let (report, _) = normalize_response(
            &super::super::SearchTurnId::new("abcdefghijklmnopqrstuvwxyzab"),
            &response,
        )
        .expect("external report");

        let path = "stravia://turns/abcdefghijklmnopqrstuvwxyzab/sources/1";
        assert_eq!(report.answer, format!("Verified claim [{path}]"));
        assert_eq!(report.sources[0].path, path);
    }
}
