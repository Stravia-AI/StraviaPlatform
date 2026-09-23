use crate::Gateway;
use crate::history_marker::{
    ClaimOutcome, HiddenHistorySegment, HistoryMarker, HistoryMarkerError, PlatformMarkerInput,
};
use crate::hook::DetachedPlatformExecution;
use crate::model_turn::TargetIdentity;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiItemAudience;
use stravia_runtime_contract::protocol::ir::AiItemProvenance;
use stravia_runtime_contract::protocol::ir::AiItemStatus;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MessageContent;

use super::ledger::RunLedger;
use super::projection::ProjectedDeltaBatch;
use super::{ClientProjectionSession, Phase, PhaseTracker};

/// Stream hooks may edit semantic deltas, but structural media events are
/// read-only. Reconcile by media order rather than item offsets: dropping text
/// may shift offsets, and replacing the whole Completed response would undo
/// those edits. The producer already stored these leaves; do not ingest again.
pub(super) fn reconcile_completed_media(
    response: &mut AiResponse,
    completed_items: Vec<AiItem>,
) -> Result<(), String> {
    let mut media = std::collections::VecDeque::new();
    for mut item in completed_items {
        if let MessageContent::Blocks(blocks) = &mut item.content {
            visit_completion_media(blocks, &mut |block| {
                media.push_back(std::mem::replace(
                    block,
                    ContentBlock::Text {
                        text: String::new(),
                        cache_control: None,
                    },
                ));
                Ok(())
            })?;
        }
    }
    for item in &mut response.items {
        if let MessageContent::Blocks(blocks) = &mut item.content {
            visit_completion_media(blocks, &mut |block| {
                let normalized = media
                    .pop_front()
                    .ok_or("stream media missing from Model Turn completion")?;
                if std::mem::discriminant(block) != std::mem::discriminant(&normalized) {
                    return Err("stream media differs from Model Turn completion".into());
                }
                *block = normalized;
                Ok(())
            })?;
        }
    }
    if !media.is_empty() {
        return Err("Model Turn completion media missing from stream".into());
    }
    Ok(())
}

