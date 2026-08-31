//! Dispatcher: single orchestration point that drives a request through the
//! full proxy pipeline.
//!
//! `orchestrate` owns the internal Inference Run lifecycle. Ingress thin shells
//! call `dispatcher::dispatch_pipeline`, which submits normalized `RunInput`.
//!
//! Pipeline:
//!   1. Authenticate the caller and begin Generation Chain Write.
//!   2. Run Request hooks before resolving and authorizing the final model.
//!   3. Execute one shared Model Turn after hooks stabilize the effective request.
//!   4. Run response/tool/client-output hooks and deliver the committed result.

mod claim;
mod completion;
mod delivery;
mod errors;
mod log;
mod stream;
mod util;
use self::claim::*;
use self::completion::*;
use self::delivery::{DeliveryAdapter, DeliveryProgress};
pub(super) use self::errors::hook_failure_response;
use self::errors::*;
use self::log::*;
use self::util::{client_session_id, forwarded_client_headers};
use super::{Phase, PhaseTracker, RunInput};
use std::sync::Arc;
use std::time::Instant;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::Gateway;
use crate::agent::{CanonicalEvent, ModelTurnExecutor, TurnInput};
#[cfg(test)]
use crate::db::models::Provider;
use crate::error::{AccessDenial, AuthFailure, GatewayError};
use crate::model_turn::StreamResponseAccumulator;
#[cfg(test)]
use crate::protocol::ids::Protocol;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::Usage;
use crate::protocol::ir::request::MediaRoutingMode;
use crate::protocol::ir::{AiRequest, AiResponse, RawEnvelope};
#[cfg(test)]
use crate::provider::VendorRegistry;
#[cfg(test)]
use crate::provider::vendor::Vendor;
use crate::proxy::context::RequestContext;
use crate::proxy::observability::{LogExtras, send_log};
use crate::proxy::security::{ClientCredential, Security};

#[cfg(test)]
fn resolve_vendor_adapter(provider: &Provider, protocol: Protocol) -> Option<Arc<dyn Vendor>> {
    let registry = VendorRegistry::global();
    let vendor_id = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|vendor| !vendor.is_empty());

    if vendor_id.is_none() && protocol == Protocol::OpenResponses {
        return registry
            .get_vendor(crate::provider::registry::protocol_default_vendor(protocol))
            .cloned();
    }

    registry
        .get_vendor(vendor_id.unwrap_or("custom"))
        .cloned()
        .or_else(|| {
            registry
                .get_vendor(crate::provider::registry::protocol_default_vendor(protocol))
                .cloned()
        })
}

#[cfg(test)]
fn is_openai_generation_target(
    vendor: Option<&str>,
    preset_key: Option<&str>,
    _ingress: ProtocolId,
    is_embedding_request: bool,
) -> bool {
    if is_embedding_request {
        return false;
    }

    vendor
        .map(str::trim)
        .filter(|vendor| !vendor.is_empty())
        .is_some_and(|vendor| vendor.eq_ignore_ascii_case("openai"))
        && preset_key.map(str::trim).is_none_or(|preset_key| {
            preset_key.is_empty() || preset_key.eq_ignore_ascii_case("openai")
        })
}

pub(super) enum RoundOutcome {
    Deliver {
        response: Response,
        delivery: DeliveryState,
    },
    NextRound {
        run: Option<Box<crate::hook::InferenceRun>>,
        phase: Option<PhaseTracker>,
    },
}

