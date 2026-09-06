//! Streaming response handlers. Every path decodes provider events to canonical
//! deltas, applies HookRuntime stream transformations, and encodes the resulting
//! semantic stream for the ingress protocol.

use std::collections::HashSet;
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
    ClientOutputCommit, ClientProjectionSession, CompletionContext, CompletionFailure,
    CompletionInput, CompletionOutcome, DeliveryAdapter, DeliveryProgress, EarlyPlatformExecution,
    FollowupModelTurn, LogBuilder, PhaseTracker, ProjectedDeltaBatch, ProjectionDelivery,
    PublishedPlatformExecutions, RequestExtras, RoundOutcome, StreamResponseAccumulator,
    acquire_followup_model_turn, ai_response_to_deltas, buffered_response,
    complete_canonical_response, error_response, hook_failure_response, live_response,
    prepare_platform_markers, render_completion_failure,
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
    pub(super) turn_started: Instant,
    pub(super) request_extras: RequestExtras,
    pub(super) log: LogBuilder,
    pub(super) projection: ClientProjectionSession,
}

enum ProjectedDeliveryFailure {
    Delivery(DeliveryProgress),
    Marker(crate::history_marker::HistoryMarkerError),
}

async fn deliver_projected(
    delivery: &mut DeliveryAdapter,
    projection: &mut ClientProjectionSession,
    batch: ProjectedDeltaBatch,
) -> Result<Vec<String>, ProjectedDeliveryFailure> {
    let progress = delivery.send_deltas(batch.deltas()).await;
    let outcome = if progress == DeliveryProgress::Sent {
        ProjectionDelivery::Sent
    } else {
        ProjectionDelivery::Cancelled
    };
    let published = projection
        .report_delivery(batch, outcome)
        .await
        .map_err(ProjectedDeliveryFailure::Marker)?;
    if progress == DeliveryProgress::Sent {
        Ok(published)
    } else {
        Err(ProjectedDeliveryFailure::Delivery(progress))
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
        mut turn_started,
        request_extras,
        log,
        projection,
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
    let fixed_media_plan = request.meta.media_routing.clone();

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
        let mut projection = projection;
        'model_legs: loop {
            let carrier_facts = super::thinking_carrier_facts(
                ingress,
                turn.route.egress,
                turn.reasoning_encrypted_content_requested,
            );
            projection.begin_model_leg(carrier_facts, inference_run.exposed_tool_names());
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
            let mut committed_failure_delivered = false;
            let mut final_client_status = None;
            let mut cancelled = false;
            let mut receiver_closed = false;
            let mut protocol_failed = false;
            let mut preflight_failure = None;
            let mut leg_client_output_committed = false;
            let mut early_platform_executions = Vec::new();

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
                            let projected_batches = match projection
                                .project_live_deltas(
                                    transformed.clone(),
                                    !terminal_deltas.is_empty(),
                                )
                                .await
                            {
                                Ok(projected) => projected,
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
                            for batch in projected_batches {
                                let has_visible = !batch.is_empty();
                                match deliver_projected(&mut delivery, &mut projection, batch).await
                                {
                                    Ok(_) => leg_client_output_committed |= has_visible,
                                    Err(ProjectedDeliveryFailure::Delivery(progress)) => {
                                        match progress {
                                            DeliveryProgress::Cancelled => cancelled = true,
                                            DeliveryProgress::ReceiverClosed => {
                                                receiver_closed = true
                                            }
                                            DeliveryProgress::ProtocolFailed => {
                                                protocol_failed = true
                                            }
                                            DeliveryProgress::Sent => {
                                                unreachable!("Sent is not a delivery failure")
                                            }
                                        }
                                        break;
                                    }
                                    Err(ProjectedDeliveryFailure::Marker(error)) => {
                                        tracing::error!(
                                            "failed to publish streamed Thinking marker: {error}"
                                        );
                                        aborted = true;
                                        break;
                                    }
                                }
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
                                let mut published_references = Vec::new();
                                for marker in &markers {
                                    let batch = projection.project_platform_marker(marker.marker());
                                    match deliver_projected(&mut delivery, &mut projection, batch)
                                        .await
                                    {
                                        Ok(references) => {
                                            leg_client_output_committed = true;
                                            published_references.extend(references);
                                        }
                                        Err(ProjectedDeliveryFailure::Delivery(progress)) => {
                                            match progress {
                                                DeliveryProgress::Cancelled => cancelled = true,
                                                DeliveryProgress::ReceiverClosed => {
                                                    receiver_closed = true
                                                }
                                                DeliveryProgress::ProtocolFailed => {
                                                    protocol_failed = true
                                                }
                                                DeliveryProgress::Sent => {
                                                    unreachable!("Sent is not a delivery failure")
                                                }
                                            }
                                            break;
                                        }
                                        Err(ProjectedDeliveryFailure::Marker(error)) => {
                                            tracing::error!(
                                                "failed to publish streamed Platform marker: {error}"
                                            );
                                            aborted = true;
                                            break;
                                        }
                                    }
                                }
                                if aborted || cancelled || receiver_closed || protocol_failed {
                                    break;
                                }
                                let mut published = request_context
                                    .extensions
                                    .get::<PublishedPlatformExecutions>()
                                    .unwrap_or_default();
                                published.references.extend(published_references);
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
                            if aborted || cancelled || receiver_closed || protocol_failed {
                                break;
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
                        match projection.project_live_deltas(flushed, true).await {
                            Ok(batches) => {
                                for batch in batches {
                                    let has_visible = !batch.is_empty();
                                    match deliver_projected(&mut delivery, &mut projection, batch)
                                        .await
                                    {
                                        Ok(_) => leg_client_output_committed |= has_visible,
                                        Err(ProjectedDeliveryFailure::Delivery(progress)) => {
                                            match progress {
                                                DeliveryProgress::Cancelled => cancelled = true,
                                                DeliveryProgress::ReceiverClosed => {
                                                    receiver_closed = true
                                                }
                                                DeliveryProgress::ProtocolFailed => {
                                                    protocol_failed = true
                                                }
                                                DeliveryProgress::Sent => {
                                                    unreachable!("Sent is not a delivery failure")
                                                }
                                            }
                                            break;
                                        }
                                        Err(ProjectedDeliveryFailure::Marker(error)) => {
                                            tracing::error!(
                                                "failed to publish flushed Thinking marker: {error}"
                                            );
                                            aborted = true;
                                            break;
                                        }
                                    }
                                }
                            }
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
                            }
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
            let leg_egress = turn.route.egress;
            let stream_metrics = attempt_trace.stream_metrics();
            let mut model_turn_log = Some(
                log.clone()
                    .model_turn(&turn.route, &turn.target)
                    .status(200)
                    .usage(response.usage.clone())
                    .upstream_protocol(&leg_egress.to_string())
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
                    .stream_metrics(stream_metrics.chunks_count, stream_metrics.first_chunk_ms)
                    .model_turn_completed(turn_started),
            );

            let mut pending_generation_chain = None;
            let mut background_executions = Vec::new();
            let mut started_executions = Vec::new();
            let mut staged_delivery = None;
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
                        projection: &mut projection,
                    },
                )
                .await
                {
                    CompletionOutcome::PlatformOnly(continuation) => {
                        model_turn_log
                            .take()
                            .expect("current Model Turn log")
                            .without_client_exchange()
                            .emit();
                        let marker_delivery = projection.take_staged_delivery();
                        if !marker_delivery.is_empty() {
                            match deliver_projected(&mut delivery, &mut projection, marker_delivery)
                                .await
                            {
                                Ok(references) => {
                                    let mut published = request_context
                                        .extensions
                                        .get::<PublishedPlatformExecutions>()
                                        .unwrap_or_default();
                                    published.references.extend(references);
                                    request_context.extensions.insert(published);
                                }
                                Err(ProjectedDeliveryFailure::Delivery(progress)) => match progress
                                {
                                    DeliveryProgress::Cancelled => cancelled = true,
                                    DeliveryProgress::ReceiverClosed => receiver_closed = true,
                                    DeliveryProgress::ProtocolFailed => protocol_failed = true,
                                    DeliveryProgress::Sent => {
                                        unreachable!("Sent is not a delivery failure")
                                    }
                                },
                                Err(ProjectedDeliveryFailure::Marker(error)) => {
                                    tracing::error!(
                                        "failed to publish staged Platform marker: {error}"
                                    );
                                    aborted = true;
                                }
                            }
                        }
                        response = completion_context.empty_response();
                        if !aborted && !cancelled && !receiver_closed && !protocol_failed {
                            if let Err(failure) = continuation
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
                                &mut projection,
                                &mut phase,
                                completion_context.principal(),
                                &generation,
                                fixed_media_plan.as_ref(),
                                start,
                                &request_extras,
                            )
                            .await
                            {
                                Ok(FollowupModelTurn::Turn(next_turn, next_turn_started)) => {
                                    turn = next_turn;
                                    turn_started = next_turn_started;
                                    continue 'model_legs;
                                }
                                Ok(FollowupModelTurn::HookResponse {
                                    response: hook_response,
                                    pending_generation_chain: hook_generation_chain,
                                }) => {
                                    model_turn_log = Some(
                                        LogBuilder::from_dispatch(
                                            &gateway,
                                            &ingress.to_string(),
                                            &request.model,
                                            request.reasoning.level,
                                            request_context.auth_subject.as_ref(),
                                            start,
                                        )
                                        .stream_flag(true)
                                        .status(200)
                                        .with_req_extras(&request_extras),
                                    );
                                    response = hook_response;
                                    pending_generation_chain = hook_generation_chain;
                                    let hook_marker_delivery = projection.take_staged_delivery();
                                    if !buffer_terminal_hooks {
                                        let mut deltas = ai_response_to_deltas(&response);
                                        terminal_deltas = deltas
                                            .iter()
                                            .filter(|delta| {
                                                matches!(
                                                    delta,
                                                    AiStreamDelta::ResponseTerminal { .. }
                                                )
                                            })
                                            .cloned()
                                            .collect();
                                        deltas.retain(|delta| {
                                            !matches!(
                                                delta,
                                                AiStreamDelta::Usage(_)
                                                    | AiStreamDelta::ResponseTerminal { .. }
                                                    | AiStreamDelta::Done { .. }
                                            )
                                        });
                                        let progress = delivery.send_deltas(&deltas).await;
                                        let outcome = if progress == DeliveryProgress::Sent {
                                            ProjectionDelivery::Sent
                                        } else {
                                            ProjectionDelivery::Cancelled
                                        };
                                        match projection
                                            .report_delivery(hook_marker_delivery, outcome)
                                            .await
                                        {
                                            Ok(references)
                                                if progress == DeliveryProgress::Sent =>
                                            {
                                                let mut published = request_context
                                                    .extensions
                                                    .get::<PublishedPlatformExecutions>()
                                                    .unwrap_or_default();
                                                published.references.extend(references);
                                                request_context.extensions.insert(published);
                                            }
                                            Ok(_) => match progress {
                                                DeliveryProgress::Cancelled => cancelled = true,
                                                DeliveryProgress::ReceiverClosed => {
                                                    receiver_closed = true
                                                }
                                                DeliveryProgress::ProtocolFailed => {
                                                    protocol_failed = true
                                                }
                                                DeliveryProgress::Sent => {}
                                            },
                                            Err(error) => {
                                                tracing::error!(
                                                    "failed to publish Hook response markers: {error}"
                                                );
                                                aborted = true;
                                            }
                                        }
                                    } else {
                                        staged_delivery = Some(hook_marker_delivery);
                                    }
                                }
                                Ok(FollowupModelTurn::StreamError(error)) => {
                                    final_client_status = error.status_code;
                                    model_turn_log = Some(
                                        LogBuilder::from_dispatch(
                                            &gateway,
                                            &ingress.to_string(),
                                            &request.model,
                                            request.reasoning.level,
                                            request_context.auth_subject.as_ref(),
                                            start,
                                        )
                                        .stream_flag(true)
                                        .status(error.status_code.unwrap_or(500))
                                        .with_req_extras(&request_extras),
                                    );
                                    let error = [AiStreamDelta::StreamError { error }];
                                    if delivery.send_deltas(&error).await == DeliveryProgress::Sent
                                        && delivery.finish_stream("failed".into()).await
                                            == DeliveryProgress::Sent
                                    {
                                        committed_failure_delivered = true;
                                    }
                                    aborted = true;
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
                            staged_delivery = Some(projection.take_staged_delivery());
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
                if let Some(marker_delivery) = staged_delivery.take()
                    && !marker_delivery.is_empty()
                {
                    match deliver_projected(&mut delivery, &mut projection, marker_delivery).await {
                        Ok(references) => {
                            let mut published = request_context
                                .extensions
                                .get::<PublishedPlatformExecutions>()
                                .unwrap_or_default();
                            published.references.extend(references);
                            request_context.extensions.insert(published);
                        }
                        Err(ProjectedDeliveryFailure::Delivery(progress)) => match progress {
                            DeliveryProgress::Cancelled => cancelled = true,
                            DeliveryProgress::ReceiverClosed => receiver_closed = true,
                            DeliveryProgress::ProtocolFailed => protocol_failed = true,
                            DeliveryProgress::Sent => {
                                unreachable!("Sent is not a delivery failure")
                            }
                        },
                        Err(ProjectedDeliveryFailure::Marker(error)) => {
                            tracing::error!("failed to publish final projected markers: {error}");
                            aborted = true;
                        }
                    }
                }
                if !aborted && !cancelled && !receiver_closed && !protocol_failed {
                    let suffix = projection.complete_live_model_leg();
                    if !suffix.is_empty() {
                        match deliver_projected(&mut delivery, &mut projection, suffix).await {
                            Ok(_) => {}
                            Err(ProjectedDeliveryFailure::Delivery(progress)) => match progress {
                                DeliveryProgress::Cancelled => cancelled = true,
                                DeliveryProgress::ReceiverClosed => receiver_closed = true,
                                DeliveryProgress::ProtocolFailed => protocol_failed = true,
                                DeliveryProgress::Sent => {
                                    unreachable!("Sent is not a delivery failure")
                                }
                            },
                            Err(ProjectedDeliveryFailure::Marker(error)) => {
                                tracing::error!("failed to publish projected suffix: {error}");
                                aborted = true;
                            }
                        }
                    }
                }
                if !aborted && !cancelled && !receiver_closed && !protocol_failed {
                    let usage = [AiStreamDelta::Usage(response.usage.clone())];
                    match delivery.send_deltas(&usage).await {
                        DeliveryProgress::Sent => {}
                        DeliveryProgress::Cancelled => cancelled = true,
                        DeliveryProgress::ReceiverClosed => receiver_closed = true,
                        DeliveryProgress::ProtocolFailed => protocol_failed = true,
                    }
                }
                if !aborted && !cancelled && !receiver_closed && !protocol_failed {
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
                marker_output_delivered =
                    !aborted && !cancelled && !receiver_closed && !protocol_failed;
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
                let progress = delivery.send_deltas(&final_deltas).await;
                match progress {
                    DeliveryProgress::Sent => {}
                    DeliveryProgress::Cancelled => cancelled = true,
                    DeliveryProgress::ReceiverClosed => receiver_closed = true,
                    DeliveryProgress::ProtocolFailed => protocol_failed = true,
                }
                if let Some(marker_delivery) = staged_delivery.take() {
                    let outcome = if progress == DeliveryProgress::Sent {
                        ProjectionDelivery::Sent
                    } else {
                        ProjectionDelivery::Cancelled
                    };
                    match projection.report_delivery(marker_delivery, outcome).await {
                        Ok(references) if progress == DeliveryProgress::Sent => {
                            let mut published = request_context
                                .extensions
                                .get::<PublishedPlatformExecutions>()
                                .unwrap_or_default();
                            published.references.extend(references);
                            request_context.extensions.insert(published);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::error!("failed to publish terminal Hook markers: {error}");
                            aborted = true;
                        }
                    }
                }
                marker_output_delivered =
                    !aborted && !cancelled && !receiver_closed && !protocol_failed;
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
                let response = if request_context.deadline.is_exceeded() {
                    error_response(504, "request deadline exceeded")
                } else {
                    error_response(499, "request cancelled")
                };
                delivery.fail_before_commit(buffered_response(response))
            } else {
                false
            };

            let mut terminal_delivered = false;
            if aborted && !preflight_failed && !committed_failure_delivered {
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
            } else if !aborted
                && !preflight_failed
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
            if let Some(model_turn_log) = model_turn_log.take() {
                model_turn_log
                    .status(final_client_status.unwrap_or(if aborted { 500 } else { 200 }))
                    .with_client_response(None, client_response_body)
                    .emit();
            }
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