fn visit_completion_media(
    blocks: &mut [ContentBlock],
    visit: &mut impl FnMut(&mut ContentBlock) -> Result<(), String>,
) -> Result<(), String> {
    use stravia_runtime_contract::protocol::ir::{DocumentSource, ToolResultContentKind};
    for block in blocks {
        match block {
            ContentBlock::Image { .. }
            | ContentBlock::Audio { .. }
            | ContentBlock::File { .. }
            | ContentBlock::Video { .. }
            | ContentBlock::Document {
                source: DocumentSource::Base64Pdf { .. } | DocumentSource::Url(_),
                ..
            } => visit(block)?,
            ContentBlock::Document {
                source: DocumentSource::Blocks { content },
                ..
            }
            | ContentBlock::SearchResult { content, .. } => visit_completion_media(content, visit)?,
            ContentBlock::ToolResult {
                content,
                content_kind: Some(ToolResultContentKind::ContentBlocks),
                ..
            }
            | ContentBlock::ServerToolResult {
                content,
                content_kind: Some(ToolResultContentKind::ContentBlocks),
                ..
            } => {
                let mut nested: Vec<ContentBlock> = serde_json::from_value(std::mem::take(content))
                    .map_err(|error| error.to_string())?;
                visit_completion_media(&mut nested, visit)?;
                *content = serde_json::to_value(nested).map_err(|error| error.to_string())?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone)]
struct GenerationChainCompletion {
    write: crate::generation_chain::GenerationChainWrite,
    source: crate::generation_chain::GenerationSource,
    owns_response_identity: bool,
    response_continuation_available: std::sync::Arc<std::sync::atomic::AtomicBool>,
    prior_publications: Vec<crate::plugin::VendorPublicationFence>,
    publication: Option<crate::model_turn::VendorPublication>,
}

#[derive(Clone)]
pub(super) struct CompletionContext {
    gateway: Gateway,
    actual_model: String,
    thinking_source: crate::history_marker::ThinkingSource,
    logical_model: String,
    principal: Principal,
    generation_chain: Option<GenerationChainCompletion>,
    model_turn_id: String,
    observer: crate::interaction_observation::RunObserver,
}

impl CompletionContext {
    pub(super) fn from_model_turn(
        gateway: Gateway,
        generation: super::GenerationChainRun,
        ingress: stravia_runtime_contract::protocol::ids::ProtocolId,
        target: &TargetIdentity,
        model_turn_id: String,
        observer: crate::interaction_observation::RunObserver,
    ) -> Self {
        let owns_response_identity = ingress == OPEN_RESPONSES_2026_04_24;
        let logical_model = generation.write.as_ref().map_or_else(
            || generation.client_request.model.clone(),
            |write| write.request().model.clone(),
        );
        let prior_publications = generation.vendor_publications.clone();
        let generation_chain = generation.write.map(|write| GenerationChainCompletion {
            write,
            source: crate::generation_chain::GenerationSource::Target {
                namespace: target.namespace.clone(),
                protocol: target.protocol_identity(),
                actual_model: target.actual_model.clone(),
                selected_target_key: target.target_id.clone(),
            },
            owns_response_identity,
            response_continuation_available: target.response_continuation_available.clone(),
            prior_publications: prior_publications.clone(),
            publication: target.publication.clone(),
        });
        Self {
            gateway,
            actual_model: target.actual_model.clone(),
            thinking_source: crate::history_marker::ThinkingSource {
                namespace: target.namespace.clone(),
                protocol: target.protocol_identity(),
                actual_model: target.actual_model.clone(),
                target_id: target.target_id.clone(),
            },
            logical_model,
            principal: generation.principal,
            generation_chain,
            model_turn_id,
            observer,
        }
    }

    pub(super) fn generation_chain_id(&self) -> Option<&str> {
        self.generation_chain
            .as_ref()
            .filter(|chain| chain.owns_response_identity)
            .map(|chain| chain.write.id())
    }

    pub(super) fn generation_chain_identity(&self) -> Option<(&str, &str)> {
        self.generation_chain_id()
            .map(|id| (id, self.logical_model.as_str()))
    }

    pub(super) fn principal(&self) -> &Principal {
        &self.principal
    }

    pub(super) fn gateway(&self) -> &Gateway {
        &self.gateway
    }

    pub(super) fn current_vendor_publication(
        &self,
    ) -> anyhow::Result<Option<crate::plugin::VendorPublicationFence>> {
        self.generation_chain
            .as_ref()
            .and_then(|chain| chain.publication.as_ref())
            .map(crate::model_turn::VendorPublication::current)
            .transpose()
    }

    pub(super) fn empty_response(&self) -> AiResponse {
        let model = if self.generation_chain.is_some() {
            &self.logical_model
        } else {
            &self.actual_model
        };
        AiResponse::new("", model)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientOutputCommit {
    Pending,
    Committed,
}

impl ClientOutputCommit {
    /// Client Output Commit is owned by the projection session: it flips when
    /// `report_delivery` confirms a `Sent` delivery, so the commit observed at
    /// any completion stage is the session's current value.
    pub(super) fn of(committed: bool) -> Self {
        if committed {
            Self::Committed
        } else {
            Self::Pending
        }
    }
}

pub(super) enum CompletionOutcome {
    PlatformOnly {
        continuation: Box<PlatformOnlyContinuation>,
        staged_delivery: ProjectedDeltaBatch,
    },
    Ready(Box<CompletionLease>),
    Failed(CompletionFailure),
}

pub(super) struct PlatformOnlyContinuation {
    projected_response: AiResponse,
    canonical_response: AiResponse,
    markers: Vec<PreparedPlatformMarker>,
    jobs: Vec<crate::HistoryMarkerExecutionJob>,
    started_executions: Vec<crate::StartedHistoryMarkerExecution>,
    commit: ClientOutputCommit,
}

impl PlatformOnlyContinuation {
    pub(super) async fn finish(
        self,
        context: &CompletionContext,
        ledger: &RunLedger,
        request: &mut AiRequest,
        run: &mut crate::hook::InferenceRun,
        phase: &mut PhaseTracker,
    ) -> Result<(), CompletionFailure> {
        ledger.record_hidden_round(&self.projected_response);
        context
            .gateway
            .run_history_marker_executions(context.principal.clone(), self.jobs, run)
            .await;
        context
            .gateway
            .run_started_history_marker_executions(self.started_executions, run)
            .await;
        let terminal = wait_platform_markers(context, &self.markers)
            .await
            .map_err(|error| CompletionFailure::hook(error, self.commit))?;
        append_restored_platform_round(request, &self.canonical_response, terminal);
        {
            let states = ledger
                .compaction_records
                .lock()
                .iter()
                .filter(|publication| {
                    publication.model_turn_id == context.model_turn_id
                        && matches!(
                            publication.mode,
                            crate::interaction_observation::CompactionMode::Inline
                        )
                })
                .map(|publication| publication.state.clone())
                .collect::<Vec<_>>();
            if let Some(start) =
                stravia_protocol_codec::codec::open_responses::inline_compaction_boundary(
                    &request.items,
                    &states,
                )
            {
                request.items.drain(..start);
                crate::router::clear_previous_response_id(request);
            }
        }
        run.next_round();
        phase
            .transition(Phase::HiddenRound)
            .map_err(|error| CompletionFailure::hook(error, self.commit))
    }
}

pub(super) struct CompletionLease {
    response: Box<AiResponse>,
    staged_delivery: ProjectedDeltaBatch,
    pending_generation_chain: Option<Box<super::PendingGenerationChainWrite>>,
    background_executions: Vec<crate::HistoryMarkerExecutionJob>,
    started_executions: Vec<crate::StartedHistoryMarkerExecution>,
    commit: ClientOutputCommit,
}

pub(super) struct PreparedDelivery {
    pub(super) response: AiResponse,
    pub(super) staged_delivery: ProjectedDeltaBatch,
    pub(super) pending_generation_chain: Option<super::PendingGenerationChainWrite>,
    pub(super) background_executions: Vec<crate::HistoryMarkerExecutionJob>,
    pub(super) started_executions: Vec<crate::StartedHistoryMarkerExecution>,
}

impl CompletionLease {
    pub(super) fn prepare(
        self,
        phase: &mut PhaseTracker,
    ) -> Result<PreparedDelivery, CompletionFailure> {
        phase
            .transition(Phase::AwaitingDelivery)
            .map_err(|error| CompletionFailure::hook(error, self.commit))?;
        Ok(PreparedDelivery {
            response: *self.response,
            staged_delivery: self.staged_delivery,
            pending_generation_chain: self.pending_generation_chain.map(|pending| *pending),
            background_executions: self.background_executions,
            started_executions: self.started_executions,
        })
    }
}

pub(super) enum CompletionFailure {
    Control(Box<stravia_runtime_contract::hook::HookControl>),
    Hook(String),
    AfterCommit(String),
}

impl CompletionFailure {
    pub(super) fn hook(error: impl std::fmt::Display, commit: ClientOutputCommit) -> Self {
        let message = error.to_string();
        match commit {
            ClientOutputCommit::Pending => Self::Hook(message),
            ClientOutputCommit::Committed => Self::AfterCommit(message),
        }
    }

    fn control(
        control: stravia_runtime_contract::hook::HookControl,
        commit: ClientOutputCommit,
    ) -> Self {
        match commit {
            ClientOutputCommit::Pending => Self::Control(Box::new(control)),
            ClientOutputCommit::Committed => Self::AfterCommit(
                "completion Hook controlled output after Client Output Commit".into(),
            ),
        }
    }

    fn hook_outcome(
        outcome: stravia_runtime_contract::hook::ResponseHookOutcome,
        commit: ClientOutputCommit,
        stage: &str,
    ) -> Result<(), Self> {
        if commit == ClientOutputCommit::Committed && outcome.modified {
            return Err(Self::AfterCommit(format!(
                "{stage} Hook modified output after Client Output Commit"
            )));
        }
        match outcome.control {
            stravia_runtime_contract::hook::HookControl::Continue => Ok(()),
            control => Err(Self::control(control, commit)),
        }
    }
}

pub(super) struct CompletionInput<'a> {
    pub(super) request: &'a mut AiRequest,
    pub(super) run: &'a mut crate::hook::InferenceRun,
    pub(super) phase: &'a mut PhaseTracker,
    pub(super) response: AiResponse,
    pub(super) upstream_response_id: Option<String>,
    pub(super) early_platform_executions: Vec<EarlyPlatformExecution>,
    pub(super) projection: &'a mut ClientProjectionSession,
    pub(super) ledger: &'a RunLedger,
}

pub(super) struct PreparedPlatformMarker {
    call_id: stravia_runtime_contract::protocol::ir::ToolCallId,
    marker: HistoryMarker,
}

impl PreparedPlatformMarker {
    pub(super) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(super) fn marker(&self) -> &HistoryMarker {
        &self.marker
    }
}

pub(super) struct EarlyPlatformExecution {
    pub(super) marker: PreparedPlatformMarker,
    pub(super) execution: crate::StartedHistoryMarkerExecution,
}

pub(super) async fn prepare_platform_markers(
    context: &CompletionContext,
    executions: Vec<DetachedPlatformExecution>,
) -> Result<
    (
        Vec<PreparedPlatformMarker>,
        Vec<crate::HistoryMarkerExecutionJob>,
    ),
    HistoryMarkerError,
> {
    const PENDING_RETENTION: std::time::Duration = std::time::Duration::from_secs(60 * 60);

    let vendor_publication = context
        .current_vendor_publication()
        .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?;
    let _vendor_guard = match vendor_publication.as_ref() {
        Some(publication) => Some(
            publication
                .write_fence()
                .await
                .map_err(|error| HistoryMarkerError::Storage(error.to_string()))?,
        ),
        None => None,
    };
    let mut pending =
        Vec::<(PreparedPlatformMarker, String, DetachedPlatformExecution)>::with_capacity(
            executions.len(),
        );
    for execution in executions {
        let marker = context
            .gateway
            .history_markers
            .create_platform(
                &context.principal,
                PlatformMarkerInput {
                    tool_id: execution.call().tool_id.to_string(),
                    call: execution.call().call.clone(),
                    activity: execution.activity().to_owned(),
                    execution_limit: execution.limit(),
                    pending_retention: PENDING_RETENTION,
                },
            )
            .await?;
        let owner_id = stravia_runtime_contract::identifier::new_id();
        pending.push((
            PreparedPlatformMarker {
                call_id: execution.call().call.id.clone(),
                marker,
            },
            owner_id,
            execution,
        ));
    }
    for (prepared, owner_id, execution) in &pending {
        let claim = context
            .gateway
            .history_markers
            .claim_execution(
                &context.principal,
                &prepared.marker.reference,
                owner_id,
                execution.limit(),
            )
            .await?;
        if claim != ClaimOutcome::Claimed {
            return Err(HistoryMarkerError::Storage(
                "new Platform Tool execution could not be claimed".into(),
            ));
        }
    }
    let mut prepared = Vec::with_capacity(pending.len());
    let mut jobs = Vec::with_capacity(pending.len());
    for (marker, owner_id, execution) in pending {
        let execution_deadline_unix_ms = context
            .gateway
            .history_markers
            .resolve(&context.principal, &marker.marker.reference)
            .await?
            .and_then(|resolved| resolved.execution_deadline_unix_ms)
            .ok_or_else(|| {
                HistoryMarkerError::Storage(
                    "claimed Platform execution is missing its persisted deadline".into(),
                )
            })?;
        jobs.push(crate::HistoryMarkerExecutionJob {
            marker_reference: marker.marker.reference.clone(),
            owner_id,
            execution_deadline_unix_ms,
            execution,
            observer: Some(context.observer.clone()),
            model_turn_id: context.model_turn_id.clone(),
        });
        prepared.push(marker);
    }
    Ok((prepared, jobs))
}

async fn wait_platform_markers(
    context: &CompletionContext,
    markers: &[PreparedPlatformMarker],
) -> Result<Vec<HiddenHistorySegment>, HistoryMarkerError> {
    let mut terminal = Vec::with_capacity(markers.len());
    for prepared in markers {
        let resolved = context
            .gateway
            .history_markers
            .wait_terminal(&context.principal, &prepared.marker.reference)
            .await?
            .ok_or_else(|| {
                HistoryMarkerError::Storage(
                    "published Platform History Marker became unavailable".into(),
                )
            })?;
        let segment = resolved.segment.ok_or(HistoryMarkerError::InvalidPayload)?;
        if !matches!(segment, HiddenHistorySegment::Platform { .. }) {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        terminal.push(segment);
    }
    Ok(terminal)
}

fn append_restored_platform_round(
    request: &mut AiRequest,
    response: &AiResponse,
    terminal: Vec<HiddenHistorySegment>,
) {
    request.items.extend(response.items.iter().cloned());
    request.items.extend(terminal.into_iter().map(|segment| {
        let HiddenHistorySegment::Platform { result, .. } = segment else {
            unreachable!("terminal Platform markers contain Platform segments");
        };
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            content_kind,
            is_error,
            cache_control,
        } = result
        else {
            unreachable!("Platform segments contain ToolResult blocks");
        };
        AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content,
                content_kind,
                is_error,
                cache_control,
            }]),
            tool_calls: None,
            tool_call_id: Some(tool_use_id),
            meta: None,
        }
    }));
}

