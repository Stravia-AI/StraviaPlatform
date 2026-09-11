use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream;
use rust_decimal::prelude::ToPrimitive;

use super::continuation::{ContinuationLookup, ContinuationTarget};
use super::provider::{
    AttemptObservation, ProviderAdapter, ProviderBinding, ProviderCall, ProviderStreamError,
    ProviderStreamResponse, ResponsesWebSocketBinding,
};
use super::support::{
    ai_response_to_deltas, is_openai_generation_target, load_route_targets, merge_provider_headers,
    resolve_vendor_adapter, runtime_binding_headers,
};
use super::{
    CanonicalEvent, ModelTurn, ModelTurnAuthorization, ModelTurnError, ModelTurnExecutor,
    StreamResponseAccumulator, TargetIdentity, TurnInput,
};
use crate::Gateway;
use crate::error::GatewayError;
use crate::interaction_observation::RunEvent;
use crate::protocol::ProviderProtocols;
use crate::provider::VendorRegistry;
use crate::proxy::client::ProxyClient;
use crate::proxy::context::RequestContext;
use crate::proxy::planner::{ProtocolMode, ProtocolPlan, negotiate};
use crate::proxy::security::Security;
use crate::router::{
    AttemptFailureDisposition, RouteAttemptContext, RouteAttemptPolicy, RoutePolicyState,
    RouteSchedulingSnapshot, SelectedTarget, conversation_identity, selected_target_key,
};
use stravia_runtime_contract::hook::RouteContext;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ir::AiError;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiStreamDelta;
use stravia_runtime_contract::protocol::ir::request::MediaRoutingMode;

#[derive(Clone)]
pub struct LiveModelTurnExecutor {
    gateway: Gateway,
    continuation: Arc<dyn ContinuationLookup>,
}

impl LiveModelTurnExecutor {
    pub fn new(gateway: Gateway, continuation: Arc<dyn ContinuationLookup>) -> Self {
        Self {
            gateway,
            continuation,
        }
    }
}

