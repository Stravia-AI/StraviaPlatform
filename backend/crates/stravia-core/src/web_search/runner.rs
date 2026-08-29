use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::config::{WebSearchConfigStore, resolve_enabled_config};
use super::{
    ResolvedWebSearchBackend, SearchCompletion, SearchEvidence, SearchEvidenceSet, SearchReport,
    SearchReportValidator, SearchTurnId, WebSearchBackendKind, WebSearchError, WebSearchEvent,
    WebSearchInput, WebSearchPhase, WebSearchResult, WebSearchRunPolicy,
};
use crate::hook::Principal;
use crate::protocol::ir::Usage;
use crate::proxy::context::CancellationToken;
use crate::turn_chain::{TurnChainStore, TurnCommit, TurnNodeKind};

const SEARCH_PAYLOAD_VERSION: u32 = 1;
const MAX_QUERY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct SearchAncestor {
    pub turn_id: SearchTurnId,
    pub query: String,
    pub policy: WebSearchRunPolicy,
    pub completion: SearchCompletion,
    pub report: SearchReport,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchBackendInput {
    pub turn_id: SearchTurnId,
    pub principal: Principal,
    pub query: String,
    pub policy: WebSearchRunPolicy,
    pub ancestors: Vec<SearchAncestor>,
    pub binding: ResolvedWebSearchBackend,
    pub definition_revision: Option<u32>,
    pub local_limits: Option<LocalSearchLimits>,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalSearchLimits {
    pub max_turns: u32,
    pub total_time: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct BackendOutput {
    pub completion: SearchCompletion,
    pub partial_cause: Option<super::SearchPartialCause>,
    pub report: SearchReport,
    pub evidence: SearchEvidenceSet,
    pub usage: Usage,
    pub model_turns: u32,
    pub tool_calls: u32,
}

struct SearchAudit {
    started_at: Instant,
    principal: String,
    parent_turn_id: Option<String>,
    turn_id: Option<String>,
    backend: Option<&'static str>,
    provider_id: Option<String>,
    model_id: Option<String>,
    config_revision: Option<u64>,
    definition_revision: Option<u32>,
    usage: Usage,
    model_turns: u32,
    tool_calls: u32,
}

impl SearchAudit {
    fn new(input: &WebSearchInput) -> Self {
        Self {
            started_at: Instant::now(),
            principal: input.principal.continuation_key(),
            parent_turn_id: input.previous_turn_id.as_ref().map(ToString::to_string),
            turn_id: None,
            backend: None,
            provider_id: None,
            model_id: None,
            config_revision: None,
            definition_revision: None,
            usage: Usage::default(),
            model_turns: 0,
            tool_calls: 0,
        }
    }

    fn bind(&mut self, turn_id: &SearchTurnId, snapshot: &SearchSnapshot) {
        self.turn_id = Some(turn_id.to_string());
        self.config_revision = Some(snapshot.config_revision);
        self.definition_revision = snapshot.definition_revision;
        match &snapshot.backend {
            ResolvedWebSearchBackend::Local { model_id } => {
                self.backend = Some("local");
                self.model_id = Some(model_id.clone());
            }
            ResolvedWebSearchBackend::Codex {
                provider_id,
                upstream_model,
            } => {
                self.backend = Some("codex");
                self.provider_id = Some(provider_id.clone());
                self.model_id = Some(upstream_model.clone());
            }
        }
    }

    fn emit(&self, result: &Result<WebSearchResult, WebSearchError>) {
        let (outcome, completion, error_code, error_backend) = match result {
            Ok(result) => ("completed", Some(result.completion), None, self.backend),
            Err(error) => (
                "failed",
                None,
                Some(error.code.as_str()),
                error.backend.map(|backend| match backend {
                    WebSearchBackendKind::Local => "local",
                    WebSearchBackendKind::Codex => "codex",
                }),
            ),
        };
        tracing::info!(
            target: "stravia::audit",
            event = "web_search",
            outcome,
            completion = ?completion,
            error_code,
            principal = %self.principal,
            search_turn_id = self.turn_id.as_deref(),
            parent_turn_id = self.parent_turn_id.as_deref(),
            backend = self.backend.or(error_backend),
            provider_id = self.provider_id.as_deref(),
            model_id = self.model_id.as_deref(),
            config_revision = self.config_revision,
            definition_revision = self.definition_revision,
            model_turns = self.model_turns,
            tool_calls = self.tool_calls,
            prompt_tokens = self.usage.prompt_tokens,
            completion_tokens = self.usage.completion_tokens,
            total_tokens = self.usage.total_tokens,
            elapsed_ms = self.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "Web Search terminal outcome"
        );
    }
}

#[async_trait]
pub(crate) trait SearchBackend: Send + Sync {
    fn kind(&self) -> WebSearchBackendKind;

    async fn run(&self, input: SearchBackendInput) -> Result<BackendOutput, WebSearchError>;
}

pub struct WebSearchEventStream {
    inner: Pin<Box<dyn Stream<Item = WebSearchEvent> + Send>>,
    cancellation: CancellationToken,
    terminal_seen: bool,
}

#[async_trait]
pub(crate) trait SearchRunAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        principal: &Principal,
        binding: &ResolvedWebSearchBackend,
    ) -> Result<(), WebSearchError>;
}

#[cfg(test)]
pub(crate) struct AllowSearchRun;

#[cfg(test)]
#[async_trait]
impl SearchRunAuthorizer for AllowSearchRun {
    async fn authorize(
        &self,
        _principal: &Principal,
        _binding: &ResolvedWebSearchBackend,
    ) -> Result<(), WebSearchError> {
        Ok(())
    }
}

impl Stream for WebSearchEventStream {
    type Item = WebSearchEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(event)) => {
                if event.is_terminal() {
                    self.terminal_seen = true;
                }
                Poll::Ready(Some(event))
            }
            other => other,
        }
    }
}