impl RoundOutcome {
    fn with_lifecycle(self, run: crate::hook::InferenceRun, phase: PhaseTracker) -> Self {
        match self {
            Self::NextRound { .. } => Self::NextRound {
                run: Some(Box::new(run)),
                phase: Some(phase),
            },
            other => other,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryState {
    Buffered,
    Live,
}

pub(super) fn buffered_response(response: Response) -> RoundOutcome {
    RoundOutcome::Deliver {
        response,
        delivery: DeliveryState::Buffered,
    }
}

pub(super) fn buffered_completion(response: Response) -> RoundOutcome {
    RoundOutcome::Deliver {
        response,
        delivery: DeliveryState::Buffered,
    }
}

pub(super) fn live_response(response: Response) -> RoundOutcome {
    RoundOutcome::Deliver {
        response,
        delivery: DeliveryState::Live,
    }
}

fn enter_phase(phase: &mut PhaseTracker, next: Phase) -> Result<(), Box<Response>> {
    phase
        .transition(next)
        .map_err(|error| Box::new(hook_failure_response(error)))
}

/// Materialized Generation Chain state owned by the Inference Run while the
/// Model Turn Executor prepares only the selected target's continuation.
#[derive(Clone)]
pub(super) struct GenerationChainRun {
    principal: crate::hook::Principal,
    write: Option<crate::generation_chain::GenerationChainWrite>,
    client_request: AiRequest,
    previous_response_id: Option<String>,
}

struct DispatchContext<'a> {
    gw: Gateway,
    executor: Arc<dyn ModelTurnExecutor>,
    headers: HeaderMap,
    envelope: RawEnvelope,
    request: &'a mut AiRequest,
    ingress: ProtocolId,
    ctx: &'a mut RequestContext,
    inference_run: &'a mut Option<crate::hook::InferenceRun>,
    phase: &'a mut PhaseTracker,
    generation: &'a mut GenerationChainRun,
}

struct SharedModelTurnInput<'a> {
    executor: Arc<dyn ModelTurnExecutor>,
    gateway: &'a Gateway,
    request: &'a mut AiRequest,
    ingress: ProtocolId,
    request_context: &'a RequestContext,
    inference_run: &'a mut Option<crate::hook::InferenceRun>,
    phase: &'a mut PhaseTracker,
    generation: GenerationChainRun,
    start: Instant,
    request_extras: &'a RequestExtras,
    headers: &'a HeaderMap,
}

fn stabilize_media_generation_chain(
    generation: &GenerationChainRun,
    rewritten: &AiRequest,
) -> bool {
    let Some(plan) = rewritten.meta.media_routing.as_ref() else {
        return true;
    };
    if plan.mode != MediaRoutingMode::Bridge {
        return true;
    }
    let client_delta = generation
        .write
        .as_ref()
        .map_or(&generation.client_request, |write| write.request_delta());
    let image_count = client_delta
        .items
        .iter()
        .filter_map(|message| match &message.content {
            crate::protocol::ir::MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .filter(|block| matches!(block, crate::protocol::ir::ContentBlock::Image { .. }))
        .count();
    if image_count == 0 {
        return true;
    }
    if image_count != plan.source_artifact_ids.len() {
        return false;
    }
    let markers = plan
        .source_artifact_ids
        .iter()
        .filter_map(|source_id| {
            let identity = format!("artifact_id=\"{source_id}\"");
            rewritten
                .items
                .iter()
                .filter_map(|message| match &message.content {
                    crate::protocol::ir::MessageContent::Blocks(blocks) => Some(blocks),
                    _ => None,
                })
                .flatten()
                .find(|block| {
                    matches!(
                        block,
                        crate::protocol::ir::ContentBlock::Text { text, .. }
                            if text.starts_with("[stravia_media ") && text.contains(&identity)
                    )
                })
                .cloned()
        })
        .collect::<Vec<_>>();
    if markers.len() != image_count {
        return false;
    }
    markers.len() == image_count
}

pub(super) async fn orchestrate(
    input: RunInput,
    inference_run: &mut Option<crate::hook::InferenceRun>,
    phase: &mut PhaseTracker,
) -> Response {
    let RunInput {
        gateway: gw,
        executor,
        headers,
        envelope,
        request,
        ingress,
        context: mut ctx,
    } = input;
    let mut request = request;
    if let Some(session_id) = client_session_id(&headers, &request) {
        crate::generation_chain::set_generation_session_id(&mut request, session_id);
    }
    let client_request = request.clone();
    let ingress_capabilities = crate::protocol::registry::ProtocolRegistry::global()
        .capabilities(&ingress)
        .expect("registered ingress protocol");
    let request_kind = if ingress_capabilities.embeddings {
        crate::hook::RequestKind::Embeddings
    } else {
        crate::hook::RequestKind::Generation
    };
    if let Some(crate::protocol::ir::ProtocolExt::OpenResponses(extension)) = request.ext.as_ref()
        && extension.background == Some(true)
    {
        return parameter_error_response(
            StatusCode::BAD_REQUEST,
            "unsupported_feature",
            "background",
            "Responses background mode is not supported.",
        );
    }
    let previous_response_id = match request.ext.as_ref() {
        Some(crate::protocol::ir::ProtocolExt::OpenResponses(extension)) => {
            extension.previous_response_id.clone()
        }
        _ => None,
    };
    let credential = ClientCredential::from_inference_headers(&headers);
    let authenticated_principal = match Security::new(gw.storage.auth())
        .authenticated_principal(&credential)
        .await
    {
        Ok(principal) => principal,
        Err(error) => return inference_access_error_response(error),
    };
    let concurrency_limit = authenticated_principal.concurrency_limit;
    let api_key_name = authenticated_principal.api_key_name;
    let principal = authenticated_principal.principal;
    ctx.auth_subject = Some(crate::proxy::context::AuthSubject {
        api_key_id: Some(principal.api_key_id().to_owned()),
        label: Some(api_key_name),
    });
    let generation_chain_write = if matches!(request_kind, crate::hook::RequestKind::Generation) {
        match gw.generation_chains.begin(principal.clone(), request).await {
            Ok(write) => {
                #[cfg(debug_assertions)]
                if let Some(capture) = &gw.wire_capture {
                    capture.bind_chain(&ctx.request_id, write.root_id());
                }
                request = write.request().clone();
                Some(write)
            }
            Err(error) => {
                let code = error.to_string();
                return coded_error_response(StatusCode::BAD_REQUEST, &code, &code);
            }
        }
    } else {
        None
    };
    let execution_window = ctx.deadline.remaining();
    let marker_resolution = match crate::history_marker::resolve_request_markers(
        gw.history_markers.as_ref(),
        &principal,
        &mut request,
    )
    .await
    {
        Ok(resolution) => resolution,
        Err(error) => {
            return coded_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_marker_unavailable",
                &error.to_string(),
            );
        }
    };
    if marker_resolution.restored_thinking_segments > 0 {
        request.meta.vendor.ingress.insert(
            "__stravia_opaque_context_required".into(),
            serde_json::Value::Bool(true),
        );
    }
    ctx.deadline = crate::proxy::context::Deadline::from_now(execution_window);
    let admission = if marker_resolution.restored_platform_segments > 0 {
        tokio::select! {
            admission = gw.principal_admission.acquire_wait(&principal, concurrency_limit) => admission,
            _ = ctx.cancellation.cancelled() => {
                return error_response(499, "request cancelled");
            }
        }
    } else {
        gw.principal_admission
            .acquire(&principal, concurrency_limit)
    };
    let admission = match admission {
        Ok(admission) => admission,
        Err(error) => return inference_access_error_response(error),
    };
    let inherited_media_turns = generation_chain_write
        .as_ref()
        .map(|write| write.inherited_media_turns().to_vec())
        .unwrap_or_default();
    let mut generation = GenerationChainRun {
        principal: principal.clone(),
        write: generation_chain_write,
        client_request,
        previous_response_id: previous_response_id.clone(),
    };
    let session_context = crate::hook::SessionContext {
        request_id: ctx.request_id.clone(),
        run_id: format!("run-{}", uuid::Uuid::new_v4()),
        request_kind,
        ingress,
        transport: crate::hook::TransportKind::Http,
        inherited_media_turns,
        principal,
        cancellation: ctx.cancellation.clone(),
        response_id: generation.write.as_ref().map(|write| write.id().to_owned()),
        previous_response_id,
    };
    *inference_run = Some(
        match gw.hook_runtime().begin(
            session_context,
            &request,
            crate::hook::ContextCompleteness::from_request(&request),
        ) {
            Ok(run) => run,
            Err(error) => return hook_failure_response(error),
        },
    );
    let response = dispatch_pipeline_inner(DispatchContext {
        gw: gw.clone(),
        executor,
        headers,
        envelope,
        request: &mut request,
        ingress,
        ctx: &mut ctx,
        inference_run: &mut *inference_run,
        phase: &mut *phase,
        generation: &mut generation,
    })
    .await;
    phase.finish();
    if response.status().is_success() {
        let (delivery_admission, background_admission) = split_admission(admission);
        let references = ctx
            .extensions
            .get::<PublishedPlatformExecutions>()
            .unwrap_or_default()
            .references;
        if references.is_empty() {
            drop(background_admission);
        } else {
            let store = Arc::clone(&gw.history_markers);
            let principal = generation.principal.clone();
            gw.lifecycle.spawn(async move {
                for reference in references {
                    let _ = store.wait_terminal(&principal, &reference).await;
                }
                drop(background_admission);
            });
        }
        wrap_delivery(response, delivery_admission)
    } else {
        response
    }
}

async fn dispatch_pipeline_inner(context: DispatchContext<'_>) -> Response {
    let DispatchContext {
        gw,
        executor,
        headers,
        envelope,
        request,
        ingress,
        ctx,
        inference_run,
        phase,
        generation,
    } = context;
    let start = Instant::now();
    let mut delivery = DeliveryState::Buffered;
    let response = Box::pin(dispatch_round(
        DispatchContext {
            gw: gw.clone(),
            executor,
            headers,
            envelope,
            request: &mut *request,
            ingress,
            ctx: &mut *ctx,
            inference_run: &mut *inference_run,
            phase: &mut *phase,
            generation: &mut *generation,
        },
        start,
        &mut delivery,
    ))
    .await;
    if delivery == DeliveryState::Buffered
        && let Some(inference_run) = inference_run.as_mut()
        && let Err(error) = inference_run.flush_stream()
    {
        return hook_failure_response(error);
    }
    response
}
async fn dispatch_round(
    context: DispatchContext<'_>,
    start: Instant,
    delivery_state: &mut DeliveryState,
) -> Response {
    let DispatchContext {
        gw,
        executor,
        headers,
        envelope,
        request,
        ingress,
        ctx,
        inference_run,
        phase,
        generation: generation_chain,
    } = context;
    let mut fixed_media_plan = request.meta.media_routing.clone();
    'round: loop {
        if request.meta.media_routing.is_none() {
            request.meta.media_routing = fixed_media_plan.clone();
        }
        // Derive logging strings from envelope.
        let method_owned = envelope.method.clone();
        let path_owned = envelope.path.clone();
        let request_body_str = (!crate::media::contains_images(request)
            && fixed_media_plan.is_none()
            && request.meta.media_routing.is_none())
        .then(|| {
            envelope
                .body
                .as_ref()
                .and_then(|body| serde_json::to_string(body).ok())
        })
        .flatten();
        let request_headers_str =
            crate::proxy::observability::header_map_to_redacted_json(&envelope.headers);
        // Built early so it can be used by both pre-loop log entries and the per-target handlers.
        let req_extras = RequestExtras {
            method: method_owned.clone(),
            path: path_owned.clone(),
            headers: request_headers_str.clone(),
            body: request_body_str.clone(),
        };

        // Request hooks run before the route is selected so a hook may change the
        // model or synthesize a response. Authorization is applied to the resulting
        // model below.
        let request_hook_pending = match phase.current() {
            Phase::Request => true,
            Phase::HiddenRound => true,
            current => {
                return hook_failure_response(format!(
                    "Inference Run entered request handling in {current:?}"
                ));
            }
        };
        if ctx.cancellation.is_cancelled() {
            return error_response(499, "request cancelled");
        }
        if request_hook_pending {
            let request_hook_result = inference_run
                .as_mut()
                .expect("buffered Inference Run")
                .on_request(request)
                .await;
            match request_hook_result {
                Ok(crate::hook::HookControl::Continue) => {}
                Ok(crate::hook::HookControl::Respond(response)) => {
                    let mut response = *response;
                    let run = inference_run.as_mut().expect("buffered Inference Run");
                    run.set_route(crate::hook::RouteContext {
                        model_id: request.model.clone(),
                        provider_id: "hook".into(),
                        target_id: "hook".into(),
                        egress: ingress,
                    });
                    match run.on_client_output(&mut response).await {
                        Ok(crate::hook::HookControl::Continue) => {}
                        Ok(crate::hook::HookControl::Respond(replacement)) => {
                            response = *replacement;
                        }
                        Ok(control) => {
                            return render_hook_control(control, ingress, request.stream.enabled);
                        }
                        Err(error) => return hook_failure_response(error),
                    }
                    if ingress == crate::protocol::ids::OPEN_RESPONSES_2026_04_24
                        && let Some(write) = generation_chain.write.as_ref()
                    {
                        response.id = write.id().to_owned();
                    }
                    let pending_generation_chain =
                        generation_chain.write.take().and_then(|mut write| {
                            write.observe_effective(request.clone());
                            crate::generation_chain::mark_generation_target(
                                &mut response,
                                "hook",
                                ingress,
                                &request.model,
                            );
                            write.stage(&mut response, None).then_some(write)
                        });
                    LogBuilder::from_dispatch(
                        &gw,
                        &ingress.to_string(),
                        &request.model,
                        request.reasoning.level,
                        ctx.auth_subject.as_ref(),
                        start,
                    )
                    .stream_flag(request.stream.enabled)
                    .status(200)
                    .with_req_extras(&req_extras)
                    .emit();
                    if let Err(response) = enter_phase(phase, Phase::SemanticComplete) {
                        return *response;
                    }
                    if let Err(response) = enter_phase(phase, Phase::AwaitingDelivery) {
                        return *response;
                    }
                    let response = render_hook_control(
                        crate::hook::HookControl::Respond(Box::new(response)),
                        ingress,
                        request.stream.enabled,
                    );
                    return if let Some(pending) = pending_generation_chain {
                        DeliveryAdapter::commit_response_after_delivery(response, pending)
                    } else {
                        response
                    };
                }
                Ok(control) => {
                    return render_hook_control(control, ingress, request.stream.enabled);
                }
                Err(error) => return hook_failure_response(error),
            }
        }
        match (&fixed_media_plan, request.meta.media_routing.clone()) {
            (None, Some(plan)) => fixed_media_plan = Some(plan),
            (Some(plan), _) => request.meta.media_routing = Some(plan.clone()),
            (None, None) => {}
        }
        if request
            .meta
            .media_routing
            .as_ref()
            .is_some_and(|plan| plan.mode == MediaRoutingMode::Bridge)
            && !stabilize_media_generation_chain(&generation_chain, request)
        {
            return coded_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "media_response_chain_invalid",
                "Media bridge could not prepare the request",
            );
        }
        if let Some(write) = generation_chain.write.as_mut() {
            write.observe_effective(request.clone());
        }
        if let Err(response) = enter_phase(phase, Phase::Selecting) {
            return *response;
        }

        let outcome = execute_shared_model_turn(SharedModelTurnInput {
            executor: Arc::clone(&executor),
            gateway: &gw,
            request,
            ingress,
            request_context: ctx,
            inference_run,
            phase,
            generation: generation_chain.clone(),
            start,
            request_extras: &req_extras,
            headers: &headers,
        })
        .await;
        match outcome {
            RoundOutcome::NextRound {
                run,
                phase: next_phase,
            } => {
                if let Some(run) = run {
                    *inference_run = Some(*run);
                }
                if let Some(next_phase) = next_phase {
                    *phase = next_phase;
                }
                continue 'round;
            }
            RoundOutcome::Deliver { response, delivery } => {
                *delivery_state = delivery;
                return response;
            }
        }
    }
}