#[async_trait]
impl ModelTurnExecutor for LiveModelTurnExecutor {
    async fn execute(&self, mut input: TurnInput) -> Result<ModelTurn, ModelTurnError> {
        tokio::select! {
            biased;
            _ = input.cancellation.cancelled() => return Err(interruption_error(input.deadline)),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(input.deadline)) => return Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")),
            result = crate::media::ingest::normalize_request(&self.gateway, &input.principal, &mut input.request, &input.cancellation) => result.map_err(|error| ModelTurnError::new("attachment_ingest_failed", error.to_string()))?,
        }
        let model_turn_id = uuid::Uuid::new_v4().to_string();
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
                .models
                .iter()
                .find(|route| route.model_id == input.request.model)
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
            if observer.debug_enabled() {
                match serde_json::to_value(&input.request) {
                    Ok(payload) => {
                        observer.record(RunEvent::Checkpoint {
                            stage: "artifact_normalized_request".into(),
                            model_turn_id: Some(model_turn_id.clone()),
                            attempt_id: None,
                            payload: payload.clone(),
                        });
                        observer.record(RunEvent::Checkpoint {
                            stage: "canonical_request".into(),
                            model_turn_id: Some(model_turn_id.clone()),
                            attempt_id: None,
                            payload,
                        });
                    }
                    Err(_) => observer.record(RunEvent::ObservationGap {
                        reason: "canonical_request_serialization_failed".into(),
                    }),
                }
            }
        }
        let mut terminal = Some(ModelTurnTerminal {
            observer: observer.clone(),
            model_turn_id: model_turn_id.clone(),
            standalone,
            operation_started,
            finished: false,
        });
        let result = if input.cancellation.is_cancelled() {
            Err(interruption_error(input.deadline))
        } else if Instant::now() >= input.deadline {
            Err(ModelTurnError::new(
                "deadline_exceeded",
                "Model Turn deadline exceeded",
            ))
        } else {
            let deadline = tokio::time::Instant::from_std(input.deadline);
            let cancellation = input.cancellation.clone();
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Err(interruption_error(deadline.into_std()))
                }
                _ = tokio::time::sleep_until(deadline) => {
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
                        protocol: turn.route.egress,
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
                    turn.output = register_compaction_stream(
                        turn.output, self.gateway.compaction.clone(), principal.clone(),
                        crate::compaction::CompactionTarget { target_key: turn.target.target_id.clone(), namespace: turn.target.namespace.clone(), model: turn.target.actual_model.clone(), protocol: turn.route.egress.to_string() },
                        source_generation_id, source.map(|source| source.record_ids).unwrap_or_default(),
                        model_turn_id.clone(), registrations, observer.clone(), incoming_states, operation_started,
                    );
                    turn.output = completion_stream(
                        turn.output,
                        self.gateway.redaction.clone(),
                        principal,
                        trace,
                        cancellation.clone(),
                        deadline,
                        terminal.take().expect("Model Turn terminal owner"),
                    );
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

fn register_compaction_stream(
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
) -> super::CanonicalEventStream {
    use crate::interaction_observation::{CompactionMode, CompactionPhase};
    use futures::StreamExt;
    use stravia_runtime_contract::protocol::ir::canonical::native_compaction_item;
    let state = (
        output,
        compaction,
        principal,
        target,
        source_generation_id,
        source_record_ids,
        model_turn_id,
        registrations,
        observer,
        incoming_states,
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
                uuid::Uuid::new_v4().to_string()
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
                    registrations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(super::CompactionPublication {
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

fn completion_stream(
    output: super::CanonicalEventStream,
    redaction: crate::reversible_redaction::ReversibleRedaction,
    principal: stravia_runtime_contract::Principal,
    trace: stravia_runtime_contract::redaction::RedactionTrace,
    cancellation: stravia_runtime_contract::CancellationToken,
    deadline: tokio::time::Instant,
    terminal: ModelTurnTerminal,
) -> super::CanonicalEventStream {
    use futures::StreamExt;

    // Unfold retains its pending future in the stream, not in the caller's next()
    // future. Pausing consumption cannot restart a publication already in flight.
    let state = (output, redaction, principal, trace, cancellation, terminal);
    Box::pin(stream::unfold(state, move |mut state| async move {
        let (output, redaction, principal, trace, cancellation, terminal) = &mut state;
        if terminal.finished {
            return None;
        }
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(if tokio::time::Instant::now() >= deadline {
                ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")
            } else {
                ModelTurnError::new("cancelled", "Model Turn cancelled")
            }),
            _ = tokio::time::sleep_until(deadline) => Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")),
            result = async {
                match output.next().await {
                    Some(Ok(CanonicalEvent::Completed(response))) => {
                        // restore_stream has already yielded every trailing delta.
                        // Read the shared trace here, not when the turn was constructed.
                        redaction.publish(principal, trace).await?;
                        Ok(CanonicalEvent::Completed(response))
                    }
                    Some(Ok(CanonicalEvent::Compacted(response))) => {
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
        let result = if tokio::time::Instant::now() >= deadline {
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
    }).fuse())
}

async fn execute_inner(
    executor: LiveModelTurnExecutor,
    mut input: TurnInput,
    model_turn_id: String,
) -> Result<ModelTurn, ModelTurnError> {
    let gateway = &executor.gateway;
    let route = {
        let cache = gateway.model_cache.read().await;
        cache
            .match_model(&input.request.model)
            .or_else(|| {
                cache
                    .models
                    .iter()
                    .find(|model| model.id == input.request.model)
            })
            .cloned()
    }
    .ok_or_else(|| ModelTurnError::new("model_not_found", "Model is unavailable"))?;

    if let Some(requested) = input.request.reasoning.level {
        input.request.reasoning.level = requested
            .clamp(&route.supported_thinking_levels)
            .ok_or_else(|| {
                ModelTurnError::new(
                    "thinking_level_unsupported",
                    "Route has no Supported Thinking Level for this request",
                )
            })
            .map(Some)?;
    }

    if input.authorization == ModelTurnAuthorization::CapabilityGrant
        && stravia_media::contains_images(&input.request)
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

    let targets = load_route_targets(gateway, &route).await;
    let preferred_target =
        gateway
            .cache_affinity
            .preferred_target(&input.principal, &route.id, &input.request);
    let conversation = conversation_identity(&input.request);
    let conversation_affinity_target = if matches!(
        conversation,
        Some(crate::router::ConversationIdentity::GenerationParent(_))
    ) {
        executor
            .continuation
            .preferred_target(&input.principal, &input.request)
            .await
    } else {
        None
    };
    let scheduling_snapshot = load_scheduling_snapshot(gateway, &targets, input.observer.as_ref())
        .await
        .map_err(|error| {
            ModelTurnError::new(
                "route_scheduling_unavailable",
                format!("Route scheduling snapshot is unavailable: {error}"),
            )
        })?;
    let attempt_context = RouteAttemptContext {
        principal: input.principal.continuation_key(),
        route_id: route.id.clone(),
        conversation,
        conversation_affinity_target,
        cache_affinity_target: preferred_target,
        estimated_uncached_input_tokens: estimate_uncached_input_tokens(&input.request),
        now_ms: gateway.route_policy_state.now_ms(),
    };
    let mut attempts = RouteAttemptPolicy::new(
        &route.balance,
        &targets,
        attempt_context.clone(),
        &scheduling_snapshot,
        gateway.route_policy_state.clone(),
    );
    if let Some(plan) = input.request.meta.media_routing.as_ref() {
        attempts.retain(|target| plan.target_keys.contains(&selected_target_key(target)));
        if attempts.is_empty() {
            return Err(ModelTurnError::new(
                "input_modality_unsupported",
                "No eligible Target remains for the fixed Media routing plan",
            ));
        }
    }
    if attempts.is_empty() {
        return Err(ModelTurnError::new(
            "model_unavailable",
            "Model has no configured Target",
        ));
    }

    let native_compaction_requested = input.purpose == super::ModelTurnPurpose::Compact
        || crate::compaction::NativeCompactionControls::classify(&input.request).requested();
    let mut last_error = None;
    while let Some(target) = attempts.next_healthy(&gateway.health_registry) {
        let mut omit_protected_thinking = false;
        loop {
            let attempt_started = Instant::now();
            let mut protected_thinking_sent = false;
            let result = match prepare_attempt(
                &executor,
                &route,
                &target,
                &input,
                &model_turn_id,
                omit_protected_thinking,
            )
            .await
            {
                Ok(prepared) => {
                    protected_thinking_sent = prepared.protected_thinking_replayed;
                    let attempt = begin_attempt(
                        gateway,
                        &route,
                        &target,
                        &input,
                        prepared,
                        attempt_started,
                        gateway.route_policy_state.clone(),
                        attempt_context.clone(),
                    );
                    let result = if target.first_token_timeout_ms == 0 {
                        attempt.await
                    } else {
                        match tokio::time::timeout(
                            Duration::from_millis(target.first_token_timeout_ms as u64),
                            attempt,
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(AttemptFailure::upstream(
                                stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout,
                                None,
                                "first_token_timeout",
                                "Target did not produce a First Token before its timeout",
                                None,
                            )),
                        }
                    };
                    result
                }
                Err(failure) => Err(failure),
            };
            let failure = match result {
                Ok(turn) => {
                    attempts.accept_current();
                    return Ok(turn);
                }
                Err(failure) => failure,
            };
            if native_compaction_requested {
                return Err(failure.error);
            }
            if !omit_protected_thinking
                && protected_thinking_sent
                && failure.protected_reasoning_rejected
            {
                // 只在上游明确拒绝密文/签名、且尚未产出 canonical 输出时修正一次请求。
                omit_protected_thinking = true;
                continue;
            }
            let Some(kind) = failure.kind.clone() else {
                return Err(failure.error);
            };
            if !failure.record_health {
                attempts.skip_current();
                last_error = Some(failure.error);
                break;
            }
            match attempts.record_failure(
                &gateway.health_registry,
                &target,
                kind,
                false,
                failure.retry_after,
                gateway.route_policy_state.now_ms(),
                rand::random(),
            ) {
                AttemptFailureDisposition::RetrySame { delay } => {
                    tokio::time::sleep(delay).await;
                }
                AttemptFailureDisposition::TryNextTarget => {
                    last_error = Some(failure.error);
                    break;
                }
                AttemptFailureDisposition::Stop => return Err(failure.error),
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| ModelTurnError::new("provider_unavailable", "all Model Targets failed")))
}

async fn load_scheduling_snapshot(
    gateway: &Gateway,
    targets: &[crate::db::models::Target],
    observer: Option<&crate::interaction_observation::RunObserver>,
) -> anyhow::Result<RouteSchedulingSnapshot> {
    let usage = gateway
        .storage
        .usage_stats()
        .route_scheduling_snapshot()
        .await;
    if usage.stale
        && let Some(observer) = observer
    {
        observer.record(RunEvent::ObservationGap {
            reason: "usage_stats_snapshot_stale".into(),
        });
    }
    let mut snapshot = RouteSchedulingSnapshot {
        targets: usage.targets,
    };
    for target in targets {
        let key = format!("{}:{}", target.provider_id, target.model);
        let index = snapshot
            .targets
            .iter()
            .position(|item| item.target_key == key)
            .unwrap_or_else(|| {
                snapshot
                    .targets
                    .push(crate::router::TargetSchedulingSnapshot {
                        target_key: key.clone(),
                        ..Default::default()
                    });
                snapshot.targets.len() - 1
            });
        let Some(provider_model) = gateway
            .storage
            .provider_models()
            .get(&target.provider_id, &target.model)
            .await?
        else {
            continue;
        };
        let Some(cost) = provider_model.metadata.cost else {
            continue;
        };
        let target_snapshot = &mut snapshot.targets[index];
        target_snapshot.cost_input = cost.prices.input.and_then(|value| value.to_f64());
        target_snapshot.cost_output = cost.prices.output.and_then(|value| value.to_f64());
        target_snapshot.cost_cache_read = cost.prices.cache_read.and_then(|value| value.to_f64());
        target_snapshot.cost_cache_write = cost.prices.cache_write.and_then(|value| value.to_f64());
    }
    Ok(snapshot)
}

fn estimate_uncached_input_tokens(request: &AiRequest) -> u64 {
    serde_json::to_vec(&request.items)
        .map(|bytes| bytes.len().div_ceil(4) as u64)
        .unwrap_or_default()
}

struct PreparedAttempt {
    model_turn_id: String,
    route: RouteContext,
    provider_call: ProviderCall,
    reasoning_encrypted_content_requested: bool,
    protected_thinking_replayed: bool,
    force_stream: bool,
    actual_model: String,
    namespace: String,
}

struct AttemptFailure {
    error: ModelTurnError,
    kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
    record_health: bool,
    retry_after: Option<Duration>,
    protected_reasoning_rejected: bool,
}

impl AttemptFailure {
    fn retryable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ModelTurnError::new(code, message),
            kind: Some(stravia_runtime_contract::protocol::ir::AiErrorKind::ServiceUnavailable),
            record_health: true,
            retry_after: None,
            protected_reasoning_rejected: false,
        }
    }

    fn upstream(
        kind: stravia_runtime_contract::protocol::ir::AiErrorKind,
        status: Option<u16>,
        code: impl Into<String>,
        message: impl Into<String>,
        retry_after: Option<Duration>,
    ) -> Self {
        let record_health = status.map_or_else(
            || kind.is_retryable(),
            |status| matches!(status, 408 | 429 | 500 | 502 | 503 | 529),
        );
        Self {
            error: ModelTurnError::new(code, message),
            kind: Some(kind),
            record_health,
            retry_after,
            protected_reasoning_rejected: false,
        }
    }

    fn with_upstream_body(
        mut self,
        passthrough: bool,
        status: Option<u16>,
        body: Option<serde_json::Value>,
    ) -> Self {
        self.protected_reasoning_rejected = matches!(status, None | Some(400 | 422))
            && body.as_ref().is_some_and(protected_reasoning_rejected);
        if !passthrough {
            return self;
        }
        self.error.upstream_status = status.filter(|status| *status >= 400);
        if let Some(body) = &body {
            let error = body.get("error").unwrap_or(body);
            if let Some(message) = error.get("message").and_then(serde_json::Value::as_str) {
                self.error.message = message.to_owned();
            }
        }
        self.error.upstream_body = body;
        self
    }

    fn terminal(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ModelTurnError::new(code, message),
            kind: None,
            record_health: false,
            retry_after: None,
            protected_reasoning_rejected: false,
        }
    }

    fn ineligible(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ModelTurnError::new(code, message),
            kind: Some(stravia_runtime_contract::protocol::ir::AiErrorKind::ModelNotAvailable),
            record_health: false,
            retry_after: None,
            protected_reasoning_rejected: false,
        }
    }
}

async fn prepare_attempt(
    executor: &LiveModelTurnExecutor,
    route: &crate::db::models::Route,
    target: &SelectedTarget,
    input: &TurnInput,
    model_turn_id: &str,
    omit_protected_thinking: bool,
) -> Result<PreparedAttempt, AttemptFailure> {
    let gateway = &executor.gateway;
    let target_key = selected_target_key(target);
    let provider = gateway
        .storage
        .providers()
        .get(&target.provider_id)
        .await
        .map_err(|error| {
            AttemptFailure::retryable(
                "provider_unavailable",
                format!("provider unavailable: {error}"),
            )
        })?
        .filter(|provider| provider.is_enabled)
        .ok_or_else(|| {
            AttemptFailure::retryable(
                "provider_unavailable",
                format!("provider unavailable: {}", target.provider_id),
            )
        })?;
    let actual_model = if target.model.is_empty() || target.model == "*" {
        route.model_id.clone()
    } else {
        target.model.clone()
    };

    let metadata_required = input.request.meta.media_routing.is_some()
        || stravia_web_search::native_web_search_requested(&input.request)
        || input
            .request
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        || request_contains_video(&input.request)
        || stravia_media::contains_images(&input.request);
    let provider_model = gateway
        .storage
        .provider_models()
        .get(&provider.id, &actual_model)
        .await
        .map_err(|error| {
            if metadata_required {
                AttemptFailure::terminal(
                    "provider_metadata_unavailable",
                    format!("Provider Model metadata is unavailable: {error}"),
                )
            } else {
                AttemptFailure::retryable("provider_unavailable", error.to_string())
            }
        })?;

    let supports_tools = provider_model
        .as_ref()
        .and_then(|model| model.metadata.tool_call)
        .unwrap_or(false);
    if input.purpose != super::ModelTurnPurpose::Compact
        && input
            .request
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        && !supports_tools
    {
        return Err(AttemptFailure::ineligible(
            if stravia_web_search::native_web_search_requested(&input.request) {
                "web_search_unsupported"
            } else {
                "tools_unsupported"
            },
            "selected provider model does not support function tools",
        ));
    }
    if request_contains_video(&input.request)
        && !provider_model
            .as_ref()
            .is_some_and(|model| supports_modality(&model.metadata, "video"))
    {
        return Err(AttemptFailure::ineligible(
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
        && !provider_model
            .as_ref()
            .is_some_and(|model| crate::media::supports_image(&model.metadata))
    {
        return Err(AttemptFailure::ineligible(
            "input_modality_unsupported",
            "selected provider model does not support native image input",
        ));
    }

    let provider_runtime = gateway
        .admin()
        .resolve_provider_runtime(&provider)
        .await
        .map_err(|error| {
            AttemptFailure::retryable("provider_credential_error", error.to_string())
        })?;
    let provider_protocols = ProviderProtocols::from_provider(&provider);
    let ingress = input
        .request
        .meta
        .source_protocol
        .unwrap_or(OPEN_RESPONSES_2026_04_24);
    let openai_generation_target = is_openai_generation_target(
        provider.vendor.as_deref(),
        provider.preset_key.as_deref(),
        input.request.embedding.is_some(),
    );
    let responses_representable = openai_generation_target && {
        crate::protocol::transform::ProtocolTransform::global()
            .bind(ingress, OPEN_RESPONSES_2026_04_24)
            .and_then(|pair| {
                pair.encode_request(&input.request).or_else(|error| {
                    let mut probe = input.request.clone();
                    if !crate::protocol::transform::prepare_thinking_replay(
                        &mut probe,
                        OPEN_RESPONSES_2026_04_24,
                        |item| {
                            crate::history_marker::ThinkingSource::from_item(item).map_or(
                                ingress == OPEN_RESPONSES_2026_04_24,
                                |source| {
                                    source.protocol == OPEN_RESPONSES_2026_04_24
                                        && source.target_id == target_key
                                        && source.actual_model == actual_model
                                },
                            )
                        },
                    ) {
                        return Err(error);
                    }
                    pair.encode_request(&probe)
                })
            })
            .is_ok()
    };
    let mut request_context = RequestContext::new(
        ingress,
        input
            .deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(Duration::from_millis(1)),
    );
    request_context.cancellation = input.cancellation.clone();
    let plan = if responses_representable {
        ProtocolPlan {
            ingress,
            egress: OPEN_RESPONSES_2026_04_24,
            mode: if ingress == OPEN_RESPONSES_2026_04_24 {
                ProtocolMode::Native
            } else {
                ProtocolMode::Transform
            },
            base_url: provider_protocols.base_url.clone(),
            needs_conversion: ingress != OPEN_RESPONSES_2026_04_24,
        }
    } else {
        negotiate(
            ingress,
            None,
            Some(&provider_protocols),
            &mut request_context,
        )
        .map_err(|error| {
            AttemptFailure::terminal("protocol_negotiation_failed", error.to_string())
        })?
    };
    let egress = plan.egress;
    let egress_base_url = provider_runtime
        .binding
        .base_url_override
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if plan.base_url.is_empty() {
                provider.base_url.clone()
            } else {
                plan.base_url.clone()
            }
        });
    let vendor = resolve_vendor_adapter(&provider, egress.protocol).ok_or_else(|| {
        AttemptFailure::retryable(
            "provider_adapter_unavailable",
            format!(
                "no vendor adapter registered for '{}' or protocol '{}'",
                provider.vendor.as_deref().unwrap_or("custom"),
                egress.protocol
            ),
        )
    })?;
    let adapter = ProviderAdapter::new(
        vendor,
        ProviderBinding {
            provider: provider.clone(),
            protocol: egress,
            egress_base_url,
            api_key: provider_runtime.access_token.clone(),
            actual_model: actual_model.clone(),
            gateway: gateway.clone(),
            disable_default_auth: provider_runtime.binding.disable_default_auth,
            observer: input.observer.clone(),
            model_turn_id: model_turn_id.to_owned(),
            target_id: target_key.clone(),
            provider_name: provider.name.clone(),
        },
    );

    let target_namespace = target_namespace(
        &provider,
        &provider_runtime,
        &adapter,
        &target_key,
        &actual_model,
    );
    let target_capabilities = VendorRegistry::global()
        .resolve(&provider, egress)
        .map(|adapter| adapter.target_capabilities(egress))
        .unwrap_or_default();
    let compact = input.purpose == super::ModelTurnPurpose::Compact;
    let mut provider_request = input.request.clone();
    let thinking_source = crate::history_marker::ThinkingSource {
        namespace: target_namespace.clone(),
        protocol: egress,
        actual_model: actual_model.clone(),
        target_id: target_key.clone(),
    };
    // 降级只改变当前 Target 的回放视图；权威历史保留密文，切回来源时仍可原生回放。
    let thinking_replayed = crate::protocol::transform::prepare_thinking_replay(
        &mut provider_request,
        egress,
        |item| {
            !omit_protected_thinking
                && crate::history_marker::ThinkingSource::from_item(item)
                    .map_or(ingress == egress, |source| source == thinking_source)
        },
    );
    let protected_thinking_replayed = provider_request.items.iter().any(|item| {
        matches!(
            &item.content,
            stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks)
                if blocks.iter().any(|block| matches!(
                    block,
                    stravia_runtime_contract::protocol::ir::ContentBlock::Thinking { signature: Some(_), .. }
                        | stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning { encrypted_content: Some(_), .. }
                        | stravia_runtime_contract::protocol::ir::ContentBlock::RedactedThinking { .. }
                ))
        )
    });
    let controls = crate::compaction::NativeCompactionControls::classify(&provider_request);
    // Capability booleans describe advertised support, not a negative guarantee.
    // Unknown Responses targets must receive the client's native controls unchanged.
    if (compact
        || controls.requested()
        || provider_request
            .items
            .iter()
            .any(stravia_runtime_contract::protocol::ir::AiItem::is_compaction))
        && egress != OPEN_RESPONSES_2026_04_24
    {
        return Err(AttemptFailure::terminal(
            "compaction_unsupported",
            "Target protocol cannot represent the requested native compaction contract",
        ));
    }
    let binding = crate::compaction::CompactionTarget {
        target_key: target_key.clone(),
        namespace: target_namespace.clone(),
        model: actual_model.clone(),
        protocol: egress.to_string(),
    };
    if let Some(resolved) = gateway
        .compaction
        .resolve(&input.principal, &input.request.items)
        .await
        .map_err(|error| AttemptFailure::terminal(error.code(), error.to_string()))?
        && resolved.target != binding
    {
        return Err(AttemptFailure::ineligible(
            "compaction_target_mismatch",
            "Native compaction state is not compatible with this Target binding",
        ));
    }
    let websocket_enabled =
        !compact && openai_generation_target && target_capabilities.responses_websocket;
    if let Some(level) = provider_request.reasoning.level {
        let Some(control) = crate::thinking::mapping_control(&target.thinking_level_map, level)
        else {
            return Err(AttemptFailure::ineligible(
                "protocol_lossy_rejected",
                format!(
                    "Target has no mapping for Thinking Level {}",
                    level.as_str()
                ),
            ));
        };
        if control.is_hidden() {
            return Err(AttemptFailure::ineligible(
                "protocol_lossy_rejected",
                format!("Target hides Thinking Level {}", level.as_str()),
            ));
        }
        provider_request.reasoning.target_control = Some(control.clone());
    } else {
        provider_request.reasoning.target_control = None;
    }
    provider_request.model.clone_from(&route.model_id);
    if let Some(observer) = &input.observer {
        observer.record_debug(|| RunEvent::Checkpoint {
            stage: "artifact_normalized_request".into(),
            model_turn_id: Some(model_turn_id.to_owned()),
            attempt_id: None,
            payload: serde_json::to_value(&provider_request).unwrap_or_default(),
        });
    }
    let artifact_transfers = crate::media::ingest::materialize_request(
        gateway,
        &input.principal,
        &mut provider_request,
        egress,
    )
    .await
    .map_err(|error| AttemptFailure::terminal("attachment_delivery_failed", error.to_string()))?;
    let mut full_provider_request = provider_request.clone();
    crate::model_turn::clear_previous_response_id(&mut full_provider_request);
    let mut full_outbound = if compact {
        adapter
            .build_compact_request(&mut full_provider_request)
            .await
    } else {
        adapter.build_request(&mut full_provider_request).await
    }
    .map_err(|error| AttemptFailure::terminal(error.stable_code(), error.to_string()))?;
    if egress == OPEN_RESPONSES_2026_04_24
        && let serde_json::Value::Object(profile) =
            crate::protocol::codec::open_responses::encoder::effective_response_profile_from_request(
                &full_provider_request,
            )
    {
        normalize_provider_effective_request(&mut full_provider_request, &profile);
    }
    input
        .request
        .meta
        .redaction
        .observe_provider_request(&full_provider_request)
        .map_err(|error| {
            AttemptFailure::terminal("reversible_redaction_failed", error.to_string())
        })?;
    let require_affinity = provider.channel.as_deref() == Some("codex")
        || full_outbound
            .body
            .get("store")
            .and_then(serde_json::Value::as_bool)
            == Some(false);
    let continued_id = if compact || thinking_replayed {
        // 原生续接的前缀不能替代已经按当前 Target 改写过的完整历史。
        crate::model_turn::clear_previous_response_id(&mut provider_request);
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
                    logical_model: &input.request.model,
                    allow_ephemeral_response: websocket_enabled && require_affinity,
                },
                &mut provider_request,
            )
            .await
    };
    let mut outbound = if let Some(previous_response_id) = continued_id.as_ref() {
        let mut outbound = adapter
            .build_request(&mut provider_request)
            .await
            .map_err(|error| AttemptFailure::terminal(error.stable_code(), error.to_string()))?;
        outbound.body["previous_response_id"] =
            serde_json::Value::String(previous_response_id.clone());
        outbound
    } else {
        full_outbound.clone()
    };

    let binding_headers = runtime_binding_headers(&provider_runtime.binding)
        .map_err(|error| AttemptFailure::retryable("provider_runtime_error", error.to_string()))?;
    let client_headers = if provider.vendor.as_deref() == Some("openai")
        && provider.channel.as_deref() == Some("codex")
    {
        crate::provider::openai::codex::forwarded_client_headers(&input.extra_headers)
    } else {
        input.extra_headers.clone()
    };
    outbound.headers = merge_provider_headers(
        client_headers.clone(),
        outbound.headers,
        binding_headers.clone(),
    );
    full_outbound.headers =
        merge_provider_headers(client_headers, full_outbound.headers, binding_headers);

    let http_client = gateway
        .http_client_for_provider(provider.use_proxy)
        .await
        .map_err(|error| {
            AttemptFailure::retryable("provider_transport_error", error.to_string())
        })?;
    let client = if websocket_enabled {
        let websocket_client = gateway
            .responses_websocket_client_for_provider(provider.use_proxy)
            .await
            .map_err(|error| {
                AttemptFailure::retryable("provider_transport_error", error.to_string())
            })?;
        ProxyClient::with_responses_websocket(http_client, websocket_client)
    } else {
        ProxyClient::new(http_client)
    };
    let session_affinity = crate::generation_chain::generation_session_fingerprint(&input.request);
    if require_affinity && let Some(prompt_cache_key) = session_affinity.as_ref() {
        insert_default_prompt_cache_key(&mut outbound.body, prompt_cache_key);
        insert_default_prompt_cache_key(&mut full_outbound.body, prompt_cache_key);
    }
    let reasoning_encrypted_content_requested =
        requests_reasoning_encrypted_content(&outbound.body);
    let mut provider_call = if websocket_enabled {
        adapter.bind_responses_websocket(ResponsesWebSocketBinding {
            client,
            outbound,
            full_outbound,
            registry: gateway.responses_websockets.clone(),
            namespace: target_namespace.clone(),
            provider_id: provider.id.clone(),
            target_id: target_key.clone(),
            transport_attempt: uuid::Uuid::new_v4().to_string(),
            require_affinity,
            session_affinity,
        })
    } else if continued_id.is_some() {
        adapter.bind_with_continuation_fallback(client, outbound, full_outbound)
    } else {
        adapter.bind(client, outbound)
    };
    provider_call.set_artifact_transfers(input.principal.clone(), artifact_transfers);
    if compact || controls.requested() {
        provider_call.disable_retries();
    }
    Ok(PreparedAttempt {
        model_turn_id: model_turn_id.to_owned(),
        route: RouteContext {
            model_id: route.id.clone(),
            provider_id: provider.id.clone(),
            target_id: target_key,
            egress,
        },
        provider_call,
        reasoning_encrypted_content_requested,
        protected_thinking_replayed,
        force_stream: !compact
            && (input.request.stream.enabled
                || websocket_enabled
                || target_capabilities.stream_only),
        actual_model,
        namespace: target_namespace,
    })
}

fn protected_reasoning_rejected(body: &serde_json::Value) -> bool {
    let error = body.get("error").unwrap_or(body);
    if error.get("code").and_then(serde_json::Value::as_str) == Some("invalid_encrypted_content") {
        return true;
    }
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    message.contains("invalid signature in thinking block")
        || message.contains("invalid thinking signature")
}

fn requests_reasoning_encrypted_content(body: &serde_json::Value) -> bool {
    body.get("include")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|include| {
            include.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|value| value == "reasoning.encrypted_content")
            })
        })
}

