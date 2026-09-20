//! Model Leg consumption: the shared lifecycle that drives one Model Turn's
//! canonical event stream through Hook stream transformation, client
//! projection, early Platform execution, completion, and follow-up
//! acquisition.
//!
//! `ModelLegConsume` is the event-driven state machine; the two callers are
//! pump shells that differ only in their `LegOps` realization:
//!
//! - `LegOps::Live` emits projected batches over the open stream as they are
//!   produced and reports staged batches as wire delivery;
//! - `LegOps::Buffered` accumulates the leg for the final body and reports
//!   staged batches as Sent without emitting wire deltas.
//!
//! The module owns the leg-local invariants — terminal-delta partitioning,
//! Hook transform and flush, Generation Chain identity application, media
//! reconcile, and the Platform-only hidden-round choreography — so buffered
//! and live callers share the same lifecycle instead of drifting apart.

use std::collections::HashSet;

use super::*;
use crate::hook::InferenceRun;
use stravia_runtime_contract::hook::HookError;
use stravia_runtime_contract::model_turn::ModelTurnError;
use stravia_runtime_contract::protocol::ir::request::MediaRoutingPlan;
use stravia_runtime_contract::protocol::ir::{AiError, AiStreamDelta};

/// Per-leg emit policy chosen by the caller's transport shape.
pub(super) struct LegPolicy {
    /// Emit projected batches during the pump instead of accumulating the leg
    /// for one staged delivery. Live transports set this unless terminal
    /// buffering hooks defer all client output to leg completion.
    pub emit_live: bool,
    /// Start Platform Tool executions as soon as their calls complete rather
    /// than waiting for the Completed event.
    pub early_platform: bool,
}

/// A failure surfaced by the leg before or during completion. Callers render
/// it with [`render_leg_failure`] because rendering needs the request and the
/// caller's transport shape.
pub(super) enum LegFailure {
    /// The Model Turn stream yielded an error event.
    ModelTurn(ModelTurnError),
    /// A standalone Compacted event is only legal on the compaction path.
    UnexpectedCompaction,
    /// The stream declared a terminal fault (`StreamError` / `UnexpectedEof`).
    TerminalFault(Option<AiError>),
    /// Hook stream transformation or leg close failed.
    Hook(HookError),
    /// Client projection or Marker staging failed.
    Projection(crate::history_marker::HistoryMarkerError),
    /// The completion pipeline failed.
    Completion(CompletionFailure),
    /// The stream ended without a Completed event.
    Incomplete,
    /// Stream/completion media reconciliation failed.
    Reconcile(String),
}

impl LegFailure {
    pub(super) fn public_stream_error(&self) -> AiError {
        use stravia_runtime_contract::protocol::ir::AiErrorKind;

        match self {
            Self::ModelTurn(error) => {
                let status = model_turn_error_status(error).as_u16();
                let kind = match error.code.as_str() {
                    "protocol_lossy_rejected" | "STRAVIA_PROTOCOL_LOSSY_REJECTED" => {
                        AiErrorKind::InvalidRequest
                    }
                    _ => AiError::kind_from_status(status, error.upstream_body.as_deref()),
                };
                AiError::new(kind, error.message.clone()).with_status(status)
            }
            Self::TerminalFault(Some(error)) => error.clone(),
            Self::TerminalFault(None) | Self::Incomplete => AiError::new(
                AiErrorKind::UnexpectedEof,
                "upstream stream ended unexpectedly",
            ),
            Self::UnexpectedCompaction => AiError::new(
                AiErrorKind::InvalidRequest,
                "unexpected compaction terminal",
            ),
            Self::Hook(_) | Self::Completion(_) => {
                AiError::new(AiErrorKind::Unknown, "Hook failed after stream commit")
            }
            Self::Projection(_) => AiError::new(AiErrorKind::Unknown, "stream projection failed"),
            Self::Reconcile(_) => {
                AiError::new(AiErrorKind::Unknown, "output reconciliation failed")
            }
        }
    }
}

/// What the pump should do after `feed`.
pub(super) enum LegReaction {
    /// The event was absorbed; keep pumping.
    Absorbed,
    /// Transformed deltas are ready; run `perform_emit` on them.
    Emit(Vec<AiStreamDelta>),
    /// The leg's Completed event arrived; close the Hook leg and seal.
    Ended,
    /// The leg failed; render with [`render_leg_failure`].
    Failed(LegFailure),
}

/// Result of one emit step inside the leg.
pub(super) enum LegFlow {
    /// Emitted (or skipped for the staged transport); continue.
    Open,
    /// The transport disrupted mid-emit; the caller maps the progress to its
    /// own cancellation flags.
    Disrupted(DeliveryProgress),
    /// A mid-flight fault aborted the leg without a renderable failure.
    Faulted,
    /// A renderable leg failure.
    Failed(LegFailure),
}

/// What `seal` decided about the merged leg.
pub(super) enum SealOutcome {
    /// The leg merged cleanly; the caller may `advance` when its own transport
    /// flags allow.
    Ready,
    /// The transport disrupted while emitting the Hook flush.
    Disrupted(DeliveryProgress),
    /// A mid-flight fault aborted the leg during the flush emit.
    Aborted,
    /// The leg failed; render with [`render_leg_failure`].
    Failed(LegFailure),
}

