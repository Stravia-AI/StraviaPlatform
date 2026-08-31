use crate::Gateway;
use crate::history_marker::{
    ClaimOutcome, HiddenHistorySegment, HistoryMarker, HistoryMarkerError, PlatformMarkerInput,
    ThinkingMarkerInput, render_history_marker,
};
use crate::hook::{DetachedPlatformExecution, Principal};
use crate::model_turn::TargetIdentity;
use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
use crate::protocol::ir::{
    AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, AiRequest, AiResponse, ContentBlock,
    MessageContent, Usage,
};
use crate::proxy::context::RequestContext;

use super::{ClientProjector, Phase, PhaseTracker, is_protected_thinking};

#[derive(Clone)]
struct GenerationChainCompletion {
    write: crate::generation_chain::GenerationChainWrite,
    target_namespace: String,
    target_protocol: crate::protocol::ids::ProtocolId,
    actual_model: String,
    owns_response_identity: bool,
    response_continuation_available: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone)]
pub(super) struct CompletionContext {
    gateway: Gateway,
    actual_model: String,
    logical_model: String,
    principal: Principal,
    ingress: crate::protocol::ids::ProtocolId,
    generation_chain: Option<GenerationChainCompletion>,
    client_output_commit: ClientOutputCommit,
}

impl CompletionContext {
    pub(super) fn from_model_turn(
        gateway: Gateway,
        generation: super::GenerationChainRun,
        ingress: crate::protocol::ids::ProtocolId,
        target: &TargetIdentity,
        egress: crate::protocol::ids::ProtocolId,
    ) -> Self {
        let owns_response_identity = ingress == OPEN_RESPONSES_2026_04_24;
        let logical_model = generation.write.as_ref().map_or_else(
            || generation.client_request.model.clone(),
            |write| write.request().model.clone(),
        );
        let generation_chain = generation.write.map(|write| GenerationChainCompletion {
            write,
            target_namespace: target.namespace.clone(),
            target_protocol: egress,
            actual_model: target.actual_model.clone(),
            owns_response_identity,
            response_continuation_available: target.response_continuation_available.clone(),
        });
        Self {
            gateway,
            actual_model: target.actual_model.clone(),
            logical_model,
            principal: generation.principal,
            ingress,
            generation_chain,
            client_output_commit: ClientOutputCommit::Pending,
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

    pub(super) fn mark_client_output_committed(&mut self) {
        self.client_output_commit = ClientOutputCommit::Committed;
    }

    pub(super) fn client_output_commit(&self) -> ClientOutputCommit {
        self.client_output_commit
    }

    pub(super) fn principal(&self) -> &Principal {
        &self.principal
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

pub(super) enum CompletionOutcome {
    PlatformOnly(Box<PlatformOnlyContinuation>),
    Ready(Box<CompletionLease>),
    Failed(CompletionFailure),
}

pub(super) struct PlatformOnlyContinuation {
    projected_response: AiResponse,
    canonical_response: AiResponse,
    markers: Vec<PreparedPlatformMarker>,
    jobs: Vec<crate::HistoryMarkerExecutionJob>,
    started_executions: Vec<crate::StartedHistoryMarkerExecution>,
    publish_references: Vec<String>,
}

impl PlatformOnlyContinuation {
    pub(super) fn projected_response(&self) -> &AiResponse {
        &self.projected_response
    }

    pub(super) async fn publish(
        &self,
        context: &CompletionContext,
    ) -> Result<(), CompletionFailure> {
        publish_markers(context, &self.publish_references)
            .await
            .map_err(|error| CompletionFailure::hook(error, context.client_output_commit))
    }

    pub(super) async fn finish(
        self,
        context: &CompletionContext,
        request_context: &RequestContext,
        request: &mut AiRequest,
        run: &mut crate::hook::InferenceRun,
        phase: &mut PhaseTracker,
    ) -> Result<(), CompletionFailure> {
        record_hidden_round(request_context, &self.projected_response);
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
            .map_err(|error| CompletionFailure::hook(error, context.client_output_commit))?;
        append_restored_platform_round(request, &self.canonical_response, terminal);
        run.next_round();
        phase
            .transition(Phase::HiddenRound)
            .map_err(|error| CompletionFailure::hook(error, context.client_output_commit))
    }
}

pub(super) struct CompletionLease {
    response: Box<AiResponse>,
    pending_generation_chain: Option<Box<crate::generation_chain::GenerationChainWrite>>,
    background_executions: Vec<crate::HistoryMarkerExecutionJob>,
    started_executions: Vec<crate::StartedHistoryMarkerExecution>,
    publish_references: Vec<String>,
    commit: ClientOutputCommit,
}

pub(super) struct PreparedDelivery {
    pub(super) response: AiResponse,
    pub(super) pending_generation_chain: Option<crate::generation_chain::GenerationChainWrite>,
    pub(super) background_executions: Vec<crate::HistoryMarkerExecutionJob>,
    pub(super) started_executions: Vec<crate::StartedHistoryMarkerExecution>,
    pub(super) publish_references: Vec<String>,
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
            pending_generation_chain: self.pending_generation_chain.map(|pending| *pending),
            background_executions: self.background_executions,
            started_executions: self.started_executions,
            publish_references: self.publish_references,
        })
    }
}

pub(super) enum CompletionFailure {
    Control(Box<crate::hook::HookControl>),
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

