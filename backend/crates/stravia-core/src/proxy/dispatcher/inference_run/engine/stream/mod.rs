//! Streaming response handlers. Every path decodes provider events to canonical
//! deltas, applies HookRuntime stream transformations, and encodes the resulting
//! semantic stream for the ingress protocol.
//!
//! The pump is a thin shell over the shared Model Leg consumption in
//! `super::leg`: it owns the live-transport concerns — cancellation, receiver
//! watch, per-batch wire delivery — while `ModelLegConsume` owns the leg
//! lifecycle shared with the buffered path.

use std::convert::Infallible;
use std::sync::Arc;

use axum::http::HeaderMap;
use futures::StreamExt;

use crate::agent::ModelTurn;
use crate::agent::ModelTurnExecutor;
use crate::proxy::context::RequestContext;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;

use super::delivery::LiveStreamRequest;
use super::{
    ClientOutputCommit, ClientProjectionSession, CompletionFailure, DeliveryAdapter,
    DeliveryProgress, FollowupEnv, HookLegGuard, HookResponsePlan, LegAdvance, LegEnv, LegFailure,
    LegFlow, LegOps, LegParts, LegPolicy, LegReaction, LiveLegOps, ModelLegConsume, PhaseTracker,
    PreparedDelivery, ProjectedDeliveryFailure, ProjectionDelivery, RoundOutcome, RunLedger,
    SealOutcome, Settlement, ai_response_to_deltas, buffered_response, deliver_projected,
    error_response, live_response, record_marker_failure, render_completion_failure,
    render_leg_failure, report_projected_delivery, settle,
};

pub(super) struct ModelTurnStreamInput {
    pub(super) turn: ModelTurn,
    pub(super) executor: Arc<dyn ModelTurnExecutor>,
    pub(super) gateway: crate::Gateway,
    pub(super) headers: HeaderMap,
    pub(super) ingress: stravia_runtime_contract::protocol::ids::ProtocolId,
    pub(super) request_context: RequestContext,
    pub(super) request: AiRequest,
    pub(super) generation: super::GenerationChainRun,
    pub(super) inference_run: crate::hook::InferenceRun,
    pub(super) phase: PhaseTracker,
    pub(super) projection: ClientProjectionSession,
    pub(super) ledger: RunLedger,
}

/// Map a mid-leg delivery disruption onto the pump's transport flags.
fn apply_delivery_progress(progress: DeliveryProgress, flags: &mut LiveTransportFlags) {
    match progress {
        DeliveryProgress::Cancelled => flags.cancelled = true,
        DeliveryProgress::ReceiverClosed => flags.receiver_closed = true,
        DeliveryProgress::ProtocolFailed => flags.protocol_failed = true,
        DeliveryProgress::Sent => {}
    }
}

/// The pump's transport flags — what the `select!` and emit failures observed
/// on the live wire during one Model Leg.
#[derive(Default)]
struct LiveTransportFlags {
    cancelled: bool,
    receiver_closed: bool,
    protocol_failed: bool,
}

impl LiveTransportFlags {
    fn disrupted(&self) -> bool {
        self.cancelled || self.receiver_closed || self.protocol_failed
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
        mut generation,
        mut inference_run,
        mut phase,
        projection,
        ledger,
    } = input;
    let egress = turn.route.egress;
    let previous_response_id = generation.previous_response_id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Infallible>>(64);
    let receiver_watch = tx.clone();
    let (preflight_tx, preflight_rx) = tokio::sync::oneshot::channel::<Result<(), RoundOutcome>>();
    let (commit_tx, commit_rx) = tokio::sync::oneshot::channel();
    let (terminal_delivery_tx, terminal_delivery_rx) = tokio::sync::oneshot::channel();
    let cancellation = request_context.cancellation.clone();
    let fixed_media_plan = request.meta.media_routing.clone();
    let observe_delivery =
        !crate::proxy::dispatcher::is_websocket_delivery_deferred(&request_context);
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let completion_ledger = ledger.clone();