/// What `advance` resolved for the run after completion.
pub(super) enum LegAdvance {
    /// The response is ready; the caller delivers it and settles.
    Ready(Box<PreparedDelivery>),
    /// A follow-up Model Turn begins the next Model Leg.
    NextLeg(Box<ModelTurn>),
    /// A Hook produced the run's response during follow-up acquisition.
    HookResponse(Box<HookResponsePlan>),
    /// Follow-up acquisition resolved to a mid-stream error.
    StreamError(AiError),
    /// Follow-up acquisition already rendered a terminal outcome.
    Outcome(RoundOutcome),
    /// The leg failed; render with [`render_leg_failure`].
    Failed(LegFailure),
    /// The transport disrupted while emitting the staged batch; the
    /// continuation was not finished.
    Disrupted(DeliveryProgress),
    /// A mid-flight fault aborted the leg during the staged emit.
    Aborted,
}

/// Run state the leg borrows for completion and follow-up acquisition.
pub(super) struct LegParts<'a> {
    pub request: &'a mut AiRequest,
    pub run: &'a mut InferenceRun,
    pub phase: &'a mut PhaseTracker,
    pub projection: &'a mut ClientProjectionSession,
    pub ledger: &'a RunLedger,
}

/// The leg's run-scoped environment: everything `begin` needs that stays
/// constant while Model Legs iterate inside one Inference Run.
pub(super) struct LegEnv<'a> {
    pub gateway: &'a crate::Gateway,
    pub generation: &'a GenerationChainRun,
    pub ingress: ProtocolId,
    pub observer: &'a crate::interaction_observation::RunObserver,
}

/// Follow-up acquisition environment: everything `advance` needs beyond the
/// parts to pull the next Model Turn.
pub(super) struct FollowupEnv<'a> {
    pub executor: &'a dyn ModelTurnExecutor,
    pub headers: &'a HeaderMap,
    pub request_context: &'a RequestContext,
    pub generation: &'a mut GenerationChainRun,
    pub fixed_media_plan: Option<&'a MediaRoutingPlan>,
}

/// The leg's emit channel. Exactly two realizations exist — one per delivery
/// transport — so the seam is a closed enum rather than a trait.
pub(super) enum LegOps<'a> {
    Live(LiveLegOps<'a>),
    Buffered(BufferedLegOps<'a>),
}

/// Live delivery state for the emit channel.
pub(super) struct LiveLegOps<'a> {
    pub delivery: &'a mut DeliveryAdapter,
    pub ledger: &'a RunLedger,
    pub observer: &'a crate::interaction_observation::RunObserver,
    pub model_turn_id: String,
    pub observe_delivery: bool,
}

/// Buffered delivery state: staged batches report as Sent against the
/// projection session directly.
pub(super) struct BufferedLegOps<'a> {
    pub ledger: &'a RunLedger,
}

impl LegOps<'_> {
    /// Emit a transformed batch: project it for the live wire, or absorb it on
    /// the staged transport.
    async fn emit_deltas(
        &mut self,
        projection: &mut ClientProjectionSession,
        deltas: Vec<AiStreamDelta>,
        leg_completed: bool,
    ) -> LegFlow {
        let Self::Live(ops) = self else {
            return LegFlow::Open;
        };
        let batches = match projection.project_live_deltas(deltas, leg_completed).await {
            Ok(batches) => batches,
            Err(error) => return LegFlow::Failed(LegFailure::Projection(error)),
        };
        for batch in batches {
            match ops.deliver(projection, batch).await {
                Ok(()) => {}
                Err(ProjectedDeliveryFailure::Delivery(progress)) => {
                    return LegFlow::Disrupted(progress);
                }
                Err(ProjectedDeliveryFailure::Marker(error)) => {
                    tracing::error!("failed to publish streamed History Marker: {error}");
                    return LegFlow::Faulted;
                }
            }
        }
        LegFlow::Open
    }

    /// Emit a prepared Platform Marker before its execution starts.
    async fn emit_platform_marker(
        &mut self,
        projection: &mut ClientProjectionSession,
        marker: &crate::history_marker::HistoryMarker,
    ) -> LegFlow {
        let Self::Live(ops) = self else {
            return LegFlow::Open;
        };
        let batch = projection.project_platform_marker(marker);
        match ops.deliver(projection, batch).await {
            Ok(()) => LegFlow::Open,
            Err(ProjectedDeliveryFailure::Delivery(progress)) => LegFlow::Disrupted(progress),
            Err(ProjectedDeliveryFailure::Marker(error)) => {
                tracing::error!("failed to publish Platform History Marker: {error}");
                LegFlow::Faulted
            }
        }
    }

    /// Deliver the staged Marker batch a Platform-only leg produced: over the
    /// wire for live, reported Sent against the projection for buffered.
    async fn emit_staged(
        &mut self,
        projection: &mut ClientProjectionSession,
        batch: ProjectedDeltaBatch,
    ) -> LegFlow {
        match self {
            Self::Live(ops) => {
                if batch.is_empty() {
                    return LegFlow::Open;
                }
                match ops.deliver(projection, batch).await {
                    Ok(()) => LegFlow::Open,
                    Err(ProjectedDeliveryFailure::Delivery(progress)) => {
                        LegFlow::Disrupted(progress)
                    }
                    Err(ProjectedDeliveryFailure::Marker(error)) => {
                        tracing::error!("failed to publish staged Platform markers: {error}");
                        LegFlow::Faulted
                    }
                }
            }
            Self::Buffered(ops) => {
                match report_projected_delivery(
                    projection,
                    ops.ledger,
                    batch,
                    ProjectionDelivery::Sent,
                )
                .await
                {
                    Ok(_) => LegFlow::Open,
                    Err(error) => LegFlow::Failed(LegFailure::Projection(error)),
                }
            }
        }
    }

    /// The leg's current Client Output Commit state.
    pub(super) fn client_commit(&self, projection: &ClientProjectionSession) -> ClientOutputCommit {
        match self {
            Self::Live(_) => ClientOutputCommit::of(projection.client_output_committed()),
            Self::Buffered(_) => ClientOutputCommit::Pending,
        }
    }
}