async fn execute_shared_model_turn(input: SharedModelTurnInput<'_>) -> RoundOutcome {
    let SharedModelTurnInput {
        executor,
        gateway,
        request,
        ingress,
        request_context,
        inference_run,
        phase,
        generation,
        start,
        request_extras,
        headers,
    } = input;
    let make_input = |effective_request: AiRequest| {
        let input = TurnInput::new(generation.principal.clone(), effective_request).with_execution(
            request_context.cancellation.clone(),
            request_context.deadline.at(),
        );
        #[cfg(debug_assertions)]
        let input = input.with_wire_capture_id(request_context.request_id.clone());
        input.with_extra_headers(forwarded_client_headers(headers))
    };

    let mut effective_request = request.clone();
    let turn_started = Instant::now();
    let turn = match executor
        .execute(make_input(effective_request.clone()))
        .await
    {
        Ok(turn) => turn,
        Err(error)
            if error.code == "tools_unsupported"
                && !crate::web_search::native_web_search_requested(&effective_request) =>
        {
            let original_tools = effective_request.tools.clone();
            inference_run
                .as_mut()
                .expect("buffered Inference Run")
                .remove_exposed_tools(&mut effective_request);
            if effective_request.tools == original_tools {
                return model_turn_execute_failure(
                    gateway,
                    request,
                    ingress,
                    start,
                    request_extras,
                    request_context.auth_subject.as_ref(),
                    error,
                );
            }
            match executor
                .execute(make_input(effective_request.clone()))
                .await
            {
                Ok(turn) => turn,
                Err(error) => {
                    return model_turn_execute_failure(
                        gateway,
                        request,
                        ingress,
                        start,
                        request_extras,
                        request_context.auth_subject.as_ref(),
                        error,
                    );
                }
            }
        }
        Err(error) => {
            return model_turn_execute_failure(
                gateway,
                request,
                ingress,
                start,
                request_extras,
                request_context.auth_subject.as_ref(),
                error,
            );
        }
    };
    *request = effective_request;
    inference_run
        .as_mut()
        .expect("Inference Run before Model Turn output")
        .set_route(turn.route.clone());
    if let Err(response) = enter_phase(phase, Phase::Calling) {
        return buffered_response(*response);
    }

    if request.stream.enabled {
        let log = LogBuilder::from_dispatch(
            gateway,
            &ingress.to_string(),
            &request.model,
            request.reasoning.level,
            request_context.auth_subject.as_ref(),
            start,
        )
        .stream_flag(true)
        .model_turn(&turn.route, &turn.target)
        .with_req_extras(request_extras);
        return stream::handle_model_turn_stream(stream::ModelTurnStreamInput {
            turn,
            executor: Arc::clone(&executor),
            gateway: gateway.clone(),
            headers: headers.clone(),
            ingress,
            request_context: request_context.clone(),
            request: request.clone(),
            generation,
            inference_run: inference_run.take().expect("live Inference Run"),
            phase: std::mem::replace(phase, PhaseTracker::at(Phase::Finished)),
            start,
            turn_started,
            request_extras: request_extras.clone(),
            log,
        })
        .await;
    }

    let route = turn.route.clone();
    let streamed = turn.streamed;
    let attempt_trace = turn.transport.clone();
    let completion_context = CompletionContext::from_model_turn(
        gateway.clone(),
        generation,
        ingress,
        &turn.target,
        turn.route.egress,
    );
    let mut output = turn.output;
    let mut completed_response = None;
    let mut streamed_response = streamed.then(StreamResponseAccumulator::default);
    if streamed {
        let mut terminal_deltas = Vec::new();
        let mut hook_leg = stream::HookLegGuard::new(
            inference_run
                .as_mut()
                .expect("buffered Inference Run stream"),
        );
        while let Some(event) = output.next().await {
            match event {
                Ok(CanonicalEvent::Delta(delta)) => {
                    let (terminal, deltas) = stream::partition_terminal_deltas(vec![delta]);
                    terminal_deltas.extend(terminal);
                    let transformed =
                        match stream::transform_stream_deltas(hook_leg.run_mut(), deltas) {
                            Ok(deltas) => deltas,
                            Err(error) => {
                                return buffered_response(hook_failure_response(error));
                            }
                        };
                    streamed_response
                        .as_mut()
                        .expect("stream accumulator")
                        .apply_all(&transformed);
                }
                Ok(CanonicalEvent::Completed(completed)) => {
                    completed_response = Some(*completed);
                }
                Err(error) => return model_turn_error_outcome(error),
            }
        }
        let flushed = match hook_leg.close().await {
            Ok(flushed) => flushed,
            Err(error) => return buffered_response(hook_failure_response(error)),
        };
        streamed_response
            .as_mut()
            .expect("stream accumulator")
            .apply_all(&flushed);
        streamed_response
            .as_mut()
            .expect("stream accumulator")
            .apply_all(&terminal_deltas);
    } else {
        while let Some(event) = output.next().await {
            match event {
                Ok(CanonicalEvent::Delta(_)) => {}
                Ok(CanonicalEvent::Completed(completed)) => {
                    completed_response = Some(*completed);
                }
                Err(error) => return model_turn_error_outcome(error),
            }
        }
    }
    let Some(completed_response) = completed_response else {
        return model_turn_error_outcome(crate::agent::ModelTurnError::new(
            "model_stream_incomplete",
            "Model Turn ended without a completion",
        ));
    };
    let mut response = streamed_response
        .map(StreamResponseAccumulator::into_ai_response)
        .unwrap_or_else(|| completed_response.clone());
    if response.usage.prompt_tokens == 0 && response.usage.completion_tokens == 0 {
        response.usage = completed_response.usage;
    }
    if response.id.is_empty() {
        response.id = completed_response.id;
    }
    if response.stop_reason.is_none() {
        response.stop_reason = completed_response.stop_reason;
    }
    let upstream_response_id = (!response.id.is_empty()).then(|| response.id.clone());
    let stream_metrics = attempt_trace.stream_metrics();
    let mut model_turn_log = Some(
        LogBuilder::from_dispatch(
            gateway,
            &ingress.to_string(),
            &request.model,
            request.reasoning.level,
            request_context.auth_subject.as_ref(),
            turn_started,
        )
        .stream_flag(request.stream.enabled)
        .model_turn(&route, &turn.target)
        .status(200)
        .usage(response.usage.clone())
        .with_req_extras(request_extras)
        .upstream_protocol(&route.egress.to_string())
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
            request.meta.media_routing.is_none().then(|| {
                String::from_utf8_lossy(&attempt_trace.response_body.lock().expect("response body"))
                    .into_owned()
            }),
            None,
        )
        .stream_metrics(stream_metrics.chunks_count, stream_metrics.first_chunk_ms)
        .model_turn_completed(turn_started),
    );
    let completed = match complete_canonical_response(
        &completion_context,
        CompletionInput {
            request_context,
            request,
            run: inference_run
                .as_mut()
                .expect("buffered Inference Run completion"),
            phase,
            response,
            upstream_response_id,
            early_platform_executions: Vec::new(),
            early_thinking_markers: Vec::new(),
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
            if let Err(failure) = continuation.publish(&completion_context).await {
                return buffered_response(render_completion_failure(
                    failure,
                    ingress,
                    request.stream.enabled,
                ));
            }
            if let Err(failure) = continuation
                .finish(
                    &completion_context,
                    request_context,
                    request,
                    inference_run
                        .as_mut()
                        .expect("Platform continuation Inference Run"),
                    phase,
                )
                .await
            {
                return buffered_response(render_completion_failure(
                    failure,
                    ingress,
                    request.stream.enabled,
                ));
            }
            return dispatch_next_round();
        }
        CompletionOutcome::Ready(lease) => match (*lease).prepare(phase) {
            Ok(delivery) => delivery,
            Err(failure) => {
                return buffered_response(render_completion_failure(
                    failure,
                    ingress,
                    request.stream.enabled,
                ));
            }
        },
        CompletionOutcome::Failed(failure) => {
            return buffered_response(render_completion_failure(
                failure,
                ingress,
                request.stream.enabled,
            ));
        }
    };
    if let Err(failure) = completed.publish(&completion_context).await {
        return buffered_response(render_completion_failure(
            failure,
            ingress,
            request.stream.enabled,
        ));
    }
    let mut started_executions = completed.started_executions;
    if !completed.background_executions.is_empty() {
        started_executions.extend(gateway.start_history_marker_executions(
            completion_context.principal().clone(),
            completed.background_executions,
        ));
    }
    if !started_executions.is_empty() {
        gateway.spawn_started_history_marker_executions(
            started_executions,
            inference_run
                .take()
                .expect("background Platform execution requires its Inference Run"),
        );
    }

    let mut delivery = if request.stream.enabled {
        DeliveryAdapter::buffered_stream(ingress, route.egress)
    } else {
        DeliveryAdapter::non_stream(ingress, route.egress)
    };
    let mut delivered = delivery.deliver_canonical(&completed.response, StatusCode::OK);
    let client_response_body = request
        .meta
        .media_routing
        .is_none()
        .then(|| delivered.body.clone());
    if let Some(pending) = completed.pending_generation_chain {
        delivered.response =
            DeliveryAdapter::commit_response_after_delivery(delivered.response, pending);
    }
    model_turn_log
        .take()
        .expect("current Model Turn log")
        .with_client_response(None, client_response_body)
        .emit();
    buffered_completion(delivered.response)
}