fn insert_default_prompt_cache_key(body: &mut serde_json::Value, prompt_cache_key: &str) {
    if let Some(body) = body.as_object_mut() {
        body.entry("prompt_cache_key")
            .or_insert_with(|| serde_json::Value::String(prompt_cache_key.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        handle_terminal_stream_error, insert_default_prompt_cache_key,
        requests_reasoning_encrypted_content,
    };
    use stravia_runtime_contract::protocol::ir::AiError;
    use stravia_runtime_contract::protocol::ir::AiErrorKind;
    use stravia_runtime_contract::protocol::ir::AiStreamDelta;

    #[test]
    fn session_cache_key_fills_only_missing_provider_value() {
        let mut missing = serde_json::json!({"model": "gpt-test"});
        insert_default_prompt_cache_key(&mut missing, "session-cache");
        assert_eq!(missing["prompt_cache_key"], "session-cache");

        let mut explicit = serde_json::json!({"prompt_cache_key": "client-cache"});
        insert_default_prompt_cache_key(&mut explicit, "session-cache");
        assert_eq!(explicit["prompt_cache_key"], "client-cache");
    }

    #[test]
    fn encrypted_reasoning_detection_uses_the_outbound_include_contract() {
        assert!(requests_reasoning_encrypted_content(&serde_json::json!({
            "include": [
                "web_search_call.action.sources",
                "reasoning.encrypted_content"
            ]
        })));
        assert!(!requests_reasoning_encrypted_content(
            &serde_json::json!({"include": ["web_search_call.action.sources"]})
        ));
        assert!(!requests_reasoning_encrypted_content(
            &serde_json::json!({})
        ));
    }

    #[test]
    fn request_scoped_stream_errors_do_not_degrade_target_health() {
        let health = crate::router::health::HealthRegistry::new();
        let deltas = vec![AiStreamDelta::StreamError {
            error: AiError::new(AiErrorKind::StreamMidError, "invalid request").with_status(400),
        }];

        for _ in 0..3 {
            assert!(handle_terminal_stream_error(
                &health,
                "provider:model",
                &deltas
            ));
        }

        assert!(health.is_healthy("provider:model"));
    }

    #[test]
    fn retryable_stream_errors_degrade_target_health() {
        let health = crate::router::health::HealthRegistry::new();
        let deltas = vec![AiStreamDelta::StreamError {
            error: AiError::new(AiErrorKind::StreamMidError, "unavailable").with_status(503),
        }];

        for _ in 0..3 {
            assert!(handle_terminal_stream_error(
                &health,
                "provider:model",
                &deltas
            ));
        }

        assert!(!health.is_healthy("provider:model"));
    }
}

async fn begin_attempt(
    gateway: &Gateway,
    route: &crate::db::models::Route,
    target: &SelectedTarget,
    input: &TurnInput,
    mut prepared: PreparedAttempt,
    attempt_started: Instant,
    route_policy_state: RoutePolicyState,
    attempt_context: RouteAttemptContext,
) -> Result<ModelTurn, AttemptFailure> {
    let native_compaction_requested = input.purpose == super::ModelTurnPurpose::Compact
        || crate::compaction::NativeCompactionControls::classify(&input.request).requested();
    let mut target_identity = TargetIdentity {
        actual_model: prepared.actual_model.clone(),
        provider_id: prepared.route.provider_id.clone(),
        target_id: prepared.route.target_id.clone(),
        namespace: prepared.namespace.clone(),
        response_continuation_available: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )),
    };

    if input.purpose == super::ModelTurnPurpose::Compact {
        let (raw, status, _headers, attempt) = prepared
            .provider_call
            .call_compact()
            .await
            .map_err(|error| {
                if let Some(decode) =
                    error.downcast_ref::<crate::proxy::client::UpstreamResponseDecodeError>()
                {
                    AttemptFailure::terminal(
                        "upstream_error",
                        String::from_utf8_lossy(&decode.body).into_owned(),
                    )
                    .with_upstream_body(true, Some(decode.status), None)
                } else {
                    AttemptFailure::terminal("upstream_execution_uncertain", error.to_string())
                }
            })?;
        if status >= 400 {
            attempt.finish("failed", Some(status), Some("upstream_error".into()), None);
            return Err(AttemptFailure::terminal(
                "upstream_error",
                format!("upstream returned HTTP {status}"),
            )
            .with_upstream_body(native_compaction_requested, Some(status), Some(raw)));
        }
        let response =
            crate::protocol::codec::open_responses::parser::parse_compaction_response(&raw)
                .map_err(|error| {
                    AttemptFailure::terminal("invalid_compaction_response", error.to_string())
                })?;
        if let Some(usage) = &response.usage {
            attempt.confirm_usage(usage);
        }
        attempt.finish(
            "completed",
            Some(status),
            None,
            Some(attempt_started.elapsed().as_millis() as i64),
        );
        record_success(gateway, target, &route_policy_state, &attempt_context);
        return Ok(ModelTurn {
            model_turn_id: prepared.model_turn_id,
            route: prepared.route,
            target: target_identity,
            output: Box::pin(stream::once(async move {
                Ok(CanonicalEvent::Compacted(Box::new(response)))
            })),
            reasoning_encrypted_content_requested: false,
            streamed: false,
        });
    }

    if !prepared.force_stream {
        let call = prepared
            .provider_call
            .call_non_stream()
            .await
            .map_err(|error| {
                if let Some(decode) =
                    error.downcast_ref::<crate::proxy::client::UpstreamResponseDecodeError>()
                {
                    AttemptFailure::upstream(
                        AiError::kind_from_status(decode.status, None),
                        Some(decode.status),
                        "upstream_error",
                        error.to_string(),
                        retry_after(&decode.headers),
                    )
                    .with_upstream_body(
                        native_compaction_requested,
                        Some(decode.status),
                        None,
                    )
                } else {
                    AttemptFailure::retryable("upstream_error", error.to_string())
                }
            })?;
        if call.status >= 400 {
            let kind = call
                .canonical
                .as_ref()
                .ok()
                .and_then(|response| response.error.as_ref())
                .map(|error| error.kind.clone())
                .unwrap_or_else(|| AiError::kind_from_status(call.status, Some(&call.raw)));
            call.attempt.finish(
                "failed",
                Some(call.status),
                Some("upstream_error".into()),
                None,
            );
            return Err(AttemptFailure::upstream(
                kind,
                Some(call.status),
                "upstream_error",
                format!("upstream returned HTTP {}", call.status),
                retry_after(&call.headers),
            )
            .with_upstream_body(
                native_compaction_requested,
                Some(call.status),
                Some(call.raw),
            ));
        }
        let mut response = match call.canonical {
            Ok(response) => response,
            Err(error) => {
                call.attempt.finish(
                    "failed",
                    Some(call.status),
                    Some(error.stable_code().to_owned()),
                    None,
                );
                return Err(AttemptFailure::terminal(
                    error.stable_code(),
                    error.to_string(),
                ));
            }
        };
        crate::media::ingest::normalize_response(
            gateway,
            &input.principal,
            &mut response,
            &input.cancellation,
        )
        .await
        .map_err(|error| {
            AttemptFailure::terminal("output_media_ingest_failed", error.to_string())
        })?;
        gateway.cache_affinity.record_success(
            &input.principal,
            &route.id,
            &input.request,
            &prepared.route.target_id,
            &response.usage,
        );
        record_success(gateway, target, &route_policy_state, &attempt_context);
        call.attempt.confirm_usage(&response.usage);
        call.attempt
            .checkpoint("canonical_terminal_response", &response);
        let canonical_deltas = ai_response_to_deltas(&response);
        for delta in &canonical_deltas {
            call.attempt.observe_delta(delta);
            call.attempt.checkpoint("canonical_delta", delta);
        }
        call.attempt.finish(
            "completed",
            Some(call.status),
            None,
            Some(attempt_started.elapsed().as_millis() as i64),
        );
        let mut events = canonical_deltas
            .into_iter()
            .map(CanonicalEvent::Delta)
            .map(Ok)
            .collect::<Vec<_>>();
        events.push(Ok(CanonicalEvent::Completed(Box::new(response))));
        return Ok(ModelTurn {
            model_turn_id: prepared.model_turn_id,
            route: prepared.route,
            target: target_identity,
            output: Box::pin(stream::iter(events)),
            reasoning_encrypted_content_requested: prepared.reasoning_encrypted_content_requested,
            streamed: false,
        });
    }

    let stream_started = Instant::now();
    let response = prepared
        .provider_call
        .call_stream()
        .await
        .map_err(|error| AttemptFailure::retryable("upstream_error", error.to_string()))?;
    let mut provider_stream = match response {
        ProviderStreamResponse::Stream(stream) => stream,
        ProviderStreamResponse::Error {
            status,
            headers,
            body,
            attempt,
        } => {
            let kind = AiError::kind_from_status(status, body.as_ref().ok());
            let retry_after = retry_after(&headers);
            attempt.finish("failed", Some(status), Some("upstream_error".into()), None);
            return Err(AttemptFailure::upstream(
                kind,
                Some(status),
                "upstream_error",
                format!("upstream returned HTTP {status}"),
                retry_after,
            )
            .with_upstream_body(native_compaction_requested, Some(status), body.ok()));
        }
        ProviderStreamResponse::Uncertain { message } => {
            return Err(AttemptFailure::terminal(
                "upstream_acceptance_unknown",
                message,
            ));
        }
    };
    target_identity.response_continuation_available =
        provider_stream.response_continuation_available();
    debug_assert!(provider_stream.status < 400);

    let mut first_deltas = Vec::new();
    let mut first_token_ms = None;
    loop {
        match provider_stream.next().await {
            Ok(Some(chunk)) => {
                let ready = chunk
                    .deltas
                    .iter()
                    .any(|delta| is_first_output(delta) || is_terminal_delta(delta));
                first_deltas.extend(chunk.deltas);
                if ready {
                    first_token_ms = Some(stream_started.elapsed().as_millis() as i64);
                    break;
                }
            }
            Ok(None) => {
                match provider_stream.finish().await {
                    Ok(deltas) => first_deltas.extend(deltas),
                    Err(error) => {
                        let failure = stream_failure(error);
                        provider_stream.attempt().finish(
                            "failed",
                            None,
                            Some(failure.error.code.clone()),
                            None,
                        );
                        return Err(failure);
                    }
                }
                break;
            }
            Err(error) => {
                let failure = stream_failure(error);
                provider_stream.attempt().finish(
                    "failed",
                    None,
                    Some(failure.error.code.clone()),
                    None,
                );
                return Err(failure);
            }
        }
    }
    if let Some(error) = first_deltas.iter().find_map(|delta| match delta {
        AiStreamDelta::StreamError { error } => Some(error),
        _ => None,
    }) {
        // This batch is rejected before send_deltas, but its readable thinking was received.
        for delta in &first_deltas {
            provider_stream.attempt().observe_delta(delta);
        }
        provider_stream.attempt().finish(
            "failed",
            error.status_code,
            Some("upstream_stream_error".into()),
            first_token_ms,
        );
        let mut failure = AttemptFailure::upstream(
            error.kind.clone(),
            error.status_code,
            "upstream_stream_error",
            if native_compaction_requested {
                error.message.clone()
            } else {
                "upstream stream error".into()
            },
            None,
        )
        .with_upstream_body(
            native_compaction_requested,
            error.status_code,
            error.raw.clone(),
        );
        failure.protected_reasoning_rejected &= !first_deltas.iter().any(is_first_output);
        return Err(failure);
    }

    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let principal = input.principal.clone();
    let request = input.request.clone();
    let route_id = route.id.clone();
    let route_policy_state = route_policy_state.clone();
    let attempt_context = attempt_context.clone();
    let target_key = prepared.route.target_id.clone();
    let health_target_key = selected_target_key(&target);
    let reservation =
        route_policy_state.reservation(attempt_context.clone(), health_target_key.clone());
    let gateway = gateway.clone();
    let target = target.clone();
    let cancellation = input.cancellation.clone();
    let deadline = input.deadline;
    tokio::spawn(async move {
        let mut accumulator = StreamResponseAccumulator::default();
        let terminal_error = handle_terminal_stream_error(
            &gateway.health_registry,
            &health_target_key,
            &first_deltas,
        );
        if send_deltas(
            &tx,
            &mut accumulator,
            provider_stream.attempt(),
            first_deltas,
        )
        .await
        .is_err()
        {
            provider_stream.attempt().finish(
                "interrupted",
                Some(provider_stream.status),
                Some("consumer_disconnected".into()),
                None,
            );
            return;
        }
        if terminal_error {
            provider_stream.attempt().finish(
                "failed",
                Some(provider_stream.status),
                Some("upstream_stream_error".into()),
                first_token_ms,
            );
            return;
        }
        loop {
            let next = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    let error = interruption_error(deadline);
                    let outcome = if error.code == "cancelled" { "cancelled" } else { "failed" };
                    provider_stream.attempt().finish(outcome, None, Some(error.code.clone()), None);
                    let _ = tx.send(Err(error)).await;
                    return;
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    provider_stream.attempt().finish("failed", None, Some("deadline_exceeded".into()), None);
                    let _ = tx.send(Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded"))).await;
                    return;
                }
                chunk = provider_stream.next() => chunk,
            };
            match next {
                Ok(Some(chunk)) => {
                    let terminal_error = handle_terminal_stream_error(
                        &gateway.health_registry,
                        &health_target_key,
                        &chunk.deltas,
                    );
                    if send_deltas(
                        &tx,
                        &mut accumulator,
                        provider_stream.attempt(),
                        chunk.deltas,
                    )
                    .await
                    .is_err()
                    {
                        provider_stream.attempt().finish(
                            "interrupted",
                            Some(provider_stream.status),
                            Some("consumer_disconnected".into()),
                            None,
                        );
                        return;
                    }
                    if terminal_error {
                        provider_stream.attempt().finish(
                            "failed",
                            Some(provider_stream.status),
                            Some("upstream_stream_error".into()),
                            None,
                        );
                        return;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    gateway
                        .health_registry
                        .record_failure(&selected_target_key(&target));
                    let failure = stream_failure(error);
                    provider_stream.attempt().finish(
                        "failed",
                        None,
                        Some(failure.error.code.clone()),
                        None,
                    );
                    let _ = tx.send(Err(failure.error)).await;
                    return;
                }
            }
        }
        match provider_stream.finish().await {
            Ok(deltas) => {
                let terminal_error = handle_terminal_stream_error(
                    &gateway.health_registry,
                    &health_target_key,
                    &deltas,
                );
                if send_deltas(&tx, &mut accumulator, provider_stream.attempt(), deltas)
                    .await
                    .is_err()
                {
                    provider_stream.attempt().finish(
                        "interrupted",
                        Some(provider_stream.status),
                        Some("consumer_disconnected".into()),
                        None,
                    );
                    return;
                }
                if terminal_error {
                    provider_stream.attempt().finish(
                        "failed",
                        Some(provider_stream.status),
                        Some("upstream_stream_error".into()),
                        None,
                    );
                    return;
                }
            }
            Err(error) => {
                let failure = stream_failure(error);
                provider_stream.attempt().finish(
                    "failed",
                    None,
                    Some(failure.error.code.clone()),
                    None,
                );
                let _ = tx.send(Err(failure.error)).await;
                return;
            }
        }
        let mut response = accumulator.into_ai_response();
        if let Err(error) = crate::media::ingest::normalize_response(
            &gateway,
            &principal,
            &mut response,
            &cancellation,
        )
        .await
        {
            provider_stream.attempt().finish(
                "failed",
                Some(provider_stream.status),
                Some("output_media_ingest_failed".into()),
                first_token_ms,
            );
            let _ = tx
                .send(Err(ModelTurnError::new(
                    "output_media_ingest_failed",
                    error.to_string(),
                )))
                .await;
            return;
        }
        gateway.cache_affinity.record_success(
            &principal,
            &route_id,
            &request,
            &target_key,
            &response.usage,
        );
        record_success(&gateway, &target, &route_policy_state, &attempt_context);
        reservation.complete();
        provider_stream.attempt().confirm_usage(&response.usage);
        provider_stream
            .attempt()
            .checkpoint("canonical_terminal_response", &response);
        provider_stream.attempt().finish(
            "completed",
            Some(provider_stream.status),
            None,
            first_token_ms,
        );
        let _ = tx
            .send(Ok(CanonicalEvent::Completed(Box::new(response))))
            .await;
    });

    Ok(ModelTurn {
        model_turn_id: prepared.model_turn_id,
        route: prepared.route,
        target: target_identity,
        output: Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
        reasoning_encrypted_content_requested: prepared.reasoning_encrypted_content_requested,
        streamed: true,
    })
}