impl LiveLegOps<'_> {
    async fn deliver(
        &mut self,
        projection: &mut ClientProjectionSession,
        batch: ProjectedDeltaBatch,
    ) -> Result<(), ProjectedDeliveryFailure> {
        deliver_projected(
            self.delivery,
            projection,
            self.ledger,
            self.observer,
            &self.model_turn_id,
            self.observe_delivery,
            batch,
        )
        .await
    }
}

/// Event-driven state machine consuming one Model Leg.
///
/// Lifecycle: `begin` performs the Model Leg ritual (projection boundary +
/// CompletionContext assembly), `feed` consumes canonical events, every
/// `Emit` reaction is run through `perform_emit`, `seal` merges the leg into
/// a canonical response, and `advance` hands off to completion and follow-up
/// acquisition. The module never holds `&mut InferenceRun` across calls —
/// the caller's `HookLegGuard` keeps ownership and flush-on-drop safety.
pub(super) struct ModelLegConsume {
    completion: CompletionContext,
    observer: crate::interaction_observation::RunObserver,
    ingress: ProtocolId,
    policy: LegPolicy,
    accumulator: Option<StreamResponseAccumulator>,
    terminal_deltas: Vec<AiStreamDelta>,
    completed: Option<AiResponse>,
    response: Option<AiResponse>,
    upstream_response_id: Option<String>,
    early_platform_executions: Vec<EarlyPlatformExecution>,
    tool_calls_complete: bool,
    faulted: bool,
    sealed: bool,
}

impl ModelLegConsume {
    /// Begin a Model Leg: reset the projection's leg boundary and assemble the
    /// completion context this leg's merge will need.
    pub(super) fn begin(
        env: LegEnv<'_>,
        turn: &ModelTurn,
        run: &InferenceRun,
        projection: &mut ClientProjectionSession,
        policy: LegPolicy,
    ) -> Self {
        let LegEnv {
            gateway,
            generation,
            ingress,
            observer,
        } = env;
        projection.begin_model_leg(
            super::thinking_carrier_facts(ingress, turn.route.egress),
            run.exposed_tool_names(),
            Some(crate::history_marker::ThinkingSource {
                namespace: turn.target.namespace.clone(),
                protocol: turn.route.egress,
                actual_model: turn.target.actual_model.clone(),
                target_id: turn.target.target_id.clone(),
            }),
        );
        Self {
            completion: CompletionContext::from_model_turn(
                gateway.clone(),
                generation.clone(),
                ingress,
                &turn.target,
                turn.route.egress,
                turn.model_turn_id.clone(),
                observer.clone(),
            ),
            observer: observer.clone(),
            ingress,
            policy,
            accumulator: turn.streamed.then(StreamResponseAccumulator::default),
            terminal_deltas: Vec::new(),
            completed: None,
            response: None,
            upstream_response_id: None,
            early_platform_executions: Vec::new(),
            tool_calls_complete: false,
            faulted: false,
            sealed: false,
        }
    }

    /// Terminal deltas observed so far — the caller's tail re-sends
    /// `ResponseTerminal` and recovers the native error from these.
    pub(super) fn terminal_deltas(&self) -> &[AiStreamDelta] {
        &self.terminal_deltas
    }

    /// The leg's placeholder response for failure paths that still need one.
    pub(super) fn empty_response(&self) -> AiResponse {
        self.completion.empty_response()
    }