pub(super) async fn complete_canonical_response(
    context: &CompletionContext,
    input: CompletionInput<'_>,
) -> CompletionOutcome {
    let CompletionInput {
        request,
        run,
        phase,
        mut response,
        upstream_response_id,
        early_platform_executions,
        projection,
        ledger,
    } = input;
    let commit = ClientOutputCommit::of(projection.client_output_committed());
    context.thinking_source.stamp_response(&mut response);
    fill_canonical_defaults(context, &mut response);
    let upstream_response = response.clone();

    if let Err(error) = phase.transition(Phase::Inspecting) {
        return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
    }
    match run
        .on_upstream_response_outcome(request, &mut response)
        .await
    {
        Ok(outcome) => {
            if let Err(failure) =
                CompletionFailure::hook_outcome(outcome, commit, "UpstreamResponse")
            {
                return CompletionOutcome::Failed(failure);
            }
        }
        Err(error) => {
            return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
        }
    }

    match run.on_client_output_outcome(&mut response).await {
        Ok(outcome) => {
            if let Err(failure) = CompletionFailure::hook_outcome(outcome, commit, "ClientOutput") {
                return CompletionOutcome::Failed(failure);
            }
        }
        Err(error) => {
            return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
        }
    }
    let observer = context.observer.clone();
    let classified = run.classify_tool_calls(&response);
    let has_platform_calls = !classified.platform.is_empty();
    let has_client_calls = !classified.client.is_empty();
    for call in &classified.client {
        observer.record(
            crate::interaction_observation::RunEvent::ClientToolHandoff {
                tool_id: call.id.to_string(),
                name: call.name.clone(),
                input: Some(
                    serde_json::from_str(&call.arguments)
                        .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
                ),
            },
        );
    }
    if has_client_calls {
        ledger.terminal.mark_waiting_client();
    }
    let canonical_response = response.clone();
    let mut started_executions = Vec::new();
    let mut prepared_platform = Vec::new();
    let mut platform_jobs = Vec::new();
    if has_platform_calls {
        let early_call_ids = early_platform_executions
            .iter()
            .map(|execution| execution.marker.call_id().to_owned())
            .collect::<std::collections::HashSet<_>>();
        let executions = classified
            .platform
            .into_iter()
            .filter(|call| !early_call_ids.contains(call.call.id.as_str()))
            .map(|call| {
                run.detached_platform_execution(
                    call,
                    stravia_runtime_contract::CancellationToken::new(),
                )
            })
            .collect();
        let (prepared, jobs) = match prepare_platform_markers(context, executions).await {
            Ok(prepared) => prepared,
            Err(error) => {
                return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
            }
        };
        prepared_platform = early_platform_executions
            .into_iter()
            .map(|early| {
                started_executions.push(early.execution);
                early.marker
            })
            .chain(prepared)
            .collect::<Vec<_>>();
        platform_jobs = jobs;
    }
    let platform = prepared_platform
        .iter()
        .map(|marker| (marker.call_id(), marker.marker()))
        .collect::<Vec<_>>();
    let staged_delivery = match projection.project_staged(&mut response, &platform).await {
        Ok(batch) => batch,
        Err(error) => {
            return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
        }
    };
    if has_platform_calls && !has_client_calls {
        return CompletionOutcome::PlatformOnly {
            continuation: Box::new(PlatformOnlyContinuation {
                projected_response: response,
                canonical_response,
                markers: prepared_platform,
                jobs: platform_jobs,
                started_executions,
                commit,
            }),
            staged_delivery,
        };
    }
    let background_executions = platform_jobs;
    ledger.apply_hidden_rounds(&mut response);
    if let Err(error) = phase.transition(Phase::SemanticComplete) {
        return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
    }

    let mut generation_chain = context.generation_chain.clone();
    if let Some(chain) = generation_chain.as_mut() {
        run.remove_exposed_tools(chain.write.request_mut());
        chain
            .write
            .record_inline_publications(&ledger.compaction_records.lock());
    }
    let reusable_upstream_id = generation_chain
        .as_ref()
        .is_some_and(|chain| {
            upstream_response_is_available(
                chain.write.request(),
                &chain.response_continuation_available,
            ) && crate::generation_chain::generation_node_is_completed(&response)
                && response_preserves_upstream(&upstream_response, &canonical_response)
        })
        .then_some(upstream_response_id)
        .flatten();

    let pending_generation_chain = if let Some(mut chain) = generation_chain.take() {
        let mut vendor_publications = chain.prior_publications;
        if let Some(publication) = chain.publication {
            let publication = match publication.current() {
                Ok(publication) => publication,
                Err(error) => {
                    return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
                }
            };
            if let Err(error) = publication.ensure_current() {
                return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
            }
            vendor_publications.push(publication);
        }
        chain
            .write
            .stage(&mut response, &chain.source, reusable_upstream_id)
            .then_some(super::PendingGenerationChainWrite {
                write: chain.write,
                vendor_publications,
            })
    } else {
        None
    };
    CompletionOutcome::Ready(Box::new(CompletionLease {
        response: Box::new(response),
        staged_delivery,
        commit,
        pending_generation_chain: pending_generation_chain.map(Box::new),
        background_executions,
        started_executions,
    }))
}

