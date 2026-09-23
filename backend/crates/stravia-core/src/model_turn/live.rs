use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{Stream, stream};

use super::provider::AttemptObservation;
use super::support::ai_response_to_deltas;
use super::{
    CanonicalEvent, ModelTurn, ModelTurnAuthorization, ModelTurnError, ModelTurnExecutor,
    TargetIdentity, TurnInput, VendorPublication,
};
use crate::Gateway;
use crate::error::GatewayError;
use crate::interaction_observation::RunEvent;
use crate::plugin::execution::PreparedVendorExecution;
use crate::plugin::{
    VendorCallContext, VendorEvent, VendorExecution, VendorPublicationFence, VendorRequest,
};
use crate::proxy::security::Security;
use crate::router::{
    AttemptFailureDisposition, RouteAttemptContext, RouteAttemptPolicy, RouteAttemptReservation,
    RoutePolicyState, SelectedTarget, selected_target_key,
};
use crate::router::{ContinuationLookup, ContinuationTarget};
use stravia_runtime_contract::Deadline;
use stravia_runtime_contract::hook::RouteContext;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::request::MediaRoutingMode;
use stravia_runtime_contract::thinking::ThinkingLevel;
use stravia_vendor_runtime::{RuntimeError, RuntimeEvent};
use stravia_vendor_sdk::{
    Capability, ErrorKind, OperationOutput, TRANSPORT_PREFERENCE_METADATA_KEY, TransportFailure,
    TransportPreference,
};

#[derive(Clone)]
pub struct LiveModelTurnExecutor {
    gateway: Gateway,
    continuation: Arc<dyn ContinuationLookup>,
    selector: crate::router::RouteSelector,
}

impl LiveModelTurnExecutor {
    pub fn new(gateway: Gateway, continuation: Arc<dyn ContinuationLookup>) -> Self {
        let selector = crate::router::RouteSelector::new(
            gateway.storage.clone(),
            gateway.cache_affinity.clone(),
            continuation.clone(),
            gateway.route_policy_state.clone(),
        );
        Self {
            gateway,
            continuation,
            selector,
        }
    }
}

#[async_trait]
impl ModelTurnExecutor for LiveModelTurnExecutor {
    async fn execute(&self, mut input: TurnInput) -> Result<ModelTurn, ModelTurnError> {
        if !input.attachments_normalized {
            tokio::select! {
                biased;
                _ = input.cancellation.cancelled() => return Err(interruption_error(&input.deadline)),
                () = input.deadline.wait() => return Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")),
                result = crate::media::ingest::normalize_request(&self.gateway, &input.principal, &mut input.request, &input.cancellation) => result.map_err(attachment_ingest_error)?,
            }
        }
        let model_turn_id = stravia_runtime_contract::identifier::new_id();
        let operation_started = Instant::now();
        let standalone = input.purpose == super::ModelTurnPurpose::Compact;
        let observer = input.observer.clone();
        if standalone && let Some(observer) = &observer {
            observer.record(RunEvent::CompactionOperation {
                operation_id: model_turn_id.clone(),
                model_turn_id: model_turn_id.clone(),
                attempt_id: None,
                mode: crate::interaction_observation::CompactionMode::Standalone,
                phase: crate::interaction_observation::CompactionPhase::Started,
                source_generation_id: None,
                source_operation_id: None,
                registration_id: None,
                duration_ms: None,
                error_code: None,
            });
        }
        if let Some(observer) = &observer {
            let (route_id, model_display_name) = self
                .gateway
                .model_cache
                .read()
                .await
                .resolve(&input.request.model)
                .map(|route| {
                    (
                        route.id.clone(),
                        Some(route.effective_display_name().to_owned()),
                    )
                })
                .unwrap_or_else(|| (input.request.model.clone(), None));
            observer.record(RunEvent::ModelTurnStarted {
                model_turn_id: model_turn_id.clone(),
                route_id,
                model_display_name,
            });
        }
        let mut terminal = Some(ModelTurnTerminal {
            observer: observer.clone(),
            model_turn_id: model_turn_id.clone(),
            standalone,
            operation_started,
            finished: false,
        });
        let result = if input.cancellation.is_cancelled() {
            Err(interruption_error(&input.deadline))
        } else if input.deadline.is_exceeded() {
            Err(ModelTurnError::new(
                "deadline_exceeded",
                "Model Turn deadline exceeded",
            ))
        } else {
            let deadline = input.deadline.clone();
            let cancellation = input.cancellation.clone();
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Err(interruption_error(&deadline))
                }
                () = deadline.wait() => {
                    Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded"))
                }
                result = async {
                    let trace = input.request.meta.redaction.clone();
                    let principal = input.principal.clone();
                    let source_generation_id = input.compaction_source_generation_id.clone();
                    let incoming_states = input.request.items.iter().filter_map(stravia_runtime_contract::protocol::ir::canonical::native_compaction_item).collect::<Vec<_>>();
                    let source = self.gateway.compaction.resolve(&principal, &input.request.items).await
                        .map_err(|error| ModelTurnError::new(error.code(), error.to_string()))?;
                    let mappings = self.gateway.redaction
                        .protect(&input.principal, &mut input.request, observer.as_ref()).await?;
                    if let Some(observer) = &observer {
                        observer.protect_secrets(mappings.iter().map(|mapping| mapping.secret.as_str()));
                        observer.publish_input_preview();
                    }
                    let registrations = input.compaction_records.clone();
                    let mut turn = execute_inner(self.clone(), input, model_turn_id.clone()).await?;
                    turn.output = self.gateway.redaction.restore_stream(turn.output, mappings, trace.clone());
                    let thinking_source = crate::history_marker::ThinkingSource {
                        namespace: turn.target.namespace.clone(),
                        protocol: turn.target.protocol_identity(),
                        actual_model: turn.target.actual_model.clone(),
                        target_id: turn.target.target_id.clone(),
                    };
                    {
                        use futures::StreamExt;
                        turn.output = Box::pin(turn.output.map(move |mut event| {
                            match &mut event {
                                Ok(CanonicalEvent::Completed(response)) => {
                                    thinking_source.stamp_response(response);
                                }
                                Ok(CanonicalEvent::Delta(AiStreamDelta::ItemDone { item, .. }))
                                    if matches!(
                                    &item.content,
                                    stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks)
                                        if blocks.iter().any(|block| matches!(
                                            block,
                                            stravia_runtime_contract::protocol::ir::ContentBlock::Thinking { .. }
                                                | stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning { .. }
                                                | stravia_runtime_contract::protocol::ir::ContentBlock::RedactedThinking { .. }
                                        ))
                                ) => thinking_source.stamp_item(item),
                                _ => {}
                            }
                            event
                        }));
                    }
                    turn.output = register_compaction_stream(CompactionStreamSpec {
                        output: turn.output,
                        compaction: self.gateway.compaction.clone(),
                        principal: principal.clone(),
                        target: crate::compaction::CompactionTarget { target_key: turn.target.target_id.clone(), namespace: turn.target.namespace.clone(), model: turn.target.actual_model.clone(), protocol: turn.target.protocol_hint.clone() },
                        source_generation_id,
                        source_record_ids: source.map(|source| source.record_ids).unwrap_or_default(),
                        model_turn_id: model_turn_id.clone(),
                        registrations,
                        observer: observer.clone(),
                        incoming_states,
                        operation_started,
                        publication: turn.target.publication.clone(),
                    });
                    turn.output = completion_stream(CompletionStreamSpec {
                        output: turn.output,
                        redaction: self.gateway.redaction.clone(),
                        principal,
                        trace,
                        cancellation: cancellation.clone(),
                        deadline: deadline.clone(),
                        publication: turn.target.publication.clone(),
                        terminal: terminal.take().expect("Model Turn terminal owner"),
                    });
                    Ok::<_, ModelTurnError>(turn)
                } => result,
            }
        };
        if let Err(error) = &result {
            terminal
                .as_mut()
                .expect("Model Turn terminal owner")
                .finish(&error.code);
        }
        result
    }
}

struct CompactionStreamSpec {
    output: super::CanonicalEventStream,
    compaction: crate::compaction::Compaction,
    principal: stravia_runtime_contract::Principal,
    target: crate::compaction::CompactionTarget,
    source_generation_id: Option<String>,
    source_record_ids: Vec<String>,
    model_turn_id: String,
    registrations: super::CompactionPublications,
    observer: Option<crate::interaction_observation::RunObserver>,
    incoming_states: Vec<serde_json::Value>,
    operation_started: Instant,
    publication: Option<VendorPublication>,
}

fn register_compaction_stream(spec: CompactionStreamSpec) -> super::CanonicalEventStream {
    use crate::interaction_observation::{CompactionMode, CompactionPhase};
    use futures::StreamExt;
    use stravia_runtime_contract::protocol::ir::canonical::native_compaction_item;
    let operation_started = spec.operation_started;
    let state = (
        spec.output,
        spec.compaction,
        spec.principal,
        spec.target,
        spec.source_generation_id,
        spec.source_record_ids,
        spec.model_turn_id,
        spec.registrations,
        spec.observer,
        spec.incoming_states,
        spec.publication,
        false,
    );
    Box::pin(stream::unfold(state, move |mut state| async move {
        let (
            output,
            compaction,
            principal,
            target,
            source_generation_id,
            source_record_ids,
            model_turn_id,
            registrations,
            observer,
            seen,
            publication,
            failed,
        ) = &mut state;
        if *failed {
            return None;
        }
        let mut event = output.next().await?;
        let unseen = |item: &&stravia_runtime_contract::protocol::ir::AiItem| {
            item.is_compaction()
                && native_compaction_item(item).is_some_and(|wire| !seen.contains(&wire))
        };
        let (items, window, mode) = match &event {
            Ok(CanonicalEvent::Delta(AiStreamDelta::ItemDone { item, .. })) if unseen(&item) => {
                (vec![item.clone()], Vec::new(), CompactionMode::Inline)
            }
            Ok(CanonicalEvent::Compacted(response)) => (
                response.items.iter().filter(unseen).cloned().collect(),
                response.items.clone(),
                CompactionMode::Standalone,
            ),
            Ok(CanonicalEvent::Completed(response)) => (
                response.items.iter().filter(unseen).cloned().collect(),
                Vec::new(),
                CompactionMode::Inline,
            ),
            _ => (Vec::new(), Vec::new(), CompactionMode::Inline),
        };
        let groups = if matches!(mode, CompactionMode::Standalone) {
            if items.is_empty() {
                Vec::new()
            } else {
                vec![(items, window)]
            }
        } else {
            items
                .into_iter()
                .map(|item| (vec![item.clone()], vec![item]))
                .collect()
        };
        for (state_items, window) in groups {
            let operation_id = if matches!(mode, CompactionMode::Standalone) {
                model_turn_id.clone()
            } else {
                stravia_runtime_contract::identifier::new_id()
            };
            let operation_event =
                |phase, registration_id, error_code| RunEvent::CompactionOperation {
                    operation_id: operation_id.clone(),
                    model_turn_id: model_turn_id.clone(),
                    attempt_id: None,
                    mode: mode.clone(),
                    phase,
                    source_generation_id: source_generation_id.clone(),
                    source_operation_id: None,
                    registration_id,
                    duration_ms: matches!(mode, CompactionMode::Standalone)
                        .then(|| operation_started.elapsed().as_millis() as i64),
                    error_code,
                };
            if matches!(mode, CompactionMode::Inline)
                && let Some(observer) = observer
            {
                observer.record(operation_event(CompactionPhase::Started, None, None));
            }
            let native_states = state_items
                .iter()
                .filter_map(native_compaction_item)
                .collect::<Vec<_>>();
            let publication_state = state_items[0].clone();
            let _publication_guard = match publication.as_ref() {
                Some(publication) => match publication.write_fence().await {
                    Ok(guard) => guard,
                    Err(_) => {
                        event = Err(ModelTurnError::new(
                            "cancelled",
                            "Vendor result can no longer be published",
                        ));
                        *failed = true;
                        break;
                    }
                },
                None => {
                    event = Err(ModelTurnError::new(
                        "vendor_publication_missing",
                        "Vendor result has no publication fence",
                    ));
                    *failed = true;
                    break;
                }
            };
            let result = compaction
                .register(
                    principal,
                    crate::compaction::CompactionRegistration {
                        source_generation_id: source_generation_id.clone(),
                        source_record_ids: source_record_ids.clone(),
                        operation_id: operation_id.clone(),
                        target: target.clone(),
                        window,
                        state_items,
                    },
                )
                .await;
            match result {
                Ok(record) => {
                    registrations.lock().push(super::CompactionPublication {
                        record_id: record.id.clone(),
                        operation_id: operation_id.clone(),
                        model_turn_id: model_turn_id.clone(),
                        mode: mode.clone(),
                        source_generation_id: source_generation_id.clone(),
                        state: publication_state,
                        receipt: super::CompactionReceipt::Pending,
                    });
                    seen.extend(native_states);
                    if let Some(observer) = observer {
                        observer.record(operation_event(
                            CompactionPhase::Registered,
                            Some(record.id.clone()),
                            None,
                        ));
                    }
                    // Each subsequent state is a new immutable boundary descending from this one.
                    source_record_ids.clear();
                    source_record_ids.push(record.id);
                }
                Err(error) => {
                    if let Some(observer) = observer {
                        observer.record(operation_event(
                            CompactionPhase::Failed,
                            None,
                            Some(error.code().to_owned()),
                        ));
                    }
                    event = Err(ModelTurnError::new(error.code(), error.to_string()));
                    *failed = true;
                    break;
                }
            }
        }
        Some((event, state))
    }))
}