    /// Consume one canonical event. Synchronous: Hook stream transformation is
    /// in-process, so the pump never suspends inside `feed`.
    pub(super) fn feed(
        &mut self,
        run: &mut InferenceRun,
        event: Result<CanonicalEvent, ModelTurnError>,
    ) -> LegReaction {
        debug_assert!(!self.sealed, "feed after seal");
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                self.faulted = true;
                return LegReaction::Failed(LegFailure::ModelTurn(error));
            }
        };
        match event {
            CanonicalEvent::Delta(delta) => {
                if self.accumulator.is_none() {
                    return LegReaction::Absorbed;
                }
                let (terminal, deltas) = partition_terminal_deltas(vec![delta]);
                self.tool_calls_complete |= terminal.iter().any(|delta| {
                    matches!(
                        delta,
                        AiStreamDelta::Done { stop_reason } if stop_reason == "tool_calls"
                    )
                });
                if terminal_deltas_failed(&terminal) {
                    self.faulted = true;
                    self.terminal_deltas.extend(terminal);
                    let error = self.terminal_deltas.iter().find_map(|delta| {
                        if let AiStreamDelta::StreamError { error } = delta {
                            Some(error.clone())
                        } else {
                            None
                        }
                    });
                    if let Some(error) = &error {
                        self.observer.record_failure(
                            crate::interaction_observation::FailureDiagnostic {
                                source: Some("upstream".into()),
                                code: Some("upstream_stream_error".into()),
                                message: Some(error.message.clone()),
                                status_code: error.status_code,
                                upstream_code: error
                                    .raw
                                    .as_ref()
                                    .and_then(crate::interaction_observation::upstream_body_code),
                            },
                        );
                    }
                    return LegReaction::Failed(LegFailure::TerminalFault(error));
                }
                self.terminal_deltas.extend(terminal);
                let mut transformed = match transform_stream_deltas(run, deltas) {
                    Ok(transformed) => transformed,
                    Err(error) => {
                        self.faulted = true;
                        return LegReaction::Failed(LegFailure::Hook(error));
                    }
                };
                if self.upstream_response_id.is_none() {
                    self.upstream_response_id = transformed.iter().find_map(|delta| match delta {
                        AiStreamDelta::MessageStart { id, .. } if !id.is_empty() => {
                            Some(id.clone())
                        }
                        _ => None,
                    });
                }
                apply_response_identity(
                    &mut transformed,
                    self.completion.generation_chain_identity(),
                );
                self.accumulator
                    .as_mut()
                    .expect("streamed Model Leg accumulator")
                    .apply_all(&transformed);
                LegReaction::Emit(transformed)
            }
            CanonicalEvent::Completed(response) => {
                self.completed = Some(*response);
                LegReaction::Ended
            }
            CanonicalEvent::Compacted(_) => {
                self.faulted = true;
                LegReaction::Failed(LegFailure::UnexpectedCompaction)
            }
        }
    }

    /// Emit a transformed batch and run the early-Platform pipeline on it.
    /// Callers map the returned flow onto their own transport flags.
    pub(super) async fn perform_emit(
        &mut self,
        ops: &mut LegOps<'_>,
        run: &mut InferenceRun,
        projection: &mut ClientProjectionSession,
        deltas: Vec<AiStreamDelta>,
    ) -> LegFlow {
        if self.policy.emit_live {
            match ops
                .emit_deltas(projection, deltas.clone(), !self.terminal_deltas.is_empty())
                .await
            {
                LegFlow::Open => {}
                flow => {
                    self.faulted |= matches!(flow, LegFlow::Faulted | LegFlow::Failed(_));
                    return flow;
                }
            }
        }
        if !self.policy.early_platform {
            return LegFlow::Open;
        }
        let mut completed_platform_calls = deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::ToolCallComplete { tool_call, .. } => Some(tool_call.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if self.tool_calls_complete
            && let Some(accumulator) = self.accumulator.as_ref()
        {
            completed_platform_calls.extend(accumulator.tool_calls().cloned());
        }
        let mut completed_call_ids = HashSet::new();
        completed_platform_calls.retain(|call| {
            run.is_exposed_tool(&call.name)
                && completed_call_ids.insert(call.id.clone())
                && !self
                    .early_platform_executions
                    .iter()
                    .any(|early: &EarlyPlatformExecution| early.marker.call_id() == call.id)
        });
        for call in completed_platform_calls {
            let platform_call = run
                .classify_tool_calls(&AiResponse {
                    items: vec![
                        stravia_runtime_contract::protocol::ir::AiItem::function_call(call),
                    ],
                    ..self.completion.empty_response()
                })
                .platform
                .into_iter()
                .next()
                .expect("classified Platform Tool call");
            let execution = run.detached_platform_execution(
                platform_call,
                stravia_runtime_contract::CancellationToken::new(),
            );
            let (markers, jobs) =
                match prepare_platform_markers(&self.completion, vec![execution]).await {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.faulted = true;
                        return LegFlow::Failed(LegFailure::Projection(error));
                    }
                };
            for marker in &markers {
                match ops.emit_platform_marker(projection, marker.marker()).await {
                    LegFlow::Open => {}
                    flow => {
                        self.faulted |= matches!(flow, LegFlow::Faulted | LegFlow::Failed(_));
                        return flow;
                    }
                }
            }
            let started = self
                .completion
                .gateway()
                .start_history_marker_executions(self.completion.principal().clone(), jobs);
            self.early_platform_executions.extend(
                markers
                    .into_iter()
                    .zip(started)
                    .map(|(marker, execution)| EarlyPlatformExecution { marker, execution }),
            );
        }
        LegFlow::Open
    }

    /// Close the leg: apply the Hook flush, emit it for live transports, and
    /// merge the accumulated stream with the Completed event into the leg's
    /// canonical response.
    pub(super) async fn seal(
        &mut self,
        ops: &mut LegOps<'_>,
        projection: &mut ClientProjectionSession,
        hook_close: Result<Vec<AiStreamDelta>, HookError>,
        interrupted: bool,
    ) -> SealOutcome {
        debug_assert!(!self.sealed, "Model Leg sealed twice");
        self.sealed = true;
        let mut flushed = match hook_close {
            Ok(flushed) => flushed,
            Err(error) => {
                self.faulted = true;
                return SealOutcome::Failed(LegFailure::Hook(error));
            }
        };
        if let Some(accumulator) = self.accumulator.as_mut() {
            apply_response_identity(&mut flushed, self.completion.generation_chain_identity());
            accumulator.apply_all(&flushed);
            if self.policy.emit_live && !interrupted {
                match ops.emit_deltas(projection, flushed, true).await {
                    LegFlow::Open => {}
                    LegFlow::Disrupted(progress) => return SealOutcome::Disrupted(progress),
                    LegFlow::Faulted => {
                        self.faulted = true;
                        return SealOutcome::Aborted;
                    }
                    LegFlow::Failed(failure) => {
                        self.faulted = true;
                        return SealOutcome::Failed(failure);
                    }
                }
            }
            let mut accumulator = self
                .accumulator
                .take()
                .expect("streamed Model Leg accumulator");
            accumulator.apply_all(&self.terminal_deltas);
            let mut response = accumulator.into_ai_response();
            let Some(mut completed) = self.completed.take() else {
                if self.faulted || interrupted {
                    self.response = Some(response);
                    return SealOutcome::Ready;
                }
                return SealOutcome::Failed(LegFailure::Incomplete);
            };
            if let Err(error) =
                reconcile_completed_media(&mut response, std::mem::take(&mut completed.items))
            {
                self.faulted = true;
                return SealOutcome::Failed(LegFailure::Reconcile(error));
            }
            if response.usage.prompt_tokens == 0 && response.usage.completion_tokens == 0 {
                response.usage = completed.usage;
            }
            if response.stop_reason.is_none() {
                response.stop_reason = completed.stop_reason;
            }
            if response.id.is_empty() {
                response.id = completed.id;
            }
            self.finish_merge(response)
        } else {
            match self.completed.take() {
                Some(completed) => self.finish_merge(completed),
                None if self.faulted || interrupted => {
                    self.response = Some(self.completion.empty_response());
                    SealOutcome::Ready
                }
                None => SealOutcome::Failed(LegFailure::Incomplete),
            }
        }
    }

    fn finish_merge(&mut self, response: AiResponse) -> SealOutcome {
        // The upstream MessageStart id wins; fall back to the Completed id so
        // non-streaming upstreams still offer a continuation anchor.
        self.upstream_response_id = self
            .upstream_response_id
            .take()
            .or_else(|| (!response.id.is_empty()).then(|| response.id.clone()));
        self.response = Some(response);
        SealOutcome::Ready
    }

    /// Hand the sealed leg to completion. A Platform-only leg publishes its
    /// staged markers, finishes the hidden round, and acquires the follow-up
    /// Model Turn inside the same leg loop.
    pub(super) async fn advance(
        &mut self,
        ops: &mut LegOps<'_>,
        parts: LegParts<'_>,
        env: FollowupEnv<'_>,
    ) -> LegAdvance {
        debug_assert!(self.sealed, "advance before seal");
        let LegParts {
            request,
            run,
            phase,
            projection,
            ledger,
        } = parts;
        let completion = complete_canonical_response(
            &self.completion,
            CompletionInput {
                request: &mut *request,
                run: &mut *run,
                phase: &mut *phase,
                response: self.response.take().expect("sealed Model Leg response"),
                upstream_response_id: self.upstream_response_id.take(),
                early_platform_executions: std::mem::take(&mut self.early_platform_executions),
                projection: &mut *projection,
                ledger,
            },
        )
        .await;
        let (continuation, staged_delivery) = match completion {
            CompletionOutcome::PlatformOnly {
                continuation,
                staged_delivery,
            } => (continuation, staged_delivery),
            CompletionOutcome::Ready(lease) => {
                return match (*lease).prepare(&mut *phase) {
                    Ok(prepared) => LegAdvance::Ready(Box::new(prepared)),
                    Err(failure) => LegAdvance::Failed(LegFailure::Completion(failure)),
                };
            }
            CompletionOutcome::Failed(failure) => {
                return LegAdvance::Failed(LegFailure::Completion(failure));
            }
        };
        match ops.emit_staged(projection, staged_delivery).await {
            LegFlow::Open => {}
            LegFlow::Disrupted(progress) => return LegAdvance::Disrupted(progress),
            LegFlow::Faulted => return LegAdvance::Aborted,
            LegFlow::Failed(failure) => return LegAdvance::Failed(failure),
        }
        if let Err(failure) = continuation
            .finish(&self.completion, ledger, request, run, phase)
            .await
        {
            return LegAdvance::Failed(LegFailure::Completion(failure));
        }
        match acquire_followup_model_turn(FollowupLeg {
            executor: env.executor,
            headers: env.headers,
            request,
            ingress: self.ingress,
            request_context: env.request_context,
            ledger,
            inference_run: run,
            projection,
            phase,
            generation: env.generation,
            fixed_media_plan: env.fixed_media_plan,
        })
        .await
        {
            Ok(FollowupModelTurn::Turn(turn)) => LegAdvance::NextLeg(turn),
            Ok(FollowupModelTurn::HookResponse(plan)) => LegAdvance::HookResponse(plan),
            Ok(FollowupModelTurn::StreamError(error)) => LegAdvance::StreamError(error),
            Err(outcome) => LegAdvance::Outcome(outcome),
        }
    }
}