pub(super) async fn acquire_followup_model_turn(
    executor: &dyn ModelTurnExecutor,
    gateway: &Gateway,
    headers: &HeaderMap,
    request: &mut AiRequest,
    ingress: ProtocolId,
    request_context: &RequestContext,
    inference_run: &mut crate::hook::InferenceRun,
    phase: &mut PhaseTracker,
    principal: &crate::hook::Principal,
    start: Instant,
    request_extras: &RequestExtras,
) -> Result<(crate::agent::ModelTurn, Instant), RoundOutcome> {
    enter_phase(phase, Phase::Selecting).map_err(|response| buffered_response(*response))?;
    let make_input = |effective_request: AiRequest| {
        let input = TurnInput::new(principal.clone(), effective_request).with_execution(
            request_context.cancellation.clone(),
            request_context.deadline.at(),
        );
        #[cfg(debug_assertions)]
        let input = input.with_wire_capture_id(request_context.request_id.clone());
        input.with_extra_headers(forwarded_client_headers(headers))
    };
    let mut effective_request = request.clone();
    let turn_started = Instant::now();
    let turn = match executor
        .execute(make_input(effective_request.clone()))
        .await
    {
        Ok(turn) => turn,
        Err(error)
            if error.code == "tools_unsupported"
                && !crate::web_search::native_web_search_requested(&effective_request) =>
        {
            let original_tools = effective_request.tools.clone();
            inference_run.remove_exposed_tools(&mut effective_request);
            if effective_request.tools == original_tools {
                return Err(model_turn_execute_failure(
                    gateway,
                    request,
                    ingress,
                    start,
                    request_extras,
                    request_context.auth_subject.as_ref(),
                    error,
                ));
            }
            executor
                .execute(make_input(effective_request.clone()))
                .await
                .map_err(|error| {
                    model_turn_execute_failure(
                        gateway,
                        request,
                        ingress,
                        start,
                        request_extras,
                        request_context.auth_subject.as_ref(),
                        error,
                    )
                })?
        }
        Err(error) => {
            return Err(model_turn_execute_failure(
                gateway,
                request,
                ingress,
                start,
                request_extras,
                request_context.auth_subject.as_ref(),
                error,
            ));
        }
    };
    *request = effective_request;
    inference_run.set_route(turn.route.clone());
    enter_phase(phase, Phase::Calling).map_err(|response| buffered_response(*response))?;
    Ok((turn, turn_started))
}