struct ModelTurnTerminal {
    observer: Option<crate::interaction_observation::RunObserver>,
    model_turn_id: String,
    standalone: bool,
    operation_started: Instant,
    finished: bool,
}

impl ModelTurnTerminal {
    fn finish(&mut self, status: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::ModelTurnFinished {
                model_turn_id: self.model_turn_id.clone(),
                status: status.to_owned(),
            });
            if self.standalone && status != "completed" {
                observer.record(RunEvent::CompactionOperation {
                    operation_id: self.model_turn_id.clone(),
                    model_turn_id: self.model_turn_id.clone(),
                    attempt_id: None,
                    mode: crate::interaction_observation::CompactionMode::Standalone,
                    phase: crate::interaction_observation::CompactionPhase::Failed,
                    source_generation_id: None,
                    source_operation_id: None,
                    registration_id: None,
                    duration_ms: Some(self.operation_started.elapsed().as_millis() as i64),
                    error_code: Some(status.to_owned()),
                });
            }
        }
    }
}

impl Drop for ModelTurnTerminal {
    fn drop(&mut self) {
        self.finish("cancelled");
    }
}

struct CompletionStreamSpec {
    output: super::CanonicalEventStream,
    redaction: crate::reversible_redaction::ReversibleRedaction,
    principal: stravia_runtime_contract::Principal,
    trace: stravia_runtime_contract::redaction::RedactionTrace,
    cancellation: stravia_runtime_contract::CancellationToken,
    deadline: Deadline,
    publication: Option<VendorPublication>,
    terminal: ModelTurnTerminal,
}

fn completion_stream(spec: CompletionStreamSpec) -> super::CanonicalEventStream {
    use futures::StreamExt;

    let CompletionStreamSpec {
        output,
        redaction,
        principal,
        trace,
        cancellation,
        deadline,
        publication,
        terminal,
    } = spec;
    // Unfold retains its pending future in the stream, not in the caller's next()
    // future. Pausing consumption cannot restart a publication already in flight.
    let state = (
        output,
        redaction,
        principal,
        trace,
        cancellation,
        publication,
        terminal,
    );
    Box::pin(stream::unfold(state, move |mut state| {
        let deadline = deadline.clone();
        async move {
        let (output, redaction, principal, trace, cancellation, publication, terminal) =
            &mut state;
        if terminal.finished {
            return None;
        }
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(if deadline.is_exceeded() {
                ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")
            } else {
                ModelTurnError::new("cancelled", "Model Turn cancelled")
            }),
            () = deadline.wait() => Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")),
            result = async {
                match output.next().await {
                    Some(Ok(CanonicalEvent::Completed(response))) => {
                        // restore_stream has already yielded every trailing delta.
                        // Read the shared trace here, not when the turn was constructed.
                        let _publication_guard = publication
                            .as_ref()
                            .ok_or_else(|| ModelTurnError::new(
                                "vendor_publication_missing",
                                "Vendor result has no publication fence",
                            ))?
                            .write_fence()
                            .await
                            .map_err(|_| ModelTurnError::new(
                                "cancelled",
                                "Vendor result can no longer be published",
                            ))?;
                        redaction.publish(principal, trace).await?;
                        Ok(CanonicalEvent::Completed(response))
                    }
                    Some(Ok(CanonicalEvent::Compacted(response))) => {
                        let _publication_guard = publication
                            .as_ref()
                            .ok_or_else(|| ModelTurnError::new(
                                "vendor_publication_missing",
                                "Vendor result has no publication fence",
                            ))?
                            .write_fence()
                            .await
                            .map_err(|_| ModelTurnError::new(
                                "cancelled",
                                "Vendor result can no longer be published",
                            ))?;
                        redaction.publish(principal, trace).await?;
                        Ok(CanonicalEvent::Compacted(response))
                    }
                    Some(result) => result,
                    None => Err(ModelTurnError::new("model_stream_incomplete", "Model Turn stream ended before completion")),
                }
            } => result,
        };
        // The publication future may have made cancellation/deadline ready during
        // its final poll. Success is still provisional until this last decision.
        let result = if deadline.is_exceeded() {
            Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded"))
        } else if cancellation.is_cancelled() {
            Err(ModelTurnError::new("cancelled", "Model Turn cancelled"))
        } else {
            result
        };
        match &result {
            Ok(CanonicalEvent::Completed(_) | CanonicalEvent::Compacted(_)) => terminal.finish("completed"),
            Err(error) => terminal.finish(&error.code),
            Ok(CanonicalEvent::Delta(AiStreamDelta::StreamError { .. })) => terminal.finish("failed"),
            Ok(CanonicalEvent::Delta(AiStreamDelta::UnexpectedEof)) => terminal.finish("model_stream_incomplete"),
            Ok(CanonicalEvent::Delta(_)) => {}
        }
        Some((result, state))
        }
    }).fuse())
}

fn execute_inner(
    executor: LiveModelTurnExecutor,
    mut input: TurnInput,
    model_turn_id: String,
) -> impl std::future::Future<Output = Result<ModelTurn, ModelTurnError>> + Send {
    // 在构造边界装箱，避免把准备、重试和派发状态逐层嵌入外层 select 的栈帧。
    Box::pin(async move {
        let gateway = &executor.gateway;
        let route = gateway
            .model_cache
            .read()
            .await
            .resolve(&input.request.model)
            .cloned()
            .ok_or_else(|| ModelTurnError::new("model_not_found", "Model is unavailable"))?;

        // 与 generation_chain 的推理继承判定同口径：客户端给出任何推理指令
        // （level/effort/budget/display/enabled）都算「已指定」，Route 默认档不介入。
        let mut default_level_applied = false;
        if !input.request.reasoning.enabled
            && input.request.reasoning.level.is_none()
            && input.request.reasoning.effort.is_none()
            && input.request.reasoning.budget_tokens.is_none()
            && input.request.reasoning.display.is_none()
            && let Some(value) = route.default_thinking_level.as_deref()
        {
            match ThinkingLevel::from_wire(value) {
                Ok(level) => {
                    input.request.reasoning.level = Some(level);
                    default_level_applied = true;
                }
                Err(_) => {
                    tracing::warn!(
                        route = %route.model_id,
                        value,
                        "ignoring invalid Route default Thinking Level"
                    );
                }
            }
        }

        if let Some(requested) = input.request.reasoning.level {
            input.request.reasoning.level = match requested.clamp(&route.supported_thinking_levels)
            {
                Some(level) => Some(level),
                // 默认档是管理员偏好而非客户端要求：配置漂移导致支持集为空时
                // 退回未指定，不打断整条 Route 的流量。
                None if default_level_applied => None,
                None => {
                    return Err(ModelTurnError::new(
                        "thinking_level_unsupported",
                        "Route has no Supported Thinking Level for this request",
                    ));
                }
            };
        }

        if input.authorization == ModelTurnAuthorization::CapabilityGrant
            && stravia_media::contains_images(&input.request)
            && !stravia_protocol_codec::codec::open_responses::hosted_image_generation_requested(
                &input.request,
            )
            && !crate::media::model_is_image_capable(gateway, &route).await
        {
            return Err(ModelTurnError::new(
                "media_understanding_unavailable",
                "Media Understanding is unavailable",
            ));
        }

        let security = Security::new(gateway.storage.auth());
        let _access = match input.authorization {
            ModelTurnAuthorization::RouteBinding => {
                security
                    .authorize_principal_model(&input.principal, &route)
                    .await
            }
            ModelTurnAuthorization::CapabilityGrant => {
                security
                    .authorize_principal_capability(&input.principal)
                    .await
            }
        }
        .map_err(model_turn_gateway_error)?;

        let mut attempts = executor
            .selector
            .select(
                &input.principal,
                &route,
                &input.request,
                input.request.meta.media_routing.as_ref(),
                input.observer.as_ref(),
            )
            .await
            .map_err(|error| match error {
                crate::router::SelectionError::SchedulingEvidence(source) => ModelTurnError::new(
                    "route_scheduling_unavailable",
                    format!("Route scheduling snapshot is unavailable: {source}"),
                ),
                crate::router::SelectionError::MediaPlanExhausted => ModelTurnError::new(
                    "input_modality_unsupported",
                    "No eligible Target remains for the fixed Media routing plan",
                ),
                crate::router::SelectionError::NoEligibleTarget => {
                    ModelTurnError::new("model_unavailable", "Model has no configured Target")
                }
            })?;

        let native_compaction_requested = input.purpose == super::ModelTurnPurpose::Compact
            || stravia_protocol_codec::codec::compaction::native_compaction_requested(
                &input.request,
            );
        let mut last_error = None;
        while let Some(target) = attempts.next_healthy() {
            let mut transport_preference = TransportPreference::Automatic;
            loop {
                // The target may have been re-cooled by another request while this
                // one prepared or backed off; never send on a stale generation.
                if !attempts.retry_current() {
                    break;
                }
                let result = match prepare_attempt(
                    &executor,
                    &route,
                    &target,
                    &input,
                    &model_turn_id,
                    attempts.current_is_probe(),
                    transport_preference,
                )
                .await
                {
                    // Preparation awaits storage/protocol work; another request may
                    // have cooled the target meanwhile — recheck right before the
                    // upstream send, not only before the backoff sleep.
                    Ok(_) if !attempts.retry_current() => break,
                    Ok(mut prepared) => {
                        prepared.first_token_timed_out = (target.first_token_timeout_ms != 0
                            && input.observer.is_some())
                        .then(|| Arc::new(AtomicBool::new(false)));
                        // If the outer deadline/cancellation select drops this
                        // attempt before first token, count a real deadline only
                        // after Vendor transport has started. Local preparation
                        // and user cancellation do not consume the failure budget.
                        let upstream_state = prepared.upstream_state.clone();
                        let deadline_guard = AttemptDeadlineGuard::armed(
                            &attempts,
                            &target,
                            input.deadline.clone(),
                            upstream_state,
                        );
                        begin_attempt(
                            gateway,
                            &route,
                            &target,
                            &input,
                            prepared,
                            AttemptRoutePolicy {
                                state: attempts.state().clone(),
                                context: attempts.context().clone(),
                                epoch: attempts.current_epoch(),
                                probe: attempts.current_is_probe(),
                            },
                            deadline_guard,
                        )
                        .await
                    }
                    Err(failure) => Err(failure),
                };
                let mut failure = match result {
                    Ok(turn) => {
                        attempts.accept_current();
                        return Ok(turn);
                    }
                    Err(failure) => failure,
                };
                if native_compaction_requested {
                    if failure.error.code == "vendor_operation_unsupported" {
                        failure = AttemptFailure::terminal(
                            "compaction_unsupported",
                            "Selected Target does not support native compaction",
                        );
                    }
                    record_upstream_failure(&attempts, &target, &failure);
                    return Err(failure.finish(input.observer.as_ref()));
                }
                // Local preparation/credential/storage failures may move this request
                // to another Target, but they are not evidence that the upstream
                // Target failed and must not consume its shared failure budget.
                if !failure.is_upstream() {
                    if failure.try_next_target {
                        attempts.skip_current();
                        last_error = Some(failure);
                        break;
                    }
                    return Err(failure.finish(input.observer.as_ref()));
                }
                let Some(kind) = failure.error.upstream_error_kind.clone() else {
                    record_upstream_failure(&attempts, &target, &failure);
                    return Err(failure.finish(input.observer.as_ref()));
                };
                // 错误类别可能合并不同 HTTP 状态；共享计数不能扩大原有同目标重试范围。
                if kind.is_retryable()
                    && failure.transport_failure.is_none()
                    && failure
                        .diagnostic
                        .status_code
                        .is_some_and(|status| !matches!(status, 408 | 429 | 500 | 502 | 503 | 529))
                {
                    record_upstream_failure(&attempts, &target, &failure);
                    attempts.skip_current();
                    last_error = Some(failure);
                    break;
                }
                let retry_over_http = matches!(
                    failure.transport_failure.as_ref(),
                    Some(TransportFailure::Websocket)
                );
                match attempts.record_failure(
                    &target,
                    crate::router::selector::AttemptFailureSignal {
                        kind,
                        client_output_committed: false,
                        retry_after: failure.retry_after,
                        now_ms: gateway.route_policy_state.now_ms(),
                        jitter_sample: rand::random(),
                    },
                ) {
                    AttemptFailureDisposition::RetrySame { delay } => {
                        if retry_over_http {
                            transport_preference = TransportPreference::HttpOnly;
                        }
                        tokio::time::sleep(delay).await;
                    }
                    AttemptFailureDisposition::TryNextTarget => {
                        last_error = Some(failure);
                        break;
                    }
                    AttemptFailureDisposition::Stop => {
                        return Err(failure.finish(input.observer.as_ref()));
                    }
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| {
                AttemptFailure::terminal("provider_unavailable", "all Model Targets failed")
            })
            .finish(input.observer.as_ref()))
    })
}

const UPSTREAM_NOT_STARTED: u8 = 0;
const UPSTREAM_STARTED: u8 = 1;
const UPSTREAM_FINISHED: u8 = 2;
const UPSTREAM_LOCAL_WORK: u8 = 4;
const UPSTREAM_LOCAL_DEADLINE: u8 = 8;
const UPSTREAM_FAILURE_MASK: u8 =
    UPSTREAM_STARTED | UPSTREAM_FINISHED | UPSTREAM_LOCAL_WORK | UPSTREAM_LOCAL_DEADLINE;

struct UpstreamLocalWork<'a> {
    state: &'a AtomicU8,
    deadline: Deadline,
}