fn upstream_response_is_available(
    request: &AiRequest,
    response_continuation_available: &std::sync::atomic::AtomicBool,
) -> bool {
    crate::generation_chain::request_preserves_upstream_response(request)
        || response_continuation_available.load(std::sync::atomic::Ordering::Acquire)
}

fn response_preserves_upstream(original: &AiResponse, candidate: &AiResponse) -> bool {
    original.items.len() == candidate.items.len()
        && original
            .items
            .iter()
            .zip(&candidate.items)
            .all(|(left, right)| {
                stravia_runtime_contract::protocol::ir::canonical::item_hash(left)
                    == stravia_runtime_contract::protocol::ir::canonical::item_hash(right)
            })
}

fn fill_canonical_defaults(context: &CompletionContext, response: &mut AiResponse) {
    if response.id.is_empty() {
        response.id = stravia_runtime_contract::identifier::new_id();
    }
    if response.model.is_empty() {
        response.model.clone_from(&context.actual_model);
    }
    if response.stop_reason.is_none() {
        response.stop_reason = Some("stop".into());
    }
    if let Some(response_id) = context.generation_chain_id() {
        response.id = response_id.to_owned();
        response.model.clone_from(&context.logical_model);
        let terminal_item_status = response_item_default_status(response);
        for (index, item) in response.items.iter_mut().enumerate() {
            if item.is_compaction() || item.is_compaction_trigger() {
                continue;
            }
            let prefix = if item.thinking_ref().is_some() || item.reasoning_ref().is_some() {
                "rs"
            } else if item.function_call_ref().is_some() {
                "fc"
            } else if item.function_call_output_ref().is_some() {
                "fco"
            } else if item.unknown_ref().is_some() {
                "item"
            } else {
                "msg"
            };
            item.set_graph_metadata(
                Some(
                    stravia_protocol_codec::codec::open_responses::formatter::gateway_item_id(
                        prefix,
                        response_id,
                        index,
                    ),
                ),
                item.status().or(Some(terminal_item_status)),
                if item
                    .unknown_ref()
                    .and_then(|raw| raw.get("type"))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|item_type| item_type.starts_with("stravia:"))
                {
                    AiItemProvenance::Platform
                } else {
                    AiItemProvenance::Provider
                },
                AiItemAudience::Client,
            );
        }
    }
}