fn model_turn_error_outcome(error: crate::agent::ModelTurnError) -> RoundOutcome {
    let status = match error.code.as_str() {
        "cancelled" => StatusCode::from_u16(499).expect("valid cancellation status"),
        "deadline_exceeded" => StatusCode::GATEWAY_TIMEOUT,
        "model_not_found" | "STRAVIA_NOT_FOUND" => StatusCode::NOT_FOUND,
        "model_unavailable" | "provider_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "tools_unsupported"
        | "web_search_unsupported"
        | "input_modality_unsupported"
        | "thinking_level_unsupported"
        | "protected_context_unrepresentable" => StatusCode::BAD_REQUEST,
        "protocol_lossy_rejected" | "STRAVIA_PROTOCOL_LOSSY_REJECTED" => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        "api_key_model_forbidden" | "capability_forbidden" | "STRAVIA_FORBIDDEN" => {
            StatusCode::FORBIDDEN
        }
        "STRAVIA_AUTH_ERROR" => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_GATEWAY,
    };
    let response = if error.code.starts_with("STRAVIA_") {
        let message = match error.code.as_str() {
            "STRAVIA_FORBIDDEN" if error.message == "access to this model is not permitted" => {
                "api key not allowed for this model".to_owned()
            }
            _ => error.message,
        };
        (
            status,
            axum::Json(serde_json::json!({
                "error": {
                    "type": error.code,
                    "code": status.as_u16(),
                    "message": message,
                }
            })),
        )
            .into_response()
    } else {
        coded_error_response(status, &error.code, &error.message)
    };
    buffered_response(response)
}