impl<'a> UpstreamLocalWork<'a> {
    fn begin(state: &'a AtomicU8, deadline: Deadline) -> Self {
        state.fetch_or(UPSTREAM_LOCAL_WORK, Ordering::AcqRel);
        Self { state, deadline }
    }
}

impl Drop for UpstreamLocalWork<'_> {
    fn drop(&mut self) {
        if self.deadline.is_exceeded() {
            self.state
                .fetch_or(UPSTREAM_LOCAL_DEADLINE, Ordering::AcqRel);
        }
        self.state.fetch_and(!UPSTREAM_LOCAL_WORK, Ordering::AcqRel);
    }
}

struct PreparedAttempt {
    model_turn_id: String,
    route: RouteContext,
    provider_name: String,
    compact: bool,
    preserve_upstream_error: bool,
    observer: Option<crate::interaction_observation::RunObserver>,
    request: AiRequest,
    continuation_fallback: Option<AiRequest>,
    dispatch_model: String,
    actual_model: String,
    namespace: String,
    protocol_hint: String,
    egress_base_url: String,
    metadata: BTreeMap<String, serde_json::Value>,
    client_headers: Vec<(String, String)>,
    websocket_affinity: Option<String>,
    execution: Option<PreparedVendorExecution>,
    pinned_execution: PreparedVendorExecution,
    response_continuation_available: Arc<AtomicBool>,
    first_token_timed_out: Option<Arc<AtomicBool>>,
    upstream_state: Arc<AtomicU8>,
    allow_recovery: bool,
    can_refresh_auth: bool,
}

struct AttemptFailure {
    error: Box<ModelTurnError>,
    diagnostic: Box<crate::interaction_observation::FailureDiagnostic>,
    // Local Target preparation failures may reroute, but must not masquerade as
    // a canonical upstream error merely to drive that routing decision.
    try_next_target: bool,
    retry_after: Option<Duration>,
    transport_failure: Option<TransportFailure>,
}

impl AttemptFailure {
    fn upstream_origin(mut self) -> Self {
        self.diagnostic.source = Some("upstream".into());
        self
    }

    /// `true` when the failure came from the upstream provider rather than
    /// local preparation or validation.
    fn is_upstream(&self) -> bool {
        self.diagnostic.source.as_deref() == Some("upstream")
    }
    fn reroutable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: Box::new(ModelTurnError::new(code, message)),
            diagnostic: Box::new(crate::interaction_observation::FailureDiagnostic {
                source: Some("platform".into()),
                ..Default::default()
            }),
            try_next_target: true,
            retry_after: None,
            transport_failure: None,
        }
    }

    fn upstream(
        kind: stravia_runtime_contract::protocol::ir::AiErrorKind,
        status: Option<u16>,
        code: impl Into<String>,
        message: impl Into<String>,
        retry_after: Option<Duration>,
    ) -> Self {
        let mut error = ModelTurnError::new(code, message);
        error.upstream_error_kind = Some(kind);
        Self {
            error: Box::new(error),
            diagnostic: Box::new(crate::interaction_observation::FailureDiagnostic {
                source: Some("upstream".into()),
                status_code: status,
                ..Default::default()
            }),
            try_next_target: false,
            retry_after,
            transport_failure: None,
        }
    }

    fn with_transport_failure(mut self, transport_failure: Option<TransportFailure>) -> Self {
        self.transport_failure = transport_failure;
        self
    }

    fn with_diagnostic_message(mut self, message: Option<String>) -> Self {
        if let Some(message) = message {
            self.diagnostic.message = Some(message);
        }
        self
    }

    fn with_status(mut self, status: Option<u16>) -> Self {
        self.diagnostic.status_code = status;
        self.error.upstream_status = status.filter(|status| *status >= 400);
        self
    }

    fn terminal(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: Box::new(ModelTurnError::new(code, message)),
            diagnostic: Box::new(crate::interaction_observation::FailureDiagnostic {
                source: Some("platform".into()),
                ..Default::default()
            }),
            try_next_target: false,
            retry_after: None,
            transport_failure: None,
        }
    }

    fn finish(
        mut self,
        observer: Option<&crate::interaction_observation::RunObserver>,
    ) -> ModelTurnError {
        if self.error.code != "cancelled" {
            self.diagnostic.code = Some(self.error.code.clone());
            if self.diagnostic.message.is_none() {
                self.diagnostic.message = Some(self.error.message.clone());
            }
            if let Some(observer) = observer {
                observer.record_failure(*self.diagnostic);
            }
        }
        *self.error
    }
}

async fn prepare_attempt(
    executor: &LiveModelTurnExecutor,
    route: &crate::db::models::Route,
    target: &SelectedTarget,
    input: &TurnInput,
    model_turn_id: &str,
    probe: bool,
    transport_preference: TransportPreference,
) -> Result<PreparedAttempt, AttemptFailure> {
    let gateway = &executor.gateway;
    let omit_protected_thinking = false;
    let target_key = selected_target_key(target);
    let actual_model = match target.model.as_deref().map(str::trim) {
        Some("*") => route.model_id.clone(),
        Some(model) if !model.is_empty() => model.to_owned(),
        _ => {
            return Err(AttemptFailure::reroutable(
                "provider_model_unavailable",
                "Inference Target does not select an upstream model",
            ));
        }
    };
    let compact = input.purpose == super::ModelTurnPurpose::Compact;
    let response_continuation_available = Arc::new(AtomicBool::new(false));
    let mut preparation_context =
        VendorCallContext::new(input.cancellation.clone(), input.deadline.clone());
    preparation_context.observer = input.observer.clone();
    preparation_context.metadata.insert(
        TRANSPORT_PREFERENCE_METADATA_KEY.into(),
        serde_json::Value::String(transport_preference.as_metadata_value().into()),
    );
    preparation_context.client_headers = header_pairs(&input.extra_headers);
    preparation_context.response_continuation_available = response_continuation_available.clone();
    let mut execution = gateway
        .prepare_vendor_execution(
            &target.provider_id,
            Some(&actual_model),
            if compact {
                stravia_vendor_sdk::Operation::Compact
            } else {
                stravia_vendor_sdk::Operation::Infer
            },
            &preparation_context,
        )
        .await
        .map_err(|_| {
            if input.cancellation.is_cancelled() || input.deadline.is_exceeded() {
                AttemptFailure::terminal(
                    interruption_error(&input.deadline).code,
                    interruption_error(&input.deadline).message,
                )
            } else {
                AttemptFailure::reroutable(
                    "provider_unavailable",
                    "Vendor execution could not be prepared",
                )
            }
        })?;
    let supplier_id = execution.descriptor().provider_id.clone();
    let provider = gateway
        .storage
        .providers()
        .get(&target.provider_id)
        .await
        .map_err(|_| {
            AttemptFailure::reroutable(
                "provider_unavailable",
                "Provider connection could not be read",
            )
        })?
        .ok_or_else(|| {
            AttemptFailure::reroutable(
                "provider_unavailable",
                format!("provider unavailable: {}", target.provider_id),
            )
        })?;
    let provider_model = execution.provider().model_metadata.as_ref();
    if !compact
        && input.request.embedding.is_none()
        && !stravia_protocol_codec::codec::open_responses::hosted_image_generation_requested(
            &input.request,
        )
        && provider_model.is_some_and(vendor_metadata_declares_only_image_operation)
    {
        return Err(AttemptFailure::reroutable(
            "model_unavailable",
            "selected Provider Model supports image generation but not chat inference",
        ));
    }
    let supports_tools = provider_model.is_some_and(|model| {
        model
            .capabilities
            .iter()
            .any(|capability| capability == "tools")
    });
    if input.purpose != super::ModelTurnPurpose::Compact
        && input
            .request
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        && !supports_tools
    {
        return Err(AttemptFailure::reroutable(
            if stravia_web_search::native_web_search_requested(&input.request) {
                "web_search_unsupported"
            } else {
                "tools_unsupported"
            },
            "selected provider model does not support function tools",
        ));
    }
    if request_contains_video(&input.request)
        && !provider_model.is_some_and(|model| vendor_metadata_supports_modality(model, "video"))
    {
        return Err(AttemptFailure::reroutable(
            "input_modality_unsupported",
            "selected provider model does not support native video input",
        ));
    }
    if input
        .request
        .meta
        .media_routing
        .as_ref()
        .is_some_and(|plan| plan.mode == MediaRoutingMode::Native)
        && !provider_model.is_some_and(|model| {
            model
                .capabilities
                .iter()
                .any(|capability| capability == "image_input")
        })
    {
        return Err(AttemptFailure::reroutable(
            "input_modality_unsupported",
            "selected provider model does not support native image input",
        ));
    }

    gateway
        .select_vendor_protocol(&mut execution, &input.request, &preparation_context)
        .await
        .map_err(classify_vendor_error)?;
    let protocol_hint = execution.protocol().trim().to_owned();
    let provider_snapshot = execution.provider().clone();
    let provider_model = provider_snapshot.model_metadata.as_ref();
    let ingress = input.request.meta.source_protocol;
    let egress =
        stravia_protocol_codec::registry::ProtocolRegistry::global().resolve_alias(&protocol_hint);
    let protocol_identity = egress
        .map(Into::into)
        .or_else(|| (!protocol_hint.is_empty()).then(|| protocol_hint.clone().into()));
    let egress_base_url = provider_snapshot.base_url.trim().to_owned();
    let can_refresh_auth = execution
        .descriptor()
        .channels
        .iter()
        .find(|channel| channel.id == provider_snapshot.channel)
        .is_some_and(|channel| channel.capabilities.contains(&Capability::AuthOauth));
    let target_namespace = target_namespace(
        &target.provider_id,
        &supplier_id,
        &provider_snapshot,
        execution.oauth_connection_id(),
        &target_key,
        &actual_model,
        execution.use_proxy(),
    );
    let mut provider_request = input.request.clone();
    let thinking_source = crate::history_marker::ThinkingSource {
        namespace: target_namespace.clone(),
        protocol: protocol_identity.clone(),
        actual_model: actual_model.clone(),
        target_id: target_key.clone(),
    };
    let thinking_replayed = if let Some(egress) = egress {
        stravia_protocol_codec::transform::prepare_thinking_replay(
            &mut provider_request,
            egress,
            |item| {
                thinking_replay_source_is_compatible(
                    item,
                    ingress,
                    &thinking_source,
                    omit_protected_thinking,
                )
            },
        )
    } else {
        prepare_canonical_thinking_replay(
            &mut provider_request,
            &thinking_source,
            omit_protected_thinking,
        )
    };
    let native_compaction_requested =
        stravia_protocol_codec::codec::compaction::native_compaction_requested(&provider_request);
    let binding = crate::compaction::CompactionTarget {
        target_key: target_key.clone(),
        namespace: target_namespace.clone(),
        model: actual_model.clone(),
        protocol: protocol_identity
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
    };
    if let Some(resolved) = gateway
        .compaction
        .resolve(&input.principal, &input.request.items)
        .await
        .map_err(|error| AttemptFailure::terminal(error.code(), error.to_string()))?
        && resolved.target != binding
    {
        return Err(AttemptFailure::reroutable(
            "compaction_target_mismatch",
            "Native compaction state is not compatible with this Target binding",
        ));
    }
    if let Some(level) = provider_request.reasoning.level {
        let Some(control) = crate::thinking::mapping_control(&target.thinking_level_map, level)
        else {
            return Err(AttemptFailure::reroutable(
                "protocol_lossy_rejected",
                format!(
                    "Target has no mapping for Thinking Level {}",
                    level.as_str()
                ),
            ));
        };
        if control.is_hidden() {
            return Err(AttemptFailure::reroutable(
                "protocol_lossy_rejected",
                format!("Target hides Thinking Level {}", level.as_str()),
            ));
        }
        let canonical_model_metadata = provider_model
            .and_then(|metadata| {
                serde_json::from_value::<crate::provider_models::ProviderModelMetadata>(
                    serde_json::Value::Object(metadata.extensions.clone().into_iter().collect()),
                )
                .ok()
            })
            .unwrap_or_default();
        let toggle_declared = execution
            .descriptor()
            .channels
            .iter()
            .find(|channel| channel.id == provider_snapshot.channel)
            .is_some_and(|channel| {
                channel
                    .model_capabilities
                    .contains(stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE)
            })
            || provider_model.is_some_and(|metadata| {
                vendor_metadata_declares_capability(
                    metadata,
                    stravia_vendor_sdk::MODEL_CAPABILITY_THINKING_TOGGLE,
                )
            });
        if !crate::thinking::control_is_writable(
            &protocol_hint,
            &canonical_model_metadata,
            toggle_declared,
            control,
        ) {
            return Err(AttemptFailure::reroutable(
                "protocol_lossy_rejected",
                format!(
                    "Target cannot write Thinking Level {} control {}",
                    level.as_str(),
                    control.kind()
                ),
            ));
        }
        provider_request.reasoning.target_control = Some(control.clone());
    } else {
        provider_request.reasoning.target_control = None;
    }
    let mut full_provider_request = provider_request.clone();
    crate::router::clear_previous_response_id(&mut full_provider_request);
    // 准备期间保留逻辑模型；仅在交给 guest 的请求副本上改写派发模型。
    input
        .request
        .meta
        .redaction
        .observe_provider_request(&full_provider_request)
        .map_err(|_| {
            AttemptFailure::terminal(
                "reversible_redaction_failed",
                "Provider request redaction validation failed",
            )
        })?;
    let require_affinity = request_requires_affinity(&provider_request);
    let continued_id = if compact || thinking_replayed {
        crate::router::clear_previous_response_id(&mut provider_request);
        None
    } else {
        executor
            .continuation
            .prepare(
                &input.principal,
                ContinuationTarget {
                    namespace: &target_namespace,
                    protocol: egress,
                    actual_model: &actual_model,
                    allow_ephemeral_response: input.allow_responses_websocket
                        && transport_preference != TransportPreference::HttpOnly
                        && require_affinity,
                },
                &mut provider_request,
            )
            .await
    };

    let session_affinity = crate::generation_chain::generation_session_fingerprint(&input.request);
    let websocket_affinity = namespace_fingerprint(&(
        input.principal.continuation_key(),
        target_key.as_str(),
        session_affinity.as_deref(),
    ));
    let mut metadata = BTreeMap::new();
    metadata.insert(
        TRANSPORT_PREFERENCE_METADATA_KEY.into(),
        serde_json::Value::String(transport_preference.as_metadata_value().into()),
    );
    if let Some(ingress) = ingress {
        metadata.insert(
            "ingress_protocol".into(),
            serde_json::Value::String(ingress.to_string()),
        );
    }
    if !protocol_hint.is_empty() {
        metadata.insert(
            "egress_protocol".into(),
            serde_json::Value::String(protocol_hint.to_owned()),
        );
    }
    if !egress_base_url.is_empty() {
        metadata.insert(
            "egress_base_url".into(),
            serde_json::Value::String(egress_base_url.clone()),
        );
    }

    Ok(PreparedAttempt {
        model_turn_id: model_turn_id.to_owned(),
        route: RouteContext {
            model_id: route.id.clone(),
            provider_id: target.provider_id.clone(),
            target_id: target_key,
            egress,
        },
        provider_name: provider.name,
        compact,
        preserve_upstream_error: compact || native_compaction_requested,
        observer: input.observer.clone(),
        request: provider_request,
        continuation_fallback: continued_id.map(|_| full_provider_request),
        dispatch_model: route.model_id.clone(),
        actual_model,
        namespace: target_namespace,
        protocol_hint: protocol_hint.to_owned(),
        egress_base_url,
        metadata,
        client_headers: header_pairs(&input.extra_headers),
        websocket_affinity: (input.allow_responses_websocket
            && transport_preference != TransportPreference::HttpOnly)
            .then_some(websocket_affinity),
        execution: Some(execution.clone()),
        pinned_execution: execution,
        response_continuation_available,
        first_token_timed_out: None,
        upstream_state: Arc::new(AtomicU8::new(UPSTREAM_NOT_STARTED)),
        allow_recovery: !compact && !native_compaction_requested && !probe,
        can_refresh_auth,
    })
}

