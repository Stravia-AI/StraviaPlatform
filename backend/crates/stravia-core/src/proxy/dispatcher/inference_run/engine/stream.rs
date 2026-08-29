//! Streaming response handlers. Every path decodes provider events to canonical
//! deltas, applies HookRuntime stream transformations, and encodes the resulting
//! semantic stream for the ingress protocol.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderMap;
use futures::StreamExt;

use crate::agent::{CanonicalEvent, ModelTurn, ModelTurnExecutor};
use crate::protocol::ir::{AiRequest, AiResponse, AiStreamDelta};
use crate::proxy::context::RequestContext;

use super::delivery::LiveStreamRequest;
use super::{
    ClientOutputCommit, CompletionContext, CompletionFailure, CompletionInput, CompletionOutcome,
    DeliveryAdapter, DeliveryProgress, EarlyPlatformExecution, EarlyThinkingMarkers, LogBuilder,
    PhaseTracker, PublishedPlatformExecutions, RequestExtras, RoundOutcome,
    StreamResponseAccumulator, acquire_followup_model_turn, ai_response_to_deltas,
    buffered_response, complete_canonical_response, error_response, hook_failure_response,
    live_response, prepare_platform_markers, prepare_thinking_markers, publish_markers,
    render_completion_failure,
};

pub(super) struct HookLegGuard<'a> {
    run: &'a mut crate::hook::InferenceRun,
    closed: bool,
}

impl<'a> HookLegGuard<'a> {
    pub(super) fn new(run: &'a mut crate::hook::InferenceRun) -> Self {
        Self { run, closed: false }
    }

    pub(super) fn run_mut(&mut self) -> &mut crate::hook::InferenceRun {
        self.run
    }

    pub(super) async fn close(&mut self) -> Result<Vec<AiStreamDelta>, crate::hook::HookError> {
        if self.closed {
            return Ok(Vec::new());
        }
        let result = self.run.flush_stream();
        self.closed = true;
        result
    }
}

impl Drop for HookLegGuard<'_> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.run.flush_stream();
            self.closed = true;
        }
    }
}

pub(super) struct ModelTurnStreamInput {
    pub(super) turn: ModelTurn,
    pub(super) executor: Arc<dyn ModelTurnExecutor>,
    pub(super) gateway: crate::Gateway,
    pub(super) headers: HeaderMap,
    pub(super) ingress: crate::protocol::ids::ProtocolId,
    pub(super) request_context: RequestContext,
    pub(super) request: AiRequest,
    pub(super) generation: super::GenerationChainRun,
    pub(super) inference_run: crate::hook::InferenceRun,
    pub(super) phase: PhaseTracker,
    pub(super) start: Instant,
    pub(super) request_extras: RequestExtras,
    pub(super) log: LogBuilder,
}

#[derive(Default)]
struct LiveDeltaGate {
    pending_prefix: Vec<AiStreamDelta>,
    pending_tool_deltas: HashMap<usize, Vec<AiStreamDelta>>,
    pending_tool_names: HashMap<usize, String>,
    platform_tool_indices: HashSet<usize>,
    client_output_started: bool,
    response_started: bool,
}