fn model_turn_execute_failure(
    gateway: &Gateway,
    request: &AiRequest,
    ingress: ProtocolId,
    start: Instant,
    request_extras: &RequestExtras,
    auth_subject: Option<&crate::proxy::context::AuthSubject>,
    error: crate::agent::ModelTurnError,
) -> RoundOutcome {
    let outcome = model_turn_error_outcome(error);
    let status = match &outcome {
        RoundOutcome::Deliver { response, .. } => response.status().as_u16(),
        RoundOutcome::NextRound { .. } => 500,
    };
    LogBuilder::from_dispatch(
        gateway,
        &ingress.to_string(),
        &request.model,
        request.reasoning.level,
        auth_subject,
        start,
    )
    .stream_flag(request.stream.enabled)
    .status(status)
    .with_req_extras(request_extras)
    .emit();
    outcome
}

pub(super) fn dispatch_next_round() -> RoundOutcome {
    RoundOutcome::NextRound {
        run: None,
        phase: None,
    }
}

/// Owned request HTTP metadata kept for log entries. Used by the non-stream
/// and stream handlers (not the force-stream handler which omits request
/// details from its log path).
#[derive(Clone)]
pub(super) struct RequestExtras {
    method: String,
    path: String,
    headers: Option<String>,
    body: Option<String>,
}