impl Drop for WebSearchEventStream {
    fn drop(&mut self) {
        if !self.terminal_seen {
            self.cancellation.cancel();
        }
    }
}

/// Executes one bounded, provider-neutral Web Search run.
///
/// Configuration and backend/model bindings are snapshotted at the root turn.
/// Continuations materialize the complete same-principal ancestor chain and
/// keep that root snapshot; no implicit latest turn or backend fallback exists.
/// Calling [`WebSearchRunner::run`] is lazy: work starts only when the
/// returned stream is polled, and dropping it before a terminal event cancels
/// the request-bound run.
#[derive(Clone)]
pub struct WebSearchRunner {
    config: Arc<dyn WebSearchConfigStore>,
    turns: Arc<dyn TurnChainStore>,
    local: Arc<dyn SearchBackend>,
    codex: Arc<dyn SearchBackend>,
    validator: Arc<SearchReportValidator>,
    turn_ttl: Duration,
    authorizer: Arc<dyn SearchRunAuthorizer>,
}

impl WebSearchRunner {
    pub(crate) fn new(
        config: Arc<dyn WebSearchConfigStore>,
        turns: Arc<dyn TurnChainStore>,
        local: Arc<dyn SearchBackend>,
        codex: Arc<dyn SearchBackend>,
        validator: Arc<SearchReportValidator>,
        turn_ttl: Duration,
        authorizer: Arc<dyn SearchRunAuthorizer>,
    ) -> Self {
        Self {
            config,
            turns,
            local,
            codex,
            validator,
            turn_ttl,
            authorizer,
        }
    }

    /// Starts a lazy Web Search event stream.
    ///
    /// The input requires an authenticated API-key [`Principal`], a non-empty
    /// query of at most 64 KiB in UTF-8, and an explicit deadline. Supplying
    /// `previous_turn_id` continues or branches that exact same-principal turn.
    /// An omitted policy inherits the direct parent's policy; a supplied policy
    /// replaces it. The stream ends in exactly one `Completed`, `Partial`, or
    /// `Failed` event. Completed and partial results carry a stable turn ID and
    /// a validated sourced Report; failures do not commit a turn.
    pub fn run(&self, input: WebSearchInput) -> WebSearchEventStream {
        let cancellation = input.cancellation.clone();
        let runner = self.clone();
        let (events, receiver) = mpsc::channel(32);
        let driver = stream::once(async move {
            let terminal = match runner.execute(input, &events).await {
                Ok(result) if result.completion == SearchCompletion::Complete => {
                    WebSearchEvent::Completed(result)
                }
                Ok(result) => WebSearchEvent::Partial(result),
                Err(error) => WebSearchEvent::Failed(error),
            };
            let _ = events.send(terminal).await;
            None::<WebSearchEvent>
        })
        .filter_map(futures::future::ready);
        WebSearchEventStream {
            inner: Box::pin(stream::select(ReceiverStream::new(receiver), driver)),
            cancellation,
            terminal_seen: false,
        }
    }