impl LiveDeltaGate {
    fn commit_visible(&mut self, mut deltas: Vec<AiStreamDelta>) -> Vec<AiStreamDelta> {
        if !self.client_output_started {
            self.client_output_started = true;
            self.pending_prefix.append(&mut deltas);
            let committed = std::mem::take(&mut self.pending_prefix);
            self.response_started |= committed
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::MessageStart { .. }));
            committed
        } else {
            deltas
        }
    }

    fn filter(
        &mut self,
        run: &crate::hook::InferenceRun,
        deltas: Vec<AiStreamDelta>,
    ) -> Vec<AiStreamDelta> {
        let mut visible = Vec::new();
        for delta in deltas {
            if matches!(delta, AiStreamDelta::Usage(_)) {
                continue;
            }
            if self.response_started
                && matches!(
                    delta,
                    AiStreamDelta::MessageStart { .. } | AiStreamDelta::ResponseMetadata { .. }
                )
            {
                continue;
            }
            match &delta {
                AiStreamDelta::ToolCallStart { index, name, .. } => {
                    if self.pending_tool_deltas.contains_key(index) {
                        let index = *index;
                        let accumulated = self.pending_tool_names.entry(index).or_default();
                        accumulated.push_str(name);
                        let is_platform = run.is_exposed_tool(accumulated);
                        let remains_ambiguous = run.could_be_exposed_tool_prefix(accumulated);
                        self.pending_tool_deltas
                            .entry(index)
                            .or_default()
                            .push(delta);
                        if is_platform {
                            self.pending_tool_deltas.remove(&index);
                            self.pending_tool_names.remove(&index);
                            self.platform_tool_indices.insert(index);
                        } else if !remains_ambiguous {
                            if let Some(mut pending) = self.pending_tool_deltas.remove(&index) {
                                visible.append(&mut pending);
                            }
                            self.pending_tool_names.remove(&index);
                        }
                        continue;
                    }
                    if run.is_exposed_tool(name) {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if run.could_be_exposed_tool_prefix(name) {
                        self.pending_tool_names.insert(*index, name.clone());
                        self.pending_tool_deltas
                            .entry(*index)
                            .or_default()
                            .push(delta);
                        continue;
                    }
                }
                AiStreamDelta::ToolCallDelta { index, .. }
                    if self.pending_tool_deltas.contains_key(index) =>
                {
                    self.pending_tool_deltas
                        .entry(*index)
                        .or_default()
                        .push(delta);
                    continue;
                }
                AiStreamDelta::ToolCallComplete { index, tool_call } => {
                    if run.is_exposed_tool(&tool_call.name) {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if let Some(mut pending) = self.pending_tool_deltas.remove(index) {
                        visible.append(&mut pending);
                    }
                    self.pending_tool_names.remove(index);
                }
                AiStreamDelta::ItemDone { index, item } => {
                    let platform = item
                        .function_call_ref()
                        .is_some_and(|call| run.is_exposed_tool(&call.name));
                    if platform {
                        self.pending_tool_deltas.remove(index);
                        self.pending_tool_names.remove(index);
                        self.platform_tool_indices.insert(*index);
                        continue;
                    }
                    if let Some(mut pending) = self.pending_tool_deltas.remove(index) {
                        visible.append(&mut pending);
                    }
                    self.pending_tool_names.remove(index);
                }
                _ => {}
            }
            let hidden_platform_delta = match &delta {
                AiStreamDelta::ToolCallStart { index, name, .. } if run.is_exposed_tool(name) => {
                    self.platform_tool_indices.insert(*index);
                    true
                }
                AiStreamDelta::ToolCallDelta { index, .. } => {
                    self.platform_tool_indices.contains(index)
                }
                AiStreamDelta::ToolCallComplete { index, tool_call } => {
                    let hidden = self.platform_tool_indices.contains(index)
                        || run.is_exposed_tool(&tool_call.name);
                    if hidden {
                        self.platform_tool_indices.insert(*index);
                    }
                    hidden
                }
                AiStreamDelta::ItemDone { index, item } => {
                    let hidden = self.platform_tool_indices.contains(index)
                        || item
                            .function_call_ref()
                            .is_some_and(|call| run.is_exposed_tool(&call.name));
                    if hidden {
                        self.platform_tool_indices.insert(*index);
                    }
                    hidden
                }
                _ => false,
            };
            if hidden_platform_delta {
                continue;
            }

            let prefix_only = matches!(
                delta,
                AiStreamDelta::MessageStart { .. }
                    | AiStreamDelta::ResponseMetadata { .. }
                    | AiStreamDelta::Usage(_)
                    | AiStreamDelta::ResponseTerminal { .. }
                    | AiStreamDelta::Unknown { .. }
            );
            if !self.client_output_started && prefix_only {
                self.pending_prefix.push(delta);
                continue;
            }
            if !self.client_output_started {
                self.client_output_started = true;
                visible.append(&mut self.pending_prefix);
            }
            visible.push(delta);
        }
        self.response_started |= visible
            .iter()
            .any(|delta| matches!(delta, AiStreamDelta::MessageStart { .. }));
        visible
    }
}

pub(super) async fn handle_model_turn_stream(input: ModelTurnStreamInput) -> RoundOutcome {
    let ModelTurnStreamInput {
        mut turn,
        executor,
        gateway,
        headers,
        ingress,
        request_context,
        mut request,
        generation,
        mut inference_run,
        mut phase,
        start,
        request_extras,
        log,
    } = input;
    let egress = turn.route.egress;
    let previous_response_id = generation.previous_response_id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Infallible>>(64);
    let receiver_watch = tx.clone();
    let (preflight_tx, preflight_rx) = tokio::sync::oneshot::channel::<Result<(), RoundOutcome>>();
    let (commit_tx, commit_rx) = tokio::sync::oneshot::channel();
    let (terminal_delivery_tx, terminal_delivery_rx) = tokio::sync::oneshot::channel();
    let cancellation = request_context.cancellation.clone();
    let redact_payloads = request.meta.media_routing.is_some();

    tokio::spawn(async move {
        let mut delivery = DeliveryAdapter::live_stream(LiveStreamRequest {
            ingress,
            egress,
            tx,
            cancellation: cancellation.clone(),
            preflight: preflight_tx,
            terminal_delivery: terminal_delivery_rx,
            commit: commit_rx,
            capture_payload: true,
        });
        delivery.set_response_profile(&request, previous_response_id.as_deref());
        let mut live_delta_gate = LiveDeltaGate::default();
        let mut emitted_marker_texts = HashSet::new();
        'model_legs: loop {
            let attempt_trace = turn.transport.clone();
            let mut completion_context = CompletionContext::from_model_turn(
                gateway.clone(),
                generation.clone(),
                ingress,
                &turn.target,
                turn.route.egress,
            );
            let mut output = turn.output;
            let buffer_terminal_hooks = inference_run.requires_terminal_buffering();
            let mut hook_leg = HookLegGuard::new(&mut inference_run);
            let mut accumulator = StreamResponseAccumulator::default();
            let mut terminal_deltas = Vec::new();
            let mut completed_response = None;
            let mut upstream_response_id = None;
            let mut aborted = false;
            let mut cancelled = false;
            let mut receiver_closed = false;
            let mut protocol_failed = false;
            let mut preflight_failure = None;
            let mut leg_client_output_committed = false;
            let mut early_platform_executions = Vec::new();
            let mut early_thinking_markers = Vec::new();

            while !aborted && !cancelled && !receiver_closed && !protocol_failed {
                let event = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    _ = receiver_watch.closed() => {
                        receiver_closed = true;
                        break;
                    }
                    event = output.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    Ok(CanonicalEvent::Delta(delta)) => {
                        let (terminal, deltas) = partition_terminal_deltas(vec![delta]);
                        let tool_calls_complete = terminal.iter().any(|delta| {
                            matches!(
                                delta,
                                AiStreamDelta::Done { stop_reason }
                                    if stop_reason == "tool_calls"
                            )
                        });
                        if terminal_deltas_failed(&terminal) {
                            aborted = true;
                            preflight_failure = Some(buffered_response(error_response(
                                502,
                                "upstream stream error",
                            )));
                        }
                        terminal_deltas.extend(terminal);
                        let mut transformed =
                            match transform_stream_deltas(hook_leg.run_mut(), deltas) {
                                Ok(deltas) => deltas,
                                Err(error) => {
                                    aborted = true;
                                    preflight_failure =
                                        Some(buffered_response(hook_failure_response(error)));
                                    break;
                                }
                            };
                        if upstream_response_id.is_none() {
                            upstream_response_id =
                                transformed.iter().find_map(|delta| match delta {
                                    AiStreamDelta::MessageStart { id, .. } if !id.is_empty() => {
                                        Some(id.clone())
                                    }
                                    _ => None,
                                });
                        }
                        apply_response_identity(
                            &mut transformed,
                            completion_context.generation_chain_identity(),
                        );
                        accumulator.apply_all(&transformed);
                        if !buffer_terminal_hooks {
                            let completed_thinking_items = transformed
                                .iter()
                                .filter_map(|delta| match delta {
                                    AiStreamDelta::ItemDone { index, item }
                                        if !early_thinking_markers.iter().any(
                                            |early: &EarlyThinkingMarkers| {
                                                early.output_index == *index
                                            },
                                        ) =>
                                    {
                                        Some((*index, item))
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            for (output_index, item) in completed_thinking_items {
                                let markers = match prepare_thinking_markers(
                                    &completion_context,
                                    &item,
                                )
                                .await
                                {
                                    Ok(markers) => markers,
                                    Err(error) => {
                                        aborted = true;
                                        preflight_failure =
                                            Some(buffered_response(render_completion_failure(
                                                CompletionFailure::hook(
                                                    error,
                                                    completion_context.client_output_commit(),
                                                ),
                                                ingress,
                                                true,
                                            )));
                                        break;
                                    }
                                };
                                if markers.is_empty() {
                                    continue;
                                }
                                let references = markers
                                    .iter()
                                    .map(|marker| marker.reference.clone())
                                    .collect::<Vec<_>>();
                                let marker_deltas = live_delta_gate.commit_visible(
                                    markers
                                        .iter()
                                        .map(|marker| {
                                            let rendered =
                                                crate::history_marker::render_history_marker(
                                                    marker,
                                                );
                                            emitted_marker_texts.insert(rendered.clone());
                                            AiStreamDelta::TextDelta(format!("\n\n{rendered}"))
                                        })
                                        .collect(),
                                );
                                match delivery.send_deltas(&marker_deltas).await {
                                    DeliveryProgress::Sent => {
                                        leg_client_output_committed = true;
                                    }
                                    DeliveryProgress::Cancelled => {
                                        cancelled = true;
                                        break;
                                    }
                                    DeliveryProgress::ReceiverClosed => {
                                        receiver_closed = true;
                                        break;
                                    }
                                    DeliveryProgress::ProtocolFailed => {
                                        protocol_failed = true;
                                        break;
                                    }
                                }
                                if let Err(error) =
                                    publish_markers(&completion_context, &references).await
                                {
                                    tracing::error!(
                                        "failed to publish streamed thinking marker: {error}"
                                    );
                                    aborted = true;
                                    break;
                                }
                                early_thinking_markers.push(EarlyThinkingMarkers {
                                    output_index,
                                    markers,
                                });
                            }
                            if aborted || cancelled || receiver_closed || protocol_failed {
                                break;
                            }
                            let mut completed_platform_calls = transformed
                                .iter()
                                .filter_map(|delta| match delta {
                                    AiStreamDelta::ToolCallComplete { tool_call, .. } => {
                                        Some(tool_call.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            if tool_calls_complete {
                                completed_platform_calls.extend(accumulator.tool_calls().cloned());
                            }
                            let mut completed_call_ids = HashSet::new();
                            completed_platform_calls.retain(|call| {
                                hook_leg.run_mut().is_exposed_tool(&call.name)
                                    && completed_call_ids.insert(call.id.clone())
                                    && !early_platform_executions.iter().any(
                                        |early: &EarlyPlatformExecution| {
                                            early.marker.call_id() == call.id
                                        },
                                    )
                            });
                            for call in completed_platform_calls {
                                let platform_call = hook_leg
                                    .run_mut()
                                    .classify_tool_calls(&AiResponse {
                                        items: vec![crate::protocol::ir::AiItem::function_call(
                                            call,
                                        )],
                                        ..completion_context.empty_response()
                                    })
                                    .platform
                                    .into_iter()
                                    .next()
                                    .expect("classified Platform Tool call");
                                let execution = hook_leg.run_mut().detached_platform_execution(
                                    platform_call,
                                    crate::proxy::context::CancellationToken::new(),
                                );
                                let (markers, jobs) = match prepare_platform_markers(
                                    &completion_context,
                                    vec![execution],
                                )
                                .await
                                {
                                    Ok(prepared) => prepared,
                                    Err(error) => {
                                        aborted = true;
                                        preflight_failure =
                                            Some(buffered_response(render_completion_failure(
                                                CompletionFailure::hook(
                                                    error,
                                                    completion_context.client_output_commit(),
                                                ),
                                                ingress,
                                                true,
                                            )));
                                        break;
                                    }
                                };
                                let references = markers
                                    .iter()
                                    .map(|marker| marker.reference().to_owned())
                                    .collect::<Vec<_>>();
                                let marker_deltas = live_delta_gate.commit_visible(
                                    markers
                                        .iter()
                                        .map(|marker| {
                                            let rendered = marker.render();
                                            emitted_marker_texts.insert(rendered.clone());
                                            AiStreamDelta::TextDelta(format!("\n\n{rendered}"))
                                        })
                                        .collect(),
                                );
                                match delivery.send_deltas(&marker_deltas).await {
                                    DeliveryProgress::Sent => {
                                        leg_client_output_committed = true;
                                    }
                                    DeliveryProgress::Cancelled => {
                                        cancelled = true;
                                        break;
                                    }
                                    DeliveryProgress::ReceiverClosed => {
                                        receiver_closed = true;
                                        break;
                                    }
                                    DeliveryProgress::ProtocolFailed => {
                                        protocol_failed = true;
                                        break;
                                    }
                                }
                                if let Err(error) =
                                    publish_markers(&completion_context, &references).await
                                {
                                    tracing::error!(
                                        "failed to publish streamed history marker: {error}"
                                    );
                                    aborted = true;
                                    break;
                                }
                                let mut published = request_context
                                    .extensions
                                    .get::<PublishedPlatformExecutions>()
                                    .unwrap_or_default();
                                published.references.extend(references);
                                request_context.extensions.insert(published);
                                let started = gateway.start_history_marker_executions(
                                    completion_context.principal().clone(),
                                    jobs,
                                );
                                early_platform_executions.extend(
                                    markers.into_iter().zip(started).map(|(marker, execution)| {
                                        EarlyPlatformExecution { marker, execution }
                                    }),
                                );
                            }
                        }
                        if !buffer_terminal_hooks {
                            let visible = live_delta_gate.filter(hook_leg.run_mut(), transformed);
                            let has_visible = !visible.is_empty();
                            match delivery.send_deltas(&visible).await {
                                DeliveryProgress::Sent => {
                                    leg_client_output_committed |= has_visible;
                                }
                                DeliveryProgress::Cancelled => cancelled = true,
                                DeliveryProgress::ReceiverClosed => receiver_closed = true,
                                DeliveryProgress::ProtocolFailed => protocol_failed = true,
                            }
                        }
                    }
                    Ok(CanonicalEvent::Completed(response)) => {
                        completed_response = Some(*response);
                    }
                    Err(error) => {
                        aborted = true;
                        preflight_failure = Some(super::model_turn_error_outcome(error));
                    }
                }
            }

            match hook_leg.close().await {
                Ok(mut flushed) => {
                    apply_response_identity(
                        &mut flushed,
                        completion_context.generation_chain_identity(),
                    );
                    accumulator.apply_all(&flushed);
                    if !buffer_terminal_hooks && !cancelled && !receiver_closed && !protocol_failed
                    {
                        let visible = live_delta_gate.filter(hook_leg.run_mut(), flushed);
                        let has_visible = !visible.is_empty();
                        match delivery.send_deltas(&visible).await {
                            DeliveryProgress::Sent => {
                                leg_client_output_committed |= has_visible;
                            }
                            DeliveryProgress::Cancelled => cancelled = true,
                            DeliveryProgress::ReceiverClosed => receiver_closed = true,
                            DeliveryProgress::ProtocolFailed => protocol_failed = true,
                        }
                    }
                }
                Err(error) => {
                    aborted = true;
                    preflight_failure = Some(buffered_response(hook_failure_response(error)));
                }
            }
            accumulator.apply_all(&terminal_deltas);
            let mut response = accumulator.into_ai_response();
            if let Some(completed) = completed_response {
                if response.usage.prompt_tokens == 0 && response.usage.completion_tokens == 0 {
                    response.usage = completed.usage;
                }
                if response.stop_reason.is_none() {
                    response.stop_reason = completed.stop_reason;
                }
                if response.id.is_empty() {
                    response.id = completed.id;
                }
            } else if !aborted && !cancelled && !receiver_closed {
                aborted = true;
                preflight_failure = Some(buffered_response(error_response(
                    502,
                    "Model Turn ended without a completion",
                )));
            }

            let mut pending_generation_chain = None;
            let mut background_executions = Vec::new();
            let mut started_executions = Vec::new();
            let mut publish_references = Vec::new();
            if !aborted && !cancelled && !receiver_closed && !protocol_failed {
                if leg_client_output_committed {
                    completion_context.mark_client_output_committed();
                }
                let commit = completion_context.client_output_commit();
                match complete_canonical_response(
                    &completion_context,
                    CompletionInput {
                        request_context: &request_context,
                        request: &mut request,
                        run: hook_leg.run_mut(),
                        phase: &mut phase,
                        response,
                        upstream_response_id,
                        early_platform_executions,
                        early_thinking_markers,
                    },
                )
                .await
                {
                    CompletionOutcome::PlatformOnly(continuation) => {
                        let marker_deltas = continuation
                            .projected_response()
                            .items
                            .iter()
                            .filter_map(|item| item.output_text_ref())
                            .filter(|text| {
                                text.contains(crate::history_marker::HISTORY_MARKER_PREFIX)
                            })
                            .filter(|text| emitted_marker_texts.insert((*text).to_owned()))
                            .map(|text| AiStreamDelta::TextDelta(format!("\n\n{text}")))
                            .collect::<Vec<_>>();
                        let marker_deltas = live_delta_gate.commit_visible(marker_deltas);
                        match delivery.send_deltas(&marker_deltas).await {
                            DeliveryProgress::Sent => {}
                            DeliveryProgress::Cancelled => cancelled = true,
                            DeliveryProgress::ReceiverClosed => receiver_closed = true,
                            DeliveryProgress::ProtocolFailed => protocol_failed = true,
                        }
                        response = completion_context.empty_response();
                        if !cancelled && !receiver_closed && !protocol_failed {
                            if let Err(failure) = continuation.publish(&completion_context).await {
                                if commit == ClientOutputCommit::Pending {
                                    preflight_failure = Some(buffered_response(
                                        render_completion_failure(failure, ingress, true),
                                    ));
                                }
                                aborted = true;
                            } else if let Err(failure) = continuation
                                .finish(
                                    &completion_context,
                                    &request_context,
                                    &mut request,
                                    hook_leg.run_mut(),
                                    &mut phase,
                                )
                                .await
                            {
                                if commit == ClientOutputCommit::Pending {
                                    preflight_failure = Some(buffered_response(
                                        render_completion_failure(failure, ingress, true),
                                    ));
                                }
                                aborted = true;
                            }
                        } else {
                            aborted = true;
                        }
                        if !aborted {
                            match acquire_followup_model_turn(
                                executor.as_ref(),
                                &gateway,
                                &headers,
                                &mut request,
                                ingress,
                                &request_context,
                                hook_leg.run_mut(),
                                &mut phase,
                                completion_context.principal(),
                                start,
                                &request_extras,
                            )
                            .await
                            {
                                Ok(next_turn) => {
                                    turn = next_turn;
                                    continue 'model_legs;
                                }
                                Err(outcome) => {
                                    preflight_failure = Some(outcome);
                                    aborted = true;
                                }
                            }
                        }
                    }
                    CompletionOutcome::Ready(lease) => match (*lease).prepare(&mut phase) {
                        Ok(prepared) => {
                            response = prepared.response;
                            pending_generation_chain = prepared.pending_generation_chain;
                            background_executions = prepared.background_executions;
                            started_executions = prepared.started_executions;
                            publish_references = prepared.publish_references;
                        }
                        Err(failure) => {
                            if commit == ClientOutputCommit::Pending {
                                preflight_failure = Some(buffered_response(
                                    render_completion_failure(failure, ingress, true),
                                ));
                            }
                            response = completion_context.empty_response();
                            aborted = true;
                        }
                    },
                    CompletionOutcome::Failed(failure) => {
                        if commit == ClientOutputCommit::Pending {
                            preflight_failure = Some(buffered_response(render_completion_failure(
                                failure, ingress, true,
                            )));
                        }
                        response = completion_context.empty_response();
                        aborted = true;
                    }
                }
            }
            drop(hook_leg);
            let mut owned_run = Some(inference_run);
            let mut owned_phase = Some(phase);
            let mut marker_output_delivered = false;

            if !buffer_terminal_hooks
                && preflight_failure.is_none()
                && !aborted
                && !cancelled
                && !receiver_closed
                && !protocol_failed
            {
                let markers = response
                    .items
                    .iter()
                    .filter_map(|item| item.output_text_ref())
                    .filter(|text| text.contains(crate::history_marker::HISTORY_MARKER_PREFIX))
                    .filter(|text| emitted_marker_texts.insert((*text).to_owned()))
                    .map(|text| AiStreamDelta::TextDelta(format!("\n\n{text}")))
                    .collect::<Vec<_>>();
                let markers = live_delta_gate.commit_visible(markers);
                if !markers.is_empty() {
                    match delivery.send_deltas(&markers).await {
                        DeliveryProgress::Sent => {}
                        DeliveryProgress::Cancelled => cancelled = true,
                        DeliveryProgress::ReceiverClosed => receiver_closed = true,
                        DeliveryProgress::ProtocolFailed => protocol_failed = true,
                    }
                }
                if !cancelled && !receiver_closed && !protocol_failed {
                    let usage = [AiStreamDelta::Usage(response.usage.clone())];
                    match delivery.send_deltas(&usage).await {
                        DeliveryProgress::Sent => {}
                        DeliveryProgress::Cancelled => cancelled = true,
                        DeliveryProgress::ReceiverClosed => receiver_closed = true,
                        DeliveryProgress::ProtocolFailed => protocol_failed = true,
                    }
                }
                if !cancelled && !receiver_closed && !protocol_failed {
                    let response_terminal = terminal_deltas
                        .iter()
                        .filter(|delta| matches!(delta, AiStreamDelta::ResponseTerminal { .. }))
                        .cloned()
                        .collect::<Vec<_>>();
                    match delivery.send_deltas(&response_terminal).await {
                        DeliveryProgress::Sent => {}
                        DeliveryProgress::Cancelled => cancelled = true,
                        DeliveryProgress::ReceiverClosed => receiver_closed = true,
                        DeliveryProgress::ProtocolFailed => protocol_failed = true,
                    }
                }
                marker_output_delivered = !cancelled && !receiver_closed && !protocol_failed;
            }

            if buffer_terminal_hooks
                && preflight_failure.is_none()
                && !aborted
                && !cancelled
                && !receiver_closed
                && !protocol_failed
            {
                delivery.reset_stream_encoder();
                let mut final_deltas = ai_response_to_deltas(&response);
                final_deltas.retain(|delta| !matches!(delta, AiStreamDelta::Done { .. }));
                match delivery.send_deltas(&final_deltas).await {
                    DeliveryProgress::Sent => {}
                    DeliveryProgress::Cancelled => cancelled = true,
                    DeliveryProgress::ReceiverClosed => receiver_closed = true,
                    DeliveryProgress::ProtocolFailed => protocol_failed = true,
                }
                marker_output_delivered = !cancelled && !receiver_closed && !protocol_failed;
            }

            if marker_output_delivered && !publish_references.is_empty() {
                if let Err(error) = publish_markers(&completion_context, &publish_references).await
                {
                    tracing::error!("failed to publish delivered history markers: {error}");
                    aborted = true;
                }
            }
            if marker_output_delivered && !aborted && !background_executions.is_empty() {
                started_executions.extend(gateway.start_history_marker_executions(
                    completion_context.principal().clone(),
                    background_executions,
                ));
            }
            if marker_output_delivered && !aborted && !started_executions.is_empty() {
                gateway.spawn_started_history_marker_executions(
                    started_executions,
                    owned_run
                        .take()
                        .expect("background Platform execution requires its Inference Run"),
                );
            }

            let preflight_failed = if let Some(outcome) = preflight_failure.take() {
                let outcome = match (owned_run.take(), owned_phase.take()) {
                    (Some(run), Some(phase)) => outcome.with_lifecycle(run, phase),
                    _ => outcome,
                };
                delivery.fail_before_commit(outcome)
            } else if cancelled {
                delivery
                    .fail_before_commit(buffered_response(error_response(499, "request cancelled")))
            } else {
                false
            };

            let mut terminal_delivered = false;
            if aborted && !preflight_failed {
                if !cancelled && !receiver_closed && !protocol_failed {
                    let error = [AiStreamDelta::StreamError {
                        error: crate::protocol::ir::AiError::new(
                            crate::protocol::ir::AiErrorKind::StreamMidError,
                            "stream aborted",
                        ),
                    }];
                    if delivery.send_deltas(&error).await == DeliveryProgress::Sent {
                        let _ = delivery.finish_stream("failed".into()).await;
                    }
                }
            } else if !preflight_failed
                && !cancelled
                && !receiver_closed
                && !protocol_failed
                && delivery
                    .finish_stream(
                        response
                            .stop_reason
                            .clone()
                            .unwrap_or_else(|| "stop".into()),
                    )
                    .await
                    == DeliveryProgress::Sent
            {
                terminal_delivered =
                    delivery.wait_for_terminal_delivery().await == DeliveryProgress::Sent;
            }

            if terminal_delivered {
                let generation_committed = if let Some(mut pending) =
                    pending_generation_chain.take()
                {
                    match pending.persist().await {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::error!(
                                "failed to commit Generation Chain node after terminal delivery: {error}"
                            );
                            false
                        }
                    }
                } else {
                    true
                };
                let _ = generation_committed;
            }
            if let Some(mut phase) = owned_phase.take() {
                phase.finish();
            }
            let client_response_body = if redact_payloads {
                None
            } else {
                delivery.captured_body()
            };
            let stream_metrics = attempt_trace.stream_metrics();
            log.status(if aborted { 500 } else { 200 })
                .usage(response.usage.clone())
                .upstream_protocol(&egress.to_string())
                .upstream_url(&attempt_trace.upstream_url)
                .with_upstream_request(
                    attempt_trace.request_headers.clone(),
                    attempt_trace.request_body.clone(),
                )
                .with_upstream_response(
                    200,
                    attempt_trace
                        .response_headers
                        .lock()
                        .expect("response headers")
                        .clone(),
                    (!redact_payloads).then(|| {
                        String::from_utf8_lossy(
                            &attempt_trace.response_body.lock().expect("response body"),
                        )
                        .into_owned()
                    }),
                    None,
                )
                .with_client_response(None, client_response_body)
                .stream_metrics(stream_metrics.chunks_count, stream_metrics.first_chunk_ms)
                .emit();
            break 'model_legs;
        }
    });

    match preflight_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(response)) => return response,
        Err(_) => {
            return buffered_response(error_response(
                502,
                "Model Turn stream ended before delivery",
            ));
        }
    }
    live_response(DeliveryAdapter::response_from_receiver(
        rx,
        commit_tx,
        terminal_delivery_tx,
        ingress,
    ))
}

// ── Streaming response handler ────────────────────────────────────────────────

pub(super) fn apply_response_identity(
    deltas: &mut [AiStreamDelta],
    identity: Option<(&str, &str)>,
) {
    let Some((response_id, logical_model)) = identity else {
        return;
    };
    for delta in deltas {
        if let AiStreamDelta::MessageStart { id, model } = delta {
            *id = response_id.to_owned();
            *model = logical_model.to_owned();
        }
    }
}

pub(super) fn partition_terminal_deltas(
    deltas: Vec<AiStreamDelta>,
) -> (Vec<AiStreamDelta>, Vec<AiStreamDelta>) {
    deltas.into_iter().partition(|delta| {
        matches!(
            delta,
            AiStreamDelta::ResponseTerminal { .. }
                | AiStreamDelta::Done { .. }
                | AiStreamDelta::StreamError { .. }
                | AiStreamDelta::UnexpectedEof
        )
    })
}

pub(super) fn terminal_deltas_failed(deltas: &[AiStreamDelta]) -> bool {
    deltas.iter().any(|delta| {
        matches!(
            delta,
            AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
        )
    })
}

pub(super) fn transform_stream_deltas(
    inference_run: &mut crate::hook::InferenceRun,
    deltas: Vec<AiStreamDelta>,
) -> Result<Vec<AiStreamDelta>, crate::hook::HookError> {
    let mut transformed = Vec::new();
    for delta in deltas {
        transformed.extend(inference_run.transform_stream(delta)?);
    }
    Ok(transformed)
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn response_terminal_is_dispatched_with_done() {
        let (terminal, content) = partition_terminal_deltas(vec![
            AiStreamDelta::TextDelta("partial".into()),
            AiStreamDelta::ResponseTerminal {
                status: "incomplete".into(),
                incomplete_details: Some(serde_json::json!({"reason": "max_output_tokens"})),
            },
            AiStreamDelta::Done {
                stop_reason: "length".into(),
            },
        ]);

        assert_eq!(content.len(), 1);
        assert!(matches!(
            terminal.as_slice(),
            [
                AiStreamDelta::ResponseTerminal { status, .. },
                AiStreamDelta::Done { .. },
            ] if status == "incomplete"
        ));
    }
}