// Utility helpers (is_retryable, runtime_binding_headers, load_model_backends,
// forwarded_client_headers) are in util.rs.

pub(super) fn ai_response_to_deltas(resp: &AiResponse) -> Vec<crate::protocol::ir::AiStreamDelta> {
    use crate::protocol::ir::AiStreamDelta;
    let mut deltas = Vec::new();
    let mut response_profile = serde_json::Map::new();
    for key in [
        "__open_responses_effective_request",
        "__open_responses_response_profile",
    ] {
        if let Some(profile) = resp
            .vendor
            .ingress
            .get(key)
            .and_then(serde_json::Value::as_object)
        {
            response_profile.extend(profile.clone());
        }
    }
    if !response_profile.is_empty() {
        deltas.push(AiStreamDelta::ResponseMetadata {
            metadata: serde_json::Value::Object(response_profile),
        });
    }
    deltas.push(AiStreamDelta::MessageStart {
        id: if resp.id.is_empty() {
            format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
        } else {
            resp.id.clone()
        },
        model: resp.model.clone(),
    });
    for (output_index, item) in resp.items.iter().enumerate() {
        if let Some(text) = item.output_text_ref()
            && !text.is_empty()
        {
            deltas.push(AiStreamDelta::TextDeltaWithMetadata {
                text: text.to_owned(),
                logprobs: Vec::new(),
                obfuscation: None,
                output_index: Some(output_index),
                content_index: Some(0),
            });
        } else if let Some(refusal) = item.refusal_ref()
            && !refusal.is_empty()
        {
            deltas.push(AiStreamDelta::RefusalDeltaWithIndex {
                text: refusal.to_owned(),
                output_index,
                content_index: 0,
            });
        } else if let Some((summary, content, _)) = item.reasoning_ref() {
            for (content_index, text) in summary.iter().enumerate() {
                deltas.push(AiStreamDelta::ReasoningSummaryDelta {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                });
            }
            for (content_index, text) in content.iter().enumerate() {
                deltas.push(AiStreamDelta::ThinkingDeltaWithMetadata {
                    text: text.clone(),
                    obfuscation: None,
                    output_index: Some(output_index),
                    content_index: Some(content_index),
                });
            }
        } else if let Some((text, signature)) = item.thinking_ref()
            && !text.is_empty()
        {
            deltas.push(AiStreamDelta::ThinkingDelta(text.to_owned()));
            if let Some(signature) = signature.filter(|value| !value.is_empty()) {
                deltas.push(AiStreamDelta::ThinkingSignature(signature.to_owned()));
            }
        } else if let Some(call) = item.function_call_ref() {
            deltas.push(AiStreamDelta::ToolCallStart {
                index: output_index,
                id: call.id.clone(),
                name: call.name.clone(),
            });
            if !call.arguments.is_empty() {
                deltas.push(AiStreamDelta::ToolCallDelta {
                    index: output_index,
                    arguments: call.arguments.clone(),
                });
            }
        } else if let Some(raw) = item.unknown_ref() {
            deltas.push(AiStreamDelta::Unknown {
                raw: raw.to_string(),
            });
        }
        deltas.push(AiStreamDelta::ItemDone {
            index: output_index,
            item: item.clone(),
        });
    }

    if let Some(metadata) = resp.vendor.ingress.get("__google_response_metadata") {
        deltas.push(AiStreamDelta::Unknown {
            raw: serde_json::json!({"__google_response_metadata": metadata}).to_string(),
        });
    }
    deltas.push(AiStreamDelta::Usage(resp.usage.clone()));
    if let Some(terminal) = resp.vendor.egress.get("__open_responses_terminal")
        && let Some(status) = terminal.get("status").and_then(serde_json::Value::as_str)
    {
        deltas.push(AiStreamDelta::ResponseTerminal {
            status: status.to_owned(),
            incomplete_details: terminal
                .get("incomplete_details")
                .filter(|value| !value.is_null())
                .cloned(),
        });
    }
    deltas.push(AiStreamDelta::Done {
        stop_reason: resp
            .stop_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string()),
    });
    deltas
}