    async fn execute(
        &self,
        input: WebSearchInput,
        events: &mpsc::Sender<WebSearchEvent>,
    ) -> Result<WebSearchResult, WebSearchError> {
        let mut audit = SearchAudit::new(&input);
        let result = self.execute_inner(input, events, &mut audit).await;
        audit.emit(&result);
        result
    }

    async fn await_stage<T, E, F, M>(
        &self,
        input: &WebSearchInput,
        binding: &ResolvedWebSearchBackend,
        deadline: Instant,
        events: &mpsc::Sender<WebSearchEvent>,
        future: F,
        map_error: M,
    ) -> Result<T, WebSearchError>
    where
        F: Future<Output = Result<T, E>>,
        M: FnOnce(E) -> WebSearchError,
    {
        let authorization = self.authorizer.authorize(&input.principal, binding);
        tokio::pin!(authorization);
        tokio::select! {
            _ = events.closed() => {
                input.cancellation.cancel();
                return Err(WebSearchError::new("cancelled", "Web Search consumer disconnected"));
            }
            _ = input.cancellation.cancelled() => {
                return Err(WebSearchError::new("cancelled", "Web Search was cancelled"));
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                input.cancellation.cancel();
                return Err(WebSearchError::new("deadline_exceeded", "Web Search deadline exceeded"));
            }
            authorization = &mut authorization => authorization?,
        }

        let mut revalidation = tokio::time::interval(Duration::from_secs(1));
        revalidation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        revalidation.tick().await;
        tokio::pin!(future);
        let result = loop {
            tokio::select! {
                _ = events.closed() => {
                    input.cancellation.cancel();
                    return Err(WebSearchError::new("cancelled", "Web Search consumer disconnected"));
                }
                _ = input.cancellation.cancelled() => {
                    return Err(WebSearchError::new("cancelled", "Web Search was cancelled"));
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    input.cancellation.cancel();
                    return Err(WebSearchError::new("deadline_exceeded", "Web Search deadline exceeded"));
                }
                authorization = async {
                    revalidation.tick().await;
                    self.authorizer.authorize(&input.principal, binding).await
                } => {
                    authorization?;
                }
                result = &mut future => break result,
            }
        };
        result.map_err(map_error)
    }