    tokio::spawn(async move {
        let mut delivery = DeliveryAdapter::live_stream(LiveStreamRequest {
            ingress,
            egress,
            tx,
            cancellation: cancellation.clone(),
            preflight: preflight_tx,
            terminal_delivery: terminal_delivery_rx,
            commit: commit_rx,
        });
        delivery.set_response_profile(&request, previous_response_id.as_deref());
        let observer = request_context
            .extensions
            .get::<crate::interaction_observation::RunObserver>()
            .expect("admitted Inference Run observer")
            .clone();
        let mut projection = projection;
        'model_legs: loop {
            let buffer_terminal_hooks = inference_run.requires_terminal_buffering();
            let mut leg = ModelLegConsume::begin(
                LegEnv {
                    gateway: &gateway,
                    generation: &generation,
                    ingress,
                    observer: &observer,
                },
                &turn,
                &inference_run,
                &mut projection,
                LegPolicy {
                    emit_live: !buffer_terminal_hooks,
                    early_platform: !buffer_terminal_hooks,
                },
            );
            let mut hook_leg = HookLegGuard::new(&mut inference_run);
            let mut ops = LegOps::Live(LiveLegOps {
                delivery: &mut delivery,
                ledger: &ledger,
                observer: &observer,
                model_turn_id: turn.model_turn_id.clone(),
                observe_delivery,
            });
            let mut output = turn.output;
            let mut aborted = false;
            let mut committed_failure_delivered = false;
            let mut transport = LiveTransportFlags::default();
            let mut preflight_failure = None;
            let mut response = leg.empty_response();
            let mut pending_generation_chain = None;
            let mut background_executions = Vec::new();
            let mut started_executions = Vec::new();
            let mut staged_delivery = None;

            while !(aborted || transport.disrupted()) {
                let event = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        transport.cancelled = true;
                        break;
                    }
                    _ = receiver_watch.closed() => {
                        transport.receiver_closed = true;
                        break;
                    }
                    event = output.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                match leg.feed(hook_leg.run_mut(), event) {
                    LegReaction::Absorbed => {}
                    LegReaction::Emit(deltas) => {
                        match leg
                            .perform_emit(&mut ops, hook_leg.run_mut(), &mut projection, deltas)
                            .await
                        {
                            LegFlow::Open => {}
                            LegFlow::Disrupted(progress) => {
                                apply_delivery_progress(progress, &mut transport);
                            }
                            LegFlow::Faulted => aborted = true,
                            LegFlow::Failed(failure) => {
                                aborted = true;
                                preflight_failure.get_or_insert_with(|| {
                                    render_leg_failure(
                                        failure,
                                        &request,
                                        ingress,
                                        true,
                                        ops.client_commit(&projection),
                                    )
                                });
                            }
                        }
                    }
                    LegReaction::Ended => break,
                    LegReaction::Failed(failure) => {
                        aborted = true;
                        preflight_failure.get_or_insert_with(|| {
                            render_leg_failure(
                                failure,
                                &request,
                                ingress,
                                true,
                                ops.client_commit(&projection),
                            )
                        });
                    }
                }
                if transport.disrupted() {
                    break;
                }
            }
            let mut terminal_deltas = leg.terminal_deltas().to_vec();
            match leg
                .seal(
                    &mut ops,
                    &mut projection,
                    hook_leg.close().await,
                    transport.disrupted(),
                )
                .await
            {
                SealOutcome::Ready => {}
                SealOutcome::Failed(failure) => {
                    aborted = true;
                    preflight_failure.get_or_insert_with(|| {
                        render_leg_failure(
                            failure,
                            &request,
                            ingress,
                            true,
                            ops.client_commit(&projection),
                        )
                    });
                }
                SealOutcome::Disrupted(progress) => {
                    apply_delivery_progress(progress, &mut transport);
                }
                SealOutcome::Aborted => aborted = true,
            }

            if !(aborted || transport.disrupted()) {
                let commit = ops.client_commit(&projection);
                let advance = leg
                    .advance(
                        &mut ops,
                        LegParts {
                            request: &mut request,
                            run: hook_leg.run_mut(),
                            phase: &mut phase,
                            projection: &mut projection,
                            ledger: &ledger,
                        },
                        FollowupEnv {
                            executor: executor.as_ref(),
                            headers: &headers,
                            request_context: &request_context,
                            generation: &mut generation,
                            fixed_media_plan: fixed_media_plan.as_ref(),
                        },
                    )
                    .await;
                drop(ops);
                match advance {
                    LegAdvance::Ready(prepared) => {
                        let PreparedDelivery {
                            response: prepared_response,
                            staged_delivery: prepared_staged,
                            pending_generation_chain: prepared_generation_chain,
                            background_executions: prepared_background,
                            started_executions: prepared_started,
                        } = *prepared;
                        response = prepared_response;
                        pending_generation_chain = prepared_generation_chain;
                        background_executions = prepared_background;
                        started_executions = prepared_started;
                        staged_delivery = Some(prepared_staged);
                    }
                    LegAdvance::NextLeg(next) => {
                        turn = *next;
                        continue 'model_legs;
                    }
                    LegAdvance::HookResponse(plan) => {
                        let HookResponsePlan {
                            response: hook_response,
                            staged_delivery: hook_marker_delivery,
                            pending_generation_chain: hook_generation_chain,
                        } = *plan;
                        response = hook_response;
                        pending_generation_chain = hook_generation_chain.map(|chain| *chain);
                        if !buffer_terminal_hooks {
                            let delivered_response =
                                match projection.prepare_upload_delivery(&response).await {
                                    Ok(response) => Some(response),
                                    Err(error) => {
                                        aborted = true;
                                        preflight_failure =
                                            Some(buffered_response(render_completion_failure(
                                                CompletionFailure::hook(
                                                    error,
                                                    ClientOutputCommit::of(
                                                        projection.client_output_committed(),
                                                    ),
                                                ),
                                                ingress,
                                                true,
                                            )));
                                        None
                                    }
                                };
                            if let Some(delivered_response) = delivered_response {
                                let mut deltas = ai_response_to_deltas(delivered_response.as_ref());
                                terminal_deltas = deltas
                                    .iter()
                                    .filter(|delta| {
                                        matches!(delta, AiStreamDelta::ResponseTerminal { .. })
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
                                match report_projected_delivery(
                                    &mut projection,
                                    &ledger,
                                    hook_marker_delivery,
                                    outcome,
                                )
                                .await
                                {
                                    Ok(_) if progress == DeliveryProgress::Sent => {}
                                    Ok(_) => apply_delivery_progress(progress, &mut transport),
                                    Err(error) => {
                                        record_marker_failure(&observer, &error);
                                        tracing::error!(
                                            "failed to publish Hook response markers: {error}"
                                        );
                                        aborted = true;
                                    }
                                }
                            }
                        } else {
                            staged_delivery = Some(hook_marker_delivery);
                        }
                    }
                    LegAdvance::StreamError(error) => {
                        let error = [AiStreamDelta::StreamError { error }];
                        if delivery.send_deltas(&error).await == DeliveryProgress::Sent
                            && delivery.finish_stream("failed".into()).await
                                == DeliveryProgress::Sent
                        {
                            committed_failure_delivered = true;
                        }
                        aborted = true;
                    }
                    LegAdvance::Outcome(outcome) => {
                        preflight_failure = Some(outcome);
                        aborted = true;
                    }
                    LegAdvance::Failed(failure) => {
                        aborted = true;
                        // A commit already reached the wire, so there is no
                        // preflight channel left to render the failure through.
                        if !matches!(
                            failure,
                            LegFailure::Completion(CompletionFailure::AfterCommit(_))
                        ) {
                            preflight_failure.get_or_insert_with(|| {
                                render_leg_failure(failure, &request, ingress, true, commit)
                            });
                        }
                    }
                    LegAdvance::Disrupted(progress) => {
                        apply_delivery_progress(progress, &mut transport);
                        aborted = true;
                    }
                    LegAdvance::Aborted => aborted = true,
                }
            } else {
                drop(ops);
            }
            drop(leg);
            drop(hook_leg);
            let mut owned_run = Some(inference_run);
            let mut owned_phase = Some(phase);
            let mut marker_output_delivered = false;

            if !buffer_terminal_hooks
                && preflight_failure.is_none()
                && !aborted
                && !transport.disrupted()
            {
                if let Some(marker_delivery) = staged_delivery.take()
                    && !marker_delivery.is_empty()
                {
                    match deliver_projected(
                        &mut delivery,
                        &mut projection,
                        &ledger,
                        &observer,
                        &turn.model_turn_id,
                        observe_delivery,
                        marker_delivery,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(ProjectedDeliveryFailure::Delivery(progress)) => {
                            apply_delivery_progress(progress, &mut transport)
                        }
                        Err(ProjectedDeliveryFailure::Marker(error)) => {
                            tracing::error!("failed to publish final projected markers: {error}");
                            aborted = true;
                        }
                    }
                }
                if !(aborted || transport.disrupted()) {
                    let suffix = projection.complete_live_model_leg();
                    if !suffix.is_empty() {
                        match deliver_projected(
                            &mut delivery,
                            &mut projection,
                            &ledger,
                            &observer,
                            &turn.model_turn_id,
                            observe_delivery,
                            suffix,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(ProjectedDeliveryFailure::Delivery(progress)) => {
                                apply_delivery_progress(progress, &mut transport)
                            }
                            Err(ProjectedDeliveryFailure::Marker(error)) => {
                                tracing::error!("failed to publish projected suffix: {error}");
                                aborted = true;
                            }
                        }
                    }
                }
                if !(aborted || transport.disrupted()) {
                    let usage = [AiStreamDelta::Usage(response.usage.clone())];
                    match delivery.send_deltas(&usage).await {
                        DeliveryProgress::Sent => {}
                        progress => apply_delivery_progress(progress, &mut transport),
                    }
                }
                if !(aborted || transport.disrupted()) {
                    let response_terminal = terminal_deltas
                        .iter()
                        .filter(|delta| matches!(delta, AiStreamDelta::ResponseTerminal { .. }))
                        .cloned()
                        .collect::<Vec<_>>();
                    match delivery.send_deltas(&response_terminal).await {
                        DeliveryProgress::Sent => {}
                        progress => apply_delivery_progress(progress, &mut transport),
                    }
                }
                marker_output_delivered = !(aborted || transport.disrupted());
            }

            if buffer_terminal_hooks
                && preflight_failure.is_none()
                && !aborted
                && !transport.disrupted()
            {
                let delivered_response = match projection.prepare_upload_delivery(&response).await {
                    Ok(response) => Some(response),
                    Err(error) => {
                        aborted = true;
                        preflight_failure = Some(buffered_response(render_completion_failure(
                            CompletionFailure::hook(
                                error,
                                ClientOutputCommit::of(projection.client_output_committed()),
                            ),
                            ingress,
                            true,
                        )));
                        None
                    }
                };
                if let Some(delivered_response) = delivered_response {
                    delivery.reset_stream_encoder();
                    let mut final_deltas = ai_response_to_deltas(delivered_response.as_ref());
                    final_deltas.retain(|delta| !matches!(delta, AiStreamDelta::Done { .. }));
                    let progress = delivery.send_deltas(&final_deltas).await;
                    match progress {
                        DeliveryProgress::Sent => {}
                        progress => apply_delivery_progress(progress, &mut transport),
                    }
                    if let Some(marker_delivery) = staged_delivery.take() {
                        let outcome = if progress == DeliveryProgress::Sent {
                            ProjectionDelivery::Sent
                        } else {
                            ProjectionDelivery::Cancelled
                        };
                        match report_projected_delivery(
                            &mut projection,
                            &ledger,
                            marker_delivery,
                            outcome,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(error) => {
                                record_marker_failure(&observer, &error);
                                tracing::error!("failed to publish terminal Hook markers: {error}");
                                aborted = true;
                            }
                        }
                    }
                    marker_output_delivered = !(aborted || transport.disrupted());
                }
            }

            if marker_output_delivered
                && !aborted
                && (!background_executions.is_empty() || !started_executions.is_empty())
            {
                settle(
                    &gateway,
                    &mut projection,
                    &ledger,
                    &observer,
                    ingress,
                    Settlement {
                        background_executions: std::mem::take(&mut background_executions),
                        started_executions: std::mem::take(&mut started_executions),
                        run: owned_run.take(),
                        ..Default::default()
                    },
                )
                .await;
            }

            let preflight_failed = if let Some(outcome) = preflight_failure.take() {
                if outcome.response.status().as_u16() != 499
                    && let Some(error) = outcome
                        .response
                        .extensions()
                        .get::<crate::interaction_observation::FailureDiagnostic>()
                {
                    observer.record_response_failure(error.clone());
                }
                delivery.fail_before_commit(outcome)
            } else if transport.cancelled {
                let response = if request_context.deadline.is_exceeded() {
                    observer.record_response_failure(
                        crate::interaction_observation::FailureDiagnostic {
                            source: Some("platform".into()),
                            code: Some("request_deadline_exceeded".into()),
                            message: Some("request deadline exceeded".into()),
                            status_code: None,
                            upstream_code: None,
                        },
                    );
                    error_response(504, "request deadline exceeded")
                } else {
                    error_response(499, "request cancelled")
                };
                delivery.fail_before_commit(buffered_response(response))
            } else {
                false
            };

            let mut delivery_completed_at = None;
            if aborted && !preflight_failed && !committed_failure_delivered {
                if !transport.disrupted() {
                    let native_error = crate::compaction::NativeCompactionControls::classify(
                        &request,
                    )
                    .requested()
                    .then(|| {
                        terminal_deltas.iter().find(|delta| {
                            matches!(delta, AiStreamDelta::StreamError { error } if error.raw.is_some())
                        })
                    })
                    .flatten()
                    .cloned();
                    let error = [native_error.unwrap_or_else(|| AiStreamDelta::StreamError {
                        error: stravia_runtime_contract::protocol::ir::AiError::new(
                            stravia_runtime_contract::protocol::ir::AiErrorKind::StreamMidError,
                            "stream aborted",
                        ),
                    })];
                    if delivery.send_deltas(&error).await == DeliveryProgress::Sent {
                        let _ = delivery.finish_stream("failed".into()).await;
                    }
                }
            } else if !aborted
                && !preflight_failed
                && !transport.disrupted()
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
                delivery_completed_at = delivery.wait_for_terminal_delivery().await;
            }

            if delivery_completed_at.is_some() {
                settle(
                    &gateway,
                    &mut projection,
                    &ledger,
                    &observer,
                    ingress,
                    Settlement {
                        pending_generation_chain: pending_generation_chain.take(),
                        delivery_completed_at,
                        delivered_response: Some(response),
                        ..Default::default()
                    },
                )
                .await;
            }
            let terminal = delivery_completed_at
                .is_some()
                .then(|| ledger.terminal.clone());
            let _ = completion_tx.send(terminal);
            if let Some(mut phase) = owned_phase.take() {
                phase.finish();
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
    completion_ledger.set_stream_completion(super::super::StreamDeliveryCompletion(completion_rx));
    live_response(DeliveryAdapter::response_from_receiver(
        rx,
        commit_tx,
        terminal_delivery_tx,
        ingress,
    ))
}