    fn control(control: crate::hook::HookControl, commit: ClientOutputCommit) -> Self {
        match commit {
            ClientOutputCommit::Pending => Self::Control(Box::new(control)),
            ClientOutputCommit::Committed => Self::AfterCommit(
                "completion Hook controlled output after Client Output Commit".into(),
            ),
        }
    }

    fn hook_outcome(
        outcome: crate::hook::ResponseHookOutcome,
        commit: ClientOutputCommit,
        stage: &str,
    ) -> Result<(), Self> {
        if commit == ClientOutputCommit::Committed && outcome.modified {
            return Err(Self::AfterCommit(format!(
                "{stage} Hook modified output after Client Output Commit"
            )));
        }
        match outcome.control {
            crate::hook::HookControl::Continue => Ok(()),
            control => Err(Self::control(control, commit)),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct HiddenRoundState {
    pub(super) items: Vec<crate::protocol::ir::AiItem>,
    pub(super) usage: Usage,
    pub(super) round_count: u32,
}

#[derive(Clone, Default)]
pub(super) struct PublishedPlatformExecutions {
    pub(super) references: Vec<String>,
}

pub(super) struct CompletionInput<'a> {
    pub(super) request_context: &'a RequestContext,
    pub(super) request: &'a mut AiRequest,
    pub(super) run: &'a mut crate::hook::InferenceRun,
    pub(super) phase: &'a mut PhaseTracker,
    pub(super) response: AiResponse,
    pub(super) upstream_response_id: Option<String>,
    pub(super) early_platform_executions: Vec<EarlyPlatformExecution>,
    pub(super) early_thinking_markers: Vec<EarlyThinkingMarkers>,
}

pub(super) struct PreparedPlatformMarker {
    call_id: String,
    marker: HistoryMarker,
}

impl PreparedPlatformMarker {
    pub(super) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(super) fn reference(&self) -> &str {
        &self.marker.reference
    }

    pub(super) fn marker(&self) -> &HistoryMarker {
        &self.marker
    }

    pub(super) fn render(&self) -> String {
        render_history_marker(&self.marker)
    }
}

pub(super) struct EarlyPlatformExecution {
    pub(super) marker: PreparedPlatformMarker,
    pub(super) execution: crate::StartedHistoryMarkerExecution,
}

pub(super) struct EarlyThinkingMarkers {
    pub(super) output_index: usize,
    pub(super) markers: Vec<HistoryMarker>,
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
        let owner_id = format!("execution-{}", uuid::Uuid::new_v4());
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
            is_error,
            cache_control,
        } = result
        else {
            unreachable!("Platform segments contain ToolResult blocks");
        };
        AiItem {
            role: crate::protocol::ir::Role::Tool,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content,
                is_error,
                cache_control,
            }]),
            tool_calls: None,
            tool_call_id: Some(tool_use_id),
            meta: None,
        }
    }));
}