/// Render a leg failure for the caller's transport shape.
pub(super) fn render_leg_failure(
    failure: LegFailure,
    request: &AiRequest,
    ingress: ProtocolId,
    is_stream: bool,
    commit: ClientOutputCommit,
) -> RoundOutcome {
    match failure {
        LegFailure::ModelTurn(error) => model_turn_error_outcome(error),
        LegFailure::UnexpectedCompaction => model_turn_error_outcome(ModelTurnError::new(
            "unexpected_compaction_terminal",
            "Generation received a standalone compact result",
        )),
        LegFailure::TerminalFault(error) => error
            .and_then(|error| compaction_stream_error_outcome(request, &error))
            .unwrap_or_else(|| buffered_response(error_response(502, "upstream stream error"))),
        LegFailure::Hook(error) => buffered_response(hook_failure_response(error)),
        LegFailure::Projection(error) => buffered_response(render_completion_failure(
            CompletionFailure::hook(error, commit),
            ingress,
            is_stream,
        )),
        LegFailure::Completion(failure) => {
            buffered_response(render_completion_failure(failure, ingress, is_stream))
        }
        LegFailure::Incomplete => model_turn_error_outcome(ModelTurnError::new(
            "model_stream_incomplete",
            "Model Turn ended without a completion",
        )),
        LegFailure::Reconcile(error) => model_turn_error_outcome(ModelTurnError::new(
            "output_media_reconciliation_failed",
            error,
        )),
    }
}