    async fn execute_inner(
        &self,
        mut input: WebSearchInput,
        events: &mpsc::Sender<WebSearchEvent>,
        audit: &mut SearchAudit,
    ) -> Result<WebSearchResult, WebSearchError> {
        input.query = input.query.trim().to_owned();
        if input.query.is_empty() || input.query.len() > MAX_QUERY_BYTES {
            return Err(WebSearchError::new(
                "invalid_input",
                "Web Search query must contain between 1 byte and 64 KiB",
            ));
        }

        let parent_nodes = if let Some(parent) = input.previous_turn_id.as_ref() {
            self.turns
                .materialize(&input.principal, TurnNodeKind::WebSearch, parent)
                .await
                .map_err(|_| {
                    WebSearchError::new("turn_unavailable", "Previous Search Turn is unavailable")
                })?
        } else {
            Vec::new()
        };
        let ancestors = decode_ancestors(&parent_nodes)?;
        let snapshot = match ancestors.first() {
            Some(ancestor) => ancestor.snapshot.clone(),
            None => {
                let current_config = self.config.load().await?;
                let current_binding = resolve_enabled_config(&current_config)?;
                let definition_revision =
                    matches!(current_binding, ResolvedWebSearchBackend::Local { .. })
                        .then_some(super::LOCAL_SEARCH_DEFINITION_REVISION);
                SearchSnapshot {
                    backend: current_binding,
                    config_revision: current_config.revision,
                    definition_revision,
                    max_turns: current_config.max_turns,
                    total_time_seconds: current_config.total_time_seconds,
                }
            }
        };
        if ancestors
            .iter()
            .any(|ancestor| ancestor.snapshot != snapshot)
        {
            return Err(WebSearchError::new(
                "turn_unavailable",
                "Previous Search Turn is unavailable",
            ));
        }
        let policy = normalize_policy(match input.policy.clone() {
            Some(policy) => policy,
            None => ancestors
                .last()
                .map(|ancestor| ancestor.policy.clone())
                .unwrap_or_default(),
        })?;
        self.authorizer
            .authorize(&input.principal, &snapshot.backend)
            .await?;
        let turn_id = SearchTurnId::web_search();
        audit.bind(&turn_id, &snapshot);
        send_event(
            events,
            WebSearchEvent::RunStarted {
                turn_id: turn_id.clone(),
            },
        )
        .await?;
        send_event(
            events,
            WebSearchEvent::Progress {
                call_id: turn_id.to_string(),
                phase: WebSearchPhase::Started,
                ordinal: 1,
            },
        )
        .await?;

        let started_at = Instant::now();
        let (backend, deadline, local_limits) = match snapshot.backend.kind() {
            WebSearchBackendKind::Local => {
                let limits = LocalSearchLimits {
                    max_turns: snapshot.max_turns,
                    total_time: Duration::from_secs(snapshot.total_time_seconds),
                };
                (
                    Arc::clone(&self.local),
                    input.deadline.min(started_at + limits.total_time),
                    Some(limits),
                )
            }
            WebSearchBackendKind::Codex => (Arc::clone(&self.codex), input.deadline, None),
        };
        if backend.kind() != snapshot.backend.kind() {
            return Err(WebSearchError::new(
                "backend_unavailable",
                "Configured Search Backend is unavailable",
            ));
        }
        send_event(
            events,
            WebSearchEvent::Progress {
                call_id: turn_id.to_string(),
                phase: WebSearchPhase::Searching,
                ordinal: 2,
            },
        )
        .await?;
        let backend_input = SearchBackendInput {
            turn_id: turn_id.clone(),
            principal: input.principal.clone(),
            query: input.query.clone(),
            policy: policy.clone(),
            ancestors: ancestors.iter().map(DecodedAncestor::public).collect(),
            binding: snapshot.backend.clone(),
            definition_revision: snapshot.definition_revision,
            local_limits,
            cancellation: input.cancellation.clone(),
        };
        let output = self
            .await_stage(
                &input,
                &snapshot.backend,
                deadline,
                events,
                backend.run(backend_input),
                |mut error| {
                    if error.backend.is_none() {
                        error.backend = Some(backend.kind());
                    }
                    error
                },
            )
            .await?;
        send_event(
            events,
            WebSearchEvent::Progress {
                call_id: turn_id.to_string(),
                phase: WebSearchPhase::Synthesizing,
                ordinal: 3,
            },
        )
        .await?;

        let mut evidence =
            SearchEvidenceSet::from_evidence(ancestors.iter().flat_map(|ancestor| {
                ancestor.report.sources.iter().map(|source| SearchEvidence {
                    url: source.url.clone(),
                    title: source.title.clone(),
                })
            }));
        evidence.extend(output.evidence.iter());
        let report = self
            .await_stage(
                &input,
                &snapshot.backend,
                deadline,
                events,
                self.validator.validate(
                    &turn_id,
                    output.completion,
                    output.partial_cause,
                    output.report,
                    &evidence,
                ),
                |error| error,
            )
            .await?;
        let result = WebSearchResult {
            turn_id: turn_id.clone(),
            completion: output.completion,
            report,
        };
        audit.usage = output.usage.clone();
        audit.model_turns = output.model_turns;
        audit.tool_calls = output.tool_calls;
        let payload = SearchTurnPayload {
            query: input.query.clone(),
            policy: policy.clone(),
            snapshot: snapshot.clone(),
            completion: result.completion,
            report: result.report.clone(),
            usage: output.usage,
            elapsed_ms: started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
        };
        let commit = self.turns.commit(TurnCommit {
            id: turn_id,
            kind: TurnNodeKind::WebSearch,
            parent_id: input.previous_turn_id.clone(),
            principal: input.principal.clone(),
            payload_version: SEARCH_PAYLOAD_VERSION,
            payload: serde_json::to_value(&payload).map_err(|_| {
                WebSearchError::new("storage_failed", "Search Turn could not be encoded")
            })?,
            idle_ttl: self.turn_ttl,
            reusable_prefix: None,
        });
        self.await_stage(&input, &snapshot.backend, deadline, events, commit, |_| {
            WebSearchError::new("storage_failed", "Search Turn could not be committed")
        })
        .await?;

        send_event(
            events,
            WebSearchEvent::Progress {
                call_id: result.turn_id.to_string(),
                phase: WebSearchPhase::Completed,
                ordinal: 4,
            },
        )
        .await?;
        Ok(result)
    }
}