const THINKING_MARKER_PENDING_RETENTION: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

async fn create_thinking_marker(
    context: &CompletionContext,
    block: ContentBlock,
) -> Result<HistoryMarker, HistoryMarkerError> {
    context
        .gateway
        .history_markers
        .create_thinking(
            &context.principal,
            ThinkingMarkerInput {
                block,
                activity: "Preserving protected reasoning".into(),
                pending_retention: THINKING_MARKER_PENDING_RETENTION,
            },
        )
        .await
}

pub(super) async fn prepare_thinking_markers(
    context: &CompletionContext,
    item: &AiItem,
) -> Result<Vec<HistoryMarker>, HistoryMarkerError> {
    if context.ingress != crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        return Ok(Vec::new());
    }
    let MessageContent::Blocks(blocks) = &item.content else {
        return Ok(Vec::new());
    };
    let mut markers = Vec::new();
    for block in blocks.iter().filter(|block| is_protected_thinking(block)) {
        markers.push(create_thinking_marker(context, block.clone()).await?);
    }
    Ok(markers)
}

async fn project_protected_thinking(
    projector: &ClientProjector,
    context: &CompletionContext,
    response: &mut AiResponse,
    early_thinking_markers: Vec<EarlyThinkingMarkers>,
) -> Result<Vec<String>, HistoryMarkerError> {
    if context.ingress != crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        if !early_thinking_markers.is_empty() {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        return Ok(Vec::new());
    }
    let mut early_by_output_index = std::collections::BTreeMap::new();
    for early in early_thinking_markers {
        if early_by_output_index
            .insert(
                early.output_index,
                std::collections::VecDeque::from(early.markers),
            )
            .is_some()
        {
            return Err(HistoryMarkerError::InvalidPayload);
        }
    }
    let mut projected = Vec::with_capacity(response.items.len());
    let mut new_references = Vec::new();
    for (output_index, mut item) in std::mem::take(&mut response.items).into_iter().enumerate() {
        let mut prepared = early_by_output_index
            .remove(&output_index)
            .unwrap_or_default();
        let mut item_markers = Vec::new();
        if let MessageContent::Blocks(blocks) = &mut item.content {
            let mut visible_blocks = Vec::with_capacity(blocks.len());
            for block in std::mem::take(blocks) {
                if !is_protected_thinking(&block) {
                    visible_blocks.push(block);
                    continue;
                }
                let marker = if let Some(marker) = prepared.pop_front() {
                    marker
                } else {
                    let marker = create_thinking_marker(context, block.clone()).await?;
                    new_references.push(marker.reference.clone());
                    marker
                };
                if let Some(visible_block) = projector.visible_protected_block(&block, &marker) {
                    visible_blocks.push(visible_block);
                }
                item_markers.push(marker);
            }
            *blocks = visible_blocks;
        }
        if !prepared.is_empty() {
            return Err(HistoryMarkerError::InvalidPayload);
        }
        let item_is_empty = matches!(&item.content, MessageContent::Blocks(blocks) if blocks.is_empty())
            && item.tool_calls.is_none();
        if !item_is_empty {
            projected.push(item);
        }
        projected.extend(item_markers.iter().map(ClientProjector::marker_item));
    }
    if !early_by_output_index.is_empty() {
        return Err(HistoryMarkerError::InvalidPayload);
    }
    response.items = projected;
    Ok(new_references)
}

pub(super) async fn publish_markers(
    context: &CompletionContext,
    references: &[String],
) -> Result<(), HistoryMarkerError> {
    const PUBLISHED_RETENTION: std::time::Duration =
        std::time::Duration::from_secs(7 * 24 * 60 * 60);
    context
        .gateway
        .history_markers
        .publish(&context.principal, references, PUBLISHED_RETENTION)
        .await
}