/// Render a hidden-round Hook stream error as a buffered response. A `Reject`
/// carries the Hook's status and code; anything else is a Hook failure.
pub(super) fn buffered_stream_error_response(error: &AiError) -> Response {
    match error.status_code {
        Some(status) => coded_error_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            error
                .raw
                .as_ref()
                .and_then(|raw| raw.get("code"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("stream_error"),
            &error.message,
        ),
        None => hook_failure_response(&error.message),
    }
}

pub(super) enum ProjectedDeliveryFailure {
    Delivery(DeliveryProgress),
    Marker(crate::history_marker::HistoryMarkerError),
}

pub(super) fn record_marker_failure(
    observer: &crate::interaction_observation::RunObserver,
    error: &crate::history_marker::HistoryMarkerError,
) {
    observer.record_response_failure(crate::interaction_observation::FailureDiagnostic {
        source: Some("platform".into()),
        code: Some("marker_publish_failed".into()),
        message: Some(error.to_string()),
        status_code: None,
        upstream_code: None,
    });
}

/// Deliver one projected batch over the live wire and report its markers back
/// to the projection session.
pub(super) async fn deliver_projected(
    delivery: &mut DeliveryAdapter,
    projection: &mut ClientProjectionSession,
    ledger: &RunLedger,
    observer: &crate::interaction_observation::RunObserver,
    model_turn_id: &str,
    observe_delivery: bool,
    batch: ProjectedDeltaBatch,
) -> Result<(), ProjectedDeliveryFailure> {
    let debug_payload = if observe_delivery && observer.debug_enabled() {
        super::checkpoint_payload(observer, batch.deltas())
    } else {
        serde_json::Value::Null
    };
    let visible_text = if observe_delivery {
        batch
            .deltas()
            .iter()
            .filter_map(super::visible_delta_text)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let progress = delivery.send_deltas(batch.deltas()).await;
    let outcome = if progress == DeliveryProgress::Sent {
        ProjectionDelivery::Sent
    } else {
        ProjectionDelivery::Cancelled
    };
    report_projected_delivery(projection, ledger, batch, outcome)
        .await
        .map_err(|error| {
            record_marker_failure(observer, &error);
            ProjectedDeliveryFailure::Marker(error)
        })?;
    if progress == DeliveryProgress::Sent {
        if observe_delivery {
            observer.record_debug(|| crate::interaction_observation::RunEvent::Content {
                stage: "client_projection_content".into(),
                model_turn_id: Some(model_turn_id.to_owned()),
                attempt_id: None,
                payload: debug_payload,
            });
            for text in visible_text {
                if !text.is_empty() {
                    observer.record(
                        crate::interaction_observation::RunEvent::ClientVisibleContentDelta {
                            text,
                        },
                    );
                }
            }
        }
        Ok(())
    } else {
        Err(ProjectedDeliveryFailure::Delivery(progress))
    }
}

/// Owns the `&mut InferenceRun` for one Model Leg so the Hook stream leg is
/// flushed exactly once — on `close` or on drop if the caller bails early.
pub(super) struct HookLegGuard<'a> {
    run: &'a mut InferenceRun,
    closed: bool,
}

impl<'a> HookLegGuard<'a> {
    pub(super) fn new(run: &'a mut InferenceRun) -> Self {
        Self { run, closed: false }
    }

    pub(super) fn run_mut(&mut self) -> &mut InferenceRun {
        self.run
    }

    pub(super) async fn close(&mut self) -> Result<Vec<AiStreamDelta>, HookError> {
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
            if let Err(error) = self.run.flush_stream() {
                tracing::debug!(%error, "failed to flush stream on hook leg drop");
            }
            self.closed = true;
        }
    }
}

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