/// Emit a `LogEntry` for a request that failed to decode at the ingress
/// boundary (before `orchestrate` runs) and return the corresponding
/// 400 `Response`. Ensures decode failures show up in the in-app log module
/// rather than only in stdout tracing.
pub(crate) fn log_decode_error(
    gw: &Gateway,
    envelope: &RawEnvelope,
    ingress: ProtocolId,
    err: impl std::fmt::Display,
) -> Response {
    let msg = format!("invalid request: {err}");
    let request_body_str = envelope
        .body
        .as_ref()
        .and_then(|b| serde_json::to_string(b).ok());
    let request_headers_str = serde_json::to_string(&envelope.headers).ok();
    let ingress_str = ingress.to_string();
    LogBuilder::from_dispatch(gw, &ingress_str, "", None, None, Instant::now())
        .status(400)
        .with_req_extras(&RequestExtras {
            method: envelope.method.clone(),
            path: envelope.path.clone(),
            headers: request_headers_str,
            body: request_body_str,
        })
        .resp_body(Some(
            serde_json::json!({ "error": { "message": msg.clone() } }).to_string(),
        ))
        .emit();
    error_response(400, &msg)
}

// StreamResponseAccumulator and ensure_tool_index are in accumulator.rs.

#[cfg(test)]
mod canonical_stream_tests {
    use super::*;

    #[test]
    fn canonical_reencoding_preserves_dated_incomplete_terminal() {
        let mut response = AiResponse::new("resp_1", "logical-model");
        response.vendor.egress.insert(
            "__open_responses_terminal".into(),
            serde_json::json!({
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
            }),
        );

        let deltas = ai_response_to_deltas(&response);

        assert!(matches!(
            deltas.as_slice(),
            [
                crate::protocol::ir::AiStreamDelta::MessageStart { .. },
                crate::protocol::ir::AiStreamDelta::Usage(_),
                crate::protocol::ir::AiStreamDelta::ResponseTerminal {
                    status,
                    incomplete_details: Some(details),
                },
                crate::protocol::ir::AiStreamDelta::Done { .. },
            ] if status == "incomplete"
                && details["reason"] == "max_output_tokens"
        ));
    }
}

#[cfg(test)]
mod openai_generation_target_tests {
    use super::*;
    use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
    fn unlabelled_provider() -> Provider {
        Provider {
            id: "provider".into(),
            name: "Custom Provider".into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "https://example.com/v1".into(),
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            adapter_credentials: r#"{"apiKey":"secret"}"#.into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn unlabelled_open_responses_target_uses_openai_vendor_adapter() {
        let adapter = resolve_vendor_adapter(&unlabelled_provider(), Protocol::OpenResponses)
            .expect("Open Responses vendor adapter");

        assert_eq!(adapter.vendor_id(), "openai");
    }

    #[test]
    fn unlabelled_chat_target_keeps_custom_vendor_adapter() {
        let adapter = resolve_vendor_adapter(&unlabelled_provider(), Protocol::OpenAICompatible)
            .expect("custom vendor adapter");

        assert_eq!(adapter.vendor_id(), "custom");
    }

    #[test]
    fn unlabelled_open_responses_target_does_not_enable_generation_transport() {
        assert!(!is_openai_generation_target(
            None,
            None,
            OPEN_RESPONSES_2026_04_24,
            false
        ));
    }

    #[test]
    fn unlabelled_chat_target_does_not_change_protocol_negotiation() {
        assert!(!is_openai_generation_target(
            None,
            None,
            crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false
        ));
    }

    #[test]
    fn explicit_openai_target_keeps_generation_transport() {
        assert!(is_openai_generation_target(
            Some("openai"),
            Some("openai"),
            crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false
        ));
    }

    #[test]
    fn embeddings_never_use_responses_generation_transport() {
        assert!(!is_openai_generation_target(
            Some("openai"),
            None,
            OPEN_RESPONSES_2026_04_24,
            true
        ));
    }

    #[test]
    fn catalog_openai_vendor_does_not_enable_generation_transport() {
        assert!(!is_openai_generation_target(
            Some("openai"),
            Some("meta"),
            OPEN_RESPONSES_2026_04_24,
            false
        ));
    }
}