pub(super) async fn complete_canonical_response(
    context: &CompletionContext,
    input: CompletionInput<'_>,
) -> CompletionOutcome {
    let CompletionInput {
        request_context,
        request,
        run,
        phase,
        mut response,
        upstream_response_id,
        early_platform_executions,
        early_thinking_markers,
    } = input;
    let commit = context.client_output_commit;
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
    let classified = run.classify_tool_calls(&response);
    let has_platform_calls = !classified.platform.is_empty();
    let has_client_calls = !classified.client.is_empty();
    let canonical_response = response.clone();
    let mut projector = ClientProjector::new();
    let mut publish_references = match project_protected_thinking(
        &projector,
        context,
        &mut response,
        early_thinking_markers,
    )
    .await
    {
        Ok(references) => references,
        Err(error) => return CompletionOutcome::Failed(CompletionFailure::hook(error, commit)),
    };
    let mut background_executions = Vec::new();
    let mut started_executions = Vec::new();
    if has_platform_calls {
        let early_call_ids = early_platform_executions
            .iter()
            .map(|execution| execution.marker.call_id().to_owned())
            .collect::<std::collections::HashSet<_>>();
        let executions = classified
            .platform
            .into_iter()
            .filter(|call| !early_call_ids.contains(&call.call.id))
            .map(|call| {
                run.detached_platform_execution(
                    call,
                    crate::proxy::context::CancellationToken::new(),
                )
            })
            .collect();
        let (prepared, jobs) = match prepare_platform_markers(context, executions).await {
            Ok(prepared) => prepared,
            Err(error) => {
                return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
            }
        };
        let prepared = early_platform_executions
            .into_iter()
            .map(|early| {
                started_executions.push(early.execution);
                early.marker
            })
            .chain(prepared)
            .collect::<Vec<_>>();
        let mut published = request_context
            .extensions
            .get::<PublishedPlatformExecutions>()
            .unwrap_or_default();
        for reference in prepared
            .iter()
            .map(|prepared| prepared.marker.reference.clone())
        {
            if !published.references.contains(&reference) {
                published.references.push(reference);
            }
        }
        publish_references.extend(
            prepared
                .iter()
                .map(|prepared| prepared.marker.reference.clone()),
        );
        request_context.extensions.insert(published);
        let platform = prepared
            .iter()
            .map(|marker| (marker.call_id(), marker.marker()))
            .collect::<Vec<_>>();
        response.items = projector.project_items(std::mem::take(&mut response.items), &platform);
        if !has_client_calls {
            return CompletionOutcome::PlatformOnly(Box::new(PlatformOnlyContinuation {
                projected_response: response,
                canonical_response,
                markers: prepared,
                jobs,
                started_executions,
                publish_references,
            }));
        }
        background_executions = jobs;
    }
    apply_hidden_rounds(request_context, &mut response);
    if let Err(error) = phase.transition(Phase::SemanticComplete) {
        return CompletionOutcome::Failed(CompletionFailure::hook(error, commit));
    }

    let mut generation_chain = context.generation_chain.clone();
    if let Some(chain) = generation_chain.as_mut() {
        run.remove_exposed_tools(chain.write.request_mut());
    }
    let reusable_upstream_id = generation_chain
        .as_ref()
        .is_some_and(|chain| {
            chain.target_protocol == OPEN_RESPONSES_2026_04_24
                && upstream_response_is_available(
                    chain.write.request(),
                    &chain.response_continuation_available,
                )
                && crate::generation_chain::generation_node_is_completed(&response)
                && response_preserves_upstream(&upstream_response, &response)
        })
        .then_some(upstream_response_id)
        .flatten();

    let pending_generation_chain = generation_chain.take().and_then(|mut chain| {
        crate::generation_chain::mark_generation_target(
            &mut response,
            &chain.target_namespace,
            chain.target_protocol,
            &chain.actual_model,
        );
        chain
            .write
            .stage(&mut response, reusable_upstream_id)
            .then_some(chain.write)
    });
    CompletionOutcome::Ready(Box::new(CompletionLease {
        response: Box::new(response),
        commit,
        pending_generation_chain: pending_generation_chain.map(Box::new),
        background_executions,
        started_executions,
        publish_references,
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
                crate::protocol::ir::canonical::item_hash(left)
                    == crate::protocol::ir::canonical::item_hash(right)
            })
}

fn fill_canonical_defaults(context: &CompletionContext, response: &mut AiResponse) {
    if response.id.is_empty() {
        response.id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
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
                    crate::protocol::codec::open_responses::formatter::gateway_item_id(
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

fn record_hidden_round(context: &RequestContext, response: &AiResponse) {
    let mut state = context
        .extensions
        .get::<HiddenRoundState>()
        .unwrap_or_default();
    state.items.extend(
        response
            .items
            .iter()
            .filter(|item| retain_hidden_round_item(item))
            .cloned(),
    );
    if state.round_count == 0 {
        state.usage = response.usage.clone();
    } else {
        add_usage(&mut state.usage, &response.usage);
    }
    state.round_count = state.round_count.saturating_add(1);
    context.extensions.insert(state);
}

fn retain_hidden_round_item(item: &crate::protocol::ir::AiItem) -> bool {
    item.output_text_ref().is_some()
        || item.thinking_ref().is_some()
        || item.reasoning_ref().is_some()
}

pub(super) fn apply_hidden_rounds(context: &RequestContext, response: &mut AiResponse) {
    let Some(state) = context.extensions.get::<HiddenRoundState>() else {
        return;
    };
    if !state.items.is_empty() {
        response.items.splice(0..0, state.items);
    }
    add_usage(&mut response.usage, &state.usage);
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
    total.required_components_known =
        total.required_components_known && usage.required_components_known;
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(usage.completion_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    total.cache_read_tokens = sum_optional(total.cache_read_tokens, usage.cache_read_tokens);
    total.cache_creation_tokens =
        sum_optional(total.cache_creation_tokens, usage.cache_creation_tokens);
    total.reasoning_tokens = sum_optional(total.reasoning_tokens, usage.reasoning_tokens);
    match (&mut total.server_tool_use, &usage.server_tool_use) {
        (Some(total), Some(usage)) => {
            total.web_search_requests = total
                .web_search_requests
                .saturating_add(usage.web_search_requests);
            total.web_fetch_requests = total
                .web_fetch_requests
                .saturating_add(usage.web_fetch_requests);
        }
        (None, Some(usage)) => total.server_tool_use = Some(usage.clone()),
        _ => {}
    }
}

fn sum_optional(left: Option<u32>, right: Option<u32>) -> Option<u32> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_round_usage_accumulates_reasoning_tokens() {
        let mut total = Usage {
            reasoning_tokens: Some(3),
            required_components_known: true,
            ..Usage::default()
        };
        add_usage(
            &mut total,
            &Usage {
                reasoning_tokens: Some(4),
                required_components_known: true,
                ..Usage::default()
            },
        );

        assert_eq!(total.reasoning_tokens, Some(7));
    }

    #[test]
    fn hidden_rounds_retain_typed_reasoning_items() {
        let item = crate::protocol::ir::AiItem::reasoning(
            vec!["summary".into()],
            vec!["content".into()],
            Some("opaque".into()),
        );

        assert!(retain_hidden_round_item(&item));
    }

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
    fn ephemeral_response_reuse_requires_live_transport_affinity() {
        let mut request = AiRequest::new("model", Vec::new());
        request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
            crate::protocol::ir::OpenResponsesExt {
                store: Some(false),
                ..Default::default()
            },
        ));
        let unavailable = std::sync::atomic::AtomicBool::new(false);
        let available = std::sync::atomic::AtomicBool::new(true);

        assert!(!upstream_response_is_available(&request, &unavailable));
        assert!(upstream_response_is_available(&request, &available));
    }
}