#[derive(Clone)]
struct AttemptRoutePolicy {
    state: RoutePolicyState,
    context: RouteAttemptContext,
    /// Runtime generation of the selected target; guards the success write so
    /// a stale in-flight attempt can never clear a newer cooldown.
    epoch: u64,
    /// `true` while this attempt holds the target's half-open probe slot.
    probe: bool,
}

impl AttemptRoutePolicy {
    fn record_success(&self, target: &SelectedTarget) {
        self.state
            .record_success(&self.context, &selected_target_key(target), self.epoch);
    }

    fn record_failure(&self, target: &SelectedTarget) {
        self.state.record_failure(
            &selected_target_key(target),
            self.epoch,
            target.target_retry_budget,
            target.target_cooldown_ms,
        );
    }
}

/// Drop guard for an in-flight Provider attempt. It starts in `begin_attempt`
/// and transfers to the live driver after First Token, so either side can be
/// dropped by the outer deadline. Only an unresolved upstream operation at the
/// real deadline is a Target failure; local work and user cancellation are not.
struct AttemptDeadlineGuard {
    state: RoutePolicyState,
    target_key: String,
    epoch: u64,
    retry_budget: i32,
    cooldown_ms: i64,
    deadline: Deadline,
    upstream_state: Arc<AtomicU8>,
    armed: bool,
}