fn response_item_default_status(response: &AiResponse) -> AiItemStatus {
    response
        .vendor
        .egress
        .get("__open_responses_terminal")
        .and_then(serde_json::Value::as_object)
        .and_then(|terminal| terminal.get("status"))
        .and_then(serde_json::Value::as_str)
        .and_then(|status| match status {
            "incomplete" => Some(AiItemStatus::Incomplete),
            "failed" => Some(AiItemStatus::Failed),
            "completed" => Some(AiItemStatus::Completed),
            _ => None,
        })
        .unwrap_or(AiItemStatus::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_response_defaults_items_to_incomplete() {
        let mut response = AiResponse::new("response", "model");
        response.vendor.egress.insert(
            "__open_responses_terminal".into(),
            serde_json::json!({
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"}
            }),
        );

        assert_eq!(
            response_item_default_status(&response),
            AiItemStatus::Incomplete
        );
    }

    #[test]
    fn ephemeral_response_reuse_requires_confirmed_reusable_websocket() {
        let mut request = AiRequest::new("model", Vec::new());
        request.ext = Some(
            stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
                stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                    store: Some(false),
                    ..Default::default()
                },
            ),
        );
        let unavailable = std::sync::atomic::AtomicBool::new(false);
        let available = std::sync::atomic::AtomicBool::new(true);

        assert!(!upstream_response_is_available(&request, &unavailable));
        assert!(upstream_response_is_available(&request, &available));
    }
}