async fn send_deltas(
    tx: &tokio::sync::mpsc::Sender<Result<CanonicalEvent, ModelTurnError>>,
    accumulator: &mut StreamResponseAccumulator,
    attempt: &AttemptObservation,
    deltas: Vec<AiStreamDelta>,
) -> Result<(), ()> {
    accumulator.apply_all(&deltas);
    // Observe received content even when delivery stops partway through this batch.
    for delta in &deltas {
        attempt.observe_delta(delta);
    }
    for delta in deltas {
        attempt.checkpoint("canonical_delta", &delta);
        tx.send(Ok(CanonicalEvent::Delta(delta)))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
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

fn handle_terminal_stream_error(
    health: &crate::router::health::HealthRegistry,
    target_key: &str,
    deltas: &[AiStreamDelta],
) -> bool {
    let Some(error) = deltas.iter().find_map(|delta| match delta {
        AiStreamDelta::StreamError { error } => Some(error),
        _ => None,
    }) else {
        return false;
    };
    let degrades_health = error.status_code.map_or_else(
        || error.is_retryable(),
        |status| {
            matches!(
                AiError::kind_from_status(status, None),
                stravia_runtime_contract::protocol::ir::AiErrorKind::RateLimitError
                    | stravia_runtime_contract::protocol::ir::AiErrorKind::QuotaExceeded
                    | stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError
                    | stravia_runtime_contract::protocol::ir::AiErrorKind::ServiceUnavailable
                    | stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout
                    | stravia_runtime_contract::protocol::ir::AiErrorKind::ModelNotAvailable
            )
        },
    );
    if degrades_health {
        health.record_failure(target_key);
    }
    true
}

fn stream_failure(error: ProviderStreamError) -> AttemptFailure {
    match error {
        ProviderStreamError::Transport(message) => {
            AttemptFailure::retryable("upstream_stream_error", message)
        }
        ProviderStreamError::Uncertain(message) => {
            AttemptFailure::terminal("upstream_acceptance_unknown", message)
        }
        ProviderStreamError::Decode(error) => {
            AttemptFailure::terminal("protocol_lossy_rejected", error.to_string())
        }
        ProviderStreamError::Normalize(error) => {
            AttemptFailure::terminal(error.stable_code(), error.to_string())
        }
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (deadline.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .ok()
}

fn target_namespace(
    provider: &crate::db::models::Provider,
    runtime: &crate::admin::ResolvedProviderRuntime,
    adapter: &ProviderAdapter,
    target_key: &str,
    actual_model: &str,
) -> String {
    let account_identity = runtime
        .binding
        .extra_headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
        .map(|(_, value)| value.as_str())
        .unwrap_or(provider.id.as_str());
    let credential_identity = if provider.auth_mode.eq_ignore_ascii_case("oauth") {
        account_identity.to_owned()
    } else {
        namespace_fingerprint(&runtime.access_token)
    };
    let stable_headers = runtime
        .binding
        .extra_headers
        .iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !matches!(
                name.as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "cookie"
            ) && !name.contains("token")
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let stable_model_aliases = runtime
        .binding
        .model_aliases
        .iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut stable_models = runtime.binding.static_models_override.clone();
    if let Some(models) = &mut stable_models {
        models.sort();
    }
    namespace_fingerprint(&(
        target_key,
        provider.id.as_str(),
        provider.vendor.as_deref().unwrap_or("custom"),
        provider.channel.as_deref().unwrap_or("default"),
        provider.protocol.as_str(),
        provider.use_proxy,
        adapter.binding().egress_base_url.as_str(),
        actual_model,
        account_identity,
        credential_identity,
        (
            runtime.binding.base_url_override.as_deref(),
            stable_headers,
            stable_model_aliases,
            runtime.binding.models_source_override.as_deref(),
            runtime.binding.disable_default_auth,
            stable_models,
        ),
    ))
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

fn supports_modality(
    metadata: &crate::provider_models::ProviderModelMetadata,
    modality: &str,
) -> bool {
    metadata.modalities.as_ref().is_some_and(|modalities| {
        modalities
            .input
            .iter()
            .any(|value| value.eq_ignore_ascii_case(modality))
    })
}

fn interruption_error(deadline: Instant) -> ModelTurnError {
    if Instant::now() >= deadline {
        ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded")
    } else {
        ModelTurnError::new("cancelled", "Model Turn cancelled")
    }
}

fn model_turn_gateway_error(error: GatewayError) -> ModelTurnError {
    ModelTurnError::new(error.stable_code(), error.message())
}

fn normalize_provider_effective_request(
    request: &mut AiRequest,
    profile: &serde_json::Map<String, serde_json::Value>,
) {
    let Ok(effective) =
        crate::protocol::codec::open_responses::decoder::decode_effective_response_profile(
            &request.model,
            profile,
        )
    else {
        return;
    };
    request.generation = effective.generation;
    request.tools = effective.tools;
    request.tool_choice = effective.tool_choice;
    request.parallel_tool_calls = effective.parallel_tool_calls;
    request.disable_parallel_tool_calls = effective.disable_parallel_tool_calls;
    request.reasoning = effective.reasoning;
    request.response_format = effective.response_format;
    request.safety_settings = effective.safety_settings;
    request.ext = effective.ext;
}

fn record_success(
    gateway: &Gateway,
    target: &SelectedTarget,
    route_policy_state: &RoutePolicyState,
    attempt_context: &RouteAttemptContext,
) {
    let target_key = selected_target_key(target);
    gateway.health_registry.record_success(&target_key);
    route_policy_state.record_success(attempt_context, &target_key);
}