impl AttemptDeadlineGuard {
    fn armed(
        attempts: &RouteAttemptPolicy,
        target: &SelectedTarget,
        deadline: Deadline,
        upstream_state: Arc<AtomicU8>,
    ) -> Self {
        Self {
            state: attempts.state().clone(),
            target_key: selected_target_key(target),
            epoch: attempts.current_epoch(),
            retry_budget: target.target_retry_budget,
            cooldown_ms: target.target_cooldown_ms,
            deadline,
            upstream_state,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AttemptDeadlineGuard {
    fn drop(&mut self) {
        if self.armed
            && self.deadline.is_exceeded()
            && self.upstream_state.load(Ordering::Acquire) & UPSTREAM_FAILURE_MASK
                == UPSTREAM_STARTED
        {
            self.state.record_failure(
                &self.target_key,
                self.epoch,
                self.retry_budget,
                self.cooldown_ms,
            );
        }
    }
}

struct VendorDriverReady {
    streamed: bool,
}

struct VendorDriverHandle {
    join: Option<tokio::task::JoinHandle<()>>,
    cancellation: stravia_runtime_contract::CancellationToken,
    publication_completed: Arc<AtomicBool>,
    deadline_guard: Option<AttemptDeadlineGuard>,
}

impl VendorDriverHandle {
    fn resolve_failure(&mut self, error: &ModelTurnError) {
        if let Some(mut guard) = self.deadline_guard.take()
            && error.code != "deadline_exceeded"
        {
            guard.disarm();
        }
    }

    async fn wait(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for VendorDriverHandle {
    fn drop(&mut self) {
        // A consumed terminal still leaves the driver a short success tail: route
        // accounting and the attempt terminal observation. Detach that task so it
        // can finish, and preserve its publication fence for final history commit.
        // An incomplete consumer instead owns cancellation and must abort promptly.
        if self.publication_completed.load(Ordering::Acquire) {
            if let Some(mut guard) = self.deadline_guard.take() {
                guard.disarm();
            }
            return;
        }
        self.cancellation.cancel();
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

struct VendorPublishedResult {
    result: Result<CanonicalEvent, ModelTurnError>,
    publication: VendorPublicationFence,
}

const VENDOR_OUTPUT_BUFFER_SIZE: usize = 32;

struct VendorOutputStream {
    inner: Pin<Box<dyn Stream<Item = Result<CanonicalEvent, ModelTurnError>> + Send>>,
}

impl VendorOutputStream {
    fn new(
        receiver: tokio::sync::mpsc::Receiver<VendorPublishedResult>,
        driver: VendorDriverHandle,
        publication: VendorPublication,
    ) -> Self {
        let publication_completed = driver.publication_completed.clone();
        let inner = stream::unfold(
            (
                receiver,
                publication,
                publication_completed,
                Some(driver),
                false,
            ),
            |(mut receiver, publication, publication_completed, mut driver, finished)| async move {
                if finished {
                    return None;
                }
                let published = receiver.recv().await?;
                let fence = if published.result.is_err() {
                    published.publication.terminal_write_fence().await
                } else {
                    published.publication.write_fence().await
                };
                let result = match fence {
                    Ok(guard) => {
                        if published.result.is_ok() {
                            publication.publish(published.publication);
                        }
                        drop(guard);
                        published.result
                    }
                    Err(_) => Err(ModelTurnError::new(
                        "cancelled",
                        "Vendor result can no longer be published",
                    )),
                };
                if let Err(error) = &result
                    && let Some(driver) = driver.as_mut()
                {
                    driver.resolve_failure(error);
                }
                let terminal = matches!(
                    &result,
                    Ok(CanonicalEvent::Completed(_) | CanonicalEvent::Compacted(_))
                );
                if terminal {
                    publication_completed.store(true, Ordering::Release);
                    // The driver records route success and the attempt terminal after
                    // queueing this event. Join that bounded tail before exposing the
                    // semantic terminal, so Run finalization cannot discard it.
                    if let Some(driver) = driver.take() {
                        driver.wait().await;
                    }
                }
                let finished = result
                    .as_ref()
                    .is_err_and(|error| error.code == "cancelled");
                Some((
                    result,
                    (
                        receiver,
                        publication,
                        publication_completed,
                        driver,
                        finished,
                    ),
                ))
            },
        );
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl Stream for VendorOutputStream {
    type Item = Result<CanonicalEvent, ModelTurnError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

async fn begin_attempt(
    gateway: &Gateway,
    route: &crate::db::models::Route,
    target: &SelectedTarget,
    input: &TurnInput,
    prepared: PreparedAttempt,
    policy: AttemptRoutePolicy,
    mut deadline_guard: AttemptDeadlineGuard,
) -> Result<ModelTurn, AttemptFailure> {
    let first_token_timeout_ms = target.first_token_timeout_ms;
    let model_turn_id = prepared.model_turn_id.clone();
    let route_context = prepared.route.clone();
    let publication = VendorPublication::default();
    let target_identity = TargetIdentity {
        actual_model: prepared.actual_model.clone(),
        provider_id: prepared.route.provider_id.clone(),
        target_id: prepared.route.target_id.clone(),
        namespace: prepared.namespace.clone(),
        protocol_hint: prepared
            .route
            .egress
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| prepared.protocol_hint.clone()),
        response_continuation_available: prepared.response_continuation_available.clone(),
        publication: Some(publication.clone()),
    };
    let timeout_signal = prepared.first_token_timed_out.clone();
    let upstream_state = prepared.upstream_state.clone();
    let (output_tx, output_rx) = tokio::sync::mpsc::channel(VENDOR_OUTPUT_BUFFER_SIZE + 1);
    // 普通内容保持有界背压；预留的终态槽让到期驱动无需等待慢消费者即可结束。
    let terminal_output = output_tx
        .clone()
        .try_reserve_owned()
        .expect("new vendor output queue has terminal capacity");
    let (ready_tx, mut ready_rx) = tokio::sync::oneshot::channel();
    let operation_cancellation = stravia_runtime_contract::CancellationToken::new();
    let driver_cancellation = operation_cancellation.clone();
    let driver_gateway = gateway.clone();
    let driver_route_id = route.id.clone();
    let driver_target = target.clone();
    let principal = input.principal.clone();
    let canonical_request = input.request.clone();
    let parent_cancellation = input.cancellation.clone();
    let deadline = input.deadline.clone();
    let observer = input.observer.clone();
    let join = tokio::spawn(async move {
        drive_vendor_attempt(
            driver_gateway,
            driver_route_id,
            driver_target,
            principal,
            canonical_request,
            parent_cancellation,
            operation_cancellation,
            deadline,
            observer,
            prepared,
            policy,
            output_tx,
            terminal_output,
            ready_tx,
        )
        .await;
    });
    let mut driver = VendorDriverHandle {
        join: Some(join),
        cancellation: driver_cancellation,
        publication_completed: Arc::new(AtomicBool::new(false)),
        deadline_guard: None,
    };

    let ready_result = if first_token_timeout_ms == 0 {
        ready_rx.await
    } else {
        tokio::select! {
            result = &mut ready_rx => result,
            _ = tokio::time::sleep(Duration::from_millis(first_token_timeout_ms as u64)) => {
                if let Some(signal) = timeout_signal {
                    signal.store(true, Ordering::Release);
                }
                let upstream_timed_out = upstream_state.load(Ordering::Acquire)
                    & UPSTREAM_FAILURE_MASK
                    == UPSTREAM_STARTED;
                driver.cancellation.cancel();
                drop(output_rx);
                driver.wait().await;
                deadline_guard.disarm();
                return Err(if upstream_timed_out {
                    AttemptFailure::upstream(
                        stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout,
                        None,
                        "first_token_timeout",
                        "Target did not produce a First Token before its timeout",
                        None,
                    )
                } else {
                    AttemptFailure::terminal(
                        "first_token_timeout",
                        "Local Provider preparation exceeded the First Token timeout",
                    )
                });
            }
        }
    };

    match ready_result {
        Ok(Ok(ready)) => {
            driver.deadline_guard = Some(deadline_guard);
            Ok(ModelTurn {
                model_turn_id,
                route: route_context,
                target: target_identity,
                output: Box::pin(VendorOutputStream::new(output_rx, driver, publication)),
                streamed: ready.streamed,
            })
        }
        Ok(Err(failure)) => {
            deadline_guard.disarm();
            drop(output_rx);
            driver.wait().await;
            Err(failure)
        }
        Err(_) => {
            deadline_guard.disarm();
            drop(output_rx);
            driver.wait().await;
            Err(AttemptFailure::terminal(
                "vendor_runtime_failed",
                "Vendor operation ended before publishing a result",
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_vendor_attempt(
    gateway: Gateway,
    route_id: String,
    target: SelectedTarget,
    principal: stravia_runtime_contract::Principal,
    canonical_request: AiRequest,
    parent_cancellation: stravia_runtime_contract::CancellationToken,
    operation_cancellation: stravia_runtime_contract::CancellationToken,
    deadline: Deadline,
    failure_observer: Option<crate::interaction_observation::RunObserver>,
    mut prepared: PreparedAttempt,
    policy: AttemptRoutePolicy,
    output: tokio::sync::mpsc::Sender<VendorPublishedResult>,
    terminal_output: tokio::sync::mpsc::OwnedPermit<VendorPublishedResult>,
    ready: tokio::sync::oneshot::Sender<Result<VendorDriverReady, AttemptFailure>>,
) {
    let mut ready = Some(ready);
    let mut committed = false;
    let mut streamed = false;
    let mut last_publication = None;
    let mut reservation: Option<RouteAttemptReservation> = None;
    let mut continuation_fallback = prepared.continuation_fallback.take();
    let mut request = prepared.request.clone();
    let mut auth_recovered = false;
    let mut protected_reasoning_recovered = false;

    loop {
        let attempt = AttemptObservation::new(
            failure_observer.clone(),
            prepared.model_turn_id.clone(),
            prepared.route.target_id.clone(),
            prepared.route.provider_id.clone(),
            prepared.provider_name.clone(),
            prepared.actual_model.clone(),
            prepared.protocol_hint.clone(),
            prepared.egress_base_url.clone(),
            prepared.first_token_timed_out.clone(),
        );
        let mut first_token_ms = None;
        let outcome = run_vendor_operation(
            &gateway,
            &principal,
            &parent_cancellation,
            &operation_cancellation,
            deadline.clone(),
            &mut prepared,
            &policy,
            &target,
            &mut reservation,
            &request,
            &attempt,
            &output,
            &mut ready,
            &mut committed,
            &mut streamed,
            &mut last_publication,
            &mut first_token_ms,
        )
        .await;

        match outcome {
            Ok(VendorTerminal::Infer(mut response, publication)) => {
                let normalization = tokio::select! {
                    biased;
                    _ = publication.cancelled() => {
                        finish_vendor_failure(
                            AttemptFailure::terminal(
                                "cancelled",
                                "Vendor result can no longer be published",
                            ),
                            committed,
                            &attempt,
                            terminal_output,
                            last_publication.as_ref(),
                            &mut ready,
                            failure_observer.as_ref(),
                            &policy,
                            &target,
                        )
                        .await;
                        return;
                    }
                    result = crate::media::ingest::normalize_response(
                        &gateway,
                        &principal,
                        &mut response,
                        &operation_cancellation,
                    ) => result,
                };
                if let Err(error) = normalization {
                    let failure =
                        AttemptFailure::terminal("output_media_ingest_failed", error.to_string());
                    finish_vendor_failure(
                        failure,
                        committed,
                        &attempt,
                        terminal_output,
                        last_publication.as_ref(),
                        &mut ready,
                        failure_observer.as_ref(),
                        &policy,
                        &target,
                    )
                    .await;
                    return;
                }
                if !committed && reservation.is_none() {
                    reservation = Some(policy.state.reservation(
                        policy.context.clone(),
                        selected_target_key(&target),
                        policy.epoch,
                        policy.probe,
                    ));
                }
                let usage = response.usage.clone();
                attempt.confirm_usage(&usage);
                let first_commit = !committed;
                let first_token_ms = *first_token_ms.get_or_insert_with(|| attempt.elapsed_ms());
                let published = send_vendor_output(
                    &output,
                    &publication,
                    &parent_cancellation,
                    &operation_cancellation,
                    &deadline,
                    Ok(CanonicalEvent::Completed(response)),
                )
                .await;
                match published {
                    Ok(()) => {
                        gateway.cache_affinity.record_success(
                            &principal,
                            &route_id,
                            &canonical_request,
                            &prepared.route.target_id,
                            &usage,
                        );
                        policy.record_success(&target);
                        if let Some(reservation) = reservation.take() {
                            reservation.complete();
                        }

                        if first_commit && let Some(ready) = ready.take() {
                            let _ = ready.send(Ok(VendorDriverReady { streamed }));
                        }
                        attempt.finish("completed", None, None, Some(first_token_ms));
                    }
                    Err(error) => {
                        attempt.finish("interrupted", None, Some(error.code.clone()), None);
                        if first_commit && let Some(ready) = ready.take() {
                            let _ = ready
                                .send(Err(AttemptFailure::terminal(error.code, error.message)));
                        }
                    }
                }
                return;
            }
            Ok(VendorTerminal::Compact(response, publication)) => {
                if !committed && reservation.is_none() {
                    reservation = Some(policy.state.reservation(
                        policy.context.clone(),
                        selected_target_key(&target),
                        policy.epoch,
                        policy.probe,
                    ));
                }
                if let Some(usage) = &response.usage {
                    attempt.confirm_usage(usage);
                }
                let first_commit = !committed;
                let first_token_ms = *first_token_ms.get_or_insert_with(|| attempt.elapsed_ms());
                let published = send_vendor_output(
                    &output,
                    &publication,
                    &parent_cancellation,
                    &operation_cancellation,
                    &deadline,
                    Ok(CanonicalEvent::Compacted(Box::new(response))),
                )
                .await;
                match published {
                    Ok(()) => {
                        policy.record_success(&target);
                        if let Some(reservation) = reservation.take() {
                            reservation.complete();
                        }

                        if first_commit && let Some(ready) = ready.take() {
                            let _ = ready.send(Ok(VendorDriverReady { streamed }));
                        }
                        attempt.finish("completed", None, None, Some(first_token_ms));
                    }
                    Err(error) => {
                        attempt.finish("interrupted", None, Some(error.code.clone()), None);
                        if first_commit && let Some(ready) = ready.take() {
                            let _ = ready
                                .send(Err(AttemptFailure::terminal(error.code, error.message)));
                        }
                    }
                }
                return;
            }
            Err(failure)
                if !committed
                    && failure.error.code == "protected_reasoning_rejected"
                    && prepared.allow_recovery
                    && !protected_reasoning_recovered =>
            {
                let mut replay = continuation_fallback
                    .take()
                    .unwrap_or_else(|| request.clone());
                crate::router::clear_previous_response_id(&mut replay);
                let stripped = stravia_protocol_codec::registry::ProtocolRegistry::global()
                    .resolve_alias(&prepared.protocol_hint)
                    .is_some_and(|egress| {
                        stravia_protocol_codec::transform::prepare_thinking_replay(
                            &mut replay,
                            egress,
                            |_| false,
                        )
                    });
                if stripped
                    && policy.state.try_record_recovery_failure(
                        &selected_target_key(&target),
                        policy.epoch,
                        target.target_retry_budget,
                        target.target_cooldown_ms,
                    )
                {
                    attempt.finish(
                        "failed",
                        failure.diagnostic.status_code,
                        Some("protected_reasoning_rejected".into()),
                        None,
                    );
                    request = replay;
                    protected_reasoning_recovered = true;
                    continue;
                }
                finish_vendor_failure(
                    failure,
                    false,
                    &attempt,
                    terminal_output,
                    last_publication.as_ref(),
                    &mut ready,
                    failure_observer.as_ref(),
                    &policy,
                    &target,
                )
                .await;
                return;
            }
            Err(failure)
                if !committed
                    && failure.error.code == "provider_auth_error"
                    && prepared.allow_recovery
                    && prepared.can_refresh_auth
                    && !auth_recovered
                    && policy.state.try_record_recovery_failure(
                        &selected_target_key(&target),
                        policy.epoch,
                        target.target_retry_budget,
                        target.target_cooldown_ms,
                    ) =>
            {
                attempt.finish(
                    "failed",
                    failure.diagnostic.status_code,
                    Some("provider_auth_error".into()),
                    None,
                );
                let admin = gateway.admin();
                let refresh = admin.recover_provider_auth_with_lease(
                    &prepared.route.provider_id,
                    &prepared.pinned_execution,
                    operation_cancellation.clone(),
                    deadline.clone(),
                );
                let refreshed = tokio::select! {
                    biased;
                    _ = parent_cancellation.cancelled() => {
                        operation_cancellation.cancel();
                        Err(())
                    }
                    () = deadline.wait() => {
                        operation_cancellation.cancel();
                        Err(())
                    }
                    result = refresh => result.map_err(|_| ()),
                };
                if refreshed.is_ok() {
                    auth_recovered = true;
                    continue;
                }
                finish_vendor_failure(
                    AttemptFailure::terminal(
                        "provider_auth_error",
                        "Vendor authentication recovery failed",
                    )
                    .upstream_origin(),
                    false,
                    &attempt,
                    terminal_output,
                    last_publication.as_ref(),
                    &mut ready,
                    failure_observer.as_ref(),
                    &policy,
                    &target,
                )
                .await;
                return;
            }
            Err(failure)
                if !committed
                    && failure.error.code == "continuation_not_found"
                    && prepared.allow_recovery
                    && continuation_fallback.is_some()
                    && policy.state.try_record_recovery_failure(
                        &selected_target_key(&target),
                        policy.epoch,
                        target.target_retry_budget,
                        target.target_cooldown_ms,
                    ) =>
            {
                attempt.finish(
                    "failed",
                    failure.diagnostic.status_code,
                    Some("continuation_not_found".into()),
                    None,
                );
                request = continuation_fallback.take().expect("checked fallback");
            }
            Err(failure) => {
                finish_vendor_failure(
                    failure,
                    committed,
                    &attempt,
                    terminal_output,
                    last_publication.as_ref(),
                    &mut ready,
                    failure_observer.as_ref(),
                    &policy,
                    &target,
                )
                .await;
                return;
            }
        }
    }
}

enum VendorTerminal {
    Infer(
        Box<stravia_runtime_contract::protocol::ir::AiResponse>,
        VendorPublicationFence,
    ),
    Compact(
        stravia_runtime_contract::protocol::ir::NativeCompactionResponse,
        VendorPublicationFence,
    ),
}

#[allow(clippy::too_many_arguments)]
async fn run_vendor_operation(
    gateway: &Gateway,
    principal: &stravia_runtime_contract::Principal,
    parent_cancellation: &stravia_runtime_contract::CancellationToken,
    operation_cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: Deadline,
    prepared: &mut PreparedAttempt,
    policy: &AttemptRoutePolicy,
    target: &SelectedTarget,
    reservation: &mut Option<RouteAttemptReservation>,
    request: &AiRequest,
    attempt: &AttemptObservation,
    output: &tokio::sync::mpsc::Sender<VendorPublishedResult>,
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<VendorDriverReady, AttemptFailure>>>,
    committed: &mut bool,
    streamed: &mut bool,
    last_publication: &mut Option<VendorPublicationFence>,
    first_token_ms: &mut Option<i64>,
) -> Result<VendorTerminal, AttemptFailure> {
    prepared
        .upstream_state
        .store(UPSTREAM_NOT_STARTED, Ordering::Release);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
    let mut context = VendorCallContext::new(operation_cancellation.clone(), deadline.clone());
    context.events = Some(event_tx);
    context.observer = prepared.observer.clone();
    context.model_turn_id = Some(prepared.model_turn_id.clone());
    context.attempt_id = Some(attempt.id.clone());
    context.client_headers = prepared.client_headers.clone();
    context.metadata = prepared.metadata.clone();
    context.websocket_affinity = prepared.websocket_affinity.clone();
    context.response_continuation_available = prepared.response_continuation_available.clone();

    let kind = if prepared.compact {
        stravia_vendor_sdk::Operation::Compact
    } else {
        stravia_vendor_sdk::Operation::Infer
    };
    let execution_handle = match prepared.execution.take() {
        Some(execution) => execution,
        None => {
            let mut execution = gateway
                .prepare_vendor_execution_with_lease(
                    &prepared.pinned_execution,
                    &prepared.route.provider_id,
                    Some(&prepared.actual_model),
                    kind,
                    &context,
                )
                .await
                .map_err(|_| {
                    if operation_cancellation.is_cancelled() || deadline.is_exceeded() {
                        AttemptFailure::terminal(
                            interruption_error(&deadline).code,
                            interruption_error(&deadline).message,
                        )
                    } else {
                        AttemptFailure::reroutable(
                            "provider_unavailable",
                            "Vendor execution could not be reprepared",
                        )
                    }
                })?;
            gateway
                .select_vendor_protocol(&mut execution, request, &context)
                .await
                .map_err(classify_vendor_error)?;
            execution
        }
    };
    let connection_changed = execution_handle.protocol().trim() != prepared.protocol_hint
        || target_namespace(
            &prepared.route.provider_id,
            &execution_handle.descriptor().provider_id,
            execution_handle.provider(),
            execution_handle.oauth_connection_id(),
            &prepared.route.target_id,
            &prepared.actual_model,
            execution_handle.use_proxy(),
        ) != prepared.namespace;
    if connection_changed {
        return Err(if *committed {
            AttemptFailure::terminal(
                "vendor_snapshot_changed",
                "Vendor compatibility changed after output publication began",
            )
        } else {
            AttemptFailure::reroutable(
                "vendor_snapshot_changed",
                "Vendor compatibility changed while repreparing the operation",
            )
        });
    }

    let mut request = request.clone();
    request.model.clone_from(&prepared.dispatch_model);
    crate::media::ingest::materialize_request(
        gateway,
        principal,
        &mut request,
        prepared.route.egress,
    )
    .await
    .map_err(|error| AttemptFailure::terminal("attachment_delivery_failed", error.to_string()))?;
    let vendor_request = if prepared.compact {
        VendorRequest::Compact(request)
    } else {
        VendorRequest::Infer(request)
    };
    let compact = prepared.compact;
    let execution = gateway.execute_prepared_vendor(execution_handle, vendor_request, context);
    tokio::pin!(execution);
    let mut operation_result = None;
    let mut pending_failure = None;
    let mut precommit = Vec::new();
    let mut emitted_delta = false;

    loop {
        tokio::select! {
            biased;
            _ = parent_cancellation.cancelled() => {
                operation_cancellation.cancel();
                let interruption = interruption_error(&deadline);
                return Err(if interruption.code == "deadline_exceeded"
                    && prepared.upstream_state.load(Ordering::Acquire)
                        & UPSTREAM_FAILURE_MASK
                        == UPSTREAM_STARTED
                {
                    AttemptFailure::upstream(
                        stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout,
                        None,
                        interruption.code,
                        interruption.message,
                        None,
                    )
                } else {
                    AttemptFailure::terminal(interruption.code, interruption.message)
                });
            }
            () = deadline.wait() => {
                operation_cancellation.cancel();
                return Err(if prepared.upstream_state.load(Ordering::Acquire)
                    & UPSTREAM_FAILURE_MASK
                    == UPSTREAM_STARTED
                {
                    AttemptFailure::upstream(
                        stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout,
                        None,
                        "deadline_exceeded",
                        "Model Turn deadline exceeded",
                        None,
                    )
                } else {
                    AttemptFailure::terminal(
                        "deadline_exceeded",
                        "Model Turn deadline exceeded",
                    )
                });
            }
            _ = output.closed() => {
                operation_cancellation.cancel();
                return Err(AttemptFailure::terminal("cancelled", "Model Turn consumer disconnected"));
            }
            event = event_rx.recv() => match event {
                Some(event) => process_runtime_event(
                    event,
                    gateway,
                    principal,
                    attempt,
                    output,
                    parent_cancellation,
                    operation_cancellation,
                    &deadline,
                    ready,
                    committed,
                    streamed,
                    policy,
                    target,
                    reservation,
                    &mut emitted_delta,
                    &mut precommit,
                    &mut pending_failure,
                    last_publication,
                    first_token_ms,
                    prepared.preserve_upstream_error,
                    &prepared.upstream_state,
                ).await?,
                None => break,
            },
            result = &mut execution => {
                mark_upstream_operation_finished(&prepared.upstream_state, &result, &deadline);
                operation_result = Some(result);
                break;
            }
        }
    }

    if operation_result.is_none() {
        let result = execution.await;
        mark_upstream_operation_finished(&prepared.upstream_state, &result, &deadline);
        operation_result = Some(result);
    }
    while let Some(event) = event_rx.recv().await {
        process_runtime_event(
            event,
            gateway,
            principal,
            attempt,
            output,
            parent_cancellation,
            operation_cancellation,
            &deadline,
            ready,
            committed,
            streamed,
            policy,
            target,
            reservation,
            &mut emitted_delta,
            &mut precommit,
            &mut pending_failure,
            last_publication,
            first_token_ms,
            prepared.preserve_upstream_error,
            &prepared.upstream_state,
        )
        .await?;
    }
    if let Some(failure) = pending_failure {
        return Err(failure);
    }
    let VendorExecution {
        output: result,
        publication,
        protocol,
    } = operation_result
        .expect("operation result")
        .map_err(|error| {
            classify_vendor_operation_error(error, &prepared.upstream_state, &deadline)
        })?;
    if protocol.trim() != prepared.protocol_hint {
        return Err(if *committed {
            AttemptFailure::terminal(
                "vendor_snapshot_changed",
                "Vendor compatibility changed after output publication began",
            )
        } else {
            AttemptFailure::reroutable(
                "vendor_snapshot_changed",
                "Vendor compatibility changed while preparing the operation",
            )
        });
    }

    match (compact, result) {
        (false, OperationOutput::Infer(response)) => {
            if prepared.request.meta.redaction.has_provider_proof() {
                // 上游画像不含宿主解析出的 Target control，证明仍需保留本轮实际派发的控制。
                let target_control = prepared.request.reasoning.target_control.take();
                let effective_controls =
                    crate::generation_chain::apply_provider_effective_response(
                        &mut prepared.request,
                        &response,
                    )
                    .is_some();
                prepared.request.reasoning.target_control = target_control;
                if effective_controls {
                    prepared
                        .request
                        .meta
                        .redaction
                        .observe_provider_controls(&prepared.request);
                }
            }
            if !emitted_delta {
                // Synthetic deltas are observable and may become persisted history before the
                // terminal response is consumed. Externalize output media first so neither path
                // can publish provider bytes instead of the stable Artifact Reference.
                let mut projected = response.as_ref().clone();
                let normalization = tokio::select! {
                    biased;
                    _ = parent_cancellation.cancelled() => {
                        operation_cancellation.cancel();
                        return Err(AttemptFailure::terminal(
                            interruption_error(&deadline).code,
                            interruption_error(&deadline).message,
                        ));
                    }
                    () = deadline.wait() => {
                        operation_cancellation.cancel();
                        return Err(AttemptFailure::terminal(
                            "deadline_exceeded",
                            "Model Turn deadline exceeded",
                        ));
                    }
                    result = crate::media::ingest::normalize_response(
                        gateway,
                        principal,
                        &mut projected,
                        operation_cancellation,
                    ) => result,
                };
                normalization.map_err(|error| {
                    AttemptFailure::terminal("output_media_ingest_failed", error.to_string())
                })?;
                let deltas = ai_response_to_deltas(&projected)
                    .into_iter()
                    .map(|delta| (delta, publication.clone()))
                    .collect();
                commit_vendor_stream(
                    deltas,
                    false,
                    attempt,
                    output,
                    parent_cancellation,
                    operation_cancellation,
                    &deadline,
                    ready,
                    committed,
                    streamed,
                    policy,
                    target,
                    reservation,
                    &mut precommit,
                    last_publication,
                    first_token_ms,
                )
                .await?;
            } else if !*committed {
                commit_vendor_stream(
                    Vec::new(),
                    true,
                    attempt,
                    output,
                    parent_cancellation,
                    operation_cancellation,
                    &deadline,
                    ready,
                    committed,
                    streamed,
                    policy,
                    target,
                    reservation,
                    &mut precommit,
                    last_publication,
                    first_token_ms,
                )
                .await?;
            }
            Ok(VendorTerminal::Infer(response, publication))
        }
        (true, OperationOutput::Compact(response)) => {
            Ok(VendorTerminal::Compact(response, publication))
        }
        _ => Err(AttemptFailure::terminal(
            "vendor_output_invalid",
            "Vendor plugin returned an output for the wrong operation",
        )),
    }
}

fn mark_upstream_operation_finished(
    upstream_state: &AtomicU8,
    result: &anyhow::Result<VendorExecution>,
    deadline: &Deadline,
) {
    let ended_by_deadline =
        deadline.is_exceeded() && result.as_ref().err().is_some_and(runtime_error_is_deadline);
    if !ended_by_deadline {
        upstream_state.fetch_or(UPSTREAM_FINISHED, Ordering::AcqRel);
    }
}

fn runtime_error_is_deadline(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<RuntimeError>(),
        Some(RuntimeError::DeadlineExceeded)
            | Some(RuntimeError::Plugin {
                kind: ErrorKind::DeadlineExceeded,
                ..
            })
    )
}

fn classify_vendor_operation_error(
    error: anyhow::Error,
    upstream_state: &AtomicU8,
    deadline: &Deadline,
) -> AttemptFailure {
    if deadline.is_exceeded()
        && upstream_state.load(Ordering::Acquire) & UPSTREAM_FAILURE_MASK == UPSTREAM_STARTED
        && runtime_error_is_deadline(&error)
    {
        AttemptFailure::upstream(
            stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout,
            None,
            "deadline_exceeded",
            "Model Turn deadline exceeded",
            None,
        )
    } else {
        classify_vendor_error(error)
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_runtime_event(
    vendor_event: VendorEvent,
    gateway: &Gateway,
    principal: &stravia_runtime_contract::Principal,
    attempt: &AttemptObservation,
    output: &tokio::sync::mpsc::Sender<VendorPublishedResult>,
    parent_cancellation: &stravia_runtime_contract::CancellationToken,
    operation_cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: &Deadline,
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<VendorDriverReady, AttemptFailure>>>,
    committed: &mut bool,
    streamed: &mut bool,
    policy: &AttemptRoutePolicy,
    target: &SelectedTarget,
    reservation: &mut Option<RouteAttemptReservation>,
    emitted_delta: &mut bool,
    precommit: &mut Vec<(AiStreamDelta, VendorPublicationFence)>,
    pending_failure: &mut Option<AttemptFailure>,
    last_publication: &mut Option<VendorPublicationFence>,
    first_token_ms: &mut Option<i64>,
    preserve_upstream_error: bool,
    upstream_state: &AtomicU8,
) -> Result<(), AttemptFailure> {
    vendor_event.publication.ensure_current().map_err(|_| {
        AttemptFailure::terminal("cancelled", "Vendor result can no longer be published")
    })?;
    let VendorEvent { event, publication } = vendor_event;
    match event {
        RuntimeEvent::UpstreamStarted => {
            upstream_state.fetch_or(UPSTREAM_STARTED, Ordering::AcqRel);
        }
        RuntimeEvent::Delta(mut delta) => {
            let _local_work = UpstreamLocalWork::begin(upstream_state, deadline.clone());
            if matches!(&delta, AiStreamDelta::ItemDone { .. }) {
                let normalization = tokio::select! {
                    biased;
                    _ = parent_cancellation.cancelled() => {
                        operation_cancellation.cancel();
                        return Err(AttemptFailure::terminal(
                            interruption_error(deadline).code,
                            interruption_error(deadline).message,
                        ));
                    }
                    () = deadline.wait() => {
                        operation_cancellation.cancel();
                        return Err(AttemptFailure::terminal(
                            "deadline_exceeded",
                            "Model Turn deadline exceeded",
                        ));
                    }
                    result = crate::media::ingest::normalize_stream_delta(
                        gateway,
                        principal,
                        &mut delta,
                        operation_cancellation,
                    ) => result,
                };
                normalization.map_err(|error| {
                    AttemptFailure::terminal("output_media_ingest_failed", error.to_string())
                })?;
            }
            *emitted_delta = true;
            *streamed = true;
            if !*committed {
                if matches!(
                    delta,
                    AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
                ) {
                    *pending_failure = Some(stream_delta_failure(&delta, preserve_upstream_error));
                    return Ok(());
                }
                if precommit.len() >= 32 {
                    return Err(AttemptFailure::terminal(
                        "vendor_event_limit_exceeded",
                        "Vendor emitted too many metadata events before canonical output",
                    ));
                }
                let commits = is_first_output(&delta) || is_terminal_delta(&delta);
                precommit.push((delta, publication));
                if commits {
                    commit_vendor_stream(
                        Vec::new(),
                        true,
                        attempt,
                        output,
                        parent_cancellation,
                        operation_cancellation,
                        deadline,
                        ready,
                        committed,
                        streamed,
                        policy,
                        target,
                        reservation,
                        precommit,
                        last_publication,
                        first_token_ms,
                    )
                    .await?;
                }
            } else {
                observe_and_send_delta(
                    delta,
                    &publication,
                    attempt,
                    VendorOutputDelivery {
                        output,
                        parent_cancellation,
                        operation_cancellation,
                        deadline,
                    },
                    last_publication,
                )
                .await?;
            }
        }
        RuntimeEvent::Completed | RuntimeEvent::Compacted => {
            // The typed return value is authoritative and is fenced by
            // execute_vendor after the guest has returned. Event terminals are
            // deliberately held rather than published early.
        }
        RuntimeEvent::Failed {
            kind,
            message,
            upstream_status,
        } => {
            pending_failure
                .get_or_insert_with(|| classify_vendor_kind(kind, upstream_status, Some(message)));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn commit_vendor_stream(
    deltas: Vec<(AiStreamDelta, VendorPublicationFence)>,
    emitted_by_plugin: bool,
    attempt: &AttemptObservation,
    output: &tokio::sync::mpsc::Sender<VendorPublishedResult>,
    parent_cancellation: &stravia_runtime_contract::CancellationToken,
    operation_cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: &Deadline,
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<VendorDriverReady, AttemptFailure>>>,
    committed: &mut bool,
    streamed: &mut bool,
    policy: &AttemptRoutePolicy,
    target: &SelectedTarget,
    reservation: &mut Option<RouteAttemptReservation>,
    precommit: &mut Vec<(AiStreamDelta, VendorPublicationFence)>,
    last_publication: &mut Option<VendorPublicationFence>,
    first_token_ms: &mut Option<i64>,
) -> Result<(), AttemptFailure> {
    let first_commit = !*committed;
    if first_commit && reservation.is_none() {
        *reservation = Some(policy.state.reservation(
            policy.context.clone(),
            selected_target_key(target),
            policy.epoch,
            policy.probe,
        ));
    }
    let mut buffered = std::mem::take(precommit);
    buffered.extend(deltas);
    for (index, (delta, publication)) in buffered.into_iter().enumerate() {
        observe_and_send_delta(
            delta,
            &publication,
            attempt,
            VendorOutputDelivery {
                output,
                parent_cancellation,
                operation_cancellation,
                deadline,
            },
            last_publication,
        )
        .await?;
        if first_commit && index == 0 {
            *first_token_ms = Some(attempt.elapsed_ms());
            *committed = true;
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(VendorDriverReady {
                    streamed: *streamed || emitted_by_plugin,
                }));
            }
        }
    }
    Ok(())
}

struct VendorOutputDelivery<'a> {
    output: &'a tokio::sync::mpsc::Sender<VendorPublishedResult>,
    parent_cancellation: &'a stravia_runtime_contract::CancellationToken,
    operation_cancellation: &'a stravia_runtime_contract::CancellationToken,
    deadline: &'a Deadline,
}

async fn observe_and_send_delta(
    delta: AiStreamDelta,
    publication: &VendorPublicationFence,
    attempt: &AttemptObservation,
    delivery: VendorOutputDelivery<'_>,
    last_publication: &mut Option<VendorPublicationFence>,
) -> Result<(), AttemptFailure> {
    attempt.observe_delta(&delta);
    send_vendor_output(
        delivery.output,
        publication,
        delivery.parent_cancellation,
        delivery.operation_cancellation,
        delivery.deadline,
        Ok(CanonicalEvent::Delta(delta)),
    )
    .await
    .map_err(|error| AttemptFailure::terminal(error.code, error.message))?;
    *last_publication = Some(publication.clone());
    Ok(())
}

async fn send_vendor_output(
    output: &tokio::sync::mpsc::Sender<VendorPublishedResult>,
    publication: &VendorPublicationFence,
    parent_cancellation: &stravia_runtime_contract::CancellationToken,
    operation_cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: &Deadline,
    event: Result<CanonicalEvent, ModelTurnError>,
) -> Result<(), ModelTurnError> {
    let permit = tokio::select! {
        biased;
        _ = publication.cancelled() => {
            return Err(ModelTurnError::new("cancelled", "Vendor result can no longer be published"));
        }
        _ = parent_cancellation.cancelled() => {
            operation_cancellation.cancel();
            return Err(interruption_error(deadline));
        }
        () = deadline.wait() => {
            operation_cancellation.cancel();
            return Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded"));
        }
        permit = output.reserve() => permit.map_err(|_| {
            operation_cancellation.cancel();
            ModelTurnError::new("cancelled", "Model Turn consumer disconnected")
        })?,
    };
    let guard = publication.write_fence().await.map_err(|_| {
        ModelTurnError::new("cancelled", "Vendor result can no longer be published")
    })?;
    permit.send(VendorPublishedResult {
        result: event,
        publication: publication.clone(),
    });
    drop(guard);
    Ok(())
}

async fn send_vendor_terminal_error(
    permit: tokio::sync::mpsc::OwnedPermit<VendorPublishedResult>,
    publication: &VendorPublicationFence,
    error: ModelTurnError,
) -> Result<(), ModelTurnError> {
    let guard = publication.terminal_write_fence().await.map_err(|_| {
        ModelTurnError::new("cancelled", "Vendor result can no longer be published")
    })?;
    drop(permit.send(VendorPublishedResult {
        result: Err(error),
        publication: publication.clone(),
    }));
    drop(guard);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn finish_vendor_failure(
    mut failure: AttemptFailure,
    committed: bool,
    attempt: &AttemptObservation,
    terminal_output: tokio::sync::mpsc::OwnedPermit<VendorPublishedResult>,
    publication: Option<&VendorPublicationFence>,
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<VendorDriverReady, AttemptFailure>>>,
    observer: Option<&crate::interaction_observation::RunObserver>,
    policy: &AttemptRoutePolicy,
    target: &SelectedTarget,
) {
    if committed && failure.is_upstream() && failure.error.code == "upstream_error" {
        failure.error.code = "upstream_stream_error".into();
        failure.error.message = "Vendor upstream stream error".into();
    }
    let status = failure.diagnostic.status_code;
    let code = failure.error.code.clone();
    if committed {
        if failure.is_upstream() && failure.error.code != "deadline_exceeded" {
            policy.record_failure(target);
        }
        attempt.finish("failed", status, Some(code), None);
        let error = failure.finish(observer);
        if let Some(publication) = publication
            && let Err(error) =
                send_vendor_terminal_error(terminal_output, publication, error).await
        {
            tracing::debug!(code = %error.code, "Vendor terminal failure publication revoked");
        }
    } else {
        attempt.finish("failed", status, Some(code), None);
        if let Some(ready) = ready.take() {
            let _ = ready.send(Err(failure));
        }
    }
}

fn classify_vendor_error(error: anyhow::Error) -> AttemptFailure {
    match error.downcast::<RuntimeError>() {
        Ok(error) => {
            if error.is_upstream_failure() {
                return classify_upstream_failure(
                    error.model_error_kind(),
                    error.upstream_status(),
                    error.retry_after(),
                    error.transport_failure(),
                    error.diagnostic_message().map(str::to_owned),
                );
            }
            match error {
                RuntimeError::Plugin {
                    kind,
                    upstream_status,
                    ..
                } => classify_vendor_kind(kind, upstream_status, None),
                RuntimeError::Cancelled => {
                    AttemptFailure::terminal("cancelled", "Vendor operation was cancelled")
                }
                RuntimeError::DeadlineExceeded => AttemptFailure::terminal(
                    "deadline_exceeded",
                    "Vendor operation exceeded its deadline",
                ),
                RuntimeError::ResourceExhausted => AttemptFailure::terminal(
                    "vendor_resource_exhausted",
                    "Vendor plugin exceeded a resource limit",
                ),
                RuntimeError::Trapped => AttemptFailure::terminal(
                    "vendor_plugin_trapped",
                    "Vendor plugin execution failed",
                ),
                RuntimeError::InvalidOutput => AttemptFailure::terminal(
                    "vendor_output_invalid",
                    "Vendor plugin returned an invalid typed result",
                ),
            }
        }
        Err(_) => AttemptFailure::terminal(
            "vendor_runtime_failed",
            "Vendor operation could not be executed",
        ),
    }
}

fn classify_upstream_failure(
    model_error_kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
    upstream_status: Option<u16>,
    retry_after: Option<Duration>,
    transport_failure: Option<TransportFailure>,
    diagnostic_message: Option<String>,
) -> AttemptFailure {
    use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind};

    let kind = model_error_kind.unwrap_or_else(|| {
        upstream_status
            .map(|status| AiError::kind_from_status(status, None))
            .unwrap_or(AiErrorKind::Unknown)
    });
    let authentication_failed = upstream_status == Some(401)
        || (upstream_status != Some(403) && matches!(&kind, AiErrorKind::AuthenticationError));
    let (code, message) = if authentication_failed {
        ("provider_auth_error", "Vendor authentication failed")
    } else {
        ("upstream_error", "Vendor upstream request failed")
    };
    AttemptFailure::upstream(kind, upstream_status, code, message, retry_after)
        .with_status(upstream_status)
        .with_transport_failure(transport_failure)
        .with_diagnostic_message(diagnostic_message)
}

fn classify_vendor_kind(
    kind: ErrorKind,
    upstream_status: Option<u16>,
    diagnostic_message: Option<String>,
) -> AttemptFailure {
    let model_error_kind = kind.model_error_kind();
    let retry_after = kind.retry_after();
    let transport_failure = kind.transport_failure();
    match kind {
        ErrorKind::Upstream(_) => classify_upstream_failure(
            model_error_kind,
            upstream_status,
            retry_after,
            transport_failure,
            diagnostic_message,
        ),
        ErrorKind::ContinuationNotFound => AttemptFailure::terminal(
            "continuation_not_found",
            "Vendor continuation is no longer available",
        )
        .upstream_origin()
        .with_status(upstream_status),
        ErrorKind::ProtectedReasoningRejected => AttemptFailure::terminal(
            "protected_reasoning_rejected",
            "Vendor rejected protected reasoning replay",
        )
        .upstream_origin()
        .with_status(upstream_status),
        ErrorKind::Auth => {
            AttemptFailure::terminal("provider_auth_error", "Vendor authentication failed")
                .upstream_origin()
                .with_status(upstream_status)
        }
        ErrorKind::Unsupported => AttemptFailure::terminal(
            "vendor_operation_unsupported",
            "Vendor operation is not supported",
        ),
        ErrorKind::Invalid => AttemptFailure::terminal(
            "vendor_request_invalid",
            "Vendor operation input was rejected",
        ),
        ErrorKind::Trapped => {
            AttemptFailure::terminal("vendor_plugin_trapped", "Vendor plugin execution failed")
        }
        ErrorKind::Cancelled => {
            AttemptFailure::terminal("cancelled", "Vendor operation was cancelled")
        }
        ErrorKind::DeadlineExceeded => AttemptFailure::terminal(
            "deadline_exceeded",
            "Vendor operation exceeded its deadline",
        ),
        ErrorKind::ResourceExhausted => AttemptFailure::terminal(
            "vendor_resource_exhausted",
            "Vendor plugin exceeded a resource limit",
        ),
    }
}

fn stream_delta_failure(delta: &AiStreamDelta, preserve_upstream_error: bool) -> AttemptFailure {
    match delta {
        AiStreamDelta::StreamError { error } => {
            let mut failure = classify_upstream_failure(
                Some(error.kind.clone()),
                error.status_code,
                None,
                None,
                Some(error.message.clone()),
            );
            if failure.error.code == "upstream_error" {
                failure.error.code = "upstream_stream_error".into();
                failure.error.message = "Vendor upstream stream error".into();
            }
            if preserve_upstream_error {
                failure.error.upstream_body = error.raw.clone().map(|mut body| {
                    crate::interaction_observation::redact_value(&mut body);
                    Box::new(body)
                });
            }
            failure
        }
        AiStreamDelta::UnexpectedEof => AttemptFailure::terminal(
            "upstream_stream_incomplete",
            "Vendor upstream stream ended unexpectedly",
        )
        .upstream_origin(),
        _ => unreachable!("only terminal stream failures are classified"),
    }
}

/// Records an upstream failure on an early-return path that never reaches
/// `RouteAttemptPolicy::record_failure`. Local preparation/validation failures
/// are excluded; dropping/skipping the policy only releases their reservation.
fn record_upstream_failure(
    attempts: &RouteAttemptPolicy,
    target: &SelectedTarget,
    failure: &AttemptFailure,
) {
    if failure.is_upstream() {
        attempts.state().record_failure(
            &selected_target_key(target),
            attempts.current_epoch(),
            target.target_retry_budget,
            target.target_cooldown_ms,
        );
    }
}

fn is_first_output(delta: &AiStreamDelta) -> bool {
    match delta {
        AiStreamDelta::TextDelta(text)
        | AiStreamDelta::RefusalDelta(text)
        | AiStreamDelta::ThinkingDelta(text) => !text.is_empty(),
        AiStreamDelta::TextDeltaWithMetadata { text, .. }
        | AiStreamDelta::RefusalDeltaWithIndex { text, .. }
        | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
        | AiStreamDelta::ReasoningSummaryDelta { text, .. } => !text.is_empty(),
        AiStreamDelta::ToolCallStart { .. }
        | AiStreamDelta::ToolCallDelta { .. }
        | AiStreamDelta::ToolCallComplete { .. }
        | AiStreamDelta::ItemDone { .. } => true,
        _ => false,
    }
}

fn is_terminal_delta(delta: &AiStreamDelta) -> bool {
    matches!(
        delta,
        AiStreamDelta::StreamError { .. }
            | AiStreamDelta::UnexpectedEof
            | AiStreamDelta::Done { .. }
            | AiStreamDelta::ResponseTerminal { .. }
    )
}

fn target_namespace(
    provider_id: &str,
    supplier_id: &str,
    provider: &stravia_vendor_sdk::ProviderSnapshot,
    oauth_connection_id: Option<&str>,
    target_key: &str,
    actual_model: &str,
    use_proxy: bool,
) -> String {
    let credential_identity = oauth_connection_id
        .map(|connection_id| namespace_fingerprint(&("oauth", connection_id)))
        .unwrap_or_else(|| namespace_fingerprint(&provider.credentials));
    namespace_fingerprint(&(
        target_key,
        provider_id,
        supplier_id,
        provider.channel.as_str(),
        provider.protocol.as_str(),
        use_proxy,
        provider.base_url.as_str(),
        actual_model,
        credential_identity,
        &provider.options,
    ))
}

fn thinking_replay_source_is_compatible(
    item: &stravia_runtime_contract::protocol::ir::AiItem,
    ingress: Option<stravia_runtime_contract::protocol::ids::ProtocolEndpoint>,
    target: &crate::history_marker::ThinkingSource,
    omit_protected: bool,
) -> bool {
    if omit_protected {
        return false;
    }
    if let Some(source) = crate::history_marker::ThinkingSource::from_item(item) {
        return source == *target;
    }
    if crate::history_marker::ThinkingSource::item_has_source_stamp(item) {
        return false;
    }

    // External native history has no private source stamp. It can retain its
    // native carrier only across the same protocol; a stamped item must match
    // the exact Target identity above so protected history cannot use this path.
    ingress.is_some_and(|ingress| {
        target
            .protocol
            .as_ref()
            .is_some_and(|protocol| protocol.matches_endpoint(ingress))
    })
}

fn prepare_canonical_thinking_replay(
    request: &mut AiRequest,
    target: &crate::history_marker::ThinkingSource,
    omit_protected: bool,
) -> bool {
    let mut replayed = false;
    request.items.retain_mut(|item| {
        let preserve = !omit_protected
            && crate::history_marker::ThinkingSource::from_item(item)
                .is_some_and(|source| source == *target);
        let has_calls = item
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
            || item.tool_call_id.is_some();
        let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
            &mut item.content
        else {
            return true;
        };
        let protected_only =
            !blocks.is_empty()
                && blocks.iter().all(|block| {
                    matches!(
                block,
                stravia_runtime_contract::protocol::ir::ContentBlock::Thinking { .. }
                    | stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning { .. }
                    | stravia_runtime_contract::protocol::ir::ContentBlock::RedactedThinking { .. }
            )
                });
        blocks.retain_mut(|block| match block {
            stravia_runtime_contract::protocol::ir::ContentBlock::Thinking {
                signature, ..
            } => {
                if signature.is_some() {
                    if preserve {
                        replayed = true;
                    } else {
                        *signature = None;
                    }
                }
                true
            }
            stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning {
                encrypted_content,
                ..
            } => {
                if encrypted_content.is_some() {
                    if preserve {
                        replayed = true;
                    } else {
                        *encrypted_content = None;
                    }
                }
                true
            }
            stravia_runtime_contract::protocol::ir::ContentBlock::RedactedThinking { .. } => {
                if preserve {
                    replayed = true;
                }
                preserve
            }
            _ => true,
        });
        !(protected_only && blocks.is_empty() && !has_calls)
    });
    replayed
}

fn request_requires_affinity(request: &AiRequest) -> bool {
    matches!(
        request.ext.as_ref(),
        Some(stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(extension))
            if extension.store == Some(false)
    )
}

fn header_pairs(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

fn namespace_fingerprint<T: serde::Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    stravia_runtime_contract::protocol::ir::canonical::hash_hex(
        &stravia_runtime_contract::protocol::ir::canonical::hash_bytes(&bytes),
    )
}

fn request_contains_video(request: &AiRequest) -> bool {
    request.items.iter().any(|message| {
        let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
            &message.content
        else {
            return false;
        };
        blocks.iter().any(|block| {
            matches!(
                block,
                stravia_runtime_contract::protocol::ir::ContentBlock::Video { .. }
            )
        })
    })
}

fn vendor_metadata_declares_only_image_operation(
    metadata: &stravia_vendor_sdk::ModelMetadata,
) -> bool {
    let mut declares_image = false;
    for capability in &metadata.capabilities {
        if matches!(capability.as_str(), "media_image" | "image_output") {
            declares_image = true;
        } else if stravia_vendor_sdk::Capability::parse(capability).is_some() {
            return false;
        }
    }
    declares_image
}

fn vendor_metadata_declares_capability(
    metadata: &stravia_vendor_sdk::ModelMetadata,
    capability: &str,
) -> bool {
    metadata
        .extensions
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|capabilities| {
            capabilities
                .iter()
                .any(|value| value.as_str() == Some(capability))
        })
}

fn vendor_metadata_supports_modality(
    metadata: &stravia_vendor_sdk::ModelMetadata,
    modality: &str,
) -> bool {
    metadata
        .extensions
        .get("modalities")
        .and_then(serde_json::Value::as_object)
        .and_then(|modalities| modalities.get("input"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|inputs| {
            inputs.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case(modality))
            })
        })
}

fn interruption_error(deadline: &Deadline) -> ModelTurnError {
    if deadline.is_exceeded() {
        ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")
    } else {
        ModelTurnError::new("cancelled", "Model Turn cancelled")
    }
}

fn attachment_ingest_error(
    error: stravia_runtime_contract::artifact::ArtifactError,
) -> ModelTurnError {
    let mapping = crate::agent::artifact::artifact_error_mapping(&error);
    let mut error = ModelTurnError::new("attachment_ingest_failed", mapping.diagnostic_message);
    // ModelTurnError has one status carrier; the response renderer keeps this code platform-owned.
    error.upstream_status = Some(mapping.status);
    error
}

fn model_turn_gateway_error(error: GatewayError) -> ModelTurnError {
    ModelTurnError::new(error.stable_code(), error.message())
}

#[cfg(test)]
mod tests {
    use super::{
        AttemptDeadlineGuard, UPSTREAM_FINISHED, UPSTREAM_NOT_STARTED, UPSTREAM_STARTED,
        UpstreamLocalWork, VendorDriverHandle, prepare_canonical_thinking_replay,
        thinking_replay_source_is_compatible,
    };
    use crate::history_marker::ThinkingSource;
    use crate::router::{RoutePolicyState, TargetRuntimeState};
    use std::sync::{Arc, atomic::AtomicU8};
    use std::time::{Duration, Instant};
    use stravia_runtime_contract::Deadline;
    use stravia_runtime_contract::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    };
    use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest};

    #[test]
    fn native_thinking_replay_requires_same_protocol_or_exact_source() {
        let target = ThinkingSource {
            namespace: "target-namespace".into(),
            protocol: Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.into()),
            actual_model: "target-model".into(),
            target_id: "target-id".into(),
        };
        let native = AiItem::thinking("native reasoning", None);

        assert!(thinking_replay_source_is_compatible(
            &native,
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            &target,
            false,
        ));
        assert!(!thinking_replay_source_is_compatible(
            &native,
            Some(ANTHROPIC_MESSAGES_2023_06_01),
            &target,
            false,
        ));

        let mut matching = native.clone();
        target.stamp_item(&mut matching);
        assert!(thinking_replay_source_is_compatible(
            &matching,
            Some(ANTHROPIC_MESSAGES_2023_06_01),
            &target,
            false,
        ));

        let mut foreign = native.clone();
        ThinkingSource {
            namespace: "foreign-namespace".into(),
            ..target.clone()
        }
        .stamp_item(&mut foreign);
        assert!(!thinking_replay_source_is_compatible(
            &foreign,
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            &target,
            false,
        ));

        let opaque_target = ThinkingSource {
            namespace: "opaque-namespace".into(),
            protocol: Some(
                stravia_runtime_contract::protocol::ids::ProtocolIdentity::new(
                    "acme/private-inference-v7",
                ),
            ),
            actual_model: "opaque-model".into(),
            target_id: "opaque-target".into(),
        };
        let mut opaque = native.clone();
        opaque_target.stamp_item(&mut opaque);
        assert!(thinking_replay_source_is_compatible(
            &opaque,
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            &opaque_target,
            false,
        ));
        assert!(!thinking_replay_source_is_compatible(
            &native,
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            &opaque_target,
            false,
        ));

        let mut malformed = native.clone();
        malformed.meta = Some(serde_json::json!({"__stravia_thinking_source": "invalid"}));
        assert!(!thinking_replay_source_is_compatible(
            &malformed,
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            &target,
            false,
        ));
        assert!(!thinking_replay_source_is_compatible(
            &native,
            Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
            &target,
            true,
        ));
    }

    #[test]
    fn opaque_protocol_replays_only_exact_source_protected_payloads() {
        let target = ThinkingSource {
            namespace: "opaque-namespace".into(),
            protocol: Some(
                stravia_runtime_contract::protocol::ids::ProtocolIdentity::new(
                    "acme/private-inference-v7",
                ),
            ),
            actual_model: "opaque-model".into(),
            target_id: "opaque-target".into(),
        };
        let mut signed = AiItem::thinking("private thought", Some("opaque-signature".into()));
        target.stamp_item(&mut signed);
        let mut encrypted = AiItem::reasoning(
            vec!["summary".into()],
            vec!["content".into()],
            Some("opaque-encrypted".into()),
        );
        target.stamp_item(&mut encrypted);
        let mut matching = AiRequest::new("model", vec![signed, encrypted]);

        assert!(prepare_canonical_thinking_replay(
            &mut matching,
            &target,
            false
        ));
        assert_eq!(
            matching.items[0].thinking_ref(),
            Some(("private thought", Some("opaque-signature")))
        );
        assert!(matches!(
            matching.items[1].reasoning_ref(),
            Some((_, _, Some("opaque-encrypted")))
        ));

        let foreign = ThinkingSource {
            namespace: "different-namespace".into(),
            ..target.clone()
        };
        for item in &mut matching.items {
            foreign.stamp_item(item);
        }
        assert!(!prepare_canonical_thinking_replay(
            &mut matching,
            &target,
            false
        ));
        assert_eq!(
            matching.items[0].thinking_ref(),
            Some(("private thought", None))
        );
        assert!(matches!(
            matching.items[1].reasoning_ref(),
            Some((_, _, None))
        ));
    }

    fn armed_deadline_guard(
        state: &RoutePolicyState,
        deadline: Deadline,
        upstream_state: bool,
    ) -> AttemptDeadlineGuard {
        AttemptDeadlineGuard {
            state: state.clone(),
            target_key: "provider:model".into(),
            epoch: 0,
            retry_budget: 0,
            cooldown_ms: 120_000,
            deadline,
            upstream_state: Arc::new(AtomicU8::new(if upstream_state {
                UPSTREAM_STARTED
            } else {
                UPSTREAM_NOT_STARTED
            })),
            armed: true,
        }
    }

    #[tokio::test]
    async fn expired_armed_guard_records_an_upstream_failure() {
        let state = RoutePolicyState::default();
        let deadline = Deadline::from_now(Duration::from_millis(20));
        let pending = {
            let state = state.clone();
            let deadline = deadline.clone();
            async move {
                let _guard = armed_deadline_guard(&state, deadline, true);
                std::future::pending::<()>().await
            }
        };
        while !deadline.is_exceeded() {
            tokio::task::yield_now().await;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), pending).await;
        assert_eq!(
            state.target_status("provider:model").state,
            TargetRuntimeState::CoolingDown
        );
    }

    #[tokio::test]
    async fn expired_live_driver_drop_cools_target_for_next_selection() {
        let state = RoutePolicyState::default();
        let cancellation = stravia_runtime_contract::CancellationToken::new();
        let handle = VendorDriverHandle {
            join: Some(tokio::spawn(std::future::pending::<()>())),
            cancellation: cancellation.clone(),
            publication_completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deadline_guard: Some(armed_deadline_guard(
                &state,
                Deadline::fixed(Instant::now() - Duration::from_secs(1)),
                true,
            )),
        };
        drop(handle);
        assert!(cancellation.is_cancelled());
        assert_eq!(
            state.target_status("provider:model").state,
            TargetRuntimeState::CoolingDown
        );
    }

    #[tokio::test]
    async fn dropped_armed_guard_does_not_record_user_cancel() {
        let state = RoutePolicyState::default();
        let pending = {
            let state = state.clone();
            async move {
                let _guard = armed_deadline_guard(
                    &state,
                    Deadline::from_now(Duration::from_secs(3600)),
                    true,
                );
                std::future::pending::<()>().await
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(10), pending).await;
        assert_eq!(
            state.target_status("provider:model").state,
            TargetRuntimeState::Available
        );
    }

    #[test]
    fn expired_guard_before_provider_send_does_not_record_failure() {
        let state = RoutePolicyState::default();
        drop(armed_deadline_guard(
            &state,
            Deadline::fixed(Instant::now() - Duration::from_secs(1)),
            false,
        ));
        assert_eq!(
            state.target_status("provider:model").state,
            TargetRuntimeState::Available
        );
    }

    #[test]
    fn expired_local_delivery_keeps_target_available_for_next_selection() {
        let state = RoutePolicyState::default();
        let guard = armed_deadline_guard(
            &state,
            Deadline::fixed(Instant::now() - Duration::from_secs(1)),
            true,
        );
        {
            let _local_work = UpstreamLocalWork::begin(
                &guard.upstream_state,
                Deadline::fixed(Instant::now() - Duration::from_secs(1)),
            );
        }
        drop(guard);
        assert_eq!(
            state.target_status("provider:model").state,
            TargetRuntimeState::Available
        );
    }

    #[test]
    fn expired_guard_after_upstream_completion_keeps_target_available_for_next_selection() {
        let state = RoutePolicyState::default();
        let guard = armed_deadline_guard(
            &state,
            Deadline::fixed(Instant::now() - Duration::from_secs(1)),
            true,
        );
        guard.upstream_state.store(
            UPSTREAM_STARTED | UPSTREAM_FINISHED,
            std::sync::atomic::Ordering::Release,
        );
        drop(guard);
        assert_eq!(
            state.target_status("provider:model").state,
            TargetRuntimeState::Available
        );
    }

    #[test]
    fn disarmed_deadline_guard_does_not_record_failure() {
        let state = RoutePolicyState::default();
        let mut guard = armed_deadline_guard(
            &state,
            Deadline::fixed(Instant::now() - Duration::from_secs(1)),
            true,
        );
        guard.disarm();
        drop(guard);
        assert_eq!(
            state.target_status("provider:model").state,
            TargetRuntimeState::Available
        );
    }
}