fn transform_stream_deltas(
    inference_run: &mut InferenceRun,
    deltas: Vec<AiStreamDelta>,
) -> Result<Vec<AiStreamDelta>, HookError> {
    let mut transformed = Vec::new();
    for delta in deltas {
        transformed.extend(inference_run.transform_stream(delta)?);
    }
    Ok(transformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiItem, AiRequest, AiResponse};

    const INGRESS: ProtocolId = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;

    struct LegFixture {
        gateway: crate::Gateway,
        observer: crate::interaction_observation::RunObserver,
        projection: ClientProjectionSession,
        ledger: RunLedger,
        generation: GenerationChainRun,
        run: crate::hook::InferenceRun,
    }

    async fn fixture() -> LegFixture {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let directory = tempfile::tempdir().expect("temp dir");
        let gateway = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        let observation = crate::interaction_observation::InteractionObservation::new(
            Some(pool),
            None,
            directory.path().to_path_buf(),
            7,
            true,
            gateway.generation_chains.clone(),
        )
        .await;
        let observer = observation
            .observe_ingress(crate::interaction_observation::IngressStart {
                id: "leg-run".into(),
                method: "POST".into(),
                path: "/v1/chat/completions".into(),
                protocol: INGRESS.to_string(),
            })
            .admit(
                crate::interaction_observation::RunStart {
                    id: "leg-run".into(),
                    principal: "owner".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "local-route".into(),
                    model_display_name: None,
                    ingress_protocol: INGRESS.to_string(),
                },
                crate::interaction_observation::AdmissionFacts {
                    client_request: AiRequest::new("model", Vec::new()),
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    generation_root_id: None,
                    generation_parent_id: None,
                },
            );
        let principal = stravia_runtime_contract::Principal::new("owner");
        let request = AiRequest::new("model", Vec::<AiItem>::new());
        let run = crate::hook::HookRuntime::new(Vec::new())
            .begin(
                stravia_runtime_contract::hook::SessionContext {
                    tools_fixed: false,
                    request_id: "req-1".into(),
                    run_id: "leg-run".into(),
                    request_kind: stravia_runtime_contract::hook::RequestKind::Generation,
                    ingress: INGRESS,
                    transport: stravia_runtime_contract::hook::TransportKind::Http,
                    principal: principal.clone(),
                    cancellation: stravia_runtime_contract::CancellationToken::new(),
                    inherited_media_turns: Vec::new(),
                    response_id: None,
                    previous_response_id: None,
                },
                &request,
                stravia_runtime_contract::hook::ContextCompleteness::Full,
            )
            .expect("Inference Run");
        let projection = ClientProjectionSession::new(
            gateway.history_markers.clone(),
            principal.clone(),
            INGRESS,
        );
        let ledger = RunLedger::new(
            crate::proxy::dispatcher::inference_run::RunTerminalContext::new(
                None,
                None,
                Vec::new(),
                gateway.compaction.clone(),
                principal.clone(),
                crate::model_turn::CompactionPublications::default(),
            ),
            crate::model_turn::CompactionPublications::default(),
        );
        let generation = GenerationChainRun {
            principal,
            write: None,
            client_request: request,
            previous_response_id: None,
            compaction_source_generation_id: None,
        };
        LegFixture {
            gateway,
            observer,
            projection,
            ledger,
            generation,
            run,
        }
    }

    fn streamed_turn(events: Vec<Result<CanonicalEvent, ModelTurnError>>) -> ModelTurn {
        let mut turn = ModelTurn::in_memory(
            stravia_runtime_contract::hook::RouteContext {
                model_id: "model".into(),
                provider_id: "provider".into(),
                target_id: "target".into(),
                egress: INGRESS,
            },
            AiRequest::new("model", Vec::<AiItem>::new()),
            events,
        );
        turn.streamed = true;
        turn
    }

    fn begin_leg(fx: &mut LegFixture, turn: &ModelTurn) -> ModelLegConsume {
        ModelLegConsume::begin(
            LegEnv {
                gateway: &fx.gateway,
                generation: &fx.generation,
                ingress: INGRESS,
                observer: &fx.observer,
            },
            turn,
            &fx.run,
            &mut fx.projection,
            LegPolicy {
                emit_live: false,
                early_platform: true,
            },
        )
    }

    fn text_delta(text: &str) -> Result<CanonicalEvent, ModelTurnError> {
        Ok(CanonicalEvent::Delta(AiStreamDelta::TextDelta(
            text.to_owned(),
        )))
    }

    fn completed(id: &str) -> Result<CanonicalEvent, ModelTurnError> {
        Ok(CanonicalEvent::Completed(Box::new(AiResponse::new(
            id, "model",
        ))))
    }

    #[tokio::test]
    async fn deltas_emit_transformed() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        let reaction = leg.feed(&mut fx.run, text_delta("hi"));
        match reaction {
            LegReaction::Emit(deltas) => assert_eq!(deltas.len(), 1),
            _ => panic!("expected Emit"),
        }
    }

    #[tokio::test]
    async fn completed_event_ends_the_leg() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        assert!(matches!(
            leg.feed(&mut fx.run, completed("resp-1")),
            LegReaction::Ended
        ));
    }

    /// Terminal stream faults fail the whole Model Leg on every transport —
    /// buffered callers must not absorb them into a 200 response.
    #[tokio::test]
    async fn terminal_stream_error_fails_the_leg() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        let reaction = leg.feed(
            &mut fx.run,
            Ok(CanonicalEvent::Delta(AiStreamDelta::StreamError {
                error: AiError::new(AiErrorKind::StreamMidError, "upstream blew up"),
            })),
        );
        assert!(matches!(
            reaction,
            LegReaction::Failed(LegFailure::TerminalFault(Some(_)))
        ));
    }

    #[tokio::test]
    async fn unexpected_eof_fails_the_leg() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        let reaction = leg.feed(
            &mut fx.run,
            Ok(CanonicalEvent::Delta(AiStreamDelta::UnexpectedEof)),
        );
        assert!(matches!(
            reaction,
            LegReaction::Failed(LegFailure::TerminalFault(None))
        ));
    }

    #[tokio::test]
    async fn errored_event_fails_the_leg() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        let reaction = leg.feed(
            &mut fx.run,
            Err(ModelTurnError::new("upstream_status", "provider 500")),
        );
        assert!(matches!(
            reaction,
            LegReaction::Failed(LegFailure::ModelTurn(_))
        ));
    }

    #[tokio::test]
    async fn standalone_compaction_fails_the_leg() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        let reaction = leg.feed(
            &mut fx.run,
            Ok(CanonicalEvent::Compacted(Box::new(
                stravia_runtime_contract::protocol::ir::NativeCompactionResponse {
                    wire: serde_json::Value::Null,
                    items: Vec::new(),
                    usage: None,
                },
            ))),
        );
        assert!(matches!(
            reaction,
            LegReaction::Failed(LegFailure::UnexpectedCompaction)
        ));
    }

    /// When the upstream never sends a `MessageStart`, the Completed response id
    /// still anchors continuation — the live path previously dropped it.
    #[tokio::test]
    async fn seal_falls_back_to_completed_response_id() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        leg.feed(&mut fx.run, text_delta("hi"));
        assert!(matches!(
            leg.feed(&mut fx.run, completed("resp-9")),
            LegReaction::Ended
        ));
        let mut ops = LegOps::Buffered(BufferedLegOps { ledger: &fx.ledger });
        let outcome = leg
            .seal(&mut ops, &mut fx.projection, Ok(Vec::new()), false)
            .await;
        assert!(matches!(outcome, SealOutcome::Ready));
        assert_eq!(leg.upstream_response_id.as_deref(), Some("resp-9"));
    }

    #[tokio::test]
    async fn message_start_id_wins_over_completed_id() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        leg.feed(
            &mut fx.run,
            Ok(CanonicalEvent::Delta(AiStreamDelta::MessageStart {
                id: "upstream-1".into(),
                model: "model".into(),
            })),
        );
        assert!(matches!(
            leg.feed(&mut fx.run, completed("resp-9")),
            LegReaction::Ended
        ));
        let mut ops = LegOps::Buffered(BufferedLegOps { ledger: &fx.ledger });
        let outcome = leg
            .seal(&mut ops, &mut fx.projection, Ok(Vec::new()), false)
            .await;
        assert!(matches!(outcome, SealOutcome::Ready));
        assert_eq!(leg.upstream_response_id.as_deref(), Some("upstream-1"));
    }

    #[tokio::test]
    async fn seal_without_completion_is_incomplete() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        leg.feed(&mut fx.run, text_delta("hi"));
        let mut ops = LegOps::Buffered(BufferedLegOps { ledger: &fx.ledger });
        let outcome = leg
            .seal(&mut ops, &mut fx.projection, Ok(Vec::new()), false)
            .await;
        assert!(matches!(
            outcome,
            SealOutcome::Failed(LegFailure::Incomplete)
        ));
    }

    /// A disrupted transport suppresses the Incomplete verdict — the leg is
    /// left faulted, not converted into a missing-completion error.
    #[tokio::test]
    async fn interrupted_seal_is_not_incomplete() {
        let mut fx = fixture().await;
        let turn = streamed_turn(Vec::new());
        let mut leg = begin_leg(&mut fx, &turn);
        leg.feed(&mut fx.run, text_delta("hi"));
        let mut ops = LegOps::Buffered(BufferedLegOps { ledger: &fx.ledger });
        let outcome = leg
            .seal(&mut ops, &mut fx.projection, Ok(Vec::new()), true)
            .await;
        assert!(matches!(outcome, SealOutcome::Ready));
    }

    #[tokio::test]
    async fn non_streamed_leg_seals_from_completed() {
        let mut fx = fixture().await;
        let turn = ModelTurn::in_memory(
            stravia_runtime_contract::hook::RouteContext {
                model_id: "model".into(),
                provider_id: "provider".into(),
                target_id: "target".into(),
                egress: INGRESS,
            },
            AiRequest::new("model", Vec::<AiItem>::new()),
            Vec::new(),
        );
        let mut leg = begin_leg(&mut fx, &turn);
        assert!(matches!(
            leg.feed(&mut fx.run, completed("resp-4")),
            LegReaction::Ended
        ));
        let mut ops = LegOps::Buffered(BufferedLegOps { ledger: &fx.ledger });
        let outcome = leg
            .seal(&mut ops, &mut fx.projection, Ok(Vec::new()), false)
            .await;
        assert!(matches!(outcome, SealOutcome::Ready));
        assert_eq!(leg.upstream_response_id.as_deref(), Some("resp-4"));
    }
}