async fn send_event(
    events: &mpsc::Sender<WebSearchEvent>,
    event: WebSearchEvent,
) -> Result<(), WebSearchError> {
    events
        .send(event)
        .await
        .map_err(|_| WebSearchError::new("cancelled", "Web Search consumer disconnected"))
}

fn normalize_policy(mut policy: WebSearchRunPolicy) -> Result<WebSearchRunPolicy, WebSearchError> {
    if policy.allowed_domains.len() > 20 || policy.blocked_domains.len() > 20 {
        return Err(WebSearchError::new(
            "invalid_input",
            "Domain filters cannot contain more than 20 entries",
        ));
    }
    policy.allowed_domains = crate::web_access::normalize_domains(policy.allowed_domains)
        .map_err(|_| WebSearchError::new("invalid_input", "Invalid allowed domain policy"))?;
    policy.blocked_domains = crate::web_access::normalize_domains(policy.blocked_domains)
        .map_err(|_| WebSearchError::new("invalid_input", "Invalid blocked domain policy"))?;
    if policy
        .allowed_domains
        .iter()
        .any(|domain| policy.blocked_domains.contains(domain))
    {
        return Err(WebSearchError::new(
            "invalid_input",
            "A domain cannot be both allowed and blocked",
        ));
    }
    Ok(policy)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SearchSnapshot {
    backend: ResolvedWebSearchBackend,
    config_revision: u64,
    definition_revision: Option<u32>,
    max_turns: u32,
    total_time_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchTurnPayload {
    query: String,
    policy: WebSearchRunPolicy,
    snapshot: SearchSnapshot,
    completion: SearchCompletion,
    report: SearchReport,
    usage: Usage,
    elapsed_ms: u64,
}

#[derive(Debug, Clone)]
struct DecodedAncestor {
    turn_id: SearchTurnId,
    query: String,
    policy: WebSearchRunPolicy,
    snapshot: SearchSnapshot,
    completion: SearchCompletion,
    report: SearchReport,
}

impl DecodedAncestor {
    fn public(&self) -> SearchAncestor {
        SearchAncestor {
            turn_id: self.turn_id.clone(),
            query: self.query.clone(),
            policy: self.policy.clone(),
            completion: self.completion,
            report: self.report.clone(),
        }
    }
}

fn decode_ancestors(
    nodes: &[crate::turn_chain::TurnNode],
) -> Result<Vec<DecodedAncestor>, WebSearchError> {
    nodes
        .iter()
        .map(|node| {
            if node.payload_version != SEARCH_PAYLOAD_VERSION {
                return Err(WebSearchError::new(
                    "turn_unavailable",
                    "Previous Search Turn is unavailable",
                ));
            }
            let payload: SearchTurnPayload =
                serde_json::from_value(node.payload.clone()).map_err(|_| {
                    WebSearchError::new("turn_unavailable", "Previous Search Turn is unavailable")
                })?;
            Ok(DecodedAncestor {
                turn_id: node.id.clone(),
                query: payload.query,
                policy: payload.policy,
                snapshot: payload.snapshot,
                completion: payload.completion,
                report: payload.report,
            })
        })
        .collect()
}
